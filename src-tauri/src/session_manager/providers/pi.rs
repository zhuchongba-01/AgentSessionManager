use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use serde::Serialize;
use serde_json::Value;

use crate::session_manager::{DeleteSessionRequest, SessionMessage, SessionMeta};

use super::utils::{
    extract_text, order_selected_children, parse_timestamp_to_ms, path_basename, truncate_summary,
    FileParseCache, TITLE_MAX_CHARS,
};

const PROVIDER_ID: &str = "pi";
pub(crate) const MAX_TREE_ENTRIES: usize = 500_000;
const MAX_TREE_ID_BYTES: usize = 256;
pub(crate) const MAX_SESSION_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionLayout {
    Flat,
    ProjectDirectories,
}

#[derive(Debug, PartialEq, Eq)]
enum SessionRootResolution {
    Available {
        root: PathBuf,
        layout: SessionLayout,
    },
    RequiresProjectContext {
        configured_path: String,
    },
    Unavailable {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PiSessionDiscovery {
    Available,
    RequiresProjectContext {
        #[serde(rename = "configuredPath")]
        configured_path: String,
    },
    Unavailable {
        reason: String,
    },
}

#[derive(Debug)]
struct SessionHeader {
    id: String,
    cwd: String,
    timestamp: Option<i64>,
    version: u64,
    parent_session: Option<String>,
}

#[derive(Debug)]
struct SessionTree {
    header: SessionHeader,
    active_entry_indexes: HashSet<usize>,
    summary: SessionSummary,
}

#[derive(Debug, Default)]
struct SessionSummary {
    first_user_message: Option<String>,
    last_message: Option<String>,
    explicit_name: Option<Option<String>>,
    last_active_at: Option<i64>,
}

#[derive(Clone)]
struct CachedPiSession {
    meta: SessionMeta,
    parent_session: Option<String>,
}

static PARSE_CACHE: LazyLock<FileParseCache<CachedPiSession>> = LazyLock::new(FileParseCache::new);

/// Pi keeps a relative `sessionDir` relative through SessionManager creation;
/// its file operations therefore depend on the launching process cwd. A global
/// session browser has no authoritative launch cwd, so relative values are
/// deliberately non-enumerable and never fall back to another root.
pub fn session_roots() -> Vec<PathBuf> {
    match resolve_session_root() {
        SessionRootResolution::Available { root, .. } => vec![root],
        SessionRootResolution::RequiresProjectContext { .. }
        | SessionRootResolution::Unavailable { .. } => Vec::new(),
    }
}

#[cfg(test)]
fn session_files_from_resolution(
    resolution: SessionRootResolution,
) -> Result<Vec<PathBuf>, String> {
    match resolution {
        SessionRootResolution::Available { root, layout } => {
            let mut files = Vec::new();
            collect_jsonl_files(&root, layout, &mut files, false);
            files.sort();
            Ok(files)
        }
        SessionRootResolution::RequiresProjectContext { configured_path } => Err(format!(
            "Pi sessionDir '{configured_path}' requires a project cwd and cannot be globally enumerated"
        )),
        SessionRootResolution::Unavailable { reason } => Err(reason),
    }
}

pub fn session_discovery() -> PiSessionDiscovery {
    match resolve_session_root() {
        SessionRootResolution::Available { .. } => PiSessionDiscovery::Available,
        SessionRootResolution::RequiresProjectContext { configured_path } => {
            PiSessionDiscovery::RequiresProjectContext { configured_path }
        }
        SessionRootResolution::Unavailable { reason } => PiSessionDiscovery::Unavailable { reason },
    }
}

fn resolve_session_root() -> SessionRootResolution {
    let home = crate::session_paths::home_dir();
    if let Some(raw) = std::env::var_os("PI_CODING_AGENT_SESSION_DIR") {
        if !raw.is_empty() {
            return classify_configured_session_dir(
                raw.to_string_lossy().as_ref(),
                &home,
                "environment",
            );
        }
    }

    match crate::session_paths::read_pi_native_defaults() {
        Ok(defaults) => {
            if let Some(value) = defaults.session_dir.filter(|value| !value.is_empty()) {
                return classify_configured_session_dir(&value, &home, "settings");
            }
        }
        Err(error) => {
            return SessionRootResolution::Unavailable {
                reason: error.to_string(),
            };
        }
    }

    match crate::session_paths::pi_agent_dir() {
        Ok(agent_dir) => SessionRootResolution::Available {
            root: agent_dir.join("sessions"),
            layout: SessionLayout::ProjectDirectories,
        },
        Err(error) => SessionRootResolution::Unavailable {
            reason: error.to_string(),
        },
    }
}

fn classify_configured_session_dir(
    value: &str,
    home: &Path,
    source: &'static str,
) -> SessionRootResolution {
    match resolve_global_session_dir(value, home) {
        Some(root) => match fs::metadata(&root) {
            Ok(metadata) if !metadata.is_dir() => SessionRootResolution::Unavailable {
                reason: format!(
                    "Configured Pi session directory from {source} is not a directory: {}",
                    root.display()
                ),
            },
            Ok(_) => match fs::read_dir(&root) {
                Ok(_) => SessionRootResolution::Available {
                    root,
                    layout: SessionLayout::Flat,
                },
                Err(error) => SessionRootResolution::Unavailable {
                    reason: format!(
                        "Configured Pi session directory from {source} is not readable ({}): {error}",
                        root.display()
                    ),
                },
            },
            Err(error) => SessionRootResolution::Unavailable {
                reason: format!(
                    "Configured Pi session directory from {source} is unavailable ({}): {error}",
                    root.display()
                ),
            },
        },
        None => SessionRootResolution::RequiresProjectContext {
            configured_path: value.to_string(),
        },
    }
}

fn resolve_global_session_dir(value: &str, home: &Path) -> Option<PathBuf> {
    let path = if value.starts_with("file://") {
        tauri::Url::parse(value).ok()?.to_file_path().ok()?
    } else if value == "~" {
        home.to_path_buf()
    } else if let Some(suffix) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        home.join(suffix)
    } else {
        PathBuf::from(value)
    };
    path.is_absolute().then_some(path)
}

pub fn scan_sessions() -> Vec<SessionMeta> {
    match resolve_session_root() {
        SessionRootResolution::Available { root, layout, .. } => {
            scan_sessions_in_root(&root, layout)
        }
        SessionRootResolution::RequiresProjectContext {
            configured_path, ..
        } => {
            super::utils::scan_warning(format!(
                "会话目录 {configured_path} 需要项目上下文，未进行全局扫描"
            ));
            log::warn!(
                "Pi sessionDir '{configured_path}' requires a project cwd and cannot be globally enumerated"
            );
            Vec::new()
        }
        SessionRootResolution::Unavailable { reason } => {
            super::utils::scan_warning(reason.clone());
            log::warn!("Pi session discovery unavailable: {reason}");
            Vec::new()
        }
    }
}

fn scan_sessions_in_root(root: &Path, layout: SessionLayout) -> Vec<SessionMeta> {
    let mut files = Vec::new();
    collect_jsonl_files(root, layout, &mut files, true);
    let parsed = PARSE_CACHE.scan(files, |path| match parse_session_with_provenance(path) {
        Ok(entry) => Ok(Some(entry)),
        Err(error) => {
            super::utils::scan_warning(error.clone());
            log::debug!("Skipping invalid Pi session {}: {error}", path.display());
            Err(error)
        }
    });
    // parentSession is provenance, not a runtime dependency. Resolve lineage
    // only against files already discovered within this root; never open it.
    let lineage_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let by_path: HashMap<_, _> = parsed
        .iter()
        .filter_map(|entry| {
            entry
                .meta
                .source_path
                .as_deref()
                .map(|source| (PathBuf::from(source), entry.meta.session_id.clone()))
        })
        .collect();
    parsed
        .into_iter()
        .map(|mut entry| {
            entry.meta.parent_session_id = entry
                .parent_session
                .as_deref()
                .and_then(|parent| lineage_path(parent, &lineage_root))
                .and_then(|parent| by_path.get(&parent).cloned())
                .filter(|parent| parent != &entry.meta.session_id);
            entry.meta
        })
        .collect()
}

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    match resolve_session_root() {
        SessionRootResolution::Available { root, layout, .. } => {
            load_messages_with_layout(&root, path, layout)
        }
        SessionRootResolution::RequiresProjectContext { .. } => {
            Err("Relative Pi sessionDir cannot be globally resolved".to_string())
        }
        SessionRootResolution::Unavailable { reason } => Err(reason),
    }
}

fn load_messages_with_layout(
    root: &Path,
    path: &Path,
    layout: SessionLayout,
) -> Result<Vec<SessionMessage>, String> {
    let (_, source) = validate_source_under_root(root, path, layout)?;
    let tree = read_tree(&source)?;
    read_active_messages(&source, &tree)
}

