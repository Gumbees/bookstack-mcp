//! `/remember/v1/{resource}/{action}` — the personal-memory protocol.
//!
//! v1.0.0 reintroduces this namespace. v0.8.0 had collapsed everything onto a
//! single `briefing` tool / `POST /briefing/v1/read` endpoint after the
//! personal-memory layer was supposed to move out to memberberry.ai. That
//! split is being undone — `remember_*` endpoints are coming back into
//! bookstack-mcp with a smaller, opinionated surface.
//!
//! Resources shipped through sub-PR 2.2:
//!   - `briefing`  — thin wrapper over the existing briefing builder
//!   - `user`      — read/write the per-user `UserSettings` row
//!   - `config`    — read/write per-user `config_extras` + dismiss_setup_nudge
//!   - `directory` — serve the in-memory `DirectoryService` snapshot
//!
//! Added in sub-PR 2.4b:
//!   - `identity`  — read/write the user-identity page and per-agent
//!     AI-identity pages (one chapter+page per agent) inside the user's
//!     per-user Journal book. Bootstraps chapter + page on first read
//!     or first write when missing.
//!
//! Added in sub-PR 2.4c:
//!   - `journal`   — append-only structured journal entries (one daily
//!     page per (user|agent, day) pair, sectioned by time-of-write).
//!
//! Added in sub-PR 2.5:
//!   - `migrate`   — import legacy journal content (pre-v1.0.0 layout, or
//!     any other book on the User Journals shelf the user owns) into the
//!     new per-user-Journal-book layout. Three actions: `list_sources`,
//!     `plan` (DRY RUN), `execute`.
//!
//! Every handler returns the standard `{ok, data, meta, error}` envelope.

pub mod briefing;
pub mod config;
pub mod directory;
pub mod envelope;
pub mod identity;
pub mod journal;
pub mod migrate;
pub mod reminders;
pub mod resolvers;
pub mod user;

use std::sync::Arc;

use serde_json::Value;

use bsmcp_common::bookstack::BookStackClient;
use bsmcp_common::db::DbBackend;
use bsmcp_common::settings::{hash_token_id, UserSettings};

use crate::directory::DirectoryService;
use crate::semantic::SemanticState;

use envelope::{error_envelope, ErrorCode};

/// Dispatch a `/remember/v1/{resource}/{action}` call. Returns the JSON envelope.
///
/// `directory_service` is `Option` so HTTP and MCP entrypoints that don't yet
/// thread it through still compile during the cutover. The `directory`
/// resource returns `internal_error` if it's `None`.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch(
    resource: &str,
    action: &str,
    body: Value,
    token_id: &str,
    client: &BookStackClient,
    db: Arc<dyn DbBackend>,
    semantic: Option<Arc<SemanticState>>,
    directory_service: Option<Arc<DirectoryService>>,
) -> Value {
    let started = std::time::Instant::now();
    let trace_id = body
        .get("trace_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let token_id_hash = hash_token_id(token_id);

    let mut settings = match db.get_user_settings(&token_id_hash).await {
        Ok(Some(s)) => s,
        Ok(None) => UserSettings::default(),
        Err(e) => {
            eprintln!("Remember: failed to load settings: {e}");
            UserSettings::default()
        }
    };

    // Client-pushed timezone refresh — accepted on every remember endpoint
    // so the AI can keep the cache fresh from any call. Mirrors the
    // briefing's timezone handling exactly.
    let client_tz: Option<String> = body
        .get("client_timezone")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.parse::<chrono_tz::Tz>().is_ok());
    let mut tz_just_pushed = false;
    if let Some(ref tz) = client_tz {
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let needs_save = settings.timezone.as_deref() != Some(tz.as_str())
            || settings.timezone_fetched_at.unwrap_or(0)
                < now_unix - envelope::TIMEZONE_REFRESH_SECS;
        if needs_save {
            settings.timezone = Some(tz.clone());
            settings.timezone_fetched_at = Some(now_unix);
            if let Err(e) = db.save_user_settings(&token_id_hash, &settings).await {
                eprintln!("Remember: failed to persist client_timezone (non-fatal): {e}");
            }
        }
        tz_just_pushed = true;
    }

    let ctx = Context {
        body,
        client: client.clone(),
        db: db.clone(),
        semantic,
        directory: directory_service,
        settings,
        token_id: token_id.to_string(),
        token_id_hash,
    };

    let result = route(resource, action, &ctx).await;

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let globals = db.get_global_settings().await.unwrap_or_default();
    let meta = envelope::build_meta(
        &trace_id,
        elapsed_ms,
        &ctx.settings,
        &globals,
        Vec::new(),
        tz_just_pushed,
    );

    match result {
        Ok(data) => envelope::ok_envelope(data, meta),
        Err((code, message)) => error_envelope(code, message, meta),
    }
}

