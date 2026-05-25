# Multi-page site for news-fetcher

**Date:** 2026-05-24
**Status:** Approved

## Problem

The fetcher currently emits a single `index.html` containing every day. All days
are present in the DOM at once and JavaScript toggles which one is visible. We
want a real, navigable static site: one page per day (named with the date),
a main page, and easy navigation between dates — suitable for hosting on GitHub
Pages behind a Cloudflare-managed custom subdomain.

## Decisions

- **Main page:** `index.html` renders the **latest day in full** (option C).
- **Per-day pages:** every stored day gets a permalink at
  `feeds/yyyy/MM/dd.html` (e.g. `feeds/2026/05/24.html`).
- **Navigation:** the existing horizontal chip rail becomes real links (current
  day highlighted), plus `← prev` / `next →` links in the day header. The
  JS-based day toggling is removed.
- **Archive depth:** all stored days are rendered and listed in the rail.
- **Output root:** everything is written under `docs/` so GitHub Pages can serve
  it via "Deploy from branch → `/docs`".
- **Publishing:** local build, commit `docs/`; `news.db` stays gitignored local
  state. Custom subdomain via Cloudflare.

## Site structure

```
docs/
  .nojekyll                 # serve files as-is, skip Jekyll
  CNAME                     # written only when settings.custom_domain is set
  index.html                # latest day (root = "")
  feeds/
    2026/05/24.html         # day permalink (root = "../../../")
    2026/05/23.html
    ...
```

`news.db` and the Rust sources stay at the repo root; only `docs/` is published.

## Links / portability

Pages may be served from a subpath (`/news-fetcher/`), the bare Pages domain, or
a custom domain. To work in all cases with no base-URL config, all internal
links are **relative**, built from a `root` prefix passed into the template:

- `index.html` → `root = ""` → day link `feeds/2026/05/23.html`
- `feeds/2026/05/24.html` → `root = "../../../"` → day link
  `../../../feeds/2026/05/23.html`, home `../../../index.html`

## Code changes

### `config.rs`
- Replace `output_html: String` (default `index.html`) with
  `output_dir: String` (default `docs`).
- Add optional `custom_domain: Option<String>`.

### `store.rs`
- Add a method to fetch **all** items ordered by `COALESCE(published, first_seen)
  DESC` (the archive spans every stored day, not just the recent 100). Keep
  `recent` or generalize it; the renderer needs the full set.

### `render.rs`
- Group items by day once (as today) and build the shared `days` list used for
  the chip rail and prev/next, with the per-page metadata already computed
  (`anchor`, `md`, `dom`, `month`, `weekday`, `count`, `new_count`).
- Replace the single-file write with a loop:
  - For each day, render the template with: that day's items, the full `days`
    list for the rail (each carrying its relative `href`), the current day's
    index, prev/next neighbors, and the correct `root` prefix. Write to
    `feeds/yyyy/MM/dd.html`.
  - Render the latest day again with `root = ""` to `index.html`.
- Write `docs/.nojekyll` (always) and `docs/CNAME` (when `custom_domain` set).
- Create parent directories as needed (`feeds/yyyy/MM/`).
- Pure helpers worth unit-testing: day-permalink path builder
  (`day_path("2026-05-24") -> "feeds/2026/05/24.html"`) and the `root` prefix.

### `digest.html.j2`
- Render a **single** day's cards (the template receives one `day`, not a list).
- Chip rail: `<a href="{{ root }}{{ d.href }}">` per day, `active` class on the
  current day; rail still horizontally scrollable.
- Day header: add `← prev` / `next →` anchors (omit the missing side at the
  ends). prev = older day, next = newer day.
- Remove the day-switching JavaScript and the `.day { display:none }` mechanism.
  Keep theme toggle, highlight.js, and back-to-top.

### `main.rs`
- Fetch all items (instead of `recent(max_items)`), build the output dir from
  `cfg.settings.output_dir`, and call the new multi-page render.
- `max_items` may still cap items considered per run if desired; archive depth
  is "all days present in the fetched set."

### `.gitignore`
- Remove the obsolete `/index.html` line (no longer produced at the repo root).
- Keep `/news.db` ignored. `docs/` is committed.

## Publishing (README "Publishing" section)

1. Run the fetcher locally; it writes `docs/`.
2. Commit and push `docs/`.
3. GitHub → Settings → Pages → Source: Deploy from a branch → `main` `/docs`.
4. Custom subdomain (Cloudflare): add a `CNAME` record
   `news` → `<you>.github.io` (DNS only / grey cloud).
5. Set `custom_domain = "news.example.com"` in `sources.toml` so the generator
   writes `docs/CNAME`; GitHub → Pages → Custom domain shows it. Wait for
   "certificate provisioned," then enable **Enforce HTTPS**.
6. Optional: once HTTPS is enforced, the Cloudflare record may be proxied
   (orange) with SSL/TLS mode **Full (strict)**.

## Out of scope

- GitHub Actions automation (chosen flow is local build + branch deploy).
- Search, pagination of the rail, RSS output.
```
