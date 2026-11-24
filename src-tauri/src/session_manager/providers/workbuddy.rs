use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use serde_json::Value;

use crate::session_manager::cache;
use crate::session_manager::{SessionMessage, SessionMeta};
use crate::session_paths::workbuddy_homes;

use super::utils::{
    atomic_write_json, checked_storage_child, ensure_readable_size, extract_text,
    extract_thinking_text, is_reparse_or_symlink, is_safe_session_id, parse_timestamp_to_ms,
    path_basename, read_head_tail_lines, remove_path_if_exists, scan_warning,
    sqlite_table_has_column, sqlite_table_names, truncate_summary, FileParseCache, TITLE_MAX_CHARS,
};

const PROVIDER_ID: &str = "workbuddy";
const SOURCE_PREFIX: &str = "sqlite-workbuddy:";
const DB_FILENAME: &str = "workbuddy.db";
const HEAD_SCAN_LINES: usize = 80;
const TAIL_SCAN_LINES: usize = 30;
const MAX_SNAPSHOT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LOCKFILE_BYTES: u64 = 1024 * 1024;

const CHAIN_ENTRY_TYPES: &[&str] = &[
    "message",
    "reasoning",
    "function_call",
    "function_call_result",
    "resend-fork-notice",
];

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
                if is_compaction_summary(&val) {
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

/// Reconstruct the active conversation chain in chronological order.
///
/// WorkBuddy appends in-progress and completed items with the same `(id, parentId, logicalParentId)`,
/// branches edited prompts via `resend-fork-notice`, and links auto-compacted segments back to
/// earlier verbatim turns via `logicalParentId`. In rare cases WorkBuddy also reuses a request `id`
/// with a new `parentId` on a later turn; keeping distinct `(id, parentId, logicalParentId)` nodes
/// and resolving parent references strictly to indices `< current_idx` guarantees cycle-free
/// reconstruction of the full conversation history.
fn reconstruct_chain(entries: &[Value]) -> Vec<(&Value, bool)> {
    let mut nodes: Vec<&Value> = Vec::new();
    let mut by_id: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut last_chain_idx: Option<usize> = None;

    for entry in entries {
        let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
        let Some(id) = non_empty_field(entry, "id") else {
            continue;
        };
        let parent_id = non_empty_field(entry, "parentId");
        let logical_parent_id = non_empty_field(entry, "logicalParentId");

        if let Some(indices) = by_id.get(id) {
            if let Some(&prev_idx) = indices.last() {
                let prev = nodes[prev_idx];
                if non_empty_field(prev, "parentId") == parent_id
                    && non_empty_field(prev, "logicalParentId") == logical_parent_id
                {
                    nodes[prev_idx] = entry;
                    if CHAIN_ENTRY_TYPES.contains(&entry_type) {
                        last_chain_idx = Some(prev_idx);
                    }
                    continue;
                }
            }
        }

        let new_idx = nodes.len();
        nodes.push(entry);
        by_id.entry(id).or_default().push(new_idx);
        if CHAIN_ENTRY_TYPES.contains(&entry_type) {
            last_chain_idx = Some(new_idx);
        }
    }

    let Some(start_idx) = last_chain_idx else {
        return entries
            .iter()
            .filter(|e| {
                let t = e.get("type").and_then(Value::as_str).unwrap_or("");
                CHAIN_ENTRY_TYPES.contains(&t)
            })
            .map(|e| (e, false))
            .collect();
    };

    let mut chain = Vec::new();
    let mut cur = Some(start_idx);
    while let Some(idx) = cur {
        let node = nodes[idx];
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

        chain.push((node, used_logical_parent));
        cur = next_idx;
    }

    chain.reverse();
    chain
}

fn extract_workbuddy_message_text(entry: &Value) -> String {
    let Some(content) = entry.get("content") else {
        return String::new();
    };
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(extract_workbuddy_content_item)
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => extract_workbuddy_content_item(content).unwrap_or_default(),
        _ => String::new(),
    }
}

fn extract_workbuddy_content_item(item: &Value) -> Option<String> {
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
        "input_text" | "output_text" | "text" => item
            .get("text")
            .or_else(|| item.get("input_text"))
            .or_else(|| item.get("output_text"))
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => {
            let fallback = extract_text(item);
            (!fallback.trim().is_empty()).then_some(fallback)
        }
    }
}

