//! 各解析器共用的结构化块构造工具：工具名归一化、预览截断、标题提炼、Diff 统计与轮次划分。

use serde_json::Value;

use crate::session_manager::model::{
    ContentRef, DiffFile, DiffOp, DiffSummary, EventKind, SessionBlock, SessionMessage, ToolKind,
    ToolStatus,
};

/// 工具结果预览的最大行数
pub const PREVIEW_LINES: usize = 12;
/// 工具结果预览的最大字符数
pub const PREVIEW_CHARS: usize = 1200;
/// 工具参数预览的最大字符数
pub const INPUT_PREVIEW_CHARS: usize = 400;
/// 思考正文预览的最大字符数
pub const THINKING_PREVIEW_CHARS: usize = 400;
/// 工具标题的最大字符数
pub const TITLE_CHARS: usize = 200;
/// Text 块超过该字符数时只下发预览 + 引用（仅用于默认折叠的注入文本）
pub const INLINE_TEXT_MAX_CHARS: usize = 8 * 1024;
/// 压缩摘要等长事件说明的预览字符数
pub const EVENT_PREVIEW_CHARS: usize = 400;

/// 工具调用来自哪家 Agent；同名工具在不同 Agent 下归类可能不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    Claude,
    Codex,
    Gemini,
    OpenCode,
    /// Pi 与 OpenClaw（同构）
    Pi,
    /// DeepSeek / Kimi / Grok Build / Copilot 等通用推断
    Generic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedTool {
    pub kind: ToolKind,
    /// MCP 服务器名（kind = Mcp 时）
    pub server: Option<String>,
}

impl NormalizedTool {
    pub fn of(kind: ToolKind) -> Self {
        Self { kind, server: None }
    }

    pub fn mcp(server: impl Into<String>) -> Self {
        Self {
            kind: ToolKind::Mcp,
            server: Some(server.into()),
        }
    }
}

/// 按各家 Agent 映射表把原始工具名归一化为 [`ToolKind`]。
pub fn normalize_tool(
    source: ToolSource,
    raw_name: &str,
    namespace: Option<&str>,
) -> NormalizedTool {
    if let Some(server) = namespace.and_then(|ns| ns.strip_prefix("mcp__")) {
        return NormalizedTool::mcp(server.trim_end_matches('_'));
    }
    if let Some((server, _)) = split_mcp_name(raw_name) {
        return NormalizedTool::mcp(server);
    }

    use ToolKind::*;
    let kind = match source {
        ToolSource::Claude => match raw_name {
            "Bash" => Shell,
            "Read" => Read,
            "Grep" | "Glob" | "ToolSearch" | "ListAgents" => Search,
            "Edit" | "MultiEdit" | "NotebookEdit" => Edit,
            "Write" => Write,
            "WebFetch" | "WebSearch" => Web,
            "Agent" | "Task" | "SendMessage" | "TaskStop" => Agent,
            "AskUserQuestion" => Ask,
            "TodoWrite" => Todo,
            _ => Other,
        },
        ToolSource::Codex => match raw_name {
            "exec" | "exec_command" | "shell" | "local_shell" | "write_stdin"
            | "CommandExecution" => Shell,
            "read_file" => Read,
            "apply_patch" | "FileChange" => Edit,
            "write_file" => Write,
            "web_search" | "web_search_call" | "open_page" | "WebSearch" => Web,
            "McpToolCall" => Mcp,
            "spawn_agent" | "send_message" | "agent_message" | "SubAgentActivity"
            | "create_thread" => Agent,
            "request_user_input_async" => Ask,
            "update_plan" => Todo,
            name if name.starts_with("web.") => Web,
            _ => Other,
        },
        ToolSource::Gemini => match raw_name {
            "run_shell_command" => Shell,
            "read_file" | "read_many_files" => Read,
            "grep_search" | "glob" | "list_directory" | "search_file_content" => Search,
            "replace" | "edit" => Edit,
            "write_file" => Write,
            "web_fetch" | "google_web_search" => Web,
            "ask_user" => Ask,
            "write_todos" => Todo,
            name if name.contains('/') => {
                let server = name.split('/').next().unwrap_or_default();
                return NormalizedTool::mcp(server);
            }
            _ => Other,
        },
        ToolSource::OpenCode => match raw_name {
            "bash" => Shell,
            "read" => Read,
            "grep" | "glob" | "list" | "ast_grep_search" | "codesearch" => Search,
            "edit" | "patch" => Edit,
            "write" => Write,
            "webfetch" | "websearch" => Web,
            "task" | "background_output" | "background_cancel" => Agent,
            "question" => Ask,
            "todowrite" | "todoread" => Todo,
            "invalid" => Other,
            name if name.contains('_') => {
                let server = name.split('_').next().unwrap_or_default();
                return NormalizedTool::mcp(server);
            }
            _ => Other,
        },
        ToolSource::Pi => match raw_name {
            "bash" | "bashExecution" => Shell,
            "read" => Read,
            "grep" | "find" | "ls" => Search,
            "edit" => Edit,
            "write" => Write,
            "fetch" => Web,
            _ => Other,
        },
        ToolSource::Generic => generic_kind(raw_name),
    };
    NormalizedTool::of(kind)
}

