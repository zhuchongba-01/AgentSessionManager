//! 跨平台原生「另存为」导出会话 Markdown 文件（独立 STA 线程 + 现代系统保存面板，零新增依赖）。

use std::path::{Path, PathBuf};

/// 清洗默认导出文件名，剥离各系统非法字符与路径分隔符并确保 `.md` 后缀。
pub fn sanitize_export_filename(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_ext = trimmed
        .strip_suffix(".md")
        .or_else(|| trimmed.strip_suffix(".MD"))
        .or_else(|| trimmed.strip_suffix(".Md"))
        .or_else(|| trimmed.strip_suffix(".mD"))
        .unwrap_or(trimmed);

    let mut cleaned = String::with_capacity(without_ext.len());
    let mut prev_space = false;
    for ch in without_ext.chars() {
        let forbidden =
            matches!(ch, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || ch.is_control();
        if forbidden || ch.is_whitespace() {
            if !prev_space && !cleaned.is_empty() {
                cleaned.push(' ');
                prev_space = true;
            }
        } else {
            cleaned.push(ch);
            prev_space = false;
        }
    }

    let stem: String = cleaned
        .trim_matches(|c: char| c.is_whitespace() || c == '.')
        .chars()
        .take(80)
        .collect::<String>()
        .trim_end_matches(|c: char| c.is_whitespace() || c == '.')
        .to_string();

    let safe_stem = if stem.is_empty() || is_windows_reserved_stem(&stem) {
        "session".to_string()
    } else {
        stem
    };

    format!("{safe_stem}.md")
}

fn is_windows_reserved_stem(stem: &str) -> bool {
    let upper = stem.trim().to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn resolve_initial_dir(default_dir: Option<&str>) -> Option<PathBuf> {
    let raw = default_dir?.trim();
    if raw.is_empty() {
        return None;
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return None;
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    path.is_dir().then(|| path.to_path_buf())
}

fn ensure_md_extension(path: PathBuf) -> PathBuf {
    if path.extension().is_some() {
        path
    } else {
        let mut os = path.into_os_string();
        os.push(".md");
        PathBuf::from(os)
    }
}

pub fn write_markdown_to_path(path: &Path, content: &str) -> Result<String, String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(format!("目标目录不存在：{}", parent.display()));
        }
    }
    std::fs::write(path, content.as_bytes())
        .map_err(|error| format!("写入导出文件 {} 失败：{error}", path.display()))?;
    Ok(path.to_string_lossy().into_owned())
}

/// 弹出系统原生「另存为」对话框并将 Markdown 内容写入用户所选位置。
/// 用户取消选择时返回 `Ok(None)`。
pub fn save_markdown_with_dialog(
    owner_hwnd: usize,
    default_filename: &str,
    default_dir: Option<&str>,
    content: &str,
) -> Result<Option<String>, String> {
    let safe_filename = sanitize_export_filename(default_filename);
    let initial_dir = resolve_initial_dir(default_dir);

    let chosen = pick_save_markdown_path(owner_hwnd, &safe_filename, initial_dir.as_deref())?;
    let Some(target) = chosen else {
        return Ok(None);
    };
    let final_path = ensure_md_extension(target);
    let saved = write_markdown_to_path(&final_path, content)?;
    Ok(Some(saved))
}

#[cfg(windows)]
mod win_save_dialog {
    use std::ffi::{c_void, OsString};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    #[repr(C)]
    struct ComDlgFilterSpec {
        psz_name: *const u16,
        psz_spec: *const u16,
    }

    #[repr(C)]
    struct IShellItem {
        lp_vtbl: *const IShellItemVtbl,
    }

    #[repr(C)]
    struct IShellItemVtbl {
        query_interface:
            unsafe extern "system" fn(*mut IShellItem, *const Guid, *mut *mut c_void) -> i32,
        add_ref: unsafe extern "system" fn(*mut IShellItem) -> u32,
        release: unsafe extern "system" fn(*mut IShellItem) -> u32,
        bind_to_handler: unsafe extern "system" fn(
            *mut IShellItem,
            *mut c_void,
            *const Guid,
            *const Guid,
            *mut *mut c_void,
        ) -> i32,
        get_parent: unsafe extern "system" fn(*mut IShellItem, *mut *mut IShellItem) -> i32,
        get_display_name: unsafe extern "system" fn(*mut IShellItem, u32, *mut *mut u16) -> i32,
        get_attributes: unsafe extern "system" fn(*mut IShellItem, u32, *mut u32) -> i32,
        compare: unsafe extern "system" fn(*mut IShellItem, *mut IShellItem, u32, *mut i32) -> i32,
    }

