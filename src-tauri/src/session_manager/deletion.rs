//! Durable, idempotent deletion bookkeeping. Pending operations remain visible
//! even when an Agent's primary record has already disappeared.
use super::{DeleteSessionReply, SessionMeta};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

pub static DELETE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Serialize, Deserialize)]
struct PendingDeletion {
    targets: Vec<SessionMeta>,
    #[serde(default)]
    codex_plan: Option<super::providers::codex::DeletionPlan>,
    #[serde(default)]
    last_error: Option<String>,
}

const KNOWN_PROVIDERS: &[&str] = &[
    "codex",
    "claude",
    "gemini",
    "copilot",
    "opencode",
    "zcode",
    "grokbuild",
    "pi",
    "deepseek",
    "workbuddy",
    "codebuddy",
    "kimi",
    "openclaw",
];

fn directory() -> Result<PathBuf, String> {
    dirs::data_local_dir()
        .map(|p| p.join("AgentSessionManager").join("pending-deletions"))
        .ok_or_else(|| "无法定位清理任务记录目录".into())
}

fn operation_path(target: &SessionMeta) -> Result<PathBuf, String> {
    let mut hash = 0xcbf29ce484222325u64;
    for part in [
        target.provider_id.as_str(),
        target.session_id.as_str(),
        target.source_path.as_deref().unwrap_or(""),
    ] {
        for byte in part.as_bytes().iter().copied().chain(std::iter::once(0)) {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }
    let dir = directory()?;
    let legacy = dir.join(format!("{hash:016x}.json"));
    if legacy.exists() || !KNOWN_PROVIDERS.contains(&target.provider_id.as_str()) {
        return Ok(legacy);
    }
    Ok(dir.join(format!("{}-{hash:016x}.json", target.provider_id)))
}

#[derive(Debug, Clone)]
struct JournalWarning {
    provider_id: Option<String>,
    message: String,
}

fn infer_journal_provider(path: &Path, raw_text: Option<&str>) -> Option<String> {
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        if let Some((prefix, hash)) = stem.rsplit_once('-') {
            if hash.len() == 16
                && hash.bytes().all(|b| b.is_ascii_hexdigit())
                && KNOWN_PROVIDERS.contains(&prefix)
            {
                return Some(prefix.to_string());
            }
        }
    }
    raw_text
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .and_then(|val| {
            val.get("targets")?
                .as_array()?
                .first()?
                .get("providerId")?
                .as_str()
                .filter(|p| KNOWN_PROVIDERS.contains(p))
                .map(str::to_string)
        })
}

/// 删除路径保持严格：任何一份清理记录读不出都拒绝继续操作，避免丢失重试依据。
#[allow(dead_code)]
pub fn pending_sessions() -> Result<Vec<SessionMeta>, String> {
    strict_pending_sessions(pending_sessions_report()?)
}

/// 按 provider 隔离损坏记录的阻断范围：可归属到其他 Agent 的损坏记录只阻断对应 Agent，
/// 无法识别归属的损坏记录继续对所有 Agent 保守阻断。
pub fn ensure_pending_journals_healthy(provider_id: Option<&str>) -> Result<(), String> {
    let root = directory()?;
    ensure_pending_journals_healthy_in(&root, provider_id)
}

fn ensure_pending_journals_healthy_in(
    root: &Path,
    provider_id: Option<&str>,
) -> Result<(), String> {
    let (_, issues) = pending_sessions_detailed_in(root)?;
    let blocking: Vec<String> = issues
        .into_iter()
        .filter(|issue| match (&issue.provider_id, provider_id) {
            (Some(corrupt_provider), Some(requested)) => corrupt_provider == requested,
            _ => true,
        })
        .map(|issue| issue.message)
        .collect();
    if blocking.is_empty() {
        Ok(())
    } else {
        Err(blocking.join("；"))
    }
}

fn strict_pending_sessions(
    (sessions, warnings): (Vec<SessionMeta>, Vec<String>),
) -> Result<Vec<SessionMeta>, String> {
    if warnings.is_empty() {
        Ok(sessions)
    } else {
        Err(warnings.join("；"))
    }
}

/// 浏览路径容错：损坏的记录只转成警告，不让其他正常 Agent 的会话列表整体失败。
/// 返回可展示的待清理条目与每份损坏记录的警告。
pub fn pending_sessions_report() -> Result<(Vec<SessionMeta>, Vec<String>), String> {
    let root = directory()?;
    pending_sessions_report_in(&root)
}

fn pending_sessions_report_in(root: &Path) -> Result<(Vec<SessionMeta>, Vec<String>), String> {
    let (sessions, issues) = pending_sessions_detailed_in(root)?;
    Ok((
        sessions,
        issues.into_iter().map(|issue| issue.message).collect(),
    ))
}

fn pending_sessions_detailed_in(
    root: &Path,
) -> Result<(Vec<SessionMeta>, Vec<JournalWarning>), String> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new()));
        }
        Err(error) => return Err(error.to_string()),
    };
    let mut sessions = Vec::new();
    let mut warnings = Vec::new();
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                warnings.push(JournalWarning {
                    provider_id: None,
                    message: format!("无法枚举清理记录目录 {}：{error}", root.display()),
                });
                continue;
            }
        };
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let raw_text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                warnings.push(JournalWarning {
                    provider_id: infer_journal_provider(&path, None),
                    message: format!(
                        "清理记录 {} 无法读取（{error}），已暂停相关删除；请先核对后再操作",
                        path.display()
                    ),
                });
                continue;
            }
        };
        let operation = match serde_json::from_str::<PendingDeletion>(&raw_text) {
            Ok(operation) => operation,
            Err(error) => {
                warnings.push(JournalWarning {
                    provider_id: infer_journal_provider(&path, Some(&raw_text)),
                    message: format!(
                        "清理记录 {} 无法读取（{error}），已暂停相关删除；请先核对后再操作",
                        path.display()
                    ),
                });
                continue;
            }
        };
        let valid_primary = operation.targets.first().is_some_and(|first| {
            !first.provider_id.trim().is_empty()
                && !first.session_id.trim().is_empty()
                && first
                    .source_path
                    .as_deref()
                    .is_some_and(|source| !source.trim().is_empty())
        });
        if !valid_primary {
            let inferred = operation
                .targets
                .first()
                .map(|first| first.provider_id.as_str())
                .filter(|p| KNOWN_PROVIDERS.contains(p))
                .map(str::to_string)
                .or_else(|| infer_journal_provider(&path, Some(&raw_text)));
            warnings.push(JournalWarning {
                provider_id: inferred,
                message: format!(
                    "清理记录 {} 无法读取（缺少有效目标或会话路径），已暂停相关删除；请先核对后再操作",
                    path.display()
                ),
            });
            continue;
        }
        // One operation has one visible retry entry. Related historical copies
        // belong to its persisted plan, not to independently retryable tasks.
        for mut target in operation.targets.into_iter().take(1) {
            target.cleanup_pending = true;
            if let Some(error) = &operation.last_error {
                target.summary = Some(format!("待清理：{error}"));
            }
            sessions.push(target);
        }
    }
    Ok((sessions, warnings))
}

pub fn run<P, F>(
    target: &SessionMeta,
    snapshot: &[SessionMeta],
    preflight: P,
    action: F,
) -> Result<DeleteSessionReply, String>
where
    P: FnOnce(&[SessionMeta]) -> Result<(), String>,
    F: FnOnce(&[SessionMeta]) -> Result<DeleteSessionReply, String>,
{
    let path = operation_path(target)?;
    run_at(&path, target, snapshot, preflight, action)
}

