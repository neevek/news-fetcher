use crate::model::NewsItem;
use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Utc};
use minijinja::{context, Environment};
use pulldown_cmark::{html, Options, Parser};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Default number of items shown per day on a generated page; overridable via
/// the `update --top <N>` flag.
pub const DEFAULT_PER_DAY: usize = 20;

#[derive(Serialize)]
struct ItemView {
    /// Stable item id, used as the `#`-anchor target on the page.
    id: String,
    /// Zero-padded rank label, e.g. "01".
    rank: String,
    /// Chinese (translated) title shown in 中文 mode.
    title_zh: String,
    /// Original (usually English) title shown in EN mode.
    title_en: String,
    summary: String,
    /// One-line English standfirst shown in EN mode.
    summary_en: String,
    /// Pre-rendered Chinese article body (from the Markdown summary).
    body_html: String,
    /// Pre-rendered English article body (parallel English digest).
    body_html_en: String,
    tags: Vec<String>,
    source: String,
    author: Option<String>,
    published: String,
    score: Option<i64>,
    importance: i64,
    url: String,
    /// Short host label for the reference link, e.g. "github.com".
    host: String,
}

/// Lightweight day descriptor used for the chip rail and prev/next links.
#[derive(Serialize, Clone)]
struct DayLink {
    date: String,
    /// Compact "MM-DD" label for the date-nav chips.
    md: String,
    /// Day-of-month numeral for the big rail label.
    dom: String,
    /// Month + year, e.g. "05 / 2026".
    month: String,
    weekday: String,
    weekday_en: String,
    count: usize,
    /// Site-root-relative permalink, e.g. "feeds/2026/05/24.html".
    href: String,
}

/// A full day's page: its descriptor plus the ranked items.
struct Day {
    link: DayLink,
    items: Vec<ItemView>,
}

/// Render the whole site under `output_dir`: one page per day at
/// `feeds/yyyy/MM/dd.html`, the latest day duplicated at `index.html`, plus a
/// `days.js` data file (the day list, consumed client-side to build the
/// navigation rail), `.nojekyll`, and (when set) `CNAME`.
///
/// Each page's HTML is a pure function of *its own day's items* — the cross-day
/// rail / prev-next / counters live only in `days.js` and are assembled in the
/// browser. So re-rendering an unchanged day yields byte-identical output, and a
/// daily run only changes the affected day's page, `index.html`, and `days.js`.
pub fn render_site(
    items: &[(NewsItem, DateTime<Utc>)],
    output_dir: &Path,
    per_day: usize,
    custom_domain: Option<&str>,
) -> Result<()> {
    let days = build_days(items, per_day);

    let mut env = Environment::new();
    // Autoescape everything; body_html is injected via the |safe filter.
    env.set_auto_escape_callback(|_| minijinja::AutoEscape::Html);
    env.add_template("digest", TEMPLATE).context("loading template")?;

    let render_page = |day: &Day, root: &str| -> Result<String> {
        let tmpl = env.get_template("digest")?;
        let html = tmpl.render(context! {
            root => root,
            day => &day.link,
            items => &day.items,
        })?;
        Ok(html)
    };

    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("creating {}", output_dir.display()))?;
    // Serve files as-is; skip Jekyll processing on GitHub Pages.
    std::fs::write(output_dir.join(".nojekyll"), "")?;
    if let Some(domain) = custom_domain {
        std::fs::write(output_dir.join("CNAME"), format!("{domain}\n"))
            .context("writing CNAME")?;
    }

    // The day list, newest-first, for the client-side nav rail. Deterministic:
    // no run timestamp, so identical content re-emits byte-for-byte.
    let rail: Vec<&DayLink> = days.iter().map(|d| &d.link).collect();
    std::fs::write(
        output_dir.join("days.js"),
        format!("window.NF_DAYS={};", serde_json::to_string(&rail)?),
    )
    .context("writing days.js")?;

    for day in &days {
        let html = render_page(day, "../../../")?;
        let path = output_dir.join(&day.link.href);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, html).with_context(|| format!("writing {}", path.display()))?;
    }

    // index.html = the latest day, rendered at the site root.
    if let Some(latest) = days.first() {
        let html = render_page(latest, "")?;
        let path = output_dir.join("index.html");
        std::fs::write(&path, html).with_context(|| format!("writing {}", path.display()))?;
    }

    Ok(())
}

