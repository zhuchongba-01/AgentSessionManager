use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use serde_json::Value;

use crate::session_manager::cache;
use crate::session_manager::content::parse_file_url;
use crate::session_manager::model::{
    EventKind, ImageRef, ImageSource, MessageMeta, SessionBlock, ToolStatus,
};
use crate::session_manager::{SessionMessage, SessionMeta};
use crate::session_paths::workbuddy_homes;

use super::blocks::{
    assign_turn_ids, estimate_base64_size, large_text_block, parse_arguments, summary_event_block,
    thinking_block, tool_call_block, tool_result_block, ToolSource,
};
use super::utils::{
    atomic_write_json, checked_storage_child, extract_text, for_each_jsonl_value,
    is_reparse_or_symlink, is_safe_session_id, parse_timestamp_to_ms, path_basename,
    read_head_tail_lines, remove_path_if_exists, scan_warning, sqlite_table_has_column,
    sqlite_table_names, truncate_summary, FileParseCache, JsonlSpan, TITLE_MAX_CHARS,
};

const PROVIDER_ID: &str = "workbuddy";
const SOURCE_PREFIX: &str = "sqlite-workbuddy:";
const DB_FILENAME: &str = "workbuddy.db";
const HEAD_SCAN_LINES: usize = 80;
const TAIL_SCAN_LINES: usize = 30;
const MAX_SNAPSHOT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LOCKFILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
struct ParsedJsonlMeta {
    session_id: String,
    source_path: String,
    cwd: Option<String>,
    ai_title: Option<String>,
    meta_title: Option<String>,
    first_user_prompt: Option<String>,
    summary: Option<String>,
    created_at: Option<i64>,
    last_active_at: Option<i64>,
}

#[derive(Debug, Clone, Default)]
struct DbSessionRow {
    cwd: String,
    title: String,
    custom_title: String,
    group_title: String,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    last_activity_at: Option<i64>,
    is_deleted: bool,
    transport: String,
}

static PARSE_CACHE: LazyLock<FileParseCache<ParsedJsonlMeta>> = LazyLock::new(FileParseCache::new);

pub fn session_roots() -> Vec<PathBuf> {
    workbuddy_homes()
}

pub fn source_reference(db_path: &Path, session_id: &str) -> String {
    format!("{SOURCE_PREFIX}{}:{session_id}", db_path.display())
}

pub fn parse_source(source: &str) -> Option<(PathBuf, String)> {
    let rest = source.strip_prefix(SOURCE_PREFIX)?;
    let (path, session_id) = rest.rsplit_once(':')?;
    if path.is_empty()
        || session_id.is_empty()
        || (path.len() == 1 && path.as_bytes()[0].is_ascii_alphabetic())
    {
        return None;
    }
    Some((PathBuf::from(path), session_id.to_string()))
}

fn is_valid_workbuddy_session_id(id: &str) -> bool {
    is_safe_session_id(id)
        && !id.starts_with("agent-")
        && !id.starts_with("editor-sdk-")
        && !matches!(id, "sessions-index" | "memory" | "shared" | "temp" | "tmp")
}

fn is_uuid_like(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (idx, &b) in bytes.iter().enumerate() {
        if matches!(idx, 8 | 13 | 18 | 23) {
            if b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

fn normalize_db_timestamp(raw: i64) -> Option<i64> {
    if raw <= 0 {
        return None;
    }
    if raw > 1_000_000_000_000 {
        Some(raw)
    } else {
        raw.checked_mul(1000)
    }
}

pub fn scan_sessions() -> Vec<SessionMeta> {
    let homes = session_roots();
    scan_sessions_in_homes(&homes, &PARSE_CACHE)
}

fn scan_sessions_in_homes(
    homes: &[PathBuf],
    cache: &FileParseCache<ParsedJsonlMeta>,
) -> Vec<SessionMeta> {
    let mut seen_homes = HashSet::new();
    let mut all = Vec::new();
    for home in homes {
        if !home.exists() {
            continue;
        }
        if let Ok(canonical) = home.canonicalize() {
            if !seen_homes.insert(canonical) {
                continue;
            }
        }
        all.extend(scan_sessions_in_home(home, cache));
    }
    all
}

fn scan_sessions_in_home(home: &Path, cache: &FileParseCache<ParsedJsonlMeta>) -> Vec<SessionMeta> {
    let projects_dir = home.join("projects");
    let mut files = Vec::new();
    collect_project_jsonl_files(&projects_dir, &mut files);
    let parsed_files = cache.scan(files, parse_jsonl_meta);

    let mut jsonl_by_id: HashMap<String, Vec<ParsedJsonlMeta>> = HashMap::new();
    for item in parsed_files {
        jsonl_by_id
            .entry(item.session_id.clone())
            .or_default()
            .push(item);
    }

    let db_path = home.join(DB_FILENAME);
    let (db_rows, db_available) = if db_path.is_file() {
        match load_db_sessions(&db_path) {
            Ok(rows) => (rows, true),
            Err(error) => {
                scan_warning(format!(
                    "无法读取 WorkBuddy 数据库 {}：{error}",
                    db_path.display()
                ));
                (HashMap::new(), false)
            }
        }
    } else {
        (HashMap::new(), false)
    };

    let mut sessions = Vec::new();
    let mut emitted_ids = HashSet::new();

    for (session_id, mut copies) in jsonl_by_id {
        emitted_ids.insert(session_id.clone());
        let db_row = db_rows.get(&session_id);
        let db_cwd = db_row.map(|r| r.cwd.trim()).unwrap_or("");

        copies.sort_by(|a, b| {
            let a_cwd_match = !db_cwd.is_empty() && a.cwd.as_deref() == Some(db_cwd);
            let b_cwd_match = !db_cwd.is_empty() && b.cwd.as_deref() == Some(db_cwd);
            b_cwd_match
                .cmp(&a_cwd_match)
                .then_with(|| {
                    let a_ts = a.last_active_at.or(a.created_at).unwrap_or(0);
                    let b_ts = b.last_active_at.or(b.created_at).unwrap_or(0);
                    b_ts.cmp(&a_ts)
                })
                .then_with(|| a.source_path.cmp(&b.source_path))
        });

        for (idx, copy) in copies.into_iter().enumerate() {
            let residual = if db_available {
                match db_row {
                    Some(row) if !row.is_deleted => idx > 0,
                    _ => true,
                }
            } else {
                idx > 0
            };

            let project_dir = db_row
                .map(|r| r.cwd.trim())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| copy.cwd.filter(|s| !s.trim().is_empty()));

            let raw_title = first_non_empty([
                db_row.map(|r| r.custom_title.as_str()),
                db_row.map(|r| r.title.as_str()),
                copy.ai_title.as_deref(),
                copy.meta_title.as_deref(),
                copy.first_user_prompt.as_deref(),
                db_row.map(|r| r.group_title.as_str()),
            ])
            .map(str::to_string)
            .or_else(|| project_dir.as_deref().and_then(path_basename));

            let title = raw_title
                .as_deref()
                .map(|t| truncate_summary(t, TITLE_MAX_CHARS))
                .filter(|t| !t.is_empty());
            let summary = copy.summary.or_else(|| {
                raw_title
                    .as_deref()
                    .map(|t| truncate_summary(t, 160))
                    .filter(|t| !t.is_empty())
            });

            let created_at = db_row.and_then(|r| r.created_at).or(copy.created_at);
            let last_active_at = [
                db_row.and_then(|r| r.last_activity_at),
                db_row.and_then(|r| r.updated_at),
                copy.last_active_at,
                created_at,
            ]
            .into_iter()
            .flatten()
            .max();

            sessions.push(SessionMeta {
                provider_id: PROVIDER_ID.to_string(),
                session_id: session_id.clone(),
                residual,
                parent_session_id: None,
                archived: false,
                cleanup_pending: false,
                title,
                summary,
                project_dir,
                project_name: None,
                sidebar_section: None,
                created_at,
                last_active_at,
                source_path: Some(copy.source_path),
                resume_command: None,
            });
        }
    }

    for (session_id, row) in &db_rows {
        if emitted_ids.contains(session_id) || !is_valid_workbuddy_session_id(session_id) {
            continue;
        }
        emitted_ids.insert(session_id.clone());

        let residual = row.is_deleted || !row.transport.eq_ignore_ascii_case("cloud");
        let project_dir = (!row.cwd.trim().is_empty()).then(|| row.cwd.trim().to_string());
        let raw_title = first_non_empty([
            Some(row.custom_title.as_str()),
            Some(row.title.as_str()),
            Some(row.group_title.as_str()),
        ])
        .map(str::to_string)
        .or_else(|| project_dir.as_deref().and_then(path_basename));

        let title = raw_title
            .as_deref()
            .map(|t| truncate_summary(t, TITLE_MAX_CHARS))
            .filter(|t| !t.is_empty());
        let summary = raw_title
            .as_deref()
            .map(|t| truncate_summary(t, 160))
            .filter(|t| !t.is_empty());
        let created_at = row.created_at;
        let last_active_at = [row.last_activity_at, row.updated_at, row.created_at]
            .into_iter()
            .flatten()
            .max();

        sessions.push(SessionMeta {
            provider_id: PROVIDER_ID.to_string(),
            session_id: session_id.clone(),
            residual,
            parent_session_id: None,
            archived: false,
            cleanup_pending: false,
            title,
            summary,
            project_dir,
            project_name: None,
            sidebar_section: None,
            created_at,
            last_active_at,
            source_path: Some(source_reference(&db_path, session_id)),
            resume_command: None,
        });
    }

    if db_available || !db_path.exists() {
        scan_orphan_workspace_backups(home, &db_path, &emitted_ids, &mut sessions);
    }

    sessions
}

fn scan_orphan_workspace_backups(
    home: &Path,
    db_path: &Path,
    known_ids: &HashSet<String>,
    sessions: &mut Vec<SessionMeta>,
) {
    let ws_sessions = home.join("workspace").join("sessions");
    let entries = match fs::read_dir(&ws_sessions) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return,
        Err(err) => {
            scan_warning(format!("无法扫描 {}：{err}", ws_sessions.display()));
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let path = entry.path();
        if fs::symlink_metadata(&path).is_ok_and(|m| is_reparse_or_symlink(&m)) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !is_uuid_like(&name) || known_ids.contains(&name) {
            continue;
        }
        let modified_ms = fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_millis()).ok());
        let short_id = &name[..8];
        sessions.push(SessionMeta {
            provider_id: PROVIDER_ID.to_string(),
            session_id: name.clone(),
            residual: true,
            parent_session_id: None,
            archived: false,
            cleanup_pending: false,
            title: Some(format!("残留工作区备份 ({short_id})")),
            summary: Some("WorkBuddy 本地文件快照残留（原始会话索引已移除）".to_string()),
            project_dir: None,
            project_name: None,
            sidebar_section: None,
            created_at: modified_ms,
            last_active_at: modified_ms,
            source_path: Some(source_reference(db_path, &name)),
            resume_command: None,
        });
    }
}

fn first_non_empty<'a, I>(candidates: I) -> Option<&'a str>
where
    I: IntoIterator<Item = Option<&'a str>>,
{
    candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
}

fn collect_project_jsonl_files(projects_root: &Path, files: &mut Vec<PathBuf>) {
    let project_entries = match fs::read_dir(projects_root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return,
        Err(err) => {
            scan_warning(format!("无法扫描 {}：{err}", projects_root.display()));
            return;
        }
    };
    for proj_entry in project_entries {
        let proj_entry = match proj_entry {
            Ok(e) => e,
            Err(err) => {
                scan_warning(format!("无法枚举 {}：{err}", projects_root.display()));
                continue;
            }
        };
        let Ok(proj_ft) = proj_entry.file_type() else {
            continue;
        };
        if !proj_ft.is_dir() {
            continue;
        }
        let proj_path = proj_entry.path();
        if fs::symlink_metadata(&proj_path).is_ok_and(|m| is_reparse_or_symlink(&m)) {
            continue;
        }
        let session_entries = match fs::read_dir(&proj_path) {
            Ok(entries) => entries,
            Err(err) => {
                scan_warning(format!("无法扫描 {}：{err}", proj_path.display()));
                continue;
            }
        };
        for ses_entry in session_entries {
            let Ok(ses_entry) = ses_entry else {
                continue;
            };
            let Ok(ses_ft) = ses_entry.file_type() else {
                continue;
            };
            if !ses_ft.is_file() {
                continue;
            }
            let ses_path = ses_entry.path();
            if fs::symlink_metadata(&ses_path).is_ok_and(|m| is_reparse_or_symlink(&m)) {
                continue;
            }
            if ses_path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = ses_path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !is_valid_workbuddy_session_id(stem) {
                continue;
            }
            files.push(ses_path);
        }
    }
}

