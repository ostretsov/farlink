//! Windows 10/11 and Server 2016+: GDI capture of every monitor, SendInput for
//! mouse and keyboard. The thread follows the *input desktop*, so when the
//! helper runs as SYSTEM in the console session it also sees and controls the
//! lock screen, the sign-in screen and UAC prompts.

use super::{Desktop, to_absolute};
use crate::codec::Frame;
use crate::keys;
use crate::proto::{InputEvent, MonitorInfo, MouseButton};
use anyhow::{Result, bail};
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::StationsAndDesktops::*;
use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};
use windows_sys::core::BOOL;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Named event the service waits on to send Ctrl+Alt+Del for us.
pub const SAS_EVENT: &str = "Global\\FarlinkSendSAS";

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub struct WinDesktop {
    desk: HDESK,
}

impl WinDesktop {
    pub fn open() -> Result<WinDesktop> {
        unsafe {
            if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
                // Windows Server 2016 / older Windows 10.
                let _ = SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE);
            }
        }
        let mut d = WinDesktop { desk: null_mut() };
        d.follow_input_desktop();
        Ok(d)
    }

    /// Switch this thread to whatever desktop currently receives input
    /// (normal desktop, Winlogon/lock screen, or the UAC secure desktop).
    fn follow_input_desktop(&mut self) {
        unsafe {
            let desk = OpenInputDesktop(0, 0, GENERIC_ALL);
            if desk.is_null() {
                return;
            }
            if SetThreadDesktop(desk) != 0 {
                if !self.desk.is_null() {
                    CloseDesktop(self.desk);
                }
                self.desk = desk;
            } else {
                CloseDesktop(desk);
            }
        }
    }

    fn send(&self, inputs: &[INPUT]) {
        unsafe {
            SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32);
        }
    }

    fn mouse(flags: u32, dx: i32, dy: i32, data: i32) -> INPUT {
        let mut i: INPUT = unsafe { zeroed() };
        i.r#type = INPUT_MOUSE;
        i.Anonymous.mi = MOUSEINPUT { dx, dy, mouseData: data as _, dwFlags: flags, time: 0, dwExtraInfo: 0 };
        i
    }

    fn key(vk: u16, scan: u16, flags: u32) -> INPUT {
        let mut i: INPUT = unsafe { zeroed() };
        i.r#type = INPUT_KEYBOARD;
        i.Anonymous.ki = KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 };
        i
    }

    fn move_input(monitors: &[MonitorInfo], mon: u8, x: f32, y: f32) -> Option<INPUT> {
        let (ax, ay) = to_absolute(monitors, mon, x, y)?;
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
            )
        };
        let nx = ((ax - vx) as i64 * 65535 / (vw as i64 - 1)) as i32;
        let ny = ((ay - vy) as i64 * 65535 / (vh as i64 - 1)) as i32;
        Some(Self::mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK, nx, ny, 0))
    }

    fn named_key(&self, name: &str, down: bool) {
        let Some((vk, ext)) = keys::windows_vk(name) else { return };
        let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
        let mut flags = if down { 0 } else { KEYEVENTF_KEYUP };
        if ext {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        self.send(&[Self::key(vk, scan, flags)]);
    }

    fn ctrl_alt_del(&self) {
        unsafe {
            // Ask our service (it is allowed to); fall back to calling directly.
            let ev = OpenEventW(EVENT_MODIFY_STATE, 0, wide(SAS_EVENT).as_ptr());
            if !ev.is_null() {
                SetEvent(ev);
                CloseHandle(ev);
                return;
            }
        }
        send_sas();
    }
}

/// Call `SendSAS(FALSE)` from sas.dll (works from a service).
pub fn send_sas() {
    unsafe {
        let lib = LoadLibraryW(wide("sas.dll").as_ptr());
        if lib.is_null() {
            return;
        }
        if let Some(f) = GetProcAddress(lib, c"SendSAS".as_ptr() as *const u8) {
            let f: unsafe extern "system" fn(i32) = std::mem::transmute(f);
            f(0);
        }
    }
}

unsafe extern "system" fn enum_monitor(hmon: HMONITOR, _hdc: HDC, _rc: *mut RECT, data: LPARAM) -> BOOL {
    unsafe {
        let list = &mut *(data as *mut Vec<MonitorInfo>);
        let mut info: MONITORINFOEXW = zeroed();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(hmon, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) != 0 {
            let r = info.monitorInfo.rcMonitor;
            let len = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
            list.push(MonitorInfo {
                index: 0,
                name: String::from_utf16_lossy(&info.szDevice[..len]).trim_start_matches("\\\\.\\").to_string(),
                x: r.left,
                y: r.top,
                width: (r.right - r.left).max(1) as u32,
                height: (r.bottom - r.top).max(1) as u32,
                primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            });
        }
        1
    }
}

