//! Native data locations used by the standalone session manager.
//!
//! Session browsing only needs to locate each Agent's own files. Keeping that
//! logic here avoids depending on the inherited provider-switching settings.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use toml_edit::DocumentMut;

pub const CODEX_STATE_DB_FILENAME: &str = "state_5.sqlite";
pub const CODEX_THREAD_HISTORY_DB_FILENAME: &str = "thread_history_1.sqlite";

pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// 展开 `~` / `~/` / `~\` 前缀；其余原样保留。
pub(crate) fn resolve_user_path(raw: &str) -> PathBuf {
    if raw == "~" {
        return home_dir();
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        return home_dir().join(rest);
    }
    PathBuf::from(raw)
}

/// 读取一个目录类环境变量；未设置（或为空/纯空白）时返回 `None`。
///
/// 与 `PI_CODING_AGENT_DIR` 保持一致地展开 `~` 前缀：同一个应用里不该出现
/// 「有的变量认 `~`、有的把 `~` 当字面目录名」这种差别。
///
/// 注意：这里不校验绝对性。相对值只能相对**进程 cwd** 解析，而 GUI 进程的 cwd 由
/// 启动方式决定，因此相对值在浏览时可能扫到意外目录。破坏性的删除路径另有
/// `provider_roots` 拒绝非绝对根（见 `session_manager::provider_roots`），
/// 保证「看到意外目录」不会升级成「删掉意外目录」。
pub(crate) fn configured_dir(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .map(|v| v.to_string_lossy().trim().to_string())
        .filter(|v| !v.is_empty())
        .map(|v| resolve_user_path(&v))
}

#[allow(dead_code)]
pub fn dsh_home() -> PathBuf {
    configured_dir("DSH_HOME").unwrap_or_else(|| home_dir().join(".dsh"))
}

