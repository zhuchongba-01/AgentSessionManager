#![allow(dead_code)]

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use serde_json::Value;

use crate::session_manager::cache::TranscriptSources;
use crate::session_manager::{SessionMessage, SessionMeta};
use crate::session_paths::codebuddy_config_dir;

use super::utils::{
    checked_storage_child, ensure_readable_size, extract_text, extract_thinking_text,
    is_reparse_or_symlink, is_safe_session_id, parse_timestamp_to_ms, path_basename,
    read_head_tail_lines, remove_path_if_exists, scan_warning, truncate_summary, FileParseCache,
    TITLE_MAX_CHARS,
};

const PROVIDER_ID: &str = "codebuddy";
const HEAD_SCAN_LINES: usize = 60;
const TAIL_SCAN_LINES: usize = 40;

static PARSE_CACHE: LazyLock<FileParseCache> = LazyLock::new(FileParseCache::new);

pub fn session_roots() -> Vec<PathBuf> {
    vec![codebuddy_config_dir().join("projects")]
}

pub fn transcript_sources(path: &Path) -> Result<TranscriptSources, String> {
    Ok(vec![(path.to_path_buf(), None)])
}

pub fn scan_sessions() -> Vec<SessionMeta> {
    let root = codebuddy_config_dir().join("projects");
    scan_sessions_in_root(&root, &PARSE_CACHE)
}

fn is_valid_codebuddy_session_id(id: &str) -> bool {
    is_safe_session_id(id)
        && !id.starts_with("agent-")
        && !matches!(id, "sessions-index" | "memory" | "shared" | "temp" | "tmp")
}

fn scan_sessions_in_root(projects_root: &Path, cache: &FileParseCache) -> Vec<SessionMeta> {
    match fs::symlink_metadata(projects_root) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                scan_warning(format!(
                    "CodeBuddy 会话根目录是符号链接或重解析点，已跳过：{}",
                    projects_root.display()
                ));
                return Vec::new();
            }
            if !meta.is_dir() {
                return Vec::new();
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            scan_warning(format!(
                "无法检查 CodeBuddy 会话目录 {}：{err}",
                projects_root.display()
            ));
            return Vec::new();
        }
    }

    let mut files = Vec::new();
    let proj_entries = match fs::read_dir(projects_root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            scan_warning(format!(
                "无法扫描 CodeBuddy 目录 {}：{err}",
                projects_root.display()
            ));
            return Vec::new();
        }
    };

    for proj_entry in proj_entries {
        let proj_entry = match proj_entry {
            Ok(e) => e,
            Err(err) => {
                scan_warning(format!("无法枚举 {}：{err}", projects_root.display()));
                continue;
            }
        };
        let Ok(proj_ft) = proj_entry.file_type() else {
            continue;
        };
        if !proj_ft.is_dir() {
            continue;
        }
        let proj_path = proj_entry.path();
        if fs::symlink_metadata(&proj_path).is_ok_and(|m| is_reparse_or_symlink(&m)) {
            continue;
        }
        let session_entries = match fs::read_dir(&proj_path) {
            Ok(entries) => entries,
            Err(err) => {
                scan_warning(format!("无法扫描 {}：{err}", proj_path.display()));
                continue;
            }
        };
        for ses_entry in session_entries {
            let Ok(ses_entry) = ses_entry else {
                continue;
            };
            let Ok(ses_ft) = ses_entry.file_type() else {
                continue;
            };
            if !ses_ft.is_file() {
                continue;
            }
            let ses_path = ses_entry.path();
            if fs::symlink_metadata(&ses_path).is_ok_and(|m| is_reparse_or_symlink(&m)) {
                continue;
            }
            if ses_path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = ses_path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !is_valid_codebuddy_session_id(stem) {
                continue;
            }
            files.push(ses_path);
        }
    }

    cache.scan(files, parse_session_result)
}

fn record_kind(record: &Value) -> &str {
    record.get("type").and_then(Value::as_str).unwrap_or("")
}

fn record_role(record: &Value) -> &str {
    record.get("role").and_then(Value::as_str).unwrap_or("")
}

fn is_meta(record: &Value) -> bool {
    record
        .pointer("/providerData/isMeta")
        .and_then(Value::as_bool)
        == Some(true)
}