fn parse_jsonl_meta(path: &Path) -> Result<Option<ParsedJsonlMeta>, String> {
    let Some(session_id) = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| is_valid_workbuddy_session_id(s))
        .map(str::to_string)
    else {
        return Ok(None);
    };

    let (head, tail) = read_head_tail_lines(path, HEAD_SCAN_LINES, TAIL_SCAN_LINES)
        .map_err(|err| format!("Failed to read {}: {err}", path.display()))?;

    let mut cwd: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut meta_title: Option<String> = None;
    let mut first_user_prompt: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut created_at: Option<i64> = None;
    let mut last_active_at: Option<i64> = None;

    for line in head.iter().chain(tail.iter()) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(ts) = val.get("timestamp").and_then(parse_timestamp_to_ms) {
            created_at = Some(created_at.map_or(ts, |prev| prev.min(ts)));
            last_active_at = Some(last_active_at.map_or(ts, |prev| prev.max(ts)));
        }
        if cwd.is_none() {
            if let Some(dir) = val
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                cwd = Some(dir.to_string());
            }
        }
        let entry_type = val.get("type").and_then(Value::as_str).unwrap_or("");
        match entry_type {
            "ai-title" => {
                if let Some(t) = val
                    .get("aiTitle")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    ai_title = Some(t.to_string());
                }
            }
            "session-meta" if meta_title.is_none() => {
                if let Some(t) = val
                    .get("meta")
                    .and_then(|m| {
                        m.get("codebuddy.ai/sessionTitle")
                            .or_else(|| m.get("title"))
                    })
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    meta_title = Some(t.to_string());
                }
            }
            "message" => {
                if is_meta_entry(&val) || is_compaction_summary(&val) {
                    continue;
                }
                let role = val.get("role").and_then(Value::as_str).unwrap_or("");
                let text = extract_workbuddy_message_text(&val);
                let trimmed_text = text.trim();
                if !trimmed_text.is_empty() {
                    if role == "user" && first_user_prompt.is_none() {
                        first_user_prompt = Some(truncate_summary(trimmed_text, TITLE_MAX_CHARS));
                    }
                    summary = Some(truncate_summary(trimmed_text, 160));
                }
            }
            _ => {}
        }
    }

    if created_at.is_none() || last_active_at.is_none() {
        let modified_ms = fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_millis()).ok());
        if created_at.is_none() {
            created_at = modified_ms;
        }
        if last_active_at.is_none() {
            last_active_at = modified_ms;
        }
    }

    Ok(Some(ParsedJsonlMeta {
        session_id,
        source_path: path.to_string_lossy().to_string(),
        cwd,
        ai_title,
        meta_title,
        first_user_prompt,
        summary,
        created_at,
        last_active_at,
    }))
}

fn load_db_sessions(db_path: &Path) -> Result<HashMap<String, DbSessionRow>, String> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(|e| e.to_string())?;

    let tables = sqlite_table_names(&conn)?;
    if !tables.iter().any(|t| t == "sessions") {
        return Ok(HashMap::new());
    }
    if !sqlite_table_has_column(&conn, "sessions", "id")? {
        return Ok(HashMap::new());
    }

    let col_expr = |col: &str, default_sql: &str| -> Result<String, String> {
        if sqlite_table_has_column(&conn, "sessions", col)? {
            Ok(format!("COALESCE({col}, {default_sql})"))
        } else {
            Ok(default_sql.to_string())
        }
    };

    let cwd_sql = col_expr("cwd", "''")?;
    let title_sql = col_expr("title", "''")?;
    let custom_title_sql = col_expr("custom_title", "''")?;
    let group_title_sql = col_expr("group_title", "''")?;
    let created_sql = col_expr("created_at", "0")?;
    let updated_sql = col_expr("updated_at", "0")?;
    let activity_sql = col_expr("last_activity_at", "0")?;
    let deleted_sql = if sqlite_table_has_column(&conn, "sessions", "deleted_at")? {
        "COALESCE(deleted_at, -1) > 0".to_string()
    } else {
        "0".to_string()
    };
    let transport_sql = col_expr("transport", "'local'")?;

    let sql = format!(
        "SELECT id, {cwd_sql}, {title_sql}, {custom_title_sql}, {group_title_sql}, \
         {created_sql}, {updated_sql}, {activity_sql}, {deleted_sql}, {transport_sql} \
         FROM sessions WHERE id IS NOT NULL AND id <> ''"
    );

    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            let id: String = row.get(0)?;
            let cwd: String = row.get(1)?;
            let title: String = row.get(2)?;
            let custom_title: String = row.get(3)?;
            let group_title: String = row.get(4)?;
            let created_raw: i64 = row.get(5)?;
            let updated_raw: i64 = row.get(6)?;
            let activity_raw: i64 = row.get(7)?;
            let is_deleted: bool = row.get(8)?;
            let transport: String = row.get(9)?;
            Ok((
                id,
                DbSessionRow {
                    cwd,
                    title,
                    custom_title,
                    group_title,
                    created_at: normalize_db_timestamp(created_raw),
                    updated_at: normalize_db_timestamp(updated_raw),
                    last_activity_at: normalize_db_timestamp(activity_raw),
                    is_deleted,
                    transport,
                },
            ))
        })
        .map_err(|e| e.to_string())?;

    let mut map = HashMap::new();
    for row in rows {
        let (id, data) = row.map_err(|e| e.to_string())?;
        map.insert(id, data);
    }
    Ok(map)
}

fn is_meta_entry(entry: &Value) -> bool {
    entry
        .pointer("/providerData/isMeta")
        .and_then(Value::as_bool)
        == Some(true)
        || entry.get("isMeta").and_then(Value::as_bool) == Some(true)
}

fn is_compaction_summary(entry: &Value) -> bool {
    let Some(pd) = entry.get("providerData") else {
        return false;
    };
    pd.get("isCompacted").and_then(Value::as_bool) == Some(true)
        || pd.get("isCompactInternal").and_then(Value::as_bool) == Some(true)
        || pd.get("isSummary").and_then(Value::as_bool) == Some(true)
}

fn non_empty_field<'a>(entry: &'a Value, field: &str) -> Option<&'a str> {
    entry
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn is_chain_entry(entry: &Value) -> bool {
    match entry.get("type").and_then(Value::as_str).unwrap_or("") {
        "message"
        | "reasoning"
        | "function_call"
        | "function_call_result"
        | "resend-fork-notice"
        | "compact_boundary" => true,
        "system" => {
            entry.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
                || non_empty_field(entry, "parentId").is_some()
        }
        _ => false,
    }
}

/// Reconstruct the active conversation chain in chronological order while preserving JSONL byte spans.
///
/// WorkBuddy appends in-progress and completed items with the same `(id, parentId, logicalParentId)`,
/// branches edited prompts via `resend-fork-notice`, and links auto-compacted segments back to
/// earlier verbatim turns via `logicalParentId`. In rare cases WorkBuddy also reuses a request `id`
/// with a new `parentId` on a later turn; keeping distinct `(id, parentId, logicalParentId)` nodes
/// and resolving parent references strictly to indices `< current_idx` guarantees cycle-free
/// reconstruction of the full conversation history.
fn reconstruct_chain(entries: &[(JsonlSpan, Value)]) -> Vec<(JsonlSpan, &Value, bool)> {
    let mut nodes: Vec<(JsonlSpan, &Value)> = Vec::new();
    let mut by_id: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut last_chain_idx: Option<usize> = None;

    for (span, entry) in entries {
        let chain_member = is_chain_entry(entry);
        let Some(id) = non_empty_field(entry, "id") else {
            if chain_member {
                let new_idx = nodes.len();
                nodes.push((*span, entry));
                if last_chain_idx.is_none() {
                    last_chain_idx = Some(new_idx);
                }
            }
            continue;
        };
        let parent_id = non_empty_field(entry, "parentId");
        let logical_parent_id = non_empty_field(entry, "logicalParentId");

        if let Some(indices) = by_id.get(id) {
            if let Some(&prev_idx) = indices.last() {
                let (_, prev) = nodes[prev_idx];
                if non_empty_field(prev, "parentId") == parent_id
                    && non_empty_field(prev, "logicalParentId") == logical_parent_id
                {
                    nodes[prev_idx] = (*span, entry);
                    if chain_member {
                        last_chain_idx = Some(prev_idx);
                    }
                    continue;
                }
            }
        }

        let new_idx = nodes.len();
        nodes.push((*span, entry));
        by_id.entry(id).or_default().push(new_idx);
        if chain_member {
            last_chain_idx = Some(new_idx);
        }
    }

    let Some(start_idx) = last_chain_idx else {
        return entries
            .iter()
            .filter(|(_, e)| is_chain_entry(e))
            .map(|(span, e)| (*span, e, false))
            .collect();
    };

    let has_any_parent_links = nodes.iter().any(|(_, n)| {
        non_empty_field(n, "parentId").is_some() || non_empty_field(n, "logicalParentId").is_some()
    });
    if !has_any_parent_links && nodes.len() > 1 {
        return nodes
            .into_iter()
            .filter(|(_, e)| is_chain_entry(e))
            .map(|(span, e)| (span, e, false))
            .collect();
    }

    let mut chain = Vec::new();
    let mut cur = Some(start_idx);
    while let Some(idx) = cur {
        let (span, node) = nodes[idx];
        let direct_parent = non_empty_field(node, "parentId");
        let logical_parent = non_empty_field(node, "logicalParentId");

        let mut next_idx = None;
        let mut used_logical_parent = false;

        if let Some(pid) = direct_parent {
            next_idx = by_id
                .get(pid)
                .and_then(|list| list.iter().rev().copied().find(|&cand| cand < idx));
        } else if let Some(lpid) = logical_parent {
            next_idx = by_id
                .get(lpid)
                .and_then(|list| list.iter().rev().copied().find(|&cand| cand < idx));
            used_logical_parent = next_idx.is_some();
        }

        chain.push((span, node, used_logical_parent));
        cur = next_idx;
    }

    chain.reverse();
    chain
}

fn unwrap_user_query(raw: &str) -> String {
    const OPEN: &str = "<user_query>";
    const CLOSE: &str = "</user_query>";
    if let Some(start) = raw.rfind(OPEN) {
        let rest = &raw[start + OPEN.len()..];
        let end = rest.find(CLOSE).unwrap_or(rest.len());
        return rest[..end].trim().to_string();
    }
    if raw.contains("<system-reminder>") {
        return strip_system_reminders(raw).trim().to_string();
    }
    raw.to_string()
}

fn strip_system_reminders(raw: &str) -> String {
    const REM_OPEN: &str = "<system-reminder>";
    const REM_CLOSE: &str = "</system-reminder>";
    let mut current = raw;
    let mut out = String::new();
    while let Some(start) = current.find(REM_OPEN) {
        out.push_str(&current[..start]);
        let after_open = &current[start + REM_OPEN.len()..];
        if let Some(end) = after_open.find(REM_CLOSE) {
            current = &after_open[end + REM_CLOSE.len()..];
        } else {
            current = "";
            break;
        }
    }
    out.push_str(current);
    out
}

fn extract_workbuddy_message_text(entry: &Value) -> String {
    let is_user_prompt =
        entry.get("role").and_then(Value::as_str) == Some("user") && !is_compaction_summary(entry);
    let Some(content) = entry.get("content") else {
        return String::new();
    };
    match content {
        Value::String(s) => {
            if is_user_prompt {
                unwrap_user_query(s)
            } else {
                s.clone()
            }
        }
        Value::Array(items) => items
            .iter()
            .filter_map(|item| extract_workbuddy_content_item(item, is_user_prompt))
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => {
            extract_workbuddy_content_item(content, is_user_prompt).unwrap_or_default()
        }
        _ => String::new(),
    }
}