impl Desktop for WinDesktop {
    fn monitors(&mut self) -> Result<Vec<MonitorInfo>> {
        self.follow_input_desktop();
        let mut list: Vec<MonitorInfo> = Vec::new();
        unsafe {
            EnumDisplayMonitors(null_mut(), null(), Some(enum_monitor), &mut list as *mut _ as LPARAM);
        }
        if list.is_empty() {
            bail!("no monitors found");
        }
        list.sort_by_key(|m| (m.x, m.y));
        for (i, m) in list.iter_mut().enumerate() {
            m.index = i as u8;
        }
        Ok(list)
    }

    fn capture(&mut self, m: &MonitorInfo) -> Result<Frame> {
        self.follow_input_desktop();
        let (w, h) = (m.width as i32, m.height as i32);
        unsafe {
            let screen = GetDC(null_mut());
            if screen.is_null() {
                bail!("cannot read the screen");
            }
            let mem = CreateCompatibleDC(screen);
            let bmp = CreateCompatibleBitmap(screen, w, h);
            let old = SelectObject(mem, bmp as HGDIOBJ);
            BitBlt(mem, 0, 0, w, h, screen, m.x, m.y, SRCCOPY | CAPTUREBLT);

            // Mouse pointer.
            let mut ci: CURSORINFO = zeroed();
            ci.cbSize = size_of::<CURSORINFO>() as u32;
            if GetCursorInfo(&mut ci) != 0 && ci.flags & CURSOR_SHOWING != 0 {
                let mut ii: ICONINFO = zeroed();
                if GetIconInfo(ci.hCursor, &mut ii) != 0 {
                    let x = ci.ptScreenPos.x - ii.xHotspot as i32 - m.x;
                    let y = ci.ptScreenPos.y - ii.yHotspot as i32 - m.y;
                    DrawIconEx(mem, x, y, ci.hCursor, 0, 0, 0, null_mut(), DI_NORMAL);
                    if !ii.hbmMask.is_null() {
                        DeleteObject(ii.hbmMask as HGDIOBJ);
                    }
                    if !ii.hbmColor.is_null() {
                        DeleteObject(ii.hbmColor as HGDIOBJ);
                    }
                }
            }

            let mut bmi: BITMAPINFO = zeroed();
            bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h; // top-down rows
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;
            let mut buf = vec![0u8; (w * h * 4) as usize];
            let lines = GetDIBits(mem, bmp, 0, h as u32, buf.as_mut_ptr() as *mut _, &mut bmi, DIB_RGB_COLORS);
            SelectObject(mem, old);
            DeleteObject(bmp as HGDIOBJ);
            DeleteDC(mem);
            ReleaseDC(null_mut(), screen);
            if lines == 0 {
                bail!("screen capture failed");
            }
            Ok(Frame::from_bgra(m.width, m.height, &buf, w as usize * 4))
        }
    }

    fn input(&mut self, monitors: &[MonitorInfo], ev: &InputEvent) -> Result<()> {
        self.follow_input_desktop();
        match ev {
            InputEvent::MouseMove { mon, x, y } => {
                if let Some(i) = Self::move_input(monitors, *mon, *x, *y) {
                    self.send(&[i]);
                }
            }
            InputEvent::MouseButton { mon, x, y, button, down } => {
                let flag = match (button, down) {
                    (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
                    (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
                    (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                    (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
                    (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                    (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
                };
                let mut v = Vec::new();
                if let Some(i) = Self::move_input(monitors, *mon, *x, *y) {
                    v.push(i);
                }
                v.push(Self::mouse(flag, 0, 0, 0));
                self.send(&v);
            }
            InputEvent::Scroll { dx, dy } => {
                if *dy != 0.0 {
                    self.send(&[Self::mouse(MOUSEEVENTF_WHEEL, 0, 0, (dy * 120.0) as i32)]);
                }
                if *dx != 0.0 {
                    self.send(&[Self::mouse(MOUSEEVENTF_HWHEEL, 0, 0, (-dx * 120.0) as i32)]);
                }
            }
            InputEvent::Key { key, down } => self.named_key(key, *down),
            InputEvent::Text { text } => {
                let mut v = Vec::new();
                for unit in text.encode_utf16().take(1024) {
                    v.push(Self::key(0, unit, KEYEVENTF_UNICODE));
                    v.push(Self::key(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
                }
                self.send(&v);
            }
            InputEvent::CtrlAltDel => self.ctrl_alt_del(),
        }
        Ok(())
    }
}

impl Drop for WinDesktop {
    fn drop(&mut self) {
        if !self.desk.is_null() {
            unsafe {
                CloseDesktop(self.desk);
            }
        }
    }
}