fn run_at<P, F>(
    path: &Path,
    target: &SessionMeta,
    snapshot: &[SessionMeta],
    preflight: P,
    action: F,
) -> Result<DeleteSessionReply, String>
where
    P: FnOnce(&[SessionMeta]) -> Result<(), String>,
    F: FnOnce(&[SessionMeta]) -> Result<DeleteSessionReply, String>,
{
    let is_new = !path.exists();
    let mut operation: PendingDeletion = if is_new {
        let mut targets = vec![target.clone()];
        if !target.residual {
            targets.extend(
                snapshot
                    .iter()
                    .filter(|item| {
                        item.provider_id == target.provider_id
                            && item.session_id == target.session_id
                            && item.residual
                            && item.source_path != target.source_path
                            && (target.provider_id != "opencode"
                                || item
                                    .source_path
                                    .as_deref()
                                    .zip(target.source_path.as_deref())
                                    .is_some_and(|(a, b)| {
                                        super::providers::opencode::same_storage(a, b)
                                    }))
                    })
                    .cloned(),
            );
        }
        PendingDeletion {
            targets,
            codex_plan: None,
            last_error: None,
        }
    } else {
        serde_json::from_str(&std::fs::read_to_string(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?
    };
    if !operation.targets.first().is_some_and(|saved| {
        saved.provider_id == target.provider_id
            && saved.session_id == target.session_id
            && saved.source_path == target.source_path
    }) {
        return Err("清理记录与目标不一致，已停止操作".into());
    }
    if target.provider_id == "opencode"
        && operation.targets.iter().any(|saved| {
            saved.provider_id != target.provider_id
                || !saved
                    .source_path
                    .as_deref()
                    .zip(target.source_path.as_deref())
                    .is_some_and(|(a, b)| super::providers::opencode::same_storage(a, b))
        })
    {
        return Err(
            "OpenCode saved deletion plan spans different storage sources; rescan required".into(),
        );
    }
    // Rejections are ordinary errors, not partially completed deletions. A
    // retry retains its existing journal, but a rejected new request writes none.
    preflight(&operation.targets)?;
    if is_new {
        persist_operation(path, &operation)?;
    }
    for saved in &mut operation.targets {
        saved.cleanup_pending = true;
    }
    // Retry exactly the original plan even after scans stop finding a deleted
    // primary record. Never add new same-ID copies to an existing operation.
    let reply = match action(&operation.targets) {
        Err(error) => return mark_pending(path, vec![error]),
        Ok(DeleteSessionReply::Deleted { warnings }) if !warnings.is_empty() => {
            return mark_pending(path, warnings)
        }
        Ok(reply) => reply,
    };
    if matches!(
        reply,
        DeleteSessionReply::Deleted { .. } | DeleteSessionReply::NotFound
    ) {
        if let Err(error) = std::fs::remove_file(path) {
            return Ok(DeleteSessionReply::CleanupPending {
                warnings: vec![format!("清理已完成，但无法更新清理记录：{error}")],
            });
        }
    }
    Ok(reply)
}

/// Codex pagination expands beyond the visible scan snapshot. Persist those
/// exact units before the first destructive step, never rediscover on retry.
pub fn run_codex(
    target: &SessionMeta,
    snapshot: &[SessionMeta],
    roots: &[PathBuf],
) -> Result<DeleteSessionReply, String> {
    use super::providers::codex;
    run_codex_at(
        &operation_path(target)?,
        target,
        snapshot,
        roots,
        |target| {
            codex::should_cleanup_index_after_delete(
                &target.session_id,
                target
                    .source_path
                    .as_deref()
                    .ok_or("Missing Codex source")?,
            )
        },
        |id| {
            if !codex::should_cleanup_index_after_delete(
                id,
                target
                    .source_path
                    .as_deref()
                    .ok_or("Missing Codex source")?,
            )? {
                return Err("Codex 当前索引已指向新的日志，停止旧清理任务".into());
            }
            codex::cleanup_after_delete(id)
        },
        |path| std::fs::remove_file(path).map_err(|e| e.to_string()),
    )
}

thread_local! {
    static LAST_EXECUTED_CODEX_UNITS: std::cell::RefCell<Vec<(String, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn codex_batch_path_key(raw: &str) -> String {
    let stripped_zst = raw
        .strip_suffix(".zst")
        .filter(|plain| plain.ends_with(".jsonl"))
        .unwrap_or(raw);
    let candidate = Path::new(stripped_zst);
    let resolved = candidate
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .and_then(|parent| {
            let name = candidate.file_name()?;
            Some(parent.canonicalize().ok()?.join(name))
        })
        .unwrap_or_else(|| candidate.to_path_buf());
    let text = resolved.to_string_lossy();
    #[cfg(windows)]
    {
        let normalized = text.replace('/', "\\");
        let without_prefix = normalized
            .strip_prefix("\\\\?\\UNC\\")
            .map(|unc| format!("\\\\{unc}"))
            .unwrap_or_else(|| {
                normalized
                    .strip_prefix("\\\\?\\")
                    .unwrap_or(&normalized)
                    .to_string()
            });
        without_prefix.trim_end_matches('\\').to_lowercase()
    }
    #[cfg(not(windows))]
    {
        text.trim_end_matches('/').to_string()
    }
}

fn executed_plan_units(plan: &super::providers::codex::DeletionPlan) -> Vec<(String, String)> {
    let Ok(value) = serde_json::to_value(plan) else {
        return Vec::new();
    };
    let next_unit = value
        .get("next_unit")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as usize;
    let Some(units) = value.get("units").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    units
        .iter()
        .take(next_unit)
        .filter_map(|unit| {
            let session_id = unit.get("session_id")?.as_str()?.to_string();
            let path = unit.get("path")?.as_str()?.to_string();
            Some((session_id, path))
        })
        .collect()
}

/// A batch keeps its discovery snapshot fixed, but records successful exact
/// identities. Only this in-memory evidence permits skipping an already removed
/// unit/residual; arbitrary missing paths still fail the ordinary plan preflight.
#[derive(Default)]
pub(crate) struct CodexBatchProgress {
    completed: std::collections::HashSet<(String, String)>,
}

impl CodexBatchProgress {
    pub(crate) fn run<F>(
        &mut self,
        target: &SessionMeta,
        snapshot: &[SessionMeta],
        action: F,
    ) -> Result<DeleteSessionReply, String>
    where
        F: FnOnce(&SessionMeta, &[SessionMeta]) -> Result<DeleteSessionReply, String>,
    {
        let key = |item: &SessionMeta| {
            (
                item.session_id.clone(),
                codex_batch_path_key(item.source_path.as_deref().unwrap_or("")),
            )
        };
        let done =
            |item: &SessionMeta| item.provider_id == "codex" && self.completed.contains(&key(item));
        if done(target) {
            if !super::providers::codex::rollout_is_missing(Path::new(
                target.source_path.as_deref().unwrap_or(""),
            )) {
                return Err("Codex batch source reappeared; rescan required".into());
            }
            return Ok(DeleteSessionReply::NotFound);
        }
        for item in snapshot.iter().filter(|item| done(item)) {
            if !super::providers::codex::rollout_is_missing(Path::new(
                item.source_path.as_deref().unwrap_or(""),
            )) {
                return Err("Codex completed batch source reappeared; rescan required".into());
            }
        }
        let remaining = snapshot
            .iter()
            .filter(|item| !done(item))
            .cloned()
            .collect::<Vec<_>>();
        LAST_EXECUTED_CODEX_UNITS.with(|units| units.borrow_mut().clear());
        let reply = action(target, &remaining)?;
        LAST_EXECUTED_CODEX_UNITS.with(|units| {
            for (session_id, unit_path) in units.borrow_mut().drain(..) {
                self.completed
                    .insert((session_id, codex_batch_path_key(&unit_path)));
            }
        });
        if matches!(&reply, DeleteSessionReply::Deleted { warnings } if warnings.is_empty()) {
            self.completed.insert(key(target));
            if !target.residual {
                for item in remaining.iter().filter(|item| {
                    item.provider_id == "codex"
                        && item.session_id == target.session_id
                        && item.residual
                }) {
                    self.completed.insert(key(item));
                }
            }
        }
        Ok(reply)
    }
}

fn persist_operation(path: &Path, operation: &PendingDeletion) -> Result<(), String> {
    super::providers::utils::atomic_write_json(path, operation)
}

fn run_codex_at<I, C, D>(
    path: &Path,
    target: &SessionMeta,
    snapshot: &[SessionMeta],
    roots: &[PathBuf],
    index_decision: I,
    cleanup: C,
    remove: D,
) -> Result<DeleteSessionReply, String>
where
    I: FnOnce(&SessionMeta) -> Result<bool, String>,
    C: FnMut(&str) -> Result<(), String>,
    D: FnMut(&Path) -> Result<(), String>,
{
    use super::providers::codex;
    let existing = match std::fs::read(path) {
        Ok(bytes) => {
            Some(serde_json::from_slice::<PendingDeletion>(&bytes).map_err(|e| e.to_string())?)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.to_string()),
    };
    let legacy = existing.as_ref().is_some_and(|op| op.codex_plan.is_none());
    let mut operation = existing.unwrap_or_else(|| {
        let mut targets = vec![target.clone()];
        if !target.residual {
            targets.extend(
                snapshot
                    .iter()
                    .filter(|item| {
                        item.provider_id == target.provider_id
                            && item.session_id == target.session_id
                            && item.residual
                            && item.source_path != target.source_path
                    })
                    .cloned(),
            );
        }
        PendingDeletion {
            targets,
            codex_plan: None,
            last_error: None,
        }
    });
    if !operation.targets.first().is_some_and(|saved| {
        saved.provider_id == "codex"
            && saved.session_id == target.session_id
            && saved.source_path == target.source_path
    }) {
        return Err("Codex journal target mismatch".into());
    }
    if legacy
        && operation.targets.iter().any(|item| {
            item.source_path
                .as_deref()
                .is_none_or(|source| !Path::new(source).is_file())
        })
    {
        return Err(
            "旧 Codex 清理记录缺少分页删除计划且源文件已消失；保留记录，请人工核对剩余分页文件"
                .into(),
        );
    }
    // Never freeze the result of the index lookup across attempts. Configuration
    // and authoritative thread mappings can change after an index checkpoint.
    // Failure here must preserve both the original journal and every file.
    let current_cleanup_index = index_decision(&operation.targets[0])?;
    if operation.codex_plan.is_none() {
        operation.codex_plan = Some(codex::prepare_deletion_plan(
            &operation.targets,
            roots,
            current_cleanup_index,
        )?);
        persist_operation(path, &operation)?;
    }
    let mut plan = operation.codex_plan.clone().ok_or("Missing Codex plan")?;
    if plan.session_id != target.session_id
        || Some(plan.source_path.as_str()) != target.source_path.as_deref()
    {
        return Err("Codex persisted plan target mismatch".into());
    }
    // Validation rejection does not rewrite the existing journal.
    codex::validate_deletion_index_decision(&plan, current_cleanup_index)?;
    codex::validate_deletion_plan(&plan, roots)?;
    let result = codex::execute_deletion_plan_validated(&mut plan, cleanup, remove, |progress| {
        operation.codex_plan = Some(progress.clone());
        persist_operation(path, &operation)
    });
    let executed = executed_plan_units(&plan);
    if !executed.is_empty() {
        LAST_EXECUTED_CODEX_UNITS.with(|units| units.borrow_mut().extend(executed));
    }
    if let Err(error) = result {
        return mark_pending(path, vec![error]);
    }
    if let Err(error) = std::fs::remove_file(path) {
        return Ok(DeleteSessionReply::CleanupPending {
            warnings: vec![format!("清理完成，记录更新失败：{error}")],
        });
    }
    Ok(DeleteSessionReply::Deleted {
        warnings: Vec::new(),
    })
}

fn mark_pending(path: &Path, mut warnings: Vec<String>) -> Result<DeleteSessionReply, String> {
    let persist = || -> Result<(), String> {
        let mut operation: PendingDeletion =
            serde_json::from_str(&std::fs::read_to_string(path).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        operation.last_error = Some(warnings.join("；"));
        persist_operation(path, &operation)
    };
    if let Err(error) = persist() {
        warnings.push(format!("无法保存清理错误详情：{error}"));
    }
    Ok(DeleteSessionReply::CleanupPending { warnings })
}

/// Only shared within one batch. A short successful-check lease avoids launching
/// PowerShell for every row, while long batches recheck before further mutations.
#[derive(Default)]
pub(crate) struct AgentStopChecks {
    checked: std::collections::HashMap<String, std::time::Instant>,
}

impl AgentStopChecks {
    pub(crate) fn ensure(&mut self, provider: &str) -> Result<(), String> {
        self.ensure_with(provider, || ensure_agent_stopped(provider))
    }

    fn ensure_with<F>(&mut self, provider: &str, check: F) -> Result<(), String>
    where
        F: FnOnce() -> Result<(), String>,
    {
        if self
            .checked
            .get(provider)
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(1))
        {
            return Ok(());
        }
        self.checked.remove(provider);
        check()?;
        self.checked
            .insert(provider.into(), std::time::Instant::now());
        Ok(())
    }
}

/// Editing an Agent's live databases/files is not a supported coordination API.
enum ProcessMatch {
    Direct,
    Script { missing_cmdline: bool },
}

struct DiscoveredProcess {
    pid: u32,
    name: String,
    exe_path: Option<String>,
    command_line: Option<String>,
    argv: Option<Vec<String>>,
}

fn match_discovered_process(proc: &DiscoveredProcess, provider: &str) -> Option<ProcessMatch> {
    if proc
        .exe_path
        .as_deref()
        .is_some_and(|exe| process_matches(exe, provider))
        || process_matches(&proc.name, provider)
    {
        return Some(ProcessMatch::Direct);
    }

    let runtime_candidate = proc
        .exe_path
        .as_deref()
        .filter(|exe| is_script_runtime_name(exe))
        .or_else(|| is_script_runtime_name(&proc.name).then_some(proc.name.as_str()));
    let runtime = runtime_candidate?;

    if let Some(argv) = proc.argv.as_deref() {
        if script_process_matches_tokens(runtime, Some(argv), provider) {
            return Some(ProcessMatch::Script {
                missing_cmdline: argv.is_empty(),
            });
        }
        return None;
    }

    if script_process_matches(runtime, proc.command_line.as_deref(), provider) {
        let missing_cmdline = proc
            .command_line
            .as_deref()
            .is_none_or(|value| value.trim().is_empty());
        return Some(ProcessMatch::Script { missing_cmdline });
    }
    None
}

#[cfg(target_os = "linux")]
fn parse_status_uids(status_text: &str) -> Option<(u32, u32)> {
    for line in status_text.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            let mut parts = rest.split_whitespace();
            let ruid = parts.next()?.parse::<u32>().ok()?;
            let euid = parts.next()?.parse::<u32>().ok()?;
            return Some((ruid, euid));
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn collect_current_user_processes() -> Result<Vec<DiscoveredProcess>, String> {
    let self_status = std::fs::read_to_string("/proc/self/status")
        .map_err(|e| format!("无法读取当前进程状态，未开始清理：{e}"))?;
    let (self_ruid, self_euid) = parse_status_uids(&self_status)
        .ok_or_else(|| "无法识别当前进程 UID，未开始清理".to_string())?;
    let entries = std::fs::read_dir("/proc")
        .map_err(|e| format!("无法检查 /proc 运行状态，未开始清理：{e}"))?;
    let self_pid = std::process::id();
    let mut processes = Vec::new();

    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let file_name = entry.file_name();
        let Some(pid_str) = file_name.to_str() else {
            continue;
        };
        if !pid_str.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let proc_dir = entry.path();
        let Ok(status_text) = std::fs::read_to_string(proc_dir.join("status")) else {
            continue;
        };
        let Some((p_ruid, p_euid)) = parse_status_uids(&status_text) else {
            continue;
        };
        if p_ruid != self_ruid && p_ruid != self_euid && p_euid != self_ruid && p_euid != self_euid
        {
            continue;
        }

        let exe_path = std::fs::read_link(proc_dir.join("exe")).ok().map(|p| {
            let s = p.to_string_lossy();
            s.strip_suffix(" (deleted)").unwrap_or(&s).to_string()
        });
        let comm = std::fs::read_to_string(proc_dir.join("comm"))
            .ok()
            .map(|s| s.trim_end_matches(['\r', '\n']).to_string())
            .unwrap_or_default();
        let argv = std::fs::read(proc_dir.join("cmdline"))
            .ok()
            .filter(|bytes| !bytes.is_empty())
            .map(|bytes| {
                bytes
                    .split(|&b| b == 0)
                    .filter(|part| !part.is_empty())
                    .map(|part| String::from_utf8_lossy(part).into_owned())
                    .collect::<Vec<_>>()
            })
            .filter(|tokens| !tokens.is_empty());

        let display_name = exe_path
            .as_deref()
            .and_then(|exe| exe.rsplit('/').next())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| (!comm.is_empty()).then(|| comm.clone()))
            .unwrap_or_else(|| pid.to_string());

        // Check comm separately via command_line fallback if comm itself matches
        let name = if process_matches(&comm, "deepseek")
            || process_matches(&comm, "workbuddy")
            || !process_matches(&display_name, "deepseek")
                && !process_matches(&display_name, "workbuddy")
                && !comm.is_empty()
                && exe_path.is_none()
        {
            comm
        } else {
            display_name
        };

        processes.push(DiscoveredProcess {
            pid,
            name,
            exe_path,
            command_line: None,
            argv,
        });
    }
    Ok(processes)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn parse_pid_line(line: &str) -> Option<(u32, &str)> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    let split_idx = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let pid = trimmed[..split_idx].parse::<u32>().ok()?;
    let rest = trimmed[split_idx..]
        .trim_start()
        .trim_end_matches(['\r', '\n']);
    Some((pid, rest))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn collect_current_user_processes() -> Result<Vec<DiscoveredProcess>, String> {
    let comm_output = std::process::Command::new("ps")
        .args(["-x", "-o", "pid=", "-o", "comm="])
        .output()
        .map_err(|e| format!("无法检查 Agent 运行状态，未开始清理：{e}"))?;
    if !comm_output.status.success() {
        return Err("无法检查 Agent 运行状态，未开始清理".into());
    }
    let args_output = std::process::Command::new("ps")
        .args(["-x", "-o", "pid=", "-o", "args="])
        .output()
        .map_err(|e| format!("无法检查 Agent 运行状态，未开始清理：{e}"))?;
    if !args_output.status.success() {
        return Err("无法检查 Agent 运行状态，未开始清理".into());
    }

    let args_text = String::from_utf8_lossy(&args_output.stdout);
    let mut args_by_pid = std::collections::HashMap::new();
    for line in args_text.lines() {
        if let Some((pid, args)) = parse_pid_line(line) {
            args_by_pid.insert(pid, args.to_string());
        }
    }

    let self_pid = std::process::id();
    let comm_text = String::from_utf8_lossy(&comm_output.stdout);
    let mut processes = Vec::new();
    for line in comm_text.lines() {
        let Some((pid, comm)) = parse_pid_line(line) else {
            continue;
        };
        if pid == self_pid || comm.is_empty() {
            continue;
        }
        let command_line = args_by_pid.remove(&pid).filter(|s| !s.trim().is_empty());
        processes.push(DiscoveredProcess {
            pid,
            name: comm.to_string(),
            exe_path: Some(comm.to_string()),
            command_line,
            argv: None,
        });
    }
    Ok(processes)
}

#[cfg(target_os = "windows")]
fn collect_current_user_processes() -> Result<Vec<DiscoveredProcess>, String> {
    use std::os::windows::process::CommandExt;

    #[derive(Deserialize)]
    struct WinProcess {
        #[serde(rename = "ProcessId")]
        pid: u32,
        #[serde(rename = "Name")]
        name: String,
        #[serde(rename = "ExecutablePath")]
        executable_path: Option<String>,
        #[serde(rename = "CommandLine")]
        command_line: Option<String>,
    }

    let script = "$ErrorActionPreference='Stop'; \
        try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch {}; \
        $sid = (Get-Process -Id $PID).SessionId; \
        ConvertTo-Json -InputObject @(Get-CimInstance Win32_Process -Filter ('SessionId = ' + $sid + ' AND ProcessId != ' + $PID) | Select-Object ProcessId,Name,ExecutablePath,CommandLine) -Compress";
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(0x08000000)
        .output()
        .map_err(|e| format!("无法检查 Agent 运行状态，未开始清理：{e}"))?;
    if !output.status.success() {
        return Err("无法检查 Agent 运行状态，未开始清理".into());
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    let trimmed = listing.trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(Vec::new());
    }
    let parsed: Vec<WinProcess> =
        serde_json::from_str(trimmed).map_err(|e| format!("无法解析 Agent 运行状态：{e}"))?;
    let self_pid = std::process::id();
    Ok(parsed
        .into_iter()
        .filter(|proc| proc.pid != self_pid)
        .map(|proc| DiscoveredProcess {
            pid: proc.pid,
            name: proc.name,
            exe_path: proc.executable_path,
            command_line: proc.command_line,
            argv: None,
        })
        .collect())
}

fn find_claude_lockfile_process<'a>(
    sessions_dir: &Path,
    processes: &'a [DiscoveredProcess],
) -> Option<&'a DiscoveredProcess> {
    let entries = std::fs::read_dir(sessions_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let pid_from_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok());
        let pid_from_body = || {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .and_then(|val| val.get("pid")?.as_u64())
                .and_then(|n| u32::try_from(n).ok())
        };
        let Some(lock_pid) = pid_from_name.or_else(pid_from_body) else {
            continue;
        };
        if let Some(found) = processes.iter().find(|proc| proc.pid == lock_pid) {
            return Some(found);
        }
    }
    None
}

/// Fail closed if process inspection itself fails. No shell command contains
/// session IDs, paths or other input from session history.
pub fn ensure_agent_stopped(provider: &str) -> Result<(), String> {
    let processes = collect_current_user_processes()?;

    if let Some((pid, name, kind)) = processes.iter().find_map(|proc| {
        match_discovered_process(proc, provider)
            .map(|kind| (proc.pid.to_string(), proc.name.clone(), kind))
    }) {
        return match kind {
            ProcessMatch::Script {
                missing_cmdline: true,
            } => Err(format!(
                "请先完全退出 {provider}（检测到脚本进程 {name} [PID {pid}]，无法读取其命令行参数；包括后台进程和终端任务），再清理会话"
            )),
            _ => Err(format!(
                "请先完全退出 {provider}（检测到进程 {name} [PID {pid}]，包括后台进程和终端任务），再清理会话"
            )),
        };
    }

    if provider == "claude" {
        let sessions_dir = crate::session_paths::claude_config_dir().join("sessions");
        if let Some(proc) = find_claude_lockfile_process(&sessions_dir, &processes) {
            return Err(format!(
                "请先完全退出 {provider}（检测到进程 {} [PID {}]，包括后台进程和终端任务），再清理会话",
                proc.name, proc.pid
            ));
        }
    }

    if provider == "workbuddy" {
        for home in crate::session_paths::workbuddy_homes() {
            let sessions_dir = home.join("sessions");
            if let Some(proc) = find_claude_lockfile_process(&sessions_dir, &processes) {
                return Err(format!(
                    "请先完全退出 {provider}（检测到进程 {} [PID {}]，包括后台进程和终端任务），再清理会话",
                    proc.name, proc.pid
                ));
            }
        }
    }

    Ok(())
}

fn matches_alias(name: &str, aliases: &[&str]) -> bool {
    aliases
        .iter()
        .any(|alias| name == *alias || name.starts_with(&format!("{alias} ")))
}

fn process_matches(executable: &str, provider: &str) -> bool {
    let lower_exec = executable.to_ascii_lowercase();
    let raw_name = lower_exec.rsplit(['/', '\\']).next().unwrap_or(&lower_exec);
    if raw_name.is_empty() {
        return false;
    }
    if provider == "claude"
        && (lower_exec.contains("/claude/versions/") || lower_exec.contains("\\claude\\versions\\"))
    {
        return true;
    }
    if provider == "workbuddy"
        && (lower_exec.contains("/.workbuddy-ai/binaries/")
            || lower_exec.contains("\\.workbuddy-ai\\binaries\\")
            || lower_exec.contains("/.workbuddy/binaries/")
            || lower_exec.contains("\\.workbuddy\\binaries\\"))
    {
        return true;
    }
    let name = raw_name.strip_suffix(".exe").unwrap_or(raw_name);
    match provider {
        "codex" => {
            matches_alias(name, &["codex", "codex-app-server", "codex-cli"])
                || name.starts_with("codex-")
        }
        "claude" => matches_alias(name, &["claude", "claude-code"]),
        "opencode" => {
            matches_alias(name, &["opencode", "opencode-cli", "opencode-desktop"])
                || name.starts_with("opencode-")
        }
        "zcode" => matches_alias(name, &["zcode", "zcode-desktop"]) || name.starts_with("zcode-"),
        "grokbuild" => matches_alias(name, &["grok", "grokbuild", "grok-build", "xai-grok-pager"]),
        "pi" => matches_alias(name, &["pi", "pi-coding-agent"]),
        // The packaged Windows app is not a node/bun runtime. Match its product
        // name exactly rather than allowing similarly named helper applications.
        "deepseek" => {
            name == "deepseek harness" || matches_alias(name, &["dsh", "deepseek-harness"])
        }
        "workbuddy" => {
            name == "workbuddy ai"
                || matches_alias(
                    name,
                    &[
                        "workbuddy",
                        "workbuddyai",
                        "workbuddy-ai",
                        "workbuddysharetarget",
                    ],
                )
                || name.starts_with("workbuddy-")
        }
        "gemini" => matches_alias(name, &["gemini", "gemini-cli"]),
        "copilot" => matches_alias(name, &["copilot", "github-copilot-cli"]),
        "codebuddy" => {
            matches_alias(name, &["codebuddy", "codebuddy-cli"]) || name.starts_with("codebuddy-")
        }
        "kimi" => matches_alias(name, &["kimi", "kimi-cli"]),
        "openclaw" => {
            matches_alias(name, &["openclaw", "openclaw-gateway"]) || name.starts_with("openclaw-")
        }
        _ => false,
    }
}

fn split_command_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else if ch == '\\' && q == '"' && chars.peek() == Some(&'"') {
                    current.push(chars.next().unwrap());
                } else {
                    current.push(ch);
                }
            }
            None => {
                if ch.is_whitespace() {
                    if in_token {
                        args.push(std::mem::take(&mut current));
                        in_token = false;
                    }
                } else if ch == '\'' || ch == '"' {
                    in_token = true;
                    quote = Some(ch);
                } else {
                    in_token = true;
                    current.push(ch);
                }
            }
        }
    }
    if in_token {
        args.push(current);
    }
    args
}

