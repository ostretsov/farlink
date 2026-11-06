//! Windows 10/11 and Server 2016+: GDI capture of every monitor, SendInput for
//! mouse and keyboard. The thread follows the *input desktop*, so when the
//! helper runs as SYSTEM in the console session it also sees and controls the
//! lock screen, the sign-in screen and UAC prompts.
//!
//! Pictures come from DXGI Desktop Duplication (`dxgi.rs`: cheap, knows what
//! changed). Where that is not available (older systems, turned screens,
//! briefly after a desktop switch) GDI `BitBlt` is used instead.

use super::{Desktop, PointerImage, PointerPos, to_absolute};
use crate::codec::Frame;
use crate::keys;
use crate::proto::{InputEvent, MonitorInfo, MouseButton};
use super::dxgi::Duplicator;
use anyhow::{Result, bail};
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::time::{Duration, Instant};
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
    /// Name of the desktop we are on ("Default", "Winlogon", ...).
    desk_name: String,
    dxgi: Option<Duplicator>,
    /// When to try DXGI again after it failed.
    dxgi_retry: Instant,
    /// Reused GDI capture surfaces per monitor size.
    gdi: HashMap<(i32, i32), GdiSurface>,
    /// Monitors as last reported (to know which ones DXGI covers).
    monitors: Vec<MonitorInfo>,
    /// Pointer position + shape last time we looked.
    pointer: (i32, i32, usize, bool),
    /// Draw the pointer into pictures (off when the viewer draws it).
    pointer_in_picture: bool,
    /// Shape of the last `pointer()` call, and its picture once made.
    cursor: usize,
    cursor_image: Option<(usize, PointerImage)>,
}

