use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use serde_json::Value;

use crate::session_manager::{SessionMessage, SessionMeta};
use crate::session_paths::claude_config_dir;

use super::utils::{
    collect_files_where, ensure_readable_size, extract_text, is_safe_session_id,
    parse_timestamp_to_ms, path_basename, read_head_tail_lines, remove_path_if_exists,
    truncate_summary, FileParseCache, TITLE_MAX_CHARS,
};

const PROVIDER_ID: &str = "claude";

static PARSE_CACHE: LazyLock<FileParseCache> = LazyLock::new(FileParseCache::new);

pub fn scan_sessions() -> Vec<SessionMeta> {
    let root = claude_config_dir().join("projects");
    let mut files = Vec::new();
    collect_jsonl_files(&root, &mut files);
    PARSE_CACHE.scan(files, parse_session_result)
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

        if role == "user" {
            if let Some(Value::Array(items)) = message.get("content") {
                if !items.is_empty()
                    && items
                        .iter()
                        .all(|item| item.get("type").and_then(Value::as_str) == Some("tool_result"))
                {
                    role = "tool".to_string();
                }
            }
        }
        let content = message.get("content").map(extract_text).unwrap_or_default();
        if !content.trim().is_empty() {
            messages.push(SessionMessage {
                role,
                content,
                ts: value.get("timestamp").and_then(parse_timestamp_to_ms),
            });
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

pub fn delete_session(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    let meta = parse_session(path).ok_or_else(|| {
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

    if let Some(stem) = path.file_stem() {
        let sibling = path.parent().unwrap_or_else(|| Path::new("")).join(stem);
        let sibling = super::utils::checked_storage_child(root, &sibling)?;
        remove_path_if_exists(&sibling).map_err(|e| {
            format!(
                "Failed to delete Claude session sidecar {}: {e}",
                sibling.display()
            )
        })?;
    }

    std::fs::remove_file(path).map_err(|e| {
        format!(
            "Failed to delete Claude session file {}: {e}",
            path.display()
        )
    })?;

    Ok(true)
}

fn parse_session(path: &Path) -> Option<SessionMeta> {
    parse_session_result(path).ok().flatten()
}

/// 扫描缓存用：读失败（权限、被占用）返回 `Err`，本轮不缓存、下轮重试；`agent-*`
/// 前缀或解析不出会话 ID 属于只依赖文件本身的确定性结果，返回 `Ok(None)` 进缓存。
fn parse_session_result(path: &Path) -> Result<Option<SessionMeta>, String> {
    if is_agent_session(path) {
        return Ok(None);
    }

    let (head, tail) = read_head_tail_lines(path, 10, 30).map_err(|error| {
        let message = format!("无法读取 {}：{error}", path.display());
        super::utils::scan_warning(message.clone());
        message
    })?;

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
        if session_id.is_none() {
            session_id = value
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
        if session_id.is_some()
            && project_dir.is_some()
            && created_at.is_some()
            && first_user_message.is_some()
        {
            break;
        }
    }

    // Extract last_active_at, summary, and custom-title from tail lines (reverse order)
    let mut last_active_at: Option<i64> = None;
    let mut summary: Option<String> = None;
    let mut custom_title: Option<String> = None;

    for line in tail.iter().rev() {
        let value: Value = match serde_json::from_str(line) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        if last_active_at.is_none() {
            last_active_at = value.get("timestamp").and_then(parse_timestamp_to_ms);
        }
        // Look for custom-title entry (take the last one, i.e. first in reverse)
        if custom_title.is_none()
            && value.get("type").and_then(Value::as_str) == Some("custom-title")
        {
            custom_title = value
                .get("customTitle")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
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
        if last_active_at.is_some() && summary.is_some() && custom_title.is_some() {
            break;
        }
    }

    let session_id = session_id.or_else(|| infer_session_id_from_filename(path));
    let Some(session_id) = session_id else {
        return Ok(None);
    };

    // Explicit titles can be anywhere (including outside both sampling windows).
    // Read the complete bounded stream without retaining transcript bodies.
    let oversized = std::fs::metadata(path)
        .map_err(|error| error.to_string())?
        .len()
        > super::utils::MAX_MESSAGES_FILE_BYTES;
    let mut ai_title = None;
    let title_lines: Box<dyn Iterator<Item = std::io::Result<String>>> = if oversized {
        super::utils::scan_warning(format!(
            "Claude 会话 {} 正文超限，标题仅依据头尾元信息",
            path.display()
        ));
        Box::new(
            head.iter()
                .chain(tail.iter())
                .cloned()
                .map(Ok)
                .collect::<Vec<_>>()
                .into_iter(),
        )
    } else {
        let file =
            File::open(path).map_err(|error| format!("Failed to read Claude titles: {error}"))?;
        Box::new(BufReader::new(file).lines())
    };
    custom_title = None;
    for line in title_lines {
        let line = line.map_err(|error| format!("Failed to read Claude titles: {error}"))?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if !is_main_entry(&value) {
            continue;
        }
        for (field, target) in [
            ("customTitle", &mut custom_title),
            ("aiTitle", &mut ai_title),
        ] {
            if let Some(text) = value
                .get(field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                *target = Some(text.to_string());
            }
        }
    }
    // SDK priority: customTitle > aiTitle > first user prompt > directory basename.
    let title = custom_title
        .or(ai_title)
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
            ["summary", "legacy mixed", "answer"]
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
        let path = temp.path().join("abc123-session.jsonl");
        let sidecar = temp.path().join("abc123-session");
        let independent_branch = temp.path().join("def456-branch.jsonl");
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

        delete_session(temp.path(), &path, "session-123").expect("delete session");

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
        let path = temp.path().join("abc123-session.jsonl");
        let sidecar = temp.path().join("abc123-session");

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

        delete_session(temp.path(), &path, "session-123").expect("delete session");

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
        // 无 sessionId 时 ID 兜底来自文件名 stem，同样不允许带 shell 元字符
        let path = temp.path().join("weird; id-session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"cwd\":\"/tmp/project\",\"timestamp\":\"2026-03-06T10:00:00Z\"}\n",
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello\"},\"timestamp\":\"2026-03-06T10:01:00Z\"}\n"
            ),
        )
        .expect("write");

        let meta = parse_session(&path).unwrap();
        assert_eq!(meta.session_id, "weird; id-session");
        assert!(meta.resume_command.is_none());
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
}
