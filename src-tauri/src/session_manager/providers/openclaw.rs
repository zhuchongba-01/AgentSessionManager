use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use serde_json::Value;

use crate::session_manager::cache::TranscriptSources;
use crate::session_manager::{SessionMessage, SessionMeta};
use crate::session_paths::openclaw_state_dir;

use super::pi_blocks::PiTranscript;
use super::utils::{
    atomic_write_json, checked_storage_child, ensure_readable_size, extract_text,
    for_each_jsonl_value, is_reparse_or_symlink, is_safe_session_id, parse_timestamp_to_ms,
    path_basename, read_head_tail_lines, remove_path_if_exists, scan_warning, truncate_summary,
    FileParseCache, TITLE_MAX_CHARS,
};

const PROVIDER_ID: &str = "openclaw";
const HEAD_SCAN_LINES: usize = 20;
const TAIL_SCAN_LINES: usize = 30;
const MAX_SESSIONS_INDEX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
struct ParsedOpenClawMeta {
    session_id: String,
    source_path: String,
    cwd: Option<String>,
    first_user_message: Option<String>,
    summary: Option<String>,
    created_at: Option<i64>,
    last_active_at: Option<i64>,
}

static PARSE_CACHE: LazyLock<FileParseCache<ParsedOpenClawMeta>> =
    LazyLock::new(FileParseCache::new);

pub fn session_roots() -> Vec<PathBuf> {
    vec![openclaw_state_dir().join("agents")]
}

pub fn transcript_sources(path: &Path) -> Result<TranscriptSources, String> {
    Ok(vec![(path.to_path_buf(), None)])
}

/// Strip trailing `\n[message_id: ...]` metadata injected by OpenClaw gateway.
fn strip_message_id_suffix(text: &str) -> &str {
    if let Some(pos) = text.rfind("\n[message_id:") {
        text[..pos].trim_end()
    } else {
        text
    }
}

fn is_archive_session_filename(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".jsonl") else {
        return true;
    };
    stem.is_empty()
        || stem.ends_with(".deleted")
        || stem.ends_with(".reset")
        || stem.contains(".deleted.")
        || stem.contains(".reset.")
}

pub fn scan_sessions() -> Vec<SessionMeta> {
    let agents_dir = openclaw_state_dir().join("agents");
    scan_sessions_in_root(&agents_dir, &PARSE_CACHE)
}

fn scan_sessions_in_root(
    agents_dir: &Path,
    cache: &FileParseCache<ParsedOpenClawMeta>,
) -> Vec<SessionMeta> {
    match fs::symlink_metadata(agents_dir) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                scan_warning(format!(
                    "OpenClaw 会话根目录是符号链接或重解析点，已跳过：{}",
                    agents_dir.display()
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
                "无法检查 OpenClaw 会话目录 {}：{err}",
                agents_dir.display()
            ));
            return Vec::new();
        }
    }

    let agent_entries = match fs::read_dir(agents_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            scan_warning(format!(
                "无法扫描 OpenClaw 目录 {}：{err}",
                agents_dir.display()
            ));
            return Vec::new();
        }
    };

    let mut files = Vec::new();
    let mut display_names_by_dir: HashMap<PathBuf, HashMap<String, String>> = HashMap::new();

    for agent_entry in agent_entries {
        let Ok(agent_entry) = agent_entry else {
            continue;
        };
        if !agent_entry.file_type().is_ok_and(|ft| ft.is_dir()) {
            continue;
        }
        let agent_path = agent_entry.path();
        if fs::symlink_metadata(&agent_path).is_ok_and(|m| is_reparse_or_symlink(&m)) {
            continue;
        }

        let sessions_dir = agent_path.join("sessions");
        match fs::symlink_metadata(&sessions_dir) {
            Ok(meta) if !is_reparse_or_symlink(&meta) && meta.is_dir() => {}
            _ => continue,
        }

        let names = load_display_names(&sessions_dir);
        if !names.is_empty() {
            display_names_by_dir.insert(sessions_dir.clone(), names);
        }

        let Ok(ses_entries) = fs::read_dir(&sessions_dir) else {
            continue;
        };
        for entry in ses_entries.flatten() {
            if !entry.file_type().is_ok_and(|ft| ft.is_file()) {
                continue;
            }
            let path = entry.path();
            if fs::symlink_metadata(&path).is_ok_and(|m| is_reparse_or_symlink(&m)) {
                continue;
            }
            let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if is_archive_session_filename(fname) {
                continue;
            }
            files.push(path);
        }
    }

    let parsed = cache.scan(files, parse_openclaw_meta);
    parsed
        .into_iter()
        .map(|item| {
            let display_name = Path::new(&item.source_path)
                .parent()
                .and_then(|dir| display_names_by_dir.get(dir))
                .and_then(|map| map.get(&item.session_id))
                .filter(|s| !s.trim().is_empty());

            let title = display_name
                .map(|t| truncate_summary(t, TITLE_MAX_CHARS))
                .or_else(|| {
                    item.first_user_message
                        .as_deref()
                        .map(|t| truncate_summary(t, TITLE_MAX_CHARS))
                })
                .or_else(|| item.cwd.as_deref().and_then(path_basename));

            let summary = item.summary.as_deref().map(|s| truncate_summary(s, 160));
            let project_name = item.cwd.as_deref().and_then(path_basename);

            SessionMeta {
                provider_id: PROVIDER_ID.to_string(),
                session_id: item.session_id,
                residual: false,
                parent_session_id: None,
                archived: false,
                cleanup_pending: false,
                title,
                summary,
                project_dir: item.cwd,
                project_name,
                sidebar_section: None,
                created_at: item.created_at,
                last_active_at: item.last_active_at.or(item.created_at),
                source_path: Some(item.source_path),
                resume_command: None, // OpenClaw sessions are gateway-managed, no CLI resume
            }
        })
        .collect()
}

