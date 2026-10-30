#![windows_subsystem = "windows"]

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{Mutex, RwLock};
use std::thread;

use windows::ApplicationModel::{StartupTask, StartupTaskState};
use windows::Win32::Devices::Display::*;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::*;
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, DEVICE_STATE_ACTIVE, IAudioClient,
    IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, eRender,
};
use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    STGM_READ,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, HSTRING, PCSTR, PCWSTR, Result, w};

const WM_TRAY: u32 = WM_APP + 1;
const ID_EXIT: usize = 1;
const ID_MOUSE_LOCK: usize = 2;
const ID_RESCUE: usize = 3;
const ID_AUTOSTART: usize = 4;
const ID_KEEP_AWAKE: usize = 5;
const ID_LOW_RES: usize = 6;
const ID_DEVICE_FIRST: usize = 100;
const TIMER_KEEP_AWAKE: usize = 1;
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");

struct Config {
    device: String,
    mouse_lock: bool,
    rescue: bool,
    keep_awake: bool,
    low_res: bool,
    saved_mode: String, // "WxH@Hz" to go back to when low_res is turned off
}

const fn default_config() -> Config {
    Config {
        device: String::new(),
        mouse_lock: true,
        rescue: true,
        keep_awake: true,
        low_res: false,
        saved_mode: String::new(),
    }
}

struct Layout {
    phantoms: Vec<RECT>,
    real: Vec<RECT>,
    speaker_devices: Vec<String>,
}

struct KeepAwake {
    device: String,
    client: IAudioClient,
    render: IAudioRenderClient,
    frames: u32,
}

static LAYOUT: RwLock<Layout> = RwLock::new(Layout {
    phantoms: Vec::new(),
    real: Vec::new(),
    speaker_devices: Vec::new(),
});
static CONFIG: RwLock<Config> = RwLock::new(default_config());
static MENU_DEVICES: Mutex<Vec<String>> = Mutex::new(Vec::new());
static MOVING: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

thread_local! {
    // COM, main thread only
    static KEEP_AWAKE: RefCell<Option<KeepAwake>> = const { RefCell::new(None) };
}

fn wide_to_string(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

fn config_path() -> PathBuf {
    PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default())
        .join("Unmonitor")
        .join("config.ini")
}

fn load_config() -> Config {
    let mut config = default_config();
    for line in fs::read_to_string(config_path())
        .unwrap_or_default()
        .lines()
    {
        match line.split_once('=') {
            Some(("device", v)) => config.device = v.to_string(),
            Some(("mouse_lock", v)) => config.mouse_lock = v == "1",
            Some(("rescue_windows", v)) => config.rescue = v == "1",
            Some(("keep_awake", v)) => config.keep_awake = v == "1",
            Some(("low_res", v)) => config.low_res = v == "1",
            Some(("saved_mode", v)) => config.saved_mode = v.to_string(),
            _ => {}
        }
    }
    config
}

fn save_config(config: &Config) {
    let path = config_path();
    let _ = fs::create_dir_all(path.parent().unwrap());
    let _ = fs::write(
        path,
        format!(
            "device={}\nmouse_lock={}\nrescue_windows={}\nkeep_awake={}\nlow_res={}\nsaved_mode={}\n",
            config.device,
            config.mouse_lock as u8,
            config.rescue as u8,
            config.keep_awake as u8,
            config.low_res as u8,
            config.saved_mode
        ),
    );
}

// the store build is an msix package, which can't use the Run key
fn packaged() -> bool {
    let mut len = 0;
    unsafe { GetCurrentPackageFullName(&mut len, None) != APPMODEL_ERROR_NO_PACKAGE }
}

// id matches the startupTask in packaging/AppxManifest.xml
fn startup_task() -> Result<StartupTask> {
    StartupTask::GetAsync(&HSTRING::from("UnmonitorStartup"))?.join()
}

