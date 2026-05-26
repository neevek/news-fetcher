use crate::model::NewsItem;
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Hard ceiling on a single `codex exec` call. Past this we kill it and fall
/// back to offline summaries rather than hanging the whole run.
const CODEX_TIMEOUT: Duration = Duration::from_secs(600);

/// Items per codex call. Smaller batches keep each call within the timeout and
/// limit the blast radius of any single failure. Kept small because each item
/// now yields a long bilingual digest, so a big batch can blow the timeout.
const CHUNK_SIZE: usize = 6;

/// Summarize new items via `codex exec`, in chunks. Each chunk asks Codex for
/// structured JSON (enforced via --output-schema): a Chinese title, a thorough
/// Chinese summary, highlight bullets, tags, and an importance score. Captured
/// with -o. A failing chunk falls back to offline summaries for those items.
pub fn summarize(items: &mut [NewsItem], model: Option<&str>) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }

    let total_chunks = items.len().div_ceil(CHUNK_SIZE);
    let mut last_err: Option<anyhow::Error> = None;
    for (i, chunk) in items.chunks_mut(CHUNK_SIZE).enumerate() {
        eprintln!("  summarizing chunk {}/{} ({} items)…", i + 1, total_chunks, chunk.len());
        if let Err(e) = summarize_resilient(chunk, model) {
            last_err = Some(e);
        }
    }
    // Surface a representative error only if *every* item ended up unsummarized
    // by the LLM; otherwise we succeeded at least partially.
    if let Some(e) = last_err {
        if items.iter().all(|i| i.importance.is_none()) {
            return Err(e);
        }
    }
    Ok(())
}

/// Summarize one chunk, recovering from a "poison" item that reliably crashes
/// codex. Try the whole chunk (with one retry for transient timeouts/SIGKILLs);
/// if it still fails and the chunk holds more than one item, split it in half
/// and recurse — so a single bad item only degrades itself instead of dragging
/// its neighbours to the offline fallback. A lone item that still fails falls
/// back to an offline summary. Returns the last error if anything degraded.
fn summarize_resilient(chunk: &mut [NewsItem], model: Option<&str>) -> Result<()> {
    let mut result = summarize_chunk(chunk, model);
    if let Err(e) = &result {
        eprintln!("    chunk of {} failed ({e:#}); retrying once…", chunk.len());
        result = summarize_chunk(chunk, model);
    }
    let e = match result {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };

    if chunk.len() == 1 {
        eprintln!("    item {} failed again ({e:#}); using offline fallback", chunk[0].id);
        summarize_offline(chunk);
        return Err(e);
    }
    let mid = chunk.len() / 2;
    eprintln!(
        "    chunk of {} failed again ({e:#}); splitting {}+{} to isolate the bad item…",
        chunk.len(),
        mid,
        chunk.len() - mid
    );
    let (a, b) = chunk.split_at_mut(mid);
    let ra = summarize_resilient(a, model);
    let rb = summarize_resilient(b, model);
    ra.and(rb)
}

fn summarize_chunk(items: &mut [NewsItem], model: Option<&str>) -> Result<()> {
    let tmp = std::env::temp_dir();
    let schema_path = tmp.join("news-fetcher-schema.json");
    let out_path = tmp.join(format!("news-fetcher-out-{}.json", std::process::id()));
    std::fs::write(&schema_path, output_schema().to_string()).context("writing schema")?;
    let _ = std::fs::remove_file(&out_path);

    let prompt = build_prompt(items);
    if std::env::var("NF_DUMP_PROMPT").is_ok() {
        let _ = std::fs::write("/tmp/nf-prompt.txt", &prompt);
        eprintln!("      [debug] prompt bytes: {}, schema: {}", prompt.len(), schema_path.display());
    }

    let mut cmd = Command::new("codex");
    // Give codex an empty stdin: `codex exec` treats a non-TTY stdin as piped
    // input and blocks waiting for EOF, so without this it hangs indefinitely.
    // stdout/stderr are piped so we can both stream them live (progress) and
    // capture them for an error reason; the result still comes from the -o file.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .arg("exec")
        .arg("--skip-git-repo-check")
        .arg("--sandbox")
        .arg("read-only")
        .arg("--output-schema")
        .arg(&schema_path)
        .arg("-o")
        .arg(&out_path)
        .arg("--color")
        .arg("never");
    if let Some(m) = model {
        cmd.arg("-m").arg(m);
    }
    cmd.arg(&prompt);

    let mut child = cmd
        .spawn()
        .context("launching `codex` — is the codex CLI installed and on PATH?")?;

    // Tee codex's output to our stderr so the run shows live progress, while
    // capturing it for diagnostics. The bulky prompt echo (what codex prints
    // back between its "user" and "codex" markers) is suppressed on stdout.
    let captured = Arc::new(Mutex::new(String::new()));
    let mut tees = Vec::new();
    if let Some(out) = child.stdout.take() {
        tees.push(spawn_tee(Box::new(out), captured.clone(), true));
    }
    if let Some(err) = child.stderr.take() {
        tees.push(spawn_tee(Box::new(err), captured.clone(), false));
    }

    let start = Instant::now();
    let deadline = start + CODEX_TIMEOUT;
    let mut next_tick = start + Duration::from_secs(15);
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                for t in tees {
                    let _ = t.join();
                }
                return Err(anyhow!("codex exec exceeded {}s timeout", CODEX_TIMEOUT.as_secs()));
            }
            None => {
                if Instant::now() >= next_tick {
                    eprintln!("      … codex still working ({}s elapsed)", start.elapsed().as_secs());
                    next_tick += Duration::from_secs(15);
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    };
    for t in tees {
        let _ = t.join();
    }
    if !status.success() {
        let log = captured.lock().map(|g| g.clone()).unwrap_or_default();
        return Err(anyhow!("codex exec exited with status {status}: {}", codex_reason(&log)));
    }

    let raw = std::fs::read_to_string(&out_path).context("reading codex output")?;
    apply_summaries(items, &raw)?;
    Ok(())
}

