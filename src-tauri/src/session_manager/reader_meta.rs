//! 结构化会话阅读元数据（对齐 cc-switch 阅读器模型）。
//!
//! 在不改变原有 `SessionMessage { role, content, ts, meta: None }` 文本契约的前提下，
//! 为前端阅读器补充：
//! - 思维链摘要（`summary`）、加密/隐藏思考标记（`redacted`）、思考耗时（`duration_ms`）；
//! - 工具调用的 `call_id`、原始名、MCP 服务器名、提炼后的单行标题与次要信息、JSON 参数预览；
//! - Edit / MultiEdit / Write / apply_patch 的增删行数与还原出的 Unified Diff；
//! - 工具返回值的配对 `call_id`、状态（`success` / `error` / `interrupted`）、退出码与耗时；
//! - 模型名称（`model`）、Token 数与事件块（上下文压缩、用户中断、PR 链接、Hook 报错等）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const INPUT_PREVIEW_MAX_CHARS: usize = 2400;
pub const DIFF_TEXT_MAX_CHARS: usize = 16 * 1024;
pub const TITLE_MAX_CHARS: usize = 200;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageReaderMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_summary: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redacted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub injected: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<ReaderBlock>,
}

impl MessageReaderMeta {
    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.thinking_summary.is_none()
            && !self.redacted
            && self.duration_ms.is_none()
            && self.tokens.is_none()
            && self.cost_usd.is_none()
            && !self.injected
            && self.blocks.is_empty()
    }

    pub fn into_boxed_option(self) -> Option<Box<Self>> {
        (!self.is_empty()).then(|| Box::new(self))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ReaderBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    ToolCall {
        id: String,
        raw_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server: Option<String>,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        input_preview: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        input_truncated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff_added: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff_removed: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff_text: Option<String>,
    },
    ToolResult {
        call_id: String,
        /// "success" | "error" | "interrupted"
        status: String,
        output: String,
        line_count: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    Event {
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
}

pub fn one_line_title(text: &str) -> String {
    let trimmed = text.trim();
    let mut lines = trimmed.lines();
    let first = lines.next().unwrap_or_default().trim_end();
    let more_lines = lines.any(|line| !line.trim().is_empty());
    let mut title: String = first.chars().take(TITLE_MAX_CHARS).collect();
    if more_lines || first.chars().count() > TITLE_MAX_CHARS {
        title.push('…');
    }
    title
}

fn truncate_chars(text: &str, max_chars: usize) -> (String, bool) {
    if let Some((idx, _)) = text.char_indices().nth(max_chars) {
        (text[..idx].to_string(), true)
    } else {
        (text.to_string(), false)
    }
}

/// 剥掉常见的 shell 包装（如 `/bin/zsh -lc '...'`、`bash -c "..."`、`powershell -Command ...`）
pub fn unwrap_shell_command(raw: &str) -> String {
    let trimmed = raw.trim();
    for prefix in [
        "/bin/zsh -lc ",
        "/bin/bash -lc ",
        "/bin/sh -lc ",
        "/bin/zsh -c ",
        "/bin/bash -c ",
        "/bin/sh -c ",
        "zsh -lc ",
        "bash -lc ",
        "sh -lc ",
        "zsh -c ",
        "bash -c ",
        "sh -c ",
        "cmd /c ",
        "cmd.exe /c ",
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let inner = rest.trim();
            if inner.len() >= 2
                && ((inner.starts_with('\'') && inner.ends_with('\''))
                    || (inner.starts_with('"') && inner.ends_with('"')))
            {
                return inner[1..inner.len() - 1].to_string();
            }
            return inner.to_string();
        }
    }
    trimmed.to_string()
}

pub fn split_mcp_name(raw_name: &str) -> Option<(&str, &str)> {
    let rest = raw_name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then_some((server, tool))
}

fn first_string_field(input: &Value) -> Option<String> {
    let map = input.as_object()?;
    for (_, val) in map {
        if let Some(s) = val.as_str().map(str::trim).filter(|s| !s.is_empty()) {
            return Some(one_line_title(s));
        }
    }
    None
}

fn pick_str<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a str> {
    for &key in keys {
        if let Some(s) = input
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(s);
        }
    }
    None
}

