//! 会话源安全校验与大内容 / 图片按需读取（零额外第三方依赖）。
//!
//! 前端回传的 `sourcePath`、[`ContentRef`]、[`ImageRef`] 均视为不可信输入：
//! - [`validate_source`]：校验文件源必须位于对应 Provider 配置存储根目录内（含重解析点与符号链接防御），
//!   SQLite 源必须匹配对应 Provider 的合法数据库与安全 `session_id`；
//! - [`resolve_content_ref`]：按白名单校验并定位 JSONL 行区间 / SQLite 单元格 / 存储文件 / 附属输出文件，单次上限 32 MiB；
//! - [`load_image_data_url`]：校验图片归属与位图格式（拒绝 SVG），上限 20 MiB，返回 `data:<mime>;base64,...` 兼容严格 CSP。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::Value;

use super::model::{ContentRef, ImageRef, ImageSource};
use super::{canonicalize_existing_path, provider_roots, validated_file_source};

pub const MAX_TEXT_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
pub const MAX_PAGE_CHARS: u32 = 512 * 1024;

const STALE: &str = "会话已更新，请重新读取";
const NOT_TEXT: &str = "引用的内容不是文本";
const OUTSIDE: &str = "引用的路径不在会话允许目录内";
const TOO_LARGE_TEXT: &str = "内容超过 32MB，请直接打开源文件查看";
const TOO_LARGE_IMAGE: &str = "图片超过 20MB，无法预览";

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];
pub(crate) const GROK_CHAT_HISTORY: &str = "chat_history.jsonl";

