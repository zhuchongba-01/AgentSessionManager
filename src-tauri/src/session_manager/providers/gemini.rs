use std::collections::HashMap;
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
use crate::session_paths::gemini_home;

use super::blocks::{
    assign_turn_ids, count_diff_lines, estimate_base64_size, large_text_block, parse_arguments,
    single_file_diff, summary_event_block, thinking_block, tool_call_block, tool_result_block,
    ToolSource,
};
use super::utils::{
    checked_storage_child, ensure_readable_size, is_reparse_or_symlink, is_safe_session_id,
    parse_timestamp_to_ms, path_basename, remove_path_if_exists, scan_warning, truncate_summary,
    FileParseCache, TITLE_MAX_CHARS,
};

const PROVIDER_ID: &str = "gemini";
const MAX_PROJECT_ROOT_BYTES: u64 = 4096;

static PARSE_CACHE: LazyLock<FileParseCache> = LazyLock::new(FileParseCache::new);

pub fn session_roots() -> Vec<PathBuf> {
    vec![gemini_home().join("tmp")]
}

#[allow(dead_code)]
pub fn transcript_sources(path: &Path) -> Result<TranscriptSources, String> {
    Ok(vec![(path.to_path_buf(), None)])
}

pub fn scan_sessions() -> Vec<SessionMeta> {
    let tmp_dir = gemini_home().join("tmp");
    scan_sessions_in_root(&tmp_dir, &PARSE_CACHE)
}

fn scan_sessions_in_root(tmp_dir: &Path, cache: &FileParseCache) -> Vec<SessionMeta> {
    match fs::symlink_metadata(tmp_dir) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                scan_warning(format!(
                    "Gemini 会话根目录是符号链接或重解析点，已跳过：{}",
                    tmp_dir.display()
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
                "无法检查 Gemini 会话目录 {}：{err}",
                tmp_dir.display()
            ));
            return Vec::new();
        }
    }

    let entries = match fs::read_dir(tmp_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            scan_warning(format!("无法扫描 Gemini 目录 {}：{err}", tmp_dir.display()));
            return Vec::new();
        }
    };

    let mut files = Vec::new();
    let mut project_roots_by_chats_dir: HashMap<PathBuf, String> = HashMap::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                scan_warning(format!("无法枚举 {}：{err}", tmp_dir.display()));
                continue;
            }
        };
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let project_slug_dir = entry.path();
        if fs::symlink_metadata(&project_slug_dir).is_ok_and(|m| is_reparse_or_symlink(&m)) {
            continue;
        }

        let chats_dir = project_slug_dir.join("chats");
        match fs::symlink_metadata(&chats_dir) {
            Ok(meta) if !is_reparse_or_symlink(&meta) && meta.is_dir() => {}
            _ => continue,
        }

        if let Some(proj_root) = read_project_root_marker(&project_slug_dir) {
            project_roots_by_chats_dir.insert(chats_dir.clone(), proj_root);
        }

        let chat_entries = match fs::read_dir(&chats_dir) {
            Ok(list) => list,
            Err(err) => {
                scan_warning(format!("无法扫描 {}：{err}", chats_dir.display()));
                continue;
            }
        };

        for file_entry in chat_entries {
            let Ok(file_entry) = file_entry else {
                continue;
            };
            let Ok(file_ft) = file_entry.file_type() else {
                continue;
            };
            if !file_ft.is_file() {
                continue;
            }
            let path = file_entry.path();
            if fs::symlink_metadata(&path).is_ok_and(|m| is_reparse_or_symlink(&m)) {
                continue;
            }
            if !is_session_file(&path) {
                continue;
            }
            files.push(path);
        }
    }

    let mut sessions = cache.scan(files, parse_session_result);
    for session in &mut sessions {
        if let Some(src) = session.source_path.as_deref() {
            if let Some(chats_dir) = Path::new(src).parent() {
                if let Some(proj_dir) = project_roots_by_chats_dir.get(chats_dir) {
                    session.project_dir = Some(proj_dir.clone());
                }
            }
        }
        session.project_name = session.project_dir.as_deref().and_then(path_basename);
    }
    sessions
}

fn read_project_root_marker(project_slug_dir: &Path) -> Option<String> {
    let marker = project_slug_dir.join(".project_root");
    let meta = fs::symlink_metadata(&marker).ok()?;
    if is_reparse_or_symlink(&meta) || !meta.is_file() || meta.len() > MAX_PROJECT_ROOT_BYTES {
        return None;
    }
    let mut raw = String::new();
    File::open(&marker)
        .ok()?
        .take(MAX_PROJECT_ROOT_BYTES + 1)
        .read_to_string(&mut raw)
        .ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Legacy `.json`, or newer Gemini CLI `.jsonl`. When a session is resumed and
/// migrated to `.jsonl`, the older `.json` sibling may remain on disk; hide it so
/// the conversation is not listed twice.
pub(crate) fn is_session_file(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some("jsonl") => true,
        Some("json") => !path.with_extension("jsonl").exists(),
        _ => false,
    }
}

