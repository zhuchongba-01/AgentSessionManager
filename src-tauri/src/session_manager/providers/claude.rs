use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;

use crate::session_manager::content::parse_file_url;
use crate::session_manager::model::{
    ContentRef, DiffFile, DiffOp, DiffSummary, EventKind, ImageRef, ImageSource, MessageMeta,
    SessionBlock, ToolKind, ToolStatus,
};
use crate::session_manager::{SessionMessage, SessionMeta};
use crate::session_paths::claude_config_dir;

use super::blocks::{
    assign_turn_ids, diff_from_input, estimate_base64_size, first_string_field, large_text_block,
    normalize_tool, one_line_title, preview, preview_chars, refine_shell_kind, split_mcp_name,
    summary_event_block, title_agent, title_ask, title_mcp, title_other, title_path, title_read,
    title_search, title_shell, title_todo, title_web, tool_result_block as make_tool_result_block,
    NormalizedTool, ToolSource, INPUT_PREVIEW_CHARS, THINKING_PREVIEW_CHARS,
};
use super::utils::{
    atomic_write_json, checked_storage_child, ensure_readable_size, extract_text,
    is_reparse_or_symlink, is_safe_session_id, parse_timestamp_to_ms, path_basename,
    read_head_tail_lines, remove_path_if_exists, scan_warning, truncate_summary, FileParseCache,
    JsonlSpan, LineSpans, TITLE_MAX_CHARS,
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

// ─── 会话正文解析（TranscriptBuilder 结构化块）─────────────────────────────
//
// 逐行读取 JSONL，用借用型的轻量结构体反序列化（只取需要的字段，图片 base64、
// 工具大输出等都只扫过不复制），再按记录类型转成结构化 blocks。大内容只放预览，
// 全文给 `ContentRef::Jsonl`（行的字节区间 + JSON Pointer）或 `ContentRef::Sidecar`，由前端按需取。

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    ensure_readable_size(path)?;
    let file = File::open(path).map_err(|e| format!("Failed to open session file: {e}"))?;
    let mut lines = LineSpans::new(BufReader::with_capacity(256 * 1024, file));
    let mut builder = TranscriptBuilder::new(path.with_extension(""));

    while let Some(span) = lines
        .next_line()
        .map_err(|e| format!("Failed to read Claude transcript: {e}"))?
    {
        if span.bytes.trim_ascii().is_empty() {
            continue;
        }
        let record: RawRecord = match serde_json::from_slice(span.bytes) {
            Ok(record) => record,
            Err(_) => continue,
        };
        builder.push(record, span.span);
    }

    Ok(builder.finish())
}

fn is_main_entry(entry: &Value) -> bool {
    entry.get("isMeta").and_then(Value::as_bool) != Some(true)
        && entry.get("isSidechain").and_then(Value::as_bool) != Some(true)
        && entry
            .get("teamName")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
}

/// 解析中的消息；全部记录处理完后再统一投影出 `content`（结果记录会回填前面的 ToolCall）。
struct Draft {
    role: String,
    ts: Option<i64>,
    id: Option<String>,
    injected: bool,
    blocks: Vec<SessionBlock>,
    meta: Option<MessageMeta>,
    /// assistant 的 API `message.id`：Claude Code 把一次回复按内容块拆成多条记录，
    /// 同 id 的相邻记录合并成一条消息
    api_id: Option<String>,
}

/// 已见过的 tool_use 在 `drafts` 中的位置，供结果记录配对与回填。
struct CallSlot {
    draft: usize,
    block: usize,
    ts: Option<i64>,
    name: String,
}

struct TranscriptBuilder {
    /// 会话附属目录 `<sessionId>/`（tool-results 等落盘文件）
    sidecar_dir: PathBuf,
    drafts: Vec<Draft>,
    calls: HashMap<String, CallSlot>,
    /// pr-link 会在每轮结束时重复写入，同一个 URL 只出一次
    pr_urls: HashSet<String>,
}

impl TranscriptBuilder {
    fn new(sidecar_dir: PathBuf) -> Self {
        Self {
            sidecar_dir,
            drafts: Vec::new(),
            calls: HashMap::new(),
            pr_urls: HashSet::new(),
        }
    }

    fn finish(self) -> Vec<SessionMessage> {
        let mut messages: Vec<SessionMessage> = self
            .drafts
            .into_iter()
            .map(|draft| {
                let mut message = SessionMessage::from_blocks(draft.role, draft.ts, draft.blocks);
                message.id = draft.id;
                message.injected = draft.injected;
                message.meta = draft.meta;
                message
            })
            .filter(|message| !message.is_empty())
            .collect();
        assign_turn_ids(&mut messages);
        messages
    }

    fn push(&mut self, record: RawRecord<'_>, line: JsonlSpan) {
        if is_true(&record.is_sidechain)
            || record
                .team_name
                .as_str()
                .is_some_and(|team| !team.is_empty())
        {
            return;
        }
        let is_meta = is_true(&record.is_meta);
        let ts = record.timestamp.as_ref().and_then(parse_timestamp_to_ms);

        match record.kind.as_str() {
            Some("system") if !is_meta => self.push_system(&record, ts, line),
            Some("pr-link") if !is_meta => self.push_pr_link(&record, ts),
            Some("system") | Some("pr-link") => {}
            Some("user") | Some("assistant") | None => {
                let kind = record.kind.as_str().map(str::to_string);
                let uuid = record.uuid.as_str().map(str::to_string);
                let thinking_ms = record
                    .thinking_metadata
                    .0
                    .as_ref()
                    .and_then(|m| m.duration_ms.as_ref())
                    .or(record.thinking_duration_ms.as_ref())
                    .and_then(as_u64);
                let compact_summary = is_true(&record.is_compact_summary);
                let tool_use_result = record.tool_use_result.0;
                let Some(message) = record.message.0 else {
                    return;
                };
                let role = message
                    .role
                    .as_str()
                    .map(str::to_string)
                    .or(kind)
                    .unwrap_or_else(|| "unknown".to_string());
                if compact_summary {
                    self.push_compact_summary(message.content, ts, line);
                } else if role == "user" {
                    self.push_user(message.content, tool_use_result, uuid, ts, line, is_meta);
                } else if !is_meta {
                    self.push_assistant(role, message, uuid, thinking_ms, ts, line);
                }
            }
            // attachment、file-history-*、queue-operation、mode、custom-title 等：跳过
            _ => {}
        }
    }

    fn push_draft(
        &mut self,
        role: &str,
        ts: Option<i64>,
        id: Option<String>,
        blocks: Vec<SessionBlock>,
    ) {
        self.drafts.push(Draft {
            role: role.to_string(),
            ts,
            id,
            injected: false,
            blocks,
            meta: None,
            api_id: None,
        });
    }

    fn push_event(
        &mut self,
        ts: Option<i64>,
        kind: EventKind,
        text: Option<String>,
        url: Option<String>,
    ) {
        self.push_draft(
            "system",
            ts,
            None,
            vec![SessionBlock::event(kind, text, url)],
        );
    }

    // ── system / pr-link ──

    fn push_system(&mut self, record: &RawRecord<'_>, ts: Option<i64>, line: JsonlSpan) {
        match record.subtype.as_str() {
            Some("stop_hook_summary") => {
                let errors: Vec<String> = match &record.hook_errors {
                    Some(Value::Array(items)) => items
                        .iter()
                        .map(|item| match item {
                            Value::String(s) => s.trim().to_string(),
                            other => other.to_string(),
                        })
                        .filter(|s| !s.is_empty())
                        .collect(),
                    _ => Vec::new(),
                };
                if !errors.is_empty() {
                    self.push_event(ts, EventKind::Hook, Some(errors.join("\n")), None);
                }
            }
            Some("compact_boundary") => {
                let text = record
                    .content
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Conversation compacted");
                self.push_event(ts, EventKind::Compaction, Some(text.to_string()), None);
            }
            Some("turn_duration") => {
                if let Some(ms) = record
                    .duration_ms
                    .as_ref()
                    .and_then(as_u64)
                    .filter(|&n| n > 0)
                {
                    if let Some(last_assistant) =
                        self.drafts.iter_mut().rev().find(|d| d.role == "assistant")
                    {
                        last_assistant
                            .meta
                            .get_or_insert_with(MessageMeta::default)
                            .duration_ms = Some(ms);
                    }
                }
            }
            // 新版 Claude Code 把本地斜杠命令记成 system.local_command
            Some("local_command") => {
                if let Some(content) = record.content.as_str() {
                    let uuid = record.uuid.as_str().map(str::to_string);
                    self.push_user_texts(
                        vec![(Cow::Borrowed(content), line.content_ref("/content"))],
                        Vec::new(),
                        Vec::new(),
                        uuid,
                        ts,
                        false,
                    );
                }
            }
            // api_error、informational、away_summary…：不展示
            _ => {}
        }
    }

