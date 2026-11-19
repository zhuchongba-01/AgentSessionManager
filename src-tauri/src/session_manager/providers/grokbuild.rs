use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use serde::Deserialize;
use serde_json::Value;

use crate::session_manager::{DeleteSessionRequest, SessionMessage, SessionMeta};

use super::utils::{
    checked_storage_child, collect_files_where, collect_files_where_checked, ensure_readable_size,
    extract_text, is_safe_session_id, order_selected_children, parse_timestamp_to_ms,
    truncate_summary, validate_parent_graph, FileParseCache, TITLE_MAX_CHARS,
};

#[derive(Debug, Deserialize)]
struct GrokSessionInfo {
    id: String,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GrokSessionSummary {
    info: GrokSessionInfo,
    #[serde(default)]
    session_summary: Option<String>,
    #[serde(default)]
    generated_title: Option<String>,
    #[serde(default)]
    created_at: Option<Value>,
    #[serde(default)]
    updated_at: Option<Value>,
    #[serde(default)]
    last_active_at: Option<Value>,
    #[serde(default)]
    parent_session_id: Option<String>,
    #[serde(default)]
    session_kind: Option<String>,
    #[serde(default)]
    chat_format_version: Option<u64>,
}

pub fn session_roots() -> Vec<PathBuf> {
    let config_dir = crate::session_paths::grok_config_dir();
    vec![
        config_dir.join("sessions"),
        config_dir.join("archived_sessions"),
    ]
}

static PARSE_CACHE: LazyLock<FileParseCache> = LazyLock::new(FileParseCache::new);

pub fn scan_sessions() -> Vec<SessionMeta> {
    let mut summaries = Vec::new();
    for root in session_roots() {
        collect_summary_files(&root, &mut summaries);
    }
    PARSE_CACHE.scan(summaries, parse_discovered_summary)
}

fn parse_discovered_summary(path: &Path) -> Result<Option<SessionMeta>, String> {
    Ok(parse_summary_result(path)?.map(|mut session| {
        if is_archived_summary(path) {
            session.residual = true;
            session.resume_command = None;
        }
        session
    }))
}

fn is_archived_summary(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case("archived_sessions"))
    })
}

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    let session_dir = path
        .parent()
        .ok_or_else(|| format!("Invalid Grok Build session path: {}", path.display()))?;
    let chat_path = session_dir.join("chat_history.jsonl");
    let needs_rebuild = match std::fs::metadata(&chat_path) {
        Ok(meta) => meta.len() == 0,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => {
            return Err(format!(
                "Failed to inspect Grok Build chat history: {error}"
            ))
        }
    };
    if needs_rebuild && read_summary(path)?.chat_format_version == Some(1) {
        let updates_path = session_dir.join("updates.jsonl");
        ensure_readable_size(&updates_path)?;
        let file = File::open(&updates_path)
            .map_err(|error| format!("Failed to open Grok Build updates: {error}"))?;
        return read_update_messages(BufReader::new(file));
    }
    ensure_readable_size(&chat_path)?;
    let file = File::open(&chat_path)
        .map_err(|e| format!("Failed to open Grok Build chat history: {e}"))?;
    read_chat_messages(BufReader::new(file))
}

/// Cache dependencies include missing files, so creating/removing the derived cache
/// or changing the durable update stream invalidates an already-loaded transcript.
pub fn transcript_sources(path: &Path) -> Result<Vec<(PathBuf, Option<u64>)>, String> {
    let dir = path.parent().ok_or("Invalid Grok Build session path")?;
    // None is the cache API's unbounded-read marker, not file existence/length.
    // Its fingerprint layer records metadata and missing-file state separately.
    Ok([
        path.to_path_buf(),
        dir.join("chat_history.jsonl"),
        dir.join("updates.jsonl"),
    ]
    .into_iter()
    .map(|source| (source, None))
    .collect())
}

fn read_chat_messages(reader: impl BufRead) -> Result<Vec<SessionMessage>, String> {
    let mut messages = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.map_err(|error| {
            format!(
                "Failed to read Grok Build chat history at line {}: {error}",
                index + 1
            )
        })?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(message) = conversation_message(&value) {
            messages.push(message);
        }
    }

    Ok(messages)
}

