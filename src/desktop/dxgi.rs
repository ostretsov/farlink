//! Windows 8+ screen capture with DXGI Desktop Duplication (what RustDesk
//! and the Windows remote tools use). Compared with GDI `BitBlt` it is
//! cheap, comes from the graphics card, and tells us *whether* and *where*
//! the screen changed, so a still screen costs almost nothing.
//!
//! One [`Output`] per monitor. [`Output::poll`] takes the newest picture (if
//! any) and copies only the changed parts into an RGBA [`Frame`]. The mouse
//! pointer is not part of these pictures; `windows.rs` draws it.
//!
//! Anything unusual (turned screens, other pixel formats, the desktop
//! switching to the lock screen) returns an error, and the caller falls back
//! to GDI and tries again later.

use crate::codec::{Frame, Rect};
use anyhow::{Result, bail};
use std::mem::size_of;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

/// One duplicated monitor.
pub struct Output {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    ctx: ID3D11DeviceContext,
    device: ID3D11Device,
    dup: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    /// Newest picture; `None` until the first one arrived.
    pub frame: Option<Frame>,
}

/// All monitors that can be duplicated.
pub struct Duplicator {
    pub outputs: Vec<Output>,
}

impl Duplicator {
    pub fn new() -> Result<Duplicator> {
        let mut outputs = Vec::new();
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let mut a = 0;
            while let Ok(adapter) = factory.EnumAdapters1(a) {
                a += 1;
                let mut device: Option<(ID3D11Device, ID3D11DeviceContext)> = None;
                let mut o = 0;
                while let Ok(output) = adapter.EnumOutputs(o) {
                    o += 1;
                    let Ok(desc) = output.GetDesc() else { continue };
                    if !desc.AttachedToDesktop.as_bool() {
                        continue;
                    }
                    // Turned screens come unrotated; GDI handles those.
                    if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY && desc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED {
                        continue;
                    }
                    if device.is_none() {
                        let base: IDXGIAdapter = adapter.cast()?;
                        let mut dev = None;
                        let mut ctx = None;
                        D3D11CreateDevice(
                            &base,
                            D3D_DRIVER_TYPE_UNKNOWN,
                            HMODULE::default(),
                            D3D11_CREATE_DEVICE_FLAG(0),
                            None,
                            D3D11_SDK_VERSION,
                            Some(&mut dev),
                            None,
                            Some(&mut ctx),
                        )?;
                        match (dev, ctx) {
                            (Some(d), Some(c)) => device = Some((d, c)),
                            _ => break,
                        }
                    }
                    let Some((dev, ctx)) = device.clone() else { break };
                    let output1: IDXGIOutput1 = match output.cast() {
                        Ok(o) => o,
                        Err(_) => continue,
                    };
                    let dup = match output1.DuplicateOutput(&dev) {
                        Ok(d) => d,
                        Err(e) => {
                            log::debug!("duplicate output: {e}");
                            continue;
                        }
                    };
                    let r = desc.DesktopCoordinates;
                    outputs.push(Output {
                        x: r.left,
                        y: r.top,
                        width: (r.right - r.left).max(1) as u32,
                        height: (r.bottom - r.top).max(1) as u32,
                        ctx,
                        device: dev,
                        dup,
                        staging: None,
                        frame: None,
                    });
                }
            }
        }
        if outputs.is_empty() {
            bail!("no screen can be duplicated");
        }
        Ok(Duplicator { outputs })
    }
}

