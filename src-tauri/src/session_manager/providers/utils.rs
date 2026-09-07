use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset};
use serde_json::Value;

thread_local! {
    static SCAN_WARNINGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub fn scan_warning(message: impl Into<String>) {
    SCAN_WARNINGS.with(|warnings| {
        let mut warnings = warnings.borrow_mut();
        if warnings.len() < 20 {
            warnings.push(message.into());
        }
    });
}

pub fn scan_with_diagnostics(
    provider: &str,
    scan: fn() -> Vec<crate::session_manager::SessionMeta>,
) -> (Vec<crate::session_manager::SessionMeta>, Vec<String>) {
    SCAN_WARNINGS.with(|warnings| warnings.borrow_mut().clear());
    let sessions = scan();
    let warnings = SCAN_WARNINGS.with(|warnings| std::mem::take(&mut *warnings.borrow_mut()));
    (
        sessions,
        warnings
            .into_iter()
            .map(|warning| format!("{provider}: {warning}"))
            .collect(),
    )
}

/// Maximum number of characters for session titles (shared across providers).
pub const TITLE_MAX_CHARS: usize = 80;

/// 整读会话文件前的体积上限，与 Pi provider 的 `MAX_SESSION_BYTES` 对齐。
/// 超限常见于携带 base64 截图或大段日志的会话：整读会把数倍内存拉进进程，
/// 再一次性序列化过 IPC。
pub const MAX_MESSAGES_FILE_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_QUERY_BYTES: i64 = 32 * 1024 * 1024;
pub const MAX_QUERY_ROWS: i64 = 20_000;
const MAX_SCAN_LINE_BYTES: u64 = 4 * 1024 * 1024;

/// Validate every derived path, including symlinked ancestors of missing targets.
pub fn checked_storage_child(root: &Path, child: &Path) -> Result<PathBuf, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    if child
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err("Storage path contains a parent-directory component".into());
    }
    let mut existing = child;
    let mut suffix = Vec::new();
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                suffix.push(
                    existing
                        .file_name()
                        .ok_or("Invalid storage path")?
                        .to_os_string(),
                );
                existing = existing.parent().ok_or("Invalid storage ancestor")?;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    let mut resolved = existing.canonicalize().map_err(|e| e.to_string())?;
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    if resolved == root || !resolved.starts_with(&root) {
        return Err("Derived storage path escapes its root".into());
    }
    Ok(resolved)
}

pub fn check_sqlite_message_budget(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<(), String> {
    for table in ["message", "part"] {
        let sql = format!("SELECT COUNT(*), COALESCE(SUM(LENGTH(CAST(data AS BLOB))), 0) FROM {table} WHERE session_id=?1");
        let (rows, bytes): (i64, i64) = conn
            .query_row(&sql, [session_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|e| format!("无法检查会话读取大小：{e}"))?;
        if rows > MAX_QUERY_ROWS || bytes > MAX_QUERY_BYTES {
            return Err(
                "会话超过单次读取限制（每表 20000 条 / 32 MB），请使用原 Agent 分段查看".into(),
            );
        }
    }
    Ok(())
}

/// 会话 ID 会被拼进 `claude --resume {id}` 这类 resume 命令，并在 macOS 上
/// 经 shell 执行（Windows 上也会被复制进剪贴板供用户粘贴）。因此只接受
/// 真实 UUID/雪花 ID 会出现的字符，杜绝 `;`、`$` 等元字符借道会话 JSON 或
/// 文件名注入。校验不过时 resume_command 置空，前端恢复按钮自然禁用。
pub fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// 读取整份会话消息前检查文件体积。
pub fn ensure_readable_size(path: &Path) -> Result<(), String> {
    let len = std::fs::metadata(path)
        .map_err(|error| format!("Failed to stat session file {}: {error}", path.display()))?
        .len();
    if len > MAX_MESSAGES_FILE_BYTES {
        return Err(format!(
            "会话文件 {} 超过 {} MB 加载上限",
            path.display(),
            MAX_MESSAGES_FILE_BYTES / 1024 / 1024
        ));
    }
    Ok(())
}

/// 递归收集 `root` 下满足 `predicate` 的文件。
///
/// 用 `DirEntry::file_type()` 判定目录：它不跟随符号链接。`Path::is_dir()`
/// 会跟随链接，数据根里一个自引用链接（解压备份、同步盘冲突副本中并不
/// 罕见）就能让扫描无限递归直至栈溢出，还会深入到数据根之外。Pi provider
/// 早已采用同样的写法。
pub fn collect_files_where<F>(root: &Path, predicate: F, files: &mut Vec<PathBuf>)
where
    F: Fn(&Path) -> bool,
{
    // 递归体接受 &F（?Sized）：若在此处直接传递 &predicate，每层递归都会
    // 再叠加一层引用，实例化出 &&&&…F 直至触发递归上限。
    fn walk<F>(root: &Path, predicate: &F, files: &mut Vec<PathBuf>)
    where
        F: Fn(&Path) -> bool + ?Sized,
    {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                scan_warning(format!("无法扫描 {}：{error}", root.display()));
                return;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    scan_warning(format!("无法枚举 {}：{error}", root.display()));
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(kind) => kind,
                Err(error) => {
                    scan_warning(format!("无法读取 {}：{error}", entry.path().display()));
                    continue;
                }
            };
            let path = entry.path();
            if file_type.is_dir() {
                walk(&path, predicate, files);
            } else if file_type.is_file() && predicate(&path) {
                files.push(path);
            }
        }
    }
    walk(root, &predicate, files)
}

