use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session_manager::{DeleteSessionRequest, SessionMessage, SessionMeta};
use crate::session_paths::{
    codex_cleanup_db_paths, codex_config_dir, codex_sqlite_home_dir, codex_state_db_paths,
    discover_numbered_sqlite_dbs, read_codex_config_text, CODEX_THREAD_HISTORY_DB_FILENAME,
};

use super::utils::{
    checked_storage_child, collect_files_where, collect_files_where_checked, extract_text,
    extract_thinking_text, is_reparse_or_symlink, is_safe_session_id, parse_timestamp_to_ms,
    path_basename, quote_sqlite_identifier, read_head_tail_lines, remove_path_if_exists,
    sqlite_table_names, truncate_summary, FileParseCache, MAX_MESSAGES_FILE_BYTES, TITLE_MAX_CHARS,
};

const PROVIDER_ID: &str = "codex";
const CODEX_SESSION_INDEX_FILENAME: &str = "session_index.jsonl";
const VSCODE_CONTEXT_PREFIX: &str = "# Context from my IDE setup:";
const CODEX_REQUEST_MARKER: &str = "my request for codex";
const MAX_INDEX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SUBAGENT_CLEANUP_NODES: usize = 256;

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
    thread_sections: HashMap<String, crate::session_manager::SidebarSection>,
}

#[derive(Clone, Debug)]
struct HistoryBaseRef {
    thread_id: String,
    end_byte_offset: u64,
}

#[derive(Clone, Debug)]
struct RolloutDescriptor {
    path: PathBuf,
    /// `path` 的比较键：同一份 rollout 在不同入口可能是普通路径、`\\?\` 扩展路径
    /// 或 Windows 8.3 短名，直接比较字符串会把同一文件当成两个。
    key: String,
    session_id: String,
    rollout_id: String,
    created_at: Option<i64>,
    file_len: u64,
    history_base: Option<HistoryBaseRef>,
}

#[derive(Clone, Debug)]
struct HistorySegment {
    path: PathBuf,
    session_id: String,
    end_byte_offset: Option<u64>,
}

#[derive(Clone)]
struct RawCodexSession {
    session_id: String,
    project_dir: Option<String>,
    created_at: Option<i64>,
    last_active_at: Option<i64>,
    first_user_message: Option<String>,
    latest_user_message: Option<String>,
    summary: Option<String>,
    source_path: String,
}

impl RawCodexSession {
    fn into_session_meta(self, thread_titles: &HashMap<String, String>) -> SessionMeta {
        let title = thread_titles
            .get(&self.session_id)
            .filter(|text| !text.trim().is_empty())
            .map(|t| truncate_summary(t, TITLE_MAX_CHARS))
            .or_else(|| {
                self.latest_user_message
                    .filter(|text| !text.trim().is_empty())
                    .map(|t| truncate_summary(&t, TITLE_MAX_CHARS))
            })
            .or_else(|| {
                self.first_user_message
                    .filter(|text| !text.trim().is_empty())
                    .map(|t| truncate_summary(&t, TITLE_MAX_CHARS))
            })
            .or_else(|| {
                self.project_dir
                    .as_deref()
                    .and_then(path_basename)
                    .filter(|text| !text.trim().is_empty())
                    .map(|t| truncate_summary(&t, TITLE_MAX_CHARS))
            });
        let resume_command = is_safe_session_id(&self.session_id)
            .then(|| format!("codex resume {}", self.session_id));

        SessionMeta {
            provider_id: PROVIDER_ID.to_string(),
            session_id: self.session_id,
            residual: false,
            parent_session_id: None,
            archived: false,
            cleanup_pending: false,
            sidebar_section: None,
            title,
            summary: self.summary,
            project_dir: self.project_dir,
            project_name: None,
            created_at: self.created_at,
            last_active_at: self.last_active_at,
            source_path: Some(self.source_path),
            resume_command,
        }
    }
}

#[derive(Clone)]
struct CachedCodexFile {
    descriptor: Option<RolloutDescriptor>,
    session: Option<RawCodexSession>,
}

static PARSE_CACHE: LazyLock<FileParseCache<CachedCodexFile>> = LazyLock::new(FileParseCache::new);

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
        reconcile_indexed_rollouts(
            &mut sessions,
            indexed,
            native_rollouts.as_ref(),
            &thread_titles,
        );
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
                    parent_session_id: None,
                    archived: false,
                    cleanup_pending: true,
                    sidebar_section: None,
                    title: Some(truncate_summary(
                        &format!("仅剩侧边栏索引的任务 {id}"),
                        TITLE_MAX_CHARS,
                    )),
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

