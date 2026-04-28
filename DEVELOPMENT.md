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
cargo run -p bsmcp-worker    # optional, for v1.0.0 reconciliation index
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

- `development` — default branch; all active work lands here. Direct pushes are **authorized** (small touchups, scaffolding, emergency hotfixes). Pushes trigger CI build/package.
- `release` — stable/production; merged from development when ready to ship. The **only** protected branch (org-level ruleset blocks force-push and deletion; PR required).
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

Direct pushes to `development` stay available — use them for small atomic changes, scaffolding, or emergency hotfixes. The PR flow is the team norm for anything else.

## CI/CD

**Contributor-uploads-image.** Heavy multi-arch Docker builds run on the **contributor's machine**, not in CI. Before pushing a PR commit, run `scripts/publish-pr-image.sh` to build and push the per-PR images to GHCR. CI then runs lightweight verification jobs that confirm the images exist and are multi-arch — that's the merge gate. The SBOM/STRUCTURE auto-commit still runs on every PR commit (it's quick).

This trades CI minutes for a small contributor onboarding step. The gate is preserved (PRs cannot merge without verified images); the build cost moves to the engineer who actually edited the source.

### Contributor flow (per PR)

```
1. git checkout -b improvement/my-change
2. ... commit work ...
3. scripts/publish-pr-image.sh        # build + push multi-arch images to GHCR
4. git push -u origin improvement/my-change
5. Open PR; build-server / build-embedder / build-worker verify checks should pass
6. On every subsequent commit, re-run scripts/publish-pr-image.sh before pushing
7. Squash-merge into development; delete the work branch
```

If you push the commit before pushing the image, the verify check fails with a clear error pointing at the script. Run it, then re-trigger the check (push an empty commit or click "Re-run jobs" in GitHub Actions).

### Path-aware fast path (`scripts/publish-pr-image.sh`)

The publish script diffs your branch against `origin/development` and skips the rebuild for any binary whose dependency files didn't change. It retags the latest published `:dev` image as the per-PR tags instead — a manifest-only operation that takes seconds.

What counts as "changed paths":

| Binary | Paths that trigger a rebuild |
|---|---|
| `bsmcp-server` | `crates/bsmcp-server/`, `crates/bsmcp-common/`, `crates/bsmcp-db-sqlite/`, `crates/bsmcp-db-postgres/`, `Cargo.toml`, `Cargo.lock`, `docker/Dockerfile.server`, `entrypoint.sh` |
| `bsmcp-embedder` | `crates/bsmcp-embedder/`, `crates/bsmcp-common/`, `crates/bsmcp-db-sqlite/`, `crates/bsmcp-db-postgres/`, `Cargo.toml`, `Cargo.lock`, `docker/Dockerfile.embedder`, `entrypoint.sh` |
| `bsmcp-worker` | `crates/bsmcp-worker/`, `crates/bsmcp-common/`, `crates/bsmcp-db-sqlite/`, `crates/bsmcp-db-postgres/`, `Cargo.toml`, `Cargo.lock`, `docker/Dockerfile.worker`, `entrypoint.sh` |

Note that PRs touching `crates/bsmcp-server/` only — like most v1.0.0 phase work — skip the embedder rebuild entirely. That's the change that takes a typical PR from ~25 min of multi-arch build time down to ~10 min.

Override with `scripts/publish-pr-image.sh both --force` if you need to force a full rebuild (e.g., to validate a Dockerfile change that the path filter would otherwise miss).

**Direct push to `development` or `release` always forces a rebuild** regardless of path filter results. The path-aware retag-from-`:dev` shortcut is correct for PR work (the contributor's per-PR image already encodes the PR head's tree, which is bit-identical to the squash-merge commit). But for a direct push to a canonical branch, retagging an older image's manifest would leave `org.opencontainers.image.revision` pointing at the older commit, which drifts from the SHA actually being pushed. The script auto-forces the build in this case so the resulting image's revision label matches the push SHA.

### Cargo target / registry caching

Both Dockerfiles use BuildKit `--mount=type=cache` for `target/`, `~/.cargo/registry`, and `~/.cargo/git`. The first build is still cold (~15 min on linux/arm64 via QEMU), but subsequent builds reuse the dep-tree compilation across PR pushes. Cache mount IDs include `$TARGETPLATFORM` so linux/amd64 and linux/arm64 don't poison each other's caches.

### Embedder is opt-in for deployments

`bsmcp-embedder` is required only when running the **built-in** embedder provider (the default `BSMCP_EMBED_PROVIDER=local` ONNX model). Deployments configured for external providers (`ollama`, `openai`) don't need the embedder container at all — `bsmcp-server` talks to the external endpoint directly.

**One-time setup:**

```bash
# Multi-platform builder
docker buildx create --name multiarch --use --bootstrap

# GHCR login (PAT needs write:packages scope)
echo $GHCR_PAT | docker login ghcr.io -u <gh-user> --password-stdin
```

### What runs on what

| Event | Workflow | What happens |
|-------|----------|-------------|
| Push to a work branch with **no open PR** | nothing | test locally |
| `pull_request: opened/synchronize/reopened` against `development` or `release` | `release.yml` (`build-server`, `build-embedder`, `build-worker` verify jobs) | confirms the contributor's per-PR images exist on GHCR with both `linux/amd64` and `linux/arm64`. ~30 seconds. No build. |
| Same trigger | `generate-artifacts.yml` | regenerates `SBOM.md` + `STRUCTURE.md`, commits to PR source branch with `[skip ci]` |
| `push` to `development` (squash-merge or direct push) | `release.yml` (`push-retag`) | retags the contributor's per-PR image to `:dev`, `:dev-{push_sha}`, `:{version}-dev`, `:{version}-dev-{push_sha}`. No rebuild. |
| `push` to `release` (squash-merge or direct push) | `release.yml` (`push-retag` + `github-release-on-merge` + `release-binaries-on-merge`) | retags to `:{version}`, `:{version}-{push_sha}`, `:release`, `:latest`; creates GitHub Release; builds `bsmcp-server` native binaries for 5 targets and attaches them to the Release. |
| `v*` tag push (emergency hotfix only) | `release.yml` (`tag-release` + `github-release-on-tag` + `release-binaries-on-tag`) | builds & pushes semver-tagged images directly in CI (the only build path that still runs in CI), creates the Release, attaches the server binaries. Use only when the contributor cannot push images themselves. |
| `workflow_dispatch` on `release.yml`, ref = `development` or `release` | `release.yml` (`push-retag`) | manual recovery path when a `push` event drops (GitHub Actions occasionally fails to fire workflow runs for squash-merge commits). Run via `gh workflow run "Build and Release" --ref development`. |

### Why this shape

- **CI verifies, contributor builds.** The merge gate is preserved: a PR cannot merge unless its per-PR images exist on GHCR. The actual build work moves out of CI onto the engineer's machine, where it amortizes over the local build cache and saves ~15 min of CI minutes per PR push.
- **Stable job names (`build-server`, `build-embedder`, `build-worker`).** Branch protection's required status checks reference these names; renaming them would silently disable the gate until the rule is updated. The job names lie a little — they verify, not build — but the trade-off is worth it.
- **Retag instead of rebuild on merge.** A squash-merge to development produces a new commit SHA, but its source tree is identical to the PR head. The contributor's image is bit-identical to what a CI build would produce, so promote just moves the rolling tag.
- **Native binaries: server only.** `bsmcp-server` is pure Rust + bundled SQLite and cross-compiles cleanly. `bsmcp-embedder` depends on `fastembed` → ONNX Runtime → a per-platform C++ shared library; bare binaries would need ONNX Runtime installed on the host. Container is the only supported distribution for the embedder.
- **External fork PRs are not supported under this model.** Forks cannot push to `ghcr.io/bees-roadhouse/*`, so fork PRs cannot satisfy the verify gate. If outside contribution becomes a real workflow, a maintainer will need to manually build/push the contributor's branch (or use the emergency `v*` tag path).

### Tag conventions on GHCR

Per-PR (pushed by the contributor via `scripts/publish-pr-image.sh`):
- `{version}-{branch-slug}-{sha}` — pinnable to one specific commit
- `{version}-{branch-slug}` — rolling, moves with each PR push

Development stream (after merge to development):
- `dev` — rolling, latest dev build
- `{version}-dev` — version-level dev tag

Release stream (after merge to release or `v*` tag):
- `latest` — rolling, latest release
- `release` — alias for `latest`
- `{version}` — pinned semver (e.g., `0.7.4`)
- `{major}.{minor}`, `{major}` — broader semver pointers (only emitted on `v*` tag push)

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

Protection lives at the **organization level** via a GitHub Ruleset (`Release Branch Protection`) targeting `refs/heads/release` on every repo in `bees-roadhouse`:

- `pull_request` (0 required approvals — the gate is CI status, not approver count)
- `non_fast_forward` (blocks force-push)
- `deletion` (blocks branch delete)

`development` is **intentionally unprotected** — direct pushes stay authorized so scaffolding, hotfixes, and small atomic changes don't get stuck in PR ceremony. CI runs on every push regardless, so regressions are still caught.

Required status checks (`build-server`, `build-embedder`) gate the merge into `release` once the PR is open. They now verify rather than build, but the contract from the protection rule's perspective is unchanged.

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
```