fn load_display_names(sessions_dir: &Path) -> HashMap<String, String> {
    let index_path = sessions_dir.join("sessions.json");
    let Ok(meta) = fs::symlink_metadata(&index_path) else {
        return HashMap::new();
    };
    if is_reparse_or_symlink(&meta)
        || !meta.file_type().is_file()
        || meta.len() > MAX_SESSIONS_INDEX_BYTES
    {
        return HashMap::new();
    }
    let mut content = String::new();
    let Ok(file) = File::open(&index_path) else {
        return HashMap::new();
    };
    if file
        .take(MAX_SESSIONS_INDEX_BYTES + 1)
        .read_to_string(&mut content)
        .is_err()
    {
        return HashMap::new();
    }
    let Ok(index) = serde_json::from_str::<serde_json::Map<String, Value>>(&content) else {
        return HashMap::new();
    };

    let mut map = HashMap::new();
    for (_key, entry) in &index {
        if let (Some(id), Some(name)) = (
            entry.get("sessionId").and_then(Value::as_str),
            entry.get("displayName").and_then(Value::as_str),
        ) {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                map.insert(id.to_string(), trimmed.to_string());
            }
        }
    }
    map
}

fn parse_openclaw_meta(path: &Path) -> Result<Option<ParsedOpenClawMeta>, String> {
    let (head, tail) = read_head_tail_lines(path, HEAD_SCAN_LINES, TAIL_SCAN_LINES)
        .map_err(|err| format!("无法读取 OpenClaw 会话 {}：{err}", path.display()))?;

    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut created_at: Option<i64> = None;
    let mut summary: Option<String> = None;
    let mut first_user_message: Option<String> = None;

    for line in &head {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if created_at.is_none() {
            created_at = value.get("timestamp").and_then(parse_timestamp_to_ms);
        }

        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        if event_type == "session" {
            if session_id.is_none() {
                session_id = value
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
            }
            if cwd.is_none() {
                cwd = value
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
            }
            if let Some(ts) = value.get("timestamp").and_then(parse_timestamp_to_ms) {
                created_at.get_or_insert(ts);
            }
            continue;
        }

        if event_type == "message" {
            if let Some(message) = value.get("message") {
                let role = message.get("role").and_then(Value::as_str).unwrap_or("");
                if matches!(role, "user" | "assistant") {
                    let raw = message
                        .get("content")
                        .map(extract_visible_without_tools)
                        .unwrap_or_default();
                    let cleaned = if role == "user" {
                        strip_message_id_suffix(&raw).trim()
                    } else {
                        raw.trim()
                    };
                    if !cleaned.is_empty() {
                        if first_user_message.is_none() && role == "user" {
                            first_user_message = Some(cleaned.to_string());
                        }
                        if summary.is_none() {
                            summary = Some(cleaned.to_string());
                        }
                    }
                }
            }
        }
    }

    let mut last_active_at: Option<i64> = None;
    for line in tail.iter().rev() {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if last_active_at.is_none() {
            last_active_at = value.get("timestamp").and_then(parse_timestamp_to_ms);
        }
        if value.get("type").and_then(Value::as_str) == Some("message") {
            if let Some(message) = value.get("message") {
                let role = message.get("role").and_then(Value::as_str).unwrap_or("");
                if matches!(role, "user" | "assistant") {
                    let raw = message
                        .get("content")
                        .map(extract_visible_without_tools)
                        .unwrap_or_default();
                    let cleaned = if role == "user" {
                        strip_message_id_suffix(&raw).trim()
                    } else {
                        raw.trim()
                    };
                    if !cleaned.is_empty() {
                        summary = Some(cleaned.to_string());
                        break;
                    }
                }
            }
        }
    }
    if last_active_at.is_none() {
        last_active_at = tail.iter().rev().find_map(|line| {
            serde_json::from_str::<Value>(line.trim())
                .ok()
                .and_then(|v| v.get("timestamp").and_then(parse_timestamp_to_ms))
        });
    }

    let session_id = session_id.or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    });
    let Some(session_id) = session_id else {
        return Ok(None);
    };

    Ok(Some(ParsedOpenClawMeta {
        session_id,
        source_path: path.to_string_lossy().to_string(),
        cwd,
        first_user_message,
        summary,
        created_at,
        last_active_at,
    }))
}

