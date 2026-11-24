pub mod cache;
pub mod content;
mod deletion;
pub mod model;
pub mod providers;
pub mod search_index;
pub mod terminal;
pub mod wire;

pub use model::SessionMessage;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use providers::{
    claude, codebuddy, codex, copilot, deepseek, gemini, grokbuild, kimi, openclaw, opencode, pi,
    workbuddy, zcode,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub provider_id: String,
    pub session_id: String,
    pub residual: bool,
    /// 父会话的原生 ID（子 Agent 会话、分支会话）。
    ///
    /// 界面用它显示归属（「某某会话的子会话」），删除父会话时也用它把整条子会话链
    /// 一次性带上；父会话本身的这一项为 `None`。缺失的父会话不算线索：条目只是
    /// 展示为孤儿，删除依旧由各 provider 的预检决定。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub cleanup_pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar_section: Option<SidebarSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidebarSection {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteSessionRequest {
    pub provider_id: String,
    pub session_id: String,
    pub source_path: String,
    #[serde(default)]
    pub include_project: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteSessionOutcome {
    pub provider_id: String,
    pub session_id: String,
    pub source_path: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// 删除请求的结构化结果；任何未完成的清理都必须保留可重试记录。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DeleteSessionReply {
    /// 记录已删除。内部清理警告由持久化协调器转换为 CleanupPending。
    Deleted {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<String>,
    },
    CleanupPending {
        warnings: Vec<String>,
    },
    /// 记录本就不存在（可能已被同批其他项的级联清理删掉），目标状态已达成。
    NotFound,
}

#[derive(Serialize)]
pub struct SessionScanReport {
    pub sessions: Vec<SessionMeta>,
    pub warnings: Vec<String>,
}

pub fn scan_sessions() -> Vec<SessionMeta> {
    scan_report().sessions
}

type ProviderScanFn = fn() -> Vec<SessionMeta>;

fn scan_report() -> SessionScanReport {
    let results = std::thread::scope(|s| {
        let specs: &[(&str, ProviderScanFn)] = &[
            ("codex", codex::scan_sessions),
            ("claude", claude::scan_sessions),
            ("gemini", gemini::scan_sessions),
            ("copilot", copilot::scan_sessions),
            ("opencode", opencode::scan_sessions),
            ("grokbuild", grokbuild::scan_sessions),
            ("pi", pi::scan_sessions),
            ("zcode", zcode::scan_sessions),
            ("deepseek", deepseek::scan_sessions),
            ("workbuddy", workbuddy::scan_sessions),
            ("codebuddy", codebuddy::scan_sessions),
            ("kimi", kimi::scan_sessions),
            ("openclaw", openclaw::scan_sessions),
        ];
        let handles: Vec<_> = specs
            .iter()
            .map(|&(name, scan_fn)| {
                s.spawn(move || providers::utils::scan_with_diagnostics(name, scan_fn))
            })
            .collect();
        handles
            .into_iter()
            .map(join_scan_results)
            .collect::<Vec<_>>()
    });

    let mut sessions = Vec::new();
    let mut warnings = Vec::new();
    for (items, issues) in results {
        sessions.extend(items);
        warnings.extend(issues);
    }

    let (pending, journal_warnings) = deletion::pending_sessions_report()
        .unwrap_or_else(|error| (Vec::new(), vec![format!("无法读取待清理记录：{error}")]));
    warnings.extend(journal_warnings);
    for item in pending {
        if let Some(existing) = sessions.iter_mut().find(|s| {
            s.provider_id == item.provider_id
                && s.session_id == item.session_id
                && s.source_path == item.source_path
        }) {
            existing.cleanup_pending = true;
            existing.resume_command = None;
            existing.residual = item.residual;
            existing.archived = item.archived;
            // The native title may have disappeared with the index during
            // partial deletion. Keep the name saved in the retry journal.
            if item.title.is_some() {
                existing.title = item.title;
            }
            existing.summary = item.summary;
        } else {
            sessions.push(item);
        }
    }
    sessions.sort_by(|a, b| {
        let a_ts = a.last_active_at.or(a.created_at).unwrap_or(0);
        let b_ts = b.last_active_at.or(b.created_at).unwrap_or(0);
        b_ts.cmp(&a_ts)
    });

    SessionScanReport { sessions, warnings }
}

/// 单个 provider 的扫描线程 panic 时不允许无声吞掉：至少留下日志，否则
/// 用户看到的是"该 Agent 没有会话"，与真的没有会话无法区分。
fn join_scan_results(
    handle: std::thread::ScopedJoinHandle<'_, (Vec<SessionMeta>, Vec<String>)>,
) -> (Vec<SessionMeta>, Vec<String>) {
    match handle.join() {
        Ok(items) => items,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&'static str>()
                .map(|text| (*text).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic payload".to_string());
            log::warn!("provider scan thread panicked: {message}");
            (Vec::new(), vec![format!("Agent 扫描线程失败：{message}")])
        }
    }
}

#[cfg(windows)]
fn special_windows_prefix_key(path: &Path) -> Option<String> {
    use std::path::{Component, Prefix};
    let Some(Component::Prefix(prefix_comp)) = path.components().next() else {
        return None;
    };
    match prefix_comp.kind() {
        Prefix::Disk(_) | Prefix::VerbatimDisk(_) => None,
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => Some(format!(
            "unc:{}\\{}",
            server.to_string_lossy().to_ascii_lowercase(),
            share.to_string_lossy().to_ascii_lowercase()
        )),
        Prefix::DeviceNS(dev) => Some(format!(
            "dev:{}",
            dev.to_string_lossy().to_ascii_lowercase()
        )),
        Prefix::Verbatim(v) => Some(format!(
            "verbatim:{}",
            v.to_string_lossy().to_ascii_lowercase()
        )),
    }
}

fn validate_lexical_source_path(path: &Path, roots: &[PathBuf]) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!(
            "Session source path must be absolute: {}",
            path.display()
        ));
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!(
            "Session source path contains parent traversal: {}",
            path.display()
        ));
    }
    #[cfg(windows)]
    if let Some(prefix_key) = special_windows_prefix_key(path) {
        let allowed = roots
            .iter()
            .any(|root| special_windows_prefix_key(root).as_deref() == Some(prefix_key.as_str()));
        if !allowed {
            return Err(format!(
                "Session source path uses an unconfigured UNC or device prefix: {}",
                path.display()
            ));
        }
    }
    #[cfg(not(windows))]
    let _ = roots;
    Ok(())
}

/// 校验 `source_path` 归属并返回规范化路径；SQLite 源没有文件路径，由调用方先行分发。
fn validated_file_source(provider_id: &str, source_path: &str) -> Result<PathBuf, String> {
    let roots = provider_roots(provider_id)?;
    let raw_path = Path::new(source_path);
    validate_lexical_source_path(raw_path, &roots)?;
    let physical = if provider_id == "codex" {
        codex::physical_rollout_path(raw_path)?
    } else {
        raw_path.to_path_buf()
    };
    if matches!(
        provider_id,
        "pi" | "claude" | "workbuddy" | "gemini" | "copilot" | "codebuddy" | "kimi" | "openclaw"
    ) {
        let raw_meta = std::fs::symlink_metadata(&physical)
            .map_err(|_| format!("session source not found: {}", physical.display()))?;
        if providers::utils::is_reparse_or_symlink(&raw_meta) {
            return Err(format!(
                "Session source path must not be a symlink or reparse point: {}",
                physical.display()
            ));
        }
        if !roots
            .iter()
            .filter(|root| root.exists())
            .any(|root| providers::utils::checked_storage_child(root, &physical).is_ok())
        {
            return Err("Session source is outside configured storage".into());
        }
    }
    let canonical = canonicalize_existing_path(&physical, "session source")?;
    if !roots.iter().any(|root| {
        root.canonicalize()
            .is_ok_and(|root| canonical.starts_with(root))
    }) {
        return Err("Session source is outside configured storage".into());
    }
    Ok(canonical)
}

pub fn search_messages(messages: &[SessionMessage], query: &str) -> Vec<usize> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|term| term.to_lowercase())
        .collect();
    if terms.is_empty() {
        return Vec::new();
    }
    messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            let haystack = message.content.to_lowercase();
            terms
                .iter()
                .all(|term| haystack.contains(term.as_str()))
                .then_some(index)
        })
        .collect()
}

fn dispatch_messages(provider_id: &str, path: &Path) -> Result<Vec<SessionMessage>, String> {
    match provider_id {
        "codex" => codex::load_messages(path),
        "claude" => claude::load_messages(path),
        "gemini" => gemini::load_messages(path),
        "copilot" => copilot::load_messages(path),
        "opencode" => opencode::load_messages(path),
        "grokbuild" => grokbuild::load_messages(path),
        "pi" => pi::load_messages(path),
        "deepseek" => deepseek::load_messages(path),
        "workbuddy" => workbuddy::load_messages(path),
        "codebuddy" => codebuddy::load_messages(path),
        "kimi" => kimi::load_messages(path),
        "openclaw" => openclaw::load_messages(path),
        _ => Err(format!("Unsupported provider: {provider_id}")),
    }
}

