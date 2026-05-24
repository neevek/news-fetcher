# news-fetcher

A small Rust tool that monitors **coding-agent industry news** — focused on
OpenAI **Codex** and Anthropic **Claude Code** — and generates a static HTML
digest. Each run fetches from configured sources, keeps only new and relevant
items, summarizes them with the **Codex CLI**, and renders `index.html`.

The digest is **grouped by day** and shows the **top 10 items per day** (ranked
by an LLM-assigned importance score). Each item has a one-line Chinese standfirst
plus a **thorough Chinese article body rendered from Markdown** (lists, headings,
inline code, and fenced code blocks), with the **reference link at the end**;
product names, versions, commands, and code identifiers are kept in English.

The page is an *editorial-terminal* design: **sticky date navigation** with
scroll-spy and a back-to-top button, **syntax-highlighted code** (highlight.js),
and a **mobile/desktop-friendly** layout that defaults to **dark mode** with a
**light-mode toggle**. Fonts (Fraunces / Noto Serif SC / Noto Sans SC / JetBrains
Mono) and highlight.js are loaded from CDNs, so code highlighting and the display
typefaces need network access when the page is *viewed* (the rest renders
offline).

## How it works

```
sources.toml ──► fetchers (GitHub Releases / Hacker News / Reddit / RSS)
                      │
                      ▼
        relevance filter ──► dedupe against SQLite "seen" store
                      │
                      ▼
        new items ──► codex exec (batched, structured JSON) ──► summaries + tags
                      │
                      ▼
        SQLite store ──► minijinja template ──► index.html
```

The SQLite store (`news.db`) is what makes this a *monitor* rather than a
scraper: every run only treats not-yet-stored items as new and badges them
`NEW` in the output.

## Sources

Configured in [`sources.toml`](sources.toml). Supported `kind` values:

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

# Fetch, summarize with codex, and write index.html:
./target/release/news-fetcher

# Skip the LLM (raw snippets instead of summaries):
./target/release/news-fetcher --no-summarize

# Re-render HTML from the existing DB without fetching:
./target/release/news-fetcher --render-only

# Use a specific codex model / custom config:
./target/release/news-fetcher --model gpt-5.1-codex --config sources.toml
```

### Summarization (Codex CLI)

Summaries are produced by **batched** `codex exec` calls (12 items per call)
that request structured JSON — Chinese title, thorough summary, highlights,
tags, and a 0–100 importance score — enforced via `--output-schema`. Requires
the [`codex` CLI](https://developers.openai.com/codex) installed and
authenticated. If a chunk is missing codex, errors, or exceeds the 300s
timeout, that chunk falls back to raw-snippet summaries (untranslated) so the
run always produces output.

## Scheduling (set up later)

The tool is a single run-to-completion binary, so any scheduler works. Example
cron entry (every 2 hours):

```cron
0 */2 * * * cd /path/to/news-fetcher && ./target/release/news-fetcher >> run.log 2>&1
```

To publish, point a static host (e.g. GitHub Pages) at the generated
`index.html`, or commit it from the cron job.

## Not included (yet)

- **X/Twitter** — no reliable free API; intentionally skipped.
- Per-item links open the source; there is no built-in archive page.
