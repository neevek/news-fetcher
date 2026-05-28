mod codex;
mod config;
mod fetch;
mod filter;
mod message;
mod model;
mod rank;
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
    command: Commands,

    /// Path to the config file. When omitted, uses `config.toml` next to the
    /// binary if present, else `~/.news-fetcher/config.toml`.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Fetch new items, summarize them, and regenerate the site. With no date
    /// flag, today is assumed.
    Update(UpdateArgs),
    /// Regenerate the site from the stored DB without fetching anything.
    Render,
    /// Re-enrich and re-summarize every stored item in place, then render. Use
    /// after changing enrichment or the summary prompt to refresh stale content.
    Resummarize(SummarizeArgs),
    /// Re-summarize only items that look degraded (Chinese title missing or
    /// equal to the original, or no English body), then render. Useful for
    /// healing legacy rows stored before the COMPLETE-or-nothing gate; fresh
    /// runs no longer produce degraded rows (they abort instead).
    Repair(SummarizeArgs),
    /// Print a plain-text IM digest (top-10 titles + deep-links) for one day.
    /// Reads the existing DB only; does not fetch or render.
    Digest(DigestArgs),
}

/// Date selection + summarizer options for the `update` command. The date flags
/// are mutually exclusive by precedence (date > yesterday > days > today).
#[derive(clap::Args, Debug)]
struct UpdateArgs {
    /// Only ingest items published today (UTC). The default when no date flag is given.
    #[arg(long)]
    today: bool,
    /// Only ingest items published yesterday (UTC).
    #[arg(long)]
    yesterday: bool,
    /// Only ingest items published on this date (UTC), e.g. 2026-05-26.
    #[arg(long, value_name = "YYYY-MM-DD")]
    date: Option<String>,
    /// Only ingest items from the last N UTC days (today and the N-1 days before it).
    #[arg(long, value_name = "N")]
    days: Option<i64>,
    /// Number of items to show per day on the rendered site.
    #[arg(long, value_name = "N", default_value_t = render::DEFAULT_PER_DAY)]
    top: usize,
    #[command(flatten)]
    summarize: SummarizeArgs,
}

/// Cross-cutting summarizer options, shared by `update`/`resummarize`/`repair`.
#[derive(clap::Args, Debug)]
struct SummarizeArgs {
    /// Model to pass to `codex exec -m`. Overrides `model` in [settings].
    #[arg(long)]
    model: Option<String>,
    /// Reasoning effort for codex (minimal/low/medium/high). Overrides
    /// `thinking` in [settings].
    #[arg(long)]
    thinking: Option<String>,
}

/// Day selection for the `digest` command (no `--days`: a digest is one day).
#[derive(clap::Args, Debug)]
struct DigestArgs {
    /// Summarize today (UTC).
    #[arg(long)]
    today: bool,
    /// Summarize yesterday (UTC).
    #[arg(long)]
    yesterday: bool,
    /// Day to summarize (UTC, YYYY-MM-DD). Defaults to the latest stored day.
    #[arg(long, value_name = "YYYY-MM-DD")]
    date: Option<String>,
    /// Number of top titles to list in the message.
    #[arg(long, value_name = "N", default_value_t = message::DEFAULT_TOP)]
    top: usize,
}

/// Start of the UTC calendar day containing `d`.
fn day_start(d: chrono::NaiveDate) -> DateTime<Utc> {
    d.and_hms_opt(0, 0, 0).unwrap().and_utc()
}

/// Parse a `YYYY-MM-DD` string, erroring with context on bad input.
fn parse_ymd(s: &str) -> Result<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .with_context(|| format!("parsing date {s:?} (expected YYYY-MM-DD)"))
}

impl UpdateArgs {
    /// Half-open UTC window [start, end) that an item's date must fall in to be
    /// ingested. Precedence: --date > --yesterday > --days > --today/default.
    fn window(&self) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
        if let Some(s) = &self.date {
            let start = day_start(parse_ymd(s)?);
            return Ok((start, start + chrono::Duration::days(1)));
        }
        if self.yesterday {
            let start = day_start(Utc::now().date_naive()) - chrono::Duration::days(1);
            return Ok((start, start + chrono::Duration::days(1)));
        }
        if let Some(n) = self.days {
            anyhow::ensure!(n >= 1, "--days must be >= 1 (got {n})");
            // Calendar-aligned, like --date/--yesterday: today and the N-1 days
            // before it → [day_start(today) - (N-1)d, day_start(today) + 1d).
            let today = day_start(Utc::now().date_naive());
            return Ok((today - chrono::Duration::days(n - 1), today + chrono::Duration::days(1)));
        }
        // --today, or no date flag at all: today.
        let start = day_start(Utc::now().date_naive());
        Ok((start, start + chrono::Duration::days(1)))
    }
}

