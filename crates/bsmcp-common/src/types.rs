#[derive(Clone, Debug)]
pub struct PageMeta {
    pub page_id: i64,
    pub book_id: i64,
    pub chapter_id: Option<i64>,
    pub name: String,
    pub slug: String,
    pub content_hash: String,
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

#[derive(Clone, Debug)]
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