fn autostart_enabled() -> bool {
    if packaged() {
        return startup_task()
            .and_then(|task| task.State())
            .is_ok_and(|state| {
                state == StartupTaskState::Enabled || state == StartupTaskState::EnabledByPolicy
            });
    }
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            w!("Unmonitor"),
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        ) == ERROR_SUCCESS
    }
}

fn set_autostart(enable: bool) {
    if packaged() {
        if let Ok(task) = startup_task() {
            let _ = if enable {
                task.RequestEnableAsync()
                    .and_then(|op| op.join())
                    .map(|_| ())
            } else {
                task.Disable()
            };
        }
        return;
    }
    unsafe {
        if !enable {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, w!("Unmonitor"));
            return;
        }
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let value: Vec<u16> = format!("\"{}\"", exe.display())
            .encode_utf16()
            .chain([0])
            .collect();
        let _ = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            w!("Unmonitor"),
            REG_SZ.0,
            Some(value.as_ptr().cast()),
            (value.len() * 2) as u32,
        );
    }
}

// (monitor name, gdi name like \\.\DISPLAY2)
fn outputs() -> Vec<(String, String)> {
    let mut paths = Vec::new();
    unsafe {
        loop {
            let (mut path_count, mut mode_count) = (0, 0);
            if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
                != ERROR_SUCCESS
            {
                return Vec::new();
            }
            paths.resize(path_count as usize, DISPLAYCONFIG_PATH_INFO::default());
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
            // topology can change between the two calls
            match QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            ) {
                ERROR_SUCCESS => {
                    paths.truncate(path_count as usize);
                    break;
                }
                ERROR_INSUFFICIENT_BUFFER => continue,
                _ => return Vec::new(),
            }
        }
    }

    paths
        .iter()
        .filter_map(|path| unsafe {
            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            target.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            target.header.size = size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            target.header.adapterId = path.targetInfo.adapterId;
            target.header.id = path.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut target.header) != 0 {
                return None;
            }
            let name = wide_to_string(&target.monitorFriendlyDeviceName);
            if name.is_empty() {
                return None;
            }

            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            source.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            source.header.adapterId = path.sourceInfo.adapterId;
            source.header.id = path.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0 {
                return None;
            }
            Some((name, wide_to_string(&source.viewGdiDeviceName)))
        })
        .collect()
}

fn monitor_info(monitor: HMONITOR) -> MONITORINFOEXW {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    unsafe {
        let _ = GetMonitorInfoW(monitor, &mut info.monitorInfo);
    }
    info
}

fn refresh_layout() {
    let device = CONFIG.read().unwrap().device.clone();
    let phantom_names: Vec<String> = outputs()
        .into_iter()
        .filter(|(name, _)| *name == device)
        .map(|(_, gdi)| gdi)
        .collect();

    unsafe extern "system" fn collect(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        unsafe { (*(data.0 as *mut Vec<HMONITOR>)).push(m) };
        true.into()
    }
    let mut monitors: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut monitors as *mut _ as isize),
        );
    }

    let mut layout = Layout {
        phantoms: Vec::new(),
        real: Vec::new(),
        speaker_devices: Vec::new(),
    };
    for monitor in monitors {
        let info = monitor_info(monitor);
        let gdi = wide_to_string(&info.szDevice);
        // picking the primary by mistake would lock the cursor onto the invisible screen
        let primary = info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0;
        if !primary && phantom_names.contains(&gdi) {
            layout.phantoms.push(info.monitorInfo.rcMonitor);
            layout.speaker_devices.push(gdi);
        } else {
            layout.real.push(info.monitorInfo.rcMonitor);
        }
    }
    let speaker_devices = layout.speaker_devices.clone();
    *LAYOUT.write().unwrap() = layout;

    // mode change re-enters here via WM_DISPLAYCHANGE, don't hold locks
    if CONFIG.read().unwrap().low_res {
        for gdi in speaker_devices {
            if let Some(previous) = set_speaker_mode(&gdi, true, "") {
                let mut config = CONFIG.write().unwrap();
                config.saved_mode = previous;
                save_config(&config);
            }
        }
    }
}