#[derive(Debug, Clone)]
pub struct ValidatedSource {
    pub provider_id: String,
    pub raw: String,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceLocation {
    Path { path: PathBuf, root: PathBuf },
    Sqlite { db: PathBuf, session_id: String },
}

impl ValidatedSource {
    fn sidecar_base(&self) -> Option<PathBuf> {
        match &self.location {
            SourceLocation::Path { path, .. } if path.is_file() => Some(path.with_extension("")),
            SourceLocation::Path { path, .. } => Some(path.clone()),
            SourceLocation::Sqlite { .. } => None,
        }
    }
}

/// 校验 `source_path` 归属并返回 [`ValidatedSource`]。
pub fn validate_source(provider_id: &str, source_path: &str) -> Result<ValidatedSource, String> {
    let location = if source_path.starts_with("sqlite-zcode:") {
        if provider_id != "zcode" {
            return Err("会话来源与 Agent 不匹配".to_string());
        }
        let (db, session_id) =
            super::providers::zcode::validate_sqlite_source_for_content(source_path)?;
        SourceLocation::Sqlite { db, session_id }
    } else if source_path.starts_with("sqlite-workbuddy:") {
        if provider_id != "workbuddy" {
            return Err("会话来源与 Agent 不匹配".to_string());
        }
        let (root, path) =
            super::providers::workbuddy::resolve_sqlite_jsonl_for_content(source_path)?;
        SourceLocation::Path { path, root }
    } else if source_path.starts_with("sqlite:") {
        if provider_id != "opencode" {
            return Err("会话来源与 Agent 不匹配".to_string());
        }
        let (db, session_id) =
            super::providers::opencode::validate_sqlite_source_for_content(source_path)?;
        SourceLocation::Sqlite { db, session_id }
    } else {
        let canonical = validated_file_source(provider_id, source_path)?;
        let roots = provider_roots(provider_id)?;
        let root = roots
            .into_iter()
            .filter_map(|r| r.canonicalize().ok())
            .find(|r| canonical.starts_with(r))
            .ok_or_else(|| OUTSIDE.to_string())?;
        SourceLocation::Path {
            path: canonical,
            root,
        }
    };

    Ok(ValidatedSource {
        provider_id: provider_id.to_string(),
        raw: source_path.to_string(),
        location,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockContent {
    pub text: String,
    pub total_len: u32,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u32>,
}

pub fn paginate(full: &str, offset: Option<u32>, limit: Option<u32>) -> BlockContent {
    let offset = offset.unwrap_or(0) as usize;
    let limit = limit.unwrap_or(MAX_PAGE_CHARS).clamp(1, MAX_PAGE_CHARS) as usize;
    let total = full.chars().count();
    let text: String = full.chars().skip(offset).take(limit).collect();
    let end = offset.saturating_add(limit);
    let truncated = end < total;
    BlockContent {
        text,
        total_len: total.min(u32::MAX as usize) as u32,
        truncated,
        next_offset: truncated.then(|| end.min(u32::MAX as usize) as u32),
    }
}

pub fn resolve_content_ref(
    source: &ValidatedSource,
    content: &ContentRef,
) -> Result<String, String> {
    match content {
        ContentRef::Jsonl {
            offset,
            len,
            pointer,
            segment,
        } => {
            let path = jsonl_path(source, *segment)?;
            let line = read_jsonl_line(&source.provider_id, &path, *offset, *len)?;
            let value: Value = serde_json::from_slice(&line).map_err(|_| STALE.to_string())?;
            extract_text(&value, pointer)
        }
        ContentRef::Sqlite {
            table,
            id,
            column,
            pointer,
        } => {
            let SourceLocation::Sqlite { db, session_id } = &source.location else {
                return Err(STALE.to_string());
            };
            let raw = read_sqlite_cell(&source.provider_id, db, session_id, table, id, column)?;
            if pointer.is_empty() {
                return Ok(raw);
            }
            let value: Value = serde_json::from_str(&raw).map_err(|_| STALE.to_string())?;
            extract_text(&value, pointer)
        }
        ContentRef::File { rel_path, pointer } => {
            let SourceLocation::Path { root, .. } = &source.location else {
                return Err(OUTSIDE.to_string());
            };
            let file = safe_join(root, rel_path)?;
            let bytes = read_limited(&file, MAX_TEXT_BYTES, TOO_LARGE_TEXT)?;
            let value: Value = if source.provider_id == "gemini" {
                std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(super::providers::gemini::parse_session_document_for_content)
                    .ok_or_else(|| STALE.to_string())?
            } else {
                serde_json::from_slice(&bytes).map_err(|_| STALE.to_string())?
            };
            if source.provider_id == "gemini" && pointer.ends_with("/thoughts") {
                if let Some(thoughts) = value
                    .pointer(pointer)
                    .and_then(super::providers::gemini::format_thoughts_for_content)
                {
                    return Ok(thoughts);
                }
            }
            extract_text(&value, pointer)
        }
        ContentRef::Sidecar { rel_path } => {
            let base = source.sidecar_base().ok_or_else(|| OUTSIDE.to_string())?;
            let SourceLocation::Path { root, .. } = &source.location else {
                return Err(OUTSIDE.to_string());
            };
            let base = canonicalize_existing_path(&base, "sidecar directory")
                .map_err(|_| "附属输出目录不存在".to_string())?;
            if !base.starts_with(root) {
                return Err(OUTSIDE.to_string());
            }
            let file = safe_join(&base, rel_path)?;
            let bytes = read_limited(&file, MAX_TEXT_BYTES, TOO_LARGE_TEXT)?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        }
    }
}

fn jsonl_path(source: &ValidatedSource, segment: Option<u16>) -> Result<PathBuf, String> {
    let SourceLocation::Path { path, root } = &source.location else {
        return Err(STALE.to_string());
    };
    if source.provider_id == "codex" {
        if let Some(seg_idx) = segment {
            let chain = super::providers::codex::transcript_sources(path)?;
            let (seg_path, _) = chain
                .get(usize::from(seg_idx))
                .ok_or_else(|| STALE.to_string())?;
            let canonical = canonicalize_existing_path(seg_path, "codex segment")?;
            let roots = provider_roots("codex")?;
            if !roots
                .into_iter()
                .filter_map(|r| r.canonicalize().ok())
                .any(|r| canonical.starts_with(r))
            {
                return Err(OUTSIDE.to_string());
            }
            return Ok(canonical);
        }
    }
    if source.provider_id != "grokbuild" {
        return Ok(path.clone());
    }
    if path.file_name().and_then(|name| name.to_str()) != Some("summary.json") {
        return Err(OUTSIDE.to_string());
    }
    let chat = path
        .with_file_name(GROK_CHAT_HISTORY)
        .canonicalize()
        .map_err(|_| "会话正文文件不存在".to_string())?;
    if !chat.starts_with(root)
        || chat.file_name().and_then(|n| n.to_str()) != Some(GROK_CHAT_HISTORY)
    {
        return Err(OUTSIDE.to_string());
    }
    Ok(chat)
}

fn read_jsonl_line(
    provider_id: &str,
    path: &Path,
    offset: u64,
    len: u32,
) -> Result<Vec<u8>, String> {
    if len == 0 {
        return Err(STALE.to_string());
    }
    if u64::from(len) > MAX_TEXT_BYTES {
        return Err(TOO_LARGE_TEXT.to_string());
    }
    let end = offset
        .checked_add(u64::from(len))
        .ok_or_else(|| STALE.to_string())?;
    let start = offset.saturating_sub(1);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();

    let (buf, more_after_end) = if ext.eq_ignore_ascii_case("zst")
        || ext.eq_ignore_ascii_case("zstd")
        || (provider_id == "codex" && super::providers::codex::is_compressed_path(path))
    {
        read_zstd_window(path, start, end)?
    } else {
        read_plain_window(path, start, end)?
    };

    let line_start = (offset - start) as usize;
    if offset > 0 && buf[0] != b'\n' {
        return Err(STALE.to_string());
    }
    let mut line = &buf[line_start..line_start + len as usize];
    if line.last() == Some(&b'\n') {
        line = &line[..line.len() - 1];
    } else if more_after_end {
        let next = buf[buf.len() - 1];
        if next != b'\n' && next != b'\r' {
            return Err(STALE.to_string());
        }
    }
    if line.last() == Some(&b'\r') {
        line = &line[..line.len() - 1];
    }
    if line.is_empty() || line.contains(&b'\n') {
        return Err(STALE.to_string());
    }
    Ok(line.to_vec())
}

fn read_plain_window(path: &Path, start: u64, end: u64) -> Result<(Vec<u8>, bool), String> {
    let mut file = File::open(path).map_err(|_| "无法读取会话文件".to_string())?;
    let file_len = file
        .metadata()
        .map_err(|_| "无法读取会话文件".to_string())?
        .len();
    if end > file_len {
        return Err(STALE.to_string());
    }
    let read_end = (end + 1).min(file_len);
    let mut buf = vec![0u8; (read_end - start) as usize];
    file.seek(SeekFrom::Start(start))
        .and_then(|_| file.read_exact(&mut buf))
        .map_err(|_| STALE.to_string())?;
    Ok((buf, end < file_len))
}

fn read_zstd_window(path: &Path, start: u64, end: u64) -> Result<(Vec<u8>, bool), String> {
    let file = File::open(path).map_err(|_| "无法读取压缩会话文件".to_string())?;
    let mut decoder =
        zstd::stream::read::Decoder::new(file).map_err(|_| "无法解压会话文件".to_string())?;
    let skipped = std::io::copy(&mut (&mut decoder).take(start), &mut std::io::sink())
        .map_err(|_| STALE.to_string())?;
    if skipped < start {
        return Err(STALE.to_string());
    }
    let mut buf = Vec::new();
    decoder
        .take(end + 1 - start)
        .read_to_end(&mut buf)
        .map_err(|_| STALE.to_string())?;
    let wanted = (end - start) as usize;
    if buf.len() < wanted {
        return Err(STALE.to_string());
    }
    let more_after_end = buf.len() > wanted;
    Ok((buf, more_after_end))
}

fn extract_text(value: &Value, pointer: &str) -> Result<String, String> {
    let target = if pointer.is_empty() {
        value
    } else {
        value.pointer(pointer).ok_or_else(|| STALE.to_string())?
    };
    match target {
        Value::String(text) => Ok(text.clone()),
        Value::Array(items) => {
            let texts: Vec<&str> = items
                .iter()
                .filter_map(|item| match item {
                    Value::String(text) => Some(text.as_str()),
                    Value::Object(map) => map.get("text").and_then(Value::as_str),
                    _ => None,
                })
                .collect();
            if texts.is_empty() {
                pretty_json(target)
            } else {
                Ok(texts.join("\n"))
            }
        }
        Value::Object(_) => pretty_json(target),
        _ => Err(NOT_TEXT.to_string()),
    }
}

fn pretty_json(value: &Value) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|_| NOT_TEXT.to_string())
}

fn sqlite_allowed(provider_id: &str, table: &str, column: &str) -> bool {
    let (tables, columns): (&[&str], &[&str]) = match provider_id {
        "opencode" => (
            &["part", "message", "session_message"],
            &["data", "content"],
        ),
        "zcode" => (&["part", "message"], &["data"]),
        _ => (&[], &[]),
    };
    tables.contains(&table) && columns.contains(&column)
}

fn read_sqlite_cell(
    provider_id: &str,
    db: &Path,
    session_id: &str,
    table: &str,
    id: &str,
    column: &str,
) -> Result<String, String> {
    if !sqlite_allowed(provider_id, table, column) {
        return Err("不支持的内容引用".to_string());
    }
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "无法打开会话数据库".to_string())?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(2));
    let sql = format!(
        "SELECT length(CAST({column} AS BLOB)), {column} FROM {table} WHERE id = ?1 AND session_id = ?2"
    );
    let (size, cell): (Option<i64>, rusqlite::types::Value) = conn
        .query_row(&sql, rusqlite::params![id, session_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .map_err(|_| STALE.to_string())?;
    if size.unwrap_or(0) as u64 > MAX_TEXT_BYTES {
        return Err(TOO_LARGE_TEXT.to_string());
    }
    match cell {
        rusqlite::types::Value::Text(text) => Ok(text),
        rusqlite::types::Value::Blob(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        _ => Err(NOT_TEXT.to_string()),
    }
}

fn safe_join(base: &Path, rel_path: &str) -> Result<PathBuf, String> {
    let rel = Path::new(rel_path);
    if rel_path.is_empty()
        || !rel
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(OUTSIDE.to_string());
    }
    let base = base
        .canonicalize()
        .map_err(|_| "会话目录不存在".to_string())?;
    let joined = base
        .join(rel)
        .canonicalize()
        .map_err(|_| "引用的文件不存在".to_string())?;
    if !joined.starts_with(&base) {
        return Err(OUTSIDE.to_string());
    }
    Ok(joined)
}

fn read_limited(path: &Path, max: u64, too_large: &str) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|_| "无法读取文件".to_string())?;
    let meta = file.metadata().map_err(|_| "无法读取文件".to_string())?;
    if !meta.is_file() {
        return Err("引用的不是文件".to_string());
    }
    if meta.len() > max {
        return Err(too_large.to_string());
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "无法读取文件".to_string())?;
    if bytes.len() as u64 > max {
        return Err(too_large.to_string());
    }
    Ok(bytes)
}

