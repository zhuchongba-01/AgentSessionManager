#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use serde_json::Value;

use crate::session_manager::cache::TranscriptSources;
use crate::session_manager::{SessionMessage, SessionMeta};
use crate::session_paths::{configured_dir, kimi_home};

use super::utils::{
    checked_storage_child, ensure_readable_size, extract_text, is_reparse_or_symlink,
    is_safe_session_id, parse_timestamp_to_ms, path_basename, read_head_tail_lines,
    remove_path_if_exists, scan_warning, truncate_summary, FileParseCache, TITLE_MAX_CHARS,
};

const PROVIDER_ID: &str = "kimi";
const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024;
const HEAD_SCAN_LINES: usize = 40;
const TAIL_SCAN_LINES: usize = 20;

#[derive(Debug, Clone, Default)]
struct ParsedWireSummary {
    source_path: PathBuf,
    first_user_prompt: Option<String>,
    last_visible: Option<String>,
    created_at: Option<i64>,
    last_active_at: Option<i64>,
}

static PARSE_CACHE: LazyLock<FileParseCache<ParsedWireSummary>> =
    LazyLock::new(FileParseCache::new);

pub fn session_roots() -> Vec<PathBuf> {
    vec![kimi_home().join("sessions")]
}

pub fn transcript_sources(path: &Path) -> Result<TranscriptSources, String> {
    Ok(vec![(path.to_path_buf(), None)])
}

pub fn scan_sessions() -> Vec<SessionMeta> {
    let home = kimi_home();
    scan_sessions_in_home(&home, &PARSE_CACHE)
}

