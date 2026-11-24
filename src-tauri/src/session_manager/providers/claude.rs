use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use serde_json::Value;

use crate::session_manager::{SessionMessage, SessionMeta};
use crate::session_paths::claude_config_dir;

use super::utils::{
    atomic_write_json, checked_storage_child, ensure_readable_size, extract_text,
    extract_thinking_text, is_reparse_or_symlink, is_safe_session_id, parse_timestamp_to_ms,
    path_basename, read_head_tail_lines, remove_path_if_exists, scan_warning, truncate_summary,
    FileParseCache, TITLE_MAX_CHARS,
};

const PROVIDER_ID: &str = "claude";
const HEAD_SCAN_LINES: usize = 60;
const TAIL_SCAN_LINES: usize = 30;
const MAX_CLAUDE_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_HEAD_SCAN_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SESSIONS_INDEX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_HISTORY_JSONL_BYTES: u64 = 64 * 1024 * 1024;

static PARSE_CACHE: LazyLock<FileParseCache> = LazyLock::new(FileParseCache::new);

pub fn scan_sessions() -> Vec<SessionMeta> {
    let root = claude_config_dir().join("projects");
    scan_sessions_in_root(&root, &PARSE_CACHE)
}

fn scan_sessions_in_root(root: &Path, cache: &FileParseCache) -> Vec<SessionMeta> {
    let mut project_dirs = Vec::new();
    let mut files = Vec::new();
    collect_project_dirs_and_jsonl_files(root, &mut project_dirs, &mut files);
    let mut sessions = cache.scan(files, parse_session_result);
    let mut ghosts = Vec::new();
    for project_dir in &project_dirs {
        scan_project_index_ghosts(project_dir, &sessions, &mut ghosts);
    }
    sessions.extend(ghosts);
    sessions
}

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    ensure_readable_size(path)?;
    let file = File::open(path).map_err(|e| format!("Failed to open session file: {e}"))?;
    let reader = BufReader::new(file);
    let mut entries = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|error| format!("Failed to read Claude transcript: {error}"))?;
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            entries.push(value);
        }
    }
    let mut messages = Vec::new();

    for value in conversation_chain(&entries) {
        if !is_main_entry(value)
            || (value.get("uuid").and_then(Value::as_str).is_some()
                && !matches!(entry_kind(value), "user" | "assistant"))
        {
            continue;
        }
        let message = match value.get("message") {
            Some(message) => message,
            None => continue,
        };

        let mut role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let ts = value.get("timestamp").and_then(parse_timestamp_to_ms);

        if role == "assistant" {
            if let Some(content_val) = message.get("content") {
                let thinking = extract_thinking_text(content_val);
                if !thinking.trim().is_empty() {
                    messages.push(SessionMessage {
                        role: "thinking".to_string(),
                        content: thinking,
                        ts,
                    });
                }
            }
        }

        if role == "user" {
            if let Some(Value::Array(items)) = message.get("content") {
                if !items.is_empty()
                    && items.iter().all(|item| {
                        matches!(
                            item.get("type").and_then(Value::as_str),
                            Some("tool_result" | "mcp_tool_result")
                        )
                    })
                {
                    role = "tool".to_string();
                }
            }
        }
        let mut content = message.get("content").map(extract_text).unwrap_or_default();
        if !content.trim().is_empty() {
            if role == "user"
                && value.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
                && !content.starts_with("[Compacted Summary]")
            {
                content = format!("[Compacted Summary]\n{content}");
            }
            messages.push(SessionMessage { role, content, ts });
        }
    }
    Ok(messages)
}

fn is_main_entry(entry: &Value) -> bool {
    entry.get("isMeta").and_then(Value::as_bool) != Some(true)
        && entry.get("isSidechain").and_then(Value::as_bool) != Some(true)
        && entry
            .get("teamName")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
}

fn entry_kind(entry: &Value) -> &str {
    entry
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| entry.pointer("/message/role").and_then(Value::as_str))
        .unwrap_or("")
}

fn response_id(entry: &Value) -> Option<&str> {
    (entry_kind(entry) == "assistant")
        .then(|| entry.pointer("/message/id").and_then(Value::as_str))
        .flatten()
        .filter(|id| !id.is_empty())
}

fn block_ids<'a>(entry: &'a Value, kind: &str, field: &str) -> Vec<&'a str> {
    entry
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some(kind))
        .filter_map(|block| block.get(field).and_then(Value::as_str))
        .collect()
}

/// SDK 0.2.178 (3be907ed), sessions.py:931-1245. UUID replacement is last-write-wins.
/// Compaction follows parentUuid only: logicalParentUuid would duplicate pre-summary content.
/// Legacy rows without UUID remain in file order, including mixed-format transcripts; a
/// compact boundary drops only the older unlinked rows, whose ancestry cannot be proved.
fn conversation_chain(entries: &[Value]) -> Vec<&Value> {
    let mut by_uuid = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if !matches!(
            entry_kind(entry),
            "user" | "assistant" | "progress" | "system" | "attachment"
        ) {
            continue;
        }
        if let Some(uuid) = entry
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            by_uuid.insert(uuid, index);
        }
    }
    if by_uuid.is_empty() {
        return entries.iter().collect();
    }
    let mut indices: Vec<usize> = by_uuid.values().copied().collect();
    indices.sort_unstable();
    let parents: HashSet<&str> = indices
        .iter()
        .filter_map(|&i| entries[i].get("parentUuid").and_then(Value::as_str))
        .collect();
    let mut leaves = Vec::new();
    for &i in &indices {
        if parents.contains(entries[i]["uuid"].as_str().unwrap_or("")) {
            continue;
        }
        let mut current = Some(i);
        let mut seen = HashSet::new();
        while let Some(index) = current {
            if !seen.insert(index) {
                break;
            }
            let entry = &entries[index];
            if matches!(entry_kind(entry), "user" | "assistant") {
                leaves.push(index);
                break;
            }
            current = entry
                .get("parentUuid")
                .and_then(Value::as_str)
                .and_then(|id| by_uuid.get(id))
                .copied();
        }
    }
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut current = leaves
        .iter()
        .copied()
        .filter(|&i| is_main_entry(&entries[i]))
        .max()
        .or_else(|| leaves.into_iter().max());
    while let Some(index) = current {
        if !seen.insert(index) {
            break;
        }
        chain.push(index);
        current = entries[index]
            .get("parentUuid")
            .and_then(Value::as_str)
            .and_then(|id| by_uuid.get(id))
            .copied();
    }
    chain.reverse();

    let mut anchors = HashMap::new();
    let mut owners: HashMap<&str, Option<usize>> = HashMap::new();
    for &i in &chain {
        if let Some(id) = response_id(&entries[i]) {
            anchors.insert(id, i);
        }
    }
    for &i in &indices {
        if let Some(response) = response_id(&entries[i]) {
            for call in block_ids(&entries[i], "tool_use", "id") {
                let owner = match owners.get(call) {
                    None => Some(i),
                    Some(Some(old)) if response_id(&entries[*old]) == Some(response) => Some(i),
                    _ => None,
                };
                owners.insert(call, owner);
            }
        }
    }
    let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut results_by_assistant: HashMap<usize, Vec<usize>> = HashMap::new();
    for &i in &indices {
        if let Some(response) = response_id(&entries[i]) {
            groups.entry(response).or_default().push(i);
        } else if entry_kind(&entries[i]) == "user" && is_main_entry(&entries[i]) {
            let ids = block_ids(&entries[i], "tool_result", "tool_use_id");
            if ids.is_empty() {
                continue;
            }
            let mut targets = HashSet::new();
            for field in ["parentUuid", "sourceToolAssistantUUID"] {
                if let Some(&target) = entries[i]
                    .get(field)
                    .and_then(Value::as_str)
                    .and_then(|id| by_uuid.get(id))
                {
                    targets.insert(target);
                }
            }
            for id in ids {
                if let Some(target) = owners.get(id).copied().flatten() {
                    targets.insert(target);
                }
            }
            for target in targets {
                results_by_assistant.entry(target).or_default().push(i);
            }
        }
    }
    let answered_on_chain: HashSet<&str> = chain
        .iter()
        .flat_map(|&i| block_ids(&entries[i], "tool_result", "tool_use_id"))
        .collect();
    let mut inserts: HashMap<usize, Vec<usize>> = HashMap::new();
    for &anchor in &chain {
        let Some(response) = response_id(&entries[anchor]) else {
            continue;
        };
        if anchors.get(response) != Some(&anchor) {
            continue;
        }
        let same_writer = |i: usize, j: usize| {
            entries[i].get("agentId") == entries[j].get("agentId")
                && is_main_entry(&entries[i])
                && is_main_entry(&entries[j])
        };
        let group = &groups[response];
        let members: HashSet<usize> = group
            .iter()
            .copied()
            .filter(|&i| same_writer(i, anchor))
            .collect();
        let mut siblings: Vec<usize> = group
            .iter()
            .copied()
            .filter(|i| members.contains(i) && !seen.contains(i))
            .collect();
        let mut candidates: Vec<usize> = members
            .iter()
            .filter_map(|i| results_by_assistant.get(i))
            .flatten()
            .copied()
            .collect();
        candidates.sort_unstable();
        candidates.dedup();
        let mut results = Vec::new();
        let mut strays = Vec::new();
        for i in candidates {
            if seen.contains(&i) || !is_main_entry(&entries[i]) || entry_kind(&entries[i]) != "user"
            {
                continue;
            }
            let ids = block_ids(&entries[i], "tool_result", "tool_use_id");
            if ids.is_empty() {
                continue;
            }
            let Some(parent) = entries[i].get("parentUuid").and_then(Value::as_str) else {
                continue;
            };
            if by_uuid
                .get(parent)
                .is_some_and(|p| members.contains(p) && same_writer(i, *p))
            {
                results.push(i);
            } else {
                let source_matches = entries[i]
                    .get("sourceToolAssistantUUID")
                    .and_then(Value::as_str)
                    .and_then(|id| by_uuid.get(id))
                    .is_some_and(|p| members.contains(p) && same_writer(i, *p));
                let owned = ids.iter().any(|id| {
                    owners
                        .get(id)
                        .copied()
                        .flatten()
                        .is_some_and(|p| members.contains(&p) && same_writer(i, p))
                });
                if source_matches || owned {
                    strays.push(i);
                }
            }
        }
        let mut answered: HashSet<&str> = results
            .iter()
            .flat_map(|&i| block_ids(&entries[i], "tool_result", "tool_use_id"))
            .collect();
        for i in strays {
            let ids = block_ids(&entries[i], "tool_result", "tool_use_id");
            if ids.iter().any(|id| {
                !answered.contains(id)
                    && !answered_on_chain.contains(id)
                    && owners
                        .get(id)
                        .copied()
                        .flatten()
                        .is_some_and(|p| members.contains(&p) && same_writer(i, p))
            }) {
                answered.extend(ids);
                results.push(i);
            }
        }
        let timestamp = |i: &usize| {
            entries[*i]
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or("")
        };
        siblings.sort_by_key(timestamp);
        results.sort_by_key(timestamp);
        siblings.extend(results);
        siblings.retain(|i| seen.insert(*i));
        inserts.insert(anchor, siblings);
    }
    let compact_start = chain
        .iter()
        .copied()
        .filter(|&i| {
            entries[i].get("subtype").and_then(Value::as_str) == Some("compact_boundary")
                || entries[i].get("isCompactSummary").and_then(Value::as_bool) == Some(true)
        })
        .min()
        .unwrap_or(0);
    let mut legacy = entries
        .iter()
        .enumerate()
        .filter(|(i, entry)| {
            *i >= compact_start
                && entry
                    .get("uuid")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
        })
        .peekable();
    let mut output = Vec::new();
    for index in chain {
        while legacy.peek().is_some_and(|(i, _)| *i < index) {
            output.push(legacy.next().unwrap().1);
        }
        output.push(&entries[index]);
        if let Some(recovered) = inserts.get(&index) {
            output.extend(recovered.iter().map(|&i| &entries[i]));
        }
    }
    output.extend(legacy.map(|(_, entry)| entry));
    output
}

