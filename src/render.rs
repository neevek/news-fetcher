use crate::model::NewsItem;
use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Utc};
use minijinja::{context, Environment};
use pulldown_cmark::{html, Options, Parser};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// Max items shown per day.
const PER_DAY: usize = 10;

#[derive(Serialize)]
struct ItemView {
    /// Zero-padded rank label, e.g. "01".
    rank: String,
    title: String,
    summary: String,
    /// Pre-rendered, trusted HTML from the Markdown body.
    body_html: String,
    tags: Vec<String>,
    source: String,
    author: Option<String>,
    published: String,
    score: Option<i64>,
    importance: i64,
    url: String,
    /// Short host label for the reference link, e.g. "github.com".
    host: String,
    is_new: bool,
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
    count: usize,
    new_count: usize,
    /// Site-root-relative permalink, e.g. "feeds/2026/05/24.html".
    href: String,
}

/// A full day's page: its descriptor plus the ranked items.
struct Day {
    link: DayLink,
    items: Vec<ItemView>,
}

/// Render the whole site under `output_dir`: one page per day at
/// `feeds/yyyy/MM/dd.html`, the latest day duplicated at `index.html`, plus
/// `.nojekyll` and (when set) `CNAME`. `new_ids` marks items first seen this run.
pub fn render_site(
    items: &[(NewsItem, DateTime<Utc>)],
    new_ids: &HashSet<String>,
    output_dir: &Path,
    custom_domain: Option<&str>,
) -> Result<()> {
    let days = build_days(items, new_ids);

    let mut env = Environment::new();
    // Autoescape everything; body_html is injected via the |safe filter.
    env.set_auto_escape_callback(|_| minijinja::AutoEscape::Html);
    env.add_template("digest", TEMPLATE).context("loading template")?;

    let rail: Vec<DayLink> = days.iter().map(|d| d.link.clone()).collect();
    let generated = Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
    let day_count = days.len();
    let new_total = new_ids.len();

    let render_page = |day: &Day, older: Option<&DayLink>, newer: Option<&DayLink>, root: &str| -> Result<String> {
        let tmpl = env.get_template("digest")?;
        let html = tmpl.render(context! {
            root => root,
            day => &day.link,
            items => &day.items,
            rail => &rail,
            older => older,
            newer => newer,
            generated => &generated,
            day_count => day_count,
            new_total => new_total,
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

    // days[0] is newest. Older = next index, newer = previous index.
    for (i, day) in days.iter().enumerate() {
        let older = days.get(i + 1).map(|d| &d.link);
        let newer = i.checked_sub(1).and_then(|j| days.get(j)).map(|d| &d.link);
        let html = render_page(day, older, newer, "../../../")?;
        let path = output_dir.join(&day.link.href);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, html).with_context(|| format!("writing {}", path.display()))?;
    }

    // index.html = the latest day, rendered at the site root.
    if let Some(latest) = days.first() {
        let older = days.get(1).map(|d| &d.link);
        let html = render_page(latest, older, None, "")?;
        let path = output_dir.join("index.html");
        std::fs::write(&path, html).with_context(|| format!("writing {}", path.display()))?;
    }

    Ok(())
}

/// Group items by day (newest first) and rank the top `PER_DAY` within each.
fn build_days(items: &[(NewsItem, DateTime<Utc>)], new_ids: &HashSet<String>) -> Vec<Day> {
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
        group.truncate(PER_DAY);

        let when0 = group.first().map(|(_, d)| *d).unwrap_or_else(Utc::now);
        let new_count = group.iter().filter(|(it, _)| new_ids.contains(&it.id)).count();
        let items: Vec<ItemView> = group
            .iter()
            .enumerate()
            .map(|(i, (it, when))| ItemView {
                rank: format!("{:02}", i + 1),
                title: it.title_zh.clone().unwrap_or_else(|| it.title.clone()),
                summary: it.summary.clone().unwrap_or_default(),
                body_html: markdown_to_html(it.body_md.as_deref().unwrap_or("")),
                tags: it.tags.clone(),
                source: it.source.clone(),
                author: it.author.clone(),
                published: when.format("%H:%M").to_string(),
                score: it.score,
                importance: it.importance.unwrap_or(0),
                host: host_of(&it.url),
                url: it.url.clone(),
                is_new: new_ids.contains(&it.id),
            })
            .collect();

        days.push(Day {
            link: DayLink {
                md: format!("{:02}-{:02}", when0.month(), when0.day()),
                dom: format!("{:02}", when0.day()),
                month: format!("{:02} / {}", when0.month(), when0.year()),
                weekday: weekday_zh(&when0).to_string(),
                count: items.len(),
                new_count,
                href: day_path(&date),
                date,
            },
            items,
        });
    }
    days
}

/// Site-root-relative permalink for a day, e.g. "2026-05-24" -> "feeds/2026/05/24.html".
fn day_path(date: &str) -> String {
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

const TEMPLATE: &str = include_str!("digest.html.j2");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_path_splits_date_into_nested_permalink() {
        assert_eq!(day_path("2026-05-24"), "feeds/2026/05/24.html");
        assert_eq!(day_path("2025-12-01"), "feeds/2025/12/01.html");
    }
}
