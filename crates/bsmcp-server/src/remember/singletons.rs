//! Singleton resources: whoami, user, config.
//!
//! These don't fit the collection model — each user has exactly one of each.
//! Reads pull straight from BookStack (or settings); writes update the
//! manifest page (or persist settings).

use serde_json::{json, Value};

use bsmcp_common::settings::{GlobalSettings, UserSettings};

use super::envelope::ErrorCode;
use super::frontmatter;
use super::provision;
use super::section;
use super::user_provision;
use super::{Context, Outcome};

// --- whoami ---

pub async fn read_whoami(ctx: &Context) -> Outcome {
    let globals = ctx.db.get_global_settings().await.unwrap_or_default();
    let resolved = globals.resolve_identity(&ctx.settings);
    let page_id = match resolved.page_id {
        Some(id) => id,
        None => {
            return Outcome::settings_not_configured(
                "ai_identity_page_id",
                "ai_identity_page_id not configured (no user setting and no org default)",
            );
        }
    };

    let page = match ctx.client.get_page(page_id).await {
        Ok(p) => p,
        Err(e) => return Outcome::error(ErrorCode::NotFound, e, Some("ai_identity_page_id")),
    };

    let raw_md = page.get("markdown").and_then(|v| v.as_str()).unwrap_or("");
    let body = frontmatter::strip(raw_md).to_string();

    Outcome::ok(json!({
        "ouid": resolved.ouid,
        "name": resolved.name.clone()
            .or_else(|| page.get("name").and_then(|v| v.as_str()).map(|s| s.to_string())),
        "using_org_default": resolved.using_default,
        "manifest": {
            "page_id": page_id,
            "name": page.get("name").cloned().unwrap_or(Value::Null),
            "markdown": body,
            "url": page.get("url").cloned().unwrap_or(Value::Null),
            "updated_at": page.get("updated_at").cloned().unwrap_or(Value::Null),
        },
        "shelf_id": ctx.settings.ai_hive_shelf_id,
        "identity_book_id": ctx.settings.ai_identity_book_id,
        "books": {
            "journal": ctx.settings.ai_hive_journal_book_id,
            "collage": ctx.settings.ai_collage_book_id,
            "shared_collage": ctx.settings.ai_shared_collage_book_id,
        },
    }))
}

pub async fn write_whoami(ctx: &Context) -> Outcome {
    let page_id = match ctx.settings.ai_identity_page_id {
        Some(id) => id,
        None => {
            return Outcome::settings_not_configured(
                "ai_identity_page_id",
                "ai_identity_page_id not configured — set the manifest page in /settings first",
            );
        }
    };
    let body = match ctx.body_str("body") {
        Some(b) => b,
        None => {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                "body field is required",
                Some("body"),
            );
        }
    };

    let frontmatter_block = frontmatter::build(
        &ctx.settings,
        &ctx.trace_id,
        "whoami",
        None,
        Some(page_id),
    );
    let payload = json!({ "markdown": format!("{frontmatter_block}{body}") });
    match ctx.client.update_page(page_id, &payload).await {
        Ok(updated) => Outcome::ok_with_target(
            json!({
                "action": "updated",
                "id": page_id,
                "name": updated.get("name").cloned().unwrap_or(Value::Null),
                "url": updated.get("url").cloned().unwrap_or(Value::Null),
                "updated_at": updated.get("updated_at").cloned().unwrap_or(Value::Null),
            }),
            Some(page_id),
            None,
        ),
        Err(e) => Outcome::error(ErrorCode::BookStackError, e, None),
    }
}

pub async fn update_section_whoami(ctx: &Context) -> Outcome {
    section_op_singleton(ctx, "whoami", false).await
}

pub async fn append_section_whoami(ctx: &Context) -> Outcome {
    section_op_singleton(ctx, "whoami", true).await
}

// --- user ---

