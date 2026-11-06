//! File transfer on the remote computer: list folders, send files to the
//! viewer, receive uploaded files. Only for control sessions (never view-only).

use crate::proto::{FileChunk, FsEntry, SessionMsg};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// Largest upload accepted (20 GB).
pub const MAX_UPLOAD: u64 = 20 * 1024 * 1024 * 1024;

fn modified_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs())
}

/// Starting places: drives on Windows, home and `/` elsewhere.
pub fn roots() -> Vec<FsEntry> {
    let mut v = Vec::new();
    if cfg!(windows) {
        for letter in b'A'..=b'Z' {
            let p = format!("{}:\\", letter as char);
            if Path::new(&p).exists() {
                v.push(FsEntry { name: p.clone(), path: p, is_dir: true, size: 0, modified: 0 });
            }
        }
    } else {
        if let Some(h) = dirs::home_dir() {
            v.push(FsEntry { name: format!("Home ({})", h.display()), path: h.display().to_string(), is_dir: true, size: 0, modified: 0 });
        }
        v.push(FsEntry { name: "/".into(), path: "/".into(), is_dir: true, size: 0, modified: 0 });
    }
    v
}

/// Read a folder: folders first, then files, both alphabetical.
pub fn read_dir(path: &Path) -> std::io::Result<Vec<FsEntry>> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(path)? {
        let Ok(e) = e else { continue };
        let Ok(meta) = e.metadata() else { continue };
        v.push(FsEntry {
            name: e.file_name().to_string_lossy().into_owned(),
            path: e.path().display().to_string(),
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: modified_secs(&meta),
        });
    }
    v.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(v)
}

/// The folder above `path`, or `Some("")` (= starting places) at a root.
pub fn parent_of(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    Some(Path::new(path).parent().map(|p| p.display().to_string()).unwrap_or_default())
}

pub fn listing(req: u32, path: &str) -> SessionMsg {
    if path.is_empty() {
        return SessionMsg::FsListing { req, path: String::new(), parent: None, entries: roots(), error: None };
    }
    match read_dir(Path::new(path)) {
        Ok(entries) => SessionMsg::FsListing { req, path: path.into(), parent: parent_of(path), entries, error: None },
        Err(e) => SessionMsg::FsListing { req, path: path.into(), parent: parent_of(path), entries: vec![], error: Some(e.to_string()) },
    }
}

/// A plain file name (no folders, no `..`).
pub fn safe_name(name: &str) -> Option<String> {
    let n = name.trim();
    let bad = n.is_empty() || n == "." || n == ".." || n.contains(['/', '\\', '\0']) || (cfg!(windows) && n.contains(':'));
    (!bad).then(|| n.to_string())
}

/// `dir/name`, or `dir/name (2).ext` etc. if that already exists — never overwrites.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let p = Path::new(name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| name.to_string());
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..).map(|i| dir.join(format!("{stem} ({i}){ext}"))).find(|c| !c.exists()).unwrap()
}

struct Upload {
    file: std::fs::File,
    path: PathBuf,
    remaining: u64,
}

/// Per-session file transfer state on the remote computer.
#[derive(Default)]
pub struct FileService {
    uploads: HashMap<u32, Upload>,
}

fn json(m: &SessionMsg) -> Message {
    Message::Text(serde_json::to_string(m).unwrap().into())
}

impl FileService {
    /// Handle a file message. Returns false if it isn't one.
    pub async fn handle(&mut self, m: SessionMsg, out: &mpsc::Sender<Message>) -> bool {
        match m {
            SessionMsg::FsList { req, path } => {
                let reply = tokio::task::spawn_blocking(move || listing(req, &path)).await.unwrap();
                let _ = out.send(json(&reply)).await;
            }
            SessionMsg::FsMkdir { req, path } => {
                let error = std::fs::create_dir_all(&path).err().map(|e| e.to_string());
                let _ = out.send(json(&SessionMsg::FsDone { req, path, error })).await;
            }
            SessionMsg::FsDownload { req, path } => {
                let out = out.clone();
                tokio::spawn(send_file(req, PathBuf::from(path), out));
            }
            SessionMsg::FsUpload { req, dir, name, size } => {
                let reply = match self.start_upload(req, &dir, &name, size) {
                    Ok(()) => None,
                    Err(e) => Some(SessionMsg::FsDone { req, path: dir, error: Some(e) }),
                };
                if let Some(r) = reply {
                    let _ = out.send(json(&r)).await;
                } else if size == 0 {
                    // Empty file: nothing will follow.
                    if let Some(r) = self.chunk(FileChunk { req, last: true }, &[]) {
                        let _ = out.send(json(&r)).await;
                    }
                }
            }
            _ => return false,
        }
        true
    }

    fn start_upload(&mut self, req: u32, dir: &str, name: &str, size: u64) -> Result<(), String> {
        let name = safe_name(name).ok_or("not a valid file name")?;
        if size > MAX_UPLOAD {
            return Err("file is too large".into());
        }
        let dir = Path::new(dir);
        if !dir.is_dir() {
            return Err("folder does not exist".into());
        }
        let path = unique_path(dir, &name);
        let file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
        self.uploads.insert(req, Upload { file, path, remaining: size });
        Ok(())
    }

