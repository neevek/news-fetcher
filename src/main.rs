mod config;
mod fetch;
mod filter;
mod message;
mod model;
mod render;
mod store;
mod summarize;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use config::Config;
use std::collections::HashSet;
use std::path::PathBuf;
use store::Store;

/// Monitor coding-agent news (Codex & Claude Code) and generate an HTML digest.
#[derive(Parser, Debug)]
#[command(name = "news-fetcher", version)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to the sources config file.
    #[arg(short, long, global = true, default_value = "sources.toml")]
    config: PathBuf,

    /// Skip the LLM summarizer (use raw snippets instead of calling codex).
    #[arg(long)]
    no_summarize: bool,

    /// Model to pass to `codex exec -m` (optional).
    #[arg(long)]
    model: Option<String>,

    /// Re-render the HTML from the existing DB without fetching new items.
    #[arg(long)]
    render_only: bool,

    /// Re-enrich and re-summarize every item already in the DB (in place),
    /// then render. Use after changing enrichment or the summary prompt to
    /// refresh stale content without losing "new"/first-seen history.
    #[arg(long)]
    resummarize: bool,

    /// Re-summarize only items that look degraded (an LLM summary failed and
    /// fell back to raw text). Cheap way to repair a few items after a
    /// transient codex failure, without redoing the whole DB.
    #[arg(long)]
    repair: bool,

    /// Only ingest items published today (UTC). Sources return the last ~10
    /// days of items by default; this narrows ingestion to a single day.
    #[arg(long)]
    today: bool,

    /// Only ingest items published on this date (UTC), e.g. 2026-05-26.
    #[arg(long, value_name = "YYYY-MM-DD")]
    date: Option<String>,

    /// Only ingest items published within the last N days (UTC).
    #[arg(long, value_name = "N")]
    days: Option<i64>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Print a plain-text daily digest (top-10 titles + deep-links) for an IM
    /// channel. Reads the existing DB only; does not fetch or render.
    Digest {
        /// Day to summarize (UTC, YYYY-MM-DD). Defaults to the latest day with
        /// stored items (the day index.html shows).
        #[arg(long, value_name = "YYYY-MM-DD")]
        date: Option<String>,
    },
}

/// Build the daily IM digest message and print it to stdout. Read-only.
fn run_digest(cfg: &Config, store: &Store, date: Option<String>) -> Result<()> {
    let base_url = cfg.settings.site_base_url()?;
    let all = store.all()?;
    let date = match date {
        Some(d) => d,
        None => message::latest_day(&all).context("no stored items to build a digest from")?,
    };
    let msg = message::build_message(&all, &date, &base_url)?;
    print!("{msg}");
    Ok(())
}