pub async fn read_user(ctx: &Context) -> Outcome {
    // Auto-provision missing per-user structure (Identity book, identity page,
    // journal book, journal-agent page). No-op when everything's already in
    // settings or when `user_id` isn't configured. The first read_user call
    // after a user is set up populates the per-user shelf in one shot, and
    // subsequent calls are cheap idempotent shelf-membership checks.
    let globals = ctx.db.get_global_settings().await.unwrap_or_default();
    let mut working_settings = ctx.settings.clone();
    let provision_result = user_provision::auto_provision_user_identity(
        &ctx.client,
        ctx.index_db.as_ref(),
        globals.user_journals_shelf_id,
        &mut working_settings,
    )
    .await;
    if provision_result.any_changes() {
        if let Err(e) = ctx
            .db
            .save_user_settings(&ctx.token_id_hash, &working_settings)
            .await
        {
            eprintln!("read_user: failed to persist auto-provisioned IDs (non-fatal): {e}");
        }
        // Lock the freshly-created journal to owner-only on the same pass,
        // matching the existing settings-save behavior.
        provision::lock_journal_books_to_owner(
            &ctx.client,
            working_settings.ai_hive_journal_book_id,
            working_settings.user_journal_book_id,
        )
        .await;
    }

    let page_id = match working_settings.user_identity_page_id {
        Some(id) => id,
        None => {
            // user_id alone is enough for a partial response.
            if working_settings.user_id.is_some() {
                return Outcome::ok(json!({
                    "user_id": working_settings.user_id,
                    "identity_page": Value::Null,
                    "journal_book_id": working_settings.user_journal_book_id,
                    "auto_provisioned": auto_provision_summary(&provision_result),
                }));
            }
            return Outcome::settings_not_configured(
                "user_identity_page_id",
                "user_identity_page_id not configured (and user_id not set, so auto-provisioning is skipped — set user_id first)",
            );
        }
    };

    let page = match ctx.client.get_page(page_id).await {
        Ok(p) => p,
        Err(e) => return Outcome::error(ErrorCode::NotFound, e, Some("user_identity_page_id")),
    };
    let raw_md = page.get("markdown").and_then(|v| v.as_str()).unwrap_or("");
    let body = frontmatter::strip(raw_md).to_string();

    Outcome::ok(json!({
        "user_id": working_settings.user_id,
        "identity_page": {
            "page_id": page_id,
            "name": page.get("name").cloned().unwrap_or(Value::Null),
            "markdown": body,
            "url": page.get("url").cloned().unwrap_or(Value::Null),
            "updated_at": page.get("updated_at").cloned().unwrap_or(Value::Null),
        },
        "journal_book_id": working_settings.user_journal_book_id,
        "identity_book_id": working_settings.user_identity_book_id,
        "journal_agent_page_id": working_settings.user_journal_agent_page_id,
        "auto_provisioned": auto_provision_summary(&provision_result),
    }))
}

/// Summarize a provisioning pass into JSON for the user response. Returns
/// `Null` when nothing changed so consumers can treat the field as a "did
/// anything happen" flag.
fn auto_provision_summary(r: &user_provision::UserProvisionResult) -> Value {
    if !r.any_changes() && r.warnings.is_empty() {
        return Value::Null;
    }
    json!({
        "created_identity_book": r.created_identity_book,
        "created_identity_page": r.created_identity_page,
        "created_journal_book": r.created_journal_book,
        "created_journal_agent_page": r.created_journal_agent_page,
        "moved_to_shelf": r.moved_to_shelf,
        "warnings": r.warnings,
    })
}

