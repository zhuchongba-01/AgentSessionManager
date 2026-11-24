use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use serde_json::Value;

use crate::session_manager::cache::TranscriptSources;
use crate::session_manager::content::parse_file_url;
use crate::session_manager::model::{
    EventKind, ImageRef, ImageSource, MessageMeta, SessionBlock, ToolStatus,
};
use crate::session_manager::{SessionMessage, SessionMeta};
use crate::session_paths::codebuddy_config_dir;

use super::blocks::{
    assign_turn_ids, estimate_base64_size, large_text_block, parse_arguments, summary_event_block,
    thinking_block, tool_call_block, tool_result_block, ToolSource,
};
use super::utils::{
    checked_storage_child, extract_text, for_each_jsonl_value, is_reparse_or_symlink,
    is_safe_session_id, parse_timestamp_to_ms, path_basename, read_head_tail_lines,
    remove_path_if_exists, scan_warning, truncate_summary, FileParseCache, JsonlSpan,
    TITLE_MAX_CHARS,
};

const PROVIDER_ID: &str = "codebuddy";
const HEAD_SCAN_LINES: usize = 60;
const TAIL_SCAN_LINES: usize = 40;

static PARSE_CACHE: LazyLock<FileParseCache> = LazyLock::new(FileParseCache::new);

pub fn session_roots() -> Vec<PathBuf> {
    vec![codebuddy_config_dir().join("projects")]
}

#[allow(dead_code)]
pub fn transcript_sources(path: &Path) -> Result<TranscriptSources, String> {
    Ok(vec![(path.to_path_buf(), None)])
}

pub fn scan_sessions() -> Vec<SessionMeta> {
    let root = codebuddy_config_dir().join("projects");
    scan_sessions_in_root(&root, &PARSE_CACHE)
}

fn is_valid_codebuddy_session_id(id: &str) -> bool {
    is_safe_session_id(id)
        && !id.starts_with("agent-")
        && !matches!(id, "sessions-index" | "memory" | "shared" | "temp" | "tmp")
}

fn scan_sessions_in_root(projects_root: &Path, cache: &FileParseCache) -> Vec<SessionMeta> {
    match fs::symlink_metadata(projects_root) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                scan_warning(format!(
                    "CodeBuddy 会话根目录是符号链接或重解析点，已跳过：{}",
                    projects_root.display()
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
                "无法检查 CodeBuddy 会话目录 {}：{err}",
                projects_root.display()
            ));
            return Vec::new();
        }
    }

    let mut files = Vec::new();
    let proj_entries = match fs::read_dir(projects_root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            scan_warning(format!(
                "无法扫描 CodeBuddy 目录 {}：{err}",
                projects_root.display()
            ));
            return Vec::new();
        }
    };

    for proj_entry in proj_entries {
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
            if !is_valid_codebuddy_session_id(stem) {
                continue;
            }
            files.push(ses_path);
        }
    }

    cache.scan(files, parse_session_result)
}

fn record_kind(record: &Value) -> &str {
    record.get("type").and_then(Value::as_str).unwrap_or("")
}

fn record_role(record: &Value) -> &str {
    record.get("role").and_then(Value::as_str).unwrap_or("")
}

fn is_meta(record: &Value) -> bool {
    record
        .pointer("/providerData/isMeta")
        .and_then(Value::as_bool)
        == Some(true)
}

fn is_compaction_summary(record: &Value) -> bool {
    let Some(pd) = record.get("providerData") else {
        return false;
    };
    pd.get("isCompacted").and_then(Value::as_bool) == Some(true)
        || pd.get("isCompactInternal").and_then(Value::as_bool) == Some(true)
        || pd.get("isSummary").and_then(Value::as_bool) == Some(true)
}

fn unwrap_user_query(raw: &str) -> String {
    const OPEN: &str = "<user_query>";
    const CLOSE: &str = "</user_query>";
    if let Some(start) = raw.rfind(OPEN) {
        let rest = &raw[start + OPEN.len()..];
        let end = rest.find(CLOSE).unwrap_or(rest.len());
        return rest[..end].trim().to_string();
    }
    strip_system_reminders(raw).trim().to_string()
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

fn extract_codebuddy_content_item(item: &Value, is_user: bool) -> Option<String> {
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
            let text = if is_user {
                unwrap_user_query(raw)
            } else {
                raw.to_string()
            };
            (!text.trim().is_empty()).then_some(text)
        }
        _ => {
            let fallback = extract_text(item);
            let text = if is_user {
                unwrap_user_query(&fallback)
            } else {
                fallback
            };
            (!text.trim().is_empty()).then_some(text)
        }
    }
}

