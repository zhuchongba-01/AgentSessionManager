use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use chrono::{DateTime, FixedOffset};
use serde::Serialize;
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

/// Check whether metadata describes a symlink or (on Windows) any reparse point
/// such as an NTFS directory junction (`FILE_ATTRIBUTE_REPARSE_POINT = 0x400`).
pub fn is_reparse_or_symlink(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_type().is_symlink() || (meta.file_attributes() & 0x400 != 0)
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

fn same_directory_entry(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    let (Some(a_name), Some(b_name), Some(a_parent), Some(b_parent)) =
        (a.file_name(), b.file_name(), a.parent(), b.parent())
    else {
        return false;
    };
    let names_match = {
        #[cfg(windows)]
        {
            a_name
                .to_string_lossy()
                .eq_ignore_ascii_case(&b_name.to_string_lossy())
        }
        #[cfg(not(windows))]
        {
            a_name == b_name
        }
    };
    names_match
        && a_parent
            .canonicalize()
            .ok()
            .zip(b_parent.canonicalize().ok())
            .is_some_and(|(pa, pb)| pa == pb)
}

/// Verify that `child` descends from `root` and that no existing intermediate
/// directory strictly between `root` and `child` is a symlink or reparse point.
fn verify_no_intermediate_links(
    raw_root: &Path,
    canonical_root: &Path,
    child: &Path,
) -> Result<(), String> {
    let mut current = child
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or("Invalid storage path")?;

    loop {
        if current == raw_root || current == canonical_root {
            return Ok(());
        }
        match std::fs::symlink_metadata(current) {
            Ok(meta) => {
                if is_reparse_or_symlink(&meta) {
                    if same_directory_entry(current, raw_root) {
                        return Ok(());
                    }
                    return Err(format!(
                        "Storage path traverses a symlink or reparse point: {}",
                        current.display()
                    ));
                }
                if current
                    .canonicalize()
                    .is_ok_and(|resolved| resolved == canonical_root)
                {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        current = current
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or("Derived storage path escapes its root")?;
    }
}

/// Validate every derived path, including symlinked ancestors of missing targets.
///
/// 目标本身是一个符号链接时**不解析它**：`canonicalize` 会把链接换成它的目标，
/// 调用方随后拿到的就是目标路径，删除时删掉的是链接指向的真实数据（同根目录下的
/// 备份/同步副本很容易形成这种链接）。这里改为只解析链接的父目录并保留链接文件名，
/// 让删除落在链接本身上—— [`remove_path_if_exists`] 会摘掉链接而不跟随。
pub fn checked_storage_child(root: &Path, child: &Path) -> Result<PathBuf, String> {
    let canonical_root = root.canonicalize().map_err(|e| e.to_string())?;
    if child
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err("Storage path contains a parent-directory component".into());
    }
    if child == root || child == canonical_root {
        return Err("Derived storage path escapes its root".into());
    }

    verify_no_intermediate_links(root, &canonical_root, child)?;

    // 目标本身是链接/重解析点：解析它的父目录，但保留链接自身的文件名
    if std::fs::symlink_metadata(child).is_ok_and(|meta| is_reparse_or_symlink(&meta)) {
        let parent = child
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or("Invalid storage path")?;
        let name = child.file_name().ok_or("Invalid storage path")?;
        let resolved = resolve_existing_ancestors(parent)?;
        let resolved = resolved.join(name);
        if resolved == canonical_root || !resolved.starts_with(&canonical_root) {
            return Err("Derived storage path escapes its root".into());
        }
        return Ok(resolved);
    }

    let resolved = resolve_existing_ancestors(child)?;
    if resolved == canonical_root || !resolved.starts_with(&canonical_root) {
        return Err("Derived storage path escapes its root".into());
    }
    Ok(resolved)
}

/// Canonicalize the nearest existing ancestor of `path` and re-append the missing
/// suffix as literal components (no symlink resolution for the missing part).
fn resolve_existing_ancestors(path: &Path) -> Result<PathBuf, String> {
    let mut existing = path;
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
    Ok(resolved)
}

/// 删除一个可能存在也可能不存在的路径。
///
/// 对符号链接/重解析点只摘掉链接本身，绝不跟随：跟随会把「删除一个链接」变成
/// 「删除链接指向的真实数据」。绝不调用 `path.is_dir()`（它会跟随链接）；Windows
/// 上目录链接与目录 junction 均根据自身的元数据属性用 `remove_dir` 摘除。
pub fn remove_path_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if is_reparse_or_symlink(&meta) => {
            #[cfg(windows)]
            {
                use std::os::windows::fs::{FileTypeExt, MetadataExt};
                if meta.file_type().is_symlink_dir() || (meta.file_attributes() & 0x10 != 0) {
                    std::fs::remove_dir(path)
                } else {
                    std::fs::remove_file(path)
                }
            }
            #[cfg(not(windows))]
            {
                std::fs::remove_file(path)
            }
        }
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
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

/// 仅接受真实 UUID/雪花 ID 会出现的字符，杜绝路径穿越与控制字符借道会话 JSON 或文件名混入删除/SQLite 引用。
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

/// Strict dependency discovery: only a missing optional root is an empty tree.
/// Do not publish a partial file set when any directory/entry cannot be inspected.
pub fn collect_files_where_checked<F>(
    root: &Path,
    predicate: F,
    files: &mut Vec<PathBuf>,
) -> Result<(), String>
where
    F: Fn(&Path) -> bool,
{
    fn walk<F>(root: &Path, predicate: &F, files: &mut Vec<PathBuf>) -> Result<(), String>
    where
        F: Fn(&Path) -> bool + ?Sized,
    {
        let entries = std::fs::read_dir(root)
            .map_err(|error| format!("Failed to scan {}: {error}", root.display()))?;
        for entry in entries {
            let entry = entry
                .map_err(|error| format!("Failed to enumerate {}: {error}", root.display()))?;
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|error| format!("Failed to inspect {}: {error}", path.display()))?;
            // Links/reparse points may hide dependent sessions. Never skip them
            // silently or follow them outside the store during deletion checks.
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|error| format!("Failed to inspect {}: {error}", path.display()))?;
            reject_scan_link(&path, &metadata)?;
            if kind.is_dir() {
                walk(&path, predicate, files)?;
            } else if kind.is_file() && predicate(&path) {
                files.push(path);
            }
        }
        Ok(())
    }
    match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("Failed to inspect {}: {error}", root.display())),
        Ok(metadata) => reject_scan_link(root, &metadata)?,
    }
    let mut discovered = Vec::new();
    walk(root, &predicate, &mut discovered)?;
    files.extend(discovered);
    Ok(())
}