fn generic_kind(raw_name: &str) -> ToolKind {
    let name = raw_name.to_ascii_lowercase();
    let has = |keys: &[&str]| keys.iter().any(|k| name.contains(k));
    if name.starts_with("web") || has(&["fetch_url", "web_search", "web_fetch", "search_web"]) {
        ToolKind::Web
    } else if has(&["shell", "bash", "exec", "terminal", "powershell", "cmd"]) {
        ToolKind::Shell
    } else if name.starts_with("read") || name == "view" || name == "cat" {
        ToolKind::Read
    } else if has(&["grep", "glob", "search", "find", "list_dir"]) || name == "ls" {
        ToolKind::Search
    } else if has(&["edit", "patch", "replace", "str_replace"]) {
        ToolKind::Edit
    } else if name.starts_with("write") || name.starts_with("create_file") {
        ToolKind::Write
    } else if has(&["subagent", "task", "delegate"]) {
        ToolKind::Agent
    } else if has(&["ask_user", "question"]) {
        ToolKind::Ask
    } else if has(&["todo", "plan"]) {
        ToolKind::Todo
    } else {
        ToolKind::Other
    }
}

/// 拆 `mcp__<server>__<tool>`；不是这种形式返回 `None`。
pub fn split_mcp_name(raw_name: &str) -> Option<(&str, &str)> {
    let rest = raw_name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then_some((server, tool))
}

/// Shell 命令按首个程序名细化：`rg`/`grep`/`fd`/`find`/`ls` 开头算搜索，其余仍是 Shell。
pub fn refine_shell_kind(command: &str) -> ToolKind {
    let program = command
        .split_whitespace()
        .next()
        .map(|p| {
            p.rsplit(['/', '\\'])
                .next()
                .unwrap_or(p)
                .trim_end_matches(".exe")
        })
        .unwrap_or_default();
    match program {
        "rg" | "grep" | "fd" | "find" | "ls" | "Get-ChildItem" | "Select-String" => {
            ToolKind::Search
        }
        "cat" | "Get-Content" => ToolKind::Read,
        _ => ToolKind::Shell,
    }
}

/// Codex `CommandExecution.parsed_cmd[].type` → kind
pub fn kind_from_codex_parsed_cmd(parsed_type: &str) -> Option<ToolKind> {
    match parsed_type {
        "read" => Some(ToolKind::Read),
        "search" | "list" | "list_files" => Some(ToolKind::Search),
        _ => None,
    }
}

// ─── 预览 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub text: String,
    pub total_len: u32,
    pub line_count: u32,
    pub truncated: bool,
}

pub fn preview(text: &str) -> Preview {
    preview_with(text, PREVIEW_LINES, PREVIEW_CHARS)
}

pub fn preview_chars(text: &str, max_chars: usize) -> Preview {
    preview_with(text, usize::MAX, max_chars)
}

