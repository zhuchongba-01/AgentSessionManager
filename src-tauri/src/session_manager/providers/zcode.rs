use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::session_manager::cache;
use crate::session_manager::{DeleteSessionRequest, SessionMessage, SessionMeta};

use super::utils::{
    extract_thinking_text, is_safe_session_id, order_selected_children, path_basename,
    quote_sqlite_identifier, sqlite_table_has_column, sqlite_table_names, truncate_summary,
    validate_parent_graph, TITLE_MAX_CHARS,
};

const PROVIDER_ID: &str = "zcode";
const SOURCE_PREFIX: &str = "sqlite-zcode:";

pub(crate) fn database_path() -> PathBuf {
    crate::session_paths::zcode_cli_dir()
        .join("db")
        .join("db.sqlite")
}

fn tasks_index_path() -> PathBuf {
    crate::session_paths::zcode_home_dir()
        .join("v2")
        .join("tasks-index.sqlite")
}

fn source_reference(path: &std::path::Path, session_id: &str) -> String {
    format!("{SOURCE_PREFIX}{}:{session_id}", path.display())
}

fn parse_source(source: &str) -> Option<(PathBuf, String)> {
    let value = source.strip_prefix(SOURCE_PREFIX)?;
    let (path, session_id) = value.rsplit_once(':')?;
    if path.is_empty() || session_id.is_empty() {
        return None;
    }
    Some((PathBuf::from(path), session_id.to_string()))
}

pub fn scan_sessions() -> Vec<SessionMeta> {
    let path = database_path();
    if !path.is_file() {
        return Vec::new();
    }
    let connection = match Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(connection) => connection,
        Err(error) => {
            super::utils::scan_warning(error.to_string());
            return Vec::new();
        }
    };
    if let Err(error) = connection.busy_timeout(Duration::from_secs(2)) {
        super::utils::scan_warning(error.to_string());
        return Vec::new();
    }

    let has_title = sqlite_table_has_column(&connection, "session", "title").unwrap_or(false);
    let has_directory =
        sqlite_table_has_column(&connection, "session", "directory").unwrap_or(false);
    let has_created =
        sqlite_table_has_column(&connection, "session", "time_created").unwrap_or(false);
    let has_updated =
        sqlite_table_has_column(&connection, "session", "time_updated").unwrap_or(false);
    let has_time_deleted =
        sqlite_table_has_column(&connection, "session", "time_deleted").unwrap_or(false);
    let has_time_archived =
        sqlite_table_has_column(&connection, "session", "time_archived").unwrap_or(false);

    let updated_expr = match (has_updated, has_created) {
        (true, true) => "COALESCE(time_updated, time_created, 0)",
        (true, false) => "COALESCE(time_updated, 0)",
        (false, true) => "COALESCE(time_created, 0)",
        (false, false) => "0",
    };
    let order_by = if has_updated {
        "time_updated DESC"
    } else if has_created {
        "time_created DESC"
    } else {
        "rowid DESC"
    };
    let sql = format!(
        "SELECT id, {}, {}, {}, {}, {}, {} FROM session ORDER BY {order_by}",
        if has_title {
            "COALESCE(title, '')"
        } else {
            "''"
        },
        if has_directory {
            "COALESCE(directory, '')"
        } else {
            "''"
        },
        if has_created {
            "COALESCE(time_created, 0)"
        } else {
            "0"
        },
        updated_expr,
        if has_time_deleted {
            "time_deleted IS NOT NULL AND time_deleted > 0"
        } else {
            "0"
        },
        if has_time_archived {
            "time_archived IS NOT NULL AND time_archived > 0"
        } else {
            "0"
        },
    );

    let mut statement = match connection.prepare(&sql) {
        Ok(statement) => statement,
        Err(error) => {
            super::utils::scan_warning(error.to_string());
            return Vec::new();
        }
    };
    let rows = match statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, bool>(5)?,
            row.get::<_, bool>(6)?,
        ))
    }) {
        Ok(rows) => rows,
        Err(error) => {
            super::utils::scan_warning(error.to_string());
            return Vec::new();
        }
    };

    let index_path = tasks_index_path();
    let task_index_state = load_task_index_state(&index_path);
    if index_path.exists() && task_index_state.is_none() {
        super::utils::scan_warning("任务侧边栏索引无法读取，残留分类暂不可用");
    }
    let parent_relations = match load_parent_relations(&path, &index_path) {
        Ok(relations) => Some(relations),
        Err(error) => {
            super::utils::scan_warning(format!("父子会话关系无法读取：{error}"));
            None
        }
    };

    rows.filter_map(|row| match row {
        Ok(row) => Some(row),
        Err(error) => {
            super::utils::scan_warning(error.to_string());
            None
        }
    })
    .map(
        |(session_id, title, directory, created, updated, session_deleted, session_archived)| {
            let raw_title = if title.trim().is_empty() {
                path_basename(&directory)
            } else {
                Some(title)
            };
            let display_title = raw_title
                .as_deref()
                .map(|t| truncate_summary(t, TITLE_MAX_CHARS))
                .filter(|t| !t.is_empty());
            let summary = raw_title
                .as_deref()
                .map(|t| truncate_summary(t, 160))
                .filter(|t| !t.is_empty());
            let residual = session_deleted
                || task_index_state
                    .as_ref()
                    .is_some_and(|state| !state.active_task_ids.contains(&session_id));
            let archived = session_archived
                || task_index_state
                    .as_ref()
                    .is_some_and(|state| state.archived_task_ids.contains(&session_id));
            let parent_session_id = parent_relations
                .as_ref()
                .and_then(|relations| relations.get(&session_id))
                .cloned();
            SessionMeta {
                provider_id: PROVIDER_ID.to_string(),
                session_id: session_id.clone(),
                residual,
                parent_session_id,
                archived,
                cleanup_pending: false,
                sidebar_section: None,
                title: display_title,
                summary,
                project_dir: (!directory.is_empty()).then_some(directory),
                project_name: None,
                created_at: (created > 0).then_some(created),
                last_active_at: (updated > 0).then_some(updated),
                source_path: Some(source_reference(&path, &session_id)),
                resume_command: (!residual && is_safe_session_id(&session_id))
                    .then(|| format!("zcode -s {session_id}")),
            }
        },
    )
    .collect()
}

#[derive(Debug, Default)]
struct TaskIndexState {
    active_task_ids: BTreeSet<String>,
    archived_task_ids: BTreeSet<String>,
}

/// ZCode's desktop task list is backed by a separate index database. Session
/// rows that remain in the CLI database after their task index entry vanished
/// or was soft-deleted (`deleted != 0`) still contain recoverable conversation
/// data, but are no longer active in ZCode's native sidebar.
fn load_task_index_state(path: &Path) -> Option<TaskIndexState> {
    if !path.is_file() {
        return None;
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    connection.busy_timeout(Duration::from_secs(2)).ok()?;
    if !sqlite_table_has_column(&connection, "tasks", "task_id").ok()? {
        return None;
    }
    let has_deleted = sqlite_table_has_column(&connection, "tasks", "deleted").ok()?;
    let has_archived = sqlite_table_has_column(&connection, "tasks", "archived").ok()?;
    let sql = format!(
        "SELECT task_id, {}, {} FROM tasks",
        if has_deleted {
            "COALESCE(deleted, 0) != 0"
        } else {
            "0"
        },
        if has_archived {
            "COALESCE(archived, 0) != 0"
        } else {
            "0"
        },
    );
    let mut statement = connection.prepare(&sql).ok()?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, bool>(1)?,
                row.get::<_, bool>(2)?,
            ))
        })
        .ok()?;
    let mut state = TaskIndexState::default();
    for (task_id, is_deleted, is_archived) in rows.flatten() {
        if task_id.trim().is_empty() {
            continue;
        }
        if !is_deleted {
            state.active_task_ids.insert(task_id.clone());
        }
        if is_archived {
            state.archived_task_ids.insert(task_id);
        }
    }
    Some(state)
}

