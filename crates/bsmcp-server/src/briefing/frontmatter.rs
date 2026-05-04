//! Frontmatter helpers.
//!
//! BookStack pages may carry leading YAML frontmatter (provenance from older
//! `/remember` writes, manually authored metadata). `strip` removes the
//! leading block so user-facing reads return only the body.

/// Strip a leading YAML frontmatter block from a markdown body (if present).
pub fn strip(markdown: &str) -> &str {
    let trimmed = markdown.trim_start();
    if !trimmed.starts_with("---\n") && !trimmed.starts_with("---\r\n") {
        return markdown;
    }
    let after_open = &trimmed[4..];
    if let Some(end_idx) = find_closing_marker(after_open) {
        let rest = &after_open[end_idx..];
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

/// Current UTC time as ISO 8601 (e.g., "2026-04-26T05:51:23Z").
pub fn now_iso_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Current Unix timestamp in seconds.
#[allow(dead_code)]
pub fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
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
}