/// Normalize a `.json` or `.jsonl` Gemini session file into `{sessionId, ..., messages: [...]}`.
///
/// Legacy `.json` files are already a single JSON object. Newer `.jsonl` files replay
/// Gemini CLI's `loadConversationRecord`:
/// - records with `sessionId` carry metadata (and optional initial `messages`)
/// - records with string `id` upsert a message in-place preserving original position
/// - `{"$set": {...}}` merges metadata and resets `messages` if `$set.messages` is present
/// - `{"$rewindTo": id}` truncates that message and all subsequent messages
pub(crate) fn parse_session_document(data: &str) -> Option<Value> {
    if let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(data) {
        map.entry("messages")
            .or_insert_with(|| Value::Array(Vec::new()));
        return Some(Value::Object(map));
    }

    let mut metadata = serde_json::Map::new();
    let mut log = MessageLog::default();
    let mut seen_record = false;

    for line in data.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(Value::Object(mut record)) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        seen_record = true;
        if let Some(id) = record.get("$rewindTo").and_then(Value::as_str) {
            log.rewind_to(id);
        } else if record.get("id").is_some_and(Value::is_string) {
            log.upsert(Value::Object(record));
        } else if let Some(Value::Object(mut set)) = record.remove("$set") {
            if let Some(Value::Array(list)) = set.remove("messages") {
                log.reset(list);
            }
            metadata.extend(set);
        } else if record.get("sessionId").is_some_and(Value::is_string) {
            if let Some(Value::Array(list)) = record.remove("messages") {
                for msg in list {
                    log.upsert(msg);
                }
            }
            metadata.extend(record);
        }
    }

    if !seen_record {
        return None;
    }
    metadata.insert("messages".to_string(), Value::Array(log.messages));
    Some(Value::Object(metadata))
}

#[derive(Default)]
struct MessageLog {
    messages: Vec<Value>,
    index: HashMap<String, usize>,
}

impl MessageLog {
    fn upsert(&mut self, msg: Value) {
        let Some(id) = msg.get("id").and_then(Value::as_str).map(str::to_string) else {
            return;
        };
        match self.index.get(&id) {
            Some(&i) => self.messages[i] = msg,
            None => {
                self.index.insert(id, self.messages.len());
                self.messages.push(msg);
            }
        }
    }

    fn rewind_to(&mut self, id: &str) {
        let len = self.index.get(id).copied().unwrap_or(0);
        self.messages.truncate(len);
        self.index.retain(|_, i| *i < len);
    }

    fn reset(&mut self, list: Vec<Value>) {
        self.messages.clear();
        self.index.clear();
        for msg in list {
            self.upsert(msg);
        }
    }
}

fn is_ignored_user_text(text: &str) -> bool {
    let t = text.trim();
    t.is_empty() || t.starts_with('/') || t.starts_with('?') || is_injected_user_text(t)
}

fn is_injected_user_text(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("<session_context>") || t.starts_with("<hook_context>")
}

fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub(crate) fn parse_session_document_for_content(raw: &str) -> Option<Value> {
    parse_session_document(raw)
}

pub(crate) fn format_thoughts_for_content(thoughts: &Value) -> Option<String> {
    format_thoughts(thoughts).map(|merged| merged.text)
}

pub(crate) struct MergedThoughts {
    pub summary: String,
    pub text: String,
}

pub(crate) fn format_thoughts(value: &Value) -> Option<MergedThoughts> {
    let items = value.as_array()?;
    let mut thoughts = Vec::new();
    for item in items {
        let object = item.as_object()?;
        let field = |key: &str| object.get(key).and_then(Value::as_str).unwrap_or("").trim();
        let (subject, description) = (field("subject"), field("description"));
        if !subject.is_empty() || !description.is_empty() {
            thoughts.push((subject, description));
        }
    }
    if thoughts.is_empty() {
        return None;
    }
    let summary = thoughts
        .iter()
        .map(|(subject, _)| *subject)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    let text = thoughts
        .iter()
        .map(
            |(subject, description)| match (subject.is_empty(), description.is_empty()) {
                (false, false) => format!("**{subject}**\n\n{description}"),
                (false, true) => format!("**{subject}**"),
                _ => description.to_string(),
            },
        )
        .collect::<Vec<_>>()
        .join("\n\n");
    Some(MergedThoughts { summary, text })
}

fn relative_to_tmp_root(path: &Path) -> String {
    let parts: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_string()),
            _ => None,
        })
        .rev()
        .take(3)
        .collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