fn display_modes(gdi: &str) -> Vec<DEVMODEW> {
    let name = HSTRING::from(gdi);
    let mut modes = Vec::new();
    for i in 0.. {
        let mut mode = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        if !unsafe { EnumDisplaySettingsW(&name, ENUM_DISPLAY_SETTINGS_MODE(i), &mut mode) }
            .as_bool()
        {
            break;
        }
        modes.push(mode);
    }
    modes
}

fn mode_key(mode: &DEVMODEW) -> String {
    format!(
        "{}x{}@{}",
        mode.dmPelsWidth, mode.dmPelsHeight, mode.dmDisplayFrequency
    )
}

// returns the mode it switched away from
fn set_speaker_mode(gdi: &str, low: bool, saved: &str) -> Option<String> {
    let modes = display_modes(gdi);
    let size = |m: &&DEVMODEW| m.dmPelsWidth * m.dmPelsHeight;
    let target = if low {
        // below 720p hdmi may not have bandwidth left for all audio channels
        modes
            .iter()
            .filter(|m| m.dmPelsWidth >= 1280 && m.dmPelsHeight >= 720)
            .min_by_key(|m| (size(m), m.dmDisplayFrequency.abs_diff(60)))?
    } else {
        modes
            .iter()
            .find(|m| mode_key(m) == saved)
            .or_else(|| modes.iter().max_by_key(|m| (size(m), m.dmDisplayFrequency)))?
    };

    let name = HSTRING::from(gdi);
    let mut current = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    unsafe {
        if !EnumDisplaySettingsW(&name, ENUM_CURRENT_SETTINGS, &mut current).as_bool()
            || mode_key(&current) == mode_key(target)
        {
            return None;
        }
        let result = ChangeDisplaySettingsExW(&name, Some(target), None, CDS_UPDATEREGISTRY, None);
        (result == DISP_CHANGE_SUCCESSFUL).then(|| mode_key(&current))
    }
}

fn endpoint_name(endpoint: &IMMDevice) -> Option<String> {
    unsafe {
        let store = endpoint.OpenPropertyStore(STGM_READ).ok()?;
        let mut value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let name = PropVariantToStringAlloc(&value).ok();
        let _ = PropVariantClear(&mut value);
        let name = name?;
        let text = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as _));
        text
    }
}

fn fill_silence(stream: &KeepAwake) -> Result<()> {
    unsafe {
        let free = stream.frames - stream.client.GetCurrentPadding()?;
        stream.render.GetBuffer(free)?;
        stream
            .render
            .ReleaseBuffer(free, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
    }
}

fn open_keep_awake(device: &str) -> Option<KeepAwake> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let endpoints = enumerator
            .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            .ok()?;
        // e.g. "LG-SBAR (NVIDIA High Definition Audio)"
        let prefix = format!("{device} (");
        let endpoint = (0..endpoints.GetCount().ok()?)
            .filter_map(|i| endpoints.Item(i).ok())
            .find(|e| endpoint_name(e).is_some_and(|name| name.starts_with(&prefix)))?;

        let client: IAudioClient = endpoint.Activate(CLSCTX_ALL, None).ok()?;
        let format = client.GetMixFormat().ok()?;
        let initialized =
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 20_000_000, 0, format, None);
        CoTaskMemFree(Some(format as _));
        initialized.ok()?;

        let stream = KeepAwake {
            device: device.to_string(),
            render: client.GetService().ok()?,
            frames: client.GetBufferSize().ok()?,
            client,
        };
        fill_silence(&stream).ok()?;
        stream.client.Start().ok()?;
        Some(stream)
    }
}

