mod session_manager;
mod session_paths;
mod tray;

use tauri::tray::TrayIconBuilder;
use tauri::Theme;

#[tauri::command]
async fn list_sessions() -> Result<session_manager::SessionScanReport, String> {
    tauri::async_runtime::spawn_blocking(session_manager::checked_list_sessions)
        .await
        .map_err(|error| format!("扫描会话失败：{error}"))?
}

#[tauri::command]
async fn get_session_messages(
    #[allow(non_snake_case)] providerId: String,
    #[allow(non_snake_case)] sourcePath: String,
) -> Result<Vec<session_manager::wire::SessionMessageWire>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        session_manager::load_transcript(&providerId, &sourcePath).map(|transcript| {
            session_manager::wire::wire_messages(
                transcript.cacheable,
                &providerId,
                &transcript.messages,
            )
        })
    })
    .await
    .map_err(|error| format!("读取会话失败：{error}"))?
}

/// 取回一条被截断消息的全文。
///
/// `expectedChars` 与 `expectedIdentity` 描述列表里那一版正文：两次请求之间会话若
/// 已被改写，同一下标可能指向另一条消息，校验不过就拒绝返回，让前端重新打开，而不是
/// 张冠李戴。只比长度不足以确认身份——改写后的消息可能长度恰好相同，所以身份摘要
/// 才是真正的判据；长度继续保留，用于兼容未携带摘要的旧调用。
#[tauri::command]
async fn get_message_content(
    #[allow(non_snake_case)] providerId: String,
    #[allow(non_snake_case)] sourcePath: String,
    index: usize,
    #[allow(non_snake_case)] expectedChars: usize,
    #[allow(non_snake_case)] expectedIdentity: Option<String>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let transcript = session_manager::load_transcript(&providerId, &sourcePath)?;
        let message = transcript
            .messages
            .get(index)
            .ok_or("会话已更新，请重新打开")?;
        if message.content.chars().count() != expectedChars {
            return Err("会话已更新，请重新打开".into());
        }
        if let Some(expected) = expectedIdentity {
            let actual = session_manager::wire::content_identity_hex(
                &message.role,
                message.ts,
                &message.content,
            );
            if actual != expected {
                return Err("会话已更新，请重新打开".into());
            }
        }
        Ok(message.content.clone())
    })
    .await
    .map_err(|error| format!("读取消息失败：{error}"))?
}

static STREAM_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 分块流式读取会话：依次下发 `Header` → 若干 `Messages` → `Done`。
///
/// 大会话不再一次性序列化成单个 IPC 负载，前端可以边到边渲染。读取失败时先发一个
/// `Error` 包，再以同样的文案返回 `Err`，前端任选一处处理。
#[tauri::command]
async fn stream_session_messages(
    #[allow(non_snake_case)] providerId: String,
    #[allow(non_snake_case)] sourcePath: String,
    #[allow(non_snake_case)] onChunk: tauri::ipc::Channel<session_manager::wire::TranscriptChunk>,
) -> Result<(), String> {
    use std::sync::atomic::Ordering;

    let generation = STREAM_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    tauri::async_runtime::spawn_blocking(move || {
        use session_manager::cache::chunk_ranges;
        use session_manager::wire::{wire_range, TranscriptChunk};

        let is_superseded = || STREAM_GENERATION.load(Ordering::SeqCst) != generation;
        if is_superseded() {
            return Err("会话读取已取消".to_string());
        }

        let transcript = match session_manager::load_transcript(&providerId, &sourcePath) {
            Ok(transcript) => transcript,
            Err(message) => {
                if is_superseded() {
                    return Err("会话读取已取消".to_string());
                }
                let _ = onChunk.send(TranscriptChunk::Error {
                    message: message.clone(),
                });
                return Err(message);
            }
        };

        let send = |chunk: TranscriptChunk| {
            if is_superseded() {
                return Err("会话读取已取消".to_string());
            }
            onChunk
                .send(chunk)
                .map_err(|error| format!("下发会话分块失败：{error}"))
        };
        send(TranscriptChunk::Header {
            total: transcript.messages.len(),
            approx_bytes: transcript.approx_bytes as u64,
        })?;
        for (start, end) in chunk_ranges(&transcript.message_bytes) {
            send(TranscriptChunk::Messages {
                start,
                messages: wire_range(
                    transcript.cacheable,
                    &providerId,
                    &transcript.messages,
                    start,
                    end,
                ),
            })?;
        }
        send(TranscriptChunk::Done {
            delivered: transcript.messages.len(),
        })
    })
    .await
    .map_err(|error| format!("读取会话失败：{error}"))?
}

