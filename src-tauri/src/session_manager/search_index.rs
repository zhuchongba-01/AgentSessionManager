//! 会话正文全文检索索引（SQLite FTS5）。
//!
//! 会话列表的轻量元数据过滤（标题、摘要、目录、ID）可在内存完成；完整对话正文体量大，
//! 在本地维护独立的 SQLite FTS5 缓存索引 `AgentSessionManager/session-index.db`：
//!
//! - 独立存储于本地数据目录，不修改任何 Agent 原生会话文件或数据库；删除或损坏后可随时重建。
//! - 只收录 `user` 与 `assistant` 的有效正文，跳过 `thinking`、`tool` 以及注入式系统提示词。
//! - 双表索引：`messages_fts`（`trigram case_sensitive 1`，存原文与小写规范化文本）负责 `>= 3`
//!   字符子串检索与 BM25 排序；`short_terms`（无正文 `contentless_delete=1`）收录单字符与双字符
//!   Unicode 码位组合，覆盖 1～2 字中日韩与短英文/符号词，避免全表扫描。
//! - 支持多关键词空白分隔 `AND` 组合检索，并在完整正文上计算精确的 UTF-16 半开区间高亮范围。
//! - 增量同步基于 `TranscriptSources` 指纹（含文件 `(mtime, len)` 与 SQLite `-wal` 内容摘要），
//!   未变更会话零解析、零重写。

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, params_from_iter, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;

use super::cache::{self, Fingerprint, MemberFingerprint, Transcript, TranscriptSources};
use super::{SessionMessage, SessionMeta};

const INDEX_DIR_NAME: &str = "AgentSessionManager";
const INDEX_FILE_NAME: &str = "session-index.db";
/// 索引 schema 与收录规则版本号；升级后自动丢弃旧表重建并清理边车文件。
const INDEX_VERSION: i64 = 2;
/// `rowid = (source_id << 24) | message_index`，单会话最多收录 16,777,216 条消息。
const ROW_SHIFT: u32 = 24;
const MAX_MESSAGES_PER_SOURCE: usize = 1 << ROW_SHIFT;
const ROW_INDEX_MASK: i64 = (1 << ROW_SHIFT) - 1;
/// 单条消息最多收录的字符数。
pub const MAX_ROW_CHARS: usize = 20_000;
/// 单次查询在按会话聚合前最多扫描的候选消息行数。
const MAX_HIT_ROWS: i64 = 20_000;
/// 默认最多返回的命中会话数。
pub const DEFAULT_LIMIT: usize = 200;
/// 单次查询允许的最大返回会话数上限。
const MAX_LIMIT: usize = 1_000;
/// 单个命中会话最多附带的摘录条数。
const MAX_SNIPPETS_PER_SESSION: usize = 3;
/// trigram 索引可处理的最小词长（字符数）。
const MIN_TRIGRAM_CHARS: usize = 3;
/// 摘录窗口在首个命中词前后保留的字符数。
const SNIPPET_BEFORE: usize = 30;
const SNIPPET_AFTER: usize = 90;
/// 增量同步每处理多少个会话上报一次进度。
const PROGRESS_EVERY: usize = 25;
/// 变更会话达到该阈值时触发 FTS 段合并优化（首次建库额外执行 `VACUUM`）。
const COMPACT_AFTER_CHANGES: usize = 50;
/// 提前到达的取消令牌缓存上限，防止孤立请求 ID 无限增长。
const MAX_TRACKED_SEARCH_TOKENS: usize = 256;
/// 搜索被取消时返回的固定错误文案。
pub const SEARCH_CANCELLED: &str = "Session content search cancelled";
/// 内部哨兵：`sync_index` 运行期间遇到 `clear_index` 时立即中止本轮写入。
const SYNC_ABORTED_BY_CLEAR: &str = "__session_index_cleared_during_sync__";

/// 用户消息中需要跳过的注入式脚手架/环境上下文前缀（去除前导空白后匹配）。
const INJECTED_USER_PREFIXES: &[&str] = &[
    "# AGENTS.md instructions",
    "<environment_context>",
    "<permissions instructions>",
    "<user_instructions>",
    "<system-reminder>",
    "<INSTRUCTIONS>",
];

/// 摘录内的高亮范围，采用与 JavaScript `String.prototype.slice` 一致的 UTF-16 半开区间 `[start, end)`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentHighlight {
    pub start: usize,
    pub end: usize,
}

/// 单条命中消息的摘录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentSnippet {
    /// 消息在 `load_transcript` 返回列表中的原始下标。
    pub message_index: usize,
    /// 命中位置附近的纯文本摘录（连续空白已折叠为单个空格）。
    pub text: String,
    /// 摘录内所有命中词对应的 UTF-16 半开区间（非重叠、严格递增）。
    pub highlights: Vec<ContentHighlight>,
}

/// 单个会话的正文搜索命中聚合结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentHit {
    pub provider_id: String,
    pub source_path: String,
    pub snippets: Vec<ContentSnippet>,
    pub match_count: usize,
}

/// 索引同步状态；`running == false` 且 `processed == total` 表示最近一轮同步已完成。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexStatus {
    pub running: bool,
    pub processed: usize,
    pub total: usize,
}

/// 正文搜索结果：命中列表与当前索引状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentSearchResult {
    pub hits: Vec<ContentHit>,
    pub status: IndexStatus,
}

/// 单个会话在增量同步中的判定结果。
#[derive(Debug, Clone)]
pub enum SessionIndexAction {
    /// 指纹未变，跳过解析与 FTS 改写。
    Unchanged,
    /// 新会话或指纹已变，使用新解析的正文替换索引行。
    Updated {
        revision: String,
        messages: Arc<Transcript>,
    },
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn read_rwlock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_rwlock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 返回本地独立 FTS5 索引数据库路径：`<LocalAppData>/AgentSessionManager/session-index.db`。
pub fn index_path() -> Result<PathBuf, String> {
    Ok(dirs::data_local_dir()
        .ok_or_else(|| "无法定位本地索引目录".to_string())?
        .join(INDEX_DIR_NAME)
        .join(INDEX_FILE_NAME))
}

fn is_injected_user_prompt(text: &str) -> bool {
    let trimmed = text.trim_start();
    INJECTED_USER_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => text[..byte_idx].to_string(),
        None => text.to_string(),
    }
}

/// 提取会话中需要建立全文索引的 `(message_index, content)` 行：
/// - 仅保留 `role == "user" | "assistant"`；
/// - 跳过空消息及 `user` 角色下的注入式系统提示词；
/// - 保留消息在原会话中的精确下标，并将单行截断到 [`MAX_ROW_CHARS`] 个字符。
pub fn message_rows(messages: &[SessionMessage]) -> Vec<(usize, String)> {
    messages
        .iter()
        .enumerate()
        .take(MAX_MESSAGES_PER_SOURCE)
        .filter(|(_, message)| {
            !message.injected && matches!(message.role.as_str(), "user" | "assistant")
        })
        .filter_map(|(index, message)| {
            let raw = super::model::searchable_text(message);
            let text = raw.trim();
            if text.is_empty() {
                return None;
            }
            if message.role == "user" && is_injected_user_prompt(text) {
                return None;
            }
            Some((index, truncate_chars(text, MAX_ROW_CHARS)))
        })
        .collect()
}

// ── 增量指纹编码与比对 ────────────────────────────────────────────────

fn format_system_time(time: Option<SystemTime>) -> String {
    let Some(time) = time else {
        return "none".to_string();
    };
    match time.duration_since(UNIX_EPOCH) {
        Ok(dur) => format!("+{}", dur.as_nanos()),
        Err(err) => format!("-{}", err.duration().as_nanos()),
    }
}

fn format_member_fingerprint(member: Option<&MemberFingerprint>) -> String {
    let Some(fp) = member else {
        return "M".to_string();
    };
    let mtime = format_system_time(fp.modified);
    let digest = match fp.digest {
        Some(hash) => format!("{hash:016x}"),
        None => "none".to_string(),
    };
    format!("P:{mtime}:{}:{digest}", fp.len)
}

fn format_sources_fingerprint(sources: &TranscriptSources, fingerprint: &Fingerprint) -> String {
    let mut encoded = format!("v1:{:016x}|", cache::sources_digest(sources));
    for (idx, (_path, member)) in fingerprint.members.iter().enumerate() {
        if idx > 0 {
            encoded.push(';');
        }
        encoded.push_str(&format_member_fingerprint(member.as_ref()));
    }
    encoded
}

/// 根据会话的正文依赖源计算确定性修订串（包含路径链顺序、截断偏移、mtime、长度及近期/WAL 内容摘要）。
pub fn revision_for_sources(sources: &TranscriptSources) -> Result<String, String> {
    let fingerprint = cache::transcript_fingerprint(sources).map_err(|e| e.to_string())?;
    Ok(format_sources_fingerprint(sources, &fingerprint))
}

#[derive(Debug, PartialEq, Eq)]
enum ParsedMember<'a> {
    Missing,
    Present {
        mtime: Option<&'a str>,
        len: &'a str,
        digest: Option<&'a str>,
    },
}

