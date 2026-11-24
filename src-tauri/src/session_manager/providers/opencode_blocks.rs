//! OpenCode 与 ZCode 共用的 `message` / `part` → [`SessionBlock`] 映射。
//!
//! 覆盖四条读取路径：
//! - OpenCode 旧版 JSON 目录（`ContentRef::File` 指向 `part/<msgId>/<partId>.json` 或 `message/<sesId>/<msgId>.json`）
//! - OpenCode SQLite V1（`ContentRef::Sqlite` 指向 `part.data` / `message.data`）
//! - OpenCode SQLite V2（`ContentRef::Sqlite` 指向 `session_message.data`）
//! - ZCode SQLite（同构 `message` + `part` 表；若旧表无 `part.id` 列则优雅退化为无 `ContentRef`）

use serde_json::Value;

use crate::session_manager::content::parse_file_url;
use crate::session_manager::model::{
    ContentRef, DiffOp, EventKind, ImageRef, ImageSource, MessageMeta, SessionBlock,
    SessionMessage, StepPhase, ToolKind, ToolStatus,
};

use super::blocks::{
    count_diff_lines, estimate_base64_size, input_preview, large_text_block, parse_arguments,
    single_file_diff, str_field, summary_event_block, thinking_block, title_path, title_shell,
    tool_call_block, tool_result_block, ToolSource,
};
use super::utils::parse_timestamp_to_ms;

/// 一个记录或 part 在底层存储中的定位信息，用于按需回取完整内容。
#[derive(Debug, Clone)]
pub(super) enum PartLocator {
    /// 无稳定行主键（如旧库缺少 `part.id` 列）时不生成 `ContentRef`
    None,
    /// SQLite 表行：`table` 在 `content.rs` 白名单内（`part` / `message` / `session_message`）
    Sqlite {
        table: &'static str,
        id: String,
        base: String,
    },
    /// 文件存储：相对于 OpenCode `storage` 根目录的 `/` 分隔路径
    File { rel_path: String },
}

impl PartLocator {
    pub(super) fn content_ref(&self, sub_pointer: &str) -> Option<ContentRef> {
        match self {
            PartLocator::None => None,
            PartLocator::Sqlite { table, id, base } => Some(ContentRef::Sqlite {
                table: (*table).to_string(),
                id: id.clone(),
                column: "data".to_string(),
                pointer: format!("{base}{sub_pointer}"),
            }),
            PartLocator::File { rel_path } => Some(ContentRef::File {
                rel_path: rel_path.clone(),
                pointer: sub_pointer.to_string(),
            }),
        }
    }
}

/// 将一条 OpenCode / ZCode 消息（`info` + 有序 `parts`）转换为一到多条 [`SessionMessage`]。
///
/// - 若整条消息仅含 `synthetic: true` 文本块（或 `info.synthetic == true`），输出单条 `injected: true` 消息；
/// - 若用户/系统消息同时含正常对话块与 `synthetic: true` 注入块，先输出正常消息（参与开启新轮次），
///   再紧跟输出 `injected: true` 消息（归入当前轮次并默认折叠）。
pub(super) fn messages_from_parts(
    role: &str,
    id: Option<String>,
    ts: Option<i64>,
    info: &Value,
    msg_locator: &PartLocator,
    parts: &[(PartLocator, Value)],
) -> Vec<SessionMessage> {
    let mut blocks = Vec::new();
    let mut synthetic_blocks = Vec::new();

    for (locator, part) in parts {
        push_part_blocks(&mut blocks, &mut synthetic_blocks, locator, part);
    }

    if role == "assistant" {
        blocks.extend(message_error_event(info, msg_locator));
    }

    let info_synthetic = info.get("synthetic").and_then(Value::as_bool) == Some(true);
    let meta = if role == "assistant" {
        message_meta(info)
    } else {
        None
    };

    if blocks.is_empty() {
        if synthetic_blocks.is_empty() {
            return Vec::new();
        }
        let mut msg = SessionMessage::from_blocks(role, ts, synthetic_blocks);
        msg.id = id;
        msg.injected = true;
        msg.meta = meta;
        return if msg.is_empty() {
            Vec::new()
        } else {
            vec![msg]
        };
    }

    let mut out = Vec::with_capacity(if synthetic_blocks.is_empty() { 1 } else { 2 });
    let mut primary = SessionMessage::from_blocks(role, ts, blocks);
    primary.id = id;
    primary.injected = info_synthetic;
    primary.meta = meta;
    if !primary.is_empty() {
        out.push(primary);
    }

    if !synthetic_blocks.is_empty() {
        let mut injected_msg = SessionMessage::from_blocks(role, ts, synthetic_blocks);
        injected_msg.injected = true;
        if !injected_msg.is_empty() {
            out.push(injected_msg);
        }
    }

    out
}