    /// A piece of an uploaded file arrived. Returns the final answer when done.
    pub fn chunk(&mut self, c: FileChunk, data: &[u8]) -> Option<SessionMsg> {
        let up = self.uploads.get_mut(&c.req)?;
        let err = if data.len() as u64 > up.remaining {
            Some("more data than announced".to_string())
        } else {
            up.remaining -= data.len() as u64;
            up.file.write_all(data).err().map(|e| e.to_string())
        };
        if err.is_some() || c.last {
            let up = self.uploads.remove(&c.req)?;
            let error = err.or_else(|| (up.remaining != 0).then(|| "upload ended early".to_string()));
            if error.is_some() {
                drop(up.file);
                let _ = std::fs::remove_file(&up.path);
            }
            return Some(SessionMsg::FsDone { req: c.req, path: up.path.display().to_string(), error });
        }
        None
    }
}

/// Send a file to the viewer in pieces (the bounded channel paces it).
pub async fn send_file(req: u32, path: PathBuf, out: mpsc::Sender<Message>) {
    use tokio::io::AsyncReadExt;
    let fail = |e: String| SessionMsg::FsDone { req, path: path.display().to_string(), error: Some(e) };
    let mut f = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            let _ = out.send(json(&fail(e.to_string()))).await;
            return;
        }
    };
    let size = match f.metadata().await {
        Ok(m) if m.is_file() => m.len(),
        Ok(_) => {
            let _ = out.send(json(&fail("folders can't be downloaded, open them instead".into()))).await;
            return;
        }
        Err(e) => {
            let _ = out.send(json(&fail(e.to_string()))).await;
            return;
        }
    };
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    if out.send(json(&SessionMsg::FsBegin { req, name, size })).await.is_err() {
        return;
    }
    let mut sent = 0u64;
    let mut buf = vec![0u8; FileChunk::SIZE];
    loop {
        let n = match f.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                let _ = out.send(json(&fail(e.to_string()))).await;
                return;
            }
        };
        sent += n as u64;
        let last = n == 0 || sent >= size;
        let msg = FileChunk { req, last }.encode(&buf[..n]);
        if out.send(Message::Binary(msg.into())).await.is_err() || last {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_unique_paths() {
        assert_eq!(safe_name(" report.pdf "), Some("report.pdf".into()));
        assert!(safe_name("../etc/passwd").is_none());
        assert!(safe_name("..").is_none());
        assert!(safe_name("a\\b").is_none());
        let dir = crate::util::unique_temp_dir("farlink-files");
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        std::fs::write(dir.join("a (2).txt"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (3).txt"));
        assert_eq!(unique_path(&dir, "b.txt"), dir.join("b.txt"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn listing_sorts_folders_first_and_reports_errors() {
        let dir = crate::util::unique_temp_dir("farlink-files");
        std::fs::create_dir(dir.join("zeta")).unwrap();
        std::fs::write(dir.join("Alpha.txt"), b"hello").unwrap();
        let SessionMsg::FsListing { entries, parent, error, .. } = listing(1, &dir.display().to_string()) else { panic!() };
        assert!(error.is_none());
        assert!(parent.is_some());
        assert_eq!(entries[0].name, "zeta");
        assert!(entries[0].is_dir);
        assert_eq!((entries[1].name.as_str(), entries[1].size), ("Alpha.txt", 5));
        let SessionMsg::FsListing { error, .. } = listing(2, "/definitely/not/here") else { panic!() };
        assert!(error.is_some());
        let SessionMsg::FsListing { entries, parent, .. } = listing(3, "") else { panic!() };
        assert!(!entries.is_empty() && parent.is_none(), "starting places");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn upload_then_download_round_trip() {
        let dir = crate::util::unique_temp_dir("farlink-files");
        let mut svc = FileService::default();
        let (tx, mut rx) = mpsc::channel(64);
        let data: Vec<u8> = (0..600_000u32).map(|i| (i % 251) as u8).collect();
        let up = SessionMsg::FsUpload { req: 5, dir: dir.display().to_string(), name: "big.bin".into(), size: data.len() as u64 };
        assert!(svc.handle(up, &tx).await);
        let pieces: Vec<&[u8]> = data.chunks(FileChunk::SIZE).collect();
        let mut done = None;
        for (i, p) in pieces.iter().enumerate() {
            done = svc.chunk(FileChunk { req: 5, last: i + 1 == pieces.len() }, p);
        }
        let Some(SessionMsg::FsDone { path, error: None, .. }) = done else { panic!("{done:?}") };
        assert_eq!(std::fs::read(&path).unwrap(), data);

        // Download it back.
        assert!(svc.handle(SessionMsg::FsDownload { req: 6, path: path.clone() }, &tx).await);
        let mut got = Vec::new();
        loop {
            match rx.recv().await.unwrap() {
                Message::Text(t) => {
                    let m: SessionMsg = serde_json::from_str(&t).unwrap();
                    assert!(matches!(m, SessionMsg::FsBegin { req: 6, size: 600_000, .. }), "{m:?}");
                }
                Message::Binary(b) => {
                    let (c, d) = FileChunk::decode(&b).unwrap();
                    got.extend_from_slice(d);
                    if c.last {
                        break;
                    }
                }
                _ => {}
            }
        }
        assert_eq!(got, data);

        // A short upload is removed, not left half-written.
        let up = SessionMsg::FsUpload { req: 7, dir: dir.display().to_string(), name: "short.bin".into(), size: 10 };
        svc.handle(up, &tx).await;
        let Some(SessionMsg::FsDone { error: Some(_), path, .. }) = svc.chunk(FileChunk { req: 7, last: true }, b"abc") else {
            panic!()
        };
        assert!(!Path::new(&path).exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