fn is_valid_claude_session_id(id: &str) -> bool {
    is_safe_session_id(id)
        && !matches!(id, "memory" | "sessions-index")
        && !id.starts_with("agent-")
}

fn validate_claude_session_path(
    root: &Path,
    path: &Path,
    session_id: &str,
) -> Result<(PathBuf, PathBuf, bool), String> {
    if !is_valid_claude_session_id(session_id) {
        return Err(format!("Invalid Claude session ID: {session_id}"));
    }
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(format!("Invalid Claude session path: {}", path.display()));
    }
    if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
        return Err(format!(
            "Invalid Claude session file extension: {}",
            path.display()
        ));
    }
    if is_agent_session(path) {
        return Err(format!(
            "Refusing to target Claude subagent file directly: {}",
            path.display()
        ));
    }

    let file_exists = match fs::symlink_metadata(path) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                return Err(format!(
                    "Claude session source must not be a symlink or reparse point: {}",
                    path.display()
                ));
            }
            if !meta.file_type().is_file() {
                return Err(format!(
                    "Claude session source is not a regular file: {}",
                    path.display()
                ));
            }
            true
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => false,
        Err(err) => {
            return Err(format!(
                "Failed to inspect Claude session {}: {err}",
                path.display()
            ))
        }
    };

    let canonical_root = root.canonicalize().map_err(|err| {
        format!(
            "Failed to resolve Claude projects root {}: {err}",
            root.display()
        )
    })?;
    let root_meta = fs::metadata(&canonical_root).map_err(|err| {
        format!(
            "Failed to inspect Claude projects root {}: {err}",
            canonical_root.display()
        )
    })?;
    if !root_meta.is_dir() {
        return Err(format!(
            "Claude projects root is not a directory: {}",
            canonical_root.display()
        ));
    }

    let parent_dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("Invalid Claude session parent: {}", path.display()))?;
    match fs::symlink_metadata(parent_dir) {
        Ok(parent_meta) => {
            if is_reparse_or_symlink(&parent_meta) {
                return Err(format!(
                    "Claude project directory must not be a symlink or reparse point: {}",
                    parent_dir.display()
                ));
            }
            if !parent_meta.file_type().is_dir() {
                return Err(format!(
                    "Claude project path is not a directory: {}",
                    parent_dir.display()
                ));
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "Failed to inspect Claude project directory {}: {err}",
                parent_dir.display()
            ))
        }
    }

    let resolved_path = checked_storage_child(root, path)?;
    let relative = resolved_path.strip_prefix(&canonical_root).map_err(|_| {
        format!(
            "Claude session path is outside projects root: {}",
            path.display()
        )
    })?;
    let components: Vec<_> = relative.components().collect();
    if components.len() != 2
        || !matches!(components[0], std::path::Component::Normal(_))
        || !matches!(components[1], std::path::Component::Normal(_))
    {
        return Err(format!(
            "Claude session path must be strictly <projects>/<project>/<session>.jsonl: {}",
            path.display()
        ));
    }

    let stem = resolved_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("Invalid Claude session filename: {}", path.display()))?;
    if matches!(stem, "memory" | "sessions-index") {
        return Err(format!(
            "Refusing to delete reserved Claude file: {}",
            path.display()
        ));
    }

    if file_exists {
        let meta = parse_session_result(&resolved_path)?.ok_or_else(|| {
            format!(
                "Failed to parse Claude session metadata: {}",
                path.display()
            )
        })?;
        if meta.session_id != session_id {
            return Err(format!(
                "Claude session ID mismatch: expected {session_id}, found {}",
                meta.session_id
            ));
        }
    } else if stem != session_id {
        return Err(format!(
            "Missing Claude session path does not match session ID {session_id}: {}",
            path.display()
        ));
    }

    Ok((canonical_root, resolved_path, file_exists))
}

pub fn preflight_delete(root: &Path, path: &Path, session_id: &str) -> Result<(), String> {
    validate_claude_session_path(root, path, session_id).map(|_| ())
}

fn resolve_verified_claude_dir(root: &Path, configured_claude_dir: &Path) -> Option<PathBuf> {
    let root_name = root.file_name()?.to_str()?;
    #[cfg(windows)]
    let is_projects = root_name.eq_ignore_ascii_case("projects");
    #[cfg(not(windows))]
    let is_projects = root_name == "projects";
    if !is_projects {
        return None;
    }
    let candidate = root.parent()?;
    let meta = fs::symlink_metadata(candidate).ok()?;
    if !meta.is_dir() && !is_reparse_or_symlink(&meta) {
        return None;
    }
    let canonical_candidate = candidate.canonicalize().ok()?;
    let canonical_configured = configured_claude_dir.canonicalize().ok()?;
    (canonical_candidate == canonical_configured).then_some(canonical_candidate)
}

pub fn delete_session(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    let configured = claude_config_dir();
    let verified_claude_dir = resolve_verified_claude_dir(root, &configured);
    delete_session_with_claude_dir(root, path, session_id, verified_claude_dir.as_deref())
}

fn delete_session_with_claude_dir(
    root: &Path,
    path: &Path,
    session_id: &str,
    claude_dir: Option<&Path>,
) -> Result<bool, String> {
    let (canonical_root, resolved_path, file_exists) =
        validate_claude_session_path(root, path, session_id)?;
    let project_dir = resolved_path
        .parent()
        .ok_or_else(|| format!("Invalid Claude project directory: {}", path.display()))?;
    let stem = resolved_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    let mut removed_anything = false;

    if project_dir.exists() {
        if cleanup_project_sessions_index(&canonical_root, project_dir, session_id)? {
            removed_anything = true;
        }

        if stem == session_id && is_valid_claude_session_id(session_id) {
            let sibling = project_dir.join(session_id);
            let sibling = checked_storage_child(&canonical_root, &sibling)?;
            if fs::symlink_metadata(&sibling).is_ok() {
                remove_path_if_exists(&sibling).map_err(|e| {
                    format!(
                        "Failed to delete Claude session sidecar {}: {e}",
                        sibling.display()
                    )
                })?;
                removed_anything = true;
            }
        }

        if cleanup_legacy_project_agents(&canonical_root, project_dir, session_id)? {
            removed_anything = true;
        }
    }

    if let Some(claude_dir) = claude_dir {
        if cleanup_claude_dir_sidecars(claude_dir, session_id)? {
            removed_anything = true;
        }
    }

    if file_exists {
        match fs::symlink_metadata(&resolved_path) {
            Ok(meta) if is_reparse_or_symlink(&meta) => {
                return Err(format!(
                    "Claude session source turned into a symlink before deletion: {}",
                    resolved_path.display()
                ));
            }
            Ok(_) => {
                fs::remove_file(&resolved_path).map_err(|e| {
                    format!(
                        "Failed to delete Claude session file {}: {e}",
                        resolved_path.display()
                    )
                })?;
                removed_anything = true;
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(format!(
                    "Failed to inspect Claude session file {}: {err}",
                    resolved_path.display()
                ))
            }
        }
    }

    Ok(removed_anything)
}