/// 将 OpenCode V2 `session_message` 表的单行记录转换为 [`SessionMessage`] 列表。
pub(super) fn v2_row_messages(
    row_id: &str,
    kind: &str,
    ts: i64,
    value: &Value,
) -> Vec<SessionMessage> {
    let msg_ts = Some(ts);
    let row_locator = PartLocator::Sqlite {
        table: "session_message",
        id: row_id.to_string(),
        base: String::new(),
    };

    match kind {
        "user" | "system" | "synthetic" => {
            let role = if kind == "user" { "user" } else { "system" };
            let is_synthetic = kind == "synthetic"
                || value.get("synthetic").and_then(Value::as_bool) == Some(true);

            let mut parts: Vec<(PartLocator, Value)> = Vec::new();
            if let Some(text) = value
                .get("text")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                parts.push((
                    row_locator.clone(),
                    serde_json::json!({
                        "type": "text",
                        "text": text,
                        "synthetic": is_synthetic,
                    }),
                ));
            } else if let Some(text) = value
                .get("content")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                let block = if is_synthetic {
                    large_text_block(text.to_string(), || row_locator.content_ref("/content"))
                } else {
                    SessionBlock::text(text.to_string())
                };
                let mut msg = SessionMessage::from_blocks(role, msg_ts, vec![block]);
                msg.id = Some(row_id.to_string());
                msg.injected = is_synthetic;
                if let Some(files) = value.get("files").and_then(Value::as_array) {
                    for (i, file) in files.iter().enumerate() {
                        let loc = PartLocator::Sqlite {
                            table: "session_message",
                            id: row_id.to_string(),
                            base: format!("/files/{i}"),
                        };
                        push_file_block(&mut msg.blocks, &loc, file);
                    }
                    msg.content = crate::session_manager::model::project_content(&msg.blocks);
                }
                return if msg.is_empty() {
                    Vec::new()
                } else {
                    vec![msg]
                };
            } else if let Some(Value::Array(items)) = value.get("content") {
                for (i, item) in items.iter().enumerate() {
                    let mut cloned = item.clone();
                    if is_synthetic
                        && cloned.get("type").and_then(Value::as_str) == Some("text")
                        && cloned.get("synthetic").is_none()
                    {
                        cloned["synthetic"] = Value::Bool(true);
                    }
                    parts.push((
                        PartLocator::Sqlite {
                            table: "session_message",
                            id: row_id.to_string(),
                            base: format!("/content/{i}"),
                        },
                        cloned,
                    ));
                }
            }

            if let Some(files) = value.get("files").and_then(Value::as_array) {
                for (i, file) in files.iter().enumerate() {
                    let mut file_part = file.clone();
                    if let Value::Object(ref mut map) = file_part {
                        map.entry("type")
                            .or_insert_with(|| Value::String("file".to_string()));
                    }
                    parts.push((
                        PartLocator::Sqlite {
                            table: "session_message",
                            id: row_id.to_string(),
                            base: format!("/files/{i}"),
                        },
                        file_part,
                    ));
                }
            }

            let mut msgs = messages_from_parts(
                role,
                Some(row_id.to_string()),
                msg_ts,
                value,
                &row_locator,
                &parts,
            );
            if is_synthetic {
                for m in &mut msgs {
                    m.injected = true;
                }
            }
            msgs
        }
        "assistant" => {
            let mut parts: Vec<(PartLocator, Value)> = Vec::new();
            if let Some(Value::Array(items)) = value.get("content") {
                for (i, item) in items.iter().enumerate() {
                    parts.push((
                        PartLocator::Sqlite {
                            table: "session_message",
                            id: row_id.to_string(),
                            base: format!("/content/{i}"),
                        },
                        item.clone(),
                    ));
                }
            } else if let Some(text) = value
                .get("text")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                parts.push((
                    row_locator.clone(),
                    serde_json::json!({ "type": "text", "text": text }),
                ));
            }
            messages_from_parts(
                "assistant",
                Some(row_id.to_string()),
                msg_ts,
                value,
                &row_locator,
                &parts,
            )
        }
        "shell" => {
            let command = value.get("command").and_then(Value::as_str).unwrap_or("");
            let output = value.get("output").and_then(Value::as_str).unwrap_or("");
            if command.trim().is_empty() && output.trim().is_empty() {
                return Vec::new();
            }
            let exit = value
                .get("exitCode")
                .or_else(|| value.get("exit"))
                .and_then(Value::as_i64)
                .and_then(|n| i32::try_from(n).ok());
            let preview = input_preview(&Value::String(command.to_string()));
            let call = SessionBlock::ToolCall {
                id: row_id.to_string(),
                raw_name: "shell".to_string(),
                kind: ToolKind::Shell,
                title: title_shell(command),
                detail: None,
                server: None,
                input_preview: preview.text,
                input_total_len: preview.total_len,
                input_full: if preview.truncated {
                    row_locator.content_ref("/command")
                } else {
                    None
                },
                diff: None,
                by_user: true,
            };
            let status = if exit.is_some_and(|code| code != 0) {
                ToolStatus::Error
            } else {
                ToolStatus::Success
            };
            let mut result = tool_result_block(row_id.to_string(), status, output, || {
                row_locator.content_ref("/output")
            });
            if let SessionBlock::ToolResult { exit_code, .. } = &mut result {
                *exit_code = exit;
            }
            let mut msg = SessionMessage::from_blocks("tool", msg_ts, vec![call, result]);
            msg.id = Some(row_id.to_string());
            vec![msg]
        }
        "compaction" => {
            let block = if let Some(summary) = value
                .get("summary")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                Some(summary_event_block(EventKind::Compaction, summary, || {
                    row_locator.content_ref("/summary")
                }))
            } else if let Some(err) = value.get("error").and_then(format_error_value) {
                Some(SessionBlock::event(
                    EventKind::Error,
                    Some(format!("Compaction failed: {err}")),
                    None,
                ))
            } else if matches!(
                value.get("status").and_then(Value::as_str),
                Some("failed" | "error" | "aborted")
            ) || value.get("failed").and_then(Value::as_bool) == Some(true)
            {
                Some(SessionBlock::event(
                    EventKind::Error,
                    Some("Compaction failed".to_string()),
                    None,
                ))
            } else {
                None
            };
            let Some(block) = block else {
                return Vec::new();
            };
            let mut msg = SessionMessage::from_blocks("system", msg_ts, vec![block]);
            msg.id = Some(row_id.to_string());
            vec![msg]
        }
        "agent-switched" => {
            let agent = value
                .get("agent")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("unknown");
            let mut msg = SessionMessage::from_blocks(
                "system",
                msg_ts,
                vec![SessionBlock::event(
                    EventKind::SubAgent,
                    Some(format!("@{agent}")),
                    None,
                )],
            );
            msg.id = Some(row_id.to_string());
            vec![msg]
        }
        "model-switched" => {
            let model = format_model_switched(value);
            let mut msg = SessionMessage::from_blocks(
                "system",
                msg_ts,
                vec![SessionBlock::event(
                    EventKind::ModelChange,
                    Some(model),
                    None,
                )],
            );
            msg.id = Some(row_id.to_string());
            vec![msg]
        }
        _ => {
            log::debug!("Skipping unknown OpenCode V2 message type: {kind}");
            Vec::new()
        }
    }
}