// ── 图片读取与 Data URL 编码 ──────────────────────────────────────────

pub fn load_image_data_url(source: &ValidatedSource, image: &ImageRef) -> Result<String, String> {
    let media_type = normalize_safe_image_mime(&image.media_type)?;
    let bytes = load_image_bytes(source, image)?;
    let encoded = encode_base64(&bytes);
    Ok(format!("data:{media_type};base64,{encoded}"))
}

pub fn load_image_bytes(source: &ValidatedSource, image: &ImageRef) -> Result<Vec<u8>, String> {
    match &image.source {
        ImageSource::Inline { content } => {
            let encoded = resolve_content_ref(source, content)?;
            decode_inline_image(&encoded, MAX_IMAGE_BYTES)
        }
        ImageSource::LocalFile { path } => {
            let canonical = canonical_local_image(path)?;
            let mut allowed = local_image_dirs(source);
            if !allowed.iter().any(|dir| canonical.starts_with(dir)) {
                if let Some(project) = project_dir_for(source) {
                    allowed.push(project);
                }
            }
            if !allowed.iter().any(|dir| canonical.starts_with(dir)) {
                return Err("图片不在会话项目或工具目录内，无法预览".to_string());
            }
            read_limited(&canonical, MAX_IMAGE_BYTES, TOO_LARGE_IMAGE)
        }
    }
}