fn extract_command_string(input: &Value) -> Option<String> {
    for key in ["command", "cmd", "script", "CommandLine"] {
        match input.get(key) {
            Some(Value::String(s)) if !s.trim().is_empty() => {
                return Some(unwrap_shell_command(s));
            }
            Some(Value::Array(arr)) => {
                let parts: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
                if !parts.is_empty() {
                    if parts.len() >= 3
                        && matches!(
                            parts[0].rsplit(['/', '\\']).next().unwrap_or(parts[0]),
                            "bash"
                                | "zsh"
                                | "sh"
                                | "pwsh"
                                | "powershell"
                                | "powershell.exe"
                                | "cmd"
                                | "cmd.exe"
                        )
                        && matches!(parts[1], "-lc" | "-c" | "-Command" | "/c" | "/C")
                    {
                        return Some(parts[2..].join(" "));
                    }
                    return Some(parts.join(" "));
                }
            }
            _ => {}
        }
    }
    None
}

fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let normalized = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    normalized.lines().collect()
}

fn build_diff_from_input(raw_name: &str, input: &Value) -> Option<(u32, u32, String)> {
    let lower = raw_name.to_ascii_lowercase();
    let file_path = pick_str(
        input,
        &[
            "file_path",
            "filePath",
            "path",
            "TargetFile",
            "notebook_path",
        ],
    )
    .unwrap_or("");

    let old_keys = [
        "old_string",
        "oldString",
        "oldText",
        "old_str",
        "TargetContent",
        "Target",
    ];
    let new_keys = [
        "new_string",
        "newString",
        "newText",
        "new_str",
        "ReplacementContent",
        "Replacement",
    ];
    let content_keys = ["content", "contents", "file_text", "CodeContent"];

    let mut pairs: Vec<(&str, &str)> = Vec::new();
    if let Some(edits) = input
        .get("edits")
        .or_else(|| input.get("ReplacementChunks"))
        .and_then(Value::as_array)
    {
        for edit in edits {
            let old_s = pick_str_raw(edit, &old_keys);
            let new_s = pick_str_raw(edit, &new_keys);
            if old_s.is_some() || new_s.is_some() {
                pairs.push((old_s.unwrap_or(""), new_s.unwrap_or("")));
            }
        }
    } else {
        let old_s = pick_str_raw(input, &old_keys);
        let new_s = pick_str_raw(input, &new_keys);
        if old_s.is_some() || new_s.is_some() {
            pairs.push((old_s.unwrap_or(""), new_s.unwrap_or("")));
        } else if matches!(lower.as_str(), "write" | "write_file" | "create_file") {
            if let Some(body) = pick_str_raw(input, &content_keys) {
                if !body.is_empty() {
                    pairs.push(("", body));
                }
            }
        }
    }

    if pairs.is_empty() {
        // 检查 patch / diff 字符串参数
        if let Some(patch_str) = pick_str_raw(input, &["patch", "diff", "input"]) {
            return count_patch_diff(patch_str);
        }
        return None;
    }

    let mut added = 0u32;
    let mut removed = 0u32;
    let mut hunks = Vec::new();
    if !file_path.is_empty() {
        hunks.push(format!("--- a/{file_path}\n+++ b/{file_path}"));
    }
    for (old_s, new_s) in pairs {
        if old_s.is_empty() && new_s.is_empty() {
            continue;
        }
        let old_lines = split_lines(old_s);
        let new_lines = split_lines(new_s);
        removed = removed.saturating_add(old_lines.len() as u32);
        added = added.saturating_add(new_lines.len() as u32);
        let mut hunk_lines = Vec::with_capacity(1 + old_lines.len() + new_lines.len());
        hunk_lines.push("@@".to_string());
        for l in old_lines {
            hunk_lines.push(format!("-{l}"));
        }
        for l in new_lines {
            hunk_lines.push(format!("+{l}"));
        }
        hunks.push(hunk_lines.join("\n"));
    }

    if added == 0 && removed == 0 {
        return None;
    }
    let full = hunks.join("\n");
    let (capped, _) = truncate_chars(&full, DIFF_TEXT_MAX_CHARS);
    Some((added, removed, capped))
}

fn pick_str_raw<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a str> {
    for &key in keys {
        if let Some(s) = input.get(key).and_then(Value::as_str) {
            return Some(s);
        }
    }
    None
}

pub fn count_patch_diff(patch: &str) -> Option<(u32, u32, String)> {
    let has_marker = patch.lines().any(|line| {
        line.starts_with("@@ ")
            || line.starts_with("*** Update File:")
            || line.starts_with("*** Add File:")
            || line.starts_with("*** Delete File:")
            || line.starts_with("diff --git ")
    });
    if !has_marker {
        return None;
    }
    let mut added = 0u32;
    let mut removed = 0u32;
    for line in patch.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            added = added.saturating_add(1);
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed = removed.saturating_add(1);
        }
    }
    if added == 0 && removed == 0 {
        return None;
    }
    let (capped, _) = truncate_chars(patch, DIFF_TEXT_MAX_CHARS);
    Some((added, removed, capped))
}