fn reconcile_indexed_rollouts(
    sessions: &mut Vec<SessionMeta>,
    indexed: HashMap<String, String>,
    native_rollouts: Option<&HashMap<String, String>>,
    thread_titles: &HashMap<String, String>,
) {
    for (id, source) in indexed {
        let source_key = rollout_path_key(Path::new(&source));
        if let Some(item) = sessions.iter_mut().find(|item| {
            item.session_id == id
                && item
                    .source_path
                    .as_deref()
                    .is_some_and(|path| rollout_path_key(Path::new(path)) == source_key)
        }) {
            let is_archived_file = item.source_path.as_deref().is_some_and(is_archived_rollout);
            if is_archived_file || native_rollouts.is_some_and(|active| !active.contains_key(&id)) {
                item.archived = true;
                item.residual = false;
                item.resume_command = None;
            }
        } else if rollout_is_missing(Path::new(&source)) {
            // If the rollout was moved into `archived_sessions/` while `threads.rollout_path`
            // still points to the old `sessions/` path, reconcile the archived rollout instead
            // of emitting a duplicate `缺少日志的任务` ghost.
            if let Some(archived_item) = sessions.iter_mut().find(|item| {
                item.session_id == id
                    && (item.archived
                        || item.source_path.as_deref().is_some_and(is_archived_rollout))
            }) {
                archived_item.archived = true;
                archived_item.residual = false;
                archived_item.resume_command = None;
                continue;
            }
            sessions.push(SessionMeta {
                provider_id: PROVIDER_ID.into(),
                session_id: id.clone(),
                residual: false,
                parent_session_id: None,
                archived: native_rollouts.is_some_and(|active| !active.contains_key(&id))
                    || is_archived_rollout(&source),
                cleanup_pending: true,
                sidebar_section: None,
                title: thread_titles
                    .get(&id)
                    .filter(|t| !t.trim().is_empty())
                    .map(|t| truncate_summary(t, TITLE_MAX_CHARS))
                    .or_else(|| {
                        Some(truncate_summary(
                            &format!("缺少日志的任务 {id}"),
                            TITLE_MAX_CHARS,
                        ))
                    }),
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
    session_roots_for_config_dir(&codex_config_dir())
}

fn session_roots_for_config_dir(config_dir: &Path) -> Vec<PathBuf> {
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
    let indexed = load_indexed_rollouts()?;
    Ok(should_cleanup_index_from_rollouts(
        session_id,
        source_path,
        indexed.as_ref(),
    ))
}

fn should_cleanup_index_from_rollouts(
    session_id: &str,
    source_path: &str,
    indexed: Option<&HashMap<String, String>>,
) -> bool {
    indexed.is_none_or(|rollouts| {
        rollouts.get(session_id).is_none_or(|indexed_path| {
            rollout_path_key(Path::new(indexed_path)) == rollout_path_key(Path::new(source_path))
                || (is_archived_rollout(source_path)
                    && !is_archived_rollout(indexed_path)
                    && rollout_is_missing(Path::new(indexed_path)))
        })
    })
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
                if rollout_path_key(Path::new(&previous)) != rollout_path_key(Path::new(&source)) {
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
pub fn cleanup_after_delete(session_id: &str) -> Result<(), String> {
    let config_dir = codex_config_dir();
    // 读不到 config.toml 时不能退化成「用默认库路径」：用户可能通过 `sqlite_home`
    // 把索引库挪到了别处，那样清理会落到错误的库上、同时漏掉真正要清的那一个。
    // 这里在读配置失败时直接报错；调用方在本函数之后才删 log，所以失败是安全的。
    let config_text = read_codex_config_text()?;
    let mut errors = Vec::new();

    let (subagent_ids, db_errors) =
        cleanup_index_databases_inner(&config_dir, &config_text, session_id);
    errors.extend(db_errors);

    let mut all_ids = Vec::with_capacity(1 + subagent_ids.len());
    all_ids.extend(subagent_ids);
    all_ids.push(session_id.to_string());

    if let Err(error) =
        rewrite_session_index_ids(&config_dir.join(CODEX_SESSION_INDEX_FILENAME), &all_ids)
    {
        errors.push(error);
    }

    if let Err(error) =
        cleanup_global_state_ids(&config_dir.join(".codex-global-state.json"), &all_ids)
    {
        errors.push(error);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        // 索引清理不完整必须让调用方看到：否则 Codex 桌面端会留下打不开的空条目，
        // 而删除会被报成完全成功。
        Err(format!(
            "Codex 索引未能完整清理，请在退出 Codex 后重试：{}",
            errors.join("；")
        ))
    }
}

#[cfg(test)]
fn cleanup_index_databases(
    config_dir: &Path,
    config_text: &str,
    session_id: &str,
) -> Result<(), String> {
    let (_, errors) = cleanup_index_databases_inner(config_dir, config_text, session_id);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("；"))
    }
}

fn cleanup_index_databases_inner(
    config_dir: &Path,
    config_text: &str,
    session_id: &str,
) -> (Vec<String>, Vec<String>) {
    let mut errors = Vec::new();
    let sqlite_dir = codex_sqlite_home_dir(config_dir, config_text);
    let db_paths = codex_cleanup_db_paths(&sqlite_dir);
    let roots = session_roots_for_config_dir(config_dir);

    let mut subagent_ids = Vec::new();
    let mut subagent_id_set = HashSet::new();
    let mut subagent_files = Vec::new();
    let mut subagent_file_set = HashSet::new();

    for db_path in &db_paths {
        let is_state_db = db_path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name.starts_with("state"));
        if !is_state_db {
            continue;
        }
        let (found_ids, found_files) = discover_hidden_subagents_in_db(db_path, session_id, &roots);
        for id in found_ids {
            if subagent_id_set.insert(id.clone()) {
                subagent_ids.push(id);
            }
        }
        for file in found_files {
            if subagent_file_set.insert(file.clone()) {
                subagent_files.push(file);
            }
        }
    }

    // Order deepest spawned children first, parent `session_id` last.
    subagent_ids.reverse();
    let mut ids_to_clean = Vec::with_capacity(subagent_ids.len() + 1);
    ids_to_clean.extend(subagent_ids.iter().cloned());
    ids_to_clean.push(session_id.to_string());

    for db_path in &db_paths {
        if let Err(error) = cleanup_state_db_ids(db_path, &ids_to_clean) {
            errors.push(error);
        }
    }

    match codex_desktop_db_paths(config_dir) {
        Ok(paths) => {
            for path in paths {
                if let Err(error) = cleanup_desktop_catalog_db(&path, &ids_to_clean) {
                    errors.push(error);
                }
            }
        }
        Err(error) => errors.push(error),
    }

    for subagent_file in subagent_files {
        if let Err(error) = remove_path_if_exists(&subagent_file) {
            errors.push(format!(
                "清理 Codex 子代理日志 {} 失败：{error}",
                subagent_file.display()
            ));
        }
    }

    (subagent_ids, errors)
}

#[cfg_attr(not(test), allow(dead_code))]
fn codex_primary_thread_history_db_path(sqlite_dir: &Path) -> PathBuf {
    discover_numbered_sqlite_dbs(
        sqlite_dir,
        "thread_history",
        CODEX_THREAD_HISTORY_DB_FILENAME,
    )
    .into_iter()
    .next()
    .unwrap_or_else(|| sqlite_dir.join(CODEX_THREAD_HISTORY_DB_FILENAME))
}

fn discover_hidden_subagents_in_db(
    db_path: &Path,
    root_session_id: &str,
    roots: &[PathBuf],
) -> (Vec<String>, Vec<PathBuf>) {
    if !db_path.is_file() || !is_safe_session_id(root_session_id) {
        return (Vec::new(), Vec::new());
    }
    let Ok(conn) = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return (Vec::new(), Vec::new());
    };
    let _ = conn.busy_timeout(Duration::from_secs(2));

    let Ok(table_names) = sqlite_table_names(&conn) else {
        return (Vec::new(), Vec::new());
    };
    if !table_names.iter().any(|t| t == "thread_spawn_edges")
        || !table_names.iter().any(|t| t == "threads")
    {
        return (Vec::new(), Vec::new());
    }

    let edge_cols: HashSet<String> = conn
        .prepare("PRAGMA table_info(\"thread_spawn_edges\")")
        .ok()
        .and_then(|mut s| {
            s.query_map([], |r| r.get::<_, String>(1))
                .ok()
                .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();
    if !edge_cols.contains("parent_thread_id") || !edge_cols.contains("child_thread_id") {
        return (Vec::new(), Vec::new());
    }

    let thread_cols: HashSet<String> = conn
        .prepare("PRAGMA table_info(\"threads\")")
        .ok()
        .and_then(|mut s| {
            s.query_map([], |r| r.get::<_, String>(1))
                .ok()
                .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();
    if !thread_cols.contains("id") || !thread_cols.contains("rollout_path") {
        return (Vec::new(), Vec::new());
    }
    let has_source_col = thread_cols.contains("source");

    let mut queue = VecDeque::new();
    let mut visited = HashSet::new();
    visited.insert(root_session_id.to_string());
    queue.push_back(root_session_id.to_string());

    let mut subagent_ids = Vec::new();
    let mut subagent_files = Vec::new();

    while let Some(parent_id) = queue.pop_front() {
        if subagent_ids.len() >= MAX_SUBAGENT_CLEANUP_NODES {
            break;
        }
        let Ok(mut edge_stmt) = conn
            .prepare("SELECT child_thread_id FROM thread_spawn_edges WHERE parent_thread_id = ?1")
        else {
            break;
        };
        let child_ids: Vec<String> = edge_stmt
            .query_map([parent_id.as_str()], |row| row.get::<_, String>(0))
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .collect();

        for child_id in child_ids {
            let child_id = child_id.trim().to_string();
            if !is_safe_session_id(&child_id) || !visited.insert(child_id.clone()) {
                continue;
            }

            let row_info: Option<(String, Option<String>)> = if has_source_col {
                conn.query_row(
                    "SELECT rollout_path, source FROM threads WHERE id = ?1",
                    [child_id.as_str()],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
                )
                .ok()
            } else {
                conn.query_row(
                    "SELECT rollout_path FROM threads WHERE id = ?1",
                    [child_id.as_str()],
                    |r| Ok((r.get::<_, String>(0)?, None)),
                )
                .ok()
            };

            let Some((rollout_path, db_source)) = row_info else {
                continue;
            };
            let rollout_trimmed = rollout_path.trim();
            if rollout_trimmed.is_empty() {
                continue;
            }

            let raw_path = Path::new(rollout_trimmed);
            let plain = plain_rollout_path(raw_path);
            let mut matched_files = Vec::new();
            let mut confirmed_subagent = false;
            let mut rejected = false;

            for candidate in [plain.with_extension("jsonl.zst"), plain.clone()] {
                let meta = match std::fs::symlink_metadata(&candidate) {
                    Ok(meta) => meta,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(_) => {
                        rejected = true;
                        break;
                    }
                };
                if !meta.is_file() || is_reparse_or_symlink(&meta) {
                    rejected = true;
                    break;
                }
                let Some(validated) = roots
                    .iter()
                    .filter(|r| r.exists())
                    .find_map(|root| checked_storage_child(root, &candidate).ok())
                else {
                    rejected = true;
                    break;
                };
                if !is_codex_subagent_rollout(&validated)
                    || read_subagent_rollout_id(&validated).as_deref() != Some(child_id.as_str())
                {
                    rejected = true;
                    break;
                }
                confirmed_subagent = true;
                matched_files.push(validated);
            }

            if rejected {
                continue;
            }

            if !confirmed_subagent
                && rollout_is_missing(raw_path)
                && db_source.as_deref().is_some_and(is_subagent_db_source)
                && roots
                    .iter()
                    .any(|root| checked_missing_rollout(root, &plain).is_ok())
            {
                confirmed_subagent = true;
            }

            if confirmed_subagent {
                subagent_ids.push(child_id.clone());
                subagent_files.extend(matched_files);
                queue.push_back(child_id);
            }
        }
    }

    (subagent_ids, subagent_files)
}

fn is_subagent_db_source(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("subagent") || trimmed.starts_with("subagent:") {
        return true;
    }
    serde_json::from_str::<Value>(trimmed)
        .ok()
        .is_some_and(|v| is_subagent_source(Some(&v)))
}

fn is_codex_subagent_rollout(path: &Path) -> bool {
    read_subagent_rollout_id(path).is_some()
}

fn read_subagent_rollout_id(path: &Path) -> Option<String> {
    let physical = physical_rollout_path(path).ok()?;
    let reader = BufReader::new(open_rollout(&physical).ok()?.take(4 * 1024 * 1024));
    for line in reader.lines().take(10) {
        let line = line.ok()?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let payload = value.get("payload")?;
        if !is_subagent_source(payload.get("source")) {
            return None;
        }
        let id = payload
            .get("id")
            .or_else(|| payload.get("session_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| is_safe_session_id(id))?;
        return Some(id.to_string());
    }
    None
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

fn cleanup_desktop_catalog_db(path: &Path, session_ids: &[String]) -> Result<(), String> {
    if session_ids.is_empty() {
        return Ok(());
    }
    let mut conn = Connection::open(path).map_err(|error| {
        format!(
            "Codex 桌面索引被占用或无法写入（{}）：{error}",
            path.display()
        )
    })?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|error| format!("无法设置 Codex 桌面索引等待时间：{error}"))?;
    let _ = conn.pragma_update(None, "foreign_keys", "ON");
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| format!("无法开始清理 Codex 桌面索引：{error}"))?;

    let table_names: HashSet<String> = sqlite_table_names(&tx)?.into_iter().collect();

    let mut catalog_changed = false;
    for table in [
        "local_thread_catalog",
        "local_thread_catalog_scan_entries",
        "thread_timeline_ledger",
    ] {
        if !table_names.contains(table) {
            continue;
        }
        let quoted = quote_sqlite_identifier(table);
        let sql = format!("DELETE FROM {quoted} WHERE thread_id = ?1");
        for session_id in session_ids {
            let changed = tx
                .execute(&sql, [session_id.as_str()])
                .map_err(|error| format!("清理 Codex 桌面表 {table} 失败：{error}"))?;
            catalog_changed |= table == "local_thread_catalog" && changed > 0;
        }
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

fn read_bounded_text(path: &Path, max_bytes: u64, label: &str) -> Result<String, String> {
    let file = File::open(path).map_err(|e| format!("无法读取 {label}：{e}"))?;
    let len = file
        .metadata()
        .map_err(|e| format!("无法读取 {label} 元数据：{e}"))?
        .len();
    if len > max_bytes {
        return Err(format!(
            "{label}（{}）超过 {} MB 大小上限",
            path.display(),
            max_bytes / 1024 / 1024
        ));
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("无法读取 {label}：{e}"))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!(
            "{label}（{}）超过 {} MB 大小上限",
            path.display(),
            max_bytes / 1024 / 1024
        ));
    }
    String::from_utf8(bytes).map_err(|e| format!("无法解析 {label} UTF-8：{e}"))
}

#[cfg(test)]
fn rewrite_session_index(path: &Path, session_id: &str) -> Result<(), String> {
    rewrite_session_index_ids(path, &[session_id.to_string()])
}

fn rewrite_session_index_ids(path: &Path, session_ids: &[String]) -> Result<(), String> {
    if !path.is_file() || session_ids.is_empty() {
        return Ok(());
    }
    let remove_set: HashSet<&str> = session_ids.iter().map(String::as_str).collect();
    let text = read_bounded_text(path, MAX_INDEX_FILE_BYTES, "Codex 会话索引")?;
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
                        .map(|id| !remove_set.contains(id))
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

#[cfg(test)]
fn cleanup_state_db(path: &Path, session_id: &str) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    let roots = path
        .parent()
        .map(session_roots_for_config_dir)
        .unwrap_or_else(session_roots);
    let (mut subagent_ids, subagent_files) =
        discover_hidden_subagents_in_db(path, session_id, &roots);
    subagent_ids.reverse();
    let mut ids_to_clean = Vec::with_capacity(subagent_ids.len() + 1);
    ids_to_clean.extend(subagent_ids);
    ids_to_clean.push(session_id.to_string());

    cleanup_state_db_ids(path, &ids_to_clean)?;
    for subagent_file in subagent_files {
        remove_path_if_exists(&subagent_file).map_err(|e| {
            format!(
                "清理 Codex 子代理日志 {} 失败：{e}",
                subagent_file.display()
            )
        })?;
    }
    Ok(())
}

fn cleanup_state_db_ids(path: &Path, session_ids: &[String]) -> Result<(), String> {
    if !path.is_file() || session_ids.is_empty() {
        return Ok(());
    }
    let mut conn = Connection::open(path)
        .map_err(|e| format!("Codex 数据库被占用或无法写入（{}）：{e}", path.display()))?;
    // Codex 运行时常持有状态库写锁；启用 Immediate 事务可在加写锁阶段遵守 busy_timeout，
    // 避免 DEFERRED 事务从读升级写时直接报 SQLITE_BUSY。
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| format!("无法设置 Codex 数据库等待时间：{e}"))?;
    let _ = conn.pragma_update(None, "foreign_keys", "ON");
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| format!("无法开始清理 Codex 数据库：{e}"))?;

    let mut table_names = sqlite_table_names(&tx)?;
    // Ensure child/edge tables are cleaned BEFORE entity tables (`threads`, `session`)
    // so foreign keys referencing `threads(id)` or `session(id)` do not block deletion.
    table_names.sort_by_key(|name| matches!(name.as_str(), "threads" | "session"));

    for table in &table_names {
        if table.starts_with("sqlite_") {
            continue;
        }
        let quoted = quote_sqlite_identifier(table);
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

        let is_entity_table = matches!(table.as_str(), "threads" | "session");
        if !is_entity_table {
            for column in ["thread_id", "session_id"] {
                if columns.iter().any(|item| item == column) {
                    let sql = format!("DELETE FROM {quoted} WHERE \"{column}\" = ?1");
                    for session_id in session_ids {
                        tx.execute(&sql, [session_id.as_str()])
                            .map_err(|e| format!("清理 Codex 表 {table} 失败：{e}"))?;
                    }
                }
            }
        }

        if table == "thread_spawn_edges" {
            for column in [
                "parent_thread_id",
                "child_thread_id",
                "parent_session_id",
                "child_session_id",
            ] {
                if columns.iter().any(|item| item == column) {
                    let sql = format!("DELETE FROM {quoted} WHERE \"{column}\" = ?1");
                    for session_id in session_ids {
                        tx.execute(&sql, [session_id.as_str()])
                            .map_err(|e| format!("清理 Codex 表 {table} 失败：{e}"))?;
                    }
                }
            }
        }

        if (table == "agent_job_items" || table.starts_with("agent_job"))
            && columns.iter().any(|item| item == "assigned_thread_id")
        {
            let sql = format!("DELETE FROM {quoted} WHERE \"assigned_thread_id\" = ?1");
            for session_id in session_ids {
                tx.execute(&sql, [session_id.as_str()])
                    .map_err(|e| format!("清理 Codex 表 {table} 失败：{e}"))?;
            }
        }

        if is_entity_table && columns.iter().any(|item| item == "id") {
            let sql = format!("DELETE FROM {quoted} WHERE id = ?1");
            for session_id in session_ids {
                tx.execute(&sql, [session_id.as_str()])
                    .map_err(|e| format!("清理 Codex 表 {table} 失败：{e}"))?;
            }
        }
    }

    tx.commit()
        .map_err(|e| format!("提交 Codex 索引清理失败：{e}"))
}

#[cfg(test)]
fn cleanup_global_state(path: &Path, session_id: &str) -> Result<(), String> {
    cleanup_global_state_ids(path, &[session_id.to_string()])
}

fn cleanup_global_state_ids(path: &Path, session_ids: &[String]) -> Result<(), String> {
    if !path.is_file() || session_ids.is_empty() {
        return Ok(());
    }
    let original = read_bounded_text(path, MAX_INDEX_FILE_BYTES, "Codex 全局状态")?;
    let mut value: Value =
        serde_json::from_str(&original).map_err(|e| format!("Codex 全局状态格式无效：{e}"))?;
    if !value.is_object() {
        return Ok(());
    }
    for session_id in session_ids {
        remove_thread_references(&mut value, session_id);
    }
    let Some(root) = value.as_object_mut() else {
        return Ok(());
    };
    let remove_set: HashSet<&str> = session_ids.iter().map(String::as_str).collect();
    for key in [
        "thread-project-assignments",
        "thread-writable-roots",
        "thread-projectless-output-directories",
        "thread-workspace-root-hints",
    ] {
        if let Some(map) = root.get_mut(key).and_then(Value::as_object_mut) {
            for session_id in session_ids {
                map.remove(session_id);
            }
        }
    }
    if let Some(ids) = root
        .get_mut("projectless-thread-ids")
        .and_then(Value::as_array_mut)
    {
        ids.retain(|item| item.as_str().is_none_or(|id| !remove_set.contains(id)));
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

fn atomic_write(path: &Path, text: String, expected: &str) -> Result<(), String> {
    let mut temp = tempfile::NamedTempFile::new_in(
        path.parent()
            .ok_or_else(|| "索引文件没有父目录".to_string())?,
    )
    .map_err(|e| format!("无法创建临时索引文件：{e}"))?;
    temp.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    temp.flush().map_err(|e| e.to_string())?;
    temp.as_file().sync_all().map_err(|e| e.to_string())?;
    if read_bounded_text(path, MAX_INDEX_FILE_BYTES, "Codex 索引")? != expected {
        return Err("Codex 索引在清理期间发生变化，已停止覆盖；请退出 Agent 后重试".into());
    }
    temp.persist(path)
        .map(|_| ())
        .map_err(|e| format!("无法替换 Codex 索引：{}", e.error))
}

#[cfg(test)]
fn scan_sessions_in_roots(roots: &[PathBuf]) -> Vec<SessionMeta> {
    scan_sessions_in_roots_with_context(
        roots,
        &HashMap::new(),
        &CodexProjectContext::default(),
        None,
    )
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

    let files = preferred_rollout_files(files);
    let parsed = PARSE_CACHE.scan(files, parse_codex_file);
    let mut descriptors = Vec::with_capacity(parsed.len());
    let mut sessions = Vec::with_capacity(parsed.len());
    for entry in parsed {
        if let Some(descriptor) = entry.descriptor {
            descriptors.push(descriptor);
        }
        if let Some(raw) = entry.session {
            let mut meta = raw.into_session_meta(thread_titles);
            meta.sidebar_section = project_context
                .thread_sections
                .get(&meta.session_id)
                .cloned();
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
                let leaf_key = continuation.key.clone();
                let earliest = chain
                    .iter()
                    .filter(|segment| segment.session_id == continuation.session_id)
                    .filter_map(|segment| {
                        descriptors
                            .iter()
                            .find(|descriptor| same_rollout_path(&descriptor.path, &segment.path))
                            .and_then(|descriptor| descriptor.created_at)
                    })
                    .min();
                if let Some(earliest) = earliest {
                    chain_created_at
                        .entry(leaf_key)
                        .and_modify(|existing| *existing = (*existing).min(earliest))
                        .or_insert(earliest);
                }
                for segment in chain
                    .iter()
                    .take(chain.len().saturating_sub(1))
                    .filter(|segment| segment.session_id == continuation.session_id)
                {
                    hidden_paths.insert(rollout_path_key(&segment.path));
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
                    .map(|path| rollout_path_key(Path::new(path)))
                    .unwrap_or_else(|| continuation.key.clone());
                for descriptor in descriptors
                    .iter()
                    .filter(|item| item.session_id == continuation.session_id)
                {
                    if descriptor.key != keep {
                        hidden_paths.insert(descriptor.key.clone());
                    }
                }
            }
        }
    }

    sessions.retain(|session| {
        session
            .source_path
            .as_deref()
            .is_none_or(|path| !hidden_paths.contains(&rollout_path_key(Path::new(path))))
    });
    for session in sessions {
        let Some(path) = session.source_path.as_deref() else {
            continue;
        };
        if let Some(created_at) = chain_created_at.get(&rollout_path_key(Path::new(path))) {
            session.created_at = Some(session.created_at.unwrap_or(*created_at).min(*created_at));
        }
    }
}

fn is_compressed_rollout(path: &Path) -> bool {
    path.to_string_lossy().ends_with(".jsonl.zst")
}

fn plain_rollout_path(path: &Path) -> PathBuf {
    if is_compressed_rollout(path) {
        path.with_extension("")
    } else {
        path.to_path_buf()
    }
}

pub(crate) fn physical_rollout_path(path: &Path) -> Result<PathBuf, String> {
    let plain = plain_rollout_path(path);
    for candidate in [plain.clone(), plain.with_extension("jsonl.zst")] {
        match std::fs::symlink_metadata(&candidate) {
            Ok(meta) if meta.is_file() && !is_reparse_or_symlink(&meta) => return Ok(candidate),
            Ok(_) => return Err("Codex rollout is not a regular file".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("Codex rollout is missing".into())
}

pub(crate) fn rollout_is_missing(path: &Path) -> bool {
    let plain = plain_rollout_path(path);
    [plain.clone(), plain.with_extension("jsonl.zst")].iter().all(|path| {
        matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    })
}

fn is_rollout_file(path: &Path) -> bool {
    path.extension().and_then(|s| s.to_str()) == Some("jsonl") || is_compressed_rollout(path)
}

fn preferred_rollout_files(mut files: Vec<PathBuf>) -> Vec<PathBuf> {
    files.sort_by_key(|path| is_compressed_rollout(path));
    let mut seen = HashSet::new();
    files
        .into_iter()
        .filter(|path| seen.insert(rollout_path_key(path)))
        .collect()
}

fn open_rollout(path: &Path) -> Result<Box<dyn Read>, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    if is_compressed_rollout(path) {
        let mut decoder = zstd::stream::read::Decoder::new(file).map_err(|e| e.to_string())?;
        decoder.window_log_max(27).map_err(|e| e.to_string())?;
        Ok(Box::new(decoder))
    } else {
        Ok(Box::new(file))
    }
}

// All budgets and pagination offsets refer to decompressed bytes. Never allocate
// a whole compressed transcript, nor trust its physical size as a decoding cap.
fn rollout_length(path: &Path, limit: u64) -> Result<u64, String> {
    if !is_compressed_rollout(path) {
        return path.metadata().map(|m| m.len()).map_err(|e| e.to_string());
    }
    let count = std::io::copy(
        &mut open_rollout(path)?.take(limit + 1),
        &mut std::io::sink(),
    )
    .map_err(|e| e.to_string())?;
    if count > limit {
        return Err("Codex decompressed rollout exceeds byte budget".into());
    }
    Ok(count)
}

fn descriptor_rollout_length(path: &Path) -> Result<u64, String> {
    if is_compressed_rollout(path) {
        // Codex 0.166 pledges source length in the zstd frame. Use that for
        // dependency bounds; actual message reads still enforce streaming caps.
        let mut header = [0u8; 18];
        let n = File::open(path)
            .map_err(|e| e.to_string())?
            .read(&mut header)
            .map_err(|e| e.to_string())?;
        if let Ok(Some(length)) = zstd::zstd_safe::get_frame_content_size(&header[..n]) {
            return Ok(length);
        }
    }
    rollout_length(path, MAX_MESSAGES_FILE_BYTES)
}

struct RolloutHeadTail {
    head: Vec<String>,
    tail: Vec<String>,
    known_len: Option<u64>,
}

fn rollout_head_tail_with_len(path: &Path) -> Result<RolloutHeadTail, String> {
    if !is_compressed_rollout(path) {
        let len = path
            .metadata()
            .map(|m| m.len())
            .map_err(|e| e.to_string())?;
        let (head, tail) = read_head_tail_lines(path, 10, 300).map_err(|e| e.to_string())?;
        return Ok(RolloutHeadTail {
            head,
            tail,
            known_len: Some(len),
        });
    }
    let mut reader = BufReader::new(open_rollout(path)?.take(MAX_MESSAGES_FILE_BYTES + 1));
    let mut head = Vec::new();
    let mut tail = std::collections::VecDeque::new();
    let mut total = 0u64;
    loop {
        let mut line = String::new();
        let n = reader
            .by_ref()
            .take(4 * 1024 * 1024 + 1)
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if n > 4 * 1024 * 1024 || total > MAX_MESSAGES_FILE_BYTES {
            return Err("Codex decompressed scan exceeds byte budget".into());
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        if head.len() < 10 {
            head.push(line.clone());
        }
        tail.push_back(line);
        if tail.len() > 300 {
            tail.pop_front();
        }
    }
    Ok(RolloutHeadTail {
        head,
        tail: tail.into_iter().collect(),
        known_len: Some(total),
    })
}

fn parse_descriptor_from_head_lines<'a>(
    path: &Path,
    lines: impl IntoIterator<Item = Result<std::borrow::Cow<'a, str>, String>>,
    known_len: Option<u64>,
) -> Result<RolloutDescriptor, String> {
    for line in lines {
        let line = line?;
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
        let history_base = match payload.get("history_base").filter(|base| !base.is_null()) {
            None => None,
            Some(base) => {
                let thread_id = base
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| format!("Invalid Codex history_base: {}", path.display()))?;
                let end_byte_offset = base
                    .get("end_byte_offset")
                    .and_then(Value::as_u64)
                    .filter(|offset| *offset > 0)
                    .ok_or_else(|| {
                        format!("Invalid Codex history_base offset: {}", path.display())
                    })?;
                Some(HistoryBaseRef {
                    thread_id: thread_id.into(),
                    end_byte_offset,
                })
            }
        };
        let file_len = match known_len {
            Some(len) => len,
            None => descriptor_rollout_length(path)?,
        };
        let rollout_id = rollout_instance_id(path, &session_id);
        return Ok(RolloutDescriptor {
            path: path.to_path_buf(),
            key: rollout_path_key(path),
            session_id,
            rollout_id,
            created_at,
            file_len,
            history_base,
        });
    }

    Err(format!("无法识别 Codex 会话元数据：{}", path.display()))
}

fn read_rollout_descriptor(path: &Path) -> Result<RolloutDescriptor, String> {
    let reader = BufReader::new(open_rollout(path)?.take(4 * 1024 * 1024));
    parse_descriptor_from_head_lines(
        path,
        reader
            .lines()
            .take(10)
            .map(|line| line.map(std::borrow::Cow::Owned).map_err(|e| e.to_string())),
        None,
    )
}

fn rollout_instance_id(path: &Path, session_id: &str) -> String {
    let plain = plain_rollout_path(path);
    let Some(stem) = plain.file_stem().and_then(|name| name.to_str()) else {
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
    rollout_path_key(left) == rollout_path_key(right)
}

/// 路径比较键：先解析为真实路径，再按平台规范化。
///
/// 同一个 rollout 可能以普通路径、`\\?\` 扩展路径或 Windows 8.3 短名的形式
/// 出现（`C:\Users\RUNNER~1\...` 与 `C:\Users\runneradmin\...` 是同一个文件），
/// 纯字符串比较会把它当成两个文件，导致分页链解析失败并拒绝清理。解析失败时
/// 退回字符串表示，保持原有行为。
fn rollout_path_key(path: &Path) -> String {
    let resolved = physical_rollout_path(path)
        .ok()
        .and_then(|path| path.canonicalize().ok())
        .unwrap_or_else(|| path.to_path_buf());
    normalized_rollout_path(&resolved.to_string_lossy())
}

fn resolve_paginated_chain_from_descriptors(
    current_path: &Path,
    descriptors: &[RolloutDescriptor],
) -> Result<Vec<HistorySegment>, String> {
    let mut current = descriptors
        .iter()
        .find(|descriptor| descriptor.key == rollout_path_key(current_path))
        .ok_or_else(|| format!("未找到 Codex 分页会话元数据：{}", current_path.display()))?;
    let mut visited = HashSet::new();
    let mut reversed = vec![HistorySegment {
        path: current.path.clone(),
        session_id: current.session_id.clone(),
        end_byte_offset: None,
    }];
    visited.insert(current.key.clone());

    while let Some(history_base) = &current.history_base {
        if reversed.len() >= 64 {
            return Err(format!(
                "Codex 分页历史链过长，已停止处理：{}",
                current_path.display()
            ));
        }
        let mut candidates = descriptors
            .iter()
            .filter(|candidate| candidate.rollout_id == history_base.thread_id)
            .filter(|candidate| candidate.key != current.key)
            .filter(|candidate| !visited.contains(&candidate.key))
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

        let normalized = base.key.clone();
        if !visited.insert(normalized) {
            return Err(format!(
                "Codex 分页历史形成循环，已停止处理：{}",
                current_path.display()
            ));
        }
        reversed.push(HistorySegment {
            path: base.path.clone(),
            session_id: base.session_id.clone(),
            end_byte_offset: Some(history_base.end_byte_offset),
        });
        current = base;
    }

    reversed.reverse();
    Ok(reversed)
}

#[cfg(test)]
thread_local! {
    static PAGINATED_DIR_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn rollout_descriptors_for_path(path: &Path) -> Vec<RolloutDescriptor> {
    rollout_descriptors_in_roots(path, session_roots())
}

fn rollout_descriptors_in_roots(path: &Path, mut roots: Vec<PathBuf>) -> Vec<RolloutDescriptor> {
    #[cfg(test)]
    PAGINATED_DIR_SCANS.with(|scans| scans.set(scans.get() + 1));

    // canonicalize() adds the Windows extended-length prefix. Compare paths in
    // the same representation before deciding to scan an additional root.
    let canonical_path = path.canonicalize().ok();
    if !roots.iter().any(|root| {
        root.canonicalize().is_ok_and(|root| {
            canonical_path
                .as_ref()
                .is_some_and(|path| path.starts_with(root))
        })
    }) {
        if let Some(parent) = path.parent() {
            roots.push(parent.to_path_buf());
        }
    }
    let mut files = Vec::new();
    for root in roots {
        collect_jsonl_files(&root, &mut files);
    }
    descriptors_for_files(files)
}

fn descriptors_for_files(files: Vec<PathBuf>) -> Vec<RolloutDescriptor> {
    let mut seen = HashSet::new();
    preferred_rollout_files(files)
        .into_iter()
        .filter(|path| seen.insert(rollout_path_key(path)))
        .filter_map(|candidate| read_rollout_descriptor(&candidate).ok())
        .collect()
}

/// Destructive operations must never infer absence from a partial scan.
fn rollout_descriptors_checked(roots: &[PathBuf]) -> Result<Vec<RolloutDescriptor>, String> {
    let mut files = Vec::new();
    for root in roots {
        if !root.is_absolute() {
            return Err("Codex storage root must be absolute".into());
        }
        collect_files_where_checked(root, is_rollout_file, &mut files)?;
    }
    let mut seen = HashSet::new();
    preferred_rollout_files(files)
        .into_iter()
        .filter(|path| seen.insert(rollout_path_key(path)))
        .map(|path| read_rollout_descriptor(&path))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Codex 依赖扫描不完整：{error}"))
}

fn resolve_paginated_chain(path: &Path) -> Result<Vec<HistorySegment>, String> {
    let physical = physical_rollout_path(path)?;
    let descriptor = read_rollout_descriptor(&physical)?;
    if descriptor.history_base.is_none() {
        return Ok(vec![HistorySegment {
            path: descriptor.path,
            session_id: descriptor.session_id,
            end_byte_offset: None,
        }]);
    }
    let descriptors = rollout_descriptors_for_path(&physical);
    resolve_paginated_chain_from_descriptors(&physical, &descriptors)
}

fn mark_residual_rollouts(
    sessions: &mut [SessionMeta],
    native_rollouts: Option<&HashMap<String, String>>,
) {
    for session in sessions.iter_mut() {
        if session
            .source_path
            .as_deref()
            .is_some_and(is_archived_rollout)
        {
            session.archived = true;
            session.resume_command = None;
        }
    }

    if let Some(native_rollouts) = native_rollouts {
        let mut active_present_ids = HashSet::new();
        for session in sessions.iter() {
            if session.archived {
                continue;
            }
            if let Some(current_path) = native_rollouts.get(&session.session_id) {
                let current_key = rollout_path_key(Path::new(current_path));
                if session
                    .source_path
                    .as_deref()
                    .is_some_and(|path| rollout_path_key(Path::new(path)) == current_key)
                {
                    active_present_ids.insert(session.session_id.clone());
                }
            }
        }
        for (id, current_path) in native_rollouts {
            if !is_archived_rollout(current_path) && !rollout_is_missing(Path::new(current_path)) {
                active_present_ids.insert(id.clone());
            }
        }

        let mut primary_archived_by_id: HashMap<String, usize> = HashMap::new();
        for (index, candidate) in sessions.iter().enumerate() {
            if !candidate.archived || active_present_ids.contains(&candidate.session_id) {
                continue;
            }
            let replace = primary_archived_by_id
                .get(&candidate.session_id)
                .is_none_or(|existing_index| {
                    is_newer_rollout(candidate, &sessions[*existing_index])
                });
            if replace {
                primary_archived_by_id.insert(candidate.session_id.clone(), index);
            }
        }

        for (index, session) in sessions.iter_mut().enumerate() {
            if session.archived {
                session.residual =
                    primary_archived_by_id.get(&session.session_id).copied() != Some(index);
            } else {
                session.residual =
                    native_rollouts
                        .get(&session.session_id)
                        .is_none_or(|current_path| {
                            let current_key = rollout_path_key(Path::new(current_path));
                            session
                                .source_path
                                .as_deref()
                                .is_none_or(|path| rollout_path_key(Path::new(path)) != current_key)
                        });
            }
            if session.residual || session.archived {
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
        if candidate.archived {
            continue;
        }
        let replace = current_by_id
            .get(&candidate.session_id)
            .is_none_or(|existing_index| is_newer_rollout(candidate, &sessions[*existing_index]));
        if replace {
            current_by_id.insert(candidate.session_id.clone(), index);
        }
    }
    let mut primary_archived_by_id: HashMap<String, usize> = HashMap::new();
    for (index, candidate) in sessions.iter().enumerate() {
        if !candidate.archived || current_by_id.contains_key(&candidate.session_id) {
            continue;
        }
        let replace = primary_archived_by_id
            .get(&candidate.session_id)
            .is_none_or(|existing_index| is_newer_rollout(candidate, &sessions[*existing_index]));
        if replace {
            primary_archived_by_id.insert(candidate.session_id.clone(), index);
        }
    }
    for (index, session) in sessions.iter_mut().enumerate() {
        if session.archived {
            session.residual =
                primary_archived_by_id.get(&session.session_id).copied() != Some(index);
        } else {
            session.residual = current_by_id.get(&session.session_id).copied() != Some(index);
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
    let path = path
        .strip_suffix(".zst")
        .filter(|plain| plain.ends_with(".jsonl"))
        .unwrap_or(path);
    #[cfg(not(target_os = "windows"))]
    {
        return path.trim_end_matches('/').to_string();
    }
    #[cfg(target_os = "windows")]
    {
        let path = path.replace('/', "\\");
        let without_prefix = path
            .strip_prefix("\\\\?\\UNC\\")
            .map(|unc| format!("\\\\{unc}"))
            .unwrap_or_else(|| path.strip_prefix("\\\\?\\").unwrap_or(&path).to_string());
        without_prefix.trim_end_matches('\\').to_lowercase()
    }
}

/// Read the same project assignments used by the Codex desktop sidebar.  A
/// rollout's `cwd` can point at a temporary worktree, so it is not a reliable
/// project identity and must not be used for grouping when this mapping exists.
fn load_project_context() -> CodexProjectContext {
    let config_dir = codex_config_dir();
    let config_text = read_codex_config_text().unwrap_or_default();
    load_project_context_in(&config_dir, &config_text)
}

fn load_project_context_in(config_dir: &Path, config_text: &str) -> CodexProjectContext {
    let path = config_dir.join(".codex-global-state.json");
    let mut context = read_bounded_text(&path, MAX_INDEX_FILE_BYTES, "Codex 全局状态")
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .map(|root| project_context_from_value(&root))
        .unwrap_or_default();
    for db_path in codex_state_db_paths(config_dir, config_text) {
        let Ok(conn) =
            Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        else {
            continue;
        };
        let _ = conn.busy_timeout(Duration::from_secs(2));
        context.thread_sections.extend(read_sidebar_sections(&conn));
    }
    context
}

fn read_sidebar_sections(
    conn: &Connection,
) -> HashMap<String, crate::session_manager::SidebarSection> {
    // Older Codex databases do not have sections. Keep project grouping there.
    let Ok(mut statement) = conn.prepare("SELECT t.id, s.id, s.name FROM threads t JOIN thread_sections s ON t.thread_section_id = s.id WHERE trim(s.name) != ''") else {
        return HashMap::new();
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            crate::session_manager::SidebarSection {
                id: row.get(1)?,
                name: row.get(2)?,
            },
        ))
    }) else {
        return HashMap::new();
    };
    rows.filter_map(Result::ok).collect()
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
        thread_sections: HashMap::new(),
    }
}

fn load_thread_titles() -> HashMap<String, String> {
    let config_dir = codex_config_dir();
    let config_text = read_codex_config_text().unwrap_or_default();
    load_thread_titles_in(&config_dir, &config_text)
}

fn load_thread_titles_in(config_dir: &Path, config_text: &str) -> HashMap<String, String> {
    let db_paths = codex_state_db_paths(config_dir, config_text);
    load_thread_titles_from_paths(&config_dir.join(CODEX_SESSION_INDEX_FILENAME), &db_paths)
}

fn load_native_active_rollouts() -> Option<HashMap<String, String>> {
    let config_dir = codex_config_dir();
    let config_text = read_codex_config_text().unwrap_or_default();
    load_native_active_rollouts_in(&config_dir, &config_text)
}

fn load_native_active_rollouts_in(
    config_dir: &Path,
    config_text: &str,
) -> Option<HashMap<String, String>> {
    let mut loaded = false;
    let mut rollouts = HashMap::new();
    for db_path in codex_state_db_paths(config_dir, config_text) {
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
                    .then(|| (id.to_string(), rollout_path_key(Path::new(path))))
            })
            .collect(),
    )
}

fn load_thread_titles_from_paths(
    session_index_path: &Path,
    db_paths: &[PathBuf],
) -> HashMap<String, String> {
    let mut titles = HashMap::new();
    let mut explicit_batches = Vec::with_capacity(db_paths.len());
    for db_path in db_paths {
        let (auto, explicit) = load_thread_titles_from_db_pair(db_path);
        titles.extend(auto);
        explicit_batches.push(explicit);
    }
    titles.extend(load_thread_titles_from_session_index(session_index_path));
    for explicit in explicit_batches {
        titles.extend(explicit);
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
    let len = match file.metadata() {
        Ok(meta) => meta.len(),
        Err(err) => {
            log::warn!(
                "Failed to stat Codex session index {}: {err}",
                index_path.display()
            );
            return HashMap::new();
        }
    };
    if len > MAX_INDEX_FILE_BYTES {
        log::warn!(
            "Skipping oversized Codex session index {} ({} bytes > {} bytes)",
            index_path.display(),
            len,
            MAX_INDEX_FILE_BYTES
        );
        return HashMap::new();
    }

    let mut reader = BufReader::new(file.take(MAX_INDEX_FILE_BYTES));
    let mut titles = HashMap::new();
    loop {
        let mut line = String::new();
        let count = match reader
            .by_ref()
            .take(4 * 1024 * 1024 + 1)
            .read_line(&mut line)
        {
            Ok(count) => count,
            Err(_) => break,
        };
        if count == 0 {
            break;
        }
        if count > 4 * 1024 * 1024 {
            log::warn!(
                "Skipping oversized record in Codex session index {}",
                index_path.display()
            );
            break;
        }
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

#[cfg(test)]
fn load_thread_titles_from_db(db_path: &Path) -> HashMap<String, String> {
    let (mut titles, explicit) = load_thread_titles_from_db_pair(db_path);
    titles.extend(explicit);
    titles
}

fn load_thread_titles_from_db_pair(
    db_path: &Path,
) -> (HashMap<String, String>, HashMap<String, String>) {
    if !db_path.exists() {
        return (HashMap::new(), HashMap::new());
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
            return (HashMap::new(), HashMap::new());
        }
    };
    // Codex keeps this DB open and write-locked while running; without a busy
    // timeout a read during a write fails immediately and titles silently drop.
    if let Err(err) = conn.busy_timeout(Duration::from_secs(2)) {
        log::warn!(
            "Failed to set Codex state database busy timeout for {}: {err}",
            db_path.display()
        );
        return (HashMap::new(), HashMap::new());
    }

    // Mirror Codex's own `distinct_thread_metadata_title`: keep a title only
    // when it differs from the first user message. Push the comparison into SQL
    // (NULL-safe) so we never SELECT the unbounded `first_user_message` blob —
    // it can grow large enough to OOM (openai/codex#29007).
    let columns = conn
        .prepare("PRAGMA table_info(threads)")
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(1))
                .ok()
                .map(|rows| rows.flatten().collect::<HashSet<_>>())
        })
        .unwrap_or_default();
    let generated = if columns.contains("first_user_message") {
        "CASE WHEN first_user_message IS NULL OR TRIM(title) <> TRIM(first_user_message) THEN NULLIF(TRIM(title), '') END"
    } else {
        "NULLIF(TRIM(title), '')"
    };
    let query = if columns.contains("name") {
        format!(
            "SELECT id, {generated}, NULLIF(TRIM(name), '') FROM threads WHERE ({generated}) IS NOT NULL OR NULLIF(TRIM(name), '') IS NOT NULL"
        )
    } else {
        // 旧 schema 只有 title；有效的显式标题仍高于旧 JSON 索引。
        // 新 schema 的 title 是自动标题，只有 name 才是显式覆盖。
        format!("SELECT id, NULL, {generated} FROM threads WHERE ({generated}) IS NOT NULL")
    };
    let mut stmt = match conn.prepare(&query) {
        Ok(stmt) => stmt,
        Err(err) => {
            log::warn!(
                "Failed to prepare Codex thread title query for {}: {err}",
                db_path.display()
            );
            return (HashMap::new(), HashMap::new());
        }
    };

    let rows = match stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let auto_title: Option<String> = row.get(1)?;
        let explicit_name: Option<String> = row.get(2)?;
        Ok((id, auto_title, explicit_name))
    }) {
        Ok(rows) => rows,
        Err(err) => {
            log::warn!(
                "Failed to query Codex thread titles from {}: {err}",
                db_path.display()
            );
            return (HashMap::new(), HashMap::new());
        }
    };

    let mut auto = HashMap::new();
    let mut explicit = HashMap::new();
    for (id, auto_title, explicit_name) in rows.flatten() {
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        if let Some(title) = auto_title
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            auto.insert(id.to_string(), title.to_string());
        }
        if let Some(name) = explicit_name
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            explicit.insert(id.to_string(), name.to_string());
        }
    }
    (auto, explicit)
}

/// 正文实际依赖的文件：分页历史链的每一段，以及该段只读到哪个字节偏移
/// （`None` = 读到文件末尾）。
///
/// 正文是「当前文件 + 基座文件的前 N 字节」拼出来的，只看 `path` 会漏掉基座；
/// 而链本身由磁盘上所有 rollout 头决定，链可以在文件一字未改的情况下改变，
/// 所以调用方还要把返回的顺序与偏移一起当作正文身份（见 `cache::sources_digest`）。
pub fn transcript_sources(path: &Path) -> Result<Vec<(PathBuf, Option<u64>)>, String> {
    resolve_paginated_chain(path).map(|segments| {
        segments
            .into_iter()
            .map(|segment| (segment.path, segment.end_byte_offset))
            .collect()
    })
}

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    load_history_segments(resolve_paginated_chain(path)?)
}

fn load_history_segments(segments: Vec<HistorySegment>) -> Result<Vec<SessionMessage>, String> {
    let mut messages = Vec::new();
    let mut remaining_bytes = MAX_MESSAGES_FILE_BYTES;

    // Validate the aggregate before allocating/reading any individual segment.
    for segment in &segments {
        let segment_len = match segment.end_byte_offset {
            Some(offset) => offset,
            None => rollout_length(&segment.path, remaining_bytes)?,
        };
        if segment_len > remaining_bytes {
            return Err(format!(
                "Codex 分页历史合计超过 {} MB 加载上限",
                MAX_MESSAGES_FILE_BYTES / 1024 / 1024
            ));
        }
        remaining_bytes -= segment_len;
    }
    for segment in segments {
        messages.extend(load_segment_messages(
            &segment.path,
            segment.end_byte_offset,
        )?);
    }

    Ok(messages)
}

enum SegmentEntry {
    Primary(SessionMessage),
    ReplacementHistory(SessionMessage),
    EventFallback(SessionMessage),
}

fn load_segment_messages(
    path: &Path,
    end_byte_offset: Option<u64>,
) -> Result<Vec<SessionMessage>, String> {
    let segment_len = match end_byte_offset {
        Some(offset) => offset,
        None => rollout_length(path, MAX_MESSAGES_FILE_BYTES)?,
    };
    if segment_len > MAX_MESSAGES_FILE_BYTES {
        return Err("Codex history segment exceeds message byte budget".into());
    }
    let mut reader = BufReader::new(open_rollout(path)?.take(segment_len));
    let mut consumed = 0u64;
    let mut entries = Vec::new();
    let mut has_response_item_any = false;
    let mut has_response_item_chat = false;
    let mut has_replacement_chat = false;

    loop {
        let mut line = String::new();
        let count = reader
            .by_ref()
            .take(4 * 1024 * 1024 + 1)
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        consumed += count as u64;
        if count > 4 * 1024 * 1024 {
            return Err("Codex record exceeds byte budget".into());
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };

        let ts = value.get("timestamp").and_then(parse_timestamp_to_ms);
        let item_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        let Some(payload) = value.get("payload") else {
            continue;
        };

        match item_type {
            "response_item" => {
                if let Some(msg) = parse_response_item_payload(payload, ts) {
                    has_response_item_any = true;
                    if matches!(msg.role.as_str(), "user" | "assistant") {
                        has_response_item_chat = true;
                    }
                    entries.push(SegmentEntry::Primary(msg));
                }
            }
            "compacted" => {
                if let Some(summary) = payload
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    entries.push(SegmentEntry::Primary(SessionMessage {
                        role: "assistant".to_string(),
                        content: format!("[Compacted Summary]\n{summary}"),
                        ts,
                    }));
                }
                if let Some(history) = payload.get("replacement_history").and_then(Value::as_array)
                {
                    for item in history {
                        if let Some(msg) = parse_response_item_payload(item, ts) {
                            if matches!(msg.role.as_str(), "user" | "assistant") {
                                has_replacement_chat = true;
                            }
                            entries.push(SegmentEntry::ReplacementHistory(msg));
                        }
                    }
                }
            }
            "event_msg" => {
                let event_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
                let role = match event_type {
                    "user_message" => "user",
                    "agent_message" => "assistant",
                    _ => continue,
                };
                let content = payload
                    .get("message")
                    .or_else(|| payload.get("text"))
                    .map(extract_text)
                    .unwrap_or_default();
                if content.trim().is_empty()
                    || (role == "user" && is_internal_user_context(payload, &content))
                {
                    continue;
                }
                entries.push(SegmentEntry::EventFallback(SessionMessage {
                    role: role.to_string(),
                    content,
                    ts,
                }));
            }
            _ => continue,
        }
    }

    if consumed < segment_len {
        return Err("Codex history offset exceeds decompressed length".into());
    }

    let include_replacement = !has_response_item_any;
    let include_event_fallback =
        !(has_response_item_chat || include_replacement && has_replacement_chat);
    let messages = entries
        .into_iter()
        .filter_map(|entry| match entry {
            SegmentEntry::Primary(msg) => Some(msg),
            SegmentEntry::ReplacementHistory(msg) if include_replacement => Some(msg),
            SegmentEntry::EventFallback(msg) if include_event_fallback => Some(msg),
            _ => None,
        })
        .collect();

    Ok(messages)
}

fn parse_response_item_payload(payload: &Value, ts: Option<i64>) -> Option<SessionMessage> {
    let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");

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
                return None;
            }
            (role.to_string(), content)
        }
        "reasoning" => {
            let content = extract_codex_reasoning_text(payload);
            ("thinking".to_string(), content)
        }
        "function_call" => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            ("assistant".to_string(), format!("[Tool: {name}]"))
        }
        "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => (
            "tool".to_string(),
            payload.get("output").map(extract_text).unwrap_or_default(),
        ),
        "custom_tool_call" => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let input = payload.get("input").and_then(Value::as_str).unwrap_or("");
            if input.is_empty() {
                ("assistant".to_string(), format!("[Tool: {name}]"))
            } else {
                ("assistant".to_string(), format!("[Tool: {name}]\n{input}"))
            }
        }
        "local_shell_call" => {
            let cmd = extract_shell_command(payload);
            if cmd.is_empty() {
                return None;
            }
            ("tool".to_string(), format!("$ {cmd}"))
        }
        "web_search_call" => ("tool".to_string(), format_web_search_call(payload)),
        _ => return None,
    };

    if content.trim().is_empty() {
        return None;
    }

    Some(SessionMessage { role, content, ts })
}