fn conversation_message(value: &Value) -> Option<SessionMessage> {
    let kind = value.get("type").and_then(Value::as_str)?;
    let role = match kind {
        "system" | "user" | "assistant" | "tool" => kind,
        "tool_result" => "tool",
        "backend_tool_call" => "assistant",
        _ => return None, // Never expose encrypted/internal reasoning records.
    };
    let mut content = value.get("content").map(extract_text).unwrap_or_default();
    if kind == "assistant" {
        if let Some(calls) = value.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let name = call.get("name").and_then(Value::as_str).unwrap_or("tool");
                let args = call.get("arguments").and_then(Value::as_str).unwrap_or("");
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(&format!("[Tool: {name}] {args}"));
            }
        }
    } else if kind == "backend_tool_call" {
        // `kind` is a public, structured backend tool descriptor, not reasoning.
        if let Some(tool) = value.get("kind") {
            content = format!("[Backend tool] {tool}");
        }
    }
    if content.trim().is_empty() {
        return None;
    }
    Some(SessionMessage {
        role: role.to_string(),
        content,
        ts: value
            .get("timestamp")
            .or_else(|| value.get("ts"))
            .and_then(parse_timestamp_to_ms),
    })
}

/// Read-only adaptation of xai-org/grok-build main storage/mod.rs chat_rebuild.
/// Only the verified v1 envelope/raw ACP format is interpreted; no old chat schema guessing.
/// Unlike native rebuild this never creates or rewrites chat_history.jsonl.
fn read_update_messages(reader: impl BufRead) -> Result<Vec<SessionMessage>, String> {
    #[derive(Default)]
    struct Reducer {
        user: Vec<Value>,
        agent: String,
        calls: Vec<Value>,
        in_user: bool,
        interjection: bool,
        emitted: HashSet<String>,
        args: HashMap<String, String>,
        output: Vec<SessionMessage>,
    }
    impl Reducer {
        fn emit(&mut self, value: Value) {
            if let Some(message) = conversation_message(&value) {
                self.output.push(message);
            }
        }
        fn flush_user(&mut self) {
            let parts = std::mem::take(&mut self.user);
            self.interjection = false;
            if !parts.is_empty() {
                self.emit(serde_json::json!({"type":"user","content":parts}));
            }
        }
        fn flush_agent(&mut self) {
            let text = std::mem::take(&mut self.agent);
            let calls = std::mem::take(&mut self.calls);
            if !text.is_empty() || !calls.is_empty() {
                self.emit(
                    serde_json::json!({"type":"assistant","content":text,"tool_calls":calls}),
                );
            }
        }
    }
    let mut state = Reducer::default();
    for (index, line) in reader.lines().enumerate() {
        let line = line.map_err(|error| {
            format!(
                "Failed to read Grok Build updates at line {}: {error}",
                index + 1
            )
        })?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("session/update");
        let params = if value.get("method").is_some() {
            &value["params"]
        } else {
            &value
        };
        let update = &params["update"];
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or("");
        if method == "_x.ai/session/update" {
            if kind == "compaction_checkpoint" {
                state = Reducer::default();
            }
            continue;
        }
        if method != "session/update" {
            continue;
        }
        match kind {
            "user_message_chunk" | "agent_message_chunk" => {
                if update.pointer("/_meta/hostTurn").and_then(Value::as_bool) == Some(true) {
                    state.flush_user();
                    state.flush_agent();
                    state.in_user = false;
                    continue;
                }
                let content = &update["content"];
                let text = (content.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| content.get("text").and_then(Value::as_str))
                    .flatten();
                if kind == "user_message_chunk" {
                    if !state.in_user {
                        state.flush_agent();
                        state.in_user = true;
                    }
                    let interjection = update
                        .pointer("/_meta/interjection")
                        .and_then(Value::as_bool)
                        == Some(true);
                    if interjection != state.interjection || (interjection && text.is_some()) {
                        state.flush_user();
                    }
                    state.interjection = interjection;
                    if let Some(text) = text {
                        state
                            .user
                            .push(serde_json::json!({"type":"text","text":text}));
                    }
                } else {
                    if state.in_user {
                        state.flush_user();
                        state.in_user = false;
                    }
                    if let Some(text) = text {
                        state.agent.push_str(text);
                    }
                }
            }
            "tool_call" => {
                let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
                    continue;
                };
                let args = update
                    .get("rawInput")
                    .filter(|v| !v.is_null())
                    .map(Value::to_string)
                    .unwrap_or_default();
                state.args.insert(id.to_string(), args.clone());
                state.calls.push(serde_json::json!({"id":id,"name":update.get("title").and_then(Value::as_str).unwrap_or("tool"),"arguments":args}));
            }
            "tool_call_update" => {
                let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
                    continue;
                };
                if state.args.get(id).is_none_or(String::is_empty) {
                    if let Some(raw) = update.get("rawInput").filter(|v| !v.is_null()) {
                        let args = raw.to_string();
                        state.args.insert(id.to_string(), args.clone());
                        if let Some(call) = state
                            .calls
                            .iter_mut()
                            .find(|c| c["id"].as_str() == Some(id))
                        {
                            call["arguments"] = Value::String(args);
                        }
                    }
                }
                if !matches!(
                    update.get("status").and_then(Value::as_str),
                    Some("completed" | "failed")
                ) || !state.emitted.insert(id.to_string())
                {
                    continue;
                }
                state.flush_agent();
                let mut text = update
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|c| {
                        c.get("type").and_then(Value::as_str) == Some("content")
                            && c.pointer("/content/type").and_then(Value::as_str) == Some("text")
                    })
                    .filter_map(|c| c.pointer("/content/text").and_then(Value::as_str))
                    .collect::<String>();
                if text.is_empty() {
                    text = update
                        .get("rawOutput")
                        .filter(|v| !v.is_null())
                        .map(Value::to_string)
                        .unwrap_or_default();
                }
                state.emit(
                    serde_json::json!({"type":"tool_result","tool_call_id":id,"content":text}),
                );
            }
            _ => {}
        }
    }
    state.flush_user();
    state.flush_agent();
    Ok(state.output)
}

