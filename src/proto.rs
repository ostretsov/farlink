//! Messages exchanged between viewer, relay server and host.
//!
//! Control messages are JSON text frames (easy for the browser viewer too).
//! Screen pictures are binary frames: a 20-byte [`TileHeader`] followed by a
//! JPEG or PNG image that browsers and the desktop viewer decode directly.

use crate::quality::Quality;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

/// One physical monitor on the remote machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub index: u8,
    pub name: String,
    /// Position in the remote virtual desktop (can be negative on Windows).
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

impl MonitorInfo {
    pub fn label(&self) -> String {
        format!("Screen {} · {}×{}", self.index + 1, self.width, self.height)
    }
}

/// Short monitor fingerprint so remembered window positions are only reused
/// for the same monitor setup.
pub fn monitor_fingerprint(m: &MonitorInfo) -> String {
    format!("{}:{}x{}@{},{}", m.index, m.width, m.height, m.x, m.y)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Something the viewer's person did with mouse or keyboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum InputEvent {
    /// `x`, `y` are 0..1 inside monitor `mon`.
    MouseMove { mon: u8, x: f32, y: f32 },
    MouseButton { mon: u8, x: f32, y: f32, button: MouseButton, down: bool },
    /// Wheel steps; positive `dy` scrolls up.
    Scroll { dx: f32, dy: f32 },
    /// A named key (see [`crate::keys`]) pressed or released.
    Key { key: String, down: bool },
    /// Typed text (letters, digits, any language).
    Text { text: String },
    /// Ctrl+Alt+Del (Windows secure attention sequence).
    CtrlAltDel,
}

/// Messages inside one viewing session (viewer <-> host, through the relay).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum SessionMsg {
    /// host -> viewer, first message.
    Hello { hostname: String, os: String, version: String, monitors: Vec<MonitorInfo>, view_only: bool },
    /// host -> viewer when monitors were added/removed/re-arranged.
    Monitors { monitors: Vec<MonitorInfo> },
    /// host -> viewer, a short notice for the status bar.
    Notice { text: String },
    /// viewer -> host.
    Quality { quality: Quality },
    /// viewer -> host. Dropped by the relay for view-only sessions.
    Input { event: InputEvent },
    /// viewer -> host: picture `seq` fully received (flow control).
    Ack { seq: u32 },
    /// viewer -> host: send every tile again (e.g. after a window reopened).
    Refresh,
    /// relay -> viewer: the session ended (access withdrawn, expired, host left).
    Closed { reason: String },
    /// viewer -> host: only these monitors are on screen; the host stops
    /// capturing the others (saves CPU and bandwidth).
    Watch { monitors: Vec<u8> },
    /// viewer -> relay: switch between watching only and control.
    /// relay -> host: the (checked) new mode.
    SetViewOnly { view_only: bool },
    /// relay -> viewer: the current mode, and whether it may be changed
    /// (`locked` = a view-only temporary link).
    ViewMode { view_only: bool, locked: bool },

    // ---- file transfer (control sessions only) ----
    /// viewer -> host: list a folder. Empty path = starting places (drives / home).
    FsList { req: u32, path: String },
    /// host -> viewer.
    FsListing { req: u32, path: String, parent: Option<String>, entries: Vec<FsEntry>, error: Option<String> },
    /// viewer -> host: send me this file (binary [`FileChunk`]s follow).
    FsDownload { req: u32, path: String },
    /// host -> viewer: a download starts.
    FsBegin { req: u32, name: String, size: u64 },
    /// viewer -> host: a file of `size` bytes follows as [`FileChunk`]s.
    FsUpload { req: u32, dir: String, name: String, size: u64 },
    /// viewer -> host: make a folder.
    FsMkdir { req: u32, path: String },
    /// host -> viewer: an upload / mkdir finished (or a download failed).
    FsDone { req: u32, path: String, error: Option<String> },
}

/// One entry of a folder listing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Seconds since 1970.
    pub modified: u64,
}

impl SessionMsg {
    /// Messages that change or read the remote machine and so are blocked
    /// in view-only sessions.
    pub fn is_control(&self) -> bool {
        matches!(
            self,
            SessionMsg::Input { .. }
                | SessionMsg::FsList { .. }
                | SessionMsg::FsDownload { .. }
                | SessionMsg::FsUpload { .. }
                | SessionMsg::FsMkdir { .. }
        )
    }
}

impl SessionMsg {
    /// File requests from the viewer (handled by [`crate::files`]).
    pub fn is_file_request(&self) -> bool {
        matches!(self, SessionMsg::FsList { .. } | SessionMsg::FsDownload { .. } | SessionMsg::FsUpload { .. } | SessionMsg::FsMkdir { .. })
    }
}

pub const FILE_MAGIC: u8 = 0x46; // 'F'
pub const FILE_HEADER_LEN: usize = 6;

/// A piece of a file being transferred (binary message, both directions):
/// `0 'F' | 1..5 request id (LE) | 5 flags (1 = last piece) | data`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileChunk {
    pub req: u32,
    pub last: bool,
}

