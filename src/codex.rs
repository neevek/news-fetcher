//! Shared `codex exec` runner.
//!
//! Both summarization and editorial ranking ask `codex` for schema-constrained
//! JSON. This module owns that one subprocess contract: write the schema, spawn
//! `codex exec` (read-only sandbox, empty stdin), stream its output live while
//! capturing it for diagnostics, enforce a hard timeout, and return the raw
//! JSON it writes to the `-o` file. Callers own prompt building and parsing.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Hard ceiling on a single `codex exec` call. Past this we kill the process
/// and return an error — letting the caller retry/split rather than hang.
pub const CODEX_TIMEOUT: Duration = Duration::from_secs(600);

/// Run `codex exec` with `prompt`, constraining output to `schema`, and return
/// the raw JSON string written to the output file. Errors on launch failure,
/// non-zero exit (with a distilled reason), or timeout.
pub fn exec_json(prompt: &str, schema: &Value, model: &str, thinking: &str) -> Result<String> {
    let tmp = std::env::temp_dir();
    let schema_path = tmp.join("news-fetcher-schema.json");
    let out_path = tmp.join(format!("news-fetcher-out-{}.json", std::process::id()));
    std::fs::write(&schema_path, schema.to_string()).context("writing schema")?;
    let _ = std::fs::remove_file(&out_path);

    if std::env::var("NF_DUMP_PROMPT").is_ok() {
        let _ = std::fs::write("/tmp/nf-prompt.txt", prompt);
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
        .arg("never")
        .arg("-m")
        .arg(model)
        // Reasoning effort, as a codex config override. Quoted so codex parses
        // it as a TOML string value.
        .arg("-c")
        .arg(format!("model_reasoning_effort=\"{thinking}\""));
    cmd.arg(prompt);

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

    std::fs::read_to_string(&out_path).context("reading codex output")
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
    let mut tail: Vec<&str> = log.lines().rev().filter(|l| !l.trim().is_empty()).take(3).collect();
    tail.reverse();
    let joined = tail.join(" | ");
    if joined.is_empty() {
        "(no output captured)".into()
    } else {
        truncate(&joined, 300)
    }
}

/// Truncate to `max` chars, appending an ellipsis when shortened. Shared by the
/// JSON-parsing call sites for bounded error context.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).chain(std::iter::once('…')).collect()
}
