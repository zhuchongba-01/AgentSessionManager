use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::session_manager::{DeleteSessionRequest, SessionMessage, SessionMeta};

use super::utils::{
    checked_storage_child, collect_files_where, ensure_readable_size, extract_text,
    is_safe_session_id, parse_timestamp_to_ms, truncate_summary, TITLE_MAX_CHARS,
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
}

pub fn session_roots() -> Vec<PathBuf> {
    let config_dir = crate::session_paths::grok_config_dir();
    vec![
        config_dir.join("sessions"),
        config_dir.join("archived_sessions"),
    ]
}

pub fn scan_sessions() -> Vec<SessionMeta> {
    let mut summaries = Vec::new();
    for root in session_roots() {
        collect_summary_files(&root, &mut summaries);
    }
    summaries
        .into_iter()
        .filter_map(|path| parse_discovered_summary(&path))
        .collect()
}

fn parse_discovered_summary(path: &Path) -> Option<SessionMeta> {
    let mut session = parse_summary(path)?;
    if is_archived_summary(path) {
        session.residual = true;
        session.resume_command = None;
    }
    Some(session)
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
    ensure_readable_size(&chat_path)?;
    let file = File::open(&chat_path)
        .map_err(|e| format!("Failed to open Grok Build chat history: {e}"))?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();

    for line in reader.lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let role = match kind {
            "system" | "user" | "assistant" | "tool" => kind,
            // Reasoning records can contain encrypted/internal state and are not
            // conversation messages shown by Grok's own history view.
            _ => continue,
        };
        let content = value.get("content").map(extract_text).unwrap_or_default();
        if content.trim().is_empty() {
            continue;
        }
        let ts = value
            .get("timestamp")
            .or_else(|| value.get("ts"))
            .and_then(parse_timestamp_to_ms);
        messages.push(SessionMessage {
            role: role.to_string(),
            content,
            ts,
        });
    }

    Ok(messages)
}

fn parent_relations_in_roots(roots: &[PathBuf]) -> Result<HashMap<String, String>, String> {
    let mut relations = HashMap::new();
    let mut seen_paths = HashSet::new();
    for root in roots.iter().filter(|root| root.is_dir()) {
        let root = root
            .canonicalize()
            .map_err(|error| format!("Failed to resolve Grok Build session root: {error}"))?;
        let mut summaries = Vec::new();
        collect_summary_files(&root, &mut summaries);
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

    for child in relations.keys() {
        let mut seen = HashSet::new();
        let mut current = child.as_str();
        while let Some(parent) = relations.get(current) {
            if !seen.insert(current.to_string()) {
                return Err("Grok Build 会话父子关系存在循环，已停止删除".into());
            }
            current = parent;
        }
    }
    Ok(relations)
}

fn preflight_delete_in_roots(
    roots: &[PathBuf],
    path: &Path,
    session_id: &str,
) -> Result<(), String> {
    if !is_safe_session_id(session_id) {
        return Err("Invalid Grok Build session ID".into());
    }
    let mut target = None;
    for root in roots.iter().filter(|root| root.is_dir()) {
        let root = root
            .canonicalize()
            .map_err(|error| format!("Failed to resolve Grok Build session root: {error}"))?;
        if let Ok(candidate) = checked_storage_child(&root, path) {
            target = Some(candidate);
            break;
        }
    }
    let target = target.ok_or("Grok Build session source is outside the session root")?;
    if target.file_name().and_then(|name| name.to_str()) != Some("summary.json") {
        return Err("Unexpected Grok Build session source".into());
    }
    let summary = read_summary(&target)?;
    if summary.info.id != session_id {
        return Err(format!(
            "Grok Build session ID mismatch: expected {session_id}, found {}",
            summary.info.id
        ));
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

pub fn preflight_delete(path: &Path, session_id: &str) -> Result<(), String> {
    preflight_delete_in_roots(&session_roots(), path, session_id)
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
        .map(|(index, request)| (request.session_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut remaining = initial.to_vec();
    let mut ordered = Vec::with_capacity(initial.len());
    while !remaining.is_empty() {
        let ready = remaining.iter().position(|candidate| {
            let Some(parent_id) = requests
                .get(*candidate)
                .map(|request| request.session_id.as_str())
            else {
                return true;
            };
            !relations.iter().any(|(child, parent)| {
                parent == parent_id
                    && selected
                        .get(child.as_str())
                        .is_some_and(|child_index| remaining.contains(child_index))
            })
        });
        let Some(position) = ready else {
            ordered.extend(remaining);
            break;
        };
        ordered.push(remaining.remove(position));
    }
    ordered
}

pub fn deletion_order(requests: &[DeleteSessionRequest], initial: &[usize]) -> Vec<usize> {
    let Ok(relations) = parent_relations_in_roots(&session_roots()) else {
        return initial.to_vec();
    };
    deletion_order_with_relations(requests, initial, &relations)
}

pub fn delete_session(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    preflight_delete_in_roots(&[root.to_path_buf()], path, session_id)?;
    if !path.starts_with(root) {
        return Err(format!(
            "Grok Build session source is outside the session root: {}",
            path.display()
        ));
    }
    if path.file_name().and_then(|name| name.to_str()) != Some("summary.json") {
        return Err(format!(
            "Unexpected Grok Build session source: {}",
            path.display()
        ));
    }
    let summary = read_summary(path)?;
    if summary.info.id != session_id {
        return Err(format!(
            "Grok Build session ID mismatch: expected {session_id}, found {}",
            summary.info.id
        ));
    }
    let session_dir = path
        .parent()
        .ok_or_else(|| format!("Invalid Grok Build session path: {}", path.display()))?;
    if session_dir == root || !session_dir.starts_with(root) {
        return Err(format!(
            "Refusing to delete Grok Build session directory outside its root: {}",
            session_dir.display()
        ));
    }
    if session_dir.file_name().and_then(|name| name.to_str()) != Some(session_id) {
        return Err(format!(
            "Grok Build session directory does not match session ID: {}",
            session_dir.display()
        ));
    }
    std::fs::remove_dir_all(session_dir).map_err(|e| {
        format!(
            "Failed to delete Grok Build session directory {}: {e}",
            session_dir.display()
        )
    })?;
    Ok(true)
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

fn parse_summary(path: &Path) -> Option<SessionMeta> {
    let summary = read_summary(path)
        .inspect_err(|error| {
            super::utils::scan_warning(error.clone());
        })
        .ok()?;
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

    Some(SessionMeta {
        provider_id: "grokbuild".to_string(),
        session_id: session_id.clone(),
        residual: is_subagent,
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
    })
}

#[cfg(test)]
mod tests {
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
            .filter_map(|path| parse_summary(path))
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

        let session = parse_discovered_summary(&summary_path).expect("parse summary");

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

        let session = parse_discovered_summary(&child).expect("parse child summary");
        assert!(session.residual);
        assert!(session.resume_command.is_none());
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