pub async fn write_user(ctx: &Context) -> Outcome {
    // Auto-provision identical to read_user — guarantees write_user works on
    // a freshly-configured user without forcing a separate read first.
    let globals = ctx.db.get_global_settings().await.unwrap_or_default();
    let mut working_settings = ctx.settings.clone();
    let provision_result = user_provision::auto_provision_user_identity(
        &ctx.client,
        ctx.index_db.as_ref(),
        globals.user_journals_shelf_id,
        &mut working_settings,
    )
    .await;
    if provision_result.any_changes() {
        if let Err(e) = ctx
            .db
            .save_user_settings(&ctx.token_id_hash, &working_settings)
            .await
        {
            eprintln!("write_user: failed to persist auto-provisioned IDs (non-fatal): {e}");
        }
        provision::lock_journal_books_to_owner(
            &ctx.client,
            working_settings.ai_hive_journal_book_id,
            working_settings.user_journal_book_id,
        )
        .await;
    }
    let page_id = match working_settings.user_identity_page_id {
        Some(id) => id,
        None => {
            return Outcome::settings_not_configured(
                "user_identity_page_id",
                "user_identity_page_id not configured (auto-provision needs `user_id` and the global `user_journals_shelf_id` to work)",
            );
        }
    };
    let body = match ctx.body_str("body") {
        Some(b) => b,
        None => {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                "body field is required",
                Some("body"),
            );
        }
    };
    let frontmatter_block = frontmatter::build(
        &ctx.settings,
        &ctx.trace_id,
        "user",
        None,
        Some(page_id),
    );
    let payload = json!({ "markdown": format!("{frontmatter_block}{body}") });
    match ctx.client.update_page(page_id, &payload).await {
        Ok(updated) => Outcome::ok_with_target(
            json!({
                "action": "updated",
                "id": page_id,
                "url": updated.get("url").cloned().unwrap_or(Value::Null),
            }),
            Some(page_id),
            None,
        ),
        Err(e) => Outcome::error(ErrorCode::BookStackError, e, None),
    }
}

pub async fn update_section_user(ctx: &Context) -> Outcome {
    section_op_singleton(ctx, "user", false).await
}

pub async fn append_section_user(ctx: &Context) -> Outcome {
    section_op_singleton(ctx, "user", true).await
}

// --- shared section-op machinery for whoami / user singletons ---
//
// Resolves the target page for the named singleton, reads its body, runs the
// section transform, and writes back with refreshed frontmatter that
// preserves `written_at` (set on first creation) while stamping
// `last_section_update_at` and `last_updated_section`.