fn extract_workbuddy_reasoning(entry: &Value) -> String {
    for field in ["content", "rawContent", "summary"] {
        if let Some(val) = entry.get(field) {
            let text = extract_reasoning_blocks(val);
            if !text.trim().is_empty() {
                return text;
            }
        }
    }
    entry
        .get("text")
        .or_else(|| entry.get("thinking"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn extract_reasoning_blocks(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(s) => Some(s.clone()),
                Value::Object(_) => item
                    .get("text")
                    .or_else(|| item.get("thinking"))
                    .or_else(|| item.get("input_text"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                _ => None,
            })
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => val
            .get("text")
            .or_else(|| val.get("thinking"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

fn format_function_call(entry: &Value) -> String {
    let name = entry
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("unknown");
    let args = match entry.get("arguments") {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(v) if !v.is_null() => v.to_string(),
        _ => String::new(),
    };
    if args.is_empty() {
        format!("[Tool: {name}]")
    } else {
        format!("[Tool: {name}]\n{args}")
    }
}

fn format_function_call_result(entry: &Value) -> String {
    let name = entry
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let output_text = match entry.get("output") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(map)) => map
            .get("text")
            .or_else(|| map.get("content"))
            .or_else(|| map.get("error"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| Value::Object(map.clone()).to_string()),
        Some(Value::Array(arr)) => extract_text(&Value::Array(arr.clone())),
        Some(v) if !v.is_null() => v.to_string(),
        _ => String::new(),
    };
    let trimmed = output_text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if name.is_empty() {
        trimmed.to_string()
    } else {
        format!("[Result: {name}]\n{trimmed}")
    }
}

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    ensure_readable_size(path)?;
    let file =
        File::open(path).map_err(|e| format!("Failed to open WorkBuddy session file: {e}"))?;
    let reader = BufReader::new(file);
    let mut entries = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| format!("Failed to read WorkBuddy transcript: {e}"))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            entries.push(value);
        }
    }

    let mut messages = Vec::new();
    for (entry, logical_parent_resolved) in reconstruct_chain(&entries) {
        let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
        let ts = entry.get("timestamp").and_then(parse_timestamp_to_ms);
        match entry_type {
            "reasoning" => {
                let thinking = extract_workbuddy_reasoning(entry);
                if !thinking.trim().is_empty() {
                    messages.push(SessionMessage {
                        role: "thinking".to_string(),
                        content: thinking,
                        ts,
                    });
                }
            }
            "function_call" => {
                let content = format_function_call(entry);
                if !content.trim().is_empty() {
                    messages.push(SessionMessage {
                        role: "tool".to_string(),
                        content,
                        ts,
                    });
                }
            }
            "function_call_result" => {
                let content = format_function_call_result(entry);
                if !content.trim().is_empty() {
                    messages.push(SessionMessage {
                        role: "tool".to_string(),
                        content,
                        ts,
                    });
                }
            }
            "message" => {
                if is_compaction_summary(entry) {
                    if logical_parent_resolved {
                        continue;
                    }
                    let summary_body = extract_workbuddy_message_text(entry);
                    if !summary_body.trim().is_empty() {
                        let content = if summary_body.starts_with("[Compacted Summary]") {
                            summary_body
                        } else {
                            format!("[Compacted Summary]\n{summary_body}")
                        };
                        messages.push(SessionMessage {
                            role: "system".to_string(),
                            content,
                            ts,
                        });
                    }
                    continue;
                }

                let role = entry
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();

                if role == "assistant" {
                    if let Some(content_val) = entry.get("content") {
                        let inline_thinking = extract_thinking_text(content_val);
                        if !inline_thinking.trim().is_empty() {
                            messages.push(SessionMessage {
                                role: "thinking".to_string(),
                                content: inline_thinking,
                                ts,
                            });
                        }
                    }
                }

                let content = extract_workbuddy_message_text(entry);
                if !content.trim().is_empty() {
                    messages.push(SessionMessage { role, content, ts });
                }
            }
            _ => {}
        }
    }

    Ok(messages)
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
        let summary: Vec<(&str, &str)> = messages
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_str()))
            .collect();

        assert_eq!(
            summary,
            vec![
                ("user", "Edited first question"),
                ("thinking", "Step-by-step thinking"),
                ("tool", "[Tool: read_file]\n{\"path\":\"src/main.rs\"}"),
                ("tool", "[Result: read_file]\nfn main() {}"),
                ("assistant", "Answer 1"),
                ("user", "Follow-up after compaction"),
                ("assistant", "Final answer"),
            ]
        );
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
