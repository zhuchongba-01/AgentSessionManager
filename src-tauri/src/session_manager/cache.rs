//! 会话正文解析结果缓存 `TranscriptCache`。
//!
//! 打开同一个会话（来回切换、清理后重新打开）不必重新读盘解析：命中条件是正文
//! 实际依赖的文件的 `(mtime, len)` 全部未变。指纹少算一个文件就会返回过期正文，
//! 因此宁可多算——见 [`transcript_fingerprint`]。

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use super::model::{EventKind, SessionBlock, ToolStatus, TurnIndex};
use super::SessionMessage;

/// 最多缓存的会话数
pub const MAX_ENTRIES: usize = 8;
/// 缓存内所有会话正文的合计字节上限（按内容长度估算，见 [`Transcript::new`]）
pub const MAX_TOTAL_BYTES: usize = 96 * 1024 * 1024;
const QUESTION_PREVIEW_CHARS: usize = 80;

/// 单个源的身份。
///
/// `(mtime, len)` 在多数情况下够用，但对 SQLite 的 `-wal` 不够：WAL 是就地追加与
/// 回填的，同一次写入可以保持长度不变，而且文件系统的 mtime 粒度（Windows 上约
/// 10ms、部分 Linux 上是 1s）会让「改过」和「没改过」得到同一个时间戳。这种源再
/// 补一个内容摘要：WAL 通常只有几 KB～几 MB，读一遍远比重放一遍查询便宜。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberFingerprint {
    pub modified: Option<SystemTime>,
    pub len: u64,
    /// `-wal` 源，或近期修改/缺少 mtime 且有效长度不超过 [`MAX_DIGESTED_SOURCE_BYTES`] 时的内容摘要
    pub digest: Option<u64>,
}

/// 参与内容摘要的单成员体积上限。超过这个大小的源只比 `(mtime, len)`：
/// 每次打开会话都完整读一遍大文件，代价会盖过缓存带来的收益。
pub const MAX_DIGESTED_SOURCE_BYTES: u64 = 16 * 1024 * 1024;

fn is_wal_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with("-wal"))
}

fn should_digest_member(path: &Path, metadata: &fs::Metadata) -> bool {
    if is_wal_path(path) {
        return true;
    }
    let Ok(modified) = metadata.modified() else {
        return true;
    };
    let Ok(age) = SystemTime::now().duration_since(modified) else {
        return true;
    };
    age < Duration::from_secs(2)
}

impl MemberFingerprint {
    fn of_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
            digest: None,
        }
    }

    fn of_source(path: &Path, end_byte_offset: Option<u64>) -> io::Result<Self> {
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Transcript source is not a regular file",
            ));
        }
        let mut fingerprint = Self::of_metadata(&metadata);
        let effective_len =
            end_byte_offset.map_or(metadata.len(), |limit| metadata.len().min(limit));
        if effective_len <= MAX_DIGESTED_SOURCE_BYTES && should_digest_member(path, &metadata) {
            fingerprint.digest = Some(digest_file(
                path,
                Some(effective_len.min(MAX_DIGESTED_SOURCE_BYTES)),
            )?);
        }
        Ok(fingerprint)
    }

    pub fn matches(&self, incoming: &Self) -> bool {
        if self.modified != incoming.modified || self.len != incoming.len {
            return false;
        }
        match (self.digest, incoming.digest) {
            (Some(a), Some(b)) => a == b,
            (Some(_), None) | (None, Some(_)) => self.modified.is_some(),
            (None, None) => true,
        }
    }
}

/// 文件内容的稳定摘要（FNV-1a）。只用于判断"是否变过"，不用于安全目的。
fn digest_file(path: &Path, limit: Option<u64>) -> io::Result<u64> {
    use std::io::Read;

    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let file = fs::File::open(path)?;
    let mut reader: Box<dyn Read> = match limit {
        Some(limit) => Box::new(file.take(limit)),
        None => Box::new(file),
    };
    let mut hash = OFFSET_BASIS;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        for byte in &buffer[..read] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }
    Ok(hash)
}

/// 正文实际依赖的每个成员的指纹，顺序与 `sources` 一致。
///
/// 这里刻意**逐成员保留** `(mtime, len)`，不做「取最新 mtime + 长度求和」之类的
/// 压缩：把多个成员合并成一个标量后，改动可以互相抵消——较早修改的成员被同长度
/// 改写、同时较晚的成员 mtime 不变时，合并值完全不变，于是命中旧正文。正文缓存
/// 命中错误内容是静默的（用户看到的是别的对话），所以宁可多比较几个字节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub members: Vec<(PathBuf, Option<MemberFingerprint>)>,
}

