use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, Transaction};
use serde_json::Value;

use crate::session_manager::{SessionMessage, SessionMeta};

use super::utils::{
    check_sqlite_message_budget, checked_storage_child, collect_files_where, is_safe_session_id,
    parse_timestamp_to_ms, path_basename, truncate_summary,
};

const PROVIDER_ID: &str = "opencode";

/// Return the OpenCode base directory (`$XDG_DATA_HOME/opencode`).
///
/// Respects `XDG_DATA_HOME` on all platforms; falls back to
/// `~/.local/share/opencode/`.
pub(crate) fn get_opencode_base_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("opencode");
        }
    }
    dirs::home_dir()
        .map(|h| h.join(".local/share/opencode"))
        .unwrap_or_else(|| PathBuf::from(".local/share/opencode"))
}

/// Return the OpenCode JSON storage directory (legacy flat-file layout).
pub(crate) fn get_opencode_data_dir() -> PathBuf {
    get_opencode_base_dir().join("storage")
}

fn get_opencode_db_path() -> PathBuf {
    get_opencode_base_dir().join("opencode.db")
}

/// Scan sessions from both the legacy JSON files and the newer SQLite database,
/// merging results with SQLite taking precedence on ID conflicts.
pub fn scan_sessions() -> Vec<SessionMeta> {
    let mut json_sessions = scan_sessions_json();
    let Some(sqlite_sessions) = try_scan_sessions_sqlite() else {
        if get_opencode_db_path().is_file() {
            super::utils::scan_warning("SQLite 会话索引读取失败，当前列表可能不完整");
        }
        return json_sessions;
    };

    // Current OpenCode versions use SQLite as the native session list. Copies
    // left in the legacy JSON storage are recoverable leftovers.
    let mut merged = sqlite_sessions;
    for mut session in json_sessions.drain(..) {
        // Even when the ID also exists in SQLite, the JSON record is a separate
        // legacy copy that OpenCode no longer uses for its current list.
        session.residual = true;
        session.resume_command = None;
        merged.push(session);
    }
    merged
}

fn scan_sessions_json() -> Vec<SessionMeta> {
    let storage = get_opencode_data_dir();
    let session_dir = storage.join("session");
    if !session_dir.exists() {
        return Vec::new();
    }

    let mut json_files = Vec::new();
    collect_json_files(&session_dir, &mut json_files);

    let mut sessions = Vec::new();
    for path in json_files {
        if let Some(meta) = parse_session(&storage, &path) {
            sessions.push(meta);
        } else {
            super::utils::scan_warning(format!("无法读取会话元数据 {}", path.display()));
        }
    }
    sessions
}

/// Parse a SQLite source reference in the format `sqlite:<db_path>:<session_id>`.
///
/// Uses `rfind(":ses_")` to split the path from the session ID because the
/// db path itself may contain colons (e.g. `C:\Users\...` on Windows).
/// This relies on the OpenCode convention that session IDs start with `ses_`.
fn parse_sqlite_source(source: &str) -> Option<(PathBuf, String)> {
    let rest = source.strip_prefix("sqlite:")?;
    let sep = rest.rfind(":ses_")?;
    let db_path = PathBuf::from(&rest[..sep]);
    let session_id = rest[sep + 1..].to_string();
    Some((db_path, session_id))
}

#[cfg(test)]
fn scan_sessions_sqlite() -> Vec<SessionMeta> {
    try_scan_sessions_sqlite().unwrap_or_default()
}

