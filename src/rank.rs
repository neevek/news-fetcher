//! Day-level editorial ranking.
//!
//! Summarization (`summarize.rs`) scores each item in isolation, so its
//! `importance` is not calibrated across a day and can't reliably name the
//! day's lead story. This module adds a second, comparative pass: it shows the
//! model a whole day's candidates at once and asks it, as the publication's
//! editor, to rank them and assign a day-relative `editor_score` (0-100). The
//! score is persisted and consumed by `render`/`message`, which stay offline.

use crate::codex;
use crate::model::NewsItem;
use crate::store::Store;
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};

/// Re-rank every day touched by the `changed` items (newly summarized or
/// repaired). For each such day with two or more complete items, the editor
/// pass compares them and assigns a day-relative `editor_score`; a single-item
/// day is trivially its own lead. Scores are written into `items` (so the
/// caller can render immediately) and persisted via `store`.
///
/// Best-effort: if the editor call fails for a day (after one retry), that day
/// keeps its importance-based order and the run continues — the day's news is
/// already fully summarized, so a ranking hiccup must not block publishing.
pub fn rank_days(
    store: &Store,
    items: &mut [(NewsItem, DateTime<Utc>)],
    changed: &HashSet<String>,
    model: &str,
    thinking: &str,
) -> Result<()> {
    let affected = affected_days(items, changed);
    if affected.is_empty() {
        return Ok(());
    }

    // Group complete items of the affected days: day-key → indices into `items`.
    let mut by_day: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, (it, fs)) in items.iter().enumerate() {
        if !it.is_complete() {
            continue;
        }
        let day = day_key(it, *fs);
        if affected.contains(&day) {
            by_day.entry(day).or_default().push(i);
        }
    }

    for (day, idxs) in by_day {
        // A lone item is its own lead: score it from importance, no codex call.
        if idxs.len() == 1 {
            let i = idxs[0];
            let score = items[i].0.importance.unwrap_or(0);
            items[i].0.editor_score = Some(score);
            store.set_editor_score(&items[i].0.id, Some(score), items[i].0.editor_reason.as_deref())?;
            continue;
        }

        // Build the prompt from an immutable view, then release that borrow
        // before mutating `items` to apply the result.
        let (prompt, expected) = {
            let candidates: Vec<&NewsItem> = idxs.iter().map(|&i| &items[i].0).collect();
            let expected: HashSet<String> = candidates.iter().map(|c| c.id.clone()).collect();
            (build_prompt(&candidates, &day), expected)
        };

        eprintln!("  ranking {} item(s) for {day}…", idxs.len());
        let entries = match rank_call(&prompt, &expected, model, thinking) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("  rank {day}: editorial pass failed ({e:#}); keeping importance order");
                continue;
            }
        };

        let idxset: HashSet<usize> = idxs.iter().copied().collect();
        let mut refs: Vec<&mut NewsItem> = items
            .iter_mut()
            .enumerate()
            .filter(|(i, _)| idxset.contains(i))
            .map(|(_, (it, _))| it)
            .collect();
        assign_scores(&mut refs, &entries);
        drop(refs);

        for &i in &idxs {
            let it = &items[i].0;
            store.set_editor_score(&it.id, it.editor_score, it.editor_reason.as_deref())?;
        }
    }
    Ok(())
}

/// One editor call with a single retry, covering transient codex errors and a
/// partial/garbled ranking (which `parse_ranking` rejects). Returns the parsed,
/// fully-covering ranking, or the second error.
fn rank_call(prompt: &str, expected: &HashSet<String>, model: &str, thinking: &str) -> Result<Vec<RankEntry>> {
    let attempt = || -> Result<Vec<RankEntry>> {
        let raw = codex::exec_json(prompt, &output_schema(), model, thinking)?;
        parse_ranking(&raw, expected)
    };
    attempt().or_else(|e| {
        eprintln!("    editorial pass failed ({e:#}); retrying once…");
        attempt()
    })
}

/// One item's editorial verdict, parsed from the model's ranking JSON.
#[derive(Debug, Clone)]
pub struct RankEntry {
    pub id: String,
    /// Day-relative score, clamped to 0..=100.
    pub editor_score: i64,
    /// Present only for the day's lead pick.
    pub lead_reason: Option<String>,
}