fn reject_scan_link(path: &Path, metadata: &std::fs::Metadata) -> Result<(), String> {
    if is_reparse_or_symlink(metadata) {
        return Err(format!(
            "Dependency scan encountered a link: {}",
            path.display()
        ));
    }
    Ok(())
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

    // Walk backwards in bounded blocks, assembling only requested complete
    // records. Decode after finding the boundary so split UTF-8 is harmless.
    let mut tail = Vec::new();
    if tail_n == 0 {
        return Ok((head, tail));
    }
    let mut file2 = File::open(path)?;
    let mut position = file_len;
    let mut reversed_line = Vec::new();
    let mut block = [0u8; 16_384];
    let mut line_bytes = 0u64;
    let mut line_terminated = false;
    while position > 0 && tail.len() < tail_n {
        let count = position.min(block.len() as u64) as usize;
        position -= count as u64;
        file2.seek(SeekFrom::Start(position))?;
        file2.read_exact(&mut block[..count])?;
        for index in (0..count).rev() {
            let byte = block[index];
            if byte == b'\n' {
                if position + index as u64 != file_len - 1 {
                    reversed_line.reverse();
                    if line_terminated && reversed_line.last() == Some(&b'\r') {
                        reversed_line.pop();
                    }
                    tail.push(
                        String::from_utf8(std::mem::take(&mut reversed_line))
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
                    );
                    if tail.len() == tail_n {
                        break;
                    }
                }
                line_bytes = 1; // Include the delimiter in the per-record budget.
                line_terminated = true;
            } else {
                line_bytes += 1;
                if line_bytes > MAX_SCAN_LINE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Session metadata line exceeds scan limit",
                    ));
                }
                reversed_line.push(byte);
            }
        }
    }
    if position == 0 && tail.len() < tail_n && line_bytes > 0 {
        reversed_line.reverse();
        if line_terminated && reversed_line.last() == Some(&b'\r') {
            reversed_line.pop();
        }
        tail.push(
            String::from_utf8(reversed_line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        );
    }
    tail.reverse();

    Ok((head, tail))
}

pub fn parse_timestamp_to_ms(value: &Value) -> Option<i64> {
    // Integer: milliseconds (|n| > 1e12) or seconds
    if let Some(n) = value.as_i64() {
        return if n.unsigned_abs() > 1_000_000_000_000 {
            Some(n)
        } else {
            n.checked_mul(1000)
        };
    }
    if let Some(n) = value.as_f64() {
        if !n.is_finite() || !(-9e15..=9e15).contains(&n) {
            return None;
        }
        let n = n as i64;
        return if n.unsigned_abs() > 1_000_000_000_000 {
            Some(n)
        } else {
            n.checked_mul(1000)
        };
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
        Value::Object(_) => extract_text_from_item(content).unwrap_or_default(),
        _ => String::new(),
    }
}

