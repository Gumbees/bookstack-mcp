use std::time::Duration;

/// Access tokens expire after 24 hours.
/// Used by all database backends for token cleanup and retrieval.
pub const ACCESS_TOKEN_TTL: Duration = Duration::from_secs(86400);

/// Refresh tokens expire after 30 days.
/// Users only need to re-enter BookStack API credentials when the refresh token expires.
pub const REFRESH_TOKEN_TTL: Duration = Duration::from_secs(30 * 86400);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DbBackendType {
    Sqlite,
    Postgres,
}

impl DbBackendType {
    pub fn from_env() -> Self {
        match std::env::var("BSMCP_DB_BACKEND")
            .unwrap_or_else(|_| "sqlite".into())
            .to_lowercase()
            .as_str()
        {
            "postgres" | "postgresql" => Self::Postgres,
            _ => Self::Sqlite,
        }
    }
}
