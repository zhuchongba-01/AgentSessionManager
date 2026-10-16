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
    /// 全文的字符数：前端用它提示大小
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_chars: Option<usize>,
    /// 全文的身份摘要：前端取回全文时会带回来校验，见 [`content_identity_hex`]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_identity: Option<String>,
}

/// 一条消息正文的身份摘要（FNV-1a 64 位）。
///
/// 取回被截断消息的全文时，只有「下标 + 长度」不足以确认还是同一条消息：会话在
/// 两次请求之间被改写后，同一下标可能换成了另一条**长度恰好相同**的消息，长度校验
/// 会放行，用户拿到的就是别人那一条的正文。把 role、时间戳与全文一起摘要，才真正
/// 表达了「这条消息的身份」。仅用于一致性校验，不用于安全用途。
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

/// 身份摘要的下发形态。
///
/// 用 16 进制字符串而不是 JSON 数字：64 位摘要超出 IEEE-754 双精度的整数范围
/// （2^53），直接当数字过一遍 JSON 会丢精度，前端再送回后端时比较必然失配，
/// 于是每次展开都误报「会话已更新」。字符串形式原样往返。
pub fn content_identity_hex(role: &str, ts: Option<i64>, content: &str) -> String {
    format!("{:016x}", content_identity(role, ts, content))
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
/// 前提是该会话的正文**这次解析确实进了缓存**（`Transcript::cacheable`）：否则展开时
/// 取全文要重新解析整个会话，不如直接下发。
///
/// 在这个前提下，前端只用正文开头判断 Codex 注入消息（`# AGENTS.md instructions for `
/// 与 `<environment_context>` 都是纯前缀判断），这类大块注入正文截断后判定不变、界面
/// 照样隐藏它们，因此可以只发预览——它们往往是 Codex 会话里体积最大的一部分。
///
/// 唯一的例外是 IDE 注入：真实提问在正文末尾，截断会让前端判定「没有提问」而把这条
/// 整条隐藏，所以这类正文一律下发全文。
pub fn truncation_allowed(cacheable: bool, provider_id: &str, role: &str, content: &str) -> bool {
    if !cacheable {
        return false;
    }
    let ide_injected = provider_id == "codex"
        && role.eq_ignore_ascii_case("user")
        && content.trim_start().starts_with(CODEX_IDE_CONTEXT_PREFIX);
    !ide_injected
}

/// 按需截断一条消息；`index` 是它在会话正文里的绝对下标。
pub fn wire_message(
    cacheable: bool,
    provider_id: &str,
    index: usize,
    message: &SessionMessage,
) -> SessionMessageWire {
    let chars = message.content.chars().count();
    if chars <= MAX_INLINE_MESSAGE_CHARS
        || !truncation_allowed(cacheable, provider_id, &message.role, &message.content)
    {
        return SessionMessageWire {
            role: message.role.clone(),
            content: message.content.clone(),
            ts: message.ts,
            index: None,
            truncated: false,
            content_chars: None,
            content_identity: None,
        };
    }

    // 只在真的要截断时算摘要：未截断的消息前端不会回取全文，没必要付这份成本
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

/// 下发 `[start, end)` 这一段，保留下标在整段正文里的绝对位置。
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
        }
    }

    fn text_message(role: &str, content: &str) -> SessionMessage {
        SessionMessage {
            role: role.to_string(),
            content: content.to_string(),
            ts: None,
        }
    }

    /// 身份摘要的固定向量。
    ///
    /// 前端测试用的 `contentIdentityHex`（tests/msw/state.ts）是这份 Rust 实现的
    /// 镜像，必须逐字节一致；写死向量后，任何一侧改算法都会在另一侧失败，而不是
    /// 双双"自洽地"错下去。
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

        // 与 tests/msw/state.ts 的 contentIdentityHex 输出一致
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
            content_identity("tool", None, &b),
            "同长度但不同正文必须得到不同身份，否则长度校验会放行另一条消息"
        );
        // 时间戳与角色同样参与身份
        assert_ne!(
            content_identity("tool", Some(1), &a),
            content_identity("tool", Some(2), &a)
        );
        assert_ne!(
            content_identity("tool", None, &a),
            content_identity("user", None, &a)
        );
    }

    /// 已接入正文缓存的会话：可以只发预览
    const CACHED: bool = true;
    /// 未接入正文缓存的会话：展开要重解析整个会话，不能截断
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
    fn long_messages_send_a_preview_with_a_locator() {
        let original = MAX_INLINE_MESSAGE_CHARS + 500;
        let wire = wire_message(CACHED, "claude", 7, &message("tool", original));

        assert!(wire.truncated);
        assert_eq!(wire.index, Some(7), "取全文要用正文里的绝对下标");
        assert_eq!(wire.content_chars, Some(original), "全文长度用于提示与校验");
        assert_eq!(wire.content.chars().count(), MESSAGE_PREVIEW_CHARS);
        assert!(
            wire.content.chars().all(|c| c == '中'),
            "预览按字符切，不切断多字节字符"
        );
        assert!(
            wire.content_identity.is_some(),
            "截断时必须带上身份摘要，否则取全文只能靠长度判定"
        );
    }

    #[test]
    fn exactly_at_the_limit_is_not_truncated() {
        let wire = wire_message(
            CACHED,
            "claude",
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
        assert!(!wire_message(CACHED, "codex", 0, &text_message("user", &request)).truncated);

        // 前缀前的空白不影响判定（前端先 trim 再判断）
        let indented = format!("\n  {}", request);
        assert!(!wire_message(CACHED, "codex", 0, &text_message("user", &indented)).truncated);

        // 这条例外只针对 Codex：别的 Agent 的同名前缀照常截断
        assert!(wire_message(CACHED, "claude", 0, &text_message("user", &request)).truncated);
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
                wire_message(CACHED, "codex", 0, &injected).truncated,
                "注入横幅正文只发预览：{prefix}"
            );
        }

        // 体积最大的工具输出照常截断
        let tool = message("tool", MAX_INLINE_MESSAGE_CHARS + 1);
        assert!(wire_message(CACHED, "codex", 1, &tool).truncated);
        assert!(
            wire_message(
                CACHED,
                "claude",
                0,
                &message("user", MAX_INLINE_MESSAGE_CHARS + 1)
            )
            .truncated
        );
    }

    /// 截断与否只取决于「这次解析是否真的进了缓存」，不再单独猜会话形态。
    ///
    /// 旧实现按 provider + source_path 推断可缓存性，于是 Codex 的分页链读不出来时
    /// （`transcript_sources` 返回 None，实际不缓存）仍被允许截断，展开就变成一次
    /// 全量重解析。现在这个事实由解析结果自己携带。
    #[test]
    fn truncation_requires_an_actually_cached_transcript() {
        let tool = message("tool", MAX_INLINE_MESSAGE_CHARS + 1);

        for provider in ["claude", "codex", "deepseek", "grokbuild", "pi"] {
            assert!(
                wire_message(CACHED, provider, 0, &tool).truncated,
                "{provider} 已缓存时可只发预览"
            );
            assert!(
                !wire_message(UNCACHED, provider, 0, &tool).truncated,
                "{provider} 未进缓存时不能截断"
            );
        }

        // 未接入缓存形态的会话，即使调用方传了 true 也不该被前端回取全文；
        // 调用方据此传参，这里固定住两种取值的差异
        assert!(!wire_message(UNCACHED, "opencode", 0, &tool).truncated);
        assert!(wire_message(CACHED, "opencode", 0, &tool).truncated);
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