fn extract_workbuddy_content_item(item: &Value, is_user_prompt: bool) -> Option<String> {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    match item_type {
        "thinking" | "reasoning" | "redacted_thinking" | "summary_text" | "reasoning_text" => None,
        "image_blob_ref" | "input_image" | "image" => {
            if let Some(filename) = item
                .get("original_filename")
                .or_else(|| item.get("filename"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(format!("[Image: {filename}]"))
            } else {
                Some("[Image]".to_string())
            }
        }
        "input_text" | "output_text" | "text" => {
            let raw = item
                .get("text")
                .or_else(|| item.get("input_text"))
                .or_else(|| item.get("output_text"))
                .and_then(Value::as_str)?;
            let cleaned = if is_user_prompt {
                unwrap_user_query(raw)
            } else {
                raw.to_string()
            };
            (!cleaned.trim().is_empty()).then_some(cleaned)
        }
        _ => {
            let fallback = extract_text(item);
            let cleaned = if is_user_prompt {
                unwrap_user_query(&fallback)
            } else {
                fallback
            };
            (!cleaned.trim().is_empty()).then_some(cleaned)
        }
    }
}

fn image_media_type_from_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

fn local_image_ref(raw_path: &str, alt: Option<String>) -> Option<ImageRef> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return None;
    }
    let fs_path = if trimmed.starts_with("file://") {
        parse_file_url(trimmed).ok()?
    } else {
        PathBuf::from(trimmed)
    };
    if !fs_path.is_absolute() {
        return None;
    }
    let media_type = image_media_type_from_path(&fs_path)?;
    let size = fs::metadata(&fs_path)
        .ok()
        .map(|m| u32::try_from(m.len()).unwrap_or(u32::MAX))
        .unwrap_or(0);
    Some(ImageRef {
        source: ImageSource::LocalFile {
            path: trimmed.to_string(),
        },
        media_type: media_type.to_string(),
        size,
        alt,
    })
}

fn xml_attr_value<'a>(tag_attrs: &'a str, key: &str) -> Option<&'a str> {
    let mut rest = tag_attrs;
    while let Some(pos) = rest.find(key) {
        let before_ok = pos == 0 || rest.as_bytes()[pos - 1].is_ascii_whitespace();
        let after = &rest[pos + key.len()..];
        let after_trimmed = after.trim_start();
        if before_ok {
            if let Some(after_eq) = after_trimmed.strip_prefix('=') {
                let val_start = after_eq.trim_start();
                if let Some(quote) = val_start.chars().next().filter(|&c| c == '"' || c == '\'') {
                    let inner = &val_start[quote.len_utf8()..];
                    if let Some(end) = inner.find(quote) {
                        return Some(&inner[..end]);
                    }
                }
            }
        }
        rest = &rest[pos + key.len()..];
    }
    None
}

fn extract_attachment_images(raw: &str) -> (String, Vec<ImageRef>) {
    const TAG_OPEN: &str = "<attachment";
    const TAG_CLOSE: &str = "</attachment>";
    if !raw.contains(TAG_OPEN) {
        return (raw.to_string(), Vec::new());
    }
    let mut out = String::with_capacity(raw.len());
    let mut images = Vec::new();
    let mut cursor = raw;

    while let Some(start) = cursor.find(TAG_OPEN) {
        let after_name = &cursor[start + TAG_OPEN.len()..];
        let valid_tag = after_name
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_whitespace() || ch == '>' || ch == '/');
        if !valid_tag {
            out.push_str(&cursor[..start + TAG_OPEN.len()]);
            cursor = after_name;
            continue;
        }
        let Some(gt_rel) = after_name.find('>') else {
            out.push_str(cursor);
            cursor = "";
            break;
        };
        let header = &after_name[..gt_rel];
        let self_closing = header.trim_end().ends_with('/');
        let attrs = header.trim_end().trim_end_matches('/');

        let (body, consumed_end) = if self_closing {
            ("", start + TAG_OPEN.len() + gt_rel + 1)
        } else {
            let body_start = start + TAG_OPEN.len() + gt_rel + 1;
            if let Some(close_rel) = cursor[body_start..].find(TAG_CLOSE) {
                (
                    &cursor[body_start..body_start + close_rel],
                    body_start + close_rel + TAG_CLOSE.len(),
                )
            } else {
                ("", start + TAG_OPEN.len() + gt_rel + 1)
            }
        };

        let alt = ["name", "filename", "title"]
            .into_iter()
            .find_map(|k| xml_attr_value(attrs, k))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let attr_path = ["path", "file_path", "filepath", "file", "src", "url"]
            .into_iter()
            .find_map(|k| xml_attr_value(attrs, k));

        let body_candidate = body.lines().find_map(|line| {
            let t = line.trim();
            t.strip_prefix("Path:")
                .or_else(|| t.strip_prefix("path:"))
                .map(str::trim)
                .or((!t.is_empty()).then_some(t))
        });

        let matched_image = attr_path
            .and_then(|p| local_image_ref(p, alt.clone()))
            .or_else(|| body_candidate.and_then(|p| local_image_ref(p, alt)));

        out.push_str(&cursor[..start]);
        if let Some(image) = matched_image {
            images.push(image);
        } else {
            out.push_str(&cursor[start..consumed_end]);
        }
        cursor = &cursor[consumed_end..];
    }
    out.push_str(cursor);
    (out, images)
}

fn is_input_from_hook(entry: &Value) -> bool {
    entry
        .pointer("/extra/input_from_hook")
        .and_then(Value::as_bool)
        == Some(true)
        || entry
            .pointer("/providerData/extra/input_from_hook")
            .and_then(Value::as_bool)
            == Some(true)
        || entry
            .pointer("/providerData/input_from_hook")
            .and_then(Value::as_bool)
            == Some(true)
}

fn is_interrupted_entry(entry: &Value) -> bool {
    entry.pointer("/extra/interrupted").and_then(Value::as_bool) == Some(true)
        || entry
            .pointer("/providerData/extra/interrupted")
            .and_then(Value::as_bool)
            == Some(true)
        || entry
            .pointer("/providerData/interrupted")
            .and_then(Value::as_bool)
            == Some(true)
        || entry.get("status").and_then(Value::as_str) == Some("interrupted")
}

fn has_non_empty_str(val: &Value, keys: &[&str]) -> bool {
    keys.iter().any(|k| {
        val.get(*k)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    })
}

fn extract_reasoning_with_pointer(entry: &Value) -> (String, Option<String>, bool) {
    let mut redacted_sig = has_non_empty_str(entry, &["signature", "encrypted_content"]);
    for field in ["rawContent", "content", "summary"] {
        let Some(val) = entry.get(field) else {
            continue;
        };
        match val {
            Value::String(s) if !s.trim().is_empty() => {
                return (s.clone(), Some(format!("/{field}")), redacted_sig);
            }
            Value::Array(items) => {
                for item in items {
                    if item.get("type").and_then(Value::as_str) == Some("redacted_thinking")
                        || has_non_empty_str(item, &["signature", "encrypted_content", "data"])
                    {
                        redacted_sig = true;
                    }
                }
                let parts: Vec<&str> = items
                    .iter()
                    .filter_map(|item| match item {
                        Value::String(s) => Some(s.as_str()),
                        Value::Object(_) => item
                            .get("text")
                            .or_else(|| item.get("thinking"))
                            .or_else(|| item.get("input_text"))
                            .and_then(Value::as_str),
                        _ => None,
                    })
                    .filter(|s| !s.trim().is_empty())
                    .collect();
                if !parts.is_empty() {
                    let pointer = if items.len() == 1 && items[0].get("text").is_some() {
                        format!("/{field}/0/text")
                    } else if items.len() == 1 && items[0].get("thinking").is_some() {
                        format!("/{field}/0/thinking")
                    } else {
                        format!("/{field}")
                    };
                    return (parts.join("\n"), Some(pointer), redacted_sig);
                }
            }
            Value::Object(_) => {
                if has_non_empty_str(val, &["signature", "encrypted_content", "data"]) {
                    redacted_sig = true;
                }
                for sub in ["text", "thinking"] {
                    if let Some(s) = val
                        .get(sub)
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                    {
                        return (s.to_string(), Some(format!("/{field}/{sub}")), redacted_sig);
                    }
                }
            }
            _ => {}
        }
    }
    for top in ["text", "thinking"] {
        if let Some(s) = entry
            .get(top)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            return (s.to_string(), Some(format!("/{top}")), redacted_sig);
        }
    }
    (String::new(), None, redacted_sig)
}

fn strip_custom_prefix(model: &str) -> &str {
    match model.split_once(':') {
        Some((prefix, id)) if prefix.starts_with("custom-") && !id.trim().is_empty() => id.trim(),
        _ => model,
    }
}

