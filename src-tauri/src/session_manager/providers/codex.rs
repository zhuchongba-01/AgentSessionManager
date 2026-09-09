use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;

use crate::session_manager::{SessionMessage, SessionMeta};
use crate::session_paths::{
    codex_config_dir, codex_state_db_paths, read_codex_config_text,
    CODEX_THREAD_HISTORY_DB_FILENAME,
};

use super::utils::{
    collect_files_where, ensure_readable_size, extract_text, is_safe_session_id,
    parse_timestamp_to_ms, path_basename, read_head_tail_lines, truncate_summary,
    MAX_MESSAGES_FILE_BYTES, TITLE_MAX_CHARS,
};

const PROVIDER_ID: &str = "codex";
const CODEX_SESSION_INDEX_FILENAME: &str = "session_index.jsonl";
const VSCODE_CONTEXT_PREFIX: &str = "# Context from my IDE setup:";
const CODEX_REQUEST_MARKER: &str = "my request for codex";

static UUID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
        .unwrap()
});

#[derive(Deserialize)]
struct SessionIndexEntry {
    id: String,
    thread_name: String,
}

#[derive(Clone, Debug, Default)]
struct CodexProjectBinding {
    name: String,
    root_path: Option<String>,
}

#[derive(Debug, Default)]
struct CodexProjectContext {
    thread_projects: HashMap<String, CodexProjectBinding>,
    projectless_threads: HashSet<String>,
}

#[derive(Clone, Debug)]
struct HistoryBaseRef {
    thread_id: String,
    end_byte_offset: u64,
}

#[derive(Clone, Debug)]
struct RolloutDescriptor {
    path: PathBuf,
    session_id: String,
    rollout_id: String,
    created_at: Option<i64>,
    file_len: u64,
    history_base: Option<HistoryBaseRef>,
}

#[derive(Clone, Debug)]
struct HistorySegment {
    path: PathBuf,
    end_byte_offset: Option<u64>,
}

pub fn scan_sessions() -> Vec<SessionMeta> {
    let roots = session_roots();
    let thread_titles = load_thread_titles();
    let project_context = load_project_context();
    let native_rollouts = load_native_active_rollouts();
    let mut sessions = scan_sessions_in_roots_with_context(
        &roots,
        &thread_titles,
        &project_context,
        native_rollouts.as_ref(),
    );
    let indexed = match load_indexed_rollouts() {
        Ok(indexed) => indexed,
        Err(error) => {
            super::utils::scan_warning(error);
            None
        }
    };
    let indexed_ids = indexed
        .as_ref()
        .map(|items| items.keys().cloned().collect::<HashSet<_>>());
    if let Some(indexed) = indexed {
        for (id, source) in indexed {
            if let Some(item) = sessions.iter_mut().find(|item| {
                item.session_id == id
                    && item.source_path.as_deref().is_some_and(|path| {
                        normalized_rollout_path(path) == normalized_rollout_path(&source)
                    })
            }) {
                if native_rollouts
                    .as_ref()
                    .is_some_and(|active| !active.contains_key(&id))
                {
                    item.archived = true;
                    item.residual = false;
                    item.resume_command = None;
                }
            } else if !Path::new(&source).exists() {
                sessions.push(SessionMeta {
                    provider_id: PROVIDER_ID.into(),
                    session_id: id.clone(),
                    residual: false,
                    archived: native_rollouts
                        .as_ref()
                        .is_some_and(|active| !active.contains_key(&id)),
                    cleanup_pending: true,
                    title: thread_titles
                        .get(&id)
                        .cloned()
                        .or_else(|| Some(format!("缺少日志的任务 {id}"))),
                    summary: Some("索引仍存在，但日志文件已消失；可继续清理索引".into()),
                    project_dir: None,
                    project_name: None,
                    created_at: None,
                    last_active_at: None,
                    source_path: Some(source),
                    resume_command: None,
                });
            }
        }
    }
    match desktop_catalog_ids() {
        Ok(ids) => {
            for id in ids {
                // Do not classify unreadable/unscanned live records as index ghosts.
                if indexed_ids
                    .as_ref()
                    .is_none_or(|indexed| indexed.contains(&id))
                {
                    continue;
                }
                if sessions.iter().any(|session| session.session_id == id) {
                    continue;
                }
                sessions.push(SessionMeta {
                    provider_id: PROVIDER_ID.into(),
                    session_id: id.clone(),
                    residual: false,
                    archived: false,
                    cleanup_pending: true,
                    title: Some(format!("仅剩侧边栏索引的任务 {id}")),
                    summary: Some(
                        "侧边栏索引存在，但没有找到对应日志；请退出 Codex 后继续清理".into(),
                    ),
                    project_dir: None,
                    project_name: None,
                    created_at: None,
                    last_active_at: None,
                    source_path: Some(format!("codex-index:{id}")),
                    resume_command: None,
                });
            }
        }
        Err(error) => super::utils::scan_warning(error),
    }
    sessions
}

fn desktop_catalog_ids() -> Result<HashSet<String>, String> {
    let mut ids = HashSet::new();
    for path in codex_desktop_db_paths(&codex_config_dir())? {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| e.to_string())?;
        conn.busy_timeout(Duration::from_secs(2))
            .map_err(|e| e.to_string())?;
        let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='local_thread_catalog')", [], |row| row.get(0)).map_err(|e| e.to_string())?;
        if !exists {
            continue;
        }
        let mut statement = conn
            .prepare("SELECT thread_id FROM local_thread_catalog")
            .map_err(|e| e.to_string())?;
        for row in statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?
        {
            ids.insert(row.map_err(|e| e.to_string())?);
        }
    }
    Ok(ids)
}

pub fn session_roots() -> Vec<PathBuf> {
    let config_dir = codex_config_dir();
    vec![
        config_dir.join("sessions"),
        config_dir.join("archived_sessions"),
    ]
}

/// A historical rollout may share its thread id with the current Codex task.
/// Deleting that old file must not remove the live thread from Codex's index.
pub fn should_cleanup_index_after_delete(
    session_id: &str,
    source_path: &str,
) -> Result<bool, String> {
    Ok(load_indexed_rollouts()?.is_none_or(|rollouts| {
        rollouts.get(session_id).is_none_or(|path| {
            normalized_rollout_path(path) == normalized_rollout_path(source_path)
        })
    }))
}

fn load_indexed_rollouts() -> Result<Option<HashMap<String, String>>, String> {
    let config_dir = codex_config_dir();
    let config = read_codex_config_text()?;
    load_indexed_rollouts_from_paths(codex_state_db_paths(&config_dir, &config))
}

fn load_indexed_rollouts_from_paths(
    paths: Vec<PathBuf>,
) -> Result<Option<HashMap<String, String>>, String> {
    let mut rollouts = HashMap::new();
    let mut loaded = false;
    for path in paths {
        if !path.is_file() {
            continue;
        }
        let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("无法读取 Codex 任务索引，已停止清理：{e}"))?;
        conn.busy_timeout(Duration::from_secs(2))
            .map_err(|e| e.to_string())?;
        let mut statement = conn
            .prepare("SELECT id, rollout_path FROM threads")
            .map_err(|e| format!("Codex 索引结构不兼容，已停止清理：{e}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (id, source) = row.map_err(|e| e.to_string())?;
            if let Some(previous) = rollouts.insert(id, source.clone()) {
                if normalized_rollout_path(&previous) != normalized_rollout_path(&source) {
                    return Err("多个 Codex 索引对同一任务指向不同日志，已停止清理".into());
                }
            }
        }
        loaded = true;
    }
    Ok(loaded.then_some(rollouts))
}

/// Remove the exact Codex desktop indexes that survive after a rollout file or
/// real project directory has been deleted.  This is what prevents the Codex
/// sidebar from keeping a grey, empty project entry.
pub fn cleanup_after_delete(
    session_id: &str,
    project_dir: Option<&str>,
    project_was_deleted: bool,
) -> Result<(), String> {
    let config_dir = codex_config_dir();
    let mut errors = Vec::new();
    if let Err(error) =
        rewrite_session_index(&config_dir.join(CODEX_SESSION_INDEX_FILENAME), session_id)
    {
        errors.push(error);
    }

    let config_text = read_codex_config_text().unwrap_or_default();
    if let Err(error) = cleanup_index_databases(
        &config_dir,
        &config_text,
        session_id,
        project_dir,
        project_was_deleted,
    ) {
        errors.push(error);
    }

    if let Err(error) = cleanup_global_state(
        &config_dir.join(".codex-global-state.json"),
        session_id,
        project_dir,
        project_was_deleted,
    ) {
        errors.push(error);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("；"))
    }
}

fn cleanup_index_databases(
    config_dir: &Path,
    config_text: &str,
    session_id: &str,
    project_dir: Option<&str>,
    project_was_deleted: bool,
) -> Result<(), String> {
    let mut errors = Vec::new();
    for state_db_path in codex_state_db_paths(config_dir, config_text) {
        if let Err(error) =
            cleanup_state_db(&state_db_path, session_id, project_dir, project_was_deleted)
        {
            errors.push(error);
        }

        // Codex keeps the rendered conversation projection in a separate DB.
        // Leaving these rows behind after deleting the rollout produces a
        // ghost entry that opens with "no rollout found for thread id ...".
        let history_db_path = state_db_path.with_file_name(CODEX_THREAD_HISTORY_DB_FILENAME);
        if let Err(error) = cleanup_state_db(&history_db_path, session_id, None, false) {
            errors.push(error);
        }
    }

    match codex_desktop_db_paths(config_dir) {
        Ok(paths) => {
            for path in paths {
                if let Err(error) = cleanup_desktop_catalog_db(&path, session_id) {
                    errors.push(error);
                }
            }
        }
        Err(error) => errors.push(error),
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("；"))
    }
}

fn codex_desktop_db_paths(config_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let directory = config_dir.join("sqlite");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(&directory).map_err(|error| {
        format!(
            "无法读取 Codex 桌面索引目录 {}：{error}",
            directory.display()
        )
    })?;
    Ok(entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("db")
        })
        .collect())
}

