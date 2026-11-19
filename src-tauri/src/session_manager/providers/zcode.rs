use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::session_manager::cache;
use crate::session_manager::{DeleteSessionRequest, SessionMessage, SessionMeta};

use super::utils::{
    order_selected_children, quote_sqlite_identifier, sqlite_table_has_column, sqlite_table_names,
    validate_parent_graph,
};

const PROVIDER_ID: &str = "zcode";
const SOURCE_PREFIX: &str = "sqlite-zcode:";

pub(crate) fn database_path() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".zcode/cli/db/db.sqlite"))
        .unwrap_or_else(|| PathBuf::from(".zcode/cli/db/db.sqlite"))
}

fn tasks_index_path() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".zcode/v2/tasks-index.sqlite"))
        .unwrap_or_else(|| PathBuf::from(".zcode/v2/tasks-index.sqlite"))
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
    let mut statement = match connection.prepare(
        "SELECT id, COALESCE(title, ''), COALESCE(directory, ''), \
         COALESCE(time_created, 0), COALESCE(time_updated, time_created, 0) \
         FROM session ORDER BY time_updated DESC",
    ) {
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
        ))
    }) {
        Ok(rows) => rows,
        Err(error) => {
            super::utils::scan_warning(error.to_string());
            return Vec::new();
        }
    };

    let native_task_ids = load_native_task_ids(&tasks_index_path());
    if tasks_index_path().exists() && native_task_ids.is_none() {
        super::utils::scan_warning("任务侧边栏索引无法读取，残留分类暂不可用");
    }
    rows.filter_map(|row| match row {
        Ok(row) => Some(row),
        Err(error) => {
            super::utils::scan_warning(error.to_string());
            None
        }
    })
    .map(|(session_id, title, directory, created, updated)| {
        let display_title = if title.trim().is_empty() {
            directory
                .trim_end_matches(['/', '\\'])
                .rsplit(['/', '\\'])
                .next()
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        } else {
            Some(title)
        };
        let residual = native_task_ids
            .as_ref()
            .is_some_and(|task_ids| !task_ids.contains(&session_id));
        SessionMeta {
            provider_id: PROVIDER_ID.to_string(),
            session_id: session_id.clone(),
            residual,
            parent_session_id: None,
            archived: false,
            cleanup_pending: false,
            sidebar_section: None,
            title: display_title.clone(),
            summary: display_title,
            project_dir: (!directory.is_empty()).then_some(directory),
            project_name: None,
            created_at: (created > 0).then_some(created),
            last_active_at: (updated > 0).then_some(updated),
            source_path: Some(source_reference(&path, &session_id)),
            resume_command: (!residual).then(|| format!("zcode -s {session_id}")),
        }
    })
    .collect()
}