fn image_from_gemini_part(
    part: &Value,
    part_ptr: &str,
    file_ref: &dyn Fn(String) -> Option<ContentRef>,
) -> Option<ImageRef> {
    if let Some(inline) = part.get("inlineData").and_then(Value::as_object) {
        let mime = inline
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or("image/png");
        if mime.starts_with("image/") {
            if let Some(data) = inline
                .get("data")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                let content = file_ref(format!("{part_ptr}/inlineData/data"))?;
                return Some(ImageRef {
                    source: ImageSource::Inline { content },
                    media_type: mime.to_string(),
                    size: estimate_base64_size(data.len()),
                    alt: None,
                });
            }
        }
    }

    if let Some(file_data) = part.get("fileData").and_then(Value::as_object) {
        let mime = file_data
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or("image/png");
        if mime.starts_with("image/") {
            if let Some(uri) = file_data
                .get("fileUri")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let fs_path = if uri.starts_with("file://") {
                    parse_file_url(uri).ok()
                } else {
                    let p = PathBuf::from(uri);
                    p.is_absolute().then_some(p)
                };
                if let Some(fs_path) = fs_path {
                    let size = fs::metadata(&fs_path)
                        .ok()
                        .map(|m| u32::try_from(m.len()).unwrap_or(u32::MAX))
                        .unwrap_or(0);
                    return Some(ImageRef {
                        source: ImageSource::LocalFile {
                            path: uri.to_string(),
                        },
                        media_type: mime.to_string(),
                        size,
                        alt: None,
                    });
                }
            }
        }
    }

    None
}

fn content_blocks(
    content: Option<&Value>,
    base_ptr: &str,
    injected: bool,
    file_ref: &dyn Fn(String) -> Option<ContentRef>,
) -> Vec<SessionBlock> {
    let mut blocks = Vec::new();
    match content {
        Some(Value::String(s)) if !s.trim().is_empty() => {
            if injected {
                let ptr = base_ptr.to_string();
                blocks.push(large_text_block(s.clone(), || file_ref(ptr)));
            } else {
                blocks.push(SessionBlock::text(s.clone()));
            }
        }
        Some(Value::Array(items)) => {
            let mut text_parts: Vec<&str> = Vec::new();
            let flush_text = |parts: &mut Vec<&str>, blocks: &mut Vec<SessionBlock>| {
                if parts.is_empty() {
                    return;
                }
                let joined = parts.join("\n");
                parts.clear();
                if !joined.trim().is_empty() {
                    if injected {
                        let ptr = base_ptr.to_string();
                        blocks.push(large_text_block(joined, || file_ref(ptr)));
                    } else {
                        blocks.push(SessionBlock::text(joined));
                    }
                }
            };
            for (k, item) in items.iter().enumerate() {
                let part_ptr = format!("{base_ptr}/{k}");
                if let Some(image) = image_from_gemini_part(item, &part_ptr, file_ref) {
                    flush_text(&mut text_parts, &mut blocks);
                    blocks.push(SessionBlock::Image { image });
                } else if let Some(text) = item.get("text").and_then(Value::as_str) {
                    text_parts.push(text);
                }
            }
            flush_text(&mut text_parts, &mut blocks);
        }
        _ => {}
    }
    blocks
}

fn extract_tool_result_images(
    call: &Value,
    call_base: &str,
    file_ref: &dyn Fn(String) -> Option<ContentRef>,
) -> Vec<ImageRef> {
    let mut images = Vec::new();
    for (k, item) in call
        .get("result")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let item_ptr = format!("{call_base}/result/{k}");
        if let Some(img) = image_from_gemini_part(item, &item_ptr, file_ref) {
            images.push(img);
        }
        for (p, nested) in item
            .pointer("/functionResponse/parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let nested_ptr = format!("{item_ptr}/functionResponse/parts/{p}");
            if let Some(img) = image_from_gemini_part(nested, &nested_ptr, file_ref) {
                images.push(img);
            }
        }
    }
    images
}

fn tool_output(call: &Value, call_base: &str) -> (String, Option<String>) {
    match call.get("resultDisplay") {
        Some(Value::String(text)) if !text.is_empty() => {
            return (text.clone(), Some(format!("{call_base}/resultDisplay")));
        }
        Some(display @ Value::Object(_)) => {
            if let Some(diff) = display.get("fileDiff").and_then(Value::as_str) {
                return (
                    diff.to_string(),
                    Some(format!("{call_base}/resultDisplay/fileDiff")),
                );
            }
            return (display.to_string(), None);
        }
        _ => {}
    }
    for (k, item) in call
        .get("result")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        for key in ["output", "error"] {
            if let Some(text) = item
                .pointer(&format!("/functionResponse/response/{key}"))
                .and_then(Value::as_str)
            {
                return (
                    text.to_string(),
                    Some(format!(
                        "{call_base}/result/{k}/functionResponse/response/{key}"
                    )),
                );
            }
        }
    }
    match call.get("result") {
        Some(Value::String(text)) => (text.clone(), Some(format!("{call_base}/result"))),
        Some(Value::Null) | None => (String::new(), None),
        Some(Value::Array(arr)) if arr.iter().all(|item| item.get("inlineData").is_some()) => {
            (String::new(), None)
        }
        Some(other) => (other.to_string(), None),
    }
}

