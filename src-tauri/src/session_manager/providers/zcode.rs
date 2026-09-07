use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, Transaction};
use serde_json::Value;

use crate::session_manager::{SessionMessage, SessionMeta};

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
            archived: false,
            cleanup_pending: false,
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

    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(|e| e.to_string())?;
    connection
        .execute_batch("BEGIN DEFERRED")
        .map_err(|e| e.to_string())?;
    super::utils::check_sqlite_message_budget(&connection, &session_id)?;

    let mut message_statement = connection
        .prepare(
            "SELECT id, data, time_created FROM message WHERE session_id=?1 ORDER BY time_created",
        )
        .map_err(|error| format!("Failed to prepare ZCode message query: {error}"))?;
    let messages = message_statement
        .query_map([session_id.as_str()], |row| {
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
        .query_map([session_id.as_str()], |row| {
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

    // ZCode 的运行器在配额/并发等请求失败时会自动重试，把同一条用户消息
    // 重复追加进 message 表（每次伴随一条 error 占位的 assistant 行，那些
    // 行没有文本 part，本就不会显示）。桌面端把这种失败重试链折叠为一条；
    // 这里对连续且内容完全相同的用户消息做同样的折叠（保留最后一条），
    // 中间隔着正常 AI 回复的真实重发不受影响。
    let output = collapse_retry_user_messages(output);
    Ok(output)
}

/// 折叠"连续且内容相同的用户消息"重试链，保留最后一条。
fn collapse_retry_user_messages(messages: Vec<SessionMessage>) -> Vec<SessionMessage> {
    let mut collapsed: Vec<SessionMessage> = Vec::with_capacity(messages.len());
    for message in messages {
        if let Some(last) = collapsed.last_mut() {
            if last.role == "user" && message.role == "user" && last.content == message.content {
                *last = message;
                continue;
            }
        }
        collapsed.push(message);
    }
    collapsed
}

pub fn delete_session(
    session_id: &str,
    source: &str,
    project_dir: Option<&str>,
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

    let initial_ids = BTreeSet::from([session_id.to_string()]);
    // The task index is ZCode's native sidebar. Clean it first so a later
    // primary-database failure leaves a recoverable residual session instead
    // of an unopenable native ghost entry.
    let mut session_ids = cleanup_task_index(
        &tasks_index_path(),
        &initial_ids,
        project_dir,
        project_was_deleted,
    )?;
    let (deleted, discovered_ids) =
        delete_sessions_from_database(&actual, session_id, project_dir, project_was_deleted)?;
    session_ids.extend(discovered_ids);

    let mut errors = Vec::new();
    for id in &session_ids {
        if let Err(error) = cleanup_session_sidecars(cli_root, id) {
            errors.push(error);
        }
    }

    if errors.is_empty() {
        Ok(deleted)
    } else {
        Err(errors.join("；"))
    }
}

fn delete_sessions_from_database(
    path: &Path,
    selected_session_id: &str,
    project_dir: Option<&str>,
    project_was_deleted: bool,
) -> Result<(bool, BTreeSet<String>), String> {
    let mut connection = Connection::open(path)
        .map_err(|error| format!("Failed to open ZCode database: {error}"))?;
    connection
        .busy_timeout(Duration::from_secs(3))
        .map_err(|error| format!("Failed to configure ZCode database timeout: {error}"))?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(|error| format!("Failed to enable ZCode foreign keys: {error}"))?;

    let mut session_ids = BTreeSet::from([selected_session_id.to_string()]);
    if project_was_deleted {
        if let Some(project_dir) = project_dir {
            let target = normalized_path(project_dir);
            let mut statement = connection
                .prepare("SELECT id, directory FROM session")
                .map_err(|error| format!("Failed to query ZCode project sessions: {error}"))?;
            let matches = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|error| format!("Failed to read ZCode project sessions: {error}"))?
                .flatten()
                .filter(|(_, directory)| normalized_path(directory) == target)
                .map(|(id, _)| id)
                .collect::<Vec<_>>();
            drop(statement);
            session_ids.extend(matches);
        }
    }

    for id in &session_ids {
        validate_session_id(id)?;
    }

    let transaction = connection
        .transaction()
        .map_err(|error| format!("Failed to start ZCode deletion: {error}"))?;
    let tables = table_names(&transaction)?;
    for id in &session_ids {
        for table in &tables {
            if table == "session" || !table_has_column(&transaction, table, "session_id")? {
                continue;
            }
            let quoted = quote_identifier(table);
            transaction
                .execute(&format!("DELETE FROM {quoted} WHERE session_id=?1"), [id])
                .map_err(|error| format!("Failed to delete ZCode {table} rows: {error}"))?;
        }
    }
    let mut selected_deleted = false;
    for id in &session_ids {
        let count = transaction
            .execute("DELETE FROM session WHERE id=?1", [id])
            .map_err(|error| format!("Failed to delete ZCode session: {error}"))?;
        if id == selected_session_id {
            selected_deleted = count > 0;
        }
    }
    transaction
        .commit()
        .map_err(|error| format!("Failed to commit ZCode deletion: {error}"))?;
    Ok((selected_deleted, session_ids))
}

fn cleanup_task_index(
    path: &Path,
    session_ids: &BTreeSet<String>,
    project_dir: Option<&str>,
    project_was_deleted: bool,
) -> Result<BTreeSet<String>, String> {
    if !path.is_file() {
        return Ok(BTreeSet::new());
    }
    let mut connection = Connection::open(path)
        .map_err(|error| format!("Failed to open ZCode task index: {error}"))?;
    connection
        .busy_timeout(Duration::from_secs(3))
        .map_err(|error| format!("Failed to configure ZCode task index timeout: {error}"))?;

    let target_project = project_was_deleted
        .then(|| project_dir.map(normalized_path))
        .flatten();
    let mut ids = session_ids.clone();
    if target_project.is_some() {
        let target = target_project.as_deref().unwrap_or_default();
        let mut statement = connection
            .prepare("SELECT workspace_key, workspace_path, task_id FROM tasks")
            .map_err(|error| format!("Failed to query ZCode task index: {error}"))?;
        let matches = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| format!("Failed to read ZCode task index: {error}"))?
            .flatten()
            .filter(|(workspace_key, workspace_path, _)| {
                normalized_path(workspace_key) == target
                    || normalized_path(workspace_path) == target
            })
            .map(|(_, _, id)| id)
            .collect::<Vec<_>>();
        drop(statement);
        ids.extend(matches);
    }

    for id in &ids {
        validate_session_id(id)?;
    }

    let transaction = connection
        .transaction()
        .map_err(|error| format!("Failed to start ZCode task cleanup: {error}"))?;
    let tables = table_names(&transaction)?;
    if tables.iter().any(|table| table == "task_group_members") {
        for id in &ids {
            transaction
                .execute("DELETE FROM task_group_members WHERE task_id=?1", [id])
                .map_err(|error| format!("Failed to delete ZCode task group member: {error}"))?;
        }
    }
    // Keep pace with new ZCode index tables without depending on a fixed
    // schema version. Any auxiliary table keyed by task_id is session-scoped
    // and must be cleaned before the task catalog row itself.
    for table in &tables {
        if matches!(table.as_str(), "tasks" | "task_group_members")
            || !table_has_column(&transaction, table, "task_id")?
        {
            continue;
        }
        let quoted = quote_identifier(table);
        for id in &ids {
            transaction
                .execute(&format!("DELETE FROM {quoted} WHERE task_id=?1"), [id])
                .map_err(|error| format!("Failed to delete ZCode {table} row: {error}"))?;
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
            .filter(|(_, node_key)| {
                task_order_session_id(node_key).is_some_and(|id| ids.contains(&id))
            })
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
        for id in &ids {
            transaction
                .execute("DELETE FROM tasks WHERE task_id=?1", [id])
                .map_err(|error| format!("Failed to delete ZCode task index row: {error}"))?;
        }
    }
    if let Some(target) = target_project {
        if tables
            .iter()
            .any(|table| table == "task_group_workspace_bootstraps")
        {
            let keys = collect_matching_values(
                &transaction,
                "task_group_workspace_bootstraps",
                "workspace_key",
                &target,
            )?;
            for key in keys {
                transaction
                    .execute(
                        "DELETE FROM task_group_workspace_bootstraps WHERE workspace_key=?1",
                        [key],
                    )
                    .map_err(|error| {
                        format!("Failed to delete ZCode workspace bootstrap: {error}")
                    })?;
            }
        }
    }
    transaction
        .commit()
        .map_err(|error| format!("Failed to commit ZCode task cleanup: {error}"))?;
    Ok(ids)
}

fn table_names(transaction: &Transaction<'_>) -> Result<Vec<String>, String> {
    let mut statement = transaction
        .prepare("SELECT name FROM sqlite_master WHERE type='table'")
        .map_err(|error| error.to_string())?;
    let names = statement
        .query_map([], |row| row.get(0))
        .map_err(|error| error.to_string())?
        .flatten()
        .collect();
    Ok(names)
}

fn table_has_column(
    transaction: &Transaction<'_>,
    table: &str,
    column: &str,
) -> Result<bool, String> {
    let quoted = quote_identifier(table);
    let mut statement = transaction
        .prepare(&format!("PRAGMA table_info({quoted})"))
        .map_err(|error| error.to_string())?;
    let found = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| error.to_string())?
        .flatten()
        .any(|name| name == column);
    Ok(found)
}

fn collect_matching_values(
    transaction: &Transaction<'_>,
    table: &str,
    column: &str,
    normalized_target: &str,
) -> Result<Vec<String>, String> {
    let table = quote_identifier(table);
    let column = quote_identifier(column);
    let mut statement = transaction
        .prepare(&format!("SELECT {column} FROM {table}"))
        .map_err(|error| error.to_string())?;
    let values = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .flatten()
        .filter(|value| normalized_path(value) == normalized_target)
        .collect();
    Ok(values)
}

fn task_order_session_id(node_key: &str) -> Option<String> {
    let values: Vec<String> = serde_json::from_str(node_key).ok()?;
    values.get(1).cloned()
}

fn cleanup_session_sidecars(cli_root: &Path, session_id: &str) -> Result<(), String> {
    for target in session_sidecar_targets(cli_root, session_id)? {
        remove_path_if_exists(&target).map_err(|error| {
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

fn remove_path_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
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

fn normalized_path(value: &str) -> String {
    value
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
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

    fn create_primary_database(path: &Path) -> Connection {
        let connection = Connection::open(path).expect("open primary database");
        connection
            .execute_batch(
                "PRAGMA foreign_keys=ON;
                 CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL);
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
                    quote_identifier(table),
                    quote_identifier(column)
                ),
                [id],
                |row| row.get(0),
            )
            .expect("count rows")
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

        let (deleted, ids) = delete_sessions_from_database(
            &primary_path,
            "sess_selected",
            Some(r"E:\project"),
            false,
        )
        .expect("delete selected session");
        assert!(deleted);
        assert_eq!(ids, BTreeSet::from(["sess_selected".to_string()]));

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

        cleanup_task_index(&index_path, &ids, Some(r"E:\project"), false)
            .expect("clean selected task");
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
    fn project_cleanup_removes_all_matching_workspace_tasks_only() {
        let temp = tempdir().expect("tempdir");
        let primary_path = temp.path().join("db.sqlite");
        let primary = create_primary_database(&primary_path);
        insert_primary_session(&primary, "sess_selected", r"E:\Project\");
        insert_primary_session(&primary, "sess_same_project", "e:/project");
        insert_primary_session(&primary, "sess_other", r"E:\other");
        drop(primary);

        let (deleted, ids) = delete_sessions_from_database(
            &primary_path,
            "sess_selected",
            Some(r"E:\project"),
            true,
        )
        .expect("delete project sessions");
        assert!(deleted);
        assert_eq!(
            ids,
            BTreeSet::from(["sess_same_project".to_string(), "sess_selected".to_string()])
        );

        let index_path = temp.path().join("tasks-index.sqlite");
        let index = create_task_index(&index_path);
        insert_index_task(&index, r"E:\PROJECT", "sess_selected", 0);
        insert_index_task(&index, r"E:\project", "sess_index_only", 1);
        insert_index_task(&index, r"E:\other", "sess_other", 2);
        index
            .execute(
                "INSERT INTO task_group_workspace_bootstraps (workspace_key, group_id) VALUES (?1, 'group')",
                [r"E:\Project\"],
            )
            .expect("insert workspace bootstrap");
        drop(index);

        let all_ids = cleanup_task_index(&index_path, &ids, Some(r"e:/project"), true)
            .expect("clean project tasks");
        assert!(all_ids.contains("sess_index_only"));
        let index = Connection::open(&index_path).expect("reopen index");
        assert_eq!(
            index
                .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(count_by_id(&index, "tasks", "task_id", "sess_other"), 1);
        assert_eq!(
            index
                .query_row(
                    "SELECT COUNT(*) FROM task_group_workspace_bootstraps",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
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

    #[test]
    fn retry_chain_of_identical_user_messages_collapses_to_the_last() {
        let user = |content: &str, ts: i64| SessionMessage {
            role: "user".into(),
            content: content.into(),
            ts: Some(ts),
        };
        let assistant = |content: &str| SessionMessage {
            role: "assistant".into(),
            content: content.into(),
            ts: None,
        };

        // 失败重试链：连续相同用户消息（ZCode 运行器自动重试）→ 折叠为最后一条
        let collapsed = collapse_retry_user_messages(vec![
            user("怎么这么多", 1),
            user("怎么这么多", 2),
            user("怎么这么多", 3),
            user("怎么这么多", 4),
            user("怎么这么多", 5),
        ]);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].content, "怎么这么多");
        assert_eq!(collapsed[0].ts, Some(5));

        // 中间隔着正常 AI 回复的真实重发：不折叠
        let collapsed = collapse_retry_user_messages(vec![
            user("怎么这么多", 1),
            assistant("这是一个 Tauri 项目"),
            user("怎么这么多", 2),
        ]);
        assert_eq!(collapsed.len(), 3);

        // 内容不同的连续用户消息：不折叠
        let collapsed = collapse_retry_user_messages(vec![user("第一条", 1), user("第二条", 2)]);
        assert_eq!(collapsed.len(), 2);
    }
}
