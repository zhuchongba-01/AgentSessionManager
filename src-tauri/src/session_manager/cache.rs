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
use std::time::SystemTime;

use super::SessionMessage;

/// 最多缓存的会话数
pub const MAX_ENTRIES: usize = 8;
/// 缓存内所有会话正文的合计字节上限（按内容长度估算，见 [`Transcript::new`]）
pub const MAX_TOTAL_BYTES: usize = 96 * 1024 * 1024;

/// 源的指纹：修改时间 + 长度。多文件源取较新的 mtime 与长度之和。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    pub modified: Option<SystemTime>,
    pub len: u64,
}

impl Fingerprint {
    fn of_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        }
    }

    fn of_path(path: &Path) -> io::Result<Self> {
        Ok(Self::of_metadata(&fs::metadata(path)?))
    }

    fn merge(&mut self, other: Self) {
        self.modified = match (self.modified, other.modified) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        self.len = self.len.wrapping_add(other.len);
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
pub fn transcript_fingerprint(sources: &[(PathBuf, Option<u64>)]) -> io::Result<Fingerprint> {
    let mut merged: Option<Fingerprint> = None;
    for (path, _) in sources {
        let Ok(fingerprint) = Fingerprint::of_path(path) else {
            continue;
        };
        match &mut merged {
            Some(current) => current.merge(fingerprint),
            None => merged = Some(fingerprint),
        }
    }
    merged.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "会话源不可读"))
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
    /// 每条消息的估算字节数，流式分块按它切包，避免为了统计再序列化一遍
    pub message_bytes: Vec<usize>,
    /// 正文的估算字节数，只用于容量控制
    pub approx_bytes: usize,
}

impl Transcript {
    pub fn new(messages: Vec<SessionMessage>) -> Self {
        // role/content 长度加上每条消息的固定开销（JSON 键名、引号、时间戳）
        let message_bytes = messages
            .iter()
            .map(|message| message.role.len() + message.content.len() + 32)
            .collect::<Vec<_>>();
        let approx_bytes = message_bytes.iter().sum();
        Self {
            messages,
            message_bytes,
            approx_bytes,
        }
    }
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

/// 按 LRU 控制容量：最多 [`MAX_ENTRIES`] 个会话、合计约 [`MAX_TOTAL_BYTES`]。
pub struct TranscriptCache {
    entries: Mutex<VecDeque<Entry>>,
    max_entries: usize,
    max_bytes: usize,
}

impl TranscriptCache {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
            max_entries,
            max_bytes,
        }
    }

    /// 锁中毒（上次解析 panic）时继续使用现有缓存，不影响本轮结果
    fn lock(&self) -> MutexGuard<'_, VecDeque<Entry>> {
        match self.entries.lock() {
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
        {
            let mut entries = self.lock();
            if let Some(index) = entries.iter().position(|entry| entry.key == key) {
                if entries[index].fingerprint == fingerprint {
                    // 命中：移到队尾，保持 LRU 顺序
                    if let Some(entry) = entries.remove(index) {
                        let transcript = Arc::clone(&entry.transcript);
                        entries.push_back(entry);
                        return Ok((transcript, true));
                    }
                } else {
                    // 指纹变了：旧结果作废，重新解析
                    entries.remove(index);
                }
            }
        }

        let transcript = load()?;
        let approx_bytes = transcript.approx_bytes;
        let transcript = Arc::new(transcript);
        // 超过总上限的单个会话不进缓存，否则会立刻把其他条目全部挤掉
        if approx_bytes <= self.max_bytes {
            let mut entries = self.lock();
            entries.push_back(Entry {
                key,
                fingerprint,
                transcript: Arc::clone(&transcript),
            });
            while entries.len() > self.max_entries
                || entries
                    .iter()
                    .map(|entry| entry.transcript.approx_bytes)
                    .sum::<usize>()
                    > self.max_bytes
            {
                entries.pop_front();
            }
        }
        Ok((transcript, false))
    }

    /// 删除会话后整体作废。删除会级联清理同 ID 的历史副本与索引，逐个推导受影响
    /// 的缓存键容易漏；删除是低频操作，整体清空最安全。
    pub fn clear(&self) {
        self.lock().clear();
    }

    #[cfg(test)]
    fn keys(&self) -> Vec<String> {
        self.lock().iter().map(|entry| entry.key.clone()).collect()
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

    const KEY_A: &str = "/sessions/a.jsonl";

    fn transcript(content: &str) -> Transcript {
        Transcript::new(vec![SessionMessage {
            role: "user".to_string(),
            content: content.to_string(),
            ts: None,
        }])
    }

    fn stamp(len: u64) -> Fingerprint {
        Fingerprint {
            modified: None,
            len,
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
        let (first, cached) = cache.get_or_load(key.clone(), fingerprint, load).unwrap();
        assert!(!cached);
        assert_eq!(first.messages.len(), 1);

        let (_, cached) = cache.get_or_load(key.clone(), fingerprint, load).unwrap();
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
        assert_eq!(
            only_summary,
            transcript_fingerprint(&[(summary, None), (absent, None)]).unwrap(),
            "尚未落盘的可选成员应被跳过"
        );
        assert!(transcript_fingerprint(&[(temp.path().join("nothing"), None)]).is_err());
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
                ts: None,
            },
            SessionMessage {
                role: "assistant".to_string(),
                content: "bb".to_string(),
                ts: Some(1),
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

        // WAL 尚未出现（刚建的库）时只看主库，也要能给出指纹
        std::fs::remove_file(&wal).unwrap();
        assert_eq!(
            transcript_fingerprint(&sources).unwrap(),
            transcript_fingerprint(&[(db, None)]).unwrap()
        );
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
}