#[derive(Debug)]
struct SessionDescriptor {
    path: PathBuf,
    parent_path: Option<PathBuf>,
}

fn session_descriptors(
    root: &Path,
    layout: SessionLayout,
) -> Result<Vec<SessionDescriptor>, String> {
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("Failed to resolve Pi session root: {error}"))?;
    let root_meta = fs::metadata(&canonical_root).map_err(|error| {
        format!(
            "Failed to inspect Pi session root {}: {error}",
            canonical_root.display()
        )
    })?;
    if !root_meta.is_dir() {
        return Err(format!(
            "Pi session root is not a directory: {}",
            canonical_root.display()
        ));
    }
    fs::read_dir(&canonical_root)
        .map_err(|error| format!("Failed to scan {}: {error}", canonical_root.display()))?;

    let mut files = Vec::new();
    collect_jsonl_files(&canonical_root, layout, &mut files, false);
    files.sort();
    let mut descriptors = Vec::with_capacity(files.len());
    for file in files {
        let Ok((_, path)) = validate_source_under_root(&canonical_root, &file, layout) else {
            continue;
        };
        let Ok(header) = read_session_header(&path) else {
            continue;
        };
        let parent_path = header
            .parent_session
            .as_deref()
            .and_then(|parent| lineage_path(parent, &canonical_root));
        descriptors.push(SessionDescriptor { path, parent_path });
    }

    Ok(descriptors)
}

#[cfg(windows)]
fn windows_lineage_prefix_allowed(path: &Path, canonical_root: &Path) -> bool {
    use std::path::{Component, Prefix};
    let Some(Component::Prefix(path_prefix)) = path.components().next() else {
        return false;
    };
    let Some(Component::Prefix(root_prefix)) = canonical_root.components().next() else {
        return false;
    };
    match root_prefix.kind() {
        Prefix::Disk(_) | Prefix::VerbatimDisk(_) => {
            matches!(
                path_prefix.kind(),
                Prefix::Disk(_) | Prefix::VerbatimDisk(_)
            )
        }
        Prefix::UNC(srv1, share1) | Prefix::VerbatimUNC(srv1, share1) => match path_prefix.kind() {
            Prefix::UNC(srv2, share2) | Prefix::VerbatimUNC(srv2, share2) => {
                srv1.to_string_lossy()
                    .eq_ignore_ascii_case(&srv2.to_string_lossy())
                    && share1
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&share2.to_string_lossy())
            }
            _ => false,
        },
        Prefix::DeviceNS(d1) => matches!(
            path_prefix.kind(),
            Prefix::DeviceNS(d2) if d1.to_string_lossy().eq_ignore_ascii_case(&d2.to_string_lossy())
        ),
        Prefix::Verbatim(v1) => matches!(path_prefix.kind(), Prefix::Verbatim(v2) if v1 == v2),
    }
}

fn lineage_path(value: &str, canonical_root: &Path) -> Option<PathBuf> {
    let path = if value.starts_with("file://") {
        tauri::Url::parse(value).ok()?.to_file_path().ok()?
    } else {
        PathBuf::from(value)
    };
    if !path.is_absolute() {
        return None;
    }
    if !path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
    {
        return None;
    }
    if path
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return None;
    }
    #[cfg(windows)]
    if !windows_lineage_prefix_allowed(&path, canonical_root) {
        return None;
    }
    let resolved = path.canonicalize().ok()?;
    resolved.starts_with(canonical_root).then_some(resolved)
}

fn preflight_delete_with_layout(
    root: &Path,
    path: &Path,
    session_id: &str,
    layout: SessionLayout,
) -> Result<(), String> {
    if !is_valid_tree_id(session_id) {
        return Err("Invalid Pi session ID".to_string());
    }
    let (_, source) = validate_source_under_root(root, path, layout)?;
    let target = read_session_header(&source)?;
    if target.id != session_id {
        return Err(format!(
            "Pi session ID mismatch: expected {session_id}, found {}",
            target.id
        ));
    }
    // A fork contains its own copied entries. Deletion is an explicit selection,
    // not an implicit cascade and not gated on provenance elsewhere in the root.
    Ok(())
}

pub fn preflight_delete(root: &Path, path: &Path, session_id: &str) -> Result<(), String> {
    let layout = layout_for_current_root(root)?;
    preflight_delete_with_layout(root, path, session_id, layout)
}

fn deletion_order_with_descriptors(
    requests: &[DeleteSessionRequest],
    initial: &[usize],
    descriptors: &[SessionDescriptor],
) -> Vec<usize> {
    let parent_by_path = descriptors
        .iter()
        .filter_map(|descriptor| {
            descriptor
                .parent_path
                .as_ref()
                .map(|parent| (descriptor.path.as_path(), parent))
        })
        .collect::<HashMap<_, _>>();
    let sample_root = descriptors.first().map(|d| d.path.as_path());
    let selected = requests
        .iter()
        .enumerate()
        .filter(|(_, request)| request.provider_id == PROVIDER_ID)
        .filter_map(|(index, request)| {
            let raw = Path::new(&request.source_path);
            if !raw.is_absolute()
                || raw
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                return None;
            }
            #[cfg(windows)]
            if let Some(root_ref) = sample_root {
                if !windows_lineage_prefix_allowed(raw, root_ref) {
                    return None;
                }
            }
            #[cfg(not(windows))]
            let _ = sample_root;
            let raw_meta = fs::symlink_metadata(raw).ok()?;
            if super::utils::is_reparse_or_symlink(&raw_meta) {
                return None;
            }
            let path = raw.canonicalize().ok()?;
            let parent = parent_by_path
                .get(path.as_path())
                .map(|parent| parent.to_string_lossy().into_owned());
            Some((index, path.to_string_lossy().into_owned(), parent))
        })
        .collect::<Vec<_>>();
    order_selected_children(&selected, initial)
}

pub fn deletion_order(requests: &[DeleteSessionRequest], initial: &[usize]) -> Vec<usize> {
    if !requests
        .iter()
        .any(|request| request.provider_id == PROVIDER_ID)
    {
        return initial.to_vec();
    }
    let SessionRootResolution::Available { root, layout } = resolve_session_root() else {
        return initial.to_vec();
    };
    let Ok(descriptors) = session_descriptors(&root, layout) else {
        return initial.to_vec();
    };
    deletion_order_with_descriptors(requests, initial, &descriptors)
}

pub fn delete_session(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    let layout = layout_for_current_root(root)?;
    delete_session_with_layout(root, path, session_id, layout)
}

fn delete_session_with_layout(
    root: &Path,
    path: &Path,
    session_id: &str,
    layout: SessionLayout,
) -> Result<bool, String> {
    preflight_delete_with_layout(root, path, session_id, layout)?;
    let (_, source) = validate_source_under_root(root, path, layout)?;
    fs::remove_file(&source)
        .map_err(|error| format!("Failed to delete Pi session {}: {error}", source.display()))?;
    Ok(true)
}

fn layout_for_current_root(root: &Path) -> Result<SessionLayout, String> {
    let (configured_root, layout) = match resolve_session_root() {
        SessionRootResolution::Available { root, layout, .. } => (root, layout),
        SessionRootResolution::RequiresProjectContext { .. } => {
            return Err("Relative Pi sessionDir cannot be globally resolved".to_string());
        }
        SessionRootResolution::Unavailable { reason } => return Err(reason),
    };
    let configured_root = configured_root.canonicalize().map_err(|error| {
        format!(
            "Failed to resolve Pi session root {}: {error}",
            configured_root.display()
        )
    })?;
    let requested_root = root.canonicalize().map_err(|error| {
        format!(
            "Failed to resolve Pi session root {}: {error}",
            root.display()
        )
    })?;
    if configured_root != requested_root {
        return Err("Pi session root changed before deletion".to_string());
    }
    Ok(layout)
}

