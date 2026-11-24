#![allow(dead_code)]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use serde_json::Value;

use crate::session_manager::cache::TranscriptSources;
use crate::session_manager::{SessionMessage, SessionMeta};
use crate::session_paths::gemini_home;

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

fn format_thoughts(value: &Value) -> Option<String> {
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
    Some(text)
}

fn format_tool_call(name: &str, args: Option<&Value>) -> String {
    let args_str = match args {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Object(map)) if !map.is_empty() => Value::Object(map.clone()).to_string(),
        Some(other) if !other.is_null() && other != &Value::Object(serde_json::Map::new()) => {
            other.to_string()
        }
        _ => String::new(),
    };
    if args_str.is_empty() {
        format!("[Tool: {name}]")
    } else {
        format!("[Tool: {name}]\n{args_str}")
    }
}

fn extract_tool_output(call: &Value) -> String {
    match call.get("resultDisplay") {
        Some(Value::String(text)) if !text.trim().is_empty() => return text.clone(),
        Some(display @ Value::Object(_)) => {
            if let Some(diff) = display.get("fileDiff").and_then(Value::as_str) {
                if !diff.trim().is_empty() {
                    return diff.to_string();
                }
            }
            return display.to_string();
        }
        _ => {}
    }
    for item in call
        .get("result")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for key in ["output", "error"] {
            if let Some(text) = item
                .pointer(&format!("/functionResponse/response/{key}"))
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                return text.to_string();
            }
        }
    }
    match call.get("result") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(Value::Array(arr)) if arr.is_empty() => String::new(),
        Some(other) => other.to_string(),
    }
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

    let mut result = Vec::new();
    for msg in messages {
        let ts = msg.get("timestamp").and_then(parse_timestamp_to_ms);
        match msg.get("type").and_then(Value::as_str) {
            Some("user") => {
                let text = content_text(msg.get("content"));
                if text.trim().is_empty() || is_injected_user_text(&text) {
                    continue;
                }
                result.push(SessionMessage {
                    role: "user".to_string(),
                    content: text,
                    ts,
                });
            }
            Some("gemini") => {
                if let Some(thoughts) = msg.get("thoughts").and_then(format_thoughts) {
                    if !thoughts.trim().is_empty() {
                        result.push(SessionMessage {
                            role: "thinking".to_string(),
                            content: thoughts,
                            ts,
                        });
                    }
                }

                let text = content_text(msg.get("content"));
                if !text.trim().is_empty() {
                    result.push(SessionMessage {
                        role: "assistant".to_string(),
                        content: text,
                        ts,
                    });
                }

                for call in msg
                    .get("toolCalls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Some(name) = call
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    else {
                        continue;
                    };
                    let call_content = format_tool_call(name, call.get("args"));
                    result.push(SessionMessage {
                        role: "tool".to_string(),
                        content: call_content,
                        ts,
                    });

                    let output = extract_tool_output(call);
                    let trimmed_out = output.trim();
                    if !trimmed_out.is_empty() {
                        let is_err = call.get("status").and_then(Value::as_str) == Some("error");
                        let label = if is_err { "Error" } else { "Result" };
                        result.push(SessionMessage {
                            role: "tool".to_string(),
                            content: format!("[{label}: {name}]\n{trimmed_out}"),
                            ts,
                        });
                    }
                }
            }
            Some(kind @ ("info" | "error")) => {
                let text = content_text(msg.get("content"));
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let formatted = if kind == "error" && !trimmed.starts_with("[Error]") {
                    format!("[Error] {trimmed}")
                } else {
                    trimmed.to_string()
                };
                result.push(SessionMessage {
                    role: "system".to_string(),
                    content: formatted,
                    ts,
                });
            }
            _ => {}
        }
    }

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
        let path = temp.path().join("session-2026-10-04.jsonl");
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
                    "content": "answer one",
                    "thoughts": [
                        {"subject": "Inspecting main", "description": "Read src/main.rs first."}
                    ],
                    "toolCalls": [
                        {
                            "id": "c1",
                            "name": "read_file",
                            "args": {"path": "src/main.rs"},
                            "status": "success",
                            "resultDisplay": "fn main() {}"
                        }
                    ]
                }),
                json!({"id": "u2", "type": "user", "content": "dropped"}),
                json!({"$rewindTo": "u2"}),
                json!({"id": "e1", "timestamp": "2026-10-04T10:00:04Z", "type": "error", "content": "MCP timeout"}),
            ],
        );

        let msgs = load_messages(&path).expect("load messages");
        let roles_and_text: Vec<(&str, &str)> = msgs
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_str()))
            .collect();
        assert_eq!(
            roles_and_text,
            vec![
                ("user", "first question"),
                ("thinking", "**Inspecting main**\n\nRead src/main.rs first."),
                ("assistant", "answer one"),
                ("tool", "[Tool: read_file]\n{\"path\":\"src/main.rs\"}"),
                ("tool", "[Result: read_file]\nfn main() {}"),
                ("system", "[Error] MCP timeout"),
            ]
        );
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