fn try_scan_sessions_sqlite() -> Option<Vec<SessionMeta>> {
    let db_path = get_opencode_db_path();
    if !db_path.is_file() {
        return None;
    }

    let conn = Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;

    conn.busy_timeout(Duration::from_secs(2)).ok()?;
    let mut stmt = conn
        .prepare(
            "SELECT id, title, directory, time_created, time_updated FROM session ORDER BY time_updated DESC",
        )
        .ok()?;

    let db_display = db_path.display().to_string();

    let iter = stmt
        .query_map([], |row| {
            let session_id: String = row.get(0)?;
            let title: String = row.get(1)?;
            let directory: String = row.get(2)?;
            let created: i64 = row.get(3)?;
            let updated: i64 = row.get(4)?;
            Ok((session_id, title, directory, created, updated))
        })
        .ok()?;

    let mut sessions = Vec::new();
    for row in iter {
        let row = match row {
            Ok(row) => row,
            Err(error) => {
                super::utils::scan_warning(error.to_string());
                continue;
            }
        };
        let (session_id, title, directory, created, updated) = row;
        let display_title = if title.is_empty() {
            path_basename(&directory)
        } else {
            Some(title)
        };
        sessions.push(SessionMeta {
            provider_id: PROVIDER_ID.to_string(),
            session_id: session_id.clone(),
            residual: false,
            archived: false,
            cleanup_pending: false,
            title: display_title.clone(),
            summary: display_title,
            project_dir: if directory.is_empty() {
                None
            } else {
                Some(directory)
            },
            project_name: None,
            created_at: Some(created),
            last_active_at: Some(updated),
            source_path: Some(format!("sqlite:{db_display}:{session_id}")),
            resume_command: is_safe_session_id(&session_id)
                .then(|| format!("opencode -s {session_id}")),
        });
    }
    Some(sessions)
}

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    // `path` is the message directory: storage/message/{sessionID}/
    if !path.is_dir() {
        return Err(format!("Message directory not found: {}", path.display()));
    }

    let storage = path
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| "Cannot determine storage root from message path".to_string())?;
    checked_storage_child(storage, path)?;
    let mut remaining_bytes = super::utils::MAX_QUERY_BYTES as u64;

    let mut msg_files = Vec::new();
    collect_json_files(path, &mut msg_files);

    // Parse all messages and collect (created_ts, message_id, role, parts_text)
    let mut entries: Vec<(i64, String, String, String)> = Vec::new();

    for msg_path in &msg_files {
        let value = read_json_bounded(msg_path, &mut remaining_bytes)?;

        let msg_id = match value.get("id").and_then(Value::as_str) {
            Some(id) if is_safe_session_id(id) => id.to_string(),
            _ => return Err("Invalid OpenCode message ID".into()),
        };

        let role = value
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();

        let created_ts = value
            .get("time")
            .and_then(|t| t.get("created"))
            .and_then(parse_timestamp_to_ms)
            .unwrap_or(0);

        // Collect text parts from storage/part/{messageID}/
        let part_dir = checked_storage_child(storage, &storage.join("part").join(&msg_id))?;
        let text = collect_parts_text(&part_dir, &mut remaining_bytes)?;
        if text.trim().is_empty() {
            continue;
        }

        entries.push((created_ts, msg_id, role, text));
    }

    // Sort by created timestamp
    entries.sort_by_key(|(ts, _, _, _)| *ts);

    let messages = entries
        .into_iter()
        .map(|(ts, _, role, content)| SessionMessage {
            role,
            content,
            ts: if ts > 0 { Some(ts) } else { None },
        })
        .collect();

    Ok(messages)
}

/// Load messages from the OpenCode SQLite database for a given source reference.
/// Joins the `message` and `part` tables in memory to reconstruct full messages.
pub fn load_messages_sqlite(source: &str) -> Result<Vec<SessionMessage>, String> {
    let (db_path, session_id) = parse_sqlite_source(source)
        .ok_or_else(|| format!("Invalid SQLite source reference: {source}"))?;
    if db_path.canonicalize().map_err(|e| e.to_string())?
        != get_opencode_db_path()
            .canonicalize()
            .map_err(|e| e.to_string())?
    {
        return Err("OpenCode database is outside configured storage".into());
    }

    let conn = Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("Failed to open OpenCode database: {e}"))?;

    conn.busy_timeout(Duration::from_secs(2))
        .map_err(|e| e.to_string())?;
    conn.execute_batch("BEGIN DEFERRED")
        .map_err(|e| e.to_string())?;
    check_sqlite_message_budget(&conn, &session_id)?;

    let mut msg_stmt = conn
        .prepare(
            "SELECT id, time_created, data FROM message WHERE session_id = ?1 ORDER BY time_created ASC",
        )
        .map_err(|e| format!("Failed to prepare message query: {e}"))?;

    let msg_rows = msg_stmt
        .query_map([session_id.as_str()], |row| {
            let id: String = row.get(0)?;
            let ts: i64 = row.get(1)?;
            let data: String = row.get(2)?;
            Ok((id, ts, data))
        })
        .map_err(|e| format!("Failed to query messages: {e}"))?;

    let mut part_stmt = conn
        .prepare(
            "SELECT message_id, data FROM part WHERE session_id = ?1 ORDER BY time_created ASC",
        )
        .map_err(|e| format!("Failed to prepare part query: {e}"))?;

    let part_rows = part_stmt
        .query_map([session_id.as_str()], |row| {
            let message_id: String = row.get(0)?;
            let data: String = row.get(1)?;
            Ok((message_id, data))
        })
        .map_err(|e| format!("Failed to query parts: {e}"))?;

    let mut parts_map: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for part in part_rows {
        let (message_id, data) = part.map_err(|e| e.to_string())?;
        parts_map.entry(message_id).or_default().push(data);
    }

    let mut messages = Vec::new();
    for row in msg_rows {
        let (msg_id, ts, data) = row.map_err(|e| e.to_string())?;
        let msg_value: Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let role = msg_value
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();

        let mut texts = Vec::new();
        if let Some(parts) = parts_map.get(&msg_id) {
            for part_data in parts {
                let part_value: Value = match serde_json::from_str(part_data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(text) = extract_part_text(&part_value) {
                    texts.push(text);
                }
            }
        }

        let content = texts.join("\n");
        if content.trim().is_empty() {
            continue;
        }

        messages.push(SessionMessage {
            role,
            content,
            ts: Some(ts),
        });
    }

    Ok(messages)
}

