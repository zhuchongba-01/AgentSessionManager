use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use serde_json::Value;

use crate::session_manager::cache::TranscriptSources;
use crate::session_manager::content::parse_file_url;
use crate::session_manager::model::{
    ContentRef, DiffFile, DiffOp, DiffSummary, EventKind, ImageRef, ImageSource, MessageMeta,
    SessionBlock, ToolStatus,
};
use crate::session_manager::{SessionMessage, SessionMeta};
use crate::session_paths::{configured_dir, kimi_home};

use super::blocks::{
    assign_turn_ids, count_diff_lines, estimate_base64_size, large_text_block, line_change_counts,
    one_line_title, parse_arguments, summary_event_block, thinking_block, tool_call_block,
    tool_result_block, ToolSource,
};
use super::utils::{
    checked_storage_child, ensure_readable_size, extract_text, is_reparse_or_symlink,
    is_safe_session_id, parse_timestamp_to_ms, path_basename, read_head_tail_lines,
    remove_path_if_exists, scan_warning, truncate_summary, FileParseCache, JsonlSpan, LineSpans,
    TITLE_MAX_CHARS,
};

const PROVIDER_ID: &str = "kimi";
const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024;
const HEAD_SCAN_LINES: usize = 40;
const TAIL_SCAN_LINES: usize = 20;

#[derive(Debug, Clone, Default)]
struct ParsedWireSummary {
    source_path: PathBuf,
    first_user_prompt: Option<String>,
    last_visible: Option<String>,
    created_at: Option<i64>,
    last_active_at: Option<i64>,
}

static PARSE_CACHE: LazyLock<FileParseCache<ParsedWireSummary>> =
    LazyLock::new(FileParseCache::new);

pub fn session_roots() -> Vec<PathBuf> {
    vec![kimi_home().join("sessions")]
}

#[allow(dead_code)]
pub fn transcript_sources(path: &Path) -> Result<TranscriptSources, String> {
    Ok(vec![(path.to_path_buf(), None)])
}

pub fn scan_sessions() -> Vec<SessionMeta> {
    let home = kimi_home();
    scan_sessions_in_home(&home, &PARSE_CACHE)
}

fn read_bounded_json(path: &Path) -> Option<Value> {
    let meta = fs::symlink_metadata(path).ok()?;
    if is_reparse_or_symlink(&meta) || !meta.file_type().is_file() || meta.len() > MAX_STATE_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn select_transcript_file(session_dir: &Path) -> Option<PathBuf> {
    for rel in [
        Path::new("agents").join("main").join("wire.jsonl"),
        PathBuf::from("context.jsonl"),
        PathBuf::from("wire.jsonl"),
    ] {
        let candidate = session_dir.join(rel);
        if let Ok(meta) = fs::symlink_metadata(&candidate) {
            if !is_reparse_or_symlink(&meta) && meta.file_type().is_file() {
                if candidate == session_dir.join("agents").join("main").join("wire.jsonl") {
                    let agents = session_dir.join("agents");
                    let main_dir = agents.join("main");
                    if fs::symlink_metadata(&agents).is_ok_and(|m| is_reparse_or_symlink(&m))
                        || fs::symlink_metadata(&main_dir).is_ok_and(|m| is_reparse_or_symlink(&m))
                    {
                        continue;
                    }
                }
                return Some(candidate);
            }
        }
    }
    None
}

fn build_resume_command(home: &Path, id: &str, archived: bool) -> Option<String> {
    if archived || !is_safe_session_id(id) {
        return None;
    }
    let custom_env =
        configured_dir("KIMI_SHARE_DIR").is_some() || configured_dir("KIMI_CODE_HOME").is_some();
    if custom_env {
        #[cfg(windows)]
        return Some(format!(
            "$env:KIMI_CODE_HOME = '{}'; kimi --session {id}",
            home.to_string_lossy().replace('\'', "''")
        ));
        #[cfg(not(windows))]
        return Some(format!(
            "env KIMI_CODE_HOME={} kimi --session {id}",
            crate::session_manager::terminal::shell_escape(&home.to_string_lossy())
        ));
    }
    Some(format!("kimi --session {id}"))
}

fn scan_sessions_in_home(
    home: &Path,
    cache: &FileParseCache<ParsedWireSummary>,
) -> Vec<SessionMeta> {
    let sessions_root = home.join("sessions");
    match fs::symlink_metadata(&sessions_root) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                scan_warning(format!(
                    "Kimi 会话根目录是符号链接或重解析点，已跳过：{}",
                    sessions_root.display()
                ));
                return Vec::new();
            }
            if !meta.is_dir() {
                return Vec::new();
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            scan_warning(format!(
                "无法检查 Kimi 会话目录 {}：{err}",
                sessions_root.display()
            ));
            return Vec::new();
        }
    }

    let ws_entries = match fs::read_dir(&sessions_root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            scan_warning(format!(
                "无法扫描 Kimi 目录 {}：{err}",
                sessions_root.display()
            ));
            return Vec::new();
        }
    };

    struct DiscoveredKimiSession {
        id: String,
        transcript: PathBuf,
        state: Option<Value>,
    }

    let mut discovered = Vec::new();
    let mut fallback_files = Vec::new();

    for ws_entry in ws_entries {
        let Ok(ws_entry) = ws_entry else {
            continue;
        };
        let Ok(ws_ft) = ws_entry.file_type() else {
            continue;
        };
        if !ws_ft.is_dir() {
            continue;
        }
        let ws_name = ws_entry.file_name();
        let Some(ws_name_str) = ws_name.to_str() else {
            continue;
        };
        if ws_name_str.starts_with('.') {
            continue;
        }
        let ws_path = ws_entry.path();
        if fs::symlink_metadata(&ws_path).is_ok_and(|m| is_reparse_or_symlink(&m)) {
            continue;
        }

        let Ok(ses_entries) = fs::read_dir(&ws_path) else {
            continue;
        };
        for ses_entry in ses_entries.flatten() {
            if !ses_entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                continue;
            }
            let dir = ses_entry.path();
            if fs::symlink_metadata(&dir).is_ok_and(|m| is_reparse_or_symlink(&m)) {
                continue;
            }
            let Some(id) = ses_entry
                .file_name()
                .to_str()
                .filter(|s| is_safe_session_id(s))
                .map(str::to_string)
            else {
                continue;
            };

            let state = read_bounded_json(&dir.join("state.json")).or_else(|| {
                let meta_dir = dir.join("session-meta");
                if fs::symlink_metadata(&meta_dir).is_ok_and(|m| is_reparse_or_symlink(&m)) {
                    None
                } else {
                    read_bounded_json(&meta_dir.join("state.json"))
                }
            });

            if let Some(ref st) = state {
                if st.pointer("/custom/session_kind").and_then(Value::as_str) == Some("child") {
                    continue;
                }
            }

            let Some(transcript) = select_transcript_file(&dir) else {
                continue;
            };

            let needs_wire_summary = state.as_ref().is_none_or(|st| {
                st.get("title")
                    .and_then(Value::as_str)
                    .is_none_or(|s| s.trim().is_empty())
                    || st
                        .get("lastPrompt")
                        .and_then(Value::as_str)
                        .is_none_or(|s| s.trim().is_empty())
                    || st.get("createdAt").is_none()
            });
            if needs_wire_summary {
                fallback_files.push(transcript.clone());
            }

            discovered.push(DiscoveredKimiSession {
                id,
                transcript,
                state,
            });
        }
    }

    let wire_summaries: HashMap<PathBuf, ParsedWireSummary> = cache
        .scan(fallback_files, parse_wire_summary)
        .into_iter()
        .map(|s| (s.source_path.clone(), s))
        .collect();

    let mut sessions = Vec::with_capacity(discovered.len());
    for item in discovered {
        let fallback = wire_summaries.get(&item.transcript);
        let archived = item
            .state
            .as_ref()
            .and_then(|st| st.get("archived"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let project_dir = item
            .state
            .as_ref()
            .and_then(|st| {
                st.get("cwd")
                    .or_else(|| st.get("workDir"))
                    .or_else(|| st.pointer("/custom/cwd"))
            })
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let project_name = project_dir.as_deref().and_then(path_basename);

        let title = item
            .state
            .as_ref()
            .and_then(|st| st.get("title"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|t| truncate_summary(t, TITLE_MAX_CHARS))
            .or_else(|| {
                fallback
                    .and_then(|f| f.first_user_prompt.as_deref())
                    .map(|t| truncate_summary(t, TITLE_MAX_CHARS))
            })
            .or_else(|| project_name.clone());

        let summary = item
            .state
            .as_ref()
            .and_then(|st| st.get("lastPrompt"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| truncate_summary(s, 160))
            .or_else(|| {
                fallback
                    .and_then(|f| f.last_visible.as_deref().or(f.first_user_prompt.as_deref()))
                    .map(|s| truncate_summary(s, 160))
            });

        let created_at = item
            .state
            .as_ref()
            .and_then(|st| st.get("createdAt"))
            .and_then(parse_timestamp_to_ms)
            .or_else(|| fallback.and_then(|f| f.created_at));
        let last_active_at = item
            .state
            .as_ref()
            .and_then(|st| st.get("updatedAt"))
            .and_then(parse_timestamp_to_ms)
            .or_else(|| fallback.and_then(|f| f.last_active_at))
            .or(created_at);

        let resume_command = build_resume_command(home, &item.id, archived);

        sessions.push(SessionMeta {
            provider_id: PROVIDER_ID.to_string(),
            session_id: item.id,
            residual: false,
            parent_session_id: None,
            archived,
            cleanup_pending: false,
            title,
            summary,
            project_dir,
            project_name,
            sidebar_section: None,
            created_at,
            last_active_at,
            source_path: Some(item.transcript.to_string_lossy().to_string()),
            resume_command,
        });
    }

    sessions
}

fn parse_wire_summary(path: &Path) -> Result<Option<ParsedWireSummary>, String> {
    let (head, tail) = read_head_tail_lines(path, HEAD_SCAN_LINES, TAIL_SCAN_LINES)
        .map_err(|err| format!("无法读取 Kimi 会话 {}：{err}", path.display()))?;
    let mut summary = ParsedWireSummary {
        source_path: path.to_path_buf(),
        ..Default::default()
    };

    for line in head.iter().chain(tail.iter()) {
        let Ok(record) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let ts = record
            .get("time")
            .or_else(|| record.get("timestamp"))
            .and_then(parse_timestamp_to_ms);
        if let Some(t) = ts {
            summary.created_at = Some(summary.created_at.map_or(t, |prev| prev.min(t)));
            summary.last_active_at = Some(summary.last_active_at.map_or(t, |prev| prev.max(t)));
        }

        // Kimi Code envelope or direct context.jsonl message or legacy TurnBegin
        let raw_msg = record
            .pointer("/message/message")
            .or_else(|| record.get("message"))
            .unwrap_or(&record);
        if let Some(role) = raw_msg.get("role").and_then(Value::as_str) {
            if matches!(role, "user" | "assistant") {
                let text = raw_msg
                    .get("content")
                    .map(extract_kimi_visible_text)
                    .unwrap_or_default();
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    if role == "user" && summary.first_user_prompt.is_none() {
                        summary.first_user_prompt = Some(trimmed.to_string());
                    }
                    summary.last_visible = Some(trimmed.to_string());
                }
            }
        } else if record.pointer("/message/type").and_then(Value::as_str) == Some("TurnBegin") {
            if let Some(input) = record.pointer("/message/payload/user_input") {
                let text = extract_kimi_visible_text(input);
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    if summary.first_user_prompt.is_none() {
                        summary.first_user_prompt = Some(trimmed.to_string());
                    }
                    summary.last_visible = Some(trimmed.to_string());
                }
            }
        }
    }

    Ok(Some(summary))
}

fn extract_kimi_visible_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("think" | "thinking" | "reasoning") => None,
                Some("text") => part.get("text").and_then(Value::as_str).map(str::to_string),
                _ => {
                    let t = extract_text(part);
                    (!t.trim().is_empty()).then_some(t)
                }
            })
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => extract_text(content),
        _ => String::new(),
    }
}