fn parse_session_with_provenance(path: &Path) -> Result<CachedPiSession, String> {
    let source = path
        .canonicalize()
        .map_err(|error| format!("Failed to resolve Pi session {}: {error}", path.display()))?;
    let source_path = source
        .to_str()
        .ok_or_else(|| "Pi session path is not valid UTF-8".to_string())?
        .to_string();
    let metadata =
        fs::metadata(&source).map_err(|error| format!("Failed to inspect Pi session: {error}"))?;
    let (header, summary) = if metadata.len() > MAX_SESSION_BYTES {
        let header = read_session_header(&source)?;
        super::utils::scan_warning(format!(
            "Pi session {} exceeds the {MAX_SESSION_BYTES}-byte loading limit; showing header metadata only",
            source.display()
        ));
        (header, SessionSummary::default())
    } else {
        let tree = read_tree(&source)?;
        (tree.header, tree.summary)
    };
    let title = summary.explicit_name.flatten().or_else(|| {
        summary
            .first_user_message
            .as_deref()
            .map(|message| truncate_summary(message, TITLE_MAX_CHARS))
            .filter(|message| !message.is_empty())
            .or_else(|| path_basename(&header.cwd))
    });
    let summary_text = summary
        .last_message
        .as_deref()
        .map(|message| truncate_summary(message, 160))
        .filter(|message| !message.is_empty());
    let residual = false; // parentSession is fork provenance, even when missing.
    let parent_session = header.parent_session;
    Ok(CachedPiSession {
        meta: SessionMeta {
            provider_id: PROVIDER_ID.to_string(),
            session_id: header.id,
            residual,
            // Scan resolves this provenance path to an ID only within discovered files.
            parent_session_id: None,
            archived: false,
            cleanup_pending: false,
            sidebar_section: None,
            title,
            summary: summary_text,
            project_dir: (!header.cwd.trim().is_empty()).then(|| header.cwd.clone()),
            project_name: None,
            created_at: header.timestamp,
            last_active_at: summary.last_active_at.or(header.timestamp),
            source_path: Some(source_path.clone()),
            resume_command: (!residual).then(|| {
                format!(
                    "pi --session {}",
                    crate::session_manager::terminal::shell_escape(&source_path)
                )
            }),
        },
        parent_session,
    })
}

#[cfg(test)]
fn parse_session(path: &Path) -> Result<SessionMeta, String> {
    parse_session_with_provenance(path).map(|entry| entry.meta)
}

/// Header-only fallback for oversized files: never consume the body. A bounded
/// prefix also handles legacy leading blank/invalid lines without an unbounded scan.
fn read_session_header(path: &Path) -> Result<SessionHeader, String> {
    use std::io::Read;
    const HEADER_BUDGET: u64 = 4 * 1024 * 1024;
    let file = File::open(path).map_err(|error| format!("Failed to open Pi session: {error}"))?;
    let mut reader = BufReader::new(file.take(HEADER_BUDGET + 1));
    let mut consumed = 0u64;
    loop {
        let mut line = String::new();
        let count = reader
            .read_line(&mut line)
            .map_err(|error| format!("Failed to read Pi session header: {error}"))?;
        consumed += count as u64;
        if consumed > HEADER_BUDGET {
            return Err("Pi session header exceeds the scan limit".into());
        }
        if count == 0 {
            return Err("Pi session has no valid header".into());
        }
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            return parse_header(&value);
        }
    }
}

#[derive(Debug)]
enum EntrySummaryKind {
    SessionInfo(Option<String>),
    UserMessage(String),
    AssistantMessage(String),
    ActivityOnly,
}

#[derive(Debug)]
struct EntrySummaryItem {
    ts: Option<i64>,
    kind: EntrySummaryKind,
}