pub fn preview_with(text: &str, max_lines: usize, max_chars: usize) -> Preview {
    let total_len = saturating_u32(text.chars().count());
    let line_count = saturating_u32(text.lines().count());

    let mut end = text.len();
    if max_lines > 0 {
        if let Some((idx, _)) = text.match_indices('\n').nth(max_lines - 1) {
            end = idx;
        }
    } else {
        end = 0;
    }
    if let Some((idx, _)) = text[..end].char_indices().nth(max_chars) {
        end = idx;
    }

    let truncated = !text[end..].trim_end_matches(['\r', '\n']).is_empty();
    let kept = text[..end].trim_end_matches(['\r', '\n']);
    Preview {
        text: kept.to_string(),
        total_len,
        line_count,
        truncated,
    }
}

pub(crate) fn saturating_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

pub fn estimate_base64_size(base64_len: usize) -> u32 {
    saturating_u32(base64_len / 4 * 3)
}

// ─── 标题提炼 ─────────────────────────────────────────────────────────────

pub fn one_line_title(text: &str) -> String {
    let trimmed = text.trim();
    let mut lines = trimmed.lines();
    let first = lines.next().unwrap_or_default().trim_end();
    let more_lines = lines.any(|line| !line.trim().is_empty());
    let mut title: String = first.chars().take(TITLE_CHARS).collect();
    if more_lines || first.chars().count() > TITLE_CHARS {
        title.push('…');
    }
    title
}

pub fn title_shell(command: &str) -> String {
    one_line_title(command)
}

pub fn title_read(path: &str, offset: Option<u64>, limit: Option<u64>) -> (String, Option<String>) {
    let detail = match (offset, limit) {
        (Some(start), Some(limit)) if limit > 0 => Some(format!(":{start}-{}", start + limit - 1)),
        (Some(start), None) => Some(format!(":{start}")),
        (None, Some(limit)) if limit > 0 => Some(format!(":1-{limit}")),
        _ => None,
    };
    (one_line_title(path), detail)
}

pub fn title_search(pattern: &str, path: Option<&str>) -> String {
    match path.filter(|p| !p.is_empty()) {
        Some(path) => one_line_title(&format!("{pattern} in {path}")),
        None => one_line_title(pattern),
    }
}

pub fn title_path(path: &str) -> String {
    one_line_title(path)
}

pub fn title_web(items: &[&str]) -> String {
    one_line_title(&items.join(", "))
}

pub fn title_mcp(server: &str, tool: &str) -> String {
    one_line_title(&format!("{server}.{tool}"))
}

pub fn title_agent(description: &str) -> String {
    one_line_title(description)
}

pub fn title_ask(question: &str) -> String {
    one_line_title(question)
}

pub fn title_todo(count: usize) -> String {
    format!("{count} 项")
}

pub fn title_other(raw_name: &str) -> String {
    one_line_title(raw_name)
}

pub fn first_string_field(input: &Value) -> Option<String> {
    input
        .as_object()?
        .values()
        .find_map(|v| v.as_str().filter(|s| !s.trim().is_empty()))
        .map(one_line_title)
}

// ─── 由 JSON 参数构造 block ────────────────────────────────────────────────

pub fn str_field<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
}

fn u64_field(input: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        let value = input.get(*key)?;
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
    })
}

pub fn parse_arguments(raw: &Value) -> Value {
    match raw {
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(parsed @ (Value::Object(_) | Value::Array(_))) => parsed,
            _ => raw.clone(),
        },
        other => other.clone(),
    }
}

pub fn input_preview(input: &Value) -> Preview {
    match input {
        Value::Null => preview_chars("", INPUT_PREVIEW_CHARS),
        Value::String(text) => preview_chars(text, INPUT_PREVIEW_CHARS),
        other => preview_chars(&other.to_string(), INPUT_PREVIEW_CHARS),
    }
}

pub const PATH_KEYS: &[&str] = &[
    "file_path",
    "filePath",
    "path",
    "absolute_path",
    "file",
    "filename",
    "dir_path",
    "target_file",
];