fn push_part_blocks(
    blocks: &mut Vec<SessionBlock>,
    synthetic_blocks: &mut Vec<SessionBlock>,
    locator: &PartLocator,
    part: &Value,
) {
    match part.get("type").and_then(Value::as_str) {
        Some("text") => {
            if part.get("ignored").and_then(Value::as_bool) == Some(true) {
                return;
            }
            let Some(text) = part
                .get("text")
                .and_then(Value::as_str)
                .filter(|t| !t.trim().is_empty())
            else {
                return;
            };
            if part.get("synthetic").and_then(Value::as_bool) == Some(true) {
                synthetic_blocks.push(large_text_block(text.to_string(), || {
                    locator.content_ref("/text")
                }));
            } else {
                blocks.push(SessionBlock::text(text.to_string()));
            }
        }
        Some("reasoning" | "thinking") => {
            let (text, sub_ptr) = if let Some(t) = part.get("text").and_then(Value::as_str) {
                (t, "/text")
            } else if let Some(t) = part.get("thinking").and_then(Value::as_str) {
                (t, "/thinking")
            } else {
                ("", "/text")
            };
            let summary = part
                .pointer("/metadata/openai/reasoningSummary")
                .or_else(|| part.get("summary"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            blocks.push(thinking_block(text, summary, part_duration(part), || {
                locator.content_ref(sub_ptr)
            }));
        }
        Some("step-start") => blocks.push(SessionBlock::Step {
            phase: StepPhase::Start,
            tokens: None,
            cost_usd: None,
            reason: None,
        }),
        Some("step-finish") => blocks.push(SessionBlock::Step {
            phase: StepPhase::Finish,
            tokens: part.get("tokens").and_then(total_tokens),
            cost_usd: part.get("cost").and_then(Value::as_f64),
            reason: part
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        Some("tool") => push_tool_blocks(blocks, locator, part),
        Some("agent") => {
            if let Some(name) = part
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                blocks.push(SessionBlock::event(
                    EventKind::SubAgent,
                    Some(format!("@{name}")),
                    None,
                ));
            }
        }
        Some("subtask") => push_subtask_block(blocks, locator, part),
        Some("compaction") => push_compaction_block(blocks, locator, part),
        Some("patch") => push_patch_blocks(blocks, part),
        Some("file") => push_file_block(blocks, locator, part),
        // snapshot / retry 等内部状态不进入阅读视图
        _ => {}
    }
}

/// `tool{callID, tool, state}` → `ToolCall` + `ToolResult`（兼容 V2 `name`/`id`、`state.content` 与 `state.attachments`）。
fn push_tool_blocks(blocks: &mut Vec<SessionBlock>, locator: &PartLocator, part: &Value) {
    let name = part
        .get("tool")
        .or_else(|| part.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown");
    let call_id = part
        .get("callID")
        .or_else(|| part.get("callId"))
        .or_else(|| part.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let state = part.get("state");
    let (input_ptr, raw_input) = if let Some(inp) = state.and_then(|s| s.get("input")) {
        ("/state/input", Some(inp))
    } else {
        ("/input", part.get("input"))
    };
    let input = raw_input.map(parse_arguments).unwrap_or(Value::Null);
    let metadata = state.and_then(|s| s.get("metadata"));
    let state_title = state
        .and_then(|s| s.get("title"))
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty());

    let mut call = tool_call_block(ToolSource::OpenCode, call_id.clone(), name, &input, || {
        locator.content_ref(input_ptr)
    });
    if let SessionBlock::ToolCall {
        kind, title, diff, ..
    } = &mut call
    {
        if let Some(t) = state_title
            .filter(|_| matches!(kind, ToolKind::Edit | ToolKind::Write) || title.is_empty())
        {
            *title = title_path(t);
        }
        if let Some(meta) = metadata {
            let path = str_field(&input, &["filePath", "file_path", "path"])
                .or_else(|| meta.pointer("/filediff/file").and_then(Value::as_str))
                .unwrap_or(title.as_str())
                .to_string();
            let op = if *kind == ToolKind::Write {
                if meta.get("exists").and_then(Value::as_bool) == Some(true) {
                    DiffOp::Update
                } else {
                    DiffOp::Add
                }
            } else {
                DiffOp::Update
            };
            let counts = match (
                meta.pointer("/filediff/additions").and_then(Value::as_u64),
                meta.pointer("/filediff/deletions").and_then(Value::as_u64),
            ) {
                (Some(a), Some(d)) => Some((saturate(a), saturate(d))),
                _ => meta
                    .get("diff")
                    .and_then(Value::as_str)
                    .map(count_diff_lines),
            };
            if let Some((added, removed)) = counts {
                let mut summary = single_file_diff(&path, op, added, removed);
                if meta.get("diff").and_then(Value::as_str).is_some() {
                    summary.full = locator.content_ref("/state/metadata/diff");
                }
                *diff = Some(summary);
            } else if let Some(d) = diff.as_mut() {
                for file in &mut d.files {
                    file.op = op;
                }
            }
        }
    }
    blocks.push(call);

    let status_raw = state.and_then(|s| s.get("status")).and_then(Value::as_str);
    let error_value = state
        .and_then(|s| s.get("error"))
        .or_else(|| part.get("error"));
    let error_text = error_value.and_then(format_error_value).unwrap_or_default();
    let status = match status_raw {
        Some("completed") => ToolStatus::Success,
        Some("error") if error_text.to_ascii_lowercase().contains("abort") => {
            ToolStatus::Interrupted
        }
        Some("error") => ToolStatus::Error,
        Some("running" | "pending") => ToolStatus::Pending,
        None if !error_text.is_empty() => ToolStatus::Error,
        _ => ToolStatus::Unknown,
    };

    let mut images = Vec::new();
    if let Some(attachments) = state
        .and_then(|s| s.get("attachments"))
        .and_then(Value::as_array)
    {
        for (i, att) in attachments.iter().enumerate() {
            if let Some(img) = file_item_to_image(att, locator, &format!("/state/attachments/{i}"))
            {
                images.push(img);
            }
        }
    }

    let mut structured_texts = Vec::new();
    if let Some(items) = state
        .and_then(|s| s.get("content"))
        .and_then(Value::as_array)
    {
        for (i, item) in items.iter().enumerate() {
            match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = item
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                    {
                        structured_texts.push(t.to_string());
                    }
                }
                Some("file") => {
                    if let Some(img) =
                        file_item_to_image(item, locator, &format!("/state/content/{i}"))
                    {
                        images.push(img);
                    } else if let Some(label) = file_item_label(item) {
                        structured_texts.push(format!("[File: {label}]"));
                    }
                }
                _ => {}
            }
        }
    }

    let raw_output = state
        .and_then(|s| s.get("output"))
        .or_else(|| part.get("output"))
        .and_then(Value::as_str);
    let structured_joined = structured_texts.join("\n");

    let (base_output, out_ptr): (&str, &str) = if !structured_joined.is_empty() {
        (&structured_joined, "/state/content")
    } else if let Some(out) = raw_output {
        let ptr = if state.and_then(|s| s.get("output")).is_some() {
            "/state/output"
        } else {
            "/output"
        };
        (out, ptr)
    } else {
        ("", "/state/output")
    };

    let err_ptr = if state.and_then(|s| s.get("error")).is_some() {
        "/state/error"
    } else {
        "/error"
    };

    let combined;
    let (result_text, result_ptr): (&str, &str) =
        match (!base_output.is_empty(), !error_text.is_empty()) {
            (true, false) => (base_output, out_ptr),
            (false, true) => (&error_text, err_ptr),
            (true, true) => {
                combined = format!("{base_output}\n{error_text}");
                (&combined, out_ptr)
            }
            (false, false) => ("", out_ptr),
        };

    let mut result = tool_result_block(call_id, status, result_text, || {
        locator.content_ref(result_ptr)
    });
    if let SessionBlock::ToolResult {
        exit_code,
        duration_ms,
        images: result_images,
        ..
    } = &mut result
    {
        *exit_code = metadata
            .and_then(|m| m.get("exit"))
            .and_then(Value::as_i64)
            .and_then(|code| i32::try_from(code).ok());
        *duration_ms = state.and_then(time_span);
        *result_images = images;
    }
    blocks.push(result);
}

fn push_subtask_block(blocks: &mut Vec<SessionBlock>, locator: &PartLocator, part: &Value) {
    let agent = part
        .get("agent")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (desc, ptr) = if let Some(d) = part
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        (Some(d), "/description")
    } else if let Some(p) = part
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        (Some(p), "/prompt")
    } else if let Some(t) = part
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        (Some(t), "/text")
    } else {
        (None, "/description")
    };

    let label = match (agent, desc) {
        (Some(a), Some(d)) => format!("@{a}: {d}"),
        (None, Some(d)) => d.to_string(),
        (Some(a), None) => format!("@{a}"),
        (None, None) => "subtask".to_string(),
    };
    blocks.push(summary_event_block(EventKind::SubAgent, &label, || {
        locator.content_ref(ptr)
    }));
}

