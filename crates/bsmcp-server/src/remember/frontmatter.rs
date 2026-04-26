//! YAML frontmatter helpers for /remember writes.
//!
//! Every page the protocol writes is stamped with a leading YAML block
//! describing provenance. BookStack ignores leading YAML in markdown bodies,
//! so the frontmatter is invisible in the UI but parseable by tools.

use bsmcp_common::settings::UserSettings;

/// Build a YAML frontmatter block for a write call.
pub fn build(
    settings: &UserSettings,
    trace_id: &str,
    resource: &str,
    key: Option<&str>,
    supersedes_page: Option<i64>,
) -> String {
    let mut out = String::from("---\n");
    if let Some(name) = &settings.ai_identity_name {
        out.push_str(&format!("written_by: {}\n", yaml_scalar(name)));
    }
    if let Some(ouid) = &settings.ai_identity_ouid {
        out.push_str(&format!("ai_identity_ouid: {}\n", yaml_scalar(ouid)));
    }
    if let Some(user_id) = &settings.user_id {
        out.push_str(&format!("user_id: {}\n", yaml_scalar(user_id)));
    }
    out.push_str(&format!("written_at: {}\n", iso_now()));
    out.push_str(&format!("trace_id: {}\n", yaml_scalar(trace_id)));
    out.push_str(&format!("resource: {}\n", yaml_scalar(resource)));
    if let Some(k) = key {
        out.push_str(&format!("key: {}\n", yaml_scalar(k)));
    }
    if let Some(p) = supersedes_page {
        out.push_str(&format!("supersedes_page: {}\n", p));
    }
    out.push_str("---\n\n");
    out
}

/// Strip a leading YAML frontmatter block from a markdown body (if present).
/// Used by `read` to return the user-visible content without the metadata.
pub fn strip(markdown: &str) -> &str {
    let trimmed = markdown.trim_start();
    if !trimmed.starts_with("---\n") && !trimmed.starts_with("---\r\n") {
        return markdown;
    }
    // Find the closing "---" on its own line
    let after_open = &trimmed[4..];
    if let Some(end_idx) = find_closing_marker(after_open) {
        let rest = &after_open[end_idx..];
        // Skip the closing marker + newline
        let rest = rest
            .trim_start_matches("---\n")
            .trim_start_matches("---\r\n")
            .trim_start_matches("---");
        return rest.trim_start();
    }
    markdown
}

fn find_closing_marker(s: &str) -> Option<usize> {
    let mut pos = 0;
    for line in s.split_inclusive('\n') {
        let line_trimmed = line.trim_end_matches(['\r', '\n']);
        if line_trimmed == "---" {
            return Some(pos);
        }
        pos += line.len();
    }
    None
}

fn iso_now() -> String {
    // Best-effort RFC 3339 timestamp using only std. Format: YYYY-MM-DDTHH:MM:SSZ
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let (year, month, day, hour, min, sec) = unix_to_components(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

fn unix_to_components(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let time = secs.rem_euclid(86400) as u32;
    let hour = time / 3600;
    let min = (time % 3600) / 60;
    let sec = time % 60;
    let (y, m, d) = days_to_ymd(days);
    (y, m, d, hour, min, sec)
}

fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Conservative YAML scalar quoting — wraps in double quotes if the value
/// contains characters that could be misinterpreted.
fn yaml_scalar(s: &str) -> String {
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

/// Current Unix timestamp in seconds.
pub fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Current UTC time as ISO 8601 (e.g., "2026-04-26T05:51:23Z").
pub fn now_iso_utc() -> String {
    iso_now()
}

/// Build today's date in YYYY-MM-DD format. Used as the natural key for journals.
pub fn today_iso_date() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let (y, m, d, _, _, _) = unix_to_components(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Build current YYYY-MM (for monthly journal chapters).
#[allow(dead_code)] // referenced by the journal sub_chapter_for_key path; kept exported for callers
pub fn current_month() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let (y, m, _, _, _, _) = unix_to_components(secs);
    format!("{y:04}-{m:02}")
}

/// Slugify an arbitrary string into a lowercase URL-safe slug.
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_handles_no_frontmatter() {
        assert_eq!(strip("just markdown\n"), "just markdown\n");
    }

    #[test]
    fn strip_handles_frontmatter() {
        let input = "---\nfoo: bar\n---\n\ncontent here\n";
        assert_eq!(strip(input), "content here\n");
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("Trust Infrastructure!"), "trust-infrastructure");
        assert_eq!(slugify("  spaces  "), "spaces");
    }
}