fn is_script_runtime_name(token: &str) -> bool {
    let base = token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(token)
        .to_ascii_lowercase();
    let base = base.strip_suffix(".exe").unwrap_or(&base);
    matches!(base, "node" | "nodejs" | "bun")
}

fn strip_script_extension(name: &str) -> &str {
    for suffix in [".exe", ".cmd", ".bat", ".mjs", ".cjs", ".js", ".ts"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            return stripped;
        }
    }
    name
}

fn strip_npm_version(segment: &str) -> &str {
    if let Some(rest) = segment.strip_prefix('@') {
        match rest.find('@') {
            Some(idx) => &segment[..idx + 1],
            None => segment,
        }
    } else {
        segment.split('@').next().unwrap_or(segment)
    }
}

fn scoped_package_matches(scope: &str, pkg: &str, provider: &str) -> bool {
    let pkg = strip_npm_version(pkg);
    match provider {
        "deepseek" => scope == "@deepseek-ai" && matches!(pkg, "dsh" | "deepseek-harness"),
        "grokbuild" => {
            matches!(scope, "@xai-official" | "@xai")
                && matches!(pkg, "grok" | "grokbuild" | "grok-build")
        }
        "claude" => scope == "@anthropic-ai" && matches!(pkg, "claude-code" | "claude"),
        "codex" => scope == "@openai" && (pkg == "codex" || pkg.starts_with("codex-")),
        "pi" => {
            matches!(scope, "@mariozechner" | "@earendil-works")
                && matches!(pkg, "pi-coding-agent" | "pi")
        }
        "opencode" => {
            matches!(scope, "@opencode-ai" | "@sst")
                && (pkg == "opencode" || pkg.starts_with("opencode-"))
        }
        "zcode" => scope == "@zcode" && (pkg == "zcode" || pkg.starts_with("zcode-")),
        "workbuddy" => {
            matches!(scope, "@tencent" | "@codebuddy" | "@workbuddy")
                && (pkg == "workbuddy" || pkg.starts_with("workbuddy-"))
        }
        "gemini" => scope == "@google" && matches!(pkg, "gemini-cli" | "gemini"),
        "copilot" => scope == "@github" && matches!(pkg, "copilot" | "copilot-cli"),
        "codebuddy" => {
            matches!(scope, "@tencent-ai" | "@tencent" | "@codebuddy")
                && (pkg == "codebuddy-code" || pkg == "codebuddy" || pkg.starts_with("codebuddy-"))
        }
        "kimi" => matches!(scope, "@moonshot-ai" | "@kimi") && matches!(pkg, "kimi" | "kimi-cli"),
        "openclaw" => scope == "@openclaw" && (pkg == "openclaw" || pkg.starts_with("openclaw-")),
        _ => false,
    }
}