/// Parse the editor's `{"ranking":[{id,editor_score,lead_reason?}]}` output.
/// Scores are clamped to 0..=100. Every id in `expected` must appear (a partial
/// ranking is a failure so the caller can retry/fall back); unknown extra ids
/// are ignored, mirroring `summarize::apply_summaries`.
fn parse_ranking(raw: &str, expected: &HashSet<String>) -> Result<Vec<RankEntry>> {
    let v: Value = serde_json::from_str(raw.trim())
        .with_context(|| format!("parsing rank JSON: {}", truncate(raw, 200)))?;
    let arr = v["ranking"]
        .as_array()
        .ok_or_else(|| anyhow!("rank output missing 'ranking' array"))?;

    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for entry in arr {
        let Some(id) = entry["id"].as_str() else { continue };
        // Ignore ids the day didn't ask about (hallucinated/duplicated).
        if !expected.contains(id) || !seen.insert(id.to_string()) {
            continue;
        }
        let editor_score = entry["editor_score"].as_i64().unwrap_or(0).clamp(0, 100);
        let lead_reason = entry["lead_reason"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        entries.push(RankEntry { id: id.to_string(), editor_score, lead_reason });
    }

    // A partial ranking can't be trusted to name the right #1, so fail loudly
    // and let the caller retry / fall back to importance order.
    let missing: Vec<&str> = expected.iter().map(String::as_str).filter(|id| !seen.contains(*id)).collect();
    anyhow::ensure!(missing.is_empty(), "rank output omitted {} candidate(s): {}", missing.len(), missing.join(", "));
    Ok(entries)
}

/// Build the editor prompt for one day's candidates. Each candidate is shown
/// compactly — editorial title, one-line lede, source, tags, engagement score,
/// and the per-item importance — so the model can rank them comparatively.
fn build_prompt(candidates: &[&NewsItem], date: &str) -> String {
    let mut p = format!(
        "你是本刊的主编，刊物的唯一目标：帮日常使用 Claude Code 与 OpenAI Codex 的工程师**提升 AI 编程生产力**、**学到最新的技术知识与实战技巧**。\n\
下面是 {date}（UTC）这一天已经入选的全部资讯。请你作为主编，把它们**放在一起横向比较**，评出当天的价值排序，并选出当之无愧的「头条」。\n\n\
对每一条，给出 editor_score（0-100 的**当日相对分**）：\n\
- 这是同一天内的相对排序分，不是绝对分。要把分数**拉开**，避免扎堆；**有且只有一条**最高分作为头条。\n\
- 评分维度（按重要性）：对 Claude Code / Codex 用户**实际工作的影响**、**可落地的可操作性**、**学习价值/技巧深度**、**重要性与新意**。\n\
- 来源权威性作为加权参考：官方发布/更新日志（如 *Releases* 类来源）> 厂商博客 > 高质量分析 > 论坛闲聊/转贴（如 Hacker News / Reddit 的讨论帖）。同等内容下，一手官方信息更可信、更重要。\n\
- **合并近似重复**：如果多条讲的是同一件事，只让信息最全/最权威的一条靠前，其余明显压低，避免头部被重复内容占据。\n\n\
只给**当日头条**（最高分那条）写 lead_reason：一句话中文，说明它为什么是今天最值得读的（点出最关键的收获或影响）。其余条目不需要 lead_reason。\n\n\
必须为下面列出的**每一条** id 都返回一个评分，数量、id 必须与列表完全对应。只输出符合 schema 的 JSON。\n\n候选列表：\n"
    );
    for it in candidates {
        let title = it
            .title_en
            .as_deref()
            .or(it.title_zh.as_deref())
            .unwrap_or(&it.title);
        let lede = it.summary_en.as_deref().or(it.summary.as_deref()).unwrap_or("");
        p.push_str(&format!(
            "\n- id: {}\n  source: {}\n  title: {}\n  lede: {}\n  tags: {}\n  engagement_score: {}\n  per_item_importance: {}\n",
            it.id,
            it.source,
            title,
            lede,
            if it.tags.is_empty() { "n/a".into() } else { it.tags.join(", ") },
            it.score.map(|s| s.to_string()).unwrap_or_else(|| "n/a".into()),
            it.importance.map(|s| s.to_string()).unwrap_or_else(|| "n/a".into()),
        ));
    }
    p
}

/// The day an item belongs to: its published date, falling back to first-seen.
/// Mirrors `render::build_days` so ranking groups items the same way the site
/// and digest do.
fn day_key(it: &NewsItem, first_seen: DateTime<Utc>) -> String {
    it.published.unwrap_or(first_seen).format("%Y-%m-%d").to_string()
}

/// The set of day-keys touched by the `changed` items (newly summarized or
/// repaired). Only those days need re-ranking; everything else keeps its score.
fn affected_days(items: &[(NewsItem, DateTime<Utc>)], changed: &HashSet<String>) -> HashSet<String> {
    items
        .iter()
        .filter(|(it, _)| changed.contains(&it.id))
        .map(|(it, fs)| day_key(it, *fs))
        .collect()
}

/// Apply the editor's verdicts to the day's items in place: set `editor_score`
/// on every item, and `editor_reason` only where the editor supplied one (the
/// lead). Items absent from `entries` are left untouched.
fn assign_scores(day: &mut [&mut NewsItem], entries: &[RankEntry]) {
    for entry in entries {
        if let Some(it) = day.iter_mut().find(|it| it.id == entry.id) {
            it.editor_score = Some(entry.editor_score);
            if entry.lead_reason.is_some() {
                it.editor_reason = entry.lead_reason.clone();
            }
        }
    }
}

/// JSON output schema for the editor pass: a per-id score and an optional
/// lead reason. Enforced via `codex exec --output-schema`.
fn output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "ranking": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "editor_score": { "type": "integer" },
                        "lead_reason": { "type": "string" }
                    },
                    "required": ["id", "editor_score"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["ranking"],
        "additionalProperties": false
    })
}