/// ZCode's desktop task list is backed by a separate index database. Session
/// rows that remain in the CLI database after their task index entry vanished
/// still contain recoverable conversation data, but are no longer shown by
/// ZCode itself.
fn load_native_task_ids(path: &Path) -> Option<BTreeSet<String>> {
    if !path.is_file() {
        return None;
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    connection.busy_timeout(Duration::from_secs(2)).ok()?;
    let mut statement = connection.prepare("SELECT task_id FROM tasks").ok()?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .ok()?;
    Some(rows.flatten().filter(|id| !id.trim().is_empty()).collect())
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

fn read_parent_rows(
    connection: &Connection,
    sql: &str,
    parents: &mut HashMap<String, String>,
    label: &str,
) -> Result<(), String> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| format!("Failed to inspect ZCode {label}: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| format!("Failed to read ZCode {label}: {error}"))?;
    for row in rows {
        let (child, parent) =
            row.map_err(|error| format!("Invalid ZCode {label} relation: {error}"))?;
        insert_parent_relation(parents, child, parent)?;
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
    if !primary_tables.contains("session") {
        return Err("ZCode database is missing the session table".into());
    }

    let mut parents = HashMap::new();
    if sqlite_table_has_column(&primary, "session", "parent_id")? {
        read_parent_rows(
            &primary,
            "SELECT id, parent_id FROM session WHERE parent_id IS NOT NULL AND parent_id<>''",
            &mut parents,
            "session.parent_id",
        )?;
    }
    if primary_tables.contains("session_task_link") {
        if !sqlite_table_has_column(&primary, "session_task_link", "parent_session_id")?
            || !sqlite_table_has_column(&primary, "session_task_link", "child_session_id")?
        {
            return Err("ZCode session_task_link table has an unsupported shape".into());
        }
        read_parent_rows(
            &primary,
            "SELECT child_session_id, parent_session_id FROM session_task_link \
             WHERE child_session_id IS NOT NULL AND child_session_id<>'' \
             AND parent_session_id IS NOT NULL AND parent_session_id<>''",
            &mut parents,
            "session_task_link",
        )?;
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
                read_parent_rows(
                    &index,
                    "SELECT task_id, forked_from_task_id FROM tasks \
                     WHERE forked_from_task_id IS NOT NULL AND forked_from_task_id<>''",
                    &mut parents,
                    "tasks.forked_from_task_id",
                )?;
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

/// 正文实际依赖的文件：主库与 WAL 边车。引用无法解析时返回 `None`，由读取器报错。
///
/// ZCode 的会话正文全部在 `message` / `part` 表里（`load_messages` 只打开主库），
/// 桌面任务索引库不参与正文，因此不计入指纹。
pub fn transcript_sources(source: &str) -> Option<cache::TranscriptSources> {
    let (path, _) = parse_source(source)?;
    Some(cache::sqlite_transcript_sources(&path))
}

pub fn load_messages(source: &str) -> Result<Vec<SessionMessage>, String> {
    let (path, session_id) = parse_source(source)
        .ok_or_else(|| format!("Invalid ZCode SQLite source reference: {source}"))?;
    if path.canonicalize().map_err(|e| e.to_string())?
        != database_path().canonicalize().map_err(|e| e.to_string())?
    {
        return Err("ZCode database is outside configured storage".into());
    }
    let connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("Failed to open ZCode database: {error}"))?;

    read_messages_from_connection(&connection, &session_id)
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
            "SELECT id, data, time_created FROM message WHERE session_id=?1 ORDER BY time_created",
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

    let mut part_statement = connection
        .prepare("SELECT message_id, data FROM part WHERE session_id=?1 ORDER BY time_created")
        .map_err(|error| format!("Failed to prepare ZCode part query: {error}"))?;
    let parts = part_statement
        .query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| format!("Failed to query ZCode parts: {error}"))?;

    let mut part_texts: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for row in parts {
        let (message_id, raw) = row.map_err(|e| e.to_string())?;
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(text) = value.get("text").and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    part_texts
                        .entry(message_id)
                        .or_default()
                        .push(text.to_string());
                }
            }
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
        let content = part_texts
            .remove(&message_id)
            .unwrap_or_default()
            .join("\n");
        if !content.trim().is_empty() {
            output.push(SessionMessage {
                role,
                content,
                ts: (timestamp > 0).then_some(timestamp),
            });
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

    let configured_database = database_path();
    let cli_root = configured_database
        .parent()
        .and_then(Path::parent)
        .ok_or("Invalid ZCode CLI directory")?;
    session_sidecar_targets(cli_root, session_id)?;

    // The task index is ZCode's native sidebar. Clean it first so a later
    // primary-database failure leaves a recoverable residual session instead
    // of an unopenable native ghost entry.
    cleanup_task_index(&tasks_index_path(), session_id)?;
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
        .transaction()
        .map_err(|error| format!("Failed to start ZCode deletion: {error}"))?;
    let tables = sqlite_table_names(&transaction)?;
    for table in &tables {
        if table == "session" || !sqlite_table_has_column(&transaction, table, "session_id")? {
            continue;
        }
        let quoted = quote_sqlite_identifier(table);
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
        .transaction()
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
        if matches!(table.as_str(), "tasks" | "task_group_members")
            || !sqlite_table_has_column(&transaction, table, "task_id")?
        {
            continue;
        }
        let quoted = quote_sqlite_identifier(table);
        transaction
            .execute(
                &format!("DELETE FROM {quoted} WHERE task_id=?1"),
                [session_id],
            )
            .map_err(|error| format!("Failed to delete ZCode {table} row: {error}"))?;
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
    if session_id.is_empty()
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err("Invalid ZCode session ID".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_windows_source_reference_from_the_last_separator() {
        let (path, session_id) = parse_source("sqlite-zcode:C:\\Users\\me\\db.sqlite:session-1")
            .expect("valid reference");
        assert_eq!(path, PathBuf::from("C:\\Users\\me\\db.sqlite"));
        assert_eq!(session_id, "session-1");
    }

    #[test]
    fn transcript_sources_cover_the_database_and_its_wal() {
        let sources =
            transcript_sources("sqlite-zcode:/data/db.sqlite:session-1").expect("valid reference");

        assert_eq!(
            sources,
            vec![
                (PathBuf::from("/data/db.sqlite"), None),
                (PathBuf::from("/data/db.sqlite-wal"), None),
            ],
            "正文全在主库的 message/part 表里；索引库不参与，WAL 必须计入"
        );
        assert!(transcript_sources("sqlite-zcode:/data/db.sqlite").is_none());
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
}