fn package_or_bin_segment_matches(segment: &str, provider: &str) -> bool {
    let pkg = strip_npm_version(segment);
    match provider {
        "codex" => matches!(pkg, "codex" | "codex-cli" | "codex-app-server"),
        "claude" => matches!(pkg, "claude" | "claude-code"),
        "opencode" => {
            matches!(pkg, "opencode" | "opencode-ai" | "opencode-cli")
                || pkg.starts_with("opencode-")
        }
        "zcode" => matches!(pkg, "zcode" | "zcode-cli") || pkg.starts_with("zcode-"),
        "grokbuild" => matches!(pkg, "grok" | "grokbuild" | "grok-build" | "xai-grok-pager"),
        "pi" => matches!(pkg, "pi" | "pi-coding-agent"),
        "deepseek" => matches!(pkg, "dsh" | "deepseek-harness"),
        "workbuddy" => {
            matches!(pkg, "workbuddy" | "workbuddy-ai" | "workbuddy-cli")
                || pkg.starts_with("workbuddy-")
        }
        "gemini" => matches!(pkg, "gemini" | "gemini-cli"),
        "copilot" => matches!(pkg, "copilot" | "github-copilot-cli"),
        "codebuddy" => {
            matches!(pkg, "codebuddy" | "codebuddy-code" | "codebuddy-cli")
                || pkg.starts_with("codebuddy-")
        }
        "kimi" => matches!(pkg, "kimi" | "kimi-cli"),
        "openclaw" => {
            matches!(pkg, "openclaw" | "openclaw-gateway") || pkg.starts_with("openclaw-")
        }
        _ => false,
    }
}