fn extract_visible_without_tools(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter(|item| {
                !matches!(
                    item.get("type").and_then(Value::as_str),
                    Some(
                        "toolCall"
                            | "tool_use"
                            | "server_tool_use"
                            | "mcp_tool_use"
                            | "thinking"
                            | "reasoning"
                            | "redacted_thinking"
                    )
                )
            })
            .map(extract_text)
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => extract_text(content),
        _ => String::new(),
    }
}

/// OpenClaw 与 Pi 同构，复用 [`PiTranscript`] 按文件顺序解析完整结构化块。
pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    ensure_readable_size(path)?;
    let mut transcript = PiTranscript::for_openclaw();
    for_each_jsonl_value(path, |span, value| {
        let id = value.get("id").and_then(Value::as_str).map(str::to_string);
        transcript.push_entry(&value, span, id);
        Ok(())
    })?;
    Ok(transcript.finish())
}

fn validate_openclaw_session_path(
    root: &Path,
    path: &Path,
    session_id: &str,
) -> Result<(PathBuf, PathBuf, bool), String> {
    if !is_safe_session_id(session_id) {
        return Err(format!("Invalid OpenClaw session ID: {session_id}"));
    }
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(format!("Invalid OpenClaw session path: {}", path.display()));
    }
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return Err(format!(
            "Invalid OpenClaw session file extension: {}",
            path.display()
        ));
    }

    let file_exists = match fs::symlink_metadata(path) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                return Err(format!(
                    "OpenClaw session source must not be a symlink or reparse point: {}",
                    path.display()
                ));
            }
            if !meta.file_type().is_file() {
                return Err(format!(
                    "OpenClaw session source is not a regular file: {}",
                    path.display()
                ));
            }
            true
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            return Err(format!(
                "Failed to inspect OpenClaw session {}: {err}",
                path.display()
            ))
        }
    };

    let canonical_root = root.canonicalize().map_err(|err| {
        format!(
            "Failed to resolve OpenClaw agents root {}: {err}",
            root.display()
        )
    })?;
    let root_meta = fs::metadata(&canonical_root).map_err(|err| {
        format!(
            "Failed to inspect OpenClaw agents root {}: {err}",
            canonical_root.display()
        )
    })?;
    if !root_meta.is_dir() {
        return Err(format!(
            "OpenClaw agents root is not a directory: {}",
            canonical_root.display()
        ));
    }

    let resolved_path = checked_storage_child(root, path)?;
    let relative = resolved_path.strip_prefix(&canonical_root).map_err(|_| {
        format!(
            "OpenClaw session path is outside agents root: {}",
            path.display()
        )
    })?;
    let components: Vec<_> = relative.components().collect();
    if components.len() != 3
        || !components.iter().all(|c| matches!(c, Component::Normal(_)))
        || components[1].as_os_str() != OsStr::new("sessions")
    {
        return Err(format!(
            "OpenClaw session path must be <agents>/<agent>/sessions/<file>.jsonl: {}",
            path.display()
        ));
    }

    if file_exists {
        let meta = parse_openclaw_meta(&resolved_path)?.ok_or_else(|| {
            format!(
                "Failed to parse OpenClaw session metadata: {}",
                path.display()
            )
        })?;
        if meta.session_id != session_id {
            return Err(format!(
                "OpenClaw session ID mismatch: expected {session_id}, found {}",
                meta.session_id
            ));
        }
    } else {
        let stem = resolved_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if stem != session_id {
            return Err(format!(
                "Missing OpenClaw session filename does not match session ID ({session_id}): {}",
                path.display()
            ));
        }
    }

    let sessions_dir = resolved_path
        .parent()
        .ok_or_else(|| format!("Invalid OpenClaw sessions directory: {}", path.display()))?;
    let index_path = sessions_dir.join("sessions.json");
    match fs::symlink_metadata(&index_path) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                return Err(format!(
                    "Refusing to modify symlinked OpenClaw sessions.json: {}",
                    index_path.display()
                ));
            }
            if !meta.file_type().is_file() {
                return Err(format!(
                    "OpenClaw sessions.json is not a regular file: {}",
                    index_path.display()
                ));
            }
            if meta.len() > MAX_SESSIONS_INDEX_BYTES {
                return Err(format!(
                    "OpenClaw sessions.json exceeds 16 MiB limit: {}",
                    index_path.display()
                ));
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "Failed to inspect OpenClaw sessions.json {}: {err}",
                index_path.display()
            ))
        }
    }

    Ok((canonical_root, resolved_path, file_exists))
}