fn u64_at(value: Option<&Value>, key: &str) -> u64 {
    value
        .and_then(|v| v.get(key))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn usage_meta(record: &Value) -> Option<MessageMeta> {
    if record.get("role").and_then(Value::as_str) == Some("user") {
        return None;
    }
    let data = record.get("providerData");
    let model = data
        .and_then(|d| d.get("model").or_else(|| d.get("requestModelId")))
        .or_else(|| record.get("model"))
        .and_then(Value::as_str)
        .map(|m| strip_custom_prefix(m.trim()))
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    let stop_reason = data
        .and_then(|d| d.get("stopReason").or_else(|| d.get("stop_reason")))
        .or_else(|| record.get("stop_reason"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let positive = |n: u64| (n > 0).then_some(n);

    if let Some(raw) = data
        .and_then(|d| d.get("rawUsage"))
        .filter(|v| v.is_object())
    {
        let details = raw.get("prompt_tokens_details");
        let cache_read = [
            u64_at(Some(raw), "prompt_cache_hit_tokens"),
            u64_at(Some(raw), "cache_read_input_tokens"),
            u64_at(Some(raw), "cached_tokens"),
            u64_at(details, "cached_tokens"),
        ]
        .into_iter()
        .max()
        .unwrap_or(0);
        let cache_write = u64_at(Some(raw), "cache_creation_input_tokens")
            .max(u64_at(Some(raw), "prompt_cache_write_tokens"));
        let reasoning = u64_at(Some(raw), "completion_thinking_tokens").max(u64_at(
            raw.get("completion_tokens_details"),
            "reasoning_tokens",
        ));
        let fresh_input = if raw.get("prompt_tokens").is_some() {
            u64_at(Some(raw), "prompt_tokens")
                .saturating_sub(cache_read)
                .saturating_sub(cache_write)
        } else {
            u64_at(Some(raw), "input_tokens")
        };
        let output = u64_at(Some(raw), "completion_tokens").max(u64_at(Some(raw), "output_tokens"));
        return MessageMeta {
            model,
            input_tokens: positive(fresh_input),
            output_tokens: positive(output),
            cache_read_tokens: positive(cache_read),
            cache_write_tokens: positive(cache_write),
            reasoning_tokens: positive(reasoning),
            stop_reason,
            ..MessageMeta::default()
        }
        .into_option();
    }

    if record.get("type").and_then(Value::as_str) == Some("message")
        && record.get("role").and_then(Value::as_str) == Some("assistant")
    {
        if let Some(usage) = data
            .and_then(|d| d.get("usage"))
            .or_else(|| record.get("usage"))
        {
            let cache_read = usage
                .get("inputTokensDetails")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|detail| u64_at(Some(detail), "cached_tokens"))
                .sum::<u64>()
                .max(u64_at(Some(usage), "cache_read_input_tokens"));
            let cache_write = u64_at(Some(usage), "cache_creation_input_tokens");
            let reasoning = usage
                .get("outputTokensDetails")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|detail| u64_at(Some(detail), "reasoning_tokens"))
                .sum::<u64>();
            let fresh_input = if usage.get("inputTokens").is_some() {
                u64_at(Some(usage), "inputTokens").saturating_sub(cache_read)
            } else {
                u64_at(Some(usage), "input_tokens")
            };
            let output =
                u64_at(Some(usage), "outputTokens").max(u64_at(Some(usage), "output_tokens"));
            return MessageMeta {
                model,
                input_tokens: positive(fresh_input),
                output_tokens: positive(output),
                cache_read_tokens: positive(cache_read),
                cache_write_tokens: positive(cache_write),
                reasoning_tokens: positive(reasoning),
                stop_reason,
                ..MessageMeta::default()
            }
            .into_option();
        }
    }

    MessageMeta {
        model: (record.get("type").and_then(Value::as_str) == Some("message")
            && record.get("role").and_then(Value::as_str) == Some("assistant"))
        .then_some(model)
        .flatten(),
        stop_reason,
        ..MessageMeta::default()
    }
    .into_option()
}

fn tool_status(record: &Value) -> ToolStatus {
    if is_interrupted_entry(record) {
        return ToolStatus::Interrupted;
    }
    if record
        .pointer("/providerData/toolResult/error")
        .is_some_and(|error| !error.is_null())
        || record
            .pointer("/providerData/toolResult/rawResponse/is_error")
            .and_then(Value::as_bool)
            == Some(true)
        || record.get("is_error").and_then(Value::as_bool) == Some(true)
        || record.pointer("/output/is_error").and_then(Value::as_bool) == Some(true)
    {
        return ToolStatus::Error;
    }
    match record.get("status").and_then(Value::as_str) {
        Some("completed" | "success") => ToolStatus::Success,
        Some("incomplete" | "failed" | "error") => ToolStatus::Error,
        Some("cancelled" | "canceled" | "interrupted") => ToolStatus::Interrupted,
        Some("in_progress" | "pending") => ToolStatus::Pending,
        _ => ToolStatus::Unknown,
    }
}

fn extract_tool_result_output(record: &Value) -> (String, Option<&'static str>) {
    match record.get("output") {
        Some(Value::String(text)) => (text.clone(), Some("/output")),
        Some(Value::Object(map)) => {
            if let Some(Value::String(text)) = map.get("text") {
                (text.clone(), Some("/output/text"))
            } else if let Some(Value::String(text)) = map.get("content") {
                (text.clone(), Some("/output/content"))
            } else if let Some(Value::String(text)) = map.get("error") {
                (text.clone(), Some("/output/error"))
            } else if let Some(arr @ Value::Array(_)) = map.get("content") {
                (extract_text(arr), Some("/output/content"))
            } else {
                (Value::Object(map.clone()).to_string(), Some("/output"))
            }
        }
        Some(arr @ Value::Array(_)) => (extract_text(arr), Some("/output")),
        Some(Value::Null) | None => (String::new(), None),
        Some(other) => (other.to_string(), None),
    }
}

#[derive(Default)]
struct AnonCallTracker {
    next_seq: usize,
    pending: VecDeque<(String, String)>,
}

impl AnonCallTracker {
    fn resolve_call_id(&mut self, record: &Value, name: &str) -> String {
        if let Some(id) = record
            .get("callId")
            .or_else(|| record.get("call_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return id.to_string();
        }
        self.next_seq += 1;
        let generated = format!("call_{}", self.next_seq);
        self.pending
            .push_back((name.to_string(), generated.clone()));
        generated
    }

    fn resolve_result_call_id(&mut self, record: &Value) -> String {
        if let Some(id) = record
            .get("callId")
            .or_else(|| record.get("call_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return id.to_string();
        }
        let target_name = record
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(name) = target_name {
            if let Some(idx) = self.pending.iter().position(|(n, _)| n == name) {
                return self
                    .pending
                    .remove(idx)
                    .map(|(_, id)| id)
                    .unwrap_or_default();
            }
        }
        self.pending
            .pop_front()
            .map(|(_, id)| id)
            .unwrap_or_default()
    }
}

fn build_content_image_block(
    item: &Value,
    span: JsonlSpan,
    item_ptr: &str,
) -> Option<SessionBlock> {
    let alt = item
        .get("original_filename")
        .or_else(|| item.get("filename"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    for key in ["path", "file_path", "filePath", "url"] {
        if let Some(raw_path) = item.get(key).and_then(Value::as_str) {
            if let Some(image) = local_image_ref(raw_path, alt.clone()) {
                return Some(SessionBlock::Image { image });
            }
        }
    }

    if item.pointer("/source/type").and_then(Value::as_str) == Some("base64") {
        if let Some(data) = item
            .pointer("/source/data")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let media_type = item
                .pointer("/source/media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png")
                .to_string();
            return Some(SessionBlock::Image {
                image: ImageRef {
                    source: ImageSource::Inline {
                        content: span.content_ref(format!("{item_ptr}/source/data")),
                    },
                    media_type,
                    size: estimate_base64_size(data.len()),
                    alt,
                },
            });
        }
    }

    if let Some(data) = item
        .get("data")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let media_type = item
            .get("media_type")
            .or_else(|| item.get("mimeType"))
            .and_then(Value::as_str)
            .unwrap_or("image/png")
            .to_string();
        return Some(SessionBlock::Image {
            image: ImageRef {
                source: ImageSource::Inline {
                    content: span.content_ref(format!("{item_ptr}/data")),
                },
                media_type,
                size: estimate_base64_size(data.len()),
                alt,
            },
        });
    }

    None
}

fn push_user_text_and_attachments(
    raw_text: &str,
    span: JsonlSpan,
    text_pointer: &str,
    input_from_hook: bool,
    blocks: &mut Vec<SessionBlock>,
    saw_real_prompt: &mut bool,
    saw_system_reminder_only: &mut bool,
) {
    let (without_attachments, attachment_images) = extract_attachment_images(raw_text);
    if !attachment_images.is_empty() {
        *saw_real_prompt = true;
    }

    let has_user_query = without_attachments.contains("<user_query>");
    let has_reminder = without_attachments.contains("<system-reminder>");
    let unwrapped = unwrap_user_query(&without_attachments);
    let trimmed = unwrapped.trim();

    if !trimmed.is_empty() {
        *saw_real_prompt = true;
        let ptr = text_pointer.to_string();
        if input_from_hook {
            blocks.push(large_text_block(trimmed.to_string(), || {
                Some(span.content_ref(ptr))
            }));
        } else {
            blocks.push(SessionBlock::text(trimmed.to_string()));
        }
    } else if !has_user_query && has_reminder {
        let reminder_trimmed = without_attachments.trim();
        if !reminder_trimmed.is_empty() {
            *saw_system_reminder_only = true;
            let ptr = text_pointer.to_string();
            blocks.push(large_text_block(reminder_trimmed.to_string(), || {
                Some(span.content_ref(ptr))
            }));
        }
    }

    for image in attachment_images {
        blocks.push(SessionBlock::Image { image });
    }
}

fn build_message_blocks(
    entry: &Value,
    span: JsonlSpan,
    role: &str,
    input_from_hook: bool,
    anon_calls: &mut AnonCallTracker,
) -> (Vec<SessionBlock>, bool) {
    let mut blocks = Vec::new();
    let is_user = role == "user";
    let mut saw_real_prompt = false;
    let mut saw_system_reminder_only = false;

    match entry.get("content") {
        Some(Value::String(raw)) => {
            if is_user {
                push_user_text_and_attachments(
                    raw,
                    span,
                    "/content",
                    input_from_hook,
                    &mut blocks,
                    &mut saw_real_prompt,
                    &mut saw_system_reminder_only,
                );
            } else if !raw.trim().is_empty() {
                blocks.push(SessionBlock::text(raw.clone()));
            }
        }
        Some(Value::Array(items)) => {
            let mut text_parts: Vec<String> = Vec::new();
            let mut single_text_ptr: Option<String> = None;
            let flush_text = |parts: &mut Vec<String>,
                              single_ptr: &mut Option<String>,
                              blocks: &mut Vec<SessionBlock>| {
                if parts.is_empty() {
                    return;
                }
                let joined = parts.join("\n");
                parts.clear();
                let ptr = single_ptr.take();
                if !joined.trim().is_empty() {
                    if input_from_hook {
                        blocks.push(large_text_block(joined, || {
                            ptr.map(|p| span.content_ref(p))
                        }));
                    } else {
                        blocks.push(SessionBlock::text(joined));
                    }
                }
            };

            for (idx, item) in items.iter().enumerate() {
                let item_ptr = format!("/content/{idx}");
                let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
                match item_type {
                    "thinking" | "reasoning" | "reasoning_text" | "summary_text" => {
                        flush_text(&mut text_parts, &mut single_text_ptr, &mut blocks);
                        let (field, text) =
                            if let Some(s) = item.get("thinking").and_then(Value::as_str) {
                                ("thinking", s)
                            } else if let Some(s) = item.get("text").and_then(Value::as_str) {
                                ("text", s)
                            } else {
                                ("thinking", "")
                            };
                        let has_sig = has_non_empty_str(item, &["signature", "encrypted_content"]);
                        if !text.trim().is_empty() || has_sig {
                            let ptr = format!("{item_ptr}/{field}");
                            blocks.push(thinking_block(text, None, None, || {
                                Some(span.content_ref(ptr))
                            }));
                        }
                    }
                    "redacted_thinking" => {
                        flush_text(&mut text_parts, &mut single_text_ptr, &mut blocks);
                        blocks.push(thinking_block("", None, None, || None));
                    }
                    "image_blob_ref" | "input_image" | "image" => {
                        if let Some(img_block) = build_content_image_block(item, span, &item_ptr) {
                            flush_text(&mut text_parts, &mut single_text_ptr, &mut blocks);
                            saw_real_prompt = true;
                            blocks.push(img_block);
                        } else if let Some(placeholder) =
                            extract_workbuddy_content_item(item, is_user)
                        {
                            saw_real_prompt = true;
                            single_text_ptr = None;
                            text_parts.push(placeholder);
                        }
                    }
                    "tool_use" => {
                        flush_text(&mut text_parts, &mut single_text_ptr, &mut blocks);
                        if let Some(name) = item.get("name").and_then(Value::as_str) {
                            let id = item
                                .get("id")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                                .unwrap_or_else(|| anon_calls.resolve_call_id(item, name));
                            let input = item.get("input").unwrap_or(&Value::Null);
                            let ptr = format!("{item_ptr}/input");
                            blocks.push(tool_call_block(
                                ToolSource::Claude,
                                id,
                                name,
                                input,
                                || Some(span.content_ref(ptr)),
                            ));
                        }
                    }
                    "tool_result" => {
                        flush_text(&mut text_parts, &mut single_text_ptr, &mut blocks);
                        let call_id = item
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| anon_calls.resolve_result_call_id(item));
                        let status = if item.get("is_error").and_then(Value::as_bool) == Some(true)
                        {
                            ToolStatus::Error
                        } else {
                            ToolStatus::Success
                        };
                        let text = item.get("content").map(extract_text).unwrap_or_default();
                        let ptr = format!("{item_ptr}/content");
                        blocks.push(tool_result_block(call_id, status, &text, || {
                            Some(span.content_ref(ptr))
                        }));
                    }
                    "input_text" | "output_text" | "text" | "" => {
                        let (sub_key, raw_opt) = if let Some(s) =
                            item.get("text").and_then(Value::as_str)
                        {
                            ("text", Some(s))
                        } else if let Some(s) = item.get("input_text").and_then(Value::as_str) {
                            ("input_text", Some(s))
                        } else if let Some(s) = item.get("output_text").and_then(Value::as_str) {
                            ("output_text", Some(s))
                        } else {
                            ("text", None)
                        };
                        if let Some(raw) = raw_opt {
                            let text_ptr = format!("{item_ptr}/{sub_key}");
                            if is_user {
                                let (without_attachments, attachment_images) =
                                    extract_attachment_images(raw);
                                let has_user_query = without_attachments.contains("<user_query>");
                                let has_reminder =
                                    without_attachments.contains("<system-reminder>");
                                let unwrapped = unwrap_user_query(&without_attachments);
                                let trimmed = unwrapped.trim();
                                if !trimmed.is_empty() {
                                    saw_real_prompt = true;
                                    if text_parts.is_empty() {
                                        single_text_ptr = Some(text_ptr);
                                    } else {
                                        single_text_ptr = None;
                                    }
                                    text_parts.push(trimmed.to_string());
                                } else if !has_user_query && has_reminder {
                                    let reminder_trimmed = without_attachments.trim();
                                    if !reminder_trimmed.is_empty() {
                                        saw_system_reminder_only = true;
                                        if text_parts.is_empty() {
                                            single_text_ptr = Some(text_ptr);
                                        } else {
                                            single_text_ptr = None;
                                        }
                                        text_parts.push(reminder_trimmed.to_string());
                                    }
                                }
                                if !attachment_images.is_empty() {
                                    saw_real_prompt = true;
                                    flush_text(&mut text_parts, &mut single_text_ptr, &mut blocks);
                                    for image in attachment_images {
                                        blocks.push(SessionBlock::Image { image });
                                    }
                                }
                            } else if !raw.trim().is_empty() {
                                if text_parts.is_empty() {
                                    single_text_ptr = Some(text_ptr);
                                } else {
                                    single_text_ptr = None;
                                }
                                text_parts.push(raw.to_string());
                            }
                        }
                    }
                    _ => {
                        if let Some(fallback) = extract_workbuddy_content_item(item, is_user) {
                            saw_real_prompt = true;
                            single_text_ptr = None;
                            text_parts.push(fallback);
                        }
                    }
                }
            }
            flush_text(&mut text_parts, &mut single_text_ptr, &mut blocks);
        }
        Some(obj @ Value::Object(_)) => {
            if let Some(img_block) = build_content_image_block(obj, span, "/content") {
                saw_real_prompt = true;
                blocks.push(img_block);
            } else if let Some(text) = extract_workbuddy_content_item(obj, is_user) {
                saw_real_prompt = true;
                blocks.push(SessionBlock::text(text));
            }
        }
        _ => {}
    }

    let injected = role == "system"
        || input_from_hook
        || (is_user && !saw_real_prompt && saw_system_reminder_only);
    (blocks, injected)
}

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    let mut entries = Vec::new();
    for_each_jsonl_value(path, |span, value| {
        entries.push((span, value));
        Ok(())
    })?;

    let mut messages = Vec::new();
    let mut anon_calls = AnonCallTracker::default();

    for (span, entry, logical_parent_resolved) in reconstruct_chain(&entries) {
        let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
        let ts = entry.get("timestamp").and_then(parse_timestamp_to_ms);
        let record_id = non_empty_field(entry, "id").map(str::to_string);

        match entry_type {
            "reasoning" => {
                let (thinking, pointer, has_sig) = extract_reasoning_with_pointer(entry);
                if thinking.trim().is_empty() && !has_sig {
                    continue;
                }
                let block = thinking_block(&thinking, None, None, || {
                    pointer.map(|p| span.content_ref(p))
                });
                let mut msg = SessionMessage::from_blocks("assistant", ts, vec![block]);
                msg.id = record_id;
                msg.meta = usage_meta(entry);
                messages.push(msg);
            }
            "function_call" => {
                let Some(name) = non_empty_field(entry, "name") else {
                    continue;
                };
                let call_id = anon_calls.resolve_call_id(entry, name);
                let args = parse_arguments(entry.get("arguments").unwrap_or(&Value::Null));
                let block = tool_call_block(ToolSource::Claude, call_id, name, &args, || {
                    Some(span.content_ref("/arguments"))
                });
                let mut msg = SessionMessage::from_blocks("assistant", ts, vec![block]);
                msg.id = record_id;
                msg.meta = usage_meta(entry);
                messages.push(msg);
            }
            "function_call_result" => {
                let call_id = anon_calls.resolve_result_call_id(entry);
                let (text, pointer) = extract_tool_result_output(entry);
                let block = tool_result_block(call_id, tool_status(entry), &text, || {
                    pointer.map(|p| span.content_ref(p))
                });
                let mut msg = SessionMessage::from_blocks("tool", ts, vec![block]);
                msg.id = record_id;
                messages.push(msg);
            }
            "compact_boundary" => {
                let (summary, ptr) = if let Some(s) = entry.get("summary").and_then(Value::as_str) {
                    (s, Some("/summary"))
                } else if let Some(s) = entry.get("content").and_then(Value::as_str) {
                    (s, Some("/content"))
                } else {
                    ("", None)
                };
                let block = if summary.trim().is_empty() {
                    SessionBlock::event(EventKind::Compaction, None, None)
                } else {
                    summary_event_block(EventKind::Compaction, summary, || {
                        ptr.map(|p| span.content_ref(p))
                    })
                };
                let mut msg = SessionMessage::from_blocks("system", ts, vec![block]);
                msg.id = record_id;
                msg.injected = true;
                messages.push(msg);
            }
            "system" => {
                let subtype = entry.get("subtype").and_then(Value::as_str).unwrap_or("");
                if subtype == "compact_boundary" || is_compaction_summary(entry) {
                    if logical_parent_resolved {
                        continue;
                    }
                    let (summary, ptr) =
                        if let Some(s) = entry.get("content").and_then(Value::as_str) {
                            (s.to_string(), Some("/content"))
                        } else if let Some(s) = entry.get("summary").and_then(Value::as_str) {
                            (s.to_string(), Some("/summary"))
                        } else {
                            (extract_workbuddy_message_text(entry), Some("/content"))
                        };
                    let block = if summary.trim().is_empty() {
                        SessionBlock::event(EventKind::Compaction, None, None)
                    } else {
                        summary_event_block(EventKind::Compaction, &summary, || {
                            ptr.map(|p| span.content_ref(p))
                        })
                    };
                    let mut msg = SessionMessage::from_blocks("system", ts, vec![block]);
                    msg.id = record_id;
                    msg.injected = true;
                    messages.push(msg);
                } else if is_interrupted_entry(entry) {
                    let mut msg = SessionMessage::from_blocks(
                        "system",
                        ts,
                        vec![SessionBlock::event(EventKind::Aborted, None, None)],
                    );
                    msg.id = record_id;
                    messages.push(msg);
                }
            }
            "message" => {
                let input_from_hook = is_input_from_hook(entry);
                let interrupted = is_interrupted_entry(entry);
                if is_meta_entry(entry) && !input_from_hook && !interrupted {
                    continue;
                }
                if is_compaction_summary(entry) {
                    if logical_parent_resolved {
                        continue;
                    }
                    let summary_body = extract_workbuddy_message_text(entry);
                    let block = if summary_body.trim().is_empty() {
                        SessionBlock::event(EventKind::Compaction, None, None)
                    } else {
                        summary_event_block(EventKind::Compaction, &summary_body, || {
                            Some(span.content_ref("/content"))
                        })
                    };
                    let mut msg = SessionMessage::from_blocks("system", ts, vec![block]);
                    msg.id = record_id;
                    msg.injected = true;
                    messages.push(msg);
                    continue;
                }

                let raw_role = entry
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                if !matches!(raw_role, "user" | "assistant" | "system") {
                    continue;
                }

                let (mut blocks, injected) =
                    build_message_blocks(entry, span, raw_role, input_from_hook, &mut anon_calls);
                if interrupted {
                    blocks.push(SessionBlock::event(EventKind::Aborted, None, None));
                }

                let meta = (raw_role == "assistant")
                    .then(|| usage_meta(entry))
                    .flatten();
                let mut msg = SessionMessage::from_blocks(raw_role, ts, blocks);
                if msg.is_empty() && meta.is_none() {
                    continue;
                }
                msg.id = record_id;
                msg.injected = injected;
                msg.meta = meta;
                messages.push(msg);
            }
            _ => {}
        }
    }

    assign_turn_ids(&mut messages);
    Ok(messages)
}

pub(crate) fn resolve_sqlite_jsonl_for_content(source: &str) -> Result<(PathBuf, PathBuf), String> {
    resolve_sqlite_jsonl_for_content_in_roots(&session_roots(), source)
}

fn resolve_sqlite_jsonl_for_content_in_roots(
    roots: &[PathBuf],
    source: &str,
) -> Result<(PathBuf, PathBuf), String> {
    let (db_path, session_id) = parse_source(source)
        .ok_or_else(|| format!("Invalid WorkBuddy SQLite source reference: {source}"))?;
    let (home, _) = validate_sqlite_source_in_roots(roots, &db_path, &session_id)?;
    let jsonl_path = find_existing_jsonl_in_home(&home, &session_id)
        .ok_or_else(|| "WorkBuddy 会话正文文件不存在".to_string())?;
    let canonical_home = home
        .canonicalize()
        .map_err(|e| format!("无法解析 WorkBuddy 目录：{e}"))?;
    let canonical_jsonl = jsonl_path
        .canonicalize()
        .map_err(|e| format!("无法解析 WorkBuddy 会话文件：{e}"))?;
    if !canonical_jsonl.starts_with(&canonical_home) {
        return Err("引用的路径不在会话允许目录内".to_string());
    }
    Ok((canonical_home, canonical_jsonl))
}

pub fn sqlite_transcript_sources(source: &str) -> Option<cache::TranscriptSources> {
    sqlite_transcript_sources_in_roots(&session_roots(), source)
}

fn sqlite_transcript_sources_in_roots(
    roots: &[PathBuf],
    source: &str,
) -> Option<cache::TranscriptSources> {
    let (db_path, session_id) = parse_source(source)?;
    let (home, validated_db) =
        validate_sqlite_source_in_roots(roots, &db_path, &session_id).ok()?;
    let mut sources = Vec::new();
    if let Some(jsonl) = find_existing_jsonl_in_home(&home, &session_id) {
        sources.push((jsonl, None));
    }
    sources.push((validated_db.clone(), None));
    sources.push((
        PathBuf::from(format!("{}-wal", validated_db.display())),
        None,
    ));
    Some(sources)
}

pub fn load_messages_sqlite(source: &str) -> Result<Vec<SessionMessage>, String> {
    load_messages_sqlite_in_roots(&session_roots(), source)
}

fn load_messages_sqlite_in_roots(
    roots: &[PathBuf],
    source: &str,
) -> Result<Vec<SessionMessage>, String> {
    let (db_path, session_id) = parse_source(source)
        .ok_or_else(|| format!("Invalid WorkBuddy SQLite source reference: {source}"))?;
    let (home, validated_db) = validate_sqlite_source_in_roots(roots, &db_path, &session_id)?;

    if let Some(jsonl_path) = find_existing_jsonl_in_home(&home, &session_id) {
        return load_messages(&jsonl_path);
    }

    if validated_db.exists() {
        let conn = Connection::open_with_flags(
            &validated_db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| format!("无法读取 WorkBuddy 数据库 {}：{e}", validated_db.display()))?;
        conn.busy_timeout(Duration::from_secs(2))
            .map_err(|e| format!("无法配置 WorkBuddy 数据库读取超时：{e}"))?;
        let _: i64 = conn
            .query_row("SELECT 1", [], |row| row.get(0))
            .map_err(|e| format!("WorkBuddy 数据库校验失败：{e}"))?;
    }

    Ok(Vec::new())
}

fn find_existing_jsonl_in_home(home: &Path, session_id: &str) -> Option<PathBuf> {
    if !is_valid_workbuddy_session_id(session_id) {
        return None;
    }
    let projects = home.join("projects");
    let entries = fs::read_dir(&projects).ok()?;
    let filename = format!("{session_id}.jsonl");
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|ft| ft.is_dir()) {
            continue;
        }
        let proj_dir = entry.path();
        if fs::symlink_metadata(&proj_dir).is_ok_and(|m| is_reparse_or_symlink(&m)) {
            continue;
        }
        let candidate = proj_dir.join(&filename);
        if fs::symlink_metadata(&candidate)
            .is_ok_and(|m| !is_reparse_or_symlink(&m) && m.file_type().is_file())
            && checked_storage_child(home, &candidate).is_ok()
        {
            return Some(candidate);
        }
    }
    None
}