struct BranchSegment {
    start: usize,
    end: usize,
    base: Option<(String, usize)>,
}

/// Resolve `agent.switched` branch switches before projecting messages so undone/forked turns stay hidden.
fn active_record_indices(records: &[(JsonlSpan, Value)]) -> Result<Vec<usize>, String> {
    let switches: Vec<_> = records
        .iter()
        .enumerate()
        .filter(|(_, (_, record))| {
            record.get("type").and_then(Value::as_str) == Some("agent.switched")
        })
        .collect();
    if switches.is_empty() {
        return Ok((0..records.len()).collect());
    }

    let mut segments = HashMap::new();
    let mut branch = "main".to_string();
    let mut start = 0usize;
    let mut base: Option<(String, usize)> = None;

    for (index, (_, record)) in switches {
        segments.insert(
            branch,
            BranchSegment {
                start,
                end: index,
                base,
            },
        );
        branch = record
            .get("branch")
            .and_then(Value::as_str)
            .ok_or("Invalid Kimi branch")?
            .to_string();
        let base_branch = record
            .pointer("/base/branch")
            .and_then(Value::as_str)
            .ok_or("Invalid Kimi branch base")?
            .to_string();
        let base_line = record
            .pointer("/base/line")
            .and_then(Value::as_u64)
            .ok_or("Invalid Kimi branch line")? as usize;
        base = Some((base_branch, base_line));
        start = index + 1;
    }
    segments.insert(
        branch.clone(),
        BranchSegment {
            start,
            end: records.len(),
            base,
        },
    );

    let mut chain = Vec::new();
    let mut limit = records.len();
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(branch.clone()) {
            return Err("Kimi branch chain contains a cycle".into());
        }
        let segment = segments.get(&branch).ok_or("Unknown Kimi branch")?;
        chain.push((segment.start, segment.end.min(limit)));
        if let Some((parent, line)) = &segment.base {
            branch = parent.clone();
            limit = *line;
        } else {
            break;
        }
    }

    let mut active = Vec::new();
    for (seg_start, seg_end) in chain.into_iter().rev() {
        if seg_start < seg_end {
            active.extend(seg_start..seg_end);
        }
    }
    Ok(active)
}

#[derive(Default)]
struct RequestIndex {
    step_to_turn_step: HashMap<String, String>,
    turn_step_to_model: HashMap<String, String>,
    last_loop_model: Option<String>,
    msg_step_and_latency: HashMap<String, (Option<String>, Option<u64>)>,
}