impl DigestArgs {
    /// The single `YYYY-MM-DD` day to summarize, or `None` to fall back to the
    /// latest stored day. Precedence: --date > --yesterday > --today.
    fn day(&self) -> Result<Option<String>> {
        if let Some(s) = &self.date {
            parse_ymd(s)?; // validate
            return Ok(Some(s.clone()));
        }
        if self.yesterday {
            let d = Utc::now().date_naive() - chrono::Duration::days(1);
            return Ok(Some(d.format("%Y-%m-%d").to_string()));
        }
        if self.today {
            return Ok(Some(Utc::now().date_naive().format("%Y-%m-%d").to_string()));
        }
        Ok(None)
    }
}

impl SummarizeArgs {
    /// Effective model: CLI `--model` overrides `[settings] model`.
    fn model(&self, cfg: &Config) -> String {
        self.model.clone().unwrap_or_else(|| cfg.settings.model.clone())
    }
    /// Effective reasoning effort: CLI `--thinking` overrides `[settings] thinking`.
    fn thinking(&self, cfg: &Config) -> String {
        self.thinking.clone().unwrap_or_else(|| cfg.settings.thinking.clone())
    }
}

/// Build the daily IM digest message and print it to stdout. Read-only.
fn run_digest(cfg: &Config, store: &Store, date: Option<String>, top: usize) -> Result<()> {
    anyhow::ensure!(top >= 1, "--top must be >= 1");
    let base_url = cfg.settings.site_base_url()?;
    let all = store.all()?;
    let date = match date {
        Some(d) => d,
        None => message::latest_day(&all)
            .context("no complete items to build a digest from (run `repair` if the store has degraded rows)")?,
    };
    let msg = message::build_message(&all, &date, &base_url, top)?;
    print!("{msg}");
    Ok(())
}

/// Resolve the config file to load, in priority order:
///   1. `--config <path>` if given (explicit override always wins),
///   2. `config.toml` sitting next to the binary, if it exists,
///   3. the default `~/.news-fetcher/config.toml` (tilde expanded).
fn config_path(args: &Args) -> PathBuf {
    if let Some(p) = &args.config {
        return p.clone();
    }
    if let Some(p) = binary_dir_config() {
        return p;
    }
    PathBuf::from(config::expand_tilde("~/.news-fetcher/config.toml"))
}

/// `config.toml` in the directory holding the running binary, if that file
/// exists. Note `current_exe()` resolves symlinks, so for a symlinked or
/// installed binary this is the real install dir, not the symlink's location.
fn binary_dir_config() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join("config.toml");
    candidate.exists().then_some(candidate)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = Config::load(&config_path(&args))?;
    let store = Store::open(std::path::Path::new(&cfg.settings.db_path))?;

    // `digest` is read-only: it neither ingests nor renders, so return early.
    if let Commands::Digest(d) = &args.command {
        return run_digest(&cfg, &store, d.day()?, d.top);
    }

    // Items per day on the site: from `update --top`, else the default. Only
    // `update` carries the flag; the other render commands keep the default so
    // re-rendering stays byte-stable.
    let per_day = match &args.command {
        Commands::Update(a) => a.top,
        _ => render::DEFAULT_PER_DAY,
    };
    anyhow::ensure!(per_day >= 1, "--top must be >= 1");

    let mut new_count = 0usize;
    match &args.command {
        Commands::Update(a) => {
            new_count = ingest(&cfg, &store, a.window()?, &a.summarize)?.len();
        }
        Commands::Resummarize(s) => resummarize_all(&cfg, &store, false, s)?,
        Commands::Repair(s) => resummarize_all(&cfg, &store, true, s)?,
        Commands::Render => {}
        Commands::Digest(_) => unreachable!("handled above"),
    }

    // The archive renders a page per day, so it needs every stored item.
    let all = store.all()?;
    let out_dir = PathBuf::from(&cfg.settings.output_dir);
    render::render_site(&all, &out_dir, per_day, cfg.settings.custom_domain.as_deref())?;
    println!(
        "Wrote site to {}/ ({} items, {} new this run).",
        out_dir.display(),
        all.len(),
        new_count
    );
    Ok(())
}

