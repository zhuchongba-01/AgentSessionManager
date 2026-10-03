use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::session_manager::{DeleteSessionRequest, SessionMessage, SessionMeta};

use super::utils::{
    checked_storage_child, is_safe_session_id, path_basename, scan_warning, truncate_summary,
    MAX_MESSAGES_FILE_BYTES, TITLE_MAX_CHARS,
};

const PROVIDER_ID: &str = "deepseek";
const MAX_INDEX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_HEADER_BYTES: u64 = 4 * 1024 * 1024;
const SUPPORTED_SESSION_FORMAT: i64 = 4;

#[derive(Debug)]
struct DecodedSession {
    header: Value,
    events: Vec<Value>,
}

#[derive(Debug)]
struct SessionDescriptor {
    id: String,
    parent_session: Option<String>,
    origin: Option<String>,
    source_path: PathBuf,
}

#[derive(Debug, Default)]
struct WorkspaceCatalog {
    archived: HashSet<String>,
    assignments: HashMap<String, WorkspaceAssignment>,
}

#[derive(Debug)]
struct WorkspaceAssignment {
    project_name: Option<String>,
    project_dir: String,
}

pub fn dsh_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".dsh")
}

pub fn session_roots() -> Vec<PathBuf> {
    vec![dsh_home().join("sessions")]
}

fn workspace_index_path(home: &Path) -> PathBuf {
    home.join("storages").join("workspace.json")
}

fn projection_cache_root(home: &Path) -> PathBuf {
    home.join("storages")
        .join("session_projcache")
        .join("sessions")
}

pub fn scan_sessions() -> Vec<SessionMeta> {
    scan_sessions_at(&dsh_home())
}

fn scan_sessions_at(home: &Path) -> Vec<SessionMeta> {
    let catalog = match workspace_catalog(home) {
        Ok(catalog) => catalog,
        Err(error) => {
            scan_warning(error);
            WorkspaceCatalog::default()
        }
    };
    let root = home.join("sessions");
    // A genuinely absent installation is not a scan failure. All other errors,
    // including unreadable nested directories, must remain visible.
    if fs::symlink_metadata(&root).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return Vec::new();
    }
    let discovery = discover_session_logs(&root);
    for warning in discovery.warnings {
        scan_warning(warning);
    }
    discovery
        .logs
        .into_iter()
        .filter_map(|path| match parse_session_meta(&path, &catalog) {
            Ok(Some(session)) => Some(session),
            Ok(None) => None,
            Err(error) => {
                scan_warning(error);
                None
            }
        })
        .collect()
}

#[derive(Default)]
struct SessionDiscovery {
    logs: Vec<PathBuf>,
    warnings: Vec<String>,
}

impl SessionDiscovery {
    fn warn(&mut self, warning: String) {
        // Bounded diagnostics, but never turn an incomplete scan into success.
        if self.warnings.len() < 20 {
            self.warnings.push(warning);
        }
    }

    fn complete(self) -> Result<Vec<PathBuf>, String> {
        if self.warnings.is_empty() {
            Ok(self.logs)
        } else {
            Err(format!(
                "DeepSeek Harness 依赖扫描不完整，已停止删除：{}",
                self.warnings.join("；")
            ))
        }
    }
}

fn child_directories(root: &Path, discovery: &mut SessionDiscovery) -> Vec<PathBuf> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            discovery.warn(format!(
                "无法枚举 DeepSeek Harness 目录 {}：{error}",
                root.display()
            ));
            return Vec::new();
        }
    };
    let mut directories = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                discovery.warn(format!(
                    "无法读取 DeepSeek Harness 目录项 {}：{error}",
                    root.display()
                ));
                continue;
            }
        };
        match entry.file_type() {
            Ok(kind) if kind.is_symlink() => discovery.warn(format!(
                "DeepSeek Harness 目录包含未跟随的链接：{}",
                entry.path().display()
            )),
            Ok(kind) if kind.is_dir() => directories.push(entry.path()),
            Ok(_) => {}
            Err(error) => discovery.warn(format!(
                "无法检查 DeepSeek Harness 目录项 {}：{error}",
                entry.path().display()
            )),
        }
    }
    directories.sort();
    directories
}

fn discover_session_logs(root: &Path) -> SessionDiscovery {
    let mut discovery = SessionDiscovery::default();
    for project in child_directories(root, &mut discovery) {
        for session in child_directories(&project, &mut discovery) {
            match select_generation(&session) {
                Ok(Some(path)) => discovery.logs.push(path),
                Ok(None) => {}
                Err(error) => discovery.warn(error),
            }
        }
    }
    discovery
}

fn generation_of_filename(name: &str) -> Option<(u64, bool)> {
    let (without_compression, compressed) = match name.strip_suffix(".zstd") {
        Some(value) => (value, true),
        None => (name, false),
    };
    if without_compression == "session.jsonl" {
        return Some((0, compressed));
    }
    let version = without_compression
        .strip_prefix("session.v")?
        .strip_suffix(".jsonl")?;
    if version.is_empty()
        || !version.bytes().all(|byte| byte.is_ascii_digit())
        || (version.len() > 1 && version.starts_with('0'))
    {
        return None;
    }
    version.parse().ok().map(|value| (value, compressed))
}

fn select_generation(session_dir: &Path) -> Result<Option<PathBuf>, String> {
    let entries = fs::read_dir(session_dir).map_err(|error| {
        format!(
            "无法读取 DeepSeek Harness 会话目录 {}：{error}",
            session_dir.display()
        )
    })?;
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Some((generation, compressed)) = generation_of_filename(&name) {
            if !entry
                .file_type()
                .map_err(|error| {
                    format!(
                        "无法检查 DeepSeek Harness 会话文件 {}：{error}",
                        entry.path().display()
                    )
                })?
                .is_file()
            {
                return Err(format!(
                    "DeepSeek Harness 会话文件不是普通文件或是链接：{}",
                    entry.path().display()
                ));
            }
            candidates.push((generation, compressed, entry.path()));
        }
    }
    let Some(highest) = candidates.iter().map(|item| item.0).max() else {
        return Ok(None);
    };
    let mut highest_generation = candidates
        .into_iter()
        .filter(|item| item.0 == highest)
        .collect::<Vec<_>>();
    if highest_generation.len() != 1 {
        return Err(format!(
            "DeepSeek Harness 会话目录 {} 的最高代文件不唯一，已跳过",
            session_dir.display()
        ));
    }
    Ok(highest_generation.pop().map(|item| item.2))
}

fn open_session_reader(path: &Path) -> Result<Box<dyn Read>, String> {
    let file = File::open(path)
        .map_err(|error| format!("无法打开 DeepSeek Harness 会话 {}：{error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("无法检查 DeepSeek Harness 会话 {}：{error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "DeepSeek Harness 会话不是普通文件：{}",
            path.display()
        ));
    }
    if metadata.len() > MAX_MESSAGES_FILE_BYTES {
        return Err(format!(
            "DeepSeek Harness 会话 {} 超过 {} MB 读取上限",
            path.display(),
            MAX_MESSAGES_FILE_BYTES / 1024 / 1024
        ));
    }
    if path.extension().and_then(|value| value.to_str()) == Some("zstd") {
        Ok(Box::new(zstd::stream::read::Decoder::new(file).map_err(
            |error| format!("无法解压 DeepSeek Harness 会话 {}：{error}", path.display()),
        )?))
    } else {
        Ok(Box::new(file))
    }
}

/// Lineage lives entirely in the header. Do not replay/decompress every other
/// session's history for each deletion. Still reread headers on every preflight
/// so new children and changed parents cannot be hidden by a stale cache.
fn read_header(path: &Path) -> Result<Value, String> {
    read_header_from(open_session_reader(path)?, path)
}

