use crate::model::NewsItem;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use std::path::Path;

/// SQLite-backed "seen items" store. This is what turns the tool into a
/// monitor: each run only treats not-yet-stored items as new.
pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        // SQLite won't create missing parent directories (e.g. the default
        // `~/.news-fetcher/`), so make them first.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating db directory {}", parent.display()))?;
            }
        }
        let conn = Connection::open(path).with_context(|| format!("opening db {}", path.display()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS items (
                id          TEXT PRIMARY KEY,
                source      TEXT NOT NULL,
                title       TEXT NOT NULL,
                url         TEXT NOT NULL,
                author      TEXT,
                published   TEXT,
                snippet     TEXT,
                title_zh    TEXT,
                summary     TEXT,
                body_md     TEXT,
                title_en    TEXT,
                summary_en  TEXT,
                body_md_en  TEXT,
                tags        TEXT,
                score       INTEGER,
                importance  INTEGER,
                editor_score   INTEGER,
                editor_reason  TEXT,
                first_seen  TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_items_published ON items(published);",
        )
        .context("creating schema")?;
        // Migrate older DBs that predate later columns. ALTER fails if the
        // column already exists, which is fine to ignore.
        for col in ["title_en", "summary_en", "body_md_en", "editor_reason"] {
            let _ = conn.execute(&format!("ALTER TABLE items ADD COLUMN {col} TEXT"), []);
        }
        let _ = conn.execute("ALTER TABLE items ADD COLUMN editor_score INTEGER", []);
        Ok(Store { conn })
    }

    pub fn contains(&self, id: &str) -> Result<bool> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(1) FROM items WHERE id = ?1", params![id], |r| r.get(0))?;
        Ok(n > 0)
    }

    /// Insert a freshly-seen item. `first_seen` is set to now.
    pub fn insert(&self, item: &NewsItem) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO items
                (id, source, title, url, author, published, snippet, title_zh, summary, body_md, title_en, summary_en, body_md_en, tags, score, importance, editor_score, editor_reason, first_seen)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
            params![
                item.id,
                item.source,
                item.title,
                item.url,
                item.author,
                item.published.map(|d| d.to_rfc3339()),
                item.snippet,
                item.title_zh,
                item.summary,
                item.body_md,
                item.title_en,
                item.summary_en,
                item.body_md_en,
                item.tags.join(","),
                item.score,
                item.importance,
                item.editor_score,
                item.editor_reason,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Overwrite the enrichable/summary fields of an existing item, keyed by
    /// id. Used by `--resummarize` to refresh cached content in place without
    /// disturbing `first_seen` (so "new" history and ordering are preserved).
    pub fn update(&self, item: &NewsItem) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET
                snippet = ?2, title_zh = ?3, summary = ?4, body_md = ?5,
                title_en = ?6, summary_en = ?7, body_md_en = ?8,
                tags = ?9, importance = ?10
             WHERE id = ?1",
            params![
                item.id,
                item.snippet,
                item.title_zh,
                item.summary,
                item.body_md,
                item.title_en,
                item.summary_en,
                item.body_md_en,
                item.tags.join(","),
                item.importance,
            ],
        )?;
        Ok(())
    }

    /// Persist the editorial ranking for an already-stored item: the
    /// day-relative `editor_score` and (for the day's lead) a short reason.
    /// Used by the `rank` phase after summarization; leaves every other field
    /// untouched so it can run independently of `insert`/`update`.
    pub fn set_editor_score(&self, id: &str, score: Option<i64>, reason: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET editor_score = ?2, editor_reason = ?3 WHERE id = ?1",
            params![id, score, reason],
        )?;
        Ok(())
    }

    /// All stored items, newest first (by published date, falling back to
    /// first_seen). The archive site renders a page for every day, so it needs
    /// the full set rather than a recent window.
    pub fn all(&self) -> Result<Vec<(NewsItem, DateTime<Utc>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source, title, url, author, published, snippet, title_zh, summary, body_md, title_en, summary_en, body_md_en, tags, score, importance, editor_score, editor_reason, first_seen
             FROM items
             ORDER BY COALESCE(published, first_seen) DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            let published: Option<String> = r.get(5)?;
            let tags: String = r.get(13)?;
            let first_seen: String = r.get(18)?;
            Ok((
                NewsItem {
                    id: r.get(0)?,
                    source: r.get(1)?,
                    title: r.get(2)?,
                    url: r.get(3)?,
                    author: r.get(4)?,
                    published: published.and_then(|s| DateTime::parse_from_rfc3339(&s).ok().map(|d| d.with_timezone(&Utc))),
                    snippet: r.get(6)?,
                    title_zh: r.get(7)?,
                    summary: r.get(8)?,
                    body_md: r.get(9)?,
                    title_en: r.get(10)?,
                    summary_en: r.get(11)?,
                    body_md_en: r.get(12)?,
                    tags: if tags.is_empty() { vec![] } else { tags.split(',').map(String::from).collect() },
                    score: r.get(14)?,
                    importance: r.get(15)?,
                    editor_score: r.get(16)?,
                    editor_reason: r.get(17)?,
                },
                first_seen,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (item, first_seen) = row?;
            let fs = DateTime::parse_from_rfc3339(&first_seen)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            out.push((item, fs));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique temp DB path per test (no tempfile dep). Cleaned up by the guard.
    struct TempDb(std::path::PathBuf);
    impl TempDb {
        fn new() -> TempDb {
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "nf-store-test-{}-{}.db",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_file(&p);
            TempDb(p)
        }
    }
    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
            }
        }
    }

    fn complete(url: &str) -> NewsItem {
        let mut it = NewsItem::new("Src", "Title", url);
        it.title_zh = Some("标题".into());
        it.summary = Some("导语".into());
        it.body_md = Some("正文".into());
        it.title_en = Some("Title".into());
        it.summary_en = Some("Lede".into());
        it.body_md_en = Some("Body".into());
        it.importance = Some(50);
        it
    }

    #[test]
    fn editor_score_round_trips_through_insert_and_read() {
        let db = TempDb::new();
        let store = Store::open(&db.0).unwrap();
        let mut it = complete("https://example.com/a");
        it.editor_score = Some(87);
        it.editor_reason = Some("lead: ships a usable workflow today".into());
        store.insert(&it).unwrap();

        let all = store.all().unwrap();
        let got = &all.iter().find(|(x, _)| x.id == it.id).unwrap().0;
        assert_eq!(got.editor_score, Some(87));
        assert_eq!(got.editor_reason.as_deref(), Some("lead: ships a usable workflow today"));
    }

    #[test]
    fn set_editor_score_updates_only_ranking_fields() {
        let db = TempDb::new();
        let store = Store::open(&db.0).unwrap();
        let it = complete("https://example.com/b");
        store.insert(&it).unwrap(); // inserted with editor_score = None

        store.set_editor_score(&it.id, Some(73), Some("lead reason")).unwrap();

        let all = store.all().unwrap();
        let got = &all.iter().find(|(x, _)| x.id == it.id).unwrap().0;
        assert_eq!(got.editor_score, Some(73));
        assert_eq!(got.editor_reason.as_deref(), Some("lead reason"));
        // Content fields are untouched by the ranking write.
        assert_eq!(got.importance, Some(50));
        assert_eq!(got.title_zh.as_deref(), Some("标题"));
    }
}
