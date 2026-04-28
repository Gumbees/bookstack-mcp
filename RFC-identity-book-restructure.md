# RFC: Identity Book Restructure + DB-as-Index Architecture

| Field | Value |
|---|---|
| Status | Draft v2 — accepting comments via PR review |
| Target version | **v1.0.0** (was v0.8.0 in draft v1) |
| Original RFC | merged at `4150646` (PR #26) — this PR amends it |
| Author | Nate Smith |
| Co-author | Pia (Apiara) |

## Summary

**v1.0.0 architecture pivot.** Treat our SQLite/Postgres database as the **structural index of every BookStack content item we care about**, plus a **page-body cache** populated by a reconciliation worker. BookStack remains canonical for content (markdown body, page revisions, attachments, permissions, the wiki UX) but our DB owns: metadata, parent-child relationships, identity classification, dedup constraints, and a fresh-enough cache of every page body. Result: most operations that previously required BookStack API calls become local DB queries, briefing latency drops to <100 ms typical, dedup becomes a UNIQUE constraint, and the migration tool's plan is a SQL query.

The original RFC's structural goals — chapters inside the Identity book (`Agents`, `Subagent Conversations`, `Journal`, `Journal Archive - {YEAR}`), Collage staying its own book, year-rollover archive sweep, programmatic update actions (`append` / `update_section` / `append_section`), opt-in `remember_migrate` tool — all stay. The mechanism changes: instead of walking BookStack contents on every call, we query our index; instead of name-match lookup, we hit a UNIQUE constraint; instead of fetching every body for the briefing, we serve from page-cache.

## Architecture: DB as index, BookStack as presentation

### Principle

Every BookStack call falls into one of two buckets:

- **Content bucket.** Read or write the actual markdown body. BookStack API stays in the loop here (`get_page` for a fresh body, `create_page` / `update_page` / `delete_page` / `move_page` for writes). Webhooks tell us when a user edits a page in the BookStack UI so we can refresh.
- **Structural bucket.** Existence checks, listings, parent-child traversal, identity classification, "what's the page id for today's journal entry," dedup. Under v1.0.0 these all become local SQL.

The page-body cache narrows the content bucket further: most reads (`get_page` for system_prompt_additions, identity manifests, recent journal pages displayed in the briefing) hit a cached row populated by the reconciliation worker. BookStack API is only consulted on cache miss or when we're about to write.

### New DB tables

```sql
-- Mirror of BookStack's content tree, scoped to the shelves we care about.
-- Every row is reconciled by the indexer worker. Webhook + periodic delta
-- walk + initial full walk all converge on these tables.

CREATE TABLE bookstack_shelves (
    shelf_id INTEGER PRIMARY KEY,           -- BookStack shelf id
    name TEXT NOT NULL,
    slug TEXT NOT NULL,
    shelf_kind TEXT NOT NULL,               -- 'hive' | 'user_journals' | 'unclassified'
    indexed_at INTEGER NOT NULL,            -- unix ts of most recent reconcile
    deleted INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE bookstack_books (
    book_id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    slug TEXT NOT NULL,
    shelf_id INTEGER,                       -- nullable: shelfless books exist
    -- structural classification, filled in by classify_book()
    identity_ouid TEXT,                     -- which AI identity (if any)
    book_kind TEXT NOT NULL,                -- 'identity' | 'collage' | 'shared_collage'
                                            -- | 'user_identity' | 'user_journal'
                                            -- | 'unclassified'
    indexed_at INTEGER NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (shelf_id) REFERENCES bookstack_shelves(shelf_id) ON DELETE SET NULL
);

CREATE TABLE bookstack_chapters (
    chapter_id INTEGER PRIMARY KEY,
    book_id INTEGER NOT NULL,
    name TEXT NOT NULL,
    slug TEXT NOT NULL,
    -- structural classification
    identity_ouid TEXT,
    chapter_kind TEXT NOT NULL,             -- 'agents' | 'subagent_conversations'
                                            -- | 'journal_active' | 'journal_archive'
                                            -- | 'unclassified'
    archive_year INTEGER,                   -- non-NULL only for journal_archive
    indexed_at INTEGER NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (book_id) REFERENCES bookstack_books(book_id) ON DELETE CASCADE
);

CREATE TABLE bookstack_pages (
    page_id INTEGER PRIMARY KEY,
    book_id INTEGER NOT NULL,
    chapter_id INTEGER,                     -- NULL for pages loose at book root
    name TEXT NOT NULL,
    slug TEXT NOT NULL,
    url TEXT,
    page_created_at TEXT,                   -- ISO 8601 from BookStack
    page_updated_at TEXT,                   -- ditto; webhook signal for cache freshness
    -- structural classification
    identity_ouid TEXT,
    page_kind TEXT NOT NULL,                -- 'manifest' | 'agent' | 'journal_entry'
                                            -- | 'collage_topic' | 'subagent_conversation'
                                            -- | 'system_prompt_addition' | 'unclassified'
    page_key TEXT,                          -- date for journal, slug for collage,
                                            -- agent-name for agents, etc.
    archive_year INTEGER,                   -- non-NULL when page is in archive chapter
    indexed_at INTEGER NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (book_id) REFERENCES bookstack_books(book_id) ON DELETE CASCADE,
    FOREIGN KEY (chapter_id) REFERENCES bookstack_chapters(chapter_id) ON DELETE SET NULL
);

-- Dedup enforcement: at most one non-deleted page per (identity, kind, key).
CREATE UNIQUE INDEX bookstack_pages_dedup
    ON bookstack_pages (identity_ouid, page_kind, page_key)
    WHERE deleted = 0 AND identity_ouid IS NOT NULL AND page_key IS NOT NULL;

-- Page-body cache. One row per page; refreshed by the indexer when the
-- BookStack `updated_at` advances or on explicit invalidation.
CREATE TABLE page_cache (
    page_id INTEGER PRIMARY KEY,
    markdown TEXT,                          -- frontmatter-stripped body
    raw_markdown TEXT,                      -- with frontmatter
    html TEXT,                              -- rendered (if requested)
    cached_at INTEGER NOT NULL,
    page_updated_at TEXT,                   -- BookStack updated_at when this body was fetched
    FOREIGN KEY (page_id) REFERENCES bookstack_pages(page_id) ON DELETE CASCADE
);

-- Reconciliation job queue. Mirrors `embed_jobs` shape so the worker pattern
-- is familiar.
CREATE TABLE index_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    scope TEXT NOT NULL,                    -- 'page:123' | 'book:45' | 'chapter:67'
                                            -- | 'shelf:927' | 'all' | 'delta'
    kind TEXT NOT NULL,                     -- 'index' | 'cache' | 'both'
    status TEXT NOT NULL DEFAULT 'pending', -- 'pending' | 'running' | 'completed' | 'failed'
    triggered_by TEXT NOT NULL,             -- 'webhook' | 'cron' | 'startup' | 'admin'
    started_at INTEGER,
    finished_at INTEGER,
    progress INTEGER NOT NULL DEFAULT 0,
    total INTEGER NOT NULL DEFAULT 0,
    error TEXT
);
CREATE INDEX index_jobs_pending ON index_jobs (status) WHERE status = 'pending';

-- Singleton bookkeeping for the indexer.
CREATE TABLE index_meta (
    key TEXT PRIMARY KEY,                   -- 'last_full_walk_at' | 'last_delta_walk_at'
                                            -- | 'last_webhook_event_at' | etc.
    value TEXT NOT NULL
);
```

### Reconciliation worker

A new background tokio task in `bsmcp-server` (no new binary — see Decisions). Polls `index_jobs` for `pending` rows, processes one at a time with bounded concurrency for fan-outs (matching the embedder's existing pattern). Three trigger sources:

1. **Webhook-triggered.** BookStack already POSTs to `/webhooks/bookstack` on `page.create` / `page.update` / `page.delete`. Today the handler enqueues an embed job. Under v1.0.0 it also enqueues an `index_jobs` row with scope `page:{id}`. The worker pulls the page metadata + body via BookStack API, classifies it (`classify_page()`), upserts into `bookstack_pages` + `page_cache`, and (if the embedder is enabled) enqueues an `embed_jobs` row.
2. **Periodic delta walk.** Every `BSMCP_INDEX_DELTA_INTERVAL_SECONDS` (default `300` = 5 min), enqueue a `delta` job. Worker calls `list_pages_by_updated > index_meta.last_delta_walk_at`, reconciles each. Catches webhook misses (network blip, BookStack restart, webhook secret rotation).
3. **Initial full walk.** On bsmcp-server startup, if `index_meta.last_full_walk_at` is NULL, enqueue an `all` job. Worker walks every shelf in `global_settings` (Hive shelf, User Journals shelf), every book on those shelves, every chapter, every page. Populates the entire index from scratch. Idempotent — safe to re-run; classifications converge.

Worker concurrency mirrors the embedder's `tokio::sync::Semaphore(N)` pattern (default 10 concurrent BookStack API calls). Page-cache writes happen alongside index writes in the same transaction.

### Classification

`classify_page()`, `classify_chapter()`, `classify_book()`, `classify_shelf()` are pure functions over (parent context + name + position). Examples:

- A book named `Pia Identity` on the Hive shelf → `book_kind='identity'`, `identity_ouid='019dc66e4dc27d329e4a4abd1bec0c80'` (looked up via the manifest page's frontmatter).
- A chapter named `Agents` inside an identity book → `chapter_kind='agents'`, inherits `identity_ouid` from the book.
- A chapter named `Journal Archive - 2025` → `chapter_kind='journal_archive'`, `archive_year=2025`.
- A page named `Agent: pia-journal-agent` inside an `agents` chapter → `page_kind='agent'`, `page_key='pia-journal-agent'`.
- A page named `2026-04-27` inside a `journal_active` chapter → `page_kind='journal_entry'`, `page_key='2026-04-27'`.

When classification can't decide (e.g., a user manually creates an arbitrary page), `*_kind = 'unclassified'`. The dedup UNIQUE index is conditional on `identity_ouid IS NOT NULL AND page_key IS NOT NULL`, so unclassified pages never trigger constraint violations.

### Webhook reconciliation flow

Today's `/webhooks/bookstack` handler fires on the embedder side. Under v1.0.0:

1. POST arrives, secret verified.
2. Handler enqueues an `index_jobs` row (`scope='page:{id}'`, `triggered_by='webhook'`, `kind='both'`).
3. If embedder is enabled, also enqueues an `embed_jobs` row (existing behavior).
4. Worker picks up the index job within ms, fetches page via `get_page`, classifies, upserts index + cache.

Webhook latency end-to-end: ~50-200ms for a single-page edit to be reflected in our index/cache. Far below human-perceptible.

### What stays in BookStack vs. moves to DB

| Kind of data | BookStack | DB (index) | DB (cache) |
|---|---|---|---|
| Page markdown body | canonical | — | freshness-tracked copy |
| Page metadata (id, name, parent, created_at, updated_at) | canonical | mirrored | — |
| Chapter metadata | canonical | mirrored | — |
| Book metadata | canonical | mirrored | — |
| Shelf metadata | canonical | mirrored | — |
| Page revision history | canonical | — | — |
| Attachments / images | canonical | — | — |
| Content permissions (owner-only locks etc.) | canonical | — | — |
| Identity classification (which page is whose manifest, agent, journal entry, …) | inferred via name pattern | **canonical** (computed once, persisted) | — |
| Dedup constraint | none | **canonical** (UNIQUE index) | — |
| Audit log | — | canonical (existing `remember_audit`) | — |
| Embeddings | — | canonical (existing `chunks`) | — |

### Authentication for the indexer

The reconciliation worker uses the `BSMCP_EMBED_TOKEN_*` BookStack token (same one the embedder uses to crawl pages). That token must have read access to every shelf the indexer walks. Permissions on individual locked pages (e.g., owner-only journal pages) may exclude it — those pages get skipped by the indexer with a warning, exactly the same way they'd be skipped today by the embedder. Per-user content access for live MCP requests still uses the user's own token; the indexer's role is structural reconciliation, not per-user query serving.

## Motivation

Three problems in the current shape, all visible on the live Hives.

### 1. Duplicate creation

`auto_provision_user_identity` (`crates/bsmcp-server/src/remember/user_provision.rs`) and `identity::create` (`crates/bsmcp-server/src/remember/identity.rs`) only check their own `UserSettings` columns before creating books and pages — they never ask BookStack "does a book named `{name} Identity` already exist on this shelf?". When a settings column gets cleared (manually, via `remember_config write`, or migration), the next call creates a brand-new book/page and orphans the existing one.

Live evidence — Pia's Identity book (`928`) on the Bee's Roadhouse Hive contains parallel sets of agent pages:

- `Agent Backup - code-writer` (1880, created 2026-04-03) and `Agent: pia-code-writer` (2059, created 2026-04-26)
- Same pattern for `cpa`, `documenter`, `receipt-scanner`, `nate-journal`, `pia-journal`, `pia-remember`, `researcher`

Six duplicate pairs in one identity book. The "Idempotent" doc-comment in `user_provision.rs:11` is technically correct against its own state but not against BookStack's state.

### 2. No structural enforcement

DTC's `Apis's Identity` book (`3062`) was set up with an empty `Agents` chapter (`3064`) — but the agent pages (`Agent: apis-remember`, `Agent: apis-journal`, `Agent Type Templates`) sit at the book root, not inside the chapter. The chapter scaffolding exists; the code that places pages doesn't target it.

There's a parallel `provision::create_page` helper that supports `chapter_id`, but it's marked `#[allow(dead_code)]` and never wired in. Pages that should live in `Agents` end up at the root; user has to move them by hand.

### 3. Journal sprawl

Each identity has a separate Journal book (Pia: 986, Apis: 3283) with monthly chapters, daily pages, no archive boundary. As the journal grows, the briefing's "recent pages" list and the semantic-search candidate pool both pull from the full history forever. There's no natural pruning point.

## Target shape

Per AI identity, on the Hive shelf:

```
{Name}'s Identity (book)                      ← the only book per identity on the Hive shelf
├── Identity page (loose at book root, no chapter)   ← manifest
├── Chapter: Agents
│   ├── Agent: {name}-journal-agent
│   ├── Agent: {name}-remember-agent
│   └── …
├── Chapter: Subagent Conversations           ← agent-to-agent transcripts
│   └── …
├── Chapter: Journal                          ← current year, daily pages
│   ├── 2026-04-27
│   └── … (flat YYYY-MM-DD pages, ~365 max)
├── Chapter: Journal Archive - 2025           ← lazily created on year rollover
├── Chapter: Journal Archive - 2024
└── …

{Name}'s Collage (book, separate, stays as-is)
Cross-Identity Collage (shared book, separate, stays as-is)
```

What changes:

- **Removed:** the separate `{Name}'s Journal` book per identity. Becomes a chapter inside the Identity book.
- **Added:** `Agents` chapter (mandatory). Agent definition pages live here.
- **Added:** `Subagent Conversations` chapter (mandatory, scaffolded empty). Future agent-to-agent transcripts go here.
- **Added:** `Journal Archive - {YEAR}` chapters, one per past year, lazily created on first archive.

What stays:

- **Collage** stays its own book. Per-identity content discovery worked well in book form and the shape doesn't conflict with the structural enforcement we want for the identity book.
- **Cross-Identity Collage** stays a separate shared book on the Hive shelf.
- **Hive shelf** stays the global root.

## Settings schema delta

```rust
pub struct UserSettings {
    // existing
    pub ai_identity_book_id: Option<i64>,
    pub ai_identity_page_id: Option<i64>,

    // NEW — chapters inside the identity book
    pub ai_identity_agents_chapter_id: Option<i64>,
    pub ai_identity_subagent_conversations_chapter_id: Option<i64>,
    pub ai_identity_journal_chapter_id: Option<i64>,

    // existing — collage stays a book
    pub ai_collage_book_id: Option<i64>,
    pub ai_shared_collage_book_id: Option<i64>,

    // DEPRECATED — replaced by ai_identity_journal_chapter_id
    pub ai_hive_journal_book_id: Option<i64>,

    // ... rest unchanged ...
}
```

`ai_hive_journal_book_id` stays in the schema as a tombstone for backward-compat reads during migration. Once data migration completes for an identity, it gets cleared and the briefing/journal logic stops consulting it.

The three new `_chapter_id` columns are required-for-write. Reads degrade gracefully when null (briefing returns `settings_not_configured` for the journal section, etc.) but writes that need to land in a chapter (`remember_journal action=write`, agent provisioning) refuse with an `error.fix` block pointing at `remember_migrate` or `remember_config write`.

`/settings` UI gets the three new fields, populated by the chapter dropdown for the configured identity book.

## Identity creation (one-shot)

`remember_identity action=create name=…` structures the whole thing in one call:

1. Find-or-create Identity book on Hive shelf (name = `{name} Identity`, dedup by exact name match within the shelf's books).
2. Find-or-create Identity page (manifest) at the book root, loose, no chapter (dedup by exact name `Identity` or `Identity - {name}` within the book).
3. Find-or-create three chapters inside the Identity book: `Agents`, `Subagent Conversations`, `Journal`.
4. Find-or-create Collage book on Hive shelf (name = `{name}'s Collage`, dedup by name).
5. Lock Identity book and manifest page to admin-only edit (existing behavior).
6. Return all IDs in `proposed_settings`:

```json
{
  "action": "created",  // or "found_existing"
  "name": "Pia",
  "ouid": "…",
  "book_id": 928,
  "manifest_page_id": 932,
  "agents_chapter_id": …,
  "subagent_conversations_chapter_id": …,
  "journal_chapter_id": …,
  "collage_book_id": …,
  "proposed_settings": { … }
}
```

The `auto_provision_user_identity` flow (per-user side) mirrors this shape: find-or-create the per-user Identity book, identity page, chapters (`Journal` only — no Agents/Subagent Conversations on the user side, those are AI-only), then return the IDs.

## Journal logic (chapter-scoped, year-aware)

`remember_journal action=write key=YYYY-MM-DD body=…`:

1. **Year-rollover sweep** (always, idempotent, cheap):
   - List pages in `ai_identity_journal_chapter_id`.
   - Group by `created_at` year (using the user's IANA timezone, not UTC — see Decisions).
   - Any year ≠ current year:
     - Find-or-create `Journal Archive - {Y}` chapter inside the same identity book.
     - `move_page` each stale page into that chapter.
   - Scoped strictly by `ai_identity_book_id` so we never pick up another identity's archive chapters.
2. Find-or-create the `Journal` chapter for the current year (defensive — should already exist from provisioning).
3. Look up an existing page in the journal chapter with name == `key`. If present, `update_page`. Else `create_page` with `chapter_id = journal_chapter_id`.

`remember_journal action=read key=YYYY-MM-DD`:

- Parse year from key.
- If year == current year: look in `ai_identity_journal_chapter_id`.
- Else: look in `Journal Archive - {Y}` chapter inside `ai_identity_book_id` (find by name, no settings column needed for archives).

`remember_journal action=search`:

- Default scope: `Journal` chapter only (current year).
- Extended scope: `--include-archives=true` flag walks every `Journal Archive - *` chapter in the identity book. Off by default to keep the briefing's recent-pages list and the search candidate pool small.

`remember_journal action=delete`:

- Same soft-delete behavior as today (`[archived]` prefix, `deleted: true` frontmatter). Works on either the active chapter or any archive.

## Migration

Two kinds, two triggers.

### Schema migration (auto, on server startup)

`ALTER TABLE user_settings ADD COLUMN IF NOT EXISTS …` for the three new chapter columns. Existing rows get NULL. Same pattern as every prior settings addition. No user action required.

### Data migration (opt-in, via `remember_migrate` MCP tool)

Runs under the user's BookStack auth — the server's embed token can't move the user's owner-only journal pages. Has to be explicit so the user/AI can choose when to pay the latency cost (potentially minutes for an identity with hundreds of pages).

#### Trigger via `setup_nudge`

`remember_briefing` runs a legacy-state detector and emits a nudge when it finds:

- `ai_hive_journal_book_id` is set AND `ai_identity_journal_chapter_id` is null, OR
- The identity book root contains pages matching `Agent: *` (agent pages outside their chapter), OR
- An expected chapter (`Agents`, `Subagent Conversations`, `Journal`) is missing from a book that has an identity manifest.

```json
"setup_nudge": {
  "show": true,
  "summary": "Identity book restructure available: 22 loose pages in {Name}'s Identity, 64 pages in legacy journal book.",
  "recommended_action": "remember_migrate action=plan",
  "two_paths": {
    "preview": "remember_migrate action=plan — dry-run, returns what would happen",
    "execute": "remember_migrate action=apply — execute the plan"
  }
}
```

#### `remember_migrate` tool

| Action | Behavior |
|---|---|
| `plan` | Dry-run. Returns a structured list of every move/create that would happen. No writes. |
| `apply` | Executes the plan. Idempotent. Returns per-step result (`moved`, `created`, `skipped`, `failed: <reason>`). Best-effort: a single failure doesn't abort the whole pass; it's logged in the response. |
| `status` | Returns "is this identity fully migrated?" — pass/fail with a per-check breakdown. |

Internally `apply` runs these steps in order, every step idempotent:

1. Find-or-create `Agents`, `Subagent Conversations`, `Journal` chapters in identity book.
2. Move agent-named pages (matching `Agent: *`) from identity book root → `Agents` chapter.
3. Move all pages from legacy journal book → `Journal` chapter in identity book.
4. Run year-rollover sweep on `Journal` chapter (any page with `created_at` year ≠ current → `Journal Archive - {Y}` chapter, lazy-created).
5. Update `user_settings`: write the new chapter IDs, clear `ai_hive_journal_book_id`.

Migration is scoped strictly by `book_id` so we never confuse one identity's archive chapters with another identity's chapters when looking up `Journal Archive - {Y}` by name.

#### Why not auto-run from briefing

Considered: briefing detects legacy state, runs `apply` synchronously, returns briefing once done. Rejected because:

- First post-upgrade briefing becomes 30s–2min depending on page count.
- Mid-migration partial failure leaves both legacy and new state populated; the briefing response can't surface that nuance well.
- Concurrent first-briefing-after-upgrade hits BookStack API limits at the same time.

The nudge-then-explicit-tool pattern keeps briefing fast, makes migration observable and resumable, and matches how `setup_nudge` already steers users to fix config.

### Legacy book disposition

The legacy `{Name}'s Journal` book becomes empty after migration but is **not** auto-deleted. Reasons:

- Deletion is destructive; BookStack's recycle bin is the user's safety net.
- Webhook-triggered re-indexing might still reference the book ID briefly.
- The user's preference (delete vs keep as a tombstone) is not something the migration tool should decide.

Migration sets `ai_hive_journal_book_id` to null, so subsequent operations ignore the book. The user can `delete_book` it manually via the BookStack UI or the existing `delete_book` MCP tool.

## Duplicate detection (`find_or_create_*`)

Three new helpers in `provision.rs`, replacing every direct `client.create_*` callsite in the remember module:

```rust
/// Find a book on a shelf by exact name match, or create it. Returns the book id.
pub async fn find_or_create_book_on_shelf(
    client: &BookStackClient,
    shelf_id: i64,
    name: &str,
    description: &str,
) -> ProvisionOutcome { … }

/// Find a chapter inside a book by exact name match, or create it. Returns the chapter id.
pub async fn find_or_create_chapter(
    client: &BookStackClient,
    book_id: i64,
    name: &str,
    description: &str,
) -> ProvisionOutcome { … }

/// Find a page inside a book or chapter by exact name match, or create it. Returns the page id.
/// Caller specifies parent via `chapter_id` (preferred) or `book_id` (loose page).
pub async fn find_or_create_page(
    client: &BookStackClient,
    parent_book_id: Option<i64>,
    parent_chapter_id: Option<i64>,
    name: &str,
    body: &str,
) -> ProvisionOutcome { … }
```

`ProvisionOutcome` adds a new variant `FoundExisting { id, name }` so callers can distinguish "I made this" from "this was already here." Useful for the migration plan output.

Match semantics: exact case-sensitive name match against the BookStack name. No fuzzy match. Description is only used at create time (existing books/chapters keep their existing descriptions).

Replaced callsites:

- `provision::create_named_book` → `find_or_create_book_on_shelf`
- `provision::create_named_page` → `find_or_create_page` (loose at book root)
- `provision::create_page` → `find_or_create_page` (with chapter)
- `identity::create` book creation → `find_or_create_book_on_shelf`
- `identity::create` manifest page creation → `find_or_create_page` (loose, no chapter)
- `auto_provision_user_identity` book/page creation → corresponding find-or-create helpers
- New: chapter provisioning in `identity::create` and the migration tool → `find_or_create_chapter`

## Programmatic content updates (new actions)

The current `write` action on collections (`journal`, `collage`, `shared_collage`, `user_journal`) and singletons (`whoami`, `user`) is **destructive replace** — `write` with the same `key` overwrites the entire page body. We hit this live during v0.7.4 smoke testing: a single test write to `journal key=2026-04-27` clobbered the day's actual content.

For identity updates and journal additions to be done programmatically — AI just provides content, server handles structural placement and provenance — we add three new actions to the schema. None of them break existing `write` callers; they're purely additive.

### `append` — collections only

Action signature: `journal action=append key=YYYY-MM-DD body="..." timestamp=true`.

Appends `body` to the existing page at `key`. If the page doesn't exist, creates it (same find-or-create semantics as `write`). Optional `timestamp=true` prefixes the appended chunk with a local-IANA-timezone time marker (`## 14:32 EDT`) so multi-append-per-day flows produce a readable timeline.

Use case: the AI realizes mid-conversation it wants to capture a thought in today's journal. It calls `journal action=append body="..." timestamp=true` — no read-modify-write cycle, no risk of clobbering an earlier entry.

Frontmatter: stamps `last_appended_at`, increments `append_count`, preserves `written_at` from the original create. The frontmatter `supersedes_page` field is **not** updated (the append doesn't supersede; it extends).

Available on: `journal`, `user_journal`, `collage`, `shared_collage`. Not on singletons — appending to an identity manifest doesn't have a clear semantic.

### `update_section` — collections + singletons

Action signature: `whoami action=update_section section="Communication style" body="..."` or `journal action=update_section key=YYYY-MM-DD section="Working notes" body="..."`.

Replaces the named H2 section's body, preserving every other section. Match semantics:

- Find the first H2 (`## …`) whose text equals `section` (exact match, case-sensitive, after trimming).
- The matched section's body runs from the H2 line up to (but not including) the next H2 or end-of-document.
- Replace that range with `## {section}\n\n{body}\n\n`.
- If no matching H2 exists, append `## {section}\n\n{body}` to the end of the document.

Use case: the AI learns Nate prefers terse responses. It calls `user action=update_section section="Communication style" body="Terse, direct, no preamble. Shorter beats longer."` — no need to read the rest of the manifest, no risk of clobbering "Working preferences" or "Domains and identities."

Frontmatter: stamps `last_section_update_at` and `last_updated_section: "{section}"`. `supersedes_page` updates as it does for `write` (since this IS a content change to the persistent identity).

Available on: every resource that supports `write` (i.e. all collections + the singletons `whoami`, `user`).

### `append_section` — collections + singletons

Action signature: `whoami action=append_section section="Recurring topics" body="- New project: ATLAS migration"`.

Same matching as `update_section`, but appends `body` to the section's existing body instead of replacing it. If the section doesn't exist, creates it (same as `update_section`'s fallback).

Use case: the AI wants to add one bullet to "Recurring topics" without re-rendering the whole list.

Frontmatter: same as `update_section`.

### Frontmatter timestamps — full set

After these additions:

| Field | Set on | Purpose |
|---|---|---|
| `written_at` | first `write` | initial creation timestamp |
| `last_appended_at` | every `append` | most recent append |
| `append_count` | every `append` | running counter |
| `last_section_update_at` | every `update_section` / `append_section` | most recent section edit |
| `last_updated_section` | every `update_section` / `append_section` | name of last edited section |
| `supersedes_page` | `write` and section ops | prior page id (lineage) |
| `written_by` / `ai_identity_ouid` / `user_id` / `trace_id` / `resource` / `key` | every write | provenance (existing) |

All timestamps in UTC ISO-8601; the AI uses the `meta.time` block to render them locally.

### Audit log additions

Every new action emits an audit entry with `action` ∈ `{append, update_section, append_section}` and the same `target_page_id` / `target_key` / `trace_id` shape as today's `write` and `delete`. No schema change required.

### MCP tool surface

The 12 existing `remember_*` tools each grow a few enum values in their `action` argument:

- `remember_journal action`: `read | write | append | update_section | append_section | search | delete`
- `remember_whoami action`: `read | write | update_section | append_section`
- `remember_user action`: `read | write | update_section | append_section`
- Same pattern for `remember_collage`, `remember_shared_collage`, `remember_user_journal`.

No new top-level tools needed. The richer action set keeps the tool surface flat.

### Why not just use `edit_page` / `replace_section` / `append_to_page`?

The bookstack-mcp already exposes those at the BookStack-page level — but they operate on raw page IDs, bypass the `/remember` envelope, and don't stamp provenance frontmatter or hit the audit log. Going through `remember_*` actions keeps every Hive content edit auditable, traceable, and addressed by stable resource keys (`key=YYYY-MM-DD`, `section="Communication style"`) instead of opaque page IDs that the AI shouldn't have to track.

## Decisions (with rationale)

These were left open in the design discussion. Defaults locked here; PR review can override any of them.

1. **Chapter names — literal strings:** `Agents`, `Subagent Conversations`, `Journal`, `Journal Archive - {YEAR}` (with literal space-hyphen-space and 4-digit year). Rationale: keep them readable in the BookStack UI; the year format mirrors how the user already writes archive markers. The migration tool matches by exact name, so changing these later is a real refactor — pick once, stick with it.

2. **Year semantics — local-time-in-user's-IANA-timezone, not UTC:** A user in `America/New_York` sees their `2026-01-01` journal page archive at NYC midnight, not at UTC midnight (which would be 7pm Dec 31). Rationale: the page name is `YYYY-MM-DD` — using the user's local-day boundary keeps the page name and the archive-year decision aligned. The `meta.time` block already carries the IANA timezone server-side; reuse it.

3. **Subagent Conversations chapter — scaffolded but empty initially:** No auto-write tool for it yet. Rationale: the chapter is structural; concrete write tools (e.g., `remember_subagent_conversation`) can be added in a follow-up RFC once the data shape stabilizes. Having the chapter exist now means future tools don't have to re-do the structural change.

4. **Legacy journal book cleanup — leave intact:** Migration empties the book and clears the settings pointer; the user manually deletes it. Rationale: see "Legacy book disposition" above.

5. **`auto_migrate` user setting — not added:** Migration is always explicit via `remember_migrate apply`. Rationale: data migration is observable, has cost, and a single failed page move shouldn't be hidden behind a "did it run?" boolean. Briefing's nudge does the prompting; the user/AI decides when to execute.

6. **`write` stays destructive-replace; new actions cover non-destructive cases:** Every existing `write` caller continues to work unchanged. New actions (`append`, `update_section`, `append_section`) handle the cases where AI agents should not be replacing the whole body. Rationale: changing `write` semantics under existing callers would silently break flows; making non-destructive operations their own actions is explicit and lets agents pick the right verb for the intent. The doc-string on `write` will note: "use `append` for journals, `update_section` for identity edits — `write` is full-replace and rarely what you want."

7. **Reconciliation worker lives in `bsmcp-server`, not a new binary:** Background tokio task using the same job-queue pattern as `embed_jobs`. Rationale: the indexer's work is lightweight (BookStack API calls + DB writes, no heavy compute), the server already has the BookStack credentials and the auth context, and adding a third binary multiplies operational concerns (compose files, container images, restart coordination). The embedder stays a separate binary because it does CPU-intensive ONNX/Ollama/OpenAI work and benefits from process isolation; the indexer doesn't.

8. **`index_jobs` is its own table, not piggybacked on `embed_jobs`:** Separate table with the same shape. Rationale: the two queues have different urgency (indexing should run immediately on webhook for live freshness; embedding can lag by minutes), different retry semantics (a failed embed doesn't invalidate the index; a failed index reconcile leaves stale dedup state), and different concurrency targets. Keeping them separate makes the worker policies tunable independently.

9. **Indexer uses `BSMCP_EMBED_TOKEN_*` for BookStack auth:** Same admin-scoped token the embedder uses to walk pages. Rationale: structural reconciliation needs to see every page on the configured shelves regardless of per-user permissions. Per-user content access for live MCP requests still uses the user's own token; the indexer's role is structural, not query-serving. Pages that the embed token can't access (e.g., owner-only journal pages on a separate user's identity) get logged and skipped, exactly the same way they're skipped by the embedder today.

10. **Page cache is webhook-invalidated, not TTL-based:** A row in `page_cache` is considered fresh if its `page_updated_at` matches the BookStack `updated_at` for that page in `bookstack_pages`. The indexer keeps both in lockstep (same transaction). On read-path cache lookup, if `page_cache.page_updated_at == bookstack_pages.page_updated_at`, serve from cache; otherwise treat as miss. Rationale: TTLs are inherently wrong (either too short = unnecessary refetches, or too long = stale reads); the webhook + delta-walk + matching-updated_at pattern gives us "always fresh modulo webhook latency" with zero unnecessary work.

11. **No new bsmcp-indexer container.** Phase 4's worker ships inside `bsmcp-server`. Rationale: same as decision #7. Operational simplicity wins. If profiling later shows index reconciliation contending with MCP request serving, splitting into a binary is a small refactor — the worker is already a self-contained tokio task.

## Performance targets

Numbers below are post-Phase-5 targets on the Bee's Roadhouse instance (BookStack `v26.03.3`, ~1900 indexed pages, Postgres backend with pgvector HNSW). Phases 1–4 don't aim at these directly; they set up the infrastructure for Phase 5 to deliver them.

| Operation | Today | After Phase 5 | Mechanism |
|---|---:|---:|---|
| `remember_briefing` (cold semantic) | ~1500-2500 ms | <300 ms | listings from index; bodies from cache |
| `remember_briefing` (warm — already-cached prompt context) | ~1500-2500 ms | <100 ms | full hit on index + cache, only semantic search and live embedding fetch hit network |
| `remember_directory` | ~400-700 ms | <10 ms | DB query |
| `remember_journal action=read` | ~200-400 ms (BookStack get_page) | <50 ms (cache hit) / ~250 ms (cache miss) | page_cache + bookstack_pages |
| `remember_identity action=list` | ~1000-1500 ms (per-book pages list) | <20 ms | `SELECT … FROM bookstack_books WHERE book_kind='identity'` |
| Dedup check on provisioning | ~400-700 ms (get_shelf + scan) | ~1 ms (UNIQUE constraint) | Postgres index lookup |
| Webhook → index updated | n/a (no index) | ~50-200 ms end-to-end | webhook → enqueue → worker → upsert |

Per-user briefing payload size — already trimmed in v0.7.4. Phase 5 doesn't change shape, just speeds delivery.

## Out of scope

- **Subagent conversation write tooling.** The chapter exists post-Phase-6; the tool to write to it is a follow-up.
- **Cross-identity migration coordination.** Each identity migrates independently. If a user has multiple AI identities on the same Hive (Pia + a future second identity), each one runs `remember_migrate` separately under its own context.
- **Collage book restructure.** The Collage book stays a book. This RFC doesn't touch it.
- **Briefing chunk trim revisions.** The trim work shipped in v0.7.4 is upstream of this RFC and stays.
- **Removing the deprecated `ai_hive_journal_book_id` column.** Stays as a tombstone field for at least one minor version; removal is a future cleanup PR after every active identity has migrated.
- **Storing page bodies as the canonical source.** This is **Option C** of the architecture spectrum (full DB-as-source-of-truth) and is explicitly not what we're doing. BookStack remains canonical for content; we're caching, not replacing.
- **Conflict resolution for concurrent BookStack-UI edit + MCP write.** The cache is webhook-invalidated and writes always go to BookStack first; if a user edits a page via the BookStack UI at the same instant an MCP `write` lands, BookStack's own last-writer-wins applies (same as today). The indexer reconciles on webhook receipt regardless of which side won.
- **Replicating BookStack's permission model in our DB.** Permissions stay in BookStack. The cache obeys the embed token's read-access; the live MCP request path obeys the user's token's read-access. We don't try to layer ACLs on top of `page_cache`.

## Implementation phasing

Six follow-up PRs, stacked, each independently mergeable. Phases 1 and 2 ship before the index work; Phases 3–6 land the v1.0.0 architecture in order.

1. **Phase 1 — `find_or_create_*` helpers + dedup at every existing callsite.** _Open as PR #27._ No structural change, no new chapters yet. Just stops duplicate-creation. Becomes redundant once Phase 4 ships the index (UNIQUE constraint supersedes name-match), but is the right interim step — immediately fixes the six duplicate agent-page pairs on Pia's Hive without waiting for the rest of v1.0.0.
2. **Phase 2 — `append` / `update_section` / `append_section` actions.** Pure-additive new actions on existing `remember_*` tools. Independent of the index work; ships either before or alongside Phase 3. Solves destructive-`write` for every agent immediately. New frontmatter timestamp fields (`last_appended_at`, `append_count`, `last_section_update_at`, `last_updated_section`) added at the same time.
3. **Phase 3 — DB-as-index schema + classification.** Adds `bookstack_shelves`, `bookstack_books`, `bookstack_chapters`, `bookstack_pages`, `page_cache`, `index_jobs`, `index_meta` tables. Adds the `classify_*` pure functions. No worker yet — just empty tables and the classification logic, unit-testable in isolation.
4. **Phase 4 — Reconciliation worker + initial full walk + webhook + delta cron.** Background tokio task in `bsmcp-server`. On first startup after this PR lands, queues an `all` job and walks every shelf the user has configured, populating the index and page-cache from existing BookStack content. Webhook handler enqueues `page:{id}` jobs. Periodic delta walk catches misses. Once this lands, the index is *live and authoritative* for structural reads; reads still go through BookStack temporarily until Phase 5 cuts them over.
5. **Phase 5 — Cut over the read paths to use the index.** Rewrite `remember_briefing`, `remember_directory`, `remember_journal action=read`, `remember_identity action=list`, `auto_provision_user_identity`'s discovery half, and `find_or_create_*` helpers (Phase 1) to query the index instead of BookStack. Page-body reads (system_prompt_additions, identity manifest) hit `page_cache` first, fall back to BookStack on miss. Briefing latency target: <100ms typical (vs. ~1-2s today on the BR Hive).
6. **Phase 6 — Identity book restructure: chapters + chapter-scoped journal + year rollover.** Adds the three chapter columns to `user_settings`, wires `Agents`/`Subagent Conversations`/`Journal` chapter creation into `identity::create`, rewrites `remember_journal` to be chapter-scoped with the year-rollover sweep. Now powered by the index (which makes year rollover a SQL UPDATE plus a fan-out of `move_page` calls).
7. **Phase 7 — `remember_migrate` tool + briefing setup_nudge detector.** The opt-in data migration tool plus the legacy-state detector that surfaces the nudge. The migration plan is now a query against the index (`SELECT * FROM bookstack_pages WHERE identity_ouid=? AND chapter_id IS NULL AND page_kind='agent'`); the apply step emits BookStack `move_page` calls and updates the index in one transaction. Migrates Pia and Apis to the new shape on the live Hives.

Total: seven phases. Phase 1 (PR #27) is already in review. Phases 2 and 3 can land in parallel after #27. Phases 4–7 are sequential.

## Open questions

1. **Index reconciliation under webhook secret rotation.** If `BSMCP_WEBHOOK_SECRET` rotates, in-flight webhook events are dropped. The 5-minute delta walk catches the gap, but during the gap the index can be stale. Acceptable for v1.0.0 — stale-by-up-to-5-minutes is a non-issue for the use cases here. Worth flagging in the deployment docs.
2. **Initial full-walk duration on large instances.** BR's instance has ~1900 pages; full walk is ~5-10 minutes at default concurrency. Larger instances might want a `BSMCP_INDEX_FULL_WALK_CONCURRENCY` knob. Defer to Phase 4 implementation; if the default isn't enough, add the env var then.
3. **Multi-tenant deployments.** This RFC assumes one BookStack per `bsmcp-server`. If we ever run multi-tenant (one server fronting multiple BookStack instances per request), the index needs a `bookstack_instance_id` discriminator on every row. Out of scope for v1.0.0.

---

🤖 Drafted in collaboration with Pia (Apiara) via Claude Code.