/// Read the first `head_n` lines and last `tail_n` lines from a file.
/// For small files (< 16 KB), reads all lines once to avoid unnecessary seeking.
pub fn read_head_tail_lines(
    path: &Path,
    head_n: usize,
    tail_n: usize,
) -> io::Result<(Vec<String>, Vec<String>)> {
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();

    // For small files, read all lines once and split
    if file_len < 16_384 {
        let reader = BufReader::new(file);
        let all: Vec<String> = reader.lines().collect::<io::Result<_>>()?;
        let head = all.iter().take(head_n).cloned().collect();
        let skip = all.len().saturating_sub(tail_n);
        let tail = all.into_iter().skip(skip).collect();
        return Ok((head, tail));
    }

    // Read head lines from the beginning
    let mut reader = BufReader::new(file);
    let mut head = Vec::new();
    for _ in 0..head_n {
        let mut line = String::new();
        let count = reader
            .by_ref()
            .take(MAX_SCAN_LINE_BYTES + 1)
            .read_line(&mut line)?;
        if count == 0 {
            break;
        }
        if count as u64 > MAX_SCAN_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Session metadata line exceeds scan limit",
            ));
        }
        head.push(line);
    }

    // Seek to last ~16 KB for tail lines
    let seek_pos = file_len.saturating_sub(16_384);
    let mut file2 = File::open(path)?;
    file2.seek(SeekFrom::Start(seek_pos))?;
    let mut tail_reader = BufReader::new(file2);
    // Discard the partial line as bytes, BEFORE attempting UTF-8 decoding.
    if seek_pos > 0 {
        let mut partial = Vec::new();
        tail_reader.read_until(b'\n', &mut partial)?;
    }
    let usable: Vec<String> = tail_reader.lines().collect::<io::Result<_>>()?;
    let skip = usable.len().saturating_sub(tail_n);
    let tail = usable.into_iter().skip(skip).collect();

    Ok((head, tail))
}

pub fn parse_timestamp_to_ms(value: &Value) -> Option<i64> {
    // Integer: milliseconds (>1e12) or seconds
    if let Some(n) = value.as_i64() {
        return Some(if n > 1_000_000_000_000 { n } else { n * 1000 });
    }
    if let Some(n) = value.as_f64() {
        let n = n as i64;
        return Some(if n > 1_000_000_000_000 { n } else { n * 1000 });
    }
    // RFC3339 string
    let raw = value.as_str()?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt: DateTime<FixedOffset>| dt.timestamp_millis())
}

pub fn extract_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.to_string(),
        Value::Array(items) => items
            .iter()
            .filter_map(extract_text_from_item)
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => map
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

fn extract_text_from_item(item: &Value) -> Option<String> {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");

    // Anthropic uses tool_use; Pi's assistant messages use toolCall.
    if matches!(item_type, "tool_use" | "toolCall") {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Some(format!("[Tool: {name}]"));
    }

    // tool_result: extract nested content
    if item_type == "tool_result" {
        if let Some(content) = item.get("content") {
            let text = extract_text(content);
            if !text.is_empty() {
                return Some(text);
            }
        }
        return None;
    }

    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
        return Some(text.to_string());
    }

    if let Some(text) = item.get("input_text").and_then(|v| v.as_str()) {
        return Some(text.to_string());
    }

    if let Some(text) = item.get("output_text").and_then(|v| v.as_str()) {
        return Some(text.to_string());
    }

    if let Some(content) = item.get("content") {
        let text = extract_text(content);
        if !text.is_empty() {
            return Some(text);
        }
    }

    None
}