pub fn preflight_delete(root: &Path, path: &Path, session_id: &str) -> Result<(), String> {
    validate_openclaw_session_path(root, path, session_id).map(|_| ())
}

fn same_session_file(entry_file: &str, raw_path: &Path, resolved_path: &Path) -> bool {
    let entry_path = Path::new(entry_file);
    if entry_path == raw_path || entry_path == resolved_path {
        return true;
    }
    entry_path
        .canonicalize()
        .ok()
        .is_some_and(|canon| canon == resolved_path)
}

fn prune_sessions_index(
    canonical_root: &Path,
    sessions_dir: &Path,
    session_id: &str,
    raw_path: &Path,
    resolved_path: &Path,
) -> Result<bool, String> {
    let index_candidate = sessions_dir.join("sessions.json");
    let meta = match fs::symlink_metadata(&index_candidate) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(format!(
                "Failed to inspect OpenClaw sessions.json {}: {err}",
                index_candidate.display()
            ))
        }
    };
    if is_reparse_or_symlink(&meta) || !meta.file_type().is_file() {
        return Err(format!(
            "Refusing to modify non-regular OpenClaw sessions.json: {}",
            index_candidate.display()
        ));
    }
    if meta.len() > MAX_SESSIONS_INDEX_BYTES {
        return Err(format!(
            "OpenClaw sessions.json exceeds 16 MiB limit: {}",
            index_candidate.display()
        ));
    }

    let checked_index = checked_storage_child(canonical_root, &index_candidate)?;
    let mut raw = String::new();
    File::open(&checked_index)
        .map_err(|e| format!("Failed to open OpenClaw sessions.json: {e}"))?
        .take(MAX_SESSIONS_INDEX_BYTES + 1)
        .read_to_string(&mut raw)
        .map_err(|e| format!("Failed to read OpenClaw sessions.json: {e}"))?;

    let mut index: serde_json::Map<String, Value> = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "Failed to parse OpenClaw sessions.json {}: {e}",
            checked_index.display()
        )
    })?;

    let before = index.len();
    index.retain(|_, entry| {
        let same_id = entry.get("sessionId").and_then(Value::as_str) == Some(session_id);
        let same_file = entry
            .get("sessionFile")
            .and_then(Value::as_str)
            .is_some_and(|f| same_session_file(f, raw_path, resolved_path));
        !(same_id || same_file)
    });

    if index.len() == before {
        return Ok(false);
    }
    atomic_write_json(&checked_index, &index)?;
    Ok(true)
}

