//! 下发给前端的会话正文形态。
//!
//! - 结构化消息（`!blocks.is_empty()`）：各块内部已完成细粒度预览截断与 [`ContentRef`] 引用绑定，
//!   真人提问与最终回复完整保留 Markdown，不下发冗余的投影 `content`；
//! - 旧式纯文本消息（`blocks.is_empty()`）：继续按 [`MAX_INLINE_MESSAGE_CHARS`] 做预览截断并附带身份摘要。

use serde::Serialize;

use super::model::{MessageMeta, SessionBlock, SessionMessage, TurnIndex};

pub const MAX_INLINE_MESSAGE_CHARS: usize = 4096;
pub const MESSAGE_PREVIEW_CHARS: usize = 1500;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMessageWire {
    pub role: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub injected: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<SessionBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<MessageMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_identity: Option<String>,
}

pub fn content_identity(role: &str, ts: Option<i64>, content: &str) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    let mut mix = |byte: u8| {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    };
    for byte in role.as_bytes() {
        mix(*byte);
    }
    mix(0);
    for byte in ts.unwrap_or(i64::MIN).to_le_bytes() {
        mix(byte);
    }
    for byte in content.as_bytes() {
        mix(*byte);
    }
    hash
}

pub fn content_identity_hex(role: &str, ts: Option<i64>, content: &str) -> String {
    format!("{:016x}", content_identity(role, ts, content))
}

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TranscriptChunk {
    Header {
        total: usize,
        approx_bytes: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        turns: Vec<TurnIndex>,
    },
    Messages {
        start: usize,
        messages: Vec<SessionMessageWire>,
    },
    Done {
        delivered: usize,
    },
    Error {
        message: String,
    },
}

const CODEX_IDE_CONTEXT_PREFIX: &str = "# Context from my IDE setup:";

pub fn truncation_allowed(cacheable: bool, provider_id: &str, role: &str, content: &str) -> bool {
    if !cacheable {
        return false;
    }
    let ide_injected = provider_id == "codex"
        && role.eq_ignore_ascii_case("user")
        && content.trim_start().starts_with(CODEX_IDE_CONTEXT_PREFIX);
    !ide_injected
}

pub fn wire_message(
    cacheable: bool,
    provider_id: &str,
    index: usize,
    message: &SessionMessage,
) -> SessionMessageWire {
    if !message.blocks.is_empty() {
        return SessionMessageWire {
            role: message.role.clone(),
            content: String::new(),
            ts: message.ts,
            id: message.id.clone(),
            turn_id: message.turn_id.clone(),
            injected: message.injected,
            blocks: message.blocks.clone(),
            meta: message.meta.clone(),
            index: None,
            truncated: false,
            content_chars: None,
            content_identity: None,
        };
    }

    let chars = message.content.chars().count();
    if chars <= MAX_INLINE_MESSAGE_CHARS
        || !truncation_allowed(cacheable, provider_id, &message.role, &message.content)
    {
        return SessionMessageWire {
            role: message.role.clone(),
            content: message.content.clone(),
            ts: message.ts,
            id: message.id.clone(),
            turn_id: message.turn_id.clone(),
            injected: message.injected,
            blocks: Vec::new(),
            meta: message.meta.clone(),
            index: None,
            truncated: false,
            content_chars: None,
            content_identity: None,
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
        id: message.id.clone(),
        turn_id: message.turn_id.clone(),
        injected: message.injected,
        blocks: Vec::new(),
        meta: message.meta.clone(),
        index: Some(index),
        truncated: true,
        content_chars: Some(chars),
        content_identity: Some(content_identity_hex(
            &message.role,
            message.ts,
            &message.content,
        )),
    }
}

pub fn wire_messages(
    cacheable: bool,
    provider_id: &str,
    messages: &[SessionMessage],
) -> Vec<SessionMessageWire> {
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| wire_message(cacheable, provider_id, index, message))
        .collect()
}

