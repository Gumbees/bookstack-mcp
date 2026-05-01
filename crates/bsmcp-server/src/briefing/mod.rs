//! Briefing — per-session reconstitution context.
//!
//! v0.8.0: replaces the `/remember/v1/{resource}/{action}` namespace. The
//! personal-memory layer (journals, collages, identities, whoami, user)
//! moved to memberberry.ai. What remains is the briefing — a single
//! response shape that gives the AI everything it needs to know about the
//! current session: time, org/user identity context, system-prompt-additions,
//! setup status, and KB semantic matches against the prompt.
//!
//! HTTP entry: `POST /briefing/v1/read`
//! MCP entry:  `briefing` tool
//! Auto-injection: `meta.briefing` on every MCP tool response (full content
//!                 first call per session, sticky bits thereafter — handled
//!                 by `crate::session`).

pub mod briefing;
pub mod envelope;
pub mod frontmatter;

use std::sync::Arc;

use serde_json::{json, Value};

use bsmcp_common::bookstack::BookStackClient;
use bsmcp_common::db::DbBackend;
use bsmcp_common::settings::{hash_token_id, UserSettings};

use crate::semantic::SemanticState;

#[allow(unused_imports)]
pub use envelope::{build_meta, build_time_block, ErrorCode, TIMEZONE_REFRESH_SECS};

/// Build the briefing JSON envelope for one request. Loads user settings,
/// applies any client-pushed timezone, runs the briefing builder, and wraps
/// the result in the standard `{ok, data, meta}` shape.
pub async fn read(
    body: Value,
    token_id: &str,
    client: &BookStackClient,
    db: Arc<dyn DbBackend>,
    semantic: Option<Arc<SemanticState>>,
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
            eprintln!("Briefing: failed to load settings: {e}");
            UserSettings::default()
        }
    };

    // Client-pushed timezone refresh — accepted on every briefing call so
    // the AI can keep the cache fresh from any call. No-op when the body
    // doesn't carry one or when the cache is already recent and matches.
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
                eprintln!("Briefing: failed to persist client_timezone (non-fatal): {e}");
            }
        }
        tz_just_pushed = true;
    }

    let ctx = Context {
        body,
        client: client.clone(),
        db: db.clone(),
        semantic,
        settings,
        token_id_hash,
    };

    let data = briefing::read(&ctx).await;

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

    json!({ "ok": true, "data": data, "meta": meta })
}

/// Build just the `meta.briefing` block for injection into other tool
/// responses. `full` returns the entire briefing payload; `sticky` returns
/// only the always-present bits (time, setup_status, warnings) so subsequent
/// MCP tool calls in the same session don't re-pay the briefing's cost.
pub async fn build_meta_briefing(
    body: Value,
    token_id: &str,
    client: &BookStackClient,
    db: Arc<dyn DbBackend>,
    semantic: Option<Arc<SemanticState>>,
    full: bool,
) -> Value {
    let token_id_hash = hash_token_id(token_id);
    let settings = db
        .get_user_settings(&token_id_hash)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let globals = db.get_global_settings().await.unwrap_or_default();

    if !full {
        // Sticky: time + setup_status + nothing else
        return json!({
            "time": envelope::build_time_block(&settings, false),
            "setup_summary": envelope::sticky_setup_summary(&settings, &globals),
            "shape": "sticky",
        });
    }

    let ctx = Context {
        body,
        client: client.clone(),
        db,
        semantic,
        settings,
        token_id_hash,
    };
    let mut payload = briefing::read(&ctx).await;
    if let Value::Object(ref mut m) = payload {
        m.insert("shape".to_string(), json!("full"));
    }
    payload
}

/// Per-call context passed to the briefing builder.
pub struct Context {
    pub body: Value,
    pub client: BookStackClient,
    pub db: Arc<dyn DbBackend>,
    pub semantic: Option<Arc<SemanticState>>,
    pub settings: UserSettings,
    pub token_id_hash: String,
}

impl Context {
    /// Read a string body field.
    pub fn body_str(&self, key: &str) -> Option<String> {
        self.body
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    }
}