/// Truncate to `max` chars, appending an ellipsis when shortened.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).chain(std::iter::once('…')).collect()
}

/// The day's ranking order, newest-best first. Sorts by editorial score
/// (descending; un-ranked items — `None` — sort last), then per-item
/// `importance` (descending), then publish time (descending) as the final
/// tiebreak. Used by both the site and the IM digest so they agree on #1.
pub fn day_order(a: (&NewsItem, DateTime<Utc>), b: (&NewsItem, DateTime<Utc>)) -> Ordering {
    // Sort key, highest-first. An absent editor_score sorts below every ranked
    // item (i64::MIN), so a half-ranked day keeps ranked items on top and falls
    // back to importance/time for the rest.
    fn key(x: (&NewsItem, DateTime<Utc>)) -> (i64, i64, DateTime<Utc>) {
        (x.0.editor_score.unwrap_or(i64::MIN), x.0.importance.unwrap_or(0), x.1)
    }
    key(b).cmp(&key(a))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn ids(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_ranking_extracts_scores_and_lead_reason() {
        let raw = r#"{"ranking":[
            {"id":"a","editor_score":90,"lead_reason":"ships a usable workflow today"},
            {"id":"b","editor_score":40}
        ]}"#;
        let entries = parse_ranking(raw, &ids(&["a", "b"])).unwrap();
        let by: HashMap<&str, &RankEntry> = entries.iter().map(|e| (e.id.as_str(), e)).collect();
        assert_eq!(by["a"].editor_score, 90);
        assert_eq!(by["a"].lead_reason.as_deref(), Some("ships a usable workflow today"));
        assert_eq!(by["b"].editor_score, 40);
        assert_eq!(by["b"].lead_reason, None);
    }

    #[test]
    fn parse_ranking_clamps_scores_to_0_100() {
        let raw = r#"{"ranking":[{"id":"a","editor_score":150},{"id":"b","editor_score":-5}]}"#;
        let entries = parse_ranking(raw, &ids(&["a", "b"])).unwrap();
        let by: HashMap<&str, &RankEntry> = entries.iter().map(|e| (e.id.as_str(), e)).collect();
        assert_eq!(by["a"].editor_score, 100);
        assert_eq!(by["b"].editor_score, 0);
    }

    #[test]
    fn parse_ranking_errors_when_a_candidate_is_missing() {
        // Only "a" came back, but the day had both "a" and "b": partial ranking.
        let raw = r#"{"ranking":[{"id":"a","editor_score":90}]}"#;
        let err = parse_ranking(raw, &ids(&["a", "b"])).unwrap_err();
        assert!(err.to_string().contains('b'), "error should name the missing id: {err}");
    }

    #[test]
    fn parse_ranking_ignores_unknown_extra_ids() {
        let raw = r#"{"ranking":[{"id":"a","editor_score":90},{"id":"ghost","editor_score":10}]}"#;
        let entries = parse_ranking(raw, &ids(&["a"])).unwrap();
        assert_eq!(entries.iter().filter(|e| e.id == "ghost").count(), 0);
        assert_eq!(entries.len(), 1);
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn candidate(url: &str, source: &str, title_en: &str, importance: i64, score: Option<i64>) -> NewsItem {
        let mut it = NewsItem::new(source, "raw title", url);
        it.title_en = Some(title_en.into());
        it.summary_en = Some("one-line lede".into());
        it.importance = Some(importance);
        it.score = score;
        it
    }

    fn dated(url: &str, published: Option<&str>, first_seen: &str) -> (NewsItem, DateTime<Utc>) {
        let mut it = NewsItem::new("Src", "t", url);
        it.published = published.map(at);
        (it, at(first_seen))
    }

    #[test]
    fn affected_days_uses_published_then_first_seen() {
        let a = dated("https://example.com/a", Some("2026-05-26T10:00:00Z"), "2026-05-20T00:00:00Z");
        let b = dated("https://example.com/b", Some("2026-05-27T10:00:00Z"), "2026-05-20T00:00:00Z");
        let c = dated("https://example.com/c", None, "2026-05-25T00:00:00Z"); // day = first_seen
        let (bid, cid) = (b.0.id.clone(), c.0.id.clone());
        let items = vec![a, b, c];
        let changed: HashSet<String> = [bid, cid].into_iter().collect();
        let got = affected_days(&items, &changed);
        assert_eq!(got, ["2026-05-27", "2026-05-25"].iter().map(|s| s.to_string()).collect());
    }

    #[test]
    fn assign_scores_sets_score_for_all_and_reason_for_lead() {
        let mut a = candidate("https://example.com/a", "Src", "A", 10, None);
        let mut b = candidate("https://example.com/b", "Src", "B", 99, None);
        let entries = vec![
            RankEntry { id: a.id.clone(), editor_score: 90, lead_reason: Some("today's lead".into()) },
            RankEntry { id: b.id.clone(), editor_score: 40, lead_reason: None },
        ];
        {
            let mut refs: Vec<&mut NewsItem> = vec![&mut a, &mut b];
            assign_scores(&mut refs, &entries);
        }
        assert_eq!(a.editor_score, Some(90));
        assert_eq!(a.editor_reason.as_deref(), Some("today's lead"));
        assert_eq!(b.editor_score, Some(40));
        assert_eq!(b.editor_reason, None);
    }

    #[test]
    fn build_prompt_lists_every_candidate_with_its_signals() {
        let a = candidate("https://example.com/a", "Claude Code Releases", "Hooks v2 ships", 60, None);
        let b = candidate("https://example.com/b", "Hacker News: Codex", "Codex debate thread", 30, Some(412));
        let prompt = build_prompt(&[&a, &b], "2026-05-27");
        // Every candidate's id must appear so the model can return it.
        assert!(prompt.contains(&a.id), "prompt missing id a");
        assert!(prompt.contains(&b.id), "prompt missing id b");
        // Editorial titles and source labels are carried for comparison.
        assert!(prompt.contains("Hooks v2 ships"));
        assert!(prompt.contains("Claude Code Releases"));
        assert!(prompt.contains("Hacker News: Codex"));
        // The engagement score, when present, is surfaced as an anchor.
        assert!(prompt.contains("412"));
        // The day being ranked is named.
        assert!(prompt.contains("2026-05-27"));
    }

    fn item(importance: Option<i64>, editor: Option<i64>) -> NewsItem {
        let mut it = NewsItem::new("Src", "T", "https://example.com/x");
        it.importance = importance;
        it.editor_score = editor;
        it
    }

    #[test]
    fn editor_score_beats_importance() {
        // Lower importance but higher editor_score must rank ahead.
        let a = item(Some(10), Some(90));
        let b = item(Some(99), Some(50));
        let t = at("2026-05-27T08:00:00Z");
        assert_eq!(day_order((&a, t), (&b, t)), Ordering::Less); // a sorts first
    }

    #[test]
    fn unranked_items_sort_after_ranked_ones() {
        let ranked = item(Some(10), Some(40));
        let unranked = item(Some(99), None);
        let t = at("2026-05-27T08:00:00Z");
        assert_eq!(day_order((&ranked, t), (&unranked, t)), Ordering::Less);
    }

    #[test]
    fn falls_back_to_importance_then_time_when_unranked() {
        let older = item(Some(80), None);
        let newer = item(Some(80), None);
        let t_old = at("2026-05-27T06:00:00Z");
        let t_new = at("2026-05-27T09:00:00Z");
        // Same importance, no editor score: newer publish time wins.
        assert_eq!(day_order((&newer, t_new), (&older, t_old)), Ordering::Less);
        // Higher importance wins regardless of time.
        let high = item(Some(95), None);
        assert_eq!(day_order((&high, t_old), (&older, t_new)), Ordering::Less);
    }

    #[test]
    fn sort_orders_a_full_day() {
        let lead = item(Some(10), Some(90)); // top by editor score
        let second = item(Some(99), Some(50));
        let unranked_hi = item(Some(80), None);
        let unranked_lo = item(Some(30), None);
        let t = at("2026-05-27T08:00:00Z");
        let mut v = [(&unranked_lo, t), (&second, t), (&unranked_hi, t), (&lead, t)];
        v.sort_by(|x, y| day_order((x.0, x.1), (y.0, y.1)));
        let scores: Vec<(Option<i64>, Option<i64>)> =
            v.iter().map(|(it, _)| (it.editor_score, it.importance)).collect();
        assert_eq!(
            scores,
            vec![(Some(90), Some(10)), (Some(50), Some(99)), (None, Some(80)), (None, Some(30))]
        );
    }
}