fn push_compaction_block(blocks: &mut Vec<SessionBlock>, locator: &PartLocator, part: &Value) {
    if let Some((summary, ptr)) = part
        .get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| (s, "/summary"))
        .or_else(|| {
            part.get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| (s, "/text"))
        })
    {
        blocks.push(summary_event_block(EventKind::Compaction, summary, || {
            locator.content_ref(ptr)
        }));
        return;
    }
    if let Some(err) = part.get("error").and_then(format_error_value) {
        blocks.push(SessionBlock::event(
            EventKind::Error,
            Some(format!("Compaction failed: {err}")),
            None,
        ));
        return;
    }
    blocks.push(SessionBlock::event(
        EventKind::Compaction,
        Some("Compaction".to_string()),
        None,
    ));
}

/// `patch{hash, files[]}`：步骤级文件变更快照。
fn push_patch_blocks(blocks: &mut Vec<SessionBlock>, part: &Value) {
    let files: Vec<&str> = part
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let Some(first) = files.first() else {
        return;
    };
    let id = part
        .get("id")
        .or_else(|| part.get("hash"))
        .and_then(Value::as_str)
        .unwrap_or("patch")
        .to_string();
    let input = part.get("files").cloned().unwrap_or(Value::Null);
    let mut call = tool_call_block(ToolSource::OpenCode, id.clone(), "patch", &input, || None);
    if let SessionBlock::ToolCall {
        title,
        detail,
        diff,
        ..
    } = &mut call
    {
        *title = title_path(first);
        *detail = (files.len() > 1).then(|| format!("{} 个文件", files.len()));
        let mut summary = single_file_diff(first, DiffOp::Update, 0, 0);
        summary.files = files
            .iter()
            .map(|path| single_file_diff(path, DiffOp::Update, 0, 0).files.remove(0))
            .collect();
        *diff = Some(summary);
    }
    blocks.push(call);
    blocks.push(tool_result_block(
        id,
        ToolStatus::Success,
        &files.join("\n"),
        || None,
    ));
}