impl Fingerprint {
    pub fn matches(&self, incoming: &Self) -> bool {
        self.members.len() == incoming.members.len()
            && self
                .members
                .iter()
                .zip(&incoming.members)
                .all(|((path_a, fp_a), (path_b, fp_b))| {
                    path_a == path_b
                        && match (fp_a, fp_b) {
                            (Some(a), Some(b)) => a.matches(b),
                            (None, None) => true,
                            _ => false,
                        }
                })
    }
}

/// 正文源：有序的「文件 + 只读到该字节偏移」。`None` 表示读到文件末尾。
///
/// 顺序与偏移都是正文身份的一部分：Codex 的分页历史就是「当前文件 + 基座文件的
/// 前 N 字节」拼起来的，同一个文件在不同链里可能只算前一段。
pub type TranscriptSources = Vec<(PathBuf, Option<u64>)>;

/// 正文实际依赖的文件集合对应的指纹。
///
/// 调用方给出的文件必须覆盖解析器真正读取的全部内容：少算一个就会命中过期缓存。
/// 尚未落盘的可选成员（例如一轮进行中还不存在的 `chat_history.jsonl`）会被跳过，
/// 但至少要有一个成员可读，否则不给指纹，让调用方直接读盘。
///
/// 跳过的成员以 `None` 占位保留在结果里，这样「成员本轮缺失」也构成一种身份：
/// 缺失成员后来出现时指纹随之改变，不会命中上一轮的旧缓存。
pub fn transcript_fingerprint(sources: &[(PathBuf, Option<u64>)]) -> io::Result<Fingerprint> {
    let mut members = Vec::with_capacity(sources.len());
    let mut readable = 0;
    for (path, end_byte_offset) in sources {
        match MemberFingerprint::of_source(path, *end_byte_offset) {
            Ok(fingerprint) => {
                readable += 1;
                members.push((path.clone(), Some(fingerprint)));
            }
            Err(_) => members.push((path.clone(), None)),
        }
    }
    if readable == 0 {
        return Err(io::Error::new(io::ErrorKind::NotFound, "会话源不可读"));
    }
    Ok(Fingerprint { members })
}

/// 正文源的摘要：对「有序文件列表 + 每段截断偏移」做稳定哈希（FNV-1a）。
///
/// 指纹只看文件本身的 mtime/长度，无法表达「同一组文件换了顺序」或「同一个文件这次
/// 只算前 N 字节」。Codex 的分页历史链由磁盘上的 rollout 头决定，链可以在文件一字
/// 未改的情况下改变，所以链的身份必须单独进缓存键。
pub fn sources_digest(sources: &[(PathBuf, Option<u64>)]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    let mut mix = |byte: u8| {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    };
    for (path, end_byte_offset) in sources {
        for byte in path.to_string_lossy().as_bytes() {
            mix(*byte);
        }
        // 路径分隔符：避免相邻路径被哈希成同一个序列
        mix(0);
        for byte in end_byte_offset.unwrap_or(u64::MAX).to_le_bytes() {
            mix(byte);
        }
    }
    hash
}

/// SQLite 源的正文文件集合：库文件 + WAL 边车。
///
/// WAL 模式下新写入先落在 `-wal` 里，主库的 mtime 不变——只看主库会命中旧正文。
/// `-shm` 只是协调用的共享内存，不保存已提交内容，不必计入。
pub fn sqlite_transcript_sources(database: &Path) -> TranscriptSources {
    let wal = PathBuf::from(format!("{}-wal", database.to_string_lossy()));
    vec![(database.to_path_buf(), None), (wal, None)]
}

/// 缓存键：provider + 前端回传的原始 `source_path`（与读取命令的入参一致）+ 正文源摘要。
pub fn cache_key(
    provider_id: &str,
    source_path: &str,
    sources: &[(PathBuf, Option<u64>)],
) -> String {
    format!(
        "{provider_id}\u{1}{source_path}\u{1}{:016x}",
        sources_digest(sources)
    )
}

/// 一次完整解析的结果。
#[derive(Debug, Default)]
pub struct Transcript {
    pub messages: Vec<SessionMessage>,
    pub turns: Vec<TurnIndex>,
    /// 每条消息的估算字节数，流式分块按它切包，避免为了统计再序列化一遍
    pub message_bytes: Vec<usize>,
    /// 正文的估算字节数，只用于容量控制
    pub approx_bytes: usize,
    /// 本次解析结果是否进了缓存，因而「展开取全文」不必重新解析整个会话。
    pub cacheable: bool,
}

impl Transcript {
    pub fn new(messages: Vec<SessionMessage>) -> Self {
        let message_bytes = messages
            .iter()
            .map(serialized_message_len)
            .collect::<Vec<_>>();
        let approx_bytes = message_bytes.iter().sum();
        let turns = build_turns(&messages);
        Self {
            messages,
            turns,
            message_bytes,
            approx_bytes,
            cacheable: false,
        }
    }
}

