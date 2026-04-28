//! Markdown section helpers for the `update_section` and `append_section`
//! actions. Pure functions over a markdown body — no DB, no async.
//!
//! A "section" is everything from a `## {name}` H2 line up to (but not
//! including) the next `## ` H2, or end-of-body. H1 (`# `) and deeper
//! headings (`### ` etc.) don't terminate a section. Frontmatter (leading
//! `---` block) is left untouched — callers strip it before calling here
//! and re-prepend after.

/// The byte ranges of a found H2 section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct H2Range {
    /// Byte index where the `## {name}` line starts.
    pub heading_start: usize,
    /// Byte index where the section body starts (line after the heading).
    pub content_start: usize,
    /// Byte index where the next `## ` heading or end-of-body begins.
    pub content_end: usize,
}

/// Find an H2 section by exact name match. Whitespace inside the heading
/// text is trimmed for comparison; the H2 marker itself must be `## `
/// (two hashes + space). Returns the first match; H2 names are expected
/// to be unique within a page.
pub fn find_h2_section(body: &str, target_name: &str) -> Option<H2Range> {
    let target = target_name.trim();
    let mut pos = 0usize;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if let Some(rest) = trimmed.strip_prefix("## ") {
            if rest.trim() == target {
                let heading_start = pos;
                let content_start = pos + line.len();
                let content_end = find_next_h2(body, content_start);
                return Some(H2Range {
                    heading_start,
                    content_start,
                    content_end,
                });
            }
        }
        pos += line.len();
    }
    None
}

/// Find the byte index of the next `## ` heading at or after `from`,
/// returning end-of-body if none.
fn find_next_h2(body: &str, from: usize) -> usize {
    let rest = &body[from..];
    let mut p = from;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.starts_with("## ") {
            return p;
        }
        p += line.len();
    }
    body.len()
}

/// Replace the named section's body, preserving the heading and every other
/// section. If the section doesn't exist, append it at end-of-body. Result
/// always ends with exactly one trailing newline.
pub fn replace_section(body: &str, section_name: &str, new_content: &str) -> String {
    match find_h2_section(body, section_name) {
        Some(range) => {
            let mut out = String::with_capacity(body.len() + new_content.len());
            // Everything up to and including the heading line.
            out.push_str(&body[..range.content_start]);
            // Replacement body (trimmed, then a trailing blank line for
            // separation from the next H2).
            let trimmed_new = new_content.trim();
            if !trimmed_new.is_empty() {
                out.push('\n');
                out.push_str(trimmed_new);
                out.push_str("\n\n");
            }
            // Everything from the next H2 onward.
            out.push_str(&body[range.content_end..]);
            normalize_trailing_newline(out)
        }
        None => append_section_at_end(body, section_name, new_content),
    }
}

/// Append `additional_content` to the named section's body, preserving the
/// heading and every other section. If the section doesn't exist, create
/// it at end-of-body. Result always ends with exactly one trailing newline.
pub fn append_to_section(body: &str, section_name: &str, additional_content: &str) -> String {
    match find_h2_section(body, section_name) {
        Some(range) => {
            let mut out = String::with_capacity(body.len() + additional_content.len());
            // Everything up to (but not including) the next H2.
            // Trim trailing whitespace from the section body so the appended
            // content butts up cleanly with one blank line of separation.
            let kept = body[..range.content_end].trim_end();
            out.push_str(kept);
            let trimmed_new = additional_content.trim();
            if !trimmed_new.is_empty() {
                out.push_str("\n\n");
                out.push_str(trimmed_new);
            }
            out.push_str("\n\n");
            // Everything from the next H2 onward.
            out.push_str(&body[range.content_end..]);
            normalize_trailing_newline(out)
        }
        None => append_section_at_end(body, section_name, additional_content),
    }
}