fn parent_relations_in_roots(roots: &[PathBuf]) -> Result<HashMap<String, String>, String> {
    let mut relations = HashMap::new();
    let mut seen_paths = HashSet::new();
    for root in roots {
        let mut summaries = Vec::new();
        collect_files_where_checked(
            root,
            |path| path.file_name().and_then(|name| name.to_str()) == Some("summary.json"),
            &mut summaries,
        )?;
        if summaries.is_empty() {
            continue;
        }
        let root = root
            .canonicalize()
            .map_err(|error| format!("Failed to resolve Grok Build session root: {error}"))?;
        summaries.sort();
        for summary_path in summaries {
            let summary_path = checked_storage_child(&root, &summary_path)
                .map_err(|error| format!("Invalid Grok Build summary path: {error}"))?;
            if !seen_paths.insert(summary_path.clone()) {
                continue;
            }
            let summary = read_summary(&summary_path)?;
            let child = summary.info.id;
            if !is_safe_session_id(&child) {
                return Err("Grok Build summary contains an invalid session ID".into());
            }
            let Some(parent) = summary
                .parent_session_id
                .filter(|parent| !parent.trim().is_empty())
            else {
                continue;
            };
            if !is_safe_session_id(&parent) {
                return Err(format!(
                    "Grok Build 会话 {child} 的父会话 ID 无效，已停止删除"
                ));
            }
            if child == parent {
                return Err(format!(
                    "Grok Build 会话 {child} 的父子关系指向自身，已停止删除"
                ));
            }
            if let Some(existing) = relations.insert(child.clone(), parent.clone()) {
                if existing != parent {
                    return Err(format!(
                        "Grok Build 会话 {child} 同时指向父会话 {existing} 和 {parent}，已停止删除"
                    ));
                }
            }
        }
    }

    validate_parent_graph(&relations, "Grok Build")?;
    Ok(relations)
}

/// Keep the persisted source path authoritative even after summary.json has
/// disappeared. Never resolve a session/project link into a different directory.
fn checked_delete_target(root: &Path, path: &Path, session_id: &str) -> Result<PathBuf, String> {
    if !is_safe_session_id(session_id) {
        return Err("Invalid Grok Build session ID".into());
    }
    if !root.is_absolute()
        || !path.is_absolute()
        || root
            .components()
            .chain(path.components())
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err("Invalid Grok Build absolute storage path".into());
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("Failed to resolve Grok Build session root: {error}"))?;
    let relative = path
        .strip_prefix(root)
        .or_else(|_| path.strip_prefix(&canonical_root))
        .map_err(|_| "Grok Build session source is outside the session root")?;
    let parts = relative.components().collect::<Vec<_>>();
    if parts.len() != 3
        || !parts
            .iter()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
        || parts[1].as_os_str() != std::ffi::OsStr::new(session_id)
        || parts[2].as_os_str() != "summary.json"
    {
        return Err("Unexpected Grok Build session source or session ID".into());
    }
    let mut target = canonical_root;
    for (index, part) in parts.iter().enumerate() {
        target.push(part.as_os_str());
        match std::fs::symlink_metadata(&target) {
            Ok(meta) => {
                #[cfg(windows)]
                let is_link = {
                    use std::os::windows::fs::MetadataExt;
                    meta.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
                };
                #[cfg(not(windows))]
                let is_link = meta.file_type().is_symlink();
                if is_link || (index < 2 && !meta.is_dir()) || (index == 2 && !meta.is_file()) {
                    return Err(format!(
                        "Invalid Grok Build session path component: {}",
                        target.display()
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "Failed to inspect Grok Build session path: {error}"
                ))
            }
        }
    }
    Ok(target)
}