#[tauri::command]
async fn search_session_messages(
    #[allow(non_snake_case)] providerId: String,
    #[allow(non_snake_case)] sourcePath: String,
    query: String,
) -> Result<Vec<usize>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let transcript = session_manager::load_transcript(&providerId, &sourcePath)?;
        Ok(session_manager::search_messages(
            &transcript.messages,
            &query,
        ))
    })
    .await
    .map_err(|error| format!("搜索会话失败：{error}"))?
}

#[tauri::command]
async fn delete_session(
    #[allow(non_snake_case)] providerId: String,
    #[allow(non_snake_case)] sessionId: String,
    #[allow(non_snake_case)] sourcePath: String,
    #[allow(non_snake_case)] includeProject: Option<bool>,
) -> Result<session_manager::DeleteSessionReply, String> {
    tauri::async_runtime::spawn_blocking(move || {
        session_manager::delete_session_checked(
            &providerId,
            &sessionId,
            &sourcePath,
            includeProject.unwrap_or(false),
        )
    })
    .await
    .map_err(|error| format!("删除会话失败：{error}"))?
}

#[tauri::command]
async fn delete_sessions(
    items: Vec<session_manager::DeleteSessionRequest>,
) -> Result<Vec<session_manager::DeleteSessionOutcome>, String> {
    tauri::async_runtime::spawn_blocking(move || session_manager::delete_sessions(&items))
        .await
        .map_err(|error| format!("批量删除会话失败：{error}"))
}

#[tauri::command]
async fn get_pi_session_discovery(
) -> Result<session_manager::providers::pi::PiSessionDiscovery, String> {
    tauri::async_runtime::spawn_blocking(session_manager::providers::pi::session_discovery)
        .await
        .map_err(|error| format!("读取 Pi 会话配置失败：{error}"))
}

#[tauri::command]
async fn sync_session_index(
    app: tauri::AppHandle,
) -> Result<session_manager::search_index::IndexStatus, String> {
    use tauri::Emitter;
    tauri::async_runtime::spawn_blocking(move || {
        session_manager::search_index::sync_index(|status| {
            let _ = app.emit("session-index-status", status);
        })
    })
    .await
    .map_err(|error| format!("同步正文索引失败：{error}"))?
}

#[tauri::command]
async fn search_session_content(
    query: String,
    #[allow(non_snake_case)] providerId: Option<String>,
    limit: Option<usize>,
    #[allow(non_snake_case)] searchId: Option<String>,
) -> Result<session_manager::search_index::ContentSearchResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        session_manager::search_index::search_content(
            &query,
            providerId.as_deref(),
            limit,
            searchId.as_deref(),
        )
    })
    .await
    .map_err(|error| format!("正文搜索失败：{error}"))?
}

#[tauri::command]
fn cancel_session_content_search(#[allow(non_snake_case)] searchId: String) {
    session_manager::search_index::cancel_search(&searchId);
}

#[tauri::command]
async fn clear_session_index() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(session_manager::search_index::clear_index)
        .await
        .map_err(|error| format!("清理正文索引失败：{error}"))?
}

