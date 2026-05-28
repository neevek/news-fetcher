# Editorial Day-Level Ranking

**Date:** 2026-05-27
**Status:** Approved, implementing

## Problem

`--top 1` (and top-N generally) picks the day's lead story by the per-item
`importance` score, sorted `importance DESC, time DESC`. But `importance` is
assigned **per item, in isolation, in chunks of ≤6** during summarization
(`summarize.rs`). The model never compares a day's stories against each other,
so:

- Scores cluster (many 80/85s); ties fall back to **recency**, not value.
- Near-duplicate stories can both score high and crowd the top.
- "Highest absolute score from independent batches" ≠ "the lead story of the day".

The ranking comparator is also **duplicated** in `render.rs` and `message.rs`,
so the site and the IM digest could disagree on #1.

## Goal

A reliable, editor-quality ranking of each day's items, with a clear #1.
Decisions captured from brainstorming:

- **#1 = blended editor's pick** — weigh actionability + impact + learning, the
  way a human editor chooses the lead story.
- **Scope = recalibrate the whole day** — top-3/5/10 all sharpen, #1 stands out.

## Approach

A dedicated **day-level editorial re-rank pass** that runs *after*
summarization, over the **whole day's** stored items (not just one run's new
items). It sends the model a compact candidate list and asks it to rank
comparatively, producing a persisted, day-relative `editor_score`. Render and
the digest just consume that score and stay fully offline.

Rejected alternatives:

- **Fold global scoring into summarization** — ingest only summarizes *this
  run's* new items, so it can't see items already stored for the same day
  (re-runs, backfills). Calibration would be partial.
- **Pure deterministic multi-signal sort** — heuristic proxies, not the
  editorial judgment chosen. Its signals (engagement, source authority) are
  instead fed *into* the editor pass as objective anchors.

## Design

### 1. New `rank.rs` phase

`rank_days(store, items, affected_days, model, thinking) -> Result<()>`:

- For each day key in `affected_days` with **≥2 complete items**, build a
  compact candidate list and make one `codex exec` call (reusing the
  `summarize.rs` JSON-schema + timeout + tee plumbing).
- The model returns, for every candidate id, a day-relative `editor_score`
  (0–100, spread out so #1 is clearly highest) and a `lead_reason` for the
  top pick.
- Days with a single complete item skip the call: `editor_score = importance`.
- Validation: the response must cover **every** candidate id exactly once. On
  mismatch, retry once, then fall back (best-effort, see below).

### 2. The editor prompt (the quality lever)

Frames the model as this publication's chief editor (mission: engineer
productivity + learning). Each candidate is given compactly: id, editorial
title, one-line lede, source label, tags, engagement `score` (HN points), and
current `importance`. Ranking criteria:

- Real impact on a Claude Code / Codex user's daily work; actionability.
- Learning depth; significance / novelty.
- **Source authority**: official release/changelog (`github_releases`) > vendor
  blog > reputable analysis > forum chatter (`hackernews` / `reddit`).
- **Collapse near-duplicates** so the same story can't occupy ranks 1–2.

Output schema: `{ "ranking": [ { "id", "editor_score", "lead_reason"? } ] }`.

### 3. Persistence (`store.rs`, `model.rs`)

- `NewsItem` gains `editor_score: Option<i64>` and `editor_reason:
  Option<String>`. Neither is part of `is_complete()` — ranking is a refinement,
  not a publish gate.
- Add columns via the existing additive `ALTER TABLE ADD COLUMN` pattern:
  `editor_score INTEGER`, `editor_reason TEXT`.
- `all()` reads them; `insert` writes them (new rows start NULL, then get set in
  the same run before render). New method `set_editor_score(id, score, reason)`.
- Kept **separate** from `importance` (non-destructive): `resummarize`
  regenerates per-item `importance`, then re-rank recomputes `editor_score`.

### 4. Centralized sort

Replace the duplicated comparators in `render.rs` and `message.rs` with one
`rank::day_order`: `editor_score DESC (NULLs last) → importance DESC → time
DESC`. Site and digest now agree on #1 by construction.

### 5. Where it runs

- `update` → re-rank the days its new items touched (daily cron = 1 day = 1 call).
- `resummarize` → re-rank all days present.
- `repair` → re-rank the repaired items' days.
- `render` and `digest` → **offline**: read `editor_score` only. Re-renders stay
  byte-stable (no `codex` call), preserving the existing determinism property.

### 6. Failure behavior

Ranking is **best-effort**, unlike summarization's complete-or-nothing: if the
editor call fails after one retry, log a warning and fall back to `importance`
ordering. Rationale: the day's news is already fully summarized and publishable;
a ranking hiccup must not block publishing. Summarization stays strict.

The digest message format is **unchanged** (no #1 highlight) for this iteration.

## Testing

- `rank`: prompt includes every candidate; schema parse; scores applied by id;
  missing/extra id → error then retry; single-item day short-circuits without a
  call; `day_order` orders by `editor_score` then `importance` then time, and an
  item with lower `importance` but higher `editor_score` ranks first.
- `store`: round-trip of `editor_score`/`editor_reason`; migration on a DB
  lacking the columns.
- `render`/`message`: ordering now driven by `editor_score`; fallback to
  `importance` when `editor_score` is absent.

## Out of scope (YAGNI)

- Per-source authority weights in config (the prompt infers authority from the
  source label for now).
- A visible #1 highlight in the IM digest.
- A `--strict-rank` flag.