fn validate_jsonl_in_roots(
    roots: &[PathBuf],
    path: &Path,
    session_id: &str,
) -> Result<(PathBuf, PathBuf), String> {
    if !is_valid_workbuddy_session_id(session_id) {
        return Err(format!("Invalid WorkBuddy session ID: {session_id}"));
    }
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(format!(
            "Invalid WorkBuddy session path: {}",
            path.display()
        ));
    }
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return Err(format!(
            "Invalid WorkBuddy session file extension: {}",
            path.display()
        ));
    }
    if path.file_stem().and_then(|s| s.to_str()) != Some(session_id) {
        return Err(format!(
            "WorkBuddy session ID ({session_id}) does not match file path: {}",
            path.display()
        ));
    }

    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                return Err(format!(
                    "WorkBuddy session source must not be a symlink or reparse point: {}",
                    path.display()
                ));
            }
            if !meta.file_type().is_file() {
                return Err(format!(
                    "WorkBuddy session source is not a regular file: {}",
                    path.display()
                ));
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "Failed to inspect WorkBuddy session {}: {err}",
                path.display()
            ))
        }
    }

    let project_dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("Invalid WorkBuddy project directory: {}", path.display()))?;
    let projects_dir = project_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("Invalid WorkBuddy projects root: {}", path.display()))?;
    if projects_dir.file_name() != Some(OsStr::new("projects")) {
        return Err(format!(
            "WorkBuddy session must reside directly under projects/<project>: {}",
            path.display()
        ));
    }
    let home_candidate = projects_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("Invalid WorkBuddy home directory: {}", path.display()))?;

    let mut saw_existing_root = false;
    for root in roots {
        if !root.exists() {
            continue;
        }
        saw_existing_root = true;
        let Ok(canonical_root) = root.canonicalize() else {
            continue;
        };
        let matches_home = home_candidate == root
            || home_candidate
                .canonicalize()
                .is_ok_and(|resolved| resolved == canonical_root);
        if matches_home {
            let checked = checked_storage_child(root, path)?;
            return Ok((root.clone(), checked));
        }
    }

    if !saw_existing_root {
        return Err("WorkBuddy 会话根目录不存在".to_string());
    }
    Err(format!(
        "WorkBuddy session path is outside configured storage: {}",
        path.display()
    ))
}

