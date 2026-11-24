use hbb_common::{bail, log, ResultType};
use std::{
    ffi::OsStr,
    io::Error,
    mem::size_of,
    os::windows::ffi::OsStrExt,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
#[cfg(target_pointer_width = "64")]
use winapi::um::winuser::{GetWindowLongPtrW, SetWindowLongPtrW};
#[cfg(target_pointer_width = "32")]
use winapi::um::winuser::{GetWindowLongW, SetWindowLongW};
use winapi::{
    shared::{
        minwindef::{DWORD, FALSE, HINSTANCE, HMODULE, LPARAM, LRESULT, TRUE, UINT, WPARAM},
        ntdef::{HANDLE, NULL},
        windef::{HBRUSH, HDC, HFONT, HGDIOBJ, HMENU, HMONITOR, HWND, RECT, SIZE},
        winerror::ERROR_CLASS_ALREADY_EXISTS,
    },
    um::{
        errhandlingapi::GetLastError,
        libloaderapi::GetModuleHandleW,
        processthreadsapi::GetCurrentThreadId,
        wingdi::{
            CreateFontW, CreateSolidBrush, DeleteObject, GetTextExtentPoint32W, SelectObject,
            SetBkMode, SetTextColor, TextOutW, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
            DEFAULT_CHARSET, DEFAULT_PITCH, FW_SEMIBOLD, OUT_DEFAULT_PRECIS, RGB, TRANSPARENT,
        },
        winuser::{
            BeginPaint, CloseDesktop, CreateWindowExW, DefWindowProcW, DestroyWindow,
            DispatchMessageW, EndPaint, EnumDisplayMonitors, FillRect, GetClientRect, GetDC,
            GetMessageW, GetMonitorInfoW, GetProcessWindowStation, GetThreadDesktop,
            GetUserObjectInformationA, InvalidateRect, KillTimer, OpenInputDesktop,
            PostThreadMessageW, RegisterClassExW, ReleaseDC, SetLayeredWindowAttributes, SetTimer,
            SetWindowPos, ShowWindow, TranslateMessage, UnregisterClassW, UpdateWindow, CS_HREDRAW,
            CS_VREDRAW, DESKTOP_READOBJECTS, GWLP_USERDATA, HTTRANSPARENT, HWND_TOPMOST, LWA_ALPHA,
            MONITORINFO, MONITORINFOEXW, MSG, PAINTSTRUCT, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
            SWP_SHOWWINDOW, SW_SHOWNOACTIVATE, UOI_NAME, WM_APP, WM_CREATE, WM_DESTROY,
            WM_DISPLAYCHANGE, WM_ERASEBKGND, WM_NCHITTEST, WM_PAINT, WM_TIMER, WNDCLASSEXW,
            WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
            WS_POPUP,
        },
    },
};

const WINDOW_CLASS: &str = "RustDeskPrivacyScreensaverWindowClass";
const WINDOW_NAME: &str = "RustDeskPrivacyScreensaverWindow";
const TEXT: &str = "隐私模式";
const TIMER_ID: usize = 1;
const TIMER_INTERVAL_MS: u32 = 33;
const TOPMOST_REFRESH_TICKS: u32 = 30;
const WM_APP_EXIT: UINT = WM_APP + 10;
const WM_APP_RESYNC: UINT = WM_APP + 11;
const SHOW_TIMEOUT: Duration = Duration::from_secs(2);

struct Overlay {
    device: String,
    font: HFONT,
    background: HBRUSH,
    text: Vec<u16>,
    text_width: i32,
    text_height: i32,
    margin: i32,
    client_width: i32,
    client_height: i32,
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    ticks: u32,
}

impl Overlay {
    fn new(device: String, client_width: i32, client_height: i32, index: usize) -> Self {
        let font = create_font(font_height(client_height));
        let text: Vec<u16> = TEXT.encode_utf16().collect();
        let (text_width, text_height) = text_size(font, &text);
        let background = unsafe { CreateSolidBrush(RGB(0, 0, 0)) };
        let seed = seed_for(index);
        let scale = (client_height as f32 / 1080.0).clamp(0.6, 1.8);
        let mut state = Self {
            device,
            font,
            background,
            text,
            text_width,
            text_height,
            margin: (font_height(client_height) / 2).max(12),
            client_width,
            client_height,
            x: 0.0,
            y: 0.0,
            vx: if seed & 1 == 0 {
                6.0 * scale
            } else {
                -6.0 * scale
            },
            vy: if seed & 2 == 0 {
                4.2 * scale
            } else {
                -4.2 * scale
            },
            ticks: 0,
        };
        state.x = state.margin as f32 + fraction(seed >> 2) * state.max_x();
        state.y = state.margin as f32 + fraction(seed >> 12) * state.max_y();
        state.clamp_position();
        state
    }

    fn max_x(&self) -> f32 {
        (self.client_width - self.text_width - 2 * self.margin).max(0) as f32
    }

    fn max_y(&self) -> f32 {
        (self.client_height - self.text_height - 2 * self.margin).max(0) as f32
    }

    fn clamp_position(&mut self) {
        let min_x = self.margin as f32;
        let min_y = self.margin as f32;
        self.x = self.x.clamp(min_x, min_x + self.max_x().max(0.0));
        self.y = self.y.clamp(min_y, min_y + self.max_y().max(0.0));
    }

    fn text_rect(&self) -> RECT {
        RECT {
            left: self.x as i32 - 2,
            top: self.y as i32 - 2,
            right: self.x as i32 + self.text_width + 2,
            bottom: self.y as i32 + self.text_height + 2,
        }
    }

    fn advance(&mut self) {
        let min_x = self.margin as f32;
        let max_x = min_x + self.max_x();
        if max_x > min_x {
            self.x += self.vx;
            if self.x <= min_x {
                self.x = min_x;
                self.vx = self.vx.abs();
            } else if self.x >= max_x {
                self.x = max_x;
                self.vx = -self.vx.abs();
            }
        } else {
            self.x = min_x;
            self.vx = 0.0;
        }

        let min_y = self.margin as f32;
        let max_y = min_y + self.max_y();
        if max_y > min_y {
            self.y += self.vy;
            if self.y <= min_y {
                self.y = min_y;
                self.vy = self.vy.abs();
            } else if self.y >= max_y {
                self.y = max_y;
                self.vy = -self.vy.abs();
            }
        } else {
            self.y = min_y;
            self.vy = 0.0;
        }

        self.ticks = self.ticks.wrapping_add(1);
    }

    fn set_client_size(&mut self, width: i32, height: i32) {
        if width <= 0 || height <= 0 {
            return;
        }
        if width == self.client_width && height == self.client_height {
            return;
        }
        self.client_width = width;
        self.client_height = height;
        let new_font = create_font(font_height(height));
        if !new_font.is_null() {
            unsafe {
                DeleteObject(self.font as HGDIOBJ);
            }
            self.font = new_font;
            let (text_width, text_height) = text_size(self.font, &self.text);
            self.text_width = text_width;
            self.text_height = text_height;
            self.margin = (font_height(height) / 2).max(12);
        }
        self.clamp_position();
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            if !self.font.is_null() {
                DeleteObject(self.font as HGDIOBJ);
            }
            if !self.background.is_null() {
                DeleteObject(self.background as HGDIOBJ);
            }
        }
    }
}

