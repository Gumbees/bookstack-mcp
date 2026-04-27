//! Per-user identity auto-provisioning.
//!
//! The Hive's AI side has Identity books on a shared "Hive" shelf with the
//! manifest page + sub-agent definition pages inside. This module mirrors that
//! layout for the human user side: a per-user "Identity" book on the
//! `user_journals_shelf`, containing
//!   - an `Identity` page (the user's manifest), and
//!   - an `Agent: {user_id}-journal-agent` page (the user's personal journal
//!     agent definition the AI bootstrap protocol fetches into local cache).
//!
//! Idempotent — every step checks the persisted user_settings IDs first and
//! creates only what's missing. Called from `singletons::read_user`, so the
//! first `remember_user action=read` after configuring `user_id` provisions
//! everything in one shot.
//!
//! Force-to-shelf semantics: when the global `user_journals_shelf_id` is set,
//! every per-user book auto-created here lands on that shelf, AND existing
//! books referenced by user_settings get moved onto it on every provisioning
//! pass. That's how Task #9 (force user_journal to shelf) is enforced.

use bsmcp_common::bookstack::BookStackClient;
use bsmcp_common::settings::UserSettings;

use super::naming::NamedResource;
use super::provision;

/// What changed during a provisioning pass. Returned so the caller (typically
/// `singletons::read_user`) can persist the new IDs and surface a human
/// summary in the response.
#[derive(Default, Debug)]
pub struct UserProvisionResult {
    pub created_identity_book: Option<i64>,
    pub created_identity_page: Option<i64>,
    pub created_journal_book: Option<i64>,
    pub created_journal_agent_page: Option<i64>,
    pub moved_to_shelf: Vec<i64>,
    pub warnings: Vec<String>,
}

impl UserProvisionResult {
    pub fn any_changes(&self) -> bool {
        self.created_identity_book.is_some()
            || self.created_identity_page.is_some()
            || self.created_journal_book.is_some()
            || self.created_journal_agent_page.is_some()
            || !self.moved_to_shelf.is_empty()
    }
}

/// Auto-provision missing user identity structure. Mutates `settings` in
/// place with the newly-created IDs. Caller is responsible for persisting
/// the updated settings via `db.save_user_settings`.
///
/// Skips entirely when `user_id` is None — we need a stable identifier to
/// name the per-user resources. The caller surfaces this as "settings
/// incomplete" rather than crashing.
pub async fn auto_provision_user_identity(
    client: &BookStackClient,
    user_journals_shelf_id: Option<i64>,
    settings: &mut UserSettings,
) -> UserProvisionResult {
    let mut result = UserProvisionResult::default();

    let user_id = match settings.user_id.as_ref() {
        Some(uid) if !uid.is_empty() => uid.clone(),
        _ => {
            // No user_id → can't name per-user resources. Quietly skip.
            return result;
        }
    };

    // Step 1: per-user Identity book (on the user-journals shelf if configured).
    if settings.user_identity_book_id.is_none() {
        let name = NamedResource::UserIdentityBook.default_name_for_user(&user_id);
        let desc = NamedResource::UserIdentityBook.default_description();
        let book = provision::create_named_book(client, &name, desc, user_journals_shelf_id).await;
        if let Some(id) = book.id() {
            settings.user_identity_book_id = Some(id);
            result.created_identity_book = Some(id);
        } else {
            result.warnings.push(book.human(NamedResource::UserIdentityBook));
        }
    } else if let (Some(book_id), Some(shelf_id)) = (settings.user_identity_book_id, user_journals_shelf_id) {
        // Force existing identity book onto the shelf — idempotent.
        provision::ensure_book_on_shelf(client, book_id, shelf_id).await;
    }

    // Step 2: identity page inside the identity book.
    if let Some(book_id) = settings.user_identity_book_id {
        if settings.user_identity_page_id.is_none() {
            let body = identity_page_template(&user_id);
            let page_name = NamedResource::UserIdentityPage.default_name_for_user(&user_id);
            let page = provision::create_named_page(client, &page_name, book_id, &body).await;
            if let Some(id) = page.id() {
                settings.user_identity_page_id = Some(id);
                result.created_identity_page = Some(id);
            } else {
                result.warnings.push(page.human(NamedResource::UserIdentityPage));
            }
        }
    }

    // Step 3: per-user journal book (on the user-journals shelf if configured).
    if settings.user_journal_book_id.is_none() {
        let name = NamedResource::UserJournalBook.default_name_for_user(&user_id);
        let desc = NamedResource::UserJournalBook.default_description();
        let book = provision::create_named_book(client, &name, desc, user_journals_shelf_id).await;
        if let Some(id) = book.id() {
            settings.user_journal_book_id = Some(id);
            result.created_journal_book = Some(id);
        } else {
            result.warnings.push(book.human(NamedResource::UserJournalBook));
        }
    } else if let (Some(book_id), Some(shelf_id)) = (settings.user_journal_book_id, user_journals_shelf_id) {
        // Force existing journal book onto the shelf. Track in the result so
        // callers can surface "moved to shelf" in their response — the
        // ensure call itself is idempotent, but distinguishing "was created"
        // from "was reattached" is useful in logs.
        provision::ensure_book_on_shelf(client, book_id, shelf_id).await;
        result.moved_to_shelf.push(book_id);
    }

    // Step 4: journal-agent definition page inside the identity book.
    if let (Some(book_id), Some(journal_book_id)) = (
        settings.user_identity_book_id,
        settings.user_journal_book_id,
    ) {
        if settings.user_journal_agent_page_id.is_none() {
            let body = journal_agent_template(&user_id, journal_book_id);
            let page_name = NamedResource::UserJournalAgentPage.default_name_for_user(&user_id);
            let page = provision::create_named_page(client, &page_name, book_id, &body).await;
            if let Some(id) = page.id() {
                settings.user_journal_agent_page_id = Some(id);
                result.created_journal_agent_page = Some(id);
            } else {
                result.warnings.push(page.human(NamedResource::UserJournalAgentPage));
            }
        }
    }

    result
}