fn cleanup_desktop_catalog_db(path: &Path, session_id: &str) -> Result<(), String> {
    let mut conn = Connection::open(path).map_err(|error| {
        format!(
            "Codex 桌面索引被占用或无法写入（{}）：{error}",
            path.display()
        )
    })?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|error| format!("无法设置 Codex 桌面索引等待时间：{error}"))?;
    let tx = conn
        .transaction()
        .map_err(|error| format!("无法开始清理 Codex 桌面索引：{error}"))?;

    let table_names: std::collections::HashSet<String> = {
        let mut stmt = tx
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .map_err(|error| error.to_string())?;
        let names = stmt
            .query_map([], |row| row.get(0))
            .map_err(|error| error.to_string())?
            .flatten()
            .collect();
        names
    };

    let mut catalog_changed = false;
    for table in [
        "local_thread_catalog",
        "local_thread_catalog_scan_entries",
        "thread_timeline_ledger",
    ] {
        if !table_names.contains(table) {
            continue;
        }
        let changed = tx
            .execute(
                &format!("DELETE FROM {table} WHERE thread_id = ?1"),
                [session_id],
            )
            .map_err(|error| format!("清理 Codex 桌面表 {table} 失败：{error}"))?;
        catalog_changed |= table == "local_thread_catalog" && changed > 0;
    }

    if catalog_changed && table_names.contains("local_thread_catalog_metadata") {
        tx.execute(
            "UPDATE local_thread_catalog_metadata SET catalog_revision = catalog_revision + 1",
            [],
        )
        .map_err(|error| format!("更新 Codex 桌面目录版本失败：{error}"))?;
    }

    tx.commit()
        .map_err(|error| format!("提交 Codex 桌面索引清理失败：{error}"))
}

fn rewrite_session_index(path: &Path, session_id: &str) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("无法读取 Codex 会话索引：{e}"))?;
    let kept = text
        .lines()
        .filter(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|value| {
                    value
                        .get("id")
                        .or_else(|| value.get("session_id"))
                        .or_else(|| value.get("sessionId"))
                        .and_then(Value::as_str)
                        .map(|id| id != session_id)
                })
                .unwrap_or(true)
        })
        .collect::<Vec<_>>()
        .join("\n");
    atomic_write(
        path,
        if kept.is_empty() {
            String::new()
        } else {
            kept + "\n"
        },
        &text,
    )
}

fn cleanup_state_db(
    path: &Path,
    session_id: &str,
    project_dir: Option<&str>,
    project_was_deleted: bool,
) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    let mut conn = Connection::open(path)
        .map_err(|e| format!("Codex 数据库被占用或无法写入（{}）：{e}", path.display()))?;
    // Codex 运行时常持有状态库写锁；清理是删除成功后的尽力而为步骤，
    // 稍长一点的等待能避免频繁超时。
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| format!("无法设置 Codex 数据库等待时间：{e}"))?;
    let tx = conn
        .transaction()
        .map_err(|e| format!("无法开始清理 Codex 数据库：{e}"))?;

    let table_names: Vec<String> = {
        let mut stmt = tx
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| row.get(0))
            .map_err(|e| e.to_string())?
            .flatten()
            .collect();
        rows
    };
    for table in &table_names {
        let quoted = format!("\"{}\"", table.replace('"', "\"\""));
        let columns: Vec<String> = {
            let mut stmt = tx
                .prepare(&format!("PRAGMA table_info({quoted})"))
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |row| row.get(1))
                .map_err(|e| e.to_string())?
                .flatten()
                .collect();
            rows
        };
        for column in [
            "thread_id",
            "session_id",
            "parent_session_id",
            "child_session_id",
        ] {
            if columns.iter().any(|item| item == column) {
                tx.execute(
                    &format!("DELETE FROM {quoted} WHERE \"{column}\" = ?1"),
                    [session_id],
                )
                .map_err(|e| format!("清理 Codex 表 {table} 失败：{e}"))?;
            }
        }
        if (table == "threads" || table == "session") && columns.iter().any(|item| item == "id") {
            tx.execute(&format!("DELETE FROM {quoted} WHERE id = ?1"), [session_id])
                .map_err(|e| format!("清理 Codex 表 {table} 失败：{e}"))?;
        }
    }

    if project_was_deleted {
        if let Some(project_dir) = project_dir {
            cleanup_project_tables(&tx, &table_names, project_dir)?;
        }
    }
    tx.commit()
        .map_err(|e| format!("提交 Codex 索引清理失败：{e}"))
}

