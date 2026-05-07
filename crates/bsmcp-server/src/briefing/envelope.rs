//! Shared envelope/meta types for briefing responses and the auto-injected
//! meta block on every MCP tool response.

use serde_json::{json, Value};

use bsmcp_common::settings::{GlobalSettings, UserSettings};

/// How long a client-pushed timezone is trusted before the response flags it
/// for refresh. 4h covers DST transitions and most travel within a session.
pub const TIMEZONE_REFRESH_SECS: i64 = 4 * 60 * 60;

/// Build the `meta` block for a `/briefing/v1/read` response. For the meta
/// block injected onto every MCP tool response, see `crate::briefing::build_meta_briefing`.
pub fn build_meta(
    trace_id: &str,
    elapsed_ms: u64,
    settings: &UserSettings,
    globals: &GlobalSettings,
    warnings: Vec<Value>,
    tz_just_pushed: bool,
) -> Value {
    let mut meta = json!({
        "trace_id": trace_id,
        "elapsed_ms": elapsed_ms,
        "config": {
            "label": settings.label,
            "role": settings.role,
            "user_id": settings.user_id,
        },
        "warnings": warnings,
        "time": build_time_block(settings, tz_just_pushed),
    });

    if let Some(status) = sticky_setup_summary(settings, globals) {
        meta["setup_incomplete"] = json!(true);
        meta["setup_summary"] = status;
    }

    meta
}

/// Time block — surfaced on every response. Always carries unix + UTC; adds
/// `now_local` and `now_human` when the user's timezone is set.
pub fn build_time_block(s: &UserSettings, tz_just_pushed: bool) -> Value {
    let now = chrono::Utc::now();
    let now_unix = now.timestamp();
    let now_unix_ms = now.timestamp_millis();
    let now_utc = now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();

    let (timezone, source) = match s.timezone.as_deref() {
        Some(tz) => (tz.to_string(), "user_settings"),
        None => ("UTC".to_string(), "default_utc"),
    };

    let mut block = json!({
        "now_unix": now_unix,
        "now_unix_ms": now_unix_ms,
        "now_utc": now_utc,
        "timezone": timezone,
        "timezone_source": source,
    });

    if let Ok(tz) = timezone.parse::<chrono_tz::Tz>() {
        let local = now.with_timezone(&tz);
        block["now_local"] = json!(local.format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string());
        block["now_human"] = json!(local.format("%A, %B %-d, %Y at %-I:%M %p %Z").to_string());
    }

    let refresh_due = if tz_just_pushed {
        false
    } else {
        match s.timezone_fetched_at {
            None => s.timezone.is_some(),
            Some(t) => now_unix - t > TIMEZONE_REFRESH_SECS,
        }
    };
    block["timezone_refresh_due"] = json!(refresh_due);
    block["timezone_refresh_hint"] = json!(
        "Pass `client_timezone` (IANA name like \"America/New_York\") on any \
         briefing call to refresh."
    );

    block
}

/// One-line setup status. Returns None when fully configured. Public so the
/// sticky meta-injection on non-briefing tool responses can reuse it.
pub fn sticky_setup_summary(s: &UserSettings, g: &GlobalSettings) -> Option<Value> {
    let user_missing = pending_user_summary(s);
    let global_missing = pending_global_summary(g);
    if user_missing.is_empty() && global_missing.is_empty() {
        return None;
    }
    Some(json!({
        "user_pending": user_missing,
        "global_pending": global_missing,
        "next_step": "Call `briefing` for the full setup_nudge with per-field workflow guidance.",
    }))
}

fn pending_user_summary(s: &UserSettings) -> Vec<&'static str> {
    let mut out = Vec::new();
    if s.user_id.is_none() { out.push("user_id"); }
    if s.bookstack_user_id.is_none() { out.push("bookstack_user_id"); }
    if s.domains.is_empty() { out.push("domains"); }
    out
}

fn pending_global_summary(g: &GlobalSettings) -> Vec<&'static str> {
    let mut out = Vec::new();
    if g.guide_page_id.is_none() { out.push("guide_page_id"); }
    if g.org_identity_page_id.is_none() { out.push("org_identity_page_id"); }
    if g.org_domains.is_empty() { out.push("org_domains"); }
    out
}
