use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use serde_json::Value;

use crate::session_manager::cache::TranscriptSources;
use crate::session_manager::content::parse_file_url;
use crate::session_manager::model::{
    ContentRef, DiffOp, EventKind, ImageRef, ImageSource, MessageMeta, SessionBlock, ToolStatus,
};
use crate::session_manager::{SessionMessage, SessionMeta};
use crate::session_paths::copilot_home;

use super::blocks::{
    assign_turn_ids, count_diff_lines, parse_arguments, single_file_diff, summary_event_block,
    thinking_block, tool_call_block, tool_result_block, ToolSource,
};
use super::utils::{
    checked_storage_child, extract_text, for_each_jsonl_value, is_reparse_or_symlink,
    is_safe_session_id, parse_timestamp_to_ms, path_basename, read_head_tail_lines,
    remove_path_if_exists, scan_warning, truncate_summary, FileParseCache, TITLE_MAX_CHARS,
};

const PROVIDER_ID: &str = "copilot";
const HEAD_SCAN_LINES: usize = 64;
const TAIL_SCAN_LINES: usize = 16;
const MAX_WORKSPACE_YAML_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
struct ParsedEventsMeta {
    source_path: String,
    fallback_id: String,
    event_session_id: Option<String>,
    cwd: Option<String>,
    created_at: Option<i64>,
    first_prompt: Option<String>,
    last_event_at: Option<i64>,
    is_dir_layout: bool,
}

#[derive(Debug, Default)]
struct WorkspaceYaml {
    id: Option<String>,
    cwd: Option<String>,
    name: Option<String>,
    summary: Option<String>,
    created_at: Option<i64>,
    updated_at: Option<i64>,
}

static PARSE_CACHE: LazyLock<FileParseCache<ParsedEventsMeta>> = LazyLock::new(FileParseCache::new);

pub fn session_roots() -> Vec<PathBuf> {
    vec![copilot_home().join("session-state")]
}

#[allow(dead_code)]
pub fn transcript_sources(path: &Path) -> Result<TranscriptSources, String> {
    Ok(vec![(path.to_path_buf(), None)])
}

pub fn scan_sessions() -> Vec<SessionMeta> {
    let root = copilot_home().join("session-state");
    scan_sessions_in_root(&root, &PARSE_CACHE)
}

fn scan_sessions_in_root(
    root: &Path,
    cache: &FileParseCache<ParsedEventsMeta>,
) -> Vec<SessionMeta> {
    match fs::symlink_metadata(root) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                scan_warning(format!(
                    "Copilot 会话根目录是符号链接或重解析点，已跳过：{}",
                    root.display()
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
                "无法检查 Copilot 会话目录 {}：{err}",
                root.display()
            ));
            return Vec::new();
        }
    }

    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            scan_warning(format!("无法扫描 Copilot 目录 {}：{err}", root.display()));
            return Vec::new();
        }
    };

    let mut files = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                scan_warning(format!("无法枚举 {}：{err}", root.display()));
                continue;
            }
        };
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let entry_path = entry.path();
        if fs::symlink_metadata(&entry_path).is_ok_and(|m| is_reparse_or_symlink(&m)) {
            continue;
        }

        if ft.is_dir() {
            let events_path = entry_path.join("events.jsonl");
            if fs::symlink_metadata(&events_path)
                .is_ok_and(|m| !is_reparse_or_symlink(&m) && m.file_type().is_file())
            {
                files.push(events_path);
            }
        } else if ft.is_file() && entry_path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            if let Some(stem) = entry_path.file_stem() {
                let dir_events = root.join(stem).join("events.jsonl");
                if dir_events.exists() {
                    continue;
                }
            }
            files.push(entry_path);
        }
    }

    let parsed = cache.scan(files, parse_events_meta);
    parsed
        .into_iter()
        .map(|events| {
            let ws = if events.is_dir_layout {
                Path::new(&events.source_path)
                    .parent()
                    .map(read_workspace_yaml)
                    .unwrap_or_default()
            } else {
                WorkspaceYaml::default()
            };
            build_session_meta(events, ws)
        })
        .collect()
}

/// Clean built-in flat YAML key-value parser for Copilot CLI's `workspace.yaml`.
/// Supports single/double-quoted scalars, unquoted scalars with inline `#` comments,
/// `null`/`~`, and indented block scalars (`|`, `|-`, `>`, `>-`).
pub(crate) fn parse_flat_yaml(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut idx = 0usize;

    while idx < lines.len() {
        let line = lines[idx];
        idx += 1;

        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "---" || trimmed == "..." {
            continue;
        }
        // Ignore indented lines belonging to nested maps we don't care about
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }

        let Some((raw_key, raw_rest)) = line.split_once(':') else {
            continue;
        };
        let key = raw_key.trim();
        if key.is_empty() {
            continue;
        }
        // In YAML, `key:value` without space after `:` is only a mapping if `raw_rest` is empty
        // or starts with whitespace.
        if !raw_rest.is_empty() && !raw_rest.starts_with([' ', '\t']) {
            continue;
        }

        let val_trimmed = raw_rest.trim();
        if matches!(val_trimmed, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
            let folded = val_trimmed.starts_with('>');
            let mut block_lines: Vec<&str> = Vec::new();
            let mut min_indent: Option<usize> = None;
            while idx < lines.len() {
                let next_line = lines[idx];
                if next_line.trim().is_empty() {
                    block_lines.push("");
                    idx += 1;
                    continue;
                }
                let indent = next_line.bytes().take_while(|&b| b == b' ').count();
                if indent == 0 {
                    break;
                }
                min_indent = Some(min_indent.map_or(indent, |prev| prev.min(indent)));
                block_lines.push(next_line);
                idx += 1;
            }
            let strip = min_indent.unwrap_or(0);
            let stripped: Vec<&str> = block_lines
                .into_iter()
                .map(|l| if l.len() >= strip { &l[strip..] } else { "" })
                .collect();
            let joined = if folded {
                stripped.join(" ")
            } else {
                stripped.join("\n")
            };
            let cleaned = joined.trim();
            if !cleaned.is_empty() {
                map.insert(key.to_string(), cleaned.to_string());
            }
            continue;
        }

        if let Some(parsed_val) = parse_yaml_scalar(val_trimmed) {
            if !parsed_val.is_empty() {
                map.insert(key.to_string(), parsed_val);
            }
        }
    }

    map
}