    fn push_pr_link(&mut self, record: &RawRecord<'_>, ts: Option<i64>) {
        let Some(url) = record.pr_url.as_str().filter(|u| !u.trim().is_empty()) else {
            return;
        };
        if !self.pr_urls.insert(url.to_string()) {
            return;
        }
        let text = match record.pr_number.as_ref() {
            Some(Value::Number(n)) => format!("PR #{n}"),
            Some(Value::String(s)) if !s.is_empty() => format!("PR #{s}"),
            _ => "PR".to_string(),
        };
        self.push_event(ts, EventKind::PrLink, Some(text), Some(url.to_string()));
    }

    /// `/compact` 之后的摘要记录：并入紧邻的 compact_boundary 事件，没有则单独成一条。
    /// 摘要通常有数 KB，只放预览，全文按 `/message/content` 引用取。
    fn push_compact_summary(&mut self, content: RawContent<'_>, ts: Option<i64>, line: JsonlSpan) {
        let summary = content_text(&content);
        if summary.trim().is_empty() {
            return;
        }
        let block = summary_event_block(EventKind::Compaction, &summary, || {
            Some(line.content_ref("/message/content"))
        });
        if let Some(last) = self.drafts.last_mut() {
            if let [event @ SessionBlock::Event {
                kind: EventKind::Compaction,
                ..
            }] = last.blocks.as_mut_slice()
            {
                *event = block;
                return;
            }
        }
        self.push_draft("system", ts, None, vec![block]);
    }

    // ── assistant ──

    fn push_assistant(
        &mut self,
        role: String,
        message: RawMessage<'_>,
        uuid: Option<String>,
        thinking_ms: Option<u64>,
        ts: Option<i64>,
        line: JsonlSpan,
    ) {
        let mut blocks = Vec::new();
        // (块下标, tool_use id, 工具名)
        let mut calls: Vec<(usize, String, String)> = Vec::new();
        let mut last_server_tool_id: Option<String> = None;
        match message.content {
            RawContent::Text(text) => {
                if !text.trim().is_empty() {
                    blocks.push(SessionBlock::text(text.into_owned()));
                }
            }
            RawContent::Items(items) => {
                for (i, item) in items.into_iter().enumerate() {
                    let Some(item) = item.0 else { continue };
                    let item_kind = item.kind.as_str().unwrap_or_default().to_string();
                    match item_kind.as_str() {
                        "text" => {
                            if let Some(text) = item.text.0.filter(|t| !t.trim().is_empty()) {
                                blocks.push(SessionBlock::text(text.into_owned()));
                            }
                        }
                        kind @ ("thinking" | "redacted_thinking") => {
                            if let Some(block) = thinking_block(&item, kind, i, thinking_ms, line) {
                                blocks.push(block);
                            }
                        }
                        "tool_use" | "server_tool_use" | "mcp_tool_use" => {
                            let block = tool_call_block(item, &item_kind, i, line);
                            if let SessionBlock::ToolCall { id, raw_name, .. } = &block {
                                if !id.is_empty() {
                                    if item_kind == "server_tool_use" {
                                        last_server_tool_id = Some(id.clone());
                                    }
                                    calls.push((blocks.len(), id.clone(), raw_name.clone()));
                                }
                            }
                            blocks.push(block);
                        }
                        "web_search_tool_result" => {
                            let call_id = item
                                .tool_use_id
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                                .or_else(|| last_server_tool_id.take())
                                .unwrap_or_default();
                            let formatted = web_search_result_text(&item.content);
                            let status = if item.error.as_ref().is_some_and(|e| !e.is_null()) {
                                ToolStatus::Error
                            } else {
                                ToolStatus::Success
                            };
                            blocks.push(make_tool_result_block(
                                call_id,
                                status,
                                &formatted,
                                || Some(line.content_ref(format!("/message/content/{i}/content"))),
                            ));
                        }
                        "image" => {
                            if let Some(image) = image_ref_from_item(
                                &item,
                                format!("/message/content/{i}/source/data"),
                                line,
                            ) {
                                blocks.push(SessionBlock::Image { image });
                            }
                        }
                        _ => {}
                    }
                }
            }
            RawContent::None => {}
        }
        if blocks.is_empty() {
            return;
        }

        let meta = message_meta(&message.model, message.usage.as_ref(), &message.stop_reason);
        let api_id = message.id.as_str().map(str::to_string);
        let mergeable = self
            .drafts
            .last()
            .is_some_and(|last| last.role == role && api_id.is_some() && last.api_id == api_id);
        let (draft, base) = if mergeable {
            let last = self.drafts.last_mut().expect("checked above");
            let base = last.blocks.len();
            last.blocks.extend(blocks);
            merge_meta(&mut last.meta, meta);
            (self.drafts.len() - 1, base)
        } else {
            self.drafts.push(Draft {
                role,
                ts,
                id: uuid,
                injected: false,
                blocks,
                meta,
                api_id,
            });
            (self.drafts.len() - 1, 0)
        };
        for (index, id, name) in calls {
            self.calls.insert(
                id,
                CallSlot {
                    draft,
                    block: base + index,
                    ts,
                    name,
                },
            );
        }
    }

    // ── user（提问 / 工具结果 / 注入内容）──

    fn push_user(
        &mut self,
        content: RawContent<'_>,
        tool_use_result: Option<ToolUseResult>,
        uuid: Option<String>,
        ts: Option<i64>,
        line: JsonlSpan,
        is_meta: bool,
    ) {
        let mut results = Vec::new();
        let mut texts = Vec::new();
        let mut images = Vec::new();
        match content {
            RawContent::Text(text) => texts.push((text, line.content_ref("/message/content"))),
            RawContent::Items(items) => {
                let result_count = items
                    .iter()
                    .filter(|item| {
                        item.0.as_ref().is_some_and(|i| {
                            matches!(i.kind.as_str(), Some("tool_result" | "mcp_tool_result"))
                        })
                    })
                    .count();
                // 顶层 toolUseResult 只描述一个结果；多结果记录不知道属于谁，不用
                let tool_use_result = tool_use_result.filter(|_| result_count == 1);
                for (i, item) in items.into_iter().enumerate() {
                    let Some(item) = item.0 else { continue };
                    match item.kind.as_str().unwrap_or_default() {
                        "tool_result" | "mcp_tool_result" if !is_meta => {
                            results.push(self.tool_result_block(
                                item,
                                i,
                                tool_use_result.as_ref(),
                                ts,
                                line,
                            ));
                        }
                        "tool_result" | "mcp_tool_result" => {}
                        "text" => {
                            if let Some(text) = item.text.0 {
                                texts.push((
                                    text,
                                    line.content_ref(format!("/message/content/{i}/text")),
                                ));
                            }
                        }
                        "image" => {
                            if let Some(image) = image_ref_from_item(
                                &item,
                                format!("/message/content/{i}/source/data"),
                                line,
                            ) {
                                images.push(SessionBlock::Image { image });
                            }
                        }
                        _ => {}
                    }
                }
            }
            RawContent::None => {}
        }
        self.push_user_texts(texts, results, images, uuid, ts, is_meta);
    }