fn read_bounded_json(path: &Path) -> Option<Value> {
    let meta = fs::symlink_metadata(path).ok()?;
    if is_reparse_or_symlink(&meta) || !meta.file_type().is_file() || meta.len() > MAX_STATE_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn select_transcript_file(session_dir: &Path) -> Option<PathBuf> {
    for rel in [
        Path::new("agents").join("main").join("wire.jsonl"),
        PathBuf::from("context.jsonl"),
        PathBuf::from("wire.jsonl"),
    ] {
        let candidate = session_dir.join(rel);
        if let Ok(meta) = fs::symlink_metadata(&candidate) {
            if !is_reparse_or_symlink(&meta) && meta.file_type().is_file() {
                if candidate == session_dir.join("agents").join("main").join("wire.jsonl") {
                    let agents = session_dir.join("agents");
                    let main_dir = agents.join("main");
                    if fs::symlink_metadata(&agents).is_ok_and(|m| is_reparse_or_symlink(&m))
                        || fs::symlink_metadata(&main_dir).is_ok_and(|m| is_reparse_or_symlink(&m))
                    {
                        continue;
                    }
                }
                return Some(candidate);
            }
        }
    }
    None
}

fn build_resume_command(home: &Path, id: &str, archived: bool) -> Option<String> {
    if archived || !is_safe_session_id(id) {
        return None;
    }
    let custom_env =
        configured_dir("KIMI_SHARE_DIR").is_some() || configured_dir("KIMI_CODE_HOME").is_some();
    if custom_env {
        #[cfg(windows)]
        return Some(format!(
            "$env:KIMI_CODE_HOME = '{}'; kimi --session {id}",
            home.to_string_lossy().replace('\'', "''")
        ));
        #[cfg(not(windows))]
        return Some(format!(
            "env KIMI_CODE_HOME={} kimi --session {id}",
            crate::session_manager::terminal::shell_escape(&home.to_string_lossy())
        ));
    }
    Some(format!("kimi --session {id}"))
}

fn scan_sessions_in_home(
    home: &Path,
    cache: &FileParseCache<ParsedWireSummary>,
) -> Vec<SessionMeta> {
    let sessions_root = home.join("sessions");
    match fs::symlink_metadata(&sessions_root) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                scan_warning(format!(
                    "Kimi 会话根目录是符号链接或重解析点，已跳过：{}",
                    sessions_root.display()
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
                "无法检查 Kimi 会话目录 {}：{err}",
                sessions_root.display()
            ));
            return Vec::new();
        }
    }

    let ws_entries = match fs::read_dir(&sessions_root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            scan_warning(format!(
                "无法扫描 Kimi 目录 {}：{err}",
                sessions_root.display()
            ));
            return Vec::new();
        }
    };

    struct DiscoveredKimiSession {
        id: String,
        transcript: PathBuf,
        state: Option<Value>,
    }

    let mut discovered = Vec::new();
    let mut fallback_files = Vec::new();

    for ws_entry in ws_entries {
        let Ok(ws_entry) = ws_entry else {
            continue;
        };
        let Ok(ws_ft) = ws_entry.file_type() else {
            continue;
        };
        if !ws_ft.is_dir() {
            continue;
        }
        let ws_name = ws_entry.file_name();
        let Some(ws_name_str) = ws_name.to_str() else {
            continue;
        };
        if ws_name_str.starts_with('.') {
            continue;
        }
        let ws_path = ws_entry.path();
        if fs::symlink_metadata(&ws_path).is_ok_and(|m| is_reparse_or_symlink(&m)) {
            continue;
        }

        let Ok(ses_entries) = fs::read_dir(&ws_path) else {
            continue;
        };
        for ses_entry in ses_entries.flatten() {
            if !ses_entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                continue;
            }
            let dir = ses_entry.path();
            if fs::symlink_metadata(&dir).is_ok_and(|m| is_reparse_or_symlink(&m)) {
                continue;
            }
            let Some(id) = ses_entry
                .file_name()
                .to_str()
                .filter(|s| is_safe_session_id(s))
                .map(str::to_string)
            else {
                continue;
            };

            let state = read_bounded_json(&dir.join("state.json")).or_else(|| {
                let meta_dir = dir.join("session-meta");
                if fs::symlink_metadata(&meta_dir).is_ok_and(|m| is_reparse_or_symlink(&m)) {
                    None
                } else {
                    read_bounded_json(&meta_dir.join("state.json"))
                }
            });

            if let Some(ref st) = state {
                if st.pointer("/custom/session_kind").and_then(Value::as_str) == Some("child") {
                    continue;
                }
            }

            let Some(transcript) = select_transcript_file(&dir) else {
                continue;
            };

            let needs_wire_summary = state.as_ref().is_none_or(|st| {
                st.get("title")
                    .and_then(Value::as_str)
                    .is_none_or(|s| s.trim().is_empty())
                    || st
                        .get("lastPrompt")
                        .and_then(Value::as_str)
                        .is_none_or(|s| s.trim().is_empty())
                    || st.get("createdAt").is_none()
            });
            if needs_wire_summary {
                fallback_files.push(transcript.clone());
            }

            discovered.push(DiscoveredKimiSession {
                id,
                transcript,
                state,
            });
        }
    }

    let wire_summaries: HashMap<PathBuf, ParsedWireSummary> = cache
        .scan(fallback_files, parse_wire_summary)
        .into_iter()
        .map(|s| (s.source_path.clone(), s))
        .collect();

    let mut sessions = Vec::with_capacity(discovered.len());
    for item in discovered {
        let fallback = wire_summaries.get(&item.transcript);
        let archived = item
            .state
            .as_ref()
            .and_then(|st| st.get("archived"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let project_dir = item
            .state
            .as_ref()
            .and_then(|st| {
                st.get("cwd")
                    .or_else(|| st.get("workDir"))
                    .or_else(|| st.pointer("/custom/cwd"))
            })
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let project_name = project_dir.as_deref().and_then(path_basename);

        let title = item
            .state
            .as_ref()
            .and_then(|st| st.get("title"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|t| truncate_summary(t, TITLE_MAX_CHARS))
            .or_else(|| {
                fallback
                    .and_then(|f| f.first_user_prompt.as_deref())
                    .map(|t| truncate_summary(t, TITLE_MAX_CHARS))
            })
            .or_else(|| project_name.clone());

        let summary = item
            .state
            .as_ref()
            .and_then(|st| st.get("lastPrompt"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| truncate_summary(s, 160))
            .or_else(|| {
                fallback
                    .and_then(|f| f.last_visible.as_deref().or(f.first_user_prompt.as_deref()))
                    .map(|s| truncate_summary(s, 160))
            });

        let created_at = item
            .state
            .as_ref()
            .and_then(|st| st.get("createdAt"))
            .and_then(parse_timestamp_to_ms)
            .or_else(|| fallback.and_then(|f| f.created_at));
        let last_active_at = item
            .state
            .as_ref()
            .and_then(|st| st.get("updatedAt"))
            .and_then(parse_timestamp_to_ms)
            .or_else(|| fallback.and_then(|f| f.last_active_at))
            .or(created_at);

        let resume_command = build_resume_command(home, &item.id, archived);

        sessions.push(SessionMeta {
            provider_id: PROVIDER_ID.to_string(),
            session_id: item.id,
            residual: false,
            parent_session_id: None,
            archived,
            cleanup_pending: false,
            title,
            summary,
            project_dir,
            project_name,
            sidebar_section: None,
            created_at,
            last_active_at,
            source_path: Some(item.transcript.to_string_lossy().to_string()),
            resume_command,
        });
    }

    sessions
}

fn parse_wire_summary(path: &Path) -> Result<Option<ParsedWireSummary>, String> {
    let (head, tail) = read_head_tail_lines(path, HEAD_SCAN_LINES, TAIL_SCAN_LINES)
        .map_err(|err| format!("无法读取 Kimi 会话 {}：{err}", path.display()))?;
    let mut summary = ParsedWireSummary {
        source_path: path.to_path_buf(),
        ..Default::default()
    };

    for line in head.iter().chain(tail.iter()) {
        let Ok(record) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let ts = record
            .get("time")
            .or_else(|| record.get("timestamp"))
            .and_then(parse_timestamp_to_ms);
        if let Some(t) = ts {
            summary.created_at = Some(summary.created_at.map_or(t, |prev| prev.min(t)));
            summary.last_active_at = Some(summary.last_active_at.map_or(t, |prev| prev.max(t)));
        }

        // Kimi Code envelope or direct context.jsonl message or legacy TurnBegin
        let raw_msg = record
            .pointer("/message/message")
            .or_else(|| record.get("message"))
            .unwrap_or(&record);
        if let Some(role) = raw_msg.get("role").and_then(Value::as_str) {
            if matches!(role, "user" | "assistant") {
                let text = raw_msg
                    .get("content")
                    .map(extract_kimi_visible_text)
                    .unwrap_or_default();
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    if role == "user" && summary.first_user_prompt.is_none() {
                        summary.first_user_prompt = Some(trimmed.to_string());
                    }
                    summary.last_visible = Some(trimmed.to_string());
                }
            }
        } else if record.pointer("/message/type").and_then(Value::as_str) == Some("TurnBegin") {
            if let Some(input) = record.pointer("/message/payload/user_input") {
                let text = extract_kimi_visible_text(input);
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    if summary.first_user_prompt.is_none() {
                        summary.first_user_prompt = Some(trimmed.to_string());
                    }
                    summary.last_visible = Some(trimmed.to_string());
                }
            }
        }
    }

    Ok(Some(summary))
}

fn extract_kimi_visible_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("think" | "thinking" | "reasoning") => None,
                Some("text") => part.get("text").and_then(Value::as_str).map(str::to_string),
                _ => {
                    let t = extract_text(part);
                    (!t.trim().is_empty()).then_some(t)
                }
            })
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => extract_text(content),
        _ => String::new(),
    }
}

