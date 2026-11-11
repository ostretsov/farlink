//! Windows tray: `Shell_NotifyIconW` with a hidden window on its own thread
//! (the window receives the icon's clicks; its message loop runs the menu).

use super::{Shared, TrayEvent, TrayStatus, icon};
use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, HICON, ICONINFO, MF_CHECKED,
    MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SM_CXSMICON, SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP,
    WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW,
};

/// The icon's messages arrive as this window message.
const WM_TRAY: u32 = WM_APP + 1;
const ID_OPEN: usize = 1;
const ID_AUTOSTART: usize = 2;
const ID_QUIT: usize = 3;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// The hidden window (0 until it exists).
#[derive(Clone)]
pub struct Handle(Arc<AtomicIsize>);

struct State {
    shared: Arc<Shared>,
    icon: HICON,
    /// Sent by Explorer when it (re)starts: the icon must be added again.
    taskbar_created: u32,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

pub fn start(shared: Arc<Shared>, retry_for: Duration) -> Handle {
    let handle = Handle(Arc::new(AtomicIsize::new(0)));
    let h = handle.clone();
    std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || unsafe { run(shared, retry_for, h) })
        .expect("start the tray thread");
    handle
}

unsafe fn run(shared: Arc<Shared>, retry_for: Duration, handle: Handle) {
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide("FarlinkTray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let title = wide("Farlink tray");
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            log::warn!("tray: could not create its window");
            shared.set_status(TrayStatus::Failed);
            return;
        }
        let size = GetSystemMetrics(SM_CXSMICON).clamp(16, 64) as u32;
        let icon = make_icon(size);
        let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        STATE.with(|s| *s.borrow_mut() = Some(State { shared: shared.clone(), icon, taskbar_created }));

        // Explorer may not be up yet right after login.
        let started = Instant::now();
        while !add_icon(hwnd, icon) {
            if started.elapsed() >= retry_for || shared.stop.load(Ordering::SeqCst) {
                log::warn!("tray: could not add the icon");
                shared.set_status(TrayStatus::Failed);
                DestroyWindow(hwnd);
                return;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        handle.0.store(hwnd as isize, Ordering::SeqCst);
        if shared.stop.load(Ordering::SeqCst) {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
        shared.set_status(TrayStatus::Ready);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        STATE.with(|s| {
            if let Some(st) = s.borrow_mut().take() {
                DestroyIcon(st.icon);
            }
        });
    }
}

fn notify_data(hwnd: HWND, icon: HICON) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = icon;
    for (dst, src) in nid.szTip.iter_mut().zip(crate::PRODUCT.encode_utf16()) {
        *dst = src;
    }
    nid
}

fn add_icon(hwnd: HWND, icon: HICON) -> bool {
    let nid = notify_data(hwnd, icon);
    unsafe { Shell_NotifyIconW(NIM_ADD, &nid) != 0 }
}

/// A colour icon from the drawn picture (BGRA rows, with an all-zero mask:
/// the alpha channel decides what shows).
fn make_icon(size: u32) -> HICON {
    let mut bgra = icon::rgba(size);
    for p in bgra.chunks_mut(4) {
        p.swap(0, 2);
    }
    let mask = vec![0u8; (size.div_ceil(16) * 2 * size) as usize];
    unsafe {
        let color = CreateBitmap(size as i32, size as i32, 1, 32, bgra.as_ptr().cast());
        let mask_bmp = CreateBitmap(size as i32, size as i32, 1, 1, mask.as_ptr().cast());
        let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask_bmp, hbmColor: color };
        let icon = CreateIconIndirect(&info);
        DeleteObject(color);
        DeleteObject(mask_bmp);
        icon
    }
}

unsafe fn show_menu(hwnd: HWND, shared: &Shared) {
    unsafe {
        let menu = CreatePopupMenu();
        let open = wide(&format!("Open {}", crate::PRODUCT));
        let autostart = wide("Start at login");
        let quit = wide("Quit");
        AppendMenuW(menu, MF_STRING, ID_OPEN, open.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        let checked = if shared.autostart.load(Ordering::SeqCst) { MF_CHECKED } else { 0 };
        AppendMenuW(menu, MF_STRING | checked, ID_AUTOSTART, autostart.as_ptr());
        AppendMenuW(menu, MF_STRING, ID_QUIT, quit.as_ptr());
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        // Without this the menu does not close when clicking elsewhere.
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, std::ptr::null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        match cmd as usize {
            ID_OPEN => shared.send(TrayEvent::Open),
            ID_AUTOSTART => shared.send(TrayEvent::ToggleAutostart),
            ID_QUIT => shared.send(TrayEvent::Quit),
            _ => {}
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let state = STATE.with(|s| s.borrow().as_ref().map(|st| (st.shared.clone(), st.icon, st.taskbar_created)));
        if let Some((shared, icon, taskbar_created)) = state {
            match msg {
                WM_TRAY => {
                    match (lparam & 0xffff) as u32 {
                        WM_LBUTTONUP => shared.send(TrayEvent::Open),
                        WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd, &shared),
                        _ => {}
                    }
                    return 0;
                }
                WM_CLOSE => {
                    let nid = notify_data(hwnd, icon);
                    Shell_NotifyIconW(NIM_DELETE, &nid);
                    DestroyWindow(hwnd);
                    return 0;
                }
                WM_DESTROY => {
                    PostQuitMessage(0);
                    return 0;
                }
                m if m == taskbar_created && m != 0 => {
                    add_icon(hwnd, icon);
                    return 0;
                }
                _ => {}
            }
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

impl Handle {
    /// The menu is built on each right click, so there is nothing to redraw.
    pub fn refresh(&self) {}

    /// Removes the icon and ends the tray thread.
    pub fn shutdown(&self) {
        let hwnd = self.0.swap(0, Ordering::SeqCst);
        if hwnd != 0 {
            unsafe {
                // SendMessage waits, so the icon is gone before the app exits.
                windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW(hwnd as HWND, WM_CLOSE, 0, 0);
            }
        }
    }
}