fn parse_yaml_scalar(raw: &str) -> Option<String> {
    if raw.is_empty() || matches!(raw, "~" | "null" | "Null" | "NULL") {
        return None;
    }
    if raw.starts_with('"') {
        let mut escaped = false;
        let mut close_idx = None;
        for (i, ch) in raw.char_indices().skip(1) {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                close_idx = Some(i);
                break;
            }
        }
        if let Some(end_quote) = close_idx {
            let quoted_slice = &raw[..=end_quote];
            if let Ok(unescaped) = serde_json::from_str::<String>(quoted_slice) {
                return Some(unescaped.trim().to_string());
            }
            let inner = &quoted_slice[1..quoted_slice.len() - 1];
            let mut out = String::with_capacity(inner.len());
            let mut chars = inner.chars();
            while let Some(ch) = chars.next() {
                if ch == '\\' {
                    match chars.next() {
                        Some('n') => out.push('\n'),
                        Some('r') => out.push('\r'),
                        Some('t') => out.push('\t'),
                        Some('\\') => out.push('\\'),
                        Some('"') => out.push('"'),
                        Some(other) => {
                            out.push('\\');
                            out.push(other);
                        }
                        None => out.push('\\'),
                    }
                } else {
                    out.push(ch);
                }
            }
            return Some(out.trim().to_string());
        }
    }
    if let Some(after_open) = raw.strip_prefix('\'') {
        let mut out = String::with_capacity(after_open.len());
        let mut chars = after_open.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\'' {
                if chars.peek() == Some(&'\'') {
                    let _ = chars.next();
                    out.push('\'');
                } else {
                    return Some(out.trim().to_string());
                }
            } else {
                out.push(ch);
            }
        }
    }

    // Unquoted YAML scalar: strip inline `#` comment when preceded by whitespace
    let mut end = raw.len();
    for (i, ch) in raw.char_indices() {
        if ch == '#' && i > 0 && raw.as_bytes()[i - 1].is_ascii_whitespace() {
            end = i;
            break;
        }
    }
    let cleaned = raw[..end].trim();
    if matches!(cleaned, "~" | "null" | "Null" | "NULL" | "") {
        None
    } else {
        Some(cleaned.to_string())
    }
}

fn read_workspace_yaml(dir: &Path) -> WorkspaceYaml {
    let yaml_path = dir.join("workspace.yaml");
    let Ok(meta) = fs::symlink_metadata(&yaml_path) else {
        return WorkspaceYaml::default();
    };
    if is_reparse_or_symlink(&meta)
        || !meta.file_type().is_file()
        || meta.len() > MAX_WORKSPACE_YAML_BYTES
    {
        return WorkspaceYaml::default();
    }
    let mut raw = String::new();
    let Ok(file) = File::open(&yaml_path) else {
        return WorkspaceYaml::default();
    };
    if file
        .take(MAX_WORKSPACE_YAML_BYTES + 1)
        .read_to_string(&mut raw)
        .is_err()
    {
        return WorkspaceYaml::default();
    }
    let kv = parse_flat_yaml(&raw);
    let get_str = |k: &str| kv.get(k).cloned().filter(|s| !s.is_empty());
    let get_ts = |k: &str| {
        kv.get(k)
            .and_then(|v| parse_timestamp_to_ms(&Value::String(v.clone())))
    };
    WorkspaceYaml {
        id: get_str("id"),
        cwd: get_str("cwd"),
        name: get_str("name"),
        summary: get_str("summary"),
        created_at: get_ts("created_at"),
        updated_at: get_ts("updated_at"),
    }
}