/// An item is "degraded" if its LLM summary failed and fell back to raw text:
/// the Chinese title is missing or identical to the original (English) title,
/// or the English digest body/standfirst never got generated.
///
/// This is the looser `repair`-selector heuristic for *legacy* rows (it also
/// flags an untranslated Chinese title), distinct from the strict
/// [`NewsItem::is_complete`] write/render gate (which only checks non-empty
/// fields). They serve different purposes, so they're deliberately not mirrors.
fn is_degraded(it: &model::NewsItem) -> bool {
    it.title_zh.as_deref().is_none_or(|z| z == it.title)
        || it.body_md_en.as_deref().unwrap_or("").is_empty()
        || it.summary_en.as_deref().unwrap_or("").is_empty()
}

/// Re-enrich and re-summarize every stored item in place. This refreshes
/// cached content after the enrichment logic or summary prompt changes,
/// without re-fetching sources or resetting first-seen history.
fn resummarize_all(cfg: &Config, store: &Store, repair: bool, sum: &SummarizeArgs) -> Result<()> {
    let mut items: Vec<model::NewsItem> = store.all()?.into_iter().map(|(it, _)| it).collect();
    if repair {
        // Repair both the legacy "degraded" rows (untranslated title / missing
        // English body — which `is_complete` would still accept) and any row
        // that simply isn't complete. The union leaves no row in limbo: never
        // healed by `repair` yet silently dropped at render.
        items.retain(|it| is_degraded(it) || !it.is_complete());
        if items.is_empty() {
            println!("No items need repair.");
            return Ok(());
        }
        println!("Repairing {} item(s) needing re-summarization…", items.len());
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

    // COMPLETE-or-nothing: if codex can't fully summarize every item, abort
    // before touching the store rather than overwrite good rows with degraded
    // ones. The error propagates and the run exits non-zero.
    summarize::summarize(&mut items, &sum.model(cfg), &sum.thinking(cfg))?;

    for it in &items {
        store.update(it)?;
    }

    // Re-rank every day these items belong to: `update` clears per-item
    // importance, so the prior editorial scores are stale and must be recomputed
    // against the freshly-summarized day.
    let changed: HashSet<String> = items.iter().map(|it| it.id.clone()).collect();
    let mut all = store.all()?;
    rank::rank_days(store, &mut all, &changed, &sum.model(cfg), &sum.thinking(cfg))?;
    Ok(())
}

/// Fetch all sources, filter, dedupe against the store, summarize the new
/// ones, and persist them. Returns the set of newly-seen item ids.
fn ingest(
    cfg: &Config,
    store: &Store,
    window: (DateTime<Utc>, DateTime<Utc>),
    sum: &SummarizeArgs,
) -> Result<HashSet<String>> {
    let mut new_items: Vec<model::NewsItem> = Vec::new();
    let mut seen_this_run: HashSet<String> = HashSet::new();

    // UTC date window the items must fall in. Sources return the last ~10 days.
    let (start, end) = window;
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
                    let when = item.published.unwrap_or(now);
                    if when < start || when >= end {
                        out_of_window += 1;
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

    println!(
        "Date filter {}..{} → skipped {} out-of-window item(s).",
        start.format("%Y-%m-%d"),
        (end - chrono::Duration::days(1)).format("%Y-%m-%d"),
        out_of_window
    );
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

        // COMPLETE-or-nothing: codex must return a full bilingual digest for
        // every item or the run aborts here — nothing gets stored, the site is
        // left untouched, and the error is reported (non-zero exit). This is
        // what keeps a half-translated, title-only day from ever publishing.
        summarize::summarize(&mut new_items, &sum.model(cfg), &sum.thinking(cfg))?;
    }

    let mut new_ids = HashSet::new();
    for item in &new_items {
        store.insert(item)?;
        new_ids.insert(item.id.clone());
    }

    // Editorial pass: compare each affected day's items as a whole and assign a
    // calibrated, day-relative score so the site/digest can name a real lead.
    if !new_ids.is_empty() {
        let mut all = store.all()?;
        rank::rank_days(store, &mut all, &new_ids, &sum.model(cfg), &sum.thinking(cfg))?;
    }
    Ok(new_ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(today: bool, yesterday: bool, date: Option<&str>, days: Option<i64>) -> UpdateArgs {
        UpdateArgs {
            today,
            yesterday,
            date: date.map(String::from),
            days,
            top: render::DEFAULT_PER_DAY,
            summarize: SummarizeArgs { model: None, thinking: None },
        }
    }

    fn digest(today: bool, yesterday: bool, date: Option<&str>) -> DigestArgs {
        DigestArgs { today, yesterday, date: date.map(String::from), top: message::DEFAULT_TOP }
    }

    #[test]
    fn window_date_is_the_exact_utc_day() {
        let (start, end) = update(false, false, Some("2026-05-20"), None).window().unwrap();
        assert_eq!(start.to_rfc3339(), "2026-05-20T00:00:00+00:00");
        assert_eq!(end.to_rfc3339(), "2026-05-21T00:00:00+00:00");
    }

    #[test]
    fn window_date_beats_every_other_flag() {
        // --date set alongside --today/--yesterday/--days: --date still wins.
        let (start, _) = update(true, true, Some("2026-05-20"), Some(9)).window().unwrap();
        assert_eq!(start.to_rfc3339(), "2026-05-20T00:00:00+00:00");
    }

    #[test]
    fn window_yesterday_beats_days() {
        // Precedence: --yesterday outranks --days, so a 1-day window (not 9).
        let (start, end) = update(false, true, None, Some(9)).window().unwrap();
        assert_eq!(end - start, chrono::Duration::days(1));
    }

    #[test]
    fn window_yesterday_is_one_full_day_at_midnight() {
        let (start, end) = update(false, true, None, None).window().unwrap();
        assert_eq!(start, end - chrono::Duration::days(1));
        assert_eq!(start, day_start(start.date_naive())); // aligned to 00:00
        assert_eq!(end, day_start(Utc::now().date_naive())); // ends at today 00:00
    }

    #[test]
    fn window_days_must_be_positive() {
        assert!(update(false, false, None, Some(0)).window().is_err());
        assert!(update(false, false, None, Some(-1)).window().is_err());
    }

    #[test]
    fn window_days_is_n_calendar_days_ending_today() {
        // --days 5 → today + the 4 prior UTC days, both ends at midnight.
        let today = day_start(Utc::now().date_naive());
        let (start, end) = update(false, false, None, Some(5)).window().unwrap();
        assert_eq!(end - start, chrono::Duration::days(5));
        assert_eq!(end, today + chrono::Duration::days(1));
        assert_eq!(start, today - chrono::Duration::days(4));
    }

    #[test]
    fn window_default_and_today_are_todays_utc_day() {
        for a in [update(false, false, None, None), update(true, false, None, None)] {
            let (start, end) = a.window().unwrap();
            assert_eq!(start, day_start(Utc::now().date_naive()));
            assert_eq!(end - start, chrono::Duration::days(1));
        }
    }

    #[test]
    fn digest_day_precedence_and_default() {
        // --date wins and is passed through verbatim.
        assert_eq!(digest(true, true, Some("2026-05-20")).day().unwrap().as_deref(), Some("2026-05-20"));
        // --yesterday and --today resolve to distinct, non-empty days.
        let y = digest(false, true, None).day().unwrap();
        let t = digest(true, false, None).day().unwrap();
        assert!(y.is_some() && t.is_some() && y != t);
        // No flag → None (caller falls back to the latest stored day).
        assert_eq!(digest(false, false, None).day().unwrap(), None);
    }

    #[test]
    fn digest_rejects_malformed_date() {
        assert!(digest(false, false, Some("2026/05/20")).day().is_err());
    }

    /// A healthy, fully-translated item.
    fn healthy() -> model::NewsItem {
        let mut it = model::NewsItem::new("Src", "Original EN", "https://example.com/a");
        it.title_zh = Some("中文标题".into());
        it.summary = Some("导语".into());
        it.body_md = Some("正文".into());
        it.title_en = Some("Editorial EN".into());
        it.summary_en = Some("Lede".into());
        it.body_md_en = Some("Body".into());
        it.importance = Some(70);
        it
    }

    /// The repair selector: a row needs repair if it's degraded or incomplete.
    fn needs_repair(it: &model::NewsItem) -> bool {
        is_degraded(it) || !it.is_complete()
    }

    #[test]
    fn healthy_row_needs_no_repair() {
        let it = healthy();
        assert!(!is_degraded(&it) && it.is_complete());
        assert!(!needs_repair(&it));
    }

    #[test]
    fn untranslated_title_is_degraded_but_complete() {
        // Legacy offline row: every field filled, but the "Chinese" title is
        // the original English. is_complete accepts it; is_degraded catches it.
        let mut it = healthy();
        it.title_zh = Some(it.title.clone());
        assert!(it.is_complete());
        assert!(is_degraded(&it));
        assert!(needs_repair(&it));
    }

    #[test]
    fn incomplete_row_is_repaired_even_when_not_degraded() {
        // Translated title and English body/standfirst present (so is_degraded
        // is false), but a missing field makes it incomplete. The union must
        // still select it — otherwise it's healed by nothing yet dropped at render.
        let mut it = healthy();
        it.summary = None;
        assert!(!is_degraded(&it));
        assert!(!it.is_complete());
        assert!(needs_repair(&it));
    }
}