fn read_header_from(reader: impl Read, path: &Path) -> Result<Value, String> {
    let mut reader = BufReader::new(reader.take(MAX_HEADER_BYTES + 1));
    let mut line = Vec::new();
    let mut total = 0;
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line).map_err(|error| {
            format!(
                "无法读取 DeepSeek Harness 会话头 {}：{error}",
                path.display()
            )
        })?;
        total += read as u64;
        if total > MAX_HEADER_BYTES {
            return Err(format!(
                "DeepSeek Harness 会话头超过 4 MB 上限：{}",
                path.display()
            ));
        }
        if read == 0 {
            return Err(format!("DeepSeek Harness 会话头缺失：{}", path.display()));
        }
        if line == b"\n" {
            continue;
        }
        let header: Value = serde_json::from_slice(&line)
            .map_err(|error| format!("DeepSeek Harness 会话头损坏 {}：{error}", path.display()))?;
        validate_header(&header, path)?;
        return Ok(header);
    }
}

fn read_session(path: &Path) -> Result<DecodedSession, String> {
    let mut reader = open_session_reader(path)?;
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(MAX_MESSAGES_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("无法读取 DeepSeek Harness 会话 {}：{error}", path.display()))?;
    if bytes.len() as u64 > MAX_MESSAGES_FILE_BYTES {
        return Err(format!(
            "DeepSeek Harness 会话 {} 解压后超过 {} MB 读取上限",
            path.display(),
            MAX_MESSAGES_FILE_BYTES / 1024 / 1024
        ));
    }
    let mut lines = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty());
    let header_bytes = lines
        .next()
        .ok_or_else(|| format!("DeepSeek Harness 会话为空：{}", path.display()))?;
    if header_bytes.len() as u64 > MAX_HEADER_BYTES {
        return Err(format!(
            "DeepSeek Harness 会话头超过 4 MB 上限：{}",
            path.display()
        ));
    }
    let header: Value = serde_json::from_slice(header_bytes)
        .map_err(|error| format!("DeepSeek Harness 会话头损坏 {}：{error}", path.display()))?;
    validate_header(&header, path)?;
    let mut events = Vec::new();
    for (index, line) in lines.enumerate() {
        let event: Value = serde_json::from_slice(line).map_err(|error| {
            format!(
                "DeepSeek Harness 会话事件损坏 {}（第 {} 行）：{error}",
                path.display(),
                index + 2
            )
        })?;
        let expected_seq = index as u64;
        if event.get("type").and_then(Value::as_str).is_none()
            || event.get("seq").and_then(Value::as_u64) != Some(expected_seq)
            || event.get("time").and_then(Value::as_i64).is_none()
            || event.get("data").is_none()
        {
            return Err(format!(
                "DeepSeek Harness 会话事件包络无效 {}（第 {} 行，期望 seq={expected_seq}）",
                path.display(),
                index + 2
            ));
        }
        events.push(event);
    }
    Ok(DecodedSession { header, events })
}

fn validate_header(header: &Value, path: &Path) -> Result<(), String> {
    let object = header
        .as_object()
        .ok_or_else(|| format!("DeepSeek Harness 会话头不是对象：{}", path.display()))?;
    if object.get("type").and_then(Value::as_str) != Some("session")
        || object.get("id").and_then(Value::as_str).is_none()
        || object.get("createdAt").and_then(Value::as_i64).is_none()
    {
        return Err(format!(
            "DeepSeek Harness 会话头字段无效：{}",
            path.display()
        ));
    }
    let id = object.get("id").and_then(Value::as_str).unwrap_or_default();
    if !is_safe_session_id(id) {
        return Err(format!(
            "DeepSeek Harness 会话 ID 不安全：{}",
            path.display()
        ));
    }
    // Optional does not mean malformed values can be treated as no parent.
    match object.get("parentSession") {
        None | Some(Value::Null) => {}
        Some(Value::String(parent)) if is_safe_session_id(parent) => {}
        _ => {
            return Err(format!(
                "DeepSeek Harness parentSession 字段无效：{}",
                path.display()
            ))
        }
    }
    let version = object
        .get("version")
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("DeepSeek Harness 会话缺少格式版本：{}", path.display()))?;
    if !(0..=SUPPORTED_SESSION_FORMAT).contains(&version) {
        return Err(format!(
            "DeepSeek Harness 会话格式 v{version} 高于当前支持的 v{SUPPORTED_SESSION_FORMAT}：{}",
            path.display()
        ));
    }
    Ok(())
}

fn append_surface_event(event: &Value) -> bool {
    event.get("surfaceOp").and_then(Value::as_str) == Some("append")
}

fn visible_message_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(block) if block.get("type").and_then(Value::as_str) == Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

fn user_text(event: &Value) -> Option<String> {
    if event.get("type").and_then(Value::as_str) != Some("user/message")
        || !append_surface_event(event)
        || event.pointer("/data/source/kind").and_then(Value::as_str) != Some("user")
    {
        return None;
    }
    let text = event.pointer("/data/content").map(visible_message_text)?;
    (!text.trim().is_empty()).then_some(text)
}

fn assistant_text(event: &Value) -> Option<String> {
    if event.get("type").and_then(Value::as_str) != Some("assistant/message")
        || !append_surface_event(event)
    {
        return None;
    }
    let text = event
        .pointer("/data/message/content")
        .map(visible_message_text)?;
    (!text.trim().is_empty()).then_some(text)
}

fn event_time(event: &Value) -> Option<i64> {
    event.get("time").and_then(Value::as_i64)
}

fn parse_session_meta(
    path: &Path,
    catalog: &WorkspaceCatalog,
) -> Result<Option<SessionMeta>, String> {
    let decoded = read_session(path)?;
    let id = decoded
        .header
        .get("id")
        .and_then(Value::as_str)
        .ok_or("DeepSeek Harness 会话缺少 ID")?
        .to_string();
    if !is_safe_session_id(&id) {
        return Err(format!("DeepSeek Harness 会话 ID 不安全，已跳过：{id}"));
    }
    let is_subagent = decoded.header.get("origin").and_then(Value::as_str) == Some("subagent");
    let directory_id = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str());
    if directory_id != Some(id.as_str()) {
        return Err(format!(
            "DeepSeek Harness 会话目录与 ID 不一致：{}",
            path.display()
        ));
    }
    let assignment = catalog.assignments.get(&id);
    // Match Harness's sessionListMetadata reducer: a persisted session remains
    // blank until the first committed turn/start. Initial policy, sandbox and
    // seed events do not make it a sidebar conversation.
    let is_blank = !decoded
        .events
        .iter()
        .any(|event| event.get("type").and_then(Value::as_str) == Some("turn/start"));
    let has_parent = decoded
        .header
        .get("parentSession")
        .and_then(Value::as_str)
        .is_some();
    // Hidden lineage nodes must remain available as residual cleanup targets,
    // otherwise they could protect a parent forever without a removable row.
    let hidden_dependency = is_blank && has_parent;
    if is_blank && !hidden_dependency && !is_subagent {
        return Ok(None);
    }
    let explicit_title = decoded.events.iter().rev().find_map(|event| {
        (event.get("type").and_then(Value::as_str) == Some("session/title"))
            .then(|| event.pointer("/data/title").and_then(Value::as_str))
            .flatten()
            .filter(|title| !title.trim().is_empty())
    });
    let first_user = decoded.events.iter().find_map(user_text);
    let last_visible = decoded
        .events
        .iter()
        .rev()
        .find_map(|event| assistant_text(event).or_else(|| user_text(event)));
    let title = explicit_title
        .map(|value| truncate_summary(value, TITLE_MAX_CHARS))
        .or_else(|| {
            first_user
                .as_deref()
                .map(|value| truncate_summary(value, TITLE_MAX_CHARS))
        });
    let created_at = decoded.header.get("createdAt").and_then(Value::as_i64);
    // Match Harness's session-list projection: recency is the latest real
    // user prompt, not an internal title/tool event.
    let last_prompt_at = decoded
        .events
        .iter()
        .rev()
        .filter(|event| {
            event.get("type").and_then(Value::as_str) == Some("user/message")
                && event.pointer("/data/source/kind").and_then(Value::as_str) == Some("user")
        })
        .find_map(event_time);
    let last_active_at = match (created_at, last_prompt_at) {
        (Some(created), Some(prompt)) => Some(created.max(prompt)),
        (created, prompt) => prompt.or(created),
    };

    Ok(Some(SessionMeta {
        provider_id: PROVIDER_ID.to_string(),
        session_id: id.clone(),
        // Harness does not place internal subagent sessions in its ordinary
        // workspace conversation list. Surface them as residuals here so a
        // parent dependency can be resolved deliberately instead of making
        // the parent permanently undeletable.
        residual: is_subagent || hidden_dependency,
        archived: catalog.archived.contains(&id),
        cleanup_pending: false,
        sidebar_section: None,
        title,
        summary: last_visible
            .as_deref()
            .map(|value| truncate_summary(value, 160)),
        project_name: assignment.and_then(|workspace| workspace.project_name.clone()),
        project_dir: assignment.map(|workspace| workspace.project_dir.clone()),
        created_at,
        last_active_at,
        source_path: Some(path.to_string_lossy().to_string()),
        resume_command: None,
    }))
}

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    let decoded = read_session(path)?;
    let mut messages = Vec::new();
    for event in decoded.events {
        let (role, content) = if let Some(content) = user_text(&event) {
            ("user", content)
        } else if let Some(content) = assistant_text(&event) {
            ("assistant", content)
        } else {
            continue;
        };
        messages.push(SessionMessage {
            role: role.to_string(),
            content,
            ts: event_time(&event),
        });
    }
    Ok(messages)
}