pub fn title_from_input(
    kind: ToolKind,
    raw_name: &str,
    server: Option<&str>,
    input: &Value,
) -> (String, Option<String>) {
    let detail_of = |keys: &[&str]| str_field(input, keys).map(one_line_title);
    match kind {
        ToolKind::Shell => (
            str_field(input, &["command", "cmd", "script"])
                .map(title_shell)
                .unwrap_or_default(),
            detail_of(&["description", "workdir", "cwd"]),
        ),
        ToolKind::Read => {
            if let Some(path) = str_field(input, PATH_KEYS) {
                title_read(
                    path,
                    u64_field(input, &["offset", "start_line", "view_range_start"]),
                    u64_field(input, &["limit"]),
                )
            } else {
                let paths: Vec<&str> = input
                    .get("paths")
                    .and_then(Value::as_array)
                    .map(|items| items.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                (title_web(&paths), None)
            }
        }
        ToolKind::Search => {
            let path = str_field(input, &["path", "dir_path", "directory", "cwd"]);
            match str_field(input, &["pattern", "query", "regex", "glob", "include"]) {
                Some(pattern) => (title_search(pattern, path), None),
                None => (path.map(title_path).unwrap_or_default(), None),
            }
        }
        ToolKind::Edit | ToolKind::Write => (
            str_field(input, PATH_KEYS)
                .map(title_path)
                .unwrap_or_default(),
            None,
        ),
        ToolKind::Web => {
            let mut items: Vec<&str> = ["urls", "queries"]
                .iter()
                .filter_map(|key| input.get(*key).and_then(Value::as_array))
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if items.is_empty() {
                items.extend(str_field(input, &["url", "query", "prompt"]));
            }
            (title_web(&items), None)
        }
        ToolKind::Mcp => {
            let server = server.unwrap_or_default();
            let tool = split_mcp_name(raw_name)
                .map(|(_, tool)| tool)
                .or_else(|| {
                    raw_name
                        .strip_prefix(server)
                        .map(|rest| rest.trim_start_matches(['_', '/', '.']))
                })
                .filter(|tool| !tool.is_empty())
                .unwrap_or(raw_name);
            (title_mcp(server, tool), first_string_field(input))
        }
        ToolKind::Agent => (
            str_field(input, &["description", "task_name", "prompt", "message"])
                .map(title_agent)
                .unwrap_or_default(),
            detail_of(&["subagent_type", "agent", "model"]),
        ),
        ToolKind::Ask => {
            let question = input
                .get("questions")
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(|first| {
                    first
                        .get("question")
                        .and_then(Value::as_str)
                        .or_else(|| first.as_str())
                })
                .or_else(|| str_field(input, &["question", "prompt"]));
            (question.map(title_ask).unwrap_or_default(), None)
        }
        ToolKind::Todo => {
            let count = input
                .get("todos")
                .or_else(|| input.get("plan"))
                .and_then(Value::as_array)
                .map(Vec::len);
            (count.map(title_todo).unwrap_or_default(), None)
        }
        ToolKind::Other => (title_other(raw_name), first_string_field(input)),
    }
}

fn text_lines(text: &str) -> u32 {
    saturating_u32(text.lines().count())
}

pub fn count_diff_lines(diff: &str) -> (u32, u32) {
    let (mut added, mut removed) = (0u32, 0u32);
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added = added.saturating_add(1);
        } else if line.starts_with('-') {
            removed = removed.saturating_add(1);
        }
    }
    (added, removed)
}

pub fn single_file_diff(path: &str, op: DiffOp, added: u32, removed: u32) -> DiffSummary {
    DiffSummary {
        files: vec![DiffFile {
            path: path.to_string(),
            op,
            added,
            removed,
        }],
        added,
        removed,
        full: None,
    }
}

pub fn diff_from_input(kind: ToolKind, input: &Value) -> Option<DiffSummary> {
    let path = str_field(input, PATH_KEYS)?;
    let pair = |item: &Value| -> Option<(u32, u32)> {
        let old = str_field_allow_empty(item, &["old_string", "oldString", "old_str", "oldText"])?;
        let new = str_field_allow_empty(item, &["new_string", "newString", "new_str", "newText"])?;
        Some(line_change_counts(old, new))
    };
    match kind {
        ToolKind::Edit => {
            let (added, removed) = if let Some(edits) = input.get("edits").and_then(Value::as_array)
            {
                edits
                    .iter()
                    .filter_map(pair)
                    .fold((0u32, 0u32), |(a, r), (x, y)| {
                        (a.saturating_add(x), r.saturating_add(y))
                    })
            } else {
                pair(input)?
            };
            Some(single_file_diff(path, DiffOp::Update, added, removed))
        }
        ToolKind::Write => {
            let content =
                str_field_allow_empty(input, &["content", "contents", "text", "file_text"])?;
            Some(single_file_diff(path, DiffOp::Add, text_lines(content), 0))
        }
        _ => None,
    }
}

