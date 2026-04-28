//! `/remember/v1/migrate/{plan|apply|status}` — opt-in data migration to
//! the v1.0.0 chapter structure.
//!
//! Phase 7 of the identity-book-restructure RFC. Two callsites:
//!
//! - `briefing` runs the legacy-state detector (`is_legacy_layout`) and
//!   surfaces a `setup_nudge` when an identity needs migrating.
//! - This module's MCP handler executes the plan under the user's
//!   BookStack auth (the embed token can't move owner-only journal pages).
//!
//! Idempotent on every step. Re-running `apply` after a partial failure is
//! safe — find-or-create is the dedup primitive throughout.

use serde_json::{json, Value};

use bsmcp_common::settings::UserSettings;

use super::envelope::ErrorCode;
use super::journal_archive;
use super::provision;
use super::{Context, Outcome};

const AGENTS_CHAPTER: &str = "Agents";
const SUBAGENT_CONVERSATIONS_CHAPTER: &str = "Subagent Conversations";
const JOURNAL_CHAPTER: &str = "Journal";
const AGENT_PAGE_PREFIX: &str = "Agent: ";

pub async fn handle(action: &str, ctx: &Context) -> Outcome {
    match action {
        "plan" => plan(ctx).await,
        "apply" => apply(ctx).await,
        "status" => status(ctx).await,
        _ => Outcome::error(
            ErrorCode::UnknownAction,
            format!("Unknown action {action} on migrate"),
            None,
        ),
    }
}

/// Top-level legacy-state predicate. True when the identity has the legacy
/// book structure but isn't yet wired to the Phase 6 chapter layout. Used
/// by the briefing's `setup_nudge` to decide whether to prompt.
pub fn is_legacy_layout(s: &UserSettings) -> bool {
    // Two ways to be legacy:
    //   1. Legacy journal book pointer set, but new chapter pointer unset.
    //   2. New chapter pointer unset AND identity book has no chapters
    //      configured at all (post-create, before chapter scaffolding).
    //      Captured by the same condition.
    s.ai_identity_book_id.is_some() && s.ai_identity_journal_chapter_id.is_none()
}

/// One step in the migration plan.
fn step(
    description: &str,
    target: Option<&str>,
    affects_pages: usize,
    will_create: bool,
) -> Value {
    json!({
        "description": description,
        "target": target,
        "affects_pages": affects_pages,
        "will_create": will_create,
    })
}

// --- plan ---