pub(in crate::session_manager) fn preflight_delete_in_roots(
    roots: &[PathBuf],
    path: &Path,
    session_id: &str,
) -> Result<(), String> {
    let target = roots
        .iter()
        .find_map(|root| checked_delete_target(root, path, session_id).ok())
        .ok_or("Invalid Grok Build session source, session ID, or path outside the session root")?;
    match std::fs::symlink_metadata(&target) {
        Ok(_) => {
            let summary = read_summary(&target)?;
            if summary.info.id != session_id {
                return Err(format!(
                    "Grok Build session ID mismatch: expected {session_id}, found {}",
                    summary.info.id
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Failed to inspect Grok Build summary: {error}")),
    }
    let relations = parent_relations_in_roots(roots)?;
    if let Some(child) = relations
        .iter()
        .find_map(|(child, parent)| (parent == session_id).then_some(child))
    {
        return Err(format!(
            "该 Grok Build 会话仍被子会话 {child} 依赖，请先删除子会话"
        ));
    }
    Ok(())
}

fn deletion_order_with_relations(
    requests: &[DeleteSessionRequest],
    initial: &[usize],
    relations: &HashMap<String, String>,
) -> Vec<usize> {
    let selected = requests
        .iter()
        .enumerate()
        .filter(|(_, request)| request.provider_id == "grokbuild")
        .map(|(index, request)| {
            (
                index,
                request.session_id.clone(),
                relations.get(&request.session_id).cloned(),
            )
        })
        .collect::<Vec<_>>();
    order_selected_children(&selected, initial)
}

pub fn deletion_order(requests: &[DeleteSessionRequest], initial: &[usize]) -> Vec<usize> {
    if !requests
        .iter()
        .any(|request| request.provider_id == "grokbuild")
    {
        return initial.to_vec();
    }
    let Ok(relations) = parent_relations_in_roots(&session_roots()) else {
        return initial.to_vec();
    };
    deletion_order_with_relations(requests, initial, &relations)
}

pub fn delete_session_in_roots(
    roots: &[PathBuf],
    path: &Path,
    session_id: &str,
) -> Result<bool, String> {
    preflight_delete_in_roots(roots, path, session_id)?;
    // Revalidate immediately before removal, including missing-source ancestors.
    let target = roots
        .iter()
        .find_map(|root| checked_delete_target(root, path, session_id).ok())
        .ok_or("Invalid Grok Build session deletion target")?;
    let session_dir = target
        .parent()
        .ok_or("Invalid Grok Build session directory")?;
    match std::fs::remove_dir_all(session_dir) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // NotFound can originate from a child during recursive removal; only
            // an absent directory proves that the persisted task is complete.
            match std::fs::symlink_metadata(session_dir) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
                _ => Err(format!(
                    "Grok Build session directory still needs cleanup: {}",
                    session_dir.display()
                )),
            }
        }
        Err(error) => Err(format!(
            "Failed to delete Grok Build session directory {}: {error}",
            session_dir.display()
        )),
    }
}

#[cfg(test)]
fn delete_session(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    delete_session_in_roots(&[root.to_path_buf()], path, session_id)
}

fn collect_summary_files(root: &Path, files: &mut Vec<PathBuf>) {
    collect_files_where(
        root,
        |path| path.file_name().and_then(|name| name.to_str()) == Some("summary.json"),
        files,
    );
}

fn read_summary(path: &Path) -> Result<GrokSessionSummary, String> {
    ensure_readable_size(path)?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read Grok Build session summary: {e}"))?;
    serde_json::from_str(&text)
        .map_err(|e| format!("Failed to parse Grok Build session summary: {e}"))
}

/// 扫描缓存用：读失败（权限、被占用、JSON 损坏）返回 `Err`，本轮不缓存、下轮重试，
/// 与接入缓存前「每轮重扫都重新解析并重复警告」的行为一致。
fn parse_summary_result(path: &Path) -> Result<Option<SessionMeta>, String> {
    let summary = read_summary(path).inspect_err(|error| {
        super::utils::scan_warning(error.clone());
    })?;
    let session_id = summary.info.id;
    let is_subagent = summary
        .session_kind
        .as_deref()
        .is_some_and(|kind| matches!(kind, "subagent" | "subagent_fork"));
    let title = summary
        .generated_title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            summary
                .session_summary
                .as_deref()
                .filter(|value| !value.trim().is_empty())
        })
        .map(|value| truncate_summary(value, TITLE_MAX_CHARS));
    let session_summary = summary
        .session_summary
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| truncate_summary(value, 160));
    let created_at = summary.created_at.as_ref().and_then(parse_timestamp_to_ms);
    let last_active_at = summary
        .last_active_at
        .as_ref()
        .or(summary.updated_at.as_ref())
        .and_then(parse_timestamp_to_ms);

    Ok(Some(SessionMeta {
        provider_id: "grokbuild".to_string(),
        session_id: session_id.clone(),
        residual: is_subagent,
        parent_session_id: summary
            .parent_session_id
            .clone()
            .filter(|value| !value.trim().is_empty()),
        archived: false,
        cleanup_pending: false,
        sidebar_section: None,
        title,
        summary: session_summary,
        project_dir: summary.info.cwd,
        project_name: None,
        created_at,
        last_active_at,
        source_path: Some(path.to_string_lossy().to_string()),
        resume_command: (!is_subagent && is_safe_session_id(&session_id))
            .then(|| format!("grok --resume {session_id}")),
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn compat_native_tool_result_and_calls_have_displayable_roles() {
        let input = concat!(
            "{\"type\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"id\":\"c\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}]}\n",
            "{\"type\":\"tool_result\",\"tool_call_id\":\"c\",\"content\":\"file contents\"}\n",
            "{\"type\":\"reasoning\",\"encrypted_content\":\"PRIVATE\"}\n"
        );
        let messages = super::read_chat_messages(std::io::Cursor::new(input)).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "assistant");
        assert!(messages[0].content.contains("read_file"));
        assert!(messages[0].content.contains("a.rs"));
        assert_eq!(messages[1].role, "tool");
        assert_eq!(messages[1].content, "file contents");
    }

    fn compat_updates() -> String {
        let updates = vec![
            serde_json::json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello"}}),
            serde_json::json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Let "}}),
            serde_json::json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"me check"}}),
            serde_json::json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"PRIVATE"}}),
            serde_json::json!({"sessionUpdate":"tool_call","toolCallId":"call-1","title":"read_file","kind":"read","status":"pending"}),
            serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":"call-1","rawInput":{"path":"a.rs"},"status":"completed","content":[{"type":"content","content":{"type":"text","text":"file output"}}]}),
            serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":"call-1","status":"completed","rawOutput":"duplicate"}),
            serde_json::json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"}}),
        ];
        updates.into_iter().map(|update| serde_json::json!({"timestamp":1,"method":"session/update","params":{"sessionId":"fixture","update":update}}).to_string()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn compat_missing_and_empty_v1_cache_rebuilds_read_only_from_updates() {
        let temp = tempfile::tempdir().unwrap();
        let summary = temp.path().join("summary.json");
        let chat = temp.path().join("chat_history.jsonl");
        let updates = temp.path().join("updates.jsonl");
        std::fs::write(
            &summary,
            r#"{"info":{"id":"fixture"},"chat_format_version":1}"#,
        )
        .unwrap();
        let content = compat_updates();
        std::fs::write(&updates, &content).unwrap();
        for create_empty in [false, true] {
            if create_empty {
                std::fs::write(&chat, "").unwrap();
            }
            let messages = super::load_messages(&summary).unwrap();
            assert_eq!(messages.len(), 4);
            assert_eq!(messages[0].content, "hello");
            assert!(messages[1].content.starts_with("Let me check"));
            assert!(messages[1].content.contains("a.rs"));
            assert_eq!(messages[2].role, "tool");
            assert_eq!(messages[2].content, "file output");
            assert_eq!(messages[3].content, "done");
            assert_eq!(chat.exists(), create_empty);
            if create_empty {
                assert_eq!(std::fs::metadata(&chat).unwrap().len(), 0);
            }
            assert_eq!(std::fs::read_to_string(&updates).unwrap(), content);
        }
        let sources = super::transcript_sources(&summary).unwrap();
        assert_eq!(sources.len(), 3);
        assert_eq!(sources[1], (chat.clone(), None));
        assert_eq!(sources[2], (updates, None));
        std::fs::write(&chat, "{\"type\":\"user\",\"content\":\"cached\"}\n").unwrap();
        assert_eq!(super::load_messages(&summary).unwrap()[0].content, "cached");
    }

    #[test]
    fn compat_unknown_version_does_not_guess_updates_and_sources_keep_absence() {
        let temp = tempfile::tempdir().unwrap();
        let summary = temp.path().join("summary.json");
        std::fs::write(
            &summary,
            r#"{"info":{"id":"fixture"},"chat_format_version":99}"#,
        )
        .unwrap();
        std::fs::write(temp.path().join("updates.jsonl"), compat_updates()).unwrap();
        assert!(super::load_messages(&summary).is_err());
        let sources = super::transcript_sources(&summary).unwrap();
        assert_eq!(sources[1].1, None);
        assert_eq!(sources[2], (temp.path().join("updates.jsonl"), None));
    }

    #[test]
    fn compat_updates_compaction_host_turn_interjection_and_read_errors() {
        let mut input = compat_updates();
        for (method, update) in [
            (
                "_x.ai/session/update",
                serde_json::json!({"sessionUpdate":"compaction_checkpoint"}),
            ),
            (
                "session/update",
                serde_json::json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"summary"}}),
            ),
            (
                "session/update",
                serde_json::json!({"sessionUpdate":"agent_message_chunk","_meta":{"hostTurn":true},"content":{"type":"text","text":"hidden host"}}),
            ),
            (
                "session/update",
                serde_json::json!({"sessionUpdate":"user_message_chunk","_meta":{"interjection":true},"content":{"type":"text","text":"interjection 1"}}),
            ),
            (
                "session/update",
                serde_json::json!({"sessionUpdate":"user_message_chunk","_meta":{"interjection":true},"content":{"type":"text","text":"interjection 2"}}),
            ),
        ] {
            input.push('\n');
            input.push_str(&serde_json::json!({"method":method,"params":{"sessionId":"fixture","update":update}}).to_string());
        }
        let messages = super::read_update_messages(std::io::Cursor::new(input.clone())).unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            ["summary", "interjection 1", "interjection 2"]
        );
        let mut bytes = input.into_bytes();
        bytes.extend_from_slice(b"\n\xff\n");
        assert!(super::read_update_messages(std::io::Cursor::new(bytes))
            .unwrap_err()
            .contains("line"));
    }

    #[test]
    fn regression_chat_loading_rejects_invalid_utf8_after_valid_message() {
        let temp = tempfile::tempdir().unwrap();
        let summary = temp.path().join("summary.json");
        let mut bytes = b"{\"type\":\"user\",\"content\":\"hello\"}\n".to_vec();
        bytes.extend_from_slice(b"\xff\n");
        std::fs::write(temp.path().join("chat_history.jsonl"), bytes).unwrap();
        let error = super::load_messages(&summary).unwrap_err();
        assert!(error.contains("line 2"));
    }

    #[test]
    fn regression_chat_loading_propagates_io_error_after_valid_message() {
        struct FailsAfterLine(std::io::Cursor<Vec<u8>>);
        impl std::io::Read for FailsAfterLine {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                if self.0.position() == self.0.get_ref().len() as u64 {
                    return Err(std::io::Error::other("injected read failure"));
                }
                std::io::Read::read(&mut self.0, output)
            }
        }
        let reader = std::io::BufReader::new(FailsAfterLine(std::io::Cursor::new(
            b"{\"type\":\"user\",\"content\":\"hello\"}\n".to_vec(),
        )));
        let error = super::read_chat_messages(reader).unwrap_err();
        assert!(error.contains("line 2"));
        assert!(error.contains("injected read failure"));
    }

    #[test]
    fn regression_parent_discovery_rejects_non_directory_optional_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        std::fs::write(&root, "not a directory").unwrap();
        assert!(super::parent_relations_in_roots(&[root]).is_err());
    }

    use super::*;
    use tempfile::tempdir;

    fn write_summary(
        root: &Path,
        project: &str,
        id: &str,
        parent: Option<&str>,
        kind: Option<&str>,
    ) -> PathBuf {
        let session_dir = root.join(project).join(id);
        std::fs::create_dir_all(&session_dir).expect("create session directory");
        let mut value = serde_json::json!({"info": {"id": id, "cwd": "C:/work"}});
        if let Some(parent) = parent {
            value["parent_session_id"] = serde_json::json!(parent);
        }
        if let Some(kind) = kind {
            value["session_kind"] = serde_json::json!(kind);
        }
        let path = session_dir.join("summary.json");
        std::fs::write(&path, value.to_string()).expect("write session summary");
        path
    }

    #[test]
    fn scans_native_grokbuild_session_layout() {
        let temp = tempdir().expect("tempdir");
        let sessions_dir = temp.path().join("sessions");
        let session_id = "019f6af2-18b0-7673-958e-d25be650e172";
        let session_dir = sessions_dir.join("encoded-project").join(session_id);
        std::fs::create_dir_all(&session_dir).expect("create session dir");
        std::fs::write(
            session_dir.join("summary.json"),
            format!(
                r#"{{"info":{{"id":"{session_id}","cwd":"C:/work"}},"session_summary":"hello grok","generated_title":"Grok session","created_at":"2026-07-16T12:00:00Z","last_active_at":"2026-07-16T12:00:01Z"}}"#
            ),
        )
        .expect("write summary");
        let mut files = Vec::new();
        collect_summary_files(&sessions_dir, &mut files);
        let sessions = files
            .iter()
            .filter_map(|path| parse_summary_result(path).expect("parse summary"))
            .collect::<Vec<_>>();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].provider_id, "grokbuild");
        assert_eq!(sessions[0].session_id, session_id);
        assert_eq!(sessions[0].title.as_deref(), Some("Grok session"));
        let expected_resume = format!("grok --resume {session_id}");
        assert_eq!(
            sessions[0].resume_command.as_deref(),
            Some(expected_resume.as_str())
        );
    }

    #[test]
    fn archived_grokbuild_summary_is_identified_as_residual() {
        let temp = tempdir().expect("tempdir");
        let session_id = "archived-session";
        let session_dir = temp
            .path()
            .join("archived_sessions")
            .join("project")
            .join(session_id);
        std::fs::create_dir_all(&session_dir).expect("create session dir");
        let summary_path = session_dir.join("summary.json");
        std::fs::write(
            &summary_path,
            format!(r#"{{"info":{{"id":"{session_id}"}},"generated_title":"Archived"}}"#),
        )
        .expect("write summary");

        let session = parse_discovered_summary(&summary_path)
            .expect("parse summary")
            .expect("session meta");

        assert!(session.residual);
        assert!(session.resume_command.is_none());
    }

    #[test]
    fn subagent_summary_is_exposed_as_residual() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        let child = write_summary(
            &root,
            "project",
            "session-child",
            Some("session-parent"),
            Some("subagent"),
        );

        let session = parse_discovered_summary(&child)
            .expect("parse child summary")
            .expect("session meta");
        assert!(session.residual);
        assert!(session.resume_command.is_none());
        assert_eq!(session.parent_session_id.as_deref(), Some("session-parent"));
    }

    #[test]
    fn a_root_summary_reports_no_parent() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        let parent = write_summary(&root, "project", "session-parent", None, None);

        let session = parse_discovered_summary(&parent)
            .expect("parse parent summary")
            .expect("session meta");
        assert_eq!(session.parent_session_id, None);
        assert!(!session.residual);
    }

    #[test]
    fn grok_parent_is_protected_until_child_is_deleted() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        let parent = write_summary(&root, "project", "session-parent", None, None);
        let child = write_summary(
            &root,
            "project",
            "session-child",
            Some("session-parent"),
            Some("subagent"),
        );

        let error =
            preflight_delete_in_roots(std::slice::from_ref(&root), &parent, "session-parent")
                .expect_err("parent must be protected");
        assert!(error.contains("session-child"));
        assert!(delete_session(&root, &child, "session-child").expect("delete child"));
        assert!(parent.exists());
        assert!(delete_session(&root, &parent, "session-parent").expect("delete parent"));
    }

    #[test]
    fn grok_parent_cycles_fail_closed() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        write_summary(&root, "project", "session-a", Some("session-b"), None);
        write_summary(&root, "project", "session-b", Some("session-a"), None);

        let error = parent_relations_in_roots(&[root]).expect_err("cycle must fail");
        assert!(error.contains("存在循环"));
    }

    #[test]
    fn selected_grok_children_are_ordered_before_parents() {
        let requests = vec![
            DeleteSessionRequest {
                provider_id: "grokbuild".into(),
                session_id: "session-parent".into(),
                source_path: "parent".into(),
                include_project: false,
            },
            DeleteSessionRequest {
                provider_id: "grokbuild".into(),
                session_id: "session-child".into(),
                source_path: "child".into(),
                include_project: false,
            },
        ];
        let relations = HashMap::from([("session-child".into(), "session-parent".into())]);
        assert_eq!(
            deletion_order_with_relations(&requests, &[0, 1], &relations),
            vec![1, 0]
        );
    }

    #[test]
    fn loads_native_grokbuild_chat_history() {
        let temp = tempdir().expect("tempdir");
        let summary_path = temp.path().join("summary.json");
        std::fs::write(&summary_path, "{}").expect("write summary placeholder");
        std::fs::write(
            temp.path().join("chat_history.jsonl"),
            concat!(
                "{\"type\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}\n",
                "{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"private\"}]}\n",
                "{\"type\":\"assistant\",\"content\":\"Hi there\"}\n"
            ),
        )
        .expect("write chat history");

        let messages = load_messages(&summary_path).expect("load messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "hello");
        assert_eq!(messages[1].content, "Hi there");
    }

    #[test]
    fn delete_session_removes_only_the_matching_session_directory() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        let session_id = "session-to-delete";
        let session_dir = root.join("project").join(session_id);
        let sibling_dir = root.join("project").join("session-to-keep");
        std::fs::create_dir_all(&session_dir).expect("create session directory");
        std::fs::create_dir_all(&sibling_dir).expect("create sibling directory");
        let summary_path = session_dir.join("summary.json");
        std::fs::write(
            &summary_path,
            format!(r#"{{"info":{{"id":"{session_id}"}}}}"#),
        )
        .expect("write summary");
        std::fs::write(sibling_dir.join("keep.txt"), "keep").expect("write sibling file");

        let deleted = delete_session(&root, &summary_path, session_id).expect("delete session");

        assert!(deleted);
        assert!(!session_dir.exists());
        assert!(sibling_dir.exists());
    }

    #[test]
    fn missing_summary_retry_removes_only_its_directory_and_is_idempotent() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("sessions");
        let path = write_summary(&root, "project", "session-1", None, None);
        let sentinel = path.parent().unwrap().join("sentinel");
        std::fs::write(&sentinel, "remaining").unwrap();
        std::fs::remove_file(&path).unwrap();
        let sibling = write_summary(&root, "project", "session-2", None, None);
        assert!(delete_session(&root, &path, "session-1").unwrap());
        assert!(!sentinel.exists());
        assert!(!path.parent().unwrap().exists());
        assert!(sibling.exists());
        assert!(delete_session(&root, &path, "session-1").unwrap());
    }

    #[test]
    fn missing_summary_retry_rejects_wrong_root_id_layout_and_escape() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("sessions");
        let path = root.join("project/session-1/summary.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let sentinel = path.parent().unwrap().join("sentinel");
        std::fs::write(&sentinel, "keep").unwrap();
        for (candidate_root, candidate_path, id) in [
            (temp.path().to_path_buf(), path.clone(), "session-1"),
            (root.clone(), path.clone(), "session-2"),
            (
                root.clone(),
                root.join("project/../project/session-1/summary.json"),
                "session-1",
            ),
            (
                root.clone(),
                root.join("session-1/summary.json"),
                "session-1",
            ),
            (
                root.clone(),
                root.join("project/session-1/other.json"),
                "session-1",
            ),
            (
                root.clone(),
                temp.path().join("outside/session-1/summary.json"),
                "session-1",
            ),
        ] {
            assert!(delete_session(&candidate_root, &candidate_path, id).is_err());
            assert!(sentinel.exists());
        }
    }

    #[test]
    fn existing_summary_id_mismatch_preserves_directory() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("sessions");
        let path = write_summary(&root, "project", "session-1", None, None);
        std::fs::write(&path, r#"{"info":{"id":"other-session"}}"#).unwrap();
        assert!(delete_session(&root, &path, "session-1").is_err());
        assert!(path.exists());
    }

    #[test]
    fn missing_summary_retry_still_checks_child_dependencies() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("sessions");
        let parent = write_summary(&root, "project", "parent", None, None);
        write_summary(&root, "project", "child", Some("parent"), None);
        std::fs::remove_file(&parent).unwrap();
        let sentinel = parent.parent().unwrap().join("sentinel");
        std::fs::write(&sentinel, "keep").unwrap();
        assert!(delete_session(&root, &parent, "parent")
            .unwrap_err()
            .contains("child"));
        assert!(sentinel.exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn missing_summary_retry_rejects_project_and_session_links() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("sessions");
        std::fs::create_dir_all(root.join("project")).unwrap();
        // Same-root aliases are dangerous too: a canonical containment check alone
        // would permit deleting this unrelated directory through the saved path.
        let target = root.join("keep/session-1");
        std::fs::create_dir_all(&target).unwrap();
        let sentinel = target.join("sentinel");
        std::fs::write(&sentinel, "keep").unwrap();
        for (link, destination, source) in [
            (
                root.join("project/session-1"),
                target.clone(),
                root.join("project/session-1/summary.json"),
            ),
            (
                root.join("alias"),
                root.join("keep"),
                root.join("alias/session-1/summary.json"),
            ),
        ] {
            #[cfg(unix)]
            let result = std::os::unix::fs::symlink(&destination, &link);
            #[cfg(windows)]
            let result = std::os::windows::fs::symlink_dir(&destination, &link);
            if let Err(error) = result {
                #[cfg(windows)]
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314)
                {
                    return; // Windows symlink privilege is optional on test hosts.
                }
                panic!("create fixture symlink: {error}");
            }
            assert!(delete_session(&root, &source, "session-1").is_err());
            assert!(sentinel.exists());
        }
    }

    #[test]
    fn delete_session_rejects_remove_dir_all_target_outside_root() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        let outside_dir = temp.path().join("outside").join("session-outside");
        std::fs::create_dir_all(&root).expect("create root");
        std::fs::create_dir_all(&outside_dir).expect("create outside directory");
        let summary_path = outside_dir.join("summary.json");
        std::fs::write(&summary_path, r#"{"info":{"id":"session-outside"}}"#)
            .expect("write summary");

        let error = delete_session(&root, &summary_path, "session-outside")
            .expect_err("outside path must be rejected");

        assert!(error.contains("outside the session root"));
        assert!(outside_dir.exists());
    }
}