fn extract_kimi_think_text(content: &Value) -> String {
    let Value::Array(items) = content else {
        return String::new();
    };
    items
        .iter()
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("think") => part.get("think").and_then(Value::as_str),
            Some("thinking" | "reasoning") => part
                .get("thinking")
                .or_else(|| part.get("text"))
                .and_then(Value::as_str),
            _ => None,
        })
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<&str>>()
        .join("\n")
}

struct BranchSegment {
    start: usize,
    end: usize,
    base: Option<(String, usize)>,
}

/// Resolve `agent.switched` branch switches before projecting messages so undone/forked turns stay hidden.
fn active_record_indices(records: &[Value]) -> Result<Vec<usize>, String> {
    let switches: Vec<_> = records
        .iter()
        .enumerate()
        .filter(|(_, record)| record.get("type").and_then(Value::as_str) == Some("agent.switched"))
        .collect();
    if switches.is_empty() {
        return Ok((0..records.len()).collect());
    }

    let mut segments = HashMap::new();
    let mut branch = "main".to_string();
    let mut start = 0usize;
    let mut base: Option<(String, usize)> = None;

    for (index, record) in switches {
        segments.insert(
            branch,
            BranchSegment {
                start,
                end: index,
                base,
            },
        );
        branch = record
            .get("branch")
            .and_then(Value::as_str)
            .ok_or("Invalid Kimi branch")?
            .to_string();
        let base_branch = record
            .pointer("/base/branch")
            .and_then(Value::as_str)
            .ok_or("Invalid Kimi branch base")?
            .to_string();
        let base_line = record
            .pointer("/base/line")
            .and_then(Value::as_u64)
            .ok_or("Invalid Kimi branch line")? as usize;
        base = Some((base_branch, base_line));
        start = index + 1;
    }
    segments.insert(
        branch.clone(),
        BranchSegment {
            start,
            end: records.len(),
            base,
        },
    );

    let mut chain = Vec::new();
    let mut limit = records.len();
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(branch.clone()) {
            return Err("Kimi branch chain contains a cycle".into());
        }
        let segment = segments.get(&branch).ok_or("Unknown Kimi branch")?;
        chain.push((segment.start, segment.end.min(limit)));
        if let Some((parent, line)) = &segment.base {
            branch = parent.clone();
            limit = *line;
        } else {
            break;
        }
    }

    let mut active = Vec::new();
    for (seg_start, seg_end) in chain.into_iter().rev() {
        if seg_start < seg_end {
            active.extend(seg_start..seg_end);
        }
    }
    Ok(active)
}