fn is_compaction_summary(record: &Value) -> bool {
    let Some(pd) = record.get("providerData") else {
        return false;
    };
    pd.get("isCompacted").and_then(Value::as_bool) == Some(true)
        || pd.get("isCompactInternal").and_then(Value::as_bool) == Some(true)
        || pd.get("isSummary").and_then(Value::as_bool) == Some(true)
}

fn unwrap_user_query(raw: &str) -> String {
    const OPEN: &str = "<user_query>";
    const CLOSE: &str = "</user_query>";
    if let Some(start) = raw.rfind(OPEN) {
        let rest = &raw[start + OPEN.len()..];
        let end = rest.find(CLOSE).unwrap_or(rest.len());
        return rest[..end].trim().to_string();
    }
    strip_system_reminders(raw).trim().to_string()
}

fn strip_system_reminders(raw: &str) -> String {
    const REM_OPEN: &str = "<system-reminder>";
    const REM_CLOSE: &str = "</system-reminder>";
    let mut current = raw;
    let mut out = String::new();
    while let Some(start) = current.find(REM_OPEN) {
        out.push_str(&current[..start]);
        let after_open = &current[start + REM_OPEN.len()..];
        if let Some(end) = after_open.find(REM_CLOSE) {
            current = &after_open[end + REM_CLOSE.len()..];
        } else {
            current = "";
            break;
        }
    }
    out.push_str(current);
    out
}