#[cfg(target_pointer_width = "64")]
unsafe fn get_overlay_state(hwnd: HWND) -> *mut Overlay {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Overlay
}

#[cfg(target_pointer_width = "32")]
unsafe fn get_overlay_state(hwnd: HWND) -> *mut Overlay {
    GetWindowLongW(hwnd, GWLP_USERDATA) as usize as *mut Overlay
}

#[cfg(target_pointer_width = "64")]
unsafe fn set_overlay_state(hwnd: HWND, state: *mut Overlay) {
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
}

#[cfg(target_pointer_width = "32")]
unsafe fn set_overlay_state(hwnd: HWND, state: *mut Overlay) {
    SetWindowLongW(hwnd, GWLP_USERDATA, state as usize as i32);
}

/// Shows a black, click-through window on every physical display and draws a
/// drifting "隐私模式" text on it. The windows only belong to this process; the
/// caller is expected to unplug the virtual display / restore the displays when
/// the screensaver is hidden.
pub struct Screensaver {
    thread_id: DWORD,
    join: Option<JoinHandle<()>>,
}

impl Screensaver {
    /// `devices` are the display device names (e.g. `\\.\DISPLAY1`) of the
    /// physical displays to cover. Returns an error when no window could be
    /// created, so the caller can fall back to another way of hiding the screen.
    pub fn show(devices: &[String]) -> ResultType<Self> {
        if devices.is_empty() {
            bail!("No physical display is available for the privacy screensaver");
        }
        let devices = devices.to_vec();
        let thread_id = Arc::new(AtomicU32::new(0));
        let thread_id_for_thread = thread_id.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        let join = thread::Builder::new()
            .name("privacy-screensaver".to_owned())
            .spawn(move || {
                let tid = unsafe { GetCurrentThreadId() };
                thread_id_for_thread.store(tid, Ordering::SeqCst);
                if stop_for_thread.load(Ordering::SeqCst) {
                    return;
                }
                unsafe {
                    match create_overlays(&devices) {
                        Ok(windows) => {
                            let _ = tx.send(Ok(()));
                            if !stop_for_thread.load(Ordering::SeqCst) {
                                run_message_loop(&windows);
                            }
                            let class_name = wide(WINDOW_CLASS);
                            let hinstance = GetModuleHandleW(std::ptr::null());
                            for hwnd in windows.into_iter().rev() {
                                destroy_overlay_window(hwnd);
                            }
                            UnregisterClassW(class_name.as_ptr(), hinstance);
                        }
                        Err(e) => {
                            let _ = tx.send(Err(format!("{:#}", e)));
                        }
                    }
                }
            })?;
        match rx.recv_timeout(SHOW_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                thread_id: thread_id.load(Ordering::SeqCst),
                join: Some(join),
            }),
            Ok(Err(e)) => {
                let _ = join.join();
                bail!("Failed to show the privacy screensaver: {}", e);
            }
            Err(e) => {
                stop.store(true, Ordering::SeqCst);
                let tid = thread_id.load(Ordering::SeqCst);
                if tid != 0 {
                    unsafe { PostThreadMessageW(tid, WM_APP_EXIT, 0, 0) };
                }
                let _ = join.join();
                bail!("Failed to show the privacy screensaver: {}", e);
            }
        }
    }

    pub fn hide(&mut self) {
        if let Some(join) = self.join.take() {
            unsafe { PostThreadMessageW(self.thread_id, WM_APP_EXIT, 0, 0) };
            if join.join().is_err() {
                log::error!("Privacy screensaver thread panicked");
            }
        }
    }

    /// Moves the overlay windows to the current monitor rects of their displays.
    /// `WM_DISPLAYCHANGE` normally does this already, this is only a safety net for
    /// the display mode change that turns the virtual display primary.
    pub fn resync(&self) {
        unsafe { PostThreadMessageW(self.thread_id, WM_APP_RESYNC, 0, 0) };
    }
}