impl Output {
    /// Take the newest picture if there is one. `Ok(true)`: the picture or
    /// the pointer changed. An error means the duplication is gone (desktop
    /// switch, resolution change, ...) and must be created again.
    pub fn poll(&mut self) -> windows::core::Result<bool> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            match self.dup.AcquireNextFrame(0, &mut info, &mut res) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
                Err(e) => return Err(e),
            }
            let result = self.take(&info, res);
            let _ = self.dup.ReleaseFrame();
            result
        }
    }

    unsafe fn take(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO, res: Option<IDXGIResource>) -> windows::core::Result<bool> {
        // Only the pointer moved.
        if info.LastPresentTime == 0 && self.frame.is_some() {
            return Ok(info.LastMouseUpdateTime != 0);
        }
        let Some(res) = res else { return Ok(false) };
        let tex: ID3D11Texture2D = res.cast()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(windows::core::Error::new(DXGI_ERROR_UNSUPPORTED, "screen pixel format"));
        }
        let (w, h) = (desc.Width, desc.Height);
        let same_size = self.frame.as_ref().is_some_and(|f| f.width == w && f.height == h);
        let regions = if same_size { unsafe { self.changed_regions(info, w, h) } } else { None }
            .unwrap_or_else(|| vec![Rect { x: 0, y: 0, w, h }]);
        if regions.is_empty() {
            return Ok(info.LastMouseUpdateTime != 0);
        }

        if self.staging.as_ref().is_none_or(|s| {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            unsafe { s.GetDesc(&mut d) };
            d.Width != w || d.Height != h
        }) {
            let sd = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging = None;
            unsafe { self.device.CreateTexture2D(&sd, None, Some(&mut staging))? };
            self.staging = staging;
        }
        let Some(staging) = self.staging.clone() else { return Ok(false) };

        unsafe {
            self.ctx.CopyResource(&staging, &tex);
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
            if !same_size {
                self.frame = Some(Frame::new(w, h));
            }
            let frame = self.frame.as_mut().unwrap();
            let pitch = map.RowPitch as usize;
            let src = std::slice::from_raw_parts(map.pData as *const u8, pitch * h as usize);
            for r in &regions {
                for y in r.y as usize..(r.y + r.h) as usize {
                    let s = &src[y * pitch + r.x as usize * 4..][..r.w as usize * 4];
                    let d = &mut frame.rgba[(y * w as usize + r.x as usize) * 4..][..r.w as usize * 4];
                    for (d, s) in d.chunks_exact_mut(4).zip(s.chunks_exact(4)) {
                        d[0] = s[2];
                        d[1] = s[1];
                        d[2] = s[0];
                        d[3] = 255;
                    }
                }
            }
            self.ctx.Unmap(&staging, 0);
        }
        Ok(true)
    }

    /// Parts of the picture that changed in this frame (moved + redrawn),
    /// clipped to the screen. `None`: unknown, copy everything.
    unsafe fn changed_regions(&self, info: &DXGI_OUTDUPL_FRAME_INFO, w: u32, h: u32) -> Option<Vec<Rect>> {
        let size = info.TotalMetadataBufferSize as usize;
        if size == 0 {
            return None;
        }
        let mut need = 0u32;
        let mut moves = vec![DXGI_OUTDUPL_MOVE_RECT::default(); size / size_of::<DXGI_OUTDUPL_MOVE_RECT>() + 1];
        let bytes = (moves.len() * size_of::<DXGI_OUTDUPL_MOVE_RECT>()) as u32;
        unsafe { self.dup.GetFrameMoveRects(bytes, moves.as_mut_ptr(), &mut need).ok()? };
        moves.truncate(need as usize / size_of::<DXGI_OUTDUPL_MOVE_RECT>());
        let mut dirty = vec![RECT::default(); size / size_of::<RECT>() + 1];
        let bytes = (dirty.len() * size_of::<RECT>()) as u32;
        unsafe { self.dup.GetFrameDirtyRects(bytes, dirty.as_mut_ptr(), &mut need).ok()? };
        dirty.truncate(need as usize / size_of::<RECT>());

        let clip = |r: &RECT| {
            let x0 = r.left.clamp(0, w as i32) as u32;
            let y0 = r.top.clamp(0, h as i32) as u32;
            let x1 = r.right.clamp(0, w as i32) as u32;
            let y1 = r.bottom.clamp(0, h as i32) as u32;
            (x1 > x0 && y1 > y0).then(|| Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
        };
        Some(moves.iter().map(|m| &m.DestinationRect).chain(dirty.iter()).filter_map(clip).collect())
    }
}