pub fn claude_config_dir() -> PathBuf {
    configured_dir("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home_dir().join(".claude"))
}

pub fn codex_config_dir() -> PathBuf {
    configured_dir("CODEX_HOME").unwrap_or_else(|| home_dir().join(".codex"))
}

#[allow(dead_code)]
pub fn zcode_home_dir() -> PathBuf {
    configured_dir("ZCODE_DATA_BASE_DIR")
        .map(|p| p.join(".zcode"))
        .unwrap_or_else(|| home_dir().join(".zcode"))
}

#[allow(dead_code)]
pub fn zcode_cli_dir() -> PathBuf {
    configured_dir("ZCODE_STORAGE_DIR").unwrap_or_else(|| zcode_home_dir().join("cli"))
}

#[allow(dead_code)]
pub fn opencode_base_dir() -> PathBuf {
    configured_dir("XDG_DATA_HOME")
        .map(|p| p.join("opencode"))
        .unwrap_or_else(|| home_dir().join(".local").join("share").join("opencode"))
}

pub fn read_codex_config_text() -> Result<String, String> {
    let path = codex_config_dir().join("config.toml");
    if !path.exists() {
        return Ok(String::new());
    }
    std::fs::read_to_string(&path)
        .map_err(|error| format!("无法读取 Codex 配置 {}：{error}", path.display()))
}

fn has_parent_dir(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
}

#[allow(dead_code)]
pub fn codex_sqlite_home_dir(config_dir: &Path, config_text: &str) -> PathBuf {
    codex_sqlite_home_dir_with_environment(
        config_dir,
        config_text,
        std::env::var("CODEX_SQLITE_HOME").ok().as_deref(),
    )
}

fn codex_sqlite_home_dir_with_environment(
    config_dir: &Path,
    config_text: &str,
    environment: Option<&str>,
) -> PathBuf {
    let configured = config_text
        .parse::<DocumentMut>()
        .ok()
        .and_then(|document| {
            document
                .get("sqlite_home")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(resolve_user_path)
                .map(|rel| {
                    if !rel.is_absolute() && !has_parent_dir(&rel) {
                        config_dir.join(rel)
                    } else {
                        rel
                    }
                })
        });
    let environment = environment
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(resolve_user_path);
    configured
        .or(environment)
        .unwrap_or_else(|| config_dir.to_path_buf())
}

pub fn validate_codex_sqlite_home_for_delete(
    config_dir: &Path,
    config_text: &str,
) -> Result<PathBuf, String> {
    validate_codex_sqlite_home_for_delete_with_environment(
        config_dir,
        config_text,
        std::env::var("CODEX_SQLITE_HOME").ok().as_deref(),
    )
}

fn validate_codex_sqlite_home_for_delete_with_environment(
    config_dir: &Path,
    config_text: &str,
    environment: Option<&str>,
) -> Result<PathBuf, String> {
    if !config_dir.is_absolute() || has_parent_dir(config_dir) {
        return Err(format!(
            "Codex 配置目录必须是有效绝对路径：{}",
            config_dir.display()
        ));
    }

    let document = config_text
        .parse::<DocumentMut>()
        .map_err(|error| format!("无法解析 Codex 配置：{error}"))?;
    if let Some(item) = document.get("sqlite_home") {
        let raw = item
            .as_str()
            .ok_or_else(|| "Codex sqlite_home 配置必须是字符串".to_string())?
            .trim();
        if !raw.is_empty() {
            let resolved = resolve_user_path(raw);
            if has_parent_dir(&resolved) {
                return Err(format!(
                    "Codex sqlite_home 不能包含上级目录组件（..）：{}",
                    resolved.display()
                ));
            }
            let target = if resolved.is_absolute() {
                resolved
            } else {
                config_dir.join(resolved)
            };
            return Ok(target);
        }
    }

    if let Some(raw_env) = environment.map(str::trim).filter(|value| !value.is_empty()) {
        let resolved_env = resolve_user_path(raw_env);
        if !resolved_env.is_absolute() {
            return Err(format!(
                "CODEX_SQLITE_HOME 必须是绝对路径：{}",
                resolved_env.display()
            ));
        }
        if has_parent_dir(&resolved_env) {
            return Err(format!(
                "CODEX_SQLITE_HOME 不能包含上级目录组件（..）：{}",
                resolved_env.display()
            ));
        }
        return Ok(resolved_env);
    }

    Ok(config_dir.to_path_buf())
}

fn parse_numbered_sqlite_filename(name: &str, prefix: &str) -> Option<u64> {
    let rest = name.strip_prefix(prefix)?.strip_prefix('_')?;
    let digits = rest.strip_suffix(".sqlite")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()
}

pub fn discover_numbered_sqlite_dbs(
    dir: &Path,
    prefix: &str,
    fallback_filename: &str,
) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![dir.join(fallback_filename)];
    };
    let mut matched: Vec<(u64, String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if let Some(generation) = parse_numbered_sqlite_filename(name, prefix) {
            matched.push((generation, name.to_string(), entry.path()));
        }
    }
    if matched.is_empty() {
        return vec![dir.join(fallback_filename)];
    }
    matched.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    matched.into_iter().map(|(_, _, path)| path).collect()
}

#[allow(dead_code)]
pub fn codex_cleanup_db_paths(sqlite_dir: &Path) -> Vec<PathBuf> {
    const CLEANUP_PREFIXES: &[&str] = &[
        "state",
        "thread_history",
        "logs",
        "memories",
        "goals",
        "queue",
    ];

    let mut results: Vec<(usize, u64, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(sqlite_dir) {
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            for (prefix_idx, prefix) in CLEANUP_PREFIXES.iter().enumerate() {
                if let Some(generation) = parse_numbered_sqlite_filename(name, prefix) {
                    results.push((prefix_idx, generation, entry.path()));
                    break;
                }
            }
        }
    }

    results.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    let mut paths: Vec<PathBuf> = results.into_iter().map(|(_, _, path)| path).collect();

    for fallback in [CODEX_STATE_DB_FILENAME, CODEX_THREAD_HISTORY_DB_FILENAME] {
        let candidate = sqlite_dir.join(fallback);
        if std::fs::symlink_metadata(&candidate).is_ok_and(|meta| meta.file_type().is_file())
            && !paths.contains(&candidate)
        {
            paths.push(candidate);
        }
    }
    paths
}