impl Drop for Screensaver {
    fn drop(&mut self) {
        self.hide();
    }
}

fn font_height(client_height: i32) -> i32 {
    ((client_height as f32 * 0.09) as i32).clamp(36, 220)
}

fn fraction(value: u32) -> f32 {
    (value % 1000) as f32 / 1000.0
}

fn seed_for(index: usize) -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    nanos ^ (index as u32).wrapping_mul(2_654_435_761)
}

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn wide_to_string(value: &[u16]) -> String {
    let len = value
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..len])
}

fn create_font(height: i32) -> HFONT {
    let face = wide("Microsoft YaHei UI");
    unsafe {
        CreateFontW(
            -height,
            0,
            0,
            0,
            FW_SEMIBOLD,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH,
            face.as_ptr(),
        )
    }
}

fn text_size(font: HFONT, text: &[u16]) -> (i32, i32) {
    unsafe {
        let hdc = GetDC(NULL as HWND);
        if hdc.is_null() {
            return (0, 0);
        }
        let old = SelectObject(hdc, font as HGDIOBJ);
        let mut size = SIZE { cx: 0, cy: 0 };
        let ok = GetTextExtentPoint32W(hdc, text.as_ptr(), text.len() as i32, &mut size);
        SelectObject(hdc, old);
        ReleaseDC(NULL as HWND, hdc);
        if ok == FALSE {
            (0, 0)
        } else {
            (size.cx, size.cy)
        }
    }
}

fn union_rect(a: &RECT, b: &RECT) -> RECT {
    RECT {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    }
}

struct MonitorRectSearch<'a> {
    device: &'a str,
    rect: Option<RECT>,
}

unsafe extern "system" fn enum_monitor_rect(
    hmon: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> i32 {
    let search = &mut *(lparam as *mut MonitorRectSearch);
    let mut info: MONITORINFOEXW = std::mem::zeroed();
    info.cbSize = size_of::<MONITORINFOEXW>() as DWORD;
    if FALSE == GetMonitorInfoW(hmon, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) {
        return TRUE;
    }
    if wide_to_string(&info.szDevice).eq_ignore_ascii_case(search.device) {
        search.rect = Some(info.rcMonitor);
        return FALSE;
    }
    TRUE
}

fn find_monitor_rect(device: &str) -> Option<RECT> {
    let mut search = MonitorRectSearch { device, rect: None };
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(enum_monitor_rect),
            &mut search as *mut _ as LPARAM,
        );
    }
    search.rect
}

