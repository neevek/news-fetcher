mod config;
mod fetch;
mod filter;
mod model;
mod render;
mod store;
mod summarize;

use anyhow::Result;
use clap::Parser;
use config::Config;
use std::collections::HashSet;
use std::path::PathBuf;
use store::Store;

/// Monitor coding-agent news (Codex & Claude Code) and generate an HTML digest.
#[derive(Parser, Debug)]
#[command(name = "news-fetcher", version)]
struct Args {
    /// Path to the sources config file.
    #[arg(short, long, default_value = "sources.toml")]
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
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = Config::load(&args.config)?;
    let store = Store::open(std::path::Path::new(&cfg.settings.db_path))?;

    let mut new_ids: HashSet<String> = HashSet::new();

    if args.resummarize {
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

/// Re-enrich and re-summarize every stored item in place. This refreshes
/// cached content after the enrichment logic or summary prompt changes,
/// without re-fetching sources or resetting first-seen history.
fn resummarize_all(store: &Store, args: &Args) -> Result<()> {
    let mut items: Vec<model::NewsItem> = store.all()?.into_iter().map(|(it, _)| it).collect();
    if items.is_empty() {
        println!("Nothing stored to re-summarize.");
        return Ok(());
    }
    println!("Re-summarizing {} stored item(s)…", items.len());

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

    for src in &cfg.sources {
        match fetch::fetch_source(src) {
            Ok(items) => {
                let mut kept = 0;
                for item in items {
                    if !filter::is_relevant(&item, src, &cfg.settings.keywords) {
                        continue;
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