pub fn codex_state_db_paths(config_dir: &Path, config_text: &str) -> Vec<PathBuf> {
    codex_state_db_paths_with_environment(
        config_dir,
        config_text,
        std::env::var("CODEX_SQLITE_HOME").ok().as_deref(),
    )
}

fn codex_state_db_paths_with_environment(
    config_dir: &Path,
    config_text: &str,
    environment: Option<&str>,
) -> Vec<PathBuf> {
    // One authoritative location: never mutate a stale default database when
    // Codex has selected sqlite_home elsewhere (config > environment > default).
    let base = codex_sqlite_home_dir_with_environment(config_dir, config_text, environment);
    vec![
        discover_numbered_sqlite_dbs(&base, "state", CODEX_STATE_DB_FILENAME)
            .into_iter()
            .next()
            .unwrap(),
    ]
}

#[cfg(test)]
mod compat_tests {
    use super::*;
    #[test]
    fn compat_sqlite_home_precedence_without_ambient_environment() {
        let temp = tempfile::tempdir().unwrap();
        let default = temp.path().join("default");
        let configured = temp.path().join("configured");
        let environment = temp.path().join("environment");
        let text = format!(
            "sqlite_home = {}",
            serde_json::to_string(&configured.to_string_lossy()).unwrap()
        );
        assert_eq!(
            codex_state_db_paths_with_environment(&default, &text, environment.to_str()),
            vec![configured.join(CODEX_STATE_DB_FILENAME)]
        );
        assert_eq!(
            codex_state_db_paths_with_environment(&default, "", environment.to_str()),
            vec![environment.join(CODEX_STATE_DB_FILENAME)]
        );
        assert_eq!(
            codex_state_db_paths_with_environment(&default, "", None),
            vec![default.join(CODEX_STATE_DB_FILENAME)]
        );
    }