fn format_kimi_tool_call(call: &Value) -> Option<(Option<String>, String, String)> {
    let name = call
        .get("name")
        .or_else(|| call.pointer("/function/name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let id = call
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let args = match call
        .get("arguments")
        .or_else(|| call.pointer("/function/arguments"))
    {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(v) if !v.is_null() => v.to_string(),
        _ => String::new(),
    };
    let formatted = if args.is_empty() {
        format!("[Tool: {name}]")
    } else {
        format!("[Tool: {name}]\n{args}")
    };
    Some((id, name, formatted))
}

fn emit_raw_chat_message(
    raw: &Value,
    meta: Option<&Value>,
    ts: Option<i64>,
    call_names: &mut HashMap<String, String>,
    messages: &mut Vec<SessionMessage>,
) {
    let Some(role @ ("user" | "assistant" | "tool" | "system")) =
        raw.get("role").and_then(Value::as_str)
    else {
        return;
    };

    let origin_kind = meta
        .and_then(|m| m.pointer("/origin/kind"))
        .or_else(|| raw.pointer("/origin/kind"))
        .and_then(Value::as_str);

    if matches!(origin_kind, Some("injection" | "system_trigger" | "retry")) {
        return;
    }

    if origin_kind == Some("compaction_summary") {
        let text = raw
            .get("content")
            .map(extract_kimi_visible_text)
            .unwrap_or_default();
        let trimmed = text.trim();
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
        return;
    }

    if role == "tool" {
        let call_id = raw
            .get("toolCallId")
            .or_else(|| raw.get("tool_call_id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let tool_name = call_names.get(call_id).map(String::as_str).unwrap_or("");
        let text = raw.get("content").map(extract_text).unwrap_or_default();
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            let content = if tool_name.is_empty() {
                trimmed.to_string()
            } else {
                format!("[Result: {tool_name}]\n{trimmed}")
            };
            messages.push(SessionMessage {
                role: "tool".to_string(),
                content,
                ts,
            });
        }
        return;
    }

    if let Some(content_val) = raw.get("content") {
        let think = extract_kimi_think_text(content_val);
        if !think.trim().is_empty() {
            messages.push(SessionMessage {
                role: "thinking".to_string(),
                content: think,
                ts,
            });
        }

        let visible = extract_kimi_visible_text(content_val);
        if !visible.trim().is_empty() {
            messages.push(SessionMessage {
                role: role.to_string(),
                content: visible,
                ts,
            });
        }
    }

    if let Some(calls) = raw
        .get("toolCalls")
        .or_else(|| raw.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for call in calls {
            if let Some((cid, name, formatted)) = format_kimi_tool_call(call) {
                if let Some(id) = cid {
                    call_names.insert(id, name);
                }
                messages.push(SessionMessage {
                    role: "tool".to_string(),
                    content: formatted,
                    ts,
                });
            }
        }
    }
}

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    ensure_readable_size(path)?;
    let file = File::open(path).map_err(|e| format!("Failed to open Kimi session file: {e}"))?;
    let mut reader = BufReader::new(file);

    // Preserve physical 0-based line indices (including blank/malformed lines) for `agent.switched` `base.line`.
    let mut records = Vec::new();
    let mut line_buf = Vec::new();
    loop {
        line_buf.clear();
        let n = reader
            .read_until(b'\n', &mut line_buf)
            .map_err(|e| format!("Failed to read Kimi transcript: {e}"))?;
        if n == 0 {
            break;
        }
        // Ignore trailing empty EOF newline, keep actual lines
        let slice = line_buf
            .strip_suffix(b"\n")
            .unwrap_or(&line_buf)
            .strip_suffix(b"\r")
            .unwrap_or_else(|| line_buf.strip_suffix(b"\n").unwrap_or(&line_buf));
        let val = serde_json::from_slice::<Value>(slice).unwrap_or(Value::Null);
        records.push(val);
    }

    let active = active_record_indices(&records)?;
    let has_human = active.iter().any(|&idx| {
        matches!(
            records[idx].get("type").and_then(Value::as_str),
            Some("agent.message.appended" | "human.agent.message.appended")
        )
    });
    let has_context_append = active.iter().any(|&idx| {
        records[idx].get("type").and_then(Value::as_str) == Some("context.append_message")
    });

    let mut messages = Vec::new();
    let mut call_names: HashMap<String, String> = HashMap::new();

    for idx in active {
        let record = &records[idx];
        let ts = record
            .get("time")
            .or_else(|| record.get("timestamp"))
            .and_then(parse_timestamp_to_ms);
        let kind = record.get("type").and_then(Value::as_str).unwrap_or("");

        if has_human
            && matches!(
                kind,
                "agent.message.appended" | "human.agent.message.appended"
            )
        {
            if let Some(raw) = record.pointer("/message/message") {
                emit_raw_chat_message(
                    raw,
                    record.pointer("/message/meta"),
                    ts,
                    &mut call_names,
                    &mut messages,
                );
            }
            continue;
        }

        if !has_human && kind == "context.append_message" {
            if let Some(raw) = record.get("message") {
                emit_raw_chat_message(raw, record.get("meta"), ts, &mut call_names, &mut messages);
            }
            continue;
        }

        // Fallback for Python `kimi-cli` context.jsonl (`role` at top-level) or wire.jsonl (`message.type`)
        if !has_human && !has_context_append {
            if record.get("role").is_some() {
                emit_raw_chat_message(record, None, ts, &mut call_names, &mut messages);
            } else if let Some(msg_type) = record.pointer("/message/type").and_then(Value::as_str) {
                let payload = &record["message"]["payload"];
                match msg_type {
                    "TurnBegin" => {
                        let text = payload
                            .get("user_input")
                            .map(extract_kimi_visible_text)
                            .unwrap_or_default();
                        if !text.trim().is_empty() {
                            messages.push(SessionMessage {
                                role: "user".to_string(),
                                content: text,
                                ts,
                            });
                        }
                    }
                    "ContentPart" => match payload.get("type").and_then(Value::as_str) {
                        Some("think") => {
                            if let Some(t) = payload
                                .get("think")
                                .and_then(Value::as_str)
                                .filter(|s| !s.trim().is_empty())
                            {
                                messages.push(SessionMessage {
                                    role: "thinking".to_string(),
                                    content: t.to_string(),
                                    ts,
                                });
                            }
                        }
                        Some("text") => {
                            if let Some(t) = payload
                                .get("text")
                                .and_then(Value::as_str)
                                .filter(|s| !s.trim().is_empty())
                            {
                                messages.push(SessionMessage {
                                    role: "assistant".to_string(),
                                    content: t.to_string(),
                                    ts,
                                });
                            }
                        }
                        _ => {}
                    },
                    "ToolCall" => {
                        if let Some((cid, name, formatted)) = format_kimi_tool_call(payload) {
                            if let Some(id) = cid {
                                call_names.insert(id, name);
                            }
                            messages.push(SessionMessage {
                                role: "tool".to_string(),
                                content: formatted,
                                ts,
                            });
                        }
                    }
                    "ToolResult" => {
                        let cid = payload
                            .get("tool_call_id")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let name = call_names.get(cid).map(String::as_str).unwrap_or("");
                        let out = payload
                            .pointer("/return_value/output")
                            .or_else(|| payload.pointer("/return_value/message"))
                            .map(extract_text)
                            .unwrap_or_default();
                        if !out.trim().is_empty() {
                            let content = if name.is_empty() {
                                out.trim().to_string()
                            } else {
                                format!("[Result: {name}]\n{}", out.trim())
                            };
                            messages.push(SessionMessage {
                                role: "tool".to_string(),
                                content,
                                ts,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(messages)
}

fn validate_kimi_delete(
    root: &Path,
    path: &Path,
    session_id: &str,
) -> Result<(PathBuf, PathBuf), String> {
    if !is_safe_session_id(session_id) {
        return Err(format!("Invalid Kimi session ID: {session_id}"));
    }
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(format!("Invalid Kimi session path: {}", path.display()));
    }

    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                return Err(format!(
                    "Kimi session source must not be a symlink or reparse point: {}",
                    path.display()
                ));
            }
            if !meta.file_type().is_file() {
                return Err(format!(
                    "Kimi session source is not a regular file: {}",
                    path.display()
                ));
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "Failed to inspect Kimi session {}: {err}",
                path.display()
            ))
        }
    }

    let canonical_root = root.canonicalize().map_err(|err| {
        format!(
            "Failed to resolve Kimi sessions root {}: {err}",
            root.display()
        )
    })?;
    let root_meta = fs::metadata(&canonical_root).map_err(|err| {
        format!(
            "Failed to inspect Kimi sessions root {}: {err}",
            canonical_root.display()
        )
    })?;
    if !root_meta.is_dir() {
        return Err(format!(
            "Kimi sessions root is not a directory: {}",
            canonical_root.display()
        ));
    }

    let resolved_path = checked_storage_child(root, path)?;
    let relative = resolved_path.strip_prefix(&canonical_root).map_err(|_| {
        format!(
            "Kimi session path is outside sessions root: {}",
            path.display()
        )
    })?;
    let parts: Vec<&OsStr> = relative
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    if parts.len() != relative.components().count() || parts.len() < 3 {
        return Err(format!(
            "Unexpected Kimi session layout: {}",
            path.display()
        ));
    }

    let ws_name = parts[0]
        .to_str()
        .ok_or_else(|| format!("Invalid Kimi workspace folder: {}", path.display()))?;
    if ws_name.is_empty() || ws_name.starts_with('.') {
        return Err(format!(
            "Refusing to target Kimi internal directory: {}",
            path.display()
        ));
    }
    if parts[1] != OsStr::new(session_id) {
        return Err(format!(
            "Kimi session directory does not match session ID ({session_id}): {}",
            path.display()
        ));
    }

    let valid_tail = (parts.len() == 5
        && parts[2..]
            == [
                OsStr::new("agents"),
                OsStr::new("main"),
                OsStr::new("wire.jsonl"),
            ])
        || (parts.len() == 3
            && (parts[2] == OsStr::new("wire.jsonl") || parts[2] == OsStr::new("context.jsonl")));
    if !valid_tail {
        return Err(format!(
            "Unexpected Kimi session transcript path: {}",
            path.display()
        ));
    }

    let session_dir = canonical_root.join(ws_name).join(session_id);
    if let Ok(meta) = fs::symlink_metadata(&session_dir) {
        if is_reparse_or_symlink(&meta) || !meta.file_type().is_dir() {
            return Err(format!(
                "Kimi session directory is not a regular directory: {}",
                session_dir.display()
            ));
        }
    }
    let checked_session_dir = checked_storage_child(&canonical_root, &session_dir)?;

    Ok((canonical_root, checked_session_dir))
}

pub fn preflight_delete(root: &Path, path: &Path, session_id: &str) -> Result<(), String> {
    validate_kimi_delete(root, path, session_id).map(|_| ())
}

pub fn delete_session(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    let (canonical_root, session_dir) = validate_kimi_delete(root, path, session_id)?;
    let mut removed = false;

    if fs::symlink_metadata(&session_dir).is_ok() {
        remove_path_if_exists(&session_dir).map_err(|err| {
            format!(
                "Failed to delete Kimi session directory {}: {err}",
                session_dir.display()
            )
        })?;
        removed = true;
    }

    if let Some(ws_dir) = session_dir.parent() {
        let _ = fs::remove_dir(ws_dir);
    }

    if removed {
        if let Err(err) = record_kimi_deletion_markers(&canonical_root, session_id) {
            log::warn!("Failed to update Kimi session index markers after delete: {err}");
        }
    }

    Ok(removed)
}

fn record_kimi_deletion_markers(
    canonical_sessions_root: &Path,
    session_id: &str,
) -> Result<(), String> {
    if let Some(home) = canonical_sessions_root.parent() {
        let index_path = home.join("session_index.jsonl");
        let safe_to_append = match fs::symlink_metadata(&index_path) {
            Ok(meta) => !is_reparse_or_symlink(&meta) && meta.file_type().is_file(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        };
        if safe_to_append {
            let line = serde_json::json!({ "sessionId": session_id, "deleted": true });
            if let Ok(mut file) = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&index_path)
            {
                let _ = writeln!(file, "{line}");
            }
        }
    }

    let dirty_dir = canonical_sessions_root.join(".index-dirty");
    let dirty_ok = match fs::symlink_metadata(&dirty_dir) {
        Ok(meta) => !is_reparse_or_symlink(&meta) && meta.file_type().is_dir(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(&dirty_dir).is_ok()
        }
        Err(_) => false,
    };
    if dirty_ok {
        let mark = dirty_dir.join(format!(
            "{session_id}.{}",
            chrono::Utc::now().timestamp_millis()
        ));
        let _ = fs::write(mark, b"");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[cfg(unix)]
    fn create_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
    }

    #[test]
    #[serial_test::serial]
    #[allow(deprecated)]
    fn scans_kimi_state_skips_child_sessions_and_formats_resume() {
        let prev_share = std::env::var_os("KIMI_SHARE_DIR");
        let prev_code = std::env::var_os("KIMI_CODE_HOME");
        std::env::remove_var("KIMI_SHARE_DIR");
        std::env::remove_var("KIMI_CODE_HOME");

        let home = tempdir().unwrap();
        let dir = home.path().join("sessions/wd_project_123/session-1");
        fs::create_dir_all(dir.join("agents/main")).unwrap();
        fs::write(
            dir.join("state.json"),
            json!({
                "title": "Project Refactor",
                "lastPrompt": "Fix auth flow",
                "cwd": "/work/kimi-proj",
                "createdAt": 1700000000000i64,
                "updatedAt": 1700000005000i64
            })
            .to_string(),
        )
        .unwrap();
        fs::write(dir.join("agents/main/wire.jsonl"), "{}\n").unwrap();

        // Child subagent session must be skipped
        let child_dir = home.path().join("sessions/wd_project_123/session-child");
        fs::create_dir_all(child_dir.join("agents/main")).unwrap();
        fs::write(
            child_dir.join("state.json"),
            json!({"title": "Child", "custom": {"session_kind": "child"}}).to_string(),
        )
        .unwrap();
        fs::write(child_dir.join("agents/main/wire.jsonl"), "{}\n").unwrap();

        let cache = FileParseCache::new();
        let sessions = scan_sessions_in_home(home.path(), &cache);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.provider_id, "kimi");
        assert_eq!(s.session_id, "session-1");
        assert_eq!(s.title.as_deref(), Some("Project Refactor"));
        assert_eq!(s.summary.as_deref(), Some("Fix auth flow"));
        assert_eq!(s.project_dir.as_deref(), Some("/work/kimi-proj"));
        assert_eq!(s.project_name.as_deref(), Some("kimi-proj"));
        assert_eq!(
            s.resume_command.as_deref(),
            Some("kimi --session session-1")
        );

        match prev_share {
            Some(v) => std::env::set_var("KIMI_SHARE_DIR", v),
            None => std::env::remove_var("KIMI_SHARE_DIR"),
        }
        match prev_code {
            Some(v) => std::env::set_var("KIMI_CODE_HOME", v),
            None => std::env::remove_var("KIMI_CODE_HOME"),
        }
    }

    #[test]
    fn load_messages_resolves_branch_switches_and_separates_think_and_tools() {
        let temp = tempdir().unwrap();
        let wire = temp.path().join("wire.jsonl");
        let lines = [
            json!({"type":"metadata","protocol_version":"1.5"}),
            json!({"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"duplicate"}]}}),
            json!({"type":"agent.message.appended","time":1000,"message":{"message":{"role":"user","content":[{"type":"text","text":"Keep prompt"}]}}}),
            json!({"type":"agent.message.appended","time":1001,"message":{"message":{"role":"user","content":[{"type":"text","text":"Abandoned prompt"}]}}}),
            json!({"type":"agent.switched","branch":"fork1","base":{"branch":"main","line":3}}),
            json!({"type":"agent.message.appended","time":1002,"message":{"message":{
                "role":"assistant",
                "content":[
                    {"type":"think","think":"Let me check main.rs"},
                    {"type":"text","text":"Reading file"}
                ],
                "toolCalls":[{"id":"tc1","name":"read_file","arguments":{"path":"main.rs"}}]
            }}}),
            json!({"type":"agent.message.appended","time":1003,"message":{"message":{
                "role":"tool",
                "toolCallId":"tc1",
                "content":[{"type":"text","text":"fn main() {}"}]
            }}}),
            json!({"type":"agent.message.appended","time":1004,"message":{"message":{"role":"user","content":[{"type":"text","text":"injected"}]},"meta":{"origin":{"kind":"injection"}}}}),
            json!({"type":"agent.message.appended","time":1005,"message":{"message":{"role":"assistant","content":[{"type":"text","text":"All set."}]}}}),
        ];
        fs::write(
            &wire,
            lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
        )
        .unwrap();

        let msgs = load_messages(&wire).unwrap();
        let pairs: Vec<(&str, &str)> = msgs
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("user", "Keep prompt"),
                ("thinking", "Let me check main.rs"),
                ("assistant", "Reading file"),
                ("tool", "[Tool: read_file]\n{\"path\":\"main.rs\"}"),
                ("tool", "[Result: read_file]\nfn main() {}"),
                ("assistant", "All set."),
            ]
        );
    }

    #[test]
    fn delete_session_removes_dir_writes_tombstone_and_rejects_symlinks_or_bad_layout() {
        let home = tempdir().unwrap();
        let root = home.path().join("sessions");
        let dir = root.join("wd_project_123/session_abc-1");
        let sibling = root.join("wd_project_123/session_keep");
        fs::create_dir_all(dir.join("agents/main")).unwrap();
        fs::create_dir_all(sibling.join("agents/main")).unwrap();
        let wire = dir.join("agents/main/wire.jsonl");
        fs::write(&wire, "{}\n").unwrap();
        fs::write(sibling.join("agents/main/wire.jsonl"), "{}\n").unwrap();

        // Subagent wire path must be rejected
        fs::create_dir_all(dir.join("agents/sub")).unwrap();
        let sub_wire = dir.join("agents/sub/wire.jsonl");
        fs::write(&sub_wire, "{}\n").unwrap();
        assert!(preflight_delete(&root, &sub_wire, "session_abc-1").is_err());
        assert!(preflight_delete(&root, &wire, "session_other").is_err());
        assert!(preflight_delete(&root, &wire, "../session_abc-1").is_err());

        // Symlinked session directory must be rejected
        let outside = home.path().join("outside-session");
        fs::create_dir_all(outside.join("agents/main")).unwrap();
        fs::write(outside.join("agents/main/wire.jsonl"), "{}\n").unwrap();
        let linked = root.join("wd_project_123/linked_sess");
        if create_dir_symlink(&outside, &linked).is_ok() {
            let linked_wire = linked.join("agents/main/wire.jsonl");
            assert!(preflight_delete(&root, &linked_wire, "linked_sess").is_err());
            assert!(delete_session(&root, &linked_wire, "linked_sess").is_err());
            assert!(outside.exists());
            let _ = remove_path_if_exists(&linked);
        }

        preflight_delete(&root, &wire, "session_abc-1").unwrap();
        assert!(delete_session(&root, &wire, "session_abc-1").unwrap());
        assert!(!dir.exists());
        assert!(sibling.exists());

        let index = fs::read_to_string(home.path().join("session_index.jsonl")).unwrap();
        let last: Value = serde_json::from_str(index.lines().last().unwrap()).unwrap();
        assert_eq!(last, json!({"sessionId": "session_abc-1", "deleted": true}));
        assert!(root.join(".index-dirty").exists());
    }
}