fn user_idle_ms() -> u32 {
    let mut info = LASTINPUTINFO {
        cbSize: size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    unsafe {
        let _ = GetLastInputInfo(&mut info);
        GetTickCount().wrapping_sub(info.dwTime)
    }
}

fn keep_awake_tick() {
    let (enabled, device) = {
        let config = CONFIG.read().unwrap();
        (config.keep_awake, config.device.clone())
    };
    KEEP_AWAKE.with_borrow_mut(|stream| {
        // an open stream blocks sleep, so drop it when the user is away
        if !enabled || device.is_empty() || user_idle_ms() > 60_000 {
            *stream = None;
            return;
        }
        if stream
            .as_ref()
            .is_some_and(|s| s.device != device || fill_silence(s).is_err())
        {
            *stream = None;
        }
        if stream.is_none() {
            *stream = open_keep_awake(&device);
        }
    });
}

fn clamp_cursor(pt: POINT) -> Option<POINT> {
    if !CONFIG.read().unwrap().mouse_lock {
        return None;
    }
    let layout = LAYOUT.read().unwrap();
    let inside = |r: &RECT| pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom;
    if layout.phantoms.is_empty() || layout.real.iter().any(inside) {
        return None;
    }
    layout
        .real
        .iter()
        .map(|r| POINT {
            x: pt.x.clamp(r.left, r.right - 1),
            y: pt.y.clamp(r.top, r.bottom - 1),
        })
        .min_by_key(|c| (c.x - pt.x).pow(2) + (c.y - pt.y).pow(2))
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code == HC_ACTION as i32 && wparam.0 as u32 == WM_MOUSEMOVE {
            let pt = (*(lparam.0 as *const MSLLHOOKSTRUCT)).pt;
            if let Some(clamped) = clamp_cursor(pt) {
                let _ = SetCursorPos(clamped.x, clamped.y);
                return LRESULT(1);
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

// r is in workspace coords, origin converts them to screen coords
fn fit(r: RECT, origin: POINT, from: RECT, to: RECT) -> RECT {
    let width = (r.right - r.left).min(to.right - to.left);
    let height = (r.bottom - r.top).min(to.bottom - to.top);
    let left =
        (r.left + origin.x - from.left + to.left).clamp(to.left, to.right - width) - origin.x;
    let top = (r.top + origin.y - from.top + to.top).clamp(to.top, to.bottom - height) - origin.y;
    RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

fn rescue(hwnd: HWND) {
    unsafe {
        if !CONFIG.read().unwrap().rescue
            || GetAncestor(hwnd, GA_ROOT) != hwnd
            || !IsWindowVisible(hwnd).as_bool()
            || IsIconic(hwnd).as_bool()
            || IsHungAppWindow(hwnd).as_bool()
            // taskbars
            || GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0
        {
            return;
        }
        let from = monitor_info(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)).monitorInfo;
        if !LAYOUT.read().unwrap().phantoms.contains(&from.rcMonitor) {
            return;
        }
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let to = monitor_info(MonitorFromPoint(cursor, MONITOR_DEFAULTTOPRIMARY)).monitorInfo;
        if to.rcMonitor == from.rcMonitor {
            return;
        }

        // SetWindowPlacement ignores the rect while maximized
        let maximized = IsZoomed(hwnd).as_bool();
        if maximized {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let mut placement = WINDOWPLACEMENT {
            length: size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        if GetWindowPlacement(hwnd, &mut placement).is_err() {
            return;
        }
        let primary = monitor_info(MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY));
        let origin = POINT {
            x: primary.monitorInfo.rcWork.left,
            y: primary.monitorInfo.rcWork.top,
        };
        placement.rcNormalPosition =
            fit(placement.rcNormalPosition, origin, from.rcWork, to.rcWork);
        let _ = SetWindowPlacement(hwnd, &placement);
        if maximized {
            let _ = ShowWindow(hwnd, SW_MAXIMIZE);
        }
    }
}

fn rescue_all() {
    unsafe extern "system" fn each(hwnd: HWND, _: LPARAM) -> BOOL {
        rescue(hwnd);
        true.into()
    }
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(0));
    }
}

unsafe extern "system" fn win_event(
    _: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _: u32,
    _: u32,
) {
    if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 {
        return;
    }
    match event {
        EVENT_SYSTEM_MOVESIZESTART => MOVING.store(hwnd.0 as isize, Ordering::Relaxed),
        EVENT_SYSTEM_MOVESIZEEND => {
            MOVING.store(0, Ordering::Relaxed);
            rescue(hwnd);
        }
        // don't fight the user while dragging
        _ if MOVING.load(Ordering::Relaxed) != hwnd.0 as isize => rescue(hwnd),
        _ => {}
    }
}

fn taskbar_is_light() -> bool {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    unsafe {
        let _ = RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as _),
            Some(&mut size),
        );
    }
    value == 1
}

fn tray_icon() -> HICON {
    // see unmonitor.rc
    let resource = if taskbar_is_light() { 2 } else { 3 };
    unsafe {
        let Ok(instance) = GetModuleHandleW(None) else {
            return HICON::default();
        };
        LoadImageW(
            Some(instance.into()),
            PCWSTR(resource as _),
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_SHARED,
        )
        .map(|icon| HICON(icon.0))
        .unwrap_or_default()
    }
}

fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: tray_icon(),
        ..Default::default()
    };
    let tip: Vec<u16> = "Unmonitor".encode_utf16().collect();
    data.szTip[..tip.len()].copy_from_slice(&tip);
    data
}

