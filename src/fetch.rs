use crate::config::Source;
use crate::model::NewsItem;
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

const USER_AGENT: &str = "Mozilla/5.0 (compatible; news-fetcher/0.1; coding-agent news monitor)";
const TIMEOUT_SECS: u64 = 30;

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
        .user_agent(USER_AGENT)
        .build()
}

/// Dispatch a source to the right fetcher. Errors are returned so the caller
/// can log-and-continue rather than aborting the whole run.
pub fn fetch_source(src: &Source) -> Result<Vec<NewsItem>> {
    match src.kind.as_str() {
        "github_releases" => {
            let repo = src.repo.as_deref().ok_or_else(|| missing("repo", src))?;
            github_releases(&src.name, repo)
        }
        "hackernews" => {
            let q = src.query.as_deref().ok_or_else(|| missing("query", src))?;
            hackernews(&src.name, q)
        }
        "reddit" => {
            let sub = src
                .subreddit
                .as_deref()
                .ok_or_else(|| missing("subreddit", src))?;
            reddit(&src.name, sub)
        }
        "rss" => {
            let url = src.url.as_deref().ok_or_else(|| missing("url", src))?;
            rss(&src.name, url)
        }
        other => Err(anyhow!("unknown source kind '{}' for '{}'", other, src.name)),
    }
}

fn missing(field: &str, src: &Source) -> anyhow::Error {
    anyhow!("source '{}' (kind {}) is missing '{}'", src.name, src.kind, field)
}

fn get_json(url: &str) -> Result<Value> {
    let body = agent()
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?
        .into_string()
        .context("reading response body")?;
    serde_json::from_str(&body).with_context(|| format!("parsing JSON from {url}"))
}

/// GitHub Releases API → one item per release.
fn github_releases(source: &str, repo: &str) -> Result<Vec<NewsItem>> {
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=10");
    let v = get_json(&url)?;
    let arr = v.as_array().ok_or_else(|| anyhow!("expected array from {url}"))?;
    let mut out = Vec::new();
    for r in arr {
        let html_url = r["html_url"].as_str().unwrap_or_default();
        if html_url.is_empty() {
            continue;
        }
        let title = r["name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| r["tag_name"].as_str())
            .unwrap_or("(untitled release)")
            .to_string();
        let mut item = NewsItem::new(source, title, html_url);
        item.snippet = truncate(r["body"].as_str().unwrap_or_default(), 1200);
        item.author = r["author"]["login"].as_str().map(String::from);
        item.published = r["published_at"].as_str().and_then(parse_rfc3339);
        out.push(item);
    }
    Ok(out)
}

/// HN Algolia search (newest first), no API key needed.
fn hackernews(source: &str, query: &str) -> Result<Vec<NewsItem>> {
    let url = format!(
        "https://hn.algolia.com/api/v1/search_by_date?query={}&tags=story&hitsPerPage=30",
        urlencode(query)
    );
    let v = get_json(&url)?;
    let hits = v["hits"].as_array().ok_or_else(|| anyhow!("no hits in HN response"))?;
    let mut out = Vec::new();
    for h in hits {
        let object_id = h["objectID"].as_str().unwrap_or_default();
        let title = h["title"].as_str().unwrap_or_default();
        if object_id.is_empty() || title.is_empty() {
            continue;
        }
        // Prefer the submitted link; fall back to the HN discussion page.
        let url = h["url"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| format!("https://news.ycombinator.com/item?id={object_id}"));
        let mut item = NewsItem::new(source, title, url);
        item.author = h["author"].as_str().map(String::from);
        item.score = h["points"].as_i64();
        item.snippet = truncate(h["story_text"].as_str().unwrap_or_default(), 600);
        item.published = h["created_at"].as_str().and_then(parse_rfc3339);
        out.push(item);
    }
    Ok(out)
}

/// Subreddit "new" listing. Reddit's JSON API now requires OAuth, but the
/// public Atom feed is still open, so we use that.
fn reddit(source: &str, subreddit: &str) -> Result<Vec<NewsItem>> {
    let url = format!("https://www.reddit.com/r/{subreddit}/new/.rss?limit=30");
    rss(source, &url)
}

/// Generic RSS/Atom feed via feed-rs.
fn rss(source: &str, url: &str) -> Result<Vec<NewsItem>> {
    let body = agent()
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?
        .into_string()
        .context("reading feed body")?;
    let feed = feed_rs::parser::parse(body.as_bytes())
        .with_context(|| format!("parsing feed {url}"))?;
    let mut out = Vec::new();
    for e in feed.entries {
        let link = e.links.first().map(|l| l.href.clone()).unwrap_or_default();
        if link.is_empty() {
            continue;
        }
        let title = e.title.map(|t| t.content).unwrap_or_else(|| "(untitled)".into());
        let mut item = NewsItem::new(source, title, link);
        item.published = e.published.or(e.updated).map(|d| d.with_timezone(&Utc));
        item.author = e.authors.first().map(|a| a.name.clone());
        let body = e
            .summary
            .map(|s| s.content)
            .or_else(|| e.content.and_then(|c| c.body))
            .unwrap_or_default();
        item.snippet = truncate(&strip_html(&body), 800);
        out.push(item);
    }
    Ok(out)
}

// ---- content enrichment ----------------------------------------------------

/// Below this many chars of body text, an item is too thin to summarize well,
/// so we go fetch the linked page (or HN thread) for real content.
const ENRICH_MIN_CHARS: usize = 220;
const ENRICH_TIMEOUT_SECS: u64 = 12;
const ENRICH_MAX_BYTES: u64 = 800_000;

fn enrich_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(ENRICH_TIMEOUT_SECS))
        .user_agent(USER_AGENT)
        .build()
}