fn cleanup_archive_siblings(
    canonical_root: &Path,
    sessions_dir: &Path,
    resolved_path: &Path,
) -> Result<bool, String> {
    let Some(file_name) = resolved_path.file_name().and_then(|n| n.to_str()) else {
        return Ok(false);
    };
    let Some(stem) = resolved_path.file_stem().and_then(|s| s.to_str()) else {
        return Ok(false);
    };
    let entries = match fs::read_dir(sessions_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(format!(
                "Failed to scan OpenClaw sessions directory {}: {err}",
                sessions_dir.display()
            ))
        }
    };

    let prefixes = [
        format!("{file_name}.deleted"),
        format!("{file_name}.reset"),
        format!("{stem}.deleted."),
        format!("{stem}.reset."),
    ];

    let mut removed = false;
    for entry in entries.flatten() {
        let candidate = entry.path();
        let Some(cname) = candidate.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !prefixes.iter().any(|p| cname.starts_with(p.as_str())) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&candidate) else {
            continue;
        };
        if is_reparse_or_symlink(&meta) || !meta.file_type().is_file() {
            continue;
        }
        let checked = checked_storage_child(canonical_root, &candidate)?;
        remove_path_if_exists(&checked).map_err(|e| {
            format!(
                "Failed to delete OpenClaw archive {}: {e}",
                checked.display()
            )
        })?;
        removed = true;
    }
    Ok(removed)
}