// undocumented, needed for dark menus: 135 SetPreferredAppMode, 136 FlushMenuThemes
fn uxtheme(ordinal: usize) -> Option<unsafe extern "system" fn() -> isize> {
    unsafe { GetProcAddress(LoadLibraryW(w!("uxtheme.dll")).ok()?, PCSTR(ordinal as _)) }
}

fn tr(en: &str, tr: &str) -> HSTRING {
    const LANG_TURKISH: u16 = 0x1f;
    let turkish = unsafe { GetUserDefaultUILanguage() } & 0x3ff == LANG_TURKISH;
    HSTRING::from(if turkish { tr } else { en })
}

fn checked(on: bool) -> MENU_ITEM_FLAGS {
    if on {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING
    }
}

fn show_menu(hwnd: HWND) {
    unsafe {
        let (Ok(menu), Ok(device_menu)) = (CreatePopupMenu(), CreatePopupMenu()) else {
            return;
        };
        {
            let config = CONFIG.read().unwrap();
            let mut devices = MENU_DEVICES.lock().unwrap();
            *devices = outputs().into_iter().map(|(name, _)| name).collect();
            for (i, name) in devices.iter().enumerate() {
                let flags = checked(*name == config.device);
                let _ = AppendMenuW(
                    device_menu,
                    flags,
                    ID_DEVICE_FIRST + i,
                    &HSTRING::from(name),
                );
            }
            let _ = AppendMenuW(
                menu,
                MF_POPUP,
                device_menu.0 as usize,
                &tr("Speaker device", "Hoparlör cihazı"),
            );
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(
                menu,
                checked(config.mouse_lock),
                ID_MOUSE_LOCK,
                &tr("Lock mouse", "Mouse kilidi"),
            );
            let _ = AppendMenuW(
                menu,
                checked(config.rescue),
                ID_RESCUE,
                &tr("Move windows back", "Pencereleri kurtar"),
            );
            let _ = AppendMenuW(
                menu,
                checked(config.keep_awake),
                ID_KEEP_AWAKE,
                &tr("Keep audio awake", "Sesin başı kesilmesin"),
            );
            let _ = AppendMenuW(
                menu,
                checked(config.low_res),
                ID_LOW_RES,
                &tr("Run hidden screen at 720p", "Gizli ekranı 720p'ye düşür"),
            );
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(
                menu,
                checked(autostart_enabled()),
                ID_AUTOSTART,
                &tr("Start with Windows", "Windows ile başlat"),
            );
            let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, &tr("Exit", "Çıkış"));
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        if let Some(flush_menu_themes) = uxtheme(136) {
            flush_menu_themes();
        }
        // otherwise the menu doesn't close on outside click
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, None, hwnd, None);
        let _ = DestroyMenu(menu);
    }
}