fn serialized_message_len(message: &SessionMessage) -> usize {
    if message.blocks.is_empty() {
        return message.role.len() + message.content.len() + 32;
    }
    #[derive(Default)]
    struct ByteCounter(usize);
    impl io::Write for ByteCounter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, message)
        .map(|()| counter.0)
        .unwrap_or_else(|_| message.role.len() + message.content.len() + 32)
}

pub fn build_turns(messages: &[SessionMessage]) -> Vec<TurnIndex> {
    struct Building {
        index: TurnIndex,
        has_visible: bool,
        reply_pending: bool,
    }

    let mut turns: Vec<Building> = Vec::new();
    let mut generated = 0usize;
    let mut current_key: Option<String> = None;

    for (i, message) in messages.iter().enumerate() {
        let is_question = message.role == "user" && !message.injected && !message.is_empty();
        let key = match &message.turn_id {
            Some(turn_id) => turn_id.clone(),
            None if is_question => {
                generated += 1;
                format!("t{generated}")
            }
            None => current_key.clone().unwrap_or_else(|| "t0".to_string()),
        };

        if current_key.as_deref() != Some(key.as_str()) || turns.is_empty() {
            turns.push(Building {
                index: TurnIndex {
                    turn_id: key.clone(),
                    first_message_index: i,
                    last_message_index: i,
                    question_preview: String::new(),
                    ts: None,
                    step_count: 0,
                    error_count: 0,
                    has_final_reply: false,
                    aborted: false,
                },
                has_visible: false,
                reply_pending: false,
            });
            current_key = Some(key);
        }

        let Some(turn) = turns.last_mut() else {
            continue;
        };
        turn.index.last_message_index = i;
        if !message.injected {
            turn.has_visible = true;
        }
        if is_question && turn.index.question_preview.is_empty() {
            turn.index.question_preview = question_preview(message);
            turn.index.ts = message.ts;
        }

        for block in &message.blocks {
            match block {
                SessionBlock::ToolCall { .. } => {
                    turn.index.step_count += 1;
                    turn.reply_pending = false;
                }
                SessionBlock::ToolResult { status, .. } if *status == ToolStatus::Error => {
                    turn.index.error_count += 1;
                }
                SessionBlock::Text { text, .. }
                    if message.role == "assistant" && !text.trim().is_empty() =>
                {
                    turn.reply_pending = true;
                }
                SessionBlock::Event {
                    kind: EventKind::Aborted,
                    ..
                } => turn.index.aborted = true,
                _ => {}
            }
        }
        if message.blocks.is_empty() && message.role == "assistant" {
            let content = message.content.trim();
            if content.starts_with("[Tool:") {
                turn.reply_pending = false;
            } else if !content.is_empty() {
                turn.reply_pending = true;
            }
        }
        turn.index.has_final_reply = turn.reply_pending;
    }

    turns
        .into_iter()
        .filter(|turn| turn.has_visible)
        .map(|turn| turn.index)
        .collect()
}

fn question_preview(message: &SessionMessage) -> String {
    let text_blocks = message.blocks.iter().filter_map(|block| match block {
        SessionBlock::Text { text, .. } => Some(text.as_str()),
        _ => None,
    });
    let has_text = text_blocks.clone().next().is_some();
    let sources = text_blocks.chain((!has_text).then_some(message.content.as_str()));
    let mut out = String::with_capacity(QUESTION_PREVIEW_CHARS);
    let mut count = 0;
    let mut space = false;
    for ch in sources.flat_map(|text| text.chars().chain(std::iter::once(' '))) {
        if ch.is_whitespace() {
            space = count > 0;
            continue;
        }
        if space && count < QUESTION_PREVIEW_CHARS {
            out.push(' ');
            count += 1;
        }
        if count == QUESTION_PREVIEW_CHARS {
            out.pop();
            out.push('…');
            return out;
        }
        out.push(ch);
        count += 1;
        space = false;
    }
    out
}

/// 流式分块的粒度：单块最多 256 KB 或 150 条消息，取先到者。
///
/// 单条消息本身超过上限时独占一块：永远不从中间切开一条消息，否则前端要为跨块的
/// 消息做拼接，而消息是渲染的最小单位。
pub const CHUNK_MAX_BYTES: usize = 256 * 1024;
pub const CHUNK_MAX_MESSAGES: usize = 150;
/// 按每条消息的字节数切块，返回连续、非空、覆盖全量的 `[start, end)` 区间。
pub fn chunk_ranges(message_bytes: &[usize]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    for (index, size) in message_bytes.iter().enumerate() {
        let count = index - start;
        if count > 0 && (count >= CHUNK_MAX_MESSAGES || bytes + size > CHUNK_MAX_BYTES) {
            ranges.push((start, index));
            start = index;
            bytes = 0;
        }
        bytes += size;
    }
    if start < message_bytes.len() {
        ranges.push((start, message_bytes.len()));
    }
    ranges
}