/// Best-effort: if an item arrived with little or no body, pull readable text
/// from its link so the summarizer has real material. Failures are ignored —
/// the item simply keeps whatever snippet it had.
pub fn enrich(item: &mut NewsItem) {
    if item.snippet.trim().chars().count() >= ENRICH_MIN_CHARS {
        return;
    }
    let fetched = if host_of(&item.url) == "news.ycombinator.com" {
        hn_thread_text(&item.url)
    } else {
        readable_text(&item.url)
    };
    if let Some(text) = fetched {
        let text = text.trim();
        // Only replace if we actually gained content.
        if text.chars().count() > item.snippet.trim().chars().count() {
            item.snippet = truncate(text, 3000);
        }
    }
}

/// Fetch a URL and reduce its HTML to plain readable text. Caps the response
/// size and skips obvious non-HTML responses.
fn readable_text(url: &str) -> Option<String> {
    if !url.starts_with("http") {
        return None;
    }
    let resp = enrich_agent().get(url).call().ok()?;
    let ctype = resp.header("content-type").unwrap_or("").to_string();
    if !ctype.is_empty() && !ctype.contains("html") && !ctype.contains("text") {
        return None; // PDF, image, JSON API, etc.
    }
    let mut buf = Vec::new();
    use std::io::Read;
    resp.into_reader()
        .take(ENRICH_MAX_BYTES)
        .read_to_end(&mut buf)
        .ok()?;
    let body = String::from_utf8_lossy(&buf);
    let text = html_to_text(&body);
    (text.chars().count() >= 80).then_some(text)
}

/// HN discussion fallback: pull the post text plus top comments via the
/// Algolia item API (clean JSON, no page chrome).
fn hn_thread_text(url: &str) -> Option<String> {
    let id: String = url
        .rsplit("id=")
        .next()?
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if id.is_empty() {
        return None;
    }
    let v = get_json(&format!("https://hn.algolia.com/api/v1/items/{id}")).ok()?;
    let mut out = String::new();
    collect_hn_text(&v, &mut out);
    let text = strip_html(&out);
    (text.chars().count() >= 40).then_some(text)
}

/// Depth-first gather of `text` fields from an HN item and its comment tree,
/// stopping once we have enough material.
fn collect_hn_text(v: &Value, out: &mut String) {
    if out.chars().count() > 3000 {
        return;
    }
    if let Some(t) = v["text"].as_str() {
        if !t.is_empty() {
            out.push_str("\n- ");
            out.push_str(t);
        }
    }
    if let Some(children) = v["children"].as_array() {
        for c in children.iter().take(8) {
            collect_hn_text(c, out);
        }
    }
}

/// Strip `<script>`/`<style>`/etc. blocks, prefer the main article region over
/// page chrome, then strip all tags and collapse whitespace.
fn html_to_text(html: &str) -> String {
    let cleaned = remove_blocks(
        html,
        &["script", "style", "noscript", "svg", "head", "nav", "footer", "aside", "form"],
    );
    let region = main_region(&cleaned).unwrap_or(cleaned);
    strip_html(&region)
}

/// If the page marks its main content with `<article>`/`<main>`, return just
/// that span so menus/footers don't crowd out the real text.
fn main_region(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    for tag in ["article", "main"] {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        if let (Some(s), Some(e)) = (lower.find(&open), lower.rfind(&close)) {
            if e > s && e - s > 200 {
                return Some(html[s..e].to_string());
            }
        }
    }
    None
}

/// Remove whole `<tag …>…</tag>` blocks (case-insensitive). Unclosed tags drop
/// to end-of-string. Bounded input keeps this simple scan cheap enough.
fn remove_blocks(html: &str, tags: &[&str]) -> String {
    let mut s = html.to_string();
    for tag in tags {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        loop {
            let lower = s.to_lowercase();
            let Some(start) = lower.find(&open) else { break };
            let end = match lower[start..].find(&close) {
                Some(rel) => start + rel + close.len(),
                None => s.len(),
            };
            s.replace_range(start..end, " ");
        }
    }
    s
}

fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .trim_start_matches("www.")
        .to_string()
}

// ---- small helpers ---------------------------------------------------------

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}

/// Minimal HTML tag stripper for feed bodies.
fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_drops_scripts_and_tags() {
        let html = "<html><head><title>x</title><style>.a{color:red}</style></head>\
            <body><nav>menu menu</nav><script>var a = 1; doStuff();</script>\
            <article><h1>Claude Code 2.1</h1><p>Adds <code>--resume</code> support.</p></article>\
            </body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Claude Code 2.1"));
        assert!(text.contains("--resume support"));
        // script/style contents must be gone
        assert!(!text.contains("doStuff"));
        assert!(!text.contains("color:red"));
    }

    #[test]
    #[ignore = "hits the network"]
    fn enrich_fetches_real_article() {
        let mut it = NewsItem::new("test", "Example Domain", "https://example.com");
        assert!(it.snippet.is_empty());
        enrich(&mut it);
        assert!(
            it.snippet.to_lowercase().contains("example domain"),
            "expected fetched body, got: {:?}",
            it.snippet
        );
    }

    #[test]
    fn host_of_strips_scheme_and_www() {
        assert_eq!(host_of("https://www.example.com/a/b"), "example.com");
        assert_eq!(host_of("https://news.ycombinator.com/item?id=42"), "news.ycombinator.com");
    }
}

/// URL-encode a query string component (spaces and common reserved chars).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
