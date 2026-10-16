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
    /// `len` 不超过 [`MAX_DIGESTED_SOURCE_BYTES`] 时的内容摘要；超出则为 `None`
    pub digest: Option<u64>,
}

/// 参与内容摘要的单成员体积上限。超过这个大小的源只比 `(mtime, len)`：
/// 每次打开会话都完整读一遍大文件，代价会盖过缓存带来的收益。
pub const MAX_DIGESTED_SOURCE_BYTES: u64 = 16 * 1024 * 1024;

impl MemberFingerprint {
    fn of_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
            digest: None,
        }
    }

    fn of_path(path: &Path) -> io::Result<Self> {
        let metadata = fs::metadata(path)?;
        let mut fingerprint = Self::of_metadata(&metadata);
        if metadata.len() <= MAX_DIGESTED_SOURCE_BYTES {
            fingerprint.digest = Some(digest_file(path)?);
        }
        Ok(fingerprint)
    }
}

/// 文件内容的稳定摘要（FNV-1a）。只用于判断"是否变过"，不用于安全目的。
fn digest_file(path: &Path) -> io::Result<u64> {
    use std::io::Read;

    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut file = fs::File::open(path)?;
    let mut hash = OFFSET_BASIS;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
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
    for (path, _) in sources {
        match MemberFingerprint::of_path(path) {
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
    /// 每条消息的估算字节数，流式分块按它切包，避免为了统计再序列化一遍
    pub message_bytes: Vec<usize>,
    /// 正文的估算字节数，只用于容量控制
    pub approx_bytes: usize,
    /// 本次解析结果是否进了缓存，因而「展开取全文」不必重新解析整个会话。
    ///
    /// 下发形态据此决定能否只发预览：按形态（provider + source_path）推断是不够的，
    /// Codex 的分页链读不出来时会退化成不缓存，那种情况下截断会让展开变成一次全量
    /// 重解析。由解析结果自己携带这个事实，判定就没有第二处可以走偏。
    pub cacheable: bool,
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
            cacheable: false,
        }
    }

    /// 标记该解析结果已在缓存中，可以安全地只下发预览。
    pub fn cached(mut self) -> Self {
        self.cacheable = true;
        self
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
    use std::time::Duration;

    const KEY_A: &str = "/sessions/a.jsonl";

    fn transcript(content: &str) -> Transcript {
        Transcript::new(vec![SessionMessage {
            role: "user".to_string(),
            content: content.to_string(),
            ts: None,
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
}