fn cleanup_project_tables(
    conn: &rusqlite::Transaction<'_>,
    tables: &[String],
    project_dir: &str,
) -> Result<(), String> {
    if !tables.iter().any(|name| name == "project_roots")
        || !tables.iter().any(|name| name == "projects")
    {
        return Ok(());
    }
    let target = normalized_path(project_dir);
    let mut stmt = conn
        .prepare("SELECT project_id, position, path FROM project_roots")
        .map_err(|e| e.to_string())?;
    let matches: Vec<(String, i64)> = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, String>(2)?))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .filter(|(_, _, path)| normalized_path(path) == target)
        .map(|(id, position, _)| (id, position))
        .collect();
    drop(stmt);
    for (project_id, position) in matches {
        conn.execute(
            "DELETE FROM project_roots WHERE project_id=?1 AND position=?2",
            rusqlite::params![project_id, position],
        )
        .map_err(|e| e.to_string())?;
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_roots WHERE project_id=?1",
                [&project_id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if remaining == 0 {
            conn.execute("DELETE FROM projects WHERE id=?1", [&project_id])
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn cleanup_global_state(
    path: &Path,
    session_id: &str,
    project_dir: Option<&str>,
    project_was_deleted: bool,
) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    let original =
        std::fs::read_to_string(path).map_err(|e| format!("无法读取 Codex 全局状态：{e}"))?;
    let mut value: Value =
        serde_json::from_str(&original).map_err(|e| format!("Codex 全局状态格式无效：{e}"))?;
    if !value.is_object() {
        return Ok(());
    }
    remove_thread_references(&mut value, session_id);
    let Some(root) = value.as_object_mut() else {
        return Ok(());
    };
    for key in [
        "thread-project-assignments",
        "thread-writable-roots",
        "thread-projectless-output-directories",
        "thread-workspace-root-hints",
    ] {
        if let Some(map) = root.get_mut(key).and_then(Value::as_object_mut) {
            map.remove(session_id);
        }
    }
    if let Some(ids) = root
        .get_mut("projectless-thread-ids")
        .and_then(Value::as_array_mut)
    {
        ids.retain(|item| item.as_str() != Some(session_id));
    }

    if project_was_deleted {
        if let Some(project_dir) = project_dir {
            remove_global_project(root, project_dir);
        }
    }
    let text = serde_json::to_string(&value).map_err(|e| e.to_string())? + "\n";
    atomic_write(path, text, &original)
}

/// Remove exact references to one Codex thread from the desktop state tree.
///
/// Codex stores per-thread UI state in several shapes: maps keyed by the
/// thread id, top-level keys ending in `:<thread id>`, inverse client bindings
/// whose value is the thread id, and arrays of thread ids.  Keeping any of
/// these after deleting the rollout can leave an entry that opens with
/// `no rollout found for thread id ...`.
fn remove_thread_references(value: &mut Value, session_id: &str) {
    match value {
        Value::Object(map) => {
            map.retain(|key, child| {
                let is_thread_key = key == session_id
                    || key.strip_suffix(session_id).is_some_and(|prefix| {
                        prefix.ends_with(':')
                            || prefix
                                .get(prefix.len().saturating_sub(3)..)
                                .is_some_and(|separator| separator.eq_ignore_ascii_case("%3a"))
                    });
                let is_inverse_binding = child.as_str() == Some(session_id);
                !is_thread_key && !is_inverse_binding
            });
            for child in map.values_mut() {
                remove_thread_references(child, session_id);
            }
        }
        Value::Array(items) => {
            items.retain(|item| item.as_str() != Some(session_id));
            for item in items {
                remove_thread_references(item, session_id);
            }
        }
        _ => {}
    }
}

fn remove_global_project(root: &mut serde_json::Map<String, Value>, project_dir: &str) {
    let target = normalized_path(project_dir);
    let mut removed = Vec::new();
    if let Some(projects) = root
        .get_mut("local-projects")
        .and_then(Value::as_object_mut)
    {
        for (id, project) in projects.iter_mut() {
            if let Some(paths) = project.get_mut("rootPaths").and_then(Value::as_array_mut) {
                let before = paths.len();
                paths.retain(|path| {
                    path.as_str()
                        .is_none_or(|path| normalized_path(path) != target)
                });
                if before > 0 && paths.is_empty() {
                    removed.push(id.clone());
                }
            }
        }
        for id in &removed {
            projects.remove(id);
        }
    }
    if let Some(order) = root.get_mut("project-order").and_then(Value::as_array_mut) {
        order.retain(|id| {
            id.as_str()
                .is_none_or(|id| !removed.iter().any(|item| item == id))
        });
    }
    for key in ["project-appearances", "sidebar-project-thread-orders"] {
        if let Some(map) = root.get_mut(key).and_then(Value::as_object_mut) {
            for id in &removed {
                map.remove(id);
            }
        }
    }
}

fn normalized_path(value: &str) -> String {
    value
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

fn atomic_write(path: &Path, text: String, expected: &str) -> Result<(), String> {
    let mut temp = tempfile::NamedTempFile::new_in(
        path.parent()
            .ok_or_else(|| "索引文件没有父目录".to_string())?,
    )
    .map_err(|e| format!("无法创建临时索引文件：{e}"))?;
    temp.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    temp.flush().map_err(|e| e.to_string())?;
    temp.as_file().sync_all().map_err(|e| e.to_string())?;
    if std::fs::read_to_string(path).map_err(|e| e.to_string())? != expected {
        return Err("Codex 索引在清理期间发生变化，已停止覆盖；请退出 Agent 后重试".into());
    }
    temp.persist(path)
        .map(|_| ())
        .map_err(|e| format!("无法替换 Codex 索引：{}", e.error))
}

#[cfg(test)]
fn scan_sessions_in_roots(roots: &[PathBuf]) -> Vec<SessionMeta> {
    let thread_titles = load_thread_titles();
    let project_context = load_project_context();
    scan_sessions_in_roots_with_context(roots, &thread_titles, &project_context, None)
}

fn scan_sessions_in_roots_with_context(
    roots: &[PathBuf],
    thread_titles: &HashMap<String, String>,
    project_context: &CodexProjectContext,
    native_rollouts: Option<&HashMap<String, String>>,
) -> Vec<SessionMeta> {
    let mut files = Vec::new();
    for root in roots {
        collect_jsonl_files(root, &mut files);
    }

    let descriptors = files
        .iter()
        .filter_map(|path| read_rollout_descriptor(path).ok())
        .collect::<Vec<_>>();

    let mut sessions = Vec::new();
    for path in &files {
        if let Some(mut meta) = parse_session_with_titles(path, thread_titles) {
            if let Some(binding) = project_context.thread_projects.get(&meta.session_id) {
                meta.project_dir = binding.root_path.clone();
                meta.project_name = (!binding.name.trim().is_empty()).then(|| binding.name.clone());
            } else if project_context
                .projectless_threads
                .contains(&meta.session_id)
            {
                meta.project_dir = None;
                meta.project_name = Some("最近".to_string());
            }
            sessions.push(meta);
        }
    }

    collapse_paginated_history(&mut sessions, &descriptors, native_rollouts);
    mark_residual_rollouts(&mut sessions, native_rollouts);
    sessions
}

fn collapse_paginated_history(
    sessions: &mut Vec<SessionMeta>,
    descriptors: &[RolloutDescriptor],
    native_rollouts: Option<&HashMap<String, String>>,
) {
    let continuations = descriptors
        .iter()
        .filter(|descriptor| descriptor.history_base.is_some())
        .collect::<Vec<_>>();
    if continuations.is_empty() {
        return;
    }

    let mut hidden_paths = HashSet::new();
    let mut chain_created_at = HashMap::<String, i64>::new();

    for continuation in continuations {
        match resolve_paginated_chain_from_descriptors(&continuation.path, descriptors) {
            Ok(chain) => {
                let leaf_path = normalized_rollout_path(&continuation.path.to_string_lossy());
                let earliest = chain
                    .iter()
                    .filter_map(|segment| {
                        descriptors
                            .iter()
                            .find(|descriptor| same_rollout_path(&descriptor.path, &segment.path))
                            .and_then(|descriptor| descriptor.created_at)
                    })
                    .min();
                if let Some(earliest) = earliest {
                    chain_created_at
                        .entry(leaf_path)
                        .and_modify(|existing| *existing = (*existing).min(earliest))
                        .or_insert(earliest);
                }
                for segment in chain.iter().take(chain.len().saturating_sub(1)) {
                    hidden_paths.insert(normalized_rollout_path(&segment.path.to_string_lossy()));
                }
            }
            Err(error) => {
                // A paginated base is never a safe deletion candidate. If the
                // chain cannot be resolved, keep only Codex's authoritative
                // current path (or the continuation itself without a DB) and
                // fail closed in load/delete rather than exposing bases as
                // disposable residual files.
                super::utils::scan_warning(error);
                let keep = native_rollouts
                    .and_then(|rollouts| rollouts.get(&continuation.session_id))
                    .cloned()
                    .unwrap_or_else(|| {
                        normalized_rollout_path(&continuation.path.to_string_lossy())
                    });
                for descriptor in descriptors
                    .iter()
                    .filter(|item| item.session_id == continuation.session_id)
                {
                    let path = normalized_rollout_path(&descriptor.path.to_string_lossy());
                    if path != keep {
                        hidden_paths.insert(path);
                    }
                }
            }
        }
    }

    sessions.retain(|session| {
        session
            .source_path
            .as_deref()
            .is_none_or(|path| !hidden_paths.contains(&normalized_rollout_path(path)))
    });
    for session in sessions {
        let Some(path) = session.source_path.as_deref() else {
            continue;
        };
        if let Some(created_at) = chain_created_at.get(&normalized_rollout_path(path)) {
            session.created_at = Some(session.created_at.unwrap_or(*created_at).min(*created_at));
        }
    }
}

fn read_rollout_descriptor(path: &Path) -> Result<RolloutDescriptor, String> {
    let file = File::open(path)
        .map_err(|error| format!("无法读取 Codex 会话元数据 {}：{error}", path.display()))?;
    let reader = BufReader::new(file);

    for line in reader.lines().take(10) {
        let line = line.map_err(|error| error.to_string())?;
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let payload = value
            .get("payload")
            .ok_or_else(|| format!("Codex 会话缺少元数据：{}", path.display()))?;
        let session_id = payload
            .get("id")
            .or_else(|| payload.get("session_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| format!("Codex 会话缺少 ID：{}", path.display()))?
            .to_string();
        let created_at = payload
            .get("timestamp")
            .and_then(parse_timestamp_to_ms)
            .or_else(|| value.get("timestamp").and_then(parse_timestamp_to_ms));
        let history_base = payload.get("history_base").and_then(|base| {
            let thread_id = base.get("thread_id")?.as_str()?.trim();
            let end_byte_offset = base.get("end_byte_offset")?.as_u64()?;
            (!thread_id.is_empty() && end_byte_offset > 0).then(|| HistoryBaseRef {
                thread_id: thread_id.to_string(),
                end_byte_offset,
            })
        });
        let file_len = path
            .metadata()
            .map_err(|error| format!("无法读取 Codex 会话大小 {}：{error}", path.display()))?
            .len();
        let rollout_id = rollout_instance_id(path, &session_id);
        return Ok(RolloutDescriptor {
            path: path.to_path_buf(),
            session_id,
            rollout_id,
            created_at,
            file_len,
            history_base,
        });
    }

    Err(format!("无法识别 Codex 会话元数据：{}", path.display()))
}

fn rollout_instance_id(path: &Path, session_id: &str) -> String {
    let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
        return session_id.to_string();
    };
    let Some((_, suffix)) = stem.rsplit_once('_') else {
        return session_id.to_string();
    };

    let bytes = suffix.as_bytes();
    let uuid_like = bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        });
    if uuid_like {
        suffix.to_string()
    } else {
        session_id.to_string()
    }
}

fn same_rollout_path(left: &Path, right: &Path) -> bool {
    normalized_rollout_path(&left.to_string_lossy())
        == normalized_rollout_path(&right.to_string_lossy())
}

fn resolve_paginated_chain_from_descriptors(
    current_path: &Path,
    descriptors: &[RolloutDescriptor],
) -> Result<Vec<HistorySegment>, String> {
    let mut current = descriptors
        .iter()
        .find(|descriptor| same_rollout_path(&descriptor.path, current_path))
        .ok_or_else(|| format!("未找到 Codex 分页会话元数据：{}", current_path.display()))?;
    let mut visited = HashSet::new();
    let mut reversed = vec![HistorySegment {
        path: current.path.clone(),
        end_byte_offset: None,
    }];
    visited.insert(normalized_rollout_path(&current.path.to_string_lossy()));

    while let Some(history_base) = &current.history_base {
        if reversed.len() >= 64 {
            return Err(format!(
                "Codex 分页历史链过长，已停止处理：{}",
                current_path.display()
            ));
        }
        let referenced_by_other_session = descriptors.iter().any(|candidate| {
            candidate.rollout_id == history_base.thread_id
                && candidate.session_id != current.session_id
        });
        let referenced_by_same_session = descriptors.iter().any(|candidate| {
            candidate.rollout_id == history_base.thread_id
                && candidate.session_id == current.session_id
        });
        if referenced_by_other_session && !referenced_by_same_session {
            return Err(format!(
                "Codex 分页历史引用了不同会话，已停止处理：{}",
                current_path.display()
            ));
        }

        let mut candidates = descriptors
            .iter()
            .filter(|candidate| candidate.session_id == current.session_id)
            .filter(|candidate| candidate.rollout_id == history_base.thread_id)
            .filter(|candidate| !same_rollout_path(&candidate.path, &current.path))
            .filter(|candidate| {
                !visited.contains(&normalized_rollout_path(&candidate.path.to_string_lossy()))
            })
            .filter(|candidate| candidate.file_len >= history_base.end_byte_offset)
            .filter(
                |candidate| match (candidate.created_at, current.created_at) {
                    (Some(candidate_time), Some(current_time)) => candidate_time <= current_time,
                    _ => true,
                },
            )
            .map(|candidate| (candidate.file_len - history_base.end_byte_offset, candidate))
            .collect::<Vec<_>>();
        candidates.sort_by(|(left_delta, left), (right_delta, right)| {
            left_delta
                .cmp(right_delta)
                .then_with(|| right.created_at.cmp(&left.created_at))
                .then_with(|| left.path.cmp(&right.path))
        });

        let Some((best_delta, base)) = candidates.first().copied() else {
            return Err(format!(
                "Codex 分页历史基座缺失，已禁止危险操作：{}",
                current_path.display()
            ));
        };
        if candidates.get(1).is_some_and(|(delta, candidate)| {
            *delta == best_delta && candidate.created_at == base.created_at
        }) {
            return Err(format!(
                "Codex 分页历史基座不唯一，已禁止危险操作：{}",
                current_path.display()
            ));
        }

        let normalized = normalized_rollout_path(&base.path.to_string_lossy());
        if !visited.insert(normalized) {
            return Err(format!(
                "Codex 分页历史形成循环，已停止处理：{}",
                current_path.display()
            ));
        }
        reversed.push(HistorySegment {
            path: base.path.clone(),
            end_byte_offset: Some(history_base.end_byte_offset),
        });
        current = base;
    }

    reversed.reverse();
    Ok(reversed)
}

fn rollout_descriptors_for_path(path: &Path) -> Vec<RolloutDescriptor> {
    let mut roots = session_roots();
    if !roots.iter().any(|root| path.starts_with(root)) {
        if let Some(parent) = path.parent() {
            roots.push(parent.to_path_buf());
        }
    }
    let mut files = Vec::new();
    for root in roots {
        collect_jsonl_files(&root, &mut files);
    }
    files
        .iter()
        .filter_map(|candidate| read_rollout_descriptor(candidate).ok())
        .collect()
}

fn resolve_paginated_chain(path: &Path) -> Result<Vec<HistorySegment>, String> {
    let descriptors = rollout_descriptors_for_path(path);
    resolve_paginated_chain_from_descriptors(path, &descriptors)
}

fn mark_residual_rollouts(
    sessions: &mut [SessionMeta],
    native_rollouts: Option<&HashMap<String, String>>,
) {
    if let Some(native_rollouts) = native_rollouts {
        for session in sessions {
            session.residual =
                native_rollouts
                    .get(&session.session_id)
                    .is_none_or(|current_path| {
                        session
                            .source_path
                            .as_deref()
                            .is_none_or(|path| normalized_rollout_path(path) != *current_path)
                    });
            if session.residual {
                session.resume_command = None;
            }
        }
        return;
    }

    // Older Codex versions may not have a readable state database. In that
    // case, treat the newest active rollout for each thread as current and
    // expose every other file as residual rather than hiding it.
    let mut current_by_id: HashMap<String, usize> = HashMap::new();
    for (index, candidate) in sessions.iter().enumerate() {
        if candidate
            .source_path
            .as_deref()
            .is_some_and(is_archived_rollout)
        {
            continue;
        }
        let replace = current_by_id
            .get(&candidate.session_id)
            .is_none_or(|existing_index| is_newer_rollout(candidate, &sessions[*existing_index]));
        if replace {
            current_by_id.insert(candidate.session_id.clone(), index);
        }
    }
    for (index, session) in sessions.iter_mut().enumerate() {
        session.residual = current_by_id.get(&session.session_id).copied() != Some(index);
        if !current_by_id.contains_key(&session.session_id)
            && session
                .source_path
                .as_deref()
                .is_some_and(is_archived_rollout)
        {
            session.archived = true;
            session.residual = false;
        }
        if session.residual || session.archived {
            session.resume_command = None;
        }
    }
}

fn is_newer_rollout(candidate: &SessionMeta, existing: &SessionMeta) -> bool {
    let candidate_time = candidate
        .last_active_at
        .or(candidate.created_at)
        .unwrap_or(0);
    let existing_time = existing.last_active_at.or(existing.created_at).unwrap_or(0);
    candidate_time > existing_time
        || (candidate_time == existing_time && candidate.source_path > existing.source_path)
}

fn is_archived_rollout(path: &str) -> bool {
    Path::new(path).components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case("archived_sessions"))
    })
}

