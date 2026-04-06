# Development

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

- `development` ... default branch, active work lands here
- `release` ... stable/production, merged from development when ready
- `enhancement/{name}` ... new functionality, branched from development
- `problem/{name}` ... bug fixes, branched from development

### Workflow

```
1. git checkout development
2. git checkout -b enhancement/my-feature   (or problem/my-fix)
3. ... commit work ...
4. git push -u origin enhancement/my-feature
5. Merge to development (PR optional for solo work)
6. Delete work branch
7. When ready for production: merge development into release
```

## CI/CD

GitHub Actions builds Docker images on every push to `development` and `release`:

- Push to `development` ... tags image as `dev` and `VERSION-dev.SHA` (e.g. `0.7.0-dev.abc1234`)
- Push to `release` ... tags image as `latest` and `release`
- Push `v*` tag ... adds immutable semver tags (`x.y.z`, `x.y`, `x`)

Images are published to `ghcr.io/bees-roadhouse/bsmcp-server` and `ghcr.io/bees-roadhouse/bsmcp-embedder` for `linux/amd64` and `linux/arm64`.

## Versioning

Semantic versioning (`MAJOR.MINOR.PATCH`). Version lives in workspace `Cargo.toml`.

- `enhancement/*` merge ... minor bump
- `problem/*` merge ... patch bump
- Tag on `release` branch after merging: `git tag v0.7.0 && git push origin --tags`

## Testing

```bash
cargo test
cargo clippy
```

## Adding a New Tool

1. Add API method to `BookStackClient` in `crates/bsmcp-common/src/bookstack.rs`
2. Add match arm in `execute_tool()` in `crates/bsmcp-server/src/mcp.rs`
3. Add tool definition in `tool_definitions()` in the same file
4. Use existing helpers: `arg_str`, `arg_i64`, `arg_i64_required`, `arg_str_default`, `format_json`