    /// 给用户文本归类：isMeta / 中断 / 斜杠命令 / `!` 命令 / 注入内容 / 普通提问。
    /// `texts` 每项带上它在源记录里的引用，超长注入文本只放预览 + 引用。
    fn push_user_texts(
        &mut self,
        texts: Vec<(Cow<'_, str>, ContentRef)>,
        mut blocks: Vec<SessionBlock>,
        images: Vec<SessionBlock>,
        uuid: Option<String>,
        ts: Option<i64>,
        is_meta: bool,
    ) {
        let has_results = !blocks.is_empty();
        let mut injected = false;

        let injected_blocks = |texts: &[(Cow<'_, str>, ContentRef)]| -> Vec<SessionBlock> {
            texts
                .iter()
                .filter(|(text, _)| !text.trim().is_empty())
                .map(|(text, full)| large_text_block(text.to_string(), || Some(full.clone())))
                .collect()
        };
        if is_meta {
            let meta_blocks = if has_results {
                Vec::new()
            } else {
                injected_blocks(&texts)
            };
            if !meta_blocks.is_empty() {
                injected = true;
                blocks.extend(meta_blocks);
            }
        } else if let Some(first) = texts.iter().map(|(t, _)| t.trim()).find(|t| !t.is_empty()) {
            if first.starts_with("[Request interrupted by user") {
                blocks.push(SessionBlock::event(
                    EventKind::Aborted,
                    Some(first.to_string()),
                    None,
                ));
            } else if let Some(command) = slash_command(first) {
                blocks.push(SessionBlock::event(
                    EventKind::SlashCommand,
                    Some(command),
                    None,
                ));
            } else if first.starts_with("<bash-input>") {
                blocks.extend(user_bash_blocks(first, uuid.as_deref()));
            } else if is_injected_text(first) {
                // 与工具结果同记录的注入文本直接丢弃；单独成条时整条标 injected、保留原文
                if !has_results {
                    injected = true;
                    blocks.extend(injected_blocks(&texts));
                }
            } else {
                let stripped: Vec<String> = texts
                    .iter()
                    .map(|(t, _)| strip_system_reminders(t))
                    .filter(|t| !t.trim().is_empty())
                    .collect();
                if !stripped.is_empty() {
                    blocks.extend(stripped.into_iter().map(SessionBlock::text));
                } else if !has_results && images.is_empty() {
                    // 全是 <system-reminder>：整条注入，保留原文
                    injected = true;
                    blocks.extend(injected_blocks(&texts));
                }
            }
        }
        blocks.extend(images);
        if blocks.is_empty() {
            return;
        }

        let all_results = blocks
            .iter()
            .all(|b| matches!(b, SessionBlock::ToolResult { .. }));
        let role = if all_results { "tool" } else { "user" };
        self.push_draft(role, ts, uuid, blocks);
        if let Some(last) = self.drafts.last_mut() {
            last.injected = injected;
        }
    }

    fn tool_result_block(
        &mut self,
        item: RawItem<'_>,
        index: usize,
        tool_use_result: Option<&ToolUseResult>,
        ts: Option<i64>,
        line: JsonlSpan,
    ) -> SessionBlock {
        let call_id = item.tool_use_id.as_str().unwrap_or_default().to_string();
        let base = format!("/message/content/{index}/content");
        let mut images = Vec::new();
        let (text, pointer): (Cow<'_, str>, Option<String>) = match item.content {
            RawContent::Text(text) => (text, Some(base)),
            RawContent::Items(parts) => {
                let mut texts: Vec<(usize, Cow<'_, str>)> = Vec::new();
                for (j, part) in parts.into_iter().enumerate() {
                    let Some(part) = part.0 else { continue };
                    match part.kind.as_str().unwrap_or_default() {
                        "text" => {
                            if let Some(text) = part.text.0 {
                                texts.push((j, text));
                            }
                        }
                        "image" => {
                            if let Some(image) =
                                image_ref_from_item(&part, format!("{base}/{j}/source/data"), line)
                            {
                                images.push(image);
                            }
                        }
                        _ => {}
                    }
                }
                match texts.len() {
                    0 => (Cow::Borrowed(""), None),
                    // 只有一段文本时直接指向字符串，回取时不用再拼接
                    1 => {
                        let (j, text) = texts.pop().expect("len checked");
                        (text, Some(format!("{base}/{j}/text")))
                    }
                    _ => {
                        let joined = texts
                            .iter()
                            .map(|(_, t)| t.as_ref())
                            .collect::<Vec<_>>()
                            .join("\n");
                        (Cow::Owned(joined), Some(base))
                    }
                }
            }
            RawContent::None => (Cow::Borrowed(""), None),
        };
        let text = if text.contains(SYSTEM_REMINDER_OPEN) {
            Cow::Owned(strip_system_reminders(&text))
        } else {
            text
        };
        let p = preview(&text);

        let is_error = item.is_error.as_ref().and_then(Value::as_bool) == Some(true);
        let interrupted = tool_use_result
            .and_then(|r| r.interrupted.as_ref())
            .and_then(Value::as_bool)
            == Some(true);
        let status = if interrupted {
            ToolStatus::Interrupted
        } else if is_error {
            ToolStatus::Error
        } else {
            ToolStatus::Success
        };

        let slot = self.calls.get(&call_id);
        let is_shell = slot.is_some_and(|s| s.name == "Bash");
        // 退出码与耗时只对 Bash 有意义（其余工具的「结果时间 - 调用时间」多是排队与思考时间）
        let exit_code = parse_exit_code(&text).or_else(|| {
            let background = tool_use_result.is_some_and(|r| r.background_task_id.is_some());
            (is_shell && status == ToolStatus::Success && !background).then_some(0)
        });
        let duration_ms = match (is_shell, slot.and_then(|s| s.ts), ts) {
            (true, Some(start), Some(end)) if end >= start => Some((end - start) as u64),
            _ => None,
        };

        let saved_path = tool_use_result
            .and_then(|r| r.persisted_output_path.as_ref())
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .map(str::to_string);
        let full = if p.truncated {
            saved_path
                .as_deref()
                .and_then(|path| self.sidecar_rel_path(path))
                .map(|rel_path| ContentRef::Sidecar { rel_path })
                .or_else(|| pointer.map(|ptr| line.content_ref(ptr)))
        } else {
            None
        };

        self.backfill_call(&call_id, status, tool_use_result);

        SessionBlock::ToolResult {
            call_id,
            status,
            preview: p.text,
            total_len: p.total_len,
            line_count: p.line_count,
            truncated: p.truncated,
            full,
            exit_code,
            duration_ms,
            images,
            saved_path,
        }
    }

    /// 结果到来时回填对应 ToolCall：Edit/MultiEdit/Write 用 `structuredPatch` 修正 diff，
    /// 失败的改动去掉 diff；Agent 用实际模型补 detail。
    fn backfill_call(&mut self, call_id: &str, status: ToolStatus, result: Option<&ToolUseResult>) {
        let Some(slot) = self.calls.get(call_id) else {
            return;
        };
        let Some(SessionBlock::ToolCall {
            raw_name,
            detail,
            diff,
            ..
        }) = self
            .drafts
            .get_mut(slot.draft)
            .and_then(|d| d.blocks.get_mut(slot.block))
        else {
            return;
        };

        match raw_name.as_str() {
            "Edit" | "MultiEdit" | "Write" => {
                if status != ToolStatus::Success {
                    *diff = None;
                    return;
                }
                let Some(result) = result else { return };
                let op = match result.kind.as_ref().and_then(Value::as_str) {
                    Some("create") => Some(DiffOp::Add),
                    Some("update") => Some(DiffOp::Update),
                    _ => None,
                };
                let path = result
                    .file_path
                    .as_ref()
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let patch = result.structured_patch.filter(|p| p.added + p.removed > 0);
                if op.is_none() && patch.is_none() {
                    return;
                }
                let summary = diff.get_or_insert_with(|| DiffSummary {
                    files: vec![DiffFile {
                        path: String::new(),
                        op: DiffOp::Update,
                        added: 0,
                        removed: 0,
                    }],
                    added: 0,
                    removed: 0,
                    full: None,
                });
                if let Some(file) = summary.files.first_mut() {
                    if let Some(op) = op {
                        file.op = op;
                    }
                    if let Some(path) = path {
                        file.path = path;
                    }
                    if let Some(patch) = patch {
                        file.added = patch.added;
                        file.removed = patch.removed;
                    }
                    summary.added = file.added;
                    summary.removed = file.removed;
                }
            }
            "Agent" | "Task" if detail.is_none() => {
                *detail = result
                    .and_then(|r| r.resolved_model.as_ref())
                    .and_then(Value::as_str)
                    .filter(|m| !m.is_empty())
                    .map(str::to_string);
            }
            _ => {}
        }
    }

    /// `persistedOutputPath` 在会话附属目录内时，转成相对路径（`ContentRef::Sidecar`）。
    fn sidecar_rel_path(&self, saved: &str) -> Option<String> {
        if let Ok(rel) = Path::new(saved).strip_prefix(&self.sidecar_dir) {
            let normalized = rel.to_string_lossy().replace('\\', "/");
            if !normalized.is_empty() {
                return Some(normalized);
            }
        }
        let canonical_base = self.sidecar_dir.canonicalize().ok()?;
        let canonical_saved = Path::new(saved).canonicalize().ok()?;
        let rel = canonical_saved.strip_prefix(&canonical_base).ok()?;
        let normalized = rel.to_string_lossy().replace('\\', "/");
        (!normalized.is_empty()).then_some(normalized)
    }
}

// ─── 块构造 ──────────────────────────────────────────────────────────────

const SYSTEM_REMINDER_OPEN: &str = "<system-reminder>";
const SYSTEM_REMINDER_CLOSE: &str = "</system-reminder>";

fn thinking_block(
    item: &RawItem<'_>,
    kind: &str,
    index: usize,
    duration_ms: Option<u64>,
    line: JsonlSpan,
) -> Option<SessionBlock> {
    let text = item.thinking.as_str().unwrap_or_default();
    let has_signature = item.signature.as_str().is_some_and(|s| !s.is_empty());
    let redacted = kind == "redacted_thinking" || (text.trim().is_empty() && has_signature);
    if text.trim().is_empty() && !redacted {
        return None;
    }
    let p = preview_chars(text, THINKING_PREVIEW_CHARS);
    Some(SessionBlock::Thinking {
        text: p.text,
        summary: None,
        redacted,
        duration_ms,
        full: p
            .truncated
            .then(|| line.content_ref(format!("/message/content/{index}/thinking"))),
    })
}

fn tool_call_block(
    item: RawItem<'_>,
    item_kind: &str,
    index: usize,
    line: JsonlSpan,
) -> SessionBlock {
    let id = item.id.as_str().unwrap_or_default().to_string();
    let raw_name = item.name.as_str().unwrap_or("unknown").to_string();
    let input = item.input.unwrap_or(Value::Null);
    let input_json = if input.is_null() {
        String::new()
    } else {
        serde_json::to_string(&input).unwrap_or_default()
    };
    let p = preview_chars(&input_json, INPUT_PREVIEW_CHARS);
    let mut normalized = normalize_tool(ToolSource::Claude, &raw_name, None);
    if item_kind == "mcp_tool_use" && normalized.kind != ToolKind::Mcp {
        normalized.kind = ToolKind::Mcp;
        normalized.server = item
            .server_name
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    } else if item_kind == "server_tool_use"
        && normalized.kind == ToolKind::Other
        && raw_name == "web_search"
    {
        normalized.kind = ToolKind::Web;
    }
    let (kind, title, detail) = claude_tool_title(&raw_name, &normalized, &input);
    let diff = matches!(raw_name.as_str(), "Edit" | "MultiEdit" | "Write")
        .then(|| diff_from_input(kind, &input))
        .flatten();

    SessionBlock::ToolCall {
        id,
        raw_name,
        kind,
        title,
        detail,
        server: normalized.server,
        input_preview: p.text,
        input_total_len: p.total_len,
        // 指向参数对象（不是字符串），取全文时由命令层序列化
        input_full: p
            .truncated
            .then(|| line.content_ref(format!("/message/content/{index}/input"))),
        diff,
        by_user: false,
    }
}

/// Claude Code desktop 标记章节的 MCP 工具名（完整名形如 `mcp__ccd_session__mark_chapter`）。
const MARK_CHAPTER_TOOL: &str = "mark_chapter";

/// 提炼 Claude 工具的 kind（Bash 细化为 search/read）、标题与次要信息。
fn claude_tool_title(
    raw_name: &str,
    normalized: &NormalizedTool,
    input: &Value,
) -> (ToolKind, String, Option<String>) {
    let s = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
    };
    let kind = normalized.kind;

    if kind == ToolKind::Mcp {
        let (server, tool) = split_mcp_name(raw_name)
            .unwrap_or((normalized.server.as_deref().unwrap_or_default(), raw_name));
        if tool == MARK_CHAPTER_TOOL {
            if let Some(title) = s("title") {
                return (
                    kind,
                    one_line_title(title),
                    s("summary").map(one_line_title),
                );
            }
        }
        let title = if server.is_empty() {
            one_line_title(tool)
        } else {
            title_mcp(server, tool)
        };
        return (kind, title, first_string_field(input));
    }

    match raw_name {
        "Bash" => {
            let command = s("command").unwrap_or_default();
            (
                refine_shell_kind(command),
                title_shell(command),
                s("description").map(one_line_title),
            )
        }
        "Read" => {
            let u = |key: &str| input.get(key).and_then(Value::as_u64);
            let (title, detail) =
                title_read(s("file_path").unwrap_or_default(), u("offset"), u("limit"));
            (kind, title, detail)
        }
        "Grep" => (
            kind,
            title_search(
                s("pattern").unwrap_or_default(),
                s("path").or_else(|| s("glob")),
            ),
            s("output_mode").map(str::to_string),
        ),
        "Glob" => (
            kind,
            title_search(s("pattern").unwrap_or_default(), s("path")),
            None,
        ),
        "ToolSearch" => (
            kind,
            title_search(s("query").unwrap_or_default(), None),
            None,
        ),
        "Edit" | "MultiEdit" | "Write" => {
            (kind, title_path(s("file_path").unwrap_or_default()), None)
        }
        "NotebookEdit" => (
            kind,
            title_path(s("notebook_path").unwrap_or_default()),
            None,
        ),
        "WebFetch" => (kind, title_web(&[s("url").unwrap_or_default()]), None),
        "WebSearch" | "web_search" => (kind, title_web(&[s("query").unwrap_or_default()]), None),
        "Agent" | "Task" => (
            kind,
            title_agent(s("description").or_else(|| s("prompt")).unwrap_or_default()),
            s("model").map(str::to_string),
        ),
        "AskUserQuestion" => {
            let question = input
                .get("questions")
                .and_then(Value::as_array)
                .and_then(|qs| qs.first())
                .and_then(|q| q.get("question"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            (kind, title_ask(question), None)
        }
        "TodoWrite" => {
            let count = input
                .get("todos")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            (kind, title_todo(count), None)
        }
        _ if kind == ToolKind::Agent => (
            kind,
            first_string_field(input).unwrap_or_else(|| title_other(raw_name)),
            None,
        ),
        _ => (kind, title_other(raw_name), first_string_field(input)),
    }
}

fn web_search_result_text(content: &RawContent<'_>) -> String {
    match content {
        RawContent::Text(text) => text.to_string(),
        RawContent::Items(items) => items
            .iter()
            .filter_map(|item| item.0.as_ref())
            .filter_map(|item| {
                let title = item.title.as_str().filter(|s| !s.is_empty());
                let url = item.url.as_str().filter(|s| !s.is_empty());
                match (title, url) {
                    (Some(t), Some(u)) => Some(format!("- {t} ({u})")),
                    (Some(t), None) => Some(format!("- {t}")),
                    (None, Some(u)) => Some(format!("- {u}")),
                    (None, None) => item.text.as_str().map(str::to_string),
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        RawContent::None => String::new(),
    }
}

fn image_ref_from_item(item: &RawItem<'_>, pointer: String, line: JsonlSpan) -> Option<ImageRef> {
    if let Some(source) = item.source.0.as_ref() {
        if source.kind.as_str() == Some("base64") {
            let data = source.data.as_str()?;
            return Some(ImageRef {
                source: ImageSource::Inline {
                    content: line.content_ref(pointer),
                },
                media_type: source
                    .media_type
                    .as_str()
                    .filter(|m| !m.is_empty())
                    .unwrap_or("application/octet-stream")
                    .to_string(),
                size: estimate_base64_size(data.len()),
                alt: None,
            });
        }
        if let Some(raw_path) = source
            .path
            .as_str()
            .or_else(|| source.url.as_str())
            .filter(|p| !p.trim().is_empty())
        {
            return local_file_image(raw_path, source.media_type.as_str());
        }
    }
    let raw_path = item
        .path
        .as_str()
        .or_else(|| item.file_path.as_str())
        .or_else(|| item.url.as_str())
        .filter(|p| !p.trim().is_empty())?;
    local_file_image(raw_path, None)
}

fn local_file_image(raw: &str, explicit_mime: Option<&str>) -> Option<ImageRef> {
    let resolved = if raw.starts_with("file://") {
        parse_file_url(raw)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| raw.strip_prefix("file://").unwrap_or(raw).to_string())
    } else {
        let bytes = raw.as_bytes();
        let is_win_abs = bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\');
        if raw.starts_with('/') || is_win_abs {
            raw.to_string()
        } else {
            return None;
        }
    };
    let media_type = match explicit_mime.filter(|m| !m.trim().is_empty()) {
        Some(mime) => mime.to_string(),
        None => {
            let ext = resolved
                .rsplit('.')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            match ext.as_str() {
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                "bmp" => "image/bmp",
                "svg" => "image/svg+xml",
                _ => "image/png",
            }
            .to_string()
        }
    };
    let size = std::fs::metadata(&resolved)
        .map(|m| u32::try_from(m.len()).unwrap_or(u32::MAX))
        .unwrap_or(0);
    Some(ImageRef {
        source: ImageSource::LocalFile { path: resolved },
        media_type,
        size,
        alt: None,
    })
}

/// 用户 `!cmd`：`<bash-input>cmd</bash-input><bash-stdout>…</bash-stdout><bash-stderr>…</bash-stderr>`
fn user_bash_blocks(text: &str, uuid: Option<&str>) -> Vec<SessionBlock> {
    let command = tag_content(text, "bash-input").unwrap_or_default();
    let id = format!("user-bash-{}", uuid.unwrap_or_default());
    let input = preview_chars(command, INPUT_PREVIEW_CHARS);
    let mut blocks = vec![SessionBlock::ToolCall {
        id: id.clone(),
        raw_name: "Bash".to_string(),
        kind: refine_shell_kind(command),
        title: title_shell(command),
        detail: None,
        server: None,
        input_preview: input.text,
        input_total_len: input.total_len,
        input_full: None,
        diff: None,
        by_user: true,
    }];

    let stdout = tag_content(text, "bash-stdout");
    let stderr = tag_content(text, "bash-stderr");
    if stdout.is_some() || stderr.is_some() {
        let stdout = stdout.unwrap_or_default().trim_end();
        let stderr = stderr.unwrap_or_default().trim_end();
        let output = match (stdout.is_empty(), stderr.is_empty()) {
            (_, true) => stdout.to_string(),
            (true, false) => stderr.to_string(),
            (false, false) => format!("{stdout}\n{stderr}"),
        };
        let p = preview(&output);
        blocks.push(SessionBlock::ToolResult {
            call_id: id,
            // 只有 stderr 不代表失败，状态未知
            status: if stderr.is_empty() {
                ToolStatus::Success
            } else {
                ToolStatus::Unknown
            },
            preview: p.text,
            total_len: p.total_len,
            line_count: p.line_count,
            truncated: p.truncated,
            full: None,
            exit_code: None,
            duration_ms: None,
            images: Vec::new(),
            saved_path: None,
        });
    }
    blocks
}

fn message_meta(
    model: &LStr<'_>,
    usage: Option<&Value>,
    stop_reason: &LStr<'_>,
) -> Option<MessageMeta> {
    // 0 视为缺省，省掉序列化体积
    let tokens = |key: &str| {
        usage
            .and_then(|u| u.get(key))
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
    };
    let meta = MessageMeta {
        model: model
            .as_str()
            .filter(|m| !m.is_empty() && *m != "<synthetic>")
            .map(str::to_string),
        input_tokens: tokens("input_tokens"),
        output_tokens: tokens("output_tokens"),
        cache_read_tokens: tokens("cache_read_input_tokens"),
        cache_write_tokens: tokens("cache_creation_input_tokens"),
        stop_reason: stop_reason.as_str().map(str::to_string),
        ..MessageMeta::default()
    };
    (meta != MessageMeta::default()).then_some(meta)
}

/// 同一回复拆成的多条记录：后到的非空字段覆盖先前的（usage 以最后一块为准）。
fn merge_meta(target: &mut Option<MessageMeta>, next: Option<MessageMeta>) {
    let Some(next) = next else { return };
    let Some(current) = target.as_mut() else {
        *target = Some(next);
        return;
    };
    macro_rules! take {
        ($($field:ident),*) => {$(
            if next.$field.is_some() {
                current.$field = next.$field;
            }
        )*};
    }
    take!(
        model,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        duration_ms,
        stop_reason
    );
}

// ─── 文本规则 ────────────────────────────────────────────────────────────

/// 去掉所有 `<system-reminder>…</system-reminder>` 片段；没有闭合标签时去到末尾。
fn strip_system_reminders(text: &str) -> String {
    if !text.contains(SYSTEM_REMINDER_OPEN) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(SYSTEM_REMINDER_OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + SYSTEM_REMINDER_OPEN.len()..];
        rest = match after.find(SYSTEM_REMINDER_CLOSE) {
            Some(end) => &after[end + SYSTEM_REMINDER_CLOSE.len()..],
            None => "",
        };
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// 系统/工具注入、不是用户亲手输入的文本。
fn is_injected_text(text: &str) -> bool {
    text.contains("<local-command-caveat>")
        || text.starts_with("<local-command-stdout>")
        || text.starts_with("<local-command-stderr>")
        || text.starts_with("<task-notification>")
}

/// `<command-name>/compact</command-name><command-args>…</command-args>` → `/compact …`
fn slash_command(text: &str) -> Option<String> {
    if !text.starts_with("<command-") {
        return None;
    }
    let name = tag_content(text, "command-name")?.trim();
    if name.is_empty() {
        return None;
    }
    let args = tag_content(text, "command-args").unwrap_or_default().trim();
    Some(if args.is_empty() {
        name.to_string()
    } else {
        format!("{name} {args}")
    })
}

/// 取 `<tag>…</tag>` 之间的内容（第一处）。
fn tag_content<'t>(text: &'t str, tag: &str) -> Option<&'t str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close).map_or(text.len(), |i| start + i);
    Some(&text[start..end])
}

/// `Exit code N` 开头的工具输出（Claude Bash 失败时的格式）
fn parse_exit_code(text: &str) -> Option<i32> {
    let first = text.trim_start().lines().next()?;
    let rest = first.strip_prefix("Error: ").unwrap_or(first);
    rest.strip_prefix("Exit code ")?.trim().parse().ok()
}

fn content_text(content: &RawContent<'_>) -> String {
    match content {
        RawContent::Text(text) => text.to_string(),
        RawContent::Items(items) => items
            .iter()
            .filter_map(|item| item.0.as_ref())
            .filter(|item| item.kind.as_str() == Some("text"))
            .filter_map(|item| item.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        RawContent::None => String::new(),
    }
}

fn is_true(value: &Option<Value>) -> bool {
    value.as_ref().and_then(Value::as_bool) == Some(true)
}

fn as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_f64().filter(|n| *n >= 0.0).map(|n| n as u64))
}

// ─── 原始记录（只声明用到的字段，其余由 serde 跳过）──────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRecord<'a> {
    #[serde(rename = "type", default, borrow)]
    kind: LStr<'a>,
    #[serde(default, borrow)]
    subtype: LStr<'a>,
    #[serde(default)]
    is_meta: Option<Value>,
    #[serde(default)]
    is_sidechain: Option<Value>,
    #[serde(default, borrow)]
    team_name: LStr<'a>,
    #[serde(default)]
    is_compact_summary: Option<Value>,
    #[serde(default, borrow)]
    uuid: LStr<'a>,
    #[serde(default)]
    timestamp: Option<Value>,
    #[serde(default, borrow)]
    message: Lenient<RawMessage<'a>>,
    #[serde(default)]
    tool_use_result: Lenient<ToolUseResult>,
    #[serde(default)]
    thinking_duration_ms: Option<Value>,
    #[serde(default)]
    thinking_metadata: Lenient<RawThinkingMetadata>,
    #[serde(default)]
    duration_ms: Option<Value>,
    #[serde(default)]
    hook_errors: Option<Value>,
    /// system 记录的正文（compact_boundary / local_command）
    #[serde(default, borrow)]
    content: LStr<'a>,
    #[serde(default, borrow)]
    pr_url: LStr<'a>,
    #[serde(default)]
    pr_number: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThinkingMetadata {
    #[serde(default)]
    duration_ms: Option<Value>,
}

#[derive(Deserialize)]
struct RawMessage<'a> {
    #[serde(default, borrow)]
    role: LStr<'a>,
    #[serde(default, borrow)]
    id: LStr<'a>,
    #[serde(default, borrow)]
    model: LStr<'a>,
    #[serde(default, borrow)]
    stop_reason: LStr<'a>,
    #[serde(default)]
    usage: Option<Value>,
    #[serde(default, borrow)]
    content: RawContent<'a>,
}

/// `message.content[]` 与 `tool_result.content[]` 的元素（字段取并集）
#[derive(Deserialize)]
struct RawItem<'a> {
    #[serde(rename = "type", default, borrow)]
    kind: LStr<'a>,
    #[serde(default, borrow)]
    text: LStr<'a>,
    #[serde(default, borrow)]
    thinking: LStr<'a>,
    #[serde(default, borrow)]
    signature: LStr<'a>,
    #[serde(default, borrow)]
    id: LStr<'a>,
    #[serde(default, borrow)]
    name: LStr<'a>,
    #[serde(default, borrow)]
    server_name: LStr<'a>,
    #[serde(default)]
    input: Option<Value>,
    #[serde(default, borrow)]
    tool_use_id: LStr<'a>,
    #[serde(default)]
    is_error: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
    #[serde(default, borrow)]
    title: LStr<'a>,
    #[serde(default, borrow)]
    url: LStr<'a>,
    #[serde(default, borrow)]
    path: LStr<'a>,
    #[serde(default, borrow)]
    file_path: LStr<'a>,
    #[serde(default, borrow)]
    content: RawContent<'a>,
    #[serde(default, borrow)]
    source: Lenient<RawSource<'a>>,
}

#[derive(Deserialize)]
struct RawSource<'a> {
    #[serde(rename = "type", default, borrow)]
    kind: LStr<'a>,
    #[serde(default, borrow)]
    media_type: LStr<'a>,
    #[serde(default, borrow)]
    data: LStr<'a>,
    #[serde(default, borrow)]
    path: LStr<'a>,
    #[serde(default, borrow)]
    url: LStr<'a>,
}

/// 顶层 `toolUseResult`：只取回填需要的字段（stdout、file、originalFile 等大字段跳过）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolUseResult {
    #[serde(default)]
    interrupted: Option<Value>,
    #[serde(default)]
    persisted_output_path: Option<Value>,
    #[serde(rename = "type", default)]
    kind: Option<Value>,
    #[serde(default)]
    file_path: Option<Value>,
    #[serde(default)]
    resolved_model: Option<Value>,
    #[serde(default)]
    background_task_id: Option<Value>,
    #[serde(default)]
    structured_patch: Option<PatchCounts>,
}