fn normalized_rollout_path(path: &str) -> String {
    #[cfg(not(target_os = "windows"))]
    {
        return path.trim_end_matches('/').to_string();
    }
    #[cfg(target_os = "windows")]
    {
        let path = path.replace('/', "\\");
        path.strip_prefix("\\\\?\\")
            .unwrap_or(&path)
            .trim_end_matches('\\')
            .to_lowercase()
    }
}

/// Read the same project assignments used by the Codex desktop sidebar.  A
/// rollout's `cwd` can point at a temporary worktree, so it is not a reliable
/// project identity and must not be used for grouping when this mapping exists.
fn load_project_context() -> CodexProjectContext {
    let path = codex_config_dir().join(".codex-global-state.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return CodexProjectContext::default();
    };
    let Ok(root) = serde_json::from_str::<Value>(&text) else {
        log::warn!("Failed to parse Codex global state {}", path.display());
        return CodexProjectContext::default();
    };

    project_context_from_value(&root)
}

fn project_context_from_value(root: &Value) -> CodexProjectContext {
    let projects = root
        .get("local-projects")
        .and_then(Value::as_object)
        .map(|items| {
            items
                .iter()
                .map(|(project_id, value)| {
                    let root_path = value
                        .get("rootPaths")
                        .and_then(Value::as_array)
                        .and_then(|paths| paths.first())
                        .and_then(Value::as_str)
                        .filter(|path| !path.trim().is_empty())
                        .map(str::to_string);
                    let name = value
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.trim().is_empty())
                        .map(str::to_string)
                        .or_else(|| root_path.as_deref().and_then(path_basename))
                        .unwrap_or_else(|| "项目".to_string());
                    (project_id.clone(), CodexProjectBinding { name, root_path })
                })
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();

    let mut thread_projects = HashMap::new();
    if let Some(assignments) = root
        .get("thread-project-assignments")
        .and_then(Value::as_object)
    {
        for (thread_id, assignment) in assignments {
            let project_id = assignment
                .get("projectId")
                .or_else(|| assignment.get("project_id"))
                .and_then(Value::as_str);
            if let Some(binding) = project_id.and_then(|id| projects.get(id)) {
                thread_projects.insert(thread_id.clone(), binding.clone());
            }
        }
    }

    let projectless_threads = root
        .get("projectless-thread-ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();

    CodexProjectContext {
        thread_projects,
        projectless_threads,
    }
}

fn load_thread_titles() -> HashMap<String, String> {
    let config_dir = codex_config_dir();
    let config_text = read_codex_config_text().unwrap_or_default();
    let db_paths = codex_state_db_paths(&config_dir, &config_text);
    load_thread_titles_from_paths(&config_dir.join(CODEX_SESSION_INDEX_FILENAME), &db_paths)
}

fn load_native_active_rollouts() -> Option<HashMap<String, String>> {
    let config_dir = codex_config_dir();
    let config_text = read_codex_config_text().unwrap_or_default();
    let mut loaded = false;
    let mut rollouts = HashMap::new();
    for db_path in codex_state_db_paths(&config_dir, &config_text) {
        if let Some(items) = load_native_active_rollouts_from_db(&db_path) {
            loaded = true;
            rollouts.extend(items);
        }
    }
    loaded.then_some(rollouts)
}

fn load_native_active_rollouts_from_db(db_path: &Path) -> Option<HashMap<String, String>> {
    if !db_path.is_file() {
        return None;
    }
    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|err| {
        log::warn!(
            "Failed to open Codex state database {}: {err}",
            db_path.display()
        );
        err
    })
    .ok()?;
    conn.busy_timeout(Duration::from_secs(2)).ok()?;
    let mut stmt = conn
        .prepare("SELECT id, rollout_path FROM threads WHERE archived = 0")
        .map_err(|err| {
            log::warn!(
                "Failed to read active Codex rollouts from {}: {err}",
                db_path.display()
            );
            err
        })
        .ok()?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .ok()?;

    Some(
        rows.flatten()
            .filter_map(|(id, path)| {
                let id = id.trim();
                let path = path.trim();
                (!id.is_empty() && !path.is_empty())
                    .then(|| (id.to_string(), normalized_rollout_path(path)))
            })
            .collect(),
    )
}

fn load_thread_titles_from_paths(
    session_index_path: &Path,
    db_paths: &[PathBuf],
) -> HashMap<String, String> {
    let mut titles = load_thread_titles_from_session_index(session_index_path);
    for db_path in db_paths {
        titles.extend(load_thread_titles_from_db(db_path));
    }
    titles
}

fn load_thread_titles_from_session_index(index_path: &Path) -> HashMap<String, String> {
    if !index_path.exists() {
        return HashMap::new();
    }

    let file = match File::open(index_path) {
        Ok(file) => file,
        Err(err) => {
            log::warn!(
                "Failed to open Codex session index {}: {err}",
                index_path.display()
            );
            return HashMap::new();
        }
    };

    let reader = BufReader::new(file);
    let mut titles = HashMap::new();
    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => continue,
        };
        let Ok(entry) = serde_json::from_str::<SessionIndexEntry>(line.trim()) else {
            continue;
        };
        let id = entry.id.trim();
        let title = entry.thread_name.trim();
        if !id.is_empty() && !title.is_empty() {
            titles.insert(id.to_string(), title.to_string());
        }
    }

    titles
}

fn load_thread_titles_from_db(db_path: &Path) -> HashMap<String, String> {
    if !db_path.exists() {
        return HashMap::new();
    }

    let conn = match Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(conn) => conn,
        Err(err) => {
            log::warn!(
                "Failed to open Codex state database {}: {err}",
                db_path.display()
            );
            return HashMap::new();
        }
    };
    // Codex keeps this DB open and write-locked while running; without a busy
    // timeout a read during a write fails immediately and titles silently drop.
    if let Err(err) = conn.busy_timeout(Duration::from_secs(2)) {
        log::warn!(
            "Failed to set Codex state database busy timeout for {}: {err}",
            db_path.display()
        );
        return HashMap::new();
    }

    // Mirror Codex's own `distinct_thread_metadata_title`: keep a title only
    // when it differs from the first user message. Push the comparison into SQL
    // (NULL-safe) so we never SELECT the unbounded `first_user_message` blob —
    // it can grow large enough to OOM (openai/codex#29007).
    let mut stmt = match conn.prepare(
        "SELECT id, title FROM threads \
         WHERE title <> '' \
         AND (first_user_message IS NULL OR TRIM(title) <> TRIM(first_user_message))",
    ) {
        Ok(stmt) => stmt,
        Err(err) => {
            log::warn!(
                "Failed to prepare Codex thread title query for {}: {err}",
                db_path.display()
            );
            return HashMap::new();
        }
    };

    let rows = match stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let title: String = row.get(1)?;
        Ok((id, title))
    }) {
        Ok(rows) => rows,
        Err(err) => {
            log::warn!(
                "Failed to query Codex thread titles from {}: {err}",
                db_path.display()
            );
            return HashMap::new();
        }
    };

    rows.flatten()
        .filter_map(|(id, title)| {
            let id = id.trim();
            let title = title.trim();
            if id.is_empty() || title.is_empty() {
                None
            } else {
                Some((id.to_string(), title.to_string()))
            }
        })
        .collect()
}

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    let mut messages = Vec::new();
    let mut remaining_bytes = MAX_MESSAGES_FILE_BYTES;

    for segment in resolve_paginated_chain(path)? {
        let file_len = segment
            .path
            .metadata()
            .map_err(|e| format!("Failed to inspect session file: {e}"))?
            .len();
        let segment_len = segment.end_byte_offset.unwrap_or(file_len);
        if segment_len > remaining_bytes {
            return Err(format!(
                "Codex 分页历史合计超过 {} MB 加载上限",
                MAX_MESSAGES_FILE_BYTES / 1024 / 1024
            ));
        }
        remaining_bytes -= segment_len;
        messages.extend(load_segment_messages(
            &segment.path,
            segment.end_byte_offset,
        )?);
    }

    Ok(messages)
}

fn load_segment_messages(
    path: &Path,
    end_byte_offset: Option<u64>,
) -> Result<Vec<SessionMessage>, String> {
    ensure_readable_size(path)?;
    let file_len = path
        .metadata()
        .map_err(|e| format!("Failed to inspect session file: {e}"))?
        .len();
    if end_byte_offset.is_some_and(|offset| offset > file_len) {
        return Err(format!(
            "Codex 分页历史超出基座文件范围，已停止读取：{}",
            path.display()
        ));
    }
    let file = File::open(path).map_err(|e| format!("Failed to open session file: {e}"))?;
    let reader = BufReader::new(file.take(end_byte_offset.unwrap_or(u64::MAX)));
    let mut messages = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(value) => value,
            Err(_) => continue,
        };
        let value: Value = match serde_json::from_str(&line) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };

        if value.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }

        let payload = match value.get("payload") {
            Some(payload) => payload,
            None => continue,
        };

        let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");

        // Codex uses separate payload types for tool interactions
        let (role, content) = match payload_type {
            "message" => {
                let role = payload
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let content = payload.get("content").map(extract_text).unwrap_or_default();
                if !matches!(role, "user" | "assistant")
                    || (role == "user" && is_internal_user_context(payload, &content))
                {
                    continue;
                }
                (role.to_string(), content)
            }
            "function_call" => {
                let name = payload
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                ("assistant".to_string(), format!("[Tool: {name}]"))
            }
            "function_call_output" => {
                let output = payload
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                ("tool".to_string(), output)
            }
            _ => continue,
        };

        if content.trim().is_empty() {
            continue;
        }

        let ts = value.get("timestamp").and_then(parse_timestamp_to_ms);

        messages.push(SessionMessage { role, content, ts });
    }

    Ok(messages)
}

pub fn validate_delete(path: &Path, session_id: &str) -> Result<(), String> {
    deletion_paths(path, session_id).map(|_| ())
}