fn parse_events_meta(path: &Path) -> Result<Option<ParsedEventsMeta>, String> {
    let is_dir_layout = path.file_name() == Some(OsStr::new("events.jsonl"));
    let fallback_id = if is_dir_layout {
        path.parent()
            .and_then(|d| d.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_string)
    } else {
        path.file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
    };
    let Some(fallback_id) = fallback_id.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };

    let (head, tail) = read_head_tail_lines(path, HEAD_SCAN_LINES, TAIL_SCAN_LINES)
        .map_err(|err| format!("无法读取 Copilot 会话 {}：{err}", path.display()))?;

    let mut event_session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut created_at: Option<i64> = None;
    let mut first_prompt: Option<String> = None;

    for line in &head {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if created_at.is_none() {
            created_at = event.get("timestamp").and_then(parse_timestamp_to_ms);
        }
        match event.get("type").and_then(Value::as_str) {
            Some("session.start") => {
                let data = &event["data"];
                if event_session_id.is_none() {
                    event_session_id = data
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                }
                if cwd.is_none() {
                    cwd = data
                        .pointer("/context/cwd")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                }
                if let Some(start_ts) = data.get("startTime").and_then(parse_timestamp_to_ms) {
                    created_at = Some(start_ts);
                }
            }
            Some("user.message")
                if first_prompt.is_none()
                    && event
                        .pointer("/data/parentToolCallId")
                        .is_none_or(Value::is_null) =>
            {
                first_prompt = event
                    .pointer("/data/content")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
            }
            _ => {}
        }
    }

    let last_event_at = tail.iter().rev().chain(head.iter().rev()).find_map(|line| {
        serde_json::from_str::<Value>(line.trim())
            .ok()
            .and_then(|event| event.get("timestamp").and_then(parse_timestamp_to_ms))
    });

    Ok(Some(ParsedEventsMeta {
        source_path: path.to_string_lossy().to_string(),
        fallback_id,
        event_session_id,
        cwd,
        created_at,
        first_prompt,
        last_event_at,
        is_dir_layout,
    }))
}

fn build_session_meta(events: ParsedEventsMeta, ws: WorkspaceYaml) -> SessionMeta {
    let session_id = ws
        .id
        .or(events.event_session_id)
        .unwrap_or(events.fallback_id);
    let project_dir = ws.cwd.or(events.cwd);
    let project_name = project_dir.as_deref().and_then(path_basename);
    let created_at = ws.created_at.or(events.created_at);
    let last_active_at = events.last_event_at.max(ws.updated_at).or(created_at);

    let title = ws
        .name
        .or_else(|| events.first_prompt.clone())
        .map(|t| truncate_summary(&t, TITLE_MAX_CHARS))
        .filter(|t| !t.is_empty());
    let summary = ws
        .summary
        .or(events.first_prompt)
        .map(|t| truncate_summary(&t, 160))
        .filter(|t| !t.is_empty());

    SessionMeta {
        provider_id: PROVIDER_ID.to_string(),
        session_id,
        residual: false,
        parent_session_id: None,
        archived: false,
        cleanup_pending: false,
        title,
        summary,
        project_dir,
        project_name,
        sidebar_section: None,
        created_at,
        last_active_at,
        source_path: Some(events.source_path),
    }
}

#[derive(Default, Clone)]
struct PendingUsage {
    model: Option<String>,
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    duration_ms: Option<u64>,
}

