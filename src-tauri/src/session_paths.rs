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

fn resolve_user_path(raw: &str) -> PathBuf {
    if raw == "~" {
        return home_dir();
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        return home_dir().join(rest);
    }
    PathBuf::from(raw)
}

pub fn claude_config_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".claude"))
}

pub fn codex_config_dir() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".codex"))
}

pub fn read_codex_config_text() -> Result<String, String> {
    let path = codex_config_dir().join("config.toml");
    if !path.exists() {
        return Ok(String::new());
    }
    std::fs::read_to_string(&path)
        .map_err(|error| format!("无法读取 Codex 配置 {}：{error}", path.display()))
}

pub fn codex_state_db_paths(config_dir: &Path, config_text: &str) -> Vec<PathBuf> {
    let mut paths = vec![config_dir.join(CODEX_STATE_DB_FILENAME)];
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
        });
    let environment = std::env::var("CODEX_SQLITE_HOME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| resolve_user_path(&value));

    if let Some(directory) = configured.or(environment) {
        let path = directory.join(CODEX_STATE_DB_FILENAME);
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

pub fn grok_config_dir() -> PathBuf {
    std::env::var_os("GROK_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".grok"))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiNativeDefaults {
    pub session_dir: Option<String>,
}

pub fn pi_agent_dir() -> Result<PathBuf, String> {
    let path = std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
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