pub fn delete_session(storage: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    if !is_safe_session_id(session_id) {
        return Err("Invalid OpenCode session ID".into());
    }
    let expected = checked_storage_child(storage, &storage.join("message").join(session_id))?;
    let actual = checked_storage_child(storage, path)?;
    if actual != expected {
        return Err("OpenCode source is not the selected session message directory".into());
    }
    if path.file_name().and_then(|name| name.to_str()) != Some(session_id) {
        return Err(format!(
            "OpenCode session path does not match session ID: expected {session_id}, found {}",
            path.display()
        ));
    }

    let mut message_files = Vec::new();
    collect_json_files(path, &mut message_files);

    let mut message_ids = Vec::new();
    for message_path in &message_files {
        let data = std::fs::read_to_string(message_path).map_err(|e| e.to_string())?;
        let value: Value = serde_json::from_str(&data).map_err(|e| e.to_string())?;
        if let Some(message_id) = value.get("id").and_then(Value::as_str) {
            if !is_safe_session_id(message_id) {
                return Err("Invalid OpenCode message ID; no files deleted".into());
            }
            message_ids.push(message_id.to_string());
        }
    }

    let part_dirs = message_ids
        .iter()
        .map(|id| checked_storage_child(storage, &storage.join("part").join(id)))
        .collect::<Result<Vec<_>, _>>()?;
    let session_diff_path = checked_storage_child(
        storage,
        &storage
            .join("session_diff")
            .join(format!("{session_id}.json")),
    )?;
    let session_file = find_session_file(storage, session_id)
        .map(|path| checked_storage_child(storage, &path))
        .transpose()?;
    // Complete validation before the first destructive operation.
    for part_dir in &part_dirs {
        remove_dir_all_if_exists(&part_dir).map_err(|e| {
            format!(
                "Failed to delete OpenCode part directory {}: {e}",
                part_dir.display()
            )
        })?;
    }

    remove_file_if_exists(&session_diff_path).map_err(|e| {
        format!(
            "Failed to delete OpenCode session diff {}: {e}",
            session_diff_path.display()
        )
    })?;

    remove_dir_all_if_exists(path).map_err(|e| {
        format!(
            "Failed to delete OpenCode message directory {}: {e}",
            path.display()
        )
    })?;

    if let Some(session_file) = session_file {
        remove_file_if_exists(&session_file).map_err(|e| {
            format!(
                "Failed to delete OpenCode session file {}: {e}",
                session_file.display()
            )
        })?;
    }

    Ok(true)
}