fn gemini_blocks(
    msg: &Value,
    base: &str,
    file_ref: &dyn Fn(String) -> Option<ContentRef>,
) -> Vec<SessionBlock> {
    let mut blocks = Vec::new();

    if let Some(thoughts) = msg.get("thoughts").and_then(format_thoughts) {
        blocks.push(thinking_block(
            &thoughts.text,
            (!thoughts.summary.is_empty()).then_some(thoughts.summary),
            None,
            || file_ref(format!("{base}/thoughts")),
        ));
    }

    blocks.extend(content_blocks(
        msg.get("content"),
        &format!("{base}/content"),
        false,
        file_ref,
    ));

    for (j, call) in msg
        .get("toolCalls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let Some(name) = call
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let call_base = format!("{base}/toolCalls/{j}");
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{name}-{j}"));
        let args = call.get("args").map(parse_arguments).unwrap_or(Value::Null);
        let mut call_block = tool_call_block(ToolSource::Gemini, id.clone(), name, &args, || {
            file_ref(format!("{call_base}/args"))
        });

        let (output, pointer) = tool_output(call, &call_base);
        if let Some(diff_text) = call
            .pointer("/resultDisplay/fileDiff")
            .and_then(Value::as_str)
        {
            if let SessionBlock::ToolCall { diff, title, .. } = &mut call_block {
                let (added, removed) = count_diff_lines(diff_text);
                let path = call
                    .pointer("/resultDisplay/filePath")
                    .or_else(|| call.pointer("/resultDisplay/fileName"))
                    .and_then(Value::as_str)
                    .unwrap_or(title.as_str())
                    .to_string();
                let op = diff
                    .as_ref()
                    .and_then(|d| d.files.first())
                    .map_or(DiffOp::Update, |f| f.op);
                let mut summary = single_file_diff(&path, op, added, removed);
                summary.full = file_ref(format!("{call_base}/resultDisplay/fileDiff"));
                *diff = Some(summary);
            }
        }
        blocks.push(call_block);

        let status = match call.get("status").and_then(Value::as_str) {
            Some("success" | "completed") => ToolStatus::Success,
            Some("error") => ToolStatus::Error,
            Some("cancelled" | "canceled") => ToolStatus::Interrupted,
            Some("executing" | "scheduled" | "validating" | "awaiting_approval") => {
                ToolStatus::Pending
            }
            _ => ToolStatus::Unknown,
        };
        let mut res_block = tool_result_block(id, status, &output, || pointer.and_then(file_ref));
        let inline_imgs = extract_tool_result_images(call, &call_base, file_ref);
        if !inline_imgs.is_empty() {
            if let SessionBlock::ToolResult { images, .. } = &mut res_block {
                *images = inline_imgs;
            }
        }
        blocks.push(res_block);
    }

    blocks
}

fn gemini_meta(msg: &Value) -> Option<MessageMeta> {
    let tokens = msg.get("tokens");
    let count = |key: &str| {
        tokens
            .and_then(|t| t.get(key))
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
    };
    MessageMeta {
        model: msg
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        input_tokens: count("input"),
        output_tokens: count("output"),
        cache_read_tokens: count("cached"),
        reasoning_tokens: count("thoughts"),
        ..MessageMeta::default()
    }
    .into_option()
}

fn read_session_document(path: &Path) -> Result<Value, String> {
    ensure_readable_size(path)?;
    let data = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read Gemini session {}: {e}", path.display()))?;
    parse_session_document(&data)
        .ok_or_else(|| format!("Failed to parse Gemini session JSON: {}", path.display()))
}

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    let value = read_session_document(path)?;
    let messages = value
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "No messages array found in Gemini session".to_string())?;

    let rel_path = relative_to_tmp_root(path);
    let file_ref = |pointer: String| -> Option<ContentRef> {
        (!rel_path.is_empty()).then(|| ContentRef::File {
            rel_path: rel_path.clone(),
            pointer,
        })
    };

    let mut result = Vec::new();
    for (index, msg) in messages.iter().enumerate() {
        let ts = msg.get("timestamp").and_then(parse_timestamp_to_ms);
        let base = format!("/messages/{index}");
        let mut message = match msg.get("type").and_then(Value::as_str) {
            Some("user") => {
                let text = content_text(msg.get("content"));
                let injected = is_injected_user_text(&text);
                let blocks = content_blocks(
                    msg.get("content"),
                    &format!("{base}/content"),
                    injected,
                    &file_ref,
                );
                let mut message = SessionMessage::from_blocks("user", ts, blocks);
                message.injected = injected;
                message
            }
            Some("gemini") => {
                let blocks = gemini_blocks(msg, &base, &file_ref);
                let mut message = SessionMessage::from_blocks("assistant", ts, blocks);
                message.meta = gemini_meta(msg);
                message
            }
            Some(kind @ ("info" | "warning" | "error")) => {
                let text = content_text(msg.get("content"));
                if text.trim().is_empty() {
                    continue;
                }
                let event_kind = if kind == "error" {
                    EventKind::Error
                } else {
                    EventKind::Info
                };
                let block =
                    summary_event_block(event_kind, &text, || file_ref(format!("{base}/content")));
                let mut message = SessionMessage::from_blocks("system", ts, vec![block]);
                message.injected = kind == "info";
                message
            }
            _ => continue,
        };
        if message.is_empty() && message.meta.is_none() {
            continue;
        }
        message.id = msg.get("id").and_then(Value::as_str).map(str::to_string);
        result.push(message);
    }

    assign_turn_ids(&mut result);
    Ok(result)
}