    #[repr(C)]
    pub(super) struct IFileSaveDialog {
        lp_vtbl: *const IFileSaveDialogVtbl,
    }

    #[repr(C)]
    struct IFileSaveDialogVtbl {
        // IUnknown (0..=2)
        query_interface:
            unsafe extern "system" fn(*mut IFileSaveDialog, *const Guid, *mut *mut c_void) -> i32,
        add_ref: unsafe extern "system" fn(*mut IFileSaveDialog) -> u32,
        release: unsafe extern "system" fn(*mut IFileSaveDialog) -> u32,
        // IModalWindow (3)
        show: unsafe extern "system" fn(*mut IFileSaveDialog, usize) -> i32,
        // IFileDialog (4..=26)
        set_file_types:
            unsafe extern "system" fn(*mut IFileSaveDialog, u32, *const ComDlgFilterSpec) -> i32,
        set_file_type_index: unsafe extern "system" fn(*mut IFileSaveDialog, u32) -> i32,
        get_file_type_index: unsafe extern "system" fn(*mut IFileSaveDialog, *mut u32) -> i32,
        advise: unsafe extern "system" fn(*mut IFileSaveDialog, *mut c_void, *mut u32) -> i32,
        unadvise: unsafe extern "system" fn(*mut IFileSaveDialog, u32) -> i32,
        set_options: unsafe extern "system" fn(*mut IFileSaveDialog, u32) -> i32,
        get_options: unsafe extern "system" fn(*mut IFileSaveDialog, *mut u32) -> i32,
        set_default_folder: unsafe extern "system" fn(*mut IFileSaveDialog, *mut IShellItem) -> i32,
        set_folder: unsafe extern "system" fn(*mut IFileSaveDialog, *mut IShellItem) -> i32,
        get_folder: unsafe extern "system" fn(*mut IFileSaveDialog, *mut *mut IShellItem) -> i32,
        get_current_selection:
            unsafe extern "system" fn(*mut IFileSaveDialog, *mut *mut IShellItem) -> i32,
        set_file_name: unsafe extern "system" fn(*mut IFileSaveDialog, *const u16) -> i32,
        get_file_name: unsafe extern "system" fn(*mut IFileSaveDialog, *mut *mut u16) -> i32,
        set_title: unsafe extern "system" fn(*mut IFileSaveDialog, *const u16) -> i32,
        set_ok_button_label: unsafe extern "system" fn(*mut IFileSaveDialog, *const u16) -> i32,
        set_file_name_label: unsafe extern "system" fn(*mut IFileSaveDialog, *const u16) -> i32,
        get_result: unsafe extern "system" fn(*mut IFileSaveDialog, *mut *mut IShellItem) -> i32,
        add_place: unsafe extern "system" fn(*mut IFileSaveDialog, *mut IShellItem, u32) -> i32,
        set_default_extension: unsafe extern "system" fn(*mut IFileSaveDialog, *const u16) -> i32,
        close: unsafe extern "system" fn(*mut IFileSaveDialog, i32) -> i32,
        set_client_guid: unsafe extern "system" fn(*mut IFileSaveDialog, *const Guid) -> i32,
        clear_client_data: unsafe extern "system" fn(*mut IFileSaveDialog) -> i32,
        set_filter: unsafe extern "system" fn(*mut IFileSaveDialog, *mut c_void) -> i32,
    }

    #[link(name = "ole32")]
    extern "system" {
        pub(super) fn CoInitializeEx(pv_reserved: *const c_void, dw_co_init: u32) -> i32;
        pub(super) fn CoUninitialize();
        fn CoCreateInstance(
            rclsid: *const Guid,
            p_unk_outer: *mut c_void,
            dw_cls_context: u32,
            riid: *const Guid,
            ppv: *mut *mut c_void,
        ) -> i32;
        fn CoTaskMemFree(pv: *const c_void);
    }