/// Delete a session from the OpenCode SQLite database.
pub fn delete_session_sqlite(session_id: &str, source: &str) -> Result<bool, String> {
    let (db_path, ref_session_id) = parse_sqlite_source(source)
        .ok_or_else(|| format!("Invalid SQLite source reference: {source}"))?;
    let db_path = db_path
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize SQLite database path: {e}"))?;
    let expected_db_path = get_opencode_db_path()
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize expected OpenCode database path: {e}"))?;

    if ref_session_id != session_id {
        return Err(format!(
            "OpenCode SQLite session ID mismatch: expected {session_id}, found {ref_session_id}"
        ));
    }
    if db_path != expected_db_path {
        return Err("SQLite path does not match expected OpenCode database".to_string());
    }

    let mut conn =
        Connection::open(&db_path).map_err(|e| format!("Failed to open OpenCode database: {e}"))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| format!("Failed to configure OpenCode database timeout: {e}"))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| format!("Failed to enable OpenCode foreign keys: {e}"))?;

    let tx = conn
        .transaction()
        .map_err(|e| format!("Failed to begin transaction: {e}"))?;

    let mut child_tables = Vec::new();
    for table in sqlite_table_names(&tx)? {
        if table != "session" && sqlite_table_has_column(&tx, &table, "session_id")? {
            child_tables.push(table);
        }
    }
    // message can be referenced by part-like tables, so delete it after all
    // other session-scoped tables. The final session delete remains last.
    child_tables.sort_by_key(|table| (table == "message") as u8);
    for table in child_tables {
        let quoted = quote_sqlite_identifier(&table);
        tx.execute(
            &format!("DELETE FROM {quoted} WHERE session_id = ?1"),
            [session_id],
        )
        .map_err(|e| format!("Failed to delete OpenCode {table} rows: {e}"))?;
    }

    let deleted = tx
        .execute("DELETE FROM session WHERE id = ?1", [session_id])
        .map_err(|e| format!("Failed to delete OpenCode session: {e}"))?;

    tx.commit()
        .map_err(|e| format!("Failed to commit session deletion: {e}"))?;

    Ok(deleted > 0)
}