impl WinDesktop {
    pub fn open() -> Result<WinDesktop> {
        unsafe {
            if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
                // Windows Server 2016 / older Windows 10.
                let _ = SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE);
            }
        }
        let mut d = WinDesktop {
            desk: null_mut(),
            desk_name: String::new(),
            dxgi: None,
            dxgi_retry: Instant::now(),
            gdi: HashMap::new(),
            monitors: Vec::new(),
            pointer: (0, 0, 0, false),
            pointer_in_picture: true,
            cursor: 0,
            cursor_image: None,
        };
        d.follow_input_desktop();
        Ok(d)
    }

    /// Switch this thread to whatever desktop currently receives input
    /// (normal desktop, Winlogon/lock screen, or the UAC secure desktop).
    /// Screen duplication belongs to one desktop, so it starts over on a switch.
    fn follow_input_desktop(&mut self) {
        unsafe {
            let desk = OpenInputDesktop(0, 0, GENERIC_ALL);
            if desk.is_null() {
                return;
            }
            let name = desktop_name(desk);
            if SetThreadDesktop(desk) != 0 {
                if !self.desk.is_null() {
                    CloseDesktop(self.desk);
                }
                self.desk = desk;
                if name != self.desk_name {
                    log::info!("desktop switched to {name:?}");
                    self.desk_name = name;
                    self.dxgi = None;
                    self.gdi.clear();
                    self.dxgi_retry = Instant::now();
                }
            } else {
                CloseDesktop(desk);
            }
        }
    }

    /// Start DXGI duplication when it is missing and a retry is due.
    fn ensure_dxgi(&mut self) {
        if self.dxgi.is_some() || Instant::now() < self.dxgi_retry {
            return;
        }
        match Duplicator::new() {
            Ok(d) => {
                log::info!("screen capture: DXGI desktop duplication ({} screens)", d.outputs.len());
                self.dxgi = Some(d);
            }
            Err(e) => {
                log::info!("screen capture: GDI (DXGI not available: {e:#})");
                self.dxgi_retry = Instant::now() + Duration::from_secs(5);
            }
        }
    }

    /// The DXGI output showing monitor `m`, if any.
    fn output_index(&self, m: &MonitorInfo) -> Option<usize> {
        self.dxgi.as_ref()?.outputs.iter().position(|o| o.x == m.x && o.y == m.y && o.width == m.width && o.height == m.height)
    }

    /// Cheap pointer check for `changed()`: position + shape. Uses the
    /// position even when Windows calls the pointer hidden, because we then
    /// draw an arrow there (see `pointer_now`).
    fn pointer_state() -> (i32, i32, usize, bool) {
        unsafe {
            let mut ci: CURSORINFO = zeroed();
            ci.cbSize = size_of::<CURSORINFO>() as u32;
            if GetCursorInfo(&mut ci) != 0 {
                return (ci.ptScreenPos.x, ci.ptScreenPos.y, ci.hCursor as usize, ci.flags & CURSOR_SHOWING != 0);
            }
            let mut pos: POINT = zeroed();
            if GetCursorPos(&mut pos) != 0 {
                return (pos.x, pos.y, 0, false);
            }
            (0, 0, 0, false)
        }
    }

    /// Classic GDI capture (slower; used when DXGI is not available).
    fn capture_gdi(&mut self, m: &MonitorInfo) -> Result<Frame> {
        self.follow_input_desktop();
        let (w, h) = (m.width as i32, m.height as i32);
        if !self.gdi.contains_key(&(w, h)) {
            self.gdi.insert((w, h), GdiSurface::new(w, h)?);
        }
        let surf = self.gdi.get_mut(&(w, h)).unwrap();
        unsafe {
            let screen = GetDC(null_mut());
            if screen.is_null() {
                bail!("cannot read the screen");
            }
            let ok = BitBlt(surf.dc, 0, 0, w, h, screen, m.x, m.y, SRCCOPY | CAPTUREBLT);
            ReleaseDC(null_mut(), screen);
            if ok == 0 {
                // Typically the desktop just switched; start over next time.
                self.gdi.clear();
                bail!("screen capture failed");
            }
            if self.pointer_in_picture
                && let Some(p) = pointer_now()
            {
                draw_pointer_on(surf.dc, &p, m.x, m.y);
            }
        }
        Ok(Frame::from_bgra(m.width, m.height, surf.pixels(), w as usize * 4))
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

/// Name of a desktop ("Default", "Winlogon", "Screen-saver").
unsafe fn desktop_name(desk: HDESK) -> String {
    let mut buf = [0u16; 128];
    let mut need = 0u32;
    let ok = unsafe { GetUserObjectInformationW(desk as HANDLE, UOI_NAME, buf.as_mut_ptr() as *mut _, (buf.len() * 2) as u32, &mut need) };
    if ok == 0 {
        return String::new();
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// A memory picture GDI can draw into, with its pixels directly readable
/// (no `GetDIBits` copy). Kept between pictures.
struct GdiSurface {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl GdiSurface {
    fn new(w: i32, h: i32) -> Result<GdiSurface> {
        unsafe {
            let dc = CreateCompatibleDC(null_mut());
            if dc.is_null() {
                bail!("cannot create a drawing surface");
            }
            let mut bmi: BITMAPINFO = zeroed();
            bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h; // top-down rows
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;
            let mut bits: *mut core::ffi::c_void = null_mut();
            let bmp = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if bmp.is_null() || bits.is_null() {
                DeleteDC(dc);
                bail!("cannot create a {w}x{h} picture");
            }
            let old = SelectObject(dc, bmp as HGDIOBJ);
            Ok(GdiSurface { dc, bmp, old, bits: bits as *mut u8, w, h })
        }
    }

    /// BGRX pixels, `w * 4` bytes per row.
    fn pixels(&mut self) -> &mut [u8] {
        unsafe {
            GdiFlush();
            std::slice::from_raw_parts_mut(self.bits, (self.w * self.h * 4) as usize)
        }
    }
}

impl Drop for GdiSurface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bmp as HGDIOBJ);
            DeleteDC(self.dc);
        }
    }
}

/// The mouse pointer as it should appear in the picture.
struct Pointer {
    /// Screen position of the pointer's top-left corner (hot spot subtracted).
    left: i32,
    top: i32,
    w: i32,
    h: i32,
    /// Hot spot inside the pointer picture.
    hot_x: i32,
    hot_y: i32,
    cursor: HCURSOR,
}