fn unambiguous_package_dir_matches(segment: &str, provider: &str) -> bool {
    let pkg = strip_npm_version(segment);
    match provider {
        "codex" => matches!(pkg, "codex-cli" | "codex-app-server"),
        "claude" => pkg == "claude-code",
        "opencode" => matches!(pkg, "opencode-ai" | "opencode-cli"),
        "zcode" => matches!(pkg, "zcode-cli" | "zcode-desktop"),
        "grokbuild" => matches!(pkg, "grokbuild" | "grok-build" | "xai-grok-pager"),
        "pi" => pkg == "pi-coding-agent",
        "deepseek" => pkg == "deepseek-harness",
        "workbuddy" => matches!(
            pkg,
            "workbuddy-ai" | "workbuddy-cli" | ".workbuddy-ai" | ".workbuddy"
        ),
        "gemini" => pkg == "gemini-cli",
        "copilot" => pkg == "github-copilot-cli",
        "codebuddy" => matches!(pkg, "codebuddy-code" | "codebuddy-cli"),
        "kimi" => pkg == "kimi-cli",
        "openclaw" => matches!(pkg, "openclaw" | "openclaw-gateway"),
        _ => false,
    }
}

fn script_process_matches(executable: &str, args: Option<&str>, provider: &str) -> bool {
    if !is_script_runtime_name(executable) {
        return false;
    }
    let Some(args) = args.filter(|value| !value.trim().is_empty()) else {
        return true;
    };
    let parsed = split_command_args(args);
    script_process_matches_tokens(executable, Some(&parsed), provider)
}

