# Development

How to build, test, and ship changes to `bookstack-mcp`. This document is the contributor entry point — README is the user-facing project overview, this file is for engineers.

## Prerequisites

- Rust toolchain (stable)
- Docker + Docker Buildx (for multi-arch builds)
- A BookStack instance with API token

## Local Build

```bash
# Full workspace
cargo build --release

# Individual crates
cargo build --release -p bsmcp-server
cargo build --release -p bsmcp-embedder
cargo build --release -p bsmcp-worker

# Check without building
cargo check
```

## Running Locally

Copy `.env.example` to `.env` and configure. At minimum:

```
BSMCP_BOOKSTACK_URL=https://your-bookstack.example.com
BSMCP_ENCRYPTION_KEY=your-32-char-key-here
```

Then:

```bash
cargo run -p bsmcp-server
cargo run -p bsmcp-embedder  # optional, for semantic search
cargo run -p bsmcp-worker    # optional, for the reconciliation index
```

## Docker Compose

Two deployment options:

```bash
# PostgreSQL backend (recommended for production)
docker compose -f docker/docker-compose.yml up -d

# SQLite backend (simpler, single-node)
docker compose -f docker/docker-compose.sqlite.yml up -d
```

## Branching

The canonical reference for the org-wide branching standard is the [Branching Strategy](https://kb.beesroadhouse.com/books/developer-operations-devops/page/branching-strategy) page in the Bee's Roadhouse DevOps book. This file mirrors the policy that applies to this repo specifically.

- `development` — default branch; all active work lands here. **PR required** (org-level `Default Branch Protection` ruleset: 1 approving review, thread resolution, no force-push, no deletion). PR merges trigger CI build/package.
- `release` — stable/production; merged from development when ready to ship. PR required (org-level `Release Branch Protection` ruleset: 1 approving review, merge-commit only, no force-push, no deletion).
- Work branches use the four-prefix taxonomy below.

No `main` or `master` branches exist.

### Work branch prefixes

| Prefix | Use for | GitHub labels | Default semver bump | Example |
|--------|---------|---------------|---------------------|---------|
| `feature/{name}` | New capability that didn't exist | `type:enhancement` + `category:feature` | minor | `feature/export-api` |
| `improvement/{name}` | Existing capability, done better | `type:enhancement` + `category:improvement` | minor | `improvement/search-relevance` |
| `refactor/{name}` | Design or structure redo | `type:problem` + `category:refactor` | patch (or minor if external behavior changes) | `refactor/auth-flow` |
| `bug/{name}` | Implementation mistake, something broken | `type:problem` + `category:bug` | patch | `bug/oauth-token-refresh` |

Breaking changes are orthogonal to type — prefix the **PR title** with `BREAKING:` regardless of the branch prefix to force a major-version bump.

### Workflow

```
1. git checkout development && git pull
2. git checkout -b improvement/my-change      # or feature/, refactor/, bug/
3. ... commit work (signed via SSH; see Commit Signing below) ...
4. scripts/publish-pr-image.sh                # build + push multi-arch images to GHCR
5. git push -u origin improvement/my-change
6. Open PR against development; apply the matching type: + category: labels
7. CI verifies your images and regenerates SBOM/STRUCTURE (see CI/CD below)
8. Squash-merge PR into development; delete the work branch
9. When ready to ship: open PR from development -> release
```

All changes go through a PR — direct pushes are blocked by the org ruleset (returns `GH013`). For CI emergencies (workflow-bootstrap gap, broken build), use `workflow_dispatch` on `direct-push.yml` rather than bypassing the ruleset. Org admins can `gh pr merge --admin` for bootstrap PRs and small docs touchups, but `--admin` still routes through the PR machinery (CI runs, audit trail preserved) — it is not a direct push.

## CI/CD

The org-canonical PR-builds + post-merge-retag pattern. Reference docs:

- BR DevOps [Docker Image Build Workflows (1905)](https://kb.beesroadhouse.com/books/developer-operations-devops/page/docker-image-build-workflows) — canonical trigger / tag / cache shape.
- BR DevOps [Branching Strategy (1860)](https://kb.beesroadhouse.com/books/developer-operations-devops/page/branching-strategy) — branch model and direct-push authorization.

**CI builds. Squash-merge retags.** Heavy multi-arch Docker builds run in CI on every push to a PR. The PR's image is published under a single rolling tag `{version}-{slug}` that moves with each push. After squash-merge, a separate workflow retags that PR-head image as the stream tags via `docker buildx imagetools create` — pure manifest operation, no rebuild. The squash-merge commit's source tree is bit-identical to the PR head, so the image is the right artifact. Commit-level pinning during PR review is via `image@sha256:digest` from `docker buildx imagetools inspect`, not a per-commit tag (see *Tag conventions on GHCR* below for why).

### Contributor flow (per PR)

```
1. git checkout -b improvement/my-change
2. ... commit work, sign each commit ...
3. git push -u origin improvement/my-change
4. Open PR; build-server / build-embedder / build-worker run on every PR push
5. Squash-merge into development; delete the work branch
6. promote-on-merge.yml retags the PR head image as :dev / :{version}-dev
```

No local image build needed. `scripts/publish-pr-image.sh` is still in the repo as an out-of-band escape hatch for emergency hotfixes when CI is unavailable, but it's not part of the normal flow.

### Path-aware fast path (CI)

`build-pr.yml` uses [`dorny/paths-filter@v3`](https://github.com/dorny/paths-filter) to detect which binaries' build deps were touched by the PR. When nothing relevant changed for a given binary, the job retags `:dev` (the latest published stream image) as the per-PR tags instead of doing the 15-min cross-arch build. ~2s vs ~15min.

Falls back to a full build automatically when `:dev` doesn't exist (cold start or pre-first-release).

What counts as "changed paths" per binary:

| Binary | Paths that trigger a rebuild |
|---|---|
| `bsmcp-server` | `crates/bsmcp-server/`, `crates/bsmcp-common/`, `crates/bsmcp-db-sqlite/`, `crates/bsmcp-db-postgres/`, `Cargo.toml`, `Cargo.lock`, `docker/Dockerfile.server`, `entrypoint.sh` |
| `bsmcp-embedder` | `crates/bsmcp-embedder/`, `crates/bsmcp-common/`, `crates/bsmcp-db-sqlite/`, `crates/bsmcp-db-postgres/`, `Cargo.toml`, `Cargo.lock`, `docker/Dockerfile.embedder`, `entrypoint.sh` |
| `bsmcp-worker` | `crates/bsmcp-worker/`, `crates/bsmcp-common/`, `crates/bsmcp-db-sqlite/`, `crates/bsmcp-db-postgres/`, `Cargo.toml`, `Cargo.lock`, `docker/Dockerfile.worker`, `entrypoint.sh` |

The same path-set is mirrored in `scripts/publish-pr-image.sh` (`SERVER_PATHS` / `EMBEDDER_PATHS` / `WORKER_PATHS`). Keep them in sync — the script is a manual fallback for the same logic.

### Cargo target / registry caching

Both Dockerfiles use BuildKit `--mount=type=cache` for `target/`, `~/.cargo/registry`, and `~/.cargo/git`. CI uses scoped GHA cache (`scope=server`, `scope=embedder`, `scope=worker`) so parallel jobs don't evict each other's layers. Cache mount IDs include `$TARGETPLATFORM` so linux/amd64 and linux/arm64 don't poison each other's caches.

### Embedder is opt-in for deployments

`bsmcp-embedder` is required only when running the **built-in** embedder provider (the default `BSMCP_EMBED_PROVIDER=local` ONNX model). Deployments configured for external providers (`ollama`, `openai`) don't need the embedder container at all — `bsmcp-server` talks to the external endpoint directly.

### What runs on what

| Event | Workflow | What happens |
|-------|----------|-------------|
| Push to a work branch with **no open PR** | nothing | test locally |
| `pull_request: opened/synchronize/reopened` against `development` or `release` | `build-pr.yml` (`build-server`, `build-embedder`, `build-worker`) | path-aware multi-arch build per image; tag `{version}-{slug}` (rolling per-PR) |
| Same trigger | `generate-artifacts.yml` | regenerates `SBOM.md` + `STRUCTURE.md`, commits to PR source branch (re-fire loop broken by `paths-ignore`, not `[skip ci]`, so squash-merge bodies stay clean) |
| `pull_request: closed` (merged: true) on `development` or `release` | `promote-on-merge.yml` | retags the PR head image as the appropriate stream tags via `imagetools create`. No rebuild. |
| `push` to `development` that is **not** a PR-merge commit | `direct-push.yml` (`workflow_dispatch` only) | manual recovery — full multi-arch build + stream tags. The ruleset blocks ad-hoc direct pushes; this workflow runs on demand for CI emergencies (e.g. workflow-bootstrap gap). |
| `workflow_dispatch` on `direct-push.yml` | `direct-push.yml` | manual recovery for the development stream — bypasses the PR-merge guard and rebuilds the four `:dev*` tags at the current `development` HEAD. Use after a workflow-bootstrap gap (workflow file introduced by the very PR whose merge would have run it). |
| `push` to `release` (always a PR-merge from development) | `release.yml` (`github-release-on-merge` + `release-binaries-on-merge`) | creates the `v{version}` git tag (if missing) and the GitHub Release entry; builds `bsmcp-server` native binaries for 5 targets and attaches them. Image version tags were already moved by `promote-on-merge.yml` when the PR closed. |
| `v*` tag push (emergency hotfix only) | `release.yml` (`tag-release` + `github-release-on-tag` + `release-binaries-on-tag`) | builds & pushes semver-tagged images directly in CI, creates the Release, attaches the server binaries. Use only when the normal PR flow isn't available. |
| `workflow_dispatch` on `release.yml` | `release.yml` | manual recovery path for the release stream |

### Why this shape

- **CI builds, not contributor.** Build work runs in CI on every PR push. Engineers don't need a local multi-arch builder or a GHCR PAT to get their PR through review. Cost: ~15 min of CI minutes per PR push (mitigated by the path-aware fast path for docs/config-only PRs, which retag in ~2s).
- **Squash-merge retags, doesn't rebuild.** The squash-merge commit's source tree is bit-identical to the PR head, so the PR head image is the right artifact. `promote-on-merge.yml` moves the rolling tags atomically; the registry handles the cleanup of the old manifest.
- **`direct-push.yml` is the CI-emergency escape hatch, not an authorized push path.** Page 1860 (BR Branching Strategy) blocks ad-hoc direct pushes via the org ruleset. `direct-push.yml` runs on `workflow_dispatch` for manual recovery (workflow-bootstrap gap, broken CI) and is gated to skip PR-merge commits so `promote-on-merge.yml` owns those.
- **Native binaries: server only.** `bsmcp-server` is pure Rust + bundled SQLite and cross-compiles cleanly. `bsmcp-embedder` depends on `fastembed` → ONNX Runtime → a per-platform C++ shared library; bare binaries would need ONNX Runtime installed on the host. Container is the only supported distribution for the embedder.
- **External fork PRs are skipped.** Forks cannot push to `ghcr.io/bees-roadhouse/*`. `build-pr.yml`, `promote-on-merge.yml`, and `generate-artifacts.yml` all gate on `head.repo.full_name == github.repository`.

### Tag conventions on GHCR

Per-PR (pushed by `build-pr.yml` on every PR commit):
- `{version}-{branch-slug}` — rolling, moves with each PR push

No per-commit immutable tag. Commit-level pinning is via `image@sha256:digest` from `docker buildx imagetools inspect`. Two reasons: (1) digest pinning is a strict superset of tag pinning, and (2) `generate-artifacts.yml` lands an SBOM/STRUCTURE auto-commit AFTER `build-pr.yml` runs — PR head moves to a SHA the gating build never built, so a per-PR `{sha}` immutable tag would be a footgun for `promote-on-merge.yml` (it'd look for a tag that doesn't exist). The rolling tag captures the most recent successful build regardless of head drift.

Development stream (set by `promote-on-merge.yml` on PR close, or `direct-push.yml` via `workflow_dispatch`):
- `dev` — rolling, latest dev build
- `dev-{merge_sha}` — immutable per-merge / per-push
- `{version}-dev` — version-level dev rolling
- `{version}-dev-{merge_sha}` — version-level dev immutable

Release stream (set by `promote-on-merge.yml` on `development → release` PR close):
- `latest` — rolling, latest release
- `release` — alias for `latest`
- `{version}` — pinned semver (e.g., `0.10.0`)
- `{version}-{merge_sha}` — immutable per-release-merge

Tag-push hotfix (`v*` tag → `release.yml` `tag-release`):
- `{version}`, `{major}.{minor}`, `{major}` — full semver hierarchy

Images are published to `ghcr.io/bees-roadhouse/bsmcp-server`, `ghcr.io/bees-roadhouse/bsmcp-embedder`, and `ghcr.io/bees-roadhouse/bsmcp-worker` for `linux/amd64` and `linux/arm64`.

### Native binary release artifacts

Each GitHub Release attaches `bsmcp-server` archives for these targets:

| Target | Archive | Runner |
|--------|---------|--------|
| `x86_64-unknown-linux-gnu` | `.tar.gz` | ubuntu-22.04 (glibc ≥ 2.35) |
| `aarch64-unknown-linux-gnu` | `.tar.gz` | ubuntu-22.04 + cross-linker |
| `x86_64-apple-darwin` | `.tar.gz` | macos-13 |
| `aarch64-apple-darwin` | `.tar.gz` | macos-14 |
| `x86_64-pc-windows-msvc` | `.zip` | windows-2022 |

Each archive contains the `bsmcp-server` (or `.exe`) binary plus `README.md` and `LICENSE`.

### Branch protection

Protection lives at the **organization level** via two GitHub Rulesets that apply to every repo in `bees-roadhouse`:

- `Default Branch Protection` (`~DEFAULT_BRANCH`) — `pull_request` (1 approval, thread resolution), `non_fast_forward`, `deletion`. Bypass: `OrganizationAdmin` in `pull_request` mode.
- `Release Branch Protection` (`refs/heads/release`, `refs/heads/release/*`, `refs/heads/release-*`) — `pull_request` (1 approval, merge-commit only, thread resolution), `non_fast_forward`, `deletion`. Bypass: `OrganizationAdmin` in `pull_request` mode.

Both rulesets enforce on every ref update on the targeted branches — direct pushes are rejected with `GH013`. CI runs on every PR push, so regressions are caught before merge. The `OrganizationAdmin` bypass uses `bypass_mode: pull_request` (skip review on a PR via `gh pr merge --admin`), not `repository` (which would allow direct push) — direct push is intentionally not configured.

Required status checks for `build-server` / `build-embedder` / `build-worker` are **not** wired up yet. After this CI rework lands and the new check names stabilize, a follow-up will add them to both rulesets.

### Commit signing

Every commit must be signed via SSH using 1Password's SSH agent. See the [Commit Signing](https://kb.beesroadhouse.com/books/developer-operations-devops/page/commit-signing) page in the DevOps book for full configuration.

## Versioning

Semantic versioning (`MAJOR.MINOR.PATCH`). Version lives in workspace `Cargo.toml`.

Default semver bump per branch prefix (override with `BREAKING:` in the PR title for a major bump):

- `feature/*` — minor
- `improvement/*` — minor
- `refactor/*` — patch (minor if external behavior changes)
- `bug/*` — patch

Release: open PR from development -> release. The release-merge promote job retags with the current `{version}` and creates the GitHub Release.

## Testing

```bash
cargo test
cargo clippy
```

## Adding a New Tool

1. Add API method to `BookStackClient` in `crates/bsmcp-server/src/bookstack.rs`
2. Add match arm in `execute_tool()` in `crates/bsmcp-server/src/mcp.rs`
3. Add tool definition in `tool_definitions()` in the same file
4. Use existing helpers: `arg_str`, `arg_i64`, `arg_i64_required`, `arg_str_default`, `format_json`

## Migration

**SQLite -> PostgreSQL auto-migration:** When `BSMCP_DB_BACKEND=postgres` and a SQLite DB exists at `BSMCP_DB_PATH`, the server auto-migrates on startup and renames the file to `.db.migrated`.

**Manual migration:**

```bash
bsmcp-server migrate --from-sqlite /path/to/db --to-postgres postgres://user:pass@host/db
```

Migrates `access_tokens`, `pages`, `chunks` (BLOB -> pgvector), `relationships`, `embed_jobs`. Validates row counts.

## Multi-arch Docker builds (manual)

Normally CI handles this. For local multi-arch testing:

```bash
docker buildx build --builder multiarch --platform linux/amd64,linux/arm64 \
  -f docker/Dockerfile.server \
  -t ghcr.io/bees-roadhouse/bsmcp-server:VERSION --push .

docker buildx build --builder multiarch --platform linux/amd64,linux/arm64 \
  -f docker/Dockerfile.embedder \
  -t ghcr.io/bees-roadhouse/bsmcp-embedder:VERSION --push .

docker buildx build --builder multiarch --platform linux/amd64,linux/arm64 \
  -f docker/Dockerfile.worker \
  -t ghcr.io/bees-roadhouse/bsmcp-worker:VERSION --push .
```