fn read_index(home: &Path) -> Result<Option<Value>, String> {
    let path = workspace_index_path(home);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("无法打开 DeepSeek Harness 工作区索引：{error}")),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("无法检查 DeepSeek Harness 工作区索引：{error}"))?;
    if !metadata.is_file() {
        return Err("DeepSeek Harness 工作区索引不是普通文件".into());
    }
    if metadata.len() > MAX_INDEX_BYTES {
        return Err("DeepSeek Harness 工作区索引超过安全读取上限".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_INDEX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("无法读取 DeepSeek Harness 工作区索引：{error}"))?;
    if bytes.len() as u64 > MAX_INDEX_BYTES {
        return Err("DeepSeek Harness 工作区索引超过安全读取上限".into());
    }
    let value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("DeepSeek Harness 工作区索引损坏：{error}"))?;
    validate_index(&value)?;
    Ok(Some(value))
}

fn validate_string_array(value: Option<&Value>, label: &str) -> Result<(), String> {
    let array = value
        .and_then(Value::as_array)
        .ok_or_else(|| format!("DeepSeek Harness 工作区索引缺少 {label} 数组"))?;
    if array.iter().any(|item| !item.is_string()) {
        return Err(format!("DeepSeek Harness 工作区索引 {label} 含非字符串项"));
    }
    Ok(())
}

fn validate_index(value: &Value) -> Result<(), String> {
    let workspaces = value
        .pointer("/tables/workspaces")
        .and_then(Value::as_object)
        .ok_or("DeepSeek Harness 工作区索引缺少 tables.workspaces")?;
    for (id, workspace) in workspaces {
        validate_string_array(
            workspace.get("sessionIds"),
            &format!("工作区 {id} 的 sessionIds"),
        )?;
    }
    validate_string_array(
        value.pointer("/global/archivedSessionIds"),
        "global.archivedSessionIds",
    )
}

fn workspace_catalog(home: &Path) -> Result<WorkspaceCatalog, String> {
    let Some(index) = read_index(home)? else {
        return Ok(WorkspaceCatalog::default());
    };
    let archived = index
        .pointer("/global/archivedSessionIds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let mut assignments = HashMap::new();
    let workspaces = index
        .pointer("/tables/workspaces")
        .and_then(Value::as_object)
        .ok_or("DeepSeek Harness 工作区索引缺少 tables.workspaces")?;
    for (workspace_id, workspace) in workspaces {
        let project_dir = workspace
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| !path.trim().is_empty())
            .ok_or_else(|| format!("DeepSeek Harness 工作区 {workspace_id} 缺少 path"))?
            .to_string();
        let project_name = workspace
            .get("title")
            .and_then(Value::as_str)
            .filter(|title| !title.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| path_basename(&project_dir));
        for session_id in workspace
            .get("sessionIds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !is_safe_session_id(session_id) {
                return Err(format!(
                    "DeepSeek Harness 工作区 {workspace_id} 含无效会话 ID"
                ));
            }
            if assignments
                .insert(
                    session_id.to_string(),
                    WorkspaceAssignment {
                        project_name: project_name.clone(),
                        project_dir: project_dir.clone(),
                    },
                )
                .is_some()
            {
                return Err(format!(
                    "DeepSeek Harness 会话 {session_id} 同时属于多个工作区"
                ));
            }
        }
    }
    Ok(WorkspaceCatalog {
        archived,
        assignments,
    })
}

fn write_index_atomically(path: &Path, value: &Value) -> Result<(), String> {
    let parent = path.parent().ok_or("DeepSeek Harness 索引路径无效")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("无法创建 DeepSeek Harness 索引临时文件：{error}"))?;
    serde_json::to_writer(&mut temp, value)
        .map_err(|error| format!("无法序列化 DeepSeek Harness 工作区索引：{error}"))?;
    temp.write_all(b"\n")
        .map_err(|error| format!("无法写入 DeepSeek Harness 工作区索引：{error}"))?;
    temp.flush()
        .map_err(|error| format!("无法刷新 DeepSeek Harness 工作区索引：{error}"))?;
    temp.as_file()
        .sync_all()
        .map_err(|error| format!("无法同步 DeepSeek Harness 工作区索引：{error}"))?;
    temp.persist(path)
        .map_err(|error| format!("无法替换 DeepSeek Harness 工作区索引：{error}"))?;
    Ok(())
}

fn cleanup_index(home: &Path, session_id: &str) -> Result<(), String> {
    let Some(mut index) = read_index(home)? else {
        return Ok(());
    };
    let mut changed = false;
    let workspaces = index
        .pointer_mut("/tables/workspaces")
        .and_then(Value::as_object_mut)
        .ok_or("DeepSeek Harness 工作区索引结构已变化")?;
    for workspace in workspaces.values_mut() {
        let session_ids = workspace
            .get_mut("sessionIds")
            .and_then(Value::as_array_mut)
            .ok_or("DeepSeek Harness 工作区索引 sessionIds 结构已变化")?;
        let before = session_ids.len();
        session_ids.retain(|item| item.as_str() != Some(session_id));
        changed |= session_ids.len() != before;
    }
    let archived = index
        .pointer_mut("/global/archivedSessionIds")
        .and_then(Value::as_array_mut)
        .ok_or("DeepSeek Harness 工作区索引 archivedSessionIds 结构已变化")?;
    let before = archived.len();
    archived.retain(|item| item.as_str() != Some(session_id));
    changed |= archived.len() != before;
    if changed {
        write_index_atomically(&workspace_index_path(home), &index)?;
    }
    Ok(())
}