/// 正文解析结果可缓存的 provider：返回正文实际依赖的文件（有序）与每段只读到的偏移。
///
/// 少列一个文件、或漏掉链的顺序/截断偏移，都会命中过期正文，因此只接受已经逐行核对
/// 过读取范围的 provider：
/// - `claude` / `deepseek` / `pi` / `workbuddy`：正文就是源文件本身
/// - `grokbuild`：正文在同目录 `chat_history.jsonl`，而 `summary.json` 只在一轮
///   结束时改写，只盯后者会让一轮进行中的会话命中旧正文
/// - `codex`：正文是「当前文件 + 基座文件的前 N 字节」拼起来的分页历史链
///
/// SQLite 源（`opencode` / `zcode` / `workbuddy`）在这里拿不到路径，由 `load_transcript` 的 SQLite
/// 分支处理。`opencode` 的旧版消息目录不接入：列出兄弟 `part/` 目录需要先读
/// 消息文件取 id，等于把读取本身做一遍。`pi` 扫描期的 `residual` 标志依赖父会话文件是否
/// 存在，那是元数据的顾虑，与正文无关。
fn transcript_cache_sources(provider_id: &str, path: &Path) -> Option<cache::TranscriptSources> {
    match provider_id {
        "claude" | "deepseek" | "pi" | "workbuddy" => Some(vec![(path.to_path_buf(), None)]),
        "gemini" => gemini::transcript_sources(path).ok(),
        "copilot" => copilot::transcript_sources(path).ok(),
        "codebuddy" => codebuddy::transcript_sources(path).ok(),
        "kimi" => kimi::transcript_sources(path).ok(),
        "openclaw" => openclaw::transcript_sources(path).ok(),
        "grokbuild" => grokbuild::transcript_sources(path).ok(),
        // 链解析失败时不缓存，交给正文读取去报真正的错误
        "codex" => codex::transcript_sources(path).ok(),
        _ => None,
    }
}

/// `source_path` 里 SQLite 引用的种类。
///
/// `provider_id` 与 `source_path` 都来自前端且互不绑定，所以判定必须成对进行：
/// 只按前缀分发时，`providerId="opencode"` 配一个 `sqlite-zcode:` 来源也会被当成
/// OpenCode 的库去解析。这里先分类、再核对 provider 是否为该类的所有者。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SqliteSourceKind {
    OpenCode,
    ZCode,
    WorkBuddy,
}

fn sqlite_source_kind(source_path: &str) -> Option<SqliteSourceKind> {
    if source_path.starts_with("sqlite-zcode:") {
        Some(SqliteSourceKind::ZCode)
    } else if source_path.starts_with("sqlite-workbuddy:") {
        Some(SqliteSourceKind::WorkBuddy)
    } else if source_path.starts_with("sqlite:") {
        Some(SqliteSourceKind::OpenCode)
    } else {
        None
    }
}

/// 该 provider 拥有的 SQLite 引用种类。
fn sqlite_source_owner(provider_id: &str) -> Option<SqliteSourceKind> {
    match provider_id {
        "opencode" => Some(SqliteSourceKind::OpenCode),
        "zcode" => Some(SqliteSourceKind::ZCode),
        "workbuddy" => Some(SqliteSourceKind::WorkBuddy),
        _ => None,
    }
}

/// 校验 `source_path` 的 SQLite 引用确实属于 `provider_id`。
///
/// 非 SQLite 来源（普通文件路径）直接放行，由各自的文件路径校验处理。
fn checked_sqlite_source(
    provider_id: &str,
    source_path: &str,
) -> Result<Option<SqliteSourceKind>, String> {
    let Some(kind) = sqlite_source_kind(source_path) else {
        return Ok(None);
    };
    if sqlite_source_owner(provider_id) != Some(kind) {
        return Err(format!(
            "会话来源与 Agent 不匹配，已拒绝读取（{provider_id}）：{source_path}"
        ));
    }
    Ok(Some(kind))
}

pub(crate) fn index_sources(
    provider_id: &str,
    source_path: &str,
) -> Option<cache::TranscriptSources> {
    if let Some(kind) = checked_sqlite_source(provider_id, source_path).ok()? {
        return match kind {
            SqliteSourceKind::OpenCode => opencode::sqlite_transcript_sources(source_path),
            SqliteSourceKind::ZCode => zcode::transcript_sources(source_path),
            SqliteSourceKind::WorkBuddy => workbuddy::sqlite_transcript_sources(source_path),
        };
    }
    let path = validated_file_source(provider_id, source_path).ok()?;
    transcript_cache_sources(provider_id, &path)
}

/// 读取会话正文：校验来源后按指纹复用上次解析结果。
///
/// 未接入缓存的会话与解析失败一样直接读取，不会返回过期正文。返回的
/// [`cache::Transcript::cacheable`] 如实反映这次解析是否进了缓存，下发形态据此
/// 决定能否只发预览——不再需要第二处按 provider/source_path 推断「可不可缓存」。
pub fn load_transcript(
    provider_id: &str,
    source_path: &str,
) -> Result<Arc<cache::Transcript>, String> {
    if let Some(kind) = checked_sqlite_source(provider_id, source_path)? {
        return match kind {
            SqliteSourceKind::OpenCode => match opencode::sqlite_transcript_sources(source_path) {
                Some(sources) => cached_transcript(provider_id, source_path, sources, || {
                    opencode::load_messages_sqlite(source_path)
                }),
                // 引用无法解析：不猜指纹，交给读取器报错
                None => Ok(Arc::new(cache::Transcript::new(
                    opencode::load_messages_sqlite(source_path)?,
                ))),
            },
            SqliteSourceKind::ZCode => match zcode::transcript_sources(source_path) {
                Some(sources) => cached_transcript(provider_id, source_path, sources, || {
                    zcode::load_messages(source_path)
                }),
                None => Ok(Arc::new(cache::Transcript::new(zcode::load_messages(
                    source_path,
                )?))),
            },
            SqliteSourceKind::WorkBuddy => {
                match workbuddy::sqlite_transcript_sources(source_path) {
                    Some(sources) => cached_transcript(provider_id, source_path, sources, || {
                        workbuddy::load_messages_sqlite(source_path)
                    }),
                    None => Ok(Arc::new(cache::Transcript::new(
                        workbuddy::load_messages_sqlite(source_path)?,
                    ))),
                }
            }
        };
    }

    let path = validated_file_source(provider_id, source_path)?;
    let Some(sources) = transcript_cache_sources(provider_id, &path) else {
        return Ok(Arc::new(cache::Transcript::new(dispatch_messages(
            provider_id,
            &path,
        )?)));
    };
    cached_transcript(provider_id, source_path, sources, || {
        dispatch_messages(provider_id, &path)
    })
}

/// 指纹可读时按指纹复用缓存，否则直接读取（不缓存）。
fn cached_transcript<F>(
    provider_id: &str,
    source_path: &str,
    sources: cache::TranscriptSources,
    load: F,
) -> Result<Arc<cache::Transcript>, String>
where
    F: FnOnce() -> Result<Vec<SessionMessage>, String>,
{
    match cache::transcript_fingerprint(&sources) {
        Ok(fingerprint) => cache::global()
            .get_or_load(
                cache::cache_key(provider_id, source_path, &sources),
                fingerprint,
                || load().map(cache::Transcript::new),
            )
            .map(|(transcript, _cached)| transcript),
        // 指纹里的文件都读不到（库被移走、权限不足）：交给读取器报真正的错误
        Err(_) => Ok(Arc::new(cache::Transcript::new(load()?))),
    }
}

pub fn checked_list_sessions() -> Result<SessionScanReport, String> {
    // 浏览不因单份清理记录损坏而整体失败：扫描报告会携带警告；删除路径仍
    // 单独走 pending_sessions()? 严格校验，继续拒绝操作。
    Ok(scan_report())
}

fn validate_codex_sqlite_home_preflight() -> Result<(), String> {
    let config_dir = crate::session_paths::codex_config_dir();
    let config_text = crate::session_paths::read_codex_config_text()?;
    crate::session_paths::validate_codex_sqlite_home_for_delete(&config_dir, &config_text)
        .map(|_| ())
}

pub fn delete_session_checked(
    provider_id: &str,
    session_id: &str,
    source_path: &str,
    include_project: bool,
) -> Result<DeleteSessionReply, String> {
    if include_project {
        return Err("会话清理不再删除项目目录；请使用文件管理器单独处理项目文件".into());
    }
    let _guard = deletion::DELETE_LOCK
        .lock()
        .map_err(|_| "清理锁不可用，请重启管理器")?;
    deletion::ensure_agent_stopped(provider_id)?;
    deletion::ensure_pending_journals_healthy(Some(provider_id))?;
    let sessions = scan_sessions();
    let target = sessions
        .iter()
        .find(|item| {
            item.provider_id == provider_id
                && item.session_id == session_id
                && item.source_path.as_deref() == Some(source_path)
        })
        .ok_or("会话已变化或不存在，请重新扫描")?;
    let reply = if provider_id == "codex" {
        let roots = provider_roots("codex")?;
        validate_codex_sqlite_home_preflight()?;
        deletion::run_codex(target, &sessions, &roots)
    } else {
        deletion::run(target, &sessions, preflight_deletion, |plan| {
            delete_session_in_snapshot(provider_id, session_id, source_path, include_project, plan)
        })
    };
    // 删除会级联清理同 ID 的历史副本与索引，正文缓存整体作废
    cache::global().clear();
    let _ = search_index::remove_indexed_sources(&[(provider_id, source_path)]);
    reply
}