fn parse_member(raw: &str) -> Option<ParsedMember<'_>> {
    if raw == "M" {
        return Some(ParsedMember::Missing);
    }
    let rest = raw.strip_prefix("P:")?;
    let mut parts = rest.split(':');
    let mtime_raw = parts.next()?;
    let len = parts.next()?;
    let digest_raw = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let mtime = (mtime_raw != "none").then_some(mtime_raw);
    let digest = (digest_raw != "none").then_some(digest_raw);
    Some(ParsedMember::Present { mtime, len, digest })
}

/// 比对已持久化的修订串与最新修订串是否代表同一份正文快照。
///
/// 与 `cache::MemberFingerprint::matches` 保持一致：普通文件刚写入 2 秒内会附带内容摘要，
/// 2 秒后未再修改时摘要退化为 `None`；只要 `mtime` 存在且 `(mtime, len)` 完全一致，仍视为未变更，
/// 避免刚落盘的会话在下一轮同步时被多余重扫一次。SQLite `-wal` 始终携带摘要，任何同长度改写都会触发更新。
pub fn revision_matches(stored: &str, incoming: &str) -> bool {
    if stored == incoming {
        return true;
    }
    let (Some(stored_body), Some(incoming_body)) =
        (stored.strip_prefix("v1:"), incoming.strip_prefix("v1:"))
    else {
        return false;
    };
    let (Some((stored_head, stored_members)), Some((incoming_head, incoming_members))) =
        (stored_body.split_once('|'), incoming_body.split_once('|'))
    else {
        return false;
    };
    if stored_head != incoming_head {
        return false;
    }
    if stored_members.is_empty() && incoming_members.is_empty() {
        return true;
    }
    let mut s_iter = stored_members.split(';');
    let mut i_iter = incoming_members.split(';');
    loop {
        match (s_iter.next(), i_iter.next()) {
            (None, None) => return true,
            (Some(s_raw), Some(i_raw)) => {
                let (Some(s_mem), Some(i_mem)) = (parse_member(s_raw), parse_member(i_raw)) else {
                    return false;
                };
                match (s_mem, i_mem) {
                    (ParsedMember::Missing, ParsedMember::Missing) => continue,
                    (
                        ParsedMember::Present {
                            mtime: s_mtime,
                            len: s_len,
                            digest: s_digest,
                        },
                        ParsedMember::Present {
                            mtime: i_mtime,
                            len: i_len,
                            digest: i_digest,
                        },
                    ) => {
                        if s_mtime != i_mtime || s_len != i_len {
                            return false;
                        }
                        let digest_ok = match (s_digest, i_digest) {
                            (Some(a), Some(b)) => a == b,
                            (Some(_), None) | (None, Some(_)) => s_mtime.is_some(),
                            (None, None) => true,
                        };
                        if !digest_ok {
                            return false;
                        }
                    }
                    _ => return false,
                }
            }
            _ => return false,
        }
    }
}

/// 对无法直接枚举文件源的回退路径（如旧版目录散列源），按已解析消息正文计算稳定摘要。
fn revision_from_messages(messages: &[SessionMessage]) -> String {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    let mut mix = |bytes: &[u8]| {
        for &b in bytes {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(PRIME);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(PRIME);
    };
    mix(&messages.len().to_le_bytes());
    for msg in messages {
        mix(msg.role.as_bytes());
        mix(msg.content.as_bytes());
    }
    format!("msg:{hash:016x}:{}", messages.len())
}

// ── 摘录裁剪与多关键词 UTF-16 高亮计算 ───────────────────────────────

fn split_query_terms(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .filter(|term| !term.is_empty() && seen.insert(term.clone()))
        .collect()
}

/// 在完整原文上定位所有查询词（大小写不敏感子串），生成裁剪后的单行摘录与精确的 UTF-16 高亮区间。
pub fn make_highlighted_snippet(text: &str, query: &str) -> (String, Vec<ContentHighlight>) {
    let hay: Vec<char> = text.chars().collect();
    if hay.is_empty() {
        return (String::new(), Vec::new());
    }

    let normalized = text.to_lowercase();
    let terms = split_query_terms(query);

    // 某些字符小写化后会展开为多个码位（如 İ → i + \u{307}）；建立展开后码位下标到原文字符下标的映射，
    // 保证裁剪与高亮始终对齐原文字符边界，同时保留词尾 Σ 等上下文大小写结果。
    let original_positions: Vec<usize> = hay
        .iter()
        .enumerate()
        .flat_map(|(index, ch)| std::iter::repeat_n(index, ch.to_lowercase().count()))
        .collect();

    let mut matched = vec![false; hay.len()];
    let mut earliest_match: Option<(usize, usize)> = None;

    for needle in &terms {
        let needle_chars = needle.chars().count();
        if needle_chars == 0 {
            continue;
        }
        let mut previous_byte = 0;
        let mut normalized_char_at = 0;
        for (byte_idx, _) in normalized.match_indices(needle.as_str()) {
            normalized_char_at += normalized[previous_byte..byte_idx].chars().count();
            previous_byte = byte_idx;
            let start = original_positions[normalized_char_at];
            let end = original_positions[normalized_char_at + needle_chars - 1] + 1;
            matched[start..end].fill(true);
            match earliest_match {
                None => earliest_match = Some((start, end)),
                Some((prev_start, prev_end))
                    if start < prev_start || (start == prev_start && end > prev_end) =>
                {
                    earliest_match = Some((start, end));
                }
                _ => {}
            }
        }
    }

    let (window_start, window_end) = match earliest_match {
        Some((start, end)) => (
            start.saturating_sub(SNIPPET_BEFORE),
            (end + SNIPPET_AFTER).min(hay.len()),
        ),
        None => (0, (SNIPPET_BEFORE + SNIPPET_AFTER).min(hay.len())),
    };

    let mut snippet = String::new();
    let mut highlights: Vec<ContentHighlight> = Vec::new();
    let mut utf16_offset = 0usize;

    let mut append = |ch: char, is_highlighted: bool| {
        let next_utf16 = utf16_offset + ch.len_utf16();
        snippet.push(ch);
        if is_highlighted {
            if let Some(last) = highlights
                .last_mut()
                .filter(|range| range.end == utf16_offset)
            {
                last.end = next_utf16;
            } else {
                highlights.push(ContentHighlight {
                    start: utf16_offset,
                    end: next_utf16,
                });
            }
        }
        utf16_offset = next_utf16;
    };

    if window_start > 0 {
        append('…', false);
    }

    let mut pending_space = false;
    let mut pending_highlight = false;
    let mut started = false;
    for index in window_start..window_end {
        let ch = hay[index];
        if ch.is_whitespace() {
            pending_space = started;
            pending_highlight |= matched[index];
            continue;
        }
        if pending_space {
            append(' ', pending_highlight);
        }
        pending_space = false;
        pending_highlight = false;
        append(ch, matched[index]);
        started = true;
    }

    if window_end < hay.len() {
        append('…', false);
    }

    (snippet, highlights)
}

// ── FTS5 查询编码 ────────────────────────────────────────────────────

fn trigram_phrase(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

/// 将规范化文本中的全部单字符与相邻双字符编码为纯 ASCII 词元（`u<hex>` / `b<hex>z<hex>`），
/// 使 `1`～`2` 个字符的中日韩、ASCII 短词、标点与 Emoji 均可走倒排索引精确匹配字面子串。
fn encode_short_grams(normalized: &str) -> String {
    let mut grams = HashSet::new();
    let mut previous = None;
    for ch in normalized.chars() {
        grams.insert((ch, None));
        if let Some(prev) = previous {
            grams.insert((prev, Some(ch)));
        }
        previous = Some(ch);
    }
    let mut out = String::new();
    for (first, second) in grams {
        match second {
            Some(second) => {
                let _ = write!(out, "b{:x}z{:x} ", first as u32, second as u32);
            }
            None => {
                let _ = write!(out, "u{:x} ", first as u32);
            }
        }
    }
    out
}

fn encode_short_term_query(term: &str) -> String {
    let mut chars = term.chars();
    let first = chars.next().expect("short term must not be empty") as u32;
    match chars.next() {
        Some(second) => format!("b{first:x}z{:x}", second as u32),
        None => format!("u{first:x}"),
    }
}

enum SearchExecError {
    Cancelled,
    Sql(rusqlite::Error),
}

impl From<rusqlite::Error> for SearchExecError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sql(err)
    }
}

// ── SQLite FTS5 索引存储引擎 ──────────────────────────────────────────

pub struct SessionIndex {
    conn: Connection,
    generation: Arc<()>,
}