fn source_layout(
    root: &Path,
    source: &Path,
    session_id: &str,
) -> Result<(PathBuf, PathBuf), String> {
    if !is_safe_session_id(session_id) {
        return Err("DeepSeek Harness 会话 ID 不安全".into());
    }
    let root = root
        .canonicalize()
        .map_err(|error| format!("无法解析 DeepSeek Harness 会话根目录：{error}"))?;
    let source = checked_storage_child(&root, source)?;
    let relative = source
        .strip_prefix(&root)
        .map_err(|_| "DeepSeek Harness 会话路径超出存储根目录")?;
    let parts = relative.components().collect::<Vec<_>>();
    if parts.len() != 3
        || parts[1].as_os_str().to_str() != Some(session_id)
        || parts[2]
            .as_os_str()
            .to_str()
            .and_then(generation_of_filename)
            .is_none()
    {
        return Err(format!(
            "DeepSeek Harness 会话路径结构无效：{}",
            source.display()
        ));
    }
    let session_dir = source
        .parent()
        .ok_or("DeepSeek Harness 会话目录无效")?
        .to_path_buf();
    Ok((source, session_dir))
}

pub fn validate_delete_under_root(
    root: &Path,
    source: &Path,
    session_id: &str,
) -> Result<(), String> {
    let (source, session_dir) = source_layout(root, source, session_id)?;
    match fs::symlink_metadata(&session_dir) {
        Ok(_) => {
            if let Some(current) = select_generation(&session_dir)? {
                let current = current.canonicalize().map_err(|error| error.to_string())?;
                if current != source {
                    return Err(format!(
                        "DeepSeek Harness 会话代文件已变化，请重新扫描：{}",
                        source.display()
                    ));
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("无法检查 DeepSeek Harness 会话目录：{error}")),
    }
    if source.try_exists().map_err(|error| error.to_string())? {
        let header = read_header(&source)?;
        let stored_id = header.get("id").and_then(Value::as_str);
        if stored_id != Some(session_id) {
            return Err(format!(
                "DeepSeek Harness 会话 ID 不一致：期望 {session_id}，实际 {}",
                stored_id.unwrap_or("<missing>")
            ));
        }
    }
    Ok(())
}

fn descriptors(root: &Path) -> Result<Vec<SessionDescriptor>, String> {
    let mut items = Vec::new();
    for source_path in discover_session_logs(root).complete()? {
        let header = read_header(&source_path)?;
        let id = header
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("DeepSeek Harness 会话缺少 ID：{}", source_path.display()))?
            .to_string();
        // A valid header in the wrong directory is not authoritative lineage.
        let (source_path, _) = source_layout(root, &source_path, &id)?;
        items.push(SessionDescriptor {
            id,
            parent_session: header
                .get("parentSession")
                .and_then(Value::as_str)
                .map(str::to_owned),
            origin: header
                .get("origin")
                .and_then(Value::as_str)
                .map(str::to_owned),
            source_path,
        });
    }
    validate_lineage(&items)?;
    Ok(items)
}

fn validate_lineage(items: &[SessionDescriptor]) -> Result<(), String> {
    let mut by_id = HashMap::new();
    for item in items {
        if by_id.insert(item.id.as_str(), item).is_some() {
            return Err(format!("DeepSeek Harness 存在重复会话 ID：{}", item.id));
        }
    }
    // Memoize within this freshly read graph only, not across deletion requests.
    // Each edge is visited once, including very deep branch chains.
    let mut completed = HashSet::new();
    for item in items {
        let mut cursor = Some(item.id.as_str());
        let mut seen = HashSet::new();
        while let Some(id) = cursor {
            if completed.contains(id) {
                break;
            }
            if !seen.insert(id) {
                return Err("DeepSeek Harness 会话父子关系存在循环，已停止删除".into());
            }
            // Missing parents are existing orphans, not undiscovered children.
            cursor = by_id
                .get(id)
                .and_then(|item| item.parent_session.as_deref());
        }
        completed.extend(seen);
    }
    Ok(())
}

fn active_schedules(events: &[Value]) -> Result<usize, String> {
    let mut active = HashMap::<String, String>::new();
    let mut seen = HashSet::new();
    let inherited_cut = events
        .iter()
        .rev()
        .find(|event| {
            event.get("type").and_then(Value::as_str) == Some("session/end-seed")
                && event.pointer("/data/inherited").and_then(Value::as_bool) == Some(true)
        })
        .and_then(|event| event.get("seq").and_then(Value::as_u64))
        .unwrap_or(0);
    for event in events.iter().filter(|event| {
        event
            .get("seq")
            .and_then(Value::as_u64)
            .is_none_or(|seq| seq >= inherited_cut)
    }) {
        if event.get("type").and_then(Value::as_str) != Some("schedule/change") {
            continue;
        }
        let data = event
            .get("data")
            .and_then(Value::as_object)
            .ok_or("DeepSeek Harness 定时任务记录损坏")?;
        match data.get("operation").and_then(Value::as_str) {
            Some("create") => {
                let schedule = data
                    .get("schedule")
                    .and_then(Value::as_object)
                    .ok_or("DeepSeek Harness 定时任务创建记录损坏")?;
                let id = schedule
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("DeepSeek Harness 定时任务缺少 ID")?;
                let kind = schedule
                    .get("kind")
                    .and_then(Value::as_str)
                    .ok_or("DeepSeek Harness 定时任务缺少类型")?;
                if !seen.insert(id.to_string()) {
                    return Err("DeepSeek Harness 定时任务 ID 重复".into());
                }
                active.insert(id.to_string(), kind.to_string());
            }
            Some("delete") => {
                let id = data
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("DeepSeek Harness 定时任务删除记录缺少 ID")?;
                if active.remove(id).is_none() {
                    return Err("DeepSeek Harness 定时任务删除了不存在的记录".into());
                }
            }
            Some("dispatch") => {
                let id = data
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("DeepSeek Harness 定时任务触发记录缺少 ID")?;
                let Some(kind) = active.get(id) else {
                    return Err("DeepSeek Harness 定时任务触发了不存在的记录".into());
                };
                if kind != "every" {
                    active.remove(id);
                }
            }
            _ => return Err("DeepSeek Harness 定时任务操作类型未知".into()),
        }
    }
    Ok(active.len())
}

pub fn preflight_delete(root: &Path, source: &Path, session_id: &str) -> Result<(), String> {
    validate_delete_under_root(root, source, session_id)?;
    let (source, _) = source_layout(root, source, session_id)?;
    let home = root.parent().ok_or("DeepSeek Harness 根目录无效")?;
    read_index(home)?;
    if source.try_exists().map_err(|error| error.to_string())? {
        let decoded = read_session(&source)?;
        let active = active_schedules(&decoded.events)?;
        if active > 0 {
            return Err(format!(
                "该 DeepSeek Harness 会话仍有 {active} 个活动定时任务，请先在 Harness 中取消"
            ));
        }
    }
    if let Some(child) = descriptors(root)?.into_iter().find(|item| {
        item.parent_session.as_deref() == Some(session_id) && item.source_path != source
    }) {
        let child_kind = if child.origin.as_deref() == Some("subagent") {
            "子 Agent 会话"
        } else {
            "分支会话"
        };
        return Err(format!(
            "该会话仍被 DeepSeek Harness {child_kind} {} 依赖，请先删除子会话",
            child.id
        ));
    }
    Ok(())
}

pub fn delete_session(root: &Path, source: &Path, session_id: &str) -> Result<bool, String> {
    validate_delete_under_root(root, source, session_id)?;
    let (_, session_dir) = source_layout(root, source, session_id)?;
    let home = root.parent().ok_or("DeepSeek Harness 根目录无效")?;

    // Hide the row first. If a later filesystem step fails, the manager's
    // durable cleanup journal retains an explicit retry entry instead of a
    // Harness sidebar row that opens to a missing transcript.
    cleanup_index(home, session_id)?;

    if session_dir.exists() {
        fs::remove_dir_all(&session_dir).map_err(|error| {
            format!(
                "DeepSeek Harness 索引已更新，但会话目录 {} 删除失败：{error}",
                session_dir.display()
            )
        })?;
        if let Some(project_dir) = session_dir.parent() {
            if project_dir
                .read_dir()
                .is_ok_and(|mut entries| entries.next().is_none())
            {
                let _ = fs::remove_dir(project_dir);
            }
        }
    }

    let cache_root = projection_cache_root(home);
    if cache_root.exists() {
        let cache =
            checked_storage_child(&cache_root, &cache_root.join(format!("{session_id}.json")))?;
        if cache.exists() {
            fs::remove_file(&cache).map_err(|error| {
                format!(
                    "会话已删除，但 DeepSeek Harness 投影缓存 {} 清理失败：{error}",
                    cache.display()
                )
            })?;
        }
    }
    Ok(true)
}

/// Preserve the caller's order except where selected DeepSeek children must
/// run before their selected parents. Unselected children remain protected by
/// the execution-time preflight.
pub fn deletion_order(requests: &[DeleteSessionRequest], initial: &[usize]) -> Vec<usize> {
    let selected = requests
        .iter()
        .enumerate()
        .filter(|(_, request)| request.provider_id == PROVIDER_ID)
        .filter_map(|(index, request)| {
            let header = read_header(Path::new(&request.source_path)).ok()?;
            if header.get("id")?.as_str()? != request.session_id {
                return None;
            }
            Some((
                index,
                request.session_id.clone(),
                header
                    .get("parentSession")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            ))
        })
        .collect::<Vec<_>>();
    order_selected_children(&selected, initial)
}

fn order_selected_children(
    selected: &[(usize, String, Option<String>)],
    initial: &[usize],
) -> Vec<usize> {
    let positions = initial
        .iter()
        .enumerate()
        .map(|(position, index)| (*index, position))
        .collect::<HashMap<_, _>>();
    let mut by_id: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, id, _) in selected {
        if let Some(position) = positions.get(index) {
            by_id.entry(id).or_default().push(*position);
        }
    }
    let mut pending_children = vec![0usize; initial.len()];
    let mut parents = vec![Vec::new(); initial.len()];
    for (index, _, parent) in selected {
        let Some(&child) = positions.get(index) else {
            continue;
        };
        if let Some(parent_positions) = parent.as_deref().and_then(|id| by_id.get(id)) {
            for &parent in parent_positions {
                pending_children[parent] += 1;
                parents[child].push(parent);
            }
        }
    }
    // Stable Kahn ordering: earliest ready position wins. Other providers retain
    // their previously established relative order. No repeated Vec::contains.
    let mut ready = pending_children
        .iter()
        .enumerate()
        .filter_map(|(position, count)| (*count == 0).then_some(position))
        .collect::<BTreeSet<_>>();
    let mut ordered = Vec::with_capacity(initial.len());
    let mut emitted = vec![false; initial.len()];
    while let Some(position) = ready.pop_first() {
        ordered.push(initial[position]);
        emitted[position] = true;
        for &parent in &parents[position] {
            pending_children[parent] -= 1;
            if pending_children[parent] == 0 {
                ready.insert(parent);
            }
        }
    }
    // Ordering is only a hint, never permission to delete. Cycles and invalid
    // sources are rejected by each execution-time preflight without mutation.
    ordered.extend(
        initial
            .iter()
            .enumerate()
            .filter_map(|(position, index)| (!emitted[position]).then_some(*index)),
    );
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_plain_session(path: &Path, id: &str, parent: Option<&str>, origin: Option<&str>) {
        fs::create_dir_all(path.parent().expect("session parent")).expect("create session dir");
        let mut header = json!({
            "type": "session", "version": 3, "id": id, "createdAt": 1_700_000_000_000_i64,
            "cwd": "C:/work/demo", "isSeeded": false, "delegationDepth": 0
        });
        if let Some(parent) = parent {
            header["parentSession"] = json!(parent);
        }
        if let Some(origin) = origin {
            header["origin"] = json!(origin);
        }
        let rows = [
            header,
            json!({"type":"turn/start","seq":0,"time":1700000000050_i64,"data":{}}),
            json!({"type":"user/message","seq":1,"time":1700000000100_i64,"surfaceOp":"append","data":{"role":"user","source":{"kind":"plugin","plugin":"system"},"content":[{"type":"text","text":"hidden instructions"}]}}),
            json!({"type":"user/message","seq":2,"time":1700000000200_i64,"surfaceOp":"append","data":{"role":"user","source":{"kind":"user"},"content":[{"type":"text","text":"real prompt"}]}}),
            json!({"type":"assistant/message","seq":3,"time":1700000000300_i64,"surfaceOp":"append","data":{"message":{"role":"assistant","content":[{"type":"reasoning","text":"private reasoning"},{"type":"text","text":"answer"},{"type":"tool-call","name":"read","arguments":"{}"}]}}}),
            json!({"type":"session/title","seq":4,"time":1700000000400_i64,"data":{"title":"Harness title"}}),
        ];
        let text = rows
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(path, text).expect("write session");
    }

    fn write_index(home: &Path, ids: &[&str], archived: &[&str]) {
        let path = workspace_index_path(home);
        fs::create_dir_all(path.parent().expect("index parent")).expect("create index parent");
        fs::write(
            path,
            serde_json::to_vec(&json!({
                "unit": {},
                "global": {"initialized": true, "workspaceIds": ["workspace-1"], "archivedSessionIds": archived},
                "tables": {"workspaces": {"workspace-1": {"path":"C:/work/demo", "title":"demo", "sessionIds":ids, "createdAt":1, "updatedAt":2}}}
            }))
            .expect("serialize index"),
        )
        .expect("write index");
    }

    #[test]
    fn generation_parser_accepts_only_canonical_names() {
        assert_eq!(generation_of_filename("session.jsonl"), Some((0, false)));
        assert_eq!(
            generation_of_filename("session.v3.jsonl.zstd"),
            Some((3, true))
        );
        assert_eq!(generation_of_filename("session.v03.jsonl.zstd"), None);
        assert_eq!(generation_of_filename("SESSION.v3.jsonl.zstd"), None);
        assert_eq!(generation_of_filename("session.v+3.jsonl"), None);
        assert_eq!(generation_of_filename("session.v-3.jsonl"), None);
    }

    #[test]
    fn reads_concatenated_zstd_frames_used_by_harness() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("session.v3.jsonl.zstd");
        let header = json!({
            "type":"session", "version":3, "id":"session-zstd",
            "createdAt":1700000000000_i64, "isSeeded":false, "delegationDepth":0
        });
        let event = json!({
            "type":"user/message", "seq":0, "time":1700000000100_i64,
            "surfaceOp":"append", "data":{"role":"user", "source":{"kind":"user"},
            "content":[{"type":"text", "text":"compressed prompt"}]}
        });
        let mut encoded = zstd::stream::encode_all(format!("{header}\n").as_bytes(), 0)
            .expect("encode header frame");
        encoded.extend(
            zstd::stream::encode_all(format!("{event}\n").as_bytes(), 0)
                .expect("encode event frame"),
        );
        fs::write(&path, encoded).expect("write zstd frames");

        let messages = load_messages(&path).expect("read concatenated frames");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "compressed prompt");
    }

    #[test]
    fn scans_and_filters_injected_user_messages() {
        let temp = tempfile::tempdir().expect("tempdir");
        let id = "session-1";
        let path = temp
            .path()
            .join("sessions")
            .join("--C-work-demo--")
            .join(id)
            .join("session.v3.jsonl");
        write_plain_session(&path, id, None, None);
        write_index(temp.path(), &[id], &[]);

        let sessions = scan_sessions_at(temp.path());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title.as_deref(), Some("Harness title"));
        assert_eq!(sessions[0].summary.as_deref(), Some("answer"));
        assert_eq!(sessions[0].last_active_at, Some(1700000000200_i64));
        let messages = load_messages(&path).expect("load messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "real prompt");
        assert_eq!(messages[1].content, "answer");
    }

    #[test]
    fn v4_sessions_are_scanned_like_v3() {
        let temp = tempfile::tempdir().expect("tempdir");
        let id = "session-v4";
        let path = temp
            .path()
            .join("sessions")
            .join("--C-work-demo--")
            .join(id)
            .join("session.v4.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let rows = [
            json!({
                "type": "session", "version": 4, "id": id,
                "createdAt": 1700000000000_i64, "cwd": "C:/work/demo",
                "isSeeded": false, "delegationDepth": 0, "agentPreset": "standard"
            }),
            json!({"type":"turn/start","seq":0,"time":1700000000100_i64,"data":{"turn":1}}),
            json!({"type":"user/message","seq":1,"time":1700000000200_i64,"surfaceOp":"append","data":{"role":"user","source":{"kind":"user"},"content":[{"type":"text","text":"v4 prompt"}]}}),
            json!({"type":"assistant/message","seq":2,"time":1700000000300_i64,"surfaceOp":"append","data":{"turn":1,"step":1,"message":{"role":"assistant","content":[{"type":"reasoning","text":"r"},{"type":"text","text":"v4 answer"}]}}}),
            json!({"type":"session/title","seq":3,"time":1700000000400_i64,"data":{"title":"v4 title"}}),
        ];
        fs::write(
            &path,
            rows.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n",
        )
        .unwrap();
        write_index(temp.path(), &[id], &[]);

        let sessions = scan_sessions_at(temp.path());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title.as_deref(), Some("v4 title"));
        assert_eq!(sessions[0].summary.as_deref(), Some("v4 answer"));
    }

    #[test]
    fn blank_sessions_are_hidden_like_the_native_sidebar() {
        let temp = tempfile::tempdir().expect("tempdir");
        let id = "session-blank";
        let path = temp
            .path()
            .join("sessions")
            .join("--C-work-demo--")
            .join(id)
            .join("session.v3.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let header = json!({
            "type": "session", "version": 3, "id": id,
            "createdAt": 1700000000000_i64, "cwd": "C:/work/demo"
        });
        let initial_policy = json!({
            "type": "approval/policy", "seq": 0, "time": 1700000000100_i64,
            "data": {"policy": "default"}
        });
        let seed = json!({
            "type": "session/end-seed", "seq": 1, "time": 1700000000200_i64,
            "data": {"inherited": false}
        });
        fs::write(&path, format!("{header}\n{initial_policy}\n{seed}\n")).unwrap();
        write_index(temp.path(), &[id], &[]);

        assert!(scan_sessions_at(temp.path()).is_empty());
        assert!(discover_session_logs(&temp.path().join("sessions"))
            .complete()
            .unwrap()
            .contains(&path));
    }

    #[test]
    fn first_turn_makes_a_session_visible_like_the_native_sidebar() {
        let temp = tempfile::tempdir().expect("tempdir");
        let id = "session-started";
        let path = temp
            .path()
            .join("sessions")
            .join("--C-work-demo--")
            .join(id)
            .join("session.v3.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let header = json!({
            "type": "session", "version": 3, "id": id,
            "createdAt": 1700000000000_i64, "cwd": "C:/work/demo"
        });
        let turn_start = json!({
            "type": "turn/start", "seq": 0, "time": 1700000000100_i64,
            "data": {}
        });
        fs::write(&path, format!("{header}\n{turn_start}\n")).unwrap();
        write_index(temp.path(), &[id], &[]);

        assert_eq!(scan_sessions_at(temp.path()).len(), 1);
    }

    #[test]
    fn workspace_membership_controls_grouping_instead_of_header_cwd() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions").join("--C-work-demo--");
        let assigned = root.join("session-assigned").join("session.v3.jsonl");
        let ungrouped = root.join("session-ungrouped").join("session.v3.jsonl");
        write_plain_session(&assigned, "session-assigned", None, None);
        write_plain_session(&ungrouped, "session-ungrouped", None, None);
        write_index(temp.path(), &["session-assigned"], &[]);

        let sessions = scan_sessions_at(temp.path());
        let assigned = sessions
            .iter()
            .find(|session| session.session_id == "session-assigned")
            .unwrap();
        assert_eq!(assigned.project_name.as_deref(), Some("demo"));
        assert_eq!(assigned.project_dir.as_deref(), Some("C:/work/demo"));
        let ungrouped = sessions
            .iter()
            .find(|session| session.session_id == "session-ungrouped")
            .unwrap();
        assert_eq!(ungrouped.project_name, None);
        assert_eq!(ungrouped.project_dir, None);
    }

    #[test]
    fn scan_surfaces_subagent_sessions_as_residuals() {
        let temp = tempfile::tempdir().expect("tempdir");
        let id = "session-child";
        let path = temp
            .path()
            .join("sessions")
            .join("project")
            .join(id)
            .join("session.v3.jsonl");
        write_plain_session(&path, id, Some("session-parent"), Some("subagent"));
        write_index(temp.path(), &[id], &[]);
        let sessions = scan_sessions_at(temp.path());
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].residual);
    }

    #[test]
    fn deletion_updates_index_then_removes_only_exact_session_and_cache() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        let id = "session-delete";
        let source = root.join("project").join(id).join("session.v3.jsonl");
        let sibling = root
            .join("project")
            .join("session-keep")
            .join("session.v3.jsonl");
        write_plain_session(&source, id, None, None);
        write_plain_session(&sibling, "session-keep", None, None);
        write_index(temp.path(), &[id, "session-keep"], &[id]);
        let cache_root = projection_cache_root(temp.path());
        fs::create_dir_all(&cache_root).expect("cache root");
        fs::write(cache_root.join(format!("{id}.json")), "{}").expect("cache");

        assert!(delete_session(&root, &source, id).expect("delete"));
        assert!(!source.parent().expect("session dir").exists());
        assert!(sibling.exists());
        assert!(!cache_root.join(format!("{id}.json")).exists());
        let index = read_index(temp.path()).expect("index").expect("present");
        assert_eq!(
            index.pointer("/tables/workspaces/workspace-1/sessionIds"),
            Some(&json!(["session-keep"]))
        );
        assert_eq!(
            index.pointer("/global/archivedSessionIds"),
            Some(&json!([]))
        );
    }

    #[test]
    fn parent_preflight_blocks_until_child_is_removed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        let parent = root
            .join("project")
            .join("session-parent")
            .join("session.v3.jsonl");
        let child = root
            .join("project")
            .join("session-child")
            .join("session.v3.jsonl");
        write_plain_session(&parent, "session-parent", None, None);
        write_plain_session(
            &child,
            "session-child",
            Some("session-parent"),
            Some("subagent"),
        );
        write_index(temp.path(), &["session-parent", "session-child"], &[]);

        assert!(preflight_delete(&root, &parent, "session-parent")
            .expect_err("parent must be protected")
            .contains("子 Agent"));
        delete_session(&root, &child, "session-child").expect("delete child");
        preflight_delete(&root, &parent, "session-parent").expect("parent now safe");
    }

    #[test]
    fn active_schedule_blocks_deletion() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        let source = root
            .join("project")
            .join("session-scheduled")
            .join("session.v3.jsonl");
        write_plain_session(&source, "session-scheduled", None, None);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&source)
            .expect("open");
        writeln!(file, "{}", json!({"type":"schedule/change","seq":5,"time":1700000000500_i64,"data":{"version":1,"operation":"create","schedule":{"id":"schedule-1","kind":"at","prompt":"hello","scheduledAt":"2026-09-13T00:00:00.000Z"}}})).expect("schedule");
        write_index(temp.path(), &["session-scheduled"], &[]);
        assert!(preflight_delete(&root, &source, "session-scheduled")
            .expect_err("schedule must block")
            .contains("活动定时任务"));
    }

    #[test]
    fn inherited_parent_schedule_does_not_block_child_cleanup() {
        let events = vec![
            json!({"type":"schedule/change","seq":0,"data":{"version":1,"operation":"create","schedule":{"id":"schedule-parent","kind":"at","prompt":"parent","scheduledAt":"2026-09-13T00:00:00.000Z"}}}),
            json!({"type":"session/end-seed","seq":1,"data":{"inherited":true}}),
        ];
        assert_eq!(active_schedules(&events).expect("fold schedules"), 0);
    }

    #[test]
    fn selected_children_are_ordered_before_parents() {
        let temp = tempfile::tempdir().expect("tempdir");
        let parent = temp
            .path()
            .join("project")
            .join("session-parent")
            .join("session.v3.jsonl");
        let child = temp
            .path()
            .join("project")
            .join("session-child")
            .join("session.v3.jsonl");
        write_plain_session(&parent, "session-parent", None, None);
        write_plain_session(
            &child,
            "session-child",
            Some("session-parent"),
            Some("subagent"),
        );
        let requests = vec![
            DeleteSessionRequest {
                provider_id: PROVIDER_ID.into(),
                session_id: "session-parent".into(),
                source_path: parent.to_string_lossy().to_string(),
                include_project: false,
            },
            DeleteSessionRequest {
                provider_id: PROVIDER_ID.into(),
                session_id: "session-child".into(),
                source_path: child.to_string_lossy().to_string(),
                include_project: false,
            },
        ];
        assert_eq!(deletion_order(&requests, &[0, 1]), vec![1, 0]);
    }

    #[test]
    fn generation_conflict_blocks_parent_preflight_without_mutation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        let parent = root.join("project/session-parent/session.v3.jsonl");
        let child = root.join("project/session-child/session.v3.jsonl");
        write_plain_session(&parent, "session-parent", None, None);
        write_plain_session(&child, "session-child", Some("session-parent"), None);
        let compressed = child.with_extension("jsonl.zstd");
        fs::write(
            &compressed,
            zstd::stream::encode_all(fs::read(&child).unwrap().as_slice(), 0).unwrap(),
        )
        .unwrap();
        write_index(temp.path(), &["session-parent", "session-child"], &[]);
        let index_before = fs::read(workspace_index_path(temp.path())).unwrap();
        let parent_before = fs::read(&parent).unwrap();

        let error = preflight_delete(&root, &parent, "session-parent")
            .expect_err("incomplete dependency discovery must block deletion");
        assert!(error.contains("最高代"), "{error}");
        assert_eq!(fs::read(&parent).unwrap(), parent_before);
        assert_eq!(
            fs::read(workspace_index_path(temp.path())).unwrap(),
            index_before
        );
        assert!(child.exists());
        assert!(compressed.exists());
    }

    #[test]
    fn stale_generation_cannot_authorize_deleting_a_newer_session() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        let source = root.join("project/session-safe/session.v3.jsonl");
        let newer = source.with_file_name("session.v4.jsonl");
        write_plain_session(&source, "session-safe", None, None);
        write_plain_session(&newer, "session-safe", None, None);
        write_index(temp.path(), &["session-safe"], &[]);
        let before = fs::read(workspace_index_path(temp.path())).unwrap();

        assert!(preflight_delete(&root, &source, "session-safe").is_err());
        assert!(delete_session(&root, &source, "session-safe").is_err());
        assert!(source.exists());
        assert!(newer.exists());
        assert_eq!(fs::read(workspace_index_path(temp.path())).unwrap(), before);
    }

    #[test]
    fn malformed_index_blocks_before_transcript_mutation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        let source = root
            .join("project")
            .join("session-safe")
            .join("session.v3.jsonl");
        write_plain_session(&source, "session-safe", None, None);
        let index = workspace_index_path(temp.path());
        fs::create_dir_all(index.parent().expect("index parent")).expect("index parent");
        fs::write(index, r#"{"tables":{"workspaces":[]}}"#).expect("bad index");

        assert!(delete_session(&root, &source, "session-safe").is_err());
        assert!(source.exists());
    }

    #[test]
    fn incomplete_discovery_keeps_healthy_rows_but_cannot_authorize_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let good = root.join("project/session-good/session.v3.jsonl");
        let bad = root.join("project/session-bad/session.v3.jsonl");
        write_plain_session(&good, "session-good", None, None);
        // Portable fixture for a storage error, even when tests run as admin.
        fs::create_dir_all(&bad).unwrap();
        let discovery = discover_session_logs(&root);
        assert_eq!(discovery.logs, vec![good.clone()]);
        assert!(!discovery.warnings.is_empty());
        assert!(discovery.complete().unwrap_err().contains("依赖扫描不完整"));
        assert_eq!(scan_sessions_at(temp.path()).len(), 1);
        assert!(preflight_delete(&root, &good, "session-good").is_err());
        assert!(good.exists());
    }

    #[test]
    fn missing_installation_is_empty_but_directory_errors_are_not_complete_scans() {
        let temp = tempfile::tempdir().unwrap();
        assert!(scan_sessions_at(temp.path()).is_empty());
        let root = temp.path().join("sessions");
        assert!(discover_session_logs(&root).complete().is_err());
        fs::write(&root, "not a directory").unwrap();
        let discovery = discover_session_logs(&root);
        assert!(discovery.logs.is_empty());
        assert!(discovery.complete().unwrap_err().contains("无法枚举"));
    }

    #[test]
    fn diagnostic_limit_never_turns_many_failures_into_success() {
        let mut discovery = SessionDiscovery::default();
        for index in 0..100 {
            discovery.warn(format!("failure {index}"));
        }
        assert_eq!(discovery.warnings.len(), 20);
        assert!(discovery.complete().is_err());
    }

    #[test]
    fn generation_conflict_on_selected_session_blocks_direct_delete() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let source = root.join("project/session-safe/session.v3.jsonl");
        write_plain_session(&source, "session-safe", None, None);
        fs::write(
            source.with_extension("jsonl.zstd"),
            zstd::stream::encode_all(fs::read(&source).unwrap().as_slice(), 0).unwrap(),
        )
        .unwrap();
        write_index(temp.path(), &["session-safe"], &[]);
        let before = fs::read(workspace_index_path(temp.path())).unwrap();
        assert!(delete_session(&root, &source, "session-safe").is_err());
        assert!(source.exists());
        assert_eq!(fs::read(workspace_index_path(temp.path())).unwrap(), before);
    }

    #[test]
    fn missing_old_generation_retry_does_not_delete_a_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let source = root.join("project/session-safe/session.v3.jsonl");
        let replacement = source.with_file_name("session.v4.jsonl");
        write_plain_session(&replacement, "session-safe", None, None);
        write_index(temp.path(), &["session-safe"], &[]);
        assert!(preflight_delete(&root, &source, "session-safe").is_err());
        assert!(delete_session(&root, &source, "session-safe").is_err());
        assert!(replacement.exists());
    }

    #[test]
    fn missing_transcript_retry_still_cleans_original_index_and_cache() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("project/session-gone/session.v3.jsonl");
        write_index(temp.path(), &["session-gone"], &["session-gone"]);
        let cache_root = projection_cache_root(temp.path());
        fs::create_dir_all(&cache_root).unwrap();
        let cache = cache_root.join("session-gone.json");
        fs::write(&cache, "{}").unwrap();
        preflight_delete(&root, &source, "session-gone").unwrap();
        assert!(delete_session(&root, &source, "session-gone").unwrap());
        assert!(!cache.exists());
        let index = read_index(temp.path()).unwrap().unwrap();
        assert_eq!(
            index.pointer("/tables/workspaces/workspace-1/sessionIds"),
            Some(&json!([]))
        );
        assert_eq!(
            index.pointer("/global/archivedSessionIds"),
            Some(&json!([]))
        );
        assert!(delete_session(&root, &source, "session-gone").unwrap());
    }

    #[test]
    fn parent_preflight_rechecks_children_instead_of_reusing_a_stale_graph() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let parent = root.join("project/session-parent/session.v3.jsonl");
        let child = root.join("project/session-child/session.v3.jsonl");
        write_plain_session(&parent, "session-parent", None, None);
        preflight_delete(&root, &parent, "session-parent").unwrap();
        write_plain_session(&child, "session-child", Some("session-parent"), None);
        assert!(preflight_delete(&root, &parent, "session-parent")
            .unwrap_err()
            .contains("依赖"));
    }

    #[test]
    fn malformed_parent_and_wrong_directory_cannot_hide_a_child() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let parent = root.join("project/session-parent/session.v3.jsonl");
        let child = root.join("project/session-child/session.v3.jsonl");
        write_plain_session(&parent, "session-parent", None, None);
        for invalid_parent in [json!(42), json!({}), json!(""), json!("../parent")] {
            write_plain_session(&child, "session-child", None, None);
            let text = fs::read_to_string(&child).unwrap();
            let (header, rest) = text.split_once('\n').unwrap();
            let mut header: Value = serde_json::from_str(header).unwrap();
            header["parentSession"] = invalid_parent;
            fs::write(&child, format!("{header}\n{rest}")).unwrap();
            let error = preflight_delete(&root, &parent, "session-parent").unwrap_err();
            assert!(error.contains("parentSession"), "{error}");
        }
        write_plain_session(&child, "other-id", Some("session-parent"), None);
        assert!(preflight_delete(&root, &parent, "session-parent").is_err());
        assert!(parent.exists());
    }

    #[test]
    fn malformed_target_body_still_blocks_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let source = root.join("project/session-safe/session.v3.jsonl");
        write_plain_session(&source, "session-safe", None, None);
        let mut file = fs::OpenOptions::new().append(true).open(&source).unwrap();
        writeln!(file, "not-json").unwrap();
        assert!(read_header(&source).is_ok());
        assert!(preflight_delete(&root, &source, "session-safe")
            .unwrap_err()
            .contains("事件损坏"));
        assert!(source.exists());
    }

    #[test]
    fn header_reader_is_bounded_and_does_not_replay_history() {
        struct CountingReader<R> {
            inner: R,
            bytes: std::rc::Rc<std::cell::Cell<usize>>,
        }
        impl<R: Read> Read for CountingReader<R> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let count = self.inner.read(buf)?;
                self.bytes.set(self.bytes.get() + count);
                Ok(count)
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.v3.jsonl");
        write_plain_session(&path, "session-safe", None, None);
        let mut text = fs::read(&path).unwrap();
        // Deliberately invalid/large body: this reader only interprets lineage.
        text.extend(vec![b'x'; 2 * 1024 * 1024]);
        for compressed in [false, true] {
            let input: Box<dyn Read> = if compressed {
                let encoded = zstd::stream::encode_all(text.as_slice(), 0).unwrap();
                Box::new(zstd::stream::read::Decoder::new(std::io::Cursor::new(encoded)).unwrap())
            } else {
                Box::new(std::io::Cursor::new(&text))
            };
            let count = std::rc::Rc::new(std::cell::Cell::new(0));
            let header = read_header_from(
                CountingReader {
                    inner: input,
                    bytes: count.clone(),
                },
                &path,
            )
            .unwrap();
            assert_eq!(header["id"], "session-safe");
            assert!(
                count.get() <= 8192,
                "header read consumed {} decompressed bytes",
                count.get()
            );
        }
    }

    #[test]
    fn oversized_headers_and_blank_prefixes_fail_within_the_read_budget() {
        let path = Path::new("session.v3.jsonl");
        for byte in [b'x', b'\n'] {
            let bytes = vec![byte; MAX_HEADER_BYTES as usize + 10];
            let error = read_header_from(std::io::Cursor::new(bytes), path).unwrap_err();
            assert!(error.contains("4 MB"), "{error}");
        }
    }

    #[test]
    fn duplicate_ids_and_cycles_fail_closed_but_deep_chains_are_valid() {
        let descriptor = |id: String, parent_session| SessionDescriptor {
            id,
            parent_session,
            origin: None,
            source_path: PathBuf::new(),
        };
        let duplicates = vec![
            descriptor("same".into(), None),
            descriptor("same".into(), None),
        ];
        assert!(validate_lineage(&duplicates).unwrap_err().contains("重复"));
        let cycle = vec![
            descriptor("a".into(), Some("b".into())),
            descriptor("b".into(), Some("a".into())),
        ];
        assert!(validate_lineage(&cycle).unwrap_err().contains("循环"));
        let chain = (0..10_000)
            .map(|index| {
                descriptor(
                    format!("s{index}"),
                    (index > 0).then(|| format!("s{}", index - 1)),
                )
            })
            .collect::<Vec<_>>();
        validate_lineage(&chain).unwrap();
    }

    #[test]
    fn stable_topological_order_handles_mixed_providers_duplicates_and_cycles() {
        let selected = vec![
            (0, "parent".into(), None),
            (1, "child".into(), Some("parent".into())),
        ];
        assert_eq!(
            order_selected_children(&selected, &[0, 3, 2, 1, 4]),
            vec![3, 2, 1, 0, 4]
        );
        let duplicates = vec![
            (0, "parent".into(), None),
            (1, "child".into(), Some("parent".into())),
            (2, "child".into(), Some("parent".into())),
        ];
        assert_eq!(
            order_selected_children(&duplicates, &[0, 1, 2]),
            vec![1, 2, 0]
        );
        let cycle = vec![
            (0, "a".into(), Some("b".into())),
            (1, "b".into(), Some("a".into())),
        ];
        assert_eq!(order_selected_children(&cycle, &[0, 2, 1]), vec![2, 0, 1]);
    }

    #[test]
    fn topological_order_handles_large_batches_without_recursion() {
        let count = 10_000;
        let selected = (0..count)
            .map(|index| {
                (
                    index,
                    format!("s{index}"),
                    (index > 0).then(|| format!("s{}", index - 1)),
                )
            })
            .collect::<Vec<_>>();
        let initial = (0..count).collect::<Vec<_>>();
        assert_eq!(
            order_selected_children(&selected, &initial),
            (0..count).rev().collect::<Vec<_>>()
        );
    }

    #[test]
    fn non_file_index_is_an_error_not_an_absent_index() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(workspace_index_path(temp.path())).unwrap();
        assert!(read_index(temp.path()).is_err());
    }

    #[test]
    #[ignore = "Opt-in read-only audit of the locally installed DeepSeek Harness storage"]
    fn audit_real_dsh_read_only() {
        let home = dsh_home();
        let sessions = scan_sessions_at(&home);
        for session in &sessions {
            let source = session.source_path.as_deref().expect("source path");
            let decoded = read_session(Path::new(source)).expect("read real session envelope");
            let mut event_types = std::collections::BTreeMap::<String, usize>::new();
            for event in &decoded.events {
                *event_types
                    .entry(
                        event
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("<missing>")
                            .to_string(),
                    )
                    .or_default() += 1;
            }
            let messages = load_messages(Path::new(source)).expect("read real session");
            eprintln!(
                "DeepSeek Harness audit: id={}, archived={}, project_name={:?}, project_dir={:?}, messages={}, event_types={event_types:?}",
                session.session_id,
                session.archived,
                session.project_name,
                session.project_dir,
                messages.len()
            );
        }
        assert!(
            home.join("sessions").exists(),
            "DeepSeek Harness is not installed"
        );
    }
}