fn validate_sqlite_source_in_roots(
    roots: &[PathBuf],
    db_path: &Path,
    session_id: &str,
) -> Result<(PathBuf, PathBuf), String> {
    if !is_valid_workbuddy_session_id(session_id) {
        return Err(format!("Invalid WorkBuddy session ID: {session_id}"));
    }
    if !db_path.is_absolute()
        || db_path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(format!(
            "Invalid WorkBuddy database path: {}",
            db_path.display()
        ));
    }
    if db_path.file_name() != Some(OsStr::new(DB_FILENAME)) {
        return Err(format!(
            "Unexpected WorkBuddy database filename: {}",
            db_path.display()
        ));
    }

    match fs::symlink_metadata(db_path) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                return Err(format!(
                    "WorkBuddy database must not be a symlink or reparse point: {}",
                    db_path.display()
                ));
            }
            if !meta.file_type().is_file() {
                return Err(format!(
                    "WorkBuddy database is not a regular file: {}",
                    db_path.display()
                ));
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "Failed to inspect WorkBuddy database {}: {err}",
                db_path.display()
            ))
        }
    }

    let home_candidate = db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("Invalid WorkBuddy home directory: {}", db_path.display()))?;

    let mut saw_existing_root = false;
    for root in roots {
        if !root.exists() {
            continue;
        }
        saw_existing_root = true;
        let Ok(canonical_root) = root.canonicalize() else {
            continue;
        };
        let matches_home = home_candidate == root
            || home_candidate
                .canonicalize()
                .is_ok_and(|resolved| resolved == canonical_root);
        if matches_home {
            let checked = checked_storage_child(root, db_path)?;
            return Ok((root.clone(), checked));
        }
    }

    if !saw_existing_root {
        return Err("WorkBuddy 会话根目录不存在".to_string());
    }
    Err(format!(
        "WorkBuddy database path is outside configured storage: {}",
        db_path.display()
    ))
}

fn ensure_no_active_automation_in_conn(conn: &Connection, session_id: &str) -> Result<(), String> {
    let tables = sqlite_table_names(conn)?;
    if tables.iter().any(|t| t == "automation_runtime_state")
        && sqlite_table_has_column(conn, "automation_runtime_state", "running")?
        && sqlite_table_has_column(conn, "automation_runtime_state", "running_conversation_id")?
    {
        let running: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM automation_runtime_state \
                 WHERE COALESCE(running, 0) != 0 AND running_conversation_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("无法检查 WorkBuddy 自动化运行状态：{e}"))?;
        if running > 0 {
            return Err(format!(
                "该 WorkBuddy 会话（{session_id}）正在运行后台自动化任务，请等待任务结束后再清理"
            ));
        }
    }

    if tables.iter().any(|t| t == "automation_delivery_outbox")
        && sqlite_table_has_column(conn, "automation_delivery_outbox", "run_id")?
        && sqlite_table_has_column(conn, "automation_delivery_outbox", "status")?
    {
        let finished_clause =
            if sqlite_table_has_column(conn, "automation_delivery_outbox", "finished_at")? {
                "AND (finished_at IS NULL OR finished_at <= 0)"
            } else {
                ""
            };
        let sql = format!(
            "SELECT COUNT(*) FROM automation_delivery_outbox \
             WHERE run_id = ?1 {finished_clause} \
             AND LOWER(TRIM(COALESCE(status, ''))) IN ('pending', 'sending', 'retrying', 'running', 'processing')"
        );
        let pending: i64 = conn
            .query_row(&sql, [session_id], |row| row.get(0))
            .map_err(|e| format!("无法检查 WorkBuddy 自动化投递状态：{e}"))?;
        if pending > 0 {
            return Err(format!(
                "该 WorkBuddy 会话（{session_id}）仍有未完成的自动化投递任务，请稍后再清理"
            ));
        }
    }

    Ok(())
}

fn ensure_no_active_automation_on_disk(home: &Path, session_id: &str) -> Result<(), String> {
    let db_path = home.join(DB_FILENAME);
    let meta = match fs::symlink_metadata(&db_path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(format!(
                "无法检查 WorkBuddy 数据库 {}：{err}",
                db_path.display()
            ))
        }
    };
    if is_reparse_or_symlink(&meta) || !meta.file_type().is_file() {
        return Err(format!(
            "WorkBuddy 数据库不是普通文件，已拒绝操作：{}",
            db_path.display()
        ));
    }
    let checked_db = checked_storage_child(home, &db_path)?;
    let conn = Connection::open_with_flags(
        &checked_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("无法打开 WorkBuddy 数据库执行安全预检：{e}"))?;
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(|e| format!("无法配置 WorkBuddy 数据库预检超时：{e}"))?;
    ensure_no_active_automation_in_conn(&conn, session_id)
}

pub fn preflight_delete_in_roots(
    roots: &[PathBuf],
    path: &Path,
    session_id: &str,
) -> Result<(), String> {
    let (home, _) = validate_jsonl_in_roots(roots, path, session_id)?;
    ensure_no_active_automation_on_disk(&home, session_id)
}

pub fn preflight_delete_sqlite(source: &str, session_id: &str) -> Result<(), String> {
    preflight_delete_sqlite_in_roots(&session_roots(), source, session_id)
}

fn preflight_delete_sqlite_in_roots(
    roots: &[PathBuf],
    source: &str,
    session_id: &str,
) -> Result<(), String> {
    let (db_path, referenced_id) = parse_source(source)
        .ok_or_else(|| format!("Invalid WorkBuddy SQLite source reference: {source}"))?;
    if referenced_id != session_id {
        return Err("WorkBuddy session ID does not match the source reference".to_string());
    }
    let (home, _) = validate_sqlite_source_in_roots(roots, &db_path, session_id)?;
    ensure_no_active_automation_on_disk(&home, session_id)
}

pub fn delete_session_in_roots(
    roots: &[PathBuf],
    path: &Path,
    session_id: &str,
) -> Result<bool, String> {
    let (home, checked_jsonl) = validate_jsonl_in_roots(roots, path, session_id)?;
    cleanup_home_session(&home, Some(&checked_jsonl), session_id)
}

pub fn delete_session_sqlite(session_id: &str, source: &str) -> Result<bool, String> {
    delete_session_sqlite_in_roots(&session_roots(), session_id, source)
}

fn delete_session_sqlite_in_roots(
    roots: &[PathBuf],
    session_id: &str,
    source: &str,
) -> Result<bool, String> {
    let (db_path, referenced_id) = parse_source(source)
        .ok_or_else(|| format!("Invalid WorkBuddy SQLite source reference: {source}"))?;
    if referenced_id != session_id {
        return Err("WorkBuddy session ID does not match the source reference".to_string());
    }
    let (home, _) = validate_sqlite_source_in_roots(roots, &db_path, session_id)?;
    cleanup_home_session(&home, None, session_id)
}

fn cleanup_home_session(
    home: &Path,
    primary_jsonl: Option<&Path>,
    session_id: &str,
) -> Result<bool, String> {
    if !is_valid_workbuddy_session_id(session_id) {
        return Err(format!("Invalid WorkBuddy session ID: {session_id}"));
    }

    let mut removed_any = false;

    // 1. Primary SQLite database (`workbuddy.db`)
    if cleanup_primary_db(home, session_id)? {
        removed_any = true;
    }

    // 2. Companion `edge-sync-mapping*.db` databases
    if cleanup_edge_sync_dbs(home, session_id)? {
        removed_any = true;
    }

    // 3. `sidebar-list-snapshot.json` caches (in `<home>` and `<home>/<user-dir>/`)
    if cleanup_sidebar_snapshots(home, session_id)? {
        removed_any = true;
    }

    // 4. Primary & project-level transcripts, `.file-rollback.ndjson`, and `<proj>/<session_id>/` sidecars
    if let Some(jsonl) = primary_jsonl {
        if remove_checked_target(home, jsonl)? {
            removed_any = true;
        }
        if let Some(proj_dir) = jsonl.parent() {
            if cleanup_project_session_artifacts(home, proj_dir, session_id)? {
                removed_any = true;
            }
            let _ = fs::remove_dir(proj_dir);
        }
    }

    let projects_root = home.join("projects");
    if let Ok(entries) = fs::read_dir(&projects_root) {
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                continue;
            }
            let proj_dir = entry.path();
            if fs::symlink_metadata(&proj_dir).is_ok_and(|m| is_reparse_or_symlink(&m)) {
                continue;
            }
            if cleanup_project_session_artifacts(home, &proj_dir, session_id)? {
                removed_any = true;
            }
            let _ = fs::remove_dir(&proj_dir);
        }
    }

    // 5. Home-level sidecar backup directories & JSON manifests
    let sidecars = [
        home.join("workspace").join("sessions").join(session_id),
        home.join("file-history").join(session_id),
        home.join("changes-index")
            .join(format!("{session_id}.json")),
        home.join("changes-detail").join(session_id),
        home.join("artifact-index")
            .join(format!("{session_id}.json")),
        home.join("file-tree-manifests")
            .join(format!("{session_id}.json")),
        home.join("tasks").join(session_id),
    ];
    for candidate in &sidecars {
        if remove_checked_target(home, candidate)? {
            removed_any = true;
        }
    }

    // 6. Stale PID session lockfiles referencing `sessionId`
    if cleanup_session_lockfiles(home, session_id)? {
        removed_any = true;
    }

    Ok(removed_any)
}

fn cleanup_project_session_artifacts(
    home: &Path,
    proj_dir: &Path,
    session_id: &str,
) -> Result<bool, String> {
    let mut removed = false;
    let targets = [
        proj_dir.join(format!("{session_id}.jsonl")),
        proj_dir.join(format!("{session_id}.file-rollback.ndjson")),
        proj_dir.join(session_id),
    ];
    for target in &targets {
        if remove_checked_target(home, target)? {
            removed = true;
        }
    }
    Ok(removed)
}

fn remove_checked_target(home: &Path, target: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(target) {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(format!(
                "无法检查 WorkBuddy 会话附属文件 {}：{err}",
                target.display()
            ))
        }
    }
    let checked = checked_storage_child(home, target)?;
    remove_path_if_exists(&checked)
        .map(|()| true)
        .map_err(|err| format!("无法删除 {}：{err}", checked.display()))
}

fn cleanup_primary_db(home: &Path, session_id: &str) -> Result<bool, String> {
    let db_path = home.join(DB_FILENAME);
    let meta = match fs::symlink_metadata(&db_path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(format!(
                "无法检查 WorkBuddy 数据库 {}：{err}",
                db_path.display()
            ))
        }
    };
    if is_reparse_or_symlink(&meta) || !meta.file_type().is_file() {
        return Err(format!(
            "WorkBuddy 数据库不是普通文件，已拒绝写入：{}",
            db_path.display()
        ));
    }

    let checked_db = checked_storage_child(home, &db_path)?;
    let mut conn = Connection::open_with_flags(
        &checked_db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("无法打开 WorkBuddy 数据库 {}：{e}", checked_db.display()))?;
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(|e| format!("无法配置 WorkBuddy 数据库锁超时：{e}"))?;

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("无法开启 WorkBuddy 数据库清理事务：{e}"))?;

    ensure_no_active_automation_in_conn(&tx, session_id)?;

    let tables = sqlite_table_names(&tx)?;
    let mut deleted_rows = 0usize;

    for (table, col) in [
        ("session_usage", "session_id"),
        ("automation_runs", "thread_id"),
        ("cloud_conversations", "id"),
        ("sessions", "id"),
    ] {
        if tables.iter().any(|t| t == table) && sqlite_table_has_column(&tx, table, col)? {
            let sql = format!("DELETE FROM {table} WHERE {col} = ?1");
            deleted_rows += tx
                .execute(&sql, [session_id])
                .map_err(|e| format!("清理 WorkBuddy 表 {table} 失败：{e}"))?;
        }
    }

    tx.commit()
        .map_err(|e| format!("提交 WorkBuddy 数据库清理事务失败：{e}"))?;
    Ok(deleted_rows > 0)
}

