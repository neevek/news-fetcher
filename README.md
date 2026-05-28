# news-fetcher

A small Rust tool that monitors **coding-agent industry news** — focused on
OpenAI **Codex** and Anthropic **Claude Code** — and generates a static HTML
site. Each run fetches from configured sources, keeps only new and relevant
items, summarizes them with the **Codex CLI**, and renders a static site under
`docs/`.

The site is a **navigable archive**: `docs/index.html` shows the latest day, and
every stored day gets its own permalink at `docs/feeds/yyyy/MM/dd.html`. Each page
shows the **top 20 items for that day**, ranked by a **day-level editorial pass**
that compares the whole day's items at once and assigns a calibrated, day-relative
score — so the **#1 is the day's lead story**, not just the highest isolated score
(change the count with `update --top <N>`; `--top 1` surfaces only the lead).
Each item has a one-line Chinese standfirst
plus a **thorough Chinese article body rendered from Markdown** (lists, headings,
inline code, and fenced code blocks), with the **reference link at the end**;
product names, versions, commands, and code identifiers are kept in English.

The pages are an *editorial-terminal* design: a **sticky date rail** linking
across days (current day highlighted) plus **← prev / next →** navigation and a
back-to-top button, **syntax-highlighted code** (highlight.js),
and a **mobile/desktop-friendly** layout that defaults to **dark mode** with a
**light-mode toggle**. Fonts (Fraunces / Noto Serif SC / Noto Sans SC / JetBrains
Mono) and highlight.js are loaded from CDNs, so code highlighting and the display
typefaces need network access when the page is *viewed* (the rest renders
offline).

The cross-day navigation (rail, prev/next, day count) is built **client-side**
from a small `days.js` data file, so each day's page is a pure function of *its
own items* — re-rendering an unchanged day produces byte-identical HTML, and a
daily run only touches the day that changed, `index.html`, and `days.js`.

## How it works

```
config.toml ──► fetchers (GitHub Releases / Hacker News / Reddit / RSS)
                      │
                      ▼
        relevance filter ──► dedupe against SQLite "seen" store
                      │
                      ▼
        new items ──► codex exec (batched, structured JSON) ──► summaries + tags
                      │
                      ▼
        SQLite store ──► minijinja template ──► docs/ (index.html + feeds/yyyy/MM/dd.html)
```

The SQLite store (`news.db`) is what makes this a *monitor* rather than a
scraper: every run only treats not-yet-stored items as new (reported as the
"N new this run" console summary) and persists them, so re-running skips
already-seen items.

## Sources

Configured via a `config.toml` (copy [`config.toml.example`](config.toml.example)
to get started). The file is resolved in this order: `--config <path>` if given,
else `config.toml` next to the binary, else `~/.news-fetcher/config.toml`.
Supported `kind` values:

| kind              | needs        | notes                                            |
|-------------------|--------------|--------------------------------------------------|
| `github_releases` | `repo`       | GitHub Releases API, e.g. `anthropics/claude-code` |
| `hackernews`      | `query`      | HN Algolia search (no API key)                   |
| `reddit`          | `subreddit`  | Public Atom feed (`/r/<sub>/new/.rss`)           |
| `rss`             | `url`        | Any RSS/Atom feed                                |

Mark a topic-specific source with `always_relevant = true` to keep every item;
otherwise items must match one of the `keywords` in `[settings]`.

## Build & run

```sh
cargo build --release
```

The CLI is organized into **verb subcommands** (a verb is required — a bare
`news-fetcher` prints help):

| command       | what it does                                                      |
|---------------|-------------------------------------------------------------------|
| `update`      | Fetch new items, summarize with codex, regenerate the site        |
| `render`      | Regenerate the site from the stored DB (no fetching)              |
| `resummarize` | Re-enrich + re-summarize **every** stored item, then render      |
| `repair`      | Re-summarize only **degraded** items, then render                |
| `digest`      | Print a plain-text IM digest for one day (read-only)             |

```sh
# Fetch today's items, summarize with codex, and write the site to docs/:
# (with no date flag, --today is assumed)
./target/release/news-fetcher update
# then open docs/index.html

# Other date windows for `update`:
./target/release/news-fetcher update --yesterday          # just yesterday (UTC)
./target/release/news-fetcher update --date 2026-05-20    # one specific day
./target/release/news-fetcher update --days 7             # the last 7 days

# Show more (or fewer) items per day on the site (default 20):
./target/release/news-fetcher update --top 30

# Re-render HTML from the existing DB without fetching:
./target/release/news-fetcher render

# Override the model / reasoning effort (on any summarizing command):
./target/release/news-fetcher update --model gpt-5.1-codex --thinking high

# --config is a global flag, before the subcommand:
./target/release/news-fetcher --config ./config.toml.example update
```

The date flags are **mutually exclusive by precedence** (`--date` > `--yesterday`
> `--days` > `--today`); pass one. `--model` and `--thinking`
apply to `update`, `resummarize`, and `repair`. `--top <N>` (default 20) sets the
items shown per day; only `update` takes it — `render`/`resummarize`/`repair`
always use the default, so a `render` after `update --top 30` rewrites every page
back to 20. Keep `--top` consistent (or leave it default) to preserve minimal diffs.

### Daily IM digest (`digest` subcommand)

`news-fetcher digest` prints a plain-text daily message — the day's **top 10
titles** (Chinese, count set by `--top <N>`) each with a **deep-link** into the
published site — ready to paste or pipe into Telegram, Discord, or any chat app.
It reads the existing DB only (no fetching, no rendering), so it's fast and safe
to run after publishing.