pub fn extract_patch_first_file(patch: &str) -> Option<String> {
    for line in patch.lines() {
        for prefix in [
            "*** Update File: ",
            "*** Add File: ",
            "*** Delete File: ",
            "+++ b/",
            "+++ ",
        ] {
            if let Some(rest) = line.strip_prefix(prefix) {
                let trimmed = rest.trim();
                if !trimmed.is_empty() && trimmed != "/dev/null" {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    None
}

/// 从任意 Agent 的工具调用 `(id, raw_name, namespace, input_value)` 生成结构化 `ReaderBlock::ToolCall`。
pub fn build_tool_call_block(
    id: impl Into<String>,
    raw_name: &str,
    namespace: Option<&str>,
    input: &Value,
) -> ReaderBlock {
    let id = id.into();
    let mut server = namespace
        .and_then(|ns| ns.strip_prefix("mcp__"))
        .map(|s| s.trim_end_matches('_').to_string());

    let (mcp_server, mcp_tool) = match split_mcp_name(raw_name) {
        Some((srv, tl)) => {
            server = Some(srv.to_string());
            (Some(srv), Some(tl))
        }
        None => (server.as_deref(), None),
    };

    let lower = raw_name.to_ascii_lowercase();
    let mut title = String::new();
    let mut detail: Option<String> = None;

    if let (Some(srv), Some(tl)) = (mcp_server, mcp_tool) {
        if tl == "mark_chapter" {
            if let Some(ch_title) = pick_str(input, &["title"]) {
                title = one_line_title(ch_title);
                detail = pick_str(input, &["summary"]).map(one_line_title);
            }
        }
        if title.is_empty() {
            title = format!("{srv}.{tl}");
            detail = first_string_field(input);
        }
    } else if let Some(cmd) = extract_command_string(input) {
        title = one_line_title(&cmd);
        detail = pick_str(input, &["description", "workdir", "cwd"]).map(one_line_title);
    } else if matches!(
        lower.as_str(),
        "read" | "read_file" | "read_many_files" | "view"
    ) {
        if let Some(path) = pick_str(input, &["file_path", "filePath", "path"]) {
            title = one_line_title(path);
        } else if let Some(first_path) = input
            .get("paths")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(Value::as_str)
        {
            title = one_line_title(first_path);
        }
        let offset = input
            .get("offset")
            .or_else(|| input.get("start_line"))
            .or_else(|| input.get("StartLine"))
            .and_then(Value::as_u64);
        let limit = input.get("limit").and_then(Value::as_u64);
        let end_line = input
            .get("end_line")
            .or_else(|| input.get("EndLine"))
            .and_then(Value::as_u64);
        detail = match (offset, limit, end_line) {
            (Some(start), _, Some(end)) if end >= start => Some(format!(":{start}-{end}")),
            (Some(start), Some(lim), _) if lim > 0 => Some(format!(":{start}-{}", start + lim - 1)),
            (Some(start), None, None) => Some(format!(":{start}")),
            (None, Some(lim), None) if lim > 0 => Some(format!(":1-{lim}")),
            _ => None,
        };
    } else if matches!(
        lower.as_str(),
        "grep"
            | "glob"
            | "grep_search"
            | "search_file_content"
            | "toolsearch"
            | "ast_grep_search"
            | "codesearch"
            | "find"
            | "ls"
            | "list"
            | "list_directory"
    ) {
        let pattern = pick_str(input, &["pattern", "query", "include"]).unwrap_or("");
        let scope = pick_str(input, &["path", "directory", "dir", "glob"]);
        title = match (!pattern.is_empty(), scope) {
            (true, Some(p)) => one_line_title(&format!("{pattern} in {p}")),
            (true, None) => one_line_title(pattern),
            (false, Some(p)) => one_line_title(p),
            (false, None) => String::new(),
        };
        detail = pick_str(input, &["output_mode"]).map(str::to_string);
    } else if matches!(
        lower.as_str(),
        "edit"
            | "multiedit"
            | "write"
            | "write_file"
            | "edit_file"
            | "replace"
            | "notebookedit"
            | "apply_patch"
            | "patch"
            | "create_file"
            | "str_replace_editor"
    ) {
        if let Some(path) = pick_str(
            input,
            &[
                "file_path",
                "filePath",
                "path",
                "TargetFile",
                "notebook_path",
            ],
        ) {
            title = one_line_title(path);
        } else if let Some(patch_str) = input
            .as_str()
            .or_else(|| pick_str_raw(input, &["patch", "input"]))
        {
            if let Some(first_file) = extract_patch_first_file(patch_str) {
                title = one_line_title(&first_file);
            }
        }
    } else if matches!(
        lower.as_str(),
        "webfetch"
            | "websearch"
            | "web_fetch"
            | "web_search"
            | "web_search_call"
            | "google_web_search"
            | "fetch"
            | "open_page"
    ) {
        title = pick_str(input, &["query", "url", "q"])
            .map(one_line_title)
            .unwrap_or_default();
    } else if matches!(
        lower.as_str(),
        "agent" | "task" | "spawn_agent" | "send_message" | "subagent"
    ) {
        title = pick_str(input, &["description", "role", "prompt", "recipient"])
            .map(one_line_title)
            .unwrap_or_default();
        detail = pick_str(input, &["model", "subagent_type"]).map(one_line_title);
    } else if matches!(
        lower.as_str(),
        "askuserquestion" | "ask_user" | "question" | "request_user_input_async"
    ) {
        if let Some(q) = input
            .get("questions")
            .and_then(Value::as_array)
            .and_then(|qs| qs.first())
            .and_then(|item| item.get("question"))
            .and_then(Value::as_str)
        {
            title = one_line_title(q);
        } else if let Some(q) = pick_str(input, &["question", "prompt"]) {
            title = one_line_title(q);
        }
    } else if matches!(
        lower.as_str(),
        "todowrite" | "todoread" | "write_todos" | "update_plan"
    ) {
        if let Some(arr) = input
            .get("todos")
            .or_else(|| input.get("plan"))
            .and_then(Value::as_array)
        {
            title = format!("{} 项", arr.len());
        } else if let Some(exp) = pick_str(input, &["explanation"]) {
            title = one_line_title(exp);
        }
    } else {
        title = first_string_field(input).unwrap_or_default();
    }

    let (input_preview, input_truncated) = match input {
        Value::Null => (String::new(), false),
        Value::String(s) => truncate_chars(s.trim(), INPUT_PREVIEW_MAX_CHARS),
        other => {
            let serialized = serde_json::to_string(other).unwrap_or_default();
            if serialized == "{}" || serialized == "[]" {
                (String::new(), false)
            } else {
                truncate_chars(&serialized, INPUT_PREVIEW_MAX_CHARS)
            }
        }
    };

    let diff_tuple = match input {
        Value::String(s) => count_patch_diff(s),
        _ => build_diff_from_input(raw_name, input),
    };
    let (diff_added, diff_removed, diff_text) = match diff_tuple {
        Some((a, r, txt)) => (Some(a), Some(r), Some(txt)),
        None => (None, None, None),
    };

    ReaderBlock::ToolCall {
        id,
        raw_name: raw_name.to_string(),
        server,
        title,
        detail,
        input_preview,
        input_truncated,
        diff_added,
        diff_removed,
        diff_text,
    }
}

/// 拆分思考文本中首行的粗体标题摘要（如 Gemini `**Inspecting main**\n\nRead src/main.rs` 或 Codex 摘要）
pub fn split_markdown_thinking_summary(raw: &str) -> (Option<String>, String) {
    let trimmed = raw.trim();
    if !trimmed.starts_with("**") {
        return (None, trimmed.to_string());
    }
    let first_line_end = trimmed.find('\n').unwrap_or(trimmed.len());
    let first_line = trimmed[..first_line_end].trim();
    if first_line.len() > 4
        && first_line.starts_with("**")
        && first_line.ends_with("**")
        && !first_line[2..first_line.len() - 2].contains("**")
    {
        let summary = first_line[2..first_line.len() - 2].trim().to_string();
        let rest = trimmed[first_line_end..].trim().to_string();
        if !summary.is_empty() {
            return (Some(summary), rest);
        }
    }
    (None, trimmed.to_string())
}

#[derive(Debug, Clone, Default)]
pub struct ParsedToolOutput {
    pub body: String,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
    pub is_error: bool,
}

/// 解析 Codex `function_call_output` / `custom_tool_call_output` 的包装头与 JSON 载荷
pub fn parse_codex_output_envelope(raw: &str) -> ParsedToolOutput {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') {
        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
            if let Some(output_str) = val.get("output").and_then(Value::as_str) {
                let exit_code = val
                    .pointer("/metadata/exit_code")
                    .and_then(Value::as_i64)
                    .and_then(|c| i32::try_from(c).ok());
                let duration_ms = val
                    .pointer("/metadata/duration_seconds")
                    .and_then(Value::as_f64)
                    .filter(|&secs| secs.is_finite() && secs >= 0.0)
                    .map(|secs| (secs * 1000.0).round() as u64);
                let is_error = exit_code.is_some_and(|c| c != 0);
                return ParsedToolOutput {
                    body: output_str.trim_end_matches(['\r', '\n']).to_string(),
                    exit_code,
                    duration_ms,
                    is_error,
                };
            }
        }
    }

    let mut exit_code: Option<i32> = None;
    let mut duration_ms: Option<u64> = None;
    let lines: Vec<&str> = raw.lines().collect();
    let mut idx = 0usize;
    let mut saw_codex_header = false;

    while idx < lines.len() && idx < 8 {
        let line = lines[idx].trim();
        if line.starts_with("Chunk ID:")
            || line.starts_with("Original token count:")
            || line.starts_with("Total output lines:")
        {
            saw_codex_header = true;
            idx += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("Wall time:") {
            saw_codex_header = true;
            let secs_str = rest
                .trim()
                .trim_end_matches("seconds")
                .trim_end_matches('s')
                .trim();
            if let Ok(secs) = secs_str.parse::<f64>() {
                if secs.is_finite() && secs >= 0.0 {
                    duration_ms = Some((secs * 1000.0).round() as u64);
                }
            }
            idx += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("Process exited with code ") {
            saw_codex_header = true;
            if let Ok(code) = rest.trim().parse::<i32>() {
                exit_code = Some(code);
            }
            idx += 1;
            continue;
        }
        if line == "Output:" && saw_codex_header {
            idx += 1;
            break;
        }
        break;
    }

    let body = if saw_codex_header && idx <= lines.len() {
        lines[idx..].join("\n")
    } else {
        raw.to_string()
    };

    if exit_code.is_none() {
        for line in body.lines().take(5) {
            if let Some(rest) = line
                .trim()
                .strip_prefix("Exit code: ")
                .or_else(|| line.trim().strip_prefix("Exit code "))
            {
                if let Ok(code) = rest.trim().parse::<i32>() {
                    exit_code = Some(code);
                    break;
                }
            }
        }
    }

    let is_error = exit_code.is_some_and(|c| c != 0);
    ParsedToolOutput {
        body: body.trim_end_matches(['\r', '\n']).to_string(),
        exit_code,
        duration_ms,
        is_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reconstructs_diff_from_edit_tool_input_and_extracts_title() {
        let block = build_tool_call_block(
            "call_edit_1",
            "Edit",
            None,
            &json!({
                "file_path": "/repo/src/auth.ts",
                "old_string": "const ok = false;\nreturn ok;",
                "new_string": "const ok = true;\nconst role = 'admin';\nreturn ok;"
            }),
        );
        match block {
            ReaderBlock::ToolCall {
                id,
                title,
                diff_added,
                diff_removed,
                diff_text,
                ..
            } => {
                assert_eq!(id, "call_edit_1");
                assert_eq!(title, "/repo/src/auth.ts");
                assert_eq!(diff_added, Some(3));
                assert_eq!(diff_removed, Some(2));
                let diff = diff_text.expect("reconstructed diff");
                assert!(diff.contains("+++ b//repo/src/auth.ts"));
                assert!(diff.contains("-const ok = false;"));
                assert!(diff.contains("+const role = 'admin';"));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn strips_codex_chunk_envelope_and_extracts_exit_code_and_duration() {
        let raw = "Chunk ID: 98ab12\nWall time: 1.75 seconds\nProcess exited with code 2\nOriginal token count: 42\nOutput:\nerror[E0425]: cannot find value `x`\n";
        let parsed = parse_codex_output_envelope(raw);
        assert_eq!(parsed.exit_code, Some(2));
        assert_eq!(parsed.duration_ms, Some(1750));
        assert!(parsed.is_error);
        assert_eq!(parsed.body, "error[E0425]: cannot find value `x`");
    }

    #[test]
    fn splits_bold_markdown_thinking_summary() {
        let (summary, body) = split_markdown_thinking_summary(
            "**Inspecting auth flow**\n\nFirst check token signature.",
        );
        assert_eq!(summary.as_deref(), Some("Inspecting auth flow"));
        assert_eq!(body, "First check token signature.");
    }
}