fn extract_codex_reasoning_text(payload: &Value) -> String {
    let extracted = extract_thinking_text(payload);
    if !extracted.trim().is_empty() {
        return extracted;
    }
    match payload.get("summary") {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                item.as_str()
                    .or_else(|| item.get("text").and_then(Value::as_str))
            })
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn extract_shell_command(payload: &Value) -> String {
    match payload
        .pointer("/action/command")
        .or_else(|| payload.get("command"))
    {
        Some(Value::Array(args)) => args
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string(),
        Some(Value::String(cmd)) => cmd.trim().to_string(),
        _ => String::new(),
    }
}

fn format_web_search_call(payload: &Value) -> String {
    if let Some(query) = payload
        .pointer("/action/query")
        .or_else(|| payload.get("query"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|q| !q.is_empty())
    {
        return format!("[Web search] {query}");
    }
    if let Some(queries) = payload.pointer("/action/queries").and_then(Value::as_array) {
        let joined = queries
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        if !joined.is_empty() {
            return format!("[Web search] {joined}");
        }
    }
    if let Some(url) = payload
        .pointer("/action/url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        return format!("[Web search] {url}");
    }
    "[Web search]".to_string()
}

pub fn validate_delete(path: &Path, session_id: &str) -> Result<(), String> {
    deletion_paths(path, session_id).map(|_| ())
}

/// A selected child must be deleted before a selected parent. Unselected
/// dependants are still protected by the normal preflight at execution time.
pub fn deletion_order(requests: &[DeleteSessionRequest]) -> Vec<usize> {
    let codex_paths = requests
        .iter()
        .filter(|request| request.provider_id == PROVIDER_ID)
        .map(|request| Path::new(&request.source_path))
        .collect::<Vec<_>>();
    if codex_paths.is_empty() {
        return (0..requests.len()).collect();
    }
    let mut roots = session_roots();
    let mut canonical_roots = roots
        .iter()
        .filter_map(|root| root.canonicalize().ok())
        .collect::<Vec<_>>();
    for path in codex_paths {
        let canonical_path = path.canonicalize().ok();
        if !canonical_roots.iter().any(|root| {
            canonical_path
                .as_ref()
                .is_some_and(|resolved| resolved.starts_with(root))
        }) {
            if let Some(parent) = path.parent().map(Path::to_path_buf) {
                if let Ok(resolved) = parent.canonicalize() {
                    canonical_roots.push(resolved);
                }
                if !roots.contains(&parent) {
                    roots.push(parent);
                }
            }
        }
    }
    let mut files = Vec::new();
    for root in roots {
        collect_jsonl_files(&root, &mut files);
    }
    deletion_order_with_descriptors(requests, &descriptors_for_files(files))
}

fn deletion_order_with_descriptors(
    requests: &[DeleteSessionRequest],
    descriptors: &[RolloutDescriptor],
) -> Vec<usize> {
    let chains = requests
        .iter()
        .map(|request| {
            if request.provider_id != "codex" {
                return Vec::new();
            }
            resolve_paginated_chain_from_descriptors(Path::new(&request.source_path), descriptors)
                .unwrap_or_default() // preflight reports unresolved chains without mutating anything
        })
        .collect::<Vec<_>>();
    let owned = requests
        .iter()
        .zip(&chains)
        .map(|(request, chain)| {
            chain
                .iter()
                .filter(|segment| segment.session_id == request.session_id)
                .map(|segment| rollout_path_key(&segment.path))
                .collect::<HashSet<_>>()
        })
        .collect::<Vec<_>>();
    let depends = chains
        .iter()
        .enumerate()
        .map(|(child, chain)| {
            (0..requests.len())
                .filter(|parent| {
                    *parent != child
                        && chain.iter().any(|segment| {
                            owned[*parent].contains(&rollout_path_key(&segment.path))
                        })
                })
                .collect::<HashSet<_>>()
        })
        .collect::<Vec<_>>();
    let mut remaining = (0..requests.len()).collect::<Vec<_>>();
    let mut order = Vec::new();
    while !remaining.is_empty() {
        let ready = remaining.iter().position(|candidate| {
            !remaining
                .iter()
                .any(|child| depends[*child].contains(candidate))
        });
        let Some(index) = ready else {
            order.extend(remaining); // cycles remain subject to fail-closed preflight
            break;
        };
        order.push(remaining.remove(index));
    }
    order
}

fn deletion_paths(path: &Path, session_id: &str) -> Result<Vec<PathBuf>, String> {
    deletion_paths_with_descriptors(
        path,
        session_id,
        &rollout_descriptors_checked(&session_roots())?,
    )
}

fn deletion_paths_with_descriptors(
    path: &Path,
    session_id: &str,
    descriptors: &[RolloutDescriptor],
) -> Result<Vec<PathBuf>, String> {
    deletion_paths_for_plan(path, session_id, descriptors, &HashSet::new())
}

fn deletion_paths_for_plan(
    path: &Path,
    session_id: &str,
    descriptors: &[RolloutDescriptor],
    selected: &HashSet<String>,
) -> Result<Vec<PathBuf>, String> {
    let target = descriptors
        .iter()
        .find(|descriptor| descriptor.key == rollout_path_key(path))
        .ok_or_else(|| format!("无法读取 Codex 会话元数据：{}", path.display()))?;

    if target.session_id != session_id || !is_safe_session_id(session_id) {
        return Err("Codex session ID mismatch or invalid ID".into());
    }
    let owned_segments = if target.history_base.is_some() {
        resolve_paginated_chain_from_descriptors(path, descriptors)?
            .into_iter()
            .filter(|segment| segment.session_id == session_id)
            .collect::<Vec<_>>()
    } else {
        vec![HistorySegment {
            path: target.path.clone(),
            session_id: session_id.to_string(),
            end_byte_offset: None,
        }]
    };
    if owned_segments.is_empty() {
        return Err("Codex 会话没有可安全删除的自有分页文件".into());
    }

    let owned_paths = owned_segments
        .iter()
        .map(|segment| rollout_path_key(&segment.path))
        .collect::<HashSet<_>>();
    let owned_rollout_ids = descriptors
        .iter()
        .filter(|descriptor| owned_paths.contains(&descriptor.key))
        .map(|descriptor| descriptor.rollout_id.clone())
        .collect::<HashSet<_>>();

    for continuation in descriptors
        .iter()
        .filter(|descriptor| descriptor.history_base.is_some())
    {
        if owned_paths.contains(&continuation.key) || selected.contains(&continuation.key) {
            continue;
        }

        match resolve_paginated_chain_from_descriptors(&continuation.path, descriptors) {
            Ok(chain)
                if chain
                    .iter()
                    .any(|segment| owned_paths.contains(&rollout_path_key(&segment.path))) =>
            {
                if continuation.session_id == session_id {
                    return Err("该记录是当前会话依赖的分页历史，不能单独删除".into());
                }
                return Err("该会话仍被另一条 Codex 分支会话依赖，不能删除".into());
            }
            Ok(_) => {}
            Err(error)
                if continuation.session_id == session_id
                    || continuation
                        .history_base
                        .as_ref()
                        .is_some_and(|base| owned_rollout_ids.contains(&base.thread_id)) =>
            {
                return Err(format!(
                    "Codex 分页或分支历史无法确认，已禁止删除以保护会话：{error}"
                ));
            }
            Err(error) => return Err(format!("Codex dependency chain is incomplete: {error}")),
        }
    }

    Ok(owned_segments
        .into_iter()
        .map(|segment| segment.path)
        .collect())
}

/// Frozen before any index/file mutation; progress is checkpointed by deletion.rs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DeletionPlan {
    pub session_id: String,
    pub source_path: String,
    roots: Vec<PathBuf>,
    units: Vec<DeletionUnit>, // leaf before base
    cleanup_index: bool,
    index_done: bool,
    next_unit: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeletionUnit {
    root: PathBuf,
    path: PathBuf,
    session_id: String,
    rollout_id: String,
    fingerprint: (u64, u64),
}

fn deletion_fingerprint(path: &Path) -> Result<(u64, u64), String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut hash = 0xcbf29ce484222325u64;
    let mut length = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        length += count as u64;
        for byte in &buffer[..count] {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
    }
    Ok((length, hash))
}

