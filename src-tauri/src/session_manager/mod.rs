pub mod providers;
pub mod terminal;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use providers::{claude, codex, grokbuild, opencode, pi, zcode};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub provider_id: String,
    pub session_id: String,
    pub residual: bool,
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

/// 删除请求的结构化结果：让前端能区分"已删除""记录本就不存在"和
/// "需要就共享目录二次确认"，而不是靠解析错误字符串。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DeleteSessionReply {
    /// 记录已删除。warnings 携带非致命的后续清理失败（如 Codex 正在运行
    /// 时状态库被锁住），不应向用户报告为删除失败。
    Deleted {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
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

pub fn scan_sessions() -> Vec<SessionMeta> {
    let (r1, r2, r3, r4, r5, r6) = std::thread::scope(|s| {
        let h1 = s.spawn(codex::scan_sessions);
        let h2 = s.spawn(claude::scan_sessions);
        let h3 = s.spawn(opencode::scan_sessions);
        let h4 = s.spawn(grokbuild::scan_sessions);
        let h5 = s.spawn(pi::scan_sessions);
        let h6 = s.spawn(zcode::scan_sessions);
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
    sessions.extend(r1);
    sessions.extend(r2);
    sessions.extend(r3);
    sessions.extend(r4);
    sessions.extend(r5);
    sessions.extend(r6);

    sessions.sort_by(|a, b| {
        let a_ts = a.last_active_at.or(a.created_at).unwrap_or(0);
        let b_ts = b.last_active_at.or(b.created_at).unwrap_or(0);
        b_ts.cmp(&a_ts)
    });

    sessions
}

/// 单个 provider 的扫描线程 panic 时不允许无声吞掉：至少留下日志，否则
/// 用户看到的是"该 Agent 没有会话"，与真的没有会话无法区分。
fn join_scan_results(
    handle: std::thread::ScopedJoinHandle<'_, Vec<SessionMeta>>,
) -> Vec<SessionMeta> {
    match handle.join() {
        Ok(items) => items,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&'static str>()
                .map(|text| (*text).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic payload".to_string());
            log::warn!("provider scan thread panicked: {message}");
            Vec::new()
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

    let path = Path::new(source_path);
    match provider_id {
        "codex" => codex::load_messages(path),
        "claude" => claude::load_messages(path),
        "opencode" => opencode::load_messages(path),
        "grokbuild" => grokbuild::load_messages(path),
        "pi" => pi::load_messages(path),
        _ => Err(format!("Unsupported provider: {provider_id}")),
    }
}

pub fn delete_session_checked(
    provider_id: &str,
    session_id: &str,
    source_path: &str,
    include_project: bool,
    shared_confirmed: bool,
) -> Result<DeleteSessionReply, String> {
    let sessions = scan_sessions();
    delete_session_in_snapshot(
        provider_id,
        session_id,
        source_path,
        include_project,
        shared_confirmed,
        &sessions,
    )
}

/// 在预扫描的会话快照上执行删除。批量删除共用一次扫描，避免 N 项删除
/// 触发 N 次全量磁盘扫描。
fn delete_session_in_snapshot(
    provider_id: &str,
    session_id: &str,
    source_path: &str,
    include_project: bool,
    shared_confirmed: bool,
    sessions: &[SessionMeta],
) -> Result<DeleteSessionReply, String> {
    let selected = sessions.iter().find(|item| {
        item.provider_id == provider_id
            && item.session_id == session_id
            && item.source_path.as_deref() == Some(source_path)
    });

    // 同批前一项的级联清理可能已经把本项的残留副本一并删除。对这类
    // "快照仍在、文件已消失"的文件型残留直接视为已达成目标。opencode
    // 的残留元数据本就允许 source 缺失（有专门通道），zcode 等虚拟
    // sqlite 引用不做存在性判断，均不适用此短路。
    if matches!(provider_id, "codex" | "claude" | "grokbuild")
        && selected.is_some_and(|item| item.residual)
        && !Path::new(source_path).exists()
    {
        return Ok(DeleteSessionReply::NotFound);
    }

    let project_dir = selected.and_then(|item| item.project_dir.clone());
    let deleting_current_codex_session =
        selected.is_none_or(|item| item.provider_id != "codex" || !item.residual);
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
    if include_project {
        let project = project_dir
            .as_deref()
            .ok_or_else(|| "该会话没有可靠的真实工作目录映射，已拒绝删除目录".to_string())?;
        let project_path = canonicalize_existing_path(Path::new(project), "project directory")?;
        validate_project_delete_target(&project_path)?;

        let shared: Vec<&SessionMeta> = sessions
            .iter()
            .filter(|item| {
                item.project_dir
                    .as_deref()
                    .and_then(|value| Path::new(value).canonicalize().ok())
                    .is_some_and(|value| value == project_path)
            })
            .collect();
        if shared.len() > 1 && !shared_confirmed {
            // 这里是共享目录判定的唯一权威：以 canonicalize 结果为准返回
            // 需要确认，前端据此弹出警告并要求二次确认，而不是自行比较
            // 路径字符串。
            let providers = shared
                .iter()
                .map(|item| item.provider_id.to_string())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            return Ok(DeleteSessionReply::NeedsSharedConfirmation {
                shared_count: shared.len() as u32,
                providers,
            });
        }

        trash::delete(&project_path).map_err(|e| format!("无法将真实工作目录移入回收站：{e}"))?;
    }

    let deleted = delete_session_record(
        provider_id,
        session_id,
        source_path,
        project_dir.as_deref(),
        include_project,
    )?;

    let mut warnings = Vec::new();
    if deleted
        && provider_id == "codex"
        && (include_project
            || (deleting_current_codex_session
                && codex::should_cleanup_index_after_delete(session_id, source_path)))
    {
        // 主记录删除成功后的索引清理是尽力而为：Codex 正在运行时状态库
        // 可能持有写锁，清理失败不应把整体报告为删除失败。
        if let Err(error) =
            codex::cleanup_after_delete(session_id, project_dir.as_deref(), include_project)
        {
            warnings.push(format!("会话已删除，但 Codex 索引清理失败：{error}"));
        } else {
            warnings.push(
                "Codex 正在运行时可能暂时保留已删除的侧栏项；重新启动 Codex 后会刷新".to_string(),
            );
        }
    }

    // Deleting a current conversation means deleting that conversation as a
    // whole. Historical rollouts and legacy-format copies with the same native
    // ID must not surface as new leftovers afterward. Selecting a residual
    // entry itself remains deliberately scoped to that one entry.
    if deleted {
        for residual_source in related_residual_sources {
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

    Ok(if deleted {
        DeleteSessionReply::Deleted { warnings }
    } else {
        DeleteSessionReply::NotFound
    })
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
    // 全批只扫描一次磁盘；批内前项级联清理掉的同 ID 残留副本由
    // delete_session_in_snapshot 的容错短路处理。
    let sessions = scan_sessions();
    collect_delete_session_outcomes(requests, |request| {
        delete_session_in_snapshot(
            &request.provider_id,
            &request.session_id,
            &request.source_path,
            request.include_project,
            request.shared_confirmed,
            &sessions,
        )
    })
}

#[cfg(target_os = "windows")]
const PROTECTED_TOP_LEVEL_DIRS: &[&str] = &[
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
    "users",
];

#[cfg(not(target_os = "windows"))]
const PROTECTED_TOP_LEVEL_DIRS: &[&str] = &[
    "usr", "etc", "var", "bin", "sbin", "lib", "lib64", "opt", "boot", "dev", "proc", "sys", "run",
    "home", "system", "library", "private",
];

/// 会话 cwd 指向系统位置时拒绝把"真实工作目录"移入回收站。只保护根目录
/// 下的第一级目录（`C:\Windows`、`C:\Users\Public` 等）；用户主目录内部
/// 是项目常态位置，不在此列（home 本身由调用方先行拒绝）。
fn is_protected_system_location(path: &Path, home: &Path) -> bool {
    if path.starts_with(home) {
        return false;
    }
    let mut first: Option<String> = None;
    for component in path.components() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => continue,
            std::path::Component::Normal(name) => {
                first = Some(name.to_string_lossy().to_lowercase());
                break;
            }
            _ => break,
        }
    }
    first.is_some_and(|name| PROTECTED_TOP_LEVEL_DIRS.contains(&name.as_str()))
}

fn validate_project_delete_target(path: &Path) -> Result<(), String> {
    let home = dirs::home_dir()
        .ok_or_else(|| "无法确定用户主目录，已拒绝删除".to_string())?
        .canonicalize()
        .map_err(|e| format!("无法解析用户主目录：{e}"))?;
    if path == home || path.parent().is_none() {
        return Err("目标是受保护的宽泛根目录，已拒绝删除".to_string());
    }
    if path.join("AgentSessionManager").exists()
        || path.join("ClaudeCodexHistoryManager").exists()
        || is_session_manager_source(path)
        || std::env::current_exe()
            .ok()
            .and_then(|exe| exe.canonicalize().ok())
            .is_some_and(|exe| exe.starts_with(path))
    {
        return Err("目标包含 Agent 会话管理器自身工作区，已强制禁止删除".to_string());
    }
    if is_protected_system_location(path, &home) {
        return Err("目标是系统目录，已拒绝删除".to_string());
    }
    for provider in ["codex", "claude", "opencode", "grokbuild", "pi"] {
        if let Ok(roots) = provider_roots(provider) {
            for root in roots {
                if root.exists() && root.canonicalize().ok().as_deref() == Some(path) {
                    return Err("目标是 Agent 数据根目录，已拒绝删除".to_string());
                }
            }
        }
    }
    Ok(())
}

fn is_session_manager_source(path: &Path) -> bool {
    let package = path.join("package.json");
    if let Ok(text) = std::fs::read_to_string(package) {
        if text.contains("\"name\": \"agent-session-manager\"") {
            return true;
        }
    }
    path.join("ClaudeCodexHistoryManager.spec").is_file() || path.join("GCHistory.spec").is_file()
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
    fn refuses_to_delete_a_directory_containing_the_manager_workspace() {
        let root = tempdir().expect("tempdir");
        std::fs::create_dir(root.path().join("AgentSessionManager")).expect("manager marker");

        let error = validate_project_delete_target(root.path())
            .expect_err("manager parent must always be protected");

        assert!(error.contains("会话管理器自身工作区"));
    }

    #[test]
    fn refuses_to_delete_the_manager_source_directory_itself() {
        let root = tempdir().expect("tempdir");
        std::fs::write(
            root.path().join("package.json"),
            r#"{"name": "agent-session-manager"}"#,
        )
        .expect("package marker");

        let error = validate_project_delete_target(root.path())
            .expect_err("manager source must always be protected");

        assert!(error.contains("会话管理器自身工作区"));
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
    fn shared_project_delete_requires_explicit_confirmation_before_any_deletion() {
        let project = tempdir().expect("tempdir");
        let project_dir = project.path().to_string_lossy().to_string();
        let mut first = session_meta("codex", "thread-1", "current.jsonl", false);
        first.project_dir = Some(project_dir.clone());
        let mut second = session_meta("claude", "thread-2", "other.jsonl", false);
        second.project_dir = Some(project_dir);

        let reply = delete_session_in_snapshot(
            "codex",
            "thread-1",
            "current.jsonl",
            true,
            false,
            &[first, second],
        )
        .expect("needs-confirmation reply");

        // 后端以 canonicalize 比对结果为准返回权威共享信息，且在任何
        // 删除动作发生之前就返回。
        match reply {
            DeleteSessionReply::NeedsSharedConfirmation {
                shared_count,
                providers,
            } => {
                assert_eq!(shared_count, 2);
                // BTreeSet 去重排序后依次列出涉及的所有 provider
                assert_eq!(providers, vec!["claude".to_string(), "codex".to_string()]);
            }
            other => panic!("expected NeedsSharedConfirmation, got {other:?}"),
        }
        assert!(project.path().exists(), "确认前不得触碰共享目录");
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

    #[cfg(target_os = "windows")]
    #[test]
    fn system_locations_are_protected_from_project_deletion() {
        let home = Path::new(r"C:\Users\me");

        assert!(is_protected_system_location(Path::new(r"C:\Windows"), home));
        assert!(is_protected_system_location(
            Path::new(r"C:\Program Files\App"),
            home
        ));
        // 主目录之外的其他用户目录同样受 C:\Users 这层保护
        assert!(is_protected_system_location(
            Path::new(r"C:\Users\Public\shared"),
            home
        ));
        // 主目录内部是项目常态位置；其他盘符的普通目录不受影响
        assert!(!is_protected_system_location(
            Path::new(r"C:\Users\me\Desktop\Cat"),
            home
        ));
        assert!(!is_protected_system_location(
            Path::new(r"E:\projects\Cat"),
            home
        ));
    }
}