fn cleanup_project_sessions_index(
    canonical_root: &Path,
    project_dir: &Path,
    session_id: &str,
) -> Result<bool, String> {
    let index_path = project_dir.join("sessions-index.json");
    let index_path = checked_storage_child(canonical_root, &index_path)?;
    let meta = match fs::symlink_metadata(&index_path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(format!(
                "Failed to inspect Claude sessions-index.json {}: {err}",
                index_path.display()
            ))
        }
    };
    if is_reparse_or_symlink(&meta) {
        return Err(format!(
            "Refusing to modify symlinked Claude sessions-index.json: {}",
            index_path.display()
        ));
    }
    if !meta.file_type().is_file() {
        return Err(format!(
            "Claude sessions-index.json is not a regular file: {}",
            index_path.display()
        ));
    }
    if meta.len() > MAX_SESSIONS_INDEX_BYTES {
        return Err(format!(
            "Claude sessions-index.json exceeds {} MiB limit: {}",
            MAX_SESSIONS_INDEX_BYTES / 1024 / 1024,
            index_path.display()
        ));
    }

    let mut raw = String::new();
    File::open(&index_path)
        .map_err(|e| {
            format!(
                "Failed to open Claude sessions-index.json {}: {e}",
                index_path.display()
            )
        })?
        .take(MAX_SESSIONS_INDEX_BYTES + 1)
        .read_to_string(&mut raw)
        .map_err(|e| {
            format!(
                "Failed to read Claude sessions-index.json {}: {e}",
                index_path.display()
            )
        })?;
    if raw.len() as u64 > MAX_SESSIONS_INDEX_BYTES {
        return Err(format!(
            "Claude sessions-index.json exceeds {} MiB limit: {}",
            MAX_SESSIONS_INDEX_BYTES / 1024 / 1024,
            index_path.display()
        ));
    }

    let mut doc: Value = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "Failed to parse Claude sessions-index.json {}: {e}",
            index_path.display()
        )
    })?;
    let Some(entries) = doc.get_mut("entries").and_then(Value::as_array_mut) else {
        return Ok(false);
    };
    let before_len = entries.len();
    entries.retain(|entry| entry.get("sessionId").and_then(Value::as_str) != Some(session_id));
    if entries.len() == before_len {
        return Ok(false);
    }
    atomic_write_json(&index_path, &doc)?;
    Ok(true)
}

fn cleanup_legacy_project_agents(
    canonical_root: &Path,
    project_dir: &Path,
    session_id: &str,
) -> Result<bool, String> {
    let entries = match fs::read_dir(project_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(format!(
                "Failed to scan Claude project directory {}: {err}",
                project_dir.display()
            ))
        }
    };
    let mut removed_any = false;
    let mut line_buf = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            format!(
                "Failed to enumerate Claude project directory {}: {e}",
                project_dir.display()
            )
        })?;
        let candidate = entry.path();
        if candidate.extension().and_then(|e| e.to_str()) != Some("jsonl")
            || !is_agent_session(&candidate)
        {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&candidate) else {
            continue;
        };
        if is_reparse_or_symlink(&meta) || !meta.file_type().is_file() {
            continue;
        }
        if agent_file_belongs_to_session(&candidate, session_id, &mut line_buf) {
            let checked = checked_storage_child(canonical_root, &candidate)?;
            remove_path_if_exists(&checked).map_err(|e| {
                format!(
                    "Failed to delete legacy Claude subagent transcript {}: {e}",
                    checked.display()
                )
            })?;
            removed_any = true;
        }
    }
    Ok(removed_any)
}

fn agent_file_belongs_to_session(path: &Path, session_id: &str, buf: &mut Vec<u8>) -> bool {
    const AGENT_HEADER_BUDGET: u64 = 4 * 1024 * 1024;
    const AGENT_HEADER_LINES: usize = 25;
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut reader = BufReader::new(file.take(AGENT_HEADER_BUDGET));
    for _ in 0..AGENT_HEADER_LINES {
        match read_bounded_line(&mut reader, buf, MAX_CLAUDE_LINE_BYTES) {
            Ok(BoundedLine::Line(line)) => {
                let Ok(val) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(found) = val.get("sessionId").and_then(Value::as_str) {
                    return found == session_id;
                }
            }
            Ok(BoundedLine::OversizedSkipped) => continue,
            Ok(BoundedLine::Eof) | Err(_) => break,
        }
    }
    false
}

fn cleanup_claude_dir_sidecars(claude_dir: &Path, session_id: &str) -> Result<bool, String> {
    if !claude_dir.exists() {
        return Ok(false);
    }
    let mut removed_any = false;

    let direct_targets = [
        claude_dir.join("file-history").join(session_id),
        claude_dir.join("tasks").join(session_id),
        claude_dir.join("session-env").join(session_id),
        claude_dir.join("debug").join(format!("{session_id}.txt")),
    ];
    for target in &direct_targets {
        let resolved = checked_storage_child(claude_dir, target)?;
        if fs::symlink_metadata(&resolved).is_ok() {
            remove_path_if_exists(&resolved).map_err(|e| {
                format!(
                    "Failed to delete Claude sidecar {}: {e}",
                    resolved.display()
                )
            })?;
            removed_any = true;
        }
    }

    let todos_dir = claude_dir.join("todos");
    match fs::symlink_metadata(&todos_dir) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                return Err(format!(
                    "Claude todos directory must not be a symlink or reparse point: {}",
                    todos_dir.display()
                ));
            }
            if meta.file_type().is_dir() {
                let resolved_todos = checked_storage_child(claude_dir, &todos_dir)?;
                let prefix = format!("{session_id}-");
                let exact = format!("{session_id}.json");
                let entries = fs::read_dir(&resolved_todos).map_err(|e| {
                    format!(
                        "Failed to read Claude todos directory {}: {e}",
                        resolved_todos.display()
                    )
                })?;
                for entry in entries {
                    let entry = entry.map_err(|e| {
                        format!(
                            "Failed to enumerate Claude todos directory {}: {e}",
                            resolved_todos.display()
                        )
                    })?;
                    let file_name = entry.file_name();
                    let Some(name) = file_name.to_str() else {
                        continue;
                    };
                    if !name.starts_with(&prefix) && name != exact {
                        continue;
                    }
                    let entry_path = entry.path();
                    let Ok(entry_meta) = fs::symlink_metadata(&entry_path) else {
                        continue;
                    };
                    // Only remove files (or non-directory links); never remove subdirectories in todos/
                    if entry_meta.is_dir() && !is_reparse_or_symlink(&entry_meta) {
                        continue;
                    }
                    let checked = checked_storage_child(claude_dir, &entry_path)?;
                    remove_path_if_exists(&checked).map_err(|e| {
                        format!(
                            "Failed to delete Claude todo file {}: {e}",
                            checked.display()
                        )
                    })?;
                    removed_any = true;
                }
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "Failed to inspect Claude todos directory {}: {err}",
                todos_dir.display()
            ))
        }
    }

    if cleanup_claude_history_jsonl(claude_dir, session_id)? {
        removed_any = true;
    }

    Ok(removed_any)
}

fn cleanup_claude_history_jsonl(claude_dir: &Path, session_id: &str) -> Result<bool, String> {
    let history_candidate = claude_dir.join("history.jsonl");
    let history_path = checked_storage_child(claude_dir, &history_candidate)?;
    let meta = match fs::symlink_metadata(&history_path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(format!(
                "Failed to inspect Claude history.jsonl {}: {err}",
                history_path.display()
            ))
        }
    };
    if is_reparse_or_symlink(&meta) || !meta.file_type().is_file() {
        return Ok(false);
    }
    if meta.len() > MAX_HISTORY_JSONL_BYTES {
        return Ok(false);
    }

    let mut raw = Vec::new();
    File::open(&history_path)
        .map_err(|e| {
            format!(
                "Failed to open Claude history.jsonl {}: {e}",
                history_path.display()
            )
        })?
        .take(MAX_HISTORY_JSONL_BYTES + 1)
        .read_to_end(&mut raw)
        .map_err(|e| {
            format!(
                "Failed to read Claude history.jsonl {}: {e}",
                history_path.display()
            )
        })?;
    if raw.len() as u64 > MAX_HISTORY_JSONL_BYTES {
        return Ok(false);
    }

    let mut kept = Vec::with_capacity(raw.len());
    let mut removed = false;
    for chunk in raw.split_inclusive(|&b| b == b'\n') {
        let trimmed_bytes = chunk
            .strip_suffix(b"\r\n")
            .or_else(|| chunk.strip_suffix(b"\n"))
            .unwrap_or(chunk);
        if let Ok(line_str) = std::str::from_utf8(trimmed_bytes) {
            if line_str.contains(session_id) {
                if let Ok(val) = serde_json::from_str::<Value>(line_str) {
                    if val.get("sessionId").and_then(Value::as_str) == Some(session_id) {
                        removed = true;
                        continue;
                    }
                }
            }
        }
        kept.extend_from_slice(chunk);
    }

    if !removed {
        return Ok(false);
    }

    atomic_write_bytes(&history_path, &kept)?;
    Ok(true)
}

fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("Invalid target file path")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    temp.write_all(bytes).map_err(|e| e.to_string())?;
    temp.flush().map_err(|e| e.to_string())?;
    temp.as_file().sync_all().map_err(|e| e.to_string())?;
    #[cfg(windows)]
    {
        let mut current_temp = temp;
        let mut attempts = 0usize;
        loop {
            match current_temp.persist(path) {
                Ok(_) => break,
                Err(err) if err.error.kind() == io::ErrorKind::PermissionDenied && attempts < 3 => {
                    attempts += 1;
                    current_temp = err.file;
                    std::thread::sleep(std::time::Duration::from_millis(15));
                }
                Err(err) => return Err(err.error.to_string()),
            }
        }
    }
    #[cfg(not(windows))]
    {
        temp.persist(path).map_err(|e| e.error.to_string())?;
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

#[cfg(test)]
fn parse_session(path: &Path) -> Option<SessionMeta> {
    parse_session_result(path).ok().flatten()
}

enum BoundedLine {
    Eof,
    Line(String),
    OversizedSkipped,
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_bytes: usize,
) -> io::Result<BoundedLine> {
    buf.clear();
    let mut oversized = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if oversized {
                return Ok(BoundedLine::OversizedSkipped);
            }
            if buf.is_empty() {
                return Ok(BoundedLine::Eof);
            }
            break;
        }
        if let Some(newline_pos) = available.iter().position(|&b| b == b'\n') {
            if !oversized {
                if buf.len() + newline_pos <= max_bytes {
                    buf.extend_from_slice(&available[..newline_pos]);
                } else {
                    buf.clear();
                    oversized = true;
                }
            }
            reader.consume(newline_pos + 1);
            if oversized {
                return Ok(BoundedLine::OversizedSkipped);
            }
            break;
        } else {
            let len = available.len();
            if !oversized {
                if buf.len() + len <= max_bytes {
                    buf.extend_from_slice(available);
                } else {
                    buf.clear();
                    oversized = true;
                }
            }
            reader.consume(len);
        }
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    match std::str::from_utf8(buf) {
        Ok(s) => Ok(BoundedLine::Line(s.to_owned())),
        Err(_) => Ok(BoundedLine::OversizedSkipped),
    }
}

fn read_bounded_head_lines(path: &Path, head_n: usize) -> io::Result<Vec<String>> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file.take(MAX_HEAD_SCAN_BYTES));
    let mut buf = Vec::new();
    let mut head = Vec::new();
    while head.len() < head_n {
        match read_bounded_line(&mut reader, &mut buf, MAX_CLAUDE_LINE_BYTES)? {
            BoundedLine::Eof => break,
            BoundedLine::OversizedSkipped => continue,
            BoundedLine::Line(line) => head.push(line),
        }
    }
    Ok(head)
}

/// 扫描缓存用：读失败（权限、被占用）返回 `Err`，本轮不缓存、下轮重试；`agent-*`
/// 前缀或解析不出会话 ID 属于只依赖文件本身的确定性结果，返回 `Ok(None)` 进缓存。
fn parse_session_result(path: &Path) -> Result<Option<SessionMeta>, String> {
    if is_agent_session(path) {
        return Ok(None);
    }

    let file_len = fs::metadata(path)
        .map_err(|error| {
            let message = format!("无法读取 {}：{error}", path.display());
            scan_warning(message.clone());
            message
        })?
        .len();
    let oversized = file_len > super::utils::MAX_MESSAGES_FILE_BYTES;

    let head = read_bounded_head_lines(path, HEAD_SCAN_LINES).map_err(|error| {
        let message = format!("无法读取 {}：{error}", path.display());
        scan_warning(message.clone());
        message
    })?;

    let (mut tail, need_stream_tail) = match read_head_tail_lines(path, 0, TAIL_SCAN_LINES) {
        Ok((_, tail)) => (tail, false),
        Err(error) if error.kind() == io::ErrorKind::InvalidData && !oversized => {
            (Vec::new(), true)
        }
        Err(error) if error.kind() == io::ErrorKind::InvalidData && oversized => {
            (Vec::new(), false)
        }
        Err(error) => {
            let message = format!("无法读取 {}：{error}", path.display());
            scan_warning(message.clone());
            return Err(message);
        }
    };

    let mut custom_title: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut summary_title: Option<String> = None;
    let mut saw_json_object = false;

    let mut apply_title_line = |line: &str, check_any_object: bool| {
        let has_title_keyword = line.contains("customTitle")
            || line.contains("aiTitle")
            || line.contains("\"summary\"");
        if !has_title_keyword {
            if check_any_object
                && !saw_json_object
                && line.trim_start().starts_with('{')
                && serde_json::from_str::<Value>(line).is_ok_and(|v| v.is_object())
            {
                saw_json_object = true;
            }
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if value.is_object() {
            saw_json_object = true;
        }
        if !is_main_entry(&value) {
            return;
        }
        if let Some(raw) = value.get("customTitle").and_then(Value::as_str) {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                custom_title = None;
            } else {
                custom_title = Some(trimmed.to_string());
            }
        }
        if let Some(text) = value
            .get("aiTitle")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            ai_title = Some(text.to_string());
        }
        if value.get("type").and_then(Value::as_str) == Some("summary") {
            if let Some(text) = value
                .get("summary")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                summary_title = Some(text.to_string());
            }
        }
    };

    if oversized {
        scan_warning(format!(
            "Claude 会话 {} 正文超限，标题仅依据头尾元信息",
            path.display()
        ));
        for line in head.iter().chain(tail.iter()) {
            apply_title_line(line, false);
        }
    } else {
        let file = File::open(path).map_err(|error| {
            let msg = format!("Failed to read Claude titles {}: {error}", path.display());
            scan_warning(msg.clone());
            msg
        })?;
        let mut reader = BufReader::new(file);
        let mut line_buf = Vec::new();
        let mut fallback_tail = need_stream_tail.then(|| VecDeque::with_capacity(TAIL_SCAN_LINES));
        loop {
            match read_bounded_line(&mut reader, &mut line_buf, MAX_CLAUDE_LINE_BYTES) {
                Ok(BoundedLine::Eof) => break,
                Ok(BoundedLine::OversizedSkipped) => continue,
                Ok(BoundedLine::Line(line)) => {
                    apply_title_line(&line, true);
                    if let Some(ref mut ring) = fallback_tail {
                        if ring.len() == TAIL_SCAN_LINES {
                            ring.pop_front();
                        }
                        ring.push_back(line);
                    }
                }
                Err(error) => {
                    let msg = format!("Failed to read Claude titles {}: {error}", path.display());
                    scan_warning(msg.clone());
                    return Err(msg);
                }
            }
        }
        if let Some(ring) = fallback_tail {
            tail = ring.into_iter().collect();
        }
    }

    let mut json_session_id: Option<String> = None;
    let mut project_dir: Option<String> = None;
    let mut created_at: Option<i64> = None;
    let mut first_user_message: Option<String> = None;

    // Extract metadata and first user message from head lines
    for line in &head {
        let value: Value = match serde_json::from_str(line) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        if !value.is_object() {
            continue;
        }
        saw_json_object = true;
        if json_session_id.is_none() {
            json_session_id = value
                .get("sessionId")
                .and_then(Value::as_str)
                .map(|s| s.to_string());
        }
        if project_dir.is_none() {
            project_dir = value
                .get("cwd")
                .and_then(Value::as_str)
                .map(|s| s.to_string());
        }
        if created_at.is_none() {
            created_at = value.get("timestamp").and_then(parse_timestamp_to_ms);
        }
        // Extract first real user message as title candidate
        // Skip system-injected caveats and slash commands (e.g. /clear, /compact)
        if first_user_message.is_none() && is_main_entry(&value) {
            let is_user = value.get("type").and_then(Value::as_str) == Some("user")
                || value
                    .get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(Value::as_str)
                    == Some("user");
            if is_user {
                if let Some(message) = value.get("message") {
                    let text = message.get("content").map(extract_text).unwrap_or_default();
                    let trimmed = text.trim();
                    if !trimmed.is_empty()
                        && !trimmed.contains("<local-command-caveat>")
                        && !trimmed.starts_with("<command-name>")
                    {
                        first_user_message = Some(trimmed.to_string());
                    }
                }
            }
        }
        if json_session_id.is_some()
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

    for line in tail.iter().rev() {
        let value: Value = match serde_json::from_str(line) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        if !value.is_object() {
            continue;
        }
        saw_json_object = true;
        if last_active_at.is_none() {
            last_active_at = value.get("timestamp").and_then(parse_timestamp_to_ms);
        }
        if summary.is_none() {
            if !is_main_entry(&value) {
                continue;
            }
            if let Some(message) = value.get("message") {
                let text = message.get("content").map(extract_text).unwrap_or_default();
                if !text.trim().is_empty() {
                    summary = Some(text);
                }
            }
        }
        if last_active_at.is_some() && summary.is_some() {
            break;
        }
    }

    if !saw_json_object {
        return Ok(None);
    }

    let filename_stem = infer_session_id_from_filename(path);
    let session_id = filename_stem
        .filter(|stem| is_valid_claude_session_id(stem))
        .or_else(|| json_session_id.filter(|id| is_valid_claude_session_id(id)));
    let Some(session_id) = session_id else {
        return Ok(None);
    };

    // Priority: customTitle > aiTitle > summary > first user prompt > directory basename.
    let title = custom_title
        .or(ai_title)
        .or(summary_title)
        .map(|t| truncate_summary(&t, TITLE_MAX_CHARS))
        .or_else(|| first_user_message.map(|t| truncate_summary(&t, TITLE_MAX_CHARS)))
        .or_else(|| {
            project_dir
                .as_deref()
                .and_then(path_basename)
                .map(|v| v.to_string())
        });

    let summary = summary.map(|text| truncate_summary(&text, 160));

    Ok(Some(SessionMeta {
        provider_id: PROVIDER_ID.to_string(),
        session_id: session_id.clone(),
        residual: false,
        parent_session_id: None,
        archived: false,
        cleanup_pending: false,
        sidebar_section: None,
        title,
        summary,
        project_dir,
        project_name: None,
        created_at,
        last_active_at,
        source_path: Some(path.to_string_lossy().to_string()),
        resume_command: is_safe_session_id(&session_id)
            .then(|| format!("claude --resume {session_id}")),
    }))
}