async fn plan(ctx: &Context) -> Outcome {
    let settings = &ctx.settings;
    let identity_book_id = match settings.ai_identity_book_id {
        Some(id) => id,
        None => {
            return Outcome::settings_not_configured(
                "ai_identity_book_id",
                "ai_identity_book_id is not set — run `remember_identity action=create` first to scaffold the Identity book",
            );
        }
    };

    let mut steps: Vec<Value> = Vec::new();

    // Step 1: chapter creation (find-or-create — re-runs are no-ops once
    // the chapters exist).
    for (name, field, present) in [
        (
            AGENTS_CHAPTER,
            "ai_identity_agents_chapter_id",
            settings.ai_identity_agents_chapter_id.is_some(),
        ),
        (
            SUBAGENT_CONVERSATIONS_CHAPTER,
            "ai_identity_subagent_conversations_chapter_id",
            settings.ai_identity_subagent_conversations_chapter_id.is_some(),
        ),
        (
            JOURNAL_CHAPTER,
            "ai_identity_journal_chapter_id",
            settings.ai_identity_journal_chapter_id.is_some(),
        ),
    ] {
        if present {
            steps.push(step(
                &format!("Chapter \"{name}\" already exists; reuse"),
                Some(field),
                0,
                false,
            ));
        } else {
            steps.push(step(
                &format!("Find-or-create chapter \"{name}\" inside the Identity book"),
                Some(field),
                0,
                true,
            ));
        }
    }

    // Step 2: count agent-named pages currently loose at the book root.
    let book = match ctx.client.get_book(identity_book_id).await {
        Ok(b) => b,
        Err(e) => return Outcome::error(ErrorCode::BookStackError, e, Some("ai_identity_book_id")),
    };
    let loose_agent_pages: Vec<&Value> = book
        .get("contents")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|item| {
                    item.get("type").and_then(|t| t.as_str()) == Some("page")
                        && item
                            .get("name")
                            .and_then(|n| n.as_str())
                            .map(|n| n.starts_with(AGENT_PAGE_PREFIX))
                            .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    steps.push(step(
        &format!(
            "Move {} agent page(s) from book root → \"Agents\" chapter",
            loose_agent_pages.len()
        ),
        Some("agent_pages"),
        loose_agent_pages.len(),
        false,
    ));

    // Step 3: count pages in the legacy journal book.
    let legacy_pages_count = match settings.ai_hive_journal_book_id {
        Some(book_id) => match ctx.client.list_book_pages_by_updated(book_id, usize::MAX).await {
            Ok(pages) => pages.len(),
            Err(e) => {
                return Outcome::error(
                    ErrorCode::BookStackError,
                    format!("Cannot list legacy journal book {book_id}: {e}"),
                    Some("ai_hive_journal_book_id"),
                );
            }
        },
        None => 0,
    };
    if settings.ai_hive_journal_book_id.is_some() {
        steps.push(step(
            &format!(
                "Move {legacy_pages_count} page(s) from legacy journal book → \"Journal\" chapter"
            ),
            Some("ai_hive_journal_book_id"),
            legacy_pages_count,
            false,
        ));
    } else {
        steps.push(step(
            "No legacy journal book configured — skip",
            None,
            0,
            false,
        ));
    }

    // Step 4: year-rollover sweep planning is a count rather than a list.
    steps.push(step(
        "Run year-rollover sweep on \"Journal\" chapter — moves stale-year pages into Journal Archive - {YEAR}",
        Some("year_rollover"),
        0, // unknown until apply
        true,
    ));

    // Step 5: settings update.
    steps.push(step(
        "Update user_settings with new chapter IDs; clear ai_hive_journal_book_id",
        Some("user_settings"),
        0,
        false,
    ));

    Outcome::ok(json!({
        "action": "plan",
        "identity_book_id": identity_book_id,
        "is_legacy_layout": is_legacy_layout(settings),
        "steps": steps,
        "summary": format!(
            "{} loose agent page(s), {} legacy journal page(s) ready to migrate",
            loose_agent_pages.len(),
            legacy_pages_count,
        ),
    }))
}

// --- apply ---

#[derive(Default)]
struct ApplyTotals {
    chapters_created: usize,
    chapters_reused: usize,
    agent_pages_moved: usize,
    journal_pages_moved: usize,
    archived_pages: usize,
    failures: Vec<String>,
}

async fn apply(ctx: &Context) -> Outcome {
    let mut settings = ctx.settings.clone();
    let identity_book_id = match settings.ai_identity_book_id {
        Some(id) => id,
        None => {
            return Outcome::settings_not_configured(
                "ai_identity_book_id",
                "ai_identity_book_id is not set — run `remember_identity action=create` first to scaffold the Identity book",
            );
        }
    };

    let mut totals = ApplyTotals::default();
    let mut step_results: Vec<Value> = Vec::new();

    // Step 1: chapters. The helper uses find-or-create so re-runs are
    // idempotent. Track which were freshly created vs reused so the
    // response can show progress.
    for (name, description) in [
        (
            AGENTS_CHAPTER,
            "Agent definition pages for this AI identity (one page per sub-agent).",
        ),
        (
            SUBAGENT_CONVERSATIONS_CHAPTER,
            "Agent-to-agent conversation transcripts. Scaffolded empty.",
        ),
        (
            JOURNAL_CHAPTER,
            "Current-year daily journal entries. Year-rollover sweep moves stale entries into 'Journal Archive - {YEAR}' chapters.",
        ),
    ] {
        let outcome = provision::find_or_create_chapter(
            &ctx.client,
            ctx.index_db.as_ref(),
            identity_book_id,
            name,
            description,
        )
        .await;
        match (outcome.id(), &outcome) {
            (Some(id), provision::ProvisionResult::Created { .. }) => {
                totals.chapters_created += 1;
                step_results.push(json!({
                    "step": format!("create chapter {name}"),
                    "result": "created",
                    "chapter_id": id,
                }));
                match name {
                    AGENTS_CHAPTER => settings.ai_identity_agents_chapter_id = Some(id),
                    SUBAGENT_CONVERSATIONS_CHAPTER => {
                        settings.ai_identity_subagent_conversations_chapter_id = Some(id);
                    }
                    JOURNAL_CHAPTER => settings.ai_identity_journal_chapter_id = Some(id),
                    _ => {}
                }
            }
            (Some(id), provision::ProvisionResult::FoundExisting { .. }) => {
                totals.chapters_reused += 1;
                step_results.push(json!({
                    "step": format!("create chapter {name}"),
                    "result": "found_existing",
                    "chapter_id": id,
                }));
                match name {
                    AGENTS_CHAPTER => settings.ai_identity_agents_chapter_id = Some(id),
                    SUBAGENT_CONVERSATIONS_CHAPTER => {
                        settings.ai_identity_subagent_conversations_chapter_id = Some(id);
                    }
                    JOURNAL_CHAPTER => settings.ai_identity_journal_chapter_id = Some(id),
                    _ => {}
                }
            }
            _ => {
                let reason = outcome.human(super::naming::NamedResource::IdentityBook);
                totals.failures.push(format!("chapter {name}: {reason}"));
                step_results.push(json!({
                    "step": format!("create chapter {name}"),
                    "result": "failed",
                    "reason": reason,
                }));
            }
        }
    }

    let agents_chapter_id = settings.ai_identity_agents_chapter_id;
    let journal_chapter_id = settings.ai_identity_journal_chapter_id;

    // Step 2: move loose agent-named pages from the book root into the
    // Agents chapter. Best-effort per page.
    if let Some(target_chapter) = agents_chapter_id {
        let book = match ctx.client.get_book(identity_book_id).await {
            Ok(b) => b,
            Err(e) => {
                totals.failures.push(format!("get_book({identity_book_id}): {e}"));
                return finalize_apply(ctx, settings, totals, step_results).await;
            }
        };
        let loose_pages: Vec<i64> = book
            .get("contents")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let is_loose_agent = item.get("type").and_then(|t| t.as_str()) == Some("page")
                            && item
                                .get("name")
                                .and_then(|n| n.as_str())
                                .map(|n| n.starts_with(AGENT_PAGE_PREFIX))
                                .unwrap_or(false);
                        if !is_loose_agent {
                            return None;
                        }
                        item.get("id").and_then(|i| i.as_i64())
                    })
                    .collect()
            })
            .unwrap_or_default();
        for page_id in loose_pages {
            let payload = json!({ "chapter_id": target_chapter });
            match ctx.client.update_page(page_id, &payload).await {
                Ok(_) => {
                    totals.agent_pages_moved += 1;
                    step_results.push(json!({
                        "step": format!("move agent page {page_id} → chapter {target_chapter}"),
                        "result": "moved",
                    }));
                }
                Err(e) => {
                    totals.failures.push(format!("move agent page {page_id}: {e}"));
                    step_results.push(json!({
                        "step": format!("move agent page {page_id}"),
                        "result": "failed",
                        "reason": e,
                    }));
                }
            }
        }
    }

    // Step 3: move every page from the legacy journal book → Journal chapter.
    if let (Some(legacy_book_id), Some(target_chapter)) =
        (settings.ai_hive_journal_book_id, journal_chapter_id)
    {
        match ctx.client.list_book_pages_by_updated(legacy_book_id, usize::MAX).await {
            Ok(pages) => {
                for page in pages {
                    let Some(page_id) = page.get("id").and_then(|v| v.as_i64()) else { continue; };
                    let payload = json!({ "chapter_id": target_chapter });
                    match ctx.client.update_page(page_id, &payload).await {
                        Ok(_) => {
                            totals.journal_pages_moved += 1;
                            step_results.push(json!({
                                "step": format!("move journal page {page_id} → chapter {target_chapter}"),
                                "result": "moved",
                            }));
                        }
                        Err(e) => {
                            totals.failures.push(format!("move journal page {page_id}: {e}"));
                            step_results.push(json!({
                                "step": format!("move journal page {page_id}"),
                                "result": "failed",
                                "reason": e,
                            }));
                        }
                    }
                }
            }
            Err(e) => {
                totals.failures.push(format!("list legacy book {legacy_book_id}: {e}"));
            }
        }
        // Clear the legacy pointer regardless of partial failures — the
        // book may still contain pages that failed to move (user can
        // retry apply), but the canonical pointer should reflect the new
        // structure so future writes go to the chapter.
        settings.ai_hive_journal_book_id = None;
    }

    // Step 4: year-rollover sweep on the Journal chapter.
    if let Some(journal_chapter) = journal_chapter_id {
        match journal_archive::year_rollover_sweep(journal_chapter, identity_book_id, ctx).await {
            Ok(n) => {
                totals.archived_pages = n;
                step_results.push(json!({
                    "step": "year_rollover_sweep",
                    "result": "ok",
                    "archived_pages": n,
                }));
            }
            Err(e) => {
                totals.failures.push(format!("year_rollover_sweep: {e}"));
                step_results.push(json!({
                    "step": "year_rollover_sweep",
                    "result": "failed",
                    "reason": e,
                }));
            }
        }
    }

    finalize_apply(ctx, settings, totals, step_results).await
}