/// 清理计划里的这一条目是否已经不需要（也无法）预检。
///
/// 持久化的清理计划在原样重试时，可能包含**已经被删掉**的条目：主会话删除成功、
/// 但同 ID 的历史副本清理失败时，journal 会保留整个原始计划，好让重试可以把剩余
/// 副本收尾。对「正文就是一个文件」的 provider（Codex 的 rollout、Pi 的 jsonl），
/// 这些条目已经没有可校验的内容；再去读一次已经消失
/// 的文件只会让每次重试都卡在同一个错误上，而磁盘上确实还有副本等着清理。
///
/// 只对文件型 provider 成立：`sqlite:` / `sqlite-zcode:` 的 `source_path` 是引用
/// 而不是路径，把它当路径判断存在性会得到 false，从而错误地跳过真正需要的预检。
/// 这些 provider 的预检本身要能处理缺失源（DeepSeek 会清理投影缓存；OpenCode 的
/// `checked_storage_child` 对缺失目标也成立），所以不在这里按存在性跳过。
fn plan_entry_skips_preflight(item: &SessionMeta) -> Result<bool, String> {
    if !matches!(item.provider_id.as_str(), "codex" | "pi") {
        return Ok(false);
    }
    let path = item.source_path.as_deref().ok_or("缺少会话路径")?;
    Ok(std::fs::symlink_metadata(path).is_err())
}

fn preflight_deletion(plan: &[SessionMeta]) -> Result<(), String> {
    if plan.iter().any(|item| item.provider_id == "codex") {
        validate_codex_sqlite_home_preflight()?;
    }
    // Validate every Codex transcript in the persisted plan using the exact
    // canonical paths later used by deletion, before any index or journal write.
    for item in plan.iter().filter(|item| item.provider_id == "codex") {
        if plan_entry_skips_preflight(item)? {
            continue; // legitimate retry after the transcript was already removed
        }
        let path = Path::new(item.source_path.as_deref().ok_or("缺少会话路径")?);
        let canonical = canonicalize_existing_path(path, "session source")?;
        let root = provider_roots("codex")?
            .into_iter()
            .filter_map(|root| root.canonicalize().ok())
            .find(|root| canonical.starts_with(root))
            .ok_or("Session source is outside configured storage")?;
        codex::validate_delete_under_root(&root, &canonical, &item.session_id)?;
        codex::should_cleanup_index_after_delete(&item.session_id, &canonical.to_string_lossy())?;
    }
    for item in plan.iter().filter(|item| item.provider_id == "claude") {
        let path = Path::new(item.source_path.as_deref().ok_or("缺少会话路径")?);
        let root = provider_roots("claude")?
            .into_iter()
            .find(|root| root.exists())
            .ok_or("Claude 会话根目录不存在")?;
        claude::preflight_delete(&root, path, &item.session_id)?;
    }
    for item in plan.iter().filter(|item| item.provider_id == "deepseek") {
        let path = Path::new(item.source_path.as_deref().ok_or("缺少会话路径")?);
        let root = provider_roots("deepseek")?
            .into_iter()
            .find(|root| root.exists())
            .ok_or("DeepSeek Harness 会话根目录不存在")?;
        deepseek::preflight_delete(&root, path, &item.session_id)?;
    }
    for item in plan.iter().filter(|item| item.provider_id == "zcode") {
        provider_roots("zcode")?;
        zcode::preflight_delete(
            item.source_path.as_deref().ok_or("缺少会话路径")?,
            &item.session_id,
        )?;
    }
    for item in plan.iter().filter(|item| item.provider_id == "pi") {
        if plan_entry_skips_preflight(item)? {
            continue;
        }
        let path = Path::new(item.source_path.as_deref().ok_or("缺少会话路径")?);
        let root = provider_roots("pi")?
            .into_iter()
            .find(|root| root.exists())
            .ok_or("Pi 会话根目录不存在")?;
        pi::preflight_delete(&root, path, &item.session_id)?;
    }
    for item in plan.iter().filter(|item| item.provider_id == "opencode") {
        provider_roots("opencode")?;
        opencode::preflight_delete(
            item.source_path.as_deref().ok_or("缺少会话路径")?,
            &item.session_id,
        )?;
    }
    if plan.iter().any(|item| item.provider_id == "grokbuild") {
        preflight_grok_deletion(plan, &provider_roots("grokbuild")?)?;
    }
    for item in plan.iter().filter(|item| item.provider_id == "workbuddy") {
        let roots = provider_roots("workbuddy")?;
        let source = item.source_path.as_deref().ok_or("缺少会话路径")?;
        if source.starts_with("sqlite-workbuddy:") {
            workbuddy::preflight_delete_sqlite(source, &item.session_id)?;
        } else {
            workbuddy::preflight_delete_in_roots(&roots, Path::new(source), &item.session_id)?;
        }
    }
    for item in plan.iter().filter(|item| item.provider_id == "gemini") {
        let path = Path::new(item.source_path.as_deref().ok_or("缺少会话路径")?);
        let root = provider_roots("gemini")?
            .into_iter()
            .find(|root| root.exists())
            .ok_or("Gemini CLI 会话根目录不存在")?;
        gemini::preflight_delete(&root, path, &item.session_id)?;
    }
    for item in plan.iter().filter(|item| item.provider_id == "copilot") {
        let path = Path::new(item.source_path.as_deref().ok_or("缺少会话路径")?);
        let root = provider_roots("copilot")?
            .into_iter()
            .find(|root| root.exists())
            .ok_or("GitHub Copilot 会话根目录不存在")?;
        copilot::preflight_delete(&root, path, &item.session_id)?;
    }
    for item in plan.iter().filter(|item| item.provider_id == "codebuddy") {
        let path = Path::new(item.source_path.as_deref().ok_or("缺少会话路径")?);
        let root = provider_roots("codebuddy")?
            .into_iter()
            .find(|root| root.exists())
            .ok_or("CodeBuddy 会话根目录不存在")?;
        codebuddy::preflight_delete(&root, path, &item.session_id)?;
    }
    for item in plan.iter().filter(|item| item.provider_id == "kimi") {
        let path = Path::new(item.source_path.as_deref().ok_or("缺少会话路径")?);
        let root = provider_roots("kimi")?
            .into_iter()
            .find(|root| root.exists())
            .ok_or("Kimi CLI 会话根目录不存在")?;
        kimi::preflight_delete(&root, path, &item.session_id)?;
    }
    for item in plan.iter().filter(|item| item.provider_id == "openclaw") {
        let path = Path::new(item.source_path.as_deref().ok_or("缺少会话路径")?);
        let root = provider_roots("openclaw")?
            .into_iter()
            .find(|root| root.exists())
            .ok_or("OpenClaw 会话根目录不存在")?;
        openclaw::preflight_delete(&root, path, &item.session_id)?;
    }
    Ok(())
}

fn preflight_grok_deletion(plan: &[SessionMeta], roots: &[PathBuf]) -> Result<(), String> {
    for item in plan.iter().filter(|item| item.provider_id == "grokbuild") {
        // summary.json may be gone while its directory still needs cleanup.
        // Validate the exact persisted path and ID even for wholly missing dirs.
        let path = Path::new(item.source_path.as_deref().ok_or("缺少会话路径")?);
        grokbuild::preflight_delete_in_roots(roots, path, &item.session_id)?;
    }
    Ok(())
}

/// 在预扫描的会话快照上执行删除。批量删除共用一次扫描，避免 N 项删除
/// 触发 N 次全量磁盘扫描。
fn delete_session_in_snapshot(
    provider_id: &str,
    session_id: &str,
    source_path: &str,
    include_project: bool,
    sessions: &[SessionMeta],
) -> Result<DeleteSessionReply, String> {
    delete_session_in_snapshot_with(
        provider_id,
        session_id,
        source_path,
        include_project,
        sessions,
        delete_session_record,
    )
}

