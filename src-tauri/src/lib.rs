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
            session_manager::wire::wire_messages(&providerId, &sourcePath, &transcript.messages)
        })
    })
    .await
    .map_err(|error| format!("读取会话失败：{error}"))?
}

/// 取回一条被截断消息的全文。
///
/// `expectedChars` 是列表里那一版正文的长度：两次请求之间会话若已被改写，同一下标
/// 可能指向另一条消息，因此长度对不上就拒绝返回，让前端重新打开，而不是张冠李戴。
#[tauri::command]
async fn get_message_content(
    #[allow(non_snake_case)] providerId: String,
    #[allow(non_snake_case)] sourcePath: String,
    index: usize,
    #[allow(non_snake_case)] expectedChars: usize,
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
        Ok(message.content.clone())
    })
    .await
    .map_err(|error| format!("读取消息失败：{error}"))?
}

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
    tauri::async_runtime::spawn_blocking(move || {
        use session_manager::cache::chunk_ranges;
        use session_manager::wire::{wire_range, TranscriptChunk};

        let transcript = match session_manager::load_transcript(&providerId, &sourcePath) {
            Ok(transcript) => transcript,
            Err(message) => {
                let _ = onChunk.send(TranscriptChunk::Error {
                    message: message.clone(),
                });
                return Err(message);
            }
        };

        let send = |chunk: TranscriptChunk| {
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
                messages: wire_range(&providerId, &sourcePath, &transcript.messages, start, end),
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
fn get_pi_session_discovery() -> session_manager::providers::pi::PiSessionDiscovery {
    session_manager::providers::pi::session_discovery()
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
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _, _| {
            tray::show_main_window(app);
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
            delete_session,
            delete_sessions,
            get_pi_session_discovery,
            set_window_theme,
        ])
        .run(tauri::generate_context!())
        .expect("Agent会话管理启动失败");
}