    #[test]
    fn relative_sqlite_home_resolves_against_config_dir_and_rejects_parent_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("codex-config");
        assert_eq!(
            codex_state_db_paths_with_environment(
                &config_dir,
                "sqlite_home = \"sqlite_data\"",
                None
            ),
            vec![config_dir.join("sqlite_data").join(CODEX_STATE_DB_FILENAME)]
        );
        assert_eq!(
            validate_codex_sqlite_home_for_delete_with_environment(
                &config_dir,
                "sqlite_home = \"sqlite_data\"",
                None
            )
            .unwrap(),
            config_dir.join("sqlite_data")
        );
        assert!(validate_codex_sqlite_home_for_delete_with_environment(
            &config_dir,
            "sqlite_home = \"../escape\"",
            None
        )
        .is_err());
        assert!(validate_codex_sqlite_home_for_delete_with_environment(
            &config_dir,
            "",
            Some("relative/env")
        )
        .is_err());
        let bad_env = config_dir.join("..").join("escape");
        assert!(validate_codex_sqlite_home_for_delete_with_environment(
            &config_dir,
            "",
            bad_env.to_str()
        )
        .is_err());
    }

    #[test]
    fn numbered_sqlite_discovery_picks_highest_generation_and_collects_cleanup_dbs() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("sqlite");
        // Missing dir falls back to default filename
        assert_eq!(
            discover_numbered_sqlite_dbs(&dir, "state", CODEX_STATE_DB_FILENAME),
            vec![dir.join(CODEX_STATE_DB_FILENAME)]
        );

        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "state_5.sqlite",
            "state_6.sqlite",
            "state_12.sqlite",
            "thread_history_1.sqlite",
            "thread_history_3.sqlite",
            "logs_2.sqlite",
            "memories_1.sqlite",
            "goals_1.sqlite",
            "queue_4.sqlite",
            "unrelated_9.sqlite",
            "state_bad.sqlite",
        ] {
            std::fs::write(dir.join(name), b"").unwrap();
        }

        let state_dbs = discover_numbered_sqlite_dbs(&dir, "state", CODEX_STATE_DB_FILENAME);
        assert_eq!(
            state_dbs,
            vec![
                dir.join("state_12.sqlite"),
                dir.join("state_6.sqlite"),
                dir.join("state_5.sqlite"),
            ]
        );
        assert_eq!(
            codex_state_db_paths_with_environment(&dir, "", None),
            vec![dir.join("state_12.sqlite")]
        );

        let cleanup_dbs = codex_cleanup_db_paths(&dir);
        assert_eq!(cleanup_dbs.len(), 9);
        assert!(cleanup_dbs.contains(&dir.join("state_12.sqlite")));
        assert!(cleanup_dbs.contains(&dir.join("state_6.sqlite")));
        assert!(cleanup_dbs.contains(&dir.join("state_5.sqlite")));
        assert!(cleanup_dbs.contains(&dir.join("thread_history_3.sqlite")));
        assert!(cleanup_dbs.contains(&dir.join("thread_history_1.sqlite")));
        assert!(cleanup_dbs.contains(&dir.join("logs_2.sqlite")));
        assert!(cleanup_dbs.contains(&dir.join("memories_1.sqlite")));
        assert!(cleanup_dbs.contains(&dir.join("goals_1.sqlite")));
        assert!(cleanup_dbs.contains(&dir.join("queue_4.sqlite")));
        assert!(!cleanup_dbs.contains(&dir.join("unrelated_9.sqlite")));
    }

    #[test]
    #[allow(deprecated)]
    fn configured_dir_trims_whitespace_and_provider_homes_respect_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let custom = temp.path().join("custom");
        std::env::set_var("ASM_TEST_TRIM_EMPTY", "   \t  ");
        assert!(configured_dir("ASM_TEST_TRIM_EMPTY").is_none());
        std::env::set_var("ASM_TEST_TRIM_PATH", format!("  {}  ", custom.display()));
        assert_eq!(configured_dir("ASM_TEST_TRIM_PATH"), Some(custom));
        std::env::remove_var("ASM_TEST_TRIM_EMPTY");
        std::env::remove_var("ASM_TEST_TRIM_PATH");
    }
}

pub fn grok_config_dir() -> PathBuf {
    configured_dir("GROK_HOME")
        .or_else(|| configured_dir("GROK_CONFIG_DIR"))
        .unwrap_or_else(|| home_dir().join(".grok"))
}

pub fn workbuddy_homes() -> Vec<PathBuf> {
    if let Some(dir) = configured_dir("WORKBUDDY_HOME") {
        return vec![dir];
    }
    let home = home_dir();
    vec![home.join(".workbuddy-ai"), home.join(".workbuddy")]
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiNativeDefaults {
    pub session_dir: Option<String>,
}

pub fn pi_agent_dir() -> Result<PathBuf, String> {
    let path = configured_dir("PI_CODING_AGENT_DIR")
        .unwrap_or_else(|| home_dir().join(".pi").join("agent"));
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(format!(
            "PI_CODING_AGENT_DIR 必须是绝对路径：{}",
            path.display()
        ))
    }
}

pub fn read_pi_native_defaults() -> Result<PiNativeDefaults, String> {
    let path = pi_agent_dir()?.join("settings.json");
    if !path.exists() {
        return Ok(PiNativeDefaults::default());
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("无法读取 Pi 设置 {}：{error}", path.display()))?;
    json5::from_str(&text).map_err(|error| format!("无法解析 Pi 设置 {}：{error}", path.display()))
}