/// Group items by day (newest first) and rank the top `per_day` within each.
fn build_days(items: &[(NewsItem, DateTime<Utc>)], per_day: usize) -> Vec<Day> {
    let mut by_day: BTreeMap<String, Vec<(&NewsItem, DateTime<Utc>)>> = BTreeMap::new();
    for (it, first_seen) in items {
        let day = it.published.unwrap_or(*first_seen);
        let key = day.format("%Y-%m-%d").to_string();
        by_day.entry(key).or_default().push((it, day));
    }

    let mut days: Vec<Day> = Vec::new();
    for (date, mut group) in by_day.into_iter().rev() {
        group.sort_by(|a, b| {
            b.0.importance
                .unwrap_or(0)
                .cmp(&a.0.importance.unwrap_or(0))
                .then(b.1.cmp(&a.1))
        });
        group.truncate(per_day);

        // Groups are non-empty by construction; never fabricate a wall-clock
        // date here, which would make the page's bytes non-deterministic.
        let when0 = match group.first() {
            Some((_, d)) => *d,
            None => continue,
        };
        let items: Vec<ItemView> = group
            .iter()
            .enumerate()
            .map(|(i, (it, when))| ItemView {
                id: it.id.clone(),
                rank: format!("{:02}", i + 1),
                title_zh: it.title_zh.clone().unwrap_or_else(|| it.title.clone()),
                title_en: it.title_en.clone().unwrap_or_else(|| it.title.clone()),
                summary: it.summary.clone().unwrap_or_default(),
                summary_en: it.summary_en.clone().unwrap_or_default(),
                body_html: markdown_to_html(it.body_md.as_deref().unwrap_or("")),
                body_html_en: {
                    let en = it.body_md_en.as_deref().unwrap_or("");
                    let s = it.snippet.trim();
                    markdown_to_html(if !en.is_empty() { en } else if !s.is_empty() { s } else { &it.title })
                },
                tags: it.tags.clone(),
                source: it.source.clone(),
                author: it.author.clone(),
                published: when.format("%H:%M").to_string(),
                score: it.score,
                importance: it.importance.unwrap_or(0),
                host: host_of(&it.url),
                url: it.url.clone(),
            })
            .collect();

        days.push(Day {
            link: DayLink {
                md: format!("{:02}-{:02}", when0.month(), when0.day()),
                dom: format!("{:02}", when0.day()),
                month: format!("{:02} / {}", when0.month(), when0.year()),
                weekday: weekday_zh(&when0).to_string(),
                weekday_en: weekday_en(&when0).to_string(),
                count: items.len(),
                href: day_path(&date),
                date,
            },
            items,
        });
    }
    days
}

/// Site-root-relative permalink for a day, e.g. "2026-05-24" -> "feeds/2026/05/24.html".
pub(crate) fn day_path(date: &str) -> String {
    let mut p = date.split('-');
    let y = p.next().unwrap_or("");
    let m = p.next().unwrap_or("");
    let d = p.next().unwrap_or("");
    format!("feeds/{y}/{m}/{d}.html")
}

fn markdown_to_html(md: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(md, opts);
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
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

fn weekday_zh(d: &DateTime<Utc>) -> &'static str {
    match d.weekday().num_days_from_monday() {
        0 => "周一",
        1 => "周二",
        2 => "周三",
        3 => "周四",
        4 => "周五",
        5 => "周六",
        _ => "周日",
    }
}

fn weekday_en(d: &DateTime<Utc>) -> &'static str {
    match d.weekday().num_days_from_monday() {
        0 => "Mon",
        1 => "Tue",
        2 => "Wed",
        3 => "Thu",
        4 => "Fri",
        5 => "Sat",
        _ => "Sun",
    }
}

const TEMPLATE: &str = include_str!("digest.html.j2");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_path_splits_date_into_nested_permalink() {
        assert_eq!(day_path("2026-05-24"), "feeds/2026/05/24.html");
        assert_eq!(day_path("2025-12-01"), "feeds/2025/12/01.html");
    }

    #[test]
    fn build_days_carries_item_id_as_anchor() {
        let it = NewsItem::new("Src", "Title", "https://example.com/a");
        let expected_id = it.id.clone();
        let now = Utc::now();
        let days = build_days(&[(it, now)], DEFAULT_PER_DAY);
        assert_eq!(days[0].items[0].id, expected_id);
    }
}