impl RequestIndex {
    fn observe(&mut self, record: &Value) {
        match record.get("type").and_then(Value::as_str) {
            Some("context.append_loop_event") => {
                let event = &record["event"];
                match event.get("type").and_then(Value::as_str) {
                    Some("step.begin") => {
                        if let (Some(uuid), Some(turn_id), Some(step)) = (
                            event.get("uuid").and_then(Value::as_str),
                            event.get("turnId").and_then(Value::as_str),
                            event.get("step").and_then(Value::as_u64),
                        ) {
                            self.step_to_turn_step
                                .insert(uuid.to_string(), format!("{turn_id}.{step}"));
                        }
                    }
                    Some("step.end") => {
                        if let Some(msg_id) = event.get("messageId").and_then(Value::as_str) {
                            let step_uuid = event
                                .get("uuid")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                            let first = event
                                .get("llmFirstTokenLatencyMs")
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                            let stream = event
                                .get("llmStreamDurationMs")
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                            let total = first.saturating_add(stream);
                            self.msg_step_and_latency.insert(
                                msg_id.to_string(),
                                (step_uuid, (total > 0).then_some(total)),
                            );
                        }
                    }
                    _ => {}
                }
            }
            Some("llm.request") if record.get("kind").and_then(Value::as_str) == Some("loop") => {
                if let Some(model) = record
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    if let Some(turn_step) = record.get("turnStep").and_then(Value::as_str) {
                        self.turn_step_to_model
                            .insert(turn_step.to_string(), model.to_string());
                    }
                    if record
                        .get("agentId")
                        .and_then(Value::as_str)
                        .is_none_or(|id| id == "main")
                    {
                        self.last_loop_model = Some(model.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    fn model(&self, meta: &Value) -> Option<String> {
        let provider = meta.pointer("/model/provider").and_then(Value::as_str);
        if provider != Some("agent-loop") {
            if let Some(m) = meta
                .pointer("/model/model")
                .or_else(|| meta.get("model"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty() && *s != "agent-loop")
            {
                return Some(m.to_string());
            }
        }
        if let Some(msg_id) = meta.get("messageId").and_then(Value::as_str) {
            if let Some((Some(step_uuid), _)) = self.msg_step_and_latency.get(msg_id) {
                if let Some(turn_step) = self.step_to_turn_step.get(step_uuid) {
                    if let Some(model) = self.turn_step_to_model.get(turn_step) {
                        return Some(model.clone());
                    }
                }
            }
        }
        self.last_loop_model.clone()
    }

    fn duration_ms(&self, meta: &Value) -> Option<u64> {
        let msg_id = meta.get("messageId").and_then(Value::as_str)?;
        self.msg_step_and_latency
            .get(msg_id)
            .and_then(|(_, latency)| *latency)
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

fn kimi_image_ref(
    part: &Value,
    part_ptr: &str,
    ref_fn: &dyn Fn(String) -> Option<ContentRef>,
) -> Option<ImageRef> {
    let (raw_url, url_ptr) = if let Some(u) = part
        .pointer("/image_url/url")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        (u.trim(), format!("{part_ptr}/image_url/url"))
    } else if let Some(u) = part
        .get("url")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        (u.trim(), format!("{part_ptr}/url"))
    } else {
        return None;
    };

    if let Some(rest) = raw_url.strip_prefix("data:") {
        let (header, data) = rest.split_once(',')?;
        let mime = header
            .split(';')
            .next()
            .filter(|m| m.starts_with("image/"))
            .unwrap_or("image/png");
        let content = ref_fn(url_ptr)?;
        return Some(ImageRef {
            source: ImageSource::Inline { content },
            media_type: mime.to_string(),
            size: estimate_base64_size(data.len()),
            alt: None,
        });
    }

    let fs_path = if raw_url.starts_with("file://") {
        parse_file_url(raw_url).ok()?
    } else {
        let p = PathBuf::from(raw_url);
        p.is_absolute().then_some(p)?
    };
    let media_type = image_media_type_from_path(&fs_path)?;
    let size = fs::metadata(&fs_path)
        .ok()
        .map(|m| u32::try_from(m.len()).unwrap_or(u32::MAX))
        .unwrap_or(0);
    Some(ImageRef {
        source: ImageSource::LocalFile {
            path: raw_url.to_string(),
        },
        media_type: media_type.to_string(),
        size,
        alt: None,
    })
}

fn build_kimi_meta(meta: Option<&Value>, requests: &RequestIndex) -> Option<MessageMeta> {
    let meta = meta?;
    let usage = meta.get("usage")?;
    let u64_of = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| usage.get(*k).and_then(Value::as_u64))
    };
    MessageMeta {
        model: requests.model(meta),
        input_tokens: u64_of(&["inputOther", "input_other", "input_tokens"]),
        output_tokens: u64_of(&["output", "output_tokens"]),
        cache_read_tokens: u64_of(&["inputCacheRead", "input_cache_read"]),
        cache_write_tokens: u64_of(&["inputCacheCreation", "input_cache_creation"]),
        duration_ms: requests.duration_ms(meta),
        ..MessageMeta::default()
    }
    .into_option()
}

fn emit_chat_message_blocks(
    raw: &Value,
    pointer: &str,
    meta: Option<&Value>,
    ts: Option<i64>,
    requests: &RequestIndex,
    ref_fn: &dyn Fn(String) -> Option<ContentRef>,
    messages: &mut Vec<SessionMessage>,
) {
    let Some(role @ ("user" | "assistant" | "tool" | "system")) =
        raw.get("role").and_then(Value::as_str)
    else {
        return;
    };

    let origin_kind = meta
        .and_then(|m| m.pointer("/origin/kind"))
        .or_else(|| raw.pointer("/origin/kind"))
        .and_then(Value::as_str);
    let injected = role == "system"
        || matches!(
            origin_kind,
            Some("injection" | "system_trigger" | "retry" | "compaction_summary")
        );

    if origin_kind == Some("compaction_summary") {
        let text = raw
            .get("content")
            .map(extract_kimi_visible_text)
            .unwrap_or_default();
        if !text.trim().is_empty() {
            let content_ptr = if pointer.is_empty() {
                "/content".to_string()
            } else {
                format!("{pointer}/content")
            };
            let block = summary_event_block(EventKind::Compaction, &text, || ref_fn(content_ptr));
            let mut msg = SessionMessage::from_blocks("system", ts, vec![block]);
            msg.injected = true;
            messages.push(msg);
        }
        return;
    }

    let mut message_blocks = Vec::new();
    let base_prefix = if pointer.is_empty() {
        String::new()
    } else {
        pointer.to_string()
    };

    if role == "tool" {
        let call_id = raw
            .get("toolCallId")
            .or_else(|| raw.get("tool_call_id"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let status = if raw.get("isError").and_then(Value::as_bool) == Some(true)
            || raw.get("is_error").and_then(Value::as_bool) == Some(true)
        {
            ToolStatus::Error
        } else {
            ToolStatus::Success
        };
        let text = raw.get("content").map(extract_text).unwrap_or_default();
        let content_ptr = format!("{base_prefix}/content");
        message_blocks.push(tool_result_block(call_id, status, &text, || {
            ref_fn(content_ptr)
        }));
    } else {
        match raw.get("content") {
            Some(Value::String(text)) if !text.trim().is_empty() => {
                if injected {
                    let ptr = format!("{base_prefix}/content");
                    message_blocks.push(large_text_block(text.clone(), || ref_fn(ptr)));
                } else {
                    message_blocks.push(SessionBlock::text(text));
                }
            }
            Some(Value::Array(parts)) => {
                for (i, part) in parts.iter().enumerate() {
                    let part_ptr = format!("{base_prefix}/content/{i}");
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    if injected {
                                        let ptr = format!("{part_ptr}/text");
                                        message_blocks
                                            .push(large_text_block(text.to_string(), || {
                                                ref_fn(ptr)
                                            }));
                                    } else {
                                        message_blocks.push(SessionBlock::text(text));
                                    }
                                }
                            }
                        }
                        Some("think" | "thinking" | "reasoning") => {
                            let field = if part.get("think").is_some() {
                                "think"
                            } else if part.get("thinking").is_some() {
                                "thinking"
                            } else {
                                "text"
                            };
                            message_blocks.push(thinking_block(
                                part.get(field).and_then(Value::as_str).unwrap_or_default(),
                                None,
                                None,
                                || ref_fn(format!("{part_ptr}/{field}")),
                            ));
                        }
                        Some("image_url") => {
                            if let Some(image) = kimi_image_ref(part, &part_ptr, ref_fn) {
                                message_blocks.push(SessionBlock::Image { image });
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }

        let (calls_key, calls_opt) = if let Some(c) = raw.get("toolCalls") {
            ("toolCalls", c.as_array())
        } else if let Some(c) = raw.get("tool_calls") {
            ("tool_calls", c.as_array())
        } else {
            ("toolCalls", None)
        };
        for (i, call) in calls_opt.into_iter().flatten().enumerate() {
            let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
            let name = call
                .pointer("/function/name")
                .or_else(|| call.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let (args_ptr, raw_args) = if let Some(a) = call.pointer("/function/arguments") {
                (
                    format!("{base_prefix}/{calls_key}/{i}/function/arguments"),
                    a,
                )
            } else if let Some(a) = call.get("arguments") {
                (format!("{base_prefix}/{calls_key}/{i}/arguments"), a)
            } else {
                (
                    format!("{base_prefix}/{calls_key}/{i}/arguments"),
                    &Value::Null,
                )
            };
            let input = parse_arguments(raw_args);
            message_blocks.push(tool_call_block(
                ToolSource::Generic,
                id,
                name,
                &input,
                || ref_fn(args_ptr),
            ));
        }
    }

    if message_blocks.is_empty() {
        return;
    }

    let mut message = SessionMessage::from_blocks(role, ts, message_blocks);
    message.id = meta
        .and_then(|m| m.get("messageId"))
        .or_else(|| raw.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    message.injected = injected;
    if role == "assistant" {
        message.meta = build_kimi_meta(meta, requests);
    }
    messages.push(message);
}

#[derive(Default)]
struct WireStepBuilder {
    ts: Option<i64>,
    id: Option<String>,
    think_chunks: Vec<String>,
    single_think_ptr: Option<(JsonlSpan, String)>,
    text_chunks: Vec<String>,
    blocks: Vec<SessionBlock>,
    call_slots: Vec<(String, usize)>,
    pending_meta: Option<MessageMeta>,
}

impl WireStepBuilder {
    fn ensure_ts(&mut self, ts: Option<i64>) {
        if self.ts.is_none() {
            self.ts = ts;
        }
    }

    fn flush_think(&mut self, can_emit_refs: bool) {
        if self.think_chunks.is_empty() {
            return;
        }
        let joined = self.think_chunks.concat();
        self.think_chunks.clear();
        let single_ptr = self.single_think_ptr.take();
        if !joined.trim().is_empty() {
            self.blocks.push(thinking_block(&joined, None, None, || {
                if can_emit_refs {
                    single_ptr.map(|(span, ptr)| span.content_ref(ptr))
                } else {
                    None
                }
            }));
        }
    }

    fn flush_text(&mut self) {
        if self.text_chunks.is_empty() {
            return;
        }
        let joined = self.text_chunks.concat();
        self.text_chunks.clear();
        if !joined.trim().is_empty() {
            self.blocks.push(SessionBlock::text(joined));
        }
    }

    fn push_think(&mut self, text: &str, span: JsonlSpan, ptr: String, ts: Option<i64>) {
        self.ensure_ts(ts);
        self.flush_text();
        if self.think_chunks.is_empty() {
            self.single_think_ptr = Some((span, ptr));
        } else {
            self.single_think_ptr = None;
        }
        self.think_chunks.push(text.to_string());
    }

    fn push_text(&mut self, text: &str, can_emit_refs: bool, ts: Option<i64>) {
        self.ensure_ts(ts);
        self.flush_think(can_emit_refs);
        self.text_chunks.push(text.to_string());
    }

    fn push_block(&mut self, block: SessionBlock, can_emit_refs: bool, ts: Option<i64>) {
        self.ensure_ts(ts);
        self.flush_think(can_emit_refs);
        self.flush_text();
        if let SessionBlock::ToolCall { ref id, .. } = block {
            if !id.is_empty() {
                self.call_slots.push((id.clone(), self.blocks.len()));
            }
        }
        self.blocks.push(block);
    }

    fn finish_into(
        &mut self,
        can_emit_refs: bool,
        messages: &mut Vec<SessionMessage>,
        call_locations: &mut HashMap<String, (usize, usize)>,
    ) {
        self.flush_think(can_emit_refs);
        self.flush_text();
        let blocks = std::mem::take(&mut self.blocks);
        let meta = self.pending_meta.take();
        let ts = self.ts.take();
        let id = self.id.take();
        let slots = std::mem::take(&mut self.call_slots);

        if blocks.is_empty() {
            if let Some(m) = meta {
                if let Some(last_assistant) = messages
                    .iter_mut()
                    .rev()
                    .find(|msg| msg.role == "assistant")
                {
                    if last_assistant.meta.is_none() {
                        last_assistant.meta = Some(m);
                    }
                    if last_assistant.id.is_none() {
                        last_assistant.id = id;
                    }
                }
            }
            return;
        }

        let msg_idx = messages.len();
        for (cid, bidx) in slots {
            call_locations.insert(cid, (msg_idx, bidx));
        }
        let mut msg = SessionMessage::from_blocks("assistant", ts, blocks);
        msg.id = id;
        msg.meta = meta;
        messages.push(msg);
    }
}

fn parse_status_update_meta(payload: &Value) -> Option<MessageMeta> {
    let usage = payload
        .get("token_usage")
        .or_else(|| payload.get("tokenUsage"))
        .or_else(|| payload.get("usage"))?;
    let u64_of = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| usage.get(*k).and_then(Value::as_u64))
    };
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    MessageMeta {
        model,
        input_tokens: u64_of(&["input_other", "inputOther", "input_tokens"]),
        output_tokens: u64_of(&["output", "output_tokens"]),
        cache_read_tokens: u64_of(&["input_cache_read", "inputCacheRead"]),
        cache_write_tokens: u64_of(&["input_cache_creation", "inputCacheCreation"]),
        ..MessageMeta::default()
    }
    .into_option()
}

fn enrich_tool_call_from_return_display(return_value: &Value, call_block: &mut SessionBlock) {
    let SessionBlock::ToolCall {
        diff: diff_slot,
        detail: detail_slot,
        title,
        ..
    } = call_block
    else {
        return;
    };

    let mut diff_files: Vec<DiffFile> = Vec::new();
    let mut total_added = 0u32;
    let mut total_removed = 0u32;
    let mut brief_text: Option<String> = return_value
        .get("brief")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    for item in return_value
        .get("display")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("diff") => {
                let path = item
                    .get("path")
                    .or_else(|| item.get("file_path"))
                    .and_then(Value::as_str)
                    .unwrap_or(title.as_str())
                    .to_string();
                let (added, removed, op) = if let (Some(old_t), Some(new_t)) = (
                    item.get("old_text").and_then(Value::as_str),
                    item.get("new_text").and_then(Value::as_str),
                ) {
                    let (a, r) = line_change_counts(old_t, new_t);
                    let op = if old_t.is_empty() && !new_t.is_empty() {
                        DiffOp::Add
                    } else if !old_t.is_empty() && new_t.is_empty() {
                        DiffOp::Delete
                    } else {
                        DiffOp::Update
                    };
                    (a, r, op)
                } else if let Some(raw_diff) = item.get("diff").and_then(Value::as_str) {
                    let (a, r) = count_diff_lines(raw_diff);
                    (a, r, DiffOp::Update)
                } else {
                    continue;
                };
                total_added = total_added.saturating_add(added);
                total_removed = total_removed.saturating_add(removed);
                diff_files.push(DiffFile {
                    path,
                    op,
                    added,
                    removed,
                });
            }
            Some("brief") if brief_text.is_none() => {
                brief_text = item
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
            }
            _ => {}
        }
    }

    if !diff_files.is_empty() {
        *diff_slot = Some(DiffSummary {
            files: diff_files,
            added: total_added,
            removed: total_removed,
            full: None,
        });
    }
    if detail_slot.is_none() {
        if let Some(brief) = brief_text {
            let formatted = one_line_title(&brief);
            if !formatted.is_empty() && formatted != *title {
                *detail_slot = Some(formatted);
            }
        }
    }
}

fn resolve_transcript_for_load(path: &Path) -> (PathBuf, bool) {
    if path.is_file() {
        return (path.to_path_buf(), true);
    }
    if path.file_name() == Some(OsStr::new("wire.jsonl")) {
        let mut candidates = vec![path.with_file_name("context.jsonl")];
        if let Some(session_dir) = path.ancestors().nth(3) {
            if path.ends_with(Path::new("agents").join("main").join("wire.jsonl")) {
                candidates.push(session_dir.join("context.jsonl"));
            }
        }
        for candidate in candidates {
            if fs::symlink_metadata(&candidate)
                .is_ok_and(|m| !is_reparse_or_symlink(&m) && m.file_type().is_file())
            {
                return (candidate, false);
            }
        }
    }
    (path.to_path_buf(), true)
}

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    let (target_path, can_emit_refs) = resolve_transcript_for_load(path);
    ensure_readable_size(&target_path)?;
    let file =
        File::open(&target_path).map_err(|e| format!("Failed to open Kimi session file: {e}"))?;
    let mut lines = LineSpans::new(BufReader::new(file));

    // Preserve physical 0-based line indices (including blank/malformed lines) for `agent.switched` `base.line`.
    let mut records: Vec<(JsonlSpan, Value)> = Vec::new();
    while let Some(line) = lines
        .next_line()
        .map_err(|e| format!("Failed to read Kimi transcript: {e}"))?
    {
        records.push((
            line.span,
            serde_json::from_slice::<Value>(line.bytes).unwrap_or(Value::Null),
        ));
    }

    let active = active_record_indices(&records)?;
    let mut requests = RequestIndex::default();
    for &idx in &active {
        requests.observe(&records[idx].1);
    }

    let has_human = active.iter().any(|&idx| {
        matches!(
            records[idx].1.get("type").and_then(Value::as_str),
            Some("agent.message.appended" | "human.agent.message.appended")
        )
    });
    let has_context_append = active.iter().any(|&idx| {
        records[idx].1.get("type").and_then(Value::as_str) == Some("context.append_message")
    });

    let mut messages: Vec<SessionMessage> = Vec::new();
    let mut step_builder = WireStepBuilder::default();
    let mut wire_call_locations: HashMap<String, (usize, usize)> = HashMap::new();

    for idx in active {
        let (span, record) = &records[idx];
        let ts = record
            .get("time")
            .or_else(|| record.get("timestamp"))
            .and_then(parse_timestamp_to_ms);
        let kind = record.get("type").and_then(Value::as_str).unwrap_or("");
        let ref_fn = |pointer: String| -> Option<ContentRef> {
            can_emit_refs.then(|| span.content_ref(pointer))
        };

        if has_human
            && matches!(
                kind,
                "agent.message.appended" | "human.agent.message.appended"
            )
        {
            if let Some(raw) = record.pointer("/message/message") {
                emit_chat_message_blocks(
                    raw,
                    "/message/message",
                    record.pointer("/message/meta"),
                    ts,
                    &requests,
                    &ref_fn,
                    &mut messages,
                );
            }
            continue;
        }

        if !has_human && kind == "context.append_message" {
            if let Some(raw) = record.get("message") {
                emit_chat_message_blocks(
                    raw,
                    "/message",
                    record.get("meta"),
                    ts,
                    &requests,
                    &ref_fn,
                    &mut messages,
                );
            }
            continue;
        }

        if has_human || has_context_append {
            continue;
        }

        // Python `kimi-cli` context.jsonl (`role` at top-level) or wire.jsonl (`message.type` / top-level wire events)
        if record.get("role").is_some() {
            step_builder.finish_into(can_emit_refs, &mut messages, &mut wire_call_locations);
            emit_chat_message_blocks(record, "", None, ts, &requests, &ref_fn, &mut messages);
            continue;
        }

        let (msg_type, payload, payload_ptr) =
            if let Some(mt) = record.pointer("/message/type").and_then(Value::as_str) {
                if record.pointer("/message/payload").is_some() {
                    (mt, &record["message"]["payload"], "/message/payload")
                } else {
                    (mt, &record["message"], "/message")
                }
            } else if !kind.is_empty() && kind != "metadata" {
                if record.get("payload").is_some() {
                    (kind, &record["payload"], "/payload")
                } else {
                    (kind, record, "")
                }
            } else {
                continue;
            };

        match msg_type {
            "TurnBegin" => {
                step_builder.finish_into(can_emit_refs, &mut messages, &mut wire_call_locations);
                let mut user_blocks = Vec::new();
                match payload.get("user_input") {
                    Some(Value::String(s)) if !s.trim().is_empty() => {
                        user_blocks.push(SessionBlock::text(s));
                    }
                    Some(Value::Array(parts)) => {
                        for (i, part) in parts.iter().enumerate() {
                            let part_ptr = format!("{payload_ptr}/user_input/{i}");
                            match part.get("type").and_then(Value::as_str) {
                                Some("text") => {
                                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                                        if !t.trim().is_empty() {
                                            user_blocks.push(SessionBlock::text(t));
                                        }
                                    }
                                }
                                Some("image_url") => {
                                    if let Some(image) = kimi_image_ref(part, &part_ptr, &ref_fn) {
                                        user_blocks.push(SessionBlock::Image { image });
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                if !user_blocks.is_empty() {
                    messages.push(SessionMessage::from_blocks("user", ts, user_blocks));
                }
            }
            "StepBegin" | "TurnEnd" => {
                step_builder.finish_into(can_emit_refs, &mut messages, &mut wire_call_locations);
            }
            "StepInterrupted" => {
                step_builder.push_block(
                    SessionBlock::event(EventKind::Aborted, None, None),
                    can_emit_refs,
                    ts,
                );
                step_builder.finish_into(can_emit_refs, &mut messages, &mut wire_call_locations);
            }
            "ThinkPart" => {
                if let Some(t) = payload.get("think").and_then(Value::as_str) {
                    step_builder.push_think(t, *span, format!("{payload_ptr}/think"), ts);
                }
            }
            "TextPart" => {
                if let Some(t) = payload.get("text").and_then(Value::as_str) {
                    step_builder.push_text(t, can_emit_refs, ts);
                }
            }
            "ImageURLPart" => {
                if let Some(image) = kimi_image_ref(payload, payload_ptr, &ref_fn) {
                    step_builder.push_block(SessionBlock::Image { image }, can_emit_refs, ts);
                }
            }
            "ContentPart" => match payload.get("type").and_then(Value::as_str) {
                Some("think") => {
                    if let Some(t) = payload.get("think").and_then(Value::as_str) {
                        step_builder.push_think(t, *span, format!("{payload_ptr}/think"), ts);
                    }
                }
                Some("text") => {
                    if let Some(t) = payload.get("text").and_then(Value::as_str) {
                        step_builder.push_text(t, can_emit_refs, ts);
                    }
                }
                Some("image_url") => {
                    if let Some(image) = kimi_image_ref(payload, payload_ptr, &ref_fn) {
                        step_builder.push_block(SessionBlock::Image { image }, can_emit_refs, ts);
                    }
                }
                _ => {}
            },
            "ToolCall" => {
                let id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let name = payload
                    .pointer("/function/name")
                    .or_else(|| payload.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !name.is_empty() {
                    let (args_ptr, raw_args) =
                        if let Some(a) = payload.pointer("/function/arguments") {
                            (format!("{payload_ptr}/function/arguments"), a)
                        } else if let Some(a) = payload.get("arguments") {
                            (format!("{payload_ptr}/arguments"), a)
                        } else {
                            (format!("{payload_ptr}/arguments"), &Value::Null)
                        };
                    let input = parse_arguments(raw_args);
                    let block =
                        tool_call_block(ToolSource::Generic, id, name, &input, || ref_fn(args_ptr));
                    step_builder.push_block(block, can_emit_refs, ts);
                }
            }
            "StatusUpdate" => {
                if let Some(msg_id) = payload
                    .get("message_id")
                    .or_else(|| payload.get("messageId"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    step_builder.id = Some(msg_id.to_string());
                }
                if let Some(meta) = parse_status_update_meta(payload) {
                    step_builder.pending_meta = Some(meta);
                }
            }
            "ToolResult" => {
                step_builder.finish_into(can_emit_refs, &mut messages, &mut wire_call_locations);
                let call_id = payload
                    .get("tool_call_id")
                    .or_else(|| payload.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let return_val = &payload["return_value"];
                if let Some(&(msg_idx, blk_idx)) = wire_call_locations.get(call_id) {
                    if let Some(call_block) = messages
                        .get_mut(msg_idx)
                        .and_then(|m| m.blocks.get_mut(blk_idx))
                    {
                        enrich_tool_call_from_return_display(return_val, call_block);
                    }
                }
                let is_error = return_val.get("is_error").and_then(Value::as_bool) == Some(true);
                let (out_text, out_ptr) = if let Some(out) =
                    return_val.get("output").filter(|v| !v.is_null())
                {
                    let t = extract_text(out);
                    if !t.is_empty() {
                        (t, format!("{payload_ptr}/return_value/output"))
                    } else if let Some(msg) = return_val.get("message").and_then(Value::as_str) {
                        (
                            msg.to_string(),
                            format!("{payload_ptr}/return_value/message"),
                        )
                    } else {
                        (String::new(), format!("{payload_ptr}/return_value/output"))
                    }
                } else if let Some(msg) = return_val.get("message").and_then(Value::as_str) {
                    (
                        msg.to_string(),
                        format!("{payload_ptr}/return_value/message"),
                    )
                } else {
                    (String::new(), format!("{payload_ptr}/return_value/output"))
                };
                let block = tool_result_block(
                    call_id,
                    if is_error {
                        ToolStatus::Error
                    } else {
                        ToolStatus::Success
                    },
                    &out_text,
                    || ref_fn(out_ptr),
                );
                messages.push(SessionMessage::from_blocks("tool", ts, vec![block]));
            }
            "CompactionBegin" => {
                step_builder.finish_into(can_emit_refs, &mut messages, &mut wire_call_locations);
                let summary = payload
                    .get("summary")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let block = match summary {
                    Some(s) => summary_event_block(EventKind::Compaction, s, || {
                        ref_fn(format!("{payload_ptr}/summary"))
                    }),
                    None => SessionBlock::event(EventKind::Compaction, None, None),
                };
                let mut msg = SessionMessage::from_blocks("system", ts, vec![block]);
                msg.injected = true;
                messages.push(msg);
            }
            _ => {}
        }
    }

    step_builder.finish_into(can_emit_refs, &mut messages, &mut wire_call_locations);
    assign_turn_ids(&mut messages);
    Ok(messages)
}

fn validate_kimi_delete(
    root: &Path,
    path: &Path,
    session_id: &str,
) -> Result<(PathBuf, PathBuf), String> {
    if !is_safe_session_id(session_id) {
        return Err(format!("Invalid Kimi session ID: {session_id}"));
    }
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(format!("Invalid Kimi session path: {}", path.display()));
    }

    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                return Err(format!(
                    "Kimi session source must not be a symlink or reparse point: {}",
                    path.display()
                ));
            }
            if !meta.file_type().is_file() {
                return Err(format!(
                    "Kimi session source is not a regular file: {}",
                    path.display()
                ));
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "Failed to inspect Kimi session {}: {err}",
                path.display()
            ))
        }
    }

    let canonical_root = root.canonicalize().map_err(|err| {
        format!(
            "Failed to resolve Kimi sessions root {}: {err}",
            root.display()
        )
    })?;
    let root_meta = fs::metadata(&canonical_root).map_err(|err| {
        format!(
            "Failed to inspect Kimi sessions root {}: {err}",
            canonical_root.display()
        )
    })?;
    if !root_meta.is_dir() {
        return Err(format!(
            "Kimi sessions root is not a directory: {}",
            canonical_root.display()
        ));
    }

    let resolved_path = checked_storage_child(root, path)?;
    let relative = resolved_path.strip_prefix(&canonical_root).map_err(|_| {
        format!(
            "Kimi session path is outside sessions root: {}",
            path.display()
        )
    })?;
    let parts: Vec<&OsStr> = relative
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    if parts.len() != relative.components().count() || parts.len() < 3 {
        return Err(format!(
            "Unexpected Kimi session layout: {}",
            path.display()
        ));
    }

    let ws_name = parts[0]
        .to_str()
        .ok_or_else(|| format!("Invalid Kimi workspace folder: {}", path.display()))?;
    if ws_name.is_empty() || ws_name.starts_with('.') {
        return Err(format!(
            "Refusing to target Kimi internal directory: {}",
            path.display()
        ));
    }
    if parts[1] != OsStr::new(session_id) {
        return Err(format!(
            "Kimi session directory does not match session ID ({session_id}): {}",
            path.display()
        ));
    }

    let valid_tail = (parts.len() == 5
        && parts[2..]
            == [
                OsStr::new("agents"),
                OsStr::new("main"),
                OsStr::new("wire.jsonl"),
            ])
        || (parts.len() == 3
            && (parts[2] == OsStr::new("wire.jsonl") || parts[2] == OsStr::new("context.jsonl")));
    if !valid_tail {
        return Err(format!(
            "Unexpected Kimi session transcript path: {}",
            path.display()
        ));
    }

    let session_dir = canonical_root.join(ws_name).join(session_id);
    if let Ok(meta) = fs::symlink_metadata(&session_dir) {
        if is_reparse_or_symlink(&meta) || !meta.file_type().is_dir() {
            return Err(format!(
                "Kimi session directory is not a regular directory: {}",
                session_dir.display()
            ));
        }
    }
    let checked_session_dir = checked_storage_child(&canonical_root, &session_dir)?;

    Ok((canonical_root, checked_session_dir))
}

#[allow(dead_code)]
pub fn preflight_delete(root: &Path, path: &Path, session_id: &str) -> Result<(), String> {
    validate_kimi_delete(root, path, session_id).map(|_| ())
}

pub fn delete_session(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    let (canonical_root, session_dir) = validate_kimi_delete(root, path, session_id)?;
    let mut removed = false;

    if fs::symlink_metadata(&session_dir).is_ok() {
        remove_path_if_exists(&session_dir).map_err(|err| {
            format!(
                "Failed to delete Kimi session directory {}: {err}",
                session_dir.display()
            )
        })?;
        removed = true;
    }

    if let Some(ws_dir) = session_dir.parent() {
        let _ = fs::remove_dir(ws_dir);
    }

    if removed {
        if let Err(err) = record_kimi_deletion_markers(&canonical_root, session_id) {
            log::warn!("Failed to update Kimi session index markers after delete: {err}");
        }
    }

    Ok(removed)
}

fn record_kimi_deletion_markers(
    canonical_sessions_root: &Path,
    session_id: &str,
) -> Result<(), String> {
    if let Some(home) = canonical_sessions_root.parent() {
        let index_path = home.join("session_index.jsonl");
        let safe_to_append = match fs::symlink_metadata(&index_path) {
            Ok(meta) => !is_reparse_or_symlink(&meta) && meta.file_type().is_file(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        };
        if safe_to_append {
            let line = serde_json::json!({ "sessionId": session_id, "deleted": true });
            if let Ok(mut file) = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&index_path)
            {
                let _ = writeln!(file, "{line}");
            }
        }
    }

    let dirty_dir = canonical_sessions_root.join(".index-dirty");
    let dirty_ok = match fs::symlink_metadata(&dirty_dir) {
        Ok(meta) => !is_reparse_or_symlink(&meta) && meta.file_type().is_dir(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(&dirty_dir).is_ok()
        }
        Err(_) => false,
    };
    if dirty_ok {
        let mark = dirty_dir.join(format!(
            "{session_id}.{}",
            chrono::Utc::now().timestamp_millis()
        ));
        let _ = fs::write(mark, b"");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[cfg(unix)]
    fn create_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
    }

    #[test]
    #[serial_test::serial]
    #[allow(deprecated)]
    fn scans_kimi_state_skips_child_sessions_and_formats_resume() {
        let prev_share = std::env::var_os("KIMI_SHARE_DIR");
        let prev_code = std::env::var_os("KIMI_CODE_HOME");
        std::env::remove_var("KIMI_SHARE_DIR");
        std::env::remove_var("KIMI_CODE_HOME");

        let home = tempdir().unwrap();
        let dir = home.path().join("sessions/wd_project_123/session-1");
        fs::create_dir_all(dir.join("agents/main")).unwrap();
        fs::write(
            dir.join("state.json"),
            json!({
                "title": "Project Refactor",
                "lastPrompt": "Fix auth flow",
                "cwd": "/work/kimi-proj",
                "createdAt": 1700000000000i64,
                "updatedAt": 1700000005000i64
            })
            .to_string(),
        )
        .unwrap();
        fs::write(dir.join("agents/main/wire.jsonl"), "{}\n").unwrap();

        // Child subagent session must be skipped
        let child_dir = home.path().join("sessions/wd_project_123/session-child");
        fs::create_dir_all(child_dir.join("agents/main")).unwrap();
        fs::write(
            child_dir.join("state.json"),
            json!({"title": "Child", "custom": {"session_kind": "child"}}).to_string(),
        )
        .unwrap();
        fs::write(child_dir.join("agents/main/wire.jsonl"), "{}\n").unwrap();

        let cache = FileParseCache::new();
        let sessions = scan_sessions_in_home(home.path(), &cache);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.provider_id, "kimi");
        assert_eq!(s.session_id, "session-1");
        assert_eq!(s.title.as_deref(), Some("Project Refactor"));
        assert_eq!(s.summary.as_deref(), Some("Fix auth flow"));
        assert_eq!(s.project_dir.as_deref(), Some("/work/kimi-proj"));
        assert_eq!(s.project_name.as_deref(), Some("kimi-proj"));
        assert_eq!(
            s.resume_command.as_deref(),
            Some("kimi --session session-1")
        );

        match prev_share {
            Some(v) => std::env::set_var("KIMI_SHARE_DIR", v),
            None => std::env::remove_var("KIMI_SHARE_DIR"),
        }
        match prev_code {
            Some(v) => std::env::set_var("KIMI_CODE_HOME", v),
            None => std::env::remove_var("KIMI_CODE_HOME"),
        }
    }

    #[test]
    fn load_messages_resolves_branch_switches_and_separates_think_and_tools() {
        let temp = tempdir().unwrap();
        let wire = temp.path().join("wire.jsonl");
        let lines = [
            json!({"type":"metadata","protocol_version":"1.5"}),
            json!({"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"duplicate"}]}}),
            json!({"type":"agent.message.appended","time":1000,"message":{"message":{"role":"user","content":[{"type":"text","text":"Keep prompt"}]}}}),
            json!({"type":"agent.message.appended","time":1001,"message":{"message":{"role":"user","content":[{"type":"text","text":"Abandoned prompt"}]}}}),
            json!({"type":"agent.switched","branch":"fork1","base":{"branch":"main","line":3}}),
            json!({"type":"context.append_loop_event","event":{"type":"step.begin","uuid":"step1","turnId":"0","step":1}}),
            json!({"type":"llm.request","agentId":"main","kind":"loop","turnStep":"0.1","model":"kimi-k2"}),
            json!({"type":"agent.message.appended","time":1002,"message":{
                "message":{
                    "role":"assistant",
                    "content":[
                        {"type":"think","think":"Let me check main.rs"},
                        {"type":"text","text":"Reading file"}
                    ],
                    "toolCalls":[{"id":"tc1","name":"read_file","arguments":{"path":"main.rs"}}]
                },
                "meta":{
                    "messageId":"resp1",
                    "model":{"provider":"agent-loop","model":"agent-loop"},
                    "usage":{"inputOther":10,"output":20,"inputCacheRead":30,"inputCacheCreation":0}
                }
            }}),
            json!({"type":"context.append_loop_event","event":{"type":"step.end","uuid":"step1","messageId":"resp1","llmFirstTokenLatencyMs":300,"llmStreamDurationMs":700}}),
            json!({"type":"agent.message.appended","time":1003,"message":{"message":{
                "role":"tool",
                "toolCallId":"tc1",
                "content":[{"type":"text","text":"fn main() {}"}]
            }}}),
            json!({"type":"agent.message.appended","time":1004,"message":{"message":{"role":"user","content":[{"type":"text","text":"injected"}]},"meta":{"origin":{"kind":"injection"}}}}),
            json!({"type":"agent.message.appended","time":1005,"message":{"message":{"role":"assistant","content":[{"type":"text","text":"All set."}]}}}),
        ];
        fs::write(
            &wire,
            lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
        )
        .unwrap();

        let msgs = load_messages(&wire).unwrap();
        let roles: Vec<(&str, bool)> = msgs.iter().map(|m| (m.role.as_str(), m.injected)).collect();
        assert_eq!(
            roles,
            vec![
                ("user", false),
                ("assistant", false),
                ("tool", false),
                ("user", true),
                ("assistant", false),
            ]
        );
        assert_eq!(msgs[0].content, "Keep prompt");
        assert!(matches!(
            &msgs[1].blocks[0],
            SessionBlock::Thinking { text, .. } if text == "Let me check main.rs"
        ));
        assert!(matches!(
            &msgs[1].blocks[1],
            SessionBlock::Text { text, .. } if text == "Reading file"
        ));
        assert!(matches!(
            &msgs[1].blocks[2],
            SessionBlock::ToolCall { id, raw_name, title, .. }
                if id == "tc1" && raw_name == "read_file" && title == "main.rs"
        ));
        let meta = msgs[1].meta.as_ref().unwrap();
        assert_eq!(meta.model.as_deref(), Some("kimi-k2"));
        assert_eq!(meta.input_tokens, Some(10));
        assert_eq!(meta.output_tokens, Some(20));
        assert_eq!(meta.cache_read_tokens, Some(30));
        assert_eq!(meta.duration_ms, Some(1000));
        assert!(matches!(
            &msgs[2].blocks[0],
            SessionBlock::ToolResult { call_id, preview, .. }
                if call_id == "tc1" && preview == "fn main() {}"
        ));
        assert_eq!(msgs[4].content, "All set.");
    }

    #[test]
    fn parses_wire_protocol_events_display_diff_brief_and_context_fallback() {
        let temp = tempdir().unwrap();
        let wire = temp.path().join("wire.jsonl");
        let lines = [
            json!({"timestamp":1000.0,"message":{"type":"TurnBegin","payload":{"user_input":[{"type":"text","text":"Edit app.rs"},{"type":"image_url","image_url":{"url":"data:image/png;base64,aGVsbG8="}}]}}}),
            json!({"timestamp":1001.0,"message":{"type":"ThinkPart","payload":{"think":"Inspecting file"}}}),
            json!({"timestamp":1001.5,"message":{"type":"TextPart","payload":{"text":"Applying patch"}}}),
            json!({"timestamp":1002.0,"message":{"type":"ToolCall","payload":{"id":"call_edit","function":{"name":"str_replace_file","arguments":"{\"path\":\"src/app.rs\"}"}}}}),
            json!({"timestamp":1002.5,"message":{"type":"StatusUpdate","payload":{"model":"kimi-latest","token_usage":{"input_other":80,"output":25,"input_cache_read":200}}}}),
            json!({"timestamp":1003.0,"message":{"type":"ToolResult","payload":{"tool_call_id":"call_edit","return_value":{"is_error":false,"output":"Replaced 1 block","display":[{"type":"brief","text":"Updated handler"},{"type":"diff","path":"src/app.rs","old_text":"fn old() {}\n","new_text":"fn new_a() {}\nfn new_b() {}\n"}]}}}}),
            json!({"timestamp":1004.0,"message":{"type":"CompactionBegin","payload":{"summary":"Compacted turn"}}}),
        ];
        fs::write(
            &wire,
            lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
        )
        .unwrap();

        let msgs = load_messages(&wire).unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0].role, "user");
        assert!(matches!(&msgs[0].blocks[1], SessionBlock::Image { .. }));

        let assistant = &msgs[1];
        assert!(
            matches!(&assistant.blocks[0], SessionBlock::Thinking { text, .. } if text == "Inspecting file")
        );
        assert!(
            matches!(&assistant.blocks[1], SessionBlock::Text { text, .. } if text == "Applying patch")
        );
        match &assistant.blocks[2] {
            SessionBlock::ToolCall {
                id,
                detail,
                diff: Some(diff),
                ..
            } => {
                assert_eq!(id, "call_edit");
                assert_eq!(detail.as_deref(), Some("Updated handler"));
                assert_eq!((diff.added, diff.removed), (2, 1));
            }
            other => panic!("expected enriched ToolCall, got {other:?}"),
        }
        let meta = assistant.meta.as_ref().unwrap();
        assert_eq!(meta.model.as_deref(), Some("kimi-latest"));
        assert_eq!(meta.input_tokens, Some(80));
        assert_eq!(meta.output_tokens, Some(25));
        assert_eq!(meta.cache_read_tokens, Some(200));

        assert!(matches!(
            &msgs[2].blocks[0],
            SessionBlock::ToolResult { call_id, preview, .. }
                if call_id == "call_edit" && preview == "Replaced 1 block"
        ));
        assert!(matches!(
            &msgs[3].blocks[0],
            SessionBlock::Event {
                kind: EventKind::Compaction,
                ..
            }
        ));

        // Fallback to context.jsonl when wire.jsonl is absent
        let fb_dir = temp.path().join("fallback-sess");
        fs::create_dir_all(fb_dir.join("agents/main")).unwrap();
        fs::write(
            fb_dir.join("context.jsonl"),
            "{\"role\":\"user\",\"content\":\"from context\"}\n{\"role\":\"assistant\",\"content\":\"reply from context\"}\n",
        )
        .unwrap();
        let fb_msgs = load_messages(&fb_dir.join("agents/main/wire.jsonl")).unwrap();
        assert_eq!(fb_msgs.len(), 2);
        assert_eq!(fb_msgs[0].content, "from context");
        assert_eq!(fb_msgs[1].content, "reply from context");
    }

    #[test]
    fn delete_session_removes_dir_writes_tombstone_and_rejects_symlinks_or_bad_layout() {
        let home = tempdir().unwrap();
        let root = home.path().join("sessions");
        let dir = root.join("wd_project_123/session_abc-1");
        let sibling = root.join("wd_project_123/session_keep");
        fs::create_dir_all(dir.join("agents/main")).unwrap();
        fs::create_dir_all(sibling.join("agents/main")).unwrap();
        let wire = dir.join("agents/main/wire.jsonl");
        fs::write(&wire, "{}\n").unwrap();
        fs::write(sibling.join("agents/main/wire.jsonl"), "{}\n").unwrap();

        // Subagent wire path must be rejected
        fs::create_dir_all(dir.join("agents/sub")).unwrap();
        let sub_wire = dir.join("agents/sub/wire.jsonl");
        fs::write(&sub_wire, "{}\n").unwrap();
        assert!(preflight_delete(&root, &sub_wire, "session_abc-1").is_err());
        assert!(preflight_delete(&root, &wire, "session_other").is_err());
        assert!(preflight_delete(&root, &wire, "../session_abc-1").is_err());

        // Symlinked session directory must be rejected
        let outside = home.path().join("outside-session");
        fs::create_dir_all(outside.join("agents/main")).unwrap();
        fs::write(outside.join("agents/main/wire.jsonl"), "{}\n").unwrap();
        let linked = root.join("wd_project_123/linked_sess");
        if create_dir_symlink(&outside, &linked).is_ok() {
            let linked_wire = linked.join("agents/main/wire.jsonl");
            assert!(preflight_delete(&root, &linked_wire, "linked_sess").is_err());
            assert!(delete_session(&root, &linked_wire, "linked_sess").is_err());
            assert!(outside.exists());
            let _ = remove_path_if_exists(&linked);
        }

        preflight_delete(&root, &wire, "session_abc-1").unwrap();
        assert!(delete_session(&root, &wire, "session_abc-1").unwrap());
        assert!(!dir.exists());
        assert!(sibling.exists());

        let index = fs::read_to_string(home.path().join("session_index.jsonl")).unwrap();
        let last: Value = serde_json::from_str(index.lines().last().unwrap()).unwrap();
        assert_eq!(last, json!({"sessionId": "session_abc-1", "deleted": true}));
        assert!(root.join(".index-dirty").exists());
    }
}