/// Read a child stream line-by-line, echoing each line to our stderr (live
/// progress) and appending it to a shared buffer (for error diagnostics).
/// When `suppress_prompt` is set, the block codex echoes back between its
/// `user` and `codex` markers — i.e. our own multi-KB prompt — is not echoed.
fn spawn_tee(
    stream: Box<dyn Read + Send>,
    buf: Arc<Mutex<String>>,
    suppress_prompt: bool,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let reader = BufReader::new(stream);
        let stderr = std::io::stderr();
        let mut echoing = true;
        for line in reader.lines().map_while(Result::ok) {
            if let Ok(mut b) = buf.lock() {
                b.push_str(&line);
                b.push('\n');
            }
            let t = line.trim();
            if suppress_prompt && t == "user" {
                echoing = false;
            }
            if echoing {
                let _ = writeln!(stderr.lock(), "      │ {line}");
            }
            if suppress_prompt && t == "codex" {
                echoing = true;
            }
        }
    })
}

/// Distill a concise failure reason from codex's captured output. Codex prints
/// API errors as `ERROR: {... "message":"..."}`; surface that message if
/// present, else fall back to the last non-empty lines of the log.
fn codex_reason(log: &str) -> String {
    if let Some(i) = log.find("\"message\":\"") {
        let rest = &log[i + "\"message\":\"".len()..];
        if let Some(end) = rest.find('"') {
            return truncate(&rest[..end], 300);
        }
    }
    let tail: Vec<&str> = log.lines().rev().filter(|l| !l.trim().is_empty()).take(3).collect();
    let mut tail = tail;
    tail.reverse();
    let joined = tail.join(" | ");
    if joined.is_empty() {
        "(no output captured)".into()
    } else {
        truncate(&joined, 300)
    }
}

/// Fill missing fields from raw content without an LLM (no translation).
pub fn summarize_offline(items: &mut [NewsItem]) {
    for it in items.iter_mut() {
        if it.title_zh.is_none() {
            it.title_zh = Some(it.title.clone());
        }
        if it.summary.is_none() {
            let s = it.snippet.trim();
            it.summary = Some(if s.is_empty() { it.title.clone() } else { first_sentence(s) });
        }
        if it.body_md.is_none() {
            // Fall back to the raw source excerpt (often already Markdown).
            it.body_md = Some(if it.snippet.trim().is_empty() {
                it.summary.clone().unwrap_or_default()
            } else {
                it.snippet.clone()
            });
        }
        // English digest falls back to the original title / raw excerpt.
        if it.title_en.is_none() {
            it.title_en = Some(it.title.clone());
        }
        if it.summary_en.is_none() {
            let s = it.snippet.trim();
            it.summary_en = Some(if s.is_empty() { it.title.clone() } else { first_sentence(s) });
        }
        if it.body_md_en.is_none() {
            it.body_md_en = Some(if it.snippet.trim().is_empty() {
                it.title.clone()
            } else {
                it.snippet.clone()
            });
        }
        if it.importance.is_none() {
            // Without an LLM score, rank by engagement where available.
            it.importance = Some(it.score.unwrap_or(0).clamp(0, 100));
        }
    }
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
        .with_context(|| format!("parsing codex JSON output: {}", truncate(raw, 200)))?;
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
    // Anything codex skipped still gets an offline fallback.
    summarize_offline(items);
    Ok(())
}

fn first_sentence(s: &str) -> String {
    // Cut after the first sentence-ending punctuation, on a char boundary.
    let mut end = s.len();
    for (i, c) in s.char_indices() {
        if matches!(c, '.' | '!' | '?' | '。' | '！' | '？') {
            end = i + c.len_utf8();
            break;
        }
    }
    truncate(s[..end].trim(), 220)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}