impl FileChunk {
    pub const SIZE: usize = 256 * 1024;

    pub fn encode(&self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(FILE_HEADER_LEN + data.len());
        out.push(FILE_MAGIC);
        out.extend_from_slice(&self.req.to_le_bytes());
        out.push(self.last as u8);
        out.extend_from_slice(data);
        out
    }

    pub fn decode(msg: &[u8]) -> Option<(FileChunk, &[u8])> {
        if msg.len() < FILE_HEADER_LEN || msg[0] != FILE_MAGIC {
            return None;
        }
        let req = u32::from_le_bytes([msg[1], msg[2], msg[3], msg[4]]);
        Some((FileChunk { req, last: msg[5] & 1 != 0 }, &msg[FILE_HEADER_LEN..]))
    }
}

/// Messages on the host's permanent control connection to the relay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum AgentMsg {
    /// host -> relay. First time a host uses `enroll_token`; afterwards it
    /// presents the `machine_id` + `secret` it was given.
    Register {
        enroll_token: Option<String>,
        machine_id: Option<String>,
        secret: Option<String>,
        hostname: String,
        os: String,
        version: String,
        target: String,
        monitors: Vec<MonitorInfo>,
    },
    /// relay -> host after a successful register.
    Registered { machine_id: String, secret: String, name: String },
    /// relay -> host: refused (bad token, machine removed).
    Rejected { reason: String },
    /// relay -> host: someone wants to see this machine; open a session socket.
    OpenSession { session_id: String, view_only: bool },
    /// relay -> host: a newer version is published, update now. `force`
    /// (the administrator pressed "Update"): install the published version
    /// even if it is not newer, and report a failure with
    /// [`AgentMsg::UpdateFailed`].
    UpdateAvailable {
        version: String,
        #[serde(default)]
        force: bool,
    },
    /// host -> relay: installing an update did not work (a successful update
    /// restarts the helper, which then registers with its new version).
    UpdateFailed { version: String, error: String },
    /// host -> relay: monitors changed / still alive.
    Heartbeat { monitors: Vec<MonitorInfo> },
    /// relay -> host: send small pictures of every screen (the preview of a
    /// computer selected in the list).
    PreviewRequest { req: u32 },
    /// host -> relay: the answer to [`AgentMsg::PreviewRequest`].
    Preview { req: u32, shots: Vec<PreviewShot> },
}

/// Longest side of a preview picture.
pub const PREVIEW_MAX: u32 = 480;

/// A small JPEG of one monitor, for the preview of a selected computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewShot {
    pub index: u8,
    pub width: u32,
    pub height: u32,
    /// Base64 JPEG.
    pub jpeg: String,
}

/// `GET /api/machines/{id}/preview` answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preview {
    /// Seconds since 1970 when the pictures were taken.
    pub taken: u64,
    pub shots: Vec<PreviewShot>,
}

/// Picture compression of one tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileFormat {
    Jpeg = 0,
    Png = 1,
}

pub const TILE_MAGIC: u8 = 0x54; // 'T'
pub const TILE_HEADER_LEN: usize = 20;
/// Flag: last tile of picture `seq`.
pub const FLAG_LAST: u8 = 1;

/// Header in front of every binary picture message (little endian).
///
/// ```text
/// 0 magic 'T' | 1 monitor | 2 format | 3 flags | 4..6 frame width | 6..8 frame height
/// 8..10 x | 10..12 y | 12..14 w | 14..16 h | 16..20 seq
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileHeader {
    pub monitor: u8,
    pub format: TileFormat,
    pub flags: u8,
    /// Size of the whole (scaled) monitor picture this tile belongs to.
    pub frame_w: u16,
    pub frame_h: u16,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub seq: u32,
}

impl TileHeader {
    pub fn is_last(&self) -> bool {
        self.flags & FLAG_LAST != 0
    }