fn cleanup_edge_sync_dbs(home: &Path, session_id: &str) -> Result<bool, String> {
    let entries = match fs::read_dir(home) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(format!("无法读取 WorkBuddy 目录 {}：{err}", home.display())),
    };
    let mut removed = false;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.starts_with("edge-sync-mapping") || !name.ends_with(".db") {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if is_reparse_or_symlink(&meta) || !meta.file_type().is_file() {
            continue;
        }
        let checked = checked_storage_child(home, &path)?;
        let mut conn = Connection::open_with_flags(
            &checked,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| format!("无法打开 {}：{e}", checked.display()))?;
        conn.busy_timeout(Duration::from_secs(2))
            .map_err(|e| format!("无法配置 {} 锁超时：{e}", checked.display()))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| format!("无法开启 {} 清理事务：{e}", checked.display()))?;
        let tables = sqlite_table_names(&tx)?;
        if tables.iter().any(|t| t == "edge_sync_mapping")
            && sqlite_table_has_column(&tx, "edge_sync_mapping", "session_id")?
        {
            let count = tx
                .execute(
                    "DELETE FROM edge_sync_mapping WHERE session_id = ?1",
                    [session_id],
                )
                .map_err(|e| format!("清理 {} 失败：{e}", checked.display()))?;
            if count > 0 {
                removed = true;
            }
        }
        tx.commit()
            .map_err(|e| format!("提交 {} 事务失败：{e}", checked.display()))?;
    }
    Ok(removed)
}

fn cleanup_sidebar_snapshots(home: &Path, session_id: &str) -> Result<bool, String> {
    let mut candidates = vec![home.join("sidebar-list-snapshot.json")];
    if let Ok(entries) = fs::read_dir(home) {
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                continue;
            }
            let sub = entry.path();
            if fs::symlink_metadata(&sub).is_ok_and(|m| is_reparse_or_symlink(&m)) {
                continue;
            }
            candidates.push(sub.join("sidebar-list-snapshot.json"));
        }
    }

    let mut updated_any = false;
    for candidate in candidates {
        let meta = match fs::symlink_metadata(&candidate) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(format!("无法检查侧边栏快照 {}：{err}", candidate.display())),
        };
        if is_reparse_or_symlink(&meta) || !meta.file_type().is_file() {
            continue;
        }
        if meta.len() > MAX_SNAPSHOT_BYTES {
            return Err(format!(
                "WorkBuddy 侧边栏快照超过 16 MiB 上限，已暂停更新：{}",
                candidate.display()
            ));
        }
        let checked = checked_storage_child(home, &candidate)?;
        let raw = fs::read_to_string(&checked)
            .map_err(|e| format!("无法读取侧边栏快照 {}：{e}", checked.display()))?;
        let Ok(mut doc) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let mut changed = false;
        if let Some(items) = doc.get_mut("items").and_then(Value::as_array_mut) {
            let before = items.len();
            items.retain(|item| {
                let id_match = item.get("id").and_then(Value::as_str) == Some(session_id)
                    || item.get("sessionId").and_then(Value::as_str) == Some(session_id);
                !id_match
            });
            changed = items.len() != before;
        }
        if changed {
            atomic_write_json(&checked, &doc)?;
            updated_any = true;
        }
    }
    Ok(updated_any)
}