fn normalize_safe_image_mime(raw: &str) -> Result<&'static str, String> {
    let lower = raw.trim().to_ascii_lowercase();
    if lower.contains("svg") {
        return Err("SVG 图片不直接显示".to_string());
    }
    match lower.as_str() {
        "image/jpeg" | "image/jpg" => Ok("image/jpeg"),
        "image/gif" => Ok("image/gif"),
        "image/webp" => Ok("image/webp"),
        "image/bmp" => Ok("image/bmp"),
        _ => Ok("image/png"),
    }
}

fn decode_inline_image(encoded: &str, max: u64) -> Result<Vec<u8>, String> {
    let data = match encoded.strip_prefix("data:") {
        Some(rest) => rest
            .split_once(',')
            .map(|(_, data)| data)
            .ok_or("图片数据格式无效")?,
        None => encoded,
    };
    if data.len() as u64 > max / 3 * 4 + 64 {
        return Err(TOO_LARGE_IMAGE.to_string());
    }
    let bytes = decode_base64(data).map_err(|_| "图片数据格式无效".to_string())?;
    if bytes.len() as u64 > max {
        return Err(TOO_LARGE_IMAGE.to_string());
    }
    Ok(bytes)
}

fn canonical_local_image(raw: &str) -> Result<PathBuf, String> {
    let path = if raw.starts_with("file://") {
        parse_file_url(raw)?
    } else {
        PathBuf::from(raw)
    };
    if !path.is_absolute() {
        return Err("图片路径无效".to_string());
    }
    let ext = path
        .extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if ext == "svg" {
        return Err("SVG 图片不直接显示".to_string());
    }
    if !IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        return Err("不支持的图片格式".to_string());
    }
    path.canonicalize()
        .map_err(|_| "图片文件不存在".to_string())
}