```sh
# Latest stored day (the day index.html shows):
./target/release/news-fetcher digest

# A specific day (UTC), or yesterday / today:
./target/release/news-fetcher digest --date 2026-05-26
./target/release/news-fetcher digest --yesterday
./target/release/news-fetcher digest --today

# List more (or fewer) titles (default 10):
./target/release/news-fetcher digest --top 15
```

The per-item links point at the site's **item anchors** (e.g.
`…/feeds/2026/05/26.html#<id>`), which scroll straight to that item — every card
on a generated page has a stable `#`-anchor you can also grab by clicking its
rank number. Building absolute links needs a site root: `digest` uses
`base_url` from `[settings]` if set, otherwise derives `https://{custom_domain}`,
and errors if neither is configured. It also exits non-zero when the chosen day
has no items, so a cron job won't post an empty message.

### Summarization (Codex CLI)

Summaries are produced by **batched** `codex exec` calls (6 items per call)
that request structured JSON — Chinese title, thorough summary, highlights,
tags, and a 0–100 importance score — enforced via `--output-schema`. Requires
the [`codex` CLI](https://developers.openai.com/codex) installed and
authenticated.

### Editorial ranking

`importance` is scored per item in isolation (within each batch), so it can't
reliably name a day's lead. After summarization, a second **editorial pass**
shows `codex` the **whole day's** candidates at once — editorial title,
lede, source, tags, engagement score, and per-item importance — and asks it, as
the publication's editor, to rank them comparatively, collapse near-duplicates,
weigh source authority (official release > blog > forum chatter), and assign a
**day-relative `editor_score` (0–100)** with a one-line reason for the lead. The
site and the IM digest both sort by this score (falling back to `importance`,
then time), so they agree on #1.

It runs only on the days a run actually touched (`update` = the new items' days,
`resummarize`/`repair` = the affected days), so a daily run is one extra `codex`
call. Unlike summarization it is **best-effort**: if the pass fails after one
retry, that day keeps its importance order and the run still publishes. `render`
and `digest` never call `codex` — they read the persisted score, so re-renders
stay byte-stable.

Summarization is **complete-or-nothing**: there is no offline/raw-snippet
fallback. If a chunk errors, is missing codex, or exceeds the 600s timeout, it
is retried once and then split in half to isolate the offending item (each
sub-chunk is likewise retried, so a poison item can trigger several calls before
it's pinpointed); if any single item still can't be fully summarized, the
**whole run aborts** with a non-zero exit — nothing is stored and the site is
left untouched. This guarantees a
half-translated, title-only day is never published. The failed items are simply
re-fetched and retried on the next run. (Legacy rows from before this gate can
be healed in place with `repair`.)

## Publishing to GitHub Pages

The generator writes everything under `docs/` (set by `output_dir` in
`config.toml`), which is the directory GitHub Pages can serve directly. Links
are **relative**, so the site works unchanged whether it's served from the bare
Pages URL (`https://<you>.github.io/news-fetcher/`) or a custom domain.

1. **Generate and commit** the site:
   ```sh
   ./target/release/news-fetcher update    # writes docs/
   git add docs && git commit -m "site: update" && git push
   ```
   `news.db` stays gitignored — it's your local "seen" state and persists on disk
   between runs, so re-running skips already-seen items. Because cross-day nav is
   client-side, a daily commit touches only the changed day's page, `index.html`,
   and `days.js` — not the whole `docs/` tree.
2. **Enable Pages:** GitHub → **Settings → Pages → Source: Deploy from a branch**,
   then pick your branch and the **`/docs`** folder.
3. The site goes live at `https://<you>.github.io/news-fetcher/`.

### Custom subdomain (Cloudflare)

1. Set `custom_domain = "news.example.com"` in `config.toml` and re-run `update`; the
   generator writes `docs/CNAME` so Pages binds the domain. Commit and push.
2. In **Cloudflare DNS**, add a `CNAME` record: `news` → `<you>.github.io`.
   Start with **DNS only (grey cloud)** so GitHub can issue the TLS certificate.
3. In GitHub → **Settings → Pages → Custom domain**, confirm the domain shows,
   wait for **"certificate provisioned,"** then tick **Enforce HTTPS**.
4. *(Optional)* Once HTTPS is enforced you may switch the Cloudflare record back
   to **proxied (orange cloud)** — but only with SSL/TLS mode **Full (strict)**,
   otherwise you'll hit a redirect loop.

## Scheduling (optional)

The tool is a single run-to-completion binary, so any scheduler works. Example
cron entry (every 2 hours) that regenerates and publishes:

```cron
0 */2 * * * cd /path/to/news-fetcher && ./target/release/news-fetcher update && git -C /path/to/news-fetcher commit -am "site: update" && git -C /path/to/news-fetcher push >> run.log 2>&1
```

To also broadcast a daily message, capture `digest` and post it to your chat
platform's webhook. Use `&&` (not a pipe) so an empty day — where `digest`
exits non-zero — short-circuits before `curl` runs, skipping the post:

```cron
0 9 * * * cd /path/to/news-fetcher && msg=$(./target/release/news-fetcher digest) && curl -sf -X POST -d "$msg" "$TELEGRAM_OR_DISCORD_WEBHOOK" >> run.log 2>&1
```

## Not included (yet)

- **X/Twitter** — no reliable free API; intentionally skipped.
- No GitHub Actions automation; the publish flow is a local build + branch deploy.