// Missing roots still have an identity: resolve their nearest existing ancestor
// and reject traversal/symlinks before accepting an index-only cleanup.
fn checked_missing_rollout(root: &Path, path: &Path) -> Result<(), String> {
    if !root.is_absolute()
        || !path.is_absolute()
        || path == root
        || !path.starts_with(root)
        || root
            .components()
            .chain(path.components())
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("Missing Codex source is outside configured storage".into());
    }
    let mut anchor = root;
    while matches!(std::fs::symlink_metadata(anchor), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    {
        anchor = anchor.parent().ok_or("Missing storage ancestor")?;
    }
    let expected_root = if anchor == root {
        anchor.canonicalize().map_err(|e| e.to_string())?
    } else {
        checked_storage_child(anchor, root)?
    };
    let checked = checked_storage_child(anchor, path)?;
    if !checked.starts_with(expected_root) {
        return Err("Missing Codex source escapes root".into());
    }
    Ok(())
}

pub(crate) fn prepare_deletion_plan(
    targets: &[SessionMeta],
    roots: &[PathBuf],
    cleanup_index: bool,
) -> Result<DeletionPlan, String> {
    let first = targets.first().ok_or("Empty Codex deletion plan")?;
    let descriptors = rollout_descriptors_checked(roots)?;
    let mut units = Vec::new();
    let mut seen = HashSet::new();
    let mut selected = HashSet::new();
    for target in targets {
        if let Some(source) = target.source_path.as_deref().filter(|source| {
            descriptors
                .iter()
                .any(|descriptor| descriptor.key == rollout_path_key(Path::new(source)))
        }) {
            for segment in
                resolve_paginated_chain_from_descriptors(Path::new(source), &descriptors)?
            {
                if segment.session_id == first.session_id {
                    selected.insert(rollout_path_key(&segment.path));
                }
            }
        }
    }
    for target in targets {
        if target.provider_id != PROVIDER_ID || target.session_id != first.session_id {
            return Err("Codex deletion target mismatch".into());
        }
        let source = target
            .source_path
            .as_deref()
            .ok_or("Missing Codex source")?;
        if source == format!("codex-index:{}", first.session_id) {
            continue;
        }
        let requested = Path::new(source);
        let physical = physical_rollout_path(requested).ok();
        let path = physical.as_deref().unwrap_or(requested);
        // Only initially missing index ghosts may have no file unit. Legacy
        // journals are checked by deletion.rs before reaching this expansion.
        match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && target.cleanup_pending => {
                if !roots
                    .iter()
                    .any(|root| checked_missing_rollout(root, path).is_ok())
                {
                    return Err("Missing Codex source is outside configured storage".into());
                }
                continue;
            }
            Err(e) => return Err(e.to_string()),
            Ok(_) => {}
        }
        for candidate in deletion_paths_for_plan(path, &target.session_id, &descriptors, &selected)?
            .into_iter()
            .rev()
            .flat_map(|candidate| {
                let plain = plain_rollout_path(&candidate);
                // Delete both frozen representations, compressed first, so an
                // obsolete sibling never reappears after deleting the plain file.
                [plain.with_extension("jsonl.zst"), plain]
                    .into_iter()
                    .filter(|p| p.exists())
            })
        {
            let canonical = candidate.canonicalize().map_err(|e| e.to_string())?;
            if !seen.insert(canonical.clone()) {
                continue;
            }
            let root = roots
                .iter()
                .find_map(|root| {
                    checked_storage_child(root, &candidate)
                        .ok()
                        .filter(|checked| *checked == canonical)
                        .and_then(|_| root.canonicalize().ok())
                })
                .ok_or("Codex deletion unit is outside configured storage")?;
            let descriptor = read_rollout_descriptor(&canonical)?;
            if descriptor.session_id != first.session_id {
                return Err("Codex sibling identity mismatch".into());
            }
            units.push(DeletionUnit {
                root,
                path: canonical.clone(),
                session_id: descriptor.session_id,
                rollout_id: descriptor.rollout_id,
                fingerprint: deletion_fingerprint(&canonical)?,
            });
        }
    }
    // Union first, then sort every selected continuation ahead of its shared
    // bases. Per-target leaf/base concatenation would delete B before sibling C.
    let mut depths = HashMap::new();
    for unit in &units {
        depths.insert(
            rollout_path_key(&unit.path),
            resolve_paginated_chain_from_descriptors(&unit.path, &descriptors)?.len(),
        );
    }
    units.sort_by_key(|unit| std::cmp::Reverse(depths[&rollout_path_key(&unit.path)]));
    let plan = DeletionPlan {
        session_id: first.session_id.clone(),
        source_path: first.source_path.clone().ok_or("Missing Codex source")?,
        roots: roots.to_vec(),
        units,
        cleanup_index,
        index_done: false,
        next_unit: 0,
    };
    validate_deletion_plan(&plan, roots)?;
    Ok(plan)
}

