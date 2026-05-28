use crate::codex;
use crate::model::NewsItem;
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

/// Items per codex call. Smaller batches keep each call within the timeout and
/// limit the blast radius of any single failure. Kept small because each item
/// now yields a long bilingual digest, so a big batch can blow the timeout.
const CHUNK_SIZE: usize = 6;

/// Summarize new items via `codex exec`, in chunks. Each chunk asks Codex for
/// structured JSON (enforced via --output-schema): a Chinese title, a thorough
/// Chinese summary, highlight bullets, tags, and an importance score. Captured
/// with -o.
///
/// COMPLETE-or-nothing: there is no offline fallback. If codex fails (or comes
/// back missing fields) for any item, this aborts the entire run with an error
/// so a degraded, half-translated digest is never published. On success, every
/// item is guaranteed [`NewsItem::is_complete`].
pub fn summarize(items: &mut [NewsItem], model: &str, thinking: &str) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }

    let total_chunks = items.len().div_ceil(CHUNK_SIZE);
    for (i, chunk) in items.chunks_mut(CHUNK_SIZE).enumerate() {
        eprintln!("  summarizing chunk {}/{} ({} items)…", i + 1, total_chunks, chunk.len());
        // Abort on the first chunk that can't be fully summarized — no point
        // burning codex calls on the rest when the whole run is forfeit.
        summarize_resilient(chunk, model, thinking)?;
    }

    // Belt-and-braces: every item must have come back complete. summarize_chunk
    // already enforces this per chunk, but guard here too so a future change
    // can't let a half-empty item slip through to the store.
    let missing: Vec<&str> = items.iter().filter(|i| !i.is_complete()).map(|i| i.id.as_str()).collect();
    anyhow::ensure!(
        missing.is_empty(),
        "codex returned incomplete summaries for {} item(s): {}",
        missing.len(),
        missing.join(", ")
    );
    Ok(())
}

/// Summarize one chunk, isolating a "poison" item that reliably crashes codex.
/// Try the whole chunk (with one retry for transient timeouts/SIGKILLs); if it
/// still fails and the chunk holds more than one item, split it in half and
/// recurse — narrowing down to the offending item so the aborting error names
/// the exact culprit. With no offline fallback, a lone item that still fails
/// returns an error, which aborts the whole run.
///
/// Splitting only happens around a genuinely failing item (rare), and `?`
/// stops at the first failing half — so at worst we spend a few extra codex
/// calls on a run that's already forfeit. We accept that cost for a precise
/// error over a vaguer "some chunk failed".
fn summarize_resilient(chunk: &mut [NewsItem], model: &str, thinking: &str) -> Result<()> {
    let mut result = summarize_chunk(chunk, model, thinking);
    if let Err(e) = &result {
        eprintln!("    chunk of {} failed ({e:#}); retrying once…", chunk.len());
        result = summarize_chunk(chunk, model, thinking);
    }
    let e = match result {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };

    if chunk.len() == 1 {
        // No fallback: a single item that still won't summarize forfeits the run.
        return Err(e.context(format!("item {} could not be summarized", chunk[0].id)));
    }
    let mid = chunk.len() / 2;
    eprintln!(
        "    chunk of {} failed again ({e:#}); splitting {}+{} to isolate the bad item…",
        chunk.len(),
        mid,
        chunk.len() - mid
    );
    let (a, b) = chunk.split_at_mut(mid);
    summarize_resilient(a, model, thinking)?;
    summarize_resilient(b, model, thinking)?;
    Ok(())
}

fn summarize_chunk(items: &mut [NewsItem], model: &str, thinking: &str) -> Result<()> {
    let prompt = build_prompt(items);
    let raw = codex::exec_json(&prompt, &output_schema(), model, thinking)?;
    apply_summaries(items, &raw)?;

    // codex may parse-and-exit cleanly yet omit an item (or leave a field
    // blank). Treat that as a chunk failure so summarize_resilient retries and,
    // if it persists, splits to name the culprit — never a partial item.
    let incomplete: Vec<&str> = items.iter().filter(|i| !i.is_complete()).map(|i| i.id.as_str()).collect();
    anyhow::ensure!(
        incomplete.is_empty(),
        "codex output missing complete summaries for: {}",
        incomplete.join(", ")
    );
    Ok(())
}