    #[link(name = "shell32")]
    extern "system" {
        fn SHCreateItemFromParsingName(
            psz_path: *const u16,
            pbc: *mut c_void,
            riid: *const Guid,
            ppv: *mut *mut c_void,
        ) -> i32;
    }

    const COINIT_APARTMENTTHREADED: u32 = 0x2;
    const COINIT_DISABLE_OLE1DDE: u32 = 0x4;
    const CLSCTX_INPROC_SERVER: u32 = 0x1;

    const FOS_OVERWRITEPROMPT: u32 = 0x0000_0002;
    const FOS_STRICTFILETYPES: u32 = 0x0000_0004;
    const FOS_NOCHANGEDIR: u32 = 0x0000_0008;
    const FOS_FORCEFILESYSTEM: u32 = 0x0000_0040;
    const FOS_PATHMUSTEXIST: u32 = 0x0000_0800;

    const SIGDN_FILESYSPATH: u32 = 0x8005_8000;
    const HR_ERROR_CANCELLED: i32 = 0x8007_04C7u32 as i32;
    const HR_ERROR_INVALID_WINDOW_HANDLE: i32 = 0x8007_0578u32 as i32;

    const CLSID_FILE_SAVE_DIALOG: Guid = Guid {
        data1: 0xC0B4_E2F3,
        data2: 0xBA21,
        data3: 0x4773,
        data4: [0x8D, 0xBA, 0x33, 0x5E, 0xC9, 0x46, 0xEB, 0x8B],
    };

    const IID_IFILE_SAVE_DIALOG: Guid = Guid {
        data1: 0x84BC_CD23,
        data2: 0x5FDE,
        data3: 0x4CDB,
        data4: [0xAE, 0xA4, 0xAF, 0x64, 0xB8, 0x3D, 0x78, 0xAB],
    };

    const IID_ISHELL_ITEM: Guid = Guid {
        data1: 0x4382_6D1E,
        data2: 0xE718,
        data3: 0x42EE,
        data4: [0xBC, 0x55, 0xA1, 0xE2, 0x61, 0xC3, 0x7B, 0xFE],
    };

    struct ComStaGuard;

    impl ComStaGuard {
        fn init() -> Result<Self, String> {
            // SAFETY: 在专用的新建 OS 线程上初始化单线程套间（STA）。
            let hr = unsafe {
                CoInitializeEx(
                    std::ptr::null(),
                    COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE,
                )
            };
            if hr < 0 {
                return Err(format!(
                    "初始化 Windows STA 组件环境失败 (HRESULT 0x{:08x})",
                    hr as u32
                ));
            }
            Ok(Self)
        }
    }

    impl Drop for ComStaGuard {
        fn drop(&mut self) {
            // SAFETY: 与 `CoInitializeEx` 成功调用一一配对。
            unsafe { CoUninitialize() };
        }
    }

    pub(super) struct SaveDialogPtr(*mut IFileSaveDialog);

