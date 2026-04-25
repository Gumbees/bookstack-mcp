#[derive(Clone, Debug)]
pub struct PageMeta {
    pub page_id: i64,
    pub book_id: i64,
    pub chapter_id: Option<i64>,
    pub name: String,
    pub slug: String,
    pub content_hash: String,
    /// ISO 8601 timestamp from BookStack API (e.g. "2025-03-10T14:30:00.000000Z")
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ChunkInsert {
    pub chunk_index: usize,
    pub heading_path: String,
    pub content: String,
    pub content_hash: String,
    pub embedding: Vec<f32>,
}

#[derive(Clone, Debug)]
pub struct ChunkDetail {
    pub chunk_id: i64,
    pub page_id: i64,
    pub heading_path: String,
    pub content: String,
    pub page_name: String,
}

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub chunk_id: i64,
    pub page_id: i64,
    pub score: f32,
}

#[derive(Clone, Debug, Default)]
pub struct MarkovBlanket {
    pub linked_from: Vec<RelatedPage>,
    pub links_to: Vec<RelatedPage>,
    pub co_linked: Vec<RelatedPage>,
    pub siblings: Vec<RelatedPage>,
}

#[derive(Clone, Debug)]
pub struct RelatedPage {
    pub page_id: i64,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct EmbedJob {
    pub id: i64,
    pub scope: String,
    pub status: String,
    pub total_pages: i64,
    pub done_pages: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
    pub worker_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct EmbedStats {
    pub total_pages: i64,
    pub total_chunks: i64,
    pub latest_job: Option<EmbedJob>,
}

/// One write/read record from the /remember protocol audit log.
#[derive(Clone, Debug)]
pub struct AuditEntry {
    pub id: i64,
    pub token_id_hash: String,
    pub ai_identity_ouid: Option<String>,
    pub user_id: Option<String>,
    pub resource: String,
    pub action: String,
    pub target_page_id: Option<i64>,
    pub target_key: Option<String>,
    pub success: bool,
    pub error: Option<String>,
    pub trace_id: Option<String>,
    pub occurred_at: i64,
}

/// Insert payload for an audit entry — same fields as AuditEntry minus id.
#[derive(Clone, Debug)]
pub struct AuditEntryInsert {
    pub token_id_hash: String,
    pub ai_identity_ouid: Option<String>,
    pub user_id: Option<String>,
    pub resource: String,
    pub action: String,
    pub target_page_id: Option<i64>,
    pub target_key: Option<String>,
    pub success: bool,
    pub error: Option<String>,
    pub trace_id: Option<String>,
}
