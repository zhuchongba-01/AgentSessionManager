//! 会话阅读器的统一结构化数据模型（对齐 cc-switch SessionBlock 规范）。
//!
//! 各 Provider 解析器将原生记录转换为 [`SessionMessage`]：结构化内容存入 `blocks`，
//! `content` 由 [`project_content`] 派生纯文本投影，供旧路径与预览兼容使用。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMessage {
    /// user | assistant | tool | system | thinking
    pub role: String,
    /// 纯文本投影（见 [`project_content`]）
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<i64>,
    /// 源记录 ID（Claude uuid / Codex payload.id / OpenCode message id / Pi entry id）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// 同一轮（turn）的标识：Codex 原生 turn_id 或按 user 提问递增生成的 `t{n}`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// 注入型上下文（AGENTS.md、environment_context、system-reminder、input_from_hook 等）
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub injected: bool,
    /// 结构化内容块；为空时前端按 `content` 兜底
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<SessionBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<MessageMeta>,
}

impl SessionMessage {
    /// 由结构化块构造消息，自动派生 `content` 纯文本投影。
    pub fn from_blocks(
        role: impl Into<String>,
        ts: Option<i64>,
        blocks: Vec<SessionBlock>,
    ) -> Self {
        Self {
            role: role.into(),
            content: project_content(&blocks),
            ts,
            blocks,
            ..Self::default()
        }
    }

    /// 是否不含任何可展示内容
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.content.trim().is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

impl MessageMeta {
    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.cache_read_tokens.is_none()
            && self.cache_write_tokens.is_none()
            && self.reasoning_tokens.is_none()
            && self.cost_usd.is_none()
            && self.duration_ms.is_none()
            && self.stop_reason.is_none()
    }

    pub fn into_option(self) -> Option<Self> {
        (!self.is_empty()).then_some(self)
    }
}