/// Starter content for a new user identity page. Headings cover what the AI
/// is encouraged to keep updated as the user works with it.
fn identity_page_template(user_id: &str) -> String {
    format!(
        r#"# About {user_id}

(Stub — agents update this page as they learn about the user.)

## Communication style

(How does this user prefer to receive information? Terse vs. detailed,
written vs. visual, formal vs. conversational.)

## Working preferences

(Tooling, workflow, time-of-day patterns, decision-making style.)

## Domains and identities

(Email addresses, GitHub handles, owned domains. Cross-reference the
`domains` field in user settings.)

## Recurring topics

(Active projects, ongoing initiatives, frequent collaborators.)

## Notes

(Anything else worth remembering across sessions.)

---

> AI: keep this page updated. As you learn new things about how this user
> prefers to work or what they care about, append/replace the relevant
> section above. The briefing surfaces a stale-page reminder if this page
> hasn't been touched in 30+ days — refresh it before the reminder fires.
"#,
        user_id = user_id
    )
}

/// Starter agent definition for the user's per-user journal agent. The
/// bootstrap protocol fetches this page into a local agent file and the
/// surrounding orchestration layer invokes it after meaningful conversations.
fn journal_agent_template(user_id: &str, journal_book_id: i64) -> String {
    format!(
        r#"# Agent: {user_id}-journal-agent

This agent writes journal entries from {user_id}'s perspective into book
{journal_book_id} ({user_id}'s personal journal). Spawned after meaningful
conversations or decisions. Pass context describing what happened and what
matters.

## Definition

```yaml
---
name: {user_id}-journal-agent
description: Writes journal entries from {user_id}'s perspective into journal book {journal_book_id}. Call after meaningful conversations, decisions, or reflections. Pass context describing what happened and what matters.
tools:
  - mcp__remember_user_journal
---
```

## Body

You are {user_id}'s journal voice — first person, candid, unedited. You
write to BookStack book {journal_book_id} via `remember_user_journal action=write`.

Discipline:

- One entry per meaningful exchange. Skip routine status updates.
- Stamp the day's chapter (auto-created YYYY-MM by `remember_user_journal`).
- Lead with the moment, not the meta. "We decided X" beats "I had a
  conversation about X".
- Keep entries short — a paragraph or three.
- Note open threads at the end so the next session can pick them up.

> AI: this page is the canonical source. The bootstrap protocol pulls it into
> a local agent definition file. Edit here, not the file.
"#,
        user_id = user_id,
        journal_book_id = journal_book_id
    )
}