#[allow(dead_code)]
pub fn extract_thinking_text(content: &Value) -> String {
    match content {
        Value::Array(items) => items
            .iter()
            .filter_map(extract_thinking_from_item)
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => extract_thinking_from_item(content)
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn extract_thinking_from_item(item: &Value) -> Option<String> {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    match item_type {
        "thinking" => item
            .get("thinking")
            .or_else(|| item.get("text"))
            .and_then(Value::as_str)
            .map(str::to_string),
        "summary_text" | "reasoning_text" => {
            item.get("text").and_then(Value::as_str).map(str::to_string)
        }
        "reasoning" => {
            if let Some(text) = item
                .get("text")
                .or_else(|| item.get("thinking"))
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                return Some(text.to_string());
            }
            let mut parts = Vec::new();
            if let Some(summary) = item.get("summary") {
                let s = extract_thinking_text(summary);
                if !s.trim().is_empty() {
                    parts.push(s);
                }
            }
            if let Some(nested) = item.get("content") {
                let c = extract_thinking_text(nested);
                if !c.trim().is_empty() {
                    parts.push(c);
                }
            }
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        _ => None,
    }
}

fn format_web_search_tool_result(item: &Value) -> String {
    if let Some(err) = item
        .get("error")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return format!("[Web search error: {err}]");
    }
    match item.get("content") {
        Some(Value::Array(results)) => {
            let mut lines = Vec::new();
            for entry in results {
                let title = entry
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                let url = entry
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                match (!title.is_empty(), !url.is_empty()) {
                    (true, true) => lines.push(format!("- {title} ({url})")),
                    (true, false) => lines.push(format!("- {title}")),
                    (false, true) => lines.push(format!("- {url}")),
                    (false, false) => {}
                }
            }
            if lines.is_empty() {
                "[Web search result]".to_string()
            } else {
                lines.join("\n")
            }
        }
        Some(Value::Object(map)) => {
            if let Some(err) = map
                .get("error_code")
                .or_else(|| map.get("error"))
                .or_else(|| map.get("message"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                format!("[Web search error: {err}]")
            } else {
                "[Web search result]".to_string()
            }
        }
        _ => "[Web search result]".to_string(),
    }
}

fn extract_text_from_item(item: &Value) -> Option<String> {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");

    // Keep thinking/reasoning blocks strictly out of visible text / SessionMeta summaries.
    if matches!(
        item_type,
        "thinking" | "reasoning" | "redacted_thinking" | "summary_text" | "reasoning_text"
    ) {
        return None;
    }

    // Anthropic uses tool_use / server_tool_use / mcp_tool_use; Pi's assistant messages use toolCall.
    if matches!(
        item_type,
        "tool_use" | "toolCall" | "server_tool_use" | "mcp_tool_use"
    ) {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Some(format!("[Tool: {name}]"));
    }

    if item_type == "web_search_tool_result" {
        return Some(format_web_search_tool_result(item));
    }

    if matches!(item_type, "image" | "input_image") {
        return Some("[Image]".to_string());
    }

    if matches!(item_type, "document" | "input_file") {
        return Some("[Document]".to_string());
    }

    // tool_result / mcp_tool_result: extract nested content
    if matches!(item_type, "tool_result" | "mcp_tool_result") {
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
    if normalized.is_empty() {
        return trimmed.starts_with('/').then(|| "/".to_string());
    }
    let last = normalized
        .split(['/', '\\'])
        .next_back()
        .filter(|segment| !segment.is_empty())?;
    Some(last.to_string())
}

/// 会话文件的扫描指纹：修改时间 + 长度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub modified: Option<SystemTime>,
    pub len: u64,
}

pub fn file_stamp(path: &Path) -> io::Result<FileStamp> {
    let metadata = std::fs::metadata(path)?;
    Ok(FileStamp {
        modified: metadata.modified().ok(),
        len: metadata.len(),
    })
}

#[derive(Clone)]
struct CachedParse<T> {
    stamp: FileStamp,
    meta: Option<T>,
}

/// 会话文件解析结果的缓存：按文件的修改时间和大小判断有没有变，没变就复用上次
/// 解析出的 [`SessionMeta`](crate::session_manager::SessionMeta) 或原始结构体，只重新解析新增或
/// 改过的文件。
///
/// 只存摘要（标题、路径、时间等），不存会话正文；已删除文件的条目自动淘汰，内存占用
/// 与会话数量成正比。解析结果必须只取决于文件本身——依赖外部数据的部分（DeepSeek
/// Harness 的 `workspace.json` 项目归属、Codex 的 `config.toml` 分区、Pi 的父会话文件
/// 存在性）在缓存之外叠加，否则会出现「文件没变但结论已过期」的陈旧行。
pub struct FileParseCache<T = crate::session_manager::SessionMeta> {
    entries: Mutex<HashMap<PathBuf, CachedParse<T>>>,
}

impl<T: Clone> FileParseCache<T> {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// 按顺序解析 `files`，没变过的文件直接用缓存。返回能解析出会话的那些。
    ///
    /// `parse` 读失败（权限、被占用等）必须返回 `Err`：这类结果不进缓存，下一轮
    /// 重试。若把读失败当成 `Ok(None)`，文件恢复可读后因为「文件没变」会一直命中
    /// 「没有会话」，直到文件被改动或进程重启。
    pub fn scan<F>(&self, files: Vec<PathBuf>, parse: F) -> Vec<T>
    where
        F: Fn(&Path) -> Result<Option<T>, String>,
    {
        // 短暂加锁复制当前快照：并发扫描都能命中已有缓存，锁中毒时继续恢复内部表
        let previous = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();

        let mut scanned = HashMap::with_capacity(files.len());
        let mut evict = Vec::new();
        let mut sessions = Vec::new();
        for path in files {
            // 元数据都读不到（权限、被占用、刚被删除）：本轮跳过并移出缓存，下轮重试
            let Ok(stamp) = file_stamp(&path) else {
                evict.push(path);
                continue;
            };
            let meta = match previous.get(&path) {
                Some(entry) if entry.stamp == stamp => entry.meta.clone(),
                _ => match parse(&path) {
                    Ok(meta) => meta,
                    Err(error) => {
                        log::debug!("会话文件暂时读取失败，下轮重试 {}: {error}", path.display());
                        evict.push(path);
                        continue;
                    }
                },
            };
            if let Some(meta) = &meta {
                sessions.push(meta.clone());
            }
            scanned.insert(path, CachedParse { stamp, meta });
        }

        let mut guard = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for path in evict {
            guard.remove(&path);
        }
        guard.retain(|path, _| scanned.contains_key(path) || path.exists());
        guard.extend(scanned);
        sessions
    }
}

impl<T: Clone> Default for FileParseCache<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Quote a SQLite identifier safely against injection/special characters.
pub fn quote_sqlite_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// Return all table names in a SQLite database or transaction (`Transaction` derefs to `Connection`).
pub fn sqlite_table_names(connection: &rusqlite::Connection) -> Result<Vec<String>, String> {
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='table'")
        .map_err(|error| error.to_string())?;
    let names = statement
        .query_map([], |row| row.get(0))
        .map_err(|error| error.to_string())?
        .flatten()
        .collect();
    Ok(names)
}

/// Check if a table in a SQLite database or transaction contains a specific column.
pub fn sqlite_table_has_column(
    connection: &rusqlite::Connection,
    table: &str,
    column: &str,
) -> Result<bool, String> {
    let quoted = quote_sqlite_identifier(table);
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({quoted})"))
        .map_err(|error| error.to_string())?;
    let found = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| error.to_string())?
        .flatten()
        .any(|name| name == column);
    Ok(found)
}

/// Validate a child -> parent mapping for cycles using O(V) memoized traversal.
pub fn validate_parent_graph(
    parents: &HashMap<String, String>,
    provider_label: &str,
) -> Result<(), String> {
    let mut completed = HashSet::new();
    for child in parents.keys() {
        let mut seen = HashSet::new();
        let mut current = child.as_str();
        while let Some(parent) = parents.get(current) {
            if completed.contains(current) {
                break;
            }
            if !seen.insert(current) {
                return Err(format!("{provider_label} 会话父子关系存在循环，已停止删除"));
            }
            current = parent.as_str();
        }
        completed.extend(seen);
    }
    Ok(())
}

/// Stable Kahn topological ordering: places selected children before their selected parents,
/// while preserving the caller's relative order for all other items.
pub fn order_selected_children(
    selected: &[(usize, String, Option<String>)],
    initial: &[usize],
) -> Vec<usize> {
    let positions = initial
        .iter()
        .enumerate()
        .map(|(position, index)| (*index, position))
        .collect::<HashMap<_, _>>();
    let mut by_id: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, id, _) in selected {
        if let Some(position) = positions.get(index) {
            by_id.entry(id.as_str()).or_default().push(*position);
        }
    }
    let mut pending_children = vec![0usize; initial.len()];
    let mut parents = vec![Vec::new(); initial.len()];
    for (index, _, parent) in selected {
        let Some(&child) = positions.get(index) else {
            continue;
        };
        if let Some(parent_positions) = parent.as_deref().and_then(|id| by_id.get(id)) {
            for &parent in parent_positions {
                pending_children[parent] += 1;
                parents[child].push(parent);
            }
        }
    }
    let mut ready = pending_children
        .iter()
        .enumerate()
        .filter_map(|(position, count)| (*count == 0).then_some(position))
        .collect::<BTreeSet<_>>();
    let mut ordered = Vec::with_capacity(initial.len());
    let mut emitted = vec![false; initial.len()];
    while let Some(position) = ready.pop_first() {
        ordered.push(initial[position]);
        emitted[position] = true;
        for &parent in &parents[position] {
            pending_children[parent] -= 1;
            if pending_children[parent] == 0 {
                ready.insert(parent);
            }
        }
    }
    ordered.extend(
        initial
            .iter()
            .enumerate()
            .filter_map(|(position, index)| (!emitted[position]).then_some(*index)),
    );
    ordered
}