pub fn wire_range(
    cacheable: bool,
    provider_id: &str,
    messages: &[SessionMessage],
    start: usize,
    end: usize,
) -> Vec<SessionMessageWire> {
    messages[start..end]
        .iter()
        .enumerate()
        .map(|(offset, message)| wire_message(cacheable, provider_id, start + offset, message))
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
            ..SessionMessage::default()
        }
    }

    #[test]
    fn content_identity_hex_matches_the_pinned_vectors() {
        let cases = [
            ("tool", None, "hello"),
            ("user", Some(0), ""),
            ("assistant", Some(1_700_000_000_000), "中"),
            ("tool", Some(-1), "混合 mixed 内容"),
        ];
        let actual = cases
            .iter()
            .map(|(role, ts, content)| content_identity_hex(role, *ts, content))
            .collect::<Vec<_>>();

        assert_eq!(
            actual,
            vec![
                "89d353117a8bfbab".to_string(),
                "cfae74179de2f5f6".to_string(),
                "b9aa5e9f6edb5fe8".to_string(),
                "3de0a87aad98a68b".to_string(),
            ]
        );
    }

    #[test]
    fn content_identity_distinguishes_same_length_content() {
        let a = "AAAA".repeat(1100);
        let b = "BBBB".repeat(1100);
        assert_eq!(a.len(), b.len());
        assert_ne!(
            content_identity("tool", None, &a),
            content_identity("tool", None, &b)
        );
        assert_ne!(
            content_identity("tool", Some(1), &a),
            content_identity("tool", Some(2), &a)
        );
        assert_ne!(
            content_identity("tool", None, &a),
            content_identity("user", None, &a)
        );
    }

    const CACHED: bool = true;
    const UNCACHED: bool = false;

    #[test]
    fn short_messages_keep_the_old_shape() {
        let wire = wire_message(CACHED, "claude", 3, &message("user", 100));

        assert_eq!(wire.content.chars().count(), 100);
        assert!(!wire.truncated);
        assert_eq!(wire.index, None);
        assert_eq!(wire.content_chars, None);
    }

    #[test]
    fn structured_blocks_bypass_legacy_content_truncation() {
        let long_reply = "答复".repeat(MAX_INLINE_MESSAGE_CHARS + 200);
        let msg = SessionMessage::from_blocks(
            "assistant",
            Some(10),
            vec![SessionBlock::text(long_reply.clone())],
        );
        let wire = wire_message(CACHED, "claude", 0, &msg);
        assert!(!wire.truncated);
        assert!(wire.content.is_empty());
        assert_eq!(wire.blocks, vec![SessionBlock::text(long_reply)]);
    }

    #[test]
    fn long_messages_send_a_preview_with_a_locator() {
        let original = MAX_INLINE_MESSAGE_CHARS + 500;
        let wire = wire_message(CACHED, "claude", 7, &message("tool", original));

        assert!(wire.truncated);
        assert_eq!(wire.index, Some(7));
        assert_eq!(wire.content_chars, Some(original));
        assert_eq!(wire.content.chars().count(), MESSAGE_PREVIEW_CHARS);
        assert!(wire.content.chars().all(|c| c == '中'));
        assert!(wire.content_identity.is_some());
    }

    #[test]
    fn truncation_requires_an_actually_cached_transcript() {
        let tool = message("tool", MAX_INLINE_MESSAGE_CHARS + 1);
        for provider in ["claude", "codex", "deepseek", "grokbuild", "pi"] {
            assert!(wire_message(CACHED, provider, 0, &tool).truncated);
            assert!(!wire_message(UNCACHED, provider, 0, &tool).truncated);
        }
    }

    #[test]
    fn wire_range_keeps_absolute_indexes() {
        let messages = vec![
            message("user", 10),
            message("assistant", 10),
            message("tool", MAX_INLINE_MESSAGE_CHARS + 1),
            message("tool", 10),
        ];

        let wire = wire_range(CACHED, "claude", &messages, 2, 4);
        assert_eq!(wire.len(), 2);
        assert_eq!(wire[0].index, Some(2));
        assert!(wire[0].truncated);
        assert_eq!(wire[1].index, None);
    }
}