pub(crate) fn validate_deletion_plan(plan: &DeletionPlan, roots: &[PathBuf]) -> Result<(), String> {
    let descriptors = rollout_descriptors_checked(roots)?;
    validate_deletion_plan_with_descriptors(plan, roots, &descriptors)
}

fn validate_deletion_plan_with_descriptors(
    plan: &DeletionPlan,
    roots: &[PathBuf],
    descriptors: &[RolloutDescriptor],
) -> Result<(), String> {
    if !is_safe_session_id(&plan.session_id)
        || plan.next_unit > plan.units.len()
        || plan.roots != roots
    {
        return Err("Codex saved deletion plan/root mismatch".into());
    }
    // An index-only plan froze absence, not permission to remove the index of
    // a valid transcript restored or created before a retry.
    if plan.units.is_empty() {
        if descriptors
            .iter()
            .any(|item| item.session_id == plan.session_id)
        {
            return Err("Codex index-only cleanup found a new or restored rollout".into());
        }
        if !plan.source_path.starts_with("codex-index:")
            && !rollout_is_missing(Path::new(&plan.source_path))
        {
            return Err("Codex index-only cleanup source reappeared".into());
        }
    }
    let mut remaining = HashSet::new();
    for (index, unit) in plan.units.iter().enumerate() {
        if unit.session_id != plan.session_id
            || !roots
                .iter()
                .any(|root| root.canonicalize().is_ok_and(|root| root == unit.root))
            || checked_storage_child(&unit.root, &unit.path)? != unit.path
        {
            return Err("Codex saved deletion unit/root/ID mismatch".into());
        }
        let plain = plain_rollout_path(&unit.path);
        for sibling in [plain.clone(), plain.with_extension("jsonl.zst")] {
            match std::fs::symlink_metadata(&sibling) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
                Ok(_) => {
                    if !plan.units.iter().any(|saved| saved.path == sibling) {
                        return Err(
                            "Codex representation changed after deletion plan was frozen".into(),
                        );
                    }
                }
            }
        }
        if index < plan.next_unit {
            if std::fs::symlink_metadata(&unit.path).is_ok() {
                return Err("Codex completed deletion unit reappeared".into());
            }
            continue;
        }
        remaining.insert(rollout_path_key(&unit.path));
        match std::fs::symlink_metadata(&unit.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.to_string()),
            Ok(meta) if !meta.is_file() || is_reparse_or_symlink(&meta) => {
                return Err("Codex saved file was replaced".into())
            }
            Ok(_) => {}
        }
        let descriptor = read_rollout_descriptor(&unit.path)?;
        if descriptor.session_id != unit.session_id
            || descriptor.rollout_id != unit.rollout_id
            || deletion_fingerprint(&unit.path)? != unit.fingerprint
        {
            return Err("Codex saved file identity changed; stopped cleanup".into());
        }
    }
    // Planned continuations may already have gone; only outside references can
    // veto the remaining exact units. Never re-expand a saved leaf on retry.
    for descriptor in descriptors
        .iter()
        .filter(|d| d.history_base.is_some() && !remaining.contains(&d.key))
    {
        let chain = resolve_paginated_chain_from_descriptors(&descriptor.path, descriptors)?;
        if chain
            .iter()
            .any(|segment| remaining.contains(&rollout_path_key(&segment.path)))
        {
            return Err("Codex deletion unit is still referenced by another rollout".into());
        }
    }
    Ok(())
}

/// Frozen file ownership and current index ownership must agree on every retry.
/// A residual that became the indexed rollout must be selected again, not
/// silently promoted to a broader index-cleaning operation.
pub(crate) fn validate_deletion_index_decision(
    plan: &DeletionPlan,
    current_cleanup_index: bool,
) -> Result<(), String> {
    if plan.cleanup_index != current_cleanup_index {
        return Err(
            "Codex index ownership changed since the deletion plan was frozen; rescan required"
                .into(),
        );
    }
    Ok(())
}

pub(crate) fn execute_deletion_plan_validated<C, D, S>(
    plan: &mut DeletionPlan,
    mut cleanup: C,
    mut remove: D,
    mut save: S,
) -> Result<(), String>
where
    C: FnMut(&str) -> Result<(), String>,
    D: FnMut(&Path) -> Result<(), String>,
    S: FnMut(&DeletionPlan) -> Result<(), String>,
{
    // index_done is a checkpoint, not proof about today's configured databases.
    // Cleanup resolves and rechecks the effective index each time; rerunning it
    // is idempotent and must precede even the remaining frozen file removals.
    if plan.cleanup_index {
        cleanup(&plan.session_id)?;
    }
    if !plan.index_done {
        plan.index_done = true;
        save(plan)?;
    }
    while plan.next_unit < plan.units.len() {
        let path = &plan.units[plan.next_unit].path;
        match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
            Ok(_) => remove(path)?,
        }
        plan.next_unit += 1;
        save(plan)?;
    }
    Ok(())
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn execute_deletion_plan<C, D, S>(
    plan: &mut DeletionPlan,
    roots: &[PathBuf],
    cleanup: C,
    remove: D,
    save: S,
) -> Result<(), String>
where
    C: FnMut(&str) -> Result<(), String>,
    D: FnMut(&Path) -> Result<(), String>,
    S: FnMut(&DeletionPlan) -> Result<(), String>,
{
    validate_deletion_plan(plan, roots)?;
    execute_deletion_plan_validated(plan, cleanup, remove, save)
}