impl SessionIndex {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let mut conn = Connection::open(path)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != 0 && version != INDEX_VERSION {
            drop(conn);
            let _ = remove_index_files(path);
            conn = Connection::open(path)?;
        }
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    fn open_reader(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_millis(100))?;
        Ok(Self {
            conn,
            generation: Arc::new(()),
        })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != INDEX_VERSION {
            conn.execute_batch(
                "DROP TABLE IF EXISTS short_terms;
                 DROP TABLE IF EXISTS messages_fts;
                 DROP TABLE IF EXISTS sources;",
            )?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sources (
                 id INTEGER PRIMARY KEY,
                 provider_id TEXT NOT NULL,
                 source_path TEXT NOT NULL,
                 revision TEXT NOT NULL,
                 UNIQUE (provider_id, source_path)
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                 raw_content UNINDEXED,
                 content,
                 tokenize = 'trigram case_sensitive 1'
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS short_terms USING fts5(
                 tokens,
                 content = '',
                 contentless_delete = 1,
                 detail = 'none',
                 tokenize = 'unicode61 tokenchars ''_-'''
             );",
        )?;
        conn.pragma_update(None, "user_version", INDEX_VERSION)?;
        Ok(Self {
            conn,
            generation: Arc::new(()),
        })
    }

    #[allow(dead_code)]
    pub fn stored_revision(
        &self,
        provider_id: &str,
        source_path: &str,
    ) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT revision FROM sources WHERE provider_id = ?1 AND source_path = ?2",
                params![provider_id, source_path],
                |row| row.get(0),
            )
            .optional()
    }

    fn all_revisions(&self) -> rusqlite::Result<HashMap<(String, String), String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT provider_id, source_path, revision FROM sources")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect()
    }

    pub fn replace_source(
        &mut self,
        provider_id: &str,
        source_path: &str,
        revision: &str,
        rows: &[(usize, String)],
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        let existing_id: Option<i64> = tx
            .query_row(
                "SELECT id FROM sources WHERE provider_id = ?1 AND source_path = ?2",
                params![provider_id, source_path],
                |row| row.get(0),
            )
            .optional()?;
        let source_id = match existing_id {
            Some(id) => {
                delete_source_rows(&tx, id)?;
                tx.execute(
                    "UPDATE sources SET revision = ?1 WHERE id = ?2",
                    params![revision, id],
                )?;
                id
            }
            None => {
                tx.execute(
                    "INSERT INTO sources (provider_id, source_path, revision) VALUES (?1, ?2, ?3)",
                    params![provider_id, source_path, revision],
                )?;
                tx.last_insert_rowid()
            }
        };

        {
            let mut insert_fts = tx.prepare(
                "INSERT INTO messages_fts (rowid, raw_content, content) VALUES (?1, ?2, ?3)",
            )?;
            let mut insert_short =
                tx.prepare("INSERT INTO short_terms (rowid, tokens) VALUES (?1, ?2)")?;
            for (index, raw_text) in rows {
                if *index >= MAX_MESSAGES_PER_SOURCE {
                    break;
                }
                let rowid = (source_id << ROW_SHIFT) | (*index as i64);
                let normalized = raw_text.to_lowercase();
                let grams = encode_short_grams(&normalized);
                insert_fts.execute(params![rowid, raw_text, normalized])?;
                insert_short.execute(params![rowid, grams])?;
            }
        }

        tx.commit()
    }

    #[allow(dead_code)]
    pub fn remove_source(&mut self, provider_id: &str, source_path: &str) -> rusqlite::Result<()> {
        self.remove_sources_batch(&[(provider_id, source_path)])
    }

    pub fn remove_sources_batch(&mut self, targets: &[(&str, &str)]) -> rusqlite::Result<()> {
        if targets.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut lookup =
                tx.prepare("SELECT id FROM sources WHERE provider_id = ?1 AND source_path = ?2")?;
            let mut delete_src = tx.prepare("DELETE FROM sources WHERE id = ?1")?;
            for &(provider_id, source_path) in targets {
                if let Some(id) = lookup
                    .query_row(params![provider_id, source_path], |row| {
                        row.get::<_, i64>(0)
                    })
                    .optional()?
                {
                    delete_source_rows(&tx, id)?;
                    delete_src.execute(params![id])?;
                }
            }
        }
        tx.commit()
    }

    pub fn optimize_fts(&self) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO messages_fts (messages_fts) VALUES ('optimize')",
            [],
        )?;
        self.conn.execute(
            "INSERT INTO short_terms (short_terms) VALUES ('optimize')",
            [],
        )?;
        Ok(())
    }

    pub fn checkpoint(&self) -> rusqlite::Result<()> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
    }

    pub fn optimize_and_checkpoint(&self) -> rusqlite::Result<()> {
        self.optimize_fts()?;
        self.checkpoint()
    }

    pub fn compact(&self) -> rusqlite::Result<()> {
        self.optimize_fts()?;
        self.conn.execute_batch("VACUUM;")?;
        self.checkpoint()
    }

    #[cfg(test)]
    pub fn search(
        &self,
        query: &str,
        provider_id: Option<&str>,
        limit: usize,
    ) -> rusqlite::Result<Vec<ContentHit>> {
        let never_cancelled = AtomicBool::new(false);
        match self.search_cancellable(query, provider_id, limit, &never_cancelled) {
            Ok(hits) => Ok(hits),
            Err(SearchExecError::Cancelled) => Ok(Vec::new()),
            Err(SearchExecError::Sql(err)) => Err(err),
        }
    }

    fn search_cancellable(
        &self,
        query: &str,
        provider_id: Option<&str>,
        limit: usize,
        cancelled: &AtomicBool,
    ) -> Result<Vec<ContentHit>, SearchExecError> {
        let check = || {
            if cancelled.load(Ordering::Relaxed) {
                Err(SearchExecError::Cancelled)
            } else {
                Ok(())
            }
        };
        check()?;

        let terms = split_query_terms(query);
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let provider_filter = provider_id.map(str::trim).filter(|p| !p.is_empty());
        let mut long_terms = Vec::new();
        let mut short_terms = Vec::new();
        for term in &terms {
            if term.chars().count() >= MIN_TRIGRAM_CHARS {
                long_terms.push(term.as_str());
            } else {
                short_terms.push(term.as_str());
            }
        }

        // 在同一只读事务快照下完成候选检索、元数据关联与摘录读取，不阻塞后台写入线程。
        let snapshot = self.conn.unchecked_transaction()?;
        check()?;

        let (sql, arguments) = build_candidate_sql(&long_terms, &short_terms, provider_filter);
        let mut rowids = Vec::new();
        {
            let mut stmt = snapshot.prepare(&sql)?;
            let mut rows = stmt.query(params_from_iter(arguments.iter()))?;
            loop {
                check()?;
                let Some(row) = rows.next()? else { break };
                rowids.push(row.get::<_, i64>(0)?);
            }
        }

        if rowids.is_empty() {
            return Ok(Vec::new());
        }

        // 按会话聚合：保持首次命中顺序（相关度最高或最新），统计总命中消息数并截取前 N 条消息生成摘录。
        let mut order: Vec<i64> = Vec::new();
        let mut per_source: HashMap<i64, (Vec<i64>, usize)> = HashMap::new();
        for rowid in rowids {
            check()?;
            let source_id = rowid >> ROW_SHIFT;
            let (top_rows, count) = per_source.entry(source_id).or_insert_with(|| {
                order.push(source_id);
                (Vec::new(), 0)
            });
            *count += 1;
            if top_rows.len() < MAX_SNIPPETS_PER_SESSION {
                top_rows.push(rowid);
            }
        }

        order.truncate(limit);
        let selected_rowids: Vec<i64> = order
            .iter()
            .flat_map(|id| per_source[id].0.iter().copied())
            .collect();

        let mut snippets_by_row: HashMap<i64, ContentSnippet> =
            HashMap::with_capacity(selected_rowids.len());
        let mut source_meta: HashMap<i64, (String, String)> = HashMap::with_capacity(order.len());

        for chunk in selected_rowids.chunks(900) {
            check()?;
            let placeholders = vec!["?"; chunk.len()].join(",");
            let batch_sql = format!(
                "SELECT m.rowid, m.raw_content, s.id, s.provider_id, s.source_path
                 FROM messages_fts m
                 JOIN sources s ON s.id = (m.rowid >> {ROW_SHIFT})
                 WHERE m.rowid IN ({placeholders})"
            );
            let mut stmt = snapshot.prepare(&batch_sql)?;
            let mut rows = stmt.query(params_from_iter(chunk.iter().copied()))?;
            loop {
                check()?;
                let Some(row) = rows.next()? else { break };
                let rowid: i64 = row.get(0)?;
                let raw_content: String = row.get(1)?;
                check()?;
                let (text, highlights) = make_highlighted_snippet(&raw_content, query);
                snippets_by_row.insert(
                    rowid,
                    ContentSnippet {
                        message_index: (rowid & ROW_INDEX_MASK) as usize,
                        text,
                        highlights,
                    },
                );
                let src_id: i64 = row.get(2)?;
                if let std::collections::hash_map::Entry::Vacant(entry) = source_meta.entry(src_id)
                {
                    entry.insert((row.get::<_, String>(3)?, row.get::<_, String>(4)?));
                }
            }
        }

        let mut hits = Vec::with_capacity(order.len());
        for source_id in order {
            check()?;
            let (top_rows, match_count) = &per_source[&source_id];
            let (hit_provider, hit_source_path) = source_meta
                .get(&source_id)
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            let snippets = top_rows
                .iter()
                .map(|rowid| {
                    snippets_by_row
                        .remove(rowid)
                        .ok_or(rusqlite::Error::QueryReturnedNoRows)
                })
                .collect::<rusqlite::Result<Vec<_>>>()?;
            hits.push(ContentHit {
                provider_id: hit_provider.clone(),
                source_path: hit_source_path.clone(),
                snippets,
                match_count: *match_count,
            });
        }

        check()?;
        Ok(hits)
    }
}