fn parse_session_result(path: &Path) -> Result<Option<SessionMeta>, String> {
    if let Err(err) = ensure_readable_size(path) {
        scan_warning(err.clone());
        return Err(err);
    }
    let data = fs::read_to_string(path).map_err(|err| {
        let msg = format!("无法读取 Gemini 会话 {}：{err}", path.display());
        scan_warning(msg.clone());
        msg
    })?;
    let Some(value) = parse_session_document(&data) else {
        return Ok(None);
    };

    let Some(session_id) = value
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        return Ok(None);
    };

    let created_at = value.get("startTime").and_then(parse_timestamp_to_ms);
    let mut last_active_at = value.get("lastUpdated").and_then(parse_timestamp_to_ms);

    let msgs = value.get("messages").and_then(Value::as_array);
    if last_active_at.is_none() {
        last_active_at = msgs.and_then(|list| {
            list.iter()
                .rev()
                .find_map(|m| m.get("timestamp").and_then(parse_timestamp_to_ms))
        });
    }

    let first_user = msgs.and_then(|list| {
        list.iter()
            .filter(|m| m.get("type").and_then(Value::as_str) == Some("user"))
            .map(|m| content_text(m.get("content")))
            .find(|s| !is_ignored_user_text(s))
    });

    let last_visible = msgs.and_then(|list| {
        list.iter()
            .rev()
            .find_map(|m| match m.get("type").and_then(Value::as_str) {
                Some("gemini") => {
                    let t = content_text(m.get("content"));
                    (!t.trim().is_empty()).then_some(t)
                }
                Some("user") => {
                    let t = content_text(m.get("content"));
                    (!is_ignored_user_text(&t)).then_some(t)
                }
                _ => None,
            })
    });

    let title = first_user
        .as_deref()
        .map(|s| truncate_summary(s, TITLE_MAX_CHARS))
        .filter(|s| !s.is_empty());

    let summary = value
        .get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| truncate_summary(s, 160))
        .or_else(|| {
            last_visible
                .as_deref()
                .map(|s| truncate_summary(s, 160))
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            first_user
                .as_deref()
                .map(|s| truncate_summary(s, 160))
                .filter(|s| !s.is_empty())
        });

    let project_dir = value
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let project_name = project_dir.as_deref().and_then(path_basename);
    let resume_command =
        is_safe_session_id(&session_id).then(|| format!("gemini --resume {session_id}"));

    Ok(Some(SessionMeta {
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
        last_active_at: last_active_at.or(created_at),
        source_path: Some(path.to_string_lossy().to_string()),
        resume_command,
    }))
}