fn sqlite_table_names(transaction: &Transaction<'_>) -> Result<Vec<String>, String> {
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

fn sqlite_table_has_column(
    transaction: &Transaction<'_>,
    table: &str,
    column: &str,
) -> Result<bool, String> {
    let quoted = quote_sqlite_identifier(table);
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

fn quote_sqlite_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn parse_session(storage: &Path, path: &Path) -> Option<SessionMeta> {
    let value = read_json_bounded(path, &mut (super::utils::MAX_QUERY_BYTES as u64)).ok()?;

    let session_id = value.get("id").and_then(Value::as_str)?.to_string();
    if !is_safe_session_id(&session_id) {
        return None;
    }
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let directory = value
        .get("directory")
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    let created_at = value
        .get("time")
        .and_then(|t| t.get("created"))
        .and_then(parse_timestamp_to_ms);
    let updated_at = value
        .get("time")
        .and_then(|t| t.get("updated"))
        .and_then(parse_timestamp_to_ms);

    // Derive title from directory basename if no explicit title
    let has_title = title.is_some();
    let display_title = title.or_else(|| {
        directory
            .as_deref()
            .and_then(path_basename)
            .map(|s| s.to_string())
    });

    // Build source_path = message directory for this session
    let msg_dir = storage.join("message").join(&session_id);
    checked_storage_child(storage, &msg_dir).ok()?;
    let source_path = msg_dir.to_string_lossy().to_string();

    // Skip expensive I/O if title already available from session JSON
    let summary = if has_title {
        display_title.clone()
    } else {
        get_first_user_summary(storage, &session_id)
    };

    Some(SessionMeta {
        provider_id: PROVIDER_ID.to_string(),
        session_id: session_id.clone(),
        residual: false,
        archived: false,
        cleanup_pending: false,
        title: display_title,
        summary,
        project_dir: directory,
        project_name: None,
        created_at,
        last_active_at: updated_at.or(created_at),
        source_path: Some(source_path),
        resume_command: is_safe_session_id(&session_id)
            .then(|| format!("opencode -s {session_id}")),
    })
}

/// Read the first user message's first text part to use as summary.
fn get_first_user_summary(storage: &Path, session_id: &str) -> Option<String> {
    if !is_safe_session_id(session_id) {
        return None;
    }
    let msg_dir = checked_storage_child(storage, &storage.join("message").join(session_id)).ok()?;
    if !msg_dir.is_dir() {
        return None;
    }

    let mut msg_files = Vec::new();
    collect_json_files(&msg_dir, &mut msg_files);

    // Collect user messages with timestamps for ordering
    let mut user_msgs: Vec<(i64, String)> = Vec::new();
    let mut remaining_bytes = super::utils::MAX_QUERY_BYTES as u64;
    for msg_path in &msg_files {
        let value = read_json_bounded(msg_path, &mut remaining_bytes).ok()?;

        if value.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }

        let msg_id = match value.get("id").and_then(Value::as_str) {
            Some(id) if is_safe_session_id(id) => id.to_string(),
            _ => return None,
        };

        let ts = value
            .get("time")
            .and_then(|t| t.get("created"))
            .and_then(parse_timestamp_to_ms)
            .unwrap_or(0);

        user_msgs.push((ts, msg_id));
    }

    user_msgs.sort_by_key(|(ts, _)| *ts);

    // Take first user message and get its parts
    let (_, first_id) = user_msgs.first()?;
    let part_dir = checked_storage_child(storage, &storage.join("part").join(first_id)).ok()?;
    let text = collect_parts_text(&part_dir, &mut remaining_bytes).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    Some(truncate_summary(&text, 160))
}

/// Collect text content from all parts in a part directory.
fn extract_part_text(part_value: &Value) -> Option<String> {
    match part_value.get("type").and_then(Value::as_str) {
        Some("text") => part_value
            .get("text")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .map(|t| t.to_string()),
        Some("tool") => {
            let tool = part_value
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            Some(format!("[Tool: {tool}]"))
        }
        _ => None,
    }
}

fn collect_parts_text(part_dir: &Path, remaining_bytes: &mut u64) -> Result<String, String> {
    if !part_dir.is_dir() {
        return Ok(String::new());
    }

    let mut parts = Vec::new();
    collect_json_files(part_dir, &mut parts);

    let mut texts = Vec::new();
    for part_path in &parts {
        let value = read_json_bounded(part_path, remaining_bytes)?;

        if let Some(text) = extract_part_text(&value) {
            texts.push(text);
        }
    }

    Ok(texts.join("\n"))
}

fn read_json_bounded(path: &Path, remaining_bytes: &mut u64) -> Result<Value, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut text = String::new();
    file.take(*remaining_bytes + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if text.len() as u64 > *remaining_bytes {
        return Err("OpenCode JSON 会话超过 32 MiB 读取限制，请使用原 Agent 查看".into());
    }
    *remaining_bytes -= text.len() as u64;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn collect_json_files(root: &Path, files: &mut Vec<PathBuf>) {
    collect_files_where(
        root,
        |path| path.extension().and_then(|ext| ext.to_str()) == Some("json"),
        files,
    );
}

fn find_session_file(storage: &Path, session_id: &str) -> Option<PathBuf> {
    let session_root = storage.join("session");
    let mut files = Vec::new();
    collect_json_files(&session_root, &mut files);
    let expected = format!("{session_id}.json");

    files
        .into_iter()
        .find(|path| path.file_name().and_then(|name| name.to_str()) == Some(expected.as_str()))
}

fn remove_file_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn remove_dir_all_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_message_id_is_rejected_before_any_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let source = storage.join("message").join("ses_1");
        std::fs::create_dir_all(&source).unwrap();
        let victim = temp.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        let sentinel = victim.join("keep.txt");
        std::fs::write(&sentinel, "valuable").unwrap();
        std::fs::write(source.join("msg_1.json"), r#"{"id":"../../../victim"}"#).unwrap();
        assert!(load_messages(&source).is_err());
        assert!(delete_session(&storage, &source, "ses_1").is_err());
        assert!(source.join("msg_1.json").exists());
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "valuable");
    }

    #[test]
    fn json_reads_share_a_finite_byte_budget() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("part.json");
        std::fs::write(&path, r#"{"text":"hello"}"#).unwrap();
        let mut small_budget = 4;
        assert!(read_json_bounded(&path, &mut small_budget).is_err());
        let mut budget = 20;
        assert!(read_json_bounded(&path, &mut budget).is_ok());
        assert!(budget < 20);
        assert!(read_json_bounded(&path, &mut budget).is_err());
    }
    use rusqlite::Connection;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;

    fn opencode_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn create_sqlite_schema(conn: &Connection) {
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                directory TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                data TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE
            );
            CREATE TABLE part (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                message_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                data TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE,
                FOREIGN KEY(message_id) REFERENCES message(id) ON DELETE CASCADE
            );
            CREATE TABLE todo (
                session_id TEXT NOT NULL,
                position INTEGER NOT NULL,
                content TEXT NOT NULL,
                PRIMARY KEY(session_id, position),
                FOREIGN KEY(session_id) REFERENCES session(id) ON DELETE CASCADE
            );
            CREATE TABLE future_session_cache (
                session_id TEXT PRIMARY KEY,
                payload TEXT NOT NULL
            );
            ",
        )
        .expect("create sqlite schema");
    }

    #[test]
    #[allow(deprecated)] // set_var/remove_var deprecated since Rust 1.81; serialized by mutex
    fn legacy_json_session_is_residual_when_missing_from_native_sqlite_list() {
        let _guard = opencode_env_lock().lock().expect("lock");
        let temp = tempdir().expect("tempdir");
        let original_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", temp.path());

        let base_dir = temp.path().join("opencode");
        let session_dir = base_dir.join("storage/session/project");
        std::fs::create_dir_all(&session_dir).expect("create legacy session dir");
        std::fs::write(
            session_dir.join("ses_legacy.json"),
            r#"{"id":"ses_legacy","title":"Legacy session","directory":"/tmp/project","time":{"created":1,"updated":2}}"#,
        )
        .expect("write legacy session");
        let database = Connection::open(base_dir.join("opencode.db")).expect("open database");
        create_sqlite_schema(&database);
        drop(database);

        let sessions = scan_sessions();

        if let Some(value) = original_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "ses_legacy");
        assert!(sessions[0].residual);
        assert!(sessions[0].resume_command.is_none());
    }

    #[test]
    fn delete_session_removes_session_diff_messages_and_parts() {
        let temp = tempdir().expect("tempdir");
        let storage = temp.path();
        let project_id = "project-123";
        let session_id = "ses_123";
        let session_dir = storage.join("session").join(project_id);
        let message_dir = storage.join("message").join(session_id);
        let session_diff = storage
            .join("session_diff")
            .join(format!("{session_id}.json"));
        let part_dir = storage.join("part").join("msg_1");
        let session_file = session_dir.join(format!("{session_id}.json"));

        std::fs::create_dir_all(&session_dir).expect("create session dir");
        std::fs::create_dir_all(&message_dir).expect("create message dir");
        std::fs::create_dir_all(&part_dir).expect("create part dir");
        std::fs::create_dir_all(storage.join("project")).expect("create project dir");
        std::fs::create_dir_all(storage.join("session_diff")).expect("create session diff dir");

        std::fs::write(
            &session_file,
            format!(
                r#"{{
                  "id": "{session_id}",
                  "projectID": "{project_id}",
                  "directory": "/tmp/project",
                  "time": {{ "created": 1, "updated": 2 }}
                }}"#
            ),
        )
        .expect("write session file");
        std::fs::write(
            message_dir.join("msg_1.json"),
            format!(r#"{{"id":"msg_1","sessionID":"{session_id}","role":"user"}}"#),
        )
        .expect("write message file");
        std::fs::write(
            part_dir.join("prt_1.json"),
            r#"{"id":"prt_1","messageID":"msg_1"}"#,
        )
        .expect("write part file");
        std::fs::write(&session_diff, "[]").expect("write session diff");
        std::fs::write(
            storage.join("project").join(format!("{project_id}.json")),
            r#"{"id":"project-123"}"#,
        )
        .expect("write project file");

        delete_session(storage, &message_dir, session_id).expect("delete session");

        assert!(!session_file.exists());
        assert!(!message_dir.exists());
        assert!(!session_diff.exists());
        assert!(!part_dir.exists());
        assert!(storage
            .join("project")
            .join(format!("{project_id}.json"))
            .exists());
    }

    #[test]
    fn load_messages_includes_tool_parts() {
        let temp = tempdir().expect("tempdir");
        let storage = temp.path();
        let session_id = "ses_test";
        let msg_id = "msg_1";

        let msg_dir = storage.join("message").join(session_id);
        let part_dir = storage.join("part").join(msg_id);
        std::fs::create_dir_all(&msg_dir).expect("create msg dir");
        std::fs::create_dir_all(&part_dir).expect("create part dir");

        std::fs::write(
            msg_dir.join(format!("{msg_id}.json")),
            r#"{"id":"msg_1","role":"assistant","time":{"created":"2026-03-06T10:00:00Z"}}"#,
        )
        .expect("write msg");

        std::fs::write(
            part_dir.join("prt_1.json"),
            r#"{"id":"prt_1","type":"tool","tool":"bash","state":{"status":"completed","input":{"command":"ls"},"output":"file.txt"}}"#,
        )
        .expect("write tool part");

        std::fs::write(
            part_dir.join("prt_2.json"),
            r#"{"id":"prt_2","type":"text","text":"Here are the files."}"#,
        )
        .expect("write text part");

        let msgs = load_messages(&msg_dir).expect("load");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "assistant");
        assert!(msgs[0].content.contains("[Tool: bash]"));
        assert!(msgs[0].content.contains("Here are the files."));
    }

    #[test]
    fn parse_sqlite_source_accepts_valid_references() {
        let parsed = parse_sqlite_source("sqlite:/tmp/opencode.db:ses_123").expect("valid source");

        assert_eq!(parsed.0, PathBuf::from("/tmp/opencode.db"));
        assert_eq!(parsed.1, "ses_123");
    }

    #[test]
    fn parse_sqlite_source_rejects_invalid_references() {
        assert!(parse_sqlite_source("/tmp/opencode.db:ses_123").is_none());
        assert!(parse_sqlite_source("sqlite:/tmp/opencode.db:msg_123").is_none());
        assert!(parse_sqlite_source("sqlite:/tmp/opencode.db").is_none());
    }

    #[test]
    #[allow(deprecated)] // set_var/remove_var deprecated since Rust 1.81; safe here under mutex
    fn scan_sessions_sqlite_reads_temp_database() {
        let _guard = opencode_env_lock().lock().expect("lock");
        let temp = tempdir().expect("tempdir");
        let original_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", temp.path());

        let base_dir = temp.path().join("opencode");
        std::fs::create_dir_all(&base_dir).expect("create base dir");
        let db_path = base_dir.join("opencode.db");
        let conn = Connection::open(&db_path).expect("open sqlite db");
        create_sqlite_schema(&conn);

        conn.execute(
            "INSERT INTO session (id, title, directory, time_created, time_updated) VALUES (?1, ?2, ?3, ?4, ?5)",
            ("ses_1", "", "/tmp/project-a", 1_771_061_953_033_i64, 1_771_061_954_033_i64),
        )
        .expect("insert session 1");
        conn.execute(
            "INSERT INTO session (id, title, directory, time_created, time_updated) VALUES (?1, ?2, ?3, ?4, ?5)",
            ("ses_2", "Named Session", "/tmp/project-b", 1_771_061_950_000_i64, 1_771_061_955_000_i64),
        )
        .expect("insert session 2");
        drop(conn);

        let sessions = scan_sessions_sqlite();

        #[allow(deprecated)]
        if let Some(value) = original_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "ses_2");
        assert_eq!(sessions[0].title.as_deref(), Some("Named Session"));
        assert_eq!(sessions[1].session_id, "ses_1");
        assert_eq!(sessions[1].title.as_deref(), Some("project-a"));
        assert_eq!(sessions[1].project_dir.as_deref(), Some("/tmp/project-a"));
        let expected_source = format!("sqlite:{}:ses_1", db_path.display());
        assert_eq!(
            sessions[1].source_path.as_deref(),
            Some(expected_source.as_str())
        );
        assert_eq!(
            sessions[1].resume_command.as_deref(),
            Some("opencode -s ses_1")
        );
    }

    #[test]
    fn load_messages_sqlite_reads_messages_and_parts() {
        let _env_guard = opencode_env_lock().lock().expect("env lock");
        let temp = tempdir().expect("tempdir");
        let original_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", temp.path());
        std::fs::create_dir(temp.path().join("opencode")).unwrap();
        let db_path = get_opencode_db_path();
        let conn = Connection::open(&db_path).expect("open sqlite db");
        create_sqlite_schema(&conn);

        conn.execute(
            "INSERT INTO session (id, title, directory, time_created, time_updated) VALUES (?1, ?2, ?3, ?4, ?5)",
            ("ses_1", "Session", "/tmp/project-a", 1000_i64, 3000_i64),
        )
        .expect("insert session");
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4)",
            ("msg_1", "ses_1", 1000_i64, r#"{"role":"user"}"#),
        )
        .expect("insert message 1");
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4)",
            ("msg_2", "ses_1", 2000_i64, r#"{"role":"assistant"}"#),
        )
        .expect("insert message 2");
        conn.execute(
            "INSERT INTO part (id, session_id, message_id, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            ("prt_1", "ses_1", "msg_1", 1000_i64, r#"{"type":"text","text":"Hello"}"#),
        )
        .expect("insert part 1");
        conn.execute(
            "INSERT INTO part (id, session_id, message_id, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                "prt_2",
                "ses_1",
                "msg_2",
                2000_i64,
                r#"{"type":"tool","tool":"bash"}"#,
            ),
        )
        .expect("insert part 2");
        conn.execute(
            "INSERT INTO part (id, session_id, message_id, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                "prt_3",
                "ses_1",
                "msg_2",
                2001_i64,
                r#"{"type":"text","text":"Done"}"#,
            ),
        )
        .expect("insert part 3");
        drop(conn);

        let source = format!("sqlite:{}:ses_1", db_path.display());
        let messages = load_messages_sqlite(&source).expect("load sqlite messages");

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "Hello");
        assert_eq!(messages[0].ts, Some(1000));
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].content, "[Tool: bash]\nDone");
        assert_eq!(messages[1].ts, Some(2000));
        if let Some(value) = original_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }

    #[test]
    fn delete_session_sqlite_removes_session() {
        let _guard = opencode_env_lock().lock().expect("lock");
        let temp = tempdir().expect("tempdir");
        let original_xdg = std::env::var_os("XDG_DATA_HOME");
        #[allow(deprecated)]
        std::env::set_var("XDG_DATA_HOME", temp.path());

        let base_dir = temp.path().join("opencode");
        std::fs::create_dir_all(&base_dir).expect("create base dir");
        let db_path = base_dir.join("opencode.db");
        let conn = Connection::open(&db_path).expect("open sqlite db");
        create_sqlite_schema(&conn);

        conn.execute(
            "INSERT INTO session (id, title, directory, time_created, time_updated) VALUES (?1, ?2, ?3, ?4, ?5)",
            ("ses_1", "Session", "/tmp/project-a", 1000_i64, 3000_i64),
        )
        .expect("insert session");
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4)",
            ("msg_1", "ses_1", 1000_i64, r#"{"role":"user"}"#),
        )
        .expect("insert message");
        conn.execute(
            "INSERT INTO part (id, session_id, message_id, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            ("prt_1", "ses_1", "msg_1", 1000_i64, r#"{"type":"text","text":"Hello"}"#),
        )
        .expect("insert part");
        conn.execute(
            "INSERT INTO todo (session_id, position, content) VALUES ('ses_1', 0, 'finish')",
            [],
        )
        .expect("insert todo");
        conn.execute(
            "INSERT INTO future_session_cache (session_id, payload) VALUES ('ses_1', '{}')",
            [],
        )
        .expect("insert future session cache");
        drop(conn);

        let source = format!("sqlite:{}:ses_1", db_path.display());
        let deleted = delete_session_sqlite("ses_1", &source).expect("delete sqlite session");
        assert!(deleted);

        let conn = Connection::open(&db_path).expect("re-open sqlite db");
        let remaining_sessions: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session WHERE id = 'ses_1'",
                [],
                |row| row.get(0),
            )
            .expect("count sessions");
        let remaining_messages: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM message WHERE session_id = 'ses_1'",
                [],
                |row| row.get(0),
            )
            .expect("count messages");
        let remaining_parts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM part WHERE session_id = 'ses_1'",
                [],
                |row| row.get(0),
            )
            .expect("count parts");
        let remaining_todos: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM todo WHERE session_id = 'ses_1'",
                [],
                |row| row.get(0),
            )
            .expect("count todos");
        let remaining_future_cache: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM future_session_cache WHERE session_id = 'ses_1'",
                [],
                |row| row.get(0),
            )
            .expect("count future cache");

        assert_eq!(remaining_sessions, 0);
        assert_eq!(remaining_messages, 0);
        assert_eq!(remaining_parts, 0);
        assert_eq!(remaining_todos, 0);
        assert_eq!(remaining_future_cache, 0);

        #[allow(deprecated)]
        if let Some(value) = original_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }

    #[test]
    fn delete_session_sqlite_rejects_foreign_db_path() {
        let _guard = opencode_env_lock().lock().expect("lock");
        let temp = tempdir().expect("tempdir");
        let original_xdg = std::env::var_os("XDG_DATA_HOME");
        #[allow(deprecated)]
        std::env::set_var("XDG_DATA_HOME", temp.path());

        let expected_base_dir = temp.path().join("opencode");
        std::fs::create_dir_all(&expected_base_dir).expect("create expected base dir");
        let expected_db_path = expected_base_dir.join("opencode.db");
        Connection::open(&expected_db_path).expect("create expected sqlite db");

        let db_path = temp.path().join("foreign.db");
        let conn = Connection::open(&db_path).expect("open sqlite db");
        create_sqlite_schema(&conn);
        conn.execute(
            "INSERT INTO session (id, title, directory, time_created, time_updated) VALUES (?1, ?2, ?3, ?4, ?5)",
            ("ses_1", "Session", "/tmp/project", 1000_i64, 3000_i64),
        )
        .expect("insert session");
        drop(conn);

        let source = format!("sqlite:{}:ses_1", db_path.display());
        let err = delete_session_sqlite("ses_1", &source).expect_err("should reject foreign db");
        assert!(err.contains("expected OpenCode database"));

        #[allow(deprecated)]
        if let Some(value) = original_xdg {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }
}
