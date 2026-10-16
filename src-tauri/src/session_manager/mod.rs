pub mod cache;
mod deletion;
pub mod providers;
pub mod terminal;
pub mod wire;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use providers::{claude, codex, deepseek, grokbuild, opencode, pi, zcode};

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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMessage {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<i64>,
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

fn scan_report() -> SessionScanReport {
    let (r1, r2, r3, r4, r5, r6, r7) = std::thread::scope(|s| {
        let h1 = s.spawn(|| providers::utils::scan_with_diagnostics("codex", codex::scan_sessions));
        let h2 =
            s.spawn(|| providers::utils::scan_with_diagnostics("claude", claude::scan_sessions));
        let h3 = s
            .spawn(|| providers::utils::scan_with_diagnostics("opencode", opencode::scan_sessions));
        let h4 = s.spawn(|| {
            providers::utils::scan_with_diagnostics("grokbuild", grokbuild::scan_sessions)
        });
        let h5 = s.spawn(|| providers::utils::scan_with_diagnostics("pi", pi::scan_sessions));
        let h6 = s.spawn(|| providers::utils::scan_with_diagnostics("zcode", zcode::scan_sessions));
        let h7 = s
            .spawn(|| providers::utils::scan_with_diagnostics("deepseek", deepseek::scan_sessions));
        (
            join_scan_results(h1),
            join_scan_results(h2),
            join_scan_results(h3),
            join_scan_results(h4),
            join_scan_results(h5),
            join_scan_results(h6),
            join_scan_results(h7),
        )
    });

    let mut sessions = Vec::new();
    let mut warnings = Vec::new();
    for (items, issues) in [r1, r2, r3, r4, r5, r6, r7] {
        sessions.extend(items);
        warnings.extend(issues);
    }

    if let Ok(pending) = deletion::pending_sessions() {
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

/// 校验 `source_path` 归属并返回规范化路径；SQLite 源没有文件路径，由调用方先行分发。
fn validated_file_source(provider_id: &str, source_path: &str) -> Result<PathBuf, String> {
    let roots = provider_roots(provider_id)?;
    let canonical = canonicalize_existing_path(Path::new(source_path), "session source")?;
    if !roots.iter().any(|root| {
        root.canonicalize()
            .is_ok_and(|root| canonical.starts_with(root))
    }) {
        return Err("Session source is outside configured storage".into());
    }
    Ok(canonical)
}

fn dispatch_messages(provider_id: &str, path: &Path) -> Result<Vec<SessionMessage>, String> {
    match provider_id {
        "codex" => codex::load_messages(path),
        "claude" => claude::load_messages(path),
        "opencode" => opencode::load_messages(path),
        "grokbuild" => grokbuild::load_messages(path),
        "pi" => pi::load_messages(path),
        "deepseek" => deepseek::load_messages(path),
        _ => Err(format!("Unsupported provider: {provider_id}")),
    }
}

/// 正文解析结果可缓存的 provider：返回正文实际依赖的文件（有序）与每段只读到的偏移。
///
/// 少列一个文件、或漏掉链的顺序/截断偏移，都会命中过期正文，因此只接受已经逐行核对
/// 过读取范围的 provider：
/// - `claude` / `deepseek` / `pi`：正文就是源文件本身
/// - `grokbuild`：正文在同目录 `chat_history.jsonl`，而 `summary.json` 只在一轮
///   结束时改写，只盯后者会让一轮进行中的会话命中旧正文
/// - `codex`：正文是「当前文件 + 基座文件的前 N 字节」拼起来的分页历史链
///
/// SQLite 源（`opencode` / `zcode`）在这里拿不到路径，由 `load_transcript` 的 SQLite
/// 分支处理。`opencode` 的旧版消息目录不接入：列出兄弟 `part/` 目录需要先读
/// 消息文件取 id，等于把读取本身做一遍。`pi` 扫描期的 `residual` 标志依赖父会话文件是否
/// 存在，那是元数据的顾虑，与正文无关。
fn transcript_cache_sources(provider_id: &str, path: &Path) -> Option<cache::TranscriptSources> {
    match provider_id {
        "claude" | "deepseek" | "pi" => Some(vec![(path.to_path_buf(), None)]),
        "grokbuild" => {
            let mut sources = vec![(path.to_path_buf(), None)];
            if let Some(parent) = path.parent() {
                sources.push((parent.join("chat_history.jsonl"), None));
            }
            Some(sources)
        }
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
}

fn sqlite_source_kind(source_path: &str) -> Option<SqliteSourceKind> {
    if source_path.starts_with("sqlite-zcode:") {
        Some(SqliteSourceKind::ZCode)
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
                || load().map(|messages| cache::Transcript::new(messages).cached()),
            )
            .map(|(transcript, _cached)| transcript),
        // 指纹里的文件都读不到（库被移走、权限不足）：交给读取器报真正的错误
        Err(_) => Ok(Arc::new(cache::Transcript::new(load()?))),
    }
}

pub fn checked_list_sessions() -> Result<SessionScanReport, String> {
    deletion::pending_sessions()?;
    Ok(scan_report())
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
    deletion::pending_sessions()?;
    let sessions = scan_sessions();
    let target = sessions
        .iter()
        .find(|item| {
            item.provider_id == provider_id
                && item.session_id == session_id
                && item.source_path.as_deref() == Some(source_path)
        })
        .ok_or("会话已变化或不存在，请重新扫描")?;
    let reply = deletion::run(target, &sessions, preflight_deletion, |plan| {
        delete_session_in_snapshot(provider_id, session_id, source_path, include_project, plan)
    });
    // 删除会级联清理同 ID 的历史副本与索引，正文缓存整体作废
    cache::global().clear();
    reply
}

/// 清理计划里的这一条目是否已经不需要（也无法）预检。
///
/// 持久化的清理计划在原样重试时，可能包含**已经被删掉**的条目：主会话删除成功、
/// 但同 ID 的历史副本清理失败时，journal 会保留整个原始计划，好让重试可以把剩余
/// 副本收尾。对「正文就是一个文件」的 provider（Codex 的 rollout、Pi 的 jsonl、
/// Grok Build 的 summary.json），这些条目已经没有可校验的内容；再去读一次已经消失
/// 的文件只会让每次重试都卡在同一个错误上，而磁盘上确实还有副本等着清理。
///
/// 只对文件型 provider 成立：`sqlite:` / `sqlite-zcode:` 的 `source_path` 是引用
/// 而不是路径，把它当路径判断存在性会得到 false，从而错误地跳过真正需要的预检。
/// 这些 provider 的预检本身要能处理缺失源（DeepSeek 会清理投影缓存；OpenCode 的
/// `checked_storage_child` 对缺失目标也成立），所以不在这里按存在性跳过。
fn plan_entry_skips_preflight(item: &SessionMeta) -> Result<bool, String> {
    if !matches!(item.provider_id.as_str(), "codex" | "pi" | "grokbuild") {
        return Ok(false);
    }
    let path = item.source_path.as_deref().ok_or("缺少会话路径")?;
    Ok(!Path::new(path).exists())
}

fn preflight_deletion(plan: &[SessionMeta]) -> Result<(), String> {
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
    for item in plan.iter().filter(|item| item.provider_id == "deepseek") {
        let path = Path::new(item.source_path.as_deref().ok_or("缺少会话路径")?);
        let root = provider_roots("deepseek")?
            .into_iter()
            .find(|root| root.exists())
            .ok_or("DeepSeek Harness 会话根目录不存在")?;
        deepseek::preflight_delete(&root, path, &item.session_id)?;
    }
    for item in plan.iter().filter(|item| item.provider_id == "zcode") {
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
        opencode::preflight_delete(
            item.source_path.as_deref().ok_or("缺少会话路径")?,
            &item.session_id,
        )?;
    }
    for item in plan.iter().filter(|item| item.provider_id == "grokbuild") {
        if plan_entry_skips_preflight(item)? {
            continue;
        }
        let path = Path::new(item.source_path.as_deref().ok_or("缺少会话路径")?);
        grokbuild::preflight_delete(path, &item.session_id)?;
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
    let selected = sessions.iter().find(|item| {
        item.provider_id == provider_id
            && item.session_id == session_id
            && item.source_path.as_deref() == Some(source_path)
    });

    if include_project {
        return Err("会话清理不再删除项目目录；请使用文件管理器单独处理项目文件".into());
    }
    if selected.is_some_and(|item| item.residual && !item.cleanup_pending)
        && matches!(
            provider_id,
            "codex" | "claude" | "grokbuild" | "pi" | "deepseek"
        )
        && !Path::new(source_path).exists()
    {
        return Ok(DeleteSessionReply::NotFound);
    }
    let project_dir = selected.and_then(|item| item.project_dir.clone());
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
        codex::cleanup_after_delete(session_id, None, false)?;
    }
    // DeepSeek is deliberately excluded: its provider is the authority for a
    // retry whose transcript is already gone but whose projection cache still
    // needs work (see `delete_session_with_roots`), so it must reach
    // `delete_session_record` even when `source_path` no longer exists.
    let missing_file = matches!(provider_id, "codex" | "claude" | "grokbuild" | "pi")
        && !Path::new(source_path).exists();
    let deleted = if missing_file {
        if selected.is_some_and(|item| item.cleanup_pending || item.residual) {
            true
        } else {
            return Err("会话文件已消失，请重新扫描".into());
        }
    } else {
        delete_session_record(
            provider_id,
            session_id,
            source_path,
            project_dir.as_deref(),
            false,
        )?
    };
    let mut warnings = Vec::new();

    // Deleting a current conversation means deleting that conversation as a
    // whole. Historical rollouts and legacy-format copies with the same native
    // ID must not surface as new leftovers afterward. Selecting a residual
    // entry itself remains deliberately scoped to that one entry.
    if deleted || selected.is_some_and(|item| item.cleanup_pending) {
        for residual_source in related_residual_sources {
            if matches!(provider_id, "codex" | "claude" | "grokbuild" | "pi")
                && !Path::new(&residual_source).exists()
            {
                continue;
            }
            if let Err(error) = delete_session_record(
                provider_id,
                session_id,
                &residual_source,
                project_dir.as_deref(),
                false,
            ) {
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
        })
        .filter_map(|item| item.source_path.clone())
        .collect()
}

fn delete_session_record(
    provider_id: &str,
    session_id: &str,
    source_path: &str,
    project_dir: Option<&str>,
    include_project: bool,
) -> Result<bool, String> {
    // SQLite sessions bypass the file-based deletion path.
    let deleted = if provider_id == "opencode" && source_path.starts_with("sqlite:") {
        opencode::delete_session_sqlite(session_id, source_path)?
    } else if provider_id == "zcode" && source_path.starts_with("sqlite-zcode:") {
        zcode::delete_session(session_id, source_path, project_dir, include_project)?
    } else {
        let roots = provider_roots(provider_id)?;
        delete_session_with_roots(provider_id, session_id, Path::new(source_path), &roots)?
    };
    Ok(deleted)
}

pub fn delete_sessions(requests: &[DeleteSessionRequest]) -> Vec<DeleteSessionOutcome> {
    let guard = deletion::DELETE_LOCK.lock();
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
    let outcomes = collect_delete_session_outcomes(&ordered, |request| {
        if guard.is_err() {
            return Err("清理锁不可用，请重启管理器".into());
        }
        if request.include_project {
            return Err("批量清理不删除项目目录".into());
        }
        deletion::ensure_agent_stopped(&request.provider_id)?;
        deletion::pending_sessions()?;
        let Some(target) = sessions.iter().find(|item| {
            item.provider_id == request.provider_id
                && item.session_id == request.session_id
                && item.source_path.as_deref() == Some(request.source_path.as_str())
        }) else {
            return Err("会话已变化或不存在，请重新扫描".into());
        };
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
    outcomes.into_iter().map(|(_, outcome)| outcome).collect()
}

fn delete_session_with_roots(
    provider_id: &str,
    session_id: &str,
    source_path: &Path,
    roots: &[PathBuf],
) -> Result<bool, String> {
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
                "grokbuild" => {
                    grokbuild::delete_session(&validated_root, &validated_source, session_id)
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
        "opencode" => vec![opencode::get_opencode_data_dir()],
        "grokbuild" => grokbuild::session_roots(),
        "pi" => pi::session_roots(),
        "deepseek" => deepseek::session_roots(),
        "zcode" => vec![zcode::database_path()
            .parent()
            .ok_or("Invalid ZCode database path")?
            .to_path_buf()],
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

        let outcomes = delete_sessions(&requests);

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
    /// 这正是 Grok Build（主目录删成功、同 ID 归档副本删失败）和 Pi 的死锁成因。
    #[test]
    fn retry_skips_preflight_for_vanished_file_backed_sources() {
        for provider in ["codex", "pi", "grokbuild"] {
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
        ] {
            let item = session_meta(provider, "session-1", source, false);
            assert!(
                !plan_entry_skips_preflight(&item).expect("source path"),
                "{provider} 必须交给 provider 预检，不能按路径存在性跳过"
            );
        }
    }

    #[test]
    fn a_vanished_grokbuild_primary_does_not_block_a_cleanup_retry() {
        // 这是死锁的完整入口：journal 原样保留了「主会话 + 同 ID 归档副本」的计划，
        // 主会话的 summary.json 已经不在了。预检必须放行，好让残留副本继续清理。
        // 旧实现会在这里读第一个 summary.json 并报「文件不存在」，于是每次重试都
        // 卡在同一处，磁盘上的副本永远清不掉。
        let plan = vec![
            session_meta(
                "grokbuild",
                "session-1",
                "/nonexistent/sessions/project/session-1/summary.json",
                false,
            ),
            session_meta(
                "grokbuild",
                "session-1",
                "/nonexistent/archived_sessions/project/session-1/summary.json",
                true,
            ),
        ];

        preflight_deletion(&plan).expect("已消失的主会话不能让整个重试永久失败");
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
}