fn extract_message_text(record: &Value) -> String {
    let is_user = record_role(record) == "user";
    let Some(content) = record.get("content") else {
        return String::new();
    };
    match content {
        Value::String(s) => {
            if is_user {
                unwrap_user_query(s)
            } else {
                s.clone()
            }
        }
        Value::Array(items) => items
            .iter()
            .filter_map(|item| extract_codebuddy_content_item(item, is_user))
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => extract_codebuddy_content_item(content, is_user).unwrap_or_default(),
        _ => String::new(),
    }
}

fn user_text(record: &Value) -> Option<String> {
    if record_kind(record) != "message"
        || record_role(record) != "user"
        || is_meta(record)
        || is_compaction_summary(record)
    {
        return None;
    }
    let text = extract_message_text(record);
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn assistant_text(record: &Value) -> Option<String> {
    if record_kind(record) != "message" || record_role(record) != "assistant" || is_meta(record) {
        return None;
    }
    let text = extract_message_text(record);
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn ai_title(record: &Value) -> Option<String> {
    if record_kind(record) != "ai-title" {
        return None;
    }
    record
        .get("aiTitle")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn session_meta_title(record: &Value) -> Option<String> {
    if record_kind(record) != "session-meta" {
        return None;
    }
    record
        .get("meta")
        .and_then(|m| {
            m.get("codebuddy.ai/sessionTitle")
                .or_else(|| m.get("title"))
        })
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn resume_command(id: &str) -> Option<String> {
    if !is_valid_codebuddy_session_id(id) {
        return None;
    }
    let mut prefix = String::new();
    if let Ok(value) = std::env::var("CODEBUDDY_CONFIG_DIR") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            #[cfg(windows)]
            prefix.push_str(&format!(
                "$env:CODEBUDDY_CONFIG_DIR = '{}'; ",
                trimmed.replace('\'', "''")
            ));
            #[cfg(not(windows))]
            prefix.push_str(&format!(
                "env CODEBUDDY_CONFIG_DIR={} ",
                crate::session_manager::terminal::shell_escape(trimmed)
            ));
        }
    }
    Some(format!("{prefix}codebuddy --resume {id}"))
}

fn parse_session_result(path: &Path) -> Result<Option<SessionMeta>, String> {
    let Some(stem) = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| is_valid_codebuddy_session_id(s))
        .map(str::to_string)
    else {
        return Ok(None);
    };

    let (head_lines, tail_lines) = read_head_tail_lines(path, HEAD_SCAN_LINES, TAIL_SCAN_LINES)
        .map_err(|err| format!("无法读取 CodeBuddy 会话 {}：{err}", path.display()))?;

    let head: Vec<Value> = head_lines
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .collect();
    let tail: Vec<Value> = tail_lines
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .collect();

    let session_id = head
        .iter()
        .find_map(|r| r.get("sessionId").and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or(stem);

    let prompt = head
        .iter()
        .find_map(user_text)
        .or_else(|| head.iter().find_map(assistant_text))
        .or_else(|| tail.iter().find_map(user_text))
        .or_else(|| tail.iter().find_map(assistant_text));
    let Some(prompt) = prompt else {
        return Ok(None);
    };

    let raw_title = tail
        .iter()
        .rev()
        .find_map(ai_title)
        .or_else(|| head.iter().rev().find_map(ai_title))
        .or_else(|| head.iter().find_map(session_meta_title))
        .unwrap_or_else(|| prompt.lines().next().unwrap_or_default().to_string());

    let project_dir = head
        .iter()
        .chain(tail.iter())
        .find_map(|r| r.get("cwd").and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let project_name = project_dir.as_deref().and_then(path_basename);

    let created_at = head
        .iter()
        .find_map(|r| r.get("timestamp").and_then(parse_timestamp_to_ms));
    let last_active_at = tail
        .iter()
        .rev()
        .chain(head.iter().rev())
        .find_map(|r| r.get("timestamp").and_then(parse_timestamp_to_ms))
        .or(created_at);

    Ok(Some(SessionMeta {
        provider_id: PROVIDER_ID.to_string(),
        resume_command: resume_command(&session_id),
        session_id,
        residual: false,
        parent_session_id: None,
        archived: false,
        cleanup_pending: false,
        title: Some(truncate_summary(&raw_title, TITLE_MAX_CHARS)),
        summary: Some(truncate_summary(&prompt, 160)),
        project_dir,
        project_name,
        sidebar_section: None,
        created_at,
        last_active_at,
        source_path: Some(path.to_string_lossy().to_string()),
    }))
}

fn non_empty_field<'a>(record: &'a Value, field: &str) -> Option<&'a str> {
    record
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
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

fn is_input_from_hook(record: &Value) -> bool {
    record
        .pointer("/extra/input_from_hook")
        .and_then(Value::as_bool)
        == Some(true)
        || record
            .pointer("/providerData/extra/input_from_hook")
            .and_then(Value::as_bool)
            == Some(true)
        || record
            .pointer("/providerData/input_from_hook")
            .and_then(Value::as_bool)
            == Some(true)
}

fn is_interrupted(record: &Value) -> bool {
    record
        .pointer("/extra/interrupted")
        .and_then(Value::as_bool)
        == Some(true)
        || record
            .pointer("/providerData/extra/interrupted")
            .and_then(Value::as_bool)
            == Some(true)
        || record
            .pointer("/providerData/interrupted")
            .and_then(Value::as_bool)
            == Some(true)
        || record.get("status").and_then(Value::as_str) == Some("interrupted")
}

fn has_non_empty_str(val: &Value, keys: &[&str]) -> bool {
    keys.iter().any(|k| {
        val.get(*k)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    })
}

fn extract_reasoning_with_pointer(record: &Value) -> (String, Option<String>, bool) {
    let mut redacted_sig = has_non_empty_str(record, &["signature", "encrypted_content"]);
    for key in ["rawContent", "content", "summary"] {
        let Some(val) = record.get(key) else {
            continue;
        };
        match val {
            Value::String(s) if !s.trim().is_empty() => {
                return (s.clone(), Some(format!("/{key}")), redacted_sig);
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
                        format!("/{key}/0/text")
                    } else if items.len() == 1 && items[0].get("thinking").is_some() {
                        format!("/{key}/0/thinking")
                    } else {
                        format!("/{key}")
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
                        return (s.to_string(), Some(format!("/{key}/{sub}")), redacted_sig);
                    }
                }
            }
            _ => {}
        }
    }
    for top in ["text", "thinking"] {
        if let Some(s) = record
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
    if record_role(record) == "user" {
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

    if record_kind(record) == "message" && record_role(record) == "assistant" {
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
        model: (record_kind(record) == "message" && record_role(record) == "assistant")
            .then_some(model)
            .flatten(),
        stop_reason,
        ..MessageMeta::default()
    }
    .into_option()
}

fn tool_status(record: &Value) -> ToolStatus {
    if is_interrupted(record) {
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
        Some("failed" | "error") => ToolStatus::Error,
        Some("incomplete" | "cancelled" | "canceled" | "interrupted") => ToolStatus::Interrupted,
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

fn build_message_blocks(
    record: &Value,
    span: JsonlSpan,
    role: &str,
    input_from_hook: bool,
    anon_calls: &mut AnonCallTracker,
) -> (Vec<SessionBlock>, bool) {
    let mut blocks = Vec::new();
    let is_user = role == "user";
    let mut saw_real_prompt = false;
    let mut saw_system_reminder_only = false;

    match record.get("content") {
        Some(Value::String(raw)) => {
            if is_user {
                let (without_attachments, attachment_images) = extract_attachment_images(raw);
                if !attachment_images.is_empty() {
                    saw_real_prompt = true;
                }
                let has_user_query = without_attachments.contains("<user_query>");
                let has_reminder = without_attachments.contains("<system-reminder>");
                let unwrapped = unwrap_user_query(&without_attachments);
                let trimmed = unwrapped.trim();
                if !trimmed.is_empty() {
                    saw_real_prompt = true;
                    if input_from_hook {
                        blocks.push(large_text_block(trimmed.to_string(), || {
                            Some(span.content_ref("/content"))
                        }));
                    } else {
                        blocks.push(SessionBlock::text(trimmed.to_string()));
                    }
                } else if !has_user_query && has_reminder {
                    let reminder_trimmed = without_attachments.trim();
                    if !reminder_trimmed.is_empty() {
                        saw_system_reminder_only = true;
                        blocks.push(large_text_block(reminder_trimmed.to_string(), || {
                            Some(span.content_ref("/content"))
                        }));
                    }
                }
                for image in attachment_images {
                    blocks.push(SessionBlock::Image { image });
                }
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
                            extract_codebuddy_content_item(item, is_user)
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
                        if let Some(fallback) = extract_codebuddy_content_item(item, is_user) {
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
            } else if let Some(text) = extract_codebuddy_content_item(obj, is_user) {
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

/// Deduplicate streaming updates sharing the same `(id, parentId)` in-place while preserving
/// the chronological order and JSONL byte spans of all events (including teammate sessions).
fn deduplicate_records(entries: &[(JsonlSpan, Value)]) -> Vec<(JsonlSpan, &Value)> {
    let mut nodes: Vec<(JsonlSpan, &Value)> = Vec::new();
    let mut last_by_id: HashMap<&str, usize> = HashMap::new();

    for (span, entry) in entries {
        if let Some(id) = non_empty_field(entry, "id") {
            if let Some(&prev_idx) = last_by_id.get(id) {
                let (_, prev) = nodes[prev_idx];
                if prev.get("parentId") == entry.get("parentId")
                    && record_kind(prev) == record_kind(entry)
                {
                    nodes[prev_idx] = (*span, entry);
                    continue;
                }
            }
            let idx = nodes.len();
            nodes.push((*span, entry));
            last_by_id.insert(id, idx);
        } else {
            nodes.push((*span, entry));
        }
    }
    nodes
}

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    let mut entries = Vec::new();
    for_each_jsonl_value(path, |span, val| {
        entries.push((span, val));
        Ok(())
    })?;

    let mut messages = Vec::new();
    let mut anon_calls = AnonCallTracker::default();

    for (span, record) in deduplicate_records(&entries) {
        let ts = record.get("timestamp").and_then(parse_timestamp_to_ms);
        let record_id = non_empty_field(record, "id").map(str::to_string);

        match record_kind(record) {
            "reasoning" => {
                let (thinking, pointer, has_sig) = extract_reasoning_with_pointer(record);
                if thinking.trim().is_empty() && !has_sig {
                    continue;
                }
                let block = thinking_block(&thinking, None, None, || {
                    pointer.map(|p| span.content_ref(p))
                });
                let mut msg = SessionMessage::from_blocks("assistant", ts, vec![block]);
                msg.id = record_id;
                msg.meta = usage_meta(record);
                messages.push(msg);
            }
            "function_call" => {
                let Some(name) = non_empty_field(record, "name") else {
                    continue;
                };
                let call_id = anon_calls.resolve_call_id(record, name);
                let input = parse_arguments(record.get("arguments").unwrap_or(&Value::Null));
                let block = tool_call_block(ToolSource::Claude, call_id, name, &input, || {
                    Some(span.content_ref("/arguments"))
                });
                let mut msg = SessionMessage::from_blocks("assistant", ts, vec![block]);
                msg.id = record_id;
                msg.meta = usage_meta(record);
                messages.push(msg);
            }
            "function_call_result" => {
                let call_id = anon_calls.resolve_result_call_id(record);
                let (text, pointer) = extract_tool_result_output(record);
                let block = tool_result_block(call_id, tool_status(record), &text, || {
                    pointer.map(|p| span.content_ref(p))
                });
                let mut msg = SessionMessage::from_blocks("tool", ts, vec![block]);
                msg.id = record_id;
                messages.push(msg);
            }
            "compact_boundary" => {
                let (summary, ptr) = if let Some(s) = record.get("summary").and_then(Value::as_str)
                {
                    (s, Some("/summary"))
                } else if let Some(s) = record.get("content").and_then(Value::as_str) {
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
                let subtype = record.get("subtype").and_then(Value::as_str).unwrap_or("");
                if subtype == "compact_boundary" || is_compaction_summary(record) {
                    let (summary, ptr) =
                        if let Some(s) = record.get("content").and_then(Value::as_str) {
                            (s.to_string(), Some("/content"))
                        } else if let Some(s) = record.get("summary").and_then(Value::as_str) {
                            (s.to_string(), Some("/summary"))
                        } else {
                            (extract_message_text(record), Some("/content"))
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
                } else if is_interrupted(record) {
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
                let input_from_hook = is_input_from_hook(record);
                let interrupted = is_interrupted(record);
                let is_meta_record =
                    is_meta(record) || record.get("isMeta").and_then(Value::as_bool) == Some(true);
                if is_meta_record && !input_from_hook && !interrupted {
                    continue;
                }
                if is_compaction_summary(record) {
                    let body = extract_message_text(record);
                    let block = if body.trim().is_empty() {
                        SessionBlock::event(EventKind::Compaction, None, None)
                    } else {
                        summary_event_block(EventKind::Compaction, &body, || {
                            Some(span.content_ref("/content"))
                        })
                    };
                    let mut msg = SessionMessage::from_blocks("system", ts, vec![block]);
                    msg.id = record_id;
                    msg.injected = true;
                    messages.push(msg);
                    continue;
                }

                let role = record_role(record);
                if !matches!(role, "user" | "assistant" | "system") {
                    continue;
                }

                let (mut blocks, injected) =
                    build_message_blocks(record, span, role, input_from_hook, &mut anon_calls);
                if interrupted {
                    blocks.push(SessionBlock::event(EventKind::Aborted, None, None));
                }

                let meta = (role == "assistant").then(|| usage_meta(record)).flatten();
                let mut msg = SessionMessage::from_blocks(role, ts, blocks);
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

fn validate_codebuddy_session_path(
    root: &Path,
    path: &Path,
    session_id: &str,
) -> Result<(PathBuf, PathBuf, bool), String> {
    if !is_valid_codebuddy_session_id(session_id) {
        return Err(format!("Invalid CodeBuddy session ID: {session_id}"));
    }
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(format!(
            "Invalid CodeBuddy session path: {}",
            path.display()
        ));
    }
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return Err(format!(
            "Invalid CodeBuddy session file extension: {}",
            path.display()
        ));
    }
    if path.file_stem().and_then(|s| s.to_str()) != Some(session_id) {
        return Err(format!(
            "CodeBuddy session ID ({session_id}) does not match file path: {}",
            path.display()
        ));
    }

    let file_exists = match fs::symlink_metadata(path) {
        Ok(meta) => {
            if is_reparse_or_symlink(&meta) {
                return Err(format!(
                    "CodeBuddy session source must not be a symlink or reparse point: {}",
                    path.display()
                ));
            }
            if !meta.file_type().is_file() {
                return Err(format!(
                    "CodeBuddy session source is not a regular file: {}",
                    path.display()
                ));
            }
            true
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            return Err(format!(
                "Failed to inspect CodeBuddy session {}: {err}",
                path.display()
            ))
        }
    };

    let canonical_root = root.canonicalize().map_err(|err| {
        format!(
            "Failed to resolve CodeBuddy projects root {}: {err}",
            root.display()
        )
    })?;
    let root_meta = fs::metadata(&canonical_root).map_err(|err| {
        format!(
            "Failed to inspect CodeBuddy projects root {}: {err}",
            canonical_root.display()
        )
    })?;
    if !root_meta.is_dir() {
        return Err(format!(
            "CodeBuddy projects root is not a directory: {}",
            canonical_root.display()
        ));
    }

    let resolved_path = checked_storage_child(root, path)?;
    let relative = resolved_path.strip_prefix(&canonical_root).map_err(|_| {
        format!(
            "CodeBuddy session path is outside projects root: {}",
            path.display()
        )
    })?;
    let components: Vec<_> = relative.components().collect();
    if components.len() != 2
        || !matches!(components[0], Component::Normal(_))
        || !matches!(components[1], Component::Normal(_))
    {
        return Err(format!(
            "CodeBuddy session path must be <projects>/<slug>/<session>.jsonl: {}",
            path.display()
        ));
    }

    if file_exists {
        let (head, _) = read_head_tail_lines(&resolved_path, HEAD_SCAN_LINES, 0)
            .map_err(|err| format!("Failed to read CodeBuddy session {}: {err}", path.display()))?;
        for line in head {
            if let Ok(val) = serde_json::from_str::<Value>(line.trim()) {
                if let Some(stored_id) = val
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    if stored_id != session_id {
                        return Err(format!(
                            "CodeBuddy session ID mismatch: expected {session_id}, found {stored_id}"
                        ));
                    }
                    break;
                }
            }
        }
    }

    let project_dir = resolved_path
        .parent()
        .ok_or_else(|| format!("Invalid CodeBuddy project directory: {}", path.display()))?;
    for sidecar in [
        project_dir.join(format!("{session_id}.file-rollback.ndjson")),
        project_dir.join(session_id),
    ] {
        if let Ok(meta) = fs::symlink_metadata(&sidecar) {
            if is_reparse_or_symlink(&meta) {
                return Err(format!(
                    "Refusing to delete CodeBuddy session with symlinked sidecar: {}",
                    sidecar.display()
                ));
            }
        }
    }

    Ok((canonical_root, resolved_path, file_exists))
}

#[allow(dead_code)]
pub fn preflight_delete(root: &Path, path: &Path, session_id: &str) -> Result<(), String> {
    validate_codebuddy_session_path(root, path, session_id).map(|_| ())
}

pub fn delete_session(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    let (canonical_root, resolved_path, _file_exists) =
        validate_codebuddy_session_path(root, path, session_id)?;
    let project_dir = resolved_path
        .parent()
        .ok_or_else(|| format!("Invalid CodeBuddy project directory: {}", path.display()))?;

    let mut removed = false;
    let targets = [
        resolved_path.clone(),
        project_dir.join(format!("{session_id}.file-rollback.ndjson")),
        project_dir.join(session_id),
    ];

    for target in &targets {
        match fs::symlink_metadata(target) {
            Ok(meta) => {
                if is_reparse_or_symlink(&meta) {
                    return Err(format!(
                        "CodeBuddy target turned into a symlink before deletion: {}",
                        target.display()
                    ));
                }
                let checked = checked_storage_child(&canonical_root, target)?;
                remove_path_if_exists(&checked)
                    .map_err(|err| format!("Failed to delete {}: {err}", checked.display()))?;
                removed = true;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(format!(
                    "Failed to inspect CodeBuddy artifact {}: {err}",
                    target.display()
                ))
            }
        }
    }

    let _ = fs::remove_dir(project_dir);
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const SESSION: &str = r#"{"type":"session-meta","id":"m0","sessionId":"0f3c-41aa","timestamp":1791900000000,"meta":{}}
{"id":"u1","parentId":null,"timestamp":1791900000100,"sessionId":"0f3c-41aa","cwd":"/work/proj","type":"message","role":"user","content":[{"type":"input_text","text":"list the txt files"}],"providerData":{"agent":"cli"}}
{"id":"r1","parentId":"u1","timestamp":1791900000200,"sessionId":"0f3c-41aa","cwd":"/work/proj","type":"reasoning","content":[],"rawContent":[{"type":"reasoning_text","text":"Use a glob."}],"providerData":{"messageId":"msg1","stepSeq":1,"model":"hy4-preview"}}
{"id":"c1","parentId":"r1","timestamp":1791900000300,"sessionId":"0f3c-41aa","cwd":"/work/proj","type":"function_call","callId":"call_1","name":"Bash","arguments":"{\"command\":\"ls *.txt\"}"}
{"id":"o1","parentId":"c1","timestamp":1791900000400,"sessionId":"0f3c-41aa","cwd":"/work/proj","type":"function_call_result","callId":"call_1","name":"Bash","status":"completed","output":{"type":"text","text":"a.txt"}}
{"id":"u2","parentId":"o1","timestamp":1791900000500,"sessionId":"0f3c-41aa","cwd":"/work/proj","type":"message","role":"user","content":[{"type":"input_text","text":"<system-reminder>use the task tools</system-reminder>"}],"providerData":{"isMeta":true,"taskReminderKind":"task"}}
{"timestamp":1791900000550,"type":"file-history-snapshot","snapshot":{}}
{"id":"a1","parentId":"u2","timestamp":1791900000600,"sessionId":"0f3c-41aa","cwd":"/work/proj","type":"message","role":"assistant","content":[{"type":"output_text","text":"Found a.txt."}]}
{"id":"t1","timestamp":1791900000700,"sessionId":"0f3c-41aa","type":"ai-title","aiTitle":"List text files"}
"#;

    #[cfg(unix)]
    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_file(target, link)
    }

    #[test]
    #[serial_test::serial]
    #[allow(deprecated)]
    fn scans_title_cwd_teammate_sessions_and_resume_command() {
        let prev = std::env::var_os("CODEBUDDY_CONFIG_DIR");
        std::env::remove_var("CODEBUDDY_CONFIG_DIR");

        let temp = tempdir().unwrap();
        let projects = temp.path().join("projects");
        let proj = projects.join("work-proj");
        fs::create_dir_all(proj.join("0f3c-41aa").join("tool-results")).unwrap();
        let path = proj.join("0f3c-41aa.jsonl");
        fs::write(&path, SESSION).unwrap();
        fs::write(proj.join("empty.jsonl"), "").unwrap();
        // Subagent jsonl inside sidecar dir must NOT be scanned as top-level session
        fs::write(proj.join("0f3c-41aa").join("sub.jsonl"), SESSION).unwrap();

        // Teammate session without user prompt falls back to first reply
        fs::write(
            proj.join("teammate-1.jsonl"),
            r#"{"id":"c","timestamp":1,"sessionId":"teammate-1","type":"function_call","callId":"x","name":"SpeakInChannel","arguments":"{}"}
{"id":"a","timestamp":2,"sessionId":"teammate-1","type":"message","role":"assistant","content":[{"type":"output_text","text":"Reporting in."}]}
"#,
        )
        .unwrap();

        let cache = FileParseCache::new();
        let sessions = scan_sessions_in_root(&projects, &cache);
        assert_eq!(sessions.len(), 2);

        let main = sessions
            .iter()
            .find(|s| s.session_id == "0f3c-41aa")
            .unwrap();
        assert_eq!(main.provider_id, "codebuddy");
        assert_eq!(main.title.as_deref(), Some("List text files"));
        assert_eq!(main.summary.as_deref(), Some("list the txt files"));
        assert_eq!(main.project_dir.as_deref(), Some("/work/proj"));
        assert_eq!(main.project_name.as_deref(), Some("proj"));
        assert_eq!(
            main.resume_command.as_deref(),
            Some("codebuddy --resume 0f3c-41aa")
        );
        assert_eq!(main.created_at, Some(1791900000000));
        assert_eq!(main.last_active_at, Some(1791900000700));

        let mate = sessions
            .iter()
            .find(|s| s.session_id == "teammate-1")
            .unwrap();
        assert_eq!(mate.title.as_deref(), Some("Reporting in."));

        // Custom CODEBUDDY_CONFIG_DIR prefixes resume command
        std::env::set_var("CODEBUDDY_CONFIG_DIR", temp.path());
        let cmd = resume_command("0f3c-41aa").unwrap();
        assert!(cmd.contains("CODEBUDDY_CONFIG_DIR"));
        assert!(cmd.ends_with("codebuddy --resume 0f3c-41aa"));

        match prev {
            Some(v) => std::env::set_var("CODEBUDDY_CONFIG_DIR", v),
            None => std::env::remove_var("CODEBUDDY_CONFIG_DIR"),
        }
    }

    #[test]
    fn load_messages_hides_is_meta_and_separates_thinking_and_tools() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("0f3c-41aa.jsonl");
        fs::write(&path, SESSION).unwrap();

        let messages = load_messages(&path).unwrap();
        let roles: Vec<&str> = messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "assistant", "tool", "assistant"]
        );
        assert_eq!(messages[0].content, "list the txt files");
        assert_eq!(messages[0].turn_id.as_deref(), Some("t1"));
        assert!(matches!(
            &messages[1].blocks[0],
            SessionBlock::Thinking { text, redacted: false, .. } if text == "Use a glob."
        ));
        assert!(matches!(
            &messages[2].blocks[0],
            SessionBlock::ToolCall { id, raw_name, title, .. }
                if id == "call_1" && raw_name == "Bash" && title == "ls *.txt"
        ));
        assert!(matches!(
            &messages[3].blocks[0],
            SessionBlock::ToolResult { call_id, status: ToolStatus::Success, preview, .. }
                if call_id == "call_1" && preview == "a.txt"
        ));
        assert_eq!(messages[4].content, "Found a.txt.");
        assert!(messages
            .iter()
            .all(|m| !m.content.contains("system-reminder")));
    }

    #[test]
    fn load_messages_pairs_omitted_call_ids_extracts_attachments_diffs_and_usage() {
        let temp = tempdir().unwrap();
        let img = temp.path().join("diagram.png");
        fs::write(&img, b"pngbytes").unwrap();
        let img_str = img.to_string_lossy().to_string();
        let path = temp.path().join("cb-rich.jsonl");
        let lines = [
            serde_json::json!({
                "id": "u1",
                "timestamp": 100,
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": format!("<attachment>{img_str}</attachment>\n<user_query>write file</user_query>")}]
            }),
            serde_json::json!({
                "id": "fc1",
                "timestamp": 200,
                "type": "function_call",
                "name": "Write",
                "arguments": "{\"file_path\":\"src/new.rs\",\"content\":\"line1\\nline2\"}",
                "providerData": {
                    "model": "hy4-preview",
                    "rawUsage": {
                        "prompt_tokens": 1200,
                        "completion_tokens": 40,
                        "prompt_cache_hit_tokens": 1000,
                        "completion_thinking_tokens": 12
                    }
                }
            }),
            serde_json::json!({
                "id": "fcr1",
                "timestamp": 300,
                "type": "function_call_result",
                "name": "Write",
                "status": "completed",
                "output": {"type": "text", "text": "Created"}
            }),
            serde_json::json!({
                "id": "rem1",
                "timestamp": 350,
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "<system-reminder>hook context</system-reminder>"}]
            }),
            serde_json::json!({
                "id": "cb1",
                "timestamp": 360,
                "type": "compact_boundary",
                "summary": "Compacted history"
            }),
            serde_json::json!({
                "id": "a1",
                "timestamp": 400,
                "type": "message",
                "role": "assistant",
                "extra": {"interrupted": true},
                "content": [{"type": "output_text", "text": "Stopped"}]
            }),
        ];
        fs::write(
            &path,
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let messages = load_messages(&path).unwrap();
        assert_eq!(messages.len(), 6);
        assert!(matches!(
            &messages[0].blocks[1],
            SessionBlock::Image { image } if matches!(&image.source, ImageSource::LocalFile { path } if path == &img_str)
        ));
        assert!(matches!(
            &messages[1].blocks[0],
            SessionBlock::ToolCall { id, diff: Some(diff), .. } if id == "call_1" && diff.added == 2 && diff.removed == 0
        ));
        let meta = messages[1].meta.as_ref().unwrap();
        assert_eq!(meta.model.as_deref(), Some("hy4-preview"));
        assert_eq!(meta.input_tokens, Some(200));
        assert_eq!(meta.cache_read_tokens, Some(1000));
        assert_eq!(meta.output_tokens, Some(40));
        assert_eq!(meta.reasoning_tokens, Some(12));
        assert!(matches!(
            &messages[2].blocks[0],
            SessionBlock::ToolResult { call_id, .. } if call_id == "call_1"
        ));
        assert!(messages[3].injected);
        assert!(matches!(
            &messages[4].blocks[0],
            SessionBlock::Event {
                kind: EventKind::Compaction,
                ..
            }
        ));
        assert!(matches!(
            &messages[5].blocks[1],
            SessionBlock::Event {
                kind: EventKind::Aborted,
                ..
            }
        ));
    }

    #[test]
    fn delete_session_removes_jsonl_rollback_and_sidecar_dir_and_rejects_symlinks() {
        let temp = tempdir().unwrap();
        let projects = temp.path().join("projects");
        let proj = projects.join("work-proj");
        let id = "0f3c-41aa";
        fs::create_dir_all(proj.join(id).join("tool-results")).unwrap();
        let jsonl = proj.join(format!("{id}.jsonl"));
        let rollback = proj.join(format!("{id}.file-rollback.ndjson"));
        fs::write(&jsonl, SESSION).unwrap();
        fs::write(&rollback, "{}\n").unwrap();
        fs::write(proj.join(id).join("tool-results").join("r1.txt"), "out").unwrap();

        let keep_jsonl = proj.join("keep-1.jsonl");
        fs::write(
            &keep_jsonl,
            r#"{"sessionId":"keep-1","type":"message","role":"user","content":[{"type":"text","text":"keep"}]}"#,
        )
        .unwrap();

        // Symlinked transcript is rejected
        let link = proj.join("linked-1.jsonl");
        if create_file_symlink(&keep_jsonl, &link).is_ok() {
            assert!(preflight_delete(&projects, &link, "linked-1").is_err());
            assert!(delete_session(&projects, &link, "linked-1").is_err());
            assert!(keep_jsonl.exists());
            let _ = fs::remove_file(&link);
        }

        // Depth mismatch rejected
        assert!(preflight_delete(temp.path(), &jsonl, id).is_err());
        // Session ID mismatch rejected
        assert!(preflight_delete(&projects, &jsonl, "other-id").is_err());

        preflight_delete(&projects, &jsonl, id).unwrap();
        assert!(delete_session(&projects, &jsonl, id).unwrap());
        assert!(!jsonl.exists());
        assert!(!rollback.exists());
        assert!(!proj.join(id).exists());
        assert!(keep_jsonl.exists());
    }
}