fn script_process_matches_tokens(
    executable: &str,
    tokens: Option<&[String]>,
    provider: &str,
) -> bool {
    if !is_script_runtime_name(executable) {
        return false;
    }
    let Some(tokens) = tokens.filter(|items| !items.is_empty()) else {
        return true;
    };
    let inspected = match tokens.first() {
        Some(argv0) if is_script_runtime_name(argv0) => &tokens[1..],
        _ => tokens,
    };
    if inspected.is_empty() {
        return false;
    }
    let aliases: &[&str] = match provider {
        "codex" => &["codex", "codex-cli", "codex-app-server"],
        "claude" => &["claude", "claude-code"],
        "opencode" => &["opencode", "opencode-ai", "opencode-cli"],
        "zcode" => &["zcode"],
        "grokbuild" => &["grok", "grokbuild", "grok-build", "xai-grok-pager"],
        "pi" => &["pi", "pi-coding-agent"],
        "deepseek" => &["dsh", "deepseek-harness"],
        "workbuddy" => &["workbuddy", "workbuddy-ai", "workbuddy-cli"],
        "gemini" => &["gemini", "gemini-cli"],
        "copilot" => &["copilot", "github-copilot-cli"],
        "codebuddy" => &["codebuddy", "codebuddy-code", "codebuddy-cli"],
        "kimi" => &["kimi", "kimi-cli"],
        "openclaw" => &["openclaw", "openclaw-gateway"],
        _ => &[],
    };
    inspected.iter().any(|arg| {
        if arg.starts_with('-') {
            return false;
        }
        let lower = arg.to_ascii_lowercase();
        let segments: Vec<&str> = lower
            .split(['/', '\\'])
            .filter(|segment| !segment.is_empty())
            .collect();
        let Some(&last) = segments.last() else {
            return false;
        };
        let basename = strip_npm_version(strip_script_extension(last));
        if aliases.contains(&basename) {
            return true;
        }
        // Scoped npm packages (e.g. `@deepseek-ai/dsh/lib/bin.js`, `@xai-official/grok/bin/index.js`)
        // or explicit package/bin directories (`node_modules/<alias>`, `bin/<alias>`, `.claude/versions`).
        for pair in segments.windows(2) {
            if scoped_package_matches(pair[0], pair[1], provider) {
                return true;
            }
            if matches!(pair[0], "node_modules" | "bin" | ".bin")
                && package_or_bin_segment_matches(pair[1], provider)
            {
                return true;
            }
            if provider == "claude" && pair == ["claude", "versions"] {
                return true;
            }
        }
        // Unambiguous hyphenated package directories (e.g. `/modules/pi-coding-agent/dist/cli.js`),
        // without matching ordinary user folders like `/Users/claude/project/vite.js` or `/projects/pi/server.js`.
        segments[..segments.len() - 1]
            .iter()
            .any(|segment| unambiguous_package_dir_matches(segment, provider))
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn batch_stop_checks_expire_and_do_not_cache_failures() {
        let mut checks = AgentStopChecks::default();
        let calls = std::cell::Cell::new(0);
        let check = || {
            calls.set(calls.get() + 1);
            Ok(())
        };
        checks.ensure_with("codex", check).unwrap();
        checks.ensure_with("codex", check).unwrap();
        assert_eq!(calls.get(), 1);
        checks.ensure_with("claude", check).unwrap();
        assert_eq!(calls.get(), 2);
        checks.checked.insert(
            "codex".into(),
            std::time::Instant::now() - std::time::Duration::from_secs(2),
        );
        assert!(checks
            .ensure_with("codex", || Err("running".into()))
            .is_err());
        assert!(!checks.checked.contains_key("codex"));
        checks.ensure_with("codex", check).unwrap();
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn corrupt_journal_isolated_in_browse_report_but_strict_for_delete() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("broken.json"), "not json").unwrap();
        let target: SessionMeta = serde_json::from_value(serde_json::json!({
            "providerId": "codex", "sessionId": "thread", "residual": false,
            "sourcePath": "session.jsonl"
        }))
        .unwrap();
        persist_operation(
            &temp.path().join("good.json"),
            &PendingDeletion {
                targets: vec![target],
                codex_plan: None,
                last_error: None,
            },
        )
        .unwrap();
        let (sessions, warnings) = pending_sessions_report_in(temp.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].cleanup_pending);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("无法读取"));
        assert!(strict_pending_sessions((sessions, warnings)).is_err());
        std::fs::remove_file(temp.path().join("broken.json")).unwrap();
        assert_eq!(
            strict_pending_sessions(pending_sessions_report_in(temp.path()).unwrap())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn compat_codex_batch_same_id_both_orders_fixed_snapshot() {
        for order in [[0, 1], [1, 0]] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("sessions");
            std::fs::create_dir(&root).unwrap();
            let targets = [false, true].into_iter().enumerate().map(|(index, residual)| {
                let path = root.join(format!("{index}.jsonl"));
                std::fs::write(&path, "{\"type\":\"session_meta\",\"payload\":{\"id\":\"same\"}}\n").unwrap();
                serde_json::from_value::<SessionMeta>(serde_json::json!({"providerId":"codex","sessionId":"same","residual":residual,"sourcePath":path})).unwrap()
            }).collect::<Vec<_>>();
            let mut progress = CodexBatchProgress::default();
            let mut cleanup_count = 0;
            for index in order {
                let result = progress
                    .run(&targets[index], &targets, |target, snapshot| {
                        run_codex_at(
                            &temp.path().join(format!("op-{index}.json")),
                            target,
                            snapshot,
                            std::slice::from_ref(&root),
                            |item| Ok(!item.residual),
                            |_| {
                                cleanup_count += 1;
                                Ok(())
                            },
                            |path| std::fs::remove_file(path).map_err(|e| e.to_string()),
                        )
                    })
                    .unwrap();
                assert!(matches!(
                    result,
                    DeleteSessionReply::Deleted { .. } | DeleteSessionReply::NotFound
                ));
            }
            assert_eq!(cleanup_count, 1);
            assert!(targets
                .iter()
                .all(|t| !Path::new(t.source_path.as_deref().unwrap()).exists()));
            let mut wrong = targets[1].clone();
            wrong.source_path = Some(temp.path().join("unknown.jsonl").to_string_lossy().into());
            assert!(progress
                .run(&wrong, &targets, |_, _| Err("unproven identity".into()))
                .is_err());
        }
    }
    use super::*;

    fn codex_fixture(root: &Path) -> (SessionMeta, PathBuf, PathBuf) {
        let base = root.join("base.jsonl");
        let leaf = root.join("leaf.jsonl");
        std::fs::write(
            &base,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread\"}}\n",
        )
        .unwrap();
        let offset = base.metadata().unwrap().len();
        std::fs::write(&leaf, format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"thread\",\"history_base\":{{\"thread_id\":\"thread\",\"end_byte_offset\":{offset}}}}}}}\n")).unwrap();
        let target = serde_json::from_value(serde_json::json!({
            "providerId":"codex", "sessionId":"thread", "sourcePath":leaf.to_string_lossy(), "residual":false
        })).unwrap();
        (target, leaf, base)
    }

    #[test]
    fn codex_journal_retries_hidden_base_without_new_same_id_copy() {
        let temp = tempfile::tempdir().unwrap();
        let roots = vec![temp.path().to_path_buf()];
        let journal = temp.path().join("operation.json");
        let (target, leaf, base) = codex_fixture(temp.path());
        let first = run_codex_at(
            &journal,
            &target,
            &[],
            &roots,
            |_| Ok(true),
            |_| Ok(()),
            |path| {
                let saved: PendingDeletion =
                    serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
                assert!(
                    saved.codex_plan.is_some(),
                    "plan must precede first mutation"
                );
                if path.file_name() == base.file_name() {
                    return Err("injected base failure".into());
                }
                std::fs::remove_file(path).map_err(|e| e.to_string())
            },
        )
        .unwrap();
        assert!(matches!(first, DeleteSessionReply::CleanupPending { .. }));
        assert!(!leaf.exists());
        assert!(base.exists());
        let new_copy = temp.path().join("new-copy.jsonl");
        std::fs::write(
            &new_copy,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread\"}}\n",
        )
        .unwrap();
        let mut residual = target.clone();
        residual.residual = true;
        residual.source_path = Some(new_copy.to_string_lossy().into_owned());
        let retry = run_codex_at(
            &journal,
            &target,
            &[residual],
            &roots,
            |_| Ok(true), // revalidate ownership, without re-expanding file units
            |_| Ok(()),   // idempotent index cleanup also reruns after checkpoint
            |path| std::fs::remove_file(path).map_err(|e| e.to_string()),
        )
        .unwrap();
        assert!(matches!(retry, DeleteSessionReply::Deleted { .. }));
        assert!(!base.exists());
        assert!(new_copy.exists());
        assert!(!journal.exists());
    }

    #[test]
    fn codex_retry_revalidates_index_before_cleanup_after_partial_failure() {
        let temp = tempfile::tempdir().unwrap();
        let roots = vec![temp.path().to_path_buf()];
        let (target, leaf, base) = codex_fixture(temp.path());
        let journal = temp.path().join("operation.json");
        let first = run_codex_at(
            &journal,
            &target,
            &[],
            &roots,
            |_| Ok(true),
            |_| Err("desktop cleanup failed after state cleanup".into()),
            |_| panic!("no file removal"),
        )
        .unwrap();
        assert!(matches!(first, DeleteSessionReply::CleanupPending { .. }));
        let mut cleaned_new_index = false;
        let retry = run_codex_at(
            &journal,
            &target,
            &[],
            &roots,
            |_| Ok(true), // lookup passes; cleanup also rechecks immediately before mutation
            |_| {
                let current_mapping_is_original = false; // injected replacement mapping
                if !current_mapping_is_original {
                    return Err("index now points at new rollout".into());
                }
                cleaned_new_index = true;
                Ok(())
            },
            |_| panic!("index rejection precedes file removal"),
        )
        .unwrap();
        assert!(matches!(retry, DeleteSessionReply::CleanupPending { .. }));
        assert!(!cleaned_new_index && journal.exists() && leaf.exists() && base.exists());
    }

    #[test]
    fn compat_opencode_journal_never_absorbs_cross_database_residuals() {
        let temp = tempfile::tempdir().unwrap();
        let journal = temp.path().join("operation.json");
        let make = |db: &str, residual: bool| -> SessionMeta {
            serde_json::from_value(serde_json::json!({"providerId":"opencode", "sessionId":"ses_same", "residual":residual,
                "sourcePath":format!("sqlite:{}:ses_same", temp.path().join(db).display())})).unwrap()
        };
        let target = make("default.db", false);
        let other = make("dev.db", true);
        run_at(
            &journal,
            &target,
            std::slice::from_ref(&other),
            |plan| {
                assert_eq!(plan.len(), 1);
                Ok(())
            },
            |plan| {
                assert_eq!(plan.len(), 1);
                Ok(DeleteSessionReply::Deleted { warnings: vec![] })
            },
        )
        .unwrap();
        // Old records that already span sources are preserved, never executed.
        persist_operation(
            &journal,
            &PendingDeletion {
                targets: vec![target.clone(), other],
                codex_plan: None,
                last_error: None,
            },
        )
        .unwrap();
        let before = std::fs::read(&journal).unwrap();
        assert!(run_at(
            &journal,
            &target,
            &[],
            |_| panic!("cross-store preflight"),
            |_| panic!("cross-store action")
        )
        .is_err());
        assert_eq!(std::fs::read(&journal).unwrap(), before);
    }

    #[test]
    fn codex_completed_index_retry_cleans_restored_or_reconfigured_index() {
        use std::cell::Cell;
        for changed_database in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let roots = vec![temp.path().to_path_buf()];
            let (target, leaf, base) = codex_fixture(temp.path());
            let journal = temp.path().join("operation.json");
            let old_index = Cell::new(true);
            let new_index = Cell::new(true);
            let use_new = Cell::new(false);
            let cleanup_calls = Cell::new(0);
            let cleanup = |_: &str| {
                cleanup_calls.set(cleanup_calls.get() + 1);
                if use_new.get() {
                    new_index.set(false);
                } else {
                    old_index.set(false);
                }
                Ok(())
            };
            let first = run_codex_at(
                &journal,
                &target,
                &[],
                &roots,
                |_| Ok(true),
                cleanup,
                |_| Err("injected file failure after index checkpoint".into()),
            )
            .unwrap();
            assert!(matches!(first, DeleteSessionReply::CleanupPending { .. }));
            let saved: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
            assert_eq!(saved["codex_plan"]["index_done"], true);
            // Model the effective lookup after either restoration or sqlite_home change.
            if changed_database {
                use_new.set(true);
            } else {
                old_index.set(true);
            }
            let decisions = Cell::new(0);
            let retry = run_codex_at(
                &journal,
                &target,
                &[],
                &roots,
                |_| {
                    decisions.set(decisions.get() + 1);
                    Ok(true)
                },
                cleanup,
                |path| {
                    assert!(!if use_new.get() {
                        new_index.get()
                    } else {
                        old_index.get()
                    });
                    std::fs::remove_file(path).map_err(|error| error.to_string())
                },
            )
            .unwrap();
            assert!(matches!(retry, DeleteSessionReply::Deleted { .. }));
            assert_eq!(decisions.get(), 1);
            assert_eq!(cleanup_calls.get(), 2);
            assert!(!leaf.exists() && !base.exists() && !journal.exists());
        }
    }

    #[test]
    fn codex_retry_rejects_changed_index_ownership_and_lookup_failure_without_writes() {
        for (frozen, current) in [
            (true, Ok(false)),
            (false, Ok(true)),
            (true, Err("lookup failed".into())),
            (false, Err("lookup failed".into())),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let roots = vec![temp.path().to_path_buf()];
            let (target, leaf, base) = codex_fixture(temp.path());
            let journal = temp.path().join("operation.json");
            run_codex_at(
                &journal,
                &target,
                &[],
                &roots,
                |_| Ok(frozen),
                |_| Ok(()),
                |_| Err("file failure".into()),
            )
            .unwrap();
            let before = std::fs::read(&journal).unwrap();
            assert!(run_codex_at(
                &journal,
                &target,
                &[],
                &roots,
                |_| current,
                |_| panic!("changed ownership must not clean indexes"),
                |_| panic!("changed ownership must not remove files")
            )
            .is_err());
            assert_eq!(std::fs::read(&journal).unwrap(), before);
            assert!(leaf.exists() && base.exists());
        }
    }

    #[test]
    fn codex_completed_index_retry_cleanup_failure_keeps_frozen_files() {
        let temp = tempfile::tempdir().unwrap();
        let roots = vec![temp.path().to_path_buf()];
        let (target, leaf, base) = codex_fixture(temp.path());
        let journal = temp.path().join("operation.json");
        run_codex_at(
            &journal,
            &target,
            &[],
            &roots,
            |_| Ok(true),
            |_| Ok(()),
            |_| Err("file failure".into()),
        )
        .unwrap();
        let retry = run_codex_at(
            &journal,
            &target,
            &[],
            &roots,
            |_| Ok(true),
            |_| Err("current effective index cleanup failed".into()),
            |_| panic!("cleanup must precede file deletion"),
        )
        .unwrap();
        assert!(matches!(retry, DeleteSessionReply::CleanupPending { .. }));
        assert!(leaf.exists() && base.exists() && journal.exists());
    }

    #[test]
    fn codex_legacy_missing_leaf_keeps_journal_and_base() {
        let temp = tempfile::tempdir().unwrap();
        let (target, leaf, base) = codex_fixture(temp.path());
        let journal = temp.path().join("operation.json");
        let legacy = PendingDeletion {
            targets: vec![target.clone()],
            codex_plan: None,
            last_error: None,
        };
        persist_operation(&journal, &legacy).unwrap();
        std::fs::remove_file(&leaf).unwrap();
        let before = std::fs::read(&journal).unwrap();
        assert!(run_codex_at(
            &journal,
            &target,
            &[],
            &[temp.path().to_path_buf()],
            |_| panic!("no index discovery"),
            |_| panic!("no index mutation"),
            |_| panic!("no file mutation")
        )
        .is_err());
        assert_eq!(std::fs::read(&journal).unwrap(), before);
        assert!(base.exists());
    }

    #[test]
    fn codex_legacy_intact_plan_upgrades_before_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let (target, leaf, base) = codex_fixture(temp.path());
        let journal = temp.path().join("operation.json");
        persist_operation(
            &journal,
            &PendingDeletion {
                targets: vec![target.clone()],
                codex_plan: None,
                last_error: None,
            },
        )
        .unwrap();
        run_codex_at(
            &journal,
            &target,
            &[],
            &[temp.path().to_path_buf()],
            |_| Ok(false),
            |_| panic!("no index cleanup"),
            |path| {
                let saved: PendingDeletion =
                    serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
                assert!(saved.codex_plan.is_some());
                std::fs::remove_file(path).map_err(|e| e.to_string())
            },
        )
        .unwrap();
        assert!(!leaf.exists() && !base.exists() && !journal.exists());
    }

    #[test]
    fn codex_incomplete_evidence_writes_no_journal_and_runs_no_actions() {
        let temp = tempfile::tempdir().unwrap();
        let (target, leaf, base) = codex_fixture(temp.path());
        let journal = temp.path().join("operation.json");
        std::fs::write(temp.path().join("broken.jsonl"), "bad descriptor").unwrap();
        assert!(run_codex_at(
            &journal,
            &target,
            &[],
            &[temp.path().to_path_buf()],
            |_| Ok(false),
            |_| panic!("no index mutation"),
            |_| panic!("no file mutation")
        )
        .is_err());
        assert!(!journal.exists() && leaf.exists() && base.exists());
    }

    #[test]
    fn rejected_preflight_creates_no_journal_and_does_not_run_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("operation.json");
        let target: SessionMeta = serde_json::from_value(serde_json::json!({
            "providerId": "codex", "sessionId": "parent", "residual": false,
            "sourcePath": "parent.jsonl"
        }))
        .unwrap();
        let error = run_at(
            &path,
            &target,
            &[],
            |_| Err("branch dependency".into()),
            |_| panic!("index/transcript deletion must not start"),
        )
        .unwrap_err();
        assert_eq!(error, "branch dependency");
        assert!(!path.exists());
    }

    #[test]
    fn incomplete_deepseek_discovery_creates_no_journal_or_deletion() {
        use crate::session_manager::providers::deepseek;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let parent = root.join("project/parent/session.v3.jsonl");
        let child = root.join("project/child/session.v3.jsonl");
        for (path, id, parent_id) in [(&parent, "parent", None), (&child, "child", Some("parent"))]
        {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let header = serde_json::json!({
                "type": "session", "version": 3, "id": id,
                "createdAt": 1700000000000_i64, "parentSession": parent_id
            });
            std::fs::write(path, format!("{header}\n")).unwrap();
        }
        let compressed =
            zstd::stream::encode_all(std::fs::read(&child).unwrap().as_slice(), 0).unwrap();
        std::fs::write(child.with_extension("jsonl.zstd"), compressed).unwrap();
        let target: SessionMeta = serde_json::from_value(serde_json::json!({
            "providerId": "deepseek", "sessionId": "parent", "residual": false,
            "sourcePath": parent.to_string_lossy()
        }))
        .unwrap();
        let journal = temp.path().join("operation.json");
        let error = run_at(
            &journal,
            &target,
            &[],
            |_| deepseek::preflight_delete(&root, &parent, "parent"),
            |_| panic!("must not mutate a session with incomplete dependency evidence"),
        )
        .unwrap_err();
        assert!(error.contains("依赖扫描不完整"), "{error}");
        assert!(!journal.exists());
        assert!(parent.exists());
        assert!(child.exists());
    }

    #[test]
    fn rejected_retry_keeps_original_partial_cleanup_journal() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("operation.json");
        let target: SessionMeta = serde_json::from_value(serde_json::json!({
            "providerId": "codex", "sessionId": "child", "residual": false,
            "sourcePath": "child.jsonl"
        }))
        .unwrap();
        run_at(
            &path,
            &target,
            &[],
            |_| Ok(()),
            |_| Err("file locked after index cleanup".into()),
        )
        .unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(run_at(
            &path,
            &target,
            &[],
            |_| Err("cannot validate retry".into()),
            |_| { panic!("no retry mutations") }
        )
        .is_err());
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn failed_cleanup_is_durable_and_successful_retry_removes_journal() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("operation.json");
        let target = SessionMeta {
            provider_id: "codex".into(),
            session_id: "s1".into(),
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
            source_path: Some("missing.jsonl".into()),
        };
        let mut residual = target.clone();
        residual.residual = true;
        residual.source_path = Some("historical.jsonl".into());
        let result = run_at(
            &path,
            &target,
            &[residual],
            |_| Ok(()),
            |_| Err("index locked".into()),
        )
        .unwrap();
        assert!(matches!(result, DeleteSessionReply::CleanupPending { .. }));
        let saved: PendingDeletion =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.last_error.as_deref(), Some("index locked"));
        assert_eq!(saved.targets[0].source_path, target.source_path);
        // The retry scan no longer contains the historical file, but the
        // original plan must still include it and permit a missing primary.
        run_at(
            &path,
            &target,
            &[],
            |_| Ok(()),
            |plan| {
                assert_eq!(plan.len(), 2);
                assert!(plan.iter().all(|item| item.cleanup_pending));
                assert_eq!(plan[1].source_path.as_deref(), Some("historical.jsonl"));
                Ok(DeleteSessionReply::Deleted { warnings: vec![] })
            },
        )
        .unwrap();
        assert!(!path.exists());
    }
    #[test]
    fn recognizes_deepseek_packaged_product_and_existing_cli_names() {
        for executable in [
            "DeepSeek Harness.exe",
            "DEEPSEEK HARNESS.EXE",
            "DeepSeek Harness",
            r"C:\Program Files\DeepSeek Harness\DeepSeek Harness.exe",
            "/opt/DeepSeek Harness/DeepSeek Harness.exe",
            "dsh.exe",
            "deepseek-harness.exe",
        ] {
            assert!(process_matches(executable, "deepseek"), "{executable}");
        }
    }

    #[test]
    fn deepseek_product_match_rejects_similar_names_and_other_providers() {
        for executable in [
            "DeepSeek.exe",
            "DeepSeek Harness Helper.exe",
            "DeepSeek Harness Updater.exe",
            "DeepSeek Harness-extra.exe",
            "Other DeepSeek Harness.exe",
            "DeepSeek Harness.exe.bak",
            "AgentSessionManager.exe",
            r"C:\DeepSeek Harness\unrelated.exe",
        ] {
            assert!(!process_matches(executable, "deepseek"), "{executable}");
        }
        for provider in [
            "codex",
            "claude",
            "opencode",
            "zcode",
            "grokbuild",
            "pi",
            "unknown",
        ] {
            assert!(
                !process_matches("DeepSeek Harness.exe", provider),
                "{provider}"
            );
        }
    }

    #[test]
    fn deepseek_packaged_product_is_guarded_without_script_runtime_detection() {
        // These are the same alternatives used by ensure_agent_stopped, with
        // synthetic process data only: no process inspection or session access.
        for args in [
            None,
            Some(""),
            Some(r#""C:\Program Files\DeepSeek Harness\DeepSeek Harness.exe" --type=renderer"#),
        ] {
            let executable = "DeepSeek Harness.exe";
            assert!(!script_process_matches(executable, args, "deepseek"));
            assert!(
                process_matches(executable, "deepseek")
                    || script_process_matches(executable, args, "deepseek")
            );
        }
        for executable in ["unrelated.exe", "DeepSeek Harness Helper.exe"] {
            let args = Some("--description deepseek-harness dsh");
            assert!(
                !process_matches(executable, "deepseek")
                    && !script_process_matches(executable, args, "deepseek")
            );
        }
    }

    #[test]
    fn recognizes_native_and_script_runtimes_without_matching_manager() {
        assert!(process_matches("Codex.exe", "codex"));
        assert!(script_process_matches(
            "/usr/bin/node",
            Some("node /modules/pi-coding-agent/dist/cli.js"),
            "pi"
        ));
        assert!(script_process_matches(
            "node.exe",
            Some("node npx-cli.js @deepseek-ai/dsh web"),
            "deepseek"
        ));
        assert!(!script_process_matches(
            "/usr/bin/node",
            Some("node /projects/app/server.js"),
            "pi"
        ));
        assert!(!script_process_matches(
            "/usr/bin/node",
            Some("node /projects/pi/server.js"),
            "pi"
        ));
        assert!(!script_process_matches(
            "node.exe",
            Some(r#"node C:\Users\grok\app.js"#),
            "grokbuild"
        ));
        assert!(!script_process_matches("node", Some("node"), "pi"));
        assert!(script_process_matches(
            "/usr/bin/node",
            Some("node /usr/local/bin/pi --session x"),
            "pi"
        ));
        assert!(script_process_matches("node", None, "pi"));
        assert!(!process_matches("AgentSessionManager.exe", "codex"));
    }

    #[test]
    fn recognizes_upstream_binaries_scoped_packages_and_ignores_user_dirs() {
        assert!(process_matches("zcode-windows-x64.exe", "zcode"));
        assert!(process_matches("zcode-darwin-arm64", "zcode"));
        assert!(process_matches(
            "codex-app-server-x86_64-pc-windows-msvc.exe",
            "codex"
        ));
        assert!(process_matches("xai-grok-pager", "grokbuild"));
        assert!(process_matches(
            "/home/alice/.local/share/claude/versions/2.1.144",
            "claude"
        ));
        assert!(process_matches(
            r"C:\Users\alice\.local\share\claude\versions\2.1.144.exe",
            "claude"
        ));
        assert!(process_matches("opencode-cli", "opencode"));
        assert!(process_matches("pi-coding-agent", "pi"));

        assert!(script_process_matches(
            "/usr/bin/node",
            Some("node /usr/lib/node_modules/@deepseek-ai/dsh/lib/bin.js"),
            "deepseek"
        ));
        assert!(script_process_matches(
            "node.exe",
            Some(r"node C:\npm\node_modules\@xai-official\grok\bin\index.js"),
            "grokbuild"
        ));
        assert!(script_process_matches(
            "/usr/bin/node",
            Some("node /usr/local/bin/claude.mjs"),
            "claude"
        ));
        assert!(script_process_matches(
            "node.exe",
            Some(r"node C:\tools\codex.cjs --help"),
            "codex"
        ));
        assert!(script_process_matches(
            "node.exe",
            Some(
                r"node C:\Users\zcb\AppData\Roaming\npm\node_modules\@earendil-works\pi-coding-agent\dist\bundle\cli.js"
            ),
            "pi"
        ));
        assert!(script_process_matches(
            "/usr/bin/node",
            Some("node /usr/lib/node_modules/@mariozechner/pi-coding-agent/dist/cli.js"),
            "pi"
        ));

        // Ordinary user home/project directory named `claude` must NOT block claude
        assert!(!script_process_matches(
            "/usr/bin/node",
            Some("node /Users/claude/project/vite.js"),
            "claude"
        ));
    }

    #[test]
    fn empty_or_sourceless_journal_warns_and_provider_scoped_corruption_isolates() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("claude-0123456789abcdef.json"),
            r#"{"targets":[]}"#,
        )
        .unwrap();
        std::fs::write(
            temp.path().join("opencode-fedcba9876543210.json"),
            r#"{"targets":[{"providerId":"opencode","sessionId":"s1","residual":false}]}"#,
        )
        .unwrap();

        let (sessions, warnings) = pending_sessions_report_in(temp.path()).unwrap();
        assert!(sessions.is_empty());
        assert_eq!(warnings.len(), 2);

        // Provider-scoped corruption blocks claude & opencode, but allows codex!
        assert!(ensure_pending_journals_healthy_in(temp.path(), Some("claude")).is_err());
        assert!(ensure_pending_journals_healthy_in(temp.path(), Some("opencode")).is_err());
        assert!(ensure_pending_journals_healthy_in(temp.path(), Some("codex")).is_ok());

        // Unidentifiable corruption blocks all providers.
        std::fs::write(temp.path().join("broken.json"), "not json").unwrap();
        assert!(ensure_pending_journals_healthy_in(temp.path(), Some("codex")).is_err());
    }

    #[test]
    fn codex_batch_progress_records_non_residual_paginated_plan_units() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        let (leaf_meta, _leaf_path, base_path) = codex_fixture(&root);
        let base_meta: SessionMeta = serde_json::from_value(serde_json::json!({
            "providerId": "codex",
            "sessionId": "thread",
            "sourcePath": base_path.to_string_lossy(),
            "residual": false,
            "archived": true
        }))
        .unwrap();
        let snapshot = vec![leaf_meta.clone(), base_meta.clone()];
        let mut progress = CodexBatchProgress::default();

        let first = progress
            .run(&leaf_meta, &snapshot, |target, remaining| {
                run_codex_at(
                    &temp.path().join("op-leaf.json"),
                    target,
                    remaining,
                    std::slice::from_ref(&root),
                    |_| Ok(true),
                    |_| Ok(()),
                    |p| std::fs::remove_file(p).map_err(|e| e.to_string()),
                )
            })
            .unwrap();
        assert!(matches!(first, DeleteSessionReply::Deleted { .. }));

        // Even though `base_meta.residual == false`, `CodexBatchProgress` recorded all executed
        // `DeletionPlan` unit paths and recognizes `base_meta` as already removed!
        let second = progress
            .run(&base_meta, &snapshot, |_, _| {
                panic!("base unit was already removed by leaf plan and must short-circuit")
            })
            .unwrap();
        assert!(matches!(second, DeleteSessionReply::NotFound));
    }

    #[test]
    fn recognizes_workbuddy_desktop_binaries_and_script_packages() {
        for executable in [
            "WorkBuddyAI.exe",
            "WorkBuddyShareTarget.exe",
            "WorkBuddy.exe",
            "WorkBuddy AI",
            r"D:\Program Files\WorkBuddyAI\WorkBuddyAI.exe",
            r"C:\Users\alice\.workbuddy-ai\binaries\node\22.14.0\node.exe",
            "/home/alice/.workbuddy/binaries/node/bin/node",
            "workbuddy-cli",
        ] {
            assert!(process_matches(executable, "workbuddy"), "{executable}");
        }

        for unrelated in [
            "AgentSessionManager.exe",
            "Code.exe",
            r"C:\Users\workbuddy\project\app.exe",
        ] {
            assert!(!process_matches(unrelated, "workbuddy"), "{unrelated}");
        }

        assert!(script_process_matches(
            "node.exe",
            Some(r"node C:\npm\node_modules\@tencent\workbuddy-cli\dist\index.js"),
            "workbuddy"
        ));
        assert!(script_process_matches(
            "/usr/bin/node",
            Some("node /home/alice/.workbuddy-ai/agent/runner.js"),
            "workbuddy"
        ));
        assert!(!script_process_matches(
            "/usr/bin/node",
            Some("node /Users/workbuddy/webapp/server.js"),
            "workbuddy"
        ));
    }

    #[test]
    fn live_process_discovery_succeeds_and_excludes_self() {
        let processes = collect_current_user_processes().expect("inspect current user processes");
        assert!(processes.iter().all(|p| p.pid != std::process::id()));
    }
}