#[cfg(test)]
fn load_native_task_ids(path: &Path) -> Option<BTreeSet<String>> {
    load_task_index_state(path).map(|state| state.active_task_ids)
}

fn insert_parent_relation(
    parents: &mut HashMap<String, String>,
    child: String,
    parent: String,
) -> Result<(), String> {
    validate_session_id(&child)?;
    validate_session_id(&parent)?;
    if child == parent {
        return Err(format!("ZCode 会话 {child} 的父子关系指向自身，已停止删除"));
    }
    if let Some(existing) = parents.insert(child.clone(), parent.clone()) {
        if existing != parent {
            return Err(format!(
                "ZCode 会话 {child} 同时指向父会话 {existing} 和 {parent}，已停止删除"
            ));
        }
    }
    Ok(())
}

fn load_parent_relations(
    primary_path: &Path,
    index_path: &Path,
) -> Result<HashMap<String, String>, String> {
    let primary = Connection::open_with_flags(
        primary_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("Failed to open ZCode database for dependency check: {error}"))?;
    primary
        .busy_timeout(Duration::from_secs(2))
        .map_err(|error| format!("Failed to configure ZCode dependency check: {error}"))?;
    let primary_tables: BTreeSet<String> = sqlite_table_names(&primary)?.into_iter().collect();
    if !primary_tables.contains("session") || !sqlite_table_has_column(&primary, "session", "id")? {
        return Err("ZCode database is missing the session table".into());
    }

    let mut existing_sessions = HashSet::new();
    {
        let mut id_stmt = primary
            .prepare("SELECT id FROM session WHERE id IS NOT NULL AND id<>''")
            .map_err(|error| format!("Failed to inspect ZCode sessions: {error}"))?;
        let ids = id_stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| format!("Failed to read ZCode sessions: {error}"))?;
        for id in ids {
            existing_sessions
                .insert(id.map_err(|error| format!("Invalid ZCode session row: {error}"))?);
        }
    }

    let mut parents = HashMap::new();
    let mut session_parent_children = HashSet::new();
    if sqlite_table_has_column(&primary, "session", "parent_id")? {
        let mut statement = primary
            .prepare(
                "SELECT id, parent_id FROM session WHERE parent_id IS NOT NULL AND parent_id<>''",
            )
            .map_err(|error| format!("Failed to inspect ZCode session.parent_id: {error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| format!("Failed to read ZCode session.parent_id: {error}"))?;
        for row in rows {
            let (child, parent) =
                row.map_err(|error| format!("Invalid ZCode session.parent_id relation: {error}"))?;
            insert_parent_relation(&mut parents, child.clone(), parent)?;
            session_parent_children.insert(child);
        }
    }
    if primary_tables.contains("session_task_link") {
        if !sqlite_table_has_column(&primary, "session_task_link", "parent_session_id")?
            || !sqlite_table_has_column(&primary, "session_task_link", "child_session_id")?
        {
            return Err("ZCode session_task_link table has an unsupported shape".into());
        }
        let mut statement = primary
            .prepare(
                "SELECT child_session_id, parent_session_id FROM session_task_link \
                 WHERE child_session_id IS NOT NULL AND child_session_id<>'' \
                 AND parent_session_id IS NOT NULL AND parent_session_id<>''",
            )
            .map_err(|error| format!("Failed to inspect ZCode session_task_link: {error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| format!("Failed to read ZCode session_task_link: {error}"))?;
        for row in rows {
            let (child, parent) =
                row.map_err(|error| format!("Invalid ZCode session_task_link relation: {error}"))?;
            // Ignore orphaned link rows whose child session has already been deleted.
            if !existing_sessions.contains(&child) {
                continue;
            }
            insert_parent_relation(&mut parents, child, parent)?;
        }
    }

    if index_path.is_file() {
        let index = Connection::open_with_flags(
            index_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| {
            format!("Failed to open ZCode task index for dependency check: {error}")
        })?;
        index
            .busy_timeout(Duration::from_secs(2))
            .map_err(|error| format!("Failed to configure ZCode task index check: {error}"))?;
        let index_tables: BTreeSet<String> = sqlite_table_names(&index)?.into_iter().collect();
        if index_tables.contains("tasks") {
            if !sqlite_table_has_column(&index, "tasks", "task_id")? {
                return Err("ZCode tasks table is missing task_id".into());
            }
            if sqlite_table_has_column(&index, "tasks", "forked_from_task_id")? {
                let has_deleted = sqlite_table_has_column(&index, "tasks", "deleted")?;
                let sql = format!(
                    "SELECT task_id, forked_from_task_id FROM tasks \
                     WHERE forked_from_task_id IS NOT NULL AND forked_from_task_id<>''{}",
                    if has_deleted {
                        " AND COALESCE(deleted, 0) = 0"
                    } else {
                        ""
                    }
                );
                let mut statement = index.prepare(&sql).map_err(|error| {
                    format!("Failed to inspect ZCode tasks.forked_from_task_id: {error}")
                })?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(|error| {
                        format!("Failed to read ZCode tasks.forked_from_task_id: {error}")
                    })?;
                for row in rows {
                    let (child, parent) = row.map_err(|error| {
                        format!("Invalid ZCode tasks.forked_from_task_id relation: {error}")
                    })?;
                    if !existing_sessions.contains(&child)
                        || session_parent_children.contains(&child)
                    {
                        continue;
                    }
                    insert_parent_relation(&mut parents, child, parent)?;
                }
            }
        }
    }

    validate_parent_graph(&parents, "ZCode")?;
    Ok(parents)
}

fn validate_source_database(source: &str, session_id: &str) -> Result<PathBuf, String> {
    let (path, referenced_id) = parse_source(source)
        .ok_or_else(|| format!("Invalid ZCode SQLite source reference: {source}"))?;
    if referenced_id != session_id {
        return Err("ZCode session ID does not match the source reference".to_string());
    }
    let expected = database_path()
        .canonicalize()
        .map_err(|error| format!("Failed to resolve the ZCode database: {error}"))?;
    let actual = path
        .canonicalize()
        .map_err(|error| format!("Failed to resolve the selected ZCode database: {error}"))?;
    if actual != expected {
        return Err("ZCode database path is outside the configured storage root".to_string());
    }
    validate_session_id(session_id)?;
    Ok(actual)
}

fn ensure_no_active_automations(index_path: &Path, session_id: &str) -> Result<(), String> {
    if !index_path.is_file() {
        return Ok(());
    }
    let index = Connection::open_with_flags(
        index_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("Failed to open ZCode task index for automation check: {error}"))?;
    index
        .busy_timeout(Duration::from_secs(2))
        .map_err(|error| format!("Failed to configure ZCode automation check: {error}"))?;
    let tables = sqlite_table_names(&index)?;
    if !tables.iter().any(|t| t == "automations") {
        return Ok(());
    }
    let has_task_id = sqlite_table_has_column(&index, "automations", "task_id")?;
    let has_session_id = sqlite_table_has_column(&index, "automations", "session_id")?;
    let id_clause = match (has_task_id, has_session_id) {
        (true, true) => "(task_id = ?1 OR session_id = ?1)",
        (true, false) => "task_id = ?1",
        (false, true) => "session_id = ?1",
        (false, false) => return Ok(()),
    };
    let mut clauses = vec![id_clause.to_string()];
    if sqlite_table_has_column(&index, "automations", "deleted")? {
        clauses.push("COALESCE(deleted, 0) = 0".to_string());
    }
    if sqlite_table_has_column(&index, "automations", "archived")? {
        clauses.push("COALESCE(archived, 0) = 0".to_string());
    }
    if sqlite_table_has_column(&index, "automations", "enabled")? {
        clauses.push("COALESCE(enabled, 1) != 0".to_string());
    }
    if sqlite_table_has_column(&index, "automations", "active")? {
        clauses.push("COALESCE(active, 1) != 0".to_string());
    }
    if sqlite_table_has_column(&index, "automations", "status")? {
        clauses.push(
            "LOWER(TRIM(COALESCE(status, 'active'))) IN ('active', 'enabled', 'running', 'scheduled', 'pending')"
                .to_string(),
        );
    }
    let sql = format!(
        "SELECT EXISTS(SELECT 1 FROM automations WHERE {})",
        clauses.join(" AND ")
    );
    let has_active: bool = index
        .query_row(&sql, [session_id], |row| row.get(0))
        .map_err(|error| format!("Failed to inspect ZCode automations: {error}"))?;
    if has_active {
        return Err(format!(
            "该 ZCode 会话（{session_id}）仍有关联的启用中自动化任务，请先停用或删除该自动化"
        ));
    }
    Ok(())
}

fn preflight_delete_with_paths(
    primary_path: &Path,
    index_path: &Path,
    session_id: &str,
) -> Result<(), String> {
    validate_session_id(session_id)?;
    let parents = load_parent_relations(primary_path, index_path)?;
    if let Some(child) = parents
        .iter()
        .find_map(|(child, parent)| (parent == session_id).then_some(child))
    {
        return Err(format!(
            "该 ZCode 会话仍被子会话 {child} 依赖，请先删除子会话"
        ));
    }
    ensure_no_active_automations(index_path, session_id)?;
    Ok(())
}

pub fn preflight_delete(source: &str, session_id: &str) -> Result<(), String> {
    let primary_path = validate_source_database(source, session_id)?;
    preflight_delete_with_paths(&primary_path, &tasks_index_path(), session_id)
}

fn deletion_order_with_relations(
    requests: &[DeleteSessionRequest],
    initial: &[usize],
    parents: &HashMap<String, String>,
) -> Vec<usize> {
    let selected = requests
        .iter()
        .enumerate()
        .filter(|(_, request)| request.provider_id == PROVIDER_ID)
        .map(|(index, request)| {
            (
                index,
                request.session_id.clone(),
                parents.get(&request.session_id).cloned(),
            )
        })
        .collect::<Vec<_>>();
    order_selected_children(&selected, initial)
}

pub fn deletion_order(requests: &[DeleteSessionRequest], initial: &[usize]) -> Vec<usize> {
    if !requests
        .iter()
        .any(|request| request.provider_id == PROVIDER_ID)
    {
        return initial.to_vec();
    }
    let primary = database_path();
    let Ok(parents) = load_parent_relations(&primary, &tasks_index_path()) else {
        return initial.to_vec();
    };
    deletion_order_with_relations(requests, initial, &parents)
}

/// 正文实际依赖的文件：主库与 WAL 边车。引用无法解析或未通过路径安全校验时返回 `None`，由读取器报错。
///
/// ZCode 的会话正文全部在 `message` / `part` 表里（`load_messages` 只打开主库），
/// 桌面任务索引库不参与正文，因此不计入指纹。
pub fn transcript_sources(source: &str) -> Option<cache::TranscriptSources> {
    let (_, session_id) = parse_source(source)?;
    let db = validate_source_database(source, &session_id).ok()?;
    Some(cache::sqlite_transcript_sources(&db))
}

pub fn load_messages(source: &str) -> Result<Vec<SessionMessage>, String> {
    let (_, session_id) = parse_source(source)
        .ok_or_else(|| format!("Invalid ZCode SQLite source reference: {source}"))?;
    let db = validate_source_database(source, &session_id)?;
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("Failed to open ZCode database: {error}"))?;

    read_messages_from_connection(&connection, &session_id)
}

#[derive(Default)]
struct ZCodeMessageParts {
    thinking: Vec<String>,
    visible: Vec<String>,
}

fn format_zcode_error(err: &Value) -> Option<String> {
    match err {
        Value::Null => None,
        Value::String(s) => {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Value::Object(map) => {
            let name = map
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let message = map
                .get("data")
                .and_then(|d| d.get("message"))
                .or_else(|| map.get("message"))
                .or_else(|| map.get("error"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            match (name, message) {
                (Some(n), Some(m)) => Some(format!("{n}: {m}")),
                (None, Some(m)) => Some(m.to_string()),
                (Some(n), None) => Some(n.to_string()),
                (None, None) => (!map.is_empty()).then(|| Value::Object(map.clone()).to_string()),
            }
        }
        other => {
            let rendered = other.to_string();
            (!rendered.trim().is_empty()).then_some(rendered)
        }
    }
}

fn format_zcode_tool_part(value: &Value) -> String {
    let name = value
        .get("tool")
        .or_else(|| value.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown");
    let mut text = format!("[Tool: {name}]");
    let state = value.get("state");

    let mut has_structured_content = false;
    if let Some(items) = state
        .and_then(|s| s.get("content"))
        .and_then(Value::as_array)
    {
        for item in items {
            let out = match item.get("type").and_then(Value::as_str) {
                Some("text") => item
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                Some("file") => format!(
                    "[File: {}]",
                    item.get("filename")
                        .or_else(|| item.get("name"))
                        .or_else(|| item.get("uri"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty() && !s.starts_with("data:"))
                        .unwrap_or("attachment")
                ),
                _ => String::new(),
            };
            if !out.trim().is_empty() {
                has_structured_content = true;
                text.push('\n');
                text.push_str(&out);
            }
        }
    }

    if !has_structured_content {
        if let Some(output) = state
            .and_then(|s| s.get("output"))
            .or_else(|| value.get("output"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            text.push('\n');
            text.push_str(output);
        }
    }

    if let Some(err) = state
        .and_then(|s| s.get("error"))
        .or_else(|| value.get("error"))
        .and_then(format_zcode_error)
    {
        text.push_str(&format!("\n[Error: {err}]"));
    }

    text
}

fn read_messages_from_connection(
    connection: &Connection,
    session_id: &str,
) -> Result<Vec<SessionMessage>, String> {
    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(|e| e.to_string())?;
    connection
        .execute_batch("BEGIN DEFERRED")
        .map_err(|e| e.to_string())?;
    super::utils::check_sqlite_message_budget(connection, session_id)?;

    let mut message_statement = connection
        .prepare(
            "SELECT id, data, time_created FROM message WHERE session_id=?1 ORDER BY time_created ASC, id ASC",
        )
        .map_err(|error| format!("Failed to prepare ZCode message query: {error}"))?;
    let messages = message_statement
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|error| format!("Failed to query ZCode messages: {error}"))?;

    let part_order = if sqlite_table_has_column(connection, "part", "id")? {
        "time_created ASC, id ASC"
    } else {
        "time_created ASC, rowid ASC"
    };
    let mut part_statement = connection
        .prepare(&format!(
            "SELECT message_id, data FROM part WHERE session_id=?1 ORDER BY {part_order}"
        ))
        .map_err(|error| format!("Failed to prepare ZCode part query: {error}"))?;
    let parts = part_statement
        .query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| format!("Failed to query ZCode parts: {error}"))?;

    let mut message_parts: HashMap<String, ZCodeMessageParts> = HashMap::new();
    for row in parts {
        let (message_id, raw) = row.map_err(|e| e.to_string())?;
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = value.get("text").and_then(Value::as_str) {
                    if !text.trim().is_empty() {
                        message_parts
                            .entry(message_id)
                            .or_default()
                            .visible
                            .push(text.to_string());
                    }
                }
            }
            Some("reasoning" | "thinking") => {
                let extracted = extract_thinking_text(&value);
                let thinking = if !extracted.trim().is_empty() {
                    Some(extracted)
                } else {
                    value
                        .get("text")
                        .or_else(|| value.get("thinking"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                        .map(str::to_string)
                };
                if let Some(thinking) = thinking {
                    message_parts
                        .entry(message_id)
                        .or_default()
                        .thinking
                        .push(thinking);
                }
            }
            Some("tool") => {
                message_parts
                    .entry(message_id)
                    .or_default()
                    .visible
                    .push(format_zcode_tool_part(&value));
            }
            Some("file") => {
                let label = value
                    .get("filename")
                    .or_else(|| value.get("name"))
                    .or_else(|| value.get("path"))
                    .or_else(|| value.get("uri"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && !s.starts_with("data:"))
                    .unwrap_or("attachment");
                message_parts
                    .entry(message_id)
                    .or_default()
                    .visible
                    .push(format!("[File: {label}]"));
            }
            _ => {}
        }
    }

    let mut output = Vec::new();
    for row in messages {
        let (message_id, raw, timestamp) = row.map_err(|e| e.to_string())?;
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let role = value
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let ts = (timestamp > 0).then_some(timestamp);
        let parts = message_parts.remove(&message_id).unwrap_or_default();
        let thinking = parts.thinking.join("\n");
        if !thinking.trim().is_empty() {
            output.push(SessionMessage {
                role: "thinking".to_string(),
                content: thinking,
                ts,
            });
        }
        let content = parts.visible.join("\n");
        if !content.trim().is_empty() {
            output.push(SessionMessage { role, content, ts });
        }
    }

    // Distinct persisted message IDs are distinct messages. Identical text or
    // an empty/error assistant row does not prove an automatic retry; no stable
    // retry identity is available in this schema, so preserve the original rows.
    Ok(output)
}

pub fn delete_session(
    session_id: &str,
    source: &str,
    project_was_deleted: bool,
) -> Result<bool, String> {
    if project_was_deleted {
        return Err("项目级会话级联删除已禁用；请逐项选择会话".into());
    }
    let actual = validate_source_database(source, session_id)?;
    let index_path = tasks_index_path();
    preflight_delete_with_paths(&actual, &index_path, session_id)?;

    let configured_database = database_path();
    let cli_root = configured_database
        .parent()
        .and_then(Path::parent)
        .ok_or("Invalid ZCode CLI directory")?;
    session_sidecar_targets(cli_root, session_id)?;

    // The task index is ZCode's native sidebar. Clean it first so a later
    // primary-database failure leaves a recoverable residual session instead
    // of an unopenable native ghost entry.
    cleanup_task_index(&index_path, session_id)?;
    let deleted = delete_sessions_from_database(&actual, session_id)?;

    cleanup_session_sidecars(cli_root, session_id)?;
    Ok(deleted)
}

fn delete_sessions_from_database(path: &Path, selected_session_id: &str) -> Result<bool, String> {
    validate_session_id(selected_session_id)?;
    let mut connection = Connection::open(path)
        .map_err(|error| format!("Failed to open ZCode database: {error}"))?;
    connection
        .busy_timeout(Duration::from_secs(3))
        .map_err(|error| format!("Failed to configure ZCode database timeout: {error}"))?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(|error| format!("Failed to enable ZCode foreign keys: {error}"))?;

    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| format!("Failed to start ZCode deletion: {error}"))?;
    let tables = sqlite_table_names(&transaction)?;

    if tables.iter().any(|table| table == "session_task_link") {
        let has_child =
            sqlite_table_has_column(&transaction, "session_task_link", "child_session_id")?;
        let has_parent =
            sqlite_table_has_column(&transaction, "session_task_link", "parent_session_id")?;
        match (has_child, has_parent) {
            (true, true) => {
                transaction
                    .execute(
                        "DELETE FROM session_task_link WHERE child_session_id=?1 OR parent_session_id=?1",
                        [selected_session_id],
                    )
                    .map_err(|error| {
                        format!("Failed to delete ZCode session_task_link rows: {error}")
                    })?;
            }
            (true, false) => {
                transaction
                    .execute(
                        "DELETE FROM session_task_link WHERE child_session_id=?1",
                        [selected_session_id],
                    )
                    .map_err(|error| {
                        format!("Failed to delete ZCode session_task_link rows: {error}")
                    })?;
            }
            (false, true) => {
                transaction
                    .execute(
                        "DELETE FROM session_task_link WHERE parent_session_id=?1",
                        [selected_session_id],
                    )
                    .map_err(|error| {
                        format!("Failed to delete ZCode session_task_link rows: {error}")
                    })?;
            }
            (false, false) => {}
        }
    }

    let mut child_tables = Vec::new();
    for table in &tables {
        if matches!(table.as_str(), "session" | "session_task_link")
            || !sqlite_table_has_column(&transaction, table, "session_id")?
        {
            continue;
        }
        child_tables.push(table.clone());
    }
    // Delete `part` and other session-scoped child tables before `message` to avoid
    // foreign-key constraint violations when `part.message_id` lacks ON DELETE CASCADE.
    child_tables.sort_by_key(|table| (table == "message") as u8);
    for table in child_tables {
        let quoted = quote_sqlite_identifier(&table);
        transaction
            .execute(
                &format!("DELETE FROM {quoted} WHERE session_id=?1"),
                [selected_session_id],
            )
            .map_err(|error| format!("Failed to delete ZCode {table} rows: {error}"))?;
    }
    let count = transaction
        .execute("DELETE FROM session WHERE id=?1", [selected_session_id])
        .map_err(|error| format!("Failed to delete ZCode session: {error}"))?;
    transaction
        .commit()
        .map_err(|error| format!("Failed to commit ZCode deletion: {error}"))?;
    Ok(count > 0)
}

fn cleanup_task_index(path: &Path, session_id: &str) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    validate_session_id(session_id)?;
    let mut connection = Connection::open(path)
        .map_err(|error| format!("Failed to open ZCode task index: {error}"))?;
    connection
        .busy_timeout(Duration::from_secs(3))
        .map_err(|error| format!("Failed to configure ZCode task index timeout: {error}"))?;

    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| format!("Failed to start ZCode task cleanup: {error}"))?;
    let tables = sqlite_table_names(&transaction)?;
    if tables.iter().any(|table| table == "task_group_members") {
        transaction
            .execute(
                "DELETE FROM task_group_members WHERE task_id=?1",
                [session_id],
            )
            .map_err(|error| format!("Failed to delete ZCode task group member: {error}"))?;
    }
    // Keep pace with new ZCode index tables without depending on a fixed
    // schema version. Any auxiliary table keyed by task_id is session-scoped
    // and must be cleaned before the task catalog row itself.
    for table in &tables {
        if matches!(table.as_str(), "tasks" | "task_group_members") {
            continue;
        }
        if sqlite_table_has_column(&transaction, table, "task_id")? {
            let quoted = quote_sqlite_identifier(table);
            transaction
                .execute(
                    &format!("DELETE FROM {quoted} WHERE task_id=?1"),
                    [session_id],
                )
                .map_err(|error| format!("Failed to delete ZCode {table} row: {error}"))?;
        } else if table == "automations"
            && sqlite_table_has_column(&transaction, table, "session_id")?
        {
            transaction
                .execute("DELETE FROM automations WHERE session_id=?1", [session_id])
                .map_err(|error| format!("Failed to delete ZCode automations row: {error}"))?;
        }
    }
    if tables
        .iter()
        .any(|table| table == "task_group_view_node_orders")
    {
        let mut statement = transaction
            .prepare(
                "SELECT node_type, node_key FROM task_group_view_node_orders WHERE node_type='task'",
            )
            .map_err(|error| format!("Failed to query ZCode task ordering: {error}"))?;
        let order_keys = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| format!("Failed to read ZCode task ordering: {error}"))?
            .flatten()
            .filter(|(_, node_key)| task_order_session_id(node_key).as_deref() == Some(session_id))
            .collect::<Vec<_>>();
        drop(statement);
        for (node_type, node_key) in order_keys {
            transaction
                .execute(
                    "DELETE FROM task_group_view_node_orders WHERE node_type=?1 AND node_key=?2",
                    rusqlite::params![node_type, node_key],
                )
                .map_err(|error| format!("Failed to delete ZCode task ordering: {error}"))?;
        }
    }
    if tables.iter().any(|table| table == "tasks") {
        if sqlite_table_has_column(&transaction, "tasks", "forked_from_task_id")?
            && sqlite_table_has_column(&transaction, "tasks", "deleted")?
        {
            transaction
                .execute(
                    "UPDATE tasks SET forked_from_task_id = NULL \
                     WHERE forked_from_task_id = ?1 AND COALESCE(deleted, 0) != 0",
                    [session_id],
                )
                .map_err(|error| {
                    format!("Failed to clear dangling ZCode fork references: {error}")
                })?;
        }
        transaction
            .execute("DELETE FROM tasks WHERE task_id=?1", [session_id])
            .map_err(|error| format!("Failed to delete ZCode task index row: {error}"))?;
    }
    transaction
        .commit()
        .map_err(|error| format!("Failed to commit ZCode task cleanup: {error}"))?;
    Ok(())
}

fn task_order_session_id(node_key: &str) -> Option<String> {
    let values: Vec<String> = serde_json::from_str(node_key).ok()?;
    values.get(1).cloned()
}

fn cleanup_session_sidecars(cli_root: &Path, session_id: &str) -> Result<(), String> {
    for target in session_sidecar_targets(cli_root, session_id)? {
        super::utils::remove_path_if_exists(&target).map_err(|error| {
            format!(
                "Failed to delete ZCode session sidecar {}: {error}",
                target.display()
            )
        })?;
    }
    Ok(())
}

fn session_sidecar_targets(cli_root: &Path, session_id: &str) -> Result<Vec<PathBuf>, String> {
    validate_session_id(session_id)?;
    let targets = [
        cli_root
            .join("rollout")
            .join(format!("model-io-{session_id}.jsonl")),
        cli_root.join("artifacts").join(session_id),
        cli_root.join("exec").join(session_id),
        cli_root.join("agents").join(session_id),
        cli_root.join("exec").join("bash-startup").join(session_id),
    ];
    targets
        .iter()
        .map(|target| super::utils::checked_storage_child(cli_root, target))
        .collect()
}

fn validate_session_id(session_id: &str) -> Result<(), String> {
    if !is_safe_session_id(session_id) {
        return Err("Invalid ZCode session ID".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;

    fn zcode_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn parses_windows_source_reference_from_the_last_separator() {
        let (path, session_id) = parse_source("sqlite-zcode:C:\\Users\\me\\db.sqlite:session-1")
            .expect("valid reference");
        assert_eq!(path, PathBuf::from("C:\\Users\\me\\db.sqlite"));
        assert_eq!(session_id, "session-1");
    }

    #[test]
    #[allow(deprecated)]
    fn transcript_sources_cover_the_database_and_its_wal() {
        let _guard = zcode_env_lock().lock().expect("env lock");
        let temp = tempdir().expect("tempdir");
        let cli_dir = temp.path().join("custom-cli");
        let db_dir = cli_dir.join("db");
        std::fs::create_dir_all(&db_dir).expect("create db dir");
        let db_path = db_dir.join("db.sqlite");
        Connection::open(&db_path).expect("create sqlite db");

        let prev_storage = std::env::var_os("ZCODE_STORAGE_DIR");
        std::env::set_var("ZCODE_STORAGE_DIR", &cli_dir);

        let valid_ref = source_reference(&db_path, "session-1");
        let sources = transcript_sources(&valid_ref).expect("valid configured reference");
        let canonical_db = db_path.canonicalize().expect("canonical db");
        let expected_wal = PathBuf::from(format!("{}-wal", canonical_db.display()));
        assert_eq!(
            sources,
            vec![(canonical_db, None), (expected_wal, None)],
            "正文全在主库的 message/part 表里；索引库不参与，WAL 必须计入"
        );

        let foreign_db = temp.path().join("foreign.sqlite");
        Connection::open(&foreign_db).expect("create foreign db");
        assert!(
            transcript_sources(&source_reference(&foreign_db, "session-1")).is_none(),
            "未配置路径必须在指纹探针前被拒绝"
        );
        assert!(transcript_sources("sqlite-zcode:/data/db.sqlite").is_none());

        if let Some(val) = prev_storage {
            std::env::set_var("ZCODE_STORAGE_DIR", val);
        } else {
            std::env::remove_var("ZCODE_STORAGE_DIR");
        }
    }

    fn create_primary_database(path: &Path) -> Connection {
        let connection = Connection::open(path).expect("open primary database");
        connection
            .execute_batch(
                "PRAGMA foreign_keys=ON;
                 CREATE TABLE session (
                    id TEXT PRIMARY KEY,
                    directory TEXT NOT NULL,
                    parent_id TEXT
                 );
                 CREATE TABLE message (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE
                 );
                 CREATE TABLE part (
                    id TEXT PRIMARY KEY,
                    message_id TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE,
                    session_id TEXT NOT NULL
                 );
                 CREATE TABLE input_history (
                    id TEXT PRIMARY KEY,
                    session_id TEXT,
                    text TEXT NOT NULL
                 );
                 CREATE TABLE session_task_link (
                    id TEXT PRIMARY KEY,
                    parent_session_id TEXT REFERENCES session(id) ON DELETE SET NULL,
                    child_session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE
                 );",
            )
            .expect("create primary schema");
        connection
    }

    fn insert_primary_session(connection: &Connection, id: &str, directory: &str) {
        connection
            .execute(
                "INSERT INTO session (id, directory) VALUES (?1, ?2)",
                rusqlite::params![id, directory],
            )
            .expect("insert session");
        connection
            .execute(
                "INSERT INTO message (id, session_id) VALUES (?1, ?2)",
                rusqlite::params![format!("message-{id}"), id],
            )
            .expect("insert message");
        connection
            .execute(
                "INSERT INTO part (id, message_id, session_id) VALUES (?1, ?2, ?3)",
                rusqlite::params![format!("part-{id}"), format!("message-{id}"), id],
            )
            .expect("insert part");
        connection
            .execute(
                "INSERT INTO input_history (id, session_id, text) VALUES (?1, ?2, 'input')",
                rusqlite::params![format!("input-{id}"), id],
            )
            .expect("insert input history");
    }

    fn create_task_index(path: &Path) -> Connection {
        let connection = Connection::open(path).expect("open task index");
        connection
            .execute_batch(
                "CREATE TABLE tasks (
                    workspace_key TEXT NOT NULL,
                    workspace_path TEXT NOT NULL,
                    task_id TEXT NOT NULL,
                    forked_from_task_id TEXT,
                    PRIMARY KEY (workspace_key, task_id)
                 );
                 CREATE TABLE task_group_members (
                    group_id TEXT NOT NULL,
                    workspace_key TEXT NOT NULL,
                    task_id TEXT NOT NULL,
                    PRIMARY KEY (workspace_key, task_id)
                 );
                 CREATE TABLE task_group_view_node_orders (
                    node_type TEXT NOT NULL,
                    node_key TEXT NOT NULL,
                    sort_order INTEGER NOT NULL,
                    PRIMARY KEY (node_type, node_key)
                 );
                 CREATE TABLE task_group_workspace_bootstraps (
                    workspace_key TEXT PRIMARY KEY,
                    group_id TEXT
                 );
                 CREATE TABLE future_task_cache (
                    task_id TEXT PRIMARY KEY,
                    payload TEXT NOT NULL
                 );",
            )
            .expect("create task schema");
        connection
    }

    fn insert_index_task(connection: &Connection, workspace: &str, id: &str, order: i64) {
        connection
            .execute(
                "INSERT INTO tasks (workspace_key, workspace_path, task_id) VALUES (?1, ?1, ?2)",
                rusqlite::params![workspace, id],
            )
            .expect("insert task");
        connection
            .execute(
                "INSERT INTO task_group_members (group_id, workspace_key, task_id) VALUES ('group', ?1, ?2)",
                rusqlite::params![workspace, id],
            )
            .expect("insert task group member");
        let node_key = serde_json::to_string(&[workspace, id]).expect("node key");
        connection
            .execute(
                "INSERT INTO task_group_view_node_orders (node_type, node_key, sort_order) VALUES ('task', ?1, ?2)",
                rusqlite::params![node_key, order],
            )
            .expect("insert task order");
        connection
            .execute(
                "INSERT INTO future_task_cache (task_id, payload) VALUES (?1, '{}')",
                [id],
            )
            .expect("insert future task cache");
    }

    fn count_by_id(connection: &Connection, table: &str, column: &str, id: &str) -> i64 {
        connection
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {} WHERE {}=?1",
                    quote_sqlite_identifier(table),
                    quote_sqlite_identifier(column)
                ),
                [id],
                |row| row.get(0),
            )
            .expect("count rows")
    }

    #[test]
    fn parent_session_is_protected_by_all_zcode_relation_sources() {
        let temp = tempdir().expect("tempdir");
        let primary_path = temp.path().join("db.sqlite");
        let primary = create_primary_database(&primary_path);
        insert_primary_session(&primary, "sess_parent", r"E:\project");
        insert_primary_session(&primary, "sess_child", r"E:\project");
        primary
            .execute(
                "UPDATE session SET parent_id='sess_parent' WHERE id='sess_child'",
                [],
            )
            .expect("set parent relation");
        primary
            .execute(
                "INSERT INTO session_task_link (id, parent_session_id, child_session_id) \
                 VALUES ('link', 'sess_parent', 'sess_child')",
                [],
            )
            .expect("insert task link");
        drop(primary);

        let index_path = temp.path().join("tasks-index.sqlite");
        let index = create_task_index(&index_path);
        insert_index_task(&index, r"E:\project", "sess_parent", 0);
        insert_index_task(&index, r"E:\project", "sess_child", 1);
        index
            .execute(
                "UPDATE tasks SET forked_from_task_id='sess_parent' WHERE task_id='sess_child'",
                [],
            )
            .expect("set fork relation");
        drop(index);

        let error = preflight_delete_with_paths(&primary_path, &index_path, "sess_parent")
            .expect_err("parent must be protected");
        assert!(error.contains("sess_child"));
        preflight_delete_with_paths(&primary_path, &index_path, "sess_child")
            .expect("leaf child can be deleted");
    }

    #[test]
    fn conflicting_or_cyclic_zcode_relations_fail_closed() {
        let temp = tempdir().expect("tempdir");
        let primary_path = temp.path().join("db.sqlite");
        let primary = create_primary_database(&primary_path);
        for id in ["sess_a", "sess_b", "sess_c"] {
            insert_primary_session(&primary, id, r"E:\project");
        }
        primary
            .execute(
                "UPDATE session SET parent_id='sess_a' WHERE id='sess_b'",
                [],
            )
            .expect("set parent relation");
        primary
            .execute(
                "INSERT INTO session_task_link (id, parent_session_id, child_session_id) \
                 VALUES ('link', 'sess_c', 'sess_b')",
                [],
            )
            .expect("insert conflicting task link");
        drop(primary);

        let error = load_parent_relations(&primary_path, &temp.path().join("missing.sqlite"))
            .expect_err("conflicting parents must fail");
        assert!(error.contains("同时指向父会话"));

        let primary = Connection::open(&primary_path).expect("reopen primary");
        primary
            .execute("DELETE FROM session_task_link", [])
            .expect("clear conflict");
        primary
            .execute(
                "UPDATE session SET parent_id='sess_b' WHERE id='sess_a'",
                [],
            )
            .expect("create cycle");
        drop(primary);
        let error = load_parent_relations(&primary_path, &temp.path().join("missing.sqlite"))
            .expect_err("cycles must fail");
        assert!(error.contains("存在循环"));
    }

    #[test]
    fn selected_zcode_children_are_ordered_before_parents() {
        let requests = vec![
            DeleteSessionRequest {
                provider_id: PROVIDER_ID.into(),
                session_id: "sess_parent".into(),
                source_path: "parent".into(),
                include_project: false,
            },
            DeleteSessionRequest {
                provider_id: PROVIDER_ID.into(),
                session_id: "sess_child".into(),
                source_path: "child".into(),
                include_project: false,
            },
            DeleteSessionRequest {
                provider_id: "pi".into(),
                session_id: "pi_session".into(),
                source_path: "pi".into(),
                include_project: false,
            },
        ];
        let parents = HashMap::from([("sess_child".into(), "sess_parent".into())]);
        assert_eq!(
            deletion_order_with_relations(&requests, &[0, 2, 1], &parents),
            vec![2, 1, 0]
        );
    }

    #[test]
    fn native_task_ids_are_read_from_zcode_display_index() {
        let temp = tempdir().expect("tempdir");
        let index_path = temp.path().join("tasks-index.sqlite");
        let index = create_task_index(&index_path);
        insert_index_task(&index, r"E:\project", "sess_visible", 0);
        drop(index);

        let ids = load_native_task_ids(&index_path).expect("load task ids");

        assert_eq!(ids, BTreeSet::from(["sess_visible".to_string()]));
    }

    #[test]
    fn session_only_cleanup_removes_selected_task_and_keeps_workspace_sibling() {
        let temp = tempdir().expect("tempdir");
        let primary_path = temp.path().join("db.sqlite");
        let primary = create_primary_database(&primary_path);
        insert_primary_session(&primary, "sess_selected", r"E:\project");
        insert_primary_session(&primary, "sess_sibling", r"E:\project");
        drop(primary);

        let deleted = delete_sessions_from_database(&primary_path, "sess_selected")
            .expect("delete selected session");
        assert!(deleted);

        let primary = Connection::open(&primary_path).expect("reopen primary");
        for table in ["session", "message", "part", "input_history"] {
            let column = if table == "session" {
                "id"
            } else {
                "session_id"
            };
            assert_eq!(count_by_id(&primary, table, column, "sess_selected"), 0);
            assert_eq!(count_by_id(&primary, table, column, "sess_sibling"), 1);
        }

        let index_path = temp.path().join("tasks-index.sqlite");
        let index = create_task_index(&index_path);
        insert_index_task(&index, r"E:\project", "sess_selected", 0);
        insert_index_task(&index, r"E:\project", "sess_sibling", 1);
        drop(index);

        cleanup_task_index(&index_path, "sess_selected").expect("clean selected task");
        let index = Connection::open(&index_path).expect("reopen index");
        assert_eq!(count_by_id(&index, "tasks", "task_id", "sess_selected"), 0);
        assert_eq!(count_by_id(&index, "tasks", "task_id", "sess_sibling"), 1);
        assert_eq!(
            count_by_id(&index, "future_task_cache", "task_id", "sess_selected"),
            0
        );
        assert_eq!(
            count_by_id(&index, "future_task_cache", "task_id", "sess_sibling"),
            1
        );
        assert_eq!(
            count_by_id(&index, "task_group_members", "task_id", "sess_selected"),
            0
        );
        assert_eq!(
            index
                .query_row(
                    "SELECT COUNT(*) FROM task_group_view_node_orders",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn project_cascade_delete_is_rejected_without_touching_databases() {
        let error = delete_session(
            "sess_selected",
            "sqlite-zcode:/nonexistent/db.sqlite:sess_selected",
            true,
        )
        .expect_err("project cascade must be rejected");
        assert!(error.contains("已禁用"));
    }

    #[test]
    fn sidecar_cleanup_is_scoped_to_a_safe_session_id() {
        let temp = tempdir().expect("tempdir");
        let cli = temp.path().join("cli");
        let id = "sess_selected";
        let rollout = cli.join("rollout").join(format!("model-io-{id}.jsonl"));
        let artifact = cli.join("artifacts").join(id).join("result.json");
        std::fs::create_dir_all(rollout.parent().unwrap()).expect("rollout dir");
        std::fs::create_dir_all(artifact.parent().unwrap()).expect("artifact dir");
        std::fs::write(&rollout, "rollout").expect("rollout file");
        std::fs::write(&artifact, "artifact").expect("artifact file");

        cleanup_session_sidecars(&cli, id).expect("clean sidecars");
        assert!(!rollout.exists());
        assert!(!cli.join("artifacts").join(id).exists());
        assert!(validate_session_id("../escape").is_err());
        assert!(validate_session_id(r"sess\escape").is_err());
    }

    fn repeated_user_fixture(with_error: bool) -> Vec<SessionMessage> {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, data TEXT, time_created INTEGER);
             CREATE TABLE part(message_id TEXT, session_id TEXT, data TEXT, time_created INTEGER);"
        ).unwrap();
        for index in 1..=5 {
            let id = format!("user_{index}");
            connection
                .execute(
                    "INSERT INTO message VALUES (?1, 'session', ?2, ?3)",
                    rusqlite::params![id, r#"{"role":"user"}"#, index * 2],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO part VALUES (?1, 'session', ?2, ?3)",
                    rusqlite::params![id, r#"{"type":"text","text":"怎么这么多"}"#, index * 2],
                )
                .unwrap();
            if with_error {
                connection
                    .execute(
                        "INSERT INTO message VALUES (?1, 'session', ?2, ?3)",
                        rusqlite::params![
                            format!("error_{index}"),
                            r#"{"role":"assistant","error":{"name":"APIError"}}"#,
                            index * 2 + 1
                        ],
                    )
                    .unwrap();
            }
        }
        read_messages_from_connection(&connection, "session").unwrap()
    }

    #[test]
    fn regression_distinct_identical_user_messages_are_preserved_without_retry_evidence() {
        let messages = repeated_user_fixture(false);
        assert_eq!(messages.len(), 5);
        assert!(messages
            .iter()
            .all(|message| message.content == "怎么这么多"));
        assert_eq!(
            messages
                .iter()
                .map(|message| message.ts)
                .collect::<Vec<_>>(),
            vec![Some(2), Some(4), Some(6), Some(8), Some(10)]
        );
    }

    #[test]
    fn regression_empty_error_assistant_rows_do_not_prove_user_retry() {
        let messages = repeated_user_fixture(true);
        assert_eq!(messages.len(), 5);
        assert!(messages.iter().all(|message| message.role == "user"));
    }

    #[test]
    #[allow(deprecated)]
    fn scan_sessions_populates_parent_lineage_soft_deleted_archived_and_truncates_titles() {
        let _guard = zcode_env_lock().lock().expect("env lock");
        let temp = tempdir().expect("tempdir");
        let zcode_base = temp.path().join("zcode-base");
        let cli_db_dir = zcode_base.join(".zcode").join("cli").join("db");
        let v2_dir = zcode_base.join(".zcode").join("v2");
        std::fs::create_dir_all(&cli_db_dir).expect("cli db dir");
        std::fs::create_dir_all(&v2_dir).expect("v2 dir");

        let db_path = cli_db_dir.join("db.sqlite");
        let conn = Connection::open(&db_path).expect("open db");
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                title TEXT,
                directory TEXT,
                time_created INTEGER,
                time_updated INTEGER,
                parent_id TEXT,
                time_deleted INTEGER,
                time_archived INTEGER
             );
             CREATE TABLE session_task_link (
                id TEXT PRIMARY KEY,
                parent_session_id TEXT,
                child_session_id TEXT
             );",
        )
        .expect("create session schema");

        let long_title = "T".repeat(200);
        conn.execute(
            "INSERT INTO session VALUES ('sess_root', ?1, '/work/proj', 100, 500, NULL, NULL, NULL)",
            [&long_title],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('sess_sub', 'Subagent', '/work/proj', 110, 400, 'sess_root', NULL, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('sess_linked', 'Linked Child', '/work/proj', 120, 350, NULL, NULL, 999)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_task_link VALUES ('lnk1', 'sess_root', 'sess_linked')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('sess_soft_del_task', 'Soft Deleted Task', '/work/proj', 130, 300, NULL, NULL, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('sess_time_del', 'CLI Deleted', '/work/proj', 140, 250, NULL, 1234, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('sess_fork_arch', 'Forked Archived', '/work/proj', 150, 200, NULL, 0, 0)",
            [],
        )
        .unwrap();
        drop(conn);

        let index_path = v2_dir.join("tasks-index.sqlite");
        let idx = Connection::open(&index_path).expect("open index");
        idx.execute_batch(
            "CREATE TABLE tasks (
                workspace_key TEXT NOT NULL,
                task_id TEXT NOT NULL,
                forked_from_task_id TEXT,
                deleted INTEGER DEFAULT 0,
                archived INTEGER DEFAULT 0,
                PRIMARY KEY (workspace_key, task_id)
             );",
        )
        .unwrap();
        idx.execute(
            "INSERT INTO tasks VALUES ('w1', 'sess_root', NULL, 0, 0)",
            [],
        )
        .unwrap();
        idx.execute(
            "INSERT INTO tasks VALUES ('w1', 'sess_soft_del_task', NULL, 1, 0)",
            [],
        )
        .unwrap();
        idx.execute(
            "INSERT INTO tasks VALUES ('w1', 'sess_time_del', NULL, 0, 0)",
            [],
        )
        .unwrap();
        idx.execute(
            "INSERT INTO tasks VALUES ('w1', 'sess_fork_arch', 'sess_root', 0, 1)",
            [],
        )
        .unwrap();
        drop(idx);

        let prev_base = std::env::var_os("ZCODE_DATA_BASE_DIR");
        let prev_storage = std::env::var_os("ZCODE_STORAGE_DIR");
        std::env::set_var("ZCODE_DATA_BASE_DIR", &zcode_base);
        std::env::remove_var("ZCODE_STORAGE_DIR");

        let sessions = scan_sessions();

        if let Some(v) = prev_base {
            std::env::set_var("ZCODE_DATA_BASE_DIR", v);
        } else {
            std::env::remove_var("ZCODE_DATA_BASE_DIR");
        }
        if let Some(v) = prev_storage {
            std::env::set_var("ZCODE_STORAGE_DIR", v);
        }

        let by_id: HashMap<&str, &SessionMeta> = sessions
            .iter()
            .map(|s| (s.session_id.as_str(), s))
            .collect();
        assert_eq!(by_id.len(), 6);

        let root = by_id["sess_root"];
        assert!(!root.residual);
        assert!(!root.archived);
        assert!(root.parent_session_id.is_none());
        assert_eq!(root.resume_command.as_deref(), Some("zcode -s sess_root"));
        assert_eq!(
            root.title.as_ref().unwrap().chars().count(),
            TITLE_MAX_CHARS + 3
        );
        assert_eq!(root.summary.as_ref().unwrap().chars().count(), 160 + 3);

        let sub = by_id["sess_sub"];
        assert!(sub.residual, "unindexed subagent session must be residual");
        assert!(sub.resume_command.is_none());
        assert_eq!(sub.parent_session_id.as_deref(), Some("sess_root"));

        let linked = by_id["sess_linked"];
        assert!(linked.archived, "session.time_archived > 0 marks archived");
        assert_eq!(linked.parent_session_id.as_deref(), Some("sess_root"));

        let soft_del = by_id["sess_soft_del_task"];
        assert!(
            soft_del.residual,
            "tasks.deleted != 0 marks session residual"
        );
        assert!(soft_del.resume_command.is_none());

        let time_del = by_id["sess_time_del"];
        assert!(
            time_del.residual,
            "session.time_deleted > 0 marks session residual"
        );
        assert!(time_del.resume_command.is_none());

        let fork_arch = by_id["sess_fork_arch"];
        assert!(!fork_arch.residual);
        assert!(fork_arch.archived, "tasks.archived != 0 marks archived");
        assert_eq!(fork_arch.parent_session_id.as_deref(), Some("sess_root"));
    }

    #[test]
    #[allow(deprecated)]
    fn orphaned_links_do_not_deadlock_parent_deletion_and_automations_guard_active_tasks() {
        let _guard = zcode_env_lock().lock().expect("env lock");
        let temp = tempdir().expect("tempdir");
        let zcode_base = temp.path().join("zcode-base");
        let cli_dir = zcode_base.join(".zcode").join("cli");
        let db_dir = cli_dir.join("db");
        let v2_dir = zcode_base.join(".zcode").join("v2");
        std::fs::create_dir_all(&db_dir).unwrap();
        std::fs::create_dir_all(&v2_dir).unwrap();

        let primary_path = db_dir.join("db.sqlite");
        let primary = Connection::open(&primary_path).unwrap();
        // Strict FK schema WITHOUT ON DELETE CASCADE between part -> message or session_task_link -> session
        primary
            .execute_batch(
                "PRAGMA foreign_keys=ON;
                 CREATE TABLE session (
                    id TEXT PRIMARY KEY,
                    directory TEXT NOT NULL,
                    parent_id TEXT
                 );
                 CREATE TABLE message (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL REFERENCES session(id)
                 );
                 CREATE TABLE part (
                    id TEXT PRIMARY KEY,
                    message_id TEXT NOT NULL REFERENCES message(id),
                    session_id TEXT NOT NULL REFERENCES session(id)
                 );
                 CREATE TABLE session_task_link (
                    id TEXT PRIMARY KEY,
                    parent_session_id TEXT,
                    child_session_id TEXT NOT NULL
                 );",
            )
            .unwrap();
        primary
            .execute(
                "INSERT INTO session VALUES ('sess_parent', '/proj', NULL)",
                [],
            )
            .unwrap();
        primary
            .execute("INSERT INTO message VALUES ('msg_1', 'sess_parent')", [])
            .unwrap();
        primary
            .execute(
                "INSERT INTO part VALUES ('prt_1', 'msg_1', 'sess_parent')",
                [],
            )
            .unwrap();
        // Orphaned link row pointing to a child session that no longer exists in `session`
        primary
            .execute(
                "INSERT INTO session_task_link VALUES ('orphan_lnk', 'sess_parent', 'sess_deleted_child')",
                [],
            )
            .unwrap();
        drop(primary);

        let index_path = v2_dir.join("tasks-index.sqlite");
        let index = Connection::open(&index_path).unwrap();
        index
            .execute_batch(
                "CREATE TABLE tasks (
                    workspace_key TEXT NOT NULL,
                    task_id TEXT NOT NULL,
                    forked_from_task_id TEXT,
                    deleted INTEGER DEFAULT 0,
                    PRIMARY KEY (workspace_key, task_id)
                 );
                 CREATE TABLE automations (
                    id TEXT PRIMARY KEY,
                    task_id TEXT NOT NULL,
                    status TEXT NOT NULL,
                    enabled INTEGER DEFAULT 1
                 );",
            )
            .unwrap();
        index
            .execute(
                "INSERT INTO tasks VALUES ('w1', 'sess_parent', NULL, 0)",
                [],
            )
            .unwrap();
        // Orphaned tasks row whose child session is absent from `session`
        index
            .execute(
                "INSERT INTO tasks VALUES ('w1', 'sess_vanished_fork', 'sess_parent', 0)",
                [],
            )
            .unwrap();
        // Soft-deleted tasks row referencing `sess_parent`
        index
            .execute(
                "INSERT INTO tasks VALUES ('w1', 'sess_soft_deleted', 'sess_parent', 1)",
                [],
            )
            .unwrap();
        // Active automation referencing `sess_parent`
        index
            .execute(
                "INSERT INTO automations VALUES ('auto_1', 'sess_parent', 'active', 1)",
                [],
            )
            .unwrap();
        drop(index);

        let prev_base = std::env::var_os("ZCODE_DATA_BASE_DIR");
        let prev_storage = std::env::var_os("ZCODE_STORAGE_DIR");
        std::env::set_var("ZCODE_DATA_BASE_DIR", &zcode_base);
        std::env::remove_var("ZCODE_STORAGE_DIR");

        let source = source_reference(&primary_path, "sess_parent");
        // Self-contained delete_session blocks while automation is active
        let auto_err = delete_session("sess_parent", &source, false)
            .expect_err("active automation must block");
        assert!(auto_err.contains("自动化"));

        // Disable automation -> orphaned links do NOT deadlock deletion, and strict FK order succeeds
        let index = Connection::open(&index_path).unwrap();
        index
            .execute(
                "UPDATE automations SET status='disabled', enabled=0 WHERE id='auto_1'",
                [],
            )
            .unwrap();
        drop(index);

        let deleted = delete_session("sess_parent", &source, false)
            .expect("delete succeeds once automation disabled");
        assert!(deleted);

        if let Some(v) = prev_base {
            std::env::set_var("ZCODE_DATA_BASE_DIR", v);
        } else {
            std::env::remove_var("ZCODE_DATA_BASE_DIR");
        }
        if let Some(v) = prev_storage {
            std::env::set_var("ZCODE_STORAGE_DIR", v);
        }

        let primary = Connection::open(&primary_path).unwrap();
        assert_eq!(count_by_id(&primary, "session", "id", "sess_parent"), 0);
        assert_eq!(
            count_by_id(
                &primary,
                "session_task_link",
                "parent_session_id",
                "sess_parent"
            ),
            0
        );

        let index = Connection::open(&index_path).unwrap();
        let dangling_soft_forks: i64 = index
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE forked_from_task_id='sess_parent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // Soft-deleted row had its dangling forked_from_task_id cleared to NULL
        let soft_fork_val: Option<String> = index
            .query_row(
                "SELECT forked_from_task_id FROM tasks WHERE task_id='sess_soft_deleted'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(soft_fork_val.is_none());
        assert_eq!(
            dangling_soft_forks, 1,
            "only non-deleted vanished row remained untouched"
        );

        assert!(validate_session_id(&"x".repeat(129)).is_err());
    }

    #[test]
    fn read_messages_extracts_thinking_role_and_tool_outputs_and_errors() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, data TEXT, time_created INTEGER);
                 CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, data TEXT, time_created INTEGER);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message VALUES ('m1', 's1', '{\"role\":\"user\"}', 10)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO part VALUES ('p1', 'm1', 's1', '{\"type\":\"text\",\"text\":\"Run tests\"}', 10)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message VALUES ('m2', 's1', '{\"role\":\"assistant\"}', 20)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO part VALUES ('p2', 'm2', 's1', '{\"type\":\"reasoning\",\"text\":\"Checking test suite\"}', 20)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO part VALUES ('p3', 'm2', 's1', '{\"type\":\"tool\",\"tool\":\"bash\",\"state\":{\"output\":\"ok 12 passed\",\"error\":{\"message\":\"exit 1\"}}}', 21)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO part VALUES ('p4', 'm2', 's1', '{\"type\":\"text\",\"text\":\"Tests finished.\"}', 22)",
                [],
            )
            .unwrap();

        let messages = read_messages_from_connection(&connection, "s1").unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "Run tests");
        assert_eq!(messages[1].role, "thinking");
        assert_eq!(messages[1].content, "Checking test suite");
        assert_eq!(messages[1].ts, Some(20));
        assert_eq!(messages[2].role, "assistant");
        assert_eq!(
            messages[2].content,
            "[Tool: bash]\nok 12 passed\n[Error: exit 1]\nTests finished."
        );
        assert_eq!(messages[2].ts, Some(20));
    }
}