fn on_command(id: usize) {
    let mut config = CONFIG.write().unwrap();
    match id {
        ID_EXIT => {
            unsafe { PostQuitMessage(0) };
            return;
        }
        ID_AUTOSTART => {
            set_autostart(!autostart_enabled());
            return;
        }
        ID_MOUSE_LOCK => config.mouse_lock = !config.mouse_lock,
        ID_RESCUE => config.rescue = !config.rescue,
        ID_KEEP_AWAKE => config.keep_awake = !config.keep_awake,
        ID_LOW_RES => config.low_res = !config.low_res,
        _ => {
            let devices = MENU_DEVICES.lock().unwrap();
            let Some(name) = id.checked_sub(ID_DEVICE_FIRST).and_then(|i| devices.get(i)) else {
                return;
            };
            // clicking the selected one again clears it
            config.device = if config.device == *name {
                String::new()
            } else {
                name.clone()
            };
        }
    }
    save_config(&config);
    let restore = (id == ID_LOW_RES && !config.low_res).then(|| config.saved_mode.clone());
    drop(config);
    if let Some(saved) = restore {
        let speaker_devices = LAYOUT.read().unwrap().speaker_devices.clone();
        for gdi in speaker_devices {
            set_speaker_mode(&gdi, false, &saved);
        }
    }
    refresh_layout();
    rescue_all();
    keep_awake_tick();
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_DISPLAYCHANGE => {
                refresh_layout();
                rescue_all();
            }
            WM_TRAY if lparam.0 as u32 == WM_RBUTTONUP => show_menu(hwnd),
            WM_COMMAND => on_command(wparam.0),
            WM_TIMER if wparam.0 == TIMER_KEEP_AWAKE => keep_awake_tick(),
            // light/dark switch
            WM_SETTINGCHANGE => {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &tray_data(hwnd));
            }
            // explorer restarted
            _ if msg == TASKBAR_CREATED.load(Ordering::Relaxed) => {
                let _ = Shell_NotifyIconW(NIM_ADD, &tray_data(hwnd));
            }
            _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
        }
        LRESULT(0)
    }
}

fn message_loop() {
    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            DispatchMessageW(&msg);
        }
    }
}

fn main() -> Result<()> {
    unsafe {
        let _instance_lock = CreateMutexW(None, false, w!("Unmonitor"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }
    }
    *CONFIG.write().unwrap() = load_config();

    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)?;
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        if let Some(f) = uxtheme(135) {
            let set_preferred_app_mode: unsafe extern "system" fn(i32) -> i32 =
                std::mem::transmute(f);
            set_preferred_app_mode(1); // AllowDark
        }
        refresh_layout();
        rescue_all();
        let instance = GetModuleHandleW(None)?;

        // own thread so a slow window move can't lag the cursor
        let mouse_instance = instance.0 as isize;
        thread::spawn(move || {
            let instance = HINSTANCE(mouse_instance as _);
            if SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), Some(instance), 0).is_ok() {
                message_loop();
            }
        });

        let class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            lpszClassName: w!("Unmonitor"),
            ..Default::default()
        };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("Unmonitor"),
            w!("Unmonitor"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        TASKBAR_CREATED.store(
            RegisterWindowMessageW(w!("TaskbarCreated")),
            Ordering::Relaxed,
        );
        let _ = Shell_NotifyIconW(NIM_ADD, &tray_data(hwnd));
        // 2s buffer, topped up every second
        SetTimer(Some(hwnd), TIMER_KEEP_AWAKE, 1000, None);
        keep_awake_tick();

        for (min, max) in [
            (EVENT_SYSTEM_MOVESIZESTART, EVENT_SYSTEM_MOVESIZEEND),
            (EVENT_OBJECT_SHOW, EVENT_OBJECT_SHOW),
            (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
        ] {
            SetWinEventHook(
                min,
                max,
                None,
                Some(win_event),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
        }

        message_loop();
        let _ = Shell_NotifyIconW(NIM_DELETE, &tray_data(hwnd));
    }
    Ok(())
}