fn extract_codebuddy_content_item(item: &Value, is_user: bool) -> Option<String> {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    match item_type {
        "thinking" | "reasoning" | "redacted_thinking" | "summary_text" | "reasoning_text" => None,
        "image_blob_ref" | "input_image" | "image" => {
            if let Some(filename) = item
                .get("original_filename")
                .or_else(|| item.get("filename"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(format!("[Image: {filename}]"))
            } else {
                Some("[Image]".to_string())
            }
        }
        "input_text" | "output_text" | "text" => {
            let raw = item
                .get("text")
                .or_else(|| item.get("input_text"))
                .or_else(|| item.get("output_text"))
                .and_then(Value::as_str)?;
            let text = if is_user {
                unwrap_user_query(raw)
            } else {
                raw.to_string()
            };
            (!text.trim().is_empty()).then_some(text)
        }
        _ => {
            let fallback = extract_text(item);
            let text = if is_user {
                unwrap_user_query(&fallback)
            } else {
                fallback
            };
            (!text.trim().is_empty()).then_some(text)
        }
    }
}

fn extract_message_text(record: &Value) -> String {
    let is_user = record_role(record) == "user";
    let Some(content) = record.get("content") else {
        return String::new();
    };
    match content {
        Value::String(s) => {
            if is_user {
                unwrap_user_query(s)
            } else {
                s.clone()
            }
        }
        Value::Array(items) => items
            .iter()
            .filter_map(|item| extract_codebuddy_content_item(item, is_user))
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => extract_codebuddy_content_item(content, is_user).unwrap_or_default(),
        _ => String::new(),
    }
}

fn user_text(record: &Value) -> Option<String> {
    if record_kind(record) != "message"
        || record_role(record) != "user"
        || is_meta(record)
        || is_compaction_summary(record)
    {
        return None;
    }
    let text = extract_message_text(record);
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn assistant_text(record: &Value) -> Option<String> {
    if record_kind(record) != "message" || record_role(record) != "assistant" || is_meta(record) {
        return None;
    }
    let text = extract_message_text(record);
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn ai_title(record: &Value) -> Option<String> {
    if record_kind(record) != "ai-title" {
        return None;
    }
    record
        .get("aiTitle")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn session_meta_title(record: &Value) -> Option<String> {
    if record_kind(record) != "session-meta" {
        return None;
    }
    record
        .get("meta")
        .and_then(|m| {
            m.get("codebuddy.ai/sessionTitle")
                .or_else(|| m.get("title"))
        })
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn resume_command(id: &str) -> Option<String> {
    if !is_valid_codebuddy_session_id(id) {
        return None;
    }
    let mut prefix = String::new();
    if let Ok(value) = std::env::var("CODEBUDDY_CONFIG_DIR") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            #[cfg(windows)]
            prefix.push_str(&format!(
                "$env:CODEBUDDY_CONFIG_DIR = '{}'; ",
                trimmed.replace('\'', "''")
            ));
            #[cfg(not(windows))]
            prefix.push_str(&format!(
                "env CODEBUDDY_CONFIG_DIR={} ",
                crate::session_manager::terminal::shell_escape(trimmed)
            ));
        }
    }
    Some(format!("{prefix}codebuddy --resume {id}"))
}

fn parse_session_result(path: &Path) -> Result<Option<SessionMeta>, String> {
    let Some(stem) = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| is_valid_codebuddy_session_id(s))
        .map(str::to_string)
    else {
        return Ok(None);
    };

    let (head_lines, tail_lines) = read_head_tail_lines(path, HEAD_SCAN_LINES, TAIL_SCAN_LINES)
        .map_err(|err| format!("无法读取 CodeBuddy 会话 {}：{err}", path.display()))?;

    let head: Vec<Value> = head_lines
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .collect();
    let tail: Vec<Value> = tail_lines
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .collect();

    let session_id = head
        .iter()
        .find_map(|r| r.get("sessionId").and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or(stem);

    let prompt = head
        .iter()
        .find_map(user_text)
        .or_else(|| head.iter().find_map(assistant_text))
        .or_else(|| tail.iter().find_map(user_text))
        .or_else(|| tail.iter().find_map(assistant_text));
    let Some(prompt) = prompt else {
        return Ok(None);
    };

    let raw_title = tail
        .iter()
        .rev()
        .find_map(ai_title)
        .or_else(|| head.iter().rev().find_map(ai_title))
        .or_else(|| head.iter().find_map(session_meta_title))
        .unwrap_or_else(|| prompt.lines().next().unwrap_or_default().to_string());

    let project_dir = head
        .iter()
        .chain(tail.iter())
        .find_map(|r| r.get("cwd").and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let project_name = project_dir.as_deref().and_then(path_basename);

    let created_at = head
        .iter()
        .find_map(|r| r.get("timestamp").and_then(parse_timestamp_to_ms));
    let last_active_at = tail
        .iter()
        .rev()
        .chain(head.iter().rev())
        .find_map(|r| r.get("timestamp").and_then(parse_timestamp_to_ms))
        .or(created_at);

    Ok(Some(SessionMeta {
        provider_id: PROVIDER_ID.to_string(),
        resume_command: resume_command(&session_id),
        session_id,
        residual: false,
        parent_session_id: None,
        archived: false,
        cleanup_pending: false,
        title: Some(truncate_summary(&raw_title, TITLE_MAX_CHARS)),
        summary: Some(truncate_summary(&prompt, 160)),
        project_dir,
        project_name,
        sidebar_section: None,
        created_at,
        last_active_at,
        source_path: Some(path.to_string_lossy().to_string()),
    }))
}

fn extract_reasoning(record: &Value) -> String {
    for key in ["rawContent", "content", "summary"] {
        if let Some(val) = record.get(key) {
            let text = match val {
                Value::String(s) => s.clone(),
                Value::Array(items) => items
                    .iter()
                    .filter_map(|item| match item {
                        Value::String(s) => Some(s.clone()),
                        Value::Object(_) => item
                            .get("text")
                            .or_else(|| item.get("thinking"))
                            .or_else(|| item.get("input_text"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        _ => None,
                    })
                    .filter(|s| !s.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n"),
                Value::Object(_) => val
                    .get("text")
                    .or_else(|| val.get("thinking"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                _ => String::new(),
            };
            if !text.trim().is_empty() {
                return text;
            }
        }
    }
    record
        .get("text")
        .or_else(|| record.get("thinking"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn format_function_call(record: &Value) -> Option<(Option<String>, String, String)> {
    let name = record
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let call_id = record
        .get("callId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let args = match record.get("arguments") {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(v) if !v.is_null() => v.to_string(),
        _ => String::new(),
    };
    let formatted = if args.is_empty() {
        format!("[Tool: {name}]")
    } else {
        format!("[Tool: {name}]\n{args}")
    };
    Some((call_id, name, formatted))
}

fn format_function_call_result(
    record: &Value,
    call_names: &HashMap<String, String>,
) -> Option<String> {
    let call_id = record
        .get("callId")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let name = record
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| call_names.get(call_id).map(String::as_str))
        .unwrap_or("");

    let is_error = matches!(
        record.get("status").and_then(Value::as_str),
        Some("failed" | "error")
    ) || record
        .pointer("/providerData/toolResult/error")
        .is_some_and(|v| !v.is_null());

    let output_text = match record.get("output") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(map)) => map
            .get("text")
            .or_else(|| map.get("content"))
            .or_else(|| map.get("error"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| Value::Object(map.clone()).to_string()),
        Some(Value::Array(arr)) => extract_text(&Value::Array(arr.clone())),
        Some(v) if !v.is_null() => v.to_string(),
        _ => String::new(),
    };
    let trimmed = output_text.trim();
    if trimmed.is_empty() && !is_error {
        return None;
    }
    let label = if is_error { "Error" } else { "Result" };
    Some(match (!name.is_empty(), !trimmed.is_empty()) {
        (true, true) => format!("[{label}: {name}]\n{trimmed}"),
        (true, false) => format!("[{label}: {name}]"),
        (false, true) => trimmed.to_string(),
        (false, false) => format!("[{label}]"),
    })
}

/// Deduplicate streaming updates sharing the same `(id, parentId)` in-place while preserving
/// the chronological order of all events (including teammate sessions without `parentId`).
fn deduplicate_records(entries: &[Value]) -> Vec<&Value> {
    let mut nodes: Vec<&Value> = Vec::new();
    let mut last_by_id: HashMap<&str, usize> = HashMap::new();

    for entry in entries {
        if let Some(id) = entry
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let Some(&prev_idx) = last_by_id.get(id) {
                let prev = nodes[prev_idx];
                if prev.get("parentId") == entry.get("parentId")
                    && record_kind(prev) == record_kind(entry)
                {
                    nodes[prev_idx] = entry;
                    continue;
                }
            }
            let idx = nodes.len();
            nodes.push(entry);
            last_by_id.insert(id, idx);
        } else {
            nodes.push(entry);
        }
    }
    nodes
}

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    ensure_readable_size(path)?;
    let file =
        File::open(path).map_err(|e| format!("Failed to open CodeBuddy session file: {e}"))?;
    let reader = BufReader::new(file);

    let mut entries = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| format!("Failed to read CodeBuddy transcript: {e}"))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
            entries.push(val);
        }
    }

    let mut messages = Vec::new();
    let mut call_names: HashMap<String, String> = HashMap::new();

    for record in deduplicate_records(&entries) {
        let ts = record.get("timestamp").and_then(parse_timestamp_to_ms);
        match record_kind(record) {
            "reasoning" => {
                let thinking = extract_reasoning(record);
                if !thinking.trim().is_empty() {
                    messages.push(SessionMessage {
                        role: "thinking".to_string(),
                        content: thinking,
                        ts,
                    });
                }
            }
            "function_call" => {
                if let Some((call_id, name, formatted)) = format_function_call(record) {
                    if let Some(cid) = call_id {
                        call_names.insert(cid, name);
                    }
                    messages.push(SessionMessage {
                        role: "tool".to_string(),
                        content: formatted,
                        ts,
                    });
                }
            }
            "function_call_result" => {
                if let Some(formatted) = format_function_call_result(record, &call_names) {
                    messages.push(SessionMessage {
                        role: "tool".to_string(),
                        content: formatted,
                        ts,
                    });
                }
            }
            "message" => {
                if is_meta(record) {
                    continue;
                }
                if is_compaction_summary(record) {
                    let body = extract_message_text(record);
                    let trimmed = body.trim();
                    if !trimmed.is_empty() {
                        let content = if trimmed.starts_with("[Compacted Summary]") {
                            trimmed.to_string()
                        } else {
                            format!("[Compacted Summary]\n{trimmed}")
                        };
                        messages.push(SessionMessage {
                            role: "system".to_string(),
                            content,
                            ts,
                        });
                    }
                    continue;
                }

                let role = record_role(record);
                if role == "assistant" {
                    if let Some(content_val) = record.get("content") {
                        let inline_thinking = extract_thinking_text(content_val);
                        if !inline_thinking.trim().is_empty() {
                            messages.push(SessionMessage {
                                role: "thinking".to_string(),
                                content: inline_thinking,
                                ts,
                            });
                        }
                    }
                }
                if matches!(role, "user" | "assistant" | "system") {
                    let content = extract_message_text(record);
                    if !content.trim().is_empty() {
                        messages.push(SessionMessage {
                            role: role.to_string(),
                            content,
                            ts,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    Ok(messages)
}

fn validate_codebuddy_session_path(
    root: &Path,
    path: &Path,
    session_id: &str,
) -> Result<(PathBuf, PathBuf, bool), String> {
    if !is_valid_codebuddy_session_id(session_id) {
        return Err(format!("Invalid CodeBuddy session ID: {session_id}"));
    }
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(format!(
            "Invalid CodeBuddy session path: {}",
            path.display()
        ));
    }
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return Err(format!(
            "Invalid CodeBuddy session file extension: {}",
            path.display()
        ));
    }
    if path.file_stem().and_then(|s| s.to_str()) != Some(session_id) {
        return Err(format!(
            "CodeBuddy session ID ({session_id}) does not match file path: {}",
            path.display()
        ));
    }

    let file_exists = match fs::symlink_metadata(path) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                return Err(format!(
                    "CodeBuddy session source must not be a symlink or reparse point: {}",
                    path.display()
                ));
            }
            if !meta.file_type().is_file() {
                return Err(format!(
                    "CodeBuddy session source is not a regular file: {}",
                    path.display()
                ));
            }
            true
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            return Err(format!(
                "Failed to inspect CodeBuddy session {}: {err}",
                path.display()
            ))
        }
    };

    let canonical_root = root.canonicalize().map_err(|err| {
        format!(
            "Failed to resolve CodeBuddy projects root {}: {err}",
            root.display()
        )
    })?;
    let root_meta = fs::metadata(&canonical_root).map_err(|err| {
        format!(
            "Failed to inspect CodeBuddy projects root {}: {err}",
            canonical_root.display()
        )
    })?;
    if !root_meta.is_dir() {
        return Err(format!(
            "CodeBuddy projects root is not a directory: {}",
            canonical_root.display()
        ));
    }

    let resolved_path = checked_storage_child(root, path)?;
    let relative = resolved_path.strip_prefix(&canonical_root).map_err(|_| {
        format!(
            "CodeBuddy session path is outside projects root: {}",
            path.display()
        )
    })?;
    let components: Vec<_> = relative.components().collect();
    if components.len() != 2
        || !matches!(components[0], Component::Normal(_))
        || !matches!(components[1], Component::Normal(_))
    {
        return Err(format!(
            "CodeBuddy session path must be <projects>/<slug>/<session>.jsonl: {}",
            path.display()
        ));
    }

    if file_exists {
        let (head, _) = read_head_tail_lines(&resolved_path, HEAD_SCAN_LINES, 0)
            .map_err(|err| format!("Failed to read CodeBuddy session {}: {err}", path.display()))?;
        for line in head {
            if let Ok(val) = serde_json::from_str::<Value>(line.trim()) {
                if let Some(stored_id) = val
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    if stored_id != session_id {
                        return Err(format!(
                            "CodeBuddy session ID mismatch: expected {session_id}, found {stored_id}"
                        ));
                    }
                    break;
                }
            }
        }
    }

    let project_dir = resolved_path
        .parent()
        .ok_or_else(|| format!("Invalid CodeBuddy project directory: {}", path.display()))?;
    for sidecar in [
        project_dir.join(format!("{session_id}.file-rollback.ndjson")),
        project_dir.join(session_id),
    ] {
        if let Ok(meta) = fs::symlink_metadata(&sidecar) {
            if is_reparse_or_symlink(&meta) {
                return Err(format!(
                    "Refusing to delete CodeBuddy session with symlinked sidecar: {}",
                    sidecar.display()
                ));
            }
        }
    }

    Ok((canonical_root, resolved_path, file_exists))
}

pub fn preflight_delete(root: &Path, path: &Path, session_id: &str) -> Result<(), String> {
    validate_codebuddy_session_path(root, path, session_id).map(|_| ())
}

pub fn delete_session(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    let (canonical_root, resolved_path, _file_exists) =
        validate_codebuddy_session_path(root, path, session_id)?;
    let project_dir = resolved_path
        .parent()
        .ok_or_else(|| format!("Invalid CodeBuddy project directory: {}", path.display()))?;

    let mut removed = false;
    let targets = [
        resolved_path.clone(),
        project_dir.join(format!("{session_id}.file-rollback.ndjson")),
        project_dir.join(session_id),
    ];

    for target in &targets {
        match fs::symlink_metadata(target) {
            Ok(meta) => {
                if is_reparse_or_symlink(&meta) {
                    return Err(format!(
                        "CodeBuddy target turned into a symlink before deletion: {}",
                        target.display()
                    ));
                }
                let checked = checked_storage_child(&canonical_root, target)?;
                remove_path_if_exists(&checked)
                    .map_err(|err| format!("Failed to delete {}: {err}", checked.display()))?;
                removed = true;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(format!(
                    "Failed to inspect CodeBuddy artifact {}: {err}",
                    target.display()
                ))
            }
        }
    }

    let _ = fs::remove_dir(project_dir);
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const SESSION: &str = r#"{"type":"session-meta","id":"m0","sessionId":"0f3c-41aa","timestamp":1791900000000,"meta":{}}
{"id":"u1","parentId":null,"timestamp":1791900000100,"sessionId":"0f3c-41aa","cwd":"/work/proj","type":"message","role":"user","content":[{"type":"input_text","text":"list the txt files"}],"providerData":{"agent":"cli"}}
{"id":"r1","parentId":"u1","timestamp":1791900000200,"sessionId":"0f3c-41aa","cwd":"/work/proj","type":"reasoning","content":[],"rawContent":[{"type":"reasoning_text","text":"Use a glob."}],"providerData":{"messageId":"msg1","stepSeq":1,"model":"hy4-preview"}}
{"id":"c1","parentId":"r1","timestamp":1791900000300,"sessionId":"0f3c-41aa","cwd":"/work/proj","type":"function_call","callId":"call_1","name":"Bash","arguments":"{\"command\":\"ls *.txt\"}"}
{"id":"o1","parentId":"c1","timestamp":1791900000400,"sessionId":"0f3c-41aa","cwd":"/work/proj","type":"function_call_result","callId":"call_1","name":"Bash","status":"completed","output":{"type":"text","text":"a.txt"}}
{"id":"u2","parentId":"o1","timestamp":1791900000500,"sessionId":"0f3c-41aa","cwd":"/work/proj","type":"message","role":"user","content":[{"type":"input_text","text":"<system-reminder>use the task tools</system-reminder>"}],"providerData":{"isMeta":true,"taskReminderKind":"task"}}
{"timestamp":1791900000550,"type":"file-history-snapshot","snapshot":{}}
{"id":"a1","parentId":"u2","timestamp":1791900000600,"sessionId":"0f3c-41aa","cwd":"/work/proj","type":"message","role":"assistant","content":[{"type":"output_text","text":"Found a.txt."}]}
{"id":"t1","timestamp":1791900000700,"sessionId":"0f3c-41aa","type":"ai-title","aiTitle":"List text files"}
"#;

    #[cfg(unix)]
    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_file(target, link)
    }

    #[test]
    #[serial_test::serial]
    #[allow(deprecated)]
    fn scans_title_cwd_teammate_sessions_and_resume_command() {
        let prev = std::env::var_os("CODEBUDDY_CONFIG_DIR");
        std::env::remove_var("CODEBUDDY_CONFIG_DIR");

        let temp = tempdir().unwrap();
        let projects = temp.path().join("projects");
        let proj = projects.join("work-proj");
        fs::create_dir_all(proj.join("0f3c-41aa").join("tool-results")).unwrap();
        let path = proj.join("0f3c-41aa.jsonl");
        fs::write(&path, SESSION).unwrap();
        fs::write(proj.join("empty.jsonl"), "").unwrap();
        // Subagent jsonl inside sidecar dir must NOT be scanned as top-level session
        fs::write(proj.join("0f3c-41aa").join("sub.jsonl"), SESSION).unwrap();

        // Teammate session without user prompt falls back to first reply
        fs::write(
            proj.join("teammate-1.jsonl"),
            r#"{"id":"c","timestamp":1,"sessionId":"teammate-1","type":"function_call","callId":"x","name":"SpeakInChannel","arguments":"{}"}
{"id":"a","timestamp":2,"sessionId":"teammate-1","type":"message","role":"assistant","content":[{"type":"output_text","text":"Reporting in."}]}
"#,
        )
        .unwrap();

        let cache = FileParseCache::new();
        let sessions = scan_sessions_in_root(&projects, &cache);
        assert_eq!(sessions.len(), 2);

        let main = sessions
            .iter()
            .find(|s| s.session_id == "0f3c-41aa")
            .unwrap();
        assert_eq!(main.provider_id, "codebuddy");
        assert_eq!(main.title.as_deref(), Some("List text files"));
        assert_eq!(main.summary.as_deref(), Some("list the txt files"));
        assert_eq!(main.project_dir.as_deref(), Some("/work/proj"));
        assert_eq!(main.project_name.as_deref(), Some("proj"));
        assert_eq!(
            main.resume_command.as_deref(),
            Some("codebuddy --resume 0f3c-41aa")
        );
        assert_eq!(main.created_at, Some(1791900000000));
        assert_eq!(main.last_active_at, Some(1791900000700));

        let mate = sessions
            .iter()
            .find(|s| s.session_id == "teammate-1")
            .unwrap();
        assert_eq!(mate.title.as_deref(), Some("Reporting in."));

        // Custom CODEBUDDY_CONFIG_DIR prefixes resume command
        std::env::set_var("CODEBUDDY_CONFIG_DIR", temp.path());
        let cmd = resume_command("0f3c-41aa").unwrap();
        assert!(cmd.contains("CODEBUDDY_CONFIG_DIR"));
        assert!(cmd.ends_with("codebuddy --resume 0f3c-41aa"));

        match prev {
            Some(v) => std::env::set_var("CODEBUDDY_CONFIG_DIR", v),
            None => std::env::remove_var("CODEBUDDY_CONFIG_DIR"),
        }
    }

    #[test]
    fn load_messages_hides_is_meta_and_separates_thinking_and_tools() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("0f3c-41aa.jsonl");
        fs::write(&path, SESSION).unwrap();

        let messages = load_messages(&path).unwrap();
        let pairs: Vec<(&str, &str)> = messages
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("user", "list the txt files"),
                ("thinking", "Use a glob."),
                ("tool", "[Tool: Bash]\n{\"command\":\"ls *.txt\"}"),
                ("tool", "[Result: Bash]\na.txt"),
                ("assistant", "Found a.txt."),
            ]
        );
    }

    #[test]
    fn delete_session_removes_jsonl_rollback_and_sidecar_dir_and_rejects_symlinks() {
        let temp = tempdir().unwrap();
        let projects = temp.path().join("projects");
        let proj = projects.join("work-proj");
        let id = "0f3c-41aa";
        fs::create_dir_all(proj.join(id).join("tool-results")).unwrap();
        let jsonl = proj.join(format!("{id}.jsonl"));
        let rollback = proj.join(format!("{id}.file-rollback.ndjson"));
        fs::write(&jsonl, SESSION).unwrap();
        fs::write(&rollback, "{}\n").unwrap();
        fs::write(proj.join(id).join("tool-results").join("r1.txt"), "out").unwrap();

        let keep_jsonl = proj.join("keep-1.jsonl");
        fs::write(
            &keep_jsonl,
            r#"{"sessionId":"keep-1","type":"message","role":"user","content":[{"type":"text","text":"keep"}]}"#,
        )
        .unwrap();

        // Symlinked transcript is rejected
        let link = proj.join("linked-1.jsonl");
        if create_file_symlink(&keep_jsonl, &link).is_ok() {
            assert!(preflight_delete(&projects, &link, "linked-1").is_err());
            assert!(delete_session(&projects, &link, "linked-1").is_err());
            assert!(keep_jsonl.exists());
            let _ = fs::remove_file(&link);
        }

        // Depth mismatch rejected
        assert!(preflight_delete(temp.path(), &jsonl, id).is_err());
        // Session ID mismatch rejected
        assert!(preflight_delete(&projects, &jsonl, "other-id").is_err());

        preflight_delete(&projects, &jsonl, id).unwrap();
        assert!(delete_session(&projects, &jsonl, id).unwrap());
        assert!(!jsonl.exists());
        assert!(!rollback.exists());
        assert!(!proj.join(id).exists());
        assert!(keep_jsonl.exists());
    }
}
