//! 下发给前端的会话正文形态。
//!
//! 大会话里一条工具输出可能有几 MB，整包过 IPC 再进 DOM 会让界面卡住。这里把超长
//! 正文换成「预览 + 定位」：预览足够渲染一屏，展开时前端再按 [`SessionMessageWire::index`]
//! 调 `get_message_content` 取回全文。
//!
//! 两条边界：
//! - 只有正文已接入解析缓存的 Agent 才允许截断，否则取全文要重新解析整个会话；
//! - 前端需要读全文才能判断的角色不下发预览，见 [`truncation_allowed`]。

use serde::Serialize;

use super::SessionMessage;

/// 超过这个字符数的正文才截断
pub const MAX_INLINE_MESSAGE_CHARS: usize = 4096;
/// 截断后下发的预览长度（与前端折叠时显示的字符数一致）
pub const MESSAGE_PREVIEW_CHARS: usize = 1500;

/// 一条消息的下发形态。未截断时与旧版字段完全一致。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMessageWire {
    pub role: String,
    /// 预览；未截断时就是全文
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<i64>,
    /// 截断时用于取回全文的下标（在会话正文里的绝对位置）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// 全文的字符数：前端用它提示大小，并在取全文时校验会话是否已变化
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_chars: Option<usize>,
}

/// 流式读取会话正文时按块下发的消息；与前端 `TranscriptChunk` 一一对应。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TranscriptChunk {
    /// 首包：总条数与正文规模，前端据此先建好虚拟列表
    Header { total: usize, approx_bytes: u64 },
    /// 从 `start` 开始的一段消息
    Messages {
        start: usize,
        messages: Vec<SessionMessageWire>,
    },
    /// 末包：已下发的条数
    Done { delivered: usize },
    /// 失败时先发这个包，再让命令以同样的文案返回 `Err`
    Error { message: String },
}

/// Codex 的 IDE 注入正文前缀，与前端 `CODEX_IDE_CONTEXT_PREFIX` 必须一致。
///
/// 这类正文里真实提问在**末尾**的「## My request for Codex:」段（前端
/// `extractCodexPromptFromIdeContext` 与 `extractCodexPromptPreview` 都靠它），
/// 所以这一类不能只下发开头。
const CODEX_IDE_CONTEXT_PREFIX: &str = "# Context from my IDE setup:";

/// 是否允许只下发预览。
///
/// 前提是该会话的正文已接入解析缓存（`session_manager::transcript_cache_covered`）：
/// 否则展开时取全文要重新解析整个会话，不如直接下发。
///
/// 在这个前提下，前端只用正文开头判断 Codex 注入消息（`# AGENTS.md instructions for `
/// 与 `<environment_context>` 都是纯前缀判断），这类大块注入正文截断后判定不变、界面
/// 照样隐藏它们，因此可以只发预览——它们往往是 Codex 会话里体积最大的一部分。
///
/// 唯一的例外是 IDE 注入：真实提问在正文末尾，截断会让前端判定「没有提问」而把这条
/// 整条隐藏，所以这类正文一律下发全文。
pub fn truncation_allowed(provider_id: &str, source_path: &str, role: &str, content: &str) -> bool {
    if !super::transcript_cache_covered(provider_id, source_path) {
        return false;
    }
    let ide_injected = provider_id == "codex"
        && role.eq_ignore_ascii_case("user")
        && content.trim_start().starts_with(CODEX_IDE_CONTEXT_PREFIX);
    !ide_injected
}

/// 按需截断一条消息；`index` 是它在会话正文里的绝对下标。
pub fn wire_message(
    provider_id: &str,
    source_path: &str,
    index: usize,
    message: &SessionMessage,
) -> SessionMessageWire {
    let chars = message.content.chars().count();
    if chars <= MAX_INLINE_MESSAGE_CHARS
        || !truncation_allowed(provider_id, source_path, &message.role, &message.content)
    {
        return SessionMessageWire {
            role: message.role.clone(),
            content: message.content.clone(),
            ts: message.ts,
            index: None,
            truncated: false,
            content_chars: None,
        };
    }

    SessionMessageWire {
        role: message.role.clone(),
        content: message
            .content
            .chars()
            .take(MESSAGE_PREVIEW_CHARS)
            .collect(),
        ts: message.ts,
        index: Some(index),
        truncated: true,
        content_chars: Some(chars),
    }
}

pub fn wire_messages(
    provider_id: &str,
    source_path: &str,
    messages: &[SessionMessage],
) -> Vec<SessionMessageWire> {
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| wire_message(provider_id, source_path, index, message))
        .collect()
}