/// Half-open UTC time window [start, end) that an item's date must fall in to
/// be ingested. Built from the --today / --date / --days flags; `None` means no
/// date filtering (keep everything the sources return).
fn ingest_window(args: &Args) -> Result<Option<(DateTime<Utc>, DateTime<Utc>)>> {
    let day_start = |d: chrono::NaiveDate| d.and_hms_opt(0, 0, 0).unwrap().and_utc();
    if let Some(s) = &args.date {
        let d = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .with_context(|| format!("parsing --date {s:?} (expected YYYY-MM-DD)"))?;
        let start = day_start(d);
        Ok(Some((start, start + chrono::Duration::days(1))))
    } else if args.today {
        let start = day_start(Utc::now().date_naive());
        Ok(Some((start, start + chrono::Duration::days(1))))
    } else if let Some(n) = args.days {
        let now = Utc::now();
        Ok(Some((now - chrono::Duration::days(n), now + chrono::Duration::days(1))))
    } else {
        Ok(None)
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = Config::load(&args.config)?;
    let store = Store::open(std::path::Path::new(&cfg.settings.db_path))?;

    if let Some(Commands::Digest { date }) = args.command {
        return run_digest(&cfg, &store, date);
    }

    let mut new_ids: HashSet<String> = HashSet::new();

    if args.resummarize || args.repair {
        resummarize_all(&store, &args)?;
    } else if !args.render_only {
        new_ids = ingest(&cfg, &store, &args)?;
    }

    // The archive renders a page per day, so it needs every stored item.
    let all = store.all()?;
    let out_dir = PathBuf::from(&cfg.settings.output_dir);
    render::render_site(&all, &new_ids, &out_dir, cfg.settings.custom_domain.as_deref())?;
    println!(
        "Wrote site to {}/ ({} items, {} new this run).",
        out_dir.display(),
        all.len(),
        new_ids.len()
    );
    Ok(())
}

/// An item is "degraded" if its LLM summary failed and fell back to raw text:
/// the Chinese title is missing or identical to the original (English) title,
/// or the English digest body/standfirst never got generated.
fn is_degraded(it: &model::NewsItem) -> bool {
    it.title_zh.as_deref().map_or(true, |z| z == it.title)
        || it.body_md_en.as_deref().unwrap_or("").is_empty()
        || it.summary_en.as_deref().unwrap_or("").is_empty()
}

/// Re-enrich and re-summarize every stored item in place. This refreshes
/// cached content after the enrichment logic or summary prompt changes,
/// without re-fetching sources or resetting first-seen history.
fn resummarize_all(store: &Store, args: &Args) -> Result<()> {
    let mut items: Vec<model::NewsItem> = store.all()?.into_iter().map(|(it, _)| it).collect();
    if args.repair {
        items.retain(is_degraded);
        if items.is_empty() {
            println!("No degraded items to repair.");
            return Ok(());
        }
        println!("Repairing {} degraded item(s)…", items.len());
    } else {
        if items.is_empty() {
            println!("Nothing stored to re-summarize.");
            return Ok(());
        }
        println!("Re-summarizing {} stored item(s)…", items.len());
    }

    // Re-pull real content for thin items before summarizing.
    let thin = items
        .iter()
        .filter(|i| i.snippet.trim().chars().count() < 220)
        .count();
    if thin > 0 {
        eprintln!("Enriching {thin} thin item(s) from their links…");
        for it in &mut items {
            fetch::enrich(it);
        }
    }

    // Clear cached LLM fields so the summarizer regenerates rather than
    // treating them as already-filled.
    for it in &mut items {
        it.title_zh = None;
        it.summary = None;
        it.body_md = None;
        it.title_en = None;
        it.summary_en = None;
        it.body_md_en = None;
        it.tags.clear();
        it.importance = None;
    }

    if args.no_summarize {
        summarize::summarize_offline(&mut items);
    } else if let Err(e) = summarize::summarize(&mut items, args.model.as_deref()) {
        eprintln!("Summarization failed ({e:#}); falling back to raw snippets.");
        summarize::summarize_offline(&mut items);
    }

    for it in &items {
        store.update(it)?;
    }
    Ok(())
}

/// Fetch all sources, filter, dedupe against the store, summarize the new
/// ones, and persist them. Returns the set of newly-seen item ids.
fn ingest(cfg: &Config, store: &Store, args: &Args) -> Result<HashSet<String>> {
    let mut new_items: Vec<model::NewsItem> = Vec::new();
    let mut seen_this_run: HashSet<String> = HashSet::new();

    // Optional date window from --today / --date / --days. Sources return the
    // last ~10 days of items; without a window we keep them all.
    let window = ingest_window(args)?;
    let now = Utc::now();
    let mut out_of_window = 0usize;

    for src in &cfg.sources {
        match fetch::fetch_source(src) {
            Ok(items) => {
                let mut kept = 0;
                for item in items {
                    if !filter::is_relevant(&item, src, &cfg.settings.keywords) {
                        continue;
                    }
                    // Drop items whose published date is outside the window.
                    // Items with no date are treated as "now" (seen today).
                    if let Some((start, end)) = window {
                        let when = item.published.unwrap_or(now);
                        if when < start || when >= end {
                            out_of_window += 1;
                            continue;
                        }
                    }
                    // Dedupe within this run and against the DB.
                    if !seen_this_run.insert(item.id.clone()) {
                        continue;
                    }
                    if store.contains(&item.id)? {
                        continue;
                    }
                    new_items.push(item);
                    kept += 1;
                }
                eprintln!("  {}: {} new", src.name, kept);
            }
            Err(e) => eprintln!("  {}: ERROR {e:#}", src.name),
        }
    }

    if let Some((start, end)) = window {
        println!(
            "Date filter {}..{} → skipped {} out-of-window item(s).",
            start.format("%Y-%m-%d"),
            (end - chrono::Duration::days(1)).format("%Y-%m-%d"),
            out_of_window
        );
    }
    println!("Fetched {} new items total.", new_items.len());

    if !new_items.is_empty() {
        // Pull real content for title-only / thin items so summaries have
        // substance to work with instead of meta-filler.
        let thin = new_items
            .iter()
            .filter(|i| i.snippet.trim().chars().count() < 220)
            .count();
        if thin > 0 {
            eprintln!("Enriching {thin} thin item(s) from their links…");
            for it in &mut new_items {
                fetch::enrich(it);
            }
        }

        if args.no_summarize {
            summarize::summarize_offline(&mut new_items);
        } else {
            match summarize::summarize(&mut new_items, args.model.as_deref()) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("Summarization failed ({e:#}); falling back to raw snippets.");
                    summarize::summarize_offline(&mut new_items);
                }
            }
        }
    }

    let mut new_ids = HashSet::new();
    for item in &new_items {
        store.insert(item)?;
        new_ids.insert(item.id.clone());
    }
    Ok(new_ids)
}