struct Entry {
    key: String,
    fingerprint: Fingerprint,
    transcript: Arc<Transcript>,
}

#[derive(Default)]
struct CacheState {
    entries: VecDeque<Entry>,
    generation: u64,
}

/// 按 LRU 控制容量：最多 [`MAX_ENTRIES`] 个会话、合计约 [`MAX_TOTAL_BYTES`]。
pub struct TranscriptCache {
    state: Mutex<CacheState>,
    max_entries: usize,
    max_bytes: usize,
}

impl TranscriptCache {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            state: Mutex::new(CacheState::default()),
            max_entries,
            max_bytes,
        }
    }

    /// 锁中毒（上次解析 panic）时继续使用现有缓存，不影响本轮结果
    fn lock(&self) -> MutexGuard<'_, CacheState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 命中且指纹一致时复用，否则调用 `load` 重新解析。
    ///
    /// 返回 `(正文, 是否命中缓存)`。解析失败不缓存，下一轮重试。
    pub fn get_or_load<F>(
        &self,
        key: String,
        fingerprint: Fingerprint,
        load: F,
    ) -> Result<(Arc<Transcript>, bool), String>
    where
        F: FnOnce() -> Result<Transcript, String>,
    {
        let gen_before = {
            let mut state = self.lock();
            if let Some(index) = state.entries.iter().position(|entry| entry.key == key) {
                if state.entries[index].fingerprint.matches(&fingerprint) {
                    // 命中：移到队尾，保持 LRU 顺序（保留已有摘要供同秒改写比对）
                    if let Some(entry) = state.entries.remove(index) {
                        let transcript = Arc::clone(&entry.transcript);
                        state.entries.push_back(entry);
                        return Ok((transcript, true));
                    }
                } else {
                    // 指纹变了：旧结果作废，重新解析
                    state.entries.remove(index);
                }
            }
            state.generation
        };

        let mut transcript = load()?;
        // 由实际容量与世代一致性决定是否保留正文，不能让调用方提前标记为已缓存。
        let within_budget = self.max_entries > 0 && transcript.approx_bytes <= self.max_bytes;
        if !within_budget {
            transcript.cacheable = false;
            return Ok((Arc::new(transcript), false));
        }

        let mut state = self.lock();
        if state.generation != gen_before {
            transcript.cacheable = false;
            return Ok((Arc::new(transcript), false));
        }

        transcript.cacheable = true;
        let transcript = Arc::new(transcript);
        state.entries.retain(|entry| entry.key != key);
        state.entries.push_back(Entry {
            key,
            fingerprint,
            transcript: Arc::clone(&transcript),
        });
        while state.entries.len() > self.max_entries
            || state
                .entries
                .iter()
                .map(|entry| entry.transcript.approx_bytes)
                .sum::<usize>()
                > self.max_bytes
        {
            state.entries.pop_front();
        }
        Ok((transcript, false))
    }

    /// 删除会话后整体作废。删除会级联清理同 ID 的历史副本与索引，逐个推导受影响
    /// 的缓存键容易漏；删除是低频操作，整体清空最安全。
    pub fn clear(&self) {
        let mut state = self.lock();
        state.entries.clear();
        state.generation = state.generation.wrapping_add(1);
    }

    #[cfg(test)]
    fn keys(&self) -> Vec<String> {
        self.lock()
            .entries
            .iter()
            .map(|entry| entry.key.clone())
            .collect()
    }
}

impl Default for TranscriptCache {
    fn default() -> Self {
        Self::new(MAX_ENTRIES, MAX_TOTAL_BYTES)
    }
}