/// The screensaver window is only useful on the desktop the user is looking at.
/// On a locked / disconnected session the windows would be created on another
/// desktop and stay invisible, which would silently weaken the privacy mode, so
/// the caller has to fall back to disabling the physical displays instead.
fn is_on_input_desktop() -> bool {
    unsafe {
        let window_station = GetProcessWindowStation();
        if window_station.is_null() {
            return false;
        }
        let mut window_station_name = [0u8; 256];
        let mut needed: DWORD = 0;
        if FALSE
            == GetUserObjectInformationA(
                window_station as HANDLE,
                UOI_NAME as i32,
                window_station_name.as_mut_ptr() as *mut _,
                window_station_name.len() as DWORD,
                &mut needed,
            )
        {
            return false;
        }
        if !narrow_name_eq(&window_station_name, "WinSta0") {
            return false;
        }

        let current = GetThreadDesktop(GetCurrentThreadId());
        if current.is_null() {
            return false;
        }
        let input = OpenInputDesktop(0, FALSE, DESKTOP_READOBJECTS);
        if input.is_null() {
            return false;
        }
        let mut current_name = [0u8; 256];
        let mut input_name = [0u8; 256];
        let current_ok = FALSE
            != GetUserObjectInformationA(
                current as HANDLE,
                UOI_NAME as i32,
                current_name.as_mut_ptr() as *mut _,
                current_name.len() as DWORD,
                &mut needed,
            );
        let input_ok = FALSE
            != GetUserObjectInformationA(
                input as HANDLE,
                UOI_NAME as i32,
                input_name.as_mut_ptr() as *mut _,
                input_name.len() as DWORD,
                &mut needed,
            );
        CloseDesktop(input);
        current_ok && input_ok && current_name == input_name
    }
}

fn narrow_name_eq(buffer: &[u8], expected: &str) -> bool {
    let len = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    buffer[..len].eq_ignore_ascii_case(expected.as_bytes())
}

unsafe fn create_overlay_window(
    device: &str,
    rect: &RECT,
    index: usize,
    hinstance: HINSTANCE,
    class_name: &[u16],
    window_name: &[u16],
) -> ResultType<HWND> {
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        bail!("Invalid monitor rect for {}", device);
    }
    let state = Box::into_raw(Box::new(Overlay::new(
        device.to_owned(),
        width,
        height,
        index,
    )));
    if (*state).font.is_null() || (*state).background.is_null() {
        drop(Box::from_raw(state));
        bail!(
            "Failed to create privacy screensaver resources for {}",
            device
        );
    }
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
        class_name.as_ptr(),
        window_name.as_ptr(),
        WS_POPUP,
        rect.left,
        rect.top,
        width,
        height,
        NULL as HWND,
        NULL as HMENU,
        hinstance,
        state as *mut _,
    );
    if hwnd.is_null() {
        drop(Box::from_raw(state));
        bail!(
            "Failed to create privacy screensaver window for {}, error: {}",
            device,
            Error::last_os_error()
        );
    }
    set_overlay_state(hwnd, state);
    if FALSE == SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA) {
        let error = Error::last_os_error();
        set_overlay_state(hwnd, std::ptr::null_mut());
        DestroyWindow(hwnd);
        drop(Box::from_raw(state));
        bail!(
            "Failed to make privacy screensaver window transparent to input, error: {}",
            error
        );
    }
    SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        rect.left,
        rect.top,
        width,
        height,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    UpdateWindow(hwnd);
    Ok(hwnd)
}

unsafe fn create_overlays(devices: &[String]) -> ResultType<Vec<HWND>> {
    if !is_on_input_desktop() {
        bail!("The current desktop is not the input desktop");
    }
    let class_name = wide(WINDOW_CLASS);
    let window_name = wide(WINDOW_NAME);
    let hinstance: HMODULE = GetModuleHandleW(std::ptr::null());
    if hinstance.is_null() {
        bail!(
            "Failed to get the module handle, error: {}",
            Error::last_os_error()
        );
    }
    let mut window_class: WNDCLASSEXW = std::mem::zeroed();
    window_class.cbSize = size_of::<WNDCLASSEXW>() as UINT;
    window_class.style = CS_HREDRAW | CS_VREDRAW;
    window_class.lpfnWndProc = Some(window_proc);
    window_class.hInstance = hinstance;
    window_class.hbrBackground = NULL as HBRUSH;
    window_class.lpszClassName = class_name.as_ptr();
    if 0 == RegisterClassExW(&window_class) && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
        bail!(
            "Failed to register the privacy screensaver window class, error: {}",
            Error::last_os_error()
        );
    }

    let mut windows = Vec::with_capacity(devices.len());
    for (index, device) in devices.iter().enumerate() {
        let Some(rect) = find_monitor_rect(device) else {
            log::warn!(
                "Privacy screensaver: the monitor of physical display {} was not found",
                device
            );
            continue;
        };
        match create_overlay_window(device, &rect, index, hinstance, &class_name, &window_name) {
            Ok(hwnd) => windows.push(hwnd),
            Err(e) => {
                while let Some(hwnd) = windows.pop() {
                    destroy_overlay_window(hwnd);
                }
                UnregisterClassW(class_name.as_ptr(), hinstance);
                return Err(e);
            }
        }
    }
    if windows.is_empty() {
        UnregisterClassW(class_name.as_ptr(), hinstance);
        bail!("No physical display is available for the privacy screensaver");
    }
    log::info!(
        "Privacy screensaver is shown on {} physical display(s)",
        windows.len()
    );
    Ok(windows)
}

