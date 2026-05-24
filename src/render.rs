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

#[derive(Serialize)]
struct DayView {
    /// Anchor target, e.g. "d-2026-05-24".
    anchor: String,
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
    items: Vec<ItemView>,
}

/// Render the digest: items grouped by day, top `PER_DAY` per day ranked by
/// importance. `new_ids` marks items first seen during this run.
pub fn render_html(
    items: &[(NewsItem, DateTime<Utc>)],
    new_ids: &HashSet<String>,
    out_path: &Path,
) -> Result<()> {
    // Group by the item's day (published date, falling back to first_seen).
    let mut by_day: BTreeMap<String, Vec<(&NewsItem, DateTime<Utc>)>> = BTreeMap::new();
    for (it, first_seen) in items {
        let day = it.published.unwrap_or(*first_seen);
        let key = day.format("%Y-%m-%d").to_string();
        by_day.entry(key).or_default().push((it, day));
    }

    // Newest day first; within a day, highest importance first.
    let mut days: Vec<DayView> = Vec::new();
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

        days.push(DayView {
            anchor: format!("d-{date}"),
            md: format!("{:02}-{:02}", when0.month(), when0.day()),
            dom: format!("{:02}", when0.day()),
            month: format!("{:02} / {}", when0.month(), when0.year()),
            weekday: weekday_zh(&when0).to_string(),
            count: items.len(),
            new_count,
            date,
            items,
        });
    }

    let mut env = Environment::new();
    // Autoescape everything; body_html is injected via the |safe filter.
    env.set_auto_escape_callback(|_| minijinja::AutoEscape::Html);
    env.add_template("digest", TEMPLATE).context("loading template")?;
    let tmpl = env.get_template("digest")?;
    let html = tmpl.render(context! {
        days => days,
        generated => Utc::now().format("%Y-%m-%d %H:%M UTC").to_string(),
        new_count => new_ids.len(),
        day_count => days.len(),
    })?;

    std::fs::write(out_path, html).with_context(|| format!("writing {}", out_path.display()))?;
    Ok(())
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
