use crate::model::NewsItem;
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Hard ceiling on a single `codex exec` call. Past this we kill it and fall
/// back to offline summaries rather than hanging the whole run.
const CODEX_TIMEOUT: Duration = Duration::from_secs(300);

/// Items per codex call. Smaller batches keep each call within the timeout and
/// limit the blast radius of any single failure.
const CHUNK_SIZE: usize = 12;

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
        if let Err(e) = summarize_chunk(chunk, model) {
            eprintln!("    chunk {} failed ({e:#}); using offline fallback", i + 1);
            summarize_offline(chunk);
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

fn summarize_chunk(items: &mut [NewsItem], model: Option<&str>) -> Result<()> {
    let tmp = std::env::temp_dir();
    let schema_path = tmp.join("news-fetcher-schema.json");
    let out_path = tmp.join(format!("news-fetcher-out-{}.json", std::process::id()));
    std::fs::write(&schema_path, output_schema().to_string()).context("writing schema")?;
    let _ = std::fs::remove_file(&out_path);

    let prompt = build_prompt(items);

    let mut cmd = Command::new("codex");
    // Give codex an empty stdin: `codex exec` treats a non-TTY stdin as piped
    // input and blocks waiting for EOF, so without this it hangs indefinitely.
    cmd.stdin(Stdio::null())
        // Results are read from the -o file; silence codex's own chatter.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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

    let deadline = Instant::now() + CODEX_TIMEOUT;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow!("codex exec exceeded {}s timeout", CODEX_TIMEOUT.as_secs()));
            }
            None => std::thread::sleep(Duration::from_millis(500)),
        }
    };
    if !status.success() {
        return Err(anyhow!("codex exec exited with status {status}"));
    }

    let raw = std::fs::read_to_string(&out_path).context("reading codex output")?;
    apply_summaries(items, &raw)?;
    Ok(())
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
        if it.importance.is_none() {
            // Without an LLM score, rank by engagement where available.
            it.importance = Some(it.score.unwrap_or(0).clamp(0, 100));
        }
    }
}

fn build_prompt(items: &[NewsItem]) -> String {
    let mut p = String::from(
        "你是一名科技新闻编辑，专门追踪 AI 编程智能体（Claude Code 与 OpenAI Codex）领域的动态。\
对下面每一条资讯，请完成：\n\
1. title_zh：用中文重写一个清晰的标题；产品名、版本号、命令、代码标识符等保留英文原文。\n\
2. summary：一句话的中文导语（不超过 40 字），概括这条资讯的核心。\n\
3. body_md：用中文写一段「翔实但精炼」的正文，使用 Markdown 格式排版，要求：\n\
   - 先写 1-2 句概述，再用无序列表（- 开头）列出关键改动/要点；\n\
   - 涉及命令、配置、代码、API、文件名时，用反引号 `code` 行内代码或用 ```语言 围栏代码块（标注语言，如 ```bash、```json、```ts），保留英文原文；\n\
   - 写出具体信息（功能名、修复点、版本号、影响范围），不要泛泛而谈，也不要编造原文没有的内容；\n\
   - 不要包含一级标题，不要重复 title。\n\
4. tags：1-3 个简短的英文小写标签（如 \"claude-code\"、\"codex\"、\"release\"、\"security\"、\"tooling\"、\"discussion\"）。\n\
5. importance：0-100 的整数，表示这条资讯对开发者的重要程度（官方重大版本/安全修复偏高，闲聊/重复内容偏低）。\n\
技术术语、专有名词在中文不自然时保留英文。只输出符合 schema 的 JSON。\n\n资讯列表：\n",
    );
    for it in items {
        p.push_str(&format!(
            "\n- id: {}\n  source: {}\n  title: {}\n  score: {}\n  text: {}\n",
            it.id,
            it.source,
            it.title,
            it.score.map(|s| s.to_string()).unwrap_or_else(|| "n/a".into()),
            if it.snippet.is_empty() { "(no body)" } else { &it.snippet }
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
                        "tags": { "type": "array", "items": { "type": "string" } },
                        "importance": { "type": "integer" }
                    },
                    "required": ["id", "title_zh", "summary", "body_md", "tags", "importance"],
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