fn build_candidate_sql(
    long_terms: &[&str],
    short_terms: &[&str],
    provider_filter: Option<&str>,
) -> (String, Vec<rusqlite::types::Value>) {
    match (!long_terms.is_empty(), !short_terms.is_empty()) {
        (true, false) => {
            let expr = long_terms
                .iter()
                .map(|term| trigram_phrase(term))
                .collect::<Vec<_>>()
                .join(" AND ");
            let mut args: Vec<rusqlite::types::Value> = vec![expr.into(), MAX_HIT_ROWS.into()];
            let filter_clause = if let Some(provider) = provider_filter {
                args.push(provider.to_string().into());
                "AND s.provider_id = ?3"
            } else {
                ""
            };
            let sql = format!(
                "SELECT m.rowid
                 FROM messages_fts m
                 JOIN sources s ON s.id = (m.rowid >> {ROW_SHIFT})
                 WHERE messages_fts MATCH ?1 {filter_clause}
                 ORDER BY m.rank
                 LIMIT ?2"
            );
            (sql, args)
        }
        (false, true) => {
            let expr = short_terms
                .iter()
                .map(|term| encode_short_term_query(term))
                .collect::<Vec<_>>()
                .join(" AND ");
            let mut args: Vec<rusqlite::types::Value> = vec![expr.into(), MAX_HIT_ROWS.into()];
            let filter_clause = if let Some(provider) = provider_filter {
                args.push(provider.to_string().into());
                "AND s.provider_id = ?3"
            } else {
                ""
            };
            let sql = format!(
                "SELECT m.rowid
                 FROM short_terms m
                 JOIN sources s ON s.id = (m.rowid >> {ROW_SHIFT})
                 WHERE short_terms MATCH ?1 {filter_clause}
                 ORDER BY m.rowid DESC
                 LIMIT ?2"
            );
            (sql, args)
        }
        (true, true) => {
            let long_expr = long_terms
                .iter()
                .map(|term| trigram_phrase(term))
                .collect::<Vec<_>>()
                .join(" AND ");
            let short_expr = short_terms
                .iter()
                .map(|term| encode_short_term_query(term))
                .collect::<Vec<_>>()
                .join(" AND ");
            let mut args: Vec<rusqlite::types::Value> =
                vec![long_expr.into(), short_expr.into(), MAX_HIT_ROWS.into()];
            let filter_clause = if let Some(provider) = provider_filter {
                args.push(provider.to_string().into());
                "AND s.provider_id = ?4"
            } else {
                ""
            };
            let sql = format!(
                "SELECT m.rowid
                 FROM messages_fts m
                 JOIN sources s ON s.id = (m.rowid >> {ROW_SHIFT})
                 WHERE messages_fts MATCH ?1
                   AND m.rowid IN (SELECT rowid FROM short_terms WHERE short_terms MATCH ?2)
                   {filter_clause}
                 ORDER BY m.rank
                 LIMIT ?3"
            );
            (sql, args)
        }
        (false, false) => unreachable!("caller guarantees at least one query term"),
    }
}

fn delete_source_rows(conn: &Connection, source_id: i64) -> rusqlite::Result<()> {
    let start = source_id << ROW_SHIFT;
    let end = (source_id + 1) << ROW_SHIFT;
    conn.execute(
        "DELETE FROM short_terms WHERE rowid >= ?1 AND rowid < ?2",
        params![start, end],
    )?;
    conn.execute(
        "DELETE FROM messages_fts WHERE rowid >= ?1 AND rowid < ?2",
        params![start, end],
    )?;
    Ok(())
}

fn is_corruption(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
    )
}

fn remove_index_files(path: &Path) -> std::io::Result<()> {
    remove_index_files_with(path, |target| fs::remove_file(target))
}

fn remove_index_files_with(
    path: &Path,
    mut remove: impl FnMut(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut first_error = None;
    for suffix in ["", "-wal", "-shm"] {
        let mut candidate = path.as_os_str().to_owned();
        candidate.push(suffix);
        if let Err(err) = remove(Path::new(&candidate)) {
            if err.kind() != std::io::ErrorKind::NotFound && first_error.is_none() {
                first_error = Some(err);
            }
        }
    }
    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

// ── 搜索取消令牌管理（无 dashmap、无 rusqlite hooks） ─────────────────

static SEARCH_TOKENS: LazyLock<Mutex<HashMap<String, Arc<AtomicBool>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct SearchTokenGuard {
    id: Option<String>,
    token: Arc<AtomicBool>,
}

impl SearchTokenGuard {
    fn register(search_id: Option<&str>) -> Self {
        let Some(id) = search_id.map(str::trim).filter(|id| !id.is_empty()) else {
            return Self {
                id: None,
                token: Arc::new(AtomicBool::new(false)),
            };
        };

        let mut map = lock_mutex(&SEARCH_TOKENS);
        if let Some(previous) = map.remove(id) {
            if previous.load(Ordering::Relaxed) {
                // 取消请求先于查询线程启动：直接返回已取消状态并消费掉预取消记录。
                return Self {
                    id: None,
                    token: previous,
                };
            }
            // 同 ID 的旧查询仍在运行：取消旧令牌，由新令牌接管该 ID。
            previous.store(true, Ordering::Relaxed);
        }

        let token = Arc::new(AtomicBool::new(false));
        map.insert(id.to_string(), Arc::clone(&token));
        Self {
            id: Some(id.to_string()),
            token,
        }
    }

    fn is_cancelled(&self) -> bool {
        self.token.load(Ordering::Relaxed)
    }
}

impl Drop for SearchTokenGuard {
    fn drop(&mut self) {
        let Some(id) = self.id.as_deref() else {
            return;
        };
        let mut map = lock_mutex(&SEARCH_TOKENS);
        if map
            .get(id)
            .is_some_and(|current| Arc::ptr_eq(current, &self.token))
        {
            map.remove(id);
        }
    }
}

/// 取消指定 `search_id` 的进行中（或即将启动）的正文搜索请求。
pub fn cancel_search(search_id: &str) {
    let id = search_id.trim();
    if id.is_empty() {
        return;
    }
    let mut map = lock_mutex(&SEARCH_TOKENS);
    if let Some(token) = map.get(id) {
        token.store(true, Ordering::Relaxed);
        return;
    }
    if map.len() >= MAX_TRACKED_SEARCH_TOKENS {
        map.retain(|_, token| !token.load(Ordering::Relaxed));
        if map.len() >= MAX_TRACKED_SEARCH_TOKENS {
            if let Some(victim) = map.keys().next().cloned() {
                map.remove(&victim);
            }
        }
    }
    map.insert(id.to_string(), Arc::new(AtomicBool::new(true)));
}

fn cancel_all_active_searches() {
    let map = lock_mutex(&SEARCH_TOKENS);
    for token in map.values() {
        token.store(true, Ordering::Relaxed);
    }
}

// ── 索引运行时协调器（支持全局单例与测试期独立沙箱） ──────────────────

struct IndexRuntime {
    /// 普通读写共享读锁；初始化、清理与损坏重建独占写锁，确保 Windows 下删除文件前所有连接已关闭。
    lifetime: RwLock<()>,
    slot: Mutex<Option<SessionIndex>>,
    status: Mutex<IndexStatus>,
    running: AtomicBool,
    clear_generation: AtomicU64,
}

struct SyncRunGuard<'a> {
    runtime: &'a IndexRuntime,
    sync_gen: u64,
    finished: bool,
}

impl<'a> SyncRunGuard<'a> {
    fn finish<F>(mut self, final_status: IndexStatus, emit_status: &mut F) -> IndexStatus
    where
        F: FnMut(IndexStatus),
    {
        self.finished = true;
        if self.runtime.clear_generation.load(Ordering::SeqCst) != self.sync_gen {
            self.runtime.running.store(false, Ordering::SeqCst);
            return self.runtime.current_status();
        }
        *lock_mutex(&self.runtime.status) = final_status;
        self.runtime.running.store(false, Ordering::SeqCst);
        emit_status(final_status);
        final_status
    }
}

impl Drop for SyncRunGuard<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if self.runtime.clear_generation.load(Ordering::SeqCst) == self.sync_gen {
            lock_mutex(&self.runtime.status).running = false;
        }
        self.runtime.running.store(false, Ordering::SeqCst);
    }
}

impl IndexRuntime {
    fn new() -> Self {
        Self {
            lifetime: RwLock::new(()),
            slot: Mutex::new(None),
            status: Mutex::new(IndexStatus::default()),
            running: AtomicBool::new(false),
            clear_generation: AtomicU64::new(0),
        }
    }