fn extract_entry_summary_item(value: &Value) -> Option<EntrySummaryItem> {
    match value.get("type").and_then(Value::as_str) {
        Some("session_info") => {
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string);
            let ts = value.get("timestamp").and_then(parse_timestamp_to_ms);
            Some(EntrySummaryItem {
                ts,
                kind: EntrySummaryKind::SessionInfo(name),
            })
        }
        Some("compaction") | Some("branch_summary") | Some("bashExecution") => {
            let ts = value.get("timestamp").and_then(parse_timestamp_to_ms);
            Some(EntrySummaryItem {
                ts,
                kind: EntrySummaryKind::ActivityOnly,
            })
        }
        Some("message") => {
            let message = value.get("message")?;
            let ts = message
                .get("timestamp")
                .and_then(parse_timestamp_to_ms)
                .or_else(|| value.get("timestamp").and_then(parse_timestamp_to_ms));
            let raw_role = message.get("role").and_then(Value::as_str);
            if let Some((role, content)) = parse_message(message) {
                match role.as_str() {
                    "user" => Some(EntrySummaryItem {
                        ts,
                        kind: EntrySummaryKind::UserMessage(content),
                    }),
                    "assistant" => Some(EntrySummaryItem {
                        ts,
                        kind: EntrySummaryKind::AssistantMessage(content),
                    }),
                    _ if matches!(
                        raw_role,
                        Some("bashExecution" | "branchSummary" | "compactionSummary")
                    ) =>
                    {
                        Some(EntrySummaryItem {
                            ts,
                            kind: EntrySummaryKind::ActivityOnly,
                        })
                    }
                    _ => None,
                }
            } else if raw_role == Some("assistant")
                && message
                    .get("content")
                    .map(super::utils::extract_thinking_text)
                    .is_some_and(|thinking| !thinking.trim().is_empty())
            {
                Some(EntrySummaryItem {
                    ts,
                    kind: EntrySummaryKind::ActivityOnly,
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

fn read_tree(path: &Path) -> Result<SessionTree, String> {
    validate_file_size(path)?;
    let reader = BufReader::new(
        File::open(path).map_err(|error| format!("Failed to open Pi session: {error}"))?,
    );
    let mut header = None;
    let mut parents = HashMap::<String, (Option<String>, usize)>::new();
    let mut entry_items = HashMap::<usize, EntrySummaryItem>::new();
    let mut global_explicit_name: Option<Option<String>> = None;
    let mut latest_id = None;
    let mut legacy_previous_id = None;
    let mut entry_index = 0usize;
    for line in reader.lines() {
        let line = line.map_err(|error| format!("Failed to read Pi session: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if header.is_none() {
            header = Some(parse_header(&value)?);
            continue;
        }
        entry_index += 1;
        if entry_index > MAX_TREE_ENTRIES {
            return Err(format!(
                "Pi session exceeds the {MAX_TREE_ENTRIES}-entry safety limit"
            ));
        }
        let version = header
            .as_ref()
            .map_or(1, |item: &SessionHeader| item.version);
        let Some((id, parent_id)) =
            entry_identity(&value, version, entry_index, legacy_previous_id.as_deref())
        else {
            continue;
        };
        if let Some(item) = extract_entry_summary_item(&value) {
            if let EntrySummaryKind::SessionInfo(ref name) = item.kind {
                global_explicit_name = Some(name.clone());
            }
            entry_items.insert(entry_index, item);
        }
        if parents
            .insert(id.clone(), (parent_id, entry_index))
            .is_some()
        {
            log::debug!("Pi session contains duplicate entry ID {id}; using the latest entry");
        }
        latest_id = Some(id.clone());
        legacy_previous_id = Some(id);
    }
    let header = header.ok_or_else(|| "Pi session has no valid header".to_string())?;
    let mut active_entry_indexes = HashSet::new();
    let mut visited_ids = HashSet::new();
    let mut current = latest_id;
    while let Some(id) = current {
        if !visited_ids.insert(id.clone()) {
            log::debug!("Pi session tree contains a cycle at entry {id}; stopping traversal");
            break;
        }
        let Some((parent_id, entry_index)) = parents.get(&id) else {
            log::debug!("Pi session tree references missing entry {id}; stopping traversal");
            break;
        };
        active_entry_indexes.insert(*entry_index);
        current = parent_id.clone();
    }

    let mut ordered_active: Vec<usize> = active_entry_indexes.iter().copied().collect();
    ordered_active.sort_unstable();
    let mut summary = SessionSummary::default();
    for index in ordered_active {
        let Some(item) = entry_items.remove(&index) else {
            continue;
        };
        if let Some(timestamp) = item.ts {
            summary.last_active_at = Some(
                summary
                    .last_active_at
                    .map_or(timestamp, |current| current.max(timestamp)),
            );
        }
        match item.kind {
            EntrySummaryKind::SessionInfo(name) => {
                summary.explicit_name = Some(name);
            }
            EntrySummaryKind::UserMessage(content) => {
                if summary.first_user_message.is_none() {
                    summary.first_user_message = Some(content.clone());
                }
                summary.last_message = Some(content);
            }
            EntrySummaryKind::AssistantMessage(content) => {
                summary.last_message = Some(content);
            }
            EntrySummaryKind::ActivityOnly => {}
        }
    }
    if summary.explicit_name.is_none() {
        summary.explicit_name = global_explicit_name;
    }

    Ok(SessionTree {
        header,
        active_entry_indexes,
        summary,
    })
}

fn read_active_messages(path: &Path, tree: &SessionTree) -> Result<Vec<SessionMessage>, String> {
    validate_file_size(path)?;
    let reader = BufReader::new(
        File::open(path).map_err(|error| format!("Failed to open Pi session: {error}"))?,
    );
    let mut messages = Vec::new();
    let mut saw_header = false;
    let mut entry_index = 0usize;
    let mut legacy_previous_id = None;
    for line in reader.lines() {
        let line = line.map_err(|error| format!("Failed to read Pi session: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if !saw_header {
            if value.get("type").and_then(Value::as_str) == Some("session") {
                saw_header = true;
            }
            continue;
        }
        entry_index += 1;
        if entry_index > MAX_TREE_ENTRIES {
            return Err(format!(
                "Pi session exceeds the {MAX_TREE_ENTRIES}-entry safety limit"
            ));
        }
        let Some((id, _)) = entry_identity(
            &value,
            tree.header.version,
            entry_index,
            legacy_previous_id.as_deref(),
        ) else {
            continue;
        };
        legacy_previous_id = Some(id.clone());
        if !tree.active_entry_indexes.contains(&entry_index) {
            continue;
        }
        let entry_timestamp = value.get("timestamp").and_then(parse_timestamp_to_ms);
        match value.get("type").and_then(Value::as_str) {
            Some("session_info") => {}
            Some("message") => {
                let Some(message) = value.get("message") else {
                    continue;
                };
                let timestamp = message
                    .get("timestamp")
                    .and_then(parse_timestamp_to_ms)
                    .or(entry_timestamp);
                if message.get("role").and_then(Value::as_str) == Some("assistant") {
                    if let Some(msg_content) = message.get("content") {
                        let thinking = super::utils::extract_thinking_text(msg_content);
                        if !thinking.trim().is_empty() {
                            messages.push(SessionMessage {
                                role: "thinking".to_string(),
                                content: thinking,
                                ts: timestamp,
                                meta: None,
                            });
                        }
                    }
                }
                if let Some((role, content)) = parse_message(message) {
                    messages.push(SessionMessage {
                        role,
                        content,
                        ts: timestamp,
                        meta: None,
                    });
                }
            }
            Some("compaction") | Some("branch_summary") => {
                push_system(
                    &mut messages,
                    value
                        .get("summary")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    entry_timestamp,
                );
            }
            Some("custom_message")
                if value.get("display").and_then(Value::as_bool) != Some(false) =>
            {
                push_system(
                    &mut messages,
                    &value.get("content").map(extract_text).unwrap_or_default(),
                    entry_timestamp,
                );
            }
            _ => {}
        }
    }
    Ok(messages)
}

fn push_system(messages: &mut Vec<SessionMessage>, content: &str, ts: Option<i64>) {
    if !content.trim().is_empty() {
        messages.push(SessionMessage {
            role: "system".to_string(),
            content: content.to_string(),
            ts,
            meta: None,
        });
    }
}

fn parse_header(value: &Value) -> Result<SessionHeader, String> {
    if value.get("type").and_then(Value::as_str) != Some("session") {
        return Err("Pi session header must be the first valid JSON entry".to_string());
    }
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| is_valid_tree_id(id))
        .ok_or_else(|| "Pi session header has an invalid ID".to_string())?
        .to_string();
    let version = value.get("version").and_then(Value::as_u64).unwrap_or(1);
    if version == 0 {
        return Err(format!("Unsupported Pi session version: {version}"));
    }
    let parent_session = match value.get("parentSession") {
        None | Some(Value::Null) => None,
        Some(Value::String(parent)) if !parent.trim().is_empty() => Some(parent.clone()),
        Some(Value::String(_)) => None,
        Some(_) => return Err("Pi session header has an invalid parentSession".to_string()),
    };
    Ok(SessionHeader {
        id,
        cwd: value
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        timestamp: value.get("timestamp").and_then(parse_timestamp_to_ms),
        version,
        parent_session,
    })
}

fn entry_identity(
    value: &Value,
    version: u64,
    entry_index: usize,
    legacy_previous_id: Option<&str>,
) -> Option<(String, Option<String>)> {
    if version < 2 {
        return Some((
            format!("legacy-{entry_index}"),
            legacy_previous_id.map(str::to_string),
        ));
    }
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| is_valid_tree_id(id))?
        .to_string();
    let parent_id = match value.get("parentId") {
        None | Some(Value::Null) => None,
        Some(Value::String(parent)) if is_valid_tree_id(parent) => Some(parent.clone()),
        _ => return None,
    };
    Some((id, parent_id))
}

fn parse_message(message: &Value) -> Option<(String, String)> {
    let role = message.get("role").and_then(Value::as_str)?;
    let (display_role, content) = match role {
        "user" => (
            "user".to_string(),
            message.get("content").map(extract_text).unwrap_or_default(),
        ),
        "assistant" => {
            let text = message.get("content").map(extract_text).unwrap_or_default();
            let content = if text.trim().is_empty() {
                message
                    .get("errorMessage")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|err| !err.is_empty())
                    .map(|err| format!("[Error] {err}"))
                    .unwrap_or_default()
            } else {
                text
            };
            ("assistant".to_string(), content)
        }
        "custom" | "hookMessage"
            if message.get("display").and_then(Value::as_bool) != Some(false) =>
        {
            (
                "system".to_string(),
                message.get("content").map(extract_text).unwrap_or_default(),
            )
        }
        "toolResult" => (
            "tool".to_string(),
            message.get("content").map(extract_text).unwrap_or_default(),
        ),
        "bashExecution" => (
            "tool".to_string(),
            format!(
                "$ {}\n{}",
                message
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                message
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            ),
        ),
        "branchSummary" | "compactionSummary" => (
            "system".to_string(),
            message
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        _ => return None,
    };
    (!content.trim().is_empty()).then_some((display_role, content))
}

fn validate_source_under_root(
    root: &Path,
    path: &Path,
    layout: SessionLayout,
) -> Result<(PathBuf, PathBuf), String> {
    let raw_meta = fs::symlink_metadata(path)
        .map_err(|error| format!("Failed to inspect Pi session {}: {error}", path.display()))?;
    if super::utils::is_reparse_or_symlink(&raw_meta) {
        return Err(format!(
            "Pi session source must not be a symlink or reparse point: {}",
            path.display()
        ));
    }
    let canonical_root = root.canonicalize().map_err(|error| {
        format!(
            "Failed to resolve Pi session root {}: {error}",
            root.display()
        )
    })?;
    super::utils::checked_storage_child(root, path)?;
    let source = path
        .canonicalize()
        .map_err(|error| format!("Failed to resolve Pi session {}: {error}", path.display()))?;
    if !source.starts_with(&canonical_root) {
        return Err(format!(
            "Pi session source is outside the session root: {}",
            path.display()
        ));
    }
    if !matches_session_layout(&canonical_root, &source, layout) {
        return Err(format!(
            "Pi session source does not match the active directory layout: {}",
            path.display()
        ));
    }
    let metadata = fs::symlink_metadata(&source)
        .map_err(|error| format!("Failed to inspect Pi session {}: {error}", source.display()))?;
    if super::utils::is_reparse_or_symlink(&metadata)
        || !metadata.file_type().is_file()
        || source.extension().and_then(|value| value.to_str()) != Some("jsonl")
    {
        return Err(format!("Invalid Pi session file: {}", source.display()));
    }
    Ok((canonical_root, source))
}

fn matches_session_layout(root: &Path, source: &Path, layout: SessionLayout) -> bool {
    let Ok(relative) = source.strip_prefix(root) else {
        return false;
    };
    let components = relative.components().collect::<Vec<_>>();
    let session_components = match layout {
        SessionLayout::Flat => components.as_slice(),
        SessionLayout::ProjectDirectories if components.len() >= 2 => &components[1..],
        SessionLayout::ProjectDirectories => return false,
    };
    if session_components.len() % 2 == 0 {
        return false;
    }
    session_components
        .iter()
        .enumerate()
        .all(|(index, component)| index % 2 == 0 || component.as_os_str() == "forks")
}

fn collect_session_level(
    directory: &Path,
    output: &mut Vec<PathBuf>,
    enforce_size_limit: bool,
    depth: usize,
) {
    const MAX_FORK_DEPTH: usize = 32;
    if depth > MAX_FORK_DEPTH {
        super::utils::scan_warning(format!(
            "Pi fork nesting exceeds the {MAX_FORK_DEPTH}-level safety limit: {}",
            directory.display()
        ));
        return;
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            super::utils::scan_warning(format!("Failed to scan {}: {error}", directory.display()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                super::utils::scan_warning(format!(
                    "Failed to enumerate {}: {error}",
                    directory.display()
                ));
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(kind) => kind,
            Err(error) => {
                super::utils::scan_warning(format!(
                    "Failed to inspect {}: {error}",
                    entry.path().display()
                ));
                continue;
            }
        };
        let entry_path = entry.path();
        let Ok(entry_meta) = fs::symlink_metadata(&entry_path) else {
            continue;
        };
        if super::utils::is_reparse_or_symlink(&entry_meta) {
            continue;
        }
        if file_type.is_file() {
            push_jsonl_file(&entry, output, enforce_size_limit);
            continue;
        }
        if !file_type.is_dir() {
            continue;
        }
        let forks = entry_path.join("forks");
        match fs::symlink_metadata(&forks) {
            Ok(metadata)
                if metadata.file_type().is_dir()
                    && !super::utils::is_reparse_or_symlink(&metadata) =>
            {
                collect_session_level(&forks, output, enforce_size_limit, depth + 1);
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                super::utils::scan_warning(format!(
                    "Failed to inspect {}: {error}",
                    forks.display()
                ));
            }
            _ => {}
        }
    }
}

fn validate_file_size(path: &Path) -> Result<(), String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("Failed to inspect Pi session: {error}"))?;
    if metadata.len() > MAX_SESSION_BYTES {
        Err(format!(
            "Pi session exceeds the {MAX_SESSION_BYTES}-byte safety limit"
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn is_valid_tree_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_TREE_ID_BYTES
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn collect_jsonl_files(
    root: &Path,
    layout: SessionLayout,
    output: &mut Vec<PathBuf>,
    enforce_size_limit: bool,
) {
    match layout {
        SessionLayout::Flat => collect_session_level(root, output, enforce_size_limit, 0),
        SessionLayout::ProjectDirectories => {
            let entries = match fs::read_dir(root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(error) => {
                    super::utils::scan_warning(format!(
                        "Failed to scan {}: {error}",
                        root.display()
                    ));
                    return;
                }
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        super::utils::scan_warning(format!(
                            "Failed to enumerate {}: {error}",
                            root.display()
                        ));
                        continue;
                    }
                };
                match entry.file_type() {
                    Ok(kind) if kind.is_dir() => {
                        let child_path = entry.path();
                        if fs::symlink_metadata(&child_path)
                            .is_ok_and(|meta| !super::utils::is_reparse_or_symlink(&meta))
                        {
                            collect_session_level(&child_path, output, enforce_size_limit, 0);
                        }
                    }
                    Err(error) => super::utils::scan_warning(format!(
                        "Failed to inspect {}: {error}",
                        entry.path().display()
                    )),
                    _ => {}
                }
            }
        }
    }
}

fn push_jsonl_file(entry: &fs::DirEntry, output: &mut Vec<PathBuf>, _enforce_size_limit: bool) {
    let path = entry.path();
    // Oversized sessions remain discoverable; parse_session reads only their
    // bounded header, while the message loader enforces the body limit.
    if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
        output.push(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session_header(path: &Path, id: &str) {
        fs::create_dir_all(path.parent().expect("session parent")).expect("create session parent");
        fs::write(
            path,
            format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"cwd\":\"/work\"}}\n"),
        )
        .expect("write session");
    }

    fn write_fork_header(path: &Path, id: &str, parent: &Path) {
        fs::create_dir_all(path.parent().expect("fork parent")).expect("create fork parent");
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": id,
            "cwd": "/work",
            "parentSession": parent,
        });
        fs::write(path, format!("{header}\n")).expect("write fork session");
    }

    #[test]
    fn compat_scans_native_forks_without_marking_missing_provenance_residual() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        let project = root.join("project");
        let parent = project.join("parent.jsonl");
        let child = project.join("parent").join("forks").join("child.jsonl");
        write_session_header(&parent, "session-parent");
        write_fork_header(&child, "session-child", &parent);
        let artifact = project
            .join("subagent-artifacts")
            .join("worker_transcript.jsonl");
        fs::create_dir_all(artifact.parent().expect("artifact parent")).expect("artifact dir");
        fs::write(&artifact, "{\"type\":\"message\"}\n").expect("artifact transcript");

        let sessions = scan_sessions_in_root(&root, SessionLayout::ProjectDirectories);
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().all(|session| !session.residual));
        assert!(sessions
            .iter()
            .any(|session| session.session_id == "session-child"));

        fs::remove_file(&parent).expect("remove parent fixture");
        let sessions = scan_sessions_in_root(&root, SessionLayout::ProjectDirectories);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "session-child");
        assert!(!sessions[0].residual);
        assert!(sessions[0].resume_command.is_some());
    }

    #[test]
    fn compat_parent_delete_does_not_require_removing_self_contained_fork() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        let project = root.join("project");
        let parent = project.join("parent.jsonl");
        let child = project.join("parent").join("forks").join("child.jsonl");
        write_session_header(&parent, "session-parent");
        write_fork_header(&child, "session-child", &parent);

        preflight_delete_with_layout(
            &root,
            &parent,
            "session-parent",
            SessionLayout::ProjectDirectories,
        )
        .expect("fork does not depend on parent file");
        assert!(parent.exists());
        assert!(child.exists());

        assert!(delete_session_with_layout(
            &root,
            &child,
            "session-child",
            SessionLayout::ProjectDirectories,
        )
        .expect("delete child"));
        assert!(parent.exists());
        assert!(!child.exists());
        assert!(delete_session_with_layout(
            &root,
            &parent,
            "session-parent",
            SessionLayout::ProjectDirectories,
        )
        .expect("delete parent"));
    }

    #[test]
    fn selected_pi_forks_are_ordered_before_parents() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        let parent = root.join("parent.jsonl");
        let child = root.join("parent").join("forks").join("child.jsonl");
        write_session_header(&parent, "session-parent");
        write_fork_header(&child, "session-child", &parent);
        let descriptors = session_descriptors(&root, SessionLayout::Flat).expect("descriptors");
        let requests = vec![
            DeleteSessionRequest {
                provider_id: PROVIDER_ID.into(),
                session_id: "session-parent".into(),
                source_path: parent.to_string_lossy().into_owned(),
                include_project: false,
            },
            DeleteSessionRequest {
                provider_id: PROVIDER_ID.into(),
                session_id: "session-child".into(),
                source_path: child.to_string_lossy().into_owned(),
                include_project: false,
            },
        ];
        assert_eq!(
            deletion_order_with_descriptors(&requests, &[0, 1], &descriptors),
            vec![1, 0]
        );
    }

    #[test]
    #[ignore = "Opt-in read-only audit of the locally installed Pi session storage"]
    fn audit_real_pi_sessions_read_only() {
        let SessionRootResolution::Available { root, layout } = resolve_session_root() else {
            eprintln!("Pi session storage is not globally available");
            return;
        };
        let sessions = scan_sessions_in_root(&root, layout);
        let descriptors = session_descriptors(&root, layout).expect("validate Pi descriptor graph");
        eprintln!(
            "Pi audit: sessions={}, residuals={}, descriptors={}",
            sessions.len(),
            sessions.iter().filter(|session| session.residual).count(),
            descriptors.len()
        );
    }

    #[test]
    fn compat_external_fork_provenance_never_blocks_unrelated_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let external = temp.path().join("external.jsonl");
        let fork = root.join("fork.jsonl");
        let target = root.join("target.jsonl");
        write_session_header(&external, "external");
        write_fork_header(&fork, "fork", &external);
        write_session_header(&target, "target");
        fs::write(root.join("malformed.jsonl"), "not-json").unwrap();
        assert!(delete_session_with_layout(&root, &target, "target", SessionLayout::Flat).unwrap());
        assert!(external.exists());
        assert!(fork.exists());
        assert!(delete_session_with_layout(&root, &fork, "fork", SessionLayout::Flat).unwrap());
        assert!(external.exists());
    }

    #[test]
    fn compat_lineage_maps_only_discovered_source_and_parent_deletion_preserves_fork() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let parent = root.join("parent.jsonl");
        let child = root.join("child.jsonl");
        write_session_header(&parent, "parent");
        write_fork_header(&child, "child", &parent);
        let sessions = scan_sessions_in_root(root, SessionLayout::Flat);
        assert_eq!(
            sessions
                .iter()
                .find(|s| s.session_id == "child")
                .unwrap()
                .parent_session_id
                .as_deref(),
            Some("parent")
        );
        assert!(delete_session_with_layout(root, &parent, "parent", SessionLayout::Flat).unwrap());
        assert!(child.exists());
        let child = parse_session(&child).unwrap();
        assert!(!child.residual);
        assert!(child.resume_command.is_some());
    }

    #[test]
    fn compat_file_url_roots_decode_without_expanding_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("space 中文");
        fs::create_dir(&root).unwrap();
        let uri = tauri::Url::from_directory_path(&root).unwrap().to_string();
        let normalized = resolve_global_session_dir(&uri, temp.path()).unwrap();
        assert_eq!(
            normalized.canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
        assert!(matches!(
            classify_configured_session_dir(&uri, temp.path(), "settings"),
            SessionRootResolution::Available {
                layout: SessionLayout::Flat,
                ..
            }
        ));
        assert!(resolve_global_session_dir("project/.pi/sessions", temp.path()).is_none());
    }

    #[test]
    fn compat_custom_roles_and_legacy_hook_are_distinct_from_custom_entries() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        let entries = [
            serde_json::json!({"type":"session","id":"custom-session","version":2,"cwd":"/work"}),
            serde_json::json!({"type":"message","id":"a","parentId":null,"message":{"role":"hookMessage","content":"legacy hook"}}),
            serde_json::json!({"type":"message","id":"b","parentId":"a","message":{"role":"custom","content":[{"type":"text","text":"custom role"}],"display":true}}),
            serde_json::json!({"type":"message","id":"c","parentId":"b","message":{"role":"custom","content":"hidden role","display":false}}),
            serde_json::json!({"type":"custom","id":"d","parentId":"c","content":"extension state"}),
            serde_json::json!({"type":"custom_message","id":"e","parentId":"d","content":"custom entry","display":true}),
            serde_json::json!({"type":"custom_message","id":"f","parentId":"e","content":"hidden entry","display":false}),
        ];
        fs::write(
            &path,
            entries
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let messages = load_messages_with_layout(temp.path(), &path, SessionLayout::Flat).unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            vec!["legacy hook", "custom role", "custom entry"]
        );
        assert!(messages.iter().all(|m| m.role == "system"));
    }

    #[test]
    fn latest_leaf_defines_the_active_branch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("tree.jsonl");
        fs::write(
            &path,
            "{\"type\":\"session\",\"version\":3,\"id\":\"session-1\",\"cwd\":\"/work\"}\n\
             {\"type\":\"message\",\"id\":\"root\",\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":\"question\"}}\n\
             {\"type\":\"message\",\"id\":\"dead\",\"parentId\":\"root\",\"message\":{\"role\":\"assistant\",\"content\":\"abandoned\"}}\n\
             {\"type\":\"message\",\"id\":\"live\",\"parentId\":\"root\",\"message\":{\"role\":\"assistant\",\"content\":\"active\"}}\n",
        )
        .expect("session");
        let messages =
            load_messages_with_layout(&root, &path, SessionLayout::Flat).expect("messages");
        assert_eq!(
            messages
                .into_iter()
                .map(|message| message.content)
                .collect::<Vec<_>>(),
            vec!["question", "active"]
        );
    }

    #[test]
    fn future_session_versions_are_read_with_current_semantics() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("future.jsonl");
        fs::write(
            &path,
            "{\"type\":\"session\",\"version\":4,\"id\":\"session-1\",\"cwd\":\"/work\"}\n\
             {\"type\":\"message\",\"id\":\"message-1\",\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":\"future\"}}\n",
        )
        .expect("session");

        let session = parse_session(&path).expect("parse future session");
        assert_eq!(session.summary.as_deref(), Some("future"));
    }

    #[test]
    fn duplicate_entry_ids_use_the_latest_entry() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("duplicate.jsonl");
        fs::write(
            &path,
            "{\"type\":\"session\",\"version\":3,\"id\":\"session-1\",\"cwd\":\"/work\"}\n\
             {\"type\":\"message\",\"id\":\"root\",\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":\"root\"}}\n\
             {\"type\":\"message\",\"id\":\"duplicate\",\"parentId\":\"root\",\"message\":{\"role\":\"assistant\",\"content\":\"stale\"}}\n\
             {\"type\":\"message\",\"id\":\"duplicate\",\"parentId\":\"root\",\"message\":{\"role\":\"assistant\",\"content\":\"latest\"}}\n",
        )
        .expect("session");

        let messages =
            load_messages_with_layout(&root, &path, SessionLayout::Flat).expect("messages");
        assert_eq!(
            messages
                .into_iter()
                .map(|message| message.content)
                .collect::<Vec<_>>(),
            vec!["root", "latest"]
        );
    }

    #[test]
    fn missing_parents_and_cycles_stop_at_the_readable_branch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        fs::create_dir_all(&root).expect("root");

        let missing = root.join("missing.jsonl");
        fs::write(
            &missing,
            "{\"type\":\"session\",\"version\":3,\"id\":\"session-1\",\"cwd\":\"/work\"}\n\
             {\"type\":\"message\",\"id\":\"orphan\",\"parentId\":\"missing\",\"message\":{\"role\":\"assistant\",\"content\":\"visible orphan\"}}\n",
        )
        .expect("missing-parent session");
        let messages = load_messages_with_layout(&root, &missing, SessionLayout::Flat)
            .expect("missing parent is tolerated");
        assert_eq!(messages[0].content, "visible orphan");

        let cycle = root.join("cycle.jsonl");
        fs::write(
            &cycle,
            "{\"type\":\"session\",\"version\":3,\"id\":\"session-2\",\"cwd\":\"/work\"}\n\
             {\"type\":\"message\",\"id\":\"a\",\"parentId\":\"b\",\"message\":{\"role\":\"user\",\"content\":\"a\"}}\n\
             {\"type\":\"message\",\"id\":\"b\",\"parentId\":\"a\",\"message\":{\"role\":\"assistant\",\"content\":\"b\"}}\n",
        )
        .expect("cyclic session");
        let messages = load_messages_with_layout(&root, &cycle, SessionLayout::Flat)
            .expect("cycle is tolerated");
        assert_eq!(
            messages
                .into_iter()
                .map(|message| message.content)
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn global_name_and_malformed_line_follow_pi_semantics() {
        // Pi keeps the latest global session_info even when its branch is
        // inactive, and skips malformed JSONL lines when opening a session.
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("captured.jsonl");
        fs::write(
            &path,
            "{\"type\":\"session\",\"version\":3,\"id\":\"session-1\",\"cwd\":\"/work\"}\n\
             {\"type\":\"message\",\"id\":\"root\",\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":\"root\"}}\n\
             {not valid json\n\
             {\"type\":\"session_info\",\"id\":\"dead-name\",\"parentId\":\"root\",\"name\":\"Abandoned branch name\"}\n\
             {\"type\":\"message\",\"id\":\"dead\",\"parentId\":\"dead-name\",\"message\":{\"role\":\"assistant\",\"content\":\"abandoned\"}}\n\
             {\"type\":\"message\",\"id\":\"live\",\"parentId\":\"root\",\"message\":{\"role\":\"user\",\"content\":\"active branch\"}}\n",
        )
        .expect("captured session");

        let session = parse_session(&path).expect("parse capture semantics");
        assert_eq!(session.title.as_deref(), Some("Abandoned branch name"));
        assert_eq!(session.summary.as_deref(), Some("active branch"));
    }

    #[test]
    fn relative_root_is_explicitly_non_enumerable() {
        assert_eq!(
            resolve_global_session_dir(".pi/sessions", Path::new("/home/pi")),
            None
        );
        assert_eq!(
            classify_configured_session_dir(".pi/sessions", Path::new("/home/pi"), "settings"),
            SessionRootResolution::RequiresProjectContext {
                configured_path: ".pi/sessions".to_string(),
            }
        );
    }

    #[test]
    fn unavailable_explicit_roots_are_not_reported_as_empty_history() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing = temp.path().join("missing");
        assert!(matches!(
            classify_configured_session_dir(
                missing.to_string_lossy().as_ref(),
                temp.path(),
                "settings"
            ),
            SessionRootResolution::Unavailable { .. }
        ));

        let file = temp.path().join("sessions.jsonl");
        fs::write(&file, b"").expect("session file");
        assert!(matches!(
            classify_configured_session_dir(
                file.to_string_lossy().as_ref(),
                temp.path(),
                "settings"
            ),
            SessionRootResolution::Unavailable { .. }
        ));

        let directory = temp.path().join("sessions");
        fs::create_dir(&directory).expect("session directory");
        assert!(matches!(
            classify_configured_session_dir(
                directory.to_string_lossy().as_ref(),
                temp.path(),
                "settings"
            ),
            SessionRootResolution::Available { root, .. } if root == directory
        ));

        let unavailable = SessionRootResolution::Unavailable {
            reason: "fixture unavailable".to_string(),
        };
        assert_eq!(
            session_files_from_resolution(unavailable).expect_err("discovery error"),
            "fixture unavailable"
        );
        let relative = SessionRootResolution::RequiresProjectContext {
            configured_path: ".pi/sessions".to_string(),
        };
        assert!(session_files_from_resolution(relative)
            .expect_err("relative discovery error")
            .contains("requires a project cwd"));
    }

    #[test]
    fn v3_shape_round_trips_all_consumed_fields() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("captured.jsonl");
        fs::write(
            &path,
            "{\"type\":\"session\",\"version\":3,\"id\":\"agent-manager-capture-session\",\"timestamp\":\"2023-11-14T22:13:20.000Z\",\"cwd\":\"/work/captured\",\"parentSession\":null}\n\
             {\"type\":\"session_info\",\"id\":\"00000000-0000-7000-8000-000000000001\",\"parentId\":null,\"timestamp\":\"2023-11-14T22:13:20.100Z\",\"name\":\"Captured session\"}\n\
             {\"type\":\"message\",\"id\":\"00000000-0000-7000-8000-000000000002\",\"parentId\":\"00000000-0000-7000-8000-000000000001\",\"timestamp\":\"2023-11-14T22:13:20.200Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"captured question\"}],\"timestamp\":1700000000000}}\n\
             {\"type\":\"message\",\"id\":\"00000000-0000-7000-8000-000000000003\",\"parentId\":\"00000000-0000-7000-8000-000000000002\",\"timestamp\":\"2023-11-14T22:13:21.200Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"captured answer\"}],\"api\":\"openai-responses\",\"provider\":\"capture\",\"model\":\"capture-model\",\"usage\":{\"input\":1,\"output\":1,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":2,\"cost\":{\"input\":0,\"output\":0,\"cacheRead\":0,\"cacheWrite\":0,\"total\":0}},\"stopReason\":\"stop\",\"timestamp\":1700000001000}}\n",
        )
        .expect("captured session");

        let session = parse_session(&path).expect("parse capture-generated session");
        assert_eq!(session.session_id, "agent-manager-capture-session");
        assert_eq!(session.title.as_deref(), Some("Captured session"));
        assert_eq!(session.summary.as_deref(), Some("captured answer"));
        assert_eq!(session.project_dir.as_deref(), Some("/work/captured"));
        assert_eq!(session.created_at, Some(1_700_000_000_000));
        // Pi's session picker prefers the message timestamp over the enclosing
        // entry timestamp when both are present.
        assert_eq!(session.last_active_at, Some(1_700_000_001_000));
        // Pi resumes a session by its exact file path.
        assert!(session
            .resume_command
            .as_deref()
            .is_some_and(|command| command.starts_with("pi --session ")));

        let messages =
            load_messages_with_layout(&root, &path, SessionLayout::Flat).expect("load messages");
        assert_eq!(
            messages
                .iter()
                .map(|message| (message.role.as_str(), message.content.as_str(), message.ts))
                .collect::<Vec<_>>(),
            vec![
                ("user", "captured question", Some(1_700_000_000_000)),
                ("assistant", "captured answer", Some(1_700_000_001_000)),
            ]
        );
    }

    #[test]
    fn deletion_requires_containment_and_matching_header_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("session.jsonl");
        write_session_header(&path, "session-1");
        assert!(delete_session_with_layout(&root, &path, "other", SessionLayout::Flat).is_err());
        assert!(path.exists());
        assert!(
            delete_session_with_layout(&root, &path, "session-1", SessionLayout::Flat)
                .expect("delete")
        );
    }

    #[test]
    fn scanning_respects_the_active_directory_layout() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        let flat = root.join("flat.jsonl");
        let project = root.join("project").join("project.jsonl");
        write_session_header(&flat, "flat-session");
        write_session_header(&project, "project-session");

        let flat_sessions = scan_sessions_in_root(&root, SessionLayout::Flat);
        assert_eq!(flat_sessions.len(), 1);
        assert_eq!(flat_sessions[0].session_id, "flat-session");

        let project_sessions = scan_sessions_in_root(&root, SessionLayout::ProjectDirectories);
        assert_eq!(project_sessions.len(), 1);
        assert_eq!(project_sessions[0].session_id, "project-session");
    }

    #[test]
    fn regression_oversized_session_remains_visible_from_header_without_reading_body() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("large.jsonl");
        write_session_header(&path, "large-session");
        // Sparse NUL body is deliberately not JSON. Header-only discovery must
        // not read a 128 MiB unterminated body line to obtain the list entry.
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_SESSION_BYTES + 1)
            .unwrap();
        let sessions = scan_sessions_in_root(temp.path(), SessionLayout::Flat);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "large-session");
        assert_eq!(sessions[0].project_dir.as_deref(), Some("/work"));
        assert!(sessions[0].summary.is_none());
        assert!(
            load_messages_with_layout(temp.path(), &path, SessionLayout::Flat)
                .unwrap_err()
                .contains("safety limit")
        );
    }

    #[test]
    fn regression_session_size_boundary_accepts_exact_limit_rejects_next_byte() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("boundary.jsonl");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_SESSION_BYTES).unwrap();
        assert!(validate_file_size(&path).is_ok());
        file.set_len(MAX_SESSION_BYTES + 1).unwrap();
        assert!(validate_file_size(&path).is_err());
    }

    #[test]
    fn regression_oversized_header_fallback_is_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("no-header.jsonl");
        File::create(&path)
            .unwrap()
            .set_len(MAX_SESSION_BYTES + 1)
            .unwrap();
        assert!(parse_session(&path).unwrap_err().contains("header exceeds"));
    }

    #[test]
    fn compat_oversized_child_does_not_block_parent_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent.jsonl");
        let child = temp.path().join("child.jsonl");
        write_session_header(&parent, "parent-session");
        write_fork_header(&child, "child-session", &parent);
        File::options()
            .write(true)
            .open(&child)
            .unwrap()
            .set_len(MAX_SESSION_BYTES + 1)
            .unwrap();
        preflight_delete_with_layout(temp.path(), &parent, "parent-session", SessionLayout::Flat)
            .expect("fork is self-contained");
        assert!(parent.exists());
        assert!(preflight_delete_with_layout(
            temp.path(),
            &child,
            "child-session",
            SessionLayout::Flat
        )
        .is_ok());
    }

    #[test]
    fn regression_descriptor_discovery_rejects_non_directory_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        fs::write(&root, "not a directory").unwrap();
        assert!(session_descriptors(&root, SessionLayout::Flat).is_err());
    }

    #[test]
    fn usage_collection_keeps_oversized_files_for_error_reporting() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("oversized.jsonl");
        File::create(&path)
            .expect("create sparse session")
            .set_len(MAX_SESSION_BYTES + 1)
            .expect("size sparse session");

        let mut browser_files = Vec::new();
        collect_jsonl_files(&root, SessionLayout::Flat, &mut browser_files, true);
        assert_eq!(browser_files, vec![path.clone()]);

        let mut usage_files = Vec::new();
        collect_jsonl_files(&root, SessionLayout::Flat, &mut usage_files, false);
        assert_eq!(usage_files, vec![path]);
    }

    #[test]
    fn deletion_rejects_files_outside_the_active_directory_layout() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        let flat = root.join("flat.jsonl");
        let project = root.join("project").join("project.jsonl");
        write_session_header(&flat, "flat-session");
        write_session_header(&project, "project-session");

        assert!(delete_session_with_layout(
            &root,
            &project,
            "project-session",
            SessionLayout::Flat
        )
        .is_err());
        assert!(project.exists());

        assert!(delete_session_with_layout(
            &root,
            &flat,
            "flat-session",
            SessionLayout::ProjectDirectories
        )
        .is_err());
        assert!(flat.exists());

        assert!(delete_session_with_layout(
            &root,
            &project,
            "project-session",
            SessionLayout::ProjectDirectories
        )
        .expect("delete project session"));
    }

    #[cfg(unix)]
    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_file(target, link)
    }

    #[cfg(not(any(unix, windows)))]
    fn create_file_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "unsupported platform",
        ))
    }

    #[cfg(unix)]
    fn create_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
    }

    #[cfg(not(any(unix, windows)))]
    fn create_dir_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "unsupported platform",
        ))
    }

    #[test]
    fn lineage_resolves_case_mismatched_symlinked_and_file_url_parents_without_external_probes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("SessionsRoot");
        let parent = root.join("ParentSession.jsonl");
        let child_case = root.join("child-case.jsonl");
        let child_url = root.join("child-url.jsonl");
        write_session_header(&parent, "parent-id");

        let canonical_root = root.canonicalize().expect("canonical root");
        let canonical_parent = parent.canonicalize().expect("canonical parent");

        let alt_parent_str = {
            #[cfg(windows)]
            {
                let raw = parent.to_string_lossy().replace('\\', "/");
                let mut chars = raw.chars();
                match chars.next() {
                    Some(first) if first.is_ascii_uppercase() => {
                        format!(
                            "{}{}",
                            first.to_ascii_lowercase(),
                            chars.as_str().to_uppercase()
                        )
                    }
                    Some(first) => {
                        format!(
                            "{}{}",
                            first.to_ascii_uppercase(),
                            chars.as_str().to_lowercase()
                        )
                    }
                    None => raw,
                }
            }
            #[cfg(not(windows))]
            {
                parent.to_string_lossy().into_owned()
            }
        };

        fs::write(
            &child_case,
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": "child-case-id",
                    "cwd": "/work",
                    "parentSession": alt_parent_str,
                })
            ),
        )
        .expect("write child_case");

        let parent_uri = tauri::Url::from_file_path(&parent).unwrap().to_string();
        fs::write(
            &child_url,
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": "child-url-id",
                    "cwd": "/work",
                    "parentSession": parent_uri,
                })
            ),
        )
        .expect("write child_url");

        let alias_dir = temp.path().join("alias-link");
        if create_dir_symlink(&root, &alias_dir).is_ok() {
            let via_symlink = alias_dir.join("ParentSession.jsonl");
            assert_eq!(
                lineage_path(&via_symlink.to_string_lossy(), &canonical_root),
                Some(canonical_parent.clone())
            );
        }

        let sessions = scan_sessions_in_root(&root, SessionLayout::Flat);
        for child_id in ["child-case-id", "child-url-id"] {
            let found = sessions
                .iter()
                .find(|s| s.session_id == child_id)
                .expect("child discovered");
            assert_eq!(found.parent_session_id.as_deref(), Some("parent-id"));
        }

        let descriptors = session_descriptors(&root, SessionLayout::Flat).expect("descriptors");
        let requests = vec![
            DeleteSessionRequest {
                provider_id: PROVIDER_ID.into(),
                session_id: "parent-id".into(),
                source_path: parent.to_string_lossy().into_owned(),
                include_project: false,
            },
            DeleteSessionRequest {
                provider_id: PROVIDER_ID.into(),
                session_id: "child-case-id".into(),
                source_path: child_case.to_string_lossy().into_owned(),
                include_project: false,
            },
        ];
        assert_eq!(
            deletion_order_with_descriptors(&requests, &[0, 1], &descriptors),
            vec![1, 0]
        );

        // Reject relative, parent-traversal, non-jsonl, and external paths
        let external = temp.path().join("outside.jsonl");
        write_session_header(&external, "outside-id");
        assert_eq!(
            lineage_path(&external.to_string_lossy(), &canonical_root),
            None
        );
        assert_eq!(lineage_path("relative/parent.jsonl", &canonical_root), None);
        assert_eq!(
            lineage_path(
                &root
                    .join("..")
                    .join("SessionsRoot")
                    .join("ParentSession.jsonl")
                    .to_string_lossy(),
                &canonical_root
            ),
            None
        );
        assert_eq!(
            lineage_path(&root.join("notes.txt").to_string_lossy(), &canonical_root),
            None
        );

        #[cfg(windows)]
        {
            for hostile in [
                r"\\198.51.100.1\share\evil.jsonl",
                r"\\?\UNC\198.51.100.1\share\evil.jsonl",
                "file://198.51.100.1/share/evil.jsonl",
                r"\\.\PIPE\evil.jsonl",
                r"\\?\GLOBALROOT\Device\HarddiskVolume1\evil.jsonl",
            ] {
                assert_eq!(lineage_path(hostile, &canonical_root), None);
            }
            let unc_root = Path::new(r"\\?\UNC\nas.local\team_share\pi\sessions");
            assert!(windows_lineage_prefix_allowed(
                Path::new(r"\\NAS.LOCAL\TEAM_SHARE\pi\sessions\a.jsonl"),
                unc_root
            ));
            assert!(!windows_lineage_prefix_allowed(
                Path::new(r"\\other.local\team_share\pi\sessions\a.jsonl"),
                unc_root
            ));
        }
    }

    #[test]
    fn session_descriptors_skips_malformed_and_artifact_subdirs_and_rejects_symlinks() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        let project = root.join("proj");
        let parent = project.join("parent.jsonl");
        let child = project.join("parent").join("forks").join("child.jsonl");
        write_session_header(&parent, "parent-id");
        write_fork_header(&child, "child-id", &parent);

        // Malformed sibling .jsonl and non-session subagent-artifacts directory must not abort descriptors
        fs::write(project.join("corrupt.jsonl"), "{broken json\n").unwrap();
        let artifacts = project.join("subagent-artifacts");
        fs::create_dir_all(&artifacts).unwrap();
        fs::write(artifacts.join("trace.jsonl"), "not-a-pi-session\n").unwrap();

        let descriptors =
            session_descriptors(&root, SessionLayout::ProjectDirectories).expect("descriptors");
        assert_eq!(descriptors.len(), 2);

        let link_session = project.join("evil-link.jsonl");
        if create_file_symlink(&parent, &link_session).is_ok() {
            assert!(validate_source_under_root(
                &root,
                &link_session,
                SessionLayout::ProjectDirectories
            )
            .is_err());
            assert!(delete_session_with_layout(
                &root,
                &link_session,
                "parent-id",
                SessionLayout::ProjectDirectories
            )
            .is_err());
            assert!(parent.exists(), "real target session must remain untouched");
        }
    }

    #[test]
    fn thinking_blocks_and_assistant_error_turns_are_preserved_in_pi_messages() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("rich.jsonl");
        let lines = [
            serde_json::json!({"type":"session","version":3,"id":"rich-session","cwd":"/work"}),
            serde_json::json!({
                "type":"message","id":"m1","parentId":null,"timestamp":1700000000000i64,
                "message":{"role":"user","content":[{"type":"image"}]}
            }),
            serde_json::json!({
                "type":"message","id":"m2","parentId":"m1","timestamp":1700000001000i64,
                "message":{"role":"assistant","content":[{"type":"thinking","thinking":"only thinking"}]}
            }),
            serde_json::json!({
                "type":"message","id":"m3","parentId":"m2","timestamp":1700000002000i64,
                "message":{
                    "role":"assistant",
                    "content":[
                        {"type":"thinking","thinking":"plan step"},
                        {"type":"text","text":"visible reply"}
                    ]
                }
            }),
            serde_json::json!({
                "type":"message","id":"m4","parentId":"m3","timestamp":1700000003000i64,
                "message":{
                    "role":"assistant",
                    "content":[],
                    "stopReason":"error",
                    "errorMessage":"429 rate limit exceeded"
                }
            }),
        ];
        fs::write(
            &path,
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let messages = load_messages_with_layout(&root, &path, SessionLayout::Flat).unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|m| (m.role.as_str(), m.content.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("user", "[Image]"),
                ("thinking", "only thinking"),
                ("thinking", "plan step"),
                ("assistant", "visible reply"),
                ("assistant", "[Error] 429 rate limit exceeded"),
            ]
        );

        let meta = parse_session(&path).unwrap();
        assert_eq!(
            meta.summary.as_deref(),
            Some("[Error] 429 rate limit exceeded")
        );
        assert_eq!(meta.last_active_at, Some(1_700_000_003_000));
    }

    #[test]
    fn active_branch_summary_excludes_abandoned_tree_branches_and_tracks_activity_timestamps() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("branched.jsonl");
        let lines = [
            serde_json::json!({"type":"session","version":3,"id":"branched-session","cwd":"/work","timestamp":1700000000000i64}),
            // Abandoned root branch
            serde_json::json!({"type":"message","id":"dead-root","parentId":null,"timestamp":1700000001000i64,"message":{"role":"user","content":"dead first question"}}),
            serde_json::json!({"type":"session_info","id":"dead-info","parentId":"dead-root","timestamp":1700000099000i64,"name":"Dead Branch Title"}),
            serde_json::json!({"type":"message","id":"dead-reply","parentId":"dead-info","timestamp":1700000099500i64,"message":{"role":"assistant","content":"dead summary"}}),
            // Active root branch (re-rooted via /tree)
            serde_json::json!({"type":"message","id":"live-root","parentId":null,"timestamp":1700000002000i64,"message":{"role":"user","content":"active first question"}}),
            // Clear name on active branch so Dead Branch Title does not leak, and record compaction + bashExecution timestamps
            serde_json::json!({"type":"session_info","id":"live-clear","parentId":"live-root","timestamp":1700000003000i64,"name":""}),
            serde_json::json!({"type":"message","id":"live-bash","parentId":"live-clear","timestamp":1700000004000i64,"message":{"role":"bashExecution","command":"cargo test","output":"ok","timestamp":1700000005000i64}}),
            serde_json::json!({"type":"compaction","id":"live-compact","parentId":"live-bash","timestamp":1700000006000i64,"summary":"compacted context"}),
        ];
        fs::write(
            &path,
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let meta = parse_session(&path).unwrap();
        assert_eq!(meta.title.as_deref(), Some("active first question"));
        assert_eq!(meta.summary.as_deref(), Some("active first question"));
        assert_eq!(meta.last_active_at, Some(1_700_000_006_000));
    }
}