/// `file{mime, filename, url}`：图片 → `SessionBlock::Image`，其余 → 文件名事件。
fn push_file_block(blocks: &mut Vec<SessionBlock>, locator: &PartLocator, part: &Value) {
    if let Some(image) = file_item_to_image(part, locator, "") {
        blocks.push(SessionBlock::Image { image });
        return;
    }
    if let Some(label) = file_item_label(part) {
        blocks.push(SessionBlock::event(EventKind::Other, Some(label), None));
    }
}

fn file_item_to_image(item: &Value, locator: &PartLocator, item_prefix: &str) -> Option<ImageRef> {
    let filename = item
        .get("filename")
        .or_else(|| item.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (url, url_key) = if let Some(u) = item.get("url").and_then(Value::as_str) {
        (u, "url")
    } else if let Some(u) = item.get("uri").and_then(Value::as_str) {
        (u, "uri")
    } else {
        ("", "url")
    };

    let explicit_mime = item
        .get("mime")
        .or_else(|| item.get("mimeType"))
        .or_else(|| item.get("mediaType"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let inferred_mime = explicit_mime.map(str::to_string).or_else(|| {
        if let Some(rest) = url.strip_prefix("data:") {
            let mime = rest.split([';', ',']).next().unwrap_or_default().trim();
            if !mime.is_empty() {
                return Some(mime.to_string());
            }
        }
        filename
            .or_else(|| (!url.is_empty()).then_some(url))
            .or_else(|| item.get("path").and_then(Value::as_str))
            .and_then(mime_from_extension)
            .map(str::to_string)
    })?;

    if !inferred_mime.starts_with("image/") {
        return None;
    }

    let source_and_size = if url.starts_with("file://") {
        let local = parse_file_url(url)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| url.strip_prefix("file://").unwrap_or(url).to_string());
        Some((ImageSource::LocalFile { path: local }, 0))
    } else if url.starts_with("data:") {
        let base64_len = url.split_once(',').map_or(0, |(_, data)| data.len());
        let ptr = format!("{item_prefix}/{url_key}");
        locator.content_ref(&ptr).map(|content| {
            (
                ImageSource::Inline { content },
                estimate_base64_size(base64_len),
            )
        })
    } else if let Some(data) = item.get("data").and_then(Value::as_str) {
        let ptr = format!("{item_prefix}/data");
        locator.content_ref(&ptr).map(|content| {
            (
                ImageSource::Inline { content },
                estimate_base64_size(data.len()),
            )
        })
    } else {
        item.pointer("/source/path")
            .or_else(|| item.get("path"))
            .and_then(Value::as_str)
            .filter(|p| !p.trim().is_empty())
            .map(|path| {
                (
                    ImageSource::LocalFile {
                        path: path.to_string(),
                    },
                    0,
                )
            })
    }?;

    Some(ImageRef {
        source: source_and_size.0,
        media_type: inferred_mime,
        size: source_and_size.1,
        alt: filename.map(str::to_string),
    })
}

fn mime_from_extension(path_or_url: &str) -> Option<&'static str> {
    let clean = path_or_url.split(['?', '#']).next().unwrap_or(path_or_url);
    let ext = clean.rsplit('.').next()?.to_ascii_lowercase();
    if ext.len() == clean.len() {
        return None;
    }
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

fn file_item_label(item: &Value) -> Option<String> {
    item.get("filename")
        .or_else(|| item.get("name"))
        .or_else(|| item.pointer("/source/path"))
        .or_else(|| item.get("path"))
        .or_else(|| item.get("uri"))
        .or_else(|| item.get("url"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with("data:"))
        .map(str::to_string)
}

/// 消息级 `error{name, data{message}}`：中断 → `Aborted`，其余 → `Error`。
fn message_error_event(info: &Value, msg_locator: &PartLocator) -> Option<SessionBlock> {
    let error = info.get("error")?;
    if error.is_null() {
        return None;
    }
    let name = error.get("name").and_then(Value::as_str).unwrap_or("");
    if name == "MessageAbortedError" {
        return Some(SessionBlock::event(EventKind::Aborted, None, None));
    }
    let (text, ptr) = if let Some(msg) = error
        .pointer("/data/message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        (msg.to_string(), "/error/data/message")
    } else if let Some(msg) = error
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        (msg.to_string(), "/error/message")
    } else {
        (format_error_value(error).unwrap_or_default(), "/error")
    };

    if text.is_empty() {
        Some(SessionBlock::event(EventKind::Error, None, None))
    } else {
        Some(summary_event_block(EventKind::Error, &text, || {
            msg_locator.content_ref(ptr)
        }))
    }
}

fn format_error_value(err: &Value) -> Option<String> {
    match err {
        Value::Null => None,
        Value::String(s) => {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Value::Object(map) => {
            let name = map
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let message = map
                .get("data")
                .and_then(|d| d.get("message"))
                .or_else(|| map.get("message"))
                .or_else(|| map.get("error"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            match (name, message) {
                (Some(n), Some(m)) => Some(format!("{n}: {m}")),
                (None, Some(m)) => Some(m.to_string()),
                (Some(n), None) => Some(n.to_string()),
                (None, None) => (!map.is_empty()).then(|| Value::Object(map.clone()).to_string()),
            }
        }
        other => {
            let rendered = other.to_string();
            (!rendered.trim().is_empty()).then_some(rendered)
        }
    }
}

fn format_model_switched(value: &Value) -> String {
    if let Some(model) = value.get("model") {
        match model {
            Value::String(s) if !s.trim().is_empty() => return s.trim().to_string(),
            Value::Object(map) => {
                let provider = map
                    .get("providerID")
                    .or_else(|| map.get("providerId"))
                    .or_else(|| map.get("provider_id"))
                    .or_else(|| map.get("provider"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let model_id = map
                    .get("modelID")
                    .or_else(|| map.get("modelId"))
                    .or_else(|| map.get("model_id"))
                    .or_else(|| map.get("id"))
                    .or_else(|| map.get("name"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                return match (provider, model_id) {
                    (Some(p), Some(m)) => format!("{p}/{m}"),
                    (None, Some(m)) => m.to_string(),
                    (Some(p), None) => p.to_string(),
                    (None, None) => model.to_string(),
                };
            }
            _ => {}
        }
    }
    let provider = value
        .get("providerID")
        .or_else(|| value.get("providerId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let model_id = value
        .get("modelID")
        .or_else(|| value.get("modelId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match (provider, model_id) {
        (Some(p), Some(m)) => format!("{p}/{m}"),
        (None, Some(m)) => m.to_string(),
        (Some(p), None) => p.to_string(),
        (None, None) => "unknown".to_string(),
    }
}

/// 消息级 `cost/tokens/modelID/providerID/time/finish` → [`MessageMeta`]（0 视为缺省）。
fn message_meta(info: &Value) -> Option<MessageMeta> {
    let tokens = info.get("tokens");
    let count = |pointer: &str| {
        tokens
            .and_then(|t| t.pointer(pointer))
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
    };
    let model_id = info
        .get("modelID")
        .or_else(|| info.pointer("/model/modelID"))
        .and_then(Value::as_str);
    let provider_id = info
        .get("providerID")
        .or_else(|| info.pointer("/model/providerID"))
        .and_then(Value::as_str);
    let model = match (provider_id, model_id) {
        (Some(p), Some(m)) if !p.is_empty() && !m.is_empty() => Some(format!("{p}/{m}")),
        (_, Some(m)) if !m.is_empty() => Some(m.to_string()),
        _ => None,
    };
    let created = info
        .pointer("/time/created")
        .and_then(parse_timestamp_to_ms);
    let completed = info
        .pointer("/time/completed")
        .and_then(parse_timestamp_to_ms);
    MessageMeta {
        model,
        input_tokens: count("/input"),
        output_tokens: count("/output"),
        cache_read_tokens: count("/cache/read"),
        cache_write_tokens: count("/cache/write"),
        reasoning_tokens: count("/reasoning"),
        cost_usd: info
            .get("cost")
            .and_then(Value::as_f64)
            .filter(|c| *c > 0.0),
        duration_ms: match (created, completed) {
            (Some(start), Some(end)) if end >= start => u64::try_from(end - start).ok(),
            _ => None,
        },
        stop_reason: info
            .get("finish")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    }
    .into_option()
}

/// `step-finish` 的 `tokens`：有 `total` 优先，否则将分项求和。
fn total_tokens(tokens: &Value) -> Option<u64> {
    if let Some(total) = tokens.get("total").and_then(Value::as_u64) {
        return (total > 0).then_some(total);
    }
    let sum: u64 = [
        "/input",
        "/output",
        "/reasoning",
        "/cache/read",
        "/cache/write",
    ]
    .iter()
    .filter_map(|p| tokens.pointer(p).and_then(Value::as_u64))
    .sum();
    (sum > 0).then_some(sum)
}

fn time_span(value: &Value) -> Option<u64> {
    let start = value.pointer("/time/start").and_then(Value::as_i64)?;
    let end = value.pointer("/time/end").and_then(Value::as_i64)?;
    u64::try_from(end - start).ok()
}

fn part_duration(part: &Value) -> Option<u64> {
    time_span(part).filter(|ms| *ms > 0)
}

fn saturate(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sqlite(id: &str) -> PartLocator {
        PartLocator::Sqlite {
            table: "part",
            id: id.into(),
            base: String::new(),
        }
    }

    fn msg_sqlite(id: &str) -> PartLocator {
        PartLocator::Sqlite {
            table: "message",
            id: id.into(),
            base: String::new(),
        }
    }

    #[test]
    fn tool_state_maps_to_call_result_and_attachments() {
        let long_output: String = (1..=40).map(|i| format!("{i:05}| line\n")).collect();
        let parts = vec![
            (
                sqlite("prt_1"),
                json!({"type":"tool","callID":"c1","tool":"bash","state":{
                    "status":"error","input":{"command":"bunx biome --version","description":"Check biome version"},
                    "output":"error: could not determine executable","title":"Check biome version",
                    "metadata":{"exit":1},"time":{"start":1000,"end":2312}}}),
            ),
            (
                sqlite("prt_2"),
                json!({"type":"tool","callID":"c2","tool":"read","state":{
                    "status":"completed","input":{"filePath":"/p/package.json"},
                    "output": long_output,"title":"package.json","time":{"start":1,"end":22},
                    "attachments":[{"mime":"image/png","filename":"preview.png","url":"data:image/png;base64,AAAAAAAA"}]}}),
            ),
            (
                sqlite("prt_3"),
                json!({"type":"tool","callID":"c3","tool":"task","state":{
                    "status":"running","input":{"description":"Find configs","subagent_type":"explore"}}}),
            ),
            (
                sqlite("prt_4"),
                json!({"type":"tool","callID":"c4","tool":"bash","state":{
                    "status":"error","input":{"command":"sleep 100"},"error":"Tool execution aborted"}}),
            ),
        ];
        let msgs = messages_from_parts(
            "assistant",
            Some("m".into()),
            None,
            &json!({}),
            &msg_sqlite("m"),
            &parts,
        );
        assert_eq!(msgs.len(), 1);
        let b = &msgs[0].blocks;
        assert_eq!(b.len(), 8);

        match &b[0] {
            SessionBlock::ToolCall {
                kind,
                title,
                detail,
                ..
            } => {
                assert_eq!(*kind, ToolKind::Shell);
                assert_eq!(title, "bunx biome --version");
                assert_eq!(detail.as_deref(), Some("Check biome version"));
            }
            other => panic!("{other:?}"),
        }
        match &b[1] {
            SessionBlock::ToolResult {
                call_id,
                status,
                exit_code,
                duration_ms,
                ..
            } => {
                assert_eq!(call_id, "c1");
                assert_eq!(*status, ToolStatus::Error);
                assert_eq!(*exit_code, Some(1));
                assert_eq!(*duration_ms, Some(1312));
            }
            other => panic!("{other:?}"),
        }
        match &b[3] {
            SessionBlock::ToolResult {
                status,
                truncated,
                full,
                line_count,
                images,
                ..
            } => {
                assert_eq!(*status, ToolStatus::Success);
                assert!(*truncated);
                assert_eq!(*line_count, 40);
                assert_eq!(
                    full,
                    &Some(ContentRef::Sqlite {
                        table: "part".into(),
                        id: "prt_2".into(),
                        column: "data".into(),
                        pointer: "/state/output".into(),
                    })
                );
                assert_eq!(images.len(), 1);
                assert_eq!(images[0].media_type, "image/png");
                assert_eq!(images[0].size, 6);
                assert_eq!(images[0].alt.as_deref(), Some("preview.png"));
                assert_eq!(
                    images[0].source,
                    ImageSource::Inline {
                        content: ContentRef::Sqlite {
                            table: "part".into(),
                            id: "prt_2".into(),
                            column: "data".into(),
                            pointer: "/state/attachments/0/url".into(),
                        }
                    }
                );
            }
            other => panic!("{other:?}"),
        }
        match (&b[4], &b[5]) {
            (
                SessionBlock::ToolCall {
                    kind,
                    title,
                    detail,
                    ..
                },
                SessionBlock::ToolResult { status, .. },
            ) => {
                assert_eq!(*kind, ToolKind::Agent);
                assert_eq!(title, "Find configs");
                assert_eq!(detail.as_deref(), Some("explore"));
                assert_eq!(*status, ToolStatus::Pending);
            }
            other => panic!("{other:?}"),
        }
        match &b[7] {
            SessionBlock::ToolResult {
                status, preview, ..
            } => {
                assert_eq!(*status, ToolStatus::Interrupted);
                assert_eq!(preview, "Tool execution aborted");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn steps_reasoning_synthetic_injected_subtask_compaction_and_meta() {
        let parts = vec![
            (sqlite("p0"), json!({"type":"agent","name":"explore"})),
            (sqlite("p1"), json!({"type":"step-start"})),
            (
                sqlite("p2"),
                json!({"type":"reasoning","text":"先确认 biome 是否已安装。","time":{"start":10,"end":1810}}),
            ),
            (
                sqlite("p3"),
                json!({"type":"reasoning","text":"","metadata":{"openai":{"reasoningEncryptedContent":"gAAA"}}}),
            ),
            (sqlite("p5"), json!({"type":"text","text":"done"})),
            (
                sqlite("p6"),
                json!({"type":"step-finish","reason":"stop","cost":0.0042,
                       "tokens":{"input":100,"output":10,"reasoning":0,"cache":{"read":22300,"write":0}}}),
            ),
            (
                sqlite("p7"),
                json!({"type":"step-finish","reason":"tool-calls","cost":0.0123,"tokens":{"total":18734}}),
            ),
        ];
        let info = json!({
            "role":"assistant","modelID":"claude-sonnet-4-5","providerID":"anthropic",
            "cost":0.0246,"finish":"stop","time":{"created":1791014404000_i64,"completed":1791014418200_i64},
            "tokens":{"input":61204,"output":1900,"reasoning":96,"cache":{"read":52100,"write":3010}}
        });
        let msgs = messages_from_parts(
            "assistant",
            Some("m_1".into()),
            Some(1000),
            &info,
            &msg_sqlite("m_1"),
            &parts,
        );
        assert_eq!(msgs.len(), 1);
        let msg = &msgs[0];
        assert_eq!(msg.content, "@explore\n\ndone");
        assert!(matches!(
            &msg.blocks[0],
            SessionBlock::Event { kind: EventKind::SubAgent, text: Some(t), .. } if t == "@explore"
        ));
        assert!(matches!(
            &msg.blocks[1],
            SessionBlock::Step {
                phase: StepPhase::Start,
                ..
            }
        ));
        assert!(matches!(
            &msg.blocks[2],
            SessionBlock::Thinking {
                duration_ms: Some(1800),
                redacted: false,
                ..
            }
        ));
        assert!(matches!(
            &msg.blocks[3],
            SessionBlock::Thinking { redacted: true, .. }
        ));
        assert_eq!(
            msg.blocks[5],
            SessionBlock::Step {
                phase: StepPhase::Finish,
                tokens: Some(22410),
                cost_usd: Some(0.0042),
                reason: Some("stop".into()),
            }
        );
        assert!(matches!(
            &msg.blocks[6],
            SessionBlock::Step {
                tokens: Some(18734),
                ..
            }
        ));
        let meta = msg.meta.as_ref().expect("meta");
        assert_eq!(meta.model.as_deref(), Some("anthropic/claude-sonnet-4-5"));
        assert_eq!(meta.input_tokens, Some(61204));
        assert_eq!(meta.output_tokens, Some(1900));
        assert_eq!(meta.reasoning_tokens, Some(96));
        assert_eq!(meta.cache_read_tokens, Some(52100));
        assert_eq!(meta.cache_write_tokens, Some(3010));
        assert_eq!(meta.cost_usd, Some(0.0246));
        assert_eq!(meta.duration_ms, Some(14200));
        assert_eq!(meta.stop_reason.as_deref(), Some("stop"));

        // Purely synthetic user message becomes injected: true
        let syn_msgs = messages_from_parts(
            "user",
            Some("u_syn".into()),
            Some(500),
            &json!({"role":"user"}),
            &msg_sqlite("u_syn"),
            &[(
                sqlite("ps"),
                json!({"type":"text","text":"Use the above summary","synthetic":true}),
            )],
        );
        assert_eq!(syn_msgs.len(), 1);
        assert!(syn_msgs[0].injected);
        assert_eq!(syn_msgs[0].id.as_deref(), Some("u_syn"));
        assert_eq!(syn_msgs[0].content, "Use the above summary");
    }

    #[test]
    fn edit_diff_patch_file_and_message_errors() {
        let parts = vec![
            (
                PartLocator::File {
                    rel_path: "part/msg_1/prt_1.json".into(),
                },
                json!({"type":"tool","callID":"e1","tool":"edit","state":{
                    "status":"completed","title":"package.json",
                    "input":{"filePath":"/p/package.json","oldString":"a","newString":"a,\nb"},
                    "output":"","metadata":{"diff":"--- a\n+++ b\n-a\n+a,\n+b\n"}}}),
            ),
            (
                sqlite("p2"),
                json!({"type":"patch","hash":"abc","files":["/p/a.ts","/p/b.ts"]}),
            ),
            (
                sqlite("p3"),
                json!({"type":"file","mime":"image/png","filename":"shot.png","url":"data:image/png;base64,AAAAAAAA"}),
            ),
            (
                sqlite("p4"),
                json!({"type":"file","mime":"text/plain","filename":"notes.txt","url":"file:///p/notes.txt"}),
            ),
        ];
        let info = json!({"error":{"name":"MessageAbortedError","data":{"message":"aborted"}}});
        let msgs = messages_from_parts("assistant", None, None, &info, &PartLocator::None, &parts);
        let msg = &msgs[0];
        match &msg.blocks[0] {
            SessionBlock::ToolCall {
                kind, title, diff, ..
            } => {
                assert_eq!(*kind, ToolKind::Edit);
                assert_eq!(title, "package.json");
                let diff = diff.as_ref().expect("diff");
                assert_eq!((diff.added, diff.removed), (2, 1));
                assert_eq!(diff.files[0].path, "/p/package.json");
                assert_eq!(
                    diff.full,
                    Some(ContentRef::File {
                        rel_path: "part/msg_1/prt_1.json".into(),
                        pointer: "/state/metadata/diff".into(),
                    })
                );
            }
            other => panic!("{other:?}"),
        }
        match &msg.blocks[2] {
            SessionBlock::ToolCall {
                kind,
                title,
                detail,
                diff,
                ..
            } => {
                assert_eq!(*kind, ToolKind::Edit);
                assert_eq!(title, "/p/a.ts");
                assert_eq!(detail.as_deref(), Some("2 个文件"));
                assert_eq!(diff.as_ref().map(|d| d.files.len()), Some(2));
            }
            other => panic!("{other:?}"),
        }
        match &msg.blocks[4] {
            SessionBlock::Image { image } => {
                assert_eq!(image.media_type, "image/png");
                assert_eq!(image.size, 6);
                assert!(matches!(
                    &image.source,
                    ImageSource::Inline { content: ContentRef::Sqlite { pointer, .. } } if pointer == "/url"
                ));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            &msg.blocks[5],
            SessionBlock::Event { kind: EventKind::Other, text: Some(t), .. } if t == "notes.txt"
        ));
        assert!(matches!(
            msg.blocks.last(),
            Some(SessionBlock::Event {
                kind: EventKind::Aborted,
                ..
            })
        ));

        let api_error =
            json!({"error":{"name":"APIError","data":{"message":"Model not supported"}}});
        let err_msgs = messages_from_parts(
            "assistant",
            None,
            None,
            &api_error,
            &msg_sqlite("m_err"),
            &[],
        );
        assert_eq!(err_msgs[0].content, "Model not supported");
    }

    #[test]
    fn v2_row_messages_maps_tools_compaction_shell_and_switches() {
        let assistant = v2_row_messages(
            "msg_2",
            "assistant",
            200,
            &json!({"content":[{"type":"tool","name":"shell","id":"call_1"}]}),
        );
        assert_eq!(assistant[0].content, "[Tool: shell] shell");
        assert!(matches!(
            &assistant[0].blocks[1],
            SessionBlock::ToolResult { call_id, status: ToolStatus::Unknown, .. } if call_id == "call_1"
        ));

        let synthetic = v2_row_messages(
            "msg_syn",
            "synthetic",
            300,
            &json!({"text":"injected instruction"}),
        );
        assert_eq!(synthetic.len(), 1);
        assert!(synthetic[0].injected);
        assert_eq!(synthetic[0].role, "system");
    }
}