fn deletion_paths(path: &Path, session_id: &str) -> Result<Vec<PathBuf>, String> {
    let meta = parse_session(path)
        .ok_or_else(|| format!("Failed to parse Codex session metadata: {}", path.display()))?;

    if meta.session_id != session_id {
        return Err(format!(
            "Codex session ID mismatch: expected {session_id}, found {}",
            meta.session_id
        ));
    }

    let descriptors = rollout_descriptors_for_path(path);
    let target = descriptors
        .iter()
        .find(|descriptor| same_rollout_path(&descriptor.path, path))
        .ok_or_else(|| format!("无法读取 Codex 会话元数据：{}", path.display()))?;

    if target.history_base.is_some() {
        return resolve_paginated_chain_from_descriptors(path, &descriptors)
            .map(|chain| chain.into_iter().map(|segment| segment.path).collect());
    }

    for continuation in descriptors
        .iter()
        .filter(|descriptor| descriptor.session_id == session_id)
        .filter(|descriptor| descriptor.history_base.is_some())
    {
        match resolve_paginated_chain_from_descriptors(&continuation.path, &descriptors) {
            Ok(chain)
                if chain
                    .iter()
                    .any(|segment| same_rollout_path(&segment.path, path)) =>
            {
                return Err("该记录是当前会话依赖的分页历史，不能单独删除".into());
            }
            Ok(_) => {}
            Err(error) => {
                return Err(format!(
                    "同 ID 的 Codex 分页历史无法确认，已禁止删除以保护会话：{error}"
                ));
            }
        }
    }

    Ok(vec![path.to_path_buf()])
}

pub fn delete_session(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    let paths = deletion_paths(path, session_id)?;
    let mut allowed_roots = session_roots();
    if !allowed_roots.iter().any(|candidate| candidate == root) {
        allowed_roots.push(root.to_path_buf());
    }
    let allowed_roots = allowed_roots
        .iter()
        .filter_map(|candidate| candidate.canonicalize().ok())
        .collect::<Vec<_>>();
    let mut validated_paths = Vec::new();
    for candidate in paths {
        let validated = candidate.canonicalize().map_err(|error| {
            format!(
                "无法确认 Codex 分页历史文件 {}：{error}",
                candidate.display()
            )
        })?;
        if !allowed_roots.iter().any(|root| validated.starts_with(root)) {
            return Err(format!(
                "Codex 分页历史文件位于允许目录之外：{}",
                candidate.display()
            ));
        }
        validated_paths.push(validated);
    }

    // Remove the active leaf first. If a later base removal fails, no surviving
    // continuation is left pointing at a missing history file.
    for candidate in validated_paths.into_iter().rev() {
        std::fs::remove_file(&candidate).map_err(|e| {
            format!(
                "Failed to delete Codex session file {}: {e}",
                candidate.display()
            )
        })?;
    }

    Ok(true)
}

fn parse_session(path: &Path) -> Option<SessionMeta> {
    parse_session_with_titles(path, &HashMap::new())
}

fn parse_session_with_titles(
    path: &Path,
    thread_titles: &HashMap<String, String>,
) -> Option<SessionMeta> {
    let (head, tail) = read_head_tail_lines(path, 10, 300)
        .map_err(|error| {
            super::utils::scan_warning(format!("无法读取 {}：{error}", path.display()));
            error
        })
        .ok()?;

    let mut session_id: Option<String> = None;
    let mut project_dir: Option<String> = None;
    let mut created_at: Option<i64> = None;
    let mut first_user_message: Option<String> = None;

    // Extract metadata and first user message from head lines
    for line in &head {
        let value: Value = match serde_json::from_str(line) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        if created_at.is_none() {
            created_at = value.get("timestamp").and_then(parse_timestamp_to_ms);
        }
        if value.get("type").and_then(Value::as_str) == Some("session_meta") {
            if let Some(payload) = value.get("payload") {
                if is_subagent_source(payload.get("source")) {
                    return None;
                }
                if session_id.is_none() {
                    session_id = payload
                        .get("id")
                        .and_then(Value::as_str)
                        .map(|s| s.to_string());
                }
                if project_dir.is_none() {
                    project_dir = payload
                        .get("cwd")
                        .and_then(Value::as_str)
                        .map(|s| s.to_string());
                }
                if let Some(ts) = payload.get("timestamp").and_then(parse_timestamp_to_ms) {
                    created_at.get_or_insert(ts);
                }
            }
        }
        // Extract first user message as title candidate
        if first_user_message.is_none()
            && value.get("type").and_then(Value::as_str) == Some("response_item")
        {
            if let Some(payload) = value.get("payload") {
                if payload.get("type").and_then(Value::as_str) == Some("message")
                    && payload.get("role").and_then(Value::as_str) == Some("user")
                {
                    let text = payload.get("content").map(extract_text).unwrap_or_default();
                    if !is_internal_user_context(payload, &text) {
                        if let Some(title) = title_candidate_from_user_message(&text) {
                            first_user_message = Some(title);
                        }
                    }
                }
            }
        }
        if session_id.is_some()
            && project_dir.is_some()
            && created_at.is_some()
            && first_user_message.is_some()
        {
            break;
        }
    }

    // Extract last_active_at and summary from tail lines (reverse order)
    let mut last_active_at: Option<i64> = None;
    let mut summary: Option<String> = None;
    let mut latest_user_message: Option<String> = None;

    for line in tail.iter().rev() {
        let value: Value = match serde_json::from_str(line) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        if last_active_at.is_none() {
            last_active_at = value.get("timestamp").and_then(parse_timestamp_to_ms);
        }
        if summary.is_none() && value.get("type").and_then(Value::as_str) == Some("response_item") {
            if let Some(payload) = value.get("payload") {
                if payload.get("type").and_then(Value::as_str) == Some("message") {
                    let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
                    let text = payload.get("content").map(extract_text).unwrap_or_default();
                    let visible = matches!(role, "user" | "assistant")
                        && !(role == "user" && is_internal_user_context(payload, &text));
                    if visible && latest_user_message.is_none() && role == "user" {
                        latest_user_message = title_candidate_from_user_message(&text);
                    }
                    if visible && !text.trim().is_empty() {
                        summary = Some(text);
                    }
                }
            }
        }
        if last_active_at.is_some() && summary.is_some() && latest_user_message.is_some() {
            break;
        }
    }

    let session_id = session_id.or_else(|| infer_session_id_from_filename(path));
    let session_id = session_id?;

    // Codex's sidebar uses the stored thread title.  Keep the latest complete
    // user message as the summary, but do not replace the visible sidebar title
    // with it.
    let title = thread_titles
        .get(&session_id)
        .cloned()
        .or_else(|| latest_user_message.filter(|text| !text.trim().is_empty()))
        .or_else(|| first_user_message.map(|t| truncate_summary(&t, TITLE_MAX_CHARS)))
        .or_else(|| {
            project_dir
                .as_deref()
                .and_then(path_basename)
                .map(|v| v.to_string())
        });

    let summary = summary.map(|text| truncate_summary(&text, 160));

    Some(SessionMeta {
        provider_id: PROVIDER_ID.to_string(),
        session_id: session_id.clone(),
        residual: false,
        archived: false,
        cleanup_pending: false,
        title,
        summary,
        project_dir,
        project_name: None,
        created_at,
        last_active_at,
        source_path: Some(path.to_string_lossy().to_string()),
        resume_command: is_safe_session_id(&session_id)
            .then(|| format!("codex resume {session_id}")),
    })
}

fn is_subagent_source(source: Option<&Value>) -> bool {
    source
        .and_then(|value| value.as_object())
        .map(|source| source.contains_key("subagent"))
        .unwrap_or(false)
}

fn is_internal_user_context(payload: &Value, text: &str) -> bool {
    let content_item_kinds = payload
        .get("internal_chat_message_metadata_passthrough")
        .and_then(|metadata| metadata.get("content_item_kinds"))
        .and_then(Value::as_array);
    if let Some(kinds) = content_item_kinds.filter(|kinds| !kinds.is_empty()) {
        return !kinds
            .iter()
            .filter_map(Value::as_str)
            .any(|kind| kind.starts_with("user."));
    }

    is_known_internal_context_text(text)
}

fn is_known_internal_context_text(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with("# AGENTS.md")
        || trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<recommended_plugins>")
}

fn title_candidate_from_user_message(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || is_known_internal_context_text(trimmed) {
        return None;
    }

    if trimmed.starts_with(VSCODE_CONTEXT_PREFIX) {
        return extract_codex_prompt_from_ide_context(trimmed);
    }

    Some(trimmed.to_string())
}

fn extract_codex_prompt_from_ide_context(text: &str) -> Option<String> {
    let normalized = text.replace("\r\n", "\n");
    let lines = normalized.lines().collect::<Vec<_>>();

    // VS Code injects the real prompt as the LAST "## My request for Codex:"
    // section, so keep the final matching heading. Earlier matches can be
    // headings that live inside the active selection / open file content.
    // Trade-off: if the request body itself repeats the heading, the title
    // truncates to its trailing part (rare; covered by tests below).
    let mut prompt: Option<String> = None;
    for (index, line) in lines.iter().enumerate() {
        let Some(inline_prompt) = codex_request_heading_payload(line) else {
            continue;
        };

        if !inline_prompt.is_empty() {
            prompt = Some(inline_prompt.to_string());
            continue;
        }

        let following_prompt = lines[index + 1..].join("\n").trim().to_string();
        prompt = (!following_prompt.is_empty()).then_some(following_prompt);
    }

    prompt
}

fn codex_request_heading_payload(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if !trimmed.starts_with('#') {
        return None;
    }

    let heading = trimmed.trim_start_matches('#').trim_start();
    let lowered = heading.to_ascii_lowercase();
    if !lowered.starts_with(CODEX_REQUEST_MARKER) {
        return None;
    }

    let suffix = heading[CODEX_REQUEST_MARKER.len()..].trim_start();
    if suffix.is_empty() {
        return Some("");
    }

    let Some(separator) = suffix.chars().next() else {
        return Some("");
    };
    if !matches!(separator, ':' | '：' | '-' | '—') {
        return None;
    }

    Some(
        suffix
            .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ':' | '：' | '-' | '—'))
            .trim(),
    )
}

fn infer_session_id_from_filename(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_string_lossy();
    UUID_RE.find(&file_name).map(|mat| mat.as_str().to_string())
}