    pub fn encode(&self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(TILE_HEADER_LEN + data.len());
        out.push(TILE_MAGIC);
        out.push(self.monitor);
        out.push(self.format as u8);
        out.push(self.flags);
        for v in [self.frame_w, self.frame_h, self.x, self.y, self.w, self.h] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    /// Split a binary message into header and image bytes.
    pub fn decode(msg: &[u8]) -> Option<(TileHeader, &[u8])> {
        if msg.len() < TILE_HEADER_LEN || msg[0] != TILE_MAGIC {
            return None;
        }
        let u16_at = |i: usize| u16::from_le_bytes([msg[i], msg[i + 1]]);
        let format = match msg[2] {
            0 => TileFormat::Jpeg,
            1 => TileFormat::Png,
            _ => return None,
        };
        let h = TileHeader {
            monitor: msg[1],
            format,
            flags: msg[3],
            frame_w: u16_at(4),
            frame_h: u16_at(6),
            x: u16_at(8),
            y: u16_at(10),
            w: u16_at(12),
            h: u16_at(14),
            seq: u32::from_le_bytes([msg[16], msg[17], msg[18], msg[19]]),
        };
        if h.w == 0 || h.h == 0 || h.x as u32 + h.w as u32 > h.frame_w as u32 || h.y as u32 + h.h as u32 > h.frame_h as u32 {
            return None;
        }
        Some((h, &msg[TILE_HEADER_LEN..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_header_round_trip() {
        let h = TileHeader {
            monitor: 1,
            format: TileFormat::Png,
            flags: FLAG_LAST,
            frame_w: 1920,
            frame_h: 1080,
            x: 64,
            y: 1016,
            w: 128,
            h: 64,
            seq: 123_456,
        };
        let msg = h.encode(b"hello");
        assert_eq!(msg.len(), TILE_HEADER_LEN + 5);
        let (back, data) = TileHeader::decode(&msg).unwrap();
        assert_eq!(back, h);
        assert!(back.is_last());
        assert_eq!(data, b"hello");
    }

    #[test]
    fn tile_header_rejects_garbage() {
        assert!(TileHeader::decode(b"short").is_none());
        let mut bad = TileHeader {
            monitor: 0,
            format: TileFormat::Jpeg,
            flags: 0,
            frame_w: 100,
            frame_h: 100,
            x: 90,
            y: 0,
            w: 20,
            h: 10,
            seq: 1,
        }
        .encode(&[]);
        assert!(TileHeader::decode(&bad).is_none(), "tile outside frame");
        bad[0] = b'X';
        assert!(TileHeader::decode(&bad).is_none());
    }

    #[test]
    fn json_messages_round_trip() {
        let msgs = vec![
            SessionMsg::Input { event: InputEvent::MouseButton { mon: 1, x: 0.5, y: 0.25, button: MouseButton::Left, down: true } },
            SessionMsg::Input { event: InputEvent::Key { key: "Enter".into(), down: false } },
            SessionMsg::Input { event: InputEvent::CtrlAltDel },
            SessionMsg::Ack { seq: 9 },
            SessionMsg::Refresh,
            SessionMsg::Closed { reason: "Access withdrawn".into() },
        ];
        for m in msgs {
            let s = serde_json::to_string(&m).unwrap();
            let back: SessionMsg = serde_json::from_str(&s).unwrap();
            assert_eq!(back, m);
        }
        let s = serde_json::to_string(&SessionMsg::Ack { seq: 3 }).unwrap();
        assert_eq!(s, r#"{"t":"ack","seq":3}"#, "browser viewer relies on this shape");
    }

    #[test]
    fn file_chunks_round_trip() {
        let c = FileChunk { req: 77, last: true };
        let msg = c.encode(b"abc");
        assert_eq!(FileChunk::decode(&msg), Some((c, &b"abc"[..])));
        assert!(FileChunk::decode(b"T12345").is_none(), "picture tiles are not file chunks");
        assert!(FileChunk::decode(b"F1").is_none());
    }

    #[test]
    fn only_input_is_control() {
        assert!(SessionMsg::FsList { req: 1, path: "/".into() }.is_control(), "view-only can't browse files");
        assert!(SessionMsg::FsUpload { req: 1, dir: "/".into(), name: "a".into(), size: 1 }.is_control());
        assert!(!SessionMsg::Watch { monitors: vec![0] }.is_control());
        assert!(SessionMsg::Input { event: InputEvent::Text { text: "x".into() } }.is_control());
        assert!(!SessionMsg::Refresh.is_control());
        assert!(!SessionMsg::Quality { quality: Quality::default() }.is_control());
    }

    #[test]
    fn preview_messages_round_trip() {
        let shot = PreviewShot { index: 1, width: 480, height: 270, jpeg: "/9j/".into() };
        let msgs = [AgentMsg::PreviewRequest { req: 5 }, AgentMsg::Preview { req: 5, shots: vec![shot.clone()] }];
        for m in msgs {
            let s = serde_json::to_string(&m).unwrap();
            assert_eq!(serde_json::from_str::<AgentMsg>(&s).unwrap(), m);
        }
        assert_eq!(serde_json::to_string(&AgentMsg::PreviewRequest { req: 5 }).unwrap(), r#"{"t":"preview_request","req":5}"#);
        let p = Preview { taken: 1, shots: vec![shot] };
        assert_eq!(serde_json::from_str::<Preview>(&serde_json::to_string(&p).unwrap()).unwrap(), p);
    }

    #[test]
    fn agent_register_json() {
        let m = AgentMsg::Register {
            enroll_token: Some("abc".into()),
            machine_id: None,
            secret: None,
            hostname: "pc".into(),
            os: "Windows".into(),
            version: "0.1.0".into(),
            target: "x86_64-pc-windows-gnu".into(),
            monitors: vec![],
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.starts_with(r#"{"t":"register""#));
        assert_eq!(serde_json::from_str::<AgentMsg>(&s).unwrap(), m);
    }
}