fn str_field_allow_empty<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))
}

pub fn openai_tool_calls(
    calls: Option<&Value>,
    arguments_ref: impl Fn(String) -> Option<ContentRef>,
) -> Vec<SessionBlock> {
    calls
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, call)| {
            let name = call
                .pointer("/function/name")
                .or_else(|| call.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
            let (pointer, raw) = match call.pointer("/function/arguments") {
                Some(raw) => (format!("/{i}/function/arguments"), Some(raw)),
                None => (format!("/{i}/arguments"), call.get("arguments")),
            };
            let input = raw.map(parse_arguments).unwrap_or(Value::Null);
            tool_call_block(ToolSource::Generic, id, name, &input, || {
                arguments_ref(pointer)
            })
        })
        .collect()
}

pub fn tool_call_block(
    source: ToolSource,
    id: impl Into<String>,
    raw_name: &str,
    input: &Value,
    input_full: impl FnOnce() -> Option<ContentRef>,
) -> SessionBlock {
    let NormalizedTool { mut kind, server } = normalize_tool(source, raw_name, None);
    let (title, detail) = title_from_input(kind, raw_name, server.as_deref(), input);
    if kind == ToolKind::Shell && !title.is_empty() {
        let refined = refine_shell_kind(&title);
        if refined != ToolKind::Shell {
            kind = refined;
        }
    }
    let preview = input_preview(input);
    SessionBlock::ToolCall {
        id: id.into(),
        raw_name: raw_name.to_string(),
        kind,
        title,
        detail,
        server,
        input_preview: preview.text,
        input_total_len: preview.total_len,
        input_full: if preview.truncated {
            input_full()
        } else {
            None
        },
        diff: diff_from_input(kind, input),
        by_user: false,
    }
}

pub fn tool_result_block(
    call_id: impl Into<String>,
    status: ToolStatus,
    text: &str,
    full: impl FnOnce() -> Option<ContentRef>,
) -> SessionBlock {
    let p = preview(text);
    SessionBlock::ToolResult {
        call_id: call_id.into(),
        status,
        preview: p.text,
        total_len: p.total_len,
        line_count: p.line_count,
        truncated: p.truncated,
        full: if p.truncated { full() } else { None },
        exit_code: None,
        duration_ms: None,
        images: Vec::new(),
        saved_path: None,
    }
}

pub fn thinking_block(
    text: &str,
    summary: Option<String>,
    duration_ms: Option<u64>,
    full: impl FnOnce() -> Option<ContentRef>,
) -> SessionBlock {
    let p = preview_chars(text.trim_end(), THINKING_PREVIEW_CHARS);
    SessionBlock::Thinking {
        redacted: text.trim().is_empty(),
        text: p.text,
        summary,
        duration_ms,
        full: if p.truncated { full() } else { None },
    }
}

pub fn large_text_block(
    text: impl Into<String>,
    full: impl FnOnce() -> Option<ContentRef>,
) -> SessionBlock {
    let text = text.into();
    if text.chars().nth(INLINE_TEXT_MAX_CHARS).is_none() {
        return SessionBlock::text(text);
    }
    match full() {
        Some(full) => SessionBlock::Text {
            text: preview(&text).text,
            full: Some(full),
        },
        None => SessionBlock::text(text),
    }
}

pub fn summary_event_block(
    kind: EventKind,
    text: &str,
    full: impl FnOnce() -> Option<ContentRef>,
) -> SessionBlock {
    let text = text.trim();
    let p = preview_chars(text, EVENT_PREVIEW_CHARS);
    if !p.truncated {
        return SessionBlock::event(kind, Some(p.text), None);
    }
    match full() {
        Some(full) => SessionBlock::Event {
            kind,
            text: Some(p.text),
            url: None,
            full: Some(full),
        },
        None => SessionBlock::event(kind, Some(format!("{}…", p.text)), None),
    }
}