fn is_agent_session(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.starts_with("agent-"))
        .unwrap_or(false)
}

fn infer_session_id_from_filename(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| stem.to_string())
}

fn collect_project_dirs_and_jsonl_files(
    root: &Path,
    project_dirs: &mut Vec<PathBuf>,
    files: &mut Vec<PathBuf>,
) {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            scan_warning(format!("无法扫描 {}：{error}", root.display()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                scan_warning(format!("无法枚举 {}：{error}", root.display()));
                continue;
            }
        };
        let project_path = entry.path();
        let meta = match fs::symlink_metadata(&project_path) {
            Ok(meta) => meta,
            Err(error) => {
                scan_warning(format!("无法读取 {}：{error}", project_path.display()));
                continue;
            }
        };
        if is_reparse_or_symlink(&meta) || !meta.file_type().is_dir() {
            continue;
        }
        project_dirs.push(project_path.clone());

        let project_entries = match fs::read_dir(&project_path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                scan_warning(format!("无法扫描 {}：{error}", project_path.display()));
                continue;
            }
        };
        for file_entry in project_entries {
            let file_entry = match file_entry {
                Ok(entry) => entry,
                Err(error) => {
                    scan_warning(format!("无法枚举 {}：{error}", project_path.display()));
                    continue;
                }
            };
            let file_path = file_entry.path();
            if file_path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let file_meta = match fs::symlink_metadata(&file_path) {
                Ok(meta) => meta,
                Err(error) => {
                    scan_warning(format!("无法读取 {}：{error}", file_path.display()));
                    continue;
                }
            };
            if is_reparse_or_symlink(&file_meta) || !file_meta.file_type().is_file() {
                continue;
            }
            files.push(file_path);
        }
    }
}

