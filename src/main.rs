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
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = Config::load(&args.config)?;
    let store = Store::open(std::path::Path::new(&cfg.settings.db_path))?;

    let mut new_ids: HashSet<String> = HashSet::new();

    if !args.render_only {
        new_ids = ingest(&cfg, &store, &args)?;
    }

    let recent = store.recent(cfg.settings.max_items)?;
    let out = PathBuf::from(&cfg.settings.output_html);
    render::render_html(&recent, &new_ids, &out)?;
    println!(
        "Wrote {} ({} items, {} new this run).",
        out.display(),
        recent.len(),
        new_ids.len()
    );
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