    fn current_status(&self) -> IndexStatus {
        *lock_mutex(&self.status)
    }

    fn update_status_if_current<F>(
        &self,
        sync_gen: u64,
        status: IndexStatus,
        emit_status: &mut F,
    ) -> bool
    where
        F: FnMut(IndexStatus),
    {
        if self.clear_generation.load(Ordering::SeqCst) != sync_gen {
            return false;
        }
        *lock_mutex(&self.status) = status;
        emit_status(status);
        true
    }

    fn discard_corrupt_index(&self, path: &Path, generation: Option<Arc<()>>) {
        let _exclusive = write_rwlock(&self.lifetime);
        let mut guard = lock_mutex(&self.slot);
        let matches_generation = match (guard.as_ref(), generation.as_ref()) {
            (Some(index), Some(expected)) => Arc::ptr_eq(&index.generation, expected),
            (None, None) => true,
            _ => false,
        };
        if matches_generation {
            *guard = None;
            if let Err(err) = remove_index_files(path) {
                log::warn!("清理损坏的会话正文索引失败: {err}");
            }
        }
    }

    fn with_index<T>(
        &self,
        path: &Path,
        expected_clear_gen: Option<u64>,
        f: impl FnOnce(&mut SessionIndex) -> rusqlite::Result<T>,
    ) -> Result<T, String> {
        let mut shared = Some(read_rwlock(&self.lifetime));
        let exclusive = if lock_mutex(&self.slot).is_none() {
            drop(shared.take());
            Some(write_rwlock(&self.lifetime))
        } else {
            None
        };

        let mut guard = lock_mutex(&self.slot);
        if let Some(expected) = expected_clear_gen {
            if self.clear_generation.load(Ordering::SeqCst) != expected {
                return Err(SYNC_ABORTED_BY_CLEAR.to_string());
            }
        }

        if guard.is_none() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("无法创建会话索引目录 {}: {e}", parent.display()))?;
            }
            let open = || {
                SessionIndex::open(path)
                    .map_err(|e| format!("无法打开会话正文索引 {}: {e}", path.display()))
            };
            let index = open().or_else(|first_err| {
                log::warn!("会话正文索引无法打开，尝试清理重建: {first_err}");
                remove_index_files(path).map_err(|e| e.to_string())?;
                open()
            })?;
            *guard = Some(index);
        }

        let index = guard.as_mut().expect("session index initialized above");
        let generation = Arc::clone(&index.generation);
        let result = f(index);
        drop(guard);
        drop(shared);
        drop(exclusive);

        if result.as_ref().err().is_some_and(is_corruption) {
            self.discard_corrupt_index(path, Some(generation));
        }

        result.map_err(|err| format!("会话正文索引错误: {err}"))
    }

    fn clear(&self, path: &Path) -> Result<(), String> {
        self.clear_generation.fetch_add(1, Ordering::SeqCst);
        cancel_all_active_searches();
        let _exclusive = write_rwlock(&self.lifetime);
        let mut guard = lock_mutex(&self.slot);
        *guard = None;
        let remove_res = remove_index_files(path);
        *lock_mutex(&self.status) = IndexStatus::default();
        remove_res.map_err(|e| format!("清理会话正文索引文件失败: {e}"))
    }

    fn remove_sources(&self, path: &Path, sources: &[(&str, &str)]) -> Result<(), String> {
        if sources.is_empty() {
            return Ok(());
        }
        {
            let _shared = read_rwlock(&self.lifetime);
            let guard = lock_mutex(&self.slot);
            if guard.is_none() && !path.exists() {
                return Ok(());
            }
        }
        self.with_index(path, None, |index| index.remove_sources_batch(sources))
    }

    fn search(
        &self,
        path: &Path,
        query: &str,
        provider_id: Option<&str>,
        limit: Option<usize>,
        search_id: Option<&str>,
    ) -> Result<ContentSearchResult, String> {
        let token_guard = SearchTokenGuard::register(search_id);
        if token_guard.is_cancelled() {
            return Err(SEARCH_CANCELLED.to_string());
        }

        let mut lifetime = read_rwlock(&self.lifetime);
        let status = self.current_status();
        let effective_limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        if split_query_terms(query).is_empty() || effective_limit == 0 {
            return Ok(ContentSearchResult {
                hits: Vec::new(),
                status,
            });
        }

        let needs_init = match self.slot.try_lock() {
            Ok(guard) => guard.is_none(),
            Err(std::sync::TryLockError::Poisoned(err)) => err.into_inner().is_none(),
            Err(std::sync::TryLockError::WouldBlock) => false,
        };
        if needs_init || !path.exists() {
            drop(lifetime);
            self.with_index(path, None, |_| Ok(()))?;
            lifetime = read_rwlock(&self.lifetime);
        }

        if token_guard.is_cancelled() {
            return Err(SEARCH_CANCELLED.to_string());
        }

        let search_res: Result<Vec<ContentHit>, SearchExecError> =
            match SessionIndex::open_reader(path) {
                Ok(reader) => reader.search_cancellable(
                    query,
                    provider_id,
                    effective_limit,
                    &token_guard.token,
                ),
                Err(err) => Err(SearchExecError::Sql(err)),
            };

        match search_res {
            Ok(hits) => {
                if token_guard.is_cancelled() {
                    return Err(SEARCH_CANCELLED.to_string());
                }
                Ok(ContentSearchResult { hits, status })
            }
            Err(SearchExecError::Cancelled) => Err(SEARCH_CANCELLED.to_string()),
            Err(SearchExecError::Sql(sql_err)) => {
                if is_corruption(&sql_err) {
                    let generation = lock_mutex(&self.slot)
                        .as_ref()
                        .map(|index| Arc::clone(&index.generation));
                    drop(lifetime);
                    self.discard_corrupt_index(path, generation);
                }
                let message = format!("会话正文索引查询失败: {sql_err}");
                log::warn!("{message}");
                Err(message)
            }
        }
    }

    fn sync_with<S, L, F>(
        &self,
        path: &Path,
        scan: S,
        mut load_session: L,
        mut emit_status: F,
    ) -> Result<IndexStatus, String>
    where
        S: FnOnce() -> Vec<SessionMeta>,
        L: FnMut(&str, &str, Option<&str>) -> Result<SessionIndexAction, String>,
        F: FnMut(IndexStatus),
    {
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(self.current_status());
        }

        let sync_gen = self.clear_generation.load(Ordering::SeqCst);
        let run_guard = SyncRunGuard {
            runtime: self,
            sync_gen,
            finished: false,
        };
        *lock_mutex(&self.status) = IndexStatus {
            running: true,
            processed: 0,
            total: 0,
        };

        let scanned = scan();
        if self.clear_generation.load(Ordering::SeqCst) != sync_gen {
            return Ok(self.current_status());
        }

        let mut seen = HashSet::new();
        let mut sources = Vec::new();
        for session in scanned {
            if session.cleanup_pending {
                continue;
            }
            let Some(source_path) = session.source_path.filter(|p| !p.is_empty()) else {
                continue;
            };
            let key = (session.provider_id, source_path);
            if seen.insert(key.clone()) {
                sources.push(key);
            }
        }

        let total = sources.len();
        let mut status = IndexStatus {
            running: true,
            processed: 0,
            total,
        };
        if !self.update_status_if_current(sync_gen, status, &mut emit_status) {
            return Ok(self.current_status());
        }

        let init_state = self.with_index(path, Some(sync_gen), |index| {
            let mut stored = index.all_revisions()?;
            let stale: Vec<(String, String)> = stored
                .keys()
                .filter(|key| !seen.contains(*key))
                .cloned()
                .collect();
            let removed_count = stale.len();
            if !stale.is_empty() {
                let refs: Vec<(&str, &str)> = stale
                    .iter()
                    .map(|(p, s)| (p.as_str(), s.as_str()))
                    .collect();
                index.remove_sources_batch(&refs)?;
                for key in &stale {
                    stored.remove(key);
                }
            }
            Ok((stored, removed_count))
        });

        let (mut stored_revisions, removed) = match init_state {
            Ok(pair) => pair,
            Err(err) if err == SYNC_ABORTED_BY_CLEAR => return Ok(self.current_status()),
            Err(err) => return Err(err),
        };
        let initially_empty = stored_revisions.is_empty() && removed == 0;

        let mut changed = 0usize;
        for (provider_id, source_path) in &sources {
            if self.clear_generation.load(Ordering::SeqCst) != sync_gen {
                return Ok(self.current_status());
            }
            let previous_rev = stored_revisions
                .get(&(provider_id.clone(), source_path.clone()))
                .map(String::as_str);

            match load_session(provider_id, source_path, previous_rev) {
                Ok(SessionIndexAction::Unchanged) => {}
                Ok(SessionIndexAction::Updated { revision, messages }) => {
                    let rows = message_rows(&messages.messages);
                    match self.with_index(path, Some(sync_gen), |index| {
                        index.replace_source(provider_id, source_path, &revision, &rows)
                    }) {
                        Ok(()) => {
                            changed += 1;
                            stored_revisions
                                .insert((provider_id.clone(), source_path.clone()), revision);
                        }
                        Err(err) if err == SYNC_ABORTED_BY_CLEAR => {
                            return Ok(self.current_status());
                        }
                        Err(err) => {
                            log::warn!("写入会话正文索引失败 ({provider_id}:{source_path}): {err}");
                        }
                    }
                }
                Err(err) => {
                    log::warn!("读取会话正文索引源失败 ({provider_id}:{source_path}): {err}");
                }
            }

            status.processed += 1;
            if status.processed % PROGRESS_EVERY == 0
                && status.processed < total
                && !self.update_status_if_current(sync_gen, status, &mut emit_status)
            {
                return Ok(self.current_status());
            }
        }

        if self.clear_generation.load(Ordering::SeqCst) != sync_gen {
            return Ok(self.current_status());
        }

        if changed > 0 || removed > 0 {
            let tidy = if initially_empty && changed >= COMPACT_AFTER_CHANGES {
                self.with_index(path, Some(sync_gen), |index| index.compact())
            } else if changed >= COMPACT_AFTER_CHANGES {
                self.with_index(path, Some(sync_gen), |index| {
                    index.optimize_and_checkpoint()
                })
            } else {
                self.with_index(path, Some(sync_gen), |index| index.checkpoint())
            };
            if let Err(err) = tidy {
                if err == SYNC_ABORTED_BY_CLEAR {
                    return Ok(self.current_status());
                }
                log::warn!("整理会话正文索引失败: {err}");
            }
        }

        status.running = false;
        Ok(run_guard.finish(status, &mut emit_status))
    }
}