/// Atomically serialize a JSON value to `path` via a sibling temporary file + `sync_all`.
pub fn atomic_write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<(), String> {
    let parent = path.parent().ok_or("Invalid target file path")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut temp, value).map_err(|e| e.to_string())?;
    temp.write_all(b"\n").map_err(|e| e.to_string())?;
    temp.flush().map_err(|e| e.to_string())?;
    temp.as_file().sync_all().map_err(|e| e.to_string())?;
    #[cfg(windows)]
    {
        let mut current_temp = temp;
        let mut attempts = 0usize;
        loop {
            match current_temp.persist(path) {
                Ok(_) => break,
                Err(err) if err.error.kind() == io::ErrorKind::PermissionDenied && attempts < 3 => {
                    attempts += 1;
                    current_temp = err.file;
                    std::thread::sleep(std::time::Duration::from_millis(15));
                }
                Err(err) => return Err(err.error.to_string()),
            }
        }
    }
    #[cfg(not(windows))]
    {
        temp.persist(path).map_err(|e| e.error.to_string())?;
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// 一行 JSONL 在文件内的字节区间（含行尾 `\n`），供生成 [`crate::session_manager::model::ContentRef::Jsonl`] 使用。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JsonlSpan {
    pub offset: u64,
    pub len: u32,
    pub segment: Option<u16>,
}