fn collect_jsonl_files(root: &Path, files: &mut Vec<PathBuf>) {
    collect_files_where(
        root,
        |path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"),
        files,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_lookup_includes_archived_threads() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE threads(id TEXT, rollout_path TEXT, archived INTEGER);
            INSERT INTO threads VALUES('archived-1','archived.jsonl',1),('active-1','active.jsonl',0);").unwrap();
        drop(conn);
        let indexed = load_indexed_rollouts_from_paths(vec![path])
            .unwrap()
            .unwrap();
        assert_eq!(
            indexed.get("archived-1").map(String::as_str),
            Some("archived.jsonl")
        );
        assert!(indexed.contains_key("active-1"));
    }

    #[test]
    fn index_replacement_rejects_a_changed_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.json");
        std::fs::write(&path, "new state from agent").unwrap();
        assert!(atomic_write(&path, "manager edit".into(), "old snapshot").is_err());
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "new state from agent"
        );
    }
    use crate::session_paths::CODEX_STATE_DB_FILENAME;
    use tempfile::tempdir;

    #[test]
    fn cleanup_global_state_removes_thread_and_deleted_project() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join(".codex-global-state.json");
        std::fs::write(
            &path,
            r#"{"thread-project-assignments":{"thread-1":{"projectId":"project-1"}},"local-projects":{"project-1":{"rootPaths":["C:\\work\\gone"]}},"project-order":["project-1"]}"#,
        )
        .expect("write global state");

        cleanup_global_state(&path, "thread-1", Some(r"C:\work\gone"), true)
            .expect("clean global state");

        let value: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(value["thread-project-assignments"]
            .get("thread-1")
            .is_none());
        assert!(value["local-projects"].get("project-1").is_none());
        assert_eq!(value["project-order"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn cleanup_global_state_removes_all_thread_shell_state_but_keeps_project() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join(".codex-global-state.json");
        std::fs::write(
            &path,
            r#"{
                "local-projects":{"project-1":{"rootPaths":["C:\\work\\kept"]}},
                "project-order":["project-1"],
                "thread-project-assignments":{
                    "thread-1":{"projectId":"project-1"},
                    "thread-2":{"projectId":"project-1"}
                },
                "heartbeat-thread-permissions-by-id":{
                    "thread-1":{"approvalPolicy":"never"},
                    "thread-2":{"approvalPolicy":"never"}
                },
                "client-thread-bindings-v1":{
                    "client-1":"thread-1",
                    "client-2":"thread-2"
                },
                "thread-reference-capability:thread-1":true,
                "thread-reference-capability:thread-2":true,
                "thread-client-id-v1:local%3Athread-1":"client-1",
                "thread-tab-routes-v1:thread-1":{"routes":[]},
                "unread-thread-ids-by-host-v1":{"local":["thread-1","thread-2"]},
                "prompt-history":{
                    "thread-1":["deleted conversation"],
                    "thread-2":["kept conversation"],
                    "global":["text containing thread-1 remains"]
                }
            }"#,
        )
        .expect("write global state");

        cleanup_global_state(&path, "thread-1", Some(r"C:\work\kept"), false)
            .expect("clean global state");

        let value: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let serialized = serde_json::to_string(&value).unwrap();

        assert!(value["local-projects"].get("project-1").is_some());
        assert_eq!(value["project-order"], serde_json::json!(["project-1"]));
        assert!(value["thread-project-assignments"]
            .get("thread-1")
            .is_none());
        assert!(value["thread-project-assignments"]
            .get("thread-2")
            .is_some());
        assert!(value["heartbeat-thread-permissions-by-id"]
            .get("thread-1")
            .is_none());
        assert_eq!(value["client-thread-bindings-v1"].get("client-1"), None);
        assert_eq!(value["client-thread-bindings-v1"]["client-2"], "thread-2");
        assert!(value.get("thread-reference-capability:thread-1").is_none());
        assert!(value.get("thread-reference-capability:thread-2").is_some());
        assert!(value.get("thread-client-id-v1:local%3Athread-1").is_none());
        assert!(value.get("thread-tab-routes-v1:thread-1").is_none());
        assert_eq!(
            value["unread-thread-ids-by-host-v1"]["local"],
            serde_json::json!(["thread-2"])
        );
        assert!(serialized.contains("text containing thread-1 remains"));
    }

    #[test]
    fn cleanup_index_databases_removes_thread_history_rows() {
        let temp = tempdir().expect("tempdir");
        let state_path = temp.path().join(CODEX_STATE_DB_FILENAME);
        let state = Connection::open(&state_path).expect("open state database");
        state
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY);\
                 CREATE TABLE thread_artifacts (id TEXT PRIMARY KEY, thread_id TEXT NOT NULL);\
                 INSERT INTO threads VALUES ('thread-1'), ('thread-2');\
                 INSERT INTO thread_artifacts VALUES ('artifact-1', 'thread-1'), ('artifact-2', 'thread-2');",
            )
            .expect("seed state database");
        drop(state);

        let history_path = temp.path().join(CODEX_THREAD_HISTORY_DB_FILENAME);
        let history = Connection::open(&history_path).expect("open history database");
        history
            .execute_batch(
                "CREATE TABLE thread_history_projection_state (thread_id TEXT PRIMARY KEY);\
                 CREATE TABLE thread_items (thread_id TEXT NOT NULL, item_id TEXT NOT NULL);\
                 CREATE TABLE thread_realtime_items (thread_id TEXT NOT NULL, item_id TEXT NOT NULL);\
                 CREATE TABLE thread_turns (thread_id TEXT NOT NULL, turn_id TEXT NOT NULL);\
                 INSERT INTO thread_history_projection_state VALUES ('thread-1'), ('thread-2');\
                 INSERT INTO thread_items VALUES ('thread-1', 'item-1'), ('thread-2', 'item-2');\
                 INSERT INTO thread_realtime_items VALUES ('thread-1', 'live-1'), ('thread-2', 'live-2');\
                 INSERT INTO thread_turns VALUES ('thread-1', 'turn-1'), ('thread-2', 'turn-2');",
            )
            .expect("seed history database");
        drop(history);

        let desktop_dir = temp.path().join("sqlite");
        std::fs::create_dir(&desktop_dir).expect("create desktop database directory");
        let desktop_path = desktop_dir.join("codex-dev.db");
        let desktop = Connection::open(&desktop_path).expect("open desktop database");
        desktop
            .execute_batch(
                "CREATE TABLE local_thread_catalog (thread_id TEXT PRIMARY KEY);\
                 CREATE TABLE local_thread_catalog_scan_entries (thread_id TEXT PRIMARY KEY);\
                 CREATE TABLE thread_timeline_ledger (thread_id TEXT NOT NULL, sequence INTEGER NOT NULL);\
                 CREATE TABLE local_thread_catalog_metadata (id INTEGER PRIMARY KEY, catalog_revision INTEGER NOT NULL);\
                 INSERT INTO local_thread_catalog VALUES ('thread-1'), ('thread-2');\
                 INSERT INTO local_thread_catalog_scan_entries VALUES ('thread-1'), ('thread-2');\
                 INSERT INTO thread_timeline_ledger VALUES ('thread-1', 1), ('thread-2', 1);\
                 INSERT INTO local_thread_catalog_metadata VALUES (1, 7);",
            )
            .expect("seed desktop database");
        drop(desktop);

        cleanup_index_databases(temp.path(), "", "thread-1", None, false)
            .expect("clean Codex indexes");

        let state = Connection::open(state_path).expect("reopen state database");
        let remaining_threads: Vec<String> = state
            .prepare("SELECT id FROM threads ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(remaining_threads, vec!["thread-2"]);

        let history = Connection::open(history_path).expect("reopen history database");
        for table in [
            "thread_history_projection_state",
            "thread_items",
            "thread_realtime_items",
            "thread_turns",
        ] {
            let deleted: i64 = history
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE thread_id='thread-1'"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let kept: i64 = history
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE thread_id='thread-2'"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(deleted, 0, "{table} kept the deleted thread");
            assert_eq!(kept, 1, "{table} removed an unrelated thread");
        }

        let desktop = Connection::open(desktop_path).expect("reopen desktop database");
        for table in [
            "local_thread_catalog",
            "local_thread_catalog_scan_entries",
            "thread_timeline_ledger",
        ] {
            let deleted: i64 = desktop
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE thread_id='thread-1'"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let kept: i64 = desktop
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE thread_id='thread-2'"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(deleted, 0, "{table} kept the deleted thread");
            assert_eq!(kept, 1, "{table} removed an unrelated thread");
        }
        let catalog_revision: i64 = desktop
            .query_row(
                "SELECT catalog_revision FROM local_thread_catalog_metadata WHERE id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(catalog_revision, 8);
    }

    fn write_codex_session(path: &Path, session_id: &str, message: &str) {
        std::fs::write(
            path,
            format!(
                "{{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session_id}\",\"cwd\":\"/tmp/project\"}}}}\n\
                 {{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":\"{message}\"}}}}\n",
            ),
        )
        .expect("write session");
    }

    fn write_paginated_session(path: &Path, session_id: &str, base_offset: u64, message: &str) {
        write_paginated_session_with_base(path, session_id, session_id, base_offset, message);
    }

    fn write_paginated_session_with_base(
        path: &Path,
        session_id: &str,
        base_thread_id: &str,
        base_offset: u64,
        message: &str,
    ) {
        std::fs::write(
            path,
            format!(
                "{{\"timestamp\":\"2026-03-07T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session_id}\",\"cwd\":\"/tmp/project\",\"history_mode\":\"paginated\",\"history_base\":{{\"thread_id\":\"{base_thread_id}\",\"end_ordinal_exclusive\":2,\"end_byte_offset\":{base_offset}}}}}}}\n\
                 {{\"timestamp\":\"2026-03-07T21:50:13Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":\"{message}\"}}}}\n",
            ),
        )
        .expect("write paginated session");
    }

    #[test]
    fn scan_collapses_paginated_history_base_into_current_session() {
        let temp = tempdir().expect("tempdir");
        let active = temp.path().join("sessions");
        std::fs::create_dir_all(&active).expect("active dir");
        let base = active.join("base.jsonl");
        let current = active.join("current.jsonl");
        write_codex_session(&base, "thread-id", "Earlier message");
        write_paginated_session(
            &current,
            "thread-id",
            base.metadata().expect("base metadata").len(),
            "Later message",
        );

        let native_rollouts = HashMap::from([(
            "thread-id".to_string(),
            normalized_rollout_path(&current.to_string_lossy()),
        )]);
        let sessions = scan_sessions_in_roots_with_context(
            &[active],
            &HashMap::new(),
            &CodexProjectContext::default(),
            Some(&native_rollouts),
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].source_path.as_deref(), current.to_str());
        assert!(!sessions[0].residual);
        assert_eq!(sessions[0].title.as_deref(), Some("Later message"));
    }

    #[test]
    fn load_messages_merges_paginated_history_at_recorded_offset() {
        let temp = tempdir().expect("tempdir");
        let base = temp.path().join("base.jsonl");
        let current = temp.path().join("current.jsonl");
        let base_prefix = concat!(
            "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-id\",\"cwd\":\"/tmp/project\"}}\n",
            "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"Earlier message\"}}\n"
        );
        std::fs::write(
            &base,
            format!(
                "{base_prefix}{{\"timestamp\":\"2026-03-06T21:50:14Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"Discarded tail\"}}}}\n"
            ),
        )
        .expect("write base");
        write_paginated_session(
            &current,
            "thread-id",
            base_prefix.len() as u64,
            "Later message",
        );

        let messages = load_messages(&current).expect("load paginated messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "Earlier message");
        assert_eq!(messages[1].content, "Later message");
        assert!(messages
            .iter()
            .all(|message| message.content != "Discarded tail"));
    }

    #[test]
    fn load_messages_follows_multi_page_rollout_instance_ids() {
        let temp = tempdir().expect("tempdir");
        let logical_id = "01a076ed-bae5-7a02-a191-d9a4b880afc5";
        let middle_id = "01a0868f-2daa-7f72-a3a8-870536bf6629";
        let current_id = "01a08757-bd33-7643-96cb-7d8f560cbbe3";
        let base = temp
            .path()
            .join(format!("rollout-2026-09-06T22-34-46-{logical_id}.jsonl"));
        let middle = temp.path().join(format!(
            "rollout-2026-09-09T23-25-25-{logical_id}_{middle_id}.jsonl"
        ));
        let current = temp.path().join(format!(
            "rollout-2026-09-10T03-04-29-{logical_id}_{current_id}.jsonl"
        ));
        write_codex_session(&base, logical_id, "Earlier message");
        write_paginated_session_with_base(
            &middle,
            logical_id,
            logical_id,
            base.metadata().expect("base metadata").len(),
            "Middle message",
        );
        write_paginated_session_with_base(
            &current,
            logical_id,
            middle_id,
            middle.metadata().expect("middle metadata").len(),
            "Later message",
        );

        let messages = load_messages(&current).expect("load multi-page history");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].content, "Earlier message");
        assert_eq!(messages[1].content, "Middle message");
        assert_eq!(messages[2].content, "Later message");

        let native_rollouts = HashMap::from([(
            logical_id.to_string(),
            normalized_rollout_path(&current.to_string_lossy()),
        )]);
        let sessions = scan_sessions_in_roots_with_context(
            &[temp.path().to_path_buf()],
            &HashMap::new(),
            &CodexProjectContext::default(),
            Some(&native_rollouts),
        );
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].source_path.as_deref(), current.to_str());
        assert!(!sessions[0].residual);
        assert_eq!(sessions[0].title.as_deref(), Some("Later message"));
    }

    #[test]
    fn deleting_paginated_current_session_removes_the_whole_chain() {
        let temp = tempdir().expect("tempdir");
        let base = temp.path().join("base.jsonl");
        let current = temp.path().join("current.jsonl");
        write_codex_session(&base, "thread-id", "Earlier message");
        write_paginated_session(
            &current,
            "thread-id",
            base.metadata().expect("base metadata").len(),
            "Later message",
        );

        delete_session(temp.path(), &current, "thread-id").expect("delete paginated session");

        assert!(!current.exists());
        assert!(!base.exists());
    }

    #[test]
    fn deleting_paginated_history_base_is_rejected() {
        let temp = tempdir().expect("tempdir");
        let base = temp.path().join("base.jsonl");
        let current = temp.path().join("current.jsonl");
        write_codex_session(&base, "thread-id", "Earlier message");
        write_paginated_session(
            &current,
            "thread-id",
            base.metadata().expect("base metadata").len(),
            "Later message",
        );

        let error = validate_delete(&base, "thread-id").expect_err("base must be protected");

        assert!(error.contains("分页历史"));
        assert!(base.exists());
        assert!(current.exists());
    }

    #[test]
    fn missing_paginated_history_base_blocks_deletion() {
        let temp = tempdir().expect("tempdir");
        let current = temp.path().join("current.jsonl");
        write_paginated_session(&current, "thread-id", 4096, "Later message");

        let error = validate_delete(&current, "thread-id").expect_err("missing base must block");

        assert!(error.contains("基座缺失"));
        assert!(current.exists());
    }

    #[test]
    fn session_roots_include_archives_for_residual_recovery() {
        let roots = session_roots();
        assert_eq!(roots.len(), 2);
        assert_eq!(
            roots[0].file_name().and_then(|name| name.to_str()),
            Some("sessions")
        );
        assert_eq!(
            roots[1].file_name().and_then(|name| name.to_str()),
            Some("archived_sessions")
        );
    }

    #[test]
    fn native_active_rollouts_loader_ignores_archived_threads_and_normalizes_paths() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("state.sqlite");
        {
            let conn = Connection::open(&db_path).expect("open database");
            conn.execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, archived INTEGER NOT NULL);",
            )
            .expect("create threads table");
            conn.execute(
                "INSERT INTO threads (id, rollout_path, archived) VALUES (?1, ?2, 0)",
                rusqlite::params![
                    "active-thread",
                    r"\\?\C:\Users\test\.codex\sessions\active.jsonl"
                ],
            )
            .expect("insert active thread");
            conn.execute(
                "INSERT INTO threads (id, rollout_path, archived) VALUES (?1, ?2, 1)",
                rusqlite::params![
                    "archived-thread",
                    r"C:\Users\test\.codex\archived_sessions\archived.jsonl"
                ],
            )
            .expect("insert archived thread");
        }

        let rollouts = load_native_active_rollouts_from_db(&db_path).expect("load rollouts");

        assert_eq!(rollouts.len(), 1);
        let expected = normalized_rollout_path(r"\\?\C:\Users\test\.codex\sessions\active.jsonl");
        assert_eq!(rollouts.get("active-thread"), Some(&expected));
        assert!(!rollouts.contains_key("archived-thread"));
    }

    #[test]
    fn native_rollout_path_is_authoritative_when_duplicate_files_share_a_thread_id() {
        let temp = tempdir().expect("tempdir");
        let active = temp.path().join("sessions");
        std::fs::create_dir_all(&active).expect("active dir");
        let native_path = active.join("native.jsonl");
        let leftover_path = active.join("leftover.jsonl");

        write_codex_session(&native_path, "thread-id", "Native title");
        write_codex_session(&leftover_path, "thread-id", "Leftover title");

        let native_rollouts = HashMap::from([(
            "thread-id".to_string(),
            normalized_rollout_path(&native_path.to_string_lossy()),
        )]);
        let sessions = scan_sessions_in_roots_with_context(
            &[active],
            &HashMap::new(),
            &CodexProjectContext::default(),
            Some(&native_rollouts),
        );

        assert_eq!(sessions.len(), 2);
        let native = sessions
            .iter()
            .find(|session| session.source_path.as_deref() == Some(&native_path.to_string_lossy()))
            .expect("native rollout");
        let leftover = sessions
            .iter()
            .find(|session| {
                session.source_path.as_deref() == Some(&leftover_path.to_string_lossy())
            })
            .expect("leftover rollout");
        assert!(!native.residual);
        assert!(native.resume_command.is_some());
        assert!(leftover.residual);
        assert!(leftover.resume_command.is_none());
    }

    #[test]
    fn scan_sessions_in_roots_marks_older_rollout_as_residual() {
        let temp = tempdir().expect("tempdir");
        let active = temp.path().join("sessions");
        std::fs::create_dir_all(&active).expect("active dir");

        write_codex_session(&active.join("older.jsonl"), "thread-id", "Older title");
        std::fs::write(
            active.join("newer.jsonl"),
            concat!(
                "{\"timestamp\":\"2026-03-07T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-id\",\"cwd\":\"/tmp/project\"}}\n",
                "{\"timestamp\":\"2026-03-07T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"Newer title\"}}\n"
            ),
        )
        .expect("write newer rollout");

        let sessions = scan_sessions_in_roots(&[active]);

        assert_eq!(sessions.len(), 2);
        let older = sessions
            .iter()
            .find(|session| {
                session
                    .source_path
                    .as_deref()
                    .is_some_and(|path| path.ends_with("older.jsonl"))
            })
            .expect("older rollout");
        let newer = sessions
            .iter()
            .find(|session| {
                session
                    .source_path
                    .as_deref()
                    .is_some_and(|path| path.ends_with("newer.jsonl"))
            })
            .expect("newer rollout");
        assert!(older.residual);
        assert!(older.resume_command.is_none());
        assert!(!newer.residual);
        assert_eq!(newer.title.as_deref(), Some("Newer title"));
    }

    #[test]
    fn delete_session_removes_jsonl_file() {
        let temp = tempdir().expect("tempdir");
        let path = temp
            .path()
            .join("rollout-2026-03-06T21-50-12-019cc369-bd7c-7891-b371-7b20b4fe0b18.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"019cc369-bd7c-7891-b371-7b20b4fe0b18\",\"cwd\":\"/tmp/project\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"hello\"}}\n"
            ),
        )
        .expect("write session");

        delete_session(temp.path(), &path, "019cc369-bd7c-7891-b371-7b20b4fe0b18")
            .expect("delete session");

        assert!(!path.exists());
    }

    #[test]
    fn parse_session_uses_first_user_message_as_title() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"test-id\",\"cwd\":\"/tmp/project\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"How do I deploy?\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:14Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"Here is how...\"}}\n"
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        assert_eq!(meta.title.as_deref(), Some("How do I deploy?"));
    }

    #[test]
    fn parse_session_prefers_saved_thread_title_like_codex_sidebar() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"test-id\",\"cwd\":\"/tmp/project\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"How do I deploy?\"}}\n"
            ),
        )
        .expect("write");

        let mut thread_titles = HashMap::new();
        thread_titles.insert(
            "test-id".to_string(),
            "Renamed deployment thread".to_string(),
        );

        let meta = parse_session_with_titles(&path, &thread_titles).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Renamed deployment thread"));
    }

    #[test]
    fn project_context_uses_codex_sidebar_assignments() {
        let value = serde_json::json!({
            "local-projects": {
                "project-cat": {
                    "name": "Cat",
                    "rootPaths": [r"C:\Users\zcb\Desktop\Cat"]
                }
            },
            "thread-project-assignments": {
                "thread-cat": {"projectKind": "local", "projectId": "project-cat"}
            },
            "projectless-thread-ids": ["thread-recent"]
        });

        let context = project_context_from_value(&value);
        let binding = context.thread_projects.get("thread-cat").unwrap();
        assert_eq!(binding.name, "Cat");
        assert_eq!(
            binding.root_path.as_deref(),
            Some(r"C:\Users\zcb\Desktop\Cat")
        );
        assert!(context.projectless_threads.contains("thread-recent"));
    }

    #[test]
    fn load_thread_titles_from_state_db_trims_and_filters_titles() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join(CODEX_STATE_DB_FILENAME);
        let conn = Connection::open(&db_path).expect("open sqlite db");
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT NOT NULL, first_user_message TEXT NOT NULL)",
            [],
        )
        .expect("create threads table");
        conn.execute(
            "INSERT INTO threads (id, title, first_user_message) VALUES (?1, ?2, ?3)",
            ("thread-1", "  Renamed Codex thread  ", "First prompt"),
        )
        .expect("insert renamed thread");
        conn.execute(
            "INSERT INTO threads (id, title, first_user_message) VALUES (?1, ?2, ?3)",
            ("thread-2", "   ", "First prompt"),
        )
        .expect("insert blank thread");
        conn.execute(
            "INSERT INTO threads (id, title, first_user_message) VALUES (?1, ?2, ?3)",
            ("thread-3", "  First prompt  ", "First prompt"),
        )
        .expect("insert first-message title");
        drop(conn);

        let titles = load_thread_titles_from_db(&db_path);

        assert_eq!(
            titles.get("thread-1").map(String::as_str),
            Some("Renamed Codex thread")
        );
        assert!(!titles.contains_key("thread-2"));
        assert!(!titles.contains_key("thread-3"));
    }

    #[test]
    fn load_thread_titles_from_state_db_keeps_title_when_first_user_message_null() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join(CODEX_STATE_DB_FILENAME);
        let conn = Connection::open(&db_path).expect("open sqlite db");
        // Codex stores first_user_message as a nullable column (Option<String>);
        // a renamed thread can have a title before any first message is synced.
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT NOT NULL, first_user_message TEXT)",
            [],
        )
        .expect("create threads table");
        conn.execute(
            "INSERT INTO threads (id, title, first_user_message) VALUES (?1, ?2, NULL)",
            ("thread-1", "Renamed thread"),
        )
        .expect("insert renamed thread without first message");
        conn.execute(
            "INSERT INTO threads (id, title, first_user_message) VALUES (?1, ?2, ?3)",
            ("thread-2", "First prompt", "First prompt"),
        )
        .expect("insert first-message title");
        drop(conn);

        let titles = load_thread_titles_from_db(&db_path);

        // Kept: title present and no first message to compare against.
        assert_eq!(
            titles.get("thread-1").map(String::as_str),
            Some("Renamed thread")
        );
        // Filtered: title equals the first user message.
        assert!(!titles.contains_key("thread-2"));
    }

    #[test]
    fn load_thread_titles_from_session_index_uses_latest_name() {
        let temp = tempdir().expect("tempdir");
        let index_path = temp.path().join(CODEX_SESSION_INDEX_FILENAME);
        std::fs::write(
            &index_path,
            concat!(
                "{\"id\":\"thread-1\",\"thread_name\":\"Old name\",\"updated_at\":\"2026-07-01T00:00:00Z\"}\n",
                "{\"id\":\"thread-2\",\"thread_name\":\"   \",\"updated_at\":\"2026-07-01T00:00:00Z\"}\n",
                "not json\n",
                "{\"id\":\"thread-1\",\"thread_name\":\"  New name  \",\"updated_at\":\"2026-07-02T00:00:00Z\"}\n"
            ),
        )
        .expect("write session index");

        let titles = load_thread_titles_from_session_index(&index_path);

        assert_eq!(titles.get("thread-1").map(String::as_str), Some("New name"));
        assert!(!titles.contains_key("thread-2"));
    }

    #[test]
    fn load_thread_titles_prefers_state_db_explicit_title_over_session_index() {
        let temp = tempdir().expect("tempdir");
        let index_path = temp.path().join(CODEX_SESSION_INDEX_FILENAME);
        std::fs::write(
            &index_path,
            concat!(
                "{\"id\":\"thread-1\",\"thread_name\":\"Legacy name\",\"updated_at\":\"2026-07-01T00:00:00Z\"}\n",
                "{\"id\":\"thread-2\",\"thread_name\":\"Legacy fallback\",\"updated_at\":\"2026-07-01T00:00:00Z\"}\n"
            ),
        )
        .expect("write session index");

        let db_path = temp.path().join(CODEX_STATE_DB_FILENAME);
        let conn = Connection::open(&db_path).expect("open sqlite db");
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT NOT NULL, first_user_message TEXT NOT NULL)",
            [],
        )
        .expect("create threads table");
        conn.execute(
            "INSERT INTO threads (id, title, first_user_message) VALUES (?1, ?2, ?3)",
            ("thread-1", "SQLite name", "First prompt"),
        )
        .expect("insert sqlite title");
        conn.execute(
            "INSERT INTO threads (id, title, first_user_message) VALUES (?1, ?2, ?3)",
            ("thread-2", "First prompt", "First prompt"),
        )
        .expect("insert first-message sqlite title");
        drop(conn);

        let titles = load_thread_titles_from_paths(&index_path, &[db_path]);

        assert_eq!(
            titles.get("thread-1").map(String::as_str),
            Some("SQLite name")
        );
        assert_eq!(
            titles.get("thread-2").map(String::as_str),
            Some("Legacy fallback")
        );
    }

    #[test]
    fn parse_session_clears_resume_command_for_unsafe_session_id() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"x; calc\",\"cwd\":\"/tmp/project\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"hello\"}}\n"
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        // 带元字符的 ID 可以拼进 shell 命令，必须不能给出 resume 命令
        assert_eq!(meta.session_id, "x; calc");
        assert!(meta.resume_command.is_none());
    }

    #[test]
    fn parse_session_skips_agents_md_injection() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"test-id\",\"cwd\":\"/tmp/project\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"developer\",\"content\":\"<permissions>\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"# AGENTS.md instructions for /tmp/project\\n<INSTRUCTIONS>Do stuff</INSTRUCTIONS>\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:14Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"Fix the login bug\"}}\n"
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        // Should skip AGENTS.md injection and use the real user message
        assert_eq!(meta.title.as_deref(), Some("Fix the login bug"));
    }

    #[test]
    fn parse_session_skips_subagent_sessions() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-04-28T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"subagent-id\",\"cwd\":\"/tmp/project\",\"originator\":\"codex-tui\",\"source\":{\"subagent\":{\"thread_spawn\":{\"parent_thread_id\":\"parent-id\",\"depth\":1,\"agent_role\":\"explorer\"}}}}}\n",
                "{\"timestamp\":\"2026-04-28T10:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"Inspect the project\"}}\n"
            ),
        )
        .expect("write");

        assert!(parse_session(&path).is_none());
    }

    #[test]
    fn parse_session_skips_environment_context_injection() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"test-id\",\"cwd\":\"/tmp/project\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"<environment_context>\\n  <cwd>/tmp/project</cwd>\\n</environment_context>\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:14Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"Fix the login bug\"}}\n"
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        // Should skip environment_context injection and use the real user message
        assert_eq!(meta.title.as_deref(), Some("Fix the login bug"));
    }

    #[test]
    fn parse_session_extracts_vscode_ide_request_as_title() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"test-id\",\"cwd\":\"/tmp/project\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"# Context from my IDE setup:\\n\\n## Active file: src/main.ts\\n\\n## My request for Codex:\\nFix the session title preview\"}}\n"
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Fix the session title preview"));
    }

    #[test]
    fn parse_session_extracts_inline_vscode_ide_request_as_title() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"test-id\",\"cwd\":\"/tmp/project\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"# Context from my IDE setup:\\n\\n## My request for Codex: Fix the TOC preview\"}}\n"
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Fix the TOC preview"));
    }

    #[test]
    fn parse_session_ignores_marker_mentions_before_request_heading() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"test-id\",\"cwd\":\"/tmp/project\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"# Context from my IDE setup:\\n\\n## Active selection:\\nMy request for Codex: not the prompt\\n\\n## My request for Codex:\\nUse the real request heading\"}}\n"
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Use the real request heading"));
    }

    #[test]
    fn parse_session_uses_last_request_heading_when_selection_has_one() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"test-id\",\"cwd\":\"/tmp/project\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"# Context from my IDE setup:\\n\\n## Active selection: docs/codex-format.md\\n## My request for Codex:\\nselected document content, not the real request\\n\\n## My request for Codex:\\nUse the last request heading\"}}\n"
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Use the last request heading"));
    }

    // Known limitation: the IDE marker is matched purely by text, so a
    // "## My request for Codex:" line inside the real request body is treated as
    // a new boundary and only the trailing part is kept. This pins the
    // best-effort behavior; fully fixing it needs structured IDE section data
    // that the Codex VS Code context does not provide.
    #[test]
    fn parse_session_keeps_trailing_part_when_request_body_repeats_heading() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"test-id\",\"cwd\":\"/tmp/project\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"# Context from my IDE setup:\\n\\n## Active file: foo.ts\\n\\n## My request for Codex:\\nDocument the format, for example:\\n## My request for Codex:\\nand the rest follows.\"}}\n"
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        assert_eq!(meta.title.as_deref(), Some("and the rest follows."));
    }

    #[test]
    fn parse_session_skips_vscode_ide_context_without_request() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"test-id\",\"cwd\":\"/tmp/project\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"# Context from my IDE setup:\\n\\n## Active file: src/main.ts\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:14Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"Fix the login bug\"}}\n"
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Fix the login bug"));
    }

    #[test]
    fn parse_session_falls_back_to_dir_basename() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"test-id\",\"cwd\":\"/tmp/my-project\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"Hello\"}}\n"
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        // No user message → falls back to dir basename
        assert_eq!(meta.title.as_deref(), Some("my-project"));
    }

    #[test]
    fn parse_session_keeps_complete_latest_user_message() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        let long_msg = "a".repeat(200);
        std::fs::write(
            &path,
            format!(
                "{{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"test-id\",\"cwd\":\"/tmp/p\"}}}}\n\
                 {{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":\"{long_msg}\"}}}}\n",
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        let title = meta.title.unwrap();
        assert_eq!(title, long_msg);
    }

    #[test]
    fn load_messages_includes_function_call_and_output() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"test-id\",\"cwd\":\"/tmp\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"list files\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:14Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"shell\",\"arguments\":\"{\\\"cmd\\\":[\\\"ls\\\"]}\",\"call_id\":\"call_1\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:15Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"call_1\",\"output\":\"file1.txt\\nfile2.txt\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:16Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Done.\"}]}}\n",
            ),
        )
        .expect("write");

        let msgs = load_messages(&path).expect("load");
        assert_eq!(msgs.len(), 4);

        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content, "list files");

        assert_eq!(msgs[1].role, "assistant");
        assert!(msgs[1].content.contains("[Tool: shell]"));

        assert_eq!(msgs[2].role, "tool");
        assert!(msgs[2].content.contains("file1.txt"));

        assert_eq!(msgs[3].role, "assistant");
        assert_eq!(msgs[3].content, "Done.");
    }

    #[test]
    fn load_messages_skips_codex_internal_context_messages() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"test-id\",\"cwd\":\"/tmp\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"<recommended_plugins>internal</recommended_plugins>\",\"internal_chat_message_metadata_passthrough\":{\"content_item_kinds\":[\"plugins.recommendations\",\"environments.environment_context\"]}}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:14Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"<environment_context>legacy internal</environment_context>\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:15Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"developer\",\"content\":\"internal instructions\",\"internal_chat_message_metadata_passthrough\":{\"content_item_kinds\":[\"generic.developer_instructions\"]}}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:16Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"真实问题\",\"internal_chat_message_metadata_passthrough\":{\"content_item_kinds\":[\"user.text\"]}}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:17Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"真实回答\"}}\n",
            ),
        )
        .expect("write");

        let messages = load_messages(&path).expect("load");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "真实问题");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].content, "真实回答");

        let meta = parse_session(&path).expect("parse");
        assert_eq!(meta.title.as_deref(), Some("真实问题"));
        assert_eq!(meta.summary.as_deref(), Some("真实回答"));
    }
}
