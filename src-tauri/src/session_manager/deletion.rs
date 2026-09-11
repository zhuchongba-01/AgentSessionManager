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

pub fn pending_sessions() -> Result<Vec<SessionMeta>, String> {
    let root = directory()?;
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut sessions = Vec::new();
    for entry in std::fs::read_dir(root).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let operation: PendingDeletion =
            serde_json::from_str(&std::fs::read_to_string(&path).map_err(|e| e.to_string())?)
                .map_err(|e| format!("清理记录 {} 无法读取：{e}", path.display()))?;
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
    Ok(sessions)
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
                    })
                    .cloned(),
            );
        }
        PendingDeletion {
            targets,
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
        temp.persist(&path).map_err(|e| e.to_string())?;
    }
    for saved in &mut operation.targets {
        saved.cleanup_pending = true;
    }
    // Retry exactly the original plan even after scans stop finding a deleted
    // primary record. Never add new same-ID copies to an existing operation.
    let reply = match action(&operation.targets) {
        Err(error) => return mark_pending(&path, vec![error]),
        Ok(DeleteSessionReply::Deleted { warnings }) if !warnings.is_empty() => {
            return mark_pending(&path, warnings)
        }
        Ok(reply) => reply,
    };
    if matches!(
        reply,
        DeleteSessionReply::Deleted { .. } | DeleteSessionReply::NotFound
    ) {
        if let Err(error) = std::fs::remove_file(&path) {
            return Ok(DeleteSessionReply::CleanupPending {
                warnings: vec![format!("清理已完成，但无法更新清理记录：{error}")],
            });
        }
    }
    Ok(reply)
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
    let aliases: &[&str] = match provider {
        "codex" => &["codex"],
        "claude" => &["claude", "claude-code"],
        "opencode" => &["opencode"],
        "zcode" => &["zcode"],
        "grokbuild" => &["grok", "grokbuild"],
        "pi" => &["pi"],
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
        _ => &[],
    };
    tokens.iter().any(|token| aliases.contains(&token.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            archived: false,
            cleanup_pending: false,
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
    fn recognizes_native_and_script_runtimes_without_matching_manager() {
        assert!(process_matches("Codex.exe", "codex"));
        assert!(script_process_matches(
            "/usr/bin/node",
            Some("node /modules/pi-coding-agent/dist/cli.js"),
            "pi"
        ));
        assert!(!script_process_matches(
            "/usr/bin/node",
            Some("node /projects/app/server.js"),
            "pi"
        ));
        assert!(!process_matches("AgentSessionManager.exe", "codex"));
    }
}