async fn finalize_apply(
    ctx: &Context,
    settings: UserSettings,
    totals: ApplyTotals,
    mut step_results: Vec<Value>,
) -> Outcome {
    // Step 5: persist the updated settings.
    let persist_result = ctx
        .db
        .save_user_settings(&ctx.token_id_hash, &settings)
        .await;
    match &persist_result {
        Ok(_) => step_results.push(json!({
            "step": "save_user_settings",
            "result": "ok",
        })),
        Err(e) => step_results.push(json!({
            "step": "save_user_settings",
            "result": "failed",
            "reason": e,
        })),
    }

    let success = totals.failures.is_empty() && persist_result.is_ok();
    let final_status = if success {
        "complete"
    } else if totals.chapters_created + totals.chapters_reused == 3
        && totals.failures.iter().all(|f| !f.starts_with("chapter "))
    {
        "partial_success"
    } else {
        "failed"
    };

    Outcome::ok(json!({
        "action": "apply",
        "status": final_status,
        "totals": {
            "chapters_created": totals.chapters_created,
            "chapters_reused": totals.chapters_reused,
            "agent_pages_moved": totals.agent_pages_moved,
            "journal_pages_moved": totals.journal_pages_moved,
            "archived_pages": totals.archived_pages,
            "failures": totals.failures.len(),
        },
        "failures": totals.failures,
        "steps": step_results,
        "settings": {
            "ai_identity_agents_chapter_id": settings.ai_identity_agents_chapter_id,
            "ai_identity_subagent_conversations_chapter_id": settings.ai_identity_subagent_conversations_chapter_id,
            "ai_identity_journal_chapter_id": settings.ai_identity_journal_chapter_id,
            "ai_hive_journal_book_id": settings.ai_hive_journal_book_id,
        },
    }))
}