/// Where the pointer is and what it looks like right now.
///
/// Windows often reports the pointer as hidden while it is really there for
/// us: no physical mouse plugged in (Windows then hides it, and our injected
/// input does not bring it back), touch/pen input (`CURSOR_SUPPRESSED`), or a
/// shape we may not read from the SYSTEM helper. The viewer hides its own
/// pointer over the picture, so in all those cases we draw the normal arrow
/// at the real position instead of nothing.
unsafe fn pointer_now() -> Option<Pointer> {
    unsafe {
        let mut ci: CURSORINFO = zeroed();
        ci.cbSize = size_of::<CURSORINFO>() as u32;
        let info_ok = GetCursorInfo(&mut ci) != 0;
        let mut pos = ci.ptScreenPos;
        if !info_ok && GetCursorPos(&mut pos) == 0 {
            return None;
        }
        let mut cursor = ci.hCursor;
        if !info_ok || ci.flags & CURSOR_SHOWING == 0 || cursor.is_null() {
            note_pointer_fallback(info_ok, ci.flags);
            cursor = LoadCursorW(null_mut(), IDC_ARROW);
        }
        let mut ii: ICONINFO = zeroed();
        if GetIconInfo(cursor, &mut ii) == 0 {
            let arrow = LoadCursorW(null_mut(), IDC_ARROW);
            if arrow == cursor || GetIconInfo(arrow, &mut ii) == 0 {
                return None;
            }
            cursor = arrow;
        }
        let mut bm: BITMAP = zeroed();
        let (w, h) = if !ii.hbmColor.is_null() && GetObjectW(ii.hbmColor as HGDIOBJ, size_of::<BITMAP>() as i32, &mut bm as *mut _ as *mut _) != 0 {
            (bm.bmWidth, bm.bmHeight)
        } else if !ii.hbmMask.is_null() && GetObjectW(ii.hbmMask as HGDIOBJ, size_of::<BITMAP>() as i32, &mut bm as *mut _ as *mut _) != 0 {
            (bm.bmWidth, bm.bmHeight / 2) // AND mask on top of XOR mask
        } else {
            (32, 32)
        };
        free_icon_info(&ii);
        Some(Pointer {
            left: pos.x - ii.xHotspot as i32,
            top: pos.y - ii.yHotspot as i32,
            w: w.clamp(1, 256),
            h: h.clamp(1, 256),
            hot_x: ii.xHotspot as i32,
            hot_y: ii.yHotspot as i32,
            cursor,
        })
    }
}

/// Log once why the real pointer could not be used.
fn note_pointer_fallback(info_ok: bool, flags: u32) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static NOTED: AtomicBool = AtomicBool::new(false);
    if !NOTED.swap(true, Ordering::Relaxed) {
        if info_ok {
            log::info!("Windows reports the mouse pointer hidden (flags {flags:#x}); drawing an arrow instead");
        } else {
            log::info!("cannot read the mouse pointer ({}); drawing an arrow instead", std::io::Error::last_os_error());
        }
    }
}

/// Draw the pointer onto `dc`, whose (0,0) is screen point (`ox`, `oy`).
/// Windows handles every pointer kind (colour, black/white, inverting) itself.
unsafe fn draw_pointer_on(dc: HDC, p: &Pointer, ox: i32, oy: i32) {
    unsafe {
        DrawIconEx(dc, p.left - ox, p.top - oy, p.cursor, 0, 0, 0, null_mut(), DI_NORMAL);
    }
}

