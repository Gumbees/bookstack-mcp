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

- `development` — default branch; all PRs land here
- `release` — stable/production; merged from development when ready to ship
- `enhancement/{name}` — branched from development for new functionality
- `problem/{name}` — branched from development for bug fixes

No `main` or `master` branches exist.

### Workflow

```
1. git checkout development && git pull
2. git checkout -b enhancement/my-feature   (or problem/my-fix)
3. ... commit work ...
4. git push -u origin enhancement/my-feature
5. Open PR against development
6. CI builds artifact (see CI/CD below) — PR cannot merge until it succeeds
7. Squash-merge PR into development; delete the work branch
8. When ready to ship: open PR from development -> release
```

## CI/CD

**Artifact-before-merge.** Both the Docker images and the SBOM/STRUCTURE doc artifacts are generated on every push to the PR source branch — *before* the merge happens. The PR cannot merge until those builds succeed (enforced via branch protection on `development` and `release`).

This is the inversion of the legacy "build on push to development" model: artifacts are part of the gate, not a side effect of merging.

### What runs on what

| Event | Workflow | What happens |
|-------|----------|-------------|
| Push to a work branch with **no open PR** | nothing | test locally |
| `pull_request: opened/synchronize/reopened` against `development` or `release` | `release.yml` (build jobs) | builds & pushes images tagged `{version}-{branch-slug}-{sha}` (immutable per-commit) and `{version}-{branch-slug}` (rolling per-PR) |
| Same trigger | `generate-artifacts.yml` | regenerates `SBOM.md` + `STRUCTURE.md`, commits to PR source branch with `[skip ci]` |
| `pull_request: closed && merged: true`, base = `development` | `release.yml` (promote job) | retags `{version}-{branch-slug}` -> `dev` + `{version}-dev`. No rebuild. |
| `pull_request: closed && merged: true`, base = `release` | `release.yml` (promote + github-release-on-merge) | retags `{version}-{branch-slug}` -> `{version}` + `release` + `latest`; creates GitHub Release |
| `v*` tag push (emergency hotfix only) | `release.yml` (tag-release + github-release-on-tag) | builds & pushes semver-tagged images; creates GitHub Release. Prefer the PR-into-release flow above. |

### Why this shape

- **PR-source-branch push, not push-to-development.** A push to a work branch with an open PR is the explicit signal "this is ready for review". A push to development is a merge — it's too late to gate. We want the artifact built on the source so the merge is the no-op it should be.
- **Retag instead of rebuild on merge.** A squash-merge to development produces a new commit SHA, but its source tree is identical to the PR head. Building it again produces a bit-identical image, so we save the CI minutes and just move the rolling tag.
- **Per-PR rolling tag (`{version}-{branch-slug}`) survives auto-commits.** If `generate-artifacts.yml` appends a `[skip ci]` commit to the PR, the rolling tag still points at the engineer's last manual SHA. Promote uses the rolling tag, not the PR head SHA.
- **External fork PRs.** The build jobs run for fork PRs too (forks can open PRs even though they can't push to our repo). The artifact-generate job is skipped for forks because `GITHUB_TOKEN` can't push back to a fork's branch.

### Tag conventions on GHCR

Per-PR (transient, for PR review/testing):
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

Images are published to `ghcr.io/bees-roadhouse/bsmcp-server` and `ghcr.io/bees-roadhouse/bsmcp-embedder` for `linux/amd64` and `linux/arm64`.

### Branch protection

`development` and `release` require:
- PR (no direct push)
- Status checks `build-server` and `build-embedder` must pass before merge
- Squash merges only

The required checks are what enforce "artifact must exist before merge". Configure these in **Settings → Branches → Branch protection rules**.

## Versioning

Semantic versioning (`MAJOR.MINOR.PATCH`). Version lives in workspace `Cargo.toml`.

- `enhancement/*` merge to development -> bump minor in the PR
- `problem/*` merge to development -> bump patch in the PR
- Release: open PR from development -> release. The release-merge promote job retags with the current `{version}` and creates the GitHub Release.

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