fn scan_project_index_ghosts(
    project_dir: &Path,
    existing: &[SessionMeta],
    out: &mut Vec<SessionMeta>,
) {
    let index_path = project_dir.join("sessions-index.json");
    let meta = match fs::symlink_metadata(&index_path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return,
        Err(err) => {
            scan_warning(format!(
                "无法读取 Claude 索引 {}：{err}",
                index_path.display()
            ));
            return;
        }
    };
    if is_reparse_or_symlink(&meta) || !meta.file_type().is_file() {
        scan_warning(format!(
            "跳过非普通文件 Claude 索引：{}",
            index_path.display()
        ));
        return;
    }
    if meta.len() > MAX_SESSIONS_INDEX_BYTES {
        scan_warning(format!(
            "Claude 索引 {} 超过 16 MB 上限，已跳过",
            index_path.display()
        ));
        return;
    }

    let mut raw = String::new();
    let read_res = File::open(&index_path).and_then(|file| {
        file.take(MAX_SESSIONS_INDEX_BYTES + 1)
            .read_to_string(&mut raw)
    });
    if let Err(err) = read_res {
        scan_warning(format!(
            "无法读取 Claude 索引 {}：{err}",
            index_path.display()
        ));
        return;
    }
    if raw.len() as u64 > MAX_SESSIONS_INDEX_BYTES {
        scan_warning(format!(
            "Claude 索引 {} 超过 16 MB 上限，已跳过",
            index_path.display()
        ));
        return;
    }

    let doc: Value = match serde_json::from_str(&raw) {
        Ok(doc) => doc,
        Err(err) => {
            scan_warning(format!(
                "无法解析 Claude 索引 {}：{err}",
                index_path.display()
            ));
            return;
        }
    };

    let default_project_path = doc
        .get("originalPath")
        .or_else(|| doc.get("projectPath"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let Some(entries) = doc.get("entries").and_then(Value::as_array) else {
        return;
    };

    let mut seen_ids = HashSet::new();
    for entry in entries {
        let Some(session_id) = entry
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| is_valid_claude_session_id(id))
        else {
            continue;
        };
        if !seen_ids.insert(session_id.to_string()) {
            continue;
        }
        let expected_jsonl = project_dir.join(format!("{session_id}.jsonl"));
        match fs::symlink_metadata(&expected_jsonl) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            _ => continue,
        }
        let already_discovered = existing.iter().any(|s| {
            s.session_id == session_id
                && s.source_path
                    .as_deref()
                    .and_then(|p| Path::new(p).parent())
                    .is_some_and(|p| p == project_dir)
        });
        if already_discovered {
            continue;
        }

        let entry_project_dir = entry
            .get("originalPath")
            .or_else(|| entry.get("projectPath"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| default_project_path.clone());

        let first_prompt = entry
            .get("firstPrompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| {
                !s.is_empty()
                    && !s.contains("<local-command-caveat>")
                    && !s.starts_with("<command-name>")
            });
        let entry_summary = entry
            .get("summary")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let entry_custom = entry
            .get("customTitle")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let entry_ai = entry
            .get("aiTitle")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let title = entry_custom
            .or(entry_ai)
            .or(entry_summary)
            .or(first_prompt)
            .map(|t| truncate_summary(t, TITLE_MAX_CHARS))
            .or_else(|| {
                entry_project_dir
                    .as_deref()
                    .and_then(path_basename)
                    .map(|v| v.to_string())
            });
        let summary = entry_summary
            .or(first_prompt)
            .map(|s| truncate_summary(s, 160));

        let created_at = entry
            .get("created")
            .or_else(|| entry.get("createdAt"))
            .and_then(parse_timestamp_to_ms);
        let last_active_at = entry
            .get("modified")
            .or_else(|| entry.get("updatedAt"))
            .or_else(|| entry.get("fileMtime"))
            .and_then(parse_timestamp_to_ms)
            .or(created_at);

        out.push(SessionMeta {
            provider_id: PROVIDER_ID.to_string(),
            session_id: session_id.to_string(),
            residual: true,
            parent_session_id: None,
            archived: false,
            cleanup_pending: false,
            sidebar_section: None,
            title,
            summary,
            project_dir: entry_project_dir,
            project_name: None,
            created_at,
            last_active_at,
            source_path: Some(expected_jsonl.to_string_lossy().to_string()),
            resume_command: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn compat_load(rows: Vec<Value>) -> Vec<SessionMessage> {
        let temp = tempdir().unwrap();
        let path = temp.path().join("fixture.jsonl");
        std::fs::write(
            &path,
            rows.iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        load_messages(&path).unwrap()
    }

    #[test]
    fn compat_latest_main_chain_deduplicates_and_excludes_other_writers() {
        let rows = vec![
            serde_json::json!({"type":"user","uuid":"u","message":{"role":"user","content":"root"}}),
            serde_json::json!({"type":"assistant","uuid":"old","parentUuid":"u","message":{"role":"assistant","content":"abandoned"}}),
            serde_json::json!({"type":"assistant","uuid":"a","parentUuid":"u","message":{"role":"assistant","content":"stale"}}),
            serde_json::json!({"type":"assistant","uuid":"a","parentUuid":"u","message":{"role":"assistant","content":"latest"}}),
            serde_json::json!({"type":"progress","uuid":"p","parentUuid":"a"}),
            serde_json::json!({"type":"user","uuid":"side","isSidechain":true,"message":{"role":"user","content":"side"}}),
            serde_json::json!({"type":"user","uuid":"team","teamName":"team","message":{"role":"user","content":"team"}}),
            serde_json::json!({"type":"user","uuid":"meta","isMeta":true,"message":{"role":"user","content":"meta"}}),
        ];
        let messages = compat_load(rows);
        assert_eq!(
            messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            ["root", "latest"]
        );
    }

    #[test]
    fn compat_meta_terminal_keeps_visible_parent_chain() {
        let messages = compat_load(vec![
            serde_json::json!({"type":"user","uuid":"u","message":{"role":"user","content":"visible"}}),
            serde_json::json!({"type":"user","uuid":"meta","parentUuid":"u","isMeta":true,"message":{"role":"user","content":"hidden"}}),
        ]);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "visible");
    }

    #[test]
    fn compat_parallel_calls_results_and_strays_are_retained_not_all_branches() {
        let rows = vec![
            serde_json::json!({"type":"user","uuid":"u","message":{"role":"user","content":"root"}}),
            serde_json::json!({"type":"assistant","uuid":"a1","parentUuid":"u","message":{"id":"response","role":"assistant","content":[{"type":"tool_use","id":"c1","name":"Read","input":{}}]}}),
            serde_json::json!({"type":"assistant","uuid":"a2","parentUuid":"a1","message":{"id":"response","role":"assistant","content":[{"type":"tool_use","id":"c2","name":"Search","input":{}}]}}),
            serde_json::json!({"type":"user","uuid":"r2","parentUuid":"a2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c2","content":"second result"}]}}),
            serde_json::json!({"type":"user","uuid":"r1","parentUuid":"a1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","content":"first result"}]}}),
            serde_json::json!({"type":"assistant","uuid":"end","parentUuid":"r1","message":{"id":"next","role":"assistant","content":"done"}}),
        ];
        let messages = compat_load(rows);
        assert_eq!(messages.len(), 6);
        assert!(messages[2].content.contains("Search"));
        assert_eq!(messages[3].content, "second result");
        assert_eq!(messages[4].role, "tool");
        assert_eq!(messages[5].content, "done");
    }

    #[test]
    fn compat_parallel_stray_answers_only_open_same_writer_call() {
        let rows = vec![
            serde_json::json!({"type":"assistant","uuid":"call","message":{"id":"response","role":"assistant","content":[{"type":"tool_use","id":"c","name":"Read","input":{}}]}}),
            serde_json::json!({"type":"user","uuid":"wrong-writer","parentUuid":"unrelated","agentId":"other","sourceToolAssistantUUID":"call","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":"wrong agent"}]}}),
            serde_json::json!({"type":"user","uuid":"stray","parentUuid":"unrelated","sourceToolAssistantUUID":"call","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":"recovered"}]}}),
            serde_json::json!({"type":"user","uuid":"duplicate","parentUuid":"unrelated","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":"duplicate"}]}}),
            serde_json::json!({"type":"assistant","uuid":"end","parentUuid":"call","message":{"id":"next","role":"assistant","content":"done"}}),
        ];
        let messages = compat_load(rows);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].content, "recovered");
        assert_eq!(messages[2].content, "done");
    }

    #[test]
    fn compat_compaction_does_not_follow_logical_parent_and_keeps_legacy_rows() {
        let rows = vec![
            serde_json::json!({"message":{"role":"user","content":"legacy before compact"}}),
            serde_json::json!({"type":"user","uuid":"old","message":{"role":"user","content":"old context"}}),
            serde_json::json!({"type":"system","uuid":"boundary","subtype":"compact_boundary","logicalParentUuid":"old"}),
            serde_json::json!({"type":"user","uuid":"summary","parentUuid":"boundary","isCompactSummary":true,"message":{"role":"user","content":"summary"}}),
            serde_json::json!({"message":{"role":"user","content":"legacy mixed"}}),
            serde_json::json!({"type":"assistant","uuid":"end","parentUuid":"summary","message":{"role":"assistant","content":"answer"}}),
        ];
        let messages = compat_load(rows);
        assert_eq!(
            messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            ["[Compacted Summary]\nsummary", "legacy mixed", "answer"]
        );
        let legacy = compat_load(vec![
            serde_json::json!({"message":{"role":"user","content":"old schema"}}),
        ]);
        assert_eq!(legacy[0].content, "old schema");
    }

    #[test]
    fn compat_oversized_scan_keeps_head_tail_metadata() {
        use std::io::{Seek, SeekFrom, Write};
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("large-session.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(file, "{}", json!({"sessionId":"large-session","cwd":"/fixture","type":"user","message":{"role":"user","content":"first prompt"}})).unwrap();
        for _ in 0..10 {
            writeln!(file, "{{}}").unwrap();
        }
        file.seek(SeekFrom::Start(
            super::super::utils::MAX_MESSAGES_FILE_BYTES + 1,
        ))
        .unwrap();
        writeln!(file).unwrap();
        for _ in 0..30 {
            writeln!(file, "{{}}").unwrap();
        }
        writeln!(
            file,
            "{}",
            json!({"type":"custom-title","customTitle":"Large title"})
        )
        .unwrap();
        drop(file);
        let meta = parse_session_result(&path).unwrap().unwrap();
        assert_eq!(meta.title.as_deref(), Some("Large title"));
        assert!(load_messages(&path).is_err());
    }

    #[test]
    fn compat_titles_scan_beyond_head_tail_and_custom_beats_ai() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("fixture.jsonl");
        let mut rows = vec![
            serde_json::json!({"sessionId":"fixture","cwd":"/tmp/project","type":"user","message":{"role":"user","content":"prompt"}}),
        ];
        rows.extend((0..15).map(|_| serde_json::json!({"type":"progress"})));
        rows.push(serde_json::json!({"type":"ai-title","aiTitle":"AI title"}));
        rows.extend((0..40).map(|_| serde_json::json!({"type":"progress"})));
        let write_rows = |rows: &[Value]| {
            std::fs::write(
                &path,
                rows.iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .unwrap()
        };
        write_rows(&rows);
        assert_eq!(
            parse_session(&path).unwrap().title.as_deref(),
            Some("AI title")
        );
        rows.insert(17, serde_json::json!({"customTitle":"My explicit title"}));
        rows.push(serde_json::json!({"aiTitle":"New AI title"}));
        write_rows(&rows);
        assert_eq!(
            parse_session(&path).unwrap().title.as_deref(),
            Some("My explicit title")
        );
    }

    #[test]
    fn delete_session_removes_main_file_and_sidecar_directory() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("projects");
        let project = root.join("my-project");
        let path = project.join("session-123.jsonl");
        let sidecar = project.join("session-123");
        let independent_branch = project.join("branch-456.jsonl");
        let subagents = sidecar.join("subagents");
        let tool_results = sidecar.join("tool-results");

        std::fs::create_dir_all(&subagents).expect("create subagents");
        std::fs::create_dir_all(&tool_results).expect("create tool-results");
        std::fs::write(subagents.join("agent-1.jsonl"), "{}").expect("write subagent");
        std::fs::write(tool_results.join("tool-1.txt"), "result").expect("write tool result");
        std::fs::write(
            &path,
            concat!(
                "{\"sessionId\":\"session-123\",\"cwd\":\"/tmp/project\",\"timestamp\":\"2026-03-06T10:00:00Z\"}\n",
                "{\"message\":{\"role\":\"user\",\"content\":\"hello\"},\"timestamp\":\"2026-03-06T10:01:00Z\"}\n"
            ),
        )
        .expect("write session");
        std::fs::write(
            &independent_branch,
            concat!(
                "{\"sessionId\":\"branch-456\",\"cwd\":\"/tmp/project\",\"timestamp\":\"2026-03-06T10:02:00Z\"}\n",
                "{\"message\":{\"role\":\"user\",\"content\":\"branched copy\"},\"timestamp\":\"2026-03-06T10:03:00Z\"}\n"
            ),
        )
        .expect("write independent branch");

        delete_session(&root, &path, "session-123").expect("delete session");

        assert!(!path.exists());
        assert!(!sidecar.exists());
        assert!(independent_branch.exists());
    }

    /// sidecar 位置是一个指向别处的链接时，删除只能摘掉链接本身，绝不能顺着它把
    /// 目标目录的内容删掉。旧实现用 `metadata`（跟随链接）判断 `is_dir()`，会把
    /// 链接当成真目录交给 `remove_dir_all`。
    #[test]
    fn a_symlinked_sidecar_does_not_delete_its_target_contents() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("projects");
        let project = root.join("my-project");
        std::fs::create_dir_all(&project).expect("create project");
        let path = project.join("session-123.jsonl");
        let sidecar = project.join("session-123");

        // 链接目标放在另一个目录里，模拟"项目之外的真实数据"
        let victim = temp.path().join("outside").join("real-data");
        std::fs::create_dir_all(&victim).expect("create victim");
        let sentinel = victim.join("keep.txt");
        std::fs::write(&sentinel, "keep").expect("write sentinel");

        if create_dir_symlink(&victim, &sidecar).is_err() {
            // Windows 未开启开发者模式 / 无权限时跳过；CI 的 Linux 任务仍会实际执行
            eprintln!("skipped: 本机不允许创建目录符号链接");
            return;
        }

        std::fs::write(
            &path,
            concat!(
                "{\"sessionId\":\"session-123\",\"cwd\":\"/tmp/project\",\"timestamp\":\"2026-03-06T10:00:00Z\"}\n",
                "{\"message\":{\"role\":\"user\",\"content\":\"hello\"},\"timestamp\":\"2026-03-06T10:01:00Z\"}\n"
            ),
        )
        .expect("write session");

        delete_session(&root, &path, "session-123").expect("delete session");

        assert!(!path.exists());
        assert!(
            std::fs::symlink_metadata(&sidecar).is_err(),
            "sidecar 链接本身应被删除"
        );
        assert!(
            sentinel.exists(),
            "链接目标的内容必须原样保留：删除只能摘掉链接本身"
        );
        assert_eq!(
            std::fs::read_to_string(&sentinel).expect("read sentinel"),
            "keep"
        );
    }

    #[cfg(unix)]
    fn create_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
    }

    #[cfg(not(any(unix, windows)))]
    fn create_dir_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "unsupported platform",
        ))
    }

    #[test]
    fn load_messages_tool_use_shows_as_assistant() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Write\",\"input\":{\"file_path\":\"a.txt\"}}]},\"timestamp\":\"2026-03-06T10:00:00Z\"}\n",
                "{\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_1\",\"content\":\"File written\"}]},\"timestamp\":\"2026-03-06T10:00:01Z\"}\n",
            ),
        )
        .expect("write");

        let msgs = load_messages(&path).expect("load");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "assistant");
        assert!(msgs[0].content.contains("[Tool: Write]"));
        assert_eq!(msgs[1].role, "tool");
        assert_eq!(msgs[1].content, "File written");
    }

    #[test]
    fn load_messages_mixed_text_and_tool_use() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            "{\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Let me help.\"},{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Read\",\"input\":{}}]},\"timestamp\":\"2026-03-06T10:00:00Z\"}\n",
        )
        .expect("write");

        let msgs = load_messages(&path).expect("load");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "assistant");
        assert!(msgs[0].content.contains("Let me help."));
        assert!(msgs[0].content.contains("[Tool: Read]"));
    }

    #[test]
    fn load_messages_mixed_user_tool_result_and_text_stays_user() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            "{\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_1\",\"content\":\"result\"},{\"type\":\"text\",\"text\":\"Please continue\"}]},\"timestamp\":\"2026-03-06T10:00:00Z\"}\n",
        )
        .expect("write");

        let msgs = load_messages(&path).expect("load");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        assert!(msgs[0].content.contains("Please continue"));
    }

    #[test]
    fn parse_session_clears_resume_command_for_unsafe_filename_stem() {
        let temp = tempdir().expect("tempdir");
        // 不安全的文件名 stem 在缺少合法 JSON sessionId 时直接拒绝入库，防止元字符进入恢复命令或无法删除
        let path = temp.path().join("weird; id-session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"cwd\":\"/tmp/project\",\"timestamp\":\"2026-03-06T10:00:00Z\"}\n",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello\"},\"timestamp\":\"2026-03-06T10:01:00Z\"}\n"
            ),
        )
        .expect("write");
        assert!(parse_session(&path).is_none());

        // 若 JSON 内同样携带恶意 sessionId，也必须拒绝
        std::fs::write(
            &path,
            "{\"sessionId\":\"$(evil)\",\"cwd\":\"/tmp/project\"}\n",
        )
        .expect("write unsafe json id");
        assert!(parse_session(&path).is_none());
    }

    #[test]
    fn parse_session_uses_first_user_message_as_title() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session-abc.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"sessionId\":\"session-abc\",\"cwd\":\"/tmp/project\",\"timestamp\":\"2026-03-06T10:00:00Z\"}\n",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"How do I deploy?\"},\"sessionId\":\"session-abc\",\"timestamp\":\"2026-03-06T10:01:00Z\"}\n",
                "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"Here is how...\"},\"timestamp\":\"2026-03-06T10:02:00Z\"}\n",
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        assert_eq!(meta.title.as_deref(), Some("How do I deploy?"));
    }

    #[test]
    fn parse_session_custom_title_overrides_first_message() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session-def.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"sessionId\":\"session-def\",\"cwd\":\"/tmp/project\",\"timestamp\":\"2026-03-06T10:00:00Z\"}\n",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"fix something\"},\"sessionId\":\"session-def\",\"timestamp\":\"2026-03-06T10:01:00Z\"}\n",
                "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"Done.\"},\"timestamp\":\"2026-03-06T10:02:00Z\"}\n",
                "{\"type\":\"custom-title\",\"customTitle\":\"fix-login-bug\",\"sessionId\":\"session-def\"}\n",
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        assert_eq!(meta.title.as_deref(), Some("fix-login-bug"));
    }

    #[test]
    fn parse_session_falls_back_to_dir_basename() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session-ghi.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"sessionId\":\"session-ghi\",\"cwd\":\"/tmp/my-project\",\"timestamp\":\"2026-03-06T10:00:00Z\"}\n",
                "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"timestamp\":\"2026-03-06T10:01:00Z\"}\n",
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        // No user message and no custom-title → falls back to dir basename
        assert_eq!(meta.title.as_deref(), Some("my-project"));
    }

    #[test]
    fn parse_session_truncates_long_title() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session-trunc.jsonl");
        let long_msg = "a".repeat(200);
        std::fs::write(
            &path,
            format!(
                "{{\"sessionId\":\"session-trunc\",\"cwd\":\"/tmp/p\",\"timestamp\":\"2026-03-06T10:00:00Z\"}}\n\
                 {{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{long_msg}\"}},\"sessionId\":\"session-trunc\",\"timestamp\":\"2026-03-06T10:01:00Z\"}}\n",
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        let title = meta.title.unwrap();
        assert!(title.len() <= TITLE_MAX_CHARS + 3); // +3 for "..."
        assert!(title.ends_with("..."));
    }

    #[test]
    fn parse_session_new_format_with_snapshot() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session-new.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"file-history-snapshot\",\"messageId\":\"msg-1\",\"snapshot\":{},\"isSnapshotUpdate\":false}\n",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"请帮我重构这个函数\"},\"sessionId\":\"session-new\",\"timestamp\":\"2026-03-06T10:00:00Z\",\"cwd\":\"/tmp/project\"}\n",
                "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"OK\"},\"timestamp\":\"2026-03-06T10:01:00Z\",\"cwd\":\"/tmp/project\"}\n",
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        assert_eq!(meta.title.as_deref(), Some("请帮我重构这个函数"));
    }

    #[test]
    fn parse_session_skips_command_caveat_and_slash_commands() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session-clear.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"file-history-snapshot\",\"messageId\":\"msg-1\",\"snapshot\":{},\"isSnapshotUpdate\":false}\n",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"<local-command-caveat>Caveat: The messages below were generated by the user while running local commands.</local-command-caveat>\"},\"sessionId\":\"session-clear\",\"timestamp\":\"2026-03-06T10:00:00Z\",\"cwd\":\"/tmp/project\"}\n",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"<command-name>/clear</command-name>\\n<command-message>clear</command-message>\"},\"sessionId\":\"session-clear\",\"timestamp\":\"2026-03-06T10:00:01Z\",\"cwd\":\"/tmp/project\"}\n",
                "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"Done.\"},\"timestamp\":\"2026-03-06T10:00:02Z\",\"cwd\":\"/tmp/project\"}\n",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"帮我看看工作区的改动\"},\"sessionId\":\"session-clear\",\"timestamp\":\"2026-03-06T10:01:00Z\",\"cwd\":\"/tmp/project\"}\n",
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        assert_eq!(meta.title.as_deref(), Some("帮我看看工作区的改动"));
    }

    #[cfg(unix)]
    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_file(target, link)
    }

    #[cfg(not(any(unix, windows)))]
    fn create_file_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "unsupported platform",
        ))
    }

    #[test]
    fn discovery_and_preflight_enforce_depth_two_and_protect_project_and_memory_dirs() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("projects");
        let project = root.join("proj-alpha");
        let memory_dir = project.join("memory");
        let subagents_dir = project
            .join("550e8400-e29b-41d4-a716-446655440000")
            .join("subagents");
        fs::create_dir_all(&memory_dir).unwrap();
        fs::create_dir_all(&subagents_dir).unwrap();

        fs::write(memory_dir.join("MEMORY.md"), "keep memory").unwrap();
        // Depth-3 .jsonl files inside memory/ or subagents/ must NOT be scanned
        fs::write(
            memory_dir.join("nested.jsonl"),
            "{\"sessionId\":\"nested-mem\",\"cwd\":\"/p\"}\n",
        )
        .unwrap();
        fs::write(
            subagents_dir.join("sub.jsonl"),
            "{\"sessionId\":\"sub-1\",\"cwd\":\"/p\"}\n",
        )
        .unwrap();

        // Depth-1 <root>/proj-alpha.jsonl whose stem matches the project folder must NOT be scanned or deleted
        let depth_one_hazard = root.join("proj-alpha.jsonl");
        fs::write(
            &depth_one_hazard,
            "{\"sessionId\":\"proj-alpha\",\"cwd\":\"/p\"}\n",
        )
        .unwrap();

        // Reserved memory.jsonl without valid sessionId must be ignored; empty/non-object .jsonl ignored
        fs::write(project.join("memory.jsonl"), "{\"cwd\":\"/p\"}\n").unwrap();
        fs::write(project.join("empty-uuid.jsonl"), "   \n[]\n123\n").unwrap();

        // Real depth-2 session
        let valid_id = "550e8400-e29b-41d4-a716-446655440000";
        let valid_path = project.join(format!("{valid_id}.jsonl"));
        fs::write(
            &valid_path,
            format!("{{\"sessionId\":\"{valid_id}\",\"cwd\":\"/p\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n"),
        )
        .unwrap();

        let scanned = scan_sessions_in_root(&root, &FileParseCache::new());
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].session_id, valid_id);

        // Preflight & delete_session must reject depth-1 hazard, depth-3 path, and "memory" session_id
        assert!(preflight_delete(&root, &depth_one_hazard, "proj-alpha").is_err());
        assert!(delete_session(&root, &depth_one_hazard, "proj-alpha").is_err());
        assert!(project.exists(), "project directory must never be deleted");

        assert!(preflight_delete(&root, &memory_dir.join("nested.jsonl"), "nested-mem").is_err());
        assert!(preflight_delete(&root, &project.join("memory.jsonl"), "memory").is_err());

        // Even if memory.jsonl contains a valid sessionId, preflight/delete must refuse reserved "memory.jsonl"
        fs::write(
            project.join("memory.jsonl"),
            "{\"sessionId\":\"safe-id-999\",\"cwd\":\"/p\"}\n",
        )
        .unwrap();
        assert!(preflight_delete(&root, &project.join("memory.jsonl"), "safe-id-999").is_err());
        assert!(memory_dir.join("MEMORY.md").exists());

        // Symlinked .jsonl source must be rejected before touching target
        let link_jsonl = project.join("link-session.jsonl");
        if create_file_symlink(&valid_path, &link_jsonl).is_ok() {
            assert!(preflight_delete(&root, &link_jsonl, valid_id).is_err());
            assert!(delete_session(&root, &link_jsonl, valid_id).is_err());
            assert!(valid_path.exists());
        }
    }

    #[test]
    fn forked_session_prefers_filename_stem_and_reads_deep_head_and_bounded_titles() {
        let temp = tempdir().expect("tempdir");
        let fork_id = "11111111-2222-3333-4444-555555555555";
        let path = temp.path().join(format!("{fork_id}.jsonl"));

        let mut lines = Vec::new();
        // 45 leading snapshot/queue lines before cwd and first user message (> old 10-line window!)
        lines.push(json!({"type":"file-history-snapshot","sessionId":"parent-0000"}).to_string());
        for i in 0..44 {
            lines.push(json!({"type":"queue-operation","index":i}).to_string());
        }
        lines.push(
            json!({
                "type":"user",
                "sessionId":"parent-0000",
                "cwd":"/workspace/deep-app",
                "timestamp":"2026-03-06T10:00:00Z",
                "message":{"role":"user","content":"Prompt at line 46"}
            })
            .to_string(),
        );
        // Oversized >4 MiB single line must be drained and skipped without failing the session
        let huge_blob = "x".repeat(MAX_CLAUDE_LINE_BYTES + 64 * 1024);
        lines.push(format!(
            "{{\"type\":\"progress\",\"blob\":\"{huge_blob}\"}}"
        ));
        lines.push(json!({"type":"summary","summary":"Summary fallback title"}).to_string());
        lines.push(json!({"type":"custom-title","customTitle":"Temporary custom"}).to_string());
        // Later empty customTitle clears custom_title so summary/aiTitle can take effect
        lines.push(json!({"type":"custom-title","customTitle":""}).to_string());
        lines.push(
            json!({
                "type":"assistant",
                "timestamp":"2026-03-06T10:05:00Z",
                "message":{"role":"assistant","content":"Final tail answer"}
            })
            .to_string(),
        );
        fs::write(&path, lines.join("\n")).unwrap();

        let meta = parse_session(&path).expect("parse session with >4MiB line");
        assert_eq!(meta.session_id, fork_id);
        assert_eq!(
            meta.resume_command.as_deref(),
            Some("claude --resume 11111111-2222-3333-4444-555555555555")
        );
        assert_eq!(meta.project_dir.as_deref(), Some("/workspace/deep-app"));
        assert_eq!(meta.title.as_deref(), Some("Summary fallback title"));
        assert_eq!(meta.summary.as_deref(), Some("Final tail answer"));
    }

    #[test]
    fn sessions_index_ghosts_and_claude_sidecars_are_discovered_and_cleaned() {
        let temp = tempdir().expect("tempdir");
        let claude_dir = temp.path().join(".claude");
        let root = claude_dir.join("projects");
        let project = root.join("-work-demo");
        fs::create_dir_all(&project).unwrap();

        let live_id = "live-1111";
        let ghost_id = "ghost-2222";
        let keep_id = "keep-3333";

        let live_jsonl = project.join(format!("{live_id}.jsonl"));
        fs::write(
            &live_jsonl,
            format!("{{\"sessionId\":\"{live_id}\",\"cwd\":\"/work/demo\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"live prompt\"}}}}\n"),
        )
        .unwrap();

        let index_path = project.join("sessions-index.json");
        fs::write(
            &index_path,
            serde_json::to_string_pretty(&json!({
                "version": 1,
                "originalPath": "/work/demo",
                "entries": [
                    {
                        "sessionId": live_id,
                        "firstPrompt": "live prompt",
                        "created": "2026-03-06T10:00:00Z",
                        "modified": "2026-03-06T10:01:00Z"
                    },
                    {
                        "sessionId": ghost_id,
                        "customTitle": "Orphaned Resume Entry",
                        "summary": "Ghost summary",
                        "projectPath": "/work/demo",
                        "created": "2026-03-06T09:00:00Z",
                        "modified": "2026-03-06T09:30:00Z"
                    },
                    {
                        "sessionId": keep_id,
                        "firstPrompt": "Keep me",
                        "created": "2026-03-06T08:00:00Z"
                    }
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        // Populate ~/.claude/ per-session sidecars and legacy flat agent-*.jsonl for live_id + keep_id
        for sid in [live_id, keep_id] {
            fs::create_dir_all(claude_dir.join("file-history").join(sid)).unwrap();
            fs::write(claude_dir.join("file-history").join(sid).join("v1"), "snap").unwrap();
            fs::create_dir_all(claude_dir.join("tasks").join(sid)).unwrap();
            fs::create_dir_all(claude_dir.join("session-env").join(sid)).unwrap();
            fs::create_dir_all(claude_dir.join("debug")).unwrap();
            fs::write(claude_dir.join("debug").join(format!("{sid}.txt")), "dbg").unwrap();
            fs::create_dir_all(claude_dir.join("todos")).unwrap();
            fs::write(
                claude_dir.join("todos").join(format!("{sid}-agent-a.json")),
                "[]",
            )
            .unwrap();
            fs::write(claude_dir.join("todos").join(format!("{sid}.json")), "[]").unwrap();
        }
        fs::write(
            project.join("agent-sub1.jsonl"),
            format!("{{\"sessionId\":\"{live_id}\",\"isSidechain\":true}}\n"),
        )
        .unwrap();
        fs::write(
            project.join("agent-sub2.jsonl"),
            format!("{{\"sessionId\":\"{keep_id}\",\"isSidechain\":true}}\n"),
        )
        .unwrap();
        fs::write(
            claude_dir.join("history.jsonl"),
            format!(
                "{{\"sessionId\":\"{live_id}\",\"display\":\"ask live\"}}\n\
                 {{\"sessionId\":\"{keep_id}\",\"display\":\"ask keep\"}}\n"
            ),
        )
        .unwrap();

        assert_eq!(
            resolve_verified_claude_dir(&root, &claude_dir),
            Some(claude_dir.canonicalize().unwrap())
        );
        assert_eq!(resolve_verified_claude_dir(temp.path(), &claude_dir), None);

        let sessions = scan_sessions_in_root(&root, &FileParseCache::new());
        // live_id (from .jsonl, NOT duplicated from index) + ghost_id + keep_id (both ghosts)
        assert_eq!(sessions.len(), 3);
        let ghost = sessions
            .iter()
            .find(|s| s.session_id == ghost_id)
            .expect("ghost surfaced");
        assert!(ghost.residual);
        assert!(ghost.resume_command.is_none());
        assert_eq!(ghost.title.as_deref(), Some("Orphaned Resume Entry"));
        assert_eq!(ghost.project_dir.as_deref(), Some("/work/demo"));

        // Delete live_id: removes .jsonl, index entry, flat agent-sub1.jsonl, and all ~/.claude sidecars
        assert!(
            delete_session_with_claude_dir(&root, &live_jsonl, live_id, Some(&claude_dir)).unwrap()
        );
        assert!(!live_jsonl.exists());
        assert!(!project.join("agent-sub1.jsonl").exists());
        assert!(project.join("agent-sub2.jsonl").exists());
        assert!(!claude_dir.join("file-history").join(live_id).exists());
        assert!(!claude_dir.join("tasks").join(live_id).exists());
        assert!(!claude_dir.join("session-env").join(live_id).exists());
        assert!(!claude_dir
            .join("debug")
            .join(format!("{live_id}.txt"))
            .exists());
        assert!(!claude_dir
            .join("todos")
            .join(format!("{live_id}-agent-a.json"))
            .exists());
        assert!(!claude_dir
            .join("todos")
            .join(format!("{live_id}.json"))
            .exists());
        assert!(claude_dir.join("file-history").join(keep_id).exists());
        assert!(claude_dir
            .join("todos")
            .join(format!("{keep_id}.json"))
            .exists());

        let history_after = fs::read_to_string(claude_dir.join("history.jsonl")).unwrap();
        assert!(!history_after.contains(live_id));
        assert!(history_after.contains(keep_id));

        // Delete ghost_id (whose .jsonl never existed on disk!)
        let ghost_path = PathBuf::from(ghost.source_path.as_ref().unwrap());
        preflight_delete(&root, &ghost_path, ghost_id).expect("ghost preflight");
        assert!(
            delete_session_with_claude_dir(&root, &ghost_path, ghost_id, Some(&claude_dir))
                .unwrap()
        );

        let index_after: Value =
            serde_json::from_str(&fs::read_to_string(&index_path).unwrap()).unwrap();
        let remaining_ids: Vec<&str> = index_after["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e.get("sessionId").and_then(Value::as_str))
            .collect();
        assert_eq!(remaining_ids, vec![keep_id]);
    }

    #[test]
    fn load_messages_emits_thinking_before_assistant_and_handles_rich_blocks() {
        let messages = compat_load(vec![
            json!({
                "type": "user",
                "uuid": "u1",
                "message": {
                    "role": "user",
                    "content": [
                        {"type": "image"},
                        {"type": "document"},
                        {"type": "text", "text": "Inspect these"}
                    ]
                }
            }),
            json!({
                "type": "assistant",
                "uuid": "a1",
                "parentUuid": "u1",
                "message": {
                    "id": "resp-1",
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "Analyzing attachments"},
                        {"type": "server_tool_use", "id": "srv_1", "name": "web_search"},
                        {
                            "type": "web_search_tool_result",
                            "content": [{"title": "Docs", "url": "https://docs.example.com"}]
                        },
                        {"type": "mcp_tool_use", "id": "mcp_1", "name": "linear_get_issue"},
                        {"type": "text", "text": "All set."}
                    ]
                }
            }),
        ]);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "[Image]\n[Document]\nInspect these");
        assert_eq!(messages[1].role, "thinking");
        assert_eq!(messages[1].content, "Analyzing attachments");
        assert_eq!(messages[2].role, "assistant");
        assert_eq!(
            messages[2].content,
            "[Tool: web_search]\n- Docs (https://docs.example.com)\n[Tool: linear_get_issue]\nAll set."
        );
    }
}
