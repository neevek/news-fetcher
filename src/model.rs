use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

/// A single normalized news item from any source.
#[derive(Debug, Clone)]
pub struct NewsItem {
    /// Stable unique id, derived from the canonical url (used for dedupe).
    pub id: String,
    /// Human-readable source label, e.g. "Claude Code Releases".
    pub source: String,
    pub title: String,
    pub url: String,
    pub author: Option<String>,
    pub published: Option<DateTime<Utc>>,
    /// Raw excerpt / body before summarization.
    pub snippet: String,
    /// LLM-translated Chinese title (product/version names kept in English).
    pub title_zh: Option<String>,
    /// One-line Chinese standfirst/lede; None until summarized.
    pub summary: Option<String>,
    /// Thorough Chinese article body in Markdown (lists, code fences, etc.).
    pub body_md: Option<String>,
    /// LLM editorial English title (clearer rewrite of the original headline).
    pub title_en: Option<String>,
    /// One-line English standfirst/lede; None until summarized.
    pub summary_en: Option<String>,
    /// English article body in Markdown, mirroring `body_md`'s structure.
    pub body_md_en: Option<String>,
    pub tags: Vec<String>,
    /// Engagement score where available (HN points, etc.).
    pub score: Option<i64>,
    /// LLM-assigned importance (0-100); drives the per-day top-10 ranking.
    pub importance: Option<i64>,
}

impl NewsItem {
    pub fn new(source: impl Into<String>, title: impl Into<String>, url: impl Into<String>) -> Self {
        let url = url.into();
        NewsItem {
            id: hash_id(&url),
            source: source.into(),
            title: title.into(),
            url,
            author: None,
            published: None,
            snippet: String::new(),
            title_zh: None,
            summary: None,
            body_md: None,
            title_en: None,
            summary_en: None,
            body_md_en: None,
            tags: Vec::new(),
            score: None,
            importance: None,
        }
    }
}

/// Short, stable hash of a string — used as the dedupe primary key.
pub fn hash_id(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())[..16].to_string()
}