fn delete_session_in_snapshot_with<F>(
    provider_id: &str,
    session_id: &str,
    source_path: &str,
    include_project: bool,
    sessions: &[SessionMeta],
    mut delete_record: F,
) -> Result<DeleteSessionReply, String>
where
    F: FnMut(&str, &str, &str, bool) -> Result<bool, String>,
{
    let selected = sessions.iter().find(|item| {
        item.provider_id == provider_id
            && item.session_id == session_id
            && item.source_path.as_deref() == Some(source_path)
    });

    if include_project {
        return Err("会话清理不再删除项目目录；请使用文件管理器单独处理项目文件".into());
    }
    if selected.is_some_and(|item| item.residual && !item.cleanup_pending)
        && matches!(provider_id, "codex" | "pi" | "deepseek")
        && !Path::new(source_path).exists()
    {
        return Ok(DeleteSessionReply::NotFound);
    }
    let related_residual_sources = selected
        .map(|item| {
            collect_related_residual_sources(
                sessions,
                provider_id,
                session_id,
                source_path,
                item.residual,
            )
        })
        .unwrap_or_default();

    if provider_id == "codex" && Path::new(source_path).exists() {
        codex::validate_delete(Path::new(source_path), session_id)?;
    }

    // Clear referenced indexes BEFORE removing the transcript. Archived threads
    // are included in the authoritative lookup; historical copies are not.
    if provider_id == "codex" && codex::should_cleanup_index_after_delete(session_id, source_path)?
    {
        codex::cleanup_after_delete(session_id)?;
    }
    // DeepSeek, Grok, and Claude (for sessions-index.json ghosts and sidecar
    // retries) finish index/sidecar cleanup even after the primary `.jsonl`
    // source is gone, so they must reach `delete_session_record`.
    let missing_file = matches!(provider_id, "codex" | "pi") && !Path::new(source_path).exists();
    let unexpectedly_missing = (matches!(
        provider_id,
        "claude" | "gemini" | "copilot" | "codebuddy" | "kimi" | "openclaw"
    ) || (provider_id == "workbuddy"
        && !source_path.starts_with("sqlite-workbuddy:")))
        && !Path::new(source_path).exists()
        && !selected.is_some_and(|item| item.cleanup_pending || item.residual);
    let deleted = if unexpectedly_missing {
        return Err("会话文件已消失，请重新扫描".into());
    } else if missing_file {
        if selected.is_some_and(|item| item.cleanup_pending || item.residual) {
            true
        } else {
            return Err("会话文件已消失，请重新扫描".into());
        }
    } else {
        delete_record(provider_id, session_id, source_path, false)?
    };
    let mut warnings = Vec::new();

    // Deleting a current conversation means deleting that conversation as a
    // whole. Historical rollouts and legacy-format copies with the same native
    // ID must not surface as new leftovers afterward. Selecting a residual
    // entry itself remains deliberately scoped to that one entry.
    if deleted || selected.is_some_and(|item| item.cleanup_pending) {
        for residual_source in related_residual_sources {
            if matches!(provider_id, "codex" | "pi") && !Path::new(&residual_source).exists() {
                continue;
            }
            if let Err(error) = delete_record(provider_id, session_id, &residual_source, false) {
                warnings.push(format!("会话已删除，但残留副本清理失败：{error}"));
            }
        }
    }

    Ok(
        if deleted || selected.is_some_and(|item| item.cleanup_pending) {
            DeleteSessionReply::Deleted { warnings }
        } else {
            DeleteSessionReply::NotFound
        },
    )
}

fn collect_related_residual_sources(
    sessions: &[SessionMeta],
    provider_id: &str,
    session_id: &str,
    selected_source: &str,
    selected_is_residual: bool,
) -> Vec<String> {
    if selected_is_residual {
        return Vec::new();
    }
    sessions
        .iter()
        .filter(|item| {
            item.provider_id == provider_id
                && item.session_id == session_id
                && item.residual
                && item.source_path.as_deref() != Some(selected_source)
                && (provider_id != "opencode"
                    || item
                        .source_path
                        .as_deref()
                        .is_some_and(|source| opencode::same_storage(selected_source, source)))
        })
        .filter_map(|item| item.source_path.clone())
        .collect()
}

fn delete_session_record(
    provider_id: &str,
    session_id: &str,
    source_path: &str,
    include_project: bool,
) -> Result<bool, String> {
    // SQLite sessions bypass the file-based deletion path, but still enforce absolute provider_roots.
    let deleted = if provider_id == "opencode" && source_path.starts_with("sqlite:") {
        provider_roots("opencode")?;
        opencode::delete_session_sqlite(session_id, source_path)?
    } else if provider_id == "zcode" && source_path.starts_with("sqlite-zcode:") {
        provider_roots("zcode")?;
        zcode::delete_session(session_id, source_path, include_project)?
    } else if provider_id == "workbuddy" && source_path.starts_with("sqlite-workbuddy:") {
        provider_roots("workbuddy")?;
        workbuddy::delete_session_sqlite(session_id, source_path)?
    } else {
        let roots = provider_roots(provider_id)?;
        delete_session_with_roots(provider_id, session_id, Path::new(source_path), &roots)?
    };
    Ok(deleted)
}

pub fn delete_sessions(requests: &[DeleteSessionRequest]) -> Vec<DeleteSessionOutcome> {
    let mut stop_checks = deletion::AgentStopChecks::default();
    delete_sessions_with_check(requests, |provider| stop_checks.ensure(provider))
}

fn delete_sessions_with_check<F>(
    requests: &[DeleteSessionRequest],
    mut ensure_stopped: F,
) -> Vec<DeleteSessionOutcome>
where
    F: FnMut(&str) -> Result<(), String>,
{
    if requests.is_empty() {
        return Vec::new();
    }
    let guard = deletion::DELETE_LOCK.lock();
    if guard.is_err() {
        return collect_delete_session_outcomes(requests, |_| {
            Err("清理锁不可用，请重启管理器".into())
        });
    }
    let all_journals_healthy = deletion::ensure_pending_journals_healthy(None).is_ok();
    if !all_journals_healthy
        && requests
            .iter()
            .all(|req| deletion::ensure_pending_journals_healthy(Some(&req.provider_id)).is_err())
    {
        return collect_delete_session_outcomes(requests, |request| {
            if request.include_project {
                return Err("批量清理不删除项目目录".into());
            }
            ensure_stopped(&request.provider_id)?;
            deletion::ensure_pending_journals_healthy(Some(&request.provider_id))?;
            Ok(DeleteSessionReply::NotFound)
        });
    }

    let sessions = scan_sessions();
    let codex_order = codex::deletion_order(requests);
    let order = deepseek::deletion_order(requests, &codex_order);
    let order = zcode::deletion_order(requests, &order);
    let order = pi::deletion_order(requests, &order);
    let order = opencode::deletion_order(requests, &order);
    let order = grokbuild::deletion_order(requests, &order);
    let ordered = order
        .iter()
        .map(|index| requests[*index].clone())
        .collect::<Vec<_>>();
    let mut codex_progress = deletion::CodexBatchProgress::default();
    let outcomes = collect_delete_session_outcomes(&ordered, |request| {
        if request.include_project {
            return Err("批量清理不删除项目目录".into());
        }
        ensure_stopped(&request.provider_id)?;
        if !all_journals_healthy {
            deletion::ensure_pending_journals_healthy(Some(&request.provider_id))?;
        }
        let Some(target) = sessions.iter().find(|item| {
            item.provider_id == request.provider_id
                && item.session_id == request.session_id
                && item.source_path.as_deref() == Some(request.source_path.as_str())
        }) else {
            return Err("会话已变化或不存在，请重新扫描".into());
        };
        if request.provider_id == "codex" {
            let roots = provider_roots("codex")?;
            validate_codex_sqlite_home_preflight()?;
            return codex_progress.run(target, &sessions, |target, remaining| {
                deletion::run_codex(target, remaining, &roots)
            });
        }
        deletion::run(target, &sessions, preflight_deletion, |plan| {
            delete_session_in_snapshot(
                &request.provider_id,
                &request.session_id,
                &request.source_path,
                false,
                plan,
            )
        })
    });
    // Keep the API response in the caller's original order despite executing
    // child branches first.
    let mut outcomes = order.into_iter().zip(outcomes).collect::<Vec<_>>();
    outcomes.sort_by_key(|(index, _)| *index);
    // 批量删除会级联清理同 ID 的历史副本与索引，正文缓存整体作废
    cache::global().clear();
    let deleted_sources: Vec<(&str, &str)> = requests
        .iter()
        .map(|req| (req.provider_id.as_str(), req.source_path.as_str()))
        .collect();
    let _ = search_index::remove_indexed_sources(&deleted_sources);
    outcomes.into_iter().map(|(_, outcome)| outcome).collect()
}