fn validate_gemini_session_path(
    root: &Path,
    path: &Path,
    session_id: &str,
) -> Result<(PathBuf, PathBuf, Option<PathBuf>, bool), String> {
    if !is_safe_session_id(session_id) {
        return Err(format!("Invalid Gemini session ID: {session_id}"));
    }
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(format!("Invalid Gemini session path: {}", path.display()));
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| format!("Invalid Gemini session extension: {}", path.display()))?;
    if !matches!(ext, "jsonl" | "json") {
        return Err(format!(
            "Unsupported Gemini session file extension: {}",
            path.display()
        ));
    }

    let file_exists = match fs::symlink_metadata(path) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                return Err(format!(
                    "Gemini session source must not be a symlink or reparse point: {}",
                    path.display()
                ));
            }
            if !meta.file_type().is_file() {
                return Err(format!(
                    "Gemini session source is not a regular file: {}",
                    path.display()
                ));
            }
            true
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            return Err(format!(
                "Failed to inspect Gemini session {}: {err}",
                path.display()
            ))
        }
    };

    let canonical_root = root.canonicalize().map_err(|err| {
        format!(
            "Failed to resolve Gemini tmp root {}: {err}",
            root.display()
        )
    })?;
    let root_meta = fs::metadata(&canonical_root).map_err(|err| {
        format!(
            "Failed to inspect Gemini tmp root {}: {err}",
            canonical_root.display()
        )
    })?;
    if !root_meta.is_dir() {
        return Err(format!(
            "Gemini tmp root is not a directory: {}",
            canonical_root.display()
        ));
    }

    let resolved_path = checked_storage_child(root, path)?;
    let relative = resolved_path.strip_prefix(&canonical_root).map_err(|_| {
        format!(
            "Gemini session path is outside tmp root: {}",
            path.display()
        )
    })?;
    let components: Vec<_> = relative.components().collect();
    if components.len() != 3
        || !components.iter().all(|c| matches!(c, Component::Normal(_)))
        || components[1].as_os_str() != OsStr::new("chats")
    {
        return Err(format!(
            "Gemini session path must be <tmp>/<project>/chats/<session>.(jsonl|json): {}",
            path.display()
        ));
    }

    if file_exists {
        let doc = read_session_document(&resolved_path)?;
        let actual_id = doc
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if actual_id != session_id {
            return Err(format!(
                "Gemini session ID mismatch: expected {session_id}, found {actual_id}"
            ));
        }
    }

    let sibling_ext = if ext == "jsonl" { "json" } else { "jsonl" };
    let sibling_candidate = resolved_path.with_extension(sibling_ext);
    let checked_sibling = match fs::symlink_metadata(&sibling_candidate) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                return Err(format!(
                    "Refusing to delete Gemini session with symlinked sibling: {}",
                    sibling_candidate.display()
                ));
            }
            if !meta.file_type().is_file() {
                return Err(format!(
                    "Gemini sibling path is not a regular file: {}",
                    sibling_candidate.display()
                ));
            }
            let checked = checked_storage_child(&canonical_root, &sibling_candidate)?;
            let sib_doc = read_session_document(&checked)?;
            let sib_id = sib_doc
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if sib_id != session_id {
                return Err(format!(
                    "Gemini sibling session ID mismatch ({sib_id} != {session_id}): {}",
                    checked.display()
                ));
            }
            Some(checked)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(format!(
                "Failed to inspect Gemini sibling {}: {err}",
                sibling_candidate.display()
            ))
        }
    };

    Ok((canonical_root, resolved_path, checked_sibling, file_exists))
}

#[allow(dead_code)]
pub fn preflight_delete(root: &Path, path: &Path, session_id: &str) -> Result<(), String> {
    validate_gemini_session_path(root, path, session_id).map(|_| ())
}