fn cleanup_session_lockfiles(home: &Path, session_id: &str) -> Result<bool, String> {
    let sessions_dir = home.join("sessions");
    let entries = match fs::read_dir(&sessions_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(format!(
                "无法读取 WorkBuddy 会话锁目录 {}：{err}",
                sessions_dir.display()
            ))
        }
    };
    let mut removed = false;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if is_reparse_or_symlink(&meta)
            || !meta.file_type().is_file()
            || meta.len() > MAX_LOCKFILE_BYTES
        {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if val.get("sessionId").and_then(Value::as_str) == Some(session_id)
            && remove_checked_target(home, &path)?
        {
            removed = true;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn create_workbuddy_db(home: &Path) -> Connection {
        let db_path = home.join(DB_FILENAME);
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                cwd TEXT NOT NULL,
                user_id TEXT NOT NULL DEFAULT 'u1',
                title TEXT,
                custom_title TEXT,
                group_title TEXT,
                status TEXT NOT NULL DEFAULT 'Completed',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                last_activity_at INTEGER,
                deleted_at INTEGER DEFAULT -1,
                transport TEXT NOT NULL DEFAULT 'local'
            );
            CREATE TABLE session_usage (
                session_id TEXT PRIMARY KEY,
                used INTEGER,
                size INTEGER,
                updated_at INTEGER
            );
            CREATE TABLE automation_runs (
                thread_id TEXT PRIMARY KEY,
                automation_id TEXT NOT NULL,
                status TEXT NOT NULL
            );
            CREATE TABLE automation_runtime_state (
                automation_id TEXT PRIMARY KEY,
                running INTEGER NOT NULL DEFAULT 0,
                running_conversation_id TEXT
            );
            CREATE TABLE automation_delivery_outbox (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                status TEXT NOT NULL,
                finished_at INTEGER
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn scans_active_soft_deleted_and_orphan_backup_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join(".workbuddy-ai");
        let proj = home.join("projects").join("e-demo-app");
        fs::create_dir_all(&proj).unwrap();

        let active_id = "11111111-1111-4111-8111-111111111111";
        let soft_deleted_id = "22222222-2222-4222-8222-222222222222";
        let orphan_ws_id = "33333333-3333-4333-8333-333333333333";

        let active_jsonl = proj.join(format!("{active_id}.jsonl"));
        fs::write(
            &active_jsonl,
            format!(
                "{}\n{}\n",
                json!({"type": "session-meta", "cwd": "E:/demo-app", "timestamp": 1700000000000i64}),
                json!({"id": "m1", "parentId": null, "type": "message", "role": "user", "timestamp": 1700000001000i64, "content": [{"type": "input_text", "text": "Hello WorkBuddy"}]})
            ),
        )
        .unwrap();

        // Subagent file must never be scanned as a top-level session
        let subagents_dir = proj.join(active_id).join("subagents");
        fs::create_dir_all(&subagents_dir).unwrap();
        fs::write(subagents_dir.join("agent-9999.jsonl"), "{}\n").unwrap();

        let conn = create_workbuddy_db(&home);
        conn.execute(
            "INSERT INTO sessions (id, cwd, title, custom_title, created_at, updated_at, deleted_at, transport)
             VALUES (?1, ?2, ?3, ?4, 1700000000, 1700000005, -1, 'local')",
            [active_id, "E:/demo-app", "Auto Title", "Custom Pinned Title"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, cwd, title, created_at, updated_at, deleted_at, transport)
             VALUES (?1, ?2, ?3, 1700000010, 1700000020, 1700000030, 'local')",
            [soft_deleted_id, "E:/old-project", "Soft Deleted Task"],
        )
        .unwrap();
        drop(conn);

        let ws_sessions = home.join("workspace").join("sessions");
        fs::create_dir_all(ws_sessions.join(orphan_ws_id)).unwrap();
        fs::create_dir_all(ws_sessions.join("editor-sdk-internal")).unwrap();

        let cache = FileParseCache::new();
        let scanned = scan_sessions_in_homes(std::slice::from_ref(&home), &cache);
        assert_eq!(scanned.len(), 3);

        let active = scanned.iter().find(|s| s.session_id == active_id).unwrap();
        assert!(!active.residual);
        assert_eq!(active.title.as_deref(), Some("Custom Pinned Title"));
        assert_eq!(active.project_dir.as_deref(), Some("E:/demo-app"));
        assert_eq!(
            active.source_path.as_deref(),
            Some(active_jsonl.to_string_lossy().as_ref())
        );

        let soft = scanned
            .iter()
            .find(|s| s.session_id == soft_deleted_id)
            .unwrap();
        assert!(soft.residual);
        assert_eq!(soft.title.as_deref(), Some("Soft Deleted Task"));
        assert!(soft
            .source_path
            .as_deref()
            .unwrap()
            .starts_with(SOURCE_PREFIX));

        let orphan = scanned
            .iter()
            .find(|s| s.session_id == orphan_ws_id)
            .unwrap();
        assert!(orphan.residual);
    }

    #[test]
    fn load_messages_handles_streaming_updates_forks_compaction_and_reused_ids() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        let lines = [
            json!({"type": "session-meta", "cwd": "E:/proj", "timestamp": 1000}),
            // Turn 1 user prompt (streaming duplicate ID)
            json!({"id": "u1", "parentId": null, "type": "message", "role": "user", "timestamp": 1001, "content": [{"type": "input_text", "text": "partial"}]}),
            json!({"id": "u1", "parentId": null, "type": "message", "role": "user", "timestamp": 1001, "content": [{"type": "input_text", "text": "First question"}, {"type": "image_blob_ref", "original_filename": "ui.png"}]}),
            // ai-title with parentId=null must NOT hijack the chain leaf!
            json!({"id": "title-1", "parentId": null, "type": "ai-title", "aiTitle": "Session Title"}),
            // Abandoned branch that gets superseded by resend-fork-notice
            json!({"id": "a_abandoned", "parentId": "u1", "type": "message", "role": "assistant", "timestamp": 1002, "content": [{"type": "output_text", "text": "Abandoned answer"}]}),
            json!({"id": "fork1", "parentId": null, "editedUserItemId": "u1", "type": "resend-fork-notice", "timestamp": 1003}),
            json!({"id": "u1_edited", "parentId": "fork1", "type": "message", "role": "user", "timestamp": 1004, "content": [{"type": "input_text", "text": "Edited first question"}]}),
            // Reasoning via rawContent.reasoning_text + function_call + function_call_result
            json!({"id": "r1", "parentId": "u1_edited", "type": "reasoning", "timestamp": 1005, "content": [], "rawContent": [{"type": "reasoning_text", "text": "Step-by-step thinking"}]}),
            json!({"id": "fc1", "parentId": "r1", "type": "function_call", "name": "read_file", "arguments": "{\"path\":\"src/main.rs\"}", "timestamp": 1006}),
            json!({"id": "fcr1", "parentId": "fc1", "type": "function_call_result", "name": "read_file", "status": "completed", "output": {"type": "text", "text": "fn main() {}"}, "timestamp": 1007}),
            json!({"id": "a1", "parentId": "fcr1", "type": "message", "role": "assistant", "timestamp": 1008, "content": [{"type": "output_text", "text": "Answer 1"}]}),
            // Context compaction summary linked via logicalParentId
            json!({"id": "compact1", "parentId": null, "logicalParentId": "a1", "type": "message", "role": "user", "timestamp": 1009, "providerData": {"isCompacted": true, "isCompactInternal": true, "isSummary": true}, "content": [{"type": "input_text", "text": "Internal giant summary"}]}),
            // Reused ID with a DIFFERENT parentId (must not create a cycle!)
            json!({"id": "u1_edited", "parentId": "compact1", "type": "message", "role": "user", "timestamp": 1010, "content": [{"type": "input_text", "text": "Follow-up after compaction"}]}),
            json!({"id": "a2", "parentId": "u1_edited", "type": "message", "role": "assistant", "timestamp": 1011, "content": [{"type": "output_text", "text": "Final answer"}]}),
        ];

        let body = lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, body).unwrap();

        let messages = load_messages(&path).unwrap();
        let roles: Vec<&str> = messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(
            roles,
            vec![
                "user",
                "assistant",
                "assistant",
                "tool",
                "assistant",
                "user",
                "assistant",
            ]
        );
        assert_eq!(messages[0].content, "Edited first question");
        assert_eq!(messages[0].turn_id.as_deref(), Some("t1"));
        assert!(matches!(
            &messages[1].blocks[0],
            SessionBlock::Thinking { text, redacted: false, .. } if text == "Step-by-step thinking"
        ));
        assert!(matches!(
            &messages[2].blocks[0],
            SessionBlock::ToolCall { id, raw_name, .. } if id == "call_1" && raw_name == "read_file"
        ));
        assert!(matches!(
            &messages[3].blocks[0],
            SessionBlock::ToolResult { call_id, status: ToolStatus::Success, preview, .. }
                if call_id == "call_1" && preview == "fn main() {}"
        ));
        assert_eq!(messages[4].content, "Answer 1");
        assert_eq!(messages[5].content, "Follow-up after compaction");
        assert_eq!(messages[5].turn_id.as_deref(), Some("t2"));
        assert_eq!(messages[6].content, "Final answer");
        assert_eq!(messages[6].turn_id.as_deref(), Some("t2"));
    }

    #[test]
    fn load_messages_emits_attachments_hooks_redacted_thinking_diffs_events_and_usage() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join(".workbuddy-ai");
        let proj = home.join("projects").join("e-proj");
        fs::create_dir_all(&proj).unwrap();

        let img_path = temp.path().join("screen.png");
        fs::write(&img_path, b"fakepng").unwrap();
        let img_str = img_path.to_string_lossy().to_string();

        let id = "77777777-7777-4777-8777-777777777777";
        let jsonl = proj.join(format!("{id}.jsonl"));
        let lines = [
            json!({
                "id": "u1",
                "parentId": null,
                "type": "message",
                "role": "user",
                "timestamp": 1000,
                "content": [{
                    "type": "input_text",
                    "text": format!("<attachment path=\"{img_str}\" name=\"screen.png\" />\n<user_query>edit lib.rs</user_query>")
                }]
            }),
            json!({
                "id": "r_redacted",
                "parentId": "u1",
                "type": "reasoning",
                "timestamp": 1001,
                "signature": "sig-123",
                "rawContent": []
            }),
            json!({
                "id": "fc_edit",
                "parentId": "r_redacted",
                "type": "function_call",
                "name": "Edit",
                "arguments": "{\"file_path\":\"src/lib.rs\",\"old_string\":\"a\\nb\",\"new_string\":\"a\\nc\\nd\"}",
                "timestamp": 1002
            }),
            json!({
                "id": "fcr_edit",
                "parentId": "fc_edit",
                "type": "function_call_result",
                "name": "Edit",
                "status": "completed",
                "output": {"type": "text", "text": "Updated src/lib.rs"},
                "timestamp": 1003
            }),
            json!({
                "id": "u_hook",
                "parentId": "fcr_edit",
                "type": "message",
                "role": "user",
                "timestamp": 1004,
                "extra": {"input_from_hook": true},
                "content": [{"type": "input_text", "text": "hook lint reminder"}]
            }),
            json!({
                "id": "cb1",
                "parentId": "u_hook",
                "type": "system",
                "subtype": "compact_boundary",
                "content": "Conversation compacted",
                "timestamp": 1005
            }),
            json!({
                "id": "a_abort",
                "parentId": "cb1",
                "type": "message",
                "role": "assistant",
                "timestamp": 1006,
                "extra": {"interrupted": true},
                "providerData": {
                    "requestModelId": "custom-local:deepseek-v3",
                    "rawUsage": {
                        "prompt_tokens": 500,
                        "completion_tokens": 25,
                        "prompt_cache_hit_tokens": 400,
                        "completion_thinking_tokens": 5
                    }
                },
                "content": [{"type": "output_text", "text": "Partial reply"}]
            }),
        ];
        fs::write(
            &jsonl,
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let conn = create_workbuddy_db(&home);
        drop(conn);

        let sqlite_src = source_reference(&home.join(DB_FILENAME), id);
        let (resolved_home, resolved_jsonl) =
            resolve_sqlite_jsonl_for_content_in_roots(std::slice::from_ref(&home), &sqlite_src)
                .unwrap();
        assert!(resolved_jsonl.starts_with(&resolved_home));

        let messages =
            load_messages_sqlite_in_roots(std::slice::from_ref(&home), &sqlite_src).unwrap();
        assert_eq!(messages.len(), 7);
        assert!(matches!(
            &messages[0].blocks[1],
            SessionBlock::Image { image } if matches!(&image.source, ImageSource::LocalFile { path } if path == &img_str)
        ));
        assert!(matches!(
            &messages[1].blocks[0],
            SessionBlock::Thinking { redacted: true, .. }
        ));
        assert!(matches!(
            &messages[2].blocks[0],
            SessionBlock::ToolCall { diff: Some(diff), .. } if diff.added == 2 && diff.removed == 1
        ));
        assert!(messages[4].injected);
        assert_eq!(messages[4].turn_id.as_deref(), Some("t1"));
        assert!(matches!(
            &messages[5].blocks[0],
            SessionBlock::Event {
                kind: EventKind::Compaction,
                ..
            }
        ));
        assert!(matches!(
            &messages[6].blocks[1],
            SessionBlock::Event {
                kind: EventKind::Aborted,
                ..
            }
        ));
        let meta = messages[6].meta.as_ref().unwrap();
        assert_eq!(meta.model.as_deref(), Some("deepseek-v3"));
        assert_eq!(meta.input_tokens, Some(100));
        assert_eq!(meta.cache_read_tokens, Some(400));
        assert_eq!(meta.output_tokens, Some(25));
        assert_eq!(meta.reasoning_tokens, Some(5));
    }

    #[test]
    fn delete_session_cleans_all_sidecars_and_blocks_active_automation() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join(".workbuddy-ai");
        let roots = vec![home.clone()];
        let session_id = "44444444-4444-4444-8444-444444444444";
        let other_id = "55555555-5555-4555-8555-555555555555";

        let proj = home.join("projects").join("e-my-proj");
        fs::create_dir_all(proj.join(session_id).join("subagents")).unwrap();
        let jsonl = proj.join(format!("{session_id}.jsonl"));
        fs::write(&jsonl, "{}\n").unwrap();
        fs::write(
            proj.join(format!("{session_id}.file-rollback.ndjson")),
            "{}\n",
        )
        .unwrap();
        fs::write(
            proj.join(session_id)
                .join("subagents")
                .join("agent-1.jsonl"),
            "{}\n",
        )
        .unwrap();

        // Populate all sidecars
        for dir in [
            home.join("workspace").join("sessions").join(session_id),
            home.join("file-history").join(session_id),
            home.join("changes-detail").join(session_id),
            home.join("tasks").join(session_id),
        ] {
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("data.bin"), b"payload").unwrap();
        }
        for file in [
            home.join("changes-index")
                .join(format!("{session_id}.json")),
            home.join("artifact-index")
                .join(format!("{session_id}.json")),
            home.join("file-tree-manifests")
                .join(format!("{session_id}.json")),
        ] {
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, "{}").unwrap();
        }

        // Snapshot & PID lockfile
        let user_dir = home.join("user-123");
        fs::create_dir_all(&user_dir).unwrap();
        let snapshot_path = user_dir.join("sidebar-list-snapshot.json");
        fs::write(
            &snapshot_path,
            serde_json::to_string(&json!({
                "items": [
                    {"id": session_id, "title": "Delete Me"},
                    {"id": other_id, "title": "Keep Me"}
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let locks_dir = home.join("sessions");
        fs::create_dir_all(&locks_dir).unwrap();
        fs::write(
            locks_dir.join("99999.json"),
            serde_json::to_string(&json!({"pid": 99999, "sessionId": session_id})).unwrap(),
        )
        .unwrap();

        // Databases
        let conn = create_workbuddy_db(&home);
        conn.execute(
            "INSERT INTO sessions (id, cwd, created_at, updated_at) VALUES (?1, 'E:/p', 1, 2)",
            [session_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_usage (session_id, used, size, updated_at) VALUES (?1, 10, 20, 2)",
            [session_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO automation_runtime_state (automation_id, running, running_conversation_id)
             VALUES ('auto-1', 1, ?1)",
            [session_id],
        )
        .unwrap();
        drop(conn);

        let edge_db = home.join("edge-sync-mapping-v4.db");
        let edge_conn = Connection::open(&edge_db).unwrap();
        edge_conn
            .execute_batch(
                "CREATE TABLE edge_sync_mapping (
                    session_id TEXT PRIMARY KEY,
                    conversation_id TEXT NOT NULL
                );",
            )
            .unwrap();
        edge_conn
            .execute(
                "INSERT INTO edge_sync_mapping (session_id, conversation_id) VALUES (?1, 'c1')",
                [session_id],
            )
            .unwrap();
        drop(edge_conn);

        // Active automation must block preflight & deletion!
        let err = preflight_delete_in_roots(&roots, &jsonl, session_id).unwrap_err();
        assert!(err.contains("自动化"));
        assert!(jsonl.exists());

        // Mark automation stopped -> deletion succeeds and cleans all 13 artifacts
        let conn = Connection::open(home.join(DB_FILENAME)).unwrap();
        conn.execute("UPDATE automation_runtime_state SET running = 0", [])
            .unwrap();
        drop(conn);

        preflight_delete_in_roots(&roots, &jsonl, session_id).unwrap();
        assert!(delete_session_in_roots(&roots, &jsonl, session_id).unwrap());

        assert!(!jsonl.exists());
        assert!(!proj.exists()); // Empty project folder cleaned up
        assert!(!home
            .join("workspace")
            .join("sessions")
            .join(session_id)
            .exists());
        assert!(!home.join("file-history").join(session_id).exists());
        assert!(!home.join("changes-detail").join(session_id).exists());
        assert!(!home.join("tasks").join(session_id).exists());
        assert!(!home
            .join("changes-index")
            .join(format!("{session_id}.json"))
            .exists());
        assert!(!home
            .join("artifact-index")
            .join(format!("{session_id}.json"))
            .exists());
        assert!(!home
            .join("file-tree-manifests")
            .join(format!("{session_id}.json"))
            .exists());
        assert!(!locks_dir.join("99999.json").exists());

        let updated_snapshot: Value =
            serde_json::from_str(&fs::read_to_string(&snapshot_path).unwrap()).unwrap();
        let remaining_ids: Vec<&str> = updated_snapshot["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.get("id").and_then(Value::as_str))
            .collect();
        assert_eq!(remaining_ids, vec![other_id]);

        let conn = Connection::open(home.join(DB_FILENAME)).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);

        let edge_conn = Connection::open(&edge_db).unwrap();
        let edge_count: i64 = edge_conn
            .query_row(
                "SELECT COUNT(*) FROM edge_sync_mapping WHERE session_id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(edge_count, 0);
    }

    #[test]
    fn unwraps_user_query_and_skips_is_meta_entries_across_scan_and_load() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join(".workbuddy-ai");
        let proj = home.join("projects").join("e-proj");
        fs::create_dir_all(&proj).unwrap();

        let id = "66666666-6666-4666-8666-666666666666";
        let jsonl = proj.join(format!("{id}.jsonl"));
        let lines = [
            json!({
                "id": "m_meta_head",
                "parentId": null,
                "type": "message",
                "role": "user",
                "timestamp": 1000,
                "providerData": {"isMeta": true},
                "content": [{"type": "input_text", "text": "<system-reminder>internal boot reminder</system-reminder>"}]
            }),
            json!({
                "id": "u1",
                "parentId": "m_meta_head",
                "type": "message",
                "role": "user",
                "timestamp": 1001,
                "content": [
                    {"type": "input_text", "text": "<system-reminder>cwd context</system-reminder>\n<user_query>list txt files</user_query>"},
                    {"type": "image_blob_ref", "original_filename": "shot.png"}
                ]
            }),
            json!({
                "id": "m_meta_mid",
                "parentId": "u1",
                "type": "message",
                "role": "user",
                "timestamp": 1002,
                "providerData": {"isMeta": true},
                "content": [{"type": "input_text", "text": "<system-reminder>use task tools</system-reminder>"}]
            }),
            json!({
                "id": "a1",
                "parentId": "m_meta_mid",
                "type": "message",
                "role": "assistant",
                "timestamp": 1003,
                "content": [{"type": "output_text", "text": "Found 2 files."}]
            }),
        ];
        fs::write(
            &jsonl,
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let cache = FileParseCache::new();
        let scanned = scan_sessions_in_homes(std::slice::from_ref(&home), &cache);
        assert_eq!(scanned.len(), 1);
        assert_eq!(
            scanned[0].title.as_deref(),
            Some("list txt files\n[Image: shot.png]")
        );
        assert_eq!(scanned[0].summary.as_deref(), Some("Found 2 files."));
        assert!(scanned[0].resume_command.is_none());

        let msgs = load_messages(&jsonl).unwrap();
        let pairs: Vec<(&str, &str)> = msgs
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("user", "list txt files\n[Image: shot.png]"),
                ("assistant", "Found 2 files."),
            ]
        );
    }

    #[test]
    #[ignore]
    fn audit_real_workbuddy_sessions_read_only() {
        let sessions = scan_sessions();
        for s in &sessions {
            let src = s.source_path.as_deref().expect("source_path present");
            if src.starts_with(SOURCE_PREFIX) {
                let _ = load_messages_sqlite(src).expect("sqlite source loads cleanly");
            } else {
                let msgs = load_messages(Path::new(src)).expect("jsonl source loads cleanly");
                assert!(!msgs.is_empty(), "expected non-empty messages for {}", src);
            }
        }
    }
}
