//! Minimal tray integration for the standalone Agent session manager.

use tauri::menu::{Menu, MenuBuilder, MenuItem};
use tauri::Manager;

pub const TRAY_ID: &str = "agent-session-manager";

#[derive(Clone, Copy)]
struct TrayTexts {
    show_main: &'static str,
    quit: &'static str,
}

fn texts() -> TrayTexts {
    let locale = sys_locale::get_locale().unwrap_or_default().to_lowercase();
    if locale.starts_with("en") {
        TrayTexts {
            show_main: "Open",
            quit: "Quit",
        }
    } else if locale.starts_with("ja") {
        TrayTexts {
            show_main: "開く",
            quit: "終了",
        }
    } else if locale.starts_with("zh-tw") || locale.starts_with("zh-hk") {
        TrayTexts {
            show_main: "開啟",
            quit: "退出",
        }
    } else {
        TrayTexts {
            show_main: "打开",
            quit: "退出",
        }
    }
}

pub fn create_tray_menu(app: &tauri::AppHandle) -> Result<Menu<tauri::Wry>, String> {
    let texts = texts();
    let show_main = MenuItem::with_id(app, "show_main", texts.show_main, true, None::<&str>)
        .map_err(|error| format!("创建托盘菜单失败：{error}"))?;
    let quit = MenuItem::with_id(app, "quit", texts.quit, true, None::<&str>)
        .map_err(|error| format!("创建托盘菜单失败：{error}"))?;
    MenuBuilder::new(app)
        .item(&show_main)
        .item(&quit)
        .build()
        .map_err(|error| format!("创建托盘菜单失败：{error}"))
}

pub fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        #[cfg(target_os = "windows")]
        let _ = window.set_skip_taskbar(false);
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

pub fn handle_tray_menu_event(app: &tauri::AppHandle, event_id: &str) {
    match event_id {
        "show_main" => show_main_window(app),
        "quit" => app.exit(0),
        _ => {}
    }
}