impl JsonlSpan {
    pub fn content_ref(
        &self,
        pointer: impl Into<String>,
    ) -> crate::session_manager::model::ContentRef {
        crate::session_manager::model::ContentRef::Jsonl {
            offset: self.offset,
            len: self.len,
            pointer: pointer.into(),
            segment: self.segment,
        }
    }
}

pub struct LineSpan<'a> {
    pub span: JsonlSpan,
    pub bytes: &'a [u8],
}

/// 逐行读取并累计字节偏移，供需要按需回取原文的 JSONL 解析器共用。
pub struct LineSpans<R> {
    reader: R,
    buf: Vec<u8>,
    offset: u64,
    segment: Option<u16>,
}

impl<R: BufRead> LineSpans<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buf: Vec::with_capacity(64 * 1024),
            offset: 0,
            segment: None,
        }
    }

    pub fn with_segment(reader: R, segment: Option<u16>) -> Self {
        Self {
            reader,
            buf: Vec::with_capacity(64 * 1024),
            offset: 0,
            segment,
        }
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn next_line(&mut self) -> io::Result<Option<LineSpan<'_>>> {
        self.buf.clear();
        let read = (&mut self.reader)
            .take(MAX_SCAN_LINE_BYTES + 1)
            .read_until(b'\n', &mut self.buf)?;
        if read == 0 {
            return Ok(None);
        }
        if read as u64 > MAX_SCAN_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Session JSONL record exceeds 4 MiB line budget",
            ));
        }
        let offset = self.offset;
        self.offset += read as u64;

        let mut end = self.buf.len();
        if end > 0 && self.buf[end - 1] == b'\n' {
            end -= 1;
            if end > 0 && self.buf[end - 1] == b'\r' {
                end -= 1;
            }
        }
        Ok(Some(LineSpan {
            span: JsonlSpan {
                offset,
                len: u32::try_from(read).unwrap_or(u32::MAX),
                segment: self.segment,
            },
            bytes: &self.buf[..end],
        }))
    }
}

