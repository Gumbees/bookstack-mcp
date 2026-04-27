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
    UserIdentityBook,
    UserIdentityPage,
    UserJournalBook,
    UserJournalAgentPage,
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
            // Per-user names default to placeholder text — `default_name_for_user`
            // returns the actual personalized name when a user_id is known.
            Self::UserIdentityBook => "User Identity",
            Self::UserIdentityPage => "Identity",
            Self::UserJournalBook => "Journal",
            Self::UserJournalAgentPage => "Agent: journal-agent",
        }
    }

    /// Personalized name for per-user resources. The non-personalized variants
    /// just delegate to `default_name`. Stable across runs as long as `user_id`
    /// is unchanged — used so probe + auto-create can find existing books.
    pub fn default_name_for_user(self, user_id: &str) -> String {
        match self {
            Self::UserIdentityBook => format!("{user_id} — Identity"),
            Self::UserIdentityPage => "Identity".to_string(),
            Self::UserJournalBook => format!("{user_id} — Journal"),
            Self::UserJournalAgentPage => format!("Agent: {user_id}-journal-agent"),
            _ => self.default_name().to_string(),
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
                "AI agent's identity manifest. Auto-created by /remember.",
            Self::IdentityPage =>
                "Manifest page that defines who this AI agent is. Auto-created by /remember.",
            Self::JournalBook =>
                "AI agent's daily journal entries, organized by YYYY-MM chapters. Auto-created by /remember.",
            Self::CollageBook =>
                "AI agent's active topics / collage entries. Auto-created by /remember.",
            Self::SharedCollageBook =>
                "Cross-agent shared topics. Auto-created by /remember.",
            Self::UserIdentityBook =>
                "Per-user identity container — holds the human user's identity page + their personal sub-agent definitions. Auto-created by /remember.",
            Self::UserIdentityPage =>
                "Identity page describing the human user — preferences, role, communication style. Keep it updated as you learn more. Auto-created by /remember.",
            Self::UserJournalBook =>
                "User's personal journal entries, organized by YYYY-MM chapters. Auto-created by /remember.",
            Self::UserJournalAgentPage =>
                "Agent definition for the user's per-user journal-agent. Auto-created by /remember.",
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
            // Per-user resources match by suffix because the prefix is the
            // user_id which can't be encoded statically.
            Self::UserIdentityBook => n.ends_with("— identity") || n.ends_with("- identity"),
            Self::UserIdentityPage => matches!(n.as_str(), "identity" | "about me" | "who am i"),
            Self::UserJournalBook => n.ends_with("— journal") || n.ends_with("- journal") || n == "journal",
            Self::UserJournalAgentPage => n.starts_with("agent:") && n.contains("journal-agent"),
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
    }

    #[test]
    fn find_match_returns_first_hit() {
        let items = [(10, "Random"), (20, "Identity"), (30, "Identity Backup")];
        let matched = find_match(items.iter().map(|(i, n)| (*i, *n)), NamedResource::IdentityBook);
        assert_eq!(matched, Some(20));
    }
}
