# Repository Structure

_Auto-generated on 2026-04-28 03:57 UTC from commit `90050c9`._

```
.
├── crates
│   ├── bsmcp-common
│   │   ├── src
│   │   │   ├── acl.rs
│   │   │   ├── bookstack.rs
│   │   │   ├── chunking.rs
│   │   │   ├── config.rs
│   │   │   ├── db.rs
│   │   │   ├── index.rs
│   │   │   ├── lib.rs
│   │   │   ├── settings.rs
│   │   │   ├── types.rs
│   │   │   └── vector.rs
│   │   └── Cargo.toml
│   ├── bsmcp-db-postgres
│   │   ├── src
│   │   │   └── lib.rs
│   │   └── Cargo.toml
│   ├── bsmcp-db-sqlite
│   │   ├── src
│   │   │   └── lib.rs
│   │   └── Cargo.toml
│   ├── bsmcp-embedder
│   │   ├── src
│   │   │   ├── embed.rs
│   │   │   ├── main.rs
│   │   │   └── pipeline.rs
│   │   └── Cargo.toml
│   └── bsmcp-server
│       ├── src
│       │   ├── remember
│       │   ├── index_worker.rs
│       │   ├── llm.rs
│       │   ├── main.rs
│       │   ├── mcp.rs
│       │   ├── migrate.rs
│       │   ├── oauth.rs
│       │   ├── semantic.rs
│       │   ├── settings_ui.rs
│       │   ├── sse.rs
│       │   ├── staging.rs
│       │   └── summary.rs
│       └── Cargo.toml
├── docker
│   ├── Dockerfile.embedder
│   ├── Dockerfile.server
│   ├── docker-compose.sqlite.yml
│   └── docker-compose.yml
├── scripts
│   └── publish-pr-image.sh
├── CLAUDE.md
├── Cargo.lock
├── Cargo.toml
├── DEVELOPMENT.md
├── README.md
├── RFC-identity-book-restructure.md
├── SBOM.md
├── STRUCTURE.md
└── entrypoint.sh

15 directories, 45 files
```