type DispatchResult = Result<Value, (ErrorCode, String)>;

async fn route(resource: &str, action: &str, ctx: &Context) -> DispatchResult {
    match (resource, action) {
        ("briefing", "read") => briefing::read(ctx).await,
        ("user", "read") => user::read(ctx).await,
        ("user", "write") => user::write(ctx).await,
        ("config", "read") => config::read(ctx).await,
        ("config", "write") => config::write(ctx).await,
        ("config", "dismiss_setup_nudge") => config::dismiss_setup_nudge(ctx).await,
        ("directory", "read") => directory::read(ctx).await,
        ("identity", "read") => identity::read(ctx).await,
        ("identity", "write") => identity::write(ctx).await,
        ("journal", "read") => journal::read(ctx).await,
        ("journal", "write") => journal::write(ctx).await,
        ("migrate", "list_sources") => migrate::list_sources(ctx).await,
        ("migrate", "plan") => migrate::plan(ctx).await,
        ("migrate", "execute") => migrate::execute(ctx).await,
        ("reminders", "create") => reminders::create(ctx).await,
        ("reminders", "list") => reminders::list(ctx).await,
        ("reminders", "complete") => reminders::complete(ctx).await,
        ("reminders", "delete") => reminders::delete(ctx).await,
        (r, _) if !known_resource(r) => Err((
            ErrorCode::UnknownResource,
            format!("Unknown resource: {r}"),
        )),
        _ => Err((
            ErrorCode::UnknownAction,
            format!("Unknown action {action} for resource {resource}"),
        )),
    }
}

fn known_resource(resource: &str) -> bool {
    matches!(
        resource,
        "briefing"
            | "user"
            | "config"
            | "directory"
            | "identity"
            | "journal"
            | "migrate"
            | "reminders"
    )
}

/// Per-call context passed to every handler.
pub struct Context {
    pub body: Value,
    pub client: BookStackClient,
    pub db: Arc<dyn DbBackend>,
    pub semantic: Option<Arc<SemanticState>>,
    /// Cached directory tree, populated by `crate::directory`. Optional
    /// because not every dispatch entrypoint has wired it yet — handlers
    /// that need it return an explicit InternalError when it's None.
    pub directory: Option<Arc<DirectoryService>>,
    pub settings: UserSettings,
    /// Raw BookStack token id for the calling user. Required by
    /// `briefing::read`, which hashes it internally for settings lookup.
    /// Handlers in this module otherwise use `token_id_hash` directly.
    pub token_id: String,
    pub token_id_hash: String,
}

impl Context {
    /// Read an i64 body field (accepts integer or numeric string).
    pub fn body_i64(&self, key: &str) -> Option<i64> {
        let v = self.body.get(key)?;
        v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_resource_recognizes_shipped_resources() {
        assert!(known_resource("briefing"));
        assert!(known_resource("user"));
        assert!(known_resource("config"));
        assert!(known_resource("directory"));
        assert!(known_resource("identity"));
        assert!(known_resource("journal"));
        assert!(known_resource("migrate"));
        assert!(known_resource("reminders"));
    }

    #[test]
    fn known_resource_rejects_unshipped() {
        assert!(!known_resource("nonsense"));
        assert!(!known_resource(""));
    }
}