    impl Drop for SaveDialogPtr {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: 非空 COM 接口指针在作用域结束时释放引用计数。
                unsafe { ((*(*self.0).lp_vtbl).release)(self.0) };
            }
        }
    }

    struct ShellItemPtr(*mut IShellItem);

    impl Drop for ShellItemPtr {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: 非空 `IShellItem` 在作用域结束时释放引用计数。
                unsafe { ((*(*self.0).lp_vtbl).release)(self.0) };
            }
        }
    }

    fn to_wide_z(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn check_hr(hr: i32, step: &str) -> Result<(), String> {
        if hr >= 0 {
            Ok(())
        } else {
            Err(format!("{step}失败 (HRESULT 0x{:08x})", hr as u32))
        }
    }

    /// 在当前已初始化 STA 的线程上创建并配置 `IFileSaveDialog`（供运行时与单测共用校验）。
    pub(super) fn create_configured_save_dialog(
        safe_filename: &str,
        initial_dir: Option<&Path>,
    ) -> Result<SaveDialogPtr, String> {
        let mut raw_dialog: *mut c_void = std::ptr::null_mut();
        // SAFETY: 在 STA 线程中按标准 CLSID / IID 创建系统 `FileSaveDialog` 实例。
        let hr = unsafe {
            CoCreateInstance(
                &CLSID_FILE_SAVE_DIALOG,
                std::ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_IFILE_SAVE_DIALOG,
                &mut raw_dialog,
            )
        };
        check_hr(hr, "创建系统另存为对话框")?;
        if raw_dialog.is_null() {
            return Err("创建系统另存为对话框返回空指针".into());
        }
        let dialog = SaveDialogPtr(raw_dialog.cast::<IFileSaveDialog>());
        let vtbl = unsafe { &*(*dialog.0).lp_vtbl };

        let mut opts: u32 = 0;
        check_hr(
            unsafe { (vtbl.get_options)(dialog.0, &mut opts) },
            "读取对话框选项",
        )?;
        opts |= FOS_OVERWRITEPROMPT
            | FOS_FORCEFILESYSTEM
            | FOS_PATHMUSTEXIST
            | FOS_NOCHANGEDIR
            | FOS_STRICTFILETYPES;
        check_hr(
            unsafe { (vtbl.set_options)(dialog.0, opts) },
            "设置对话框选项",
        )?;

        let md_label = to_wide_z("Markdown 文档 (*.md)");
        let md_spec = to_wide_z("*.md");
        let all_label = to_wide_z("所有文件 (*.*)");
        let all_spec = to_wide_z("*.*");
        let filters = [
            ComDlgFilterSpec {
                psz_name: md_label.as_ptr(),
                psz_spec: md_spec.as_ptr(),
            },
            ComDlgFilterSpec {
                psz_name: all_label.as_ptr(),
                psz_spec: all_spec.as_ptr(),
            },
        ];
        check_hr(
            unsafe { (vtbl.set_file_types)(dialog.0, filters.len() as u32, filters.as_ptr()) },
            "设置文件类型过滤",
        )?;
        check_hr(
            unsafe { (vtbl.set_file_type_index)(dialog.0, 1) },
            "设置默认文件类型",
        )?;

        let def_ext = to_wide_z("md");
        check_hr(
            unsafe { (vtbl.set_default_extension)(dialog.0, def_ext.as_ptr()) },
            "设置默认扩展名",
        )?;

        let title = to_wide_z("导出会话 Markdown");
        check_hr(
            unsafe { (vtbl.set_title)(dialog.0, title.as_ptr()) },
            "设置对话框标题",
        )?;

        let preset_name = to_wide_z(safe_filename);
        check_hr(
            unsafe { (vtbl.set_file_name)(dialog.0, preset_name.as_ptr()) },
            "设置默认文件名",
        )?;

        if let Some(dir) = initial_dir {
            let dir_wide: Vec<u16> = dir
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let mut raw_item: *mut c_void = std::ptr::null_mut();
            let item_hr = unsafe {
                SHCreateItemFromParsingName(
                    dir_wide.as_ptr(),
                    std::ptr::null_mut(),
                    &IID_ISHELL_ITEM,
                    &mut raw_item,
                )
            };
            if item_hr >= 0 && !raw_item.is_null() {
                let folder = ShellItemPtr(raw_item.cast::<IShellItem>());
                let _ = unsafe { (vtbl.set_default_folder)(dialog.0, folder.0) };
            }
        }

        Ok(dialog)
    }

    /// 在独立新建的 STA 线程中运行系统另存为对话框，彻底隔离 Tokio MTA 线程池。
    pub(super) fn run_sta_save_dialog(
        owner_hwnd: usize,
        safe_filename: &str,
        initial_dir: Option<&Path>,
    ) -> Result<Option<PathBuf>, String> {
        let filename_owned = safe_filename.to_string();
        let initial_dir_owned = initial_dir.map(Path::to_path_buf);

        let handle = std::thread::Builder::new()
            .name("asm-win-save-dialog".into())
            .spawn(move || -> Result<Option<PathBuf>, String> {
                let _com = ComStaGuard::init()?;
                let dialog =
                    create_configured_save_dialog(&filename_owned, initial_dir_owned.as_deref())?;
                let vtbl = unsafe { &*(*dialog.0).lp_vtbl };

                let mut show_hr = unsafe { (vtbl.show)(dialog.0, owner_hwnd) };
                if show_hr == HR_ERROR_INVALID_WINDOW_HANDLE && owner_hwnd != 0 {
                    show_hr = unsafe { (vtbl.show)(dialog.0, 0) };
                }
                if show_hr == HR_ERROR_CANCELLED {
                    return Ok(None);
                }
                check_hr(show_hr, "显示另存为窗口")?;

                let mut raw_result: *mut IShellItem = std::ptr::null_mut();
                check_hr(
                    unsafe { (vtbl.get_result)(dialog.0, &mut raw_result) },
                    "读取所选保存路径",
                )?;
                if raw_result.is_null() {
                    return Ok(None);
                }
                let result_item = ShellItemPtr(raw_result);
                let item_vtbl = unsafe { &*(*result_item.0).lp_vtbl };

                let mut psz_path: *mut u16 = std::ptr::null_mut();
                check_hr(
                    unsafe {
                        (item_vtbl.get_display_name)(
                            result_item.0,
                            SIGDN_FILESYSPATH,
                            &mut psz_path,
                        )
                    },
                    "解析所选磁盘路径",
                )?;
                if psz_path.is_null() {
                    return Ok(None);
                }

                // SAFETY: `GetDisplayName` 返回以 NUL 结尾的 `CoTaskMemAlloc` UTF-16 字符串。
                let chosen = unsafe {
                    let mut len = 0usize;
                    while *psz_path.add(len) != 0 {
                        len += 1;
                    }
                    let slice = std::slice::from_raw_parts(psz_path, len);
                    let path = PathBuf::from(OsString::from_wide(slice));
                    CoTaskMemFree(psz_path.cast::<c_void>());
                    path
                };
                Ok(Some(chosen))
            })
            .map_err(|error| format!("无法启动另存为对话框线程：{error}"))?;

        handle
            .join()
            .map_err(|_| "另存为对话框线程异常退出".to_string())?
    }

    #[cfg(test)]
    pub(super) fn verify_dialog_creation_from_mta_caller(filename: &str) -> Result<(), String> {
        const COINIT_MULTITHREADED: u32 = 0x0;

        // 模拟 Tokio / WebView2 后台工作线程已处于 MTA 模式
        let caller_hr = unsafe { CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED) };
        let should_uninit_caller = caller_hr == 0 || caller_hr == 1;

        let name_owned = filename.to_string();
        let res = std::thread::Builder::new()
            .name("asm-test-sta-save-dialog".into())
            .spawn(move || -> Result<(), String> {
                let _com = ComStaGuard::init()?;
                let dialog = create_configured_save_dialog(&name_owned, None)?;
                // 额外回读预设文件名，验证 vtable 槽位与 COM 实例完全工作正常
                let vtbl = unsafe { &*(*dialog.0).lp_vtbl };
                let mut raw_name: *mut u16 = std::ptr::null_mut();
                check_hr(
                    unsafe { (vtbl.get_file_name)(dialog.0, &mut raw_name) },
                    "读取预设文件名",
                )?;
                if raw_name.is_null() {
                    return Err("预设文件名为空".into());
                }
                let actual = unsafe {
                    let mut len = 0usize;
                    while *raw_name.add(len) != 0 {
                        len += 1;
                    }
                    let text = String::from_utf16_lossy(std::slice::from_raw_parts(raw_name, len));
                    CoTaskMemFree(raw_name.cast::<c_void>());
                    text
                };
                if actual != name_owned {
                    return Err(format!("预设文件名不匹配：{actual} != {name_owned}"));
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?
            .join()
            .map_err(|_| "STA 测试线程 panic".to_string())?;

        if should_uninit_caller {
            unsafe { CoUninitialize() };
        }
        res
    }
}

#[cfg(windows)]
fn pick_save_markdown_path(
    owner_hwnd: usize,
    safe_filename: &str,
    initial_dir: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    win_save_dialog::run_sta_save_dialog(owner_hwnd, safe_filename, initial_dir)
}

#[cfg(target_os = "macos")]
fn pick_save_markdown_path(
    _owner_hwnd: usize,
    safe_filename: &str,
    initial_dir: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    use std::process::Command;

    let mut cmd = Command::new("osascript");
    if let Some(dir) = initial_dir {
        cmd.arg("-e")
            .arg(
                "on run argv\n\
                 set defName to item 1 of argv\n\
                 set defDir to POSIX file (item 2 of argv)\n\
                 POSIX path of (choose file name with prompt \"导出会话 Markdown\" default name defName default location defDir)\n\
                 end run",
            )
            .arg("--")
            .arg(safe_filename)
            .arg(dir.as_os_str());
    } else {
        cmd.arg("-e")
            .arg(
                "on run argv\n\
                 set defName to item 1 of argv\n\
                 POSIX path of (choose file name with prompt \"导出会话 Markdown\" default name defName)\n\
                 end run",
            )
            .arg("--")
            .arg(safe_filename);
    }

    let output = cmd
        .output()
        .map_err(|error| format!("无法调用系统保存面板：{error}"))?;
    if output.status.success() {
        let chosen = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if chosen.is_empty() {
            return Ok(None);
        }
        return Ok(Some(PathBuf::from(chosen)));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("-128") || stderr.to_ascii_lowercase().contains("canceled") {
        return Ok(None);
    }
    Err(format!("打开系统保存面板失败：{}", stderr.trim()))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn pick_save_markdown_path(
    _owner_hwnd: usize,
    safe_filename: &str,
    initial_dir: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    use std::process::Command;

    let default_target = initial_dir
        .map(|dir| dir.join(safe_filename))
        .or_else(|| dirs::download_dir().map(|dir| dir.join(safe_filename)))
        .unwrap_or_else(|| PathBuf::from(safe_filename));

    if let Ok(output) = Command::new("zenity")
        .arg("--file-selection")
        .arg("--save")
        .arg("--confirm-overwrite")
        .arg("--title=导出会话 Markdown")
        .arg(format!("--filename={}", default_target.display()))
        .arg("--file-filter=Markdown (*.md) | *.md")
        .arg("--file-filter=All files (*) | *")
        .output()
    {
        if output.status.success() {
            let chosen = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return Ok((!chosen.is_empty()).then(|| PathBuf::from(chosen)));
        }
        if output.status.code() == Some(1) {
            return Ok(None);
        }
    }

    if let Ok(output) = Command::new("kdialog")
        .arg("--getsavefilename")
        .arg(&default_target)
        .arg("*.md|Markdown (*.md)")
        .output()
    {
        if output.status.success() {
            let chosen = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return Ok((!chosen.is_empty()).then(|| PathBuf::from(chosen)));
        }
        if output.status.code() == Some(1) {
            return Ok(None);
        }
    }

    Err("未检测到系统文件选择对话框组件（zenity 或 kdialog）".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_illegal_characters_and_appends_md_extension() {
        assert_eq!(
            sanitize_export_filename("排查网关: JWKS/401*?\"<>|修复.md"),
            "排查网关 JWKS 401 修复.md"
        );
        assert_eq!(sanitize_export_filename("   "), "session.md");
        assert_eq!(sanitize_export_filename("..."), "session.md");
        assert_eq!(sanitize_export_filename("CON.md"), "session.md");
        assert_eq!(sanitize_export_filename("nul"), "session.md");
        assert_eq!(sanitize_export_filename("hello-world"), "hello-world.md");
    }

    #[test]
    fn writes_utf8_markdown_content_to_selected_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("导出测试会话.md");
        let body = "# 测试标题\n\n- ⏺ Bash(cargo test)\n\n正文内容\n";

        let saved = write_markdown_to_path(&target, body).expect("write ok");
        assert_eq!(PathBuf::from(&saved), target);
        assert_eq!(std::fs::read_to_string(&target).expect("read back"), body);
    }

    #[test]
    fn rejects_writing_into_nonexistent_parent_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing = temp.path().join("missing-dir").join("out.md");
        let err = write_markdown_to_path(&missing, "# hi").expect_err("must fail");
        assert!(err.contains("目标目录不存在"));
    }

    #[cfg(windows)]
    #[test]
    fn creates_and_configures_ifile_save_dialog_even_from_mta_thread() {
        let safe = sanitize_export_filename("排查网关 JWKS 401.md");
        win_save_dialog::verify_dialog_creation_from_mta_caller(&safe)
            .expect("STA thread must instantiate and configure IFileSaveDialog cleanly");
    }
}
