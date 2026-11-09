//! Durable, idempotent deletion bookkeeping. Pending operations remain visible
//! even when an Agent's primary record has already disappeared.
use super::{DeleteSessionReply, SessionMeta};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
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
    Ok(directory()?.join(format!("{hash:016x}.json")))
}

/// 删除路径保持严格：任何一份清理记录读不出都拒绝继续操作，避免丢失重试依据。
pub fn pending_sessions() -> Result<Vec<SessionMeta>, String> {
    strict_pending_sessions(pending_sessions_report()?)
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
                warnings.push(format!("无法枚举清理记录目录 {}：{error}", root.display()));
                continue;
            }
        };
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let operation = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| {
                serde_json::from_str::<PendingDeletion>(&text).map_err(|e| e.to_string())
            });
        let operation = match operation {
            Ok(operation) => operation,
            Err(error) => {
                warnings.push(format!(
                    "清理记录 {} 无法读取（{error}），已暂停相关删除；请先核对后再操作",
                    path.display()
                ));
                continue;
            }
        };
        // One operation has one visible retry entry. Related historical copies
        // belong to its persisted plan, not to independently retryable tasks.
        for mut target in operation.targets.into_iter().take(1) {
            target.cleanup_pending = true;
            target.resume_command = None;
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
        let parent = path.parent().ok_or("Invalid pending deletion directory")?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        serde_json::to_writer(&mut temp, &operation).map_err(|e| e.to_string())?;
        temp.flush().map_err(|e| e.to_string())?;
        temp.as_file().sync_all().map_err(|e| e.to_string())?;
        temp.persist(path).map_err(|e| e.to_string())?;
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
            codex::cleanup_after_delete(id, None, false)
        },
        |path| std::fs::remove_file(path).map_err(|e| e.to_string()),
    )
}

/// A batch keeps its discovery snapshot fixed, but records successful exact
/// identities. Only this in-memory evidence permits skipping an already removed
/// residual; arbitrary missing paths still fail the ordinary plan preflight.
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
                item.source_path.clone().unwrap_or_default(),
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
        let reply = action(target, &remaining)?;
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
    let parent = path.parent().ok_or("Invalid journal path")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut temp, operation).map_err(|e| e.to_string())?;
    temp.flush().map_err(|e| e.to_string())?;
    temp.as_file().sync_all().map_err(|e| e.to_string())?;
    temp.persist(path).map_err(|e| e.to_string())?;
    Ok(())
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
    let result = codex::execute_deletion_plan(&mut plan, roots, cleanup, remove, |progress| {
        operation.codex_plan = Some(progress.clone());
        persist_operation(path, &operation)
    });
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
        let mut temp =
            tempfile::NamedTempFile::new_in(path.parent().ok_or("Invalid journal path")?)
                .map_err(|e| e.to_string())?;
        serde_json::to_writer(&mut temp, &operation).map_err(|e| e.to_string())?;
        temp.as_file().sync_all().map_err(|e| e.to_string())?;
        temp.persist(path).map_err(|e| e.to_string())?;
        Ok(())
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
/// Fail closed if process inspection itself fails. No shell command contains
/// session IDs, paths or other input from session history.
pub fn ensure_agent_stopped(provider: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let output = {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command",
                "$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[System.Text.UTF8Encoding]::new(); ConvertTo-Json -InputObject @(Get-CimInstance Win32_Process | Select-Object Name,CommandLine) -Compress"])
            .creation_flags(0x08000000).output()
    };
    #[cfg(not(target_os = "windows"))]
    let output = std::process::Command::new("ps")
        .args(["-A", "-o", "comm=", "-o", "args="])
        .output();
    let output = output.map_err(|e| format!("无法检查 Agent 运行状态，未开始清理：{e}"))?;
    if !output.status.success() {
        return Err("无法检查 Agent 运行状态，未开始清理".into());
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    #[cfg(target_os = "windows")]
    let running = {
        #[derive(Deserialize)]
        struct Process {
            #[serde(rename = "Name")]
            name: String,
            #[serde(rename = "CommandLine")]
            command_line: Option<String>,
        }
        let processes: Vec<Process> = serde_json::from_str(listing.trim_start_matches('\u{feff}'))
            .map_err(|e| format!("无法解析 Agent 运行状态：{e}"))?;
        processes.iter().any(|process| {
            process_matches(&process.name, provider)
                || script_process_matches(&process.name, process.command_line.as_deref(), provider)
        })
    };
    #[cfg(not(target_os = "windows"))]
    let running = listing.lines().any(|line| {
        let (name, args) = line
            .trim()
            .split_once(char::is_whitespace)
            .unwrap_or((line.trim(), ""));
        process_matches(name, provider) || script_process_matches(name, Some(args), provider)
    });
    if running {
        return Err(format!("请先完全退出 {provider}（包括后台进程和终端任务），再清理会话；无法识别的脚本进程也需退出"));
    }
    Ok(())
}

fn process_matches(executable: &str, provider: &str) -> bool {
    let name = executable
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(executable)
        .to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    // The packaged Windows app is not a node/bun runtime. Match its product
    // name exactly rather than allowing similarly named helper applications.
    if provider == "deepseek" && name == "deepseek harness" {
        return true;
    }
    let aliases: &[&str] = match provider {
        "codex" => &["codex"],
        "claude" => &["claude", "claude-code"],
        "opencode" => &["opencode"],
        "zcode" => &["zcode"],
        "grokbuild" => &["grok", "grokbuild"],
        "pi" => &["pi"],
        "deepseek" => &["dsh", "deepseek-harness"],
        _ => &[],
    };
    aliases
        .iter()
        .any(|alias| name == *alias || name.starts_with(&format!("{alias} ")))
}

fn script_process_matches(executable: &str, args: Option<&str>, provider: &str) -> bool {
    let name = executable
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(executable)
        .to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    if !matches!(name, "node" | "nodejs" | "bun") {
        return false;
    }
    let Some(args) = args.filter(|value| !value.trim().is_empty()) else {
        return true;
    };
    let tokens: Vec<String> = args
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let aliases: &[&str] = match provider {
        "codex" => &["codex"],
        "claude" => &["claude", "claude-code"],
        "opencode" => &["opencode"],
        "zcode" => &["zcode"],
        "grokbuild" => &["grok", "grokbuild"],
        "pi" => &["pi", "pi-coding-agent"],
        "deepseek" => &["dsh", "deepseek-harness"],
        _ => &[],
    };
    tokens.iter().any(|token| aliases.contains(&token.as_str()))
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
            resume_command: None,
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
        assert!(!process_matches("AgentSessionManager.exe", "codex"));
    }
}