pub fn global() -> &'static TranscriptCache {
    static CACHE: LazyLock<TranscriptCache> = LazyLock::new(TranscriptCache::default);
    &CACHE
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::Duration;

    const KEY_A: &str = "/sessions/a.jsonl";

    fn transcript(content: &str) -> Transcript {
        Transcript::new(vec![SessionMessage {
            role: "user".to_string(),
            content: content.to_string(),
            ..SessionMessage::default()
        }])
    }

    fn member(modified: Option<SystemTime>, len: u64) -> MemberFingerprint {
        MemberFingerprint {
            modified,
            len,
            digest: None,
        }
    }

    fn stamp(len: u64) -> Fingerprint {
        Fingerprint {
            members: vec![(PathBuf::from(KEY_A), Some(member(None, len)))],
        }
    }

    /// 单文件源的缓存键；正文源集合固定为「该文件、读到末尾」。
    fn test_key(provider_id: &str, source_path: &str) -> String {
        cache_key(
            provider_id,
            source_path,
            &[(PathBuf::from(source_path), None)],
        )
    }

    #[test]
    fn cache_hits_until_the_fingerprint_changes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        std::fs::write(&path, "one").unwrap();
        let sources = vec![(path.clone(), None)];

        let cache = TranscriptCache::new(MAX_ENTRIES, MAX_TOTAL_BYTES);
        let loads = Cell::new(0);
        let load = || {
            loads.set(loads.get() + 1);
            Ok(transcript("hello"))
        };

        let fingerprint = transcript_fingerprint(&sources).unwrap();
        let key = cache_key("claude", KEY_A, &sources);
        let (first, cached) = cache
            .get_or_load(key.clone(), fingerprint.clone(), load)
            .unwrap();
        assert!(!cached);
        assert_eq!(first.messages.len(), 1);

        let (_, cached) = cache
            .get_or_load(key.clone(), fingerprint.clone(), load)
            .unwrap();
        assert!(cached, "指纹未变必须命中缓存");
        assert_eq!(loads.get(), 1);

        // 文件变了（长度变化）→ 指纹变化 → 重新解析，不返回过期正文
        std::fs::write(&path, "one-two").unwrap();
        let changed = transcript_fingerprint(&sources).unwrap();
        assert_ne!(changed, fingerprint);
        let (_, cached) = cache.get_or_load(key, changed, load).unwrap();
        assert!(!cached);
        assert_eq!(loads.get(), 2);
    }

    #[test]
    fn failed_load_is_not_cached() {
        let cache = TranscriptCache::new(MAX_ENTRIES, MAX_TOTAL_BYTES);
        let attempts = Cell::new(0);
        let load = || {
            attempts.set(attempts.get() + 1);
            Err("denied".to_string())
        };

        let key = test_key("claude", KEY_A);
        assert!(cache.get_or_load(key.clone(), stamp(1), load).is_err());
        assert!(cache.get_or_load(key, stamp(1), load).is_err());
        assert_eq!(attempts.get(), 2, "解析失败不能缓存，下一轮必须重试");
        assert!(cache.keys().is_empty());
    }

    #[test]
    fn lru_evicts_the_least_recently_used_entry() {
        let cache = TranscriptCache::new(2, MAX_TOTAL_BYTES);
        let insert = |path: &str| {
            cache
                .get_or_load(test_key("claude", path), stamp(1), || Ok(transcript("x")))
                .unwrap();
        };

        insert("/sessions/a");
        insert("/sessions/b");
        // 命中 a，使它成为最近使用；再插入 c 时应淘汰 b
        insert("/sessions/a");
        insert("/sessions/c");

        let keys = cache.keys();
        assert_eq!(keys.len(), 2);
        assert!(keys.iter().any(|key| key.contains("/sessions/a")));
        assert!(keys.iter().any(|key| key.contains("/sessions/c")));
        assert!(
            !keys.iter().any(|key| key.contains("/sessions/b")),
            "最少使用的一条应被淘汰"
        );
    }

    #[test]
    fn oversized_transcript_is_not_cached() {
        let cache = TranscriptCache::new(MAX_ENTRIES, 16);
        let key = test_key("deepseek", "/sessions/big");

        let (_, cached) = cache
            .get_or_load(key.clone(), stamp(1), || Ok(transcript(&"x".repeat(4096))))
            .unwrap();
        assert!(!cached);
        assert!(cache.keys().is_empty(), "超过总上限的单个会话不进缓存");

        // 没有留下任何条目，同一指纹的再次请求仍要重新解析
        let (_, cached) = cache
            .get_or_load(key, stamp(1), || Ok(transcript("small")))
            .unwrap();
        assert!(!cached);
    }

    #[test]
    fn cache_admission_controls_wire_truncation() {
        use crate::session_manager::wire::{wire_messages, MAX_INLINE_MESSAGE_CHARS};

        // 缩小缓存预算复现 96 MiB 边界，避免测试分配上百 MiB。
        let content = "x".repeat(MAX_INLINE_MESSAGE_CHARS + 1);
        let make_transcript = || {
            Transcript::new(vec![
                SessionMessage {
                    role: "user".into(),
                    content: content.clone(),
                    ..SessionMessage::default()
                },
                SessionMessage {
                    role: "assistant".into(),
                    content: content.clone(),
                    ..SessionMessage::default()
                },
            ])
        };
        let size = make_transcript().approx_bytes;
        for (max_entries, max_bytes, retained) in
            [(2, size, true), (2, size - 1, false), (0, size, false)]
        {
            let cache = TranscriptCache::new(max_entries, max_bytes);
            let key = test_key("claude", KEY_A);
            let loads = Cell::new(0);
            for attempt in 0..2 {
                let (result, hit) = cache
                    .get_or_load(key.clone(), stamp(1), || {
                        loads.set(loads.get() + 1);
                        // 即使加载器误标记，缓存层也必须纠正实际保留状态。
                        let mut value = make_transcript();
                        value.cacheable = true;
                        Ok(value)
                    })
                    .unwrap();
                assert_eq!(result.cacheable, retained);
                assert_eq!(hit, retained && attempt > 0);
                let wire = wire_messages(result.cacheable, "claude", &result.messages);
                assert_eq!(wire.len(), 2);
                for message in wire {
                    assert_eq!(message.truncated, retained);
                    if !retained {
                        assert_eq!(message.content, content);
                        assert_eq!(message.index, None);
                    }
                }
            }
            assert_eq!(loads.get(), if retained { 1 } else { 2 });
            assert_eq!(cache.keys().len(), usize::from(retained));
        }
    }

    #[test]
    fn grokbuild_fingerprint_covers_the_chat_history() {
        let temp = tempfile::tempdir().unwrap();
        let session = temp.path().join("session-1");
        std::fs::create_dir_all(&session).unwrap();
        let summary = session.join("summary.json");
        let chat = session.join("chat_history.jsonl");
        std::fs::write(&summary, "{}").unwrap();
        std::fs::write(&chat, "line").unwrap();

        let sources = vec![(summary.clone(), None), (chat.clone(), None)];
        let before = transcript_fingerprint(&sources).unwrap();
        // summary.json 没变，只有正文追加：指纹必须变化，否则会命中旧正文
        std::fs::write(&chat, "line-appended").unwrap();
        assert_ne!(before, transcript_fingerprint(&sources).unwrap());
    }

    #[test]
    fn missing_optional_member_is_tolerated_but_empty_sources_are_not() {
        let temp = tempfile::tempdir().unwrap();
        let summary = temp.path().join("summary.json");
        std::fs::write(&summary, "{}").unwrap();

        let only_summary = transcript_fingerprint(&[(summary.clone(), None)]).unwrap();
        let absent = temp.path().join("chat_history.jsonl");
        let with_absent =
            transcript_fingerprint(&[(summary, None), (absent.clone(), None)]).unwrap();
        assert_ne!(
            only_summary, with_absent,
            "本轮缺失的成员必须改变指纹，否则它出现后会命中它缺席时的旧正文"
        );

        // 缺席成员出现 → 指纹再变，正文必须重新解析
        std::fs::write(&absent, "line").unwrap();
        let appeared = transcript_fingerprint(&[(absent, None)]).unwrap();
        assert!(transcript_fingerprint(&[(temp.path().join("nothing"), None)]).is_err());
        assert_ne!(
            with_absent.members[1].1, appeared.members[0].1,
            "成员出现后其指纹项必须从「缺失」变为可读"
        );
    }

    /// 曾经的指纹实现是「取最新 mtime + 长度求和」，改较早的成员可以配合改较晚的
    /// 成员让两处改动互相抵消，从而静默命中旧正文。这里把那个抵消关系固定成回归：
    /// 长度相同、mtime 更早的次要成员被改写时，指纹必须变化。
    #[test]
    fn rewriting_an_earlier_member_with_the_same_length_changes_the_fingerprint() {
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let older = base;
        let newer = base + Duration::from_secs(600);

        // 「旧」：较早成员 mtime=older、长度 4；较晚成员 mtime=newer、长度 4
        let before = Fingerprint {
            members: vec![
                (PathBuf::from("/db"), Some(member(Some(older), 4))),
                (PathBuf::from("/db-wal"), Some(member(Some(newer), 4))),
            ],
        };
        // 「新」：较早成员内容被同长度改写，mtime 前进但仍早于较晚成员
        let after = Fingerprint {
            members: vec![
                (
                    PathBuf::from("/db"),
                    Some(member(Some(older + Duration::from_secs(60)), 4)),
                ),
                (PathBuf::from("/db-wal"), Some(member(Some(newer), 4))),
            ],
        };

        assert_ne!(
            before, after,
            "同长度的次要成员改写必须改变指纹，否则会命中旧正文"
        );

        // 对照：旧实现压缩成 (max mtime, sum len) 时两者完全相同 —— 这正是要根除的行为
        let legacy = |fingerprint: &Fingerprint| {
            fingerprint.members.iter().fold(
                (None::<SystemTime>, 0u64),
                |(modified, len), (_, item)| {
                    let item = item.as_ref().expect("readable member");
                    (modified.max(item.modified), len + item.len)
                },
            )
        };
        assert_eq!(
            legacy(&before),
            legacy(&after),
            "旧压缩方式确实无法区分这两者（回归测试的前提）"
        );
    }

    #[test]
    fn sqlite_fingerprint_notices_a_same_length_wal_rewrite() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("opencode.db");
        std::fs::write(&database, "database").unwrap();

        let sources = sqlite_transcript_sources(&database);
        let before = transcript_fingerprint(&sources).unwrap();

        // WAL 尚未出现时，指纹里必须留下它的位置
        assert_eq!(before.members.len(), 2);
        assert!(before.members[1].1.is_none());

        std::fs::write(format!("{}-wal", database.to_string_lossy()), "wal-1").unwrap();
        let with_wal = transcript_fingerprint(&sources).unwrap();
        assert_ne!(before, with_wal, "WAL 出现必须改变指纹");

        // 同长度改写 WAL：主库 mtime 不变，旧实现会认为什么都没发生
        std::fs::write(format!("{}-wal", database.to_string_lossy()), "wal-2").unwrap();
        let rewritten = transcript_fingerprint(&sources).unwrap();
        assert_ne!(with_wal, rewritten, "同长度改写 WAL 必须改变指纹");
    }

    #[test]
    fn sources_digest_separates_order_and_truncation() {
        let a = PathBuf::from("/sessions/a.jsonl");
        let b = PathBuf::from("/sessions/b.jsonl");
        let forward = vec![(a.clone(), None), (b.clone(), None)];
        let reversed = vec![(b.clone(), None), (a.clone(), None)];
        let truncated = vec![(a.clone(), Some(1024)), (b.clone(), None)];

        assert_ne!(
            sources_digest(&forward),
            sources_digest(&reversed),
            "同一组文件换了顺序就是另一个正文源"
        );
        assert_ne!(
            sources_digest(&forward),
            sources_digest(&truncated),
            "只读到偏移的段与整段不同"
        );
        assert_eq!(
            sources_digest(&forward),
            sources_digest(&forward.clone()),
            "同一输入必须稳定"
        );
        // 相邻路径不能被哈希成同一个序列
        assert_ne!(
            sources_digest(&[(PathBuf::from("/ab"), None), (PathBuf::from("/c"), None)]),
            sources_digest(&[(PathBuf::from("/a"), None), (PathBuf::from("/bc"), None)])
        );
    }

    #[test]
    fn transcript_keeps_per_message_sizes_aligned_with_messages() {
        let transcript = Transcript::new(vec![
            SessionMessage {
                role: "user".to_string(),
                content: "a".to_string(),
                ..SessionMessage::default()
            },
            SessionMessage {
                role: "assistant".to_string(),
                content: "bb".to_string(),
                ts: Some(1),
                ..SessionMessage::default()
            },
        ]);

        assert_eq!(transcript.message_bytes.len(), transcript.messages.len());
        assert_eq!(
            transcript.approx_bytes,
            transcript.message_bytes.iter().sum::<usize>()
        );
    }

    #[test]
    fn sqlite_fingerprint_follows_the_wal_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("opencode.db");
        let wal = temp.path().join("opencode.db-wal");
        std::fs::write(&db, "database").unwrap();
        std::fs::write(&wal, "wal-1").unwrap();

        let sources = sqlite_transcript_sources(&db);
        assert_eq!(sources, vec![(db.clone(), None), (wal.clone(), None)]);

        let before = transcript_fingerprint(&sources).unwrap();
        // 主库一字未改，只追加 WAL：指纹必须变化，否则会命中旧正文
        std::fs::write(&wal, "wal-1-appended").unwrap();
        assert_ne!(before, transcript_fingerprint(&sources).unwrap());

        // WAL 尚未出现（刚建的库）时只看主库，也要能给出指纹；但「缺席」必须
        // 与「存在」区分开，否则 WAL 出现后仍会命中它缺席时的旧正文
        std::fs::remove_file(&wal).unwrap();
        let without_wal = transcript_fingerprint(&sources).unwrap();
        assert_ne!(without_wal, transcript_fingerprint(&[(db, None)]).unwrap());
        assert_eq!(without_wal.members[1].1, None);
    }

    #[test]
    fn chunk_ranges_cover_every_message_exactly_once() {
        // 300 条小消息 → 按 150 条切两块
        assert_eq!(chunk_ranges(&[16; 300]), vec![(0, 150), (150, 300)]);

        // 空输入不产生块
        assert!(chunk_ranges(&[]).is_empty());

        // 覆盖全量、首尾对齐、区间连续且非空
        let mixed = vec![1024, 512 * 1024, 8, 200_000, 1];
        let ranges = chunk_ranges(&mixed);
        assert_eq!(ranges.first().unwrap().0, 0);
        assert_eq!(ranges.last().unwrap().1, mixed.len());
        for window in ranges.windows(2) {
            assert_eq!(window[0].1, window[1].0, "区间必须连续");
        }
        for (start, end) in &ranges {
            assert!(end > start, "不允许空块");
        }
        // 超过单块上限的消息独占一块，不从中间切开
        assert_eq!(ranges[1], (1, 2));
    }

    #[test]
    fn chunk_ranges_split_on_bytes_before_the_count_limit() {
        let bytes = vec![CHUNK_MAX_BYTES / 2, CHUNK_MAX_BYTES / 2, 1];
        // 前两条刚好不超过上限，第三条再加就超了
        assert_eq!(chunk_ranges(&bytes), vec![(0, 2), (2, 3)]);
    }

    #[test]
    fn older_regular_member_skips_digest_while_wal_still_digests() {
        let temp = tempfile::tempdir().unwrap();
        let regular = temp.path().join("session.jsonl");
        let wal = temp.path().join("opencode.db-wal");
        std::fs::write(&regular, "regular-payload").unwrap();
        std::fs::write(&wal, "wal-payload").unwrap();

        let old_time = SystemTime::now() - Duration::from_secs(10);
        fs::OpenOptions::new()
            .write(true)
            .open(&regular)
            .unwrap()
            .set_modified(old_time)
            .unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&wal)
            .unwrap()
            .set_modified(old_time)
            .unwrap();

        let regular_fp = MemberFingerprint::of_source(&regular, None).unwrap();
        let wal_fp = MemberFingerprint::of_source(&wal, None).unwrap();
        assert!(regular_fp.digest.is_none());
        assert!(wal_fp.digest.is_some());
    }

    #[test]
    fn digest_file_respects_end_byte_offset() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("base.jsonl");
        std::fs::write(&path, "hello-1111").unwrap();
        let fp_prefix_a = MemberFingerprint::of_source(&path, Some(5)).unwrap();

        std::fs::write(&path, "hello-2222").unwrap();
        let fp_prefix_b = MemberFingerprint::of_source(&path, Some(5)).unwrap();
        assert_eq!(fp_prefix_a.digest, fp_prefix_b.digest);

        std::fs::write(&path, "world-2222").unwrap();
        let fp_prefix_c = MemberFingerprint::of_source(&path, Some(5)).unwrap();
        assert_ne!(fp_prefix_a.digest, fp_prefix_c.digest);
    }

    #[test]
    fn non_regular_file_is_rejected_by_member_fingerprint() {
        let temp = tempfile::tempdir().unwrap();
        let err = MemberFingerprint::of_source(temp.path(), None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn aging_past_two_seconds_keeps_cache_hit_while_same_second_rewrite_misses() {
        let cache = TranscriptCache::new(MAX_ENTRIES, MAX_TOTAL_BYTES);
        let key = test_key("claude", KEY_A);
        let mtime = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));

        let young = Fingerprint {
            members: vec![(
                PathBuf::from(KEY_A),
                Some(MemberFingerprint {
                    modified: mtime,
                    len: 42,
                    digest: Some(0x1111),
                }),
            )],
        };
        let aged = Fingerprint {
            members: vec![(
                PathBuf::from(KEY_A),
                Some(MemberFingerprint {
                    modified: mtime,
                    len: 42,
                    digest: None,
                }),
            )],
        };
        let rewritten_same_second = Fingerprint {
            members: vec![(
                PathBuf::from(KEY_A),
                Some(MemberFingerprint {
                    modified: mtime,
                    len: 42,
                    digest: Some(0x2222),
                }),
            )],
        };

        let (_, hit) = cache
            .get_or_load(key.clone(), young, || Ok(transcript("v1")))
            .unwrap();
        assert!(!hit);

        // File aged past 2s without mtime/len changing -> must still hit!
        let (res, hit) = cache
            .get_or_load(key.clone(), aged, || panic!("must hit aged entry"))
            .unwrap();
        assert!(hit);
        assert_eq!(res.messages[0].content, "v1");

        // Same-second rewrite with different digest -> must miss!
        let (res2, hit) = cache
            .get_or_load(key, rewritten_same_second, || Ok(transcript("v2")))
            .unwrap();
        assert!(!hit);
        assert_eq!(res2.messages[0].content, "v2");
    }

    #[test]
    fn clear_during_load_prevents_stale_insertion_and_deduplicates_concurrent_keys() {
        let cache = TranscriptCache::new(MAX_ENTRIES, MAX_TOTAL_BYTES);
        let key = test_key("claude", KEY_A);

        let (stale, hit) = cache
            .get_or_load(key.clone(), stamp(10), || {
                cache.clear();
                Ok(transcript("stale"))
            })
            .unwrap();
        assert!(!hit);
        assert!(!stale.cacheable);
        assert!(cache.keys().is_empty());

        // Concurrent duplicate load of the same key in the same generation deduplicates cleanly.
        let (_, hit) = cache
            .get_or_load(key.clone(), stamp(10), || {
                let _ = cache
                    .get_or_load(key.clone(), stamp(10), || Ok(transcript("inner")))
                    .unwrap();
                Ok(transcript("outer"))
            })
            .unwrap();
        assert!(!hit);
        assert_eq!(cache.keys().len(), 1);
    }
}