fn resolve_reveal_target(raw: &str) -> Result<std::path::PathBuf, String> {
    use std::path::{Component, Path, PathBuf};

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("路径不能为空".into());
    }
    let candidate = Path::new(trimmed);
    if !candidate.is_absolute() {
        return Err(format!("仅支持打开本地绝对路径：{trimmed}"));
    }
    if candidate
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(format!("路径不能包含上级目录组件（..）：{trimmed}"));
    }
    #[cfg(windows)]
    {
        use std::path::Prefix;
        if let Some(Component::Prefix(prefix)) = candidate.components().next() {
            if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) {
                return Err(format!("不支持定位网络共享或设备路径：{trimmed}"));
            }
        }
    }

    let existing = if candidate.exists() {
        candidate.to_path_buf()
    } else if let Some(parent) = candidate.parent().filter(|p| p.exists()) {
        parent.to_path_buf()
    } else {
        return Err(format!("目标路径不存在：{trimmed}"));
    };

    let canonical = existing
        .canonicalize()
        .map_err(|e| format!("无法解析路径 {}：{e}", existing.display()))?;
    #[cfg(windows)]
    {
        let text = canonical.to_string_lossy();
        if let Some(stripped) = text.strip_prefix("\\\\?\\") {
            if !stripped.starts_with("UNC\\") {
                return Ok(PathBuf::from(stripped));
            }
        }
    }
    Ok(canonical)
}

#[tauri::command]
async fn reveal_session_path(path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let target = resolve_reveal_target(&path)?;
        tauri_plugin_opener::reveal_item_in_dir(&target)
            .map_err(|error| format!("无法在文件管理器中定位 {}：{error}", target.display()))
    })
    .await
    .map_err(|error| format!("定位路径失败：{error}"))?
}

#[tauri::command]
fn set_window_theme(window: tauri::Window, theme: String) -> Result<(), String> {
    let theme = match theme.as_str() {
        "light" => Some(Theme::Light),
        "dark" => Some(Theme::Dark),
        "system" => None,
        _ => return Err(format!("不支持的主题：{theme}")),
    };
    window
        .set_theme(theme)
        .map_err(|error| format!("切换窗口主题失败：{error}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut builder = tauri::Builder::default();

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, args, _| {
            if args.iter().any(|arg| arg == "--quit") {
                app.exit(0);
            } else {
                tray::show_main_window(app);
            }
        }));
    }

    builder
        .plugin(tauri_plugin_opener::init())
        .plugin(
            tauri_plugin_window_state::Builder::default()
                // 自绘标题栏（decorations: false）不允许被状态恢复覆盖：
                // ALL 包含 DECORATIONS，会把旧存档的"有边框"状态还原回来
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::all()
                        .difference(tauri_plugin_window_state::StateFlags::DECORATIONS),
                )
                .build(),
        )
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                #[cfg(target_os = "windows")]
                let _ = window.set_skip_taskbar(true);
            }
        })
        .setup(|app| {
            if std::env::args().any(|arg| arg == "--quit") {
                app.handle().exit(0);
                return Ok(());
            }
            let menu = tray::create_tray_menu(app.handle())?;
            let mut tray_builder = TrayIconBuilder::with_id(tray::TRAY_ID)
                .tooltip("Agent会话管理")
                .menu(&menu)
                .on_menu_event(|app, event| {
                    tray::handle_tray_menu_event(app, &event.id.0);
                })
                .show_menu_on_left_click(true);
            if let Some(icon) = app.default_window_icon() {
                tray_builder = tray_builder.icon(icon.clone());
            }
            tray_builder.build(app)?;
            tray::show_main_window(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_sessions,
            get_session_messages,
            get_message_content,
            stream_session_messages,
            search_session_messages,
            delete_session,
            delete_sessions,
            get_pi_session_discovery,
            sync_session_index,
            search_session_content,
            cancel_session_content_search,
            clear_session_index,
            reveal_session_path,
            set_window_theme,
        ])
        .run(tauri::generate_context!())
        .expect("Agent会话管理启动失败");
}
