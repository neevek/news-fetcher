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
    /// LLM-assigned importance (0-100), scored per item in isolation. Used as
    /// the ranking fallback before/without an editorial pass.
    pub importance: Option<i64>,
    /// Day-relative editorial score (0-100) from the comparative `rank` pass,
    /// which sees the whole day's items at once. When present it drives ranking
    /// (the day's #1 is the highest); `None` until a day has been ranked.
    pub editor_score: Option<i64>,
    /// One-line reason the editor named this item the day's lead. Only the #1
    /// item carries it; informational, not used for sorting.
    pub editor_reason: Option<String>,
}

impl NewsItem {
    /// True once the LLM has produced a full bilingual digest: a Chinese title,
    /// standfirst, and body, their English mirrors, and an importance score —
    /// every field non-empty. This is the COMPLETE-or-nothing gate: an item
    /// that is not complete must never be stored or rendered. There is no
    /// offline fallback, so an incomplete item means codex failed and the run
    /// must abort.
    ///
    /// `tags` are intentionally not required: they're optional metadata, and an
    /// empty tag list is a valid, complete item.
    pub fn is_complete(&self) -> bool {
        fn filled(o: &Option<String>) -> bool {
            o.as_deref().is_some_and(|s| !s.trim().is_empty())
        }
        filled(&self.title_zh)
            && filled(&self.summary)
            && filled(&self.body_md)
            && filled(&self.title_en)
            && filled(&self.summary_en)
            && filled(&self.body_md_en)
            && self.importance.is_some()
    }

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
            editor_score: None,
            editor_reason: None,
        }
    }
}

/// Short, stable hash of a string — used as the dedupe primary key.
pub fn hash_id(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully-summarized item, as codex is required to produce.
    fn complete() -> NewsItem {
        let mut it = NewsItem::new("src", "Title", "https://example.com");
        it.title_zh = Some("中文标题".into());
        it.summary = Some("中文导语".into());
        it.body_md = Some("正文".into());
        it.title_en = Some("English title".into());
        it.summary_en = Some("English lede".into());
        it.body_md_en = Some("Body".into());
        it.importance = Some(80);
        it
    }

    #[test]
    fn complete_item_is_complete() {
        assert!(complete().is_complete());
    }

    #[test]
    fn missing_any_field_is_incomplete() {
        // Each LLM field is mandatory: drop one at a time, expect incomplete.
        let setters: Vec<fn(&mut NewsItem)> = vec![
            |i| i.title_zh = None,
            |i| i.summary = None,
            |i| i.body_md = None,
            |i| i.title_en = None,
            |i| i.summary_en = None,
            |i| i.body_md_en = None,
            |i| i.importance = None,
        ];
        for clear in setters {
            let mut it = complete();
            clear(&mut it);
            assert!(!it.is_complete());
        }
    }

    #[test]
    fn blank_or_whitespace_field_is_incomplete() {
        let mut it = complete();
        it.body_md = Some("   ".into());
        assert!(!it.is_complete());
    }
}