static GLOBAL_RUNTIME: LazyLock<IndexRuntime> = LazyLock::new(IndexRuntime::new);

// ── 面向 Tauri 命令层与业务层的公开 API ──────────────────────────────

/// 获取当前全局索引同步状态。
#[allow(dead_code)]
pub fn current_status() -> IndexStatus {
    GLOBAL_RUNTIME.current_status()
}

/// 增量同步全局会话正文索引；若已有同步正在执行，则立即返回当前运行中状态而不重复启动。
pub fn sync_index<F>(emit_status: F) -> Result<IndexStatus, String>
where
    F: FnMut(IndexStatus),
{
    let path = index_path()?;
    let mut revision_cache: HashMap<TranscriptSources, Result<String, String>> = HashMap::new();

    GLOBAL_RUNTIME.sync_with(
        &path,
        super::scan_sessions,
        |provider_id, source_path, stored_revision| {
            if let Some(sources) = super::index_sources(provider_id, source_path) {
                let revision = revision_cache
                    .entry(sources)
                    .or_insert_with_key(revision_for_sources)
                    .clone()?;
                if stored_revision.is_some_and(|stored| revision_matches(stored, &revision)) {
                    return Ok(SessionIndexAction::Unchanged);
                }
                let messages = super::load_transcript(provider_id, source_path)?;
                return Ok(SessionIndexAction::Updated { revision, messages });
            }

            let messages = super::load_transcript(provider_id, source_path)?;
            let revision = revision_from_messages(&messages.messages);
            if stored_revision == Some(revision.as_str()) {
                return Ok(SessionIndexAction::Unchanged);
            }
            Ok(SessionIndexAction::Updated { revision, messages })
        },
        emit_status,
    )
}

/// 在已建立的会话正文索引中检索关键词（支持多关键词空白分隔 `AND`、1～2 字短词、提供商过滤与取消）。
pub fn search_content(
    query: &str,
    provider_id: Option<&str>,
    limit: Option<usize>,
    search_id: Option<&str>,
) -> Result<ContentSearchResult, String> {
    let path = index_path()?;
    GLOBAL_RUNTIME.search(&path, query, provider_id, limit, search_id)
}

/// 关闭索引连接并彻底删除本地 `session-index.db`、`-wal`、`-shm` 文件，重置状态为默认值。
pub fn clear_index() -> Result<(), String> {
    let path = index_path()?;
    GLOBAL_RUNTIME.clear(&path)
}