fn local_image_dirs(source: &ValidatedSource) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let SourceLocation::Path { root, .. } = &source.location {
        dirs.push(root.clone());
    }
    if let Some(base) = source.sidecar_base() {
        dirs.push(base);
    }
    if source.provider_id == "codex" {
        dirs.push(crate::session_paths::codex_config_dir().join("visualizations"));
    }
    dirs.into_iter()
        .filter_map(|dir| dir.canonicalize().ok())
        .collect()
}

fn project_dir_for(source: &ValidatedSource) -> Option<PathBuf> {
    let sessions = match source.provider_id.as_str() {
        "codex" => super::providers::codex::scan_sessions(),
        "claude" => super::providers::claude::scan_sessions(),
        "gemini" => super::providers::gemini::scan_sessions(),
        "copilot" => super::providers::copilot::scan_sessions(),
        "opencode" => super::providers::opencode::scan_sessions(),
        "grokbuild" => super::providers::grokbuild::scan_sessions(),
        "pi" => super::providers::pi::scan_sessions(),
        "zcode" => super::providers::zcode::scan_sessions(),
        "deepseek" => super::providers::deepseek::scan_sessions(),
        "workbuddy" => super::providers::workbuddy::scan_sessions(),
        "codebuddy" => super::providers::codebuddy::scan_sessions(),
        "kimi" => super::providers::kimi::scan_sessions(),
        "openclaw" => super::providers::openclaw::scan_sessions(),
        _ => return None,
    };
    let project = sessions
        .into_iter()
        .find(|meta| meta.source_path.as_deref() == Some(source.raw.as_str()))?
        .project_dir?;
    let dir = Path::new(&project).canonicalize().ok()?;
    dir.parent()?;
    Some(dir)
}

