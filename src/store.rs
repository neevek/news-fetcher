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
                tags        TEXT,
                score       INTEGER,
                importance  INTEGER,
                first_seen  TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_items_published ON items(published);",
        )
        .context("creating schema")?;
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
                (id, source, title, url, author, published, snippet, title_zh, summary, body_md, tags, score, importance, first_seen)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
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
                item.tags.join(","),
                item.score,
                item.importance,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Most recent items (by published date, falling back to first_seen),
    /// for rendering. Returns (item, first_seen) pairs.
    pub fn recent(&self, limit: usize) -> Result<Vec<(NewsItem, DateTime<Utc>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source, title, url, author, published, snippet, title_zh, summary, body_md, tags, score, importance, first_seen
             FROM items
             ORDER BY COALESCE(published, first_seen) DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            let published: Option<String> = r.get(5)?;
            let tags: String = r.get(10)?;
            let first_seen: String = r.get(13)?;
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
                    tags: if tags.is_empty() { vec![] } else { tags.split(',').map(String::from).collect() },
                    score: r.get(11)?,
                    importance: r.get(12)?,
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