fn delete_session_with_roots(
    provider_id: &str,
    session_id: &str,
    source_path: &Path,
    roots: &[PathBuf],
) -> Result<bool, String> {
    validate_lexical_source_path(source_path, roots)?;

    // Grok owns a directory, not just summary.json. Preserve the original path
    // so its validator can reject links and finish partially removed directories.
    if provider_id == "grokbuild" {
        return grokbuild::delete_session_in_roots(roots, source_path, session_id);
    }

    // A DeepSeek cleanup retry may already have removed the transcript after
    // updating workspace.json, while its projection cache still needs work.
    // Its provider validates the complete lexical layout (including symlinked
    // ancestors) and is therefore the authority for both existing and missing
    // source paths.
    if provider_id == "deepseek" {
        let root = roots
            .iter()
            .find(|root| root.exists())
            .ok_or("DeepSeek Harness 会话根目录不存在")?;
        return deepseek::delete_session(root, source_path, session_id);
    }

    // Claude validates depth-2 layout, rejects symlinked transcripts/ancestors,
    // and supports `sessions-index.json` ghost and sidecar cleanup retries even
    // when the `.jsonl` file has already been removed.
    if provider_id == "claude" {
        let root = roots.iter().find(|root| root.exists()).ok_or_else(|| {
            format!(
                "Session root not found for provider claude: {}",
                roots
                    .first()
                    .map(|root| root.display().to_string())
                    .unwrap_or_else(|| "<none>".to_string())
            )
        })?;
        return claude::delete_session(root, source_path, session_id);
    }

    if provider_id == "workbuddy" {
        return workbuddy::delete_session_in_roots(roots, source_path, session_id);
    }

    if matches!(
        provider_id,
        "gemini" | "copilot" | "codebuddy" | "kimi" | "openclaw"
    ) {
        let root = roots.iter().find(|root| root.exists()).ok_or_else(|| {
            format!(
                "Session root not found for provider {provider_id}: {}",
                roots
                    .first()
                    .map(|root| root.display().to_string())
                    .unwrap_or_else(|| "<none>".to_string())
            )
        })?;
        return match provider_id {
            "gemini" => gemini::delete_session(root, source_path, session_id),
            "copilot" => copilot::delete_session(root, source_path, session_id),
            "codebuddy" => codebuddy::delete_session(root, source_path, session_id),
            "kimi" => kimi::delete_session(root, source_path, session_id),
            "openclaw" => openclaw::delete_session(root, source_path, session_id),
            _ => unreachable!(),
        };
    }

    // Reject symlinked Pi session paths before `canonicalize_existing_path`
    // could resolve a link onto another session target.
    if provider_id == "pi" {
        let raw_meta = std::fs::symlink_metadata(source_path)
            .map_err(|_| format!("session source not found: {}", source_path.display()))?;
        if providers::utils::is_reparse_or_symlink(&raw_meta) {
            return Err(format!(
                "Session source path must not be a symlink or reparse point: {}",
                source_path.display()
            ));
        }
        let mut saw_existing_root = false;
        for root in roots {
            if !root.exists() {
                continue;
            }
            saw_existing_root = true;
            if providers::utils::checked_storage_child(root, source_path).is_ok() {
                let validated_root = canonicalize_existing_path(root, "session root")?;
                return pi::delete_session(&validated_root, source_path, session_id);
            }
        }
        if !saw_existing_root {
            return Err(format!(
                "Session root not found for provider pi: {}",
                roots
                    .first()
                    .map(|root| root.display().to_string())
                    .unwrap_or_else(|| "<none>".to_string())
            ));
        }
        return Err(format!(
            "Session source path is outside provider roots: {}",
            source_path.display()
        ));
    }

    // A legacy OpenCode session can retain its metadata after its message
    // directory has already disappeared. The scanner deliberately surfaces
    // that record as a residual, so allow its exact expected message path to
    // reach the provider cleanup even though the path itself no longer exists.
    // The equality check keeps arbitrary missing paths outside the data root
    // from being accepted.
    if provider_id == "opencode" && !source_path.exists() {
        for root in roots.iter().filter(|root| root.exists()) {
            let expected_source = root.join("message").join(session_id);
            if source_path == expected_source {
                let validated_root = canonicalize_existing_path(root, "session root")?;
                let validated_source = validated_root.join("message").join(session_id);
                return opencode::delete_session(&validated_root, &validated_source, session_id);
            }
        }
    }

    let validated_source = canonicalize_existing_path(source_path, "session source")?;

    let mut saw_existing_root = false;
    for root in roots {
        if !root.exists() {
            continue;
        }

        saw_existing_root = true;
        let validated_root = canonicalize_existing_path(root, "session root")?;
        if validated_source.starts_with(&validated_root) {
            return match provider_id {
                "codex" => codex::delete_session(&validated_root, &validated_source, session_id),
                "claude" => claude::delete_session(&validated_root, &validated_source, session_id),
                "opencode" => {
                    opencode::delete_session(&validated_root, &validated_source, session_id)
                }
                "pi" => pi::delete_session(&validated_root, &validated_source, session_id),
                _ => Err(format!("Unsupported provider: {provider_id}")),
            };
        }
    }

    if !saw_existing_root {
        return Err(format!(
            "Session root not found for provider {provider_id}: {}",
            roots
                .first()
                .map(|root| root.display().to_string())
                .unwrap_or_else(|| "<none>".to_string())
        ));
    }

    Err(format!(
        "Session source path is outside provider roots: {}",
        source_path.display()
    ))
}

fn provider_roots(provider_id: &str) -> Result<Vec<PathBuf>, String> {
    let roots = match provider_id {
        "codex" => codex::session_roots(),
        "claude" => vec![crate::session_paths::claude_config_dir().join("projects")],
        "gemini" => gemini::session_roots(),
        "copilot" => copilot::session_roots(),
        "opencode" => vec![opencode::get_opencode_data_dir()],
        "grokbuild" => grokbuild::session_roots(),
        "pi" => pi::session_roots(),
        "deepseek" => deepseek::session_roots(),
        "zcode" => vec![zcode::database_path()
            .parent()
            .ok_or("Invalid ZCode database path")?
            .to_path_buf()],
        "workbuddy" => workbuddy::session_roots(),
        "codebuddy" => codebuddy::session_roots(),
        "kimi" => kimi::session_roots(),
        "openclaw" => openclaw::session_roots(),
        _ => return Err(format!("Unsupported provider: {provider_id}")),
    };

    // 这些根同时是删除的白名单。其中的路径可能来自 `CODEX_HOME` /
    // `CLAUDE_CONFIG_DIR` / `GROK_CONFIG_DIR` / `XDG_DATA_HOME`，而它们不像
    // `PI_CODING_AGENT_DIR` 那样自带绝对路径校验：相对值会相对**进程 cwd** 解析，
    // 由启动方式（快捷方式、服务、终端）决定。浏览时扫到意外目录只是显示问题，
    // 删除时就会变成删错目录，所以这里拒绝相对根并让用户改用绝对路径。
    for root in &roots {
        if !root.is_absolute() {
            return Err(format!(
                "{provider_id} 的会话存储路径不是绝对路径：{}。请把对应环境变量改为绝对路径后重试。",
                root.display()
            ));
        }
    }

    Ok(roots)
}

fn canonicalize_existing_path(path: &Path, label: &str) -> Result<PathBuf, String> {
    if !path.exists() {
        return Err(format!("{label} not found: {}", path.display()));
    }

    path.canonicalize()
        .map_err(|e| format!("Failed to resolve {label} {}: {e}", path.display()))
}