/// 宽松的借用字符串：非字符串值当作 `None`。
#[derive(Default)]
struct LStr<'a>(Option<Cow<'a, str>>);

impl LStr<'_> {
    fn as_str(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

/// 只接受对象的宽松包装：其他类型当作 `None`。
struct Lenient<T>(Option<T>);

impl<T> Default for Lenient<T> {
    fn default() -> Self {
        Self(None)
    }
}

/// 字符串或块数组（`message.content` / `tool_result.content`）
#[derive(Default)]
enum RawContent<'a> {
    #[default]
    None,
    Text(Cow<'a, str>),
    Items(Vec<Lenient<RawItem<'a>>>),
}

/// `structuredPatch` 里以 `+` / `-` 开头的行数
#[derive(Clone, Copy, Default)]
struct PatchCounts {
    added: u32,
    removed: u32,
}

macro_rules! skip_other_values {
    ($value:expr) => {
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok($value)
        }
    };
}

macro_rules! skip_str_values {
    ($value:expr) => {
        fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
            Ok($value)
        }
    };
}

macro_rules! skip_seq_values {
    ($value:expr) => {
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok($value)
        }
    };
}

macro_rules! skip_map_values {
    ($value:expr) => {
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok($value)
        }
    };
}

impl<'de: 'a, 'a> Deserialize<'de> for LStr<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V<'a>(PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
            type Value = LStr<'a>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any value")
            }
            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(LStr(Some(Cow::Borrowed(v))))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(LStr(Some(Cow::Owned(v.to_string()))))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(LStr(Some(Cow::Owned(v))))
            }
            skip_other_values!(LStr(None));
            skip_seq_values!(LStr(None));
            skip_map_values!(LStr(None));
        }
        deserializer.deserialize_any(V(PhantomData))
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Lenient<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = Lenient<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any value")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(de::value::MapAccessDeserializer::new(map)).map(|v| Lenient(Some(v)))
            }
            skip_other_values!(Lenient(None));
            skip_str_values!(Lenient(None));
            skip_seq_values!(Lenient(None));
        }
        deserializer.deserialize_any(V(PhantomData))
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for RawContent<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V<'a>(PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
            type Value = RawContent<'a>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("string or array")
            }
            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(RawContent::Text(Cow::Borrowed(v)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(RawContent::Text(Cow::Owned(v.to_string())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(RawContent::Text(Cow::Owned(v)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(item) = seq.next_element::<Lenient<RawItem<'a>>>()? {
                    items.push(item);
                }
                Ok(RawContent::Items(items))
            }
            skip_other_values!(RawContent::None);
            skip_map_values!(RawContent::None);
        }
        deserializer.deserialize_any(V(PhantomData))
    }
}

impl<'de> Deserialize<'de> for PatchCounts {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Hunk;
        impl<'de> Visitor<'de> for Hunk {
            type Value = PatchCounts;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("hunk")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut counts = PatchCounts::default();
                while let Some(key) = map.next_key::<LStr<'_>>()? {
                    if key.as_str() == Some("lines") {
                        counts = map.next_value_seed(Lines)?;
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(counts)
            }
            skip_other_values!(PatchCounts::default());
            skip_str_values!(PatchCounts::default());
            skip_seq_values!(PatchCounts::default());
        }

        struct Lines;
        impl<'de> de::DeserializeSeed<'de> for Lines {
            type Value = PatchCounts;
            fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
                d.deserialize_any(self)
            }
        }
        impl<'de> Visitor<'de> for Lines {
            type Value = PatchCounts;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("lines")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut counts = PatchCounts::default();
                while let Some(line) = seq.next_element::<LStr<'_>>()? {
                    match line.as_str().and_then(|l| l.chars().next()) {
                        Some('+') => counts.added += 1,
                        Some('-') => counts.removed += 1,
                        _ => {}
                    }
                }
                Ok(counts)
            }
            skip_other_values!(PatchCounts::default());
            skip_str_values!(PatchCounts::default());
            skip_map_values!(PatchCounts::default());
        }

        struct Hunks;
        impl<'de> Visitor<'de> for Hunks {
            type Value = PatchCounts;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("structuredPatch")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut total = PatchCounts::default();
                while let Some(hunk) = seq.next_element_seed(HunkSeed)? {
                    total.added += hunk.added;
                    total.removed += hunk.removed;
                }
                Ok(total)
            }
            skip_other_values!(PatchCounts::default());
            skip_str_values!(PatchCounts::default());
            skip_map_values!(PatchCounts::default());
        }
        struct HunkSeed;
        impl<'de> de::DeserializeSeed<'de> for HunkSeed {
            type Value = PatchCounts;
            fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
                d.deserialize_any(Hunk)
            }
        }

        deserializer.deserialize_any(Hunks)
    }
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
    fn sidechain_and_team_entries_are_excluded_while_is_meta_marks_injected() {
        let rows = vec![
            json!({"type":"user","uuid":"u","message":{"role":"user","content":"root"}}),
            json!({"type":"assistant","uuid":"a","parentUuid":"u","message":{"id":"m1","role":"assistant","content":"latest"}}),
            json!({"type":"progress","uuid":"p","parentUuid":"a"}),
            json!({"type":"user","uuid":"side","isSidechain":true,"message":{"role":"user","content":"side"}}),
            json!({"type":"user","uuid":"team","teamName":"team","message":{"role":"user","content":"team"}}),
            json!({"type":"user","uuid":"meta","isMeta":true,"message":{"role":"user","content":"expanded skill prompt"}}),
        ];
        let messages = compat_load(rows);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].content, "root");
        assert!(!messages[0].injected);
        assert_eq!(messages[1].content, "latest");
        assert!(!messages[1].injected);
        assert_eq!(messages[2].content, "expanded skill prompt");
        assert!(messages[2].injected);
        assert_eq!(messages[2].turn_id.as_deref(), Some("t1"));
    }

    #[test]
    fn adjacent_same_message_id_records_merge_blocks_and_meta() {
        let rows = vec![
            json!({"type":"user","uuid":"u1","message":{"role":"user","content":"hi"}}),
            json!({
                "type":"assistant",
                "uuid":"a1",
                "thinkingMetadata":{"durationMs":2400},
                "message":{
                    "id":"msg_same",
                    "role":"assistant",
                    "model":"claude-opus-4-6",
                    "usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":100,"cache_creation_input_tokens":20},
                    "content":[{"type":"thinking","thinking":"plan step","signature":"sig1"}]
                }
            }),
            json!({
                "type":"assistant",
                "uuid":"a2",
                "message":{
                    "id":"msg_same",
                    "role":"assistant",
                    "model":"claude-opus-4-6",
                    "stop_reason":"tool_use",
                    "usage":{"input_tokens":10,"output_tokens":42,"cache_read_input_tokens":100,"cache_creation_input_tokens":20},
                    "content":[
                        {"type":"text","text":"Let me read the files."},
                        {"type":"tool_use","id":"c1","name":"Read","input":{"file_path":"/p/a.rs"}},
                        {"type":"tool_use","id":"c2","name":"Grep","input":{"pattern":"foo","path":"src"}}
                    ]
                }
            }),
            json!({"type":"system","subtype":"turn_duration","durationMs":3750}),
            json!({"type":"user","uuid":"r1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","content":"first result"}]}}),
            json!({"type":"user","uuid":"r2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c2","content":"second result"}]}}),
            json!({"type":"assistant","uuid":"end","message":{"id":"msg_next","role":"assistant","content":"done"}}),
        ];
        let messages = compat_load(rows);
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[1].id.as_deref(), Some("a1"));
        assert_eq!(messages[1].blocks.len(), 4);
        assert!(matches!(
            &messages[1].blocks[0],
            SessionBlock::Thinking { text, duration_ms: Some(2400), redacted: false, .. }
                if text == "plan step"
        ));
        assert!(matches!(
            &messages[1].blocks[1],
            SessionBlock::Text { text, .. } if text == "Let me read the files."
        ));
        assert!(matches!(
            &messages[1].blocks[2],
            SessionBlock::ToolCall { id, kind: ToolKind::Read, .. } if id == "c1"
        ));
        assert!(matches!(
            &messages[1].blocks[3],
            SessionBlock::ToolCall { id, kind: ToolKind::Search, .. } if id == "c2"
        ));
        let meta = messages[1].meta.as_ref().expect("merged meta");
        assert_eq!(meta.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(meta.input_tokens, Some(10));
        assert_eq!(meta.output_tokens, Some(42));
        assert_eq!(meta.cache_read_tokens, Some(100));
        assert_eq!(meta.cache_write_tokens, Some(20));
        assert_eq!(meta.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(meta.duration_ms, Some(3750));
        assert_eq!(messages[2].role, "tool");
        assert_eq!(messages[2].content, "first result");
        assert_eq!(messages[3].role, "tool");
        assert_eq!(messages[3].content, "second result");
        assert_eq!(messages[4].content, "done");
    }

    #[test]
    fn compact_boundary_merges_with_following_compact_summary() {
        let rows = vec![
            json!({"message":{"role":"user","content":"before compact"}}),
            json!({"type":"system","uuid":"boundary","subtype":"compact_boundary","content":"Conversation compacted"}),
            json!({"type":"user","uuid":"summary","isCompactSummary":true,"message":{"role":"user","content":"Compacted summary text"}}),
            json!({"type":"assistant","uuid":"end","message":{"role":"assistant","content":"answer"}}),
        ];
        let messages = compat_load(rows);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].content, "before compact");
        assert_eq!(messages[1].role, "system");
        assert!(matches!(
            &messages[1].blocks[0],
            SessionBlock::Event { kind: EventKind::Compaction, text: Some(t), .. }
                if t == "Compacted summary text"
        ));
        assert_eq!(messages[1].content, "Compacted summary text");
        assert_eq!(messages[2].content, "answer");
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
    fn load_messages_emits_thinking_and_handles_rich_blocks() {
        let messages = compat_load(vec![
            json!({
                "type": "user",
                "uuid": "u1",
                "message": {
                    "role": "user",
                    "content": [
                        {"type": "image", "source": {"type": "file", "path": "/tmp/diagram.png"}},
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
                        {"type": "server_tool_use", "id": "srv_1", "name": "web_search", "input": {"query": "docs"}},
                        {
                            "type": "web_search_tool_result",
                            "content": [{"title": "Docs", "url": "https://docs.example.com"}]
                        },
                        {"type": "mcp_tool_use", "id": "mcp_1", "name": "linear_get_issue", "server_name": "linear"},
                        {"type": "text", "text": "All set."}
                    ]
                }
            }),
        ]);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert!(messages[0]
            .content
            .starts_with("Inspect these\n\n[Image: image/png"));
        assert!(matches!(
            &messages[0].blocks[1],
            SessionBlock::Image { image } if matches!(&image.source, ImageSource::LocalFile { path } if path == "/tmp/diagram.png")
        ));

        assert_eq!(messages[1].role, "assistant");
        assert!(matches!(
            &messages[1].blocks[0],
            SessionBlock::Thinking { text, redacted: false, .. } if text == "Analyzing attachments"
        ));
        assert!(matches!(
            &messages[1].blocks[1],
            SessionBlock::ToolCall { id, kind: ToolKind::Web, title, .. }
                if id == "srv_1" && title == "docs"
        ));
        assert!(matches!(
            &messages[1].blocks[2],
            SessionBlock::ToolResult { call_id, preview, .. }
                if call_id == "srv_1" && preview == "- Docs (https://docs.example.com)"
        ));
        assert!(matches!(
            &messages[1].blocks[3],
            SessionBlock::ToolCall { id, kind: ToolKind::Mcp, server: Some(srv), title, .. }
                if id == "mcp_1" && srv == "linear" && title == "linear.linear_get_issue"
        ));
        assert_eq!(
            messages[1].content,
            "[Tool: web_search] docs\n\n- Docs (https://docs.example.com)\n\n[Tool: linear_get_issue] linear.linear_get_issue\n\nAll set."
        );
    }

    fn parse_claude_lines(
        lines: &[Value],
    ) -> (tempfile::TempDir, PathBuf, Vec<SessionMessage>, Vec<u64>) {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        let mut body = String::new();
        let mut offsets = Vec::new();
        for line in lines {
            offsets.push(body.len() as u64);
            body.push_str(&serde_json::to_string(line).unwrap());
            body.push('\n');
        }
        std::fs::write(&path, body).expect("write");
        let messages = load_messages(&path).expect("load");
        (temp, path, messages, offsets)
    }

    fn claude_user(uuid: &str, content: Value) -> Value {
        json!({
            "type": "user",
            "uuid": uuid,
            "timestamp": "2026-10-03T10:00:00Z",
            "message": { "role": "user", "content": content }
        })
    }

    fn claude_assistant(uuid: &str, msg_id: &str, content: Value) -> Value {
        json!({
            "type": "assistant",
            "uuid": uuid,
            "timestamp": "2026-10-03T10:00:01Z",
            "message": {
                "id": msg_id,
                "role": "assistant",
                "model": "claude-opus-4-6",
                "content": content
            }
        })
    }

    #[test]
    fn structured_patch_mark_chapter_sidecar_slash_command_and_stop_hook_summary() {
        use crate::session_manager::model::project_content;

        let long_out: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        let img = "QUJD".repeat(500);
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        let sidecar = path.with_extension("");
        let persisted = sidecar
            .join("tool-results")
            .join("toolu_bash.txt")
            .to_string_lossy()
            .into_owned();

        let mut edit_result = claude_user(
            "u2",
            json!([{ "type": "tool_result", "tool_use_id": "toolu_edit", "content": "Updated /p/a.rs" }]),
        );
        edit_result["toolUseResult"] = json!({
            "filePath": "/p/a.rs",
            "structuredPatch": [
                { "lines": [" a", "-b", "+c", "+d"] },
                { "lines": ["-e", "+f"] }
            ]
        });

        let mut bash_result = claude_user(
            "u3",
            json!([{ "type": "tool_result", "tool_use_id": "toolu_bash", "content": long_out }]),
        );
        bash_result["toolUseResult"] = json!({
            "interrupted": true,
            "persistedOutputPath": persisted
        });

        let lines = vec![
            claude_user("u1", json!([
                { "type": "text", "text": "<system-reminder>opened file</system-reminder>\n\nFix bug" },
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": img } }
            ])),
            claude_assistant(
                "a1",
                "m1",
                json!([
                    { "type": "thinking", "thinking": "t".repeat(THINKING_PREVIEW_CHARS + 40), "signature": "sig" },
                    { "type": "redacted_thinking", "thinking": "" },
                    { "type": "tool_use", "id": "toolu_chap", "name": "mcp__ccd_session__mark_chapter", "input": { "title": "修复阶段", "summary": "更新配置\n补充单测" } },
                    { "type": "tool_use", "id": "toolu_edit", "name": "Edit", "input": { "file_path": "/p/a.rs", "old_string": "b", "new_string": "c" } },
                    { "type": "tool_use", "id": "toolu_bash", "name": "Bash", "input": { "command": "cargo test" } }
                ]),
            ),
            edit_result,
            bash_result,
            claude_user(
                "u4",
                json!("<command-name>/compact</command-name>\n<command-args>keep context</command-args>"),
            ),
            claude_user(
                "u5",
                json!("<local-command-caveat>Caveat: local command</local-command-caveat>"),
            ),
            claude_user(
                "u6",
                json!("<bash-input>git status</bash-input><bash-stdout>clean</bash-stdout><bash-stderr></bash-stderr>"),
            ),
            claude_user("u7", json!("[Request interrupted by user for tool use]")),
            json!({ "type": "system", "subtype": "stop_hook_summary", "hookErrors": [] }),
            json!({ "type": "system", "subtype": "stop_hook_summary", "hookErrors": ["hook.sh exited with code 1"] }),
            json!({ "type": "pr-link", "prNumber": 42, "prUrl": "https://github.com/o/r/pull/42" }),
            json!({ "type": "pr-link", "prNumber": 42, "prUrl": "https://github.com/o/r/pull/42" }),
        ];

        let mut body = String::new();
        for line in &lines {
            body.push_str(&serde_json::to_string(line).unwrap());
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        let msgs = load_messages(&path).unwrap();

        // Turn 1: user prompt stripped reminder + inline image
        assert_eq!(msgs[0].turn_id.as_deref(), Some("t1"));
        assert!(!msgs[0].injected);
        assert!(msgs[0].content.starts_with("Fix bug\n\n[Image: image/png"));
        assert!(matches!(
            &msgs[0].blocks[1],
            SessionBlock::Image { image } if matches!(image.source, ImageSource::Inline { content: ContentRef::Jsonl { offset: 0, .. } })
        ));

        // Assistant thinking preview + redacted_thinking + mark_chapter + Edit backfilled from structuredPatch
        assert!(matches!(
            &msgs[1].blocks[0],
            SessionBlock::Thinking { text, redacted: false, full: Some(ContentRef::Jsonl { pointer, .. }), .. }
                if text.chars().count() == THINKING_PREVIEW_CHARS && pointer == "/message/content/0/thinking"
        ));
        assert!(matches!(
            &msgs[1].blocks[1],
            SessionBlock::Thinking { redacted: true, .. }
        ));
        assert!(matches!(
            &msgs[1].blocks[2],
            SessionBlock::ToolCall { kind: ToolKind::Mcp, title, detail: Some(d), .. }
                if title == "修复阶段" && d == "更新配置…"
        ));
        match &msgs[1].blocks[3] {
            SessionBlock::ToolCall { diff, .. } => {
                let d = diff.as_ref().expect("backfilled structuredPatch diff");
                assert_eq!((d.added, d.removed), (3, 2));
                assert_eq!(d.files[0].path, "/p/a.rs");
                assert_eq!(d.files[0].op, DiffOp::Update);
            }
            other => panic!("unexpected {other:?}"),
        }

        // Persisted output becomes ContentRef::Sidecar relative to <sessionId>/
        assert!(matches!(
            &msgs[3].blocks[0],
            SessionBlock::ToolResult {
                status: ToolStatus::Interrupted,
                truncated: true,
                full: Some(ContentRef::Sidecar { rel_path }),
                ..
            } if rel_path == "tool-results/toolu_bash.txt"
        ));

        // <command-name> -> EventKind::SlashCommand starts turn t2
        assert_eq!(msgs[4].turn_id.as_deref(), Some("t2"));
        assert!(matches!(
            &msgs[4].blocks[0],
            SessionBlock::Event { kind: EventKind::SlashCommand, text: Some(t), .. }
                if t == "/compact keep context"
        ));

        // <local-command-caveat> -> injected: true, stays in turn t2
        assert!(msgs[5].injected);
        assert_eq!(msgs[5].turn_id.as_deref(), Some("t2"));

        // <bash-input> -> by_user: true ToolCall + ToolResult
        assert!(matches!(
            &msgs[6].blocks[0],
            SessionBlock::ToolCall { by_user: true, title, .. } if title == "git status"
        ));
        assert!(matches!(
            &msgs[6].blocks[1],
            SessionBlock::ToolResult { status: ToolStatus::Success, preview, .. } if preview == "clean"
        ));

        // [Request interrupted by user...] -> EventKind::Aborted
        assert!(matches!(
            &msgs[7].blocks[0],
            SessionBlock::Event {
                kind: EventKind::Aborted,
                ..
            }
        ));

        // stop_hook_summary with errors -> EventKind::Hook (empty errors skipped)
        assert!(matches!(
            &msgs[8].blocks[0],
            SessionBlock::Event { kind: EventKind::Hook, text: Some(t), .. }
                if t == "hook.sh exited with code 1"
        ));

        // pr-link deduplicated -> EventKind::PrLink
        assert_eq!(msgs.len(), 10);
        assert!(matches!(
            &msgs[9].blocks[0],
            SessionBlock::Event { kind: EventKind::PrLink, text: Some(t), url: Some(u), .. }
                if t == "PR #42" && u == "https://github.com/o/r/pull/42"
        ));

        for msg in &msgs {
            assert_eq!(msg.content, project_content(&msg.blocks));
        }
    }

    #[test]
    fn jsonl_content_refs_resolve_exact_line_slices() {
        let data = "QUJD".repeat(400);
        let (_temp, path, msgs, offsets) = parse_claude_lines(&[
            claude_user("u1", json!("screenshot")),
            claude_assistant(
                "a1",
                "m1",
                json!([{
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "Bash",
                    "input": { "command": "echo hi", "description": "x".repeat(INPUT_PREVIEW_CHARS + 100) }
                }]),
            ),
            claude_user(
                "u2",
                json!([{
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": [
                        { "type": "text", "text": "captured" },
                        { "type": "image", "source": { "type": "base64", "media_type": "image/jpeg", "data": data } }
                    ]
                }]),
            ),
        ]);
        let raw = std::fs::read(&path).unwrap();
        let SessionBlock::ToolCall {
            input_full:
                Some(ContentRef::Jsonl {
                    offset,
                    len,
                    pointer,
                    segment: None,
                }),
            ..
        } = &msgs[1].blocks[0]
        else {
            panic!("expected tool_call jsonl ref");
        };
        assert_eq!(*offset, offsets[1]);
        assert_eq!(pointer, "/message/content/0/input");
        let parsed_line: Value =
            serde_json::from_slice(&raw[*offset as usize..(*offset + u64::from(*len)) as usize])
                .unwrap();
        assert!(parsed_line.pointer(pointer).is_some());

        let SessionBlock::ToolResult { images, .. } = &msgs[2].blocks[0] else {
            panic!("expected tool_result");
        };
        let ImageSource::Inline {
            content:
                ContentRef::Jsonl {
                    offset,
                    len,
                    pointer,
                    segment: None,
                },
        } = &images[0].source
        else {
            panic!("expected inline image jsonl ref");
        };
        assert_eq!(*offset, offsets[2]);
        let img_line: Value =
            serde_json::from_slice(&raw[*offset as usize..(*offset + u64::from(*len)) as usize])
                .unwrap();
        assert_eq!(
            img_line.pointer(pointer).and_then(Value::as_str),
            Some(data.as_str())
        );
    }
}
