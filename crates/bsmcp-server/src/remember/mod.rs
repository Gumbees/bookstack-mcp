//! `/remember/v1/{resource}/{action}` — the memory protocol.
//!
//! Resources (singletons): briefing, whoami, user, config
//! Resources (collections): journal, collage, shared_collage, user_journal
//! Resources (special):   audit (read-only),
//!                        search (cross-resource semantic + keyword)
//!
//! Every handler returns the same envelope: `{ok, data, meta, error}`.
//! Null settings disable the affected section/resource — the request never
//! crashes when a setting is missing.

pub mod audit;
pub mod briefing;
pub mod collection;
pub mod directory;
pub mod envelope;
pub mod frontmatter;
pub mod identity;
pub mod naming;
pub mod provision;
pub mod search;
pub mod singletons;
pub mod user_provision;

use std::sync::Arc;

use serde_json::{json, Value};

use bsmcp_common::bookstack::BookStackClient;
use bsmcp_common::db::DbBackend;
use bsmcp_common::settings::{hash_token_id, UserSettings};
use bsmcp_common::types::AuditEntryInsert;

use crate::semantic::SemanticState;

pub use envelope::{ErrorCode, RememberWarning};

/// Dispatch a `/remember/v1/{resource}/{action}` call. Returns the JSON envelope.
///
/// `token_id` is the user's BookStack token ID (used for settings lookup + audit).
pub async fn dispatch(
    resource: &str,
    action: &str,
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

    // Load user settings — None becomes default (everything disabled).
    let settings = match db.get_user_settings(&token_id_hash).await {
        Ok(Some(s)) => s,
        Ok(None) => UserSettings::default(),
        Err(e) => {
            eprintln!("Remember: failed to load settings: {e}");
            UserSettings::default()
        }
    };

    let ctx = Context {
        body,
        trace_id: trace_id.clone(),
        client: client.clone(),
        db: db.clone(),
        semantic,
        settings,
        token_id_hash: token_id_hash.clone(),
        started,
    };

    let outcome = route(resource, action, &ctx).await;

    // Best-effort audit logging (only on writes/deletes, not on every read).
    let log_audit = matches!(action, "write" | "delete");
    if log_audit {
        let ouid = ctx.settings.ai_identity_ouid.clone();
        let user_id = ctx.settings.user_id.clone();
        let entry = AuditEntryInsert {
            token_id_hash,
            ai_identity_ouid: ouid,
            user_id,
            resource: resource.to_string(),
            action: action.to_string(),
            target_page_id: outcome.target_page_id,
            target_key: outcome.target_key.clone(),
            success: outcome.is_ok(),
            error: outcome.error_message(),
            trace_id: Some(trace_id.clone()),
        };
        if let Err(e) = db.insert_audit_entry(&entry).await {
            eprintln!("Remember: audit insert failed (non-fatal): {e}");
        }
    }

    let elapsed_ms = ctx.started.elapsed().as_millis() as u64;
    let meta = envelope::build_meta(&trace_id, elapsed_ms, &ctx.settings, outcome.warnings.clone());

    match outcome.result {
        Ok(data) => json!({
            "ok": true,
            "data": data,
            "meta": meta,
        }),
        Err(err) => json!({
            "ok": false,
            "error": {
                "code": err.code.as_str(),
                "message": err.message,
                "field": err.field,
            },
            "meta": meta,
        }),
    }
}

async fn route(resource: &str, action: &str, ctx: &Context) -> Outcome {
    match (resource, action) {
        // Singletons
        ("briefing", "read") => briefing::read(ctx).await,
        ("whoami", "read") => singletons::read_whoami(ctx).await,
        ("whoami", "write") => singletons::write_whoami(ctx).await,
        ("user", "read") => singletons::read_user(ctx).await,
        ("user", "write") => singletons::write_user(ctx).await,
        ("config", "read") => singletons::read_config(ctx).await,
        ("config", "write") => singletons::write_config(ctx).await,
        ("config", "dismiss_setup_nudge") => singletons::dismiss_setup_nudge(ctx).await,

        // Collections (book parent)
        ("journal", a) => collection::handle(&collection::resources::Journal, a, ctx).await,
        ("collage", a) => collection::handle(&collection::resources::Collage, a, ctx).await,
        ("shared_collage", a) => collection::handle(&collection::resources::SharedCollage, a, ctx).await,
        ("user_journal", a) => collection::handle(&collection::resources::UserJournal, a, ctx).await,

        // Special
        ("audit", "read") => audit::read(ctx).await,
        ("search", "read") => search::read(ctx).await,
        ("identity", a) => identity::handle(a, ctx).await,
        ("directory", "read") => directory::read(ctx).await,

        _ => Outcome::error(ErrorCode::UnknownAction, format!("Unknown {resource}/{action}"), None),
    }
}

/// Per-call context passed to every handler. Cheap to clone (Arc'd internals).
#[derive(Clone)]
pub struct Context {
    pub body: Value,
    pub trace_id: String,
    pub client: BookStackClient,
    pub db: Arc<dyn DbBackend>,
    pub semantic: Option<Arc<SemanticState>>,
    pub settings: UserSettings,
    pub token_id_hash: String,
    pub started: std::time::Instant,
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

    /// Read an i64 body field (accepts integer or numeric string — matches the
    /// project's existing tolerance pattern).
    pub fn body_i64(&self, key: &str) -> Option<i64> {
        let v = self.body.get(key)?;
        v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    }

    /// Read a usize body field with bounds + default.
    pub fn body_count(&self, key: &str, default: usize, max: usize) -> usize {
        self.body
            .get(key)
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(default)
            .min(max)
            .max(1)
    }
}

/// Result of a routed call before envelope-wrapping.
pub struct Outcome {
    pub result: Result<Value, RememberError>,
    pub warnings: Vec<RememberWarning>,
    pub target_page_id: Option<i64>,
    pub target_key: Option<String>,
}

impl Outcome {
    pub fn ok(data: Value) -> Self {
        Self { result: Ok(data), warnings: Vec::new(), target_page_id: None, target_key: None }
    }

    pub fn ok_with_target(data: Value, page_id: Option<i64>, key: Option<String>) -> Self {
        Self { result: Ok(data), warnings: Vec::new(), target_page_id: page_id, target_key: key }
    }

    pub fn error(code: ErrorCode, message: impl Into<String>, field: Option<&str>) -> Self {
        Self {
            result: Err(RememberError {
                code,
                message: message.into(),
                field: field.map(|s| s.to_string()),
            }),
            warnings: Vec::new(),
            target_page_id: None,
            target_key: None,
        }
    }

    pub fn with_warning(mut self, w: RememberWarning) -> Self {
        self.warnings.push(w);
        self
    }

    pub fn is_ok(&self) -> bool {
        self.result.is_ok()
    }

    pub fn error_message(&self) -> Option<String> {
        self.result.as_ref().err().map(|e| e.message.clone())
    }
}

pub struct RememberError {
    pub code: ErrorCode,
    pub message: String,
    pub field: Option<String>,
}