/// 消息内容块：`type` 使用 snake_case，块内字段使用 camelCase。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SessionBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        full: Option<ContentRef>,
    },
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        full: Option<ContentRef>,
    },
    ToolCall {
        id: String,
        raw_name: String,
        kind: ToolKind,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server: Option<String>,
        input_preview: String,
        input_total_len: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_full: Option<ContentRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<DiffSummary>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        by_user: bool,
    },
    ToolResult {
        call_id: String,
        status: ToolStatus,
        preview: String,
        total_len: u32,
        line_count: u32,
        truncated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        full: Option<ContentRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        saved_path: Option<String>,
    },
    Image {
        image: ImageRef,
    },
    Event {
        kind: EventKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        full: Option<ContentRef>,
    },
    Step {
        phase: StepPhase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl SessionBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            full: None,
        }
    }

    pub fn event(kind: EventKind, text: Option<String>, url: Option<String>) -> Self {
        Self::Event {
            kind,
            text,
            url,
            full: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Shell,
    Read,
    Search,
    Edit,
    Write,
    Web,
    Mcp,
    Agent,
    Ask,
    Todo,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Success,
    Error,
    Interrupted,
    Pending,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Aborted,
    Compaction,
    ModelChange,
    ThinkingLevel,
    Hook,
    PrLink,
    SlashCommand,
    Info,
    Error,
    SubAgent,
    Goal,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepPhase {
    Start,
    Finish,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffSummary {
    pub files: Vec<DiffFile>,
    pub added: u32,
    pub removed: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full: Option<ContentRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffFile {
    pub path: String,
    pub op: DiffOp,
    pub added: u32,
    pub removed: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffOp {
    Add,
    Update,
    Delete,
    Rename,
}

/// 大内容按需回取引用：前端原样回传，后端做路径/表白名单与边界校验后读取。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ContentRef {
    /// JSONL：行字节区间 + 行内 JSON Pointer（Codex 分页历史可选带 `segment` 段号）
    Jsonl {
        offset: u64,
        len: u32,
        pointer: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        segment: Option<u16>,
    },
    /// SQLite：表 + 主键 + 列 + 列内 JSON Pointer（空串表示整列）
    Sqlite {
        table: String,
        id: String,
        column: String,
        pointer: String,
    },
    /// 独立 JSON/JSONL 文件：相对于 provider 会话根目录的相对路径 + JSON Pointer
    File { rel_path: String, pointer: String },
    /// 工具落盘的完整输出文件（仅限会话附属目录内）
    Sidecar { rel_path: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageRef {
    pub source: ImageSource,
    pub media_type: String,
    pub size: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ImageSource {
    Inline { content: ContentRef },
    LocalFile { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnIndex {
    pub turn_id: String,
    pub first_message_index: usize,
    pub last_message_index: usize,
    pub question_preview: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<i64>,
    pub step_count: u32,
    pub error_count: u32,
    pub has_final_reply: bool,
    pub aborted: bool,
}

/// 由 `blocks` 派生纯文本投影：
/// - `Text` → 原文
/// - `ToolCall` → `[Tool: {raw_name}] {title}`
/// - `ToolResult` → 预览文本
/// - `Image` → `[Image: {media_type} {size}]`
/// - `Event` → `text`（若有）
/// - `Thinking` / `Step` → 不进入投影
pub fn project_content(blocks: &[SessionBlock]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        let part = match block {
            SessionBlock::Text { text, .. } => text.clone(),
            SessionBlock::ToolCall {
                raw_name, title, ..
            } => {
                if title.is_empty() {
                    format!("[Tool: {raw_name}]")
                } else {
                    format!("[Tool: {raw_name}] {title}")
                }
            }
            SessionBlock::ToolResult { preview, .. } => preview.clone(),
            SessionBlock::Image { image } => {
                format!("[Image: {} {}]", image.media_type, image.size)
            }
            SessionBlock::Event { text, .. } => text.clone().unwrap_or_default(),
            SessionBlock::Thinking { .. } | SessionBlock::Step { .. } => continue,
        };
        if !part.trim().is_empty() {
            parts.push(part);
        }
    }
    parts.join("\n\n")
}

/// 提取一条消息里供全文检索（FTS5）收录的纯对话文本（仅包含 `Text` 块；无 `blocks` 时退回 `content`）。
pub fn searchable_text(message: &SessionMessage) -> String {
    if message.blocks.is_empty() {
        return message.content.clone();
    }
    let mut parts = Vec::new();
    for block in &message.blocks {
        if let SessionBlock::Text { text, .. } = block {
            if !text.trim().is_empty() {
                parts.push(text.as_str());
            }
        }
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn project_content_follows_projection_rules() {
        let blocks = vec![
            SessionBlock::Thinking {
                text: "secret plan".into(),
                summary: None,
                redacted: false,
                duration_ms: None,
                full: None,
            },
            SessionBlock::text("先看看构建输出。"),
            SessionBlock::ToolCall {
                id: "call_1".into(),
                raw_name: "Bash".into(),
                kind: ToolKind::Shell,
                title: "cargo build".into(),
                detail: None,
                server: None,
                input_preview: "{}".into(),
                input_total_len: 2,
                input_full: None,
                diff: None,
                by_user: false,
            },
            SessionBlock::ToolResult {
                call_id: "call_1".into(),
                status: ToolStatus::Error,
                preview: "error[E0433]".into(),
                total_len: 12,
                line_count: 1,
                truncated: false,
                full: None,
                exit_code: Some(101),
                duration_ms: None,
                images: vec![],
                saved_path: None,
            },
            SessionBlock::Image {
                image: ImageRef {
                    source: ImageSource::LocalFile {
                        path: "/tmp/a.png".into(),
                    },
                    media_type: "image/png".into(),
                    size: 2048,
                    alt: None,
                },
            },
            SessionBlock::event(EventKind::Aborted, None, None),
            SessionBlock::Step {
                phase: StepPhase::Finish,
                tokens: Some(10),
                cost_usd: None,
                reason: None,
            },
        ];
        assert_eq!(
            project_content(&blocks),
            "先看看构建输出。\n\n[Tool: Bash] cargo build\n\nerror[E0433]\n\n[Image: image/png 2048]"
        );
    }

    #[test]
    fn serializes_blocks_with_snake_case_tags_and_camel_case_fields() {
        let block = SessionBlock::ToolResult {
            call_id: "c".into(),
            status: ToolStatus::Interrupted,
            preview: String::new(),
            total_len: 0,
            line_count: 0,
            truncated: false,
            full: Some(ContentRef::Sidecar {
                rel_path: "tool-results/c.txt".into(),
            }),
            exit_code: None,
            duration_ms: Some(5),
            images: vec![],
            saved_path: None,
        };
        assert_eq!(
            serde_json::to_value(&block).unwrap(),
            json!({
                "type": "tool_result",
                "callId": "c",
                "status": "interrupted",
                "preview": "",
                "totalLen": 0,
                "lineCount": 0,
                "truncated": false,
                "full": { "kind": "sidecar", "relPath": "tool-results/c.txt" },
                "durationMs": 5
            })
        );
    }
}