pub fn starts_turn(message: &SessionMessage) -> bool {
    if message.role != "user" || message.injected {
        return false;
    }
    if message.blocks.is_empty() {
        return !message.content.trim().is_empty();
    }
    message.blocks.iter().any(|block| match block {
        SessionBlock::Text { text, .. } => !text.trim().is_empty(),
        SessionBlock::Image { .. } => true,
        SessionBlock::Event { kind, .. } => *kind == EventKind::SlashCommand,
        _ => false,
    })
}

pub fn assign_turn_ids(messages: &mut [SessionMessage]) {
    let mut turn = 0u32;
    for message in messages {
        if starts_turn(message) {
            turn += 1;
        }
        if message.turn_id.is_none() {
            message.turn_id = Some(format!("t{turn}"));
        }
    }
}

pub fn line_change_counts(old: &str, new: &str) -> (u32, u32) {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let prefix = old_lines
        .iter()
        .zip(&new_lines)
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = old_lines[prefix..]
        .iter()
        .rev()
        .zip(new_lines[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    (
        saturating_u32(new_lines.len() - prefix - suffix),
        saturating_u32(old_lines.len() - prefix - suffix),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn line_change_counts_ignores_shared_context() {
        assert_eq!(line_change_counts("a\nb\nc", "a\nb\nx\nc"), (1, 0));
        assert_eq!(line_change_counts("a\nb", "a\nc\nd"), (2, 1));
        assert_eq!(line_change_counts("", "x\ny"), (2, 0));
        assert_eq!(line_change_counts("same", "same"), (0, 0));
    }

    #[test]
    fn normalize_and_refine_tools() {
        assert_eq!(
            normalize_tool(ToolSource::Claude, "Edit", None).kind,
            ToolKind::Edit
        );
        assert_eq!(
            normalize_tool(ToolSource::Claude, "mcp__github__list_prs", None),
            NormalizedTool::mcp("github")
        );
        assert_eq!(
            normalize_tool(ToolSource::Codex, "js", Some("mcp__cua_repl")),
            NormalizedTool::mcp("cua_repl")
        );
        assert_eq!(refine_shell_kind("rg -n foo src"), ToolKind::Search);
        assert_eq!(refine_shell_kind("cat Cargo.toml"), ToolKind::Read);
        assert_eq!(refine_shell_kind("cargo test"), ToolKind::Shell);
    }

    #[test]
    fn preview_truncates_on_line_and_char_boundaries() {
        let exact: String = (1..=12).map(|i| format!("l{i}\n")).collect();
        assert!(!preview(&exact).truncated);

        let over: String = (1..=13).map(|i| format!("l{i}\n")).collect();
        let p = preview(&over);
        assert!(p.truncated);
        assert_eq!(p.line_count, 13);

        let chars = "字".repeat(PREVIEW_CHARS + 1);
        let cp = preview(&chars);
        assert!(cp.truncated);
        assert_eq!(cp.text.chars().count(), PREVIEW_CHARS);
    }

    #[test]
    fn assign_turn_ids_groups_injected_and_by_user_into_current_turn() {
        let msg = |role: &str, injected: bool, blocks: Vec<SessionBlock>| SessionMessage {
            role: role.into(),
            injected,
            blocks,
            ..SessionMessage::default()
        };
        let mut by_user = tool_call_block(ToolSource::Pi, "u1", "bash", &json!({}), || None);
        if let SessionBlock::ToolCall {
            by_user: ref mut flag,
            ..
        } = by_user
        {
            *flag = true;
        }
        let mut messages = vec![
            msg("system", false, vec![SessionBlock::text("init")]),
            msg("user", false, vec![SessionBlock::text("q1")]),
            msg("assistant", false, vec![SessionBlock::text("a1")]),
            msg("user", true, vec![SessionBlock::text("hook")]),
            msg(
                "user",
                false,
                vec![SessionBlock::event(EventKind::Aborted, None, None)],
            ),
            msg("user", false, vec![by_user]),
            msg("user", false, vec![SessionBlock::text("q2")]),
        ];
        assign_turn_ids(&mut messages);
        let turns: Vec<_> = messages
            .iter()
            .map(|m| m.turn_id.clone().unwrap())
            .collect();
        assert_eq!(turns, ["t0", "t1", "t1", "t1", "t1", "t1", "t2"]);
    }
}