fn collect_delete_session_outcomes<F>(
    requests: &[DeleteSessionRequest],
    mut deleter: F,
) -> Vec<DeleteSessionOutcome>
where
    F: FnMut(&DeleteSessionRequest) -> Result<DeleteSessionReply, String>,
{
    requests
        .iter()
        .map(|request| {
            let mut outcome = DeleteSessionOutcome {
                provider_id: request.provider_id.clone(),
                session_id: request.session_id.clone(),
                source_path: request.source_path.clone(),
                success: false,
                error: None,
                warnings: Vec::new(),
            };
            match deleter(request) {
                Ok(DeleteSessionReply::Deleted { warnings }) => {
                    outcome.success = true;
                    outcome.warnings = warnings;
                }
                // 记录本就不存在：批量场景下目标状态已达成，不算失败。
                Ok(DeleteSessionReply::NotFound) => {
                    outcome.success = true;
                }
                Ok(DeleteSessionReply::CleanupPending { warnings }) => {
                    outcome.error = Some("清理未完成，请退出 Agent 后重试".into());
                    outcome.warnings = warnings;
                }
                Err(error) => {
                    outcome.error = Some(error);
                }
            }
            outcome
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;

    fn write_codex_session(path: &Path, session_id: &str) {
        std::fs::write(
            path,
            format!(
                "{{\"timestamp\":\"2026-03-06T21:50:12Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session_id}\",\"cwd\":\"/tmp/project\"}}}}\n\
                 {{\"timestamp\":\"2026-03-06T21:50:13Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":\"hello\"}}}}\n",
            ),
        )
        .expect("write source");
    }

    /// 需要改 `CLAUDE_CONFIG_DIR` 的测试共用一把锁，避免互相把目录指走。
    fn claude_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// 走**真实**的批量删除入口 `delete_sessions`（不是逐条调 provider），
    /// 用一批 Claude 会话确认：选了几条就删掉几条。
    ///
    /// 这条测试是「多选只删掉一个」的直接对照：请求、扫描快照、逐条执行、
    /// 结果汇总全部走生产路径。
    #[test]
    #[allow(deprecated)] // set_var/remove_var 自 Rust 1.81 起弃用；由互斥锁串行化
    fn delete_sessions_removes_every_requested_session() {
        let _guard = claude_env_lock().lock().expect("lock");
        let temp = tempdir().expect("tempdir");
        let projects = temp.path().join("projects").join("sandbox-project");
        std::fs::create_dir_all(&projects).expect("create projects dir");

        let original = std::env::var_os("CLAUDE_CONFIG_DIR");
        std::env::set_var("CLAUDE_CONFIG_DIR", temp.path());

        let mut requests = Vec::new();
        let mut files = Vec::new();
        for index in 0..5 {
            let session_id = format!("sandbox-{index}");
            let path = projects.join(format!("{session_id}.jsonl"));
            std::fs::write(
                &path,
                concat!(
                    "{\"sessionId\":\"__ID__\",\"cwd\":\"/sandbox\",\"timestamp\":\"2026-03-06T10:00:00Z\"}\n",
                    "{\"message\":{\"role\":\"user\",\"content\":\"hi\"},\"timestamp\":\"2026-03-06T10:01:00Z\"}\n"
                )
                .replace("__ID__", &session_id),
            )
            .expect("write session");
            requests.push(DeleteSessionRequest {
                provider_id: "claude".to_string(),
                session_id: session_id.clone(),
                source_path: path.to_string_lossy().to_string(),
                include_project: false,
            });
            files.push(path);
        }

        // 扫描必须能看到这 5 条，否则 delete_sessions 会以「会话已变化」拒绝
        let scanned = scan_sessions()
            .into_iter()
            .filter(|item| item.provider_id == "claude")
            .count();
        assert!(
            scanned >= requests.len(),
            "扫描应发现全部 {0} 条沙箱会话，实际 {scanned}",
            requests.len()
        );

        // 生产路径必须传播进程检查拒绝，不能开始任何文件删除。
        let blocked = delete_sessions_with_check(&requests, |_| Err("agent running".into()));
        assert_eq!(blocked.len(), requests.len());
        assert!(blocked
            .iter()
            .all(|item| !item.success && item.error.as_deref() == Some("agent running")));
        assert!(files.iter().all(|path| path.exists()));

        // 此测试仅删除临时夹具；宿主进程状态由独立检查测试覆盖。
        // 不让本机恰好运行的 Claude/Node 进程造成随机失败。
        let outcomes = delete_sessions_with_check(&requests, |_| Ok(()));

        let failures = outcomes
            .iter()
            .filter(|outcome| !outcome.success)
            .map(|outcome| format!("{}: {:?}", outcome.session_id, outcome.error))
            .collect::<Vec<_>>();
        assert!(failures.is_empty(), "批量删除不应有失败项：{failures:?}");
        assert_eq!(outcomes.len(), requests.len(), "每条请求都要有结果");

        let remaining = files.iter().filter(|path| path.exists()).count();
        assert_eq!(
            remaining,
            0,
            "选中的 {0} 条都必须被删除，实际剩下 {remaining} 条",
            requests.len()
        );

        if let Some(value) = original {
            std::env::set_var("CLAUDE_CONFIG_DIR", value);
        } else {
            std::env::remove_var("CLAUDE_CONFIG_DIR");
        }
    }

    /// 文件型 provider 的正文源可以直接枚举；Codex 的正文源要先把分页链解析出来
    /// （链读不出来时正文本身也读不出来，那种情况下 `Transcript::cacheable` 为 false，
    /// 下发形态据此放弃截断）。
    #[test]
    fn file_backed_transcript_sources_are_enumerable() {
        let file = "/sessions/pi/session.jsonl";
        for provider in ["claude", "deepseek", "grokbuild", "pi"] {
            assert!(
                transcript_cache_sources(provider, Path::new(file)).is_some(),
                "{provider} 的正文源必须可枚举"
            );
        }
        // OpenCode 旧版消息目录不接入缓存
        assert!(transcript_cache_sources("opencode", Path::new(file)).is_none());
    }

    /// `provider_id` 与 `source_path` 都来自前端且互不绑定，SQLite 引用必须成对校验：
    /// 只按前缀分发会让 `opencode` 去解析 ZCode 的库引用。
    #[test]
    fn sqlite_sources_must_belong_to_their_provider() {
        let opencode_source = "sqlite:/data/opencode.db:ses_1";
        let zcode_source = "sqlite-zcode:/data/db.sqlite:session-1";

        assert_eq!(
            sqlite_source_kind(opencode_source),
            Some(SqliteSourceKind::OpenCode)
        );
        assert_eq!(
            sqlite_source_kind(zcode_source),
            Some(SqliteSourceKind::ZCode)
        );
        // `sqlite-zcode:` 不以 `sqlite:` 开头，两种引用不会互相误判
        assert!(checked_sqlite_source("opencode", opencode_source).is_ok());
        assert!(checked_sqlite_source("zcode", zcode_source).is_ok());

        // 跨 provider 混用必须被拒绝，而不是交给 provider 内部去撞错
        assert!(checked_sqlite_source("opencode", zcode_source).is_err());
        assert!(checked_sqlite_source("zcode", opencode_source).is_err());

        // 普通文件路径不属于 SQLite 引用，交给各自的路径校验处理
        assert_eq!(sqlite_source_kind("/data/storage/message/ses_1"), None);
        assert!(
            checked_sqlite_source("opencode", "/data/storage/message/ses_1")
                .expect("非 SQLite 来源放行")
                .is_none()
        );
    }

    #[test]
    fn compat_opencode_delete_excludes_other_database_and_legacy_residuals() {
        let source = "sqlite:/fixture/main.db:ses_same";
        let sessions = vec![
            session_meta("opencode", "ses_same", source, false),
            session_meta(
                "opencode",
                "ses_same",
                "sqlite:/fixture/dev.db:ses_same",
                true,
            ),
            session_meta(
                "opencode",
                "ses_same",
                "/fixture/storage/message/ses_same",
                true,
            ),
        ];
        assert!(
            collect_related_residual_sources(&sessions, "opencode", "ses_same", source, false)
                .is_empty()
        );
        let mut deleted = Vec::new();
        delete_session_in_snapshot_with(
            "opencode",
            "ses_same",
            source,
            false,
            &sessions,
            |_, _, path, _| {
                deleted.push(path.to_string());
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(deleted, vec![source]);
    }

    fn session_meta(
        provider_id: &str,
        session_id: &str,
        source_path: &str,
        residual: bool,
    ) -> SessionMeta {
        SessionMeta {
            provider_id: provider_id.to_string(),
            session_id: session_id.to_string(),
            residual,
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
            source_path: Some(source_path.to_string()),
            resume_command: None,
        }
    }

    /// 重试一份持久化的清理计划时，计划里已经删掉的条目不能再拿去预检，否则
    /// 每次重试都会卡在同一个「文件不存在」错误上，而磁盘上仍留着待清理的副本。
    /// Grok 的删除单位是目录，即使 summary 消失也仍需目录级预检和收尾。
    #[test]
    fn retry_skips_preflight_for_vanished_file_backed_sources() {
        for provider in ["codex", "pi"] {
            let missing = session_meta(provider, "session-1", "gone.jsonl", false);
            assert!(
                plan_entry_skips_preflight(&missing).expect("source path"),
                "{provider} 的已消失源必须跳过预检"
            );

            // 源还在就必须照常预检：跳过只适用于已经删掉的情况
            let temp = tempfile::tempdir().unwrap();
            let present_path = temp.path().join("present.jsonl");
            std::fs::write(&present_path, "{}").unwrap();
            let present = session_meta(
                provider,
                "session-1",
                &present_path.to_string_lossy(),
                false,
            );
            assert!(
                !plan_entry_skips_preflight(&present).expect("source path"),
                "{provider} 的现存源不能被跳过"
            );
        }
    }

    /// `sqlite:` / `sqlite-zcode:` 的 source_path 是引用而不是路径。把它当路径判断
    /// 存在性会得到 false，从而错误地跳过真正需要的父子依赖预检——那是防误删的关卡。
    #[test]
    fn retry_never_skips_preflight_for_sqlite_references() {
        for (provider, source) in [
            ("opencode", "sqlite:/data/opencode.db:ses_1"),
            ("zcode", "sqlite-zcode:/data/db.sqlite:session-1"),
            // DeepSeek 的预检本身要处理缺失源（重试时先清投影缓存），不能跳过
            ("deepseek", "/data/sessions/project/session.v3.jsonl"),
            ("grokbuild", "/data/sessions/project/session/summary.json"),
        ] {
            let item = session_meta(provider, "session-1", source, false);
            assert!(
                !plan_entry_skips_preflight(&item).expect("source path"),
                "{provider} 必须交给 provider 预检，不能按路径存在性跳过"
            );
        }
    }

    #[test]
    fn grok_cleanup_retry_removes_missing_summary_directories_from_exact_plan() {
        let temp = tempdir().unwrap();
        let roots = vec![
            temp.path().join("sessions"),
            temp.path().join("archived_sessions"),
        ];
        let sources = roots
            .iter()
            .map(|root| root.join("project/session-1/summary.json"))
            .collect::<Vec<_>>();
        for source in &sources {
            std::fs::create_dir_all(source.parent().unwrap()).unwrap();
            std::fs::write(source.parent().unwrap().join("sentinel"), "remaining").unwrap();
        }
        // A new same-ID copy absent from the saved plan must remain untouched.
        let unplanned = roots[0].join("other-project/session-1");
        std::fs::create_dir_all(&unplanned).unwrap();
        std::fs::write(unplanned.join("sentinel"), "keep").unwrap();
        let mut plan = sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                let mut item = session_meta(
                    "grokbuild",
                    "session-1",
                    &source.to_string_lossy(),
                    index > 0,
                );
                item.cleanup_pending = true;
                item
            })
            .collect::<Vec<_>>();
        for _ in 0..2 {
            preflight_grok_deletion(&plan, &roots).expect("preflight exact saved paths");
            let reply = delete_session_in_snapshot_with(
                "grokbuild",
                "session-1",
                &sources[0].to_string_lossy(),
                false,
                &plan,
                |provider, id, source, _| {
                    delete_session_with_roots(provider, id, Path::new(source), &roots)
                },
            )
            .expect("finish directory cleanup");
            assert!(
                matches!(reply, DeleteSessionReply::Deleted { warnings } if warnings.is_empty())
            );
            assert!(sources
                .iter()
                .all(|source| !source.parent().unwrap().exists()));
            assert!(unplanned.join("sentinel").exists());
        }
        plan[1].session_id = "wrong-id".into();
        assert!(preflight_grok_deletion(&plan, &roots).is_err());
        assert!(preflight_grok_deletion(&plan[..1], &[temp.path().to_path_buf()]).is_err());
    }

    #[test]
    fn grok_missing_summary_residual_reaches_directory_cleanup() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("sessions");
        let source = root.join("project/session-1/summary.json");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(source.parent().unwrap().join("sentinel"), "remaining").unwrap();
        let item = session_meta("grokbuild", "session-1", &source.to_string_lossy(), true);
        let reply = delete_session_in_snapshot_with(
            "grokbuild",
            "session-1",
            &source.to_string_lossy(),
            false,
            &[item],
            |provider, id, source, _| {
                delete_session_with_roots(
                    provider,
                    id,
                    Path::new(source),
                    std::slice::from_ref(&root),
                )
            },
        )
        .unwrap();
        assert!(matches!(reply, DeleteSessionReply::Deleted { warnings } if warnings.is_empty()));
        assert!(!source.parent().unwrap().exists());
    }

    #[test]
    fn grok_missing_summary_cleanup_failure_preserves_retry_warning() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("project/session-1/summary.json");
        let mut primary = session_meta("grokbuild", "session-1", &source.to_string_lossy(), false);
        primary.cleanup_pending = true;
        let mut residual = primary.clone();
        residual.residual = true;
        residual.source_path = Some(
            temp.path()
                .join("archive/session-1/summary.json")
                .to_string_lossy()
                .into_owned(),
        );
        assert!(delete_session_in_snapshot_with(
            "grokbuild",
            "session-1",
            &source.to_string_lossy(),
            false,
            std::slice::from_ref(&primary),
            |_, _, _, _| Err("primary directory still locked".into()),
        )
        .is_err());
        let mut calls = 0;
        let reply = delete_session_in_snapshot_with(
            "grokbuild",
            "session-1",
            &source.to_string_lossy(),
            false,
            &[primary, residual],
            |_, _, _, _| {
                calls += 1;
                if calls == 1 {
                    Ok(true)
                } else {
                    Err("directory still locked".into())
                }
            },
        )
        .unwrap();
        assert_eq!(calls, 2);
        // deletion::run keeps the journal for any warning from a related source.
        assert!(matches!(reply, DeleteSessionReply::Deleted { warnings } if warnings.len() == 1));
    }

    #[test]
    fn a_vanished_pi_session_does_not_block_a_cleanup_retry() {
        let plan = vec![session_meta(
            "pi",
            "session-1",
            "/nonexistent/pi/sessions/project/session-1.jsonl",
            false,
        )];

        preflight_deletion(&plan).expect("Pi 的已消失源同样必须放行重试");
    }

    #[test]
    fn current_session_deletion_collects_only_same_id_residual_copies() {
        let sessions = vec![
            session_meta("codex", "thread-1", "current.jsonl", false),
            session_meta("codex", "thread-1", "older.jsonl", true),
            session_meta("codex", "thread-2", "other.jsonl", true),
            session_meta("opencode", "thread-1", "legacy.json", true),
        ];

        let related = collect_related_residual_sources(
            &sessions,
            "codex",
            "thread-1",
            "current.jsonl",
            false,
        );

        assert_eq!(related, vec!["older.jsonl".to_string()]);
        assert!(collect_related_residual_sources(
            &sessions,
            "codex",
            "thread-1",
            "older.jsonl",
            true,
        )
        .is_empty());
    }

    #[test]
    fn deletes_opencode_residual_when_message_directory_is_already_missing() {
        let storage = tempdir().expect("tempdir");
        let session_dir = storage.path().join("session").join("project-1");
        std::fs::create_dir_all(&session_dir).expect("create session directory");
        let session_file = session_dir.join("ses_legacy.json");
        std::fs::write(&session_file, r#"{"id":"ses_legacy"}"#).expect("write legacy session");
        let missing_message_dir = storage.path().join("message").join("ses_legacy");

        let deleted = delete_session_with_roots(
            "opencode",
            "ses_legacy",
            &missing_message_dir,
            &[storage.path().to_path_buf()],
        )
        .expect("delete metadata-only residual");

        assert!(deleted);
        assert!(!session_file.exists());
    }

    #[test]
    fn accepts_source_path_under_any_allowed_provider_root() {
        let active_root = tempdir().expect("active root");
        let archived_root = tempdir().expect("archived root");
        let source = archived_root.path().join("session.jsonl");
        write_codex_session(&source, "archived-session");

        let deleted = delete_session_with_roots(
            "codex",
            "archived-session",
            &source,
            &[
                active_root.path().to_path_buf(),
                archived_root.path().to_path_buf(),
            ],
        )
        .expect("delete archived session");

        assert!(deleted);
        assert!(!source.exists());
    }

    /// 批量删除把请求按「子先父后」重排，但**绝不能因此丢掉任何一条**：
    /// 排序函数任何一处返回子集，用户就会看到「选了 N 条只删掉一部分」。
    /// 这里用一个超过一页（100 条）的混合批次固定住「结果是全排列」。
    #[test]
    fn batch_ordering_is_a_permutation_of_every_request() {
        let providers = [
            "codex",
            "claude",
            "opencode",
            "zcode",
            "grokbuild",
            "pi",
            "deepseek",
        ];
        let requests = (0..150)
            .map(|index| DeleteSessionRequest {
                provider_id: providers[index % providers.len()].to_string(),
                session_id: format!("session-{index}"),
                // 不存在的路径：各 provider 的排序会退化成「保持原序」，
                // 这正是要固定住的保底行为
                source_path: format!("/nonexistent/{index}.jsonl"),
                include_project: false,
            })
            .collect::<Vec<_>>();

        let order = codex::deletion_order(&requests);
        let order = deepseek::deletion_order(&requests, &order);
        let order = zcode::deletion_order(&requests, &order);
        let order = pi::deletion_order(&requests, &order);
        let order = opencode::deletion_order(&requests, &order);
        let order = grokbuild::deletion_order(&requests, &order);

        assert_eq!(
            order.len(),
            requests.len(),
            "重排后的条数必须与请求一致，否则部分会话会被静默跳过"
        );
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            (0..requests.len()).collect::<Vec<_>>(),
            "结果必须是 0..len 的全排列：不能重复、不能遗漏"
        );
    }

    /// 批量删除逐条执行；每条都必须独立成功，不受前一条的影响。
    #[test]
    fn deleting_many_codex_sessions_in_a_row_succeeds_for_every_one() {
        let root = tempdir().expect("tempdir");
        let sessions = (0..120)
            .map(|index| {
                let path = root.path().join(format!("session-{index}.jsonl"));
                write_codex_session(&path, &format!("session-{index}"));
                (format!("session-{index}"), path)
            })
            .collect::<Vec<_>>();

        for (session_id, path) in &sessions {
            let deleted =
                delete_session_with_roots("codex", session_id, path, &[root.path().to_path_buf()])
                    .unwrap_or_else(|error| panic!("删除 {session_id} 失败：{error}"));
            assert!(deleted, "{session_id} 必须被删除");
        }

        let remaining = sessions.iter().filter(|(_, path)| path.exists()).count();
        assert_eq!(remaining, 0, "逐条删除后不应留下任何会话文件");
    }

    #[test]
    fn rejects_source_path_outside_provider_root() {
        let root = tempdir().expect("tempdir");
        let outside = tempdir().expect("tempdir");
        let source = outside.path().join("session.jsonl");
        std::fs::write(&source, "{}").expect("write source");

        let err =
            delete_session_with_roots("codex", "session-1", &source, &[root.path().to_path_buf()])
                .expect_err("expected outside-root path to be rejected");

        assert!(err.contains("outside provider roots"));
    }

    #[test]
    fn rejects_missing_source_path() {
        let root = tempdir().expect("tempdir");
        let missing = root.path().join("missing.jsonl");

        let err =
            delete_session_with_roots("codex", "session-1", &missing, &[root.path().to_path_buf()])
                .expect_err("expected missing source path to fail");

        assert!(err.contains("session source not found"));
    }

    #[test]
    fn batch_delete_collects_successes_and_failures_in_order() {
        let requests = vec![
            DeleteSessionRequest {
                provider_id: "codex".to_string(),
                session_id: "s1".to_string(),
                source_path: "/tmp/s1".to_string(),
                include_project: false,
            },
            DeleteSessionRequest {
                provider_id: "claude".to_string(),
                session_id: "s2".to_string(),
                source_path: "/tmp/s2".to_string(),
                include_project: false,
            },
            DeleteSessionRequest {
                provider_id: "gemini".to_string(),
                session_id: "s3".to_string(),
                source_path: "/tmp/s3".to_string(),
                include_project: false,
            },
            DeleteSessionRequest {
                provider_id: "codex".to_string(),
                session_id: "s4".to_string(),
                source_path: "/tmp/s4".to_string(),
                include_project: false,
            },
        ];

        let outcomes = collect_delete_session_outcomes(&requests, |request| {
            match request.session_id.as_str() {
                "s1" => Ok(DeleteSessionReply::Deleted {
                    warnings: vec!["索引清理失败".to_string()],
                }),
                "s2" => Err("boom".to_string()),
                "s3" => Ok(DeleteSessionReply::NotFound),
                _ => Ok(DeleteSessionReply::CleanupPending {
                    warnings: vec!["index locked".to_string()],
                }),
            }
        });

        assert_eq!(outcomes.len(), 4);
        // 已删除：成功，且 warnings 原样透传给前端做降级提示
        assert!(outcomes[0].success);
        assert_eq!(outcomes[0].error, None);
        assert_eq!(outcomes[0].warnings, vec!["索引清理失败".to_string()]);
        assert!(!outcomes[1].success);
        assert_eq!(outcomes[1].error.as_deref(), Some("boom"));
        // 记录本就不存在：目标状态已达成，不算失败
        assert!(outcomes[2].success);
        assert_eq!(outcomes[2].error, None);
        // 未完成清理：失败结果保留警告，供前端提示重试
        assert!(!outcomes[3].success);
        assert_eq!(outcomes[3].warnings, vec!["index locked".to_string()]);
        assert!(outcomes[3]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("清理未完成")));
    }

    #[test]
    fn legacy_project_delete_is_rejected_before_touching_any_file() {
        let project = tempdir().expect("tempdir");
        let sentinel = project.path().join("valuable.txt");
        std::fs::write(&sentinel, "keep").unwrap();
        let mut target = session_meta("codex", "thread-1", "missing.jsonl", false);
        target.project_dir = Some(project.path().to_string_lossy().into_owned());
        let error =
            delete_session_in_snapshot("codex", "thread-1", "missing.jsonl", true, &[target])
                .expect_err("legacy directory deletion must be rejected");
        assert!(error.contains("不再删除项目目录"));
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep");
    }

    #[test]
    fn stale_residual_files_deleted_by_an_earlier_batch_item_are_not_failures() {
        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("older.jsonl");
        let mut stale = session_meta("codex", "thread-1", "gone.jsonl", true);
        stale.project_dir = Some(temp.path().to_string_lossy().to_string());
        // 快照里仍在、磁盘上已消失的文件型残留（同批前项级联清理过）
        assert!(!source.exists());

        let reply = delete_session_in_snapshot("codex", "thread-1", "gone.jsonl", false, &[stale])
            .expect("stale residual short-circuits to NotFound");

        assert!(matches!(reply, DeleteSessionReply::NotFound));
    }

    #[test]
    fn lexical_source_validation_rejects_relative_parent_and_unconfigured_unc_paths() {
        let temp = tempdir().expect("tempdir");
        let roots = vec![temp.path().to_path_buf()];

        assert!(validate_lexical_source_path(Path::new("relative/session.jsonl"), &roots).is_err());
        assert!(
            validate_lexical_source_path(&temp.path().join("..").join("escape.jsonl"), &roots)
                .is_err()
        );
        assert!(validate_lexical_source_path(&temp.path().join("session.jsonl"), &roots).is_ok());

        #[cfg(windows)]
        {
            assert!(validate_lexical_source_path(
                Path::new(r"\\198.51.100.1\share\session.jsonl"),
                &roots
            )
            .is_err());
            assert!(validate_lexical_source_path(Path::new(r"\\.\PIPE\evil"), &roots).is_err());
            let unc_roots = vec![PathBuf::from(r"\\nas.local\homes\alice\.codex\sessions")];
            assert!(validate_lexical_source_path(
                Path::new(r"\\NAS.LOCAL\HOMES\alice\.codex\sessions\rollout.jsonl"),
                &unc_roots
            )
            .is_ok());
        }
    }

    #[test]
    fn empty_batch_delete_returns_immediately_and_search_messages_matches_all_terms() {
        assert!(
            delete_sessions_with_check(&[], |_| panic!("must not check empty batch")).is_empty()
        );

        let messages = vec![
            SessionMessage {
                role: "user".into(),
                content: "Fix Rust TranscriptCache race".into(),
                ..SessionMessage::default()
            },
            SessionMessage {
                role: "assistant".into(),
                content: "Updated transcriptcache generation counter".into(),
                ..SessionMessage::default()
            },
            SessionMessage {
                role: "tool".into(),
                content: "Cargo check passed".into(),
                ..SessionMessage::default()
            },
        ];
        assert_eq!(
            search_messages(&messages, "  transcriptcache   rust "),
            vec![0]
        );
        assert_eq!(search_messages(&messages, "TRANSCRIPTCACHE"), vec![0, 1]);
        assert!(search_messages(&messages, "   ").is_empty());
    }

    #[cfg(unix)]
    fn create_file_symlink_mod(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_file_symlink_mod(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_file(target, link)
    }

    #[cfg(not(any(unix, windows)))]
    fn create_file_symlink_mod(_target: &Path, _link: &Path) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "unsupported platform",
        ))
    }

    #[test]
    fn claude_and_pi_reject_symlink_sources_and_claude_ghost_deletion_reaches_provider() {
        let temp = tempdir().expect("tempdir");
        let claude_root = temp.path().join("projects");
        let project = claude_root.join("demo-proj");
        std::fs::create_dir_all(&project).unwrap();

        let real_claude = project.join("real-session.jsonl");
        std::fs::write(
            &real_claude,
            "{\"sessionId\":\"real-session\",\"cwd\":\"/demo\",\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        )
        .unwrap();

        let link_claude = project.join("link-session.jsonl");
        if create_file_symlink_mod(&real_claude, &link_claude).is_ok() {
            assert!(delete_session_with_roots(
                "claude",
                "real-session",
                &link_claude,
                std::slice::from_ref(&claude_root),
            )
            .is_err());
            assert!(
                real_claude.exists(),
                "real Claude session must survive symlink delete attempt"
            );
        }

        let pi_root = temp.path().join("pi-sessions");
        std::fs::create_dir_all(&pi_root).unwrap();
        let real_pi = pi_root.join("pi-real.jsonl");
        std::fs::write(
            &real_pi,
            "{\"type\":\"session\",\"version\":3,\"id\":\"pi-real\",\"cwd\":\"/demo\"}\n",
        )
        .unwrap();
        let link_pi = pi_root.join("pi-link.jsonl");
        if create_file_symlink_mod(&real_pi, &link_pi).is_ok() {
            assert!(delete_session_with_roots(
                "pi",
                "pi-real",
                &link_pi,
                std::slice::from_ref(&pi_root),
            )
            .is_err());
            assert!(
                real_pi.exists(),
                "real Pi session must survive symlink delete attempt"
            );
        }

        // Claude sessions-index.json ghost (residual: true, .jsonl missing on disk) reaches provider cleanup
        let ghost_id = "ghost-session-1";
        let ghost_source = project.join(format!("{ghost_id}.jsonl"));
        let index_path = project.join("sessions-index.json");
        std::fs::write(
            &index_path,
            serde_json::json!({
                "version": 1,
                "entries": [{ "sessionId": ghost_id, "firstPrompt": "orphaned" }]
            })
            .to_string(),
        )
        .unwrap();

        let ghost_meta = session_meta("claude", ghost_id, &ghost_source.to_string_lossy(), true);
        let reply = delete_session_in_snapshot_with(
            "claude",
            ghost_id,
            &ghost_source.to_string_lossy(),
            false,
            std::slice::from_ref(&ghost_meta),
            |provider, id, source, _| {
                delete_session_with_roots(
                    provider,
                    id,
                    Path::new(source),
                    std::slice::from_ref(&claude_root),
                )
            },
        )
        .expect("ghost cleanup should succeed");
        assert!(matches!(reply, DeleteSessionReply::Deleted { warnings } if warnings.is_empty()));
        let updated_index: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&index_path).unwrap()).unwrap();
        assert!(updated_index["entries"].as_array().unwrap().is_empty());

        // WorkBuddy sqlite-workbuddy: source ownership & symlink source rejection
        assert_eq!(
            checked_sqlite_source("workbuddy", "sqlite-workbuddy:C:/wb/workbuddy.db:s1").unwrap(),
            Some(SqliteSourceKind::WorkBuddy)
        );
        assert!(
            checked_sqlite_source("opencode", "sqlite-workbuddy:C:/wb/workbuddy.db:s1").is_err()
        );
        assert!(checked_sqlite_source("workbuddy", "sqlite-zcode:C:/z/db.sqlite:s1").is_err());

        let wb_home = temp.path().join(".workbuddy-ai");
        let wb_proj = wb_home.join("projects").join("demo");
        std::fs::create_dir_all(&wb_proj).unwrap();
        let real_wb = wb_proj.join("11111111-1111-4111-8111-111111111111.jsonl");
        std::fs::write(&real_wb, "{}\n").unwrap();
        let link_wb = wb_proj.join("22222222-2222-4222-8222-222222222222.jsonl");
        if create_file_symlink_mod(&real_wb, &link_wb).is_ok() {
            assert!(delete_session_with_roots(
                "workbuddy",
                "22222222-2222-4222-8222-222222222222",
                &link_wb,
                std::slice::from_ref(&wb_home),
            )
            .is_err());
            assert!(real_wb.exists());
        }
    }
}