// --- status ---

async fn status(ctx: &Context) -> Outcome {
    let s = &ctx.settings;
    let mut checks: Vec<Value> = Vec::new();
    let mut all_pass = true;

    let mut record = |name: &str, pass: bool, detail: &str| {
        if !pass {
            all_pass = false;
        }
        checks.push(json!({
            "check": name,
            "pass": pass,
            "detail": detail,
        }));
    };

    record(
        "identity_book_configured",
        s.ai_identity_book_id.is_some(),
        if s.ai_identity_book_id.is_some() {
            "ai_identity_book_id is set"
        } else {
            "ai_identity_book_id is not set — run remember_identity action=create"
        },
    );
    record(
        "agents_chapter_configured",
        s.ai_identity_agents_chapter_id.is_some(),
        if s.ai_identity_agents_chapter_id.is_some() {
            "ai_identity_agents_chapter_id is set"
        } else {
            "ai_identity_agents_chapter_id is not set — run remember_migrate action=apply"
        },
    );
    record(
        "subagent_conversations_chapter_configured",
        s.ai_identity_subagent_conversations_chapter_id.is_some(),
        if s.ai_identity_subagent_conversations_chapter_id.is_some() {
            "ai_identity_subagent_conversations_chapter_id is set"
        } else {
            "ai_identity_subagent_conversations_chapter_id is not set — run remember_migrate action=apply"
        },
    );
    record(
        "journal_chapter_configured",
        s.ai_identity_journal_chapter_id.is_some(),
        if s.ai_identity_journal_chapter_id.is_some() {
            "ai_identity_journal_chapter_id is set"
        } else {
            "ai_identity_journal_chapter_id is not set — run remember_migrate action=apply"
        },
    );
    record(
        "legacy_journal_book_cleared",
        s.ai_hive_journal_book_id.is_none(),
        if s.ai_hive_journal_book_id.is_none() {
            "legacy ai_hive_journal_book_id is cleared"
        } else {
            "legacy ai_hive_journal_book_id is still set — run remember_migrate action=apply to migrate then clear it"
        },
    );

    Outcome::ok(json!({
        "action": "status",
        "fully_migrated": all_pass,
        "is_legacy_layout": is_legacy_layout(s),
        "checks": checks,
    }))
}
