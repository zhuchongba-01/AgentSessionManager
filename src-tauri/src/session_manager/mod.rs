mod deletion;
pub mod providers;
pub mod terminal;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use providers::{claude, codex, grokbuild, opencode, pi, zcode};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub provider_id: String,
    pub session_id: String,
    pub residual: bool,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_command: Option<String>,
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
    #[serde(default)]
    pub shared_confirmed: bool,
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
    /// 真实工作目录被多个会话共享；携带后端 canonicalize 比对出的权威
    /// 共享信息，由前端弹窗要求用户二次确认。
    NeedsSharedConfirmation {
        #[serde(rename = "sharedCount")]
        shared_count: u32,
        providers: Vec<String>,
    },
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
    let (r1, r2, r3, r4, r5, r6) = std::thread::scope(|s| {
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
        (
            join_scan_results(h1),
            join_scan_results(h2),
            join_scan_results(h3),
            join_scan_results(h4),
            join_scan_results(h5),
            join_scan_results(h6),
        )
    });

    let mut sessions = Vec::new();
    let mut warnings = Vec::new();
    for (items, issues) in [r1, r2, r3, r4, r5, r6] {
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

pub fn load_messages(provider_id: &str, source_path: &str) -> Result<Vec<SessionMessage>, String> {
    // SQLite sessions use a "sqlite:" prefixed source_path
    if provider_id == "opencode" && source_path.starts_with("sqlite:") {
        return opencode::load_messages_sqlite(source_path);
    }
    if provider_id == "zcode" && source_path.starts_with("sqlite-zcode:") {
        return zcode::load_messages(source_path);
    }

    let roots = provider_roots(provider_id)?;
    let path = Path::new(source_path);
    let canonical = canonicalize_existing_path(path, "session source")?;
    if !roots.iter().any(|root| {
        root.canonicalize()
            .is_ok_and(|root| canonical.starts_with(root))
    }) {
        return Err("Session source is outside configured storage".into());
    }
    match provider_id {
        "codex" => codex::load_messages(path),
        "claude" => claude::load_messages(path),
        "opencode" => opencode::load_messages(path),
        "grokbuild" => grokbuild::load_messages(path),
        "pi" => pi::load_messages(path),
        _ => Err(format!("Unsupported provider: {provider_id}")),
    }
}

pub fn checked_list_sessions() -> Result<SessionScanReport, String> {
    deletion::pending_sessions()?;
    Ok(scan_report())
}

pub fn resume_command(provider: &str, source: &str) -> Result<String, String> {
    let session = scan_sessions()
        .into_iter()
        .find(|session| {
            session.provider_id == provider && session.source_path.as_deref() == Some(source)
        })
        .ok_or("会话已不存在，请重新扫描")?;
    if session.cleanup_pending || session.archived || session.residual {
        return Err("该记录正在清理、已归档或属于历史副本，请先在原 Agent 中处理".into());
    }
    if !providers::utils::is_safe_session_id(&session.session_id) {
        return Err("会话 ID 无法安全用于恢复命令".into());
    }
    let quote = |value: &str| {
        if cfg!(target_os = "windows") {
            format!("'{}'", value.replace('\'', "''"))
        } else {
            terminal::shell_escape(value)
        }
    };
    let id = &session.session_id;
    let command = match provider {
        "codex" => format!("codex resume {id}"),
        "claude" => format!("claude --resume {id}"),
        "opencode" => format!("opencode -s {id}"),
        "zcode" => format!("zcode -s {id}"),
        "grokbuild" => format!("grok --resume {id}"),
        "pi" => format!("pi --session {}", quote(source)),
        _ => return Err("不支持此 Agent".into()),
    };
    let cwd = if provider == "codex" {
        codex::working_directory(Path::new(source))?
    } else {
        session.project_dir
    };
    if let Some(cwd) = cwd.filter(|cwd| !cwd.trim().is_empty()) {
        if !Path::new(&cwd).is_dir() {
            return Err("会话工作目录已不存在，请先恢复目录或使用原 Agent 选择新的目录".into());
        }
        return Ok(if cfg!(target_os = "windows") {
            format!(
                "& {{ Set-Location -LiteralPath {} -ErrorAction Stop; {command} }}",
                quote(&cwd)
            )
        } else {
            format!("cd {} && {command}", quote(&cwd))
        });
    }
    Ok(command)
}

pub fn delete_session_checked(
    provider_id: &str,
    session_id: &str,
    source_path: &str,
    include_project: bool,
    shared_confirmed: bool,
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
    deletion::run(target, &sessions, |plan| {
        delete_session_in_snapshot(
            provider_id,
            session_id,
            source_path,
            include_project,
            shared_confirmed,
            plan,
        )
    })
}

/// 在预扫描的会话快照上执行删除。批量删除共用一次扫描，避免 N 项删除
/// 触发 N 次全量磁盘扫描。
fn delete_session_in_snapshot(
    provider_id: &str,
    session_id: &str,
    source_path: &str,
    include_project: bool,
    _shared_confirmed: bool,
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
        && matches!(provider_id, "codex" | "claude" | "grokbuild" | "pi")
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

    // Clear referenced indexes BEFORE removing the transcript. Archived threads
    // are included in the authoritative lookup; historical copies are not.
    if provider_id == "codex" && codex::should_cleanup_index_after_delete(session_id, source_path)?
    {
        codex::cleanup_after_delete(session_id, None, false)?;
    }
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
    collect_delete_session_outcomes(requests, |request| {
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
        deletion::run(target, &sessions, |plan| {
            delete_session_in_snapshot(
                &request.provider_id,
                &request.session_id,
                &request.source_path,
                false,
                false,
                plan,
            )
        })
    })
}

fn delete_session_with_roots(
    provider_id: &str,
    session_id: &str,
    source_path: &Path,
    roots: &[PathBuf],
) -> Result<bool, String> {
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
        "zcode" => vec![zcode::database_path()
            .parent()
            .ok_or("Invalid ZCode database path")?
            .to_path_buf()],
        _ => return Err(format!("Unsupported provider: {provider_id}")),
    };

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
                Ok(DeleteSessionReply::NeedsSharedConfirmation { .. }) => {
                    outcome.error =
                        Some("Shared directory deletion needs explicit confirmation".to_string());
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
            archived: false,
            cleanup_pending: false,
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
                shared_confirmed: false,
            },
            DeleteSessionRequest {
                provider_id: "claude".to_string(),
                session_id: "s2".to_string(),
                source_path: "/tmp/s2".to_string(),
                include_project: false,
                shared_confirmed: false,
            },
            DeleteSessionRequest {
                provider_id: "gemini".to_string(),
                session_id: "s3".to_string(),
                source_path: "/tmp/s3".to_string(),
                include_project: false,
                shared_confirmed: false,
            },
            DeleteSessionRequest {
                provider_id: "codex".to_string(),
                session_id: "s4".to_string(),
                source_path: "/tmp/s4".to_string(),
                include_project: false,
                shared_confirmed: false,
            },
        ];

        let outcomes = collect_delete_session_outcomes(&requests, |request| {
            match request.session_id.as_str() {
                "s1" => Ok(DeleteSessionReply::Deleted {
                    warnings: vec!["索引清理失败".to_string()],
                }),
                "s2" => Err("boom".to_string()),
                "s3" => Ok(DeleteSessionReply::NotFound),
                _ => Ok(DeleteSessionReply::NeedsSharedConfirmation {
                    shared_count: 2,
                    providers: vec!["codex".to_string(), "claude".to_string()],
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
        // 需要共享目录二次确认：批量通道没有确认入口，按失败返回
        assert!(!outcomes[3].success);
        assert!(outcomes[3]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("confirmation")));
    }

    #[test]
    fn legacy_project_delete_is_rejected_before_touching_any_file() {
        let project = tempdir().expect("tempdir");
        let sentinel = project.path().join("valuable.txt");
        std::fs::write(&sentinel, "keep").unwrap();
        let mut target = session_meta("codex", "thread-1", "missing.jsonl", false);
        target.project_dir = Some(project.path().to_string_lossy().into_owned());
        for confirmed in [false, true] {
            let error = delete_session_in_snapshot(
                "codex",
                "thread-1",
                "missing.jsonl",
                true,
                confirmed,
                &[target.clone()],
            )
            .expect_err("legacy directory deletion must be rejected");
            assert!(error.contains("不再删除项目目录"));
            assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep");
        }
    }

    #[test]
    fn stale_residual_files_deleted_by_an_earlier_batch_item_are_not_failures() {
        let temp = tempdir().expect("tempdir");
        let source = temp.path().join("older.jsonl");
        let mut stale = session_meta("codex", "thread-1", "gone.jsonl", true);
        stale.project_dir = Some(temp.path().to_string_lossy().to_string());
        // 快照里仍在、磁盘上已消失的文件型残留（同批前项级联清理过）
        assert!(!source.exists());

        let reply =
            delete_session_in_snapshot("codex", "thread-1", "gone.jsonl", false, false, &[stale])
                .expect("stale residual short-circuits to NotFound");

        assert!(matches!(reply, DeleteSessionReply::NotFound));
    }
}
