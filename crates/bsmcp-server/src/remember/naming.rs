//! Standard naming convention for the Hive memory flow.
//!
//! Used by:
//! - The /settings probe endpoint (find existing Hive structure by name)
//! - The auto-provision path (default names for newly created books/chapters)
//! - Identity discovery (find manifest pages within Identity books)
//!
//! All matching is case-insensitive. Patterns intentionally simple — users who
//! need fancier matching can manually set IDs on /settings.

/// Logical resource the convention covers. Each variant has a default name
/// (used for auto-create) and a set of name patterns (used for detection).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamedResource {
    HiveShelf,
    UserJournalsShelf,
    IdentityBook,
    IdentityPage,
    JournalBook,
    CollageBook,
    SharedCollageBook,
    SubagentsChapter,
    ConnectionsChapter,
    OpportunitiesChapter,
    ActivityChapter,
}

impl NamedResource {
    /// Default name used when auto-creating this resource.
    pub fn default_name(self) -> &'static str {
        match self {
            Self::HiveShelf => "Hive",
            Self::UserJournalsShelf => "User Journals",
            Self::IdentityBook => "Identity",
            Self::IdentityPage => "Identity",
            Self::JournalBook => "Journal",
            Self::CollageBook => "Topics",
            Self::SharedCollageBook => "Shared Topics",
            Self::SubagentsChapter => "Subagents",
            Self::ConnectionsChapter => "Connections",
            Self::OpportunitiesChapter => "Opportunities",
            Self::ActivityChapter => "Activity",
        }
    }

    /// Default description seeded on auto-create.
    pub fn default_description(self) -> &'static str {
        match self {
            Self::HiveShelf =>
                "Shared shelf containing every AI agent's Identity book. Auto-created by /remember.",
            Self::UserJournalsShelf =>
                "Shared shelf containing each human user's journal book. Auto-created by /remember.",
            Self::IdentityBook =>
                "AI agent's identity manifest plus structured chapters about themselves (Connections, Opportunities, Subagents). Auto-created by /remember.",
            Self::IdentityPage =>
                "Manifest page that defines who this AI agent is. Auto-created by /remember.",
            Self::JournalBook =>
                "AI agent's daily journal entries, organized by YYYY-MM chapters. Auto-created by /remember.",
            Self::CollageBook =>
                "AI agent's active topics / collage entries. Auto-created by /remember.",
            Self::SharedCollageBook =>
                "Cross-agent shared topics. Auto-created by /remember.",
            Self::SubagentsChapter =>
                "Subagent definition pages. Auto-created by /remember.",
            Self::ConnectionsChapter =>
                "People and agents the AI has met. Auto-created by /remember.",
            Self::OpportunitiesChapter =>
                "Financial / actionable opportunities the AI is tracking. Auto-created by /remember.",
            Self::ActivityChapter =>
                "Append-only feed of conversations, social events, etc. Sits before the date chapters. Auto-created by /remember.",
        }
    }

    /// Returns true if `candidate_name` matches this resource by the convention.
    /// Match is case-insensitive after trimming whitespace.
    pub fn matches(self, candidate_name: &str) -> bool {
        let n = candidate_name.trim().to_lowercase();
        match self {
            Self::HiveShelf => matches!(n.as_str(), "hive" | "the hive"),
            Self::UserJournalsShelf => matches!(n.as_str(), "user journals" | "journals" | "user journal"),
            Self::IdentityBook => n == "identity",
            Self::IdentityPage => matches!(n.as_str(), "identity" | "manifest" | "who am i"),
            Self::JournalBook => n == "journal",
            Self::CollageBook => matches!(n.as_str(), "topics" | "collage"),
            Self::SharedCollageBook => matches!(n.as_str(), "shared topics" | "shared collage"),
            Self::SubagentsChapter => matches!(n.as_str(), "subagent" | "subagents"),
            Self::ConnectionsChapter => n == "connections",
            Self::OpportunitiesChapter => n == "opportunities",
            Self::ActivityChapter => n == "activity",
        }
    }
}

/// Find the first item in `items` whose name matches `resource`. Each item is
/// a tuple `(id, name)`. Returns the matched id.
#[allow(dead_code)] // public helper; reserved for future probe/discovery callers
pub fn find_match<'a, I>(items: I, resource: NamedResource) -> Option<i64>
where
    I: IntoIterator<Item = (i64, &'a str)>,
{
    items
        .into_iter()
        .find(|(_, name)| resource.matches(name))
        .map(|(id, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_case_insensitive() {
        assert!(NamedResource::IdentityBook.matches("Identity"));
        assert!(NamedResource::IdentityBook.matches("IDENTITY"));
        assert!(NamedResource::IdentityBook.matches("  identity  "));
        assert!(!NamedResource::IdentityBook.matches("Identitys"));
    }

    #[test]
    fn matches_alternates() {
        assert!(NamedResource::CollageBook.matches("Topics"));
        assert!(NamedResource::CollageBook.matches("Collage"));
        assert!(!NamedResource::CollageBook.matches("Topic"));

        assert!(NamedResource::SubagentsChapter.matches("Subagents"));
        assert!(NamedResource::SubagentsChapter.matches("Subagent"));
    }

    #[test]
    fn find_match_returns_first_hit() {
        let items = [(10, "Random"), (20, "Identity"), (30, "Identity Backup")];
        let matched = find_match(items.iter().map(|(i, n)| (*i, *n)), NamedResource::IdentityBook);
        assert_eq!(matched, Some(20));
    }
}