/// 会话删除后按 `(provider_id, source_path)` 增量移除对应索引行；若本地尚未创建索引文件则不触碰磁盘。
pub fn remove_indexed_sources(sources: &[(&str, &str)]) -> Result<(), String> {
    if sources.is_empty() {
        return Ok(());
    }
    let path = index_path()?;
    GLOBAL_RUNTIME.remove_sources(&path, sources)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn msg(role: &str, content: &str) -> SessionMessage {
        SessionMessage {
            role: role.to_string(),
            content: content.to_string(),
            ..SessionMessage::default()
        }
    }

    fn meta(
        provider_id: &str,
        session_id: &str,
        source_path: Option<&str>,
        cleanup_pending: bool,
    ) -> SessionMeta {
        SessionMeta {
            provider_id: provider_id.to_string(),
            session_id: session_id.to_string(),
            residual: false,
            parent_session_id: None,
            archived: false,
            cleanup_pending,
            title: None,
            summary: None,
            project_dir: None,
            project_name: None,
            sidebar_section: None,
            created_at: None,
            last_active_at: None,
            source_path: source_path.map(str::to_string),
            resume_command: None,
        }
    }

    fn rows(texts: &[&str]) -> Vec<(usize, String)> {
        texts
            .iter()
            .enumerate()
            .map(|(idx, text)| (idx, (*text).to_string()))
            .collect()
    }

    fn slice_utf16(text: &str, range: &ContentHighlight) -> String {
        let units: Vec<u16> = text.encode_utf16().collect();
        String::from_utf16(&units[range.start..range.end]).expect("valid UTF-16 slice")
    }

    #[test]
    fn message_rows_filters_roles_injected_prompts_and_preserves_original_indices() {
        let messages = vec![
            msg("user", "   # AGENTS.md instructions\ndo not index this"),
            msg(
                "user",
                "\n  <environment_context>\ncwd: /repo\n</environment_context>",
            ),
            msg(
                "user",
                "<permissions instructions>sandbox</permissions instructions>",
            ),
            msg(
                "user",
                "<user_instructions>custom rules</user_instructions>",
            ),
            msg("user", "  <system-reminder>internal note</system-reminder>"),
            msg("user", "<INSTRUCTIONS>bootstrap</INSTRUCTIONS>"),
            msg("user", "帮我排查 SQLite WAL 锁冲突"),
            msg("thinking", "内部推理过程不应被索引"),
            msg("tool", "cargo test stdout output"),
            msg("assistant", "可以，先检查 busy_timeout 与连接生命周期。"),
            msg("system", "system prompt"),
            msg("user", "   \n\t  "),
            // assistant 正文即使引用了规则标题也属于正常回答，不应按 user 注入过滤
            msg(
                "assistant",
                "# AGENTS.md instructions 是 Codex 注入的头部说明",
            ),
        ];

        assert_eq!(
            message_rows(&messages),
            vec![
                (6, "帮我排查 SQLite WAL 锁冲突".to_string()),
                (9, "可以，先检查 busy_timeout 与连接生命周期。".to_string()),
                (
                    12,
                    "# AGENTS.md instructions 是 Codex 注入的头部说明".to_string()
                ),
            ]
        );
    }

    #[test]
    fn message_rows_truncates_at_max_row_chars_on_char_boundary() {
        let oversized = format!("{}{}", "🚀中".repeat(MAX_ROW_CHARS / 2), "尾部超出内容");
        let extracted = message_rows(&[msg("assistant", &oversized)]);
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].0, 0);
        assert_eq!(extracted[0].1.chars().count(), MAX_ROW_CHARS);
        assert!(!extracted[0].1.contains("尾部超出内容"));
    }

    #[test]
    fn trigram_and_short_term_queries_match_cjk_ascii_and_unicode_case_folding() {
        let mut index = SessionIndex::open_in_memory().unwrap();
        index
            .replace_source(
                "claude",
                "/sessions/a.jsonl",
                "rev-1",
                &rows(&[
                    "开场白",
                    "这是一个前端UI组件，直连db存储且进度100%",
                    "Ordered an Éclair with Привет and ΟΣ and İabc",
                ]),
            )
            .unwrap();
        index
            .replace_source(
                "codex",
                "/sessions/b.jsonl",
                "rev-1",
                &rows(&["讨论跑马灯动画帧率与 _underscore 符号"]),
            )
            .unwrap();

        // >= 3 字符：走 trigram
        let hits = index.search("跑马灯", None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].provider_id, "codex");
        assert_eq!(hits[0].snippets[0].message_index, 0);

        // 1 字与 2 字中日韩、无空格粘连的 ASCII 短词、符号：走 short_terms
        for short_query in ["端", "前端", "UI", "ui", "db", "DB", "0%", "%"] {
            let hits = index.search(short_query, None, 10).unwrap();
            assert_eq!(hits.len(), 1, "query={short_query}");
            assert_eq!(
                hits[0].source_path, "/sessions/a.jsonl",
                "query={short_query}"
            );
            assert_eq!(hits[0].snippets[0].message_index, 1, "query={short_query}");
        }

        // 单字符 `_` 仅存在于 b.jsonl，不应像 SQL LIKE 通配符那样误匹配全部行
        let underscore_hits = index.search("_", None, 10).unwrap();
        assert_eq!(underscore_hits.len(), 1);
        assert_eq!(underscore_hits[0].source_path, "/sessions/b.jsonl");

        // 非 ASCII 大小写折叠一致性（trigram 与 short_terms 同步）
        for q in [
            "éclair",
            "ÉCLAIR",
            "éc",
            "ПРИВЕТ",
            "пр",
            "ος",
            "i\u{307}a",
            "i",
        ] {
            let hits = index.search(q, None, 10).unwrap();
            assert_eq!(hits.len(), 1, "unicode query={q}");
            assert_eq!(hits[0].snippets[0].message_index, 2, "unicode query={q}");
        }
        assert!(index.search("ia", None, 10).unwrap().is_empty());
        assert!(index.search("AND OR NOT", None, 10).unwrap().is_empty());
    }

    #[test]
    fn multi_keyword_search_and_utf16_surrogate_highlights_are_exact() {
        let mut index = SessionIndex::open_in_memory().unwrap();
        index
            .replace_source(
                "codex",
                "/sessions/deploy.jsonl",
                "rev-1",
                &[
                    (0, "Only rust mentioned here".to_string()),
                    (3, "🚀 deploy ok".to_string()),
                    (
                        7,
                        "Prefix 😀𐐀 before Rust and SQLite engine — 🚀 deploy is ok!".to_string(),
                    ),
                ],
            )
            .unwrap();

        // 多长词 AND 组合（顺序无关）
        let hits = index.search("sqlite rust", None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].match_count, 1);
        let snippet = &hits[0].snippets[0];
        assert_eq!(snippet.message_index, 7);
        let highlighted_words: Vec<String> = snippet
            .highlights
            .iter()
            .map(|range| slice_utf16(&snippet.text, range))
            .collect();
        assert_eq!(highlighted_words, vec!["Rust", "SQLite"]);

        // 混合 1 字符 Emoji（代理对，UTF-16 长度 2）+ 长词 + 2 字符短词
        let rocket_hits = index.search("🚀 deploy ok", None, 10).unwrap();
        assert_eq!(rocket_hits.len(), 1);
        assert_eq!(rocket_hits[0].match_count, 2);

        let exact_msg = rocket_hits[0]
            .snippets
            .iter()
            .find(|s| s.message_index == 3)
            .expect("message_index 3 snippet");
        assert_eq!(exact_msg.text, "🚀 deploy ok");
        assert_eq!(
            exact_msg.highlights,
            vec![
                ContentHighlight { start: 0, end: 2 },
                ContentHighlight { start: 3, end: 9 },
                ContentHighlight { start: 10, end: 12 },
            ]
        );
        assert_eq!(slice_utf16(&exact_msg.text, &exact_msg.highlights[0]), "🚀");
        assert_eq!(
            slice_utf16(&exact_msg.text, &exact_msg.highlights[1]),
            "deploy"
        );
        assert_eq!(slice_utf16(&exact_msg.text, &exact_msg.highlights[2]), "ok");

        let supplementary_msg = rocket_hits[0]
            .snippets
            .iter()
            .find(|s| s.message_index == 7)
            .expect("message_index 7 snippet");
        let extracted: Vec<String> = supplementary_msg
            .highlights
            .iter()
            .map(|r| slice_utf16(&supplementary_msg.text, r))
            .collect();
        assert_eq!(extracted, vec!["🚀", "deploy", "ok"]);

        // 重叠查询词必须合并为非重叠、严格递增区间
        let (_, merged) = make_highlighted_snippet("😀 rustc compiler", "rust rustc");
        assert_eq!(merged, vec![ContentHighlight { start: 3, end: 8 }]);
    }

    #[test]
    fn provider_filter_and_snippet_cap_work_accurately() {
        let mut index = SessionIndex::open_in_memory().unwrap();
        let many: Vec<(usize, String)> = (0..6)
            .map(|i| (i * 2, format!("target keyword step {i}")))
            .collect();
        index
            .replace_source("codex", "/codex.jsonl", "r1", &many)
            .unwrap();
        index
            .replace_source("claude", "/claude.jsonl", "r1", &rows(&["target keyword"]))
            .unwrap();

        let all_hits = index.search("target keyword", None, 10).unwrap();
        assert_eq!(all_hits.len(), 2);
        let codex_hit = all_hits.iter().find(|h| h.provider_id == "codex").unwrap();
        assert_eq!(codex_hit.match_count, 6);
        assert_eq!(codex_hit.snippets.len(), MAX_SNIPPETS_PER_SESSION);

        let claude_only = index.search("target keyword", Some("claude"), 10).unwrap();
        assert_eq!(claude_only.len(), 1);
        assert_eq!(claude_only[0].provider_id, "claude");

        assert!(index
            .search("target keyword", Some("deepseek"), 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn incremental_sync_skips_unchanged_updates_changed_and_purges_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join(INDEX_FILE_NAME);
        let file_a = temp.path().join("a.jsonl");
        let file_b = temp.path().join("b.jsonl");
        fs::write(&file_a, "v1").unwrap();
        fs::write(&file_b, "v1").unwrap();

        let path_a = file_a.to_string_lossy().into_owned();
        let path_b = file_b.to_string_lossy().into_owned();
        let runtime = IndexRuntime::new();

        let loads = Cell::new(0usize);
        let mut emitted = Vec::new();

        // 第一轮同步：收录 a 与 b，跳过 cleanup_pending 与无路径会话
        let status_1 = runtime
            .sync_with(
                &db_path,
                || {
                    vec![
                        meta("claude", "sa", Some(&path_a), false),
                        meta("codex", "sb", Some(&path_b), false),
                        meta("codex", "pending", Some("/tmp/pending.jsonl"), true),
                        meta("pi", "no-path", None, false),
                    ]
                },
                |provider_id, source_path, stored_rev| {
                    let srcs = vec![(PathBuf::from(source_path), None)];
                    let rev = revision_for_sources(&srcs)?;
                    if stored_rev.is_some_and(|prev| revision_matches(prev, &rev)) {
                        return Ok(SessionIndexAction::Unchanged);
                    }
                    loads.set(loads.get() + 1);
                    let text = if provider_id == "claude" {
                        "alpha initial body"
                    } else {
                        "beta initial body"
                    };
                    Ok(SessionIndexAction::Updated {
                        revision: rev,
                        messages: Arc::new(Transcript::new(vec![msg("user", text)])),
                    })
                },
                |st| emitted.push(st),
            )
            .unwrap();

        assert_eq!(
            status_1,
            IndexStatus {
                running: false,
                processed: 2,
                total: 2,
            }
        );
        assert_eq!(loads.get(), 2);
        assert!(emitted.first().unwrap().running);
        assert!(!emitted.last().unwrap().running);
        assert_eq!(
            runtime
                .search(&db_path, "initial", None, None, None)
                .unwrap()
                .hits
                .len(),
            2
        );

        // 第二轮同步：文件未改写，必须零次调用正文加载
        loads.set(0);
        runtime
            .sync_with(
                &db_path,
                || {
                    vec![
                        meta("claude", "sa", Some(&path_a), false),
                        meta("codex", "sb", Some(&path_b), false),
                    ]
                },
                |_, source_path, stored_rev| {
                    let rev = revision_for_sources(&vec![(PathBuf::from(source_path), None)])?;
                    if stored_rev.is_some_and(|prev| revision_matches(prev, &rev)) {
                        return Ok(SessionIndexAction::Unchanged);
                    }
                    loads.set(loads.get() + 1);
                    unreachable!("unchanged source must not reload transcript");
                },
                |_| {},
            )
            .unwrap();
        assert_eq!(loads.get(), 0);

        // 第三轮同步：仅修改 a.jsonl，同时移除 b.jsonl（模拟会话已删除）
        fs::write(&file_a, "v2-updated-length").unwrap();
        runtime
            .sync_with(
                &db_path,
                || vec![meta("claude", "sa", Some(&path_a), false)],
                |_, source_path, stored_rev| {
                    let rev = revision_for_sources(&vec![(PathBuf::from(source_path), None)])?;
                    if stored_rev.is_some_and(|prev| revision_matches(prev, &rev)) {
                        return Ok(SessionIndexAction::Unchanged);
                    }
                    loads.set(loads.get() + 1);
                    Ok(SessionIndexAction::Updated {
                        revision: rev,
                        messages: Arc::new(Transcript::new(vec![msg(
                            "assistant",
                            "alpha refreshed content",
                        )])),
                    })
                },
                |_| {},
            )
            .unwrap();
        assert_eq!(loads.get(), 1);
        assert!(runtime
            .search(&db_path, "initial", None, None, None)
            .unwrap()
            .hits
            .is_empty());
        let refreshed = runtime
            .search(&db_path, "refreshed", None, None, None)
            .unwrap();
        assert_eq!(refreshed.hits.len(), 1);
        assert_eq!(refreshed.hits[0].source_path, path_a);
    }

    #[test]
    fn revision_matches_handles_aged_digest_drop_and_detects_wal_same_len_rewrite() {
        let mtime = Some(UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000));
        let sources = vec![(PathBuf::from("/sessions/a.jsonl"), None)];
        let fresh = Fingerprint {
            members: vec![(
                PathBuf::from("/sessions/a.jsonl"),
                Some(MemberFingerprint {
                    modified: mtime,
                    len: 128,
                    digest: Some(0x1234_5678_9abc_def0),
                }),
            )],
        };
        let aged = Fingerprint {
            members: vec![(
                PathBuf::from("/sessions/a.jsonl"),
                Some(MemberFingerprint {
                    modified: mtime,
                    len: 128,
                    digest: None,
                }),
            )],
        };
        let rev_fresh = format_sources_fingerprint(&sources, &fresh);
        let rev_aged = format_sources_fingerprint(&sources, &aged);
        assert!(
            revision_matches(&rev_fresh, &rev_aged),
            "2 秒窗口过后摘要退化为 None 不应触发无效重扫"
        );

        // SQLite WAL 同长度改写必须改变修订串且不相等
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("opencode.db");
        let wal = temp.path().join("opencode.db-wal");
        fs::write(&db, "sqlite-main").unwrap();
        fs::write(&wal, "wal-A").unwrap();
        let sqlite_srcs = cache::sqlite_transcript_sources(&db);
        let rev_1 = revision_for_sources(&sqlite_srcs).unwrap();
        fs::write(&wal, "wal-B").unwrap();
        let rev_2 = revision_for_sources(&sqlite_srcs).unwrap();
        assert!(!revision_matches(&rev_1, &rev_2));
    }

    #[test]
    fn search_cancellation_and_clear_index_and_remove_sources_work_cleanly() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join(INDEX_FILE_NAME);
        let runtime = IndexRuntime::new();

        // 索引文件不存在时 remove_sources 不得在磁盘上创建空库
        runtime
            .remove_sources(&db_path, &[("claude", "/missing.jsonl")])
            .unwrap();
        assert!(!db_path.exists());

        runtime
            .with_index(&db_path, None, |index| {
                index.replace_source("claude", "/s1.jsonl", "r1", &rows(&["keep searchable"]))?;
                index.replace_source("codex", "/s2.jsonl", "r1", &rows(&["remove searchable"]))
            })
            .unwrap();
        assert!(db_path.exists());

        // 提前取消（early cancellation）：先 cancel_search 再发起同 ID 搜索必须立即返回取消错误
        cancel_search("search-req-early");
        let cancelled_err = runtime
            .search(&db_path, "searchable", None, None, Some("search-req-early"))
            .expect_err("pre-cancelled search must abort");
        assert_eq!(cancelled_err, SEARCH_CANCELLED);

        // 同 ID 后续的新请求不应被已消费的取消状态误伤
        let ok_res = runtime
            .search(&db_path, "searchable", None, None, Some("search-req-early"))
            .unwrap();
        assert_eq!(ok_res.hits.len(), 2);

        // 按源精确删除只移除被删会话，保留其余索引
        runtime
            .remove_sources(&db_path, &[("codex", "/s2.jsonl")])
            .unwrap();
        let remaining = runtime
            .search(&db_path, "searchable", None, None, None)
            .unwrap();
        assert_eq!(remaining.hits.len(), 1);
        assert_eq!(remaining.hits[0].source_path, "/s1.jsonl");

        // WAL 模式下 SQLite 已生成真实边车文件；验证 clear 先释放连接（解除 Windows -shm 内存映射）再彻底删除 db / -wal / -shm
        assert!(temp.path().join(format!("{INDEX_FILE_NAME}-wal")).exists());
        assert!(temp.path().join(format!("{INDEX_FILE_NAME}-shm")).exists());
        runtime.clear(&db_path).unwrap();
        assert!(!db_path.exists());
        assert!(!temp.path().join(format!("{INDEX_FILE_NAME}-wal")).exists());
        assert!(!temp.path().join(format!("{INDEX_FILE_NAME}-shm")).exists());
        assert_eq!(runtime.current_status(), IndexStatus::default());

        // 即使磁盘上残留孤立的 -wal / -shm 边车文件，clear 同样会全部清理
        for suffix in ["", "-wal", "-shm"] {
            fs::write(
                temp.path().join(format!("{INDEX_FILE_NAME}{suffix}")),
                b"orphan-sidecar",
            )
            .unwrap();
        }
        runtime.clear(&db_path).unwrap();
        assert!(!db_path.exists());
        assert!(!temp.path().join(format!("{INDEX_FILE_NAME}-wal")).exists());
        assert!(!temp.path().join(format!("{INDEX_FILE_NAME}-shm")).exists());
    }

    #[test]
    fn in_flight_cancellation_and_corruption_recovery_and_wal_concurrency_work() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join(INDEX_FILE_NAME);
        let runtime = Arc::new(IndexRuntime::new());

        let bulk_rows: Vec<(usize, String)> = (0..500)
            .map(|i| (i, format!("searchable bulk row {i} with 🚀 deploy")))
            .collect();
        runtime
            .with_index(&db_path, None, |index| {
                index.replace_source("codex", "/bulk.jsonl", "r1", &bulk_rows)
            })
            .unwrap();

        // 验证行级轮询取消：注册活跃令牌后由 cancel_search 触发取消
        {
            let guard = SearchTokenGuard::register(Some("in-flight-id"));
            assert!(!guard.is_cancelled());
            cancel_search("in-flight-id");
            assert!(guard.is_cancelled());
            let reader = SessionIndex::open_reader(&db_path).unwrap();
            assert!(matches!(
                reader.search_cancellable("searchable", None, 50, &guard.token),
                Err(SearchExecError::Cancelled)
            ));
        }

        // WAL 读写并发：写连接持有未提交事务期间，独立只读连接仍可正常检索已提交快照
        {
            let mut slot_guard = lock_mutex(&runtime.slot);
            let writer = slot_guard.as_mut().unwrap();
            let tx = writer.conn.transaction().unwrap();
            tx.execute("UPDATE sources SET source_path = '/uncommitted.jsonl'", [])
                .unwrap();

            let rt_reader = Arc::clone(&runtime);
            let db_reader = db_path.clone();
            let reader_hits = std::thread::spawn(move || {
                rt_reader
                    .search(&db_reader, "deploy", None, Some(10), None)
                    .unwrap()
                    .hits
            })
            .join()
            .unwrap();
            assert_eq!(reader_hits.len(), 1);
            assert_eq!(reader_hits[0].source_path, "/bulk.jsonl");
            tx.rollback().unwrap();
        }

        // 模拟 FTS 段损坏：查询报错的同时自动释放连接并清除损坏的索引与边车，后续可自愈重建
        runtime
            .with_index(&db_path, None, |index| {
                index.conn.execute(
                    "UPDATE messages_fts_data SET block = x'00' WHERE id > 10",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(runtime
            .search(&db_path, "searchable", None, None, None)
            .is_err());
        assert!(!db_path.exists(), "损坏的索引文件应在释放锁后自动移除");

        runtime
            .with_index(&db_path, None, |index| {
                index.replace_source("codex", "/rebuilt.jsonl", "r2", &rows(&["rebuilt needle"]))
            })
            .unwrap();
        let rebuilt = runtime
            .search(&db_path, "rebuilt", None, None, None)
            .unwrap();
        assert_eq!(rebuilt.hits.len(), 1);
        assert_eq!(rebuilt.hits[0].source_path, "/rebuilt.jsonl");
    }

    #[test]
    fn concurrent_sync_guard_and_version_mismatch_rebuild_work() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join(INDEX_FILE_NAME);
        let runtime = Arc::new(IndexRuntime::new());

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let rt_bg = Arc::clone(&runtime);
        let db_bg = db_path.clone();
        let handle = std::thread::spawn(move || {
            rt_bg
                .sync_with(
                    &db_bg,
                    || {
                        entered_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        vec![meta("claude", "s1", Some("/s1.jsonl"), false)]
                    },
                    |_, _, _| {
                        Ok(SessionIndexAction::Updated {
                            revision: "r1".to_string(),
                            messages: Arc::new(Transcript::new(vec![msg("user", "synced text")])),
                        })
                    },
                    |_| {},
                )
                .unwrap()
        });

        entered_rx.recv().unwrap();
        // 同步已在运行时再次调用，立即返回 running 状态而不阻塞或重复启动
        let concurrent_status = runtime
            .sync_with(
                &db_path,
                || panic!("duplicate scan must not run"),
                |_, _, _| panic!("duplicate load must not run"),
                |_| {},
            )
            .unwrap();
        assert!(concurrent_status.running);

        release_tx.send(()).unwrap();
        let finished = handle.join().unwrap();
        assert!(!finished.running);
        assert_eq!(finished.processed, 1);

        // 修改 user_version 模拟版本不匹配，重新打开时清理旧库与边车并重建
        drop(runtime);
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.pragma_update(None, "user_version", INDEX_VERSION + 99)
                .unwrap();
        }
        let reopened = SessionIndex::open(&db_path).unwrap();
        assert!(reopened.search("synced", None, 10).unwrap().is_empty());
    }
}