/// 逐行读取 JSONL，跳过空行与损坏行，回调接收 `(字节区间, Value)`。
pub fn for_each_jsonl_value(
    path: &Path,
    mut f: impl FnMut(JsonlSpan, Value) -> Result<(), String>,
) -> Result<(), String> {
    ensure_readable_size(path)?;
    let file = File::open(path).map_err(|e| format!("Failed to open session file: {e}"))?;
    let mut lines = LineSpans::new(BufReader::new(file));
    while let Some(line) = lines
        .next_line()
        .map_err(|e| format!("Failed to read session file: {e}"))?
    {
        let bytes = line.bytes.trim_ascii();
        if bytes.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
            continue;
        };
        f(line.span, value)?;
    }
    Ok(())
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

    /// 目标本身是符号链接时，`checked_storage_child` 必须返回**链接自身**的路径，
    /// 而不是它指向的目标：否则调用方随后删除的就是链接目标里的真实数据。
    #[test]
    fn a_symlinked_child_is_returned_as_the_link_not_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("storage");
        let target = root.join("real-data");
        let link = root.join("link-to-data");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep.txt"), "keep").unwrap();

        if create_dir_symlink(&target, &link).is_err() {
            eprintln!("skipped: 本机不允许创建目录符号链接");
            return;
        }

        // 返回的必须是链接路径本身（Windows 上 canonicalize 会带 \\?\ 前缀，
        // 所以比文件名与链接属性，而不是比字面路径）
        let resolved = checked_storage_child(&root, &link).expect("link stays inside root");
        assert_eq!(resolved.file_name(), link.file_name());
        assert!(
            std::fs::symlink_metadata(&resolved)
                .expect("resolved exists")
                .file_type()
                .is_symlink(),
            "返回路径必须仍是链接；若返回目标路径，删除会落到链接目标上"
        );

        remove_path_if_exists(&resolved).expect("remove link");
        assert!(std::fs::symlink_metadata(&link).is_err(), "链接本身被摘掉");
        assert!(target.join("keep.txt").exists(), "链接目标的内容不能被动到");
    }

    /// 链接指向根目录之外时：路径校验按词法通过（链接自身在根内），
    /// 但删除只摘掉链接本身，绝不跟随到根外的目标。
    #[test]
    fn a_symlink_pointing_outside_the_root_is_not_followed_on_delete() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("storage");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let link = root.join("escape");

        if create_dir_symlink(&outside, &link).is_err() {
            eprintln!("skipped: 本机不允许创建目录符号链接");
            return;
        }

        // 链接自身在 root 内，所以路径校验通过；但删除只会摘掉链接本身
        let resolved = checked_storage_child(&root, &link).expect("lexically inside root");
        assert_eq!(resolved.file_name(), link.file_name());
        remove_path_if_exists(&resolved).expect("remove link");
        assert!(outside.exists(), "根目录之外的链接目标必须完好无损");
        assert!(
            std::fs::symlink_metadata(&link).is_err(),
            "链接本身应被摘掉，而不是跟随到目标"
        );
    }

    #[cfg(unix)]
    fn create_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
    }

    #[cfg(not(any(unix, windows)))]
    fn create_dir_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "unsupported platform",
        ))
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
        std::fs::write(&path, &text).unwrap();
        let (_, tail) = read_head_tail_lines(&path, 1, 30).unwrap();
        assert_eq!(tail, text.lines().collect::<Vec<_>>());
    }

    #[test]
    fn regression_checked_collection_allows_missing_root_but_rejects_file_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let mut files = vec![PathBuf::from("existing")];
        collect_files_where_checked(&root, |_| true, &mut files).unwrap();
        std::fs::write(&root, "not a directory").unwrap();
        assert!(collect_files_where_checked(&root, |_| true, &mut files).is_err());
        assert_eq!(files, vec![PathBuf::from("existing")]);
    }

    #[cfg(unix)]
    #[test]
    fn regression_checked_collection_reports_unreadable_child_directory() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("blocked");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("dependent.json"), "{}").unwrap();
        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o000)).unwrap();
        let actually_denied = std::fs::read_dir(&child).is_err();
        let mut files = Vec::new();
        let result = collect_files_where_checked(temp.path(), |_| true, &mut files);
        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
        if actually_denied {
            assert!(result.is_err());
            assert!(files.is_empty());
        } else {
            eprintln!("skipped permission assertion: current user bypasses mode bits");
        }
    }

    #[test]
    fn regression_checked_collection_rejects_hidden_link_dependencies_without_partial_output() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let outside = temp.path().join("other");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("parent.json"), "{}").unwrap();
        std::fs::write(outside.join("child.json"), "{}").unwrap();
        if create_dir_symlink(&outside, &root.join("linked")).is_err() {
            eprintln!("skipped: directory symlink creation unavailable");
            return;
        }
        let mut files = vec![PathBuf::from("existing")];
        assert!(collect_files_where_checked(&root, |_| true, &mut files).is_err());
        assert_eq!(files, vec![PathBuf::from("existing")]);
    }

    #[test]
    fn regression_tail_reads_long_final_records_with_and_without_newline() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tail.jsonl");
        for content in ["x".repeat(20_000), "中🦀".repeat(5000)] {
            for ending in ["", "\n", "\r\n"] {
                std::fs::write(&path, format!("head\n{content}{ending}")).unwrap();
                let (_, tail) = read_head_tail_lines(&path, 1, 1).unwrap();
                assert_eq!(tail, vec![content.clone()]);
            }
        }
    }

    #[test]
    fn regression_tail_enforces_four_mib_record_limit_without_reading_earlier_body() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tail.jsonl");
        // An oversized earlier line is not read when only the final record is requested.
        std::fs::write(
            &path,
            format!("{}\nlast\n", "x".repeat(MAX_SCAN_LINE_BYTES as usize + 1)),
        )
        .unwrap();
        assert_eq!(read_head_tail_lines(&path, 0, 1).unwrap().1, vec!["last"]);
        for ending in ["", "\n"] {
            let exact = "x".repeat(MAX_SCAN_LINE_BYTES as usize - ending.len());
            std::fs::write(&path, format!("head\n{exact}{ending}")).unwrap();
            assert_eq!(read_head_tail_lines(&path, 1, 1).unwrap().1, vec![exact]);
            std::fs::write(
                &path,
                format!(
                    "head\n{}{ending}",
                    "x".repeat(MAX_SCAN_LINE_BYTES as usize + 1)
                ),
            )
            .unwrap();
            assert_eq!(
                read_head_tail_lines(&path, 1, 1).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
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

    fn cache_test_meta(path: &Path, id: &str) -> Option<crate::session_manager::SessionMeta> {
        Some(crate::session_manager::SessionMeta {
            provider_id: "test".to_string(),
            session_id: id.to_string(),
            residual: false,
            parent_session_id: None,
            archived: false,
            cleanup_pending: false,
            sidebar_section: None,
            title: None,
            summary: None,
            project_dir: None,
            project_name: None,
            created_at: None,
            last_active_at: None,
            source_path: Some(path.to_string_lossy().to_string()),
        })
    }

    /// 读取失败不能缓存成「没有会话」：恢复读取后，文件没变也要重新解析出来
    #[test]
    fn file_parse_cache_retries_files_that_failed_to_parse() {
        use std::cell::Cell;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("a.jsonl");
        std::fs::write(&path, "alpha").unwrap();

        let cache = FileParseCache::new();
        let fail = Cell::new(true);
        let calls = Cell::new(0);
        let parse = |path: &Path| {
            calls.set(calls.get() + 1);
            if fail.get() {
                Err("denied".to_string())
            } else {
                Ok(cache_test_meta(path, "alpha"))
            }
        };

        assert!(cache.scan(vec![path.clone()], parse).is_empty());
        assert!(cache.scan(vec![path.clone()], parse).is_empty());
        assert_eq!(calls.get(), 2, "失败的文件每轮都要重试，不能命中缓存");

        fail.set(false);
        let sessions = cache.scan(vec![path.clone()], parse);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "alpha");
        assert_eq!(calls.get(), 3);

        // 成功后才进缓存：再扫不重新解析
        assert_eq!(cache.scan(vec![path], parse).len(), 1);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn file_parse_cache_reparses_only_changed_files() {
        use std::cell::Cell;

        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a.jsonl");
        let b = temp.path().join("b.jsonl");
        let c = temp.path().join("c.jsonl");
        std::fs::write(&a, "alpha").unwrap();
        std::fs::write(&b, "beta").unwrap();
        std::fs::write(&c, "skip").unwrap();

        let cache = FileParseCache::new();
        let calls = Cell::new(0);
        let parse = |path: &Path| {
            calls.set(calls.get() + 1);
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default();
            // 「解析不出会话」也必须进缓存：内容为 skip 的文件返回 Ok(None)，
            // 下次扫描不再重复解析它
            if std::fs::read_to_string(path).unwrap_or_default().trim() == "skip" {
                return Ok(None);
            }
            Ok(cache_test_meta(path, id))
        };
        let ids = |sessions: Vec<crate::session_manager::SessionMeta>| -> Vec<String> {
            sessions.into_iter().map(|meta| meta.session_id).collect()
        };

        // 第一次全部解析；解析不出会话的文件也记下来，下次不重复解析
        let files = vec![a.clone(), b.clone(), c.clone()];
        assert_eq!(ids(cache.scan(files.clone(), parse)), ["a", "b"]);
        assert_eq!(calls.get(), 3);

        // 没有改动：一个都不重新解析
        assert_eq!(ids(cache.scan(files.clone(), parse)), ["a", "b"]);
        assert_eq!(calls.get(), 3);

        // 改了内容（长度变了）：只重新解析这一个
        std::fs::write(&b, "beta-2").unwrap();
        assert_eq!(ids(cache.scan(files.clone(), parse)), ["a", "b"]);
        assert_eq!(calls.get(), 4);

        // 文件删掉：不再出现，条目也被丢弃；重新出现时要重新解析
        std::fs::remove_file(&a).unwrap();
        assert_eq!(ids(cache.scan(files.clone(), parse)), ["b"]);
        assert_eq!(calls.get(), 4);
        std::fs::write(&a, "alpha").unwrap();
        assert_eq!(ids(cache.scan(files, parse)), ["a", "b"]);
        assert_eq!(calls.get(), 5);
    }

    #[test]
    fn validate_parent_graph_accepts_acyclic_chains_and_rejects_cycles() {
        let acyclic = HashMap::from([
            ("grandchild".to_string(), "child".to_string()),
            ("child".to_string(), "parent".to_string()),
        ]);
        assert!(validate_parent_graph(&acyclic, "Test").is_ok());

        let cyclic = HashMap::from([
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "a".to_string()),
        ]);
        let err = validate_parent_graph(&cyclic, "Test").unwrap_err();
        assert!(err.contains("Test 会话父子关系存在循环"));
    }

    #[test]
    fn order_selected_children_puts_children_before_parents_preserving_unrelated_order() {
        let selected = vec![
            (0, "parent".to_string(), None),
            (2, "child".to_string(), Some("parent".to_string())),
            (3, "grandchild".to_string(), Some("child".to_string())),
        ];
        // Initial order: [0 (parent), 1 (unrelated), 2 (child), 3 (grandchild)]
        // Unrelated (1) stays ahead of items blocked by children; chain resolves grandchild -> child -> parent.
        assert_eq!(
            order_selected_children(&selected, &[0, 1, 2, 3]),
            vec![1, 3, 2, 0]
        );
    }

    #[test]
    fn intermediate_symlink_inside_root_is_rejected_by_checked_storage_child() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("storage");
        let real_dir = root.join("real");
        let linked_dir = root.join("linked");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::write(real_dir.join("victim.txt"), "keep").unwrap();

        if create_dir_symlink(&real_dir, &linked_dir).is_err() {
            eprintln!("skipped: directory symlink creation unavailable");
            return;
        }

        assert!(checked_storage_child(&root, &linked_dir.join("victim.txt")).is_err());
        assert!(checked_storage_child(&root, &linked_dir.join("missing.txt")).is_err());
        // Direct link child itself is still accepted as the link (not its target)
        assert!(checked_storage_child(&root, &linked_dir).is_ok());
    }

    #[test]
    fn parse_timestamp_to_ms_never_overflows_on_extreme_numbers() {
        assert_eq!(parse_timestamp_to_ms(&json!(i64::MIN)), Some(i64::MIN));
        assert_eq!(parse_timestamp_to_ms(&json!(i64::MAX)), Some(i64::MAX));
        assert_eq!(parse_timestamp_to_ms(&json!(u64::MAX)), None);
        assert_eq!(parse_timestamp_to_ms(&json!(1e100)), None);
        assert_eq!(parse_timestamp_to_ms(&json!(-1e100)), None);
        assert_eq!(parse_timestamp_to_ms(&json!(-5_i64)), Some(-5_000));
    }

    #[test]
    fn extract_text_and_thinking_handle_rich_blocks_cleanly() {
        let content = json!([
            { "type": "thinking", "thinking": "step 1" },
            { "type": "reasoning", "text": "step 2" },
            { "type": "server_tool_use", "name": "web_search" },
            { "type": "mcp_tool_use", "name": "github_pr" },
            {
                "type": "web_search_tool_result",
                "content": [
                    { "type": "web_search_result", "title": "Rust", "url": "https://rust-lang.org" }
                ]
            },
            { "type": "image" },
            { "type": "input_image" },
            { "type": "document" },
            { "type": "input_file" },
            { "type": "text", "text": "Final answer" }
        ]);
        assert_eq!(
            extract_text(&content),
            "[Tool: web_search]\n[Tool: github_pr]\n- Rust (https://rust-lang.org)\n[Image]\n[Image]\n[Document]\n[Document]\nFinal answer"
        );
        assert_eq!(extract_thinking_text(&content), "step 1\nstep 2");
        assert_eq!(
            extract_thinking_text(&json!({
                "type": "reasoning",
                "summary": [{ "type": "summary_text", "text": "summary line" }]
            })),
            "summary line"
        );
    }

    #[test]
    fn file_parse_cache_retains_entries_during_concurrent_scan() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a.jsonl");
        let b = temp.path().join("b.jsonl");
        std::fs::write(&a, "alpha").unwrap();
        std::fs::write(&b, "beta").unwrap();

        let cache = FileParseCache::new();
        // Seed `a` into cache
        assert_eq!(
            cache
                .scan(vec![a.clone()], |p| Ok(cache_test_meta(p, "a")))
                .len(),
            1
        );

        let a_parses = AtomicUsize::new(0);
        // While scanning `[a, b]`, trigger a nested concurrent scan of `[a]` inside `parse(b)`.
        // Because `scan` clones under a brief lock instead of `mem::take`, the inner scan hits `a`!
        let outer = cache.scan(vec![a.clone(), b.clone()], |p| {
            if p == a {
                a_parses.fetch_add(1, Ordering::SeqCst);
                Ok(cache_test_meta(p, "a"))
            } else {
                let inner = cache.scan(vec![a.clone()], |inner_p| {
                    a_parses.fetch_add(1, Ordering::SeqCst);
                    Ok(cache_test_meta(inner_p, "a"))
                });
                assert_eq!(inner.len(), 1);
                Ok(cache_test_meta(p, "b"))
            }
        });
        assert_eq!(outer.len(), 2);
        assert_eq!(a_parses.load(Ordering::SeqCst), 0);
    }
}