impl PendingUsage {
    fn of(usage: &Value) -> Self {
        let cache_read = usage["cacheReadTokens"].as_u64();
        Self {
            model: usage["model"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            input: usage["inputTokens"]
                .as_u64()
                .map(|input| input.saturating_sub(cache_read.unwrap_or(0))),
            output: usage["outputTokens"].as_u64(),
            cache_read,
            cache_write: usage["cacheWriteTokens"].as_u64(),
            duration_ms: usage["duration"].as_u64(),
        }
    }
}

fn image_media_type_from_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

fn attachment_image_ref(item: &Value) -> Option<ImageRef> {
    let raw_path = ["path", "filePath", "file_path", "uri", "url"]
        .into_iter()
        .find_map(|key| item.get(key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let fs_path = if raw_path.starts_with("file://") {
        parse_file_url(raw_path).ok()?
    } else {
        PathBuf::from(raw_path)
    };
    if !fs_path.is_absolute() {
        return None;
    }
    let media_type = image_media_type_from_path(&fs_path)?;
    let size = fs::metadata(&fs_path)
        .ok()
        .map(|m| u32::try_from(m.len()).unwrap_or(u32::MAX))
        .unwrap_or(0);
    let alt = ["displayName", "name", "filename"]
        .into_iter()
        .find_map(|key| item.get(key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(ImageRef {
        source: ImageSource::LocalFile {
            path: raw_path.to_string(),
        },
        media_type: media_type.to_string(),
        size,
        alt,
    })
}

fn looks_like_unified_diff(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("diff --git ")
        || (trimmed.starts_with("--- ") && text.contains("\n+++ "))
        || trimmed.starts_with("@@ ")
        || text.contains("\n@@ ")
}

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    let mut messages: Vec<SessionMessage> = Vec::new();
    let mut pending: Option<PendingUsage> = None;
    let mut ignored_call_ids: HashSet<String> = HashSet::new();
    let mut tool_start_ts: HashMap<String, i64> = HashMap::new();
    let mut call_locations: HashMap<String, (usize, usize)> = HashMap::new();

    for_each_jsonl_value(path, |span, event| {
        let data = &event["data"];
        if data.get("parentToolCallId").is_some_and(|v| !v.is_null()) {
            return Ok(());
        }
        let ts = event.get("timestamp").and_then(parse_timestamp_to_ms);
        let reference = |pointer: String| -> Option<ContentRef> { Some(span.content_ref(pointer)) };

        match event.get("type").and_then(Value::as_str) {
            Some("user.message") => {
                let mut user_blocks = Vec::new();
                if let Some(text) = data.get("content").and_then(Value::as_str) {
                    if !text.trim().is_empty() {
                        user_blocks.push(SessionBlock::text(text));
                    }
                }
                for attachment in data
                    .get("attachments")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(image) = attachment_image_ref(attachment) {
                        user_blocks.push(SessionBlock::Image { image });
                    }
                }
                if user_blocks.is_empty() {
                    return Ok(());
                }
                let mut message = SessionMessage::from_blocks("user", ts, user_blocks);
                message.id = data
                    .get("messageId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                messages.push(message);
            }
            Some("session.usage_record") => {
                pending = Some(PendingUsage::of(&data["usage"]));
            }
            Some("assistant.message") => {
                let mut message_blocks = Vec::new();
                if let Some(reasoning) = data.get("reasoningText").and_then(Value::as_str) {
                    if !reasoning.trim().is_empty() {
                        message_blocks.push(thinking_block(reasoning, None, None, || {
                            reference("/data/reasoningText".into())
                        }));
                    }
                }
                if let Some(text) = data.get("content").and_then(Value::as_str) {
                    if !text.trim().is_empty() {
                        message_blocks.push(SessionBlock::text(text));
                    }
                }
                let mut local_call_indices: Vec<(String, usize)> = Vec::new();
                for (i, call) in data
                    .get("toolRequests")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    let name = call.get("name").and_then(Value::as_str).unwrap_or_default();
                    let call_id = call
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if name == "report_intent" {
                        if !call_id.is_empty() {
                            ignored_call_ids.insert(call_id.to_string());
                        }
                        continue;
                    }
                    if name.is_empty() {
                        continue;
                    }
                    let input = parse_arguments(&call["arguments"]);
                    let block_idx = message_blocks.len();
                    message_blocks.push(tool_call_block(
                        ToolSource::Generic,
                        call_id,
                        name,
                        &input,
                        || reference(format!("/data/toolRequests/{i}/arguments")),
                    ));
                    if !call_id.is_empty() {
                        local_call_indices.push((call_id.to_string(), block_idx));
                    }
                }
                if message_blocks.is_empty() {
                    return Ok(());
                }
                let mut message = SessionMessage::from_blocks("assistant", ts, message_blocks);
                message.id = data
                    .get("messageId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let usage = pending.take().unwrap_or_default();
                let model = data
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .or(usage.model);
                let output = usage
                    .output
                    .or_else(|| data.get("outputTokens").and_then(Value::as_u64));
                message.meta = MessageMeta {
                    model,
                    input_tokens: usage.input,
                    output_tokens: output,
                    cache_read_tokens: usage.cache_read,
                    cache_write_tokens: usage.cache_write,
                    duration_ms: usage.duration_ms,
                    ..MessageMeta::default()
                }
                .into_option();
                let msg_idx = messages.len();
                for (cid, bidx) in local_call_indices {
                    call_locations.insert(cid, (msg_idx, bidx));
                }
                messages.push(message);
            }
            Some("tool.execution_start") => {
                let call_id = data
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let tool_name = data
                    .get("toolName")
                    .or_else(|| data.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if tool_name == "report_intent" {
                    if !call_id.is_empty() {
                        ignored_call_ids.insert(call_id.to_string());
                    }
                    return Ok(());
                }
                if !call_id.is_empty() {
                    if let Some(start_ms) = ts {
                        tool_start_ts.insert(call_id.to_string(), start_ms);
                    }
                }
            }
            Some("tool.execution_complete") => {
                let call_id = data
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if ignored_call_ids.contains(call_id)
                    || (data
                        .pointer("/result/detailedContent")
                        .and_then(Value::as_str)
                        .is_some()
                        && data.pointer("/result/content").and_then(Value::as_str)
                            == Some("Intent logged"))
                {
                    return Ok(());
                }

                let start_ms = tool_start_ts.remove(call_id);
                let duration_ms = data
                    .get("duration")
                    .or_else(|| data.get("durationMs"))
                    .and_then(Value::as_u64)
                    .or_else(|| match (ts, start_ms) {
                        (Some(end), Some(start)) if end >= start => Some((end - start) as u64),
                        _ => None,
                    });

                if let Some(&(msg_idx, block_idx)) = call_locations.get(call_id) {
                    let diff_candidate = data
                        .pointer("/result/diff")
                        .and_then(Value::as_str)
                        .map(|d| (d, "/data/result/diff"))
                        .or_else(|| {
                            data.pointer("/result/detailedContent")
                                .and_then(Value::as_str)
                                .filter(|d| looks_like_unified_diff(d))
                                .map(|d| (d, "/data/result/detailedContent"))
                        });
                    if let Some((diff_text, diff_ptr)) = diff_candidate {
                        if let Some(SessionBlock::ToolCall { diff, title, .. }) = messages
                            .get_mut(msg_idx)
                            .and_then(|m| m.blocks.get_mut(block_idx))
                        {
                            let (added, removed) = count_diff_lines(diff_text);
                            let path_str = data
                                .pointer("/result/path")
                                .or_else(|| data.pointer("/result/filePath"))
                                .and_then(Value::as_str)
                                .unwrap_or(title.as_str())
                                .to_string();
                            let op = diff
                                .as_ref()
                                .and_then(|d| d.files.first())
                                .map_or(DiffOp::Update, |f| f.op);
                            let mut summary = single_file_diff(&path_str, op, added, removed);
                            summary.full = reference(diff_ptr.into());
                            *diff = Some(summary);
                        }
                    }
                }

                let success = data.get("success").and_then(Value::as_bool).unwrap_or(true);
                let (text, pointer) = if success {
                    if let Some(content_val) = data.pointer("/result/content") {
                        (extract_text(content_val), "/data/result/content")
                    } else if let Some(detailed) = data
                        .pointer("/result/detailedContent")
                        .and_then(Value::as_str)
                    {
                        (detailed.to_string(), "/data/result/detailedContent")
                    } else {
                        (String::new(), "/data/result/content")
                    }
                } else if let Some(err_msg) = data.pointer("/error/message").and_then(Value::as_str)
                {
                    (err_msg.to_string(), "/data/error/message")
                } else if let Some(err_val) = data.get("error") {
                    (extract_text(err_val), "/data/error")
                } else {
                    (String::new(), "/data/error/message")
                };

                let status = if success {
                    ToolStatus::Success
                } else {
                    ToolStatus::Error
                };
                let mut block =
                    tool_result_block(call_id, status, &text, || reference(pointer.into()));
                if let SessionBlock::ToolResult {
                    duration_ms: ref mut d_slot,
                    exit_code: ref mut code_slot,
                    ..
                } = block
                {
                    *d_slot = duration_ms;
                    *code_slot = data
                        .pointer("/result/exitCode")
                        .or_else(|| data.get("exitCode"))
                        .and_then(Value::as_i64)
                        .and_then(|c| i32::try_from(c).ok());
                }
                messages.push(SessionMessage::from_blocks("tool", ts, vec![block]));
            }
            Some("session.model_change") => {
                let model = data
                    .get("newModel")
                    .or_else(|| data.get("model"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let mut message = SessionMessage::from_blocks(
                    "system",
                    ts,
                    vec![SessionBlock::event(EventKind::ModelChange, model, None)],
                );
                message.injected = true;
                messages.push(message);
            }
            Some("session.compaction_complete") => {
                let (summary, ptr) =
                    if let Some(s) = data.get("summaryContent").and_then(Value::as_str) {
                        (Some(s), "/data/summaryContent")
                    } else if let Some(s) = data.get("summary").and_then(Value::as_str) {
                        (Some(s), "/data/summary")
                    } else {
                        (None, "/data/summaryContent")
                    };
                let block = match summary.filter(|s| !s.trim().is_empty()) {
                    Some(text) => {
                        summary_event_block(EventKind::Compaction, text, || reference(ptr.into()))
                    }
                    None => SessionBlock::event(EventKind::Compaction, None, None),
                };
                let mut message = SessionMessage::from_blocks("system", ts, vec![block]);
                message.injected = true;
                messages.push(message);
            }
            Some("abort" | "session.abort" | "turn.aborted") => {
                let reason = data
                    .get("reason")
                    .or_else(|| data.get("message"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                messages.push(SessionMessage::from_blocks(
                    "system",
                    ts,
                    vec![SessionBlock::event(EventKind::Aborted, reason, None)],
                ));
            }
            _ => {}
        }
        Ok(())
    })?;

    assign_turn_ids(&mut messages);
    Ok(messages)
}

#[derive(Debug)]
enum CopilotDeleteTarget {
    Directory {
        session_dir: PathBuf,
        legacy_file: Option<PathBuf>,
    },
    LegacyFile {
        file_path: PathBuf,
    },
}

fn validate_copilot_delete(
    root: &Path,
    path: &Path,
    session_id: &str,
) -> Result<CopilotDeleteTarget, String> {
    if !is_safe_session_id(session_id) {
        return Err(format!("Invalid Copilot session ID: {session_id}"));
    }
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(format!("Invalid Copilot session path: {}", path.display()));
    }

    let file_exists = match fs::symlink_metadata(path) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                return Err(format!(
                    "Copilot session source must not be a symlink or reparse point: {}",
                    path.display()
                ));
            }
            if !meta.file_type().is_file() {
                return Err(format!(
                    "Copilot session source is not a regular file: {}",
                    path.display()
                ));
            }
            true
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            return Err(format!(
                "Failed to inspect Copilot session {}: {err}",
                path.display()
            ))
        }
    };

    let canonical_root = root.canonicalize().map_err(|err| {
        format!(
            "Failed to resolve Copilot session-state root {}: {err}",
            root.display()
        )
    })?;
    let root_meta = fs::metadata(&canonical_root).map_err(|err| {
        format!(
            "Failed to inspect Copilot session-state root {}: {err}",
            canonical_root.display()
        )
    })?;
    if !root_meta.is_dir() {
        return Err(format!(
            "Copilot session-state root is not a directory: {}",
            canonical_root.display()
        ));
    }

    let resolved_path = checked_storage_child(root, path)?;
    let relative = resolved_path.strip_prefix(&canonical_root).map_err(|_| {
        format!(
            "Copilot session path is outside session-state root: {}",
            path.display()
        )
    })?;
    let components: Vec<_> = relative.components().collect();

    if components.len() == 2
        && matches!(components[0], Component::Normal(dir_name) if dir_name == OsStr::new(session_id))
        && matches!(components[1], Component::Normal(fname) if fname == OsStr::new("events.jsonl"))
    {
        let session_dir = resolved_path
            .parent()
            .ok_or_else(|| format!("Invalid Copilot session directory: {}", path.display()))?
            .to_path_buf();
        match fs::symlink_metadata(&session_dir) {
            Ok(meta) => {
                if is_reparse_or_symlink(&meta) {
                    return Err(format!(
                        "Copilot session directory must not be a symlink or reparse point: {}",
                        session_dir.display()
                    ));
                }
                if !meta.file_type().is_dir() {
                    return Err(format!(
                        "Copilot session directory is not a directory: {}",
                        session_dir.display()
                    ));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(format!(
                    "Failed to inspect Copilot session directory {}: {err}",
                    session_dir.display()
                ))
            }
        }

        if file_exists {
            let parsed = parse_events_meta(&resolved_path)?
                .ok_or_else(|| format!("Failed to parse Copilot session: {}", path.display()))?;
            let ws = read_workspace_yaml(&session_dir);
            if let Some(ws_id) = ws.id.as_deref() {
                if ws_id != session_id {
                    return Err(format!(
                        "Copilot workspace.yaml ID ({ws_id}) does not match {session_id}"
                    ));
                }
            }
            if let Some(ev_id) = parsed.event_session_id.as_deref() {
                if ev_id != session_id {
                    return Err(format!(
                        "Copilot events.jsonl sessionId ({ev_id}) does not match {session_id}"
                    ));
                }
            }
        }

        let checked_dir = checked_storage_child(&canonical_root, &session_dir)?;
        let legacy_candidate = canonical_root.join(format!("{session_id}.jsonl"));
        let legacy_file = match fs::symlink_metadata(&legacy_candidate) {
            Ok(meta) if !is_reparse_or_symlink(&meta) && meta.file_type().is_file() => {
                Some(checked_storage_child(&canonical_root, &legacy_candidate)?)
            }
            Ok(meta) if is_reparse_or_symlink(&meta) => {
                return Err(format!(
                    "Refusing to delete Copilot session with symlinked legacy file: {}",
                    legacy_candidate.display()
                ))
            }
            _ => None,
        };

        return Ok(CopilotDeleteTarget::Directory {
            session_dir: checked_dir,
            legacy_file,
        });
    }

    let expected_legacy = format!("{session_id}.jsonl");
    if components.len() == 1
        && matches!(components[0], Component::Normal(fname) if fname == OsStr::new(&expected_legacy))
    {
        if file_exists {
            let parsed = parse_events_meta(&resolved_path)?
                .ok_or_else(|| format!("Failed to parse Copilot session: {}", path.display()))?;
            if let Some(ev_id) = parsed.event_session_id.as_deref() {
                if ev_id != session_id {
                    return Err(format!(
                        "Copilot legacy session ID ({ev_id}) does not match {session_id}"
                    ));
                }
            }
        }
        return Ok(CopilotDeleteTarget::LegacyFile {
            file_path: resolved_path,
        });
    }

    Err(format!(
        "Unexpected Copilot session layout for {session_id}: {}",
        path.display()
    ))
}

#[allow(dead_code)]
pub fn preflight_delete(root: &Path, path: &Path, session_id: &str) -> Result<(), String> {
    validate_copilot_delete(root, path, session_id).map(|_| ())
}

pub fn delete_session(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    let target = validate_copilot_delete(root, path, session_id)?;
    let mut removed = false;
    match target {
        CopilotDeleteTarget::Directory {
            session_dir,
            legacy_file,
        } => {
            if let Some(legacy) = legacy_file {
                if fs::symlink_metadata(&legacy).is_ok() {
                    remove_path_if_exists(&legacy).map_err(|err| {
                        format!(
                            "Failed to remove legacy Copilot session file {}: {err}",
                            legacy.display()
                        )
                    })?;
                    removed = true;
                }
            }
            if fs::symlink_metadata(&session_dir).is_ok() {
                remove_path_if_exists(&session_dir).map_err(|err| {
                    format!(
                        "Failed to remove Copilot session directory {}: {err}",
                        session_dir.display()
                    )
                })?;
                removed = true;
            }
        }
        CopilotDeleteTarget::LegacyFile { file_path } => {
            if fs::symlink_metadata(&file_path).is_ok() {
                remove_path_if_exists(&file_path).map_err(|err| {
                    format!(
                        "Failed to remove Copilot session file {}: {err}",
                        file_path.display()
                    )
                })?;
                removed = true;
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn write_events(path: &Path, events: &[Value]) {
        let body: String = events.iter().map(|e| format!("{e}\n")).collect();
        fs::write(path, body).unwrap();
    }

    #[cfg(unix)]
    fn create_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
    }

    #[test]
    fn flat_yaml_parser_handles_quotes_comments_windows_paths_and_blocks() {
        let yaml = concat!(
            "---\n",
            "id: '0cb916db-0000-4000-8000-000000000001'\n",
            "cwd: \"E:\\\\Work\\\\My#Project\"\n",
            "name: 'User''s Build Fix' # inline comment\n",
            "empty_key: ~\n",
            "summary: |-\n",
            "  Line one\n",
            "  Line two\n",
            "created_at: 2026-10-10T09:07:56.720Z\n",
        );
        let map = parse_flat_yaml(yaml);
        assert_eq!(
            map.get("id").map(String::as_str),
            Some("0cb916db-0000-4000-8000-000000000001")
        );
        assert_eq!(
            map.get("cwd").map(String::as_str),
            Some("E:\\Work\\My#Project")
        );
        assert_eq!(
            map.get("name").map(String::as_str),
            Some("User's Build Fix")
        );
        assert!(!map.contains_key("empty_key"));
        assert_eq!(
            map.get("summary").map(String::as_str),
            Some("Line one\nLine two")
        );
    }

    #[test]
    fn scans_workspace_yaml_and_falls_back_to_session_start_and_legacy_file() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("session-state");
        let id1 = "0cb916db-0000-4000-8000-000000000001";
        let dir1 = root.join(id1);
        fs::create_dir_all(&dir1).unwrap();
        let events1 = dir1.join("events.jsonl");
        write_events(
            &events1,
            &[
                json!({"type":"session.start","timestamp":"2026-10-10T09:07:56.748Z","data":{"sessionId":id1,"startTime":"2026-10-10T09:07:56.720Z","context":{"cwd":"/work/project"}}}),
                json!({"type":"user.message","timestamp":"2026-10-10T09:08:00.043Z","data":{"content":"Fix the build","messageId":"u1"}}),
            ],
        );

        let cache = FileParseCache::new();
        let first_scan = scan_sessions_in_root(&root, &cache);
        assert_eq!(first_scan.len(), 1);
        assert_eq!(first_scan[0].title.as_deref(), Some("Fix the build"));
        assert_eq!(first_scan[0].project_dir.as_deref(), Some("/work/project"));

        // Adding workspace.yaml updates title/cwd/summary even when events.jsonl hits PARSE_CACHE!
        fs::write(
            dir1.join("workspace.yaml"),
            format!(
                "id: {id1}\ncwd: /other/app\nname: Custom Title\nsummary: Fixed CI\nupdated_at: 2026-10-11T09:07:58.221Z\n"
            ),
        )
        .unwrap();

        let legacy_id = "legacy-sess-2";
        write_events(
            &root.join(format!("{legacy_id}.jsonl")),
            &[
                json!({"type":"session.start","timestamp":"2026-10-09T09:00:00Z","data":{"sessionId":legacy_id,"context":{"cwd":"/legacy"}}}),
                json!({"type":"user.message","timestamp":"2026-10-09T09:01:00Z","data":{"content":"Legacy prompt"}}),
            ],
        );

        let second_scan = scan_sessions_in_root(&root, &cache);
        assert_eq!(second_scan.len(), 2);
        let s1 = second_scan.iter().find(|s| s.session_id == id1).unwrap();
        assert_eq!(s1.title.as_deref(), Some("Custom Title"));
        assert_eq!(s1.summary.as_deref(), Some("Fixed CI"));
        assert_eq!(s1.project_dir.as_deref(), Some("/other/app"));
        assert_eq!(s1.project_name.as_deref(), Some("app"));
    }

    #[test]
    fn load_messages_separates_thinking_tools_errors_and_skips_subagents() {
        let temp = tempdir().unwrap();
        let img = temp.path().join("shot.png");
        fs::write(&img, b"pngdata").unwrap();
        let img_str = img.to_string_lossy().to_string();
        let events = temp.path().join("events.jsonl");
        write_events(
            &events,
            &[
                json!({"type":"user.message","timestamp":"2026-10-10T09:08:00Z","data":{
                    "messageId":"u1",
                    "content":"Fix the build",
                    "attachments":[{"type":"file","path":img_str,"displayName":"shot.png"}]
                }}),
                json!({"type":"session.model_change","timestamp":"2026-10-10T09:08:00.500Z","data":{"newModel":"gpt-5-mini"}}),
                json!({"type":"session.usage_record","timestamp":"2026-10-10T09:08:01Z","data":{
                    "usage":{"model":"gpt-5-mini","inputTokens":100,"outputTokens":12,"cacheReadTokens":30,"cacheWriteTokens":0,"duration":1500}
                }}),
                json!({"type":"assistant.message","timestamp":"2026-10-10T09:08:02Z","data":{
                    "messageId":"a1",
                    "content":"Running tests",
                    "reasoningText":"Think first",
                    "toolRequests":[
                        {"toolCallId":"intent","name":"report_intent","arguments":{"intent":"Fixing"}},
                        {"toolCallId":"t1","name":"bash","arguments":{"command":"cargo test"}},
                        {"toolCallId":"t2","name":"edit_file","arguments":{"path":"src/lib.rs"}}
                    ]
                }}),
                json!({"type":"tool.execution_start","timestamp":"2026-10-10T09:08:02.500Z","data":{"toolCallId":"t1","toolName":"bash"}}),
                json!({"type":"tool.execution_complete","timestamp":"2026-10-10T09:08:03Z","data":{"toolCallId":"intent","success":true,"result":{"content":"Intent logged","detailedContent":"Fixing"}}}),
                json!({"type":"tool.execution_complete","timestamp":"2026-10-10T09:08:04Z","data":{"toolCallId":"t1","success":false,"error":{"message":"Command blocked"}}}),
                json!({"type":"tool.execution_complete","timestamp":"2026-10-10T09:08:04.500Z","data":{
                    "toolCallId":"t2",
                    "success":true,
                    "result":{"content":"Edited src/lib.rs","detailedContent":"--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1,2 @@\n-old\n+new1\n+new2\n"}
                }}),
                json!({"type":"assistant.message","timestamp":"2026-10-10T09:08:05Z","data":{"content":"subagent inner","parentToolCallId":"t1"}}),
                json!({"type":"session.compaction_complete","timestamp":"2026-10-10T09:08:06Z","data":{"summaryContent":"Earlier turns compacted"}}),
                json!({"type":"abort","timestamp":"2026-10-10T09:08:06.500Z","data":{"reason":"User cancelled"}}),
                json!({"type":"assistant.message","timestamp":"2026-10-10T09:08:07Z","data":{"messageId":"a2","content":"Done","outputTokens":5}}),
            ],
        );

        let msgs = load_messages(&events).unwrap();
        let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(
            roles,
            vec![
                "user",
                "system",
                "assistant",
                "tool",
                "tool",
                "system",
                "system",
                "assistant",
            ]
        );
        assert_eq!(msgs[0].id.as_deref(), Some("u1"));
        assert!(matches!(
            &msgs[0].blocks[1],
            SessionBlock::Image { image } if matches!(&image.source, ImageSource::LocalFile { path } if path == &img_str)
        ));
        assert!(matches!(
            &msgs[1].blocks[0],
            SessionBlock::Event { kind: EventKind::ModelChange, text: Some(m), .. } if m == "gpt-5-mini"
        ));
        assert!(msgs[1].injected);

        let a1 = &msgs[2];
        let meta = a1.meta.as_ref().unwrap();
        assert_eq!(meta.model.as_deref(), Some("gpt-5-mini"));
        assert_eq!(meta.input_tokens, Some(70));
        assert_eq!(meta.cache_read_tokens, Some(30));
        assert_eq!(meta.output_tokens, Some(12));
        assert_eq!(meta.duration_ms, Some(1500));
        assert!(
            matches!(&a1.blocks[0], SessionBlock::Thinking { text, .. } if text == "Think first")
        );
        assert!(
            matches!(&a1.blocks[1], SessionBlock::Text { text, .. } if text == "Running tests")
        );
        assert!(
            matches!(&a1.blocks[2], SessionBlock::ToolCall { id, raw_name, .. } if id == "t1" && raw_name == "bash")
        );
        assert!(matches!(
            &a1.blocks[3],
            SessionBlock::ToolCall { id, diff: Some(diff), .. } if id == "t2" && diff.added == 2 && diff.removed == 1
        ));

        assert!(matches!(
            &msgs[3].blocks[0],
            SessionBlock::ToolResult { call_id, status: ToolStatus::Error, preview, duration_ms: Some(1500), .. }
                if call_id == "t1" && preview == "Command blocked"
        ));
        assert!(matches!(
            &msgs[4].blocks[0],
            SessionBlock::ToolResult { call_id, status: ToolStatus::Success, .. } if call_id == "t2"
        ));
        assert!(matches!(
            &msgs[5].blocks[0],
            SessionBlock::Event { kind: EventKind::Compaction, text: Some(t), .. } if t == "Earlier turns compacted"
        ));
        assert!(matches!(
            &msgs[6].blocks[0],
            SessionBlock::Event { kind: EventKind::Aborted, text: Some(t), .. } if t == "User cancelled"
        ));
        assert_eq!(msgs[7].content, "Done");
        assert_eq!(msgs[7].meta.as_ref().unwrap().output_tokens, Some(5));
    }

    #[test]
    fn delete_session_removes_directory_or_legacy_file_and_rejects_symlinks() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("session-state");
        let id = "sess-dir-1";
        let dir = root.join(id);
        fs::create_dir_all(&dir).unwrap();
        let events = dir.join("events.jsonl");
        write_events(
            &events,
            &[json!({"type":"session.start","data":{"sessionId":id}})],
        );
        fs::write(dir.join("workspace.yaml"), format!("id: {id}\n")).unwrap();

        let legacy_id = "sess-legacy-1";
        let legacy_path = root.join(format!("{legacy_id}.jsonl"));
        write_events(
            &legacy_path,
            &[json!({"type":"session.start","data":{"sessionId":legacy_id}})],
        );

        // Symlinked session directory must be rejected without touching target
        let outside = temp.path().join("outside-keep");
        fs::create_dir_all(&outside).unwrap();
        let outside_events = outside.join("events.jsonl");
        write_events(
            &outside_events,
            &[json!({"type":"session.start","data":{"sessionId":"link-sess"}})],
        );
        let linked_dir = root.join("link-sess");
        if create_dir_symlink(&outside, &linked_dir).is_ok() {
            let linked_events = linked_dir.join("events.jsonl");
            assert!(preflight_delete(&root, &linked_events, "link-sess").is_err());
            assert!(delete_session(&root, &linked_events, "link-sess").is_err());
            assert!(outside_events.exists());
        }

        // Delete modern directory session
        preflight_delete(&root, &events, id).unwrap();
        assert!(delete_session(&root, &events, id).unwrap());
        assert!(!dir.exists());
        assert!(legacy_path.exists());

        // Delete legacy file session
        preflight_delete(&root, &legacy_path, legacy_id).unwrap();
        assert!(delete_session(&root, &legacy_path, legacy_id).unwrap());
        assert!(!legacy_path.exists());
    }
}