pub fn delete_session(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    let (canonical_root, resolved_path, file_exists) =
        validate_openclaw_session_path(root, path, session_id)?;
    let sessions_dir = resolved_path
        .parent()
        .ok_or_else(|| format!("Invalid OpenClaw sessions directory: {}", path.display()))?;

    let mut removed = false;
    if prune_sessions_index(
        &canonical_root,
        sessions_dir,
        session_id,
        path,
        &resolved_path,
    )? {
        removed = true;
    }

    if cleanup_archive_siblings(&canonical_root, sessions_dir, &resolved_path)? {
        removed = true;
    }

    if file_exists {
        match fs::symlink_metadata(&resolved_path) {
            Ok(meta) if is_reparse_or_symlink(&meta) => {
                return Err(format!(
                    "OpenClaw session source turned into a symlink before deletion: {}",
                    resolved_path.display()
                ));
            }
            Ok(_) => {
                remove_path_if_exists(&resolved_path).map_err(|err| {
                    format!(
                        "Failed to delete OpenClaw session file {}: {err}",
                        resolved_path.display()
                    )
                })?;
                removed = true;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(format!(
                    "Failed to inspect OpenClaw session file {}: {err}",
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

    #[cfg(unix)]
    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_file(target, link)
    }

    #[test]
    fn scans_display_names_strips_gateway_message_id_and_skips_archives() {
        let temp = tempdir().unwrap();
        let agents = temp.path().join("agents");
        let sessions_dir = agents.join("main").join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();

        let p1 = sessions_dir.join("session-abc.jsonl");
        fs::write(
            &p1,
            concat!(
                "{\"type\":\"session\",\"id\":\"session-abc\",\"cwd\":\"/tmp/my-app\",\"timestamp\":\"2026-03-06T10:00:00Z\"}\n",
                "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"How do I deploy?\\n[message_id: gw-99]\"},\"timestamp\":\"2026-03-06T10:01:00Z\"}\n",
                "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":\"Here is how...\"},\"timestamp\":\"2026-03-06T10:02:00Z\"}\n"
            ),
        )
        .unwrap();

        // Archive siblings must not be scanned as active sessions
        fs::write(
            sessions_dir.join("session-abc.jsonl.deleted.170000"),
            "{}\n",
        )
        .unwrap();
        fs::write(sessions_dir.join("session-old.reset.jsonl"), "{}\n").unwrap();

        let cache = FileParseCache::new();
        let first = scan_sessions_in_root(&agents, &cache);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].title.as_deref(), Some("How do I deploy?"));
        assert_eq!(first[0].summary.as_deref(), Some("Here is how..."));
        assert_eq!(first[0].project_dir.as_deref(), Some("/tmp/my-app"));
        assert_eq!(first[0].project_name.as_deref(), Some("my-app"));
        assert!(first[0].resume_command.is_none());

        // Adding displayName in sessions.json overrides title even when .jsonl hits PARSE_CACHE
        fs::write(
            sessions_dir.join("sessions.json"),
            r#"{"agent:main:main": {"sessionId": "session-abc", "displayName": "重构部署流水线"}}"#,
        )
        .unwrap();
        let second = scan_sessions_in_root(&agents, &cache);
        assert_eq!(second[0].title.as_deref(), Some("重构部署流水线"));
    }

    #[test]
    fn load_messages_separates_thinking_tools_results_and_compaction() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("session-1.jsonl");
        let lines = [
            json!({"type":"session","id":"session-1","cwd":"/tmp/p","timestamp":"2026-03-06T10:00:00Z"}),
            json!({"type":"message","timestamp":"2026-03-06T10:01:00Z","message":{"role":"user","content":"Deploy app\n[message_id: m1]"}}),
            json!({"type":"message","timestamp":"2026-03-06T10:02:00Z","message":{
                "role":"assistant",
                "content":[
                    {"type":"thinking","thinking":"Checking package.json"},
                    {"type":"text","text":"Let me read the config."},
                    {"type":"toolCall","id":"c1","name":"read","arguments":{"path":"package.json"}}
                ]
            }}),
            json!({"type":"message","timestamp":"2026-03-06T10:03:00Z","message":{
                "role":"toolResult",
                "toolCallId":"c1",
                "toolName":"read",
                "isError":false,
                "content":[{"type":"text","text":"{\"name\":\"app\"}"}]
            }}),
            json!({"type":"compaction","timestamp":"2026-03-06T10:04:00Z","summary":"Config verified."}),
        ];
        fs::write(
            &path,
            lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
        )
        .unwrap();

        use crate::session_manager::model::{EventKind, SessionBlock, ToolKind, ToolStatus};

        let msgs = load_messages(&path).unwrap();
        let pairs: Vec<(&str, &str, Option<&str>)> = msgs
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_str(), m.turn_id.as_deref()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("user", "Deploy app", Some("t1")),
                (
                    "assistant",
                    "Let me read the config.\n\n[Tool: read] package.json",
                    Some("t1")
                ),
                ("tool", "{\"name\":\"app\"}", Some("t1")),
                ("system", "Config verified.", Some("t1")),
            ]
        );
        assert!(matches!(
            &msgs[1].blocks[0],
            SessionBlock::Thinking { text, .. } if text == "Checking package.json"
        ));
        assert!(matches!(
            &msgs[1].blocks[2],
            SessionBlock::ToolCall { id, kind: ToolKind::Read, title, .. }
                if id == "c1" && title == "package.json"
        ));
        assert!(matches!(
            &msgs[2].blocks[0],
            SessionBlock::ToolResult { call_id, status: ToolStatus::Success, .. }
                if call_id == "c1"
        ));
        assert!(matches!(
            &msgs[3].blocks[0],
            SessionBlock::Event {
                kind: EventKind::Compaction,
                ..
            }
        ));
    }

    #[test]
    fn delete_session_prunes_sessions_json_removes_archives_and_rejects_symlinks() {
        let temp = tempdir().unwrap();
        let agents = temp.path().join("agents");
        let sessions_dir = agents.join("main").join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();

        let target = sessions_dir.join("session-123.jsonl");
        let archive = sessions_dir.join("session-123.jsonl.reset.170000");
        let keep = sessions_dir.join("session-456.jsonl");

        fs::write(
            &target,
            "{\"type\":\"session\",\"id\":\"session-123\"}\n{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        )
        .unwrap();
        fs::write(&archive, "{}\n").unwrap();
        fs::write(
            &keep,
            "{\"type\":\"session\",\"id\":\"session-456\"}\n{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"keep\"}}\n",
        )
        .unwrap();

        fs::write(
            sessions_dir.join("sessions.json"),
            serde_json::to_string(&json!({
                "agent:main:main": {
                    "sessionId": "session-123",
                    "sessionFile": target.to_string_lossy()
                },
                "agent:main:other": {
                    "sessionId": "session-456",
                    "sessionFile": keep.to_string_lossy()
                }
            }))
            .unwrap(),
        )
        .unwrap();

        // Symlinked session file must be rejected
        let link = sessions_dir.join("session-link.jsonl");
        if create_file_symlink(&keep, &link).is_ok() {
            assert!(preflight_delete(&agents, &link, "session-456").is_err());
            assert!(delete_session(&agents, &link, "session-456").is_err());
            assert!(keep.exists());
            let _ = fs::remove_file(&link);
        }

        preflight_delete(&agents, &target, "session-123").unwrap();
        assert!(delete_session(&agents, &target, "session-123").unwrap());
        assert!(!target.exists());
        assert!(!archive.exists());
        assert!(keep.exists());

        let updated: Value =
            serde_json::from_str(&fs::read_to_string(sessions_dir.join("sessions.json")).unwrap())
                .unwrap();
        assert!(updated.get("agent:main:main").is_none());
        assert!(updated.get("agent:main:other").is_some());
    }
}