/// 将 `file://` URL 转为本地绝对路径（支持百分号解码与 Windows 盘符）。
pub fn parse_file_url(raw: &str) -> Result<PathBuf, String> {
    let rest = raw
        .trim()
        .strip_prefix("file://")
        .ok_or_else(|| "路径格式无效".to_string())?;
    let raw_path = if let Some(after_localhost) = rest.strip_prefix("localhost/") {
        format!("/{after_localhost}")
    } else if rest.starts_with('/') {
        rest.to_string()
    } else {
        return Err("不支持远程主机 file URL".to_string());
    };
    let decoded = percent_decode(&raw_path)?;
    #[cfg(windows)]
    {
        let bytes = decoded.as_bytes();
        if bytes.len() >= 3
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && bytes[2] == b':'
        {
            return Ok(PathBuf::from(decoded[1..].replace('/', "\\")));
        }
    }
    Ok(PathBuf::from(decoded))
}

fn percent_decode(input: &str) -> Result<String, String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).map_err(|_| "路径编码无效".to_string())
}

// ── RFC 4648 标准 Base64 编解码 ────────────────────────────────────────

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode_base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);

        out.push(B64_ALPHABET[(b0 >> 2) as usize] as char);
        out.push(B64_ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64_ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64_ALPHABET[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub fn decode_base64(input: &str) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u8;
    let mut saw_pad = false;

    for byte in input.bytes() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'=' {
            saw_pad = true;
            continue;
        }
        if saw_pad {
            return Err("invalid padding");
        }
        let val = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return Err("invalid base64 char"),
        };
        buf = (buf << 6) | u32::from(val);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_all_padding_lengths() {
        for sample in [
            b"".as_slice(),
            b"a",
            b"ab",
            b"abc",
            b"hello world!",
            &[0u8, 255, 128, 64, 32, 16, 1],
        ] {
            let encoded = encode_base64(sample);
            let decoded = decode_base64(&encoded).unwrap();
            assert_eq!(decoded, sample);
        }
    }

    #[test]
    fn paginate_slices_by_char_offset() {
        let page1 = paginate("一二三四五", Some(0), Some(3));
        assert_eq!(page1.text, "一二三");
        assert!(page1.truncated);
        assert_eq!(page1.next_offset, Some(3));

        let page2 = paginate("一二三四五", page1.next_offset, Some(3));
        assert_eq!(page2.text, "四五");
        assert!(!page2.truncated);
        assert_eq!(page2.next_offset, None);
    }

    #[test]
    fn jsonl_window_reads_plain_and_compressed_identically() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("a.jsonl");
        let compressed = dir.path().join("a.jsonl.zst");
        let text = "{\"a\":1}\n{\"b\":\"two\"}\r\n{\"c\":3}";
        std::fs::write(&plain, text).unwrap();
        std::fs::write(
            &compressed,
            zstd::stream::encode_all(text.as_bytes(), 3).unwrap(),
        )
        .unwrap();

        let mut offset = 0u64;
        for segment in text.split_inclusive('\n') {
            let len = segment.len() as u32;
            let expected = read_jsonl_line("claude", &plain, offset, len).unwrap();
            assert_eq!(
                read_jsonl_line("codex", &compressed, offset, len).unwrap(),
                expected
            );
            offset += u64::from(len);
        }
    }
}
