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
                first_seen  TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_items_published ON items(published);",
        )
        .context("creating schema")?;
        // Migrate older DBs that predate the English-digest columns. ALTER
        // fails if the column already exists, which is fine to ignore.
        for col in ["title_en", "summary_en", "body_md_en"] {
            let _ = conn.execute(&format!("ALTER TABLE items ADD COLUMN {col} TEXT"), []);
        }
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
                (id, source, title, url, author, published, snippet, title_zh, summary, body_md, title_en, summary_en, body_md_en, tags, score, importance, first_seen)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
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

    /// All stored items, newest first (by published date, falling back to
    /// first_seen). The archive site renders a page for every day, so it needs
    /// the full set rather than a recent window.
    pub fn all(&self) -> Result<Vec<(NewsItem, DateTime<Utc>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source, title, url, author, published, snippet, title_zh, summary, body_md, title_en, summary_en, body_md_en, tags, score, importance, first_seen
             FROM items
             ORDER BY COALESCE(published, first_seen) DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            let published: Option<String> = r.get(5)?;
            let tags: String = r.get(13)?;
            let first_seen: String = r.get(16)?;
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