async fn section_op_singleton(ctx: &Context, resource: &'static str, is_append: bool) -> Outcome {
    let action_label = if is_append { "append_section" } else { "update_section" };

    let section_name = match ctx.body_str("section") {
        Some(s) => s,
        None => {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                format!("section field is required for {action_label}"),
                Some("section"),
            );
        }
    };
    let body_text = match ctx.body_str("body") {
        Some(b) => b,
        None => {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                format!("body field is required for {action_label}"),
                Some("body"),
            );
        }
    };

    // Resolve page id per resource. `user` runs the same auto-provision
    // chain as `read_user` / `write_user` so a fresh-token user can update
    // a section without a prior read.
    let (page_id, working_settings) = match resource {
        "whoami" => match ctx.settings.ai_identity_page_id {
            Some(id) => (id, ctx.settings.clone()),
            None => {
                return Outcome::settings_not_configured(
                    "ai_identity_page_id",
                    "ai_identity_page_id not configured — set the manifest page in /settings first",
                );
            }
        },
        "user" => {
            let globals = ctx.db.get_global_settings().await.unwrap_or_default();
            let mut ws = ctx.settings.clone();
            let provision_result = user_provision::auto_provision_user_identity(
                &ctx.client,
                ctx.index_db.as_ref(),
                globals.user_journals_shelf_id,
                &mut ws,
            )
            .await;
            if provision_result.any_changes() {
                if let Err(e) = ctx.db.save_user_settings(&ctx.token_id_hash, &ws).await {
                    eprintln!("{action_label}_user: persist auto-provisioned IDs failed (non-fatal): {e}");
                }
                provision::lock_journal_books_to_owner(
                    &ctx.client,
                    ws.ai_hive_journal_book_id,
                    ws.user_journal_book_id,
                )
                .await;
            }
            match ws.user_identity_page_id {
                Some(id) => (id, ws),
                None => {
                    return Outcome::settings_not_configured(
                        "user_identity_page_id",
                        "user_identity_page_id not configured (auto-provision needs `user_id` — \
                         try `remember_user action=read` first to trigger whoami auto-discovery)",
                    );
                }
            }
        }
        _ => {
            return Outcome::error(
                ErrorCode::InternalError,
                format!("section_op_singleton called with unknown resource: {resource}"),
                None,
            );
        }
    };

    // Read existing body, run the section transform, write back.
    let page = match ctx.client.get_page(page_id).await {
        Ok(p) => p,
        Err(e) => return Outcome::error(ErrorCode::BookStackError, e, None),
    };
    let raw = page.get("markdown").and_then(|v| v.as_str()).unwrap_or("");
    let existing_body = frontmatter::strip(raw);
    let preserved_written_at = parse_written_at(raw);

    let new_body = if is_append {
        section::append_to_section(existing_body, &section_name, &body_text)
    } else {
        section::replace_section(existing_body, &section_name, &body_text)
    };

    let frontmatter_block = build_singleton_section_frontmatter(
        &working_settings,
        &ctx.trace_id,
        resource,
        Some(page_id),
        preserved_written_at.as_deref(),
        &section_name,
    );
    let payload = json!({ "markdown": format!("{frontmatter_block}{new_body}") });
    match ctx.client.update_page(page_id, &payload).await {
        Ok(updated) => Outcome::ok_with_target(
            json!({
                "action": action_label,
                "id": page_id,
                "section": section_name,
                "name": updated.get("name").cloned().unwrap_or(Value::Null),
                "url": updated.get("url").cloned().unwrap_or(Value::Null),
                "updated_at": updated.get("updated_at").cloned().unwrap_or(Value::Null),
            }),
            Some(page_id),
            None,
        ),
        Err(e) => Outcome::error(ErrorCode::BookStackError, e, None),
    }
}

