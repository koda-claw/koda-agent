use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{Local, NaiveDateTime};
use koda_agent_core::{
    AgentConfig, ChatMessage, LlmClient, auto_make_url,
    python_runtime::{PythonCommand, PythonPurpose, python_unavailable_message, resolve_python},
    redact_secret,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime},
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

const DEFAULT_VISION_PROMPT: &str = "详细描述这张图片的内容";
const DEFAULT_VISION_MAX_PIXELS: u32 = 1_440_000;

pub fn init_memory(cfg: &AgentConfig) -> Result<()> {
    cfg.ensure_dirs()
}

pub fn append_insight(cfg: &AgentConfig, value: &Value) -> Result<()> {
    fs::create_dir_all(&cfg.memory_dir)?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cfg.memory_dir.join("global_mem.txt"))?;
    writeln!(f, "\n## {}\n{}", Local::now().format("%Y-%m-%d"), value)?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct L4ArchiveReport {
    pub processed: usize,
    pub skipped: usize,
    pub errors: usize,
    pub new_sessions: usize,
    pub deleted_raw: usize,
    pub dry_run: bool,
    pub sessions: Vec<String>,
    pub skipped_reasons: Vec<(String, String)>,
    pub error_details: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct L4Compressed {
    session_name: String,
    file_name: String,
    year: String,
    content: String,
}

pub fn archive_l4_sessions(
    cfg: &AgentConfig,
    src_dir: Option<&Path>,
    dry_run: bool,
) -> Result<L4ArchiveReport> {
    archive_l4_sessions_with_min_age(cfg, src_dir, dry_run, Duration::from_secs(7200))
}

fn archive_l4_sessions_with_min_age(
    cfg: &AgentConfig,
    src_dir: Option<&Path>,
    dry_run: bool,
    min_age: Duration,
) -> Result<L4ArchiveReport> {
    let l4_dir = cfg.memory_dir.join("L4_raw_sessions");
    fs::create_dir_all(&l4_dir)?;
    let src_dir = src_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cfg.temp_dir.join("model_responses"));
    let mut report = L4ArchiveReport {
        dry_run,
        ..Default::default()
    };
    let Ok(rd) = fs::read_dir(&src_dir) else {
        return Ok(report);
    };
    let mut raw_files = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with("model_responses_") && s.ends_with(".txt"))
        })
        .collect::<Vec<_>>();
    raw_files.sort();
    let existing = existing_l4_sessions(&l4_dir)?;
    let now = SystemTime::now();
    let mut results = Vec::new();
    for path in &raw_files {
        let fname = file_name(path);
        if let Ok(modified) = fs::metadata(path).and_then(|m| m.modified())
            && now.duration_since(modified).unwrap_or_default() < min_age
        {
            report.skipped += 1;
            report.skipped_reasons.push((fname, "recent(<2h)".into()));
            continue;
        }
        match compress_l4_session(path) {
            Ok(Some(compressed)) => {
                if existing.contains_key(&compressed.session_name) {
                    report.skipped += 1;
                    report
                        .skipped_reasons
                        .push((fname, format!("dup:{}", compressed.session_name)));
                } else {
                    results.push((compressed, path.clone()));
                }
            }
            Ok(None) => {
                report.skipped += 1;
                report
                    .skipped_reasons
                    .push((fname, "too small or no timestamps".into()));
            }
            Err(e) => {
                report.errors += 1;
                report.error_details.push((fname, e.to_string()));
            }
        }
    }
    results.sort_by(|a, b| a.0.session_name.cmp(&b.0.session_name));
    report.processed = results.len();
    report.new_sessions = results.len();
    report.sessions = results
        .iter()
        .map(|(c, _)| c.session_name.clone())
        .collect();
    if dry_run {
        return Ok(report);
    }
    append_l4_histories(&l4_dir, results.iter().map(|(c, _)| c))?;
    archive_l4_zip(&l4_dir, results.iter().map(|(c, _)| c))?;
    for (_, raw) in &results {
        if fs::remove_file(raw).is_ok() {
            report.deleted_raw += 1;
        }
    }
    for (fname, reason) in &report.skipped_reasons {
        if reason.contains("recent") {
            continue;
        }
        if let Some(path) = raw_files.iter().find(|p| file_name(p) == *fname)
            && fs::remove_file(path).is_ok()
        {
            report.deleted_raw += 1;
        }
    }
    Ok(report)
}

fn compress_l4_session(src: &Path) -> Result<Option<L4Compressed>> {
    let text = fs::read_to_string(src)?;
    let timestamps = extract_l4_timestamps(&text);
    let Some(first_ts) = timestamps.first() else {
        return Ok(None);
    };
    let first = format_l4_timestamp(first_ts).context("bad timestamp format")?;
    let last = timestamps
        .last()
        .and_then(|ts| format_l4_timestamp(ts))
        .unwrap_or_else(|| first.clone());
    let format = detect_l4_format(&text);
    let content = if format == "raw" {
        compress_l4_raw(&text)
    } else {
        text
    };
    if content.len() < 4500 {
        return Ok(None);
    }
    let session_name = format!("{first}-{last}");
    Ok(Some(L4Compressed {
        session_name: session_name.clone(),
        file_name: format!("{session_name}.txt"),
        year: first_ts.chars().take(4).collect(),
        content,
    }))
}

fn extract_l4_timestamps(text: &str) -> Vec<String> {
    let prompt =
        Regex::new(r"(?m)^=== Prompt ===(?: (\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}))?").unwrap();
    let mut out = prompt
        .captures_iter(text)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .collect::<Vec<_>>();
    if out.is_empty() {
        let response =
            Regex::new(r"(?m)^=== Response ===(?: (\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}))?")
                .unwrap();
        out = response
            .captures_iter(text)
            .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
            .collect();
    }
    out
}

fn format_l4_timestamp(ts: &str) -> Option<String> {
    NaiveDateTime::parse_from_str(ts.trim(), "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|dt| dt.format("%m%d_%H%M").to_string())
}

fn detect_l4_format(text: &str) -> &'static str {
    let Some(pos) = text.find("=== Prompt ===") else {
        return "unknown";
    };
    let after_marker = text[pos..]
        .find('\n')
        .map(|i| pos + i + 1)
        .unwrap_or(text.len());
    if text[after_marker..]
        .chars()
        .take(200)
        .collect::<String>()
        .trim_start()
        .starts_with('{')
    {
        "json"
    } else {
        "raw"
    }
}

#[derive(Debug)]
struct L4Section {
    typ: &'static str,
    line: String,
    body: String,
}

fn parse_l4_sections(text: &str) -> Vec<L4Section> {
    let re = Regex::new(r"(?m)^=== (Prompt|Response|USER|ASSISTANT) ===(?:.*)?$").unwrap();
    let markers = re.find_iter(text).collect::<Vec<_>>();
    if markers.is_empty() {
        return vec![L4Section {
            typ: "preamble",
            line: String::new(),
            body: text.to_string(),
        }];
    }
    let mut out = Vec::new();
    if markers[0].start() > 0 {
        out.push(L4Section {
            typ: "preamble",
            line: String::new(),
            body: text[..markers[0].start()].to_string(),
        });
    }
    for (idx, marker) in markers.iter().enumerate() {
        let line = marker.as_str();
        let end = markers
            .get(idx + 1)
            .map(|m| m.start())
            .unwrap_or(text.len());
        let typ = if line.starts_with("=== Prompt") {
            "prompt"
        } else if line.starts_with("=== Response") {
            "response"
        } else if line.starts_with("=== USER") {
            "user"
        } else {
            "assistant"
        };
        out.push(L4Section {
            typ,
            line: line.to_string(),
            body: text[marker.end()..end].to_string(),
        });
    }
    out
}

fn compress_l4_raw(text: &str) -> String {
    let sections = parse_l4_sections(text);
    let mut out = String::new();
    for (idx, section) in sections.iter().enumerate() {
        match section.typ {
            "prompt" => {
                out.push_str(&section.line);
                out.push('\n');
                if sections.get(idx + 1).is_none_or(|s| s.typ != "user") {
                    out.push_str(&section.body);
                }
            }
            "user" | "response" => {
                out.push_str(&section.line);
                out.push('\n');
                out.push_str(&section.body);
            }
            "preamble" => out.push_str(&section.body),
            _ => {}
        }
    }
    out
}

fn append_l4_histories<'a>(
    l4_dir: &Path,
    sessions: impl Iterator<Item = &'a L4Compressed>,
) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(l4_dir.join("all_histories.txt"))?;
    for session in sessions {
        let history = extract_l4_history(&session.content);
        if !history.is_empty() {
            writeln!(f)?;
            write!(
                f,
                "{}",
                format_l4_history_block(&session.session_name, &history)
            )?;
        }
    }
    Ok(())
}

fn extract_l4_history(text: &str) -> Vec<String> {
    let re = Regex::new(r"(?s)<history>(.*?)</history>").unwrap();
    let blocks = re
        .captures_iter(text)
        .filter_map(|c| c.get(1).map(|m| parse_l4_history_block(m.as_str())))
        .filter(|b| !b.is_empty())
        .collect::<Vec<_>>();
    merge_l4_history_blocks(&blocks)
}

fn parse_l4_history_block(raw: &str) -> Vec<String> {
    let lines = raw
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("[USER]") || line.starts_with("[Agent]"))
        .map(str::to_string)
        .collect::<Vec<_>>();
    if lines.len() >= 2 {
        return lines;
    }
    if raw.contains("\\n[USER]") || raw.contains("\\n[Agent]") {
        return raw
            .replace("\\n", "\n")
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("[USER]") || line.starts_with("[Agent]"))
            .map(str::to_string)
            .collect();
    }
    lines
}

fn merge_l4_history_blocks(blocks: &[Vec<String>]) -> Vec<String> {
    let Some(first) = blocks.first() else {
        return Vec::new();
    };
    let mut acc = first.clone();
    for block in blocks.iter().skip(1).filter(|b| !b.is_empty()) {
        if acc.is_empty() {
            acc = block.clone();
            continue;
        }
        let mut best = 0;
        for k in 1..=acc.len().min(block.len()) {
            if acc[acc.len() - k..] == block[..k] {
                best = k;
            }
        }
        if best > 0 {
            acc.extend_from_slice(&block[best..]);
        } else if let Some(idx) = acc.iter().rposition(|line| line == &block[0]) {
            let mut match_len = 0;
            for j in 0..block.len().min(acc.len() - idx) {
                if acc[idx + j] == block[j] {
                    match_len = j + 1;
                } else {
                    break;
                }
            }
            acc.extend_from_slice(&block[match_len..]);
        } else {
            acc.extend_from_slice(block);
        }
    }
    acc
}

fn format_l4_history_block(session_name: &str, history_lines: &[String]) -> String {
    let sep = "=".repeat(60);
    format!(
        "{sep}\nSESSION: {session_name}\n{sep}\n{}\n",
        history_lines.join("\n")
    )
}

fn existing_l4_sessions(l4_dir: &Path) -> Result<BTreeMap<String, ()>> {
    let mut out = BTreeMap::new();
    let path = l4_dir.join("all_histories.txt");
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(out);
    };
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("SESSION: ") {
            out.insert(name.trim().to_string(), ());
        }
    }
    Ok(out)
}