fn append_section_at_end(body: &str, section_name: &str, content: &str) -> String {
    let mut out = body.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str("## ");
    out.push_str(section_name.trim());
    out.push('\n');
    let trimmed_content = content.trim();
    if !trimmed_content.is_empty() {
        out.push('\n');
        out.push_str(trimmed_content);
    }
    out.push('\n');
    out
}

fn normalize_trailing_newline(mut s: String) -> String {
    while s.ends_with("\n\n") {
        s.pop();
    }
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_section_by_exact_name() {
        let body = "## Communication style\n\nTerse.\n\n## Working preferences\n\nDocker.\n";
        let r = find_h2_section(body, "Communication style").expect("should find");
        assert_eq!(&body[r.heading_start..r.content_start], "## Communication style\n");
        assert_eq!(&body[r.content_start..r.content_end], "\nTerse.\n\n");
    }

    #[test]
    fn finds_last_section_when_no_following_h2() {
        let body = "## Notes\n\nLast section.\n";
        let r = find_h2_section(body, "Notes").expect("should find");
        assert_eq!(r.content_end, body.len());
    }

    #[test]
    fn does_not_match_h1_or_h3() {
        let body = "# Communication style\n\n## Other\n\n### Communication style\n\nfoo\n";
        assert!(find_h2_section(body, "Communication style").is_none());
    }

    #[test]
    fn replace_section_preserves_other_sections() {
        let body = "## A\n\nold A\n\n## B\n\nold B\n";
        let result = replace_section(body, "A", "new A");
        assert!(result.contains("## A\n\nnew A\n"));
        assert!(result.contains("## B\n\nold B\n"));
    }

    #[test]
    fn replace_section_appends_when_missing() {
        let body = "## A\n\nbody\n";
        let result = replace_section(body, "C", "new C");
        assert!(result.contains("## A\n\nbody"));
        assert!(result.ends_with("## C\n\nnew C\n"));
    }

    #[test]
    fn replace_section_with_empty_body_keeps_heading() {
        let body = "## A\n\nold\n\n## B\n\nB body\n";
        let result = replace_section(body, "A", "");
        assert!(result.contains("## A"));
        assert!(result.contains("## B\n\nB body"));
        assert!(!result.contains("old"));
    }

    #[test]
    fn append_to_section_extends_existing() {
        let body = "## A\n\nfirst\n\n## B\n\nB body\n";
        let result = append_to_section(body, "A", "second");
        assert!(result.contains("## A\n\nfirst\n\nsecond"));
        assert!(result.contains("## B\n\nB body"));
    }

    #[test]
    fn append_to_section_creates_when_missing() {
        let body = "## A\n\nbody\n";
        let result = append_to_section(body, "C", "C body");
        assert!(result.contains("## A\n\nbody"));
        assert!(result.contains("## C\n\nC body"));
    }

    #[test]
    fn append_at_end_of_empty_body() {
        let result = append_section_at_end("", "Notes", "first note");
        assert_eq!(result, "## Notes\n\nfirst note\n");
    }

    #[test]
    fn append_at_end_of_nonempty_body() {
        let result = append_section_at_end("## Existing\n\nbody\n", "New", "new body");
        assert!(result.contains("## Existing\n\nbody"));
        assert!(result.ends_with("## New\n\nnew body\n"));
    }

    #[test]
    fn trailing_newline_normalized() {
        let body = "## A\n\nbody\n";
        let result = replace_section(body, "A", "new");
        assert!(result.ends_with('\n'));
        assert!(!result.ends_with("\n\n\n"));
    }

    #[test]
    fn handles_crlf_endings() {
        let body = "## A\r\n\r\nbody\r\n\r\n## B\r\n\r\nB body\r\n";
        let r = find_h2_section(body, "A").expect("should find with CRLF");
        // Heading line includes CRLF
        assert!(body[r.heading_start..r.content_start].starts_with("## A"));
    }

    #[test]
    fn whitespace_in_target_name_trimmed_for_match() {
        let body = "## Notes\n\nbody\n";
        assert!(find_h2_section(body, "  Notes  ").is_some());
    }
}
