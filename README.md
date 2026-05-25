# news-fetcher

A small Rust tool that monitors **coding-agent industry news** — focused on
OpenAI **Codex** and Anthropic **Claude Code** — and generates a static HTML
site. Each run fetches from configured sources, keeps only new and relevant
items, summarizes them with the **Codex CLI**, and renders a static site under
`docs/`.

The site is a **navigable archive**: `docs/index.html` shows the latest day, and
every stored day gets its own permalink at `docs/feeds/yyyy/MM/dd.html`. Each page
shows the **top 10 items for that day** (ranked by an LLM-assigned importance
score). Each item has a one-line Chinese standfirst
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
        SQLite store ──► minijinja template ──► docs/ (index.html + feeds/yyyy/MM/dd.html)
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

# Fetch, summarize with codex, and write the site to docs/:
./target/release/news-fetcher
# then open docs/index.html

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

## Publishing to GitHub Pages

The generator writes everything under `docs/` (set by `output_dir` in
`sources.toml`), which is the directory GitHub Pages can serve directly. Links
are **relative**, so the site works unchanged whether it's served from the bare
Pages URL (`https://<you>.github.io/news-fetcher/`) or a custom domain.

1. **Generate and commit** the site:
   ```sh
   ./target/release/news-fetcher        # writes docs/
   git add docs && git commit -m "site: update" && git push
   ```
   `news.db` stays gitignored — it's your local "seen" state and persists on disk
   between runs, so re-running keeps the `NEW` badges accurate.
2. **Enable Pages:** GitHub → **Settings → Pages → Source: Deploy from a branch**,
   then pick your branch and the **`/docs`** folder.
3. The site goes live at `https://<you>.github.io/news-fetcher/`.

### Custom subdomain (Cloudflare)

1. Set `custom_domain = "news.example.com"` in `sources.toml` and re-run; the
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
0 */2 * * * cd /path/to/news-fetcher && ./target/release/news-fetcher && git -C /path/to/news-fetcher commit -am "site: update" && git -C /path/to/news-fetcher push >> run.log 2>&1
```

## Not included (yet)

- **X/Twitter** — no reliable free API; intentionally skipped.
- No GitHub Actions automation; the publish flow is a local build + branch deploy.