/// The pointer as a picture of its own, for viewers that draw it. Windows
/// draws it once on black and once on white: the difference gives the
/// transparency. Inverting parts (the text I-beam) become black with a white
/// edge, so they show on any background.
fn cursor_picture(p: &Pointer) -> Option<PointerImage> {
    let (w, h) = (p.w, p.h);
    let mut black = GdiSurface::new(w, h).ok()?;
    let mut white = GdiSurface::new(w, h).ok()?;
    black.pixels().fill(0);
    white.pixels().fill(255);
    unsafe {
        DrawIconEx(black.dc, 0, 0, p.cursor, 0, 0, 0, null_mut(), DI_NORMAL);
        DrawIconEx(white.dc, 0, 0, p.cursor, 0, 0, 0, null_mut(), DI_NORMAL);
    }
    let (b, wh) = (black.pixels().to_vec(), white.pixels().to_vec());
    let n = (w * h) as usize;
    let mut rgba = vec![0u8; n * 4];
    let mut inverted = vec![false; n];
    for i in 0..n {
        let (bp, wp) = (&b[i * 4..i * 4 + 3], &wh[i * 4..i * 4 + 3]);
        if (0..3).any(|c| wp[c] < bp[c]) {
            inverted[i] = true;
            rgba[i * 4..i * 4 + 4].copy_from_slice(&[0, 0, 0, 255]);
            continue;
        }
        // On black: colour * alpha. On white: that + (255 - alpha) * 255 / 255.
        let a = 255 - (wp[1] as i32 - bp[1] as i32).clamp(0, 255);
        if a == 0 {
            continue;
        }
        let un = |v: u8| (v as i32 * 255 / a).min(255) as u8;
        rgba[i * 4..i * 4 + 4].copy_from_slice(&[un(bp[2]), un(bp[1]), un(bp[0]), a as u8]);
    }
    // White edge around inverting parts.
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) as usize;
            if rgba[i * 4 + 3] != 0 {
                continue;
            }
            let near = [(-1, 0), (1, 0), (0, -1), (0, 1)].iter().any(|(dx, dy)| {
                let (nx, ny) = (x + dx, y + dy);
                nx >= 0 && ny >= 0 && nx < w && ny < h && inverted[(ny * w + nx) as usize]
            });
            if near {
                rgba[i * 4..i * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    Some(PointerImage {
        width: w as u32,
        height: h as u32,
        hot_x: p.hot_x.clamp(0, w - 1) as u32,
        hot_y: p.hot_y.clamp(0, h - 1) as u32,
        rgba,
    })
}

unsafe fn free_icon_info(ii: &ICONINFO) {
    unsafe {
        if !ii.hbmMask.is_null() {
            DeleteObject(ii.hbmMask as HGDIOBJ);
        }
        if !ii.hbmColor.is_null() {
            DeleteObject(ii.hbmColor as HGDIOBJ);
        }
    }
}

/// Draw the pointer into a captured RGBA picture of monitor `m` (DXGI
/// pictures don't contain it).
fn draw_pointer(frame: &mut Frame, m: &MonitorInfo) {
    let Some(p) = (unsafe { pointer_now() }) else { return };
    let (left, top, w, h) = (p.left - m.x, p.top - m.y, p.w, p.h);
    let x0 = left.max(0);
    let y0 = top.max(0);
    let x1 = (left + w).min(frame.width as i32);
    let y1 = (top + h).min(frame.height as i32);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let (rw, rh) = (x1 - x0, y1 - y0);
    let Ok(mut surf) = GdiSurface::new(rw, rh) else { return };
    let fw = frame.width as usize;
    {
        let px = surf.pixels();
        for row in 0..rh as usize {
            let src = &frame.rgba[((y0 as usize + row) * fw + x0 as usize) * 4..][..rw as usize * 4];
            let dst = &mut px[row * rw as usize * 4..][..rw as usize * 4];
            for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = 0;
            }
        }
    }
    unsafe { draw_pointer_on(surf.dc, &p, m.x + x0, m.y + y0) };
    let px = surf.pixels();
    for row in 0..rh as usize {
        let src = &px[row * rw as usize * 4..][..rw as usize * 4];
        let dst = &mut frame.rgba[((y0 as usize + row) * fw + x0 as usize) * 4..][..rw as usize * 4];
        for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = 255;
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
        if list != self.monitors {
            // Screens were added, removed or changed size: duplicate them anew.
            self.dxgi = None;
            self.dxgi_retry = Instant::now();
            self.gdi.clear();
            self.monitors = list.clone();
        }
        Ok(list)
    }

    fn changed(&mut self) -> bool {
        self.follow_input_desktop();
        self.ensure_dxgi();
        let mut changed = false;
        if self.pointer_in_picture {
            let pointer = Self::pointer_state();
            changed = pointer != self.pointer;
            self.pointer = pointer;
        }
        let Some(d) = self.dxgi.as_mut() else { return true }; // GDI can't tell
        for o in &mut d.outputs {
            match o.poll() {
                Ok(c) => changed |= c,
                Err(e) => {
                    log::info!("screen duplication stopped ({e}); using GDI for now");
                    self.dxgi = None;
                    self.dxgi_retry = Instant::now() + Duration::from_millis(500);
                    return true;
                }
            }
        }
        // Monitors DXGI doesn't cover are read with GDI every time.
        changed || self.monitors.iter().any(|m| self.output_index(m).is_none())
    }

    fn capture(&mut self, m: &MonitorInfo) -> Result<Frame> {
        if let Some(i) = self.output_index(m)
            && let Some(f) = &self.dxgi.as_ref().unwrap().outputs[i].frame
            && f.width == m.width
            && f.height == m.height
        {
            let mut frame = f.clone();
            if self.pointer_in_picture {
                draw_pointer(&mut frame, m);
            }
            return Ok(frame);
        }
        self.capture_gdi(m)
    }

    fn set_pointer_in_picture(&mut self, on: bool) {
        self.pointer_in_picture = on;
    }

    fn pointer(&mut self) -> Option<PointerPos> {
        let p = unsafe { pointer_now() }?;
        self.cursor = p.cursor as usize;
        Some(PointerPos { x: p.left + p.hot_x, y: p.top + p.hot_y, shape: p.cursor as usize as u64 })
    }

    fn pointer_image(&mut self) -> Option<PointerImage> {
        if let Some((c, img)) = &self.cursor_image
            && *c == self.cursor
        {
            return Some(img.clone());
        }
        let p = unsafe { pointer_now() }?;
        if p.cursor as usize != self.cursor {
            return None; // changed in between; asked again next time
        }
        let img = cursor_picture(&p)?;
        self.cursor_image = Some((self.cursor, img.clone()));
        Some(img)
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