/// Pull `written_at` out of a page's leading YAML frontmatter (if any).
/// Used to preserve the original creation timestamp across section edits.
fn parse_written_at(markdown: &str) -> Option<String> {
    let trimmed = markdown.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let mut iter = trimmed.lines();
    iter.next(); // opening ---
    for line in iter {
        let line = line.trim();
        if line == "---" {
            break;
        }
        if let Some(rest) = line.strip_prefix("written_at:") {
            return Some(rest.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn build_singleton_section_frontmatter(
    settings: &bsmcp_common::settings::UserSettings,
    trace_id: &str,
    resource: &str,
    supersedes_page: Option<i64>,
    preserved_written_at: Option<&str>,
    last_section: &str,
) -> String {
    let mut out = String::from("---\n");
    if let Some(name) = &settings.ai_identity_name {
        out.push_str(&format!("written_by: {}\n", yaml_quote(name)));
    }
    if let Some(ouid) = &settings.ai_identity_ouid {
        out.push_str(&format!("ai_identity_ouid: {}\n", yaml_quote(ouid)));
    }
    if let Some(user_id) = &settings.user_id {
        out.push_str(&format!("user_id: {}\n", yaml_quote(user_id)));
    }
    let written_at = preserved_written_at
        .map(|s| s.to_string())
        .unwrap_or_else(frontmatter::now_iso_utc);
    out.push_str(&format!("written_at: {}\n", yaml_quote(&written_at)));
    out.push_str(&format!(
        "last_section_update_at: {}\n",
        yaml_quote(&frontmatter::now_iso_utc())
    ));
    out.push_str(&format!("last_updated_section: {}\n", yaml_quote(last_section)));
    out.push_str(&format!("trace_id: {}\n", yaml_quote(trace_id)));
    out.push_str(&format!("resource: {}\n", yaml_quote(resource)));
    if let Some(p) = supersedes_page {
        out.push_str(&format!("supersedes_page: {p}\n"));
    }
    out.push_str("---\n\n");
    out
}

fn yaml_quote(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.chars().any(|c| matches!(c, ':' | '#' | '\'' | '"' | '\n' | '{' | '}' | '[' | ']' | ','))
        || matches!(s, "true" | "false" | "null" | "yes" | "no" | "~");
    if needs_quote {
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

// --- config (UserSettings + GlobalSettings) ---

pub async fn read_config(ctx: &Context) -> Outcome {
    let user_json = match serde_json::to_value(&ctx.settings) {
        Ok(v) => v,
        Err(e) => return Outcome::error(ErrorCode::InternalError, e.to_string(), None),
    };
    let globals = ctx.db.get_global_settings().await.unwrap_or_default();
    let global_json = match serde_json::to_value(&globals) {
        Ok(v) => v,
        Err(e) => return Outcome::error(ErrorCode::InternalError, e.to_string(), None),
    };
    Outcome::ok(json!({
        "settings": user_json,
        "global_settings": global_json,
    }))
}

pub async fn write_config(ctx: &Context) -> Outcome {
    // Two optional sub-objects:
    //   - "settings"        : per-user UserSettings (anyone can write their own)
    //   - "global_settings" : instance-wide GlobalSettings (admins only,
    //                         server-side first-write-wins for set fields)
    let user_settings_arg = ctx.body.get("settings").cloned();
    let global_settings_arg = ctx.body.get("global_settings").cloned();

    if user_settings_arg.is_none() && global_settings_arg.is_none() {
        return Outcome::error(
            ErrorCode::InvalidArgument,
            "at least one of `settings` or `global_settings` is required",
            None,
        );
    }

    let mut warnings: Vec<super::envelope::RememberWarning> = Vec::new();
    let mut saved_user: Option<UserSettings> = None;
    let mut saved_globals: Option<GlobalSettings> = None;

    // --- per-user settings ---
    if let Some(raw) = user_settings_arg {
        let new_settings: UserSettings = match serde_json::from_value(raw) {
            Ok(s) => s,
            Err(e) => {
                return Outcome::error(
                    ErrorCode::InvalidArgument,
                    format!("settings parse error: {e}"),
                    Some("settings"),
                );
            }
        };
        if let Err(e) = ctx.db.save_user_settings(&ctx.token_id_hash, &new_settings).await {
            return Outcome::error(ErrorCode::InternalError, e, None);
        }
        // Lock journal books to owner-only on every config write — covers both
        // freshly-set IDs and re-saves of existing IDs. Idempotent.
        provision::lock_journal_books_to_owner(
            &ctx.client,
            new_settings.ai_hive_journal_book_id,
            new_settings.user_journal_book_id,
        )
        .await;
        saved_user = Some(new_settings);
    }

    // --- global settings (admin-only, first-write-wins) ---
    if let Some(raw) = global_settings_arg {
        let proposed: GlobalSettings = match serde_json::from_value(raw) {
            Ok(g) => g,
            Err(e) => {
                return Outcome::error(
                    ErrorCode::InvalidArgument,
                    format!("global_settings parse error: {e}"),
                    Some("global_settings"),
                );
            }
        };
        let is_admin = ctx.client.is_admin().await.unwrap_or(false);
        if !is_admin {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                "global_settings can only be written by BookStack admins",
                Some("global_settings"),
            );
        }

        // Two policies merged in one pass:
        //   - Shelf IDs are STRUCTURAL: first-write-wins. Once set they're
        //     locked against change because the data hangs off of them and
        //     swapping a shelf out from under it isn't supported.
        //   - Org policy/instruction lists and house-identity defaults are
        //     TUNABLE: admins can update them as policy evolves. The
        //     proposed value replaces the existing one (no append; the list
        //     is meant to be small and curated).
        // A field omitted from `proposed` (None / empty Vec) is left alone —
        // partial updates work without re-sending the whole struct.
        let existing = ctx.db.get_global_settings().await.unwrap_or_default();
        let mut merged = existing.clone();

        // Shelf IDs — first-write-wins, warn on attempted change.
        if existing.hive_shelf_id.is_none() {
            if let Some(v) = proposed.hive_shelf_id {
                merged.hive_shelf_id = Some(v);
            }
        } else if proposed.hive_shelf_id.is_some()
            && proposed.hive_shelf_id != existing.hive_shelf_id
        {
            warnings.push(super::envelope::RememberWarning::new(
                "global_locked",
                "hive_shelf_id is already set; ignoring requested change (first-write-wins).",
            ));
        }
        if existing.user_journals_shelf_id.is_none() {
            if let Some(v) = proposed.user_journals_shelf_id {
                merged.user_journals_shelf_id = Some(v);
            }
        } else if proposed.user_journals_shelf_id.is_some()
            && proposed.user_journals_shelf_id != existing.user_journals_shelf_id
        {
            warnings.push(super::envelope::RememberWarning::new(
                "global_locked",
                "user_journals_shelf_id is already set; ignoring requested change (first-write-wins).",
            ));
        }

        // Tunable: house-identity defaults. Admins can re-point these.
        if proposed.default_ai_identity_page_id.is_some() {
            merged.default_ai_identity_page_id = proposed.default_ai_identity_page_id;
        }
        if proposed.default_ai_identity_name.is_some() {
            merged.default_ai_identity_name = proposed.default_ai_identity_name.clone();
        }
        if proposed.default_ai_identity_ouid.is_some() {
            merged.default_ai_identity_ouid = proposed.default_ai_identity_ouid.clone();
        }

        // Tunable: org-mandated instruction / policy page lists. A non-empty
        // proposed list replaces the current one. To clear, callers can write
        // a new empty list explicitly via the dedicated `clear_*` flags in a
        // future API; for now an empty list is treated as "unchanged" so a
        // partial update (e.g., changing only shelf IDs) doesn't wipe them.
        if !proposed.org_required_instructions_page_ids.is_empty() {
            merged.org_required_instructions_page_ids =
                proposed.org_required_instructions_page_ids.clone();
        }
        if !proposed.org_ai_usage_policy_page_ids.is_empty() {
            merged.org_ai_usage_policy_page_ids =
                proposed.org_ai_usage_policy_page_ids.clone();
        }

        if let Err(e) = ctx.db.save_global_settings(&merged, &ctx.token_id_hash).await {
            return Outcome::error(ErrorCode::InternalError, e, None);
        }
        saved_globals = Some(merged);
    }

    let mut outcome = Outcome::ok(json!({
        "action": "saved",
        "settings": saved_user,
        "global_settings": saved_globals,
    }));
    for w in warnings {
        outcome = outcome.with_warning(w);
    }
    outcome
}

/// `remember_config action=dismiss_setup_nudge days=N` — snooze the briefing's
/// "configure your settings" reminder for N days (default 7, max 365).
pub async fn dismiss_setup_nudge(ctx: &Context) -> Outcome {
    let days = ctx
        .body
        .get("days")
        .and_then(|v| v.as_i64())
        .unwrap_or(7)
        .clamp(1, 365);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let dismiss_until = now + days * 86400;

    let mut new_settings = ctx.settings.clone();
    new_settings.settings_nudge_dismissed_until = Some(dismiss_until);
    if let Err(e) = ctx.db.save_user_settings(&ctx.token_id_hash, &new_settings).await {
        return Outcome::error(ErrorCode::InternalError, e, None);
    }
    Outcome::ok(json!({
        "action": "dismissed",
        "days": days,
        "snoozed_until_unix": dismiss_until,
        "message": format!("Setup nudge snoozed for {days} days. The briefing will surface it again after that, or sooner if any setting is configured.")
    }))
}