unsafe fn destroy_overlay_window(hwnd: HWND) {
    let state = get_overlay_state(hwnd);
    set_overlay_state(hwnd, std::ptr::null_mut());
    if FALSE == DestroyWindow(hwnd) {
        log::warn!(
            "Failed to destroy privacy screensaver window, error: {}",
            Error::last_os_error()
        );
    }
    if !state.is_null() {
        drop(Box::from_raw(state));
    }
}

unsafe fn paint_window(hwnd: HWND) {
    let state = get_overlay_state(hwnd);
    if state.is_null() {
        return;
    }
    let state = &mut *state;
    let mut paint_struct: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut paint_struct);
    if hdc.is_null() {
        return;
    }
    FillRect(hdc, &paint_struct.rcPaint, state.background);
    if state.text_width > 0 && state.text_height > 0 {
        let old_font = SelectObject(hdc, state.font as HGDIOBJ);
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, RGB(0xF0, 0xF0, 0xF0));
        TextOutW(
            hdc,
            state.x as i32,
            state.y as i32,
            state.text.as_ptr(),
            state.text.len() as i32,
        );
        SelectObject(hdc, old_font);
    }
    EndPaint(hwnd, &paint_struct);
}

unsafe fn step_window(hwnd: HWND) {
    let state = get_overlay_state(hwnd);
    if state.is_null() {
        return;
    }
    let state = &mut *state;
    let old_rect = state.text_rect();
    state.advance();
    let new_rect = state.text_rect();
    InvalidateRect(hwnd, &union_rect(&old_rect, &new_rect), FALSE);
    if state.ticks % TOPMOST_REFRESH_TICKS == 0 {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

unsafe fn resync_window(hwnd: HWND) {
    let state = get_overlay_state(hwnd);
    if state.is_null() {
        return;
    }
    let device = (*state).device.clone();
    let Some(rect) = find_monitor_rect(&device) else {
        // Keep the window where it is: losing the local privacy screen is worse
        // than covering a stale area after a display topology change.
        log::warn!(
            "Privacy screensaver cannot find the monitor of {}, keeping its window in place",
            device
        );
        return;
    };
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        log::warn!(
            "Privacy screensaver got an invalid monitor rect for {}, keeping its window in place",
            device
        );
        return;
    }
    SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        rect.left,
        rect.top,
        width,
        height,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    (*state).set_client_size(width, height);
    let mut client = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if FALSE != GetClientRect(hwnd, &mut client) && client.right > 0 && client.bottom > 0 {
        (*state).set_client_size(client.right, client.bottom);
    }
    InvalidateRect(hwnd, std::ptr::null(), FALSE);
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            SetTimer(hwnd, TIMER_ID, TIMER_INTERVAL_MS, None);
            0
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            paint_window(hwnd);
            0
        }
        WM_TIMER => {
            if wparam == TIMER_ID {
                step_window(hwnd);
            }
            0
        }
        WM_DISPLAYCHANGE => {
            resync_window(hwnd);
            0
        }
        WM_NCHITTEST => HTTRANSPARENT as LRESULT,
        WM_DESTROY => {
            KillTimer(hwnd, TIMER_ID);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn run_message_loop(windows: &[HWND]) {
    let mut msg: MSG = std::mem::zeroed();
    loop {
        let result = GetMessageW(&mut msg, NULL as HWND, 0, 0);
        if result == 0 || result == -1 {
            break;
        }
        if msg.message == WM_APP_EXIT {
            break;
        }
        if msg.message == WM_APP_RESYNC {
            for hwnd in windows {
                resync_window(*hwnd);
            }
            continue;
        }
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}