/// 下发 `[start, end)` 这一段，保留下标在整段正文里的绝对位置。
pub fn wire_range(
    provider_id: &str,
    source_path: &str,
    messages: &[SessionMessage],
    start: usize,
    end: usize,
) -> Vec<SessionMessageWire> {
    messages[start..end]
        .iter()
        .enumerate()
        .map(|(offset, message)| wire_message(provider_id, source_path, start + offset, message))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: &str, chars: usize) -> SessionMessage {
        SessionMessage {
            role: role.to_string(),
            content: "中".repeat(chars),
            ts: Some(1),
        }
    }

    fn text_message(role: &str, content: &str) -> SessionMessage {
        SessionMessage {
            role: role.to_string(),
            content: content.to_string(),
            ts: None,
        }
    }

    /// 单文件源的 source_path；截断判定只关心它的形态（SQLite 前缀等）
    const FILE_SOURCE: &str = "/sessions/pi/session.jsonl";

    #[test]
    fn short_messages_keep_the_old_shape() {
        let wire = wire_message("claude", FILE_SOURCE, 3, &message("user", 100));

        assert_eq!(wire.content.chars().count(), 100);
        assert!(!wire.truncated);
        assert_eq!(wire.index, None);
        assert_eq!(wire.content_chars, None);
    }

    #[test]
    fn long_messages_send_a_preview_with_a_locator() {
        let original = MAX_INLINE_MESSAGE_CHARS + 500;
        let wire = wire_message("claude", FILE_SOURCE, 7, &message("tool", original));

        assert!(wire.truncated);
        assert_eq!(wire.index, Some(7), "取全文要用正文里的绝对下标");
        assert_eq!(wire.content_chars, Some(original), "全文长度用于提示与校验");
        assert_eq!(wire.content.chars().count(), MESSAGE_PREVIEW_CHARS);
        assert!(
            wire.content.chars().all(|c| c == '中'),
            "预览按字符切，不切断多字节字符"
        );
    }

    #[test]
    fn exactly_at_the_limit_is_not_truncated() {
        let wire = wire_message(
            "claude",
            FILE_SOURCE,
            0,
            &message("assistant", MAX_INLINE_MESSAGE_CHARS),
        );
        assert!(!wire.truncated);
    }

    #[test]
    fn truncation_keeps_codex_ide_injections_whole() {
        // IDE 注入：真实提问在末尾，截断会让前端把这条整条隐藏
        let request = format!(
            "{CODEX_IDE_CONTEXT_PREFIX}\n## My request for Codex:\n修一下登录\n{}",
            "x".repeat(MAX_INLINE_MESSAGE_CHARS)
        );
        assert!(!wire_message("codex", FILE_SOURCE, 0, &text_message("user", &request)).truncated);

        // 前缀前的空白不影响判定（前端先 trim 再判断）
        let indented = format!("\n  {}", request);
        assert!(!wire_message("codex", FILE_SOURCE, 0, &text_message("user", &indented)).truncated);

        // 这条例外只针对 Codex：别的 Agent 的同名前缀照常截断
        assert!(wire_message("claude", FILE_SOURCE, 0, &text_message("user", &request)).truncated);
    }

    #[test]
    fn truncation_still_applies_to_codex_injection_banners_and_tools() {
        // 注入横幅：前端只按开头前缀隐藏，截断后判定不变 → 只发预览
        for prefix in [
            "# AGENTS.md instructions for /tmp/project\n",
            "<environment_context>\n",
        ] {
            let injected = text_message(
                "user",
                &format!("{prefix}{}", "y".repeat(MAX_INLINE_MESSAGE_CHARS)),
            );
            assert!(
                wire_message("codex", FILE_SOURCE, 0, &injected).truncated,
                "注入横幅正文只发预览：{prefix}"
            );
        }

        // 体积最大的工具输出照常截断
        let tool = message("tool", MAX_INLINE_MESSAGE_CHARS + 1);
        assert!(wire_message("codex", FILE_SOURCE, 1, &tool).truncated);
        assert!(
            wire_message(
                "claude",
                FILE_SOURCE,
                0,
                &message("user", MAX_INLINE_MESSAGE_CHARS + 1)
            )
            .truncated
        );
    }

    #[test]
    fn truncation_follows_which_sessions_have_a_transcript_cache() {
        let tool = message("tool", MAX_INLINE_MESSAGE_CHARS + 1);
        let sqlite_db = "/data/opencode.db";

        // Pi 的正文就是那个会话文件，已接入缓存 → 可以只发预览
        assert!(wire_message("pi", FILE_SOURCE, 0, &tool).truncated);
        // OpenCode / ZCode 只有 SQLite 形态接入；旧版消息目录展开要重解析整个会话
        assert!(wire_message("opencode", "sqlite:/data/opencode.db:ses_1", 0, &tool).truncated);
        assert!(!wire_message("opencode", "/data/storage/message/ses_1", 0, &tool).truncated);
        assert!(
            wire_message("zcode", "sqlite-zcode:/data/db.sqlite:session-1", 0, &tool).truncated
        );
        assert!(!wire_message("zcode", sqlite_db, 0, &tool).truncated);
    }

    #[test]
    fn wire_range_keeps_absolute_indexes() {
        let messages = vec![
            message("user", 10),
            message("assistant", 10),
            message("tool", MAX_INLINE_MESSAGE_CHARS + 1),
            message("tool", 10),
        ];

        let wire = wire_range("claude", FILE_SOURCE, &messages, 2, 4);
        assert_eq!(wire.len(), 2);
        assert_eq!(wire[0].index, Some(2));
        assert!(wire[0].truncated);
        assert_eq!(wire[1].index, None);
    }
}