pub fn truncate_summary(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }

    let mut result = trimmed.chars().take(max_chars).collect::<String>();
    result.push_str("...");
    result
}

pub fn path_basename(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.trim_end_matches(['/', '\\']);
    let last = normalized
        .split(['/', '\\'])
        .next_back()
        .filter(|segment| !segment.is_empty())?;
    Some(last.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn derived_paths_cannot_escape_storage() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("storage");
        std::fs::create_dir_all(root.join("part")).unwrap();
        assert!(checked_storage_child(&root, &root.join("part").join("msg_1")).is_ok());
        assert!(checked_storage_child(
            &root,
            &root.join("part").join("..").join("..").join("victim")
        )
        .is_err());
        assert!(checked_storage_child(&root, temp.path()).is_err());
        assert!(checked_storage_child(&root, &root).is_err());
    }

    #[test]
    fn tail_discards_partial_utf8_before_decoding() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("unicode.jsonl");
        let text = format!(
            "{{\"text\":\"{}\"}}\n{{\"last\":true}}\n",
            "中".repeat(6000)
        );
        assert_eq!(text.as_bytes()[text.len() - 16_384] & 0xc0, 0x80);
        std::fs::write(&path, text).unwrap();
        let (_, tail) = read_head_tail_lines(&path, 1, 30).unwrap();
        assert_eq!(tail, vec!["{\"last\":true}"]);
    }

    #[test]
    fn scan_rejects_unbounded_metadata_lines() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("large.jsonl");
        std::fs::write(&path, "x".repeat(MAX_SCAN_LINE_BYTES as usize + 2)).unwrap();
        assert!(read_head_tail_lines(&path, 1, 1).is_err());
    }

    #[test]
    fn sqlite_read_budget_bounds_record_count() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE message(session_id TEXT, data TEXT); CREATE TABLE part(session_id TEXT, data TEXT);
            WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<20001)
            INSERT INTO message SELECT 'ses_1', '{}' FROM n;").unwrap();
        assert!(check_sqlite_message_budget(&conn, "ses_1").is_err());
        assert!(check_sqlite_message_budget(&conn, "other").is_ok());
    }

    #[test]
    fn parse_timestamp_to_ms_supports_integers_and_rfc3339() {
        assert_eq!(
            parse_timestamp_to_ms(&json!(1_771_061_953_033_i64)),
            Some(1_771_061_953_033)
        );
        assert_eq!(
            parse_timestamp_to_ms(&json!(1_771_061_953_i64)),
            Some(1_771_061_953_000)
        );
        assert_eq!(
            parse_timestamp_to_ms(&json!("1970-01-01T00:00:01Z")),
            Some(1_000)
        );
    }

    #[test]
    fn extract_text_supports_pi_tool_calls() {
        assert_eq!(
            extract_text(&json!([{ "type": "toolCall", "name": "read" }])),
            "[Tool: read]"
        );
    }

    #[test]
    fn is_safe_session_id_accepts_native_id_shapes_only() {
        assert!(is_safe_session_id("019cc369-bd7c-7891-b371-7b20b4fe0b18"));
        assert!(is_safe_session_id("ses_01H9x2K3"));
        assert!(is_safe_session_id("thread_1"));
        assert!(!is_safe_session_id(""));
        assert!(!is_safe_session_id("x; calc"));
        assert!(!is_safe_session_id("$(id -un)"));
        assert!(!is_safe_session_id("a/b"));
        assert!(!is_safe_session_id("空格 id"));
        assert!(!is_safe_session_id(&"a".repeat(129)));
    }

    #[test]
    fn ensure_readable_size_rejects_oversized_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let small = temp.path().join("small.jsonl");
        std::fs::write(&small, "{}").expect("write small");
        assert!(ensure_readable_size(&small).is_ok());

        let oversized = temp.path().join("oversized.jsonl");
        std::fs::File::create(&oversized)
            .expect("create sparse file")
            .set_len(MAX_MESSAGES_FILE_BYTES + 1)
            .expect("size sparse file");
        let error = ensure_readable_size(&oversized).expect_err("oversized must be rejected");
        assert!(error.contains("加载上限"));
    }
}