fn build_prompt(items: &[NewsItem]) -> String {
    let mut p = String::from(
        "你是一名资深的 AI 编程工具分析师，读者是日常使用 Claude Code 与 OpenAI Codex 的工程师。\
本刊的唯一目标：帮工程师**提升用 AI 编程的生产力**，并**学到最新的行业技术知识与实战技巧**。\
所以对每一条资讯，你都要回答读者心里那个问题：「我能从这里学到什么、用到什么，让我的开发更快更好？」\n\
对下面每一条资讯，请完成：\n\
1. title_zh：用中文重写一个清晰、具体的标题；产品名、版本号、命令、代码标识符等保留英文原文。\n\
2. summary：一句话中文导语（不超过 40 字），直接点出可操作的收获或最关键的影响（例如新能力、可借鉴的技巧、值得注意的趋势）。\n\
3. body_md：用中文写正文，Markdown 排版，价值导向，要求：\n\
   - 先 1-2 句点明这是什么、对 Claude Code / Codex 用户意味着什么；\n\
   - 再用无序列表（- 开头）提炼**可落地的要点**：具体的技巧/用法/配置/工作流改进、新功能怎么用、踩坑与最佳实践、值得关注的行业动向或趋势；\n\
   - 给出读者**可以直接照做的实操**：具体命令、配置片段、代码示例、提示词写法、操作步骤——越能直接搬去用越好；\n\
   - **篇幅可长可短，不设上限**：当有真正有用、可操作的内容时，就写得更详细、更完整（更多要点、分步骤、带可运行的代码示例），把一条资讯讲成读者能照着实践的「干货」；内容空洞时则保持简短。长度服务于实用价值，绝不为凑长度灌水。\n\
   - 涉及命令、配置、代码、API、文件名时，用反引号 `code` 或 ```语言 围栏代码块（标注语言），保留英文原文；代码示例要尽量完整、可直接运行或套用。\n\
   - 只写具体信息（功能、修复、版本、影响范围、使用场景、技巧、步骤）；不要泛泛而谈；不要包含一级标题，不要重复 title。\n\
4. title_en / summary_en / body_md_en：上面 title_zh / summary / body_md 的**英文版**，要求与中文版**结构、要点、价值完全对应**（同样的编辑式标题、同样的一句话导语、同样的无序列表与「现在能做的一步」、同样的代码/命令用反引号或围栏代码块）。这是平行的英文稿，不是机器直译：自然、地道、专业；不要照搬原始素材的大段原文，要像中文版一样经过提炼。title_en 应是清晰的编辑式英文标题（不是简单复制原标题）。\n\
5. tags：1-3 个简短英文小写标签（如 \"claude-code\"、\"codex\"、\"release\"、\"security\"、\"tooling\"、\"tips\"、\"workflow\"、\"discussion\"）。\n\
6. importance：0-100 整数，按「对工程师生产力/学习的价值」打分——能学到技巧或显著影响工作流的偏高，闲聊/重复/无操作价值的偏低。\n\
\n硬性规则（务必遵守）：\n\
- 绝对禁止谈论「素材本身」的缺失或不足。永远不要写出诸如「仅有标题」「缺少正文」「无法判断」「信息不足」「没有提供细节」之类的元评论——这些对读者毫无价值，属于失败输出。\n\
- 始终从「读者能学到/用到什么」的角度组织内容；如果一条资讯对生产力或学习没有可提炼的价值，就用最短的篇幅讲清它是什么即可，不要硬凑。\n\
- 当原始材料很少时，依据标题、来源、链接域名以及你对该领域的既有了解，推断这条资讯最可能的含义、定位与价值，并写出对工程师有用的要点（例如它大概是什么工具/项目/讨论、解决什么问题、能带来什么技巧或启发）。\n\
- 但不要编造原文没有的具体事实（如不存在的版本号、性能数字、功能清单、引述）。在「有依据的推断」与「凭空捏造」之间把握分寸：可以说「这类项目通常用于…」，不要谎称「该版本新增了 X、Y、Z」。\n\
- 长度服从价值：有干货就写足、写透（可以很长），没干货就写短；每一句都必须传递新信息或可操作的价值，绝不为凑字数灌水或重复。\n\
技术术语、专有名词在中文不自然时保留英文。只输出符合 schema 的 JSON。\n\n资讯列表：\n",
    );
    for it in items {
        p.push_str(&format!(
            "\n- id: {}\n  source: {}\n  title: {}\n  url: {}\n  score: {}\n  text: {}\n",
            it.id,
            it.source,
            it.title,
            it.url,
            it.score.map(|s| s.to_string()).unwrap_or_else(|| "n/a".into()),
            if it.snippet.is_empty() { "(无正文，请依据标题/来源/链接推断其价值)" } else { &it.snippet }
        ));
    }
    p
}

fn output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "summaries": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "title_zh": { "type": "string" },
                        "summary": { "type": "string" },
                        "body_md": { "type": "string" },
                        "title_en": { "type": "string" },
                        "summary_en": { "type": "string" },
                        "body_md_en": { "type": "string" },
                        "tags": { "type": "array", "items": { "type": "string" } },
                        "importance": { "type": "integer" }
                    },
                    "required": ["id", "title_zh", "summary", "body_md", "title_en", "summary_en", "body_md_en", "tags", "importance"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["summaries"],
        "additionalProperties": false
    })
}

fn apply_summaries(items: &mut [NewsItem], raw: &str) -> Result<()> {
    let v: Value = serde_json::from_str(raw.trim())
        .with_context(|| format!("parsing codex JSON output: {}", codex::truncate(raw, 200)))?;
    let arr = v["summaries"]
        .as_array()
        .ok_or_else(|| anyhow!("codex output missing 'summaries' array"))?;
    for entry in arr {
        let Some(id) = entry["id"].as_str() else { continue };
        let Some(item) = items.iter_mut().find(|i| i.id == id) else { continue };
        if let Some(s) = entry["title_zh"].as_str() {
            item.title_zh = Some(s.trim().to_string());
        }
        if let Some(s) = entry["summary"].as_str() {
            item.summary = Some(s.trim().to_string());
        }
        if let Some(s) = entry["body_md"].as_str() {
            item.body_md = Some(s.trim().to_string());
        }
        if let Some(s) = entry["title_en"].as_str() {
            item.title_en = Some(s.trim().to_string());
        }
        if let Some(s) = entry["summary_en"].as_str() {
            item.summary_en = Some(s.trim().to_string());
        }
        if let Some(s) = entry["body_md_en"].as_str() {
            item.body_md_en = Some(s.trim().to_string());
        }
        if let Some(tags) = entry["tags"].as_array() {
            item.tags = tags.iter().filter_map(|t| t.as_str().map(String::from)).collect();
        }
        if let Some(imp) = entry["importance"].as_i64() {
            item.importance = Some(imp.clamp(0, 100));
        }
    }
    // No fallback here: the caller (summarize_chunk) verifies every item came
    // back complete and errors out otherwise.
    Ok(())
}