pub fn delete_session(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    let validated_paths = validated_deletion_paths(root, path, session_id)?;

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

pub fn validate_delete_under_root(
    root: &Path,
    path: &Path,
    session_id: &str,
) -> Result<(), String> {
    validated_deletion_paths(root, path, session_id).map(|_| ())
}

fn validated_deletion_paths(
    root: &Path,
    path: &Path,
    session_id: &str,
) -> Result<Vec<PathBuf>, String> {
    // Explicit roots keep provider tests and callers independent of CODEX_HOME.
    let roots = vec![root.to_path_buf()];
    let paths =
        deletion_paths_with_descriptors(path, session_id, &rollout_descriptors_checked(&roots)?)?;
    let allowed_roots = roots
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

    Ok(validated_paths)
}

#[cfg(test)]
fn parse_session(path: &Path) -> Option<SessionMeta> {
    parse_session_with_titles(path, &HashMap::new())
}

#[cfg(test)]
fn parse_session_with_titles(
    path: &Path,
    thread_titles: &HashMap<String, String>,
) -> Option<SessionMeta> {
    parse_codex_file(path)
        .ok()
        .flatten()
        .and_then(|entry| entry.session)
        .map(|raw| raw.into_session_meta(thread_titles))
}

fn parse_codex_file(path: &Path) -> Result<Option<CachedCodexFile>, String> {
    let RolloutHeadTail {
        head,
        tail,
        known_len,
    } = match rollout_head_tail_with_len(path) {
        Ok(parts) => parts,
        Err(error) => {
            super::utils::scan_warning(format!("无法读取 {}：{error}", path.display()));
            return match read_rollout_descriptor(path) {
                Ok(descriptor) => Ok(Some(CachedCodexFile {
                    descriptor: Some(descriptor),
                    session: None,
                })),
                Err(_) => Err(error),
            };
        }
    };
    let descriptor = parse_descriptor_from_head_lines(
        path,
        head.iter()
            .map(|line| Ok(std::borrow::Cow::Borrowed(line.as_str()))),
        known_len,
    )
    .ok();

    let mut session_id: Option<String> = None;
    let mut project_dir: Option<String> = None;
    let mut created_at: Option<i64> = None;
    let mut first_user_message: Option<String> = None;
    let mut fallback_first_event_user: Option<String> = None;

    // Extract metadata and first user message from head lines
    for line in &head {
        let value: Value = match serde_json::from_str(line) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        if created_at.is_none() {
            created_at = value.get("timestamp").and_then(parse_timestamp_to_ms);
        }
        let item_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        if item_type == "session_meta" {
            if let Some(payload) = value.get("payload") {
                if is_subagent_source(payload.get("source")) {
                    return Ok(Some(CachedCodexFile {
                        descriptor,
                        session: None,
                    }));
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
        if first_user_message.is_none() && item_type == "response_item" {
            if let Some(payload) = value.get("payload") {
                if payload.get("type").and_then(Value::as_str) == Some("message")
                    && payload.get("role").and_then(Value::as_str) == Some("user")
                {
                    let text = payload.get("content").map(extract_text).unwrap_or_default();
                    if !is_internal_user_context(payload, &text) {
                        if let Some(title) = title_candidate_from_user_message(&text) {
                            first_user_message = Some(truncate_summary(&title, TITLE_MAX_CHARS));
                        }
                    }
                }
            }
        } else if fallback_first_event_user.is_none() && item_type == "event_msg" {
            if let Some(payload) = value.get("payload") {
                if payload.get("type").and_then(Value::as_str) == Some("user_message") {
                    let text = payload
                        .get("message")
                        .or_else(|| payload.get("text"))
                        .map(extract_text)
                        .unwrap_or_default();
                    if !is_internal_user_context(payload, &text) {
                        if let Some(title) = title_candidate_from_user_message(&text) {
                            fallback_first_event_user =
                                Some(truncate_summary(&title, TITLE_MAX_CHARS));
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
    let mut fallback_latest_event_user: Option<String> = None;
    let mut fallback_event_summary: Option<String> = None;

    for line in tail.iter().rev() {
        let value: Value = match serde_json::from_str(line) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        if last_active_at.is_none() {
            last_active_at = value.get("timestamp").and_then(parse_timestamp_to_ms);
        }
        let item_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        if item_type == "response_item" {
            if let Some(payload) = value.get("payload") {
                if payload.get("type").and_then(Value::as_str) == Some("message") {
                    let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
                    let text = payload.get("content").map(extract_text).unwrap_or_default();
                    let visible = matches!(role, "user" | "assistant")
                        && !(role == "user" && is_internal_user_context(payload, &text));
                    if visible && latest_user_message.is_none() && role == "user" {
                        latest_user_message = title_candidate_from_user_message(&text)
                            .map(|title| truncate_summary(&title, TITLE_MAX_CHARS));
                    }
                    if visible && summary.is_none() && !text.trim().is_empty() {
                        summary = Some(text);
                    }
                }
            }
        } else if item_type == "event_msg" {
            if let Some(payload) = value.get("payload") {
                let event_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
                if matches!(event_type, "user_message" | "agent_message") {
                    let text = payload
                        .get("message")
                        .or_else(|| payload.get("text"))
                        .map(extract_text)
                        .unwrap_or_default();
                    let is_user = event_type == "user_message";
                    let visible = !(is_user && is_internal_user_context(payload, &text));
                    if visible && is_user && fallback_latest_event_user.is_none() {
                        fallback_latest_event_user = title_candidate_from_user_message(&text)
                            .map(|title| truncate_summary(&title, TITLE_MAX_CHARS));
                    }
                    if visible && fallback_event_summary.is_none() && !text.trim().is_empty() {
                        fallback_event_summary = Some(text);
                    }
                }
            }
        }
        if last_active_at.is_some() && summary.is_some() && latest_user_message.is_some() {
            break;
        }
    }

    if first_user_message.is_none() && latest_user_message.is_none() && summary.is_none() {
        first_user_message = fallback_first_event_user;
        latest_user_message = fallback_latest_event_user;
        summary = fallback_event_summary;
    }

    let Some(session_id) = session_id.or_else(|| infer_session_id_from_filename(path)) else {
        return Ok(descriptor.map(|descriptor| CachedCodexFile {
            descriptor: Some(descriptor),
            session: None,
        }));
    };

    Ok(Some(CachedCodexFile {
        descriptor,
        session: Some(RawCodexSession {
            session_id,
            project_dir,
            created_at,
            last_active_at,
            first_user_message,
            latest_user_message,
            summary: summary.map(|text| truncate_summary(&text, 160)),
            source_path: path.to_string_lossy().to_string(),
        }),
    }))
}

fn is_subagent_source(source: Option<&Value>) -> bool {
    match source {
        Some(Value::String(text)) => {
            text.eq_ignore_ascii_case("subagent") || text.starts_with("subagent:")
        }
        Some(Value::Object(map)) => map.contains_key("subagent"),
        _ => false,
    }
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
    collect_files_where(root, is_rollout_file, files);
}

#[cfg(test)]
mod tests {
    #[test]
    fn compat_compressed_chain_identity_offsets_and_tool_text() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("base.jsonl");
        write_codex_session(&base, "base", "你好 base");
        let offset = std::fs::metadata(&base).unwrap().len();
        let compressed = base.with_extension("jsonl.zst");
        let bytes = std::fs::read(&base).unwrap();
        std::fs::write(
            &compressed,
            zstd::stream::encode_all(bytes.as_slice(), 0).unwrap(),
        )
        .unwrap();
        assert_eq!(
            preferred_rollout_files(vec![compressed.clone(), base.clone()]),
            vec![base.clone()]
        );
        std::fs::remove_file(&base).unwrap();
        let child = temp.path().join("child.jsonl");
        write_paginated_session_with_base(&child, "child", "base", offset, "child");
        let compressed_child = child.with_extension("jsonl.zst");
        std::fs::write(
            &compressed_child,
            zstd::stream::encode_all(std::fs::read(&child).unwrap().as_slice(), 0).unwrap(),
        )
        .unwrap();
        std::fs::remove_file(&child).unwrap();
        let descriptors = rollout_descriptors_checked(&[temp.path().to_path_buf()]).unwrap();
        let chain = resolve_paginated_chain_from_descriptors(&child, &descriptors).unwrap();
        assert_eq!(chain[0].path, compressed);
        assert_eq!(chain[0].end_byte_offset, Some(offset));
        assert_eq!(
            transcript_sources(&child).unwrap(),
            vec![
                (compressed.clone(), Some(offset)),
                (compressed_child.clone(), None)
            ]
        );
        assert!(!rollout_is_missing(&base));
        assert_eq!(physical_rollout_path(&child).unwrap(), compressed_child);
        assert_eq!(load_history_segments(chain).unwrap().len(), 2);
        assert!(deletion_paths_with_descriptors(&base, "base", &descriptors).is_err());
        assert_eq!(
            scan_sessions_in_roots(&[temp.path().to_path_buf()]).len(),
            2
        );
        let tools = temp.path().join("tools.jsonl");
        std::fs::write(&tools, concat!(
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"output\":[{\"type\":\"input_text\",\"text\":\"array output\"}]}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call\",\"name\":\"patch\",\"input\":\"patch text\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call_output\",\"output\":\"done\"}}\n"
        )).unwrap();
        let messages = load_segment_messages(&tools, None).unwrap();
        assert_eq!(messages[0].content, "array output");
        assert!(messages[1].content.contains("patch text"));
        assert_eq!(messages[2].content, "done");
    }

    #[test]
    fn compat_compressed_plan_freezes_both_representations_and_retry() {
        let temp = tempfile::tempdir().unwrap();
        let plain = temp.path().join("thread.jsonl");
        let compressed = plain.with_extension("jsonl.zst");
        write_codex_session(&plain, "thread", "old");
        std::fs::write(
            &compressed,
            zstd::stream::encode_all(std::fs::read(&plain).unwrap().as_slice(), 0).unwrap(),
        )
        .unwrap();
        let target: SessionMeta = serde_json::from_value(serde_json::json!({"providerId":"codex","sessionId":"thread","residual":false,"sourcePath":plain})).unwrap();
        let roots = [temp.path().to_path_buf()];
        let mut plan = prepare_deletion_plan(std::slice::from_ref(&target), &roots, true).unwrap();
        assert_eq!(plan.units.len(), 2);
        assert!(is_compressed_rollout(&plan.units[0].path));
        execute_deletion_plan(
            &mut plan,
            &roots,
            |_| Ok(()),
            |path| std::fs::remove_file(path).map_err(|e| e.to_string()),
            |_| Ok(()),
        )
        .unwrap();
        assert!(!plain.exists() && !compressed.exists());
        write_codex_session(&plain, "thread", "new");
        assert!(validate_deletion_plan(&plan, &roots).is_err());
        let plan = prepare_deletion_plan(&[target], &roots, true).unwrap();
        std::fs::write(
            &compressed,
            zstd::stream::encode_all(std::fs::read(&plain).unwrap().as_slice(), 0).unwrap(),
        )
        .unwrap();
        std::fs::remove_file(&plain).unwrap();
        assert!(validate_deletion_plan(&plan, &roots).is_err());
        assert!(compressed.exists());
    }

    #[test]
    fn compat_chain_representation_transition_never_deletes_base() {
        for start_compressed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let base = temp.path().join("base.jsonl");
            let leaf = temp.path().join("leaf.jsonl");
            write_codex_session(&base, "thread", "base");
            write_paginated_session(
                &leaf,
                "thread",
                std::fs::metadata(&base).unwrap().len(),
                "leaf",
            );
            let bytes = std::fs::read(&leaf).unwrap();
            let compressed = leaf.with_extension("jsonl.zst");
            if start_compressed {
                std::fs::write(
                    &compressed,
                    zstd::stream::encode_all(bytes.as_slice(), 0).unwrap(),
                )
                .unwrap();
                std::fs::remove_file(&leaf).unwrap();
            }
            let source = if start_compressed { &compressed } else { &leaf };
            let target: SessionMeta = serde_json::from_value(serde_json::json!({"providerId":"codex","sessionId":"thread","residual":false,"sourcePath":source})).unwrap();
            let roots = [temp.path().to_path_buf()];
            let mut plan = prepare_deletion_plan(&[target], &roots, true).unwrap();
            if start_compressed {
                std::fs::write(&leaf, &bytes).unwrap();
                std::fs::remove_file(&compressed).unwrap();
            } else {
                std::fs::write(
                    &compressed,
                    zstd::stream::encode_all(bytes.as_slice(), 0).unwrap(),
                )
                .unwrap();
                std::fs::remove_file(&leaf).unwrap();
            }
            assert!(execute_deletion_plan(
                &mut plan,
                &roots,
                |_| panic!("no cleanup"),
                |_| panic!("no remove"),
                |_| panic!("no checkpoint")
            )
            .is_err());
            assert!(base.exists());
        }
    }

    #[test]
    fn compat_compressed_prefix_budget_and_corrupt_child_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let plain = temp.path().join("base.jsonl");
        write_codex_session(&plain, "base", "prefix");
        let prefix = std::fs::read(&plain).unwrap();
        let compressed = plain.with_extension("jsonl.zst");
        let mut encoder =
            zstd::stream::write::Encoder::new(File::create(&compressed).unwrap(), 0).unwrap();
        let huge = MAX_MESSAGES_FILE_BYTES + 1;
        encoder
            .set_pledged_src_size(Some(huge + prefix.len() as u64))
            .unwrap();
        encoder.write_all(&prefix).unwrap();
        std::io::copy(&mut std::io::repeat(b' ').take(huge), &mut encoder).unwrap();
        encoder.finish().unwrap();
        std::fs::remove_file(&plain).unwrap();
        assert_eq!(
            load_segment_messages(&compressed, Some(prefix.len() as u64))
                .unwrap()
                .len(),
            1
        );
        assert!(load_segment_messages(&compressed, None).is_err());
        assert!(read_rollout_descriptor(&compressed).unwrap().file_len > MAX_MESSAGES_FILE_BYTES);
        let bad = temp.path().join("child.jsonl.zst");
        std::fs::write(bad, b"broken zstd").unwrap();
        assert!(rollout_descriptors_checked(&[temp.path().to_path_buf()]).is_err());
    }

    #[test]
    fn compat_name_old_schema_and_spawn_edges() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE threads(id TEXT, title TEXT, name TEXT, first_user_message TEXT); INSERT INTO threads VALUES('a','generated','explicit','question'),('b','kept','','question'); CREATE TABLE thread_spawn_edges(parent_thread_id TEXT,child_thread_id TEXT); INSERT INTO thread_spawn_edges VALUES('a','b'),('c','a'),('c','d');").unwrap();
        assert_eq!(load_thread_titles_from_db(&path)["a"], "explicit");
        let index = temp.path().join("index.jsonl");
        std::fs::write(&index, "{\"id\":\"a\",\"thread_name\":\"index a\"}\n{\"id\":\"b\",\"thread_name\":\"index b\"}\n").unwrap();
        let titles = load_thread_titles_from_paths(&index, std::slice::from_ref(&path));
        assert_eq!(titles["a"], "explicit");
        assert_eq!(titles["b"], "index b");
        cleanup_state_db(&path, "a").unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM thread_spawn_edges", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        db.execute_batch("DROP TABLE threads; CREATE TABLE threads(id TEXT,title TEXT); INSERT INTO threads VALUES('old','legacy');").unwrap();
        assert_eq!(load_thread_titles_from_db(&path)["old"], "legacy");
    }

    #[test]
    fn compat_sqlite_home_cleanup_leaves_old_default_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let configured = temp.path().join("sqlite");
        std::fs::create_dir(&configured).unwrap();
        let old = temp
            .path()
            .join(crate::session_paths::CODEX_STATE_DB_FILENAME);
        let current = configured.join(crate::session_paths::CODEX_STATE_DB_FILENAME);
        for path in [&old, &current] {
            Connection::open(path)
                .unwrap()
                .execute_batch("CREATE TABLE threads(id TEXT); INSERT INTO threads VALUES('same');")
                .unwrap();
        }
        let config = format!(
            "sqlite_home = {}",
            serde_json::to_string(&configured.to_string_lossy()).unwrap()
        );
        cleanup_index_databases(temp.path(), &config, "same").unwrap();
        for (path, count) in [(&old, 1), (&current, 0)] {
            assert_eq!(
                Connection::open(path)
                    .unwrap()
                    .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                count
            );
        }
    }

    #[test]
    fn compat_missing_sessions_root_ghost_is_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let target: SessionMeta = serde_json::from_value(serde_json::json!({"providerId":"codex","sessionId":"ghost","residual":false,"cleanupPending":true,"sourcePath":root.join("day/gone.jsonl")})).unwrap();
        let plan = prepare_deletion_plan(
            std::slice::from_ref(&target),
            std::slice::from_ref(&root),
            true,
        )
        .unwrap();
        assert!(plan.units.is_empty());
        let mut outside = target;
        outside.source_path = Some(temp.path().join("outside.jsonl").to_string_lossy().into());
        assert!(prepare_deletion_plan(&[outside], &[root], true).is_err());
    }
    #[test]
    fn compat_ghost_retry_protects_restored_plain_and_compressed_logs() {
        for compressed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("sessions");
            let source = root.join("day/gone.jsonl");
            let target: SessionMeta = serde_json::from_value(serde_json::json!({"providerId":"codex","sessionId":"ghost","residual":false,"cleanupPending":true,"sourcePath":source})).unwrap();
            let mut plan =
                prepare_deletion_plan(&[target], std::slice::from_ref(&root), true).unwrap();
            std::fs::create_dir_all(source.parent().unwrap()).unwrap();
            write_codex_session(&source, "ghost", "restored transcript");
            if compressed {
                let bytes = std::fs::read(&source).unwrap();
                std::fs::write(
                    source.with_extension("jsonl.zst"),
                    zstd::stream::encode_all(bytes.as_slice(), 0).unwrap(),
                )
                .unwrap();
                std::fs::remove_file(&source).unwrap();
            }
            assert!(execute_deletion_plan(
                &mut plan,
                &[root],
                |_| panic!("index must remain"),
                |_| panic!("file must remain"),
                |_| panic!("journal must remain")
            )
            .is_err());
        }
    }

    // Test entry points carry only fixture-local roots, never ambient CODEX_HOME.
    fn resolve_paginated_chain(path: &Path) -> Result<Vec<HistorySegment>, String> {
        let physical = physical_rollout_path(path)?;
        let descriptor = read_rollout_descriptor(&physical)?;
        if descriptor.history_base.is_none() {
            return Ok(vec![HistorySegment {
                path: descriptor.path,
                session_id: descriptor.session_id,
                end_byte_offset: None,
            }]);
        }
        resolve_paginated_chain_from_descriptors(
            &physical,
            &rollout_descriptors_checked(&[physical.parent().unwrap().to_path_buf()])?,
        )
    }
    fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
        load_history_segments(resolve_paginated_chain(path)?)
    }
    fn transcript_sources(path: &Path) -> Result<Vec<(PathBuf, Option<u64>)>, String> {
        Ok(resolve_paginated_chain(path)?
            .into_iter()
            .map(|segment| (segment.path, segment.end_byte_offset))
            .collect())
    }
    fn validate_delete(path: &Path, id: &str) -> Result<(), String> {
        deletion_paths_with_descriptors(
            path,
            id,
            &rollout_descriptors_checked(&[path.parent().unwrap().to_path_buf()])?,
        )
        .map(|_| ())
    }

    #[test]
    fn codex_segment_budget_counts_only_requested_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("huge.jsonl");
        write_codex_session(&path, "thread", "visible prefix");
        let prefix = path.metadata().unwrap().len();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_MESSAGES_FILE_BYTES + 1)
            .unwrap();
        assert_eq!(
            load_segment_messages(&path, Some(prefix)).unwrap()[0].content,
            "visible prefix"
        );
        assert!(load_segment_messages(&path, None).is_err());
        assert!(load_segment_messages(&path, Some(MAX_MESSAGES_FILE_BYTES + 2)).is_err());
        assert!(load_history_segments(vec![
            HistorySegment {
                path: path.clone(),
                session_id: "thread".into(),
                end_byte_offset: Some(MAX_MESSAGES_FILE_BYTES)
            },
            HistorySegment {
                path,
                session_id: "thread".into(),
                end_byte_offset: Some(1)
            },
        ])
        .is_err());
    }

    #[test]
    fn codex_latest_user_title_survives_newer_assistant_summary() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("thread.jsonl");
        write_codex_session(&path, "thread", "first user");
        let mut file = File::options().append(true).open(&path).unwrap();
        for (role, content) in [("user", "latest user"), ("assistant", "latest answer")] {
            writeln!(file, "{}", serde_json::json!({"type":"response_item","payload":{"type":"message","role":role,"content":content}})).unwrap();
        }
        let meta = parse_session(&path).unwrap();
        assert_eq!(meta.title.as_deref(), Some("latest user"));
        assert_eq!(meta.summary.as_deref(), Some("latest answer"));
    }

    #[test]
    fn codex_strict_dependency_scan_rejects_bad_descriptor_and_directory() {
        let temp = tempfile::tempdir().unwrap();
        let leaf = temp.path().join("leaf.jsonl");
        write_codex_session(&leaf, "thread", "hello");
        let target = parse_session(&leaf).unwrap();
        let bad = temp.path().join("unreadable.jsonl");
        std::fs::write(&bad, b"not a descriptor\n").unwrap();
        assert!(prepare_deletion_plan(
            std::slice::from_ref(&target),
            &[temp.path().to_path_buf()],
            false
        )
        .is_err());
        assert!(leaf.exists());
        assert!(prepare_deletion_plan(std::slice::from_ref(&target), &[bad], false).is_err());
        let outside = tempfile::tempdir().unwrap();
        assert!(prepare_deletion_plan(&[target], &[outside.path().to_path_buf()], false).is_err());
    }

    #[test]
    fn codex_plan_deletes_selected_sibling_leaves_before_shared_base() {
        let temp = tempfile::tempdir().unwrap();
        let roots = vec![temp.path().to_path_buf()];
        let base = temp.path().join("base.jsonl");
        let leaf = temp.path().join("leaf.jsonl");
        let sibling = temp.path().join("sibling.jsonl");
        write_codex_session(&base, "thread", "base");
        let offset = base.metadata().unwrap().len();
        write_paginated_session(&leaf, "thread", offset, "leaf");
        write_paginated_session(&sibling, "thread", offset, "sibling");
        let targets = vec![
            parse_session(&leaf).unwrap(),
            parse_session(&sibling).unwrap(),
        ];
        assert!(prepare_deletion_plan(&targets[..1], &roots, false).is_err());
        let mut plan = prepare_deletion_plan(&targets, &roots, false).unwrap();
        assert_eq!(plan.units.len(), 3);
        assert_eq!(plan.units[2].path.file_name(), base.file_name());
        assert!(execute_deletion_plan(
            &mut plan,
            &roots,
            |_| Ok(()),
            |path| {
                if path.file_name() == sibling.file_name() {
                    return Err("injected sibling failure".into());
                }
                std::fs::remove_file(path).map_err(|e| e.to_string())
            },
            |_| Ok(())
        )
        .is_err());
        assert!(!leaf.exists() && sibling.exists() && base.exists());
        execute_deletion_plan(
            &mut plan,
            &roots,
            |_| Ok(()),
            |path| std::fs::remove_file(path).map_err(|e| e.to_string()),
            |_| Ok(()),
        )
        .unwrap();
        assert!(!base.exists() && !leaf.exists() && !sibling.exists());
    }

    #[test]
    fn codex_checkpoint_failure_replays_missing_leaf_and_retains_base() {
        let temp = tempfile::tempdir().unwrap();
        let roots = vec![temp.path().to_path_buf()];
        let base = temp.path().join("base.jsonl");
        let leaf = temp.path().join("leaf.jsonl");
        write_codex_session(&base, "thread", "base");
        write_paginated_session(&leaf, "thread", base.metadata().unwrap().len(), "leaf");
        let mut plan =
            prepare_deletion_plan(&[parse_session(&leaf).unwrap()], &roots, false).unwrap();
        let mut durable = plan.clone();
        let mut saves = 0;
        let result = execute_deletion_plan(
            &mut plan,
            &roots,
            |_| Ok(()),
            |path| std::fs::remove_file(path).map_err(|e| e.to_string()),
            |progress| {
                saves += 1;
                if saves == 2 {
                    return Err("injected checkpoint failure".into());
                }
                durable = progress.clone();
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(!leaf.exists() && base.exists());
        assert_eq!(durable.next_unit, 0);
        execute_deletion_plan(
            &mut durable,
            &roots,
            |_| panic!("index already complete"),
            |path| std::fs::remove_file(path).map_err(|e| e.to_string()),
            |_| Ok(()),
        )
        .unwrap();
        assert!(!base.exists());
    }

    #[test]
    fn codex_frozen_plan_rejects_replacement_and_new_dependency() {
        let temp = tempfile::tempdir().unwrap();
        let root = vec![temp.path().to_path_buf()];
        let base = temp.path().join("base.jsonl");
        let leaf = temp.path().join("leaf.jsonl");
        write_codex_session(&base, "thread", "base");
        write_paginated_session(&leaf, "thread", base.metadata().unwrap().len(), "leaf");
        let plan = prepare_deletion_plan(&[parse_session(&leaf).unwrap()], &root, false).unwrap();
        let original = std::fs::read(&leaf).unwrap();
        write_codex_session(&leaf, "thread", "replaced");
        assert!(validate_deletion_plan(&plan, &root).is_err());
        std::fs::write(&leaf, original).unwrap();
        let child = temp.path().join("child.jsonl");
        write_paginated_session_with_base(
            &child,
            "child",
            "thread",
            base.metadata().unwrap().len(),
            "new dependency",
        );
        assert!(validate_deletion_plan(&plan, &root).is_err());
        let other = tempfile::tempdir().unwrap();
        assert!(validate_deletion_plan(&plan, &[other.path().to_path_buf()]).is_err());
    }

    #[test]
    fn sidebar_sections_follow_membership_and_support_older_databases() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        assert!(super::read_sidebar_sections(&conn).is_empty());
        conn.execute_batch("CREATE TABLE threads (id TEXT, thread_section_id TEXT);
            CREATE TABLE thread_sections (id TEXT, name TEXT);
            INSERT INTO thread_sections VALUES ('section-a', 'Deepseek11');
            INSERT INTO threads VALUES ('project-thread', 'section-a'), ('projectless-thread', 'section-a'), ('ordinary-thread', NULL), ('stale-thread', 'missing');").unwrap();
        let sections = super::read_sidebar_sections(&conn);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections["project-thread"].name, "Deepseek11");
        assert_eq!(sections["projectless-thread"].id, "section-a");
        conn.execute(
            "UPDATE threads SET thread_section_id = NULL WHERE id = 'project-thread'",
            [],
        )
        .unwrap();
        assert!(!super::read_sidebar_sections(&conn).contains_key("project-thread"));
    }
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
    fn cleanup_global_state_removes_thread_assignment_and_keeps_project() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join(".codex-global-state.json");
        std::fs::write(
            &path,
            r#"{"thread-project-assignments":{"thread-1":{"projectId":"project-1"}},"local-projects":{"project-1":{"rootPaths":["C:\\work\\kept"]}},"project-order":["project-1"]}"#,
        )
        .expect("write global state");

        cleanup_global_state(&path, "thread-1").expect("clean global state");

        let value: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(value["thread-project-assignments"]
            .get("thread-1")
            .is_none());
        assert!(value["local-projects"].get("project-1").is_some());
        assert_eq!(value["project-order"], serde_json::json!(["project-1"]));
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

        cleanup_global_state(&path, "thread-1").expect("clean global state");

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

        cleanup_index_databases(temp.path(), "", "thread-1").expect("clean Codex indexes");

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
    fn transcript_sources_lists_the_whole_paginated_chain() {
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

        let sources = transcript_sources(&current).expect("resolve transcript sources");

        assert_eq!(sources.len(), 2, "分页链的基座与当前段都要算进正文源");
        assert_eq!(sources[0].0, base, "链按时间正序：基座在前");
        assert_eq!(
            sources[0].1,
            Some(base_prefix.len() as u64),
            "基座只读到记录的偏移"
        );
        assert_eq!(sources[1].0, current);
        assert_eq!(sources[1].1, None, "当前段读到文件末尾");
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
    fn canonical_path_and_overlapping_roots_do_not_duplicate_history_bases() {
        let temp = tempdir().unwrap();
        let day = temp.path().join("day");
        std::fs::create_dir(&day).unwrap();
        let parent = day.join("parent.jsonl");
        let child = day.join("child.jsonl");
        write_codex_session(&parent, "parent", "Parent");
        write_paginated_session_with_base(
            &child,
            "child",
            "parent",
            parent.metadata().unwrap().len(),
            "Child",
        );
        let canonical = child.canonicalize().unwrap();
        let descriptors =
            rollout_descriptors_in_roots(&canonical, vec![temp.path().to_path_buf(), day]);
        assert_eq!(descriptors.len(), 2);
        let chain = resolve_paginated_chain_from_descriptors(&canonical, &descriptors).unwrap();
        assert_eq!(chain.len(), 2);
        validate_delete_under_root(temp.path(), &canonical, "child").unwrap();
        delete_session(temp.path(), &canonical, "child").unwrap();
        assert!(parent.exists());
        assert!(!child.exists());
    }

    #[test]
    fn indirect_path_segments_resolve_the_same_paginated_chain() {
        let temp = tempdir().unwrap();
        let day = temp.path().join("day");
        std::fs::create_dir(&day).unwrap();
        let parent = day.join("parent.jsonl");
        let child = day.join("child.jsonl");
        write_codex_session(&parent, "parent", "Parent");
        write_paginated_session_with_base(
            &child,
            "child",
            "parent",
            parent.metadata().unwrap().len(),
            "Child",
        );

        // 同一份文件的另一种写法：中间夹带 `..`。纯字符串比较会把它当成另一个
        // 文件，从而拒绝解析分页链；比较键必须先解析真实路径。
        let indirect = day.join("..").join("day").join("child.jsonl");
        let descriptors = rollout_descriptors_in_roots(&child, vec![temp.path().to_path_buf()]);
        assert_eq!(descriptors.len(), 2);
        let chain = resolve_paginated_chain_from_descriptors(&indirect, &descriptors).unwrap();
        assert_eq!(chain.len(), 2);
    }

    #[test]
    fn distinct_files_with_ambiguous_base_ids_are_still_rejected() {
        let temp = tempdir().unwrap();
        let parent_a = temp.path().join("parent-a.jsonl");
        let parent_b = temp.path().join("parent-b.jsonl");
        let child = temp.path().join("child.jsonl");
        write_codex_session(&parent_a, "parent", "Parent");
        std::fs::copy(&parent_a, &parent_b).unwrap();
        write_paginated_session_with_base(
            &child,
            "child",
            "parent",
            parent_a.metadata().unwrap().len(),
            "Child",
        );
        let descriptors = rollout_descriptors_in_roots(&child, vec![temp.path().to_path_buf()]);
        assert_eq!(descriptors.len(), 3);
        assert!(
            resolve_paginated_chain_from_descriptors(&child, &descriptors)
                .unwrap_err()
                .contains("基座不唯一")
        );
    }

    #[test]
    fn batch_orders_descendants_before_parents_and_can_delete_the_selected_tree() {
        let temp = tempdir().unwrap();
        let parent = temp.path().join("parent.jsonl");
        let child = temp.path().join("child.jsonl");
        let grandchild = temp.path().join("grandchild.jsonl");
        write_codex_session(&parent, "parent", "Parent");
        write_paginated_session_with_base(
            &child,
            "child",
            "parent",
            parent.metadata().unwrap().len(),
            "Child",
        );
        write_paginated_session_with_base(
            &grandchild,
            "grandchild",
            "child",
            child.metadata().unwrap().len(),
            "Grandchild",
        );
        let requests = [
            (&parent, "parent"),
            (&child, "child"),
            (&grandchild, "grandchild"),
        ]
        .map(|(path, id)| DeleteSessionRequest {
            provider_id: "codex".into(),
            session_id: id.into(),
            source_path: path.to_string_lossy().into_owned(),
            include_project: false,
        });
        let descriptors = rollout_descriptors_in_roots(&parent, vec![temp.path().to_path_buf()]);
        let order = deletion_order_with_descriptors(&requests, &descriptors);
        assert_eq!(order, vec![2, 1, 0]);
        for index in order {
            let request = &requests[index];
            delete_session(
                temp.path(),
                Path::new(&request.source_path),
                &request.session_id,
            )
            .unwrap();
        }
        assert!(!parent.exists());
        assert!(!child.exists());
        assert!(!grandchild.exists());
    }

    #[test]
    #[ignore = "Opt-in read-only audit of an explicit local rollout; never deletes data"]
    fn audit_real_rollout_read_only() {
        let path =
            PathBuf::from(std::env::var("ASM_AUDIT_CODEX_PATH").expect("explicit audit path"));
        let canonical = path.canonicalize().unwrap();
        let ordinary = resolve_paginated_chain(&path).unwrap();
        let extended = resolve_paginated_chain(&canonical).unwrap();
        assert_eq!(ordinary.len(), extended.len());
        for (a, b) in ordinary.iter().zip(&extended) {
            assert!(same_rollout_path(&a.path, &b.path));
            assert_eq!(a.end_byte_offset, b.end_byte_offset);
        }
        let id = read_rollout_descriptor(&path).unwrap().session_id;
        println!(
            "audit: {} segments, session {}, deletion preflight: {:?}",
            extended.len(),
            id,
            validate_delete(&canonical, &id)
        );
    }

    #[test]
    fn cross_session_branch_loads_history_without_hiding_parent_session() {
        let temp = tempdir().expect("tempdir");
        let parent_id = "01a076ed-bae5-7a02-a191-d9a4b880afc5";
        let parent_page_id = "01a08757-bd33-7643-96cb-7d8f560cbbe3";
        let child_id = "01a08a9d-628b-7e60-ad4a-1ecda54372dc";
        let parent_base = temp
            .path()
            .join(format!("rollout-2026-09-06T22-34-46-{parent_id}.jsonl"));
        let parent_current = temp.path().join(format!(
            "rollout-2026-09-10T03-04-29-{parent_id}_{parent_page_id}.jsonl"
        ));
        let child = temp
            .path()
            .join(format!("rollout-2026-09-10T18-19-25-{child_id}.jsonl"));
        write_codex_session(&parent_base, parent_id, "Parent earlier message");
        write_paginated_session_with_base(
            &parent_current,
            parent_id,
            parent_id,
            parent_base.metadata().expect("parent base metadata").len(),
            "Parent current message",
        );
        write_paginated_session_with_base(
            &child,
            child_id,
            parent_page_id,
            parent_current
                .metadata()
                .expect("parent current metadata")
                .len(),
            "Child message",
        );

        let messages = load_messages(&child).expect("load inherited branch history");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].content, "Parent earlier message");
        assert_eq!(messages[1].content, "Parent current message");
        assert_eq!(messages[2].content, "Child message");

        let native_rollouts = HashMap::from([
            (
                parent_id.to_string(),
                normalized_rollout_path(&parent_current.to_string_lossy()),
            ),
            (
                child_id.to_string(),
                normalized_rollout_path(&child.to_string_lossy()),
            ),
        ]);
        let sessions = scan_sessions_in_roots_with_context(
            &[temp.path().to_path_buf()],
            &HashMap::new(),
            &CodexProjectContext::default(),
            Some(&native_rollouts),
        );
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|session| {
            session.session_id == parent_id
                && session.source_path.as_deref() == parent_current.to_str()
        }));
        assert!(sessions.iter().any(|session| {
            session.session_id == child_id && session.source_path.as_deref() == child.to_str()
        }));
    }

    #[test]
    fn deleting_cross_session_branch_keeps_parent_history() {
        let temp = tempdir().expect("tempdir");
        let parent_id = "01a076ed-bae5-7a02-a191-d9a4b880afc5";
        let child_id = "01a08a9d-628b-7e60-ad4a-1ecda54372dc";
        let parent = temp
            .path()
            .join(format!("rollout-2026-09-06T22-34-46-{parent_id}.jsonl"));
        let child = temp
            .path()
            .join(format!("rollout-2026-09-10T18-19-25-{child_id}.jsonl"));
        write_codex_session(&parent, parent_id, "Parent message");
        write_paginated_session_with_base(
            &child,
            child_id,
            parent_id,
            parent.metadata().expect("parent metadata").len(),
            "Child message",
        );

        delete_session(temp.path(), &child, child_id).expect("delete child branch");

        assert!(!child.exists());
        assert!(parent.exists());
    }

    #[test]
    fn deleting_parent_with_cross_session_branch_is_rejected() {
        let temp = tempdir().expect("tempdir");
        let parent_id = "01a076ed-bae5-7a02-a191-d9a4b880afc5";
        let child_id = "01a08a9d-628b-7e60-ad4a-1ecda54372dc";
        let parent = temp
            .path()
            .join(format!("rollout-2026-09-06T22-34-46-{parent_id}.jsonl"));
        let child = temp
            .path()
            .join(format!("rollout-2026-09-10T18-19-25-{child_id}.jsonl"));
        write_codex_session(&parent, parent_id, "Parent message");
        write_paginated_session_with_base(
            &child,
            child_id,
            parent_id,
            parent.metadata().expect("parent metadata").len(),
            "Child message",
        );

        let error = validate_delete(&parent, parent_id).expect_err("parent must be protected");

        assert!(error.contains("分支会话依赖"));
        assert!(parent.exists());
        assert!(child.exists());
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
    fn parse_session_truncates_long_titles_across_all_sources() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        let long_first = "f".repeat(200);
        let long_latest = "a".repeat(200);
        std::fs::write(
            &path,
            format!(
                "{{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"test-id\",\"cwd\":\"/tmp/p\"}}}}\n\
                 {{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":\"{long_first}\"}}}}\n\
                 {{\"timestamp\":\"2026-03-06T21:50:14Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":\"{long_latest}\"}}}}\n",
            ),
        )
        .expect("write");

        let expected_latest = truncate_summary(&long_latest, TITLE_MAX_CHARS);
        assert_eq!(expected_latest.chars().count(), TITLE_MAX_CHARS + 3);
        let meta = parse_session(&path).unwrap();
        assert_eq!(meta.title.as_deref(), Some(expected_latest.as_str()));

        // Verify `first_user_message` branch is also truncated when `latest_user_message` is None.
        let raw_first_only = RawCodexSession {
            session_id: "first-only".into(),
            project_dir: None,
            created_at: None,
            last_active_at: None,
            first_user_message: Some(long_first.clone()),
            latest_user_message: None,
            summary: None,
            source_path: path.to_string_lossy().into_owned(),
        };
        let meta_first = raw_first_only.into_session_meta(&HashMap::new());
        assert_eq!(
            meta_first.title.as_deref(),
            Some(truncate_summary(&long_first, TITLE_MAX_CHARS).as_str())
        );

        // Verify `thread_titles` branch is also truncated to TITLE_MAX_CHARS.
        let long_saved = "t".repeat(200);
        let mut titles = HashMap::new();
        titles.insert("test-id".to_string(), long_saved.clone());
        let meta_saved = parse_session_with_titles(&path, &titles).unwrap();
        assert_eq!(
            meta_saved.title.as_deref(),
            Some(truncate_summary(&long_saved, TITLE_MAX_CHARS).as_str())
        );
    }

    #[test]
    fn state_6_sqlite_discovery_and_multi_generation_cleanup() {
        let temp = tempdir().expect("tempdir");
        let dir = temp.path();

        // Create stale state_5.sqlite and newer state_6.sqlite
        let state5 = dir.join("state_5.sqlite");
        let state6 = dir.join("state_6.sqlite");
        {
            let conn5 = Connection::open(&state5).unwrap();
            conn5
                .execute_batch(
                    "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, archived INTEGER NOT NULL, title TEXT, name TEXT, first_user_message TEXT, thread_section_id TEXT);
                     CREATE TABLE thread_sections (id TEXT PRIMARY KEY, name TEXT NOT NULL);
                     INSERT INTO thread_sections VALUES ('sec-old', 'OldSection');
                     INSERT INTO threads VALUES ('stale-only', 'C:\\stale.jsonl', 0, 'Stale title', 'Stale explicit', 'q', 'sec-old');
                     INSERT INTO threads VALUES ('target-1', 'C:\\old_target.jsonl', 0, 'Old title', 'Old explicit', 'q', 'sec-old');
                     INSERT INTO threads VALUES ('keep-1', 'C:\\keep.jsonl', 0, 'Keep', 'Keep', 'q', NULL);",
                )
                .unwrap();

            let conn6 = Connection::open(&state6).unwrap();
            conn6
                .execute_batch(
                    "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, archived INTEGER NOT NULL, title TEXT, name TEXT, first_user_message TEXT, thread_section_id TEXT);
                     CREATE TABLE thread_sections (id TEXT PRIMARY KEY, name TEXT NOT NULL);
                     INSERT INTO thread_sections VALUES ('sec-6', 'Gen6Section');
                     INSERT INTO threads VALUES ('target-1', 'C:\\gen6_target.jsonl', 0, 'Gen6 auto', 'Gen6 explicit', 'q', 'sec-6');
                     INSERT INTO threads VALUES ('keep-1', 'C:\\gen6_keep.jsonl', 0, 'Keep auto', 'Keep explicit', 'q', NULL);",
                )
                .unwrap();
        }

        // Create numbered auxiliary DBs across generations
        let th1 = dir.join("thread_history_1.sqlite");
        let th2 = dir.join("thread_history_2.sqlite");
        let logs2 = dir.join("logs_2.sqlite");
        let mem1 = dir.join("memories_1.sqlite");
        let goals1 = dir.join("goals_1.sqlite");
        let queue1 = dir.join("queue_1.sqlite");

        for aux in [&th1, &th2, &logs2, &mem1, &goals1, &queue1] {
            let conn = Connection::open(aux).unwrap();
            conn.execute_batch(
                "CREATE TABLE items (thread_id TEXT NOT NULL);
                 INSERT INTO items VALUES ('target-1'), ('keep-1');",
            )
            .unwrap();
        }

        // Highest generation discovery for reading active rollouts, titles, sections, and thread_history
        let config = format!(
            "sqlite_home = {}",
            serde_json::to_string(&dir.to_string_lossy()).unwrap()
        );
        let active = load_native_active_rollouts_in(dir, &config).expect("active rollouts");
        assert!(!active.contains_key("stale-only"));
        assert_eq!(
            active.get("target-1"),
            Some(&rollout_path_key(Path::new(r"C:\gen6_target.jsonl")))
        );

        let titles = load_thread_titles_in(dir, &config);
        assert!(!titles.contains_key("stale-only"));
        assert_eq!(
            titles.get("target-1").map(String::as_str),
            Some("Gen6 explicit")
        );

        let ctx = load_project_context_in(dir, &config);
        assert!(!ctx.thread_sections.contains_key("stale-only"));
        assert_eq!(ctx.thread_sections["target-1"].name, "Gen6Section");

        assert_eq!(codex_primary_thread_history_db_path(dir), th2);

        // Cleanup must purge target-1 from ALL existing generations & auxiliary SQLite DBs
        cleanup_index_databases(dir, &config, "target-1").expect("cleanup all dbs");

        for state_db in [&state5, &state6] {
            let conn = Connection::open(state_db).unwrap();
            let target_cnt: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE id = 'target-1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let keep_cnt: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE id = 'keep-1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(target_cnt, 0, "{} still had target-1", state_db.display());
            assert_eq!(keep_cnt, 1, "{} lost keep-1", state_db.display());
        }

        for aux in [&th1, &th2, &logs2, &mem1, &goals1, &queue1] {
            let conn = Connection::open(aux).unwrap();
            let target_cnt: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM items WHERE thread_id = 'target-1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let keep_cnt: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM items WHERE thread_id = 'keep-1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(target_cnt, 0, "{} still had target-1", aux.display());
            assert_eq!(keep_cnt, 1, "{} lost keep-1", aux.display());
        }
    }

    #[test]
    fn cleanup_state_db_preserves_child_parent_session_id_rows_and_honors_fk_order() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("state_6.sqlite");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.pragma_update(None, "foreign_keys", "ON").unwrap();
            // Create `threads` and `session` FIRST so `sqlite_master` lists them BEFORE child tables,
            // proving `cleanup_state_db` sorts entity tables LAST and respects FK constraints!
            conn.execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    parent_session_id TEXT,
                    title TEXT
                );
                CREATE TABLE session (
                    id TEXT PRIMARY KEY,
                    parent_session_id TEXT
                );
                CREATE TABLE logs (
                    id INTEGER PRIMARY KEY,
                    thread_id TEXT NOT NULL REFERENCES threads(id)
                );
                CREATE TABLE stage1_outputs (
                    thread_id TEXT PRIMARY KEY REFERENCES threads(id)
                );
                CREATE TABLE agent_job_items (
                    id TEXT PRIMARY KEY,
                    assigned_thread_id TEXT REFERENCES threads(id)
                );
                CREATE TABLE thread_spawn_edges (
                    parent_thread_id TEXT NOT NULL REFERENCES threads(id),
                    child_thread_id TEXT NOT NULL REFERENCES threads(id),
                    parent_session_id TEXT,
                    child_session_id TEXT
                );
                INSERT INTO threads VALUES ('parent-1', NULL, 'Parent'), ('child-1', 'parent-1', 'Child');
                INSERT INTO session VALUES ('parent-1', NULL), ('child-1', 'parent-1');
                INSERT INTO logs VALUES (1, 'parent-1'), (2, 'child-1');
                INSERT INTO stage1_outputs VALUES ('parent-1'), ('child-1');
                INSERT INTO agent_job_items VALUES ('job-1', 'parent-1'), ('job-2', 'child-1');
                INSERT INTO thread_spawn_edges VALUES ('parent-1', 'child-1', 'parent-1', 'child-1');",
            )
            .unwrap();
        }

        cleanup_state_db(&db_path, "parent-1").expect("cleanup parent-1 with FK constraints");

        let conn = Connection::open(&db_path).unwrap();
        let remaining_threads: Vec<(String, Option<String>)> = conn
            .prepare("SELECT id, parent_session_id FROM threads ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(
            remaining_threads,
            vec![("child-1".to_string(), Some("parent-1".to_string()))],
            "child session row in `threads` with parent_session_id='parent-1' must be preserved!"
        );

        let remaining_sessions: Vec<String> = conn
            .prepare("SELECT id FROM session ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(remaining_sessions, vec!["child-1".to_string()]);

        let edge_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM thread_spawn_edges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(edge_count, 0);

        let job_ids: Vec<String> = conn
            .prepare("SELECT id FROM agent_job_items")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(job_ids, vec!["job-2".to_string()]);
    }

    #[test]
    fn cleanup_index_databases_removes_orphaned_hidden_subagent_rollouts_only() {
        let temp = tempdir().expect("tempdir");
        let sessions_dir = temp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let parent_file = sessions_dir.join("parent.jsonl");
        write_codex_session(&parent_file, "parent-1", "Parent task");

        let sub1_file = sessions_dir.join("sub-1.jsonl");
        std::fs::write(
            &sub1_file,
            "{\"timestamp\":\"2026-04-28T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"sub-1\",\"cwd\":\"/tmp/project\",\"source\":{\"subagent\":{\"thread_spawn\":{\"parent_thread_id\":\"parent-1\",\"depth\":1}}}}}\n",
        )
        .unwrap();

        let sub2_file = sessions_dir.join("sub-2.jsonl");
        std::fs::write(
            &sub2_file,
            "{\"timestamp\":\"2026-04-28T10:01:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"sub-2\",\"cwd\":\"/tmp/project\",\"source\":{\"subagent\":{\"thread_spawn\":{\"parent_thread_id\":\"sub-1\",\"depth\":2}}}}}\n",
        )
        .unwrap();

        // Non-subagent child rollout must NEVER be deleted by hidden subagent cleanup
        let visible_child_file = sessions_dir.join("visible-child.jsonl");
        write_codex_session(&visible_child_file, "visible-child", "User branch");

        // Subagent file outside configured `sessions/` root must NEVER be deleted
        let outside_dir = tempdir().expect("outside");
        let outside_sub_file = outside_dir.path().join("outside-sub.jsonl");
        std::fs::write(
            &outside_sub_file,
            "{\"timestamp\":\"2026-04-28T10:02:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"outside-sub\",\"cwd\":\"/tmp/project\",\"source\":{\"subagent\":{\"thread_spawn\":{\"parent_thread_id\":\"parent-1\",\"depth\":1}}}}}\n",
        )
        .unwrap();

        let state_path = temp.path().join("state_6.sqlite");
        {
            let conn = Connection::open(&state_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, source TEXT);
                 CREATE TABLE thread_spawn_edges (parent_thread_id TEXT NOT NULL, child_thread_id TEXT NOT NULL);",
            )
            .unwrap();
            for (id, file, src) in [
                ("parent-1", &parent_file, "cli"),
                ("sub-1", &sub1_file, "{\"subagent\":{}}"),
                ("sub-2", &sub2_file, "{\"subagent\":{}}"),
                ("visible-child", &visible_child_file, "cli"),
                ("outside-sub", &outside_sub_file, "{\"subagent\":{}}"),
            ] {
                conn.execute(
                    "INSERT INTO threads VALUES (?1, ?2, ?3)",
                    rusqlite::params![id, file.to_string_lossy().to_string(), src],
                )
                .unwrap();
            }
            for (p, c) in [
                ("parent-1", "sub-1"),
                ("sub-1", "sub-2"),
                ("parent-1", "visible-child"),
                ("parent-1", "outside-sub"),
            ] {
                conn.execute(
                    "INSERT INTO thread_spawn_edges VALUES (?1, ?2)",
                    rusqlite::params![p, c],
                )
                .unwrap();
            }
        }

        let history_path = temp.path().join("thread_history_2.sqlite");
        {
            let conn = Connection::open(&history_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE thread_items (thread_id TEXT NOT NULL);
                 INSERT INTO thread_items VALUES ('parent-1'), ('sub-1'), ('sub-2'), ('visible-child');",
            )
            .unwrap();
        }

        let config = format!(
            "sqlite_home = {}",
            serde_json::to_string(&temp.path().to_string_lossy()).unwrap()
        );
        cleanup_index_databases(temp.path(), &config, "parent-1").unwrap();

        assert!(
            !sub1_file.exists(),
            "hidden subagent 1 rollout must be deleted"
        );
        assert!(
            !sub2_file.exists(),
            "nested hidden subagent 2 rollout must be deleted"
        );
        assert!(
            visible_child_file.exists(),
            "non-subagent rollout must remain intact"
        );
        assert!(
            outside_sub_file.exists(),
            "rollout outside session roots must remain intact"
        );

        let conn = Connection::open(&state_path).unwrap();
        let remaining_threads: Vec<String> = conn
            .prepare("SELECT id FROM threads ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(
            remaining_threads,
            vec!["outside-sub".to_string(), "visible-child".to_string()]
        );

        let hconn = Connection::open(&history_path).unwrap();
        let remaining_hist: Vec<String> = hconn
            .prepare("SELECT thread_id FROM thread_items ORDER BY thread_id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(remaining_hist, vec!["visible-child".to_string()]);
    }

    #[test]
    fn archived_rollout_classification_and_stale_threads_path_reconciliation() {
        let temp = tempdir().expect("tempdir");
        let active_dir = temp.path().join("sessions");
        let archived_dir = temp.path().join("archived_sessions");
        std::fs::create_dir_all(&active_dir).unwrap();
        std::fs::create_dir_all(&archived_dir).unwrap();

        let archived_file = archived_dir.join("rollout-arch.jsonl");
        write_codex_session(&archived_file, "arch-1", "Archived conversation");

        let stale_sessions_path = active_dir.join("rollout-arch.jsonl");
        // Simulate `threads` row still pointing to old `sessions/` path after file moved to `archived_sessions/`
        let native_rollouts = HashMap::from([(
            "arch-1".to_string(),
            stale_sessions_path.to_string_lossy().into_owned(),
        )]);

        let mut sessions = scan_sessions_in_roots_with_context(
            &[active_dir.clone(), archived_dir.clone()],
            &HashMap::new(),
            &CodexProjectContext::default(),
            Some(&native_rollouts),
        );
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].archived);
        assert!(!sessions[0].residual);
        assert!(sessions[0].resume_command.is_none());

        let indexed = HashMap::from([(
            "arch-1".to_string(),
            stale_sessions_path.to_string_lossy().into_owned(),
        )]);
        reconcile_indexed_rollouts(
            &mut sessions,
            indexed.clone(),
            Some(&native_rollouts),
            &HashMap::new(),
        );
        // Must reconcile instead of emitting a duplicate `缺少日志的任务` ghost!
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].archived);
        assert!(!sessions[0].residual);
        assert!(!sessions[0].cleanup_pending);
        assert!(should_cleanup_index_from_rollouts(
            "arch-1",
            &archived_file.to_string_lossy(),
            Some(&indexed),
        ));
    }

    #[test]
    fn resolve_paginated_chain_uses_o1_fast_path_for_non_paginated_rollouts() {
        let temp = tempdir().expect("tempdir");
        let standalone = temp.path().join("standalone.jsonl");
        let base = temp.path().join("base.jsonl");
        let leaf = temp.path().join("leaf.jsonl");
        write_codex_session(&standalone, "standalone-id", "Quick open");
        write_codex_session(&base, "paged-id", "Page 1");
        write_paginated_session(&leaf, "paged-id", base.metadata().unwrap().len(), "Page 2");

        let before = PAGINATED_DIR_SCANS.with(|s| s.get());
        let single_chain = super::resolve_paginated_chain(&standalone).expect("O(1) chain");
        let after_single = PAGINATED_DIR_SCANS.with(|s| s.get());
        assert_eq!(single_chain.len(), 1);
        assert_eq!(single_chain[0].session_id, "standalone-id");
        assert_eq!(
            after_single - before,
            0,
            "non-paginated rollout must NOT walk directories"
        );

        let paged_chain = super::resolve_paginated_chain(&leaf).expect("paginated chain");
        let after_paged = PAGINATED_DIR_SCANS.with(|s| s.get());
        assert_eq!(paged_chain.len(), 2);
        assert_eq!(
            after_paged - after_single,
            1,
            "paginated rollout scans roots once to resolve history_base"
        );
    }

    #[test]
    fn load_messages_supports_reasoning_shell_web_search_compacted_and_legacy_event_msg() {
        let temp = tempdir().expect("tempdir");

        // 1. Rich response_item types + compacted summary (with non-duplicated event_msg)
        let rich = temp.path().join("rich.jsonl");
        std::fs::write(
            &rich,
            concat!(
                "{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"rich-id\",\"cwd\":\"/tmp\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"Check build\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Check build\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:14Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"Analyzing Cargo.toml\"}]}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:15Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"local_shell_call\",\"action\":{\"type\":\"exec\",\"command\":[\"cargo\",\"check\",\"--locked\"]}}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:16Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"web_search_call\",\"action\":{\"type\":\"search\",\"query\":\"rusqlite TransactionBehavior Immediate\"}}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:17Z\",\"type\":\"compacted\",\"payload\":{\"message\":\"Earlier context summarized\",\"replacement_history\":[{\"type\":\"message\",\"role\":\"user\",\"content\":\"Old prompt\"}]}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:18Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"All checks green.\"}}\n",
                "{\"timestamp\":\"2026-03-06T21:50:18Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"All checks green.\"}}\n",
            ),
        )
        .unwrap();

        let msgs = load_messages(&rich).expect("load rich messages");
        assert_eq!(msgs.len(), 6);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content, "Check build");
        assert_eq!(msgs[1].role, "thinking");
        assert_eq!(msgs[1].content, "Analyzing Cargo.toml");
        assert_eq!(msgs[2].role, "tool");
        assert_eq!(msgs[2].content, "$ cargo check --locked");
        assert_eq!(msgs[3].role, "tool");
        assert_eq!(
            msgs[3].content,
            "[Web search] rusqlite TransactionBehavior Immediate"
        );
        assert_eq!(msgs[4].role, "assistant");
        assert_eq!(
            msgs[4].content,
            "[Compacted Summary]\nEarlier context summarized"
        );
        assert_eq!(msgs[5].role, "assistant");
        assert_eq!(msgs[5].content, "All checks green.");

        // 2. Compacted-only rollout with `replacement_history` and zero `response_item` lines
        let compacted_only = temp.path().join("compacted_only.jsonl");
        std::fs::write(
            &compacted_only,
            concat!(
                "{\"timestamp\":\"2026-03-06T22:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"comp-id\",\"cwd\":\"/tmp\"}}\n",
                "{\"timestamp\":\"2026-03-06T22:00:01Z\",\"type\":\"compacted\",\"payload\":{\"message\":\"\",\"replacement_history\":[{\"type\":\"message\",\"role\":\"user\",\"content\":\"Recovered user\"},{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"Recovered assistant\"}]}}\n",
            ),
        )
        .unwrap();
        let comp_msgs = load_messages(&compacted_only).expect("load compacted replacement_history");
        assert_eq!(comp_msgs.len(), 2);
        assert_eq!(comp_msgs[0].role, "user");
        assert_eq!(comp_msgs[0].content, "Recovered user");
        assert_eq!(comp_msgs[1].role, "assistant");
        assert_eq!(comp_msgs[1].content, "Recovered assistant");

        // 3. Legacy `event_msg`-only rollout
        let legacy_exec = temp.path().join("legacy_exec.jsonl");
        std::fs::write(
            &legacy_exec,
            concat!(
                "{\"timestamp\":\"2026-03-06T23:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"exec-id\",\"cwd\":\"/tmp\"}}\n",
                "{\"timestamp\":\"2026-03-06T23:00:01Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Legacy exec prompt\"}}\n",
                "{\"timestamp\":\"2026-03-06T23:00:02Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"Legacy exec reply\"}}\n",
            ),
        )
        .unwrap();
        let exec_msgs = load_messages(&legacy_exec).expect("load legacy event_msg rollout");
        assert_eq!(exec_msgs.len(), 2);
        assert_eq!(exec_msgs[0].role, "user");
        assert_eq!(exec_msgs[0].content, "Legacy exec prompt");
        assert_eq!(exec_msgs[1].role, "assistant");
        assert_eq!(exec_msgs[1].content, "Legacy exec reply");
    }

    #[test]
    fn oversized_index_and_global_state_files_are_bounded() {
        let temp = tempdir().expect("tempdir");
        let huge_index = temp.path().join(CODEX_SESSION_INDEX_FILENAME);
        File::create(&huge_index)
            .unwrap()
            .set_len(MAX_INDEX_FILE_BYTES + 1)
            .unwrap();
        assert!(load_thread_titles_from_session_index(&huge_index).is_empty());
        assert!(rewrite_session_index(&huge_index, "thread-1").is_err());

        let huge_global = temp.path().join(".codex-global-state.json");
        File::create(&huge_global)
            .unwrap()
            .set_len(MAX_INDEX_FILE_BYTES + 1)
            .unwrap();
        assert!(cleanup_global_state(&huge_global, "thread-1").is_err());
        let ctx = load_project_context_in(temp.path(), "");
        assert!(ctx.thread_projects.is_empty());
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