fn archive_l4_zip<'a>(
    l4_dir: &Path,
    sessions: impl Iterator<Item = &'a L4Compressed>,
) -> Result<()> {
    let mut by_month: BTreeMap<String, Vec<&L4Compressed>> = BTreeMap::new();
    for session in sessions {
        by_month
            .entry(format!("{}-{}", session.year, &session.session_name[..2]))
            .or_default()
            .push(session);
    }
    for (month, sessions) in by_month {
        let zpath = l4_dir.join(format!("{month}.zip"));
        let existing_bytes = fs::read(&zpath).unwrap_or_default();
        let mut existing_entries = BTreeMap::new();
        if !existing_bytes.is_empty()
            && let Ok(mut zr) = ZipArchive::new(std::io::Cursor::new(&existing_bytes))
        {
            for i in 0..zr.len() {
                let mut file = zr.by_index(i)?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                existing_entries.insert(file.name().to_string(), bytes);
            }
        }
        let file = fs::File::create(&zpath)?;
        let mut zw = ZipWriter::new(file);
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let existing_names = existing_entries.keys().cloned().collect::<Vec<_>>();
        for (name, bytes) in existing_entries {
            zw.start_file(name, opts)?;
            zw.write_all(&bytes)?;
        }
        for session in sessions {
            if existing_names.contains(&session.file_name) {
                continue;
            }
            zw.start_file(&session.file_name, opts)?;
            zw.write_all(session.content.as_bytes())?;
        }
        zw.finish()?;
    }
    Ok(())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SettlementReport {
    pub processed: usize,
    pub applied_l1: usize,
    pub applied_l2: usize,
    pub applied_l3: usize,
    pub pending: usize,
    pub assisted_attempts: usize,
    pub assisted_applied: usize,
    pub archive_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct MemoryAuditReport {
    pub l1_lines: usize,
    pub l1_valid_lines: usize,
    pub l1_invalid_lines: Vec<String>,
    pub l1_duplicate_lines: Vec<String>,
    pub l2_sections: Vec<String>,
    pub l3_files: Vec<String>,
    pub missing_l1_pointers: Vec<String>,
    pub pending_updates: usize,
    pub l4_sessions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct MemoryCleanupReport {
    pub dry_run: bool,
    pub original_l1_lines: usize,
    pub new_l1_lines: usize,
    pub removed_invalid: Vec<String>,
    pub removed_duplicates: Vec<String>,
    pub added_missing_pointers: Vec<String>,
    pub backup_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct L4RecallHit {
    pub session: String,
    pub score: usize,
    pub excerpt: String,
}

pub fn audit_memory(cfg: &AgentConfig) -> Result<MemoryAuditReport> {
    fs::create_dir_all(&cfg.memory_dir)?;
    let l1_text =
        fs::read_to_string(cfg.memory_dir.join("global_mem_insight.txt")).unwrap_or_default();
    let (valid_l1, invalid_l1, duplicate_l1) = classify_l1_lines(&l1_text);
    let l2_sections = l2_sections(cfg)?;
    let l3_files = l3_memory_files(cfg)?;
    let mut expected = l2_sections
        .iter()
        .map(|section| l1_pointer_for_l2(section))
        .chain(l3_files.iter().map(|file| l1_pointer_for_l3(file)))
        .filter_map(|p| normalize_l1_pointer(&p))
        .collect::<Vec<_>>();
    expected = dedupe_preserve_order(expected);
    let missing_l1_pointers = expected
        .into_iter()
        .filter(|pointer| !l1_lines_cover_pointer(&valid_l1, pointer))
        .collect();
    let pending_updates = fs::read_to_string(cfg.memory_dir.join("pending_long_term_updates.md"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.trim_start().starts_with("```json"))
        .count();
    let l4_sessions = existing_l4_sessions(&cfg.memory_dir.join("L4_raw_sessions"))
        .map(|m| m.len())
        .unwrap_or(0);
    Ok(MemoryAuditReport {
        l1_lines: l1_text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        l1_valid_lines: valid_l1.len(),
        l1_invalid_lines: invalid_l1,
        l1_duplicate_lines: duplicate_l1,
        l2_sections,
        l3_files,
        missing_l1_pointers,
        pending_updates,
        l4_sessions,
    })
}

pub fn cleanup_memory_indexes(
    cfg: &AgentConfig,
    dry_run: bool,
    sync_missing: bool,
) -> Result<MemoryCleanupReport> {
    fs::create_dir_all(&cfg.memory_dir)?;
    let path = cfg.memory_dir.join("global_mem_insight.txt");
    let original = fs::read_to_string(&path).unwrap_or_default();
    let (valid_l1, invalid_l1, duplicate_l1) = classify_l1_lines(&original);
    let mut next = valid_l1;
    let mut added = Vec::new();
    if sync_missing {
        let audit = audit_memory(cfg)?;
        for pointer in audit.missing_l1_pointers {
            if next.len() >= 30 {
                break;
            }
            if !next.iter().any(|line| line == &pointer) {
                next.push(pointer.clone());
                added.push(pointer);
            }
        }
    }
    let mut report = MemoryCleanupReport {
        dry_run,
        original_l1_lines: original
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        new_l1_lines: next.len(),
        removed_invalid: invalid_l1,
        removed_duplicates: duplicate_l1,
        added_missing_pointers: added,
        backup_path: None,
    };
    if !dry_run {
        if path.exists() {
            let backup = cfg.memory_dir.join(format!(
                "global_mem_insight.{}.bak",
                Local::now().format("%Y%m%d_%H%M%S")
            ));
            fs::copy(&path, &backup)?;
            report.backup_path = Some(backup);
        }
        let text = next
            .into_iter()
            .map(|line| {
                if line.starts_with('#') {
                    line
                } else {
                    format!("- {line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(
            path,
            if text.is_empty() {
                text
            } else {
                format!("{text}\n")
            },
        )?;
    }
    Ok(report)
}

pub fn recall_l4_history(cfg: &AgentConfig, query: &str, limit: usize) -> Result<Vec<L4RecallHit>> {
    let query_terms = query
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    if query_terms.is_empty() {
        return Ok(Vec::new());
    }
    let blocks = l4_recall_blocks(cfg)?;
    let mut hits = blocks
        .into_iter()
        .filter_map(|(session, body)| {
            let lower = body.to_ascii_lowercase();
            let score = query_terms
                .iter()
                .map(|term| lower.matches(term).count())
                .sum::<usize>();
            (score > 0).then(|| L4RecallHit {
                session,
                score,
                excerpt: clean_l4_excerpt(&excerpt_for_terms(&body, &query_terms, 500)),
            })
        })
        .collect::<Vec<_>>();
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.session.cmp(&a.session))
    });
    hits.truncate(limit.max(1));
    Ok(hits)
}

fn l4_recall_blocks(cfg: &AgentConfig) -> Result<Vec<(String, String)>> {
    let l4_dir = cfg.memory_dir.join("L4_raw_sessions");
    let mut blocks = Vec::new();
    let all_histories = fs::read_to_string(l4_dir.join("all_histories.txt")).unwrap_or_default();
    blocks.extend(split_l4_history_blocks(&all_histories));
    let Ok(rd) = fs::read_dir(&l4_dir) else {
        return Ok(blocks);
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file()
            || path.file_name().and_then(|s| s.to_str()) == Some("all_histories.txt")
            || path.extension().and_then(|s| s.to_str()) != Some("json")
        {
            continue;
        }
        let raw = fs::read_to_string(&path).unwrap_or_default();
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let session = value
            .get("session")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        let body = render_l4_json_for_recall(&value);
        if !session.is_empty() && !body.trim().is_empty() {
            blocks.push((session, body));
        }
    }
    Ok(blocks)
}

fn render_l4_json_for_recall(value: &Value) -> String {
    let mut out = Vec::new();
    if let Some(history) = value.get("history").and_then(Value::as_array) {
        out.extend(history.iter().filter_map(Value::as_str).map(str::to_string));
    }
    if let Some(messages) = value.get("messages").and_then(Value::as_array) {
        for msg in messages {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("message");
            let Some(content) = msg.get("content") else {
                continue;
            };
            match content {
                Value::String(s) => out.push(format!("[{role}] {s}")),
                Value::Object(obj) => {
                    for key in ["text", "thinking", "summary", "error"] {
                        if let Some(s) = obj.get(key).and_then(Value::as_str) {
                            out.push(format!("[{role}.{key}] {s}"));
                        }
                    }
                    if let Some(calls) = obj.get("tool_calls").and_then(Value::as_array) {
                        for call in calls {
                            if let Some(name) = call.get("name").and_then(Value::as_str) {
                                out.push(format!("[tool_call] {name}"));
                            }
                        }
                    }
                }
                Value::Array(items) => {
                    for item in items {
                        if let Some(s) = item.get("text").and_then(Value::as_str) {
                            out.push(format!("[{role}] {s}"));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out.join("\n")
}

/// Best-effort native settlement for queued long-term memory requests.
///
/// Upstream normally asks the LLM to inspect L0 and patch memory directly after
/// `start_long_term_update`. This worker handles structured requests
/// deterministically and moves ambiguous entries into a review file, preserving
/// the same "no unverified secret-like data" safety boundary.
pub fn settle_long_term_updates(cfg: &AgentConfig) -> Result<SettlementReport> {
    fs::create_dir_all(&cfg.memory_dir)?;
    let queue = cfg.memory_dir.join("long_term_updates.jsonl");
    if !queue.exists() {
        return Ok(SettlementReport::default());
    }
    let raw = fs::read_to_string(&queue)?;
    let mut report = SettlementReport::default();
    let mut pending = Vec::new();
    for line in raw.lines().map(str::trim).filter(|line| !line.is_empty()) {
        report.processed += 1;
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(_) => {
                pending.push(json!({"reason":"invalid_json","raw":line}));
                report.pending += 1;
                continue;
            }
        };
        match apply_memory_update(cfg, &value)? {
            ApplyOutcome::Applied { l1, l2, l3 } => {
                report.applied_l1 += l1;
                report.applied_l2 += l2;
                report.applied_l3 += l3;
            }
            ApplyOutcome::Pending(reason) => {
                pending.push(json!({"reason":reason,"entry":redact_value(&value)}));
                report.pending += 1;
            }
        }
    }
    if !pending.is_empty() {
        append_pending_updates(cfg, &pending)?;
    }
    let archive_name = format!(
        "long_term_updates.processed.{}.jsonl",
        Local::now().format("%Y%m%d_%H%M%S")
    );
    let archive = cfg.memory_dir.join(archive_name);
    fs::rename(&queue, &archive)?;
    report.archive_path = Some(archive);
    Ok(report)
}

pub async fn settle_long_term_updates_assisted(
    cfg: &AgentConfig,
    llm: &(dyn LlmClient + Send + Sync),
) -> Result<SettlementReport> {
    fs::create_dir_all(&cfg.memory_dir)?;
    let queue = cfg.memory_dir.join("long_term_updates.jsonl");
    if !queue.exists() {
        return Ok(SettlementReport::default());
    }
    let raw = fs::read_to_string(&queue)?;
    let mut report = SettlementReport::default();
    let mut pending = Vec::new();
    for line in raw.lines().map(str::trim).filter(|line| !line.is_empty()) {
        report.processed += 1;
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(_) => {
                pending.push(json!({"reason":"invalid_json","raw":line}));
                report.pending += 1;
                continue;
            }
        };
        match apply_memory_update(cfg, &value)? {
            ApplyOutcome::Applied { l1, l2, l3 } => {
                report.applied_l1 += l1;
                report.applied_l2 += l2;
                report.applied_l3 += l3;
            }
            ApplyOutcome::Pending(reason) => {
                report.assisted_attempts += 1;
                match assisted_memory_update(cfg, llm, &value, reason).await {
                    Ok(Some(ApplyOutcome::Applied { l1, l2, l3 })) => {
                        report.applied_l1 += l1;
                        report.applied_l2 += l2;
                        report.applied_l3 += l3;
                        report.assisted_applied += 1;
                    }
                    Ok(Some(ApplyOutcome::Pending(new_reason))) => {
                        pending.push(json!({"reason":new_reason,"entry":redact_value(&value)}));
                        report.pending += 1;
                    }
                    Ok(None) => {
                        pending.push(json!({"reason":reason,"entry":redact_value(&value)}));
                        report.pending += 1;
                    }
                    Err(e) => {
                        pending.push(json!({"reason":"assistant_error","error":e.to_string(),"entry":redact_value(&value)}));
                        report.pending += 1;
                    }
                }
            }
        }
    }
    if !pending.is_empty() {
        append_pending_updates(cfg, &pending)?;
    }
    let archive_name = format!(
        "long_term_updates.processed.{}.jsonl",
        Local::now().format("%Y%m%d_%H%M%S")
    );
    let archive = cfg.memory_dir.join(archive_name);
    fs::rename(&queue, &archive)?;
    report.archive_path = Some(archive);
    Ok(report)
}

async fn assisted_memory_update(
    cfg: &AgentConfig,
    llm: &(dyn LlmClient + Send + Sync),
    entry: &Value,
    reason: &'static str,
) -> Result<Option<ApplyOutcome>> {
    if reason == "secret_like_content" || reason == "empty_or_unstructured" {
        return Ok(Some(ApplyOutcome::Pending(reason)));
    }
    let prompt = build_assisted_settlement_prompt(cfg, entry, reason);
    let messages = vec![
        ChatMessage::text(
            "system",
            "You convert GenericAgent memory settlement notes into safe JSON patches. Return JSON only.",
        ),
        ChatMessage::text("user", prompt),
    ];
    let response = llm.chat(&messages, &json!([])).await?;
    let Some(value) = parse_assisted_settlement_response(&response.content) else {
        return Ok(None);
    };
    let updates = value
        .get("updates")
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| value.as_array().cloned())
        .unwrap_or_else(|| vec![value]);
    let mut total = AppliedCounts {
        l1: 0,
        l2: 0,
        l3: 0,
    };
    let mut applied_any = false;
    for update in updates {
        match apply_memory_update(cfg, &update)? {
            ApplyOutcome::Applied { l1, l2, l3 } => {
                total.l1 += l1;
                total.l2 += l2;
                total.l3 += l3;
                applied_any = true;
            }
            ApplyOutcome::Pending(_) => {}
        }
    }
    if applied_any {
        Ok(Some(ApplyOutcome::Applied {
            l1: total.l1,
            l2: total.l2,
            l3: total.l3,
        }))
    } else {
        Ok(Some(ApplyOutcome::Pending("assistant_no_safe_patch")))
    }
}

fn build_assisted_settlement_prompt(cfg: &AgentConfig, entry: &Value, reason: &str) -> String {
    let l0 =
        fs::read_to_string(cfg.memory_dir.join("memory_management_sop.md")).unwrap_or_default();
    let l1 = fs::read_to_string(cfg.memory_dir.join("global_mem_insight.txt")).unwrap_or_default();
    let l2 = fs::read_to_string(cfg.memory_dir.join("global_mem.txt")).unwrap_or_default();
    format!(
        "Reason original structured apply failed: {reason}\n\
Entry to classify, with secrets already prohibited:\n{}\n\n\
Memory SOP L0 excerpt:\n{}\n\n\
Current L1 global_mem_insight.txt:\n{}\n\n\
Current L2 global_mem.txt:\n{}\n\n\
Return JSON only in this shape:\n\
{{\"updates\":[{{\"l1_pointer\":\"short index pointer if needed\",\"l2\":{{\"section\":\"Topic\",\"facts\":[\"action-verified fact\"]}},\"l3\":{{\"file\":\"optional_sop.md\",\"content\":\"short reusable task note\"}}}}]}}\n\
Rules: no passwords, no API keys, no volatile timestamps, no guesses, omit empty layers, keep L1 short.",
        serde_json::to_string_pretty(&redact_value(entry)).unwrap_or_else(|_| "{}".into()),
        truncate_for_prompt(&l0, 4000),
        truncate_for_prompt(&l1, 2000),
        truncate_for_prompt(&l2, 6000),
    )
}

fn parse_assisted_settlement_response(content: &str) -> Option<Value> {
    let trimmed = content.trim();
    serde_json::from_str(trimmed)
        .ok()
        .or_else(|| {
            let stripped = trimmed
                .strip_prefix("```json")
                .or_else(|| trimmed.strip_prefix("```"))
                .and_then(|s| s.strip_suffix("```"))
                .map(str::trim)?;
            serde_json::from_str(stripped).ok()
        })
        .or_else(|| {
            let start = trimmed.find('{').or_else(|| trimmed.find('['))?;
            let end_obj = trimmed.rfind('}');
            let end_arr = trimmed.rfind(']');
            let end = match (end_obj, end_arr) {
                (Some(a), Some(b)) => a.max(b),
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => return None,
            };
            serde_json::from_str(&trimmed[start..=end]).ok()
        })
}

fn truncate_for_prompt(text: &str, max_chars: usize) -> String {
    let len = text.chars().count();
    if len <= max_chars {
        text.to_string()
    } else {
        format!(
            "{}\n...[truncated {} chars]",
            text.chars().take(max_chars).collect::<String>(),
            len - max_chars
        )
    }
}

#[derive(Debug, Clone, Copy)]
struct AppliedCounts {
    l1: usize,
    l2: usize,
    l3: usize,
}

enum ApplyOutcome {
    Applied { l1: usize, l2: usize, l3: usize },
    Pending(&'static str),
}

fn apply_memory_update(cfg: &AgentConfig, value: &Value) -> Result<ApplyOutcome> {
    if value.as_object().is_none_or(|obj| obj.is_empty()) {
        return Ok(ApplyOutcome::Pending("empty_or_unstructured"));
    }
    if contains_secret_marker(value) {
        return Ok(ApplyOutcome::Pending("secret_like_content"));
    }
    let mut counts = AppliedCounts {
        l1: 0,
        l2: 0,
        l3: 0,
    };
    let explicit_l1 = string_field(value, &["l1", "l1_pointer", "pointer", "keyword"]);
    let mut auto_l1 = Vec::new();
    if let Some(l2) = value.get("l2").and_then(Value::as_object) {
        if let Some(applied) = apply_l2_object(cfg, l2)? {
            counts.l2 += 1;
            auto_l1.push(applied.l1_pointer);
        }
    } else if let Some(fact) = string_field(value, &["fact", "memory", "content"]) {
        let layer = string_field(value, &["layer"])
            .unwrap_or_else(|| "L2".into())
            .to_ascii_lowercase();
        if layer == "l2" || layer == "fact" {
            let section = string_field(value, &["section", "topic"])
                .unwrap_or_else(|| "Long Term Updates".into());
            if append_l2_fact(cfg, &section, &fact)? {
                counts.l2 += 1;
                auto_l1.push(l1_pointer_for_l2(&section));
            }
        }
    }
    if let Some(arr) = value.get("facts").and_then(Value::as_array) {
        let section = string_field(value, &["section", "topic"])
            .unwrap_or_else(|| "Long Term Updates".into());
        let mut changed = false;
        for fact in arr.iter().filter_map(Value::as_str) {
            changed |= append_l2_fact(cfg, &section, fact)?;
        }
        if changed {
            counts.l2 += 1;
            auto_l1.push(l1_pointer_for_l2(&section));
        }
    }
    if let Some(pointer) = explicit_l1.as_deref()
        && ensure_l1_pointer(cfg, pointer)?
    {
        counts.l1 += 1;
    }
    if let Some(l3) = value.get("l3").and_then(Value::as_object)
        && let Some(applied) = apply_l3_object(cfg, l3)?
    {
        counts.l3 += 1;
        auto_l1.push(applied.l1_pointer);
    }
    if explicit_l1.is_none() {
        for pointer in dedupe_preserve_order(auto_l1) {
            if ensure_l1_pointer(cfg, &pointer)? {
                counts.l1 += 1;
            }
        }
    }
    if counts.l1 + counts.l2 + counts.l3 == 0 {
        Ok(ApplyOutcome::Pending("unsupported_shape"))
    } else {
        Ok(ApplyOutcome::Applied {
            l1: counts.l1,
            l2: counts.l2,
            l3: counts.l3,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppliedMemoryPointer {
    l1_pointer: String,
}

fn apply_l2_object(
    cfg: &AgentConfig,
    obj: &serde_json::Map<String, Value>,
) -> Result<Option<AppliedMemoryPointer>> {
    let section =
        string_field_obj(obj, &["section", "topic"]).unwrap_or_else(|| "Long Term Updates".into());
    let mut changed = false;
    if let Some(fact) = string_field_obj(obj, &["fact", "memory", "content"]) {
        changed |= append_l2_fact(cfg, &section, &fact)?;
    }
    if let Some(facts) = obj.get("facts").and_then(Value::as_array) {
        for fact in facts.iter().filter_map(Value::as_str) {
            changed |= append_l2_fact(cfg, &section, fact)?;
        }
    }
    Ok(changed.then(|| AppliedMemoryPointer {
        l1_pointer: l1_pointer_for_l2(&section),
    }))
}

fn append_l2_fact(cfg: &AgentConfig, section: &str, fact: &str) -> Result<bool> {
    let fact = fact.trim();
    if fact.is_empty() || contains_secret_text(fact) {
        return Ok(false);
    }
    let path = cfg.memory_dir.join("global_mem.txt");
    let mut text = fs::read_to_string(&path).unwrap_or_else(|_| "# [Global Memory - L2]\n".into());
    if text.contains(fact) {
        return Ok(false);
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    let heading = format!("## [{}]", section.trim().trim_matches(&['[', ']'][..]));
    if !text.contains(&heading) {
        text.push_str(&format!("\n{heading}\n"));
    }
    text.push_str(&format!("- {fact}\n"));
    fs::write(path, text)?;
    Ok(true)
}

fn ensure_l1_pointer(cfg: &AgentConfig, pointer: &str) -> Result<bool> {
    let Some(pointer) = normalize_l1_pointer(pointer) else {
        return Ok(false);
    };
    let path = cfg.memory_dir.join("global_mem_insight.txt");
    let mut text = fs::read_to_string(&path).unwrap_or_default();
    if l1_contains_pointer(&text, &pointer) {
        return Ok(false);
    }
    let line_count = text.lines().filter(|line| !line.trim().is_empty()).count();
    if line_count >= 30 {
        return Ok(false);
    }
    if !text.ends_with('\n') && !text.is_empty() {
        text.push('\n');
    }
    text.push_str("- ");
    text.push_str(&pointer);
    text.push('\n');
    fs::write(path, text)?;
    Ok(true)
}

fn apply_l3_object(
    cfg: &AgentConfig,
    obj: &serde_json::Map<String, Value>,
) -> Result<Option<AppliedMemoryPointer>> {
    let content = string_field_obj(obj, &["content", "body", "memory"]).unwrap_or_default();
    if content.trim().is_empty()
        || contains_secret_text(&content)
        || content.chars().count() > 2000
        || looks_like_full_memory_dump(&content)
        || looks_like_unsafe_l3_update(&content)
    {
        return Ok(None);
    }
    let raw_file = string_field_obj(obj, &["file", "path", "name", "title"])
        .unwrap_or_else(|| format!("long_term_{}.md", Local::now().format("%Y%m%d_%H%M%S")));
    let file_name = sanitize_l3_file_name(&raw_file);
    if is_reserved_memory_file(&file_name) {
        return Ok(None);
    }
    let path = cfg.memory_dir.join(file_name);
    if path.exists() {
        let existing = fs::read_to_string(&path).unwrap_or_default();
        if existing.contains(content.trim()) {
            return Ok(None);
        }
        if content.chars().count() > 1200 || content.lines().count() > 40 {
            return Ok(None);
        }
        let mut updated = existing;
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push('\n');
        updated.push_str(content.trim());
        updated.push('\n');
        fs::write(&path, updated)?;
    } else {
        fs::write(&path, format!("{}\n", content.trim()))?;
    }
    Ok(Some(AppliedMemoryPointer {
        l1_pointer: l1_pointer_for_l3(
            path.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default(),
        ),
    }))
}

fn is_reserved_memory_file(file_name: &str) -> bool {
    matches!(
        file_name,
        "global_mem.txt"
            | "global_mem_insight.txt"
            | "memory_management_sop.md"
            | "long_term_updates.jsonl"
            | "pending_long_term_updates.md"
    )
}

fn l1_pointer_for_l2(section: &str) -> String {
    format!("{} -> L2", compact_l1_token(section))
}

fn l1_pointer_for_l3(file_name: &str) -> String {
    compact_l1_token(file_name)
}

fn normalize_l1_pointer(pointer: &str) -> Option<String> {
    let pointer = pointer.trim().trim_start_matches("- ").trim();
    if pointer.is_empty()
        || contains_secret_text(pointer)
        || pointer.lines().count() > 1
        || pointer.starts_with('#')
        || looks_like_l1_detail_dump(pointer)
    {
        return None;
    }
    let compact = compact_l1_token(pointer);
    (!compact.is_empty() && compact.chars().count() <= 80).then_some(compact)
}

fn compact_l1_token(text: &str) -> String {
    let mut out = text
        .trim()
        .replace(['\n', '\r', '\t'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if out.chars().count() > 80 {
        out = out.chars().take(80).collect::<String>();
        out = out
            .trim_end_matches(['-', '>', '|', ':', ',', '.', ' '])
            .to_string();
    }
    out
}

fn looks_like_l1_detail_dump(pointer: &str) -> bool {
    let lower = pointer.to_ascii_lowercase();
    lower.contains("```")
        || lower.contains("<history>")
        || lower.contains("step ")
        || lower.contains("步骤")
}

fn l1_contains_pointer(text: &str, pointer: &str) -> bool {
    let normalized = pointer.trim().trim_start_matches("- ").trim();
    text.lines()
        .map(|line| line.trim().trim_start_matches("- ").trim())
        .any(|line| line == normalized || l1_line_covers_pointer(line, normalized))
}

fn l1_lines_cover_pointer(lines: &[String], pointer: &str) -> bool {
    lines
        .iter()
        .any(|line| line == pointer || l1_line_covers_pointer(line, pointer))
}

fn l1_line_covers_pointer(line: &str, pointer: &str) -> bool {
    let line = normalize_l1_lookup_text(line);
    let pointer = normalize_l1_lookup_text(pointer);
    if pointer.is_empty() || line.is_empty() {
        return false;
    }
    if line.contains(&pointer) {
        return true;
    }
    if pointer.ends_with(" -> l2") {
        let section = pointer.trim_end_matches(" -> l2").trim();
        return !section.is_empty() && line.contains(section) && line.contains("l2");
    }
    l3_pointer_stems(&pointer)
        .into_iter()
        .any(|stem| !stem.is_empty() && line.contains(&stem))
}

fn normalize_l1_lookup_text(text: &str) -> String {
    text.to_lowercase()
        .replace(['\n', '\r', '\t'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn l3_pointer_stems(pointer: &str) -> Vec<String> {
    let mut out = Vec::new();
    for suffix in [".template.py", ".sop.md", ".md", ".py"] {
        if let Some(stem) = pointer.strip_suffix(suffix) {
            out.push(stem.to_string());
        }
    }
    if let Some((stem, _)) = pointer.rsplit_once('.') {
        out.push(stem.to_string());
    }
    dedupe_preserve_order(out)
}

fn dedupe_preserve_order(items: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for item in items {
        if !out.iter().any(|seen| seen == &item) {
            out.push(item);
        }
    }
    out
}

fn classify_l1_lines(text: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut valid = Vec::new();
    let mut invalid = Vec::new();
    let mut duplicates = Vec::new();
    for raw in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        match normalize_existing_l1_line(raw) {
            Some(line) if valid.iter().any(|seen| seen == &line) => duplicates.push(raw.into()),
            Some(line) => valid.push(line),
            None => invalid.push(raw.into()),
        }
    }
    (valid, invalid, duplicates)
}

fn normalize_existing_l1_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.starts_with('#') {
        return (!contains_secret_text(line) && line.chars().count() <= 120)
            .then(|| line.to_string());
    }
    let line = line.trim_start_matches("- ").trim();
    if line.is_empty()
        || contains_secret_text(line)
        || line.lines().count() > 1
        || looks_like_l1_detail_dump(line)
        || line.chars().count() > 220
    {
        return None;
    }
    Some(compact_l1_token_existing(line))
}

fn compact_l1_token_existing(text: &str) -> String {
    text.trim()
        .replace(['\n', '\r', '\t'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn l2_sections(cfg: &AgentConfig) -> Result<Vec<String>> {
    let text = fs::read_to_string(cfg.memory_dir.join("global_mem.txt")).unwrap_or_default();
    Ok(dedupe_preserve_order(
        text.lines()
            .filter_map(|line| {
                let line = line.trim();
                let rest = line.strip_prefix("## ")?;
                Some(rest.trim().trim_matches(&['[', ']'][..]).trim().to_string())
            })
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("Global Memory - L2"))
            .collect(),
    ))
}

fn l3_memory_files(cfg: &AgentConfig) -> Result<Vec<String>> {
    let mut files = Vec::new();
    let Ok(rd) = fs::read_dir(&cfg.memory_dir) else {
        return Ok(files);
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if is_reserved_memory_file(name)
            || matches!(
                name,
                "global_mem_insight_template.txt" | "file_access_stats.json"
            )
        {
            continue;
        }
        if name.ends_with(".template.py") {
            continue;
        }
        if matches!(path.extension().and_then(|s| s.to_str()), Some("md" | "py")) {
            files.push(name.to_string());
        }
    }
    files.sort();
    Ok(files)
}

fn split_l4_history_blocks(text: &str) -> Vec<(String, String)> {
    let re = Regex::new(r"(?m)^SESSION: (.+)$").unwrap();
    let markers = re.captures_iter(text).collect::<Vec<_>>();
    let mut out = Vec::new();
    for (idx, cap) in markers.iter().enumerate() {
        let Some(m) = cap.get(0) else {
            continue;
        };
        let session = cap
            .get(1)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();
        let body_start = text[m.end()..]
            .find('\n')
            .map(|off| m.end() + off + 1)
            .unwrap_or(m.end());
        let body_end = markers
            .get(idx + 1)
            .and_then(|next| next.get(0))
            .map(|m| m.start())
            .unwrap_or(text.len());
        let body = text[body_start..body_end]
            .trim_matches('=')
            .trim()
            .to_string();
        if !session.is_empty() && !body.is_empty() {
            out.push((session, body));
        }
    }
    out
}

fn excerpt_for_terms(text: &str, terms: &[String], max_chars: usize) -> String {
    let lower = text.to_ascii_lowercase();
    let idx = terms
        .iter()
        .filter_map(|term| lower.find(term))
        .min()
        .unwrap_or(0);
    let start = text[..idx]
        .char_indices()
        .rev()
        .nth(80)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let excerpt = text[start..].chars().take(max_chars).collect::<String>();
    if start > 0 {
        format!("...{}", excerpt.trim())
    } else {
        excerpt.trim().to_string()
    }
}

fn clean_l4_excerpt(text: &str) -> String {
    let unescaped = text
        .replace("\\n", " ")
        .replace("\\r", " ")
        .replace("\\t", " ")
        .replace("\\\"", "\"");
    let mut cleaned = String::new();
    let mut last_space = false;
    for c in unescaped.chars() {
        if c.is_control() {
            if !last_space {
                cleaned.push(' ');
                last_space = true;
            }
            continue;
        }
        if c.is_whitespace() {
            if !last_space {
                cleaned.push(' ');
                last_space = true;
            }
        } else {
            cleaned.push(c);
            last_space = false;
        }
    }
    cleaned
        .replace("{\"type\":\"", "{type:")
        .replace("\",\"payload\":", ", payload:")
        .trim()
        .chars()
        .take(500)
        .collect()
}

fn looks_like_full_memory_dump(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    content
        .lines()
        .filter(|line| line.trim_start().starts_with("## "))
        .count()
        >= 3
        || lower.contains("# [global memory")
        || lower.contains("global_mem_insight")
        || lower.contains("api_key")
}

fn looks_like_unsafe_l3_update(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    let headings = content
        .lines()
        .filter(|line| line.trim_start().starts_with('#'))
        .count();
    content.lines().count() > 80
        || headings > 2
        || lower.contains("```")
        || lower.contains("replace the entire")
        || lower.contains("overwrite")
        || lower.contains("delete existing")
        || lower.contains("full rewrite")
        || lower.contains("全量覆盖")
        || lower.contains("覆盖整个")
        || lower.contains("删除原有")
        || lower.contains("maybe")
        || lower.contains("probably")
        || lower.contains("unverified")
        || lower.contains("猜测")
        || lower.contains("可能")
        || lower.contains("疑似")
        || lower.contains("未验证")
}

fn append_pending_updates(cfg: &AgentConfig, pending: &[Value]) -> Result<()> {
    let path = cfg.memory_dir.join("pending_long_term_updates.md");
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "\n## {}\n", Local::now().format("%Y-%m-%d %H:%M:%S"))?;
    for item in pending {
        writeln!(f, "```json\n{}\n```\n", serde_json::to_string_pretty(item)?)?;
    }
    Ok(())
}

fn sanitize_l3_file_name(raw: &str) -> String {
    let name = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("long_term_update.md")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let trimmed = name.trim_matches('_');
    let base = if trimmed.is_empty() {
        "long_term_update"
    } else {
        trimmed
    };
    if base.ends_with(".md") {
        base.to_string()
    } else {
        format!("{base}.md")
    }
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    value
        .as_object()
        .and_then(|obj| string_field_obj(obj, keys))
}

fn string_field_obj(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| obj.get(*key).and_then(Value::as_str).map(str::to_string))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn redact_value(value: &Value) -> Value {
    match value {
        Value::Object(obj) => Value::Object(
            obj.iter()
                .map(|(key, value)| {
                    let sensitive_key = contains_secret_text(key);
                    (
                        key.clone(),
                        if sensitive_key {
                            Value::String("<redacted>".into())
                        } else {
                            redact_value(value)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        Value::String(s) if contains_secret_text(s) => Value::String(redact_secret(s)),
        other => other.clone(),
    }
}

fn contains_secret_marker(value: &Value) -> bool {
    match value {
        Value::Object(obj) => obj
            .iter()
            .any(|(key, value)| contains_secret_text(key) || contains_secret_marker(value)),
        Value::Array(items) => items.iter().any(contains_secret_marker),
        Value::String(s) => contains_secret_text(s),
        _ => false,
    }
}

fn contains_secret_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("password")
        || lower.contains("passwd")
        || lower.contains("secret")
        || lower.contains("token")
        || lower.contains("bearer ")
        || lower.contains("sk-")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretPreview {
    pub name: String,
    pub preview: String,
}

#[derive(Debug, Clone)]
pub struct Keychain {
    path: PathBuf,
    mask: [u8; 32],
    data: BTreeMap<String, String>,
}

impl Keychain {
    pub fn load_default() -> Result<Self> {
        Self::load(home_path("ga_keychain.enc")?)
    }

    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let mask = key_mask();
        let data = if path.exists() {
            let bytes = xor(&fs::read(&path)?, &mask);
            serde_json::from_slice(&bytes).with_context(|| format!("decode {}", path.display()))?
        } else {
            BTreeMap::new()
        };
        Ok(Self { path, mask, data })
    }

    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) -> Result<()> {
        self.data.insert(name.into(), value.into());
        self.save()
    }

    pub fn set_from_file(&mut self, name: impl Into<String>, path: impl AsRef<Path>) -> Result<()> {
        let value = fs::read_to_string(path.as_ref())?.trim().to_string();
        self.set(name, value)
    }

    pub fn get(&self, name: &str) -> Result<String> {
        self.data
            .get(name)
            .cloned()
            .with_context(|| format!("No secret: {name}"))
    }

    pub fn list(&self) -> Vec<SecretPreview> {
        self.data
            .iter()
            .map(|(name, val)| SecretPreview {
                name: name.clone(),
                preview: redact_secret(val),
            })
            .collect()
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec(&self.data)?;
        fs::write(&self.path, xor(&bytes, &self.mask))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AndroidNode {
    pub text: String,
    pub click: bool,
    pub edit: bool,
    pub cx: i32,
    pub cy: i32,
    pub cls: String,
    pub rid: String,
}

pub fn parse_android_ui_xml(
    xml: &str,
    keyword: Option<&str>,
    clickable_only: bool,
    raw: bool,
) -> Vec<AndroidNode> {
    let keyword = keyword.map(str::to_lowercase);
    xml.split("<node")
        .skip(1)
        .filter_map(|chunk| {
            let attrs = chunk.split_once('>').map(|(a, _)| a).unwrap_or(chunk);
            let package = attr(attrs, "package");
            if package.to_ascii_lowercase().contains("termux") {
                return None;
            }
            let text = non_empty(attr(attrs, "text"))
                .or_else(|| non_empty(attr(attrs, "content-desc")))
                .unwrap_or_default();
            let click = attr(attrs, "clickable") == "true";
            let cls = attr(attrs, "class")
                .rsplit('.')
                .next()
                .unwrap_or_default()
                .to_string();
            let rid = attr(attrs, "resource-id").to_string();
            if !raw && text.is_empty() && !click {
                return None;
            }
            if clickable_only && !click {
                return None;
            }
            if let Some(k) = &keyword
                && !text.to_lowercase().contains(k)
            {
                return None;
            }
            let (cx, cy) = center_from_bounds(attr(attrs, "bounds"));
            Some(AndroidNode {
                text,
                click,
                edit: cls == "EditText",
                cx,
                cy,
                cls,
                rid,
            })
        })
        .collect()
}

pub fn adb_tap(x: i32, y: i32) -> Result<()> {
    let status = Command::new(adb_bin())
        .args(["shell", "input", "tap", &x.to_string(), &y.to_string()])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        bail!("adb tap failed with {status}")
    }
}

pub fn adb_dump_ui() -> Result<String> {
    adb_dump_ui_via_uiautomator2()
        .or_else(|u2_err| adb_dump_ui_native().with_context(|| format!("u2 fallback: {u2_err:#}")))
}

fn adb_dump_ui_via_uiautomator2() -> Result<String> {
    let script = adb_uiautomator2_dump_script();
    let Some(python) = resolve_python(&python_root_hint(), PythonPurpose::AgentHelper) else {
        return Err(anyhow::anyhow!(python_unavailable_message()));
    };
    let out = python_command(&python.command)
        .arg("-c")
        .arg(script)
        .output();
    match out {
        Ok(out) if out.status.success() && out.stdout.len() > 100 => {
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        }
        Ok(out) => Err(python_output_error(&python.command, &out)),
        Err(err) => Err(err.into()),
    }
}

fn adb_dump_ui_native() -> Result<String> {
    let adb = adb_bin();
    let _ = Command::new(&adb)
        .args(["shell", "rm", "-f", "/sdcard/ui.xml"])
        .output();
    let out = Command::new(&adb)
        .args([
            "shell",
            "uiautomator",
            "dump",
            "--compressed",
            "/sdcard/ui.xml",
        ])
        .output()?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !combined.to_ascii_lowercase().contains("dumped") {
        bail!("dump failed: {combined}");
    }
    let pull = Command::new(&adb)
        .args(["exec-out", "cat", "/sdcard/ui.xml"])
        .output()?;
    if !pull.status.success() {
        bail!("adb exec-out cat failed")
    }
    Ok(String::from_utf8_lossy(&pull.stdout).to_string())
}

fn adb_uiautomator2_dump_script() -> &'static str {
    r#"import sys
try:
    import uiautomator2 as u2
    d = u2.connect()
    xml = d.dump_hierarchy()
    if not xml or len(xml) <= 100:
        raise RuntimeError("uiautomator2 returned empty hierarchy")
    sys.stdout.write(xml)
except Exception as e:
    sys.stderr.write(str(e))
    sys.exit(1)
"#
}

pub fn format_android_nodes(nodes: &[AndroidNode]) -> String {
    let mut out = String::new();
    for node in nodes {
        let flag = if node.edit {
            "E"
        } else if node.click {
            "Y"
        } else {
            " "
        };
        let coord = if node.cx != 0 || node.cy != 0 {
            format!("  ({},{})", node.cx, node.cy)
        } else {
            String::new()
        };
        let display = if node.text.trim().is_empty() {
            node.rid
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .or_else(|| (!node.cls.is_empty()).then_some(node.cls.as_str()))
                .map(|s| format!("<{s}>"))
                .unwrap_or_else(|| "<icon>".into())
        } else {
            node.text.clone()
        };
        out.push_str(&format!("[{flag}] {display}{coord}\n"));
    }
    out.push_str(&format!("\ntotal: {} nodes", nodes.len()));
    out
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct OcrDetail {
    pub bbox: Value,
    pub text: String,
    pub conf: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct OcrResult {
    pub text: String,
    pub lines: Vec<String>,
    #[serde(default)]
    pub details: Vec<OcrDetail>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScreenBbox {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

impl ScreenBbox {
    pub fn as_tuple(self) -> (i32, i32, i32, i32) {
        (self.x1, self.y1, self.x2, self.y2)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OcrCaptureOptions {
    pub lang: Option<String>,
    pub enhance: bool,
    pub engine: Option<String>,
}

impl Default for OcrCaptureOptions {
    fn default() -> Self {
        Self {
            lang: Some("zh-Hans-CN".into()),
            enhance: false,
            engine: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum VisionCapture {
    ImageFile {
        path: PathBuf,
    },
    Window {
        title: Option<String>,
        hwnd: Option<u64>,
    },
    Region {
        bbox: ScreenBbox,
    },
    Fullscreen {
        allow_fullscreen: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VisionPreparedImage {
    pub mime: String,
    pub base64: String,
    pub bytes: usize,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VisionConfig {
    pub backend: String,
    pub base_url: String,
    pub api_key: String,
    pub api_key_header: Option<String>,
    pub model: String,
    pub timeout_secs: u64,
    pub max_pixels: u32,
    pub max_tokens: Option<u64>,
    pub token_param: Option<String>,
    pub system_prompt: Option<String>,
    pub proxy: Option<String>,
    pub verify_tls: bool,
}

impl VisionConfig {
    pub fn from_env(root_dir: impl AsRef<Path>) -> Result<Self> {
        let root_dir = root_dir.as_ref();
        let _ = dotenvy::from_path(root_dir.join(".env"));
        let agent_cfg = AgentConfig::from_env(root_dir).ok();
        let backend = env::var("VISION_BACKEND")
            .ok()
            .or_else(|| env::var("KODA_VISION_BACKEND").ok())
            .unwrap_or_else(|| {
                let style = agent_cfg
                    .as_ref()
                    .map(|cfg| cfg.llm_api_style.as_str())
                    .unwrap_or("openai");
                if style.eq_ignore_ascii_case("claude") || style.eq_ignore_ascii_case("messages") {
                    "claude"
                } else {
                    "openai"
                }
                .into()
            });
        let base_url = env::var("VISION_BASE_URL")
            .ok()
            .or_else(|| env::var("KODA_VISION_BASE_URL").ok())
            .or_else(|| agent_cfg.as_ref().map(|cfg| cfg.openai_base_url.clone()))
            .context("VISION_BASE_URL missing")?;
        let api_key = env::var("VISION_API_KEY")
            .ok()
            .or_else(|| env::var("KODA_VISION_API_KEY").ok())
            .or_else(|| agent_cfg.as_ref().map(|cfg| cfg.openai_api_key.clone()))
            .context("VISION_API_KEY missing")?;
        let api_key_header = env::var("VISION_API_KEY_HEADER")
            .or_else(|_| env::var("KODA_VISION_API_KEY_HEADER"))
            .ok()
            .or_else(|| infer_vision_api_key_header(&base_url));
        let model = env::var("VISION_MODEL")
            .ok()
            .or_else(|| env::var("KODA_VISION_MODEL").ok())
            .or_else(|| agent_cfg.as_ref().map(|cfg| cfg.openai_model.clone()))
            .context("VISION_MODEL missing")?;
        let timeout_secs = env::var("VISION_TIMEOUT_SECS")
            .or_else(|_| env::var("KODA_VISION_TIMEOUT_SECS"))
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| agent_cfg.as_ref().map(|cfg| cfg.timeout_secs.min(600)))
            .unwrap_or(60);
        let max_pixels = env::var("VISION_MAX_PIXELS")
            .or_else(|_| env::var("KODA_VISION_MAX_PIXELS"))
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_VISION_MAX_PIXELS);
        let max_tokens = env::var("VISION_MAX_TOKENS")
            .or_else(|_| env::var("KODA_VISION_MAX_TOKENS"))
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| base_url.contains("xiaomimimo.com").then_some(1024));
        let token_param = env::var("VISION_TOKEN_PARAM")
            .or_else(|_| env::var("KODA_VISION_TOKEN_PARAM"))
            .ok()
            .or_else(|| infer_vision_token_param(&base_url));
        let system_prompt = env::var("VISION_SYSTEM_PROMPT")
            .or_else(|_| env::var("KODA_VISION_SYSTEM_PROMPT"))
            .ok()
            .filter(|s| !s.trim().is_empty());
        let proxy = env::var("VISION_PROXY")
            .or_else(|_| env::var("KODA_VISION_PROXY"))
            .ok()
            .or_else(|| agent_cfg.as_ref().and_then(|cfg| cfg.proxy.clone()));
        let verify_tls = env::var("VISION_VERIFY_TLS")
            .or_else(|_| env::var("KODA_VISION_VERIFY_TLS"))
            .ok()
            .and_then(|v| parse_vision_bool(&v))
            .or_else(|| agent_cfg.as_ref().map(|cfg| cfg.verify_tls))
            .unwrap_or(true);
        Ok(Self {
            backend,
            base_url,
            api_key,
            api_key_header,
            model,
            timeout_secs,
            max_pixels,
            max_tokens,
            token_param,
            system_prompt,
            proxy,
            verify_tls,
        })
    }

    pub fn from_agent(cfg: &AgentConfig) -> Self {
        let backend = if cfg.llm_api_style.eq_ignore_ascii_case("claude")
            || cfg.llm_api_style.eq_ignore_ascii_case("messages")
        {
            "claude"
        } else {
            "openai"
        };
        Self {
            backend: backend.into(),
            base_url: cfg.openai_base_url.clone(),
            api_key: cfg.openai_api_key.clone(),
            api_key_header: infer_vision_api_key_header(&cfg.openai_base_url),
            model: cfg.openai_model.clone(),
            timeout_secs: cfg.timeout_secs.min(600),
            max_pixels: DEFAULT_VISION_MAX_PIXELS,
            max_tokens: cfg.max_tokens,
            token_param: infer_vision_token_param(&cfg.openai_base_url),
            system_prompt: None,
            proxy: cfg.proxy.clone(),
            verify_tls: cfg.verify_tls,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VisionRequest {
    pub image_path: PathBuf,
    pub prompt: String,
    pub timeout_secs: Option<u64>,
    pub max_pixels: Option<u32>,
}

impl VisionRequest {
    pub fn new(image_path: impl Into<PathBuf>, prompt: impl Into<String>) -> Self {
        Self {
            image_path: image_path.into(),
            prompt: prompt.into(),
            timeout_secs: None,
            max_pixels: None,
        }
    }
}

pub async fn ask_vision(cfg: &VisionConfig, request: &VisionRequest) -> Result<String> {
    let prepared = prepare_vision_image(
        &request.image_path,
        request.max_pixels.unwrap_or(cfg.max_pixels),
    )
    .with_context(|| "Error: 图片处理失败")?;
    ask_vision_prepared(cfg, request, &prepared).await
}

pub async fn ask_vision_from_env(
    root_dir: impl AsRef<Path>,
    image_path: impl Into<PathBuf>,
    prompt: Option<&str>,
) -> Result<String> {
    let cfg = VisionConfig::from_env(root_dir)?;
    let request = VisionRequest::new(image_path, prompt.unwrap_or(DEFAULT_VISION_PROMPT));
    ask_vision(&cfg, &request).await
}

pub async fn ask_vision_prepared(
    cfg: &VisionConfig,
    request: &VisionRequest,
    image: &VisionPreparedImage,
) -> Result<String> {
    let timeout = Duration::from_secs(request.timeout_secs.unwrap_or(cfg.timeout_secs).max(1));
    let mut builder = reqwest::Client::builder().timeout(timeout);
    if !cfg.verify_tls {
        builder = builder.danger_accept_invalid_certs(true);
    }
    if let Some(proxy) = cfg.proxy.as_deref().filter(|s| !s.trim().is_empty()) {
        builder = builder.proxy(reqwest::Proxy::all(proxy).context("invalid vision proxy")?);
    }
    let http = builder.build()?;
    let backend = cfg.backend.trim().to_ascii_lowercase();
    if backend == "claude" || backend == "messages" {
        ask_vision_claude(&http, cfg, request, image).await
    } else if backend == "responses" {
        ask_vision_responses(&http, cfg, request, image).await
    } else if backend == "openai" || backend == "chat" || backend == "modelscope" {
        ask_vision_openai_compat(&http, cfg, request, image).await
    } else {
        bail!(
            "Error: 未知backend '{}'，可选: claude, openai, responses, modelscope",
            cfg.backend
        )
    }
}

async fn ask_vision_openai_compat(
    http: &reqwest::Client,
    cfg: &VisionConfig,
    request: &VisionRequest,
    image: &VisionPreparedImage,
) -> Result<String> {
    let url = auto_make_url(&cfg.base_url, "chat/completions");
    let payload = build_openai_vision_payload(cfg, request, image);
    let res = apply_vision_auth(http.post(url), cfg)
        .json(&payload)
        .send()
        .await
        .context("send OpenAI-compatible vision request")?;
    let status = res.status();
    let body = vision_response_json(res, status).await?;
    if !status.is_success() {
        bail!("Vision error {status}: {body}");
    }
    parse_openai_vision_response(&body)
}

async fn ask_vision_claude(
    http: &reqwest::Client,
    cfg: &VisionConfig,
    request: &VisionRequest,
    image: &VisionPreparedImage,
) -> Result<String> {
    let url = auto_make_url(&cfg.base_url, "messages");
    let payload = build_claude_vision_payload(cfg, request, image);
    let res = http
        .post(url)
        .header("x-api-key", &cfg.api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&payload)
        .send()
        .await
        .context("send Claude vision request")?;
    let status = res.status();
    let body = vision_response_json(res, status).await?;
    if !status.is_success() {
        bail!("Vision error {status}: {body}");
    }
    parse_claude_vision_response(&body)
}

async fn ask_vision_responses(
    http: &reqwest::Client,
    cfg: &VisionConfig,
    request: &VisionRequest,
    image: &VisionPreparedImage,
) -> Result<String> {
    let url = auto_make_url(&cfg.base_url, "responses");
    let payload = build_responses_vision_payload(cfg, request, image);
    let res = apply_vision_auth(http.post(url), cfg)
        .json(&payload)
        .send()
        .await
        .context("send OpenAI Responses vision request")?;
    let status = res.status();
    let body = vision_response_json(res, status).await?;
    if !status.is_success() {
        bail!("Vision error {status}: {body}");
    }
    parse_responses_vision_response(&body)
}

pub fn build_openai_vision_payload(
    cfg: &VisionConfig,
    request: &VisionRequest,
    image: &VisionPreparedImage,
) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = cfg
        .system_prompt
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        messages.push(json!({"role": "system", "content": system}));
    }
    messages.push(json!({
        "role": "user",
        "content": [
            {
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{};base64,{}", image.mime, image.base64)
                }
            },
            {"type": "text", "text": request.prompt}
        ]
    }));
    let mut payload = json!({
        "model": cfg.model,
        "messages": messages
    });
    apply_vision_token_option(&mut payload, cfg, false);
    payload
}

pub fn build_claude_vision_payload(
    cfg: &VisionConfig,
    request: &VisionRequest,
    image: &VisionPreparedImage,
) -> Value {
    let mut payload = json!({
        "model": cfg.model,
        "max_tokens": cfg.max_tokens.unwrap_or(1024),
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": image.mime,
                        "data": image.base64
                    }
                },
                {"type": "text", "text": request.prompt}
            ]
        }]
    });
    if let Some(system) = cfg
        .system_prompt
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        && let Some(obj) = payload.as_object_mut()
    {
        obj.insert("system".into(), json!(system));
    }
    payload
}

pub fn build_responses_vision_payload(
    cfg: &VisionConfig,
    request: &VisionRequest,
    image: &VisionPreparedImage,
) -> Value {
    let mut payload = json!({
        "model": cfg.model,
        "input": [{
            "role": "user",
            "content": [
                {"type": "input_text", "text": request.prompt},
                {
                    "type": "input_image",
                    "image_url": format!("data:{};base64,{}", image.mime, image.base64)
                }
            ]
        }]
    });
    if let Some(system) = cfg
        .system_prompt
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        payload["instructions"] = json!(system);
    }
    apply_vision_token_option(&mut payload, cfg, true);
    payload
}

pub fn parse_openai_vision_response(body: &Value) -> Result<String> {
    body.pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .context("Error: 响应解析失败 - missing choices[0].message.content")
}

pub fn parse_claude_vision_response(body: &Value) -> Result<String> {
    let text = body
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        bail!("Error: 响应解析失败 - missing Claude text content")
    }
    Ok(text)
}

pub fn parse_responses_vision_response(body: &Value) -> Result<String> {
    if let Some(text) = body.get("output_text").and_then(Value::as_str)
        && !text.trim().is_empty()
    {
        return Ok(text.to_string());
    }
    let mut parts = Vec::new();
    if let Some(output) = body.get("output").and_then(Value::as_array) {
        for item in output {
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for block in content {
                    if matches!(
                        block.get("type").and_then(Value::as_str),
                        Some("output_text" | "text")
                    ) && let Some(text) = block.get("text").and_then(Value::as_str)
                    {
                        parts.push(text);
                    }
                }
            }
        }
    }
    let text = parts.join("\n");
    if text.trim().is_empty() {
        bail!("Error: 响应解析失败 - missing Responses output_text")
    }
    Ok(text)
}

async fn vision_response_json(
    res: reqwest::Response,
    status: reqwest::StatusCode,
) -> Result<Value> {
    let text = res
        .text()
        .await
        .with_context(|| format!("read Vision response body status {status}"))?;
    serde_json::from_str(&text).with_context(|| {
        format!(
            "parse Vision response status {status}: {}",
            truncate_for_error(&text, 500)
        )
    })
}

fn apply_vision_auth(req: reqwest::RequestBuilder, cfg: &VisionConfig) -> reqwest::RequestBuilder {
    match cfg
        .api_key_header
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(header) if header.eq_ignore_ascii_case("authorization") => {
            req.bearer_auth(&cfg.api_key)
        }
        Some(header) => req.header(header, &cfg.api_key),
        None => req.bearer_auth(&cfg.api_key),
    }
}

fn apply_vision_token_option(payload: &mut Value, cfg: &VisionConfig, responses: bool) {
    let Some(tokens) = cfg.max_tokens else {
        return;
    };
    let key = cfg
        .token_param
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(if responses {
            "max_output_tokens"
        } else {
            "max_tokens"
        });
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(key.into(), json!(tokens));
    }
}

fn truncate_for_error(text: &str, max_chars: usize) -> String {
    let mut out = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

/// Low-dependency OCR wrapper.
///
/// Uses `tesseract` when available, matching GenericAgent's "install/use local
/// tool on demand" style without forcing OCR libraries into the Rust build.
pub fn ocr_image_via_tesseract(
    image_path: impl AsRef<Path>,
    lang: Option<&str>,
) -> Result<OcrResult> {
    let image_path = image_path.as_ref();
    let mut cmd = Command::new("tesseract");
    cmd.arg(image_path).arg("stdout");
    if let Some(lang) = lang {
        cmd.arg("-l").arg(lang);
    }
    let out = cmd
        .output()
        .with_context(|| "run tesseract; install it or use memory/ocr_utils.py via code_run")?;
    if !out.status.success() {
        bail!("tesseract failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let lines = text.lines().map(strip_cjk_spaces).collect();
    let text = strip_cjk_spaces(&text);
    Ok(OcrResult {
        text,
        lines,
        details: Vec::new(),
    })
}

/// OCR with upstream-like preference: RapidOCR via Python when available, then
/// local tesseract as a low-dependency fallback.
pub fn ocr_image(image_path: impl AsRef<Path>, lang: Option<&str>) -> Result<OcrResult> {
    let path = image_path.as_ref();
    ocr_image_via_rapidocr(path).or_else(|rapid_err| {
        ocr_image_via_tesseract(path, lang)
            .with_context(|| format!("rapidocr fallback: {rapid_err:#}"))
    })
}

fn ocr_image_via_rapidocr(image_path: &Path) -> Result<OcrResult> {
    let path = image_path
        .to_str()
        .context("image path is not valid UTF-8 for rapidocr python bridge")?;
    let script = rapidocr_script();
    let Some(python) = resolve_python(&python_root_hint(), PythonPurpose::AgentHelper) else {
        return Err(anyhow::anyhow!(python_unavailable_message()));
    };
    let out = python_command(&python.command)
        .arg("-c")
        .arg(script)
        .arg(path)
        .output();
    match out {
        Ok(out) if out.status.success() => {
            let result: OcrResult = serde_json::from_slice(&out.stdout).with_context(|| {
                format!(
                    "parse rapidocr JSON: {}",
                    String::from_utf8_lossy(&out.stdout)
                )
            })?;
            Ok(normalize_ocr_result(result))
        }
        Ok(out) => Err(python_output_error(&python.command, &out)),
        Err(err) => Err(err.into()),
    }
}

fn rapidocr_script() -> &'static str {
    r#"import json, re, sys
from PIL import Image, ImageEnhance
from rapidocr_onnxruntime import RapidOCR

def _strip_cjk_spaces(t):
    return re.sub(r'(?<=[\u4e00-\u9fff])\s+(?=[\u4e00-\u9fff])', '', t or '')

def _preprocess(img, scale=3, contrast=3.0):
    img = ImageEnhance.Contrast(img).enhance(contrast)
    return img.resize((img.width * scale, img.height * scale))

engine = RapidOCR()
img = Image.open(sys.argv[1])
if len(sys.argv) > 2 and sys.argv[2] == "enhance":
    img = _preprocess(img)
import numpy as np
result, _ = engine(np.array(img))
lines = []
details = []
for item in result or []:
    try:
        text = item[1]
        if text:
            line = _strip_cjk_spaces(str(text))
            lines.append(line)
            details.append({"bbox": item[0], "text": line, "conf": float(item[2])})
    except Exception:
        pass
sys.stdout.write(json.dumps({
    "text": _strip_cjk_spaces("\n".join(lines)),
    "lines": lines,
    "details": details
}, ensure_ascii=False))
"#
}

pub fn ocr_screen_via_python(
    bbox: Option<ScreenBbox>,
    allow_fullscreen: bool,
    options: &OcrCaptureOptions,
) -> Result<OcrResult> {
    validate_screen_capture(bbox, allow_fullscreen)?;
    let payload = json!({
        "bbox": bbox.map(|b| [b.x1, b.y1, b.x2, b.y2]),
        "lang": options.lang,
        "enhance": options.enhance,
        "engine": options.engine,
    });
    run_ocr_python_json(&ocr_screen_script(), &payload)
}

pub fn ocr_window_via_python(hwnd: u64, options: &OcrCaptureOptions) -> Result<OcrResult> {
    if hwnd == 0 {
        bail!("ocr_window requires a non-zero hwnd");
    }
    let payload = json!({
        "hwnd": hwnd,
        "lang": options.lang,
        "enhance": options.enhance,
        "engine": options.engine,
    });
    run_ocr_python_json(&ocr_window_script(), &payload)
}

pub fn validate_screen_capture(bbox: Option<ScreenBbox>, allow_fullscreen: bool) -> Result<()> {
    if let Some(bbox) = bbox {
        if bbox.x2 <= bbox.x1 || bbox.y2 <= bbox.y1 {
            bail!("invalid bbox: expected x2>x1 and y2>y1");
        }
        return Ok(());
    }
    if allow_fullscreen {
        Ok(())
    } else {
        bail!("Vision SOP forbids fullscreen screenshots; pass a window hwnd/title or bounded bbox")
    }
}

pub fn validate_vision_capture(capture: &VisionCapture) -> Result<()> {
    match capture {
        VisionCapture::ImageFile { path } => {
            if path.as_os_str().is_empty() {
                bail!("vision image path is empty");
            }
            Ok(())
        }
        VisionCapture::Window { title, hwnd } => {
            if hwnd.is_some_and(|h| h != 0)
                || title.as_deref().is_some_and(|t| !t.trim().is_empty())
            {
                Ok(())
            } else {
                bail!("vision window capture requires a window title or hwnd")
            }
        }
        VisionCapture::Region { bbox } => validate_screen_capture(Some(*bbox), false),
        VisionCapture::Fullscreen { allow_fullscreen } => {
            validate_screen_capture(None, *allow_fullscreen)
        }
    }
}

pub fn prepare_vision_image(
    image_path: impl AsRef<Path>,
    max_pixels: u32,
) -> Result<VisionPreparedImage> {
    let path = image_path.as_ref();
    prepare_vision_image_via_python(path, max_pixels)
        .or_else(|python_err| prepare_vision_image_native(path).with_context(|| python_err))
}

fn prepare_vision_image_native(path: &Path) -> Result<VisionPreparedImage> {
    let bytes = fs::read(path)?;
    let mime = match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "image/png",
    }
    .to_string();
    Ok(VisionPreparedImage {
        mime,
        base64: BASE64.encode(&bytes),
        bytes: bytes.len(),
        width: None,
        height: None,
    })
}

fn prepare_vision_image_via_python(path: &Path, max_pixels: u32) -> Result<VisionPreparedImage> {
    let payload = json!({
        "path": path.to_str().context("vision image path is not valid UTF-8")?,
        "max_pixels": max_pixels.max(1),
    });
    let Some(python) = resolve_python(&python_root_hint(), PythonPurpose::AgentHelper) else {
        return Err(anyhow::anyhow!(python_unavailable_message()));
    };
    let out = python_command(&python.command)
        .arg("-c")
        .arg(vision_prepare_image_script())
        .arg(payload.to_string())
        .output();
    match out {
        Ok(out) if out.status.success() => serde_json::from_slice(&out.stdout).with_context(|| {
            format!(
                "parse vision image JSON: {}",
                String::from_utf8_lossy(&out.stdout)
            )
        }),
        Ok(out) => Err(python_output_error(&python.command, &out)),
        Err(err) => Err(err.into()),
    }
}

fn run_ocr_python_json(script: &str, payload: &Value) -> Result<OcrResult> {
    let Some(python) = resolve_python(&python_root_hint(), PythonPurpose::AgentHelper) else {
        return Err(anyhow::anyhow!(python_unavailable_message()));
    };
    let out = python_command(&python.command)
        .arg("-c")
        .arg(script)
        .arg(payload.to_string())
        .output();
    match out {
        Ok(out) if out.status.success() => {
            let result: OcrResult = serde_json::from_slice(&out.stdout).with_context(|| {
                format!("parse OCR JSON: {}", String::from_utf8_lossy(&out.stdout))
            })?;
            Ok(normalize_ocr_result(result))
        }
        Ok(out) => Err(python_output_error(&python.command, &out)),
        Err(err) => Err(err.into()),
    }
}

fn ocr_common_python() -> &'static str {
    r#"import json, re, sys
from PIL import Image, ImageEnhance

def _strip_cjk_spaces(t):
    return re.sub(r'(?<=[\u4e00-\u9fff])\s+(?=[\u4e00-\u9fff])', '', t or '')

def _preprocess(img, scale=3, contrast=3.0):
    img = ImageEnhance.Contrast(img).enhance(contrast)
    return img.resize((img.width * scale, img.height * scale))

def _ocr_image(img, enhance=False, engine=None):
    if enhance:
        img = _preprocess(img)
    if engine not in (None, 'rapid'):
        raise ValueError("Only rapid OCR is supported")
    import numpy as np
    from rapidocr_onnxruntime import RapidOCR
    result, _ = RapidOCR()(np.array(img))
    lines, details = [], []
    for item in result or []:
        text = _strip_cjk_spaces(str(item[1] or ''))
        if not text:
            continue
        lines.append(text)
        details.append({'bbox': item[0], 'text': text, 'conf': float(item[2])})
    return {'text': _strip_cjk_spaces('\n'.join(lines)), 'lines': lines, 'details': details}
"#
}

fn ocr_screen_script() -> String {
    format!(
        r#"{}
from PIL import ImageGrab
opts = json.loads(sys.argv[1])
bbox = opts.get('bbox')
img = ImageGrab.grab(bbox=tuple(bbox) if bbox else None)
sys.stdout.write(json.dumps(_ocr_image(img, opts.get('enhance', False), opts.get('engine')), ensure_ascii=False))
"#,
        ocr_common_python()
    )
}

fn ocr_window_script() -> String {
    format!(
        r#"{}
import platform
if platform.system() != 'Windows':
    raise RuntimeError('ocr_window requires Windows PrintWindow APIs')
import win32gui, win32ui
from ctypes import windll
opts = json.loads(sys.argv[1])
hwnd = int(opts['hwnd'])
l, t, r, b = win32gui.GetWindowRect(hwnd)
w, h = r - l, b - t
hwndDC = win32gui.GetWindowDC(hwnd)
mfcDC = win32ui.CreateDCFromHandle(hwndDC)
saveDC = mfcDC.CreateCompatibleDC()
saveBitMap = win32ui.CreateBitmap()
saveBitMap.CreateCompatibleBitmap(mfcDC, w, h)
saveDC.SelectObject(saveBitMap)
windll.user32.PrintWindow(hwnd, saveDC.GetSafeHdc(), 3)
bmpinfo = saveBitMap.GetInfo()
bmpstr = saveBitMap.GetBitmapBits(True)
img = Image.frombuffer('RGB', (bmpinfo['bmWidth'], bmpinfo['bmHeight']), bmpstr, 'raw', 'BGRX', 0, 1)
win32gui.DeleteObject(saveBitMap.GetHandle())
saveDC.DeleteDC()
mfcDC.DeleteDC()
win32gui.ReleaseDC(hwnd, hwndDC)
sys.stdout.write(json.dumps(_ocr_image(img, opts.get('enhance', False), opts.get('engine')), ensure_ascii=False))
"#,
        ocr_common_python()
    )
}

fn vision_prepare_image_script() -> &'static str {
    r#"import base64, json, sys
from io import BytesIO
from PIL import Image
opts = json.loads(sys.argv[1])
img = Image.open(opts['path'])
w, h = img.size
max_pixels = int(opts.get('max_pixels') or 1440000)
if w * h > max_pixels:
    scale = (max_pixels / (w * h)) ** 0.5
    img = img.resize((max(1, int(w * scale)), max(1, int(h * scale))), Image.Resampling.LANCZOS)
if img.mode in ('RGBA', 'LA', 'P'):
    rgb = Image.new('RGB', img.size, (255, 255, 255))
    rgb.paste(img, mask=img.split()[-1] if img.mode == 'RGBA' else None)
    img = rgb
buf = BytesIO()
img.save(buf, format='JPEG', quality=80, optimize=True)
data = buf.getvalue()
sys.stdout.write(json.dumps({
    'mime': 'image/jpeg',
    'base64': base64.b64encode(data).decode('utf-8'),
    'bytes': len(data),
    'width': img.size[0],
    'height': img.size[1],
}))
"#
}

fn normalize_ocr_result(mut result: OcrResult) -> OcrResult {
    result.text = strip_cjk_spaces(&result.text);
    result.lines = result
        .lines
        .into_iter()
        .map(|line| strip_cjk_spaces(&line))
        .collect();
    for detail in &mut result.details {
        detail.text = strip_cjk_spaces(&detail.text);
    }
    result
}

fn strip_cjk_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars = text.chars().collect::<Vec<_>>();
    for (idx, ch) in chars.iter().copied().enumerate() {
        if ch.is_whitespace()
            && idx > 0
            && idx + 1 < chars.len()
            && is_cjk(chars[idx - 1])
            && is_cjk(chars[idx + 1])
        {
            continue;
        }
        out.push(ch);
    }
    out
}

fn is_cjk(ch: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&ch)
}

fn key_mask() -> [u8; 32] {
    let user = env::var("USER")
        .or_else(|_| env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into());
    Sha256::digest(format!("{user}@ga_keychain").as_bytes()).into()
}

fn xor(data: &[u8], mask: &[u8; 32]) -> Vec<u8> {
    data.iter()
        .enumerate()
        .map(|(i, b)| b ^ mask[i % mask.len()])
        .collect()
}
fn home_path(name: &str) -> Result<PathBuf> {
    Ok(dirs::home_dir().context("home dir not found")?.join(name))
}
fn adb_bin() -> String {
    env::var("ADB").unwrap_or_else(|_| "adb".into())
}

fn python_root_hint() -> PathBuf {
    env::var("KODA_AGENT_ROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn python_command(command: &PythonCommand) -> Command {
    let mut cmd = Command::new(&command.program);
    cmd.args(&command.args);
    cmd
}

fn python_output_error(command: &PythonCommand, out: &std::process::Output) -> anyhow::Error {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}");
    if let Some(extra) = missing_python_extra_hint(&combined) {
        return anyhow::anyhow!(
            "Python helper dependency missing ({extra}). Run future `koda-agent bootstrap-python --extras {extra}` or install the dependency in KODA_PYTHON. Original error: {}",
            combined.trim()
        );
    }
    anyhow::anyhow!(
        "{} exited {}: {}{}",
        command.display(),
        out.status,
        stdout,
        stderr
    )
}

fn missing_python_extra_hint(output: &str) -> Option<&'static str> {
    let lower = output.to_ascii_lowercase();
    let missing_module = lower.contains("modulenotfounderror")
        || lower.contains("importerror")
        || lower.contains("no module named");
    if !missing_module {
        return None;
    }
    if lower.contains("rapidocr")
        || lower.contains("pillow")
        || lower.contains("pil")
        || lower.contains("numpy")
        || lower.contains("imagegrab")
    {
        return Some("ocr");
    }
    if lower.contains("uiautomator2") || lower.contains("win32gui") || lower.contains("win32ui") {
        return Some("automation");
    }
    Some("core")
}

fn parse_vision_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn infer_vision_api_key_header(base_url: &str) -> Option<String> {
    base_url
        .contains("xiaomimimo.com")
        .then(|| "api-key".to_string())
}

fn infer_vision_token_param(base_url: &str) -> Option<String> {
    base_url
        .contains("xiaomimimo.com")
        .then(|| "max_completion_tokens".to_string())
}
fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn attr<'a>(attrs: &'a str, key: &str) -> &'a str {
    let pat = format!("{key}=\"");
    let Some(start) = attrs.find(&pat).map(|i| i + pat.len()) else {
        return "";
    };
    let rest = &attrs[start..];
    rest.find('"').map(|end| &rest[..end]).unwrap_or(rest)
}

fn center_from_bounds(bounds: &str) -> (i32, i32) {
    let nums = bounds
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<i32>().ok())
        .collect::<Vec<_>>();
    if nums.len() >= 4 {
        ((nums[0] + nums[2]) / 2, (nums[1] + nums[3]) / 2)
    } else {
        (0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use koda_agent_core::{AgentResponse, ToolCall};
    use tempfile::tempdir;

    struct StaticLlm {
        content: String,
    }

    #[async_trait]
    impl LlmClient for StaticLlm {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _tools_schema: &Value,
        ) -> Result<AgentResponse> {
            Ok(AgentResponse {
                thinking: String::new(),
                content: self.content.clone(),
                tool_calls: Vec::<ToolCall>::new(),
                raw: Value::Null,
            })
        }

        fn name(&self) -> String {
            "StaticLlm".into()
        }
    }

    fn cfg(root: &Path) -> AgentConfig {
        AgentConfig {
            home_dir: root.into(),
            workspace_dir: root.into(),
            resource_dir: root.into(),
            root_dir: root.into(),
            temp_dir: root.join("temp"),
            memory_dir: root.join("memory"),
            logs_dir: root.join("logs"),
            sessions_dir: root.join("sessions"),
            browser_dir: root.join("browser"),
            openai_base_url: "http://x".into(),
            openai_api_key: "sk-test".into(),
            openai_model: "m".into(),
            llm_api_style: "chat".into(),
            auth_scheme: None,
            auth_header: None,
            max_turns: 3,
            verbose: false,
            stream: false,
            timeout_secs: 600,
            connect_timeout_secs: 30,
            verify_tls: true,
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            thinking_type: None,
            thinking_budget_tokens: None,
            service_tier: None,
            proxy: None,
            failover: true,
            custom_headers: Default::default(),
            mixin: Default::default(),
            llm_configs: vec![],
        }
    }

    #[test]
    fn keychain_roundtrips_without_plaintext() {
        let d = tempdir().unwrap();
        let path = d.path().join("k.enc");
        let mut kc = Keychain::load(&path).unwrap();
        kc.set("api", "super-secret-value").unwrap();
        assert!(!String::from_utf8_lossy(&fs::read(&path).unwrap()).contains("super-secret-value"));
        let kc2 = Keychain::load(&path).unwrap();
        assert_eq!(kc2.get("api").unwrap(), "super-secret-value");
    }

    #[test]
    fn parses_android_xml_nodes() {
        let xml = r#"<hierarchy><node text="OK" clickable="true" class="android.widget.TextView" resource-id="id/ok" bounds="[10,20][30,60]" package="pkg" /></hierarchy>"#;
        let nodes = parse_android_ui_xml(xml, Some("ok"), true, false);
        assert_eq!(nodes.len(), 1);
        assert_eq!((nodes[0].cx, nodes[0].cy), (20, 40));
    }

    #[test]
    fn android_nodes_format_matches_adb_ui_sop_print_shape() {
        let nodes = vec![
            AndroidNode {
                text: String::new(),
                click: true,
                edit: false,
                cx: 20,
                cy: 40,
                cls: "ImageView".into(),
                rid: "pkg:id/close".into(),
            },
            AndroidNode {
                text: "输入".into(),
                click: false,
                edit: true,
                cx: 0,
                cy: 0,
                cls: "EditText".into(),
                rid: String::new(),
            },
        ];
        let rendered = format_android_nodes(&nodes);
        assert!(rendered.contains("[Y] <close>  (20,40)"));
        assert!(rendered.contains("[E] 输入"));
        assert!(rendered.contains("total: 2 nodes"));
    }

    #[test]
    fn adb_and_ocr_python_fallback_scripts_match_upstream_helpers() {
        let u2 = adb_uiautomator2_dump_script();
        assert!(u2.contains("import uiautomator2 as u2"));
        assert!(u2.contains("dump_hierarchy"));
        let rapid = rapidocr_script();
        assert!(rapid.contains("rapidocr_onnxruntime"));
        assert!(rapid.contains("RapidOCR"));
        assert!(rapid.contains("details"));
        assert!(rapid.contains("bbox"));
        assert!(rapid.contains("conf"));
        assert!(rapid.contains("_strip_cjk_spaces"));
        assert!(!python_root_hint().as_os_str().is_empty());
    }

    #[test]
    fn python_helper_import_errors_map_to_extras() {
        assert_eq!(
            missing_python_extra_hint("ModuleNotFoundError: No module named 'PIL'"),
            Some("ocr")
        );
        assert_eq!(
            missing_python_extra_hint("ImportError: No module named uiautomator2"),
            Some("automation")
        );
        assert_eq!(
            missing_python_extra_hint("ModuleNotFoundError: No module named requests"),
            Some("core")
        );
        assert_eq!(missing_python_extra_hint("syntax error"), None);
    }

    #[test]
    fn vision_ocr_helpers_match_upstream_guardrails() {
        assert!(validate_screen_capture(None, false).is_err());
        assert!(validate_screen_capture(None, true).is_ok());
        assert!(
            validate_screen_capture(
                Some(ScreenBbox {
                    x1: 10,
                    y1: 10,
                    x2: 9,
                    y2: 20
                }),
                false
            )
            .is_err()
        );
        assert!(
            validate_vision_capture(&VisionCapture::Window {
                title: Some("Edge".into()),
                hwnd: None
            })
            .is_ok()
        );
        assert!(
            validate_vision_capture(&VisionCapture::Fullscreen {
                allow_fullscreen: false
            })
            .is_err()
        );

        let screen = ocr_screen_script();
        assert!(screen.contains("ImageGrab.grab"));
        assert!(screen.contains("bbox=tuple(bbox)"));
        let window = ocr_window_script();
        assert!(window.contains("PrintWindow"));
        assert!(window.contains("GetWindowRect"));
        let prep = vision_prepare_image_script();
        assert!(prep.contains("max_pixels"));
        assert!(prep.contains("base64.b64encode"));
    }

    #[test]
    fn vision_api_payloads_match_openai_and_claude_shapes() {
        let cfg = VisionConfig {
            backend: "openai".into(),
            base_url: "https://example.test".into(),
            api_key: "sk-test".into(),
            api_key_header: None,
            model: "vision-model".into(),
            timeout_secs: 60,
            max_pixels: DEFAULT_VISION_MAX_PIXELS,
            max_tokens: None,
            token_param: None,
            system_prompt: None,
            proxy: None,
            verify_tls: true,
        };
        let req = VisionRequest::new("x.png", "read the text");
        let img = VisionPreparedImage {
            mime: "image/jpeg".into(),
            base64: "abc123".into(),
            bytes: 3,
            width: Some(10),
            height: Some(10),
        };

        let openai = build_openai_vision_payload(&cfg, &req, &img);
        assert_eq!(openai["model"], "vision-model");
        assert_eq!(openai["messages"][0]["content"][0]["type"], "image_url");
        assert_eq!(openai["messages"][0]["content"][1]["type"], "text");
        assert_eq!(
            openai["messages"][0]["content"][0]["image_url"]["url"],
            "data:image/jpeg;base64,abc123"
        );

        let claude = build_claude_vision_payload(&cfg, &req, &img);
        assert_eq!(claude["model"], "vision-model");
        assert_eq!(claude["messages"][0]["content"][0]["type"], "image");
        assert_eq!(
            claude["messages"][0]["content"][0]["source"]["media_type"],
            "image/jpeg"
        );
        assert_eq!(claude["messages"][0]["content"][1]["text"], "read the text");

        let responses = build_responses_vision_payload(&cfg, &req, &img);
        assert_eq!(responses["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(responses["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(
            responses["input"][0]["content"][1]["image_url"],
            "data:image/jpeg;base64,abc123"
        );
    }

    #[test]
    fn vision_api_payload_matches_mimo_demo_shape() {
        let mut cfg = VisionConfig {
            backend: "openai".into(),
            base_url: "https://api.xiaomimimo.com/v1/chat/completions".into(),
            api_key: "mimo-test".into(),
            api_key_header: infer_vision_api_key_header("https://api.xiaomimimo.com")
                .or(Some("api-key".into())),
            model: "mimo-v2.5".into(),
            timeout_secs: 60,
            max_pixels: DEFAULT_VISION_MAX_PIXELS,
            max_tokens: Some(1024),
            token_param: infer_vision_token_param("https://api.xiaomimimo.com"),
            system_prompt: Some("You are MiMo, an AI assistant developed by Xiaomi.".into()),
            proxy: None,
            verify_tls: true,
        };
        cfg.token_param = Some("max_completion_tokens".into());
        let req = VisionRequest::new("x.png", "please describe the content of the image");
        let img = VisionPreparedImage {
            mime: "image/png".into(),
            base64: "abc123".into(),
            bytes: 3,
            width: Some(10),
            height: Some(10),
        };
        let payload = build_openai_vision_payload(&cfg, &req, &img);
        assert_eq!(cfg.api_key_header.as_deref(), Some("api-key"));
        assert_eq!(payload["model"], "mimo-v2.5");
        assert_eq!(payload["messages"][0]["role"], "system");
        assert_eq!(payload["messages"][1]["role"], "user");
        assert_eq!(payload["messages"][1]["content"][0]["type"], "image_url");
        assert_eq!(payload["messages"][1]["content"][1]["type"], "text");
        assert_eq!(payload["max_completion_tokens"], 1024);
    }

    #[test]
    fn vision_api_response_parsers_extract_text() {
        let openai = json!({
            "choices": [{"message": {"content": "KODA VISION OK"}}]
        });
        assert_eq!(
            parse_openai_vision_response(&openai).unwrap(),
            "KODA VISION OK"
        );
        let claude = json!({
            "content": [
                {"type":"thinking","thinking":"hidden"},
                {"type":"text","text":"line1"},
                {"type":"text","text":"line2"}
            ]
        });
        assert_eq!(
            parse_claude_vision_response(&claude).unwrap(),
            "line1\nline2"
        );
        let responses = json!({
            "output": [{
                "content": [{"type": "output_text", "text": "Responses OK"}]
            }]
        });
        assert_eq!(
            parse_responses_vision_response(&responses).unwrap(),
            "Responses OK"
        );
        assert_eq!(
            parse_responses_vision_response(&json!({"output_text": "shortcut"})).unwrap(),
            "shortcut"
        );
        assert!(parse_openai_vision_response(&json!({})).is_err());
        assert!(parse_claude_vision_response(&json!({"content": []})).is_err());
        assert!(parse_responses_vision_response(&json!({})).is_err());
    }

    #[test]
    fn cjk_space_stripping_and_ocr_normalization() {
        assert_eq!(strip_cjk_spaces("你 好 hello 世 界"), "你好 hello 世界");
        let result = normalize_ocr_result(OcrResult {
            text: "开 始\nOK".into(),
            lines: vec!["开 始".into(), "OK".into()],
            details: vec![OcrDetail {
                bbox: json!([[0, 0], [10, 0], [10, 10], [0, 10]]),
                text: "点 击".into(),
                conf: 0.9,
            }],
        });
        assert_eq!(result.text, "开始\nOK");
        assert_eq!(result.lines[0], "开始");
        assert_eq!(result.details[0].text, "点击");
    }

    #[test]
    fn settles_structured_long_term_updates() {
        let d = tempdir().unwrap();
        let cfg = cfg(d.path());
        fs::create_dir_all(&cfg.memory_dir).unwrap();
        fs::write(
            cfg.memory_dir.join("global_mem.txt"),
            "# [Global Memory - L2]\n",
        )
        .unwrap();
        fs::write(cfg.memory_dir.join("global_mem_insight.txt"), "已有\n").unwrap();
        fs::write(
            cfg.memory_dir.join("long_term_updates.jsonl"),
            r#"{"l2":{"section":"Rust Koda","facts":["Phase4 settlement worker exists"]},"l1_pointer":"Rust Koda -> L2","l3":{"file":"phase4_sop","content":"Only record action-verified facts."}}"#,
        )
        .unwrap();

        let report = settle_long_term_updates(&cfg).unwrap();
        assert_eq!(report.processed, 1);
        assert_eq!(report.pending, 0);
        assert_eq!(report.applied_l1, 1);
        assert_eq!(report.applied_l2, 1);
        assert_eq!(report.applied_l3, 1);
        assert!(!cfg.memory_dir.join("long_term_updates.jsonl").exists());
        assert!(report.archive_path.unwrap().exists());
        assert!(
            fs::read_to_string(cfg.memory_dir.join("global_mem.txt"))
                .unwrap()
                .contains("Phase4 settlement worker exists")
        );
        assert!(
            fs::read_to_string(cfg.memory_dir.join("global_mem_insight.txt"))
                .unwrap()
                .contains("Rust Koda -> L2")
        );
        assert!(cfg.memory_dir.join("phase4_sop.md").exists());
    }

    #[test]
    fn settlement_defers_empty_and_secret_like_entries() {
        let d = tempdir().unwrap();
        let cfg = cfg(d.path());
        fs::create_dir_all(&cfg.memory_dir).unwrap();
        fs::write(
            cfg.memory_dir.join("global_mem.txt"),
            "# [Global Memory - L2]\n",
        )
        .unwrap();
        fs::write(cfg.memory_dir.join("global_mem_insight.txt"), "").unwrap();
        fs::write(
            cfg.memory_dir.join("long_term_updates.jsonl"),
            "{}\n{\"fact\":\"api_key is sk-secret\"}\n",
        )
        .unwrap();

        let report = settle_long_term_updates(&cfg).unwrap();
        assert_eq!(report.processed, 2);
        assert_eq!(report.pending, 2);
        let pending =
            fs::read_to_string(cfg.memory_dir.join("pending_long_term_updates.md")).unwrap();
        assert!(pending.contains("empty_or_unstructured"));
        assert!(pending.contains("secret_like_content"));
        assert!(!pending.contains("sk-secret"));
        assert!(
            !fs::read_to_string(cfg.memory_dir.join("global_mem.txt"))
                .unwrap()
                .contains("api_key")
        );
    }

    #[test]
    fn settlement_rejects_broad_l1_and_reserved_l3_updates() {
        let d = tempdir().unwrap();
        let cfg = cfg(d.path());
        fs::create_dir_all(&cfg.memory_dir).unwrap();
        fs::write(cfg.memory_dir.join("global_mem_insight.txt"), "seed\n").unwrap();
        fs::write(
            cfg.memory_dir.join("long_term_updates.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"l1_pointer":"# heading\nnot an index line"}),
                json!({"l3":{"file":"global_mem.txt","content":"## A\nx\n## B\ny\n## C\nz"}})
            ),
        )
        .unwrap();

        let report = settle_long_term_updates(&cfg).unwrap();
        assert_eq!(report.processed, 2);
        assert_eq!(report.pending, 2);
        assert_eq!(
            fs::read_to_string(cfg.memory_dir.join("global_mem_insight.txt")).unwrap(),
            "seed\n"
        );
        assert!(!cfg.memory_dir.join("global_mem.txt.md").exists());
    }

    #[test]
    fn settlement_auto_syncs_l1_for_l2_l3_without_detail_dump() {
        let d = tempdir().unwrap();
        let cfg = cfg(d.path());
        fs::create_dir_all(&cfg.memory_dir).unwrap();
        fs::write(
            cfg.memory_dir.join("global_mem.txt"),
            "# [Global Memory - L2]\n",
        )
        .unwrap();
        fs::write(cfg.memory_dir.join("global_mem_insight.txt"), "").unwrap();
        fs::write(
            cfg.memory_dir.join("long_term_updates.jsonl"),
            r#"{"l2":{"section":"Browser Bridge","facts":["TMWebDriver extension smoke passed with contentSettings restore."]},"l3":{"file":"browser_bridge_notes","content":"Use tmwd-extension-smoke after changing bridge commands."}}"#,
        )
        .unwrap();

        let report = settle_long_term_updates(&cfg).unwrap();
        assert_eq!(report.applied_l2, 1);
        assert_eq!(report.applied_l3, 1);
        assert_eq!(report.applied_l1, 2);
        let l1 = fs::read_to_string(cfg.memory_dir.join("global_mem_insight.txt")).unwrap();
        assert!(l1.contains("Browser Bridge -> L2"));
        assert!(l1.contains("browser_bridge_notes.md"));
        assert!(!l1.contains("contentSettings restore"));
        assert!(cfg.memory_dir.join("browser_bridge_notes.md").exists());
    }

    #[test]
    fn settlement_rejects_l3_overwrite_and_unverified_notes() {
        let d = tempdir().unwrap();
        let cfg = cfg(d.path());
        fs::create_dir_all(&cfg.memory_dir).unwrap();
        fs::write(cfg.memory_dir.join("global_mem_insight.txt"), "").unwrap();
        fs::write(
            cfg.memory_dir.join("long_term_updates.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                json!({"l3":{"file":"safe_sop","content":"Overwrite the entire SOP with this new version."}}),
                json!({"l3":{"file":"guess_sop","content":"可能是浏览器插件导致的问题，未验证。"}}),
                json!({"l3":{"file":"code_sop","content":"```python\nprint('too detailed')\n```"}})
            ),
        )
        .unwrap();

        let report = settle_long_term_updates(&cfg).unwrap();
        assert_eq!(report.processed, 3);
        assert_eq!(report.pending, 3);
        assert!(!cfg.memory_dir.join("safe_sop.md").exists());
        assert!(!cfg.memory_dir.join("guess_sop.md").exists());
        assert!(!cfg.memory_dir.join("code_sop.md").exists());
    }

    #[test]
    fn l1_pointer_normalization_rejects_detail_and_caps_length() {
        assert!(normalize_l1_pointer("步骤1: read file\n步骤2: patch").is_none());
        assert!(normalize_l1_pointer("```json\n{}\n```").is_none());
        assert!(normalize_existing_l1_line("| a | b | c | d | e | f | g | h |").is_some());
        assert!(normalize_existing_l1_line("# [Global Memory Insight]").is_some());
        let long = normalize_l1_pointer(&"x".repeat(120)).unwrap();
        assert_eq!(long.chars().count(), 80);
    }

    #[test]
    fn memory_audit_cleanup_and_l4_recall_work() {
        let d = tempdir().unwrap();
        let cfg = cfg(d.path());
        fs::create_dir_all(&cfg.memory_dir).unwrap();
        fs::create_dir_all(cfg.memory_dir.join("L4_raw_sessions")).unwrap();
        fs::write(
            cfg.memory_dir.join("global_mem.txt"),
            "# [Global Memory - L2]\n\n## [Browser]\n- use tmwd\n",
        )
        .unwrap();
        fs::write(
            cfg.memory_dir.join("global_mem_insight.txt"),
            "- Browser -> L2\n- Browser -> L2\n- step 1\nstep 2\n",
        )
        .unwrap();
        fs::write(cfg.memory_dir.join("browser_sop.md"), "Browser SOP\n").unwrap();
        fs::write(
            cfg.memory_dir.join("L4_raw_sessions/all_histories.txt"),
            "============================================================\nSESSION: 0510_1000-0510_1010\n============================================================\n[USER]: browser bridge failed\n[Agent]: tmwd extension smoke fixed it\n",
        )
        .unwrap();

        let audit = audit_memory(&cfg).unwrap();
        assert_eq!(audit.l1_valid_lines, 1);
        assert_eq!(audit.l1_duplicate_lines.len(), 1);
        assert!(audit.missing_l1_pointers.contains(&"browser_sop.md".into()));
        assert_eq!(audit.l4_sessions, 1);

        let dry = cleanup_memory_indexes(&cfg, true, true).unwrap();
        assert!(dry.dry_run);
        assert_eq!(dry.added_missing_pointers, vec!["browser_sop.md"]);
        assert!(
            fs::read_to_string(cfg.memory_dir.join("global_mem_insight.txt"))
                .unwrap()
                .contains("step 1")
        );

        let run = cleanup_memory_indexes(&cfg, false, true).unwrap();
        assert!(!run.dry_run);
        assert!(run.backup_path.as_ref().is_some_and(|p| p.exists()));
        let l1 = fs::read_to_string(cfg.memory_dir.join("global_mem_insight.txt")).unwrap();
        assert!(l1.contains("Browser -> L2"));
        assert!(l1.contains("browser_sop.md"));
        assert!(!l1.contains("step 1"));

        let hits = recall_l4_history(&cfg, "tmwd smoke", 3).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session, "0510_1000-0510_1010");
        assert!(hits[0].excerpt.contains("tmwd extension smoke"));
    }

    #[test]
    fn memory_audit_treats_grouped_l1_entries_as_coverage() {
        let d = tempdir().unwrap();
        let cfg = cfg(d.path());
        fs::create_dir_all(&cfg.memory_dir).unwrap();
        fs::write(
            cfg.memory_dir.join("global_mem_insight.txt"),
            "L3: tmwebdriver_sop(浏览器) | ocr_utils.py | vision_sop\n",
        )
        .unwrap();
        fs::write(cfg.memory_dir.join("tmwebdriver_sop.md"), "tmwd\n").unwrap();
        fs::write(cfg.memory_dir.join("ocr_utils.py"), "ocr\n").unwrap();
        fs::write(cfg.memory_dir.join("vision_sop.md"), "vision\n").unwrap();
        fs::write(cfg.memory_dir.join("vision_api.template.py"), "template\n").unwrap();

        let audit = audit_memory(&cfg).unwrap();
        assert!(
            !audit
                .missing_l1_pointers
                .contains(&"tmwebdriver_sop.md".into())
        );
        assert!(!audit.missing_l1_pointers.contains(&"ocr_utils.py".into()));
        assert!(!audit.missing_l1_pointers.contains(&"vision_sop.md".into()));
        assert!(
            !audit
                .missing_l1_pointers
                .contains(&"vision_api.template.py".into())
        );
    }

    #[test]
    fn l4_recall_reads_recent_json_sessions() {
        let d = tempdir().unwrap();
        let cfg = cfg(d.path());
        fs::create_dir_all(cfg.memory_dir.join("L4_raw_sessions")).unwrap();
        fs::write(
            cfg.memory_dir
                .join("L4_raw_sessions/session_20260510_170000_1.json"),
            serde_json::to_string(&json!({
                "session": "session_20260510_170000_1",
                "history": ["[USER]: memory recall"],
                "messages": [
                    {"role":"assistant","content":{"text":"tmwd json recall works","tool_calls":[{"name":"web_execute_js"}]}}
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let hits = recall_l4_history(&cfg, "tmwd recall", 2).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session, "session_20260510_170000_1");
        assert!(hits[0].excerpt.contains("tmwd json recall works"));
    }

    #[tokio::test]
    async fn assisted_settlement_converts_unsupported_note_to_safe_patch() {
        let d = tempdir().unwrap();
        let cfg = cfg(d.path());
        fs::create_dir_all(&cfg.memory_dir).unwrap();
        fs::write(
            cfg.memory_dir.join("memory_management_sop.md"),
            "No Execution, No Memory.",
        )
        .unwrap();
        fs::write(
            cfg.memory_dir.join("global_mem.txt"),
            "# [Global Memory - L2]\n",
        )
        .unwrap();
        fs::write(cfg.memory_dir.join("global_mem_insight.txt"), "").unwrap();
        fs::write(
            cfg.memory_dir.join("long_term_updates.jsonl"),
            r#"{"note":"Validated that Koda memory assisted settlement should record only action-verified facts."}"#,
        )
        .unwrap();
        let llm = StaticLlm {
            content: r#"```json
{"updates":[{"l1_pointer":"Koda memory -> L2","l2":{"section":"Koda Memory","facts":["Assisted settlement records only action-verified facts."]}}]}
```"#
            .into(),
        };

        let report = settle_long_term_updates_assisted(&cfg, &llm).await.unwrap();
        assert_eq!(report.processed, 1);
        assert_eq!(report.pending, 0);
        assert_eq!(report.assisted_attempts, 1);
        assert_eq!(report.assisted_applied, 1);
        assert!(
            fs::read_to_string(cfg.memory_dir.join("global_mem.txt"))
                .unwrap()
                .contains("Assisted settlement records only action-verified facts.")
        );
        assert!(
            fs::read_to_string(cfg.memory_dir.join("global_mem_insight.txt"))
                .unwrap()
                .contains("Koda memory -> L2")
        );
    }

    #[test]
    fn l4_compresses_raw_log_and_merges_history_blocks() {
        let raw = format!(
            "=== Prompt === 2026-05-10 10:00:00\nsys prompt should be stripped\n=== USER ===\nhello\n=== ASSISTANT ===\necho should be stripped\n=== Response === 2026-05-10 10:05:00\n<history>\n[USER]: hi\n[Agent]: one\n</history>\nnoise {}\n=== Prompt === 2026-05-10 10:10:00\n{{\"role\":\"user\"}}\n=== Response === 2026-05-10 10:11:00\n<history>\\n[USER]: hi\\n[Agent]: one\\n[USER]: again\\n[Agent]: two</history>\n{}",
            "x".repeat(4600),
            "y".repeat(100)
        );
        let compressed = compress_l4_raw(&raw);
        assert!(compressed.contains("=== USER ==="));
        assert!(compressed.contains("hello"));
        assert!(!compressed.contains("sys prompt should be stripped"));
        assert!(!compressed.contains("echo should be stripped"));
        let history = extract_l4_history(&compressed);
        assert_eq!(
            history,
            vec![
                "[USER]: hi",
                "[Agent]: one",
                "[USER]: again",
                "[Agent]: two"
            ]
        );
    }

    #[test]
    fn l4_archive_batches_to_history_and_monthly_zip() {
        let d = tempdir().unwrap();
        let cfg = cfg(d.path());
        let raw_dir = cfg.temp_dir.join("model_responses");
        fs::create_dir_all(&raw_dir).unwrap();
        fs::create_dir_all(cfg.memory_dir.join("L4_raw_sessions")).unwrap();
        fs::write(
            raw_dir.join("model_responses_123.txt"),
            format!(
                "=== Prompt === 2026-05-10 10:00:00\nignored\n=== USER ===\nhello\n=== Response === 2026-05-10 10:10:00\n<history>\n[USER]: hi\n[Agent]: archived\n</history>\n{}\n",
                "z".repeat(5000)
            ),
        )
        .unwrap();

        let dry =
            archive_l4_sessions_with_min_age(&cfg, Some(&raw_dir), true, Duration::ZERO).unwrap();
        assert_eq!(dry.new_sessions, 1);
        assert!(raw_dir.join("model_responses_123.txt").exists());

        let report =
            archive_l4_sessions_with_min_age(&cfg, Some(&raw_dir), false, Duration::ZERO).unwrap();
        assert_eq!(report.new_sessions, 1);
        assert_eq!(report.deleted_raw, 1);
        assert!(!raw_dir.join("model_responses_123.txt").exists());
        let histories =
            fs::read_to_string(cfg.memory_dir.join("L4_raw_sessions/all_histories.txt")).unwrap();
        assert!(histories.contains("SESSION: 0510_1000-0510_1000"));
        assert!(histories.contains("[Agent]: archived"));
        let zip_path = cfg.memory_dir.join("L4_raw_sessions/2026-05.zip");
        assert!(zip_path.exists());
        let file = fs::File::open(zip_path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        assert!(archive.by_name("0510_1000-0510_1000.txt").is_ok());
    }
}