pub fn delete_session(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    let (_canonical_root, resolved_path, checked_sibling, file_exists) =
        validate_gemini_session_path(root, path, session_id)?;

    let mut removed = false;

    if let Some(sibling) = checked_sibling {
        if fs::symlink_metadata(&sibling).is_ok() {
            remove_path_if_exists(&sibling).map_err(|err| {
                format!(
                    "Failed to delete legacy Gemini session file {}: {err}",
                    sibling.display()
                )
            })?;
            removed = true;
        }
    }

    if file_exists {
        match fs::symlink_metadata(&resolved_path) {
            Ok(meta) if is_reparse_or_symlink(&meta) => {
                return Err(format!(
                    "Gemini session source turned into a symlink before deletion: {}",
                    resolved_path.display()
                ));
            }
            Ok(_) => {
                remove_path_if_exists(&resolved_path).map_err(|err| {
                    format!(
                        "Failed to delete Gemini session file {}: {err}",
                        resolved_path.display()
                    )
                })?;
                removed = true;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(format!(
                    "Failed to inspect Gemini session file {}: {err}",
                    resolved_path.display()
                ))
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

    fn write_jsonl(path: &Path, lines: &[Value]) {
        let data: String = lines.iter().map(|l| format!("{l}\n")).collect();
        fs::write(path, data).expect("write jsonl");
    }

    #[cfg(unix)]
    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_file(target, link)
    }

    #[test]
    fn scans_project_root_marker_and_hides_migrated_legacy_json() {
        let temp = tempdir().expect("tempdir");
        let tmp_root = temp.path().join("tmp");
        let proj = tmp_root.join("proj-hash-1");
        let chats = proj.join("chats");
        fs::create_dir_all(&chats).unwrap();
        fs::write(proj.join(".project_root"), "E:/workspace/my-gemini-app\n").unwrap();

        let legacy = chats.join("session-x.json");
        let migrated = chats.join("session-x.jsonl");
        let standalone_json = chats.join("session-y.json");

        fs::write(&legacy, r#"{"sessionId":"sess-x","messages":[]}"#).unwrap();
        write_jsonl(
            &migrated,
            &[
                json!({
                    "sessionId": "sess-x",
                    "startTime": "2026-10-04T10:00:00Z",
                    "lastUpdated": "2026-10-04T10:05:00Z"
                }),
                json!({
                    "$set": {
                        "messages": [
                            {"id": "ctx", "type": "user", "content": [{"text": "<session_context>\nCLI boilerplate"}]},
                        ]
                    }
                }),
                json!({"id": "cmd", "type": "user", "content": [{"text": "/model"}]}),
                json!({"id": "u1", "type": "user", "content": [{"text": "Fix the build pipeline"}]}),
                json!({"id": "g1", "type": "gemini", "content": "Pipeline fixed."}),
            ],
        );
        fs::write(
            &standalone_json,
            r#"{
                "sessionId": "sess-y",
                "startTime": "2026-10-03T08:00:00Z",
                "messages": [
                    {"id": "u1", "type": "user", "content": "Hello legacy"}
                ]
            }"#,
        )
        .unwrap();

        let cache = FileParseCache::new();
        let sessions = scan_sessions_in_root(&tmp_root, &cache);
        assert_eq!(sessions.len(), 2);

        let sx = sessions.iter().find(|s| s.session_id == "sess-x").unwrap();
        assert_eq!(sx.title.as_deref(), Some("Fix the build pipeline"));
        assert_eq!(sx.summary.as_deref(), Some("Pipeline fixed."));
        assert_eq!(
            sx.project_dir.as_deref(),
            Some("E:/workspace/my-gemini-app")
        );
        assert_eq!(sx.project_name.as_deref(), Some("my-gemini-app"));
        assert_eq!(sx.resume_command.as_deref(), Some("gemini --resume sess-x"));
        assert!(!sx.residual);
        assert!(!sx.archived);

        let sy = sessions.iter().find(|s| s.session_id == "sess-y").unwrap();
        assert_eq!(sy.title.as_deref(), Some("Hello legacy"));
        assert_eq!(sy.resume_command.as_deref(), Some("gemini --resume sess-y"));
    }

    #[test]
    fn jsonl_replay_separates_thinking_tool_calls_results_and_system_events() {
        let temp = tempdir().expect("tempdir");
        let chats = temp.path().join("tmp").join("hash1").join("chats");
        fs::create_dir_all(&chats).unwrap();
        let path = chats.join("session-2026-10-04.jsonl");
        let long_output: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        write_jsonl(
            &path,
            &[
                json!({"sessionId": "sess-1", "startTime": "2026-10-04T10:00:00Z"}),
                json!({"id": "ctx", "type": "user", "content": "<session_context>ignore me"}),
                json!({"id": "u1", "timestamp": "2026-10-04T10:00:01Z", "type": "user", "content": [{"text": "first question"}]}),
                json!({"id": "g1", "timestamp": "2026-10-04T10:00:02Z", "type": "gemini", "content": "draft"}),
                json!({
                    "id": "g1",
                    "timestamp": "2026-10-04T10:00:03Z",
                    "type": "gemini",
                    "model": "gemini-2.5-pro",
                    "tokens": {"input": 1200, "output": 85, "cached": 400, "thoughts": 30},
                    "content": "answer one",
                    "thoughts": [
                        {"subject": "Inspecting main", "description": "Read src/main.rs first."},
                        {"subject": "Planning edit", "description": "Update config.ts."}
                    ],
                    "toolCalls": [
                        {
                            "id": "c1",
                            "name": "read_file",
                            "args": {"path": "src/main.rs"},
                            "status": "success",
                            "resultDisplay": long_output,
                            "result": [
                                {"inlineData": {"mimeType": "image/png", "data": "aGVsbG8="}}
                            ]
                        },
                        {
                            "id": "c2",
                            "name": "replace",
                            "args": {"file_path": "config.ts", "old_string": "a", "new_string": "b"},
                            "status": "success",
                            "resultDisplay": {"fileName": "config.ts", "fileDiff": "--- a\n+++ b\n-a\n+b\n+c\n"}
                        }
                    ]
                }),
                json!({"id": "u2", "type": "user", "content": "dropped"}),
                json!({"$rewindTo": "u2"}),
                json!({"id": "i1", "timestamp": "2026-10-04T10:00:03Z", "type": "info", "content": "Token refreshed"}),
                json!({"id": "w1", "timestamp": "2026-10-04T10:00:03Z", "type": "warning", "content": "Rate limit near"}),
                json!({"id": "e1", "timestamp": "2026-10-04T10:00:04Z", "type": "error", "content": "MCP timeout"}),
            ],
        );

        let raw = fs::read_to_string(&path).unwrap();
        let doc = parse_session_document_for_content(&raw).unwrap();
        let formatted_thoughts =
            format_thoughts_for_content(doc.pointer("/messages/2/thoughts").unwrap()).unwrap();
        assert!(formatted_thoughts.contains("**Inspecting main**"));
        assert!(formatted_thoughts.contains("**Planning edit**"));

        let msgs = load_messages(&path).expect("load messages");
        assert_eq!(msgs.len(), 6);
        assert_eq!((msgs[0].role.as_str(), msgs[0].injected), ("user", true));
        assert_eq!(msgs[0].turn_id.as_deref(), Some("t0"));
        assert_eq!((msgs[1].role.as_str(), msgs[1].injected), ("user", false));
        assert_eq!(msgs[1].content, "first question");
        assert_eq!(msgs[1].turn_id.as_deref(), Some("t1"));

        let assistant = &msgs[2];
        assert_eq!(assistant.role, "assistant");
        assert_eq!(assistant.id.as_deref(), Some("g1"));
        assert!(matches!(
            &assistant.blocks[0],
            SessionBlock::Thinking { summary: Some(sum), .. }
                if sum == "Inspecting main · Planning edit"
        ));
        assert!(matches!(
            &assistant.blocks[1],
            SessionBlock::Text { text, .. } if text == "answer one"
        ));
        assert!(matches!(
            &assistant.blocks[2],
            SessionBlock::ToolCall { id, raw_name, .. } if id == "c1" && raw_name == "read_file"
        ));
        match &assistant.blocks[3] {
            SessionBlock::ToolResult {
                call_id,
                status,
                truncated,
                full,
                images,
                ..
            } => {
                assert_eq!(call_id, "c1");
                assert_eq!(*status, ToolStatus::Success);
                assert!(*truncated);
                assert_eq!(
                    full,
                    &Some(ContentRef::File {
                        rel_path: "hash1/chats/session-2026-10-04.jsonl".into(),
                        pointer: "/messages/2/toolCalls/0/resultDisplay".into(),
                    })
                );
                assert_eq!(images.len(), 1);
            }
            other => panic!("unexpected block: {other:?}"),
        }
        assert!(matches!(
            &assistant.blocks[4],
            SessionBlock::ToolCall { diff: Some(diff), .. } if diff.added == 2 && diff.removed == 1
        ));
        let meta = assistant.meta.as_ref().unwrap();
        assert_eq!(meta.model.as_deref(), Some("gemini-2.5-pro"));
        assert_eq!(meta.input_tokens, Some(1200));
        assert_eq!(meta.cache_read_tokens, Some(400));
        assert_eq!(meta.output_tokens, Some(85));
        assert_eq!(meta.reasoning_tokens, Some(30));

        assert_eq!((msgs[3].role.as_str(), msgs[3].injected), ("system", true));
        assert_eq!((msgs[4].role.as_str(), msgs[4].injected), ("system", false));
        assert!(matches!(
            &msgs[5].blocks[0],
            SessionBlock::Event { kind: EventKind::Error, text: Some(t), .. } if t == "MCP timeout"
        ));
    }

    #[test]
    fn unsafe_session_id_disables_resume_command_and_blocks_delete() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session-bad.json");
        fs::write(
            &path,
            r#"{"sessionId": "bad; touch /tmp/pwn", "messages": [{"id":"1","type":"user","content":"hi"}]}"#,
        )
        .unwrap();
        let meta = parse_session_result(&path).unwrap().unwrap();
        assert!(meta.resume_command.is_none());
        assert!(preflight_delete(temp.path(), &path, &meta.session_id).is_err());
    }

    #[test]
    fn delete_session_validates_layout_removes_legacy_sibling_and_rejects_symlinks() {
        let temp = tempdir().expect("tempdir");
        let tmp_root = temp.path().join("tmp");
        let chats = tmp_root.join("proj1").join("chats");
        fs::create_dir_all(&chats).unwrap();

        let migrated = chats.join("session-1.jsonl");
        let legacy = chats.join("session-1.json");
        let keep = chats.join("session-2.json");

        write_jsonl(&migrated, &[json!({"sessionId": "sess-1"})]);
        fs::write(&legacy, r#"{"sessionId": "sess-1", "messages": []}"#).unwrap();
        fs::write(&keep, r#"{"sessionId": "sess-2", "messages": []}"#).unwrap();

        // Wrong session_id must fail preflight without deleting anything
        assert!(preflight_delete(&tmp_root, &migrated, "wrong-id").is_err());
        assert!(migrated.exists() && legacy.exists());

        // Wrong layout depth must fail
        let shallow = tmp_root.join("proj1").join("session-1.jsonl");
        write_jsonl(&shallow, &[json!({"sessionId": "sess-1"})]);
        assert!(preflight_delete(&tmp_root, &shallow, "sess-1").is_err());

        // Symlinked session file must be rejected
        let link = chats.join("session-link.json");
        if create_file_symlink(&keep, &link).is_ok() {
            assert!(preflight_delete(&tmp_root, &link, "sess-2").is_err());
            assert!(delete_session(&tmp_root, &link, "sess-2").is_err());
            assert!(keep.exists());
        }

        // Valid deletion removes both .jsonl and .json sibling while keeping unrelated session
        preflight_delete(&tmp_root, &migrated, "sess-1").unwrap();
        assert!(delete_session(&tmp_root, &migrated, "sess-1").unwrap());
        assert!(!migrated.exists());
        assert!(!legacy.exists());
        assert!(keep.exists());
    }
}
