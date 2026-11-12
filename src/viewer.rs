//! Desktop viewer (egui): machine list, details, quality, temporary links and
//! one native window per remote monitor, reopened where it was left.

use crate::config::{FilesSide, ScaleMode, ViewerConfig, load_json, save_json, viewer_dir};
use crate::layout::{LayoutStore, WindowPlace, default_place};
use crate::codec::Frame;
use crate::net::{Api, ConnState, SessionHandle, Target, TransferState, decode_preview, demo_preview, parse_link};
use crate::proto::{InputEvent, MonitorInfo, MouseButton, SessionMsg, monitor_fingerprint};
use crate::quality::{Codec, ColorDepth, Preset, Quality};
use crate::server::{MachineSummary, MachineView, Overview};
use crate::tray::{Tray, TrayEvent, TrayStatus};
use crate::util::{human_duration, normalize_server_url, now_secs};
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2,
    ViewportBuilder, ViewportCommand, ViewportId,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ colours

const BG: Color32 = Color32::from_rgb(15, 18, 22);
const PANEL: Color32 = Color32::from_rgb(23, 27, 33);
const PANEL2: Color32 = Color32::from_rgb(30, 35, 43);
const LINE: Color32 = Color32::from_rgb(42, 49, 59);
const TEXT: Color32 = Color32::from_rgb(230, 233, 238);
const MUTED: Color32 = Color32::from_rgb(139, 149, 163);
const ACCENT: Color32 = Color32::from_rgb(53, 196, 166);
const AMBER: Color32 = Color32::from_rgb(242, 184, 75);
const DANGER: Color32 = Color32::from_rgb(239, 107, 107);
const OK: Color32 = Color32::from_rgb(79, 209, 139);

// ------------------------------------------------------------------ options

#[derive(Debug, Clone, Default)]
pub struct ViewerOptions {
    /// Draw ~120 frames, save the main window as PNG here, exit.
    pub screenshot: Option<PathBuf>,
    /// Show sample computers (no server needed).
    pub demo: bool,
    /// Open this temporary link right away.
    pub open_link: Option<String>,
    /// Start hidden in the tray (`--tray`, used when starting at login).
    pub start_hidden: bool,
}

/// Set when the app really quits (tray "Quit", restart after an update):
/// closing the main window otherwise only hides it to the tray.
static QUITTING: AtomicBool = AtomicBool::new(false);

/// This program's file, read once at start. Later `current_exe` names the
/// moved-aside old file (`farlink.old`) or `(deleted)` once an update has
/// replaced it, and "Restart" must start the new one.
static EXE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

fn app_exe() -> std::io::Result<PathBuf> {
    EXE.get_or_init(|| std::env::current_exe().ok()).clone().ok_or_else(|| std::io::Error::other("program path unknown"))
}

pub fn run(opts: ViewerOptions) -> eframe::Result<()> {
    let _ = app_exe();
    let native = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title(format!("Farlink {}", crate::VERSION))
            .with_app_id("farlink") // X11 WM_CLASS / Wayland app id: matches farlink.desktop
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([820.0, 520.0])
            .with_icon(Arc::new(egui::IconData { rgba: crate::tray::icon::rgba(256), width: 256, height: 256 })),
        ..Default::default()
    };
    eframe::run_native("Farlink", native, Box::new(move |cc| Ok(Box::new(ViewerApp::new(&cc.egui_ctx, opts)))))
}

fn style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = BG;
    v.faint_bg_color = PANEL2;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT.linear_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    v.widgets.inactive.weak_bg_fill = PANEL2;
    v.widgets.inactive.bg_fill = PANEL2;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, LINE);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(38, 44, 54);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(44, 52, 64);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.noninteractive] {
        w.corner_radius = CornerRadius::same(7);
    }
    ctx.set_visuals(v);
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = Vec2::new(8.0, 7.0);
        s.spacing.button_padding = Vec2::new(11.0, 5.0);
        s.spacing.interact_size.y = 26.0;
    });
}

// ------------------------------------------------------------------ background jobs

type Job<T> = Arc<Mutex<Option<Result<T, String>>>>;

fn spawn_job<T: Send + 'static>(ctx: &egui::Context, f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Job<T> {
    let job: Job<T> = Arc::new(Mutex::new(None));
    let j = job.clone();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let r = f().map_err(|e| format!("{e:#}"));
        *j.lock().unwrap() = Some(r);
        ctx.request_repaint();
    });
    job
}

fn take_job<T>(job: &mut Option<Job<T>>) -> Option<Result<T, String>> {
    let r = job.as_ref()?.lock().unwrap().take();
    if r.is_some() {
        *job = None;
    }
    r
}

// ------------------------------------------------------------------ sessions

struct OpenSession {
    id: u64,
    handle: SessionHandle,
    /// Machine id (admin) — window positions are remembered under this key.
    machine_id: Option<String>,
    key: Option<String>,
    name: String,
    quality: Quality,
    textures: HashMap<u8, (egui::TextureHandle, u64)>,
    builders: HashMap<u8, ViewportBuilder>,
    closed: HashSet<u8>,
    mods: egui::Modifiers,
    scroll_acc: Vec2,
    /// Screens in full screen that were maximised before: GNOME ignores
    /// "leave full screen" for a maximised window, so it is un-maximised on
    /// the way in and maximised again on the way out.
    max_before_fullscreen: HashSet<u8>,
    minimized: HashSet<u8>,
    /// Original size or fit to window (all screens of this session).
    scale: ScaleMode,
    /// The user picked another scale mode: remember it as the default.
    scale_changed: bool,
    /// Quality picked in one of this connection's windows: becomes the saved default.
    quality_changed: bool,
    /// Scale mode and "controls shown" each screen window was last drawn with
    /// (to resize on a switch).
    applied_scale: HashMap<u8, (ScaleMode, bool)>,
    /// Screen windows show only the picture (same for all of them).
    controls_hidden: bool,
    /// The user hid or showed the controls: remember it, apply to every window.
    controls_changed: bool,
    /// Hidden controls brought back by resting the mouse at the top edge.
    reveal: HashMap<u8, Reveal>,
    /// Keyboard grabbed while Alt is held, so Alt+Tab reaches the remote computer.
    alt_grab: Option<crate::keygrab::AltGrab>,
    /// Last "watching these screens" sent to the helper.
    last_watch: Option<Vec<u8>>,
    /// Open file browser window, if any.
    files: Option<FilesView>,
    /// Screen windows that had the keyboard last frame (the local clipboard
    /// is shared when one gets it).
    focused: HashSet<u8>,
    /// Remote pointer shapes as textures (with their pictures, for size and hot spot).
    pointer_textures: HashMap<u32, (egui::TextureHandle, [u32; 4])>,
    /// Remote pointer position (virtual-desktop pixels) and shape id.
    remote_pointer: Option<(i32, i32, u32)>,
    /// Opened with "Files": no screen windows (the helper captures nothing),
    /// ends when the file browser is closed. "Show screens" turns it into a
    /// normal connection.
    files_only: bool,
    /// What the file browser remembers (from the settings; kept up to date
    /// while it is open): this computer's side and the remote computer's.
    files_local: FilesSide,
    files_remote: FilesSide,
}

/// Two-pane file browser: this computer on the left, the remote one on the right.
pub struct FilesView {
    /// "" = starting places (drives / home).
    local_dir: String,
    local: crate::net::RemoteDir,
    local_sel: Option<crate::proto::FsEntry>,
    remote_sel: Option<crate::proto::FsEntry>,
    new_folder: String,
    local_hidden: bool,
    remote_hidden: bool,
    remote_start: RemoteStart,
}

/// Where the remote side of the file browser begins: the folder remembered
/// for that computer, or (if it is gone, or nothing is remembered) its
/// primary disk, found in the list of its drives.
#[derive(Debug, Clone, PartialEq)]
enum RemoteStart {
    /// Asked for the remembered folder; waiting to see if it still exists.
    Remembered(String),
    /// Asked for the drives; waiting to pick the primary one.
    Drives,
    /// Started: where the person goes from here is remembered.
    Done,
}

/// The folder on this computer the file browser starts in: the remembered
/// one if it still exists, else the home folder, else the root.
fn local_start(remembered: Option<&str>) -> String {
    let is_dir = |p: &str| !p.is_empty() && std::path::Path::new(p).is_dir();
    if let Some(p) = remembered.filter(|p| is_dir(p)) {
        return p.to_string();
    }
    if let Some(h) = dirs::home_dir().map(|h| h.display().to_string()).filter(|h| is_dir(h)) {
        return h;
    }
    // "" lists the drives on Windows; elsewhere there is one root.
    if cfg!(windows) { String::new() } else { "/".to_string() }
}

/// The primary disk among a computer's starting places: `C:` on Windows
/// (else its first drive), `/` elsewhere.
fn primary_disk(roots: &[crate::proto::FsEntry]) -> Option<String> {
    let drive = |p: &str| p.len() >= 2 && p.as_bytes()[1] == b':' && p.as_bytes()[0].is_ascii_alphabetic();
    roots
        .iter()
        .find(|e| e.path.to_ascii_uppercase().starts_with("C:"))
        .or_else(|| roots.iter().find(|e| drive(&e.path)))
        .or_else(|| roots.iter().find(|e| e.path == "/"))
        .or(roots.first())
        .map(|e| e.path.clone())
}

impl FilesView {
    /// `local` / `remote`: what is remembered about this computer's side
    /// and about the remote computer's.
    fn new(local: &FilesSide, remote: &FilesSide) -> FilesView {
        let mut v = FilesView {
            local_dir: local_start(Some(local.dir.as_str())),
            local: Default::default(),
            local_sel: None,
            remote_sel: None,
            new_folder: String::new(),
            local_hidden: local.show_hidden,
            remote_hidden: remote.show_hidden,
            remote_start: if remote.dir.is_empty() { RemoteStart::Drives } else { RemoteStart::Remembered(remote.dir.clone()) },
        };
        v.reload_local();
        v
    }

    /// The first thing to ask the remote computer for.
    fn first_remote_path(&self) -> &str {
        match &self.remote_start {
            RemoteStart::Remembered(p) => p,
            _ => "",
        }
    }

    /// Follow the start-up steps as listings arrive. Returns a folder to ask for next.
    fn remote_start_step(&mut self, remote: &crate::net::RemoteDir) -> Option<String> {
        if remote.loading {
            return None;
        }
        match &self.remote_start {
            RemoteStart::Remembered(p) if remote.path == *p => {
                if remote.error.is_some() {
                    // The remembered folder is gone: the primary disk instead.
                    self.remote_start = RemoteStart::Drives;
                    Some(String::new())
                } else {
                    self.remote_start = RemoteStart::Done;
                    None
                }
            }
            RemoteStart::Drives if remote.path.is_empty() => {
                self.remote_start = RemoteStart::Done;
                primary_disk(&remote.entries)
            }
            _ => None,
        }
    }

    fn reload_local(&mut self) {
        if let SessionMsg::FsListing { path, parent, entries, error, .. } = crate::files::listing(0, &self.local_dir) {
            self.local = crate::net::RemoteDir { path, parent, entries, error, loading: false };
        }
        self.local_sel = None;
    }
}

/// "1.4 MB" style sizes.
pub fn format_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

enum PaneAction {
    Open(String),
    Select(crate::proto::FsEntry),
}

/// One side of the file browser. Returns what the person clicked.
fn file_pane(
    ui: &mut egui::Ui,
    id: &str,
    title: &str,
    dir: &crate::net::RemoteDir,
    sel: Option<&crate::proto::FsEntry>,
    show_hidden: bool,
) -> Option<PaneAction> {
    let mut action = None;
    ui.label(RichText::new(title).strong().size(15.0));
    ui.horizontal(|ui| {
        if ui.add_enabled(dir.parent.is_some(), egui::Button::new("Up")).clicked() {
            action = Some(PaneAction::Open(dir.parent.clone().unwrap_or_default()));
        }
        if ui.button("Refresh").clicked() {
            action = Some(PaneAction::Open(dir.path.clone()));
        }
        let shown = if dir.path.is_empty() { "Starting places".to_string() } else { dir.path.clone() };
        ui.label(RichText::new(shown).monospace().color(MUTED));
    });
    if let Some(e) = &dir.error {
        ui.label(RichText::new(e).color(DANGER));
    }
    if dir.loading {
        ui.add(egui::Spinner::new());
    }
    egui::Frame::new().fill(BG).stroke(Stroke::new(1.0, LINE)).corner_radius(CornerRadius::same(6)).inner_margin(Margin::same(4)).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt(id).auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            for e in dir.entries.iter().filter(|e| show_hidden || !e.name.starts_with('.')) {
                let selected = sel.is_some_and(|s| s.path == e.path);
                // Room at the left for the folder icon (files line up with folder names).
                const PAD: &str = "          ";
                let text = if e.is_dir { format!("{PAD}{}", e.name) } else { format!("{PAD}{}    {}", e.name, format_size(e.size)) };
                let resp = ui.add(egui::Button::new(RichText::new(text).color(TEXT)).selected(selected).frame(selected).min_size(Vec2::new(ui.available_width(), 20.0)));
                if e.is_dir {
                    paint_folder(ui.painter(), Pos2::new(resp.rect.left() + 6.0, resp.rect.center().y));
                }
                if resp.double_clicked() && e.is_dir {
                    action = Some(PaneAction::Open(e.path.clone()));
                } else if resp.clicked() {
                    action = Some(PaneAction::Select(e.clone()));
                }
            }
            if dir.entries.is_empty() && dir.error.is_none() && !dir.loading {
                ui.label(RichText::new("(empty folder)").color(MUTED));
            }
        });
    });
    action
}

/// A small yellow folder; `at` is the middle of its left edge.
fn paint_folder(p: &egui::Painter, at: Pos2) {
    let back = Color32::from_rgb(214, 158, 46);
    let front = Color32::from_rgb(246, 196, 83);
    // The tab at the top left, then the body in front of it.
    p.rect_filled(Rect::from_min_size(at + Vec2::new(0.0, -6.5), Vec2::new(7.0, 4.0)), CornerRadius::same(1), back);
    p.rect_filled(Rect::from_min_size(at + Vec2::new(0.0, -4.5), Vec2::new(16.0, 11.0)), CornerRadius::same(2), back);
    p.rect_filled(Rect::from_min_size(at + Vec2::new(0.0, -2.5), Vec2::new(16.0, 9.0)), CornerRadius::same(2), front);
}

/// Contents of the file browser window. Returns true when it should close.
fn files_window(ui: &mut egui::Ui, s: &mut OpenSession) -> bool {
    let ctx = ui.ctx().clone();
    if ctx.input(|i| i.viewport().close_requested()) {
        return true;
    }
    let Some(fv) = s.files.as_mut() else { return true };
    let (remote, transfers) = {
        let mut sh = s.handle.shared.lock().unwrap();
        if sh.remote_changed {
            sh.remote_changed = false;
            let p = sh.remote_dir.path.clone();
            drop(sh);
            s.handle.list_remote(&p);
            sh = s.handle.shared.lock().unwrap();
        }
        (sh.remote_dir.clone(), sh.transfers.clone())
    };
    if let Some(next) = fv.remote_start_step(&remote) {
        s.handle.list_remote(&next);
    }
    egui::Panel::top("files-bar").frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(12, 8))).show(ui, |ui| {
        // Two halves, like the two sides below: this computer, the remote one.
        ui.columns(2, |cols| {
            cols[0].horizontal(|ui| {
                let up_ok = fv.local_sel.as_ref().is_some_and(|e| !e.is_dir) && !remote.path.is_empty();
                if ui.add_enabled(up_ok, egui::Button::new(RichText::new("Upload  ->").strong())).on_hover_text("Copy the selected file to the remote folder").clicked()
                    && let Some(e) = &fv.local_sel
                {
                    s.handle.upload(PathBuf::from(&e.path), remote.path.clone());
                }
                ui.checkbox(&mut fv.local_hidden, "Show hidden files");
            });
            cols[1].horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut fv.new_folder).hint_text("New remote folder name").desired_width(180.0));
                let name_ok = crate::files::safe_name(&fv.new_folder).is_some() && !remote.path.is_empty();
                if ui.add_enabled(name_ok, egui::Button::new("Create folder")).clicked() {
                    let p = PathBuf::from(&remote.path).join(fv.new_folder.trim());
                    s.handle.mkdir_remote(&p.display().to_string());
                    fv.new_folder.clear();
                }
                ui.checkbox(&mut fv.remote_hidden, "Show hidden files");
                let down_ok = fv.remote_sel.as_ref().is_some_and(|e| !e.is_dir) && !fv.local_dir.is_empty();
                if ui.add_enabled(down_ok, egui::Button::new(RichText::new("<-  Download").strong())).on_hover_text("Copy the selected remote file here").clicked()
                    && let Some(e) = &fv.remote_sel
                {
                    s.handle.download(e, PathBuf::from(&fv.local_dir));
                }
            });
        });
    });
    egui::Panel::bottom("transfers").frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(12, 8))).show(ui, |ui| {
        ui.label(RichText::new("TRANSFERS").size(11.0).color(MUTED));
        if transfers.is_empty() {
            ui.label(RichText::new("Select a file and press Upload or Download. Files are never overwritten.").color(MUTED));
        }
        for t in transfers.iter().rev().take(5) {
            ui.horizontal(|ui| {
                ui.label(if t.upload { "Upload" } else { "Download" });
                ui.label(RichText::new(&t.name).strong());
                let frac = if t.total == 0 { 1.0 } else { t.done as f32 / t.total as f32 };
                match &t.state {
                    TransferState::Running => {
                        ui.add(egui::ProgressBar::new(frac).desired_width(200.0).text(format!("{} of {}", format_size(t.done), format_size(t.total))));
                    }
                    TransferState::Done => {
                        ui.label(RichText::new(format!("done · {}", format_size(t.total))).color(OK));
                    }
                    TransferState::Failed(e) => {
                        ui.label(RichText::new(format!("failed: {e}")).color(DANGER));
                    }
                }
            });
        }
    });
    egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(Margin::same(12))).show(ui, |ui| {
        ui.columns(2, |cols| {
            if let Some(a) = file_pane(&mut cols[0], "local", "This computer", &fv.local, fv.local_sel.as_ref(), fv.local_hidden) {
                match a {
                    PaneAction::Open(p) => {
                        fv.local_dir = p;
                        fv.reload_local();
                    }
                    PaneAction::Select(e) => fv.local_sel = Some(e),
                }
            }
            let title = format!("Remote: {}", s.name);
            if let Some(a) = file_pane(&mut cols[1], "remote", &title, &remote, fv.remote_sel.as_ref(), fv.remote_hidden) {
                match a {
                    PaneAction::Open(p) => {
                        fv.remote_sel = None;
                        s.handle.list_remote(&p);
                    }
                    PaneAction::Select(e) => fv.remote_sel = Some(e),
                }
            }
        });
    });
    // Downloads land in the left folder: show them as they finish.
    if transfers.iter().any(|t| !t.upload && t.state == TransferState::Done && t.local.parent().is_some_and(|p| p.display().to_string() == fv.local_dir))
        && !fv.local.entries.iter().any(|e| transfers.iter().any(|t| !t.upload && t.local.display().to_string() == e.path))
    {
        fv.reload_local();
    }
    false
}

impl OpenSession {
    fn open_files(&mut self) {
        if self.files.is_none() {
            let fv = FilesView::new(&self.files_local, &self.files_remote);
            self.handle.list_remote(fv.first_remote_path());
            self.files = Some(fv);
        }
    }

    /// Take note of where the open file browser is now and what it shows.
    /// Returns (this computer's side changed, the remote side changed).
    fn note_files(&mut self) -> (bool, bool) {
        let Some(fv) = self.files.as_ref() else { return (false, false) };
        let mut local = FilesSide { dir: self.files_local.dir.clone(), show_hidden: fv.local_hidden };
        if !fv.local_dir.is_empty() {
            local.dir.clone_from(&fv.local_dir);
        }
        let mut remote = FilesSide { dir: self.files_remote.dir.clone(), show_hidden: fv.remote_hidden };
        {
            // The remote folder counts once the start-up steps are over and it could be read.
            let sh = self.handle.shared.lock().unwrap();
            let r = &sh.remote_dir;
            if fv.remote_start == RemoteStart::Done && !r.loading && r.error.is_none() && !r.path.is_empty() {
                remote.dir.clone_from(&r.path);
            }
        }
        let changed = (local != self.files_local, remote != self.files_remote);
        self.files_local = local;
        self.files_remote = remote;
        changed
    }
}

impl OpenSession {
    fn update_textures(&mut self, ctx: &egui::Context) {
        let mut shared = self.handle.shared.lock().unwrap();
        for (mon, canvas) in shared.canvases.iter_mut() {
            let current = self.textures.get(mon).map(|t| t.1);
            if canvas.frame.is_none() || current == Some(canvas.generation) {
                continue;
            }
            let dirty = canvas.take_dirty();
            let Some(frame) = &canvas.frame else { continue };
            let size = [frame.width as usize, frame.height as usize];
            match self.textures.get_mut(mon) {
                // Same size: upload only the parts that changed.
                Some((tex, generation)) if tex.size() == size => {
                    for r in dirty {
                        if r.w == 0 || r.h == 0 {
                            continue;
                        }
                        let part = frame.crop(r);
                        let img = egui::ColorImage::from_rgba_unmultiplied([r.w as usize, r.h as usize], &part.rgba);
                        tex.set_partial([r.x as usize, r.y as usize], img, egui::TextureOptions::LINEAR);
                    }
                    *generation = canvas.generation;
                }
                Some((tex, generation)) => {
                    tex.set(egui::ColorImage::from_rgba_unmultiplied(size, &frame.rgba), egui::TextureOptions::LINEAR);
                    *generation = canvas.generation;
                }
                None => {
                    let img = egui::ColorImage::from_rgba_unmultiplied(size, &frame.rgba);
                    let tex = ctx.load_texture(format!("s{}m{}", self.id, mon), img, egui::TextureOptions::LINEAR);
                    self.textures.insert(*mon, (tex, canvas.generation));
                }
            }
        }
        for (id, img) in &shared.pointer_shapes {
            if !self.pointer_textures.contains_key(id) {
                let pic = egui::ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.rgba);
                let tex = ctx.load_texture(format!("s{}p{}", self.id, id), pic, egui::TextureOptions::LINEAR);
                self.pointer_textures.insert(*id, (tex, [img.width, img.height, img.hot_x, img.hot_y]));
            }
        }
        self.remote_pointer = shared.pointer;
        if self.key.is_none() && shared.state == ConnState::Live {
            self.key = Some(match &self.machine_id {
                Some(id) => id.clone(),
                None => format!("host:{}", shared.hostname),
            });
        }
    }
}

// ------------------------------------------------------------------ input mapping

/// Remote key name for an egui key, and whether it normally produces text.
pub fn remote_key(key: egui::Key) -> Option<(&'static str, bool)> {
    use egui::Key as K;
    let name = match key {
        K::ArrowUp => "ArrowUp",
        K::ArrowDown => "ArrowDown",
        K::ArrowLeft => "ArrowLeft",
        K::ArrowRight => "ArrowRight",
        other => other.name(),
    };
    crate::keys::keysym(name)?;
    let texty = name.len() == 1
        || matches!(
            name,
            "Space" | "Minus" | "Equals" | "OpenBracket" | "CloseBracket" | "Backslash" | "Semicolon" | "Quote" | "Comma" | "Period"
                | "Slash" | "Backtick"
        );
    Some((name, texty))
}

/// Turn this frame's egui events into remote input for monitor `mon` shown
/// in `rect`, of which only `visible` is on screen (scrolled picture).
/// `keyboard` is false when the window isn't focused.
pub fn map_events(
    events: &[egui::Event],
    rect: Rect,
    visible: Rect,
    mon: u8,
    prev_mods: &mut egui::Modifiers,
    mods: egui::Modifiers,
    keyboard: bool,
) -> Vec<InputEvent> {
    let norm = |p: Pos2| ((p.x - rect.min.x) / rect.width(), (p.y - rect.min.y) / rect.height());
    let hit = |p: Pos2| rect.contains(p) && visible.contains(p);
    let mut out = Vec::new();
    if keyboard {
        for (was, now, name) in [
            (prev_mods.ctrl, mods.ctrl, "Control"),
            (prev_mods.shift, mods.shift, "Shift"),
            (prev_mods.alt, mods.alt, "Alt"),
        ] {
            if was != now {
                out.push(InputEvent::Key { key: name.into(), down: now });
            }
        }
        *prev_mods = mods;
    }
    let mut last_move = None;
    for ev in events {
        match ev {
            egui::Event::PointerMoved(p) if hit(*p) => {
                let (x, y) = norm(*p);
                last_move = Some(InputEvent::MouseMove { mon, x, y });
            }
            egui::Event::PointerButton { pos, button, pressed, .. } => {
                if *pressed && !hit(*pos) {
                    continue;
                }
                let button = match button {
                    egui::PointerButton::Primary => MouseButton::Left,
                    egui::PointerButton::Secondary => MouseButton::Right,
                    egui::PointerButton::Middle => MouseButton::Middle,
                    _ => continue,
                };
                if let Some(m) = last_move.take() {
                    out.push(m);
                }
                let (x, y) = norm(*pos);
                out.push(InputEvent::MouseButton { mon, x: x.clamp(0.0, 1.0), y: y.clamp(0.0, 1.0), button, down: *pressed });
            }
            egui::Event::MouseWheel { unit, delta, .. } => {
                let s = match unit {
                    egui::MouseWheelUnit::Line => *delta,
                    egui::MouseWheelUnit::Point => *delta / 40.0,
                    egui::MouseWheelUnit::Page => *delta * 10.0,
                };
                out.push(InputEvent::Scroll { dx: s.x, dy: s.y });
            }
            egui::Event::Key { key, pressed, modifiers, .. } if keyboard => {
                if let Some((name, texty)) = remote_key(*key)
                    && (!texty || modifiers.ctrl || modifiers.alt)
                {
                    out.push(InputEvent::Key { key: name.into(), down: *pressed });
                }
            }
            egui::Event::Text(t) if keyboard && !mods.ctrl && !mods.alt => {
                out.push(InputEvent::Text { text: t.clone() });
            }
            egui::Event::Copy | egui::Event::Cut | egui::Event::Paste(_) if keyboard => {
                let k = match ev {
                    egui::Event::Copy => "C",
                    egui::Event::Cut => "X",
                    _ => "V",
                };
                out.push(InputEvent::Key { key: k.into(), down: true });
                out.push(InputEvent::Key { key: k.into(), down: false });
            }
            _ => {}
        }
    }
    if let Some(m) = last_move {
        out.insert(0, m);
    }
    out
}

/// Largest rect with the picture's aspect ratio inside `avail`, centred.
pub fn fit_rect(avail: Rect, w: f32, h: f32) -> Rect {
    let scale = (avail.width() / w).min(avail.height() / h).max(0.01);
    Rect::from_center_size(avail.center(), Vec2::new(w * scale, h * scale))
}

// ------------------------------------------------------------------ app

/// An open "Rename computer" dialog.
/// How often the selected computer's screen pictures are refreshed.
const PREVIEW_EVERY: Duration = Duration::from_secs(5);

/// Small pictures of the selected computer's screens.
struct PreviewState {
    machine_id: String,
    /// When the last answer (or error) came; the next request is due after [`PREVIEW_EVERY`].
    fetched: Option<Instant>,
    /// Seconds since 1970 when the pictures were taken.
    taken: u64,
    textures: HashMap<u8, egui::TextureHandle>,
    error: Option<String>,
}

type PreviewResult = (String, u64, Vec<(u8, Frame)>);

struct NotesEdit {
    machine_id: String,
    name: String,
    text: String,
    /// The text as last saved / received from the relay.
    saved: String,
}

struct Renaming {
    machine_id: String,
    text: String,
    focus: bool,
}

struct GrantForm {
    open: bool,
    minutes: u64,
    view_only: bool,
    label: String,
    result: Option<String>,
}

impl Default for GrantForm {
    fn default() -> Self {
        GrantForm { open: false, minutes: 120, view_only: true, label: String::new(), result: None }
    }
}

pub struct ViewerApp {
    opts: ViewerOptions,
    cfg: ViewerConfig,
    cfg_path: Option<PathBuf>,
    layouts: LayoutStore,
    layouts_path: Option<PathBuf>,
    layouts_dirty: Option<Instant>,
    server_input: String,
    password_input: String,
    link_input: String,
    overview: Option<Overview>,
    overview_err: Option<String>,
    job_overview: Option<Job<Overview>>,
    job_login: Option<Job<String>>,
    job_grant: Option<Job<String>>,
    job_revoke: Option<Job<()>>,
    job_rename: Option<Job<()>>,
    job_notes: Option<Job<()>>,
    /// The open "Notes" window (from the list's right-click menu).
    notes_edit: Option<NotesEdit>,
    /// "Update" / "Update all" of helpers: what to tell the administrator.
    job_update: Option<Job<String>>,
    renaming: Option<Renaming>,
    last_refresh: Option<Instant>,
    selected: Option<String>,
    preview: Option<PreviewState>,
    job_preview: Option<Job<PreviewResult>>,
    search: String,
    sessions: Vec<OpenSession>,
    next_session: u64,
    grant: GrantForm,
    toast: Option<(String, Instant)>,
    updater: Updater,
    frames: u64,
    shot_requested: bool,
    shot_done: Arc<AtomicBool>,
    demo: bool,
    /// The icon in the system tray (not in screenshots).
    tray: Option<Tray>,
    /// The main window is hidden in the tray.
    hidden: bool,
    /// Started with `--tray`: hide the window in the first frame.
    hide_pending: bool,
}

/// Sample computers for the demo mode and screenshots.
pub fn sample_overview() -> Overview {
    let mon = |i: u8, x: i32, w: u32, h: u32| MonitorInfo {
        index: i,
        name: format!("DISPLAY{}", i + 1),
        x,
        y: 0,
        width: w,
        height: h,
        primary: i == 0,
    };
    let now = now_secs();
    let m = |id: &str, name: &str, host: &str, os: &str, mons: Vec<MonitorInfo>, online: bool, seen: u64| MachineView {
        machine: MachineSummary {
            id: id.into(),
            name: name.into(),
            hostname: host.into(),
            os: os.into(),
            version: crate::VERSION.into(),
            monitors: mons,
            last_seen: now - seen,
            owner: None,
            notes: String::new(),
        },
        online,
        sessions: 0,
        up_to_date: true,
        owner_name: None,
        updating: false,
        update_error: None,
        stats: online.then(crate::sysstat::demo_stats),
    };
    const G: u64 = 1 << 30;
    let disk = |name: &str, total: u64, free: u64| crate::sysstat::DiskStat { name: name.into(), total: total * G, free: free * G };
    let mut anna = m("demo-anna", "Anna's laptop", "ANNA-NB", "Windows 11 Home", vec![mon(0, 0, 1920, 1080)], true, 0);
    anna.machine.owner = Some("demo-user-anna".into());
    anna.owner_name = Some("Anna".into());
    // One helper behind the published version, to show the "Update" button.
    let mut srv = m("demo-srv", "Accounting server", "ACC-SRV16", "Windows Server 2016", vec![mon(0, 0, 1920, 1080)], true, 0);
    srv.machine.version = "0.0.9".into();
    srv.up_to_date = false;
    srv.machine.notes = "Backups run at 02:00, do not restart at night.\nUPS in the server room.".into();
    if let Some(s) = srv.stats.as_mut() {
        s.cpu = 94.0;
        s.mem_used = 15 * G;
        s.disks = vec![disk("C:", 120, 7), disk("D:", 2000, 860)];
    }
    let mut office = m("demo-office", "Office PC", "OFFICE-PC", "Windows 11 Pro 23H2 (build 22631)", vec![mon(0, 0, 1600, 900), mon(1, 1600, 1280, 1024)], true, 0);
    office.machine.notes = "Reception, printer on USB".into();
    if let Some(s) = office.stats.as_mut() {
        s.disks = vec![disk("C:", 476, 212)];
    }
    Overview {
        machines: vec![
            office,
            srv,
            m(
                "demo-ubuntu",
                "Design workstation",
                "design-ws",
                "Ubuntu 22.04.4 LTS",
                vec![mon(0, 0, 2560, 1440), mon(1, 2560, 1920, 1080)],
                true,
                0,
            ),
            anna,
            m("demo-front", "Front desk", "FRONTDESK", "Windows 10 Pro", vec![mon(0, 0, 1366, 768)], false, 3 * 3600),
        ],
        grants: vec![],
        sessions: vec![],
        install_url: "https://remote.example.com/install/sample".into(),
        server_version: crate::VERSION.into(),
        release_version: Some(crate::VERSION.into()),
        now,
        role: "admin".into(),
        user_name: "Administrator".into(),
        users: vec![crate::server::UserView {
            id: "demo-user-anna".into(),
            name: "Anna".into(),
            login: "anna".into(),
            install_url: "https://remote.example.com/install/sample-anna".into(),
            machines: vec!["demo-anna".into()],
        }],
        server_update: crate::server_update::UpdateStatus {
            version: crate::VERSION.into(),
            source: Some("example/farlink".into()),
            last_check: Some(now - 20 * 60),
            last_check_ok: true,
            last_check_result: format!("up to date (latest release {})", crate::VERSION),
            last_update: Some(now - 3 * 86400),
            last_update_from: Some("0.0.9".into()),
            checking: false,
        },
        slow_after_pause_fix: None,
    }
}

impl ViewerApp {
    pub fn new(ctx: &egui::Context, opts: ViewerOptions) -> ViewerApp {
        style(ctx);
        let screenshot = opts.screenshot.is_some();
        let dir = viewer_dir();
        let (cfg_path, layouts_path) =
            if screenshot { (None, None) } else { (Some(dir.join("viewer.json")), Some(dir.join("layouts.json"))) };
        let cfg: ViewerConfig = cfg_path.as_ref().map(|p| load_json(p)).unwrap_or_default();
        let layouts = layouts_path.as_ref().map(|p| LayoutStore::load(p)).unwrap_or_default();
        let demo = opts.demo || screenshot;
        let mut app = ViewerApp {
            server_input: cfg.server_url.clone(),
            cfg,
            cfg_path,
            layouts,
            layouts_path,
            layouts_dirty: None,
            password_input: String::new(),
            link_input: String::new(),
            overview: None,
            overview_err: None,
            job_overview: None,
            job_login: None,
            job_grant: None,
            job_revoke: None,
            job_rename: None,
            job_notes: None,
            notes_edit: None,
            job_update: None,
            renaming: None,
            last_refresh: None,
            selected: None,
            preview: None,
            job_preview: None,
            search: String::new(),
            sessions: Vec::new(),
            next_session: 1,
            grant: GrantForm::default(),
            toast: None,
            updater: Updater::default(),
            frames: 0,
            shot_requested: false,
            shot_done: Arc::new(AtomicBool::new(false)),
            demo,
            tray: None,
            hidden: false,
            hide_pending: false,
            opts,
        };
        if demo {
            app.overview = Some(sample_overview());
            app.selected = Some("demo-office".into());
            // Hidden switch for checking the preview of a selected computer (no session open).
            if screenshot && std::env::var_os("FARLINK_SHOT_PREVIEW").is_none() {
                app.open_session(ctx, Target::Demo, Some("demo-office".into()), "Office PC".into(), false);
                if std::env::var_os("FARLINK_SHOT_FILES").is_some() {
                    app.sessions[0].open_files();
                }
                // Hidden switch for checking the rename dialog's look.
                if std::env::var_os("FARLINK_SHOT_RENAME").is_some() {
                    app.renaming = Some(Renaming { machine_id: "demo-office".into(), text: "Office PC".into(), focus: true });
                }
            }
        }
        if let Some(link) = app.opts.open_link.clone() {
            app.open_link(ctx, &link);
        }
        if !screenshot && !app.demo {
            if let Ok(exe) = app_exe() {
                crate::update::cleanup_old(&exe);
            }
            app.updater.start(ctx, &app.cfg.server_url, app.cfg.auto_update);
        }
        if !screenshot {
            app.setup_autostart();
            let retry = if app.opts.start_hidden { crate::tray::RETRY_AT_LOGIN } else { Duration::from_secs(5) };
            app.tray = Some(Tray::start(ctx, crate::autostart::is_enabled(), retry));
        }
        // Hidden in the first frame (a command sent from here is lost).
        app.hide_pending = app.opts.start_hidden && app.tray.is_some();
        app
    }

    /// Switches "Start at login" on once, on the first start of an installed
    /// app (not development builds or the demo), and keeps it pointing at
    /// this exe.
    fn setup_autostart(&mut self) {
        let Ok(exe) = app_exe() else { return };
        if !self.cfg.autostart_set_up && !self.demo && !cfg!(debug_assertions) {
            match crate::autostart::set(true, &exe) {
                Ok(()) => {
                    self.cfg.autostart_set_up = true;
                    self.save_cfg();
                }
                Err(e) => log::warn!("start at login: {e:#}"),
            }
        } else if let Err(e) = crate::autostart::refresh(&exe) {
            log::warn!("start at login: {e:#}");
        }
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        self.hidden = true;
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Visible(false));
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        self.hidden = false;
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Visible(true));
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Focus);
    }

    /// Tray clicks, and closing the main window (hides it while there is a tray).
    fn tray_step(&mut self, ctx: &egui::Context) {
        if std::mem::take(&mut self.hide_pending) {
            self.hide_window(ctx);
        }
        while let Some(event) = self.tray.as_ref().and_then(Tray::try_recv) {
            match event {
                TrayEvent::Open => self.show_window(ctx),
                TrayEvent::ToggleAutostart => {
                    let on = !crate::autostart::is_enabled();
                    match app_exe().map_err(anyhow::Error::from).and_then(|exe| crate::autostart::set(on, &exe)) {
                        Ok(()) => {
                            self.cfg.autostart_set_up = true;
                            self.save_cfg();
                        }
                        Err(e) => self.toast = Some((format!("Start at login: {e:#}"), Instant::now())),
                    }
                    if let Some(t) = &self.tray {
                        t.set_autostart(crate::autostart::is_enabled());
                    }
                }
                TrayEvent::Quit => quit_app(ctx),
            }
        }
        let status = self.tray.as_ref().map(Tray::status);
        // Started hidden but there is no tray to bring it back: show the window.
        if self.hidden && status == Some(TrayStatus::Failed) {
            self.show_window(ctx);
        }
        if ctx.input(|i| i.viewport().close_requested()) && !QUITTING.load(Ordering::SeqCst) {
            if status == Some(TrayStatus::Ready) {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                self.hide_window(ctx);
            } else {
                QUITTING.store(true, Ordering::SeqCst);
            }
        }
        if QUITTING.load(Ordering::SeqCst)
            && let Some(t) = self.tray.take()
        {
            t.shutdown();
        }
    }

    fn signed_in(&self) -> bool {
        self.cfg.admin_token.is_some() && !self.cfg.server_url.is_empty()
    }

    fn api(&self) -> Option<Api> {
        Some(Api { server: self.cfg.server_url.clone(), token: self.cfg.admin_token.clone()? })
    }

    fn save_cfg(&self) {
        self.updater.set_server(&self.cfg.server_url);
        if let Some(p) = &self.cfg_path
            && let Err(e) = save_json(p, &self.cfg)
        {
            log::warn!("saving settings: {e}");
        }
    }

    fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), Instant::now()));
    }

    fn refresh(&mut self, ctx: &egui::Context) {
        if self.demo || self.job_overview.is_some() {
            return;
        }
        if let Some(api) = self.api() {
            self.job_overview = Some(spawn_job(ctx, move || api.overview()));
            self.last_refresh = Some(Instant::now());
        }
    }

    fn open_session(&mut self, ctx: &egui::Context, target: Target, machine_id: Option<String>, name: String, view_only: bool) {
        // One session per machine: a new connection replaces the old one.
        if let Some(id) = &machine_id {
            self.sessions.retain(|s| s.machine_id.as_ref() != Some(id));
        }
        let c = ctx.clone();
        let handle = SessionHandle::start(target, self.cfg.quality, view_only, Arc::new(move || c.request_repaint()));
        let remembered_remote = machine_id.as_ref().and_then(|id| self.cfg.files_remote.get(id).cloned()).unwrap_or_default();
        self.sessions.push(OpenSession {
            id: self.next_session,
            handle,
            key: machine_id.clone(),
            machine_id,
            name,
            quality: self.cfg.quality,
            textures: HashMap::new(),
            builders: HashMap::new(),
            closed: HashSet::new(),
            mods: egui::Modifiers::default(),
            scroll_acc: Vec2::ZERO,
            max_before_fullscreen: HashSet::new(),
            minimized: HashSet::new(),
            scale: self.cfg.scale_mode,
            scale_changed: false,
            quality_changed: false,
            applied_scale: HashMap::new(),
            controls_hidden: self.cfg.hide_controls,
            controls_changed: false,
            reveal: HashMap::new(),
            alt_grab: None,
            last_watch: None,
            files: None,
            focused: HashSet::new(),
            pointer_textures: HashMap::new(),
            remote_pointer: None,
            files_only: false,
            files_local: self.cfg.files_local.clone(),
            files_remote: remembered_remote,
        });
        self.next_session += 1;
    }

    /// Exchange files with a computer without seeing or controlling its screens.
    fn files_machine(&mut self, ctx: &egui::Context, m: &MachineSummary) {
        self.connect_machine(ctx, m, false);
        if let Some(s) = self.sessions.last_mut().filter(|s| s.machine_id.as_deref() == Some(m.id.as_str())) {
            s.files_only = true;
            s.open_files();
        }
    }

    fn connect_machine(&mut self, ctx: &egui::Context, m: &MachineSummary, view_only: bool) {
        if self.demo {
            self.open_session(ctx, Target::Demo, Some(m.id.clone()), m.name.clone(), view_only);
            return;
        }
        let Some(api) = self.api() else { return };
        let target = Target::Admin { server: api.server, token: api.token, machine_id: m.id.clone(), view_only };
        self.open_session(ctx, target, Some(m.id.clone()), m.name.clone(), view_only);
    }

    fn open_link(&mut self, ctx: &egui::Context, link: &str) {
        match parse_link(link) {
            Some(g) => {
                self.cfg.remember_link(link.trim());
                self.save_cfg();
                let name = format!("Link to {}", g.server.trim_start_matches("https://").trim_start_matches("http://"));
                self.open_session(ctx, Target::Guest(g), None, name, false);
                self.link_input.clear();
            }
            None => self.toast("That doesn't look like a Farlink link (https://.../g/...)"),
        }
    }

    fn poll_jobs(&mut self, ctx: &egui::Context) {
        if let Some(r) = take_job(&mut self.job_overview) {
            match r {
                Ok(o) => {
                    if self.selected.is_none() {
                        self.selected = o.machines.first().map(|m| m.machine.id.clone());
                    }
                    // Names changed elsewhere (web page, another app) show up in open windows too.
                    for s in &mut self.sessions {
                        let Some(m) = o.machines.iter().find(|m| Some(&m.machine.id) == s.machine_id.as_ref()) else { continue };
                        if s.name != m.machine.name {
                            s.name = m.machine.name.clone();
                            let shared = s.handle.shared.lock().unwrap();
                            for mon in &shared.monitors {
                                if let Some(b) = s.builders.get_mut(&mon.index) {
                                    *b = b.clone().with_title(format!(
                                        "{} - Screen {} ({}x{})",
                                        s.name,
                                        mon.index + 1,
                                        mon.width,
                                        mon.height
                                    ));
                                }
                            }
                        }
                    }
                    self.overview = Some(o);
                    self.overview_err = None;
                }
                Err(e) => {
                    if e.contains("signed out") || e.contains("Wrong password") {
                        self.cfg.admin_token = None;
                        self.save_cfg();
                    }
                    self.overview_err = Some(e);
                }
            }
        }
        if let Some(r) = take_job(&mut self.job_login) {
            match r {
                Ok(token) => {
                    self.cfg.admin_token = Some(token);
                    self.password_input.clear();
                    self.save_cfg();
                    self.overview_err = None;
                    self.refresh(ctx);
                }
                Err(e) => self.overview_err = Some(e),
            }
        }
        if let Some(r) = take_job(&mut self.job_grant) {
            match r {
                Ok(url) => {
                    self.grant.result = Some(url);
                    self.refresh(ctx);
                }
                Err(e) => self.toast(e),
            }
        }
        if let Some(r) = take_job(&mut self.job_rename) {
            match r {
                Ok(()) => self.toast("Computer renamed"),
                Err(e) => self.toast(format!("Rename failed: {e}")),
            }
            self.refresh(ctx);
        }
        if let Some(r) = take_job(&mut self.job_notes) {
            match r {
                Ok(()) => self.toast("Notes saved"),
                Err(e) => self.toast(format!("Saving notes failed: {e}")),
            }
            self.refresh(ctx);
        }
        if let Some(r) = take_job(&mut self.job_update) {
            match r {
                Ok(text) => self.toast(text),
                Err(e) => self.toast(format!("Update failed: {e}")),
            }
            self.refresh(ctx);
        }
        if let Some(r) = take_job(&mut self.job_revoke) {
            match r {
                Ok(()) => self.toast("Access withdrawn, open sessions were ended"),
                Err(e) => self.toast(e),
            }
            self.refresh(ctx);
        }
    }

    /// Keep the pictures of the selected computer fresh: one request every
    /// [`PREVIEW_EVERY`] while it is online and not already open (an open
    /// session shows live pictures instead).
    fn preview_step(&mut self, ctx: &egui::Context) {
        if let Some(r) = take_job(&mut self.job_preview)
            && let Some(p) = self.preview.as_mut()
        {
            p.fetched = Some(Instant::now());
            match r {
                Ok((id, taken, frames)) if id == p.machine_id => {
                    p.error = None;
                    p.taken = taken;
                    p.textures.retain(|i, _| frames.iter().any(|(j, _)| j == i));
                    for (i, f) in frames {
                        let img = egui::ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba);
                        match p.textures.get_mut(&i) {
                            Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                            None => {
                                let t = ctx.load_texture(format!("preview{i}"), img, egui::TextureOptions::LINEAR);
                                p.textures.insert(i, t);
                            }
                        }
                    }
                }
                Ok(_) => p.fetched = None, // answer for a computer no longer selected
                Err(e) => p.error = Some(e),
            }
        }
        let wanted = self
            .overview
            .as_ref()
            .and_then(|o| o.machines.iter().find(|m| Some(&m.machine.id) == self.selected.as_ref()))
            .filter(|m| m.online && (self.signed_in() || self.demo))
            .filter(|m| !self.sessions.iter().any(|s| s.machine_id.as_ref() == Some(&m.machine.id)))
            .map(|m| (m.machine.id.clone(), m.machine.monitors.clone()));
        let Some((id, monitors)) = wanted else {
            self.preview = None;
            return;
        };
        if self.preview.as_ref().is_none_or(|p| p.machine_id != id) {
            self.preview = Some(PreviewState { machine_id: id.clone(), fetched: None, taken: 0, textures: HashMap::new(), error: None });
        }
        let p = self.preview.as_ref().unwrap();
        let minimized = ctx.input(|i| i.viewport().minimized).unwrap_or(false);
        let due = p.fetched.is_none_or(|t| t.elapsed() >= PREVIEW_EVERY);
        if self.job_preview.is_none() && due && !minimized && !self.hidden {
            if self.demo {
                let tick = (now_secs() / 5) as u32;
                self.job_preview = Some(spawn_job(ctx, move || {
                    let p = demo_preview(&monitors, tick);
                    Ok((id, p.taken, decode_preview(&p)))
                }));
            } else if let Some(api) = self.api() {
                self.job_preview = Some(spawn_job(ctx, move || {
                    let p = api.preview(&id)?;
                    Ok((id, p.taken, decode_preview(&p)))
                }));
            }
        }
        if let Some(t) = p.fetched {
            ctx.request_repaint_after(PREVIEW_EVERY.saturating_sub(t.elapsed()).max(Duration::from_millis(100)));
        }
    }

    // -------------------------------------------------------------- top bar

    /// Where and as whom this app is signed in, and "Sign out": the right end
    /// of the status bar (call inside a right-to-left layout).
    fn account_status(&mut self, ui: &mut egui::Ui) {
        let small = |t: &str, c: Color32| RichText::new(t).color(c).size(12.0);
        if self.demo {
            if !self.opts.demo && self.opts.screenshot.is_none() && ui.add(egui::Link::new(small("Leave demo", ACCENT))).clicked() {
                self.demo = false;
                self.overview = None;
                self.selected = None;
            }
            status_text(ui, AMBER, "Sample computers (demo)");
            return;
        }
        if !self.signed_in() {
            status_text(ui, MUTED, "Not signed in");
            return;
        }
        if ui.add(egui::Link::new(small("Sign out", ACCENT))).clicked() {
            if let Some(api) = self.api() {
                std::thread::spawn(move || api.logout());
            }
            self.cfg.admin_token = None;
            self.overview = None;
            self.save_cfg();
        }
        let server = self.cfg.server_url.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/');
        let who = self.overview.as_ref().map(|o| o.user_name.clone()).unwrap_or_default();
        let version = self.overview.as_ref().map(|o| format!(" {}", o.server_version)).unwrap_or_default();
        let text = if who.is_empty() { format!("{server}{version}") } else { format!("{server}{version} · {who}") };
        // Hover: the relay's version and updates; amber when something needs a look.
        let (tip, warn) = self.overview.as_ref().map(relay_details).unwrap_or_default();
        let dot = if self.overview_err.is_some() { DANGER } else if warn { AMBER } else { OK };
        let resp = status_text(ui, dot, &text);
        match &self.overview_err {
            Some(e) => resp.on_hover_text(format!("{}: {e}", self.cfg.server_url)),
            None if !tip.is_empty() => resp.on_hover_text(tip),
            None => resp,
        };
    }

    /// "Farlink x.y.z · up to date · Check for updates" in the bottom bar.
    /// The app's own update state, first in the status bar. Returns whether it showed anything.
    fn update_status(&mut self, ui: &mut egui::Ui) -> bool {
        if !self.updater.started() {
            return false;
        }
        // Only the first piece goes without the "·" in front.
        let mut first = true;
        let mut dot = |t: String| {
            if std::mem::take(&mut first) { t.trim_start_matches("· ").to_string() } else { t }
        };
        let st = self.updater.status();
        let small = |t: String, c: Color32| RichText::new(t).color(c).size(12.0);
        if st.checking {
            ui.label(small(dot("· checking for updates...".into()), MUTED));
        } else if let Some(v) = &st.installed {
            ui.label(small(dot(format!("· updated to {v}")), ACCENT));
            if ui.add(egui::Link::new(small("Restart".into(), ACCENT))).on_hover_text("Close Farlink and start the new version").clicked() {
                restart_app(ui.ctx());
            }
        } else if st.installing {
            ui.label(small(dot("· installing the update...".into()), MUTED));
        } else if let Some((v, _)) = &st.deb {
            if !st.text.is_empty() {
                ui.label(small(dot(format!("· {}", st.text)), AMBER));
            }
            ui.label(small(dot(format!("· {v} available")), ACCENT));
            let tip = "Install the new Farlink package (asks for your password)";
            if ui.add(egui::Link::new(small("Install".into(), ACCENT))).on_hover_text(tip).clicked() {
                self.updater.install_deb(ui.ctx());
            }
        } else {
            if !st.text.is_empty() {
                ui.label(small(dot(format!("· {}", st.text)), if st.error { AMBER } else { MUTED }));
            }
            let tip = match st.last_check {
                Some(t) => format!("Last checked {} ago; checks every hour by itself", human_duration(t.elapsed().as_secs())),
                None => "Checks every hour by itself".into(),
            };
            if ui.add(egui::Link::new(small("Check for updates".into(), ACCENT))).on_hover_text(tip).clicked() {
                self.updater.check_now();
            }
        }
        true
    }

    // -------------------------------------------------------------- bottom

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::Panel::bottom("status")
            .exact_size(28.0)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(14, 4)).stroke(Stroke::new(1.0, LINE)))
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    self.update_status(ui);
                    if self.toast.as_ref().is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(5)) {
                        self.toast = None;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        self.account_status(ui);
                        if let Some((t, _)) = &self.toast {
                            ui.add_space(12.0);
                            ui.label(RichText::new(t.clone()).color(AMBER).size(12.0));
                        }
                    });
                });
            });

        if self.sessions.is_empty() {
            return;
        }
        egui::Panel::bottom("sessions")
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::symmetric(14, 8)).stroke(Stroke::new(1.0, LINE)))
            .show(ui, |ui| {
                ui.label(RichText::new("OPEN CONNECTIONS").size(11.0).color(MUTED));
                let mut close = None;
                for s in &mut self.sessions {
                    let shared = s.handle.shared.lock().unwrap();
                    let (dot, status) = match &shared.state {
                        ConnState::Connecting => (AMBER, "connecting...".to_string()),
                        ConnState::Live => {
                            (OK, format!("{} screen(s) · {:.0} fps · {:.0} KB/s", shared.monitors.len(), shared.fps, shared.kbps))
                        }
                        ConnState::Closed(r) => (DANGER, r.clone()),
                    };
                    let notice = shared.notice.clone();
                    let mons = shared.monitors.len();
                    let vo = shared.view_only;
                    let locked = shared.view_locked;
                    let live = shared.state == ConnState::Live;
                    drop(shared);
                    ui.horizontal(|ui| {
                        status_dot(ui, dot);
                        ui.label(RichText::new(&s.name).strong());
                        if live {
                            if let Some(v) = view_only_toggle(ui, vo, locked) {
                                s.handle.set_view_only(v);
                            }
                        } else {
                            badge(ui, vo);
                        }
                        ui.label(RichText::new(status).color(MUTED));
                        if let Some(n) = notice {
                            ui.label(RichText::new(n).color(AMBER));
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.button("Disconnect").clicked() {
                                close = Some(s.id);
                            }
                            if !vo && mons > 0 && ui.button("Files").clicked() {
                                s.open_files();
                            }
                            if (s.files_only || !s.closed.is_empty()) && mons > 0 && ui.button("Show screens").clicked() {
                                s.files_only = false;
                                s.closed.clear();
                                s.builders.clear();
                            }
                        });
                    });
                }
                if let Some(id) = close {
                    self.sessions.retain(|s| s.id != id);
                    ctx.request_repaint();
                }
            });
    }

    // -------------------------------------------------------------- machine list

    fn machine_list(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("machines")
            .exact_size(450.0)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::same(12)).stroke(Stroke::new(1.0, LINE)))
            .show(ui, |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search computers").desired_width(f32::INFINITY));
                ui.add_space(6.0);
                let Some(ov) = &self.overview else {
                    ui.add_space(20.0);
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        ui.label(RichText::new("Loading computers...").color(MUTED));
                    });
                    return;
                };
                let q = self.search.to_lowercase();
                let online = ov.machines.iter().filter(|m| m.online).count();
                ui.label(RichText::new(format!("{online} ONLINE OF {}", ov.machines.len())).size(11.0).color(MUTED));
                // Helpers behind the published version that can be updated right now.
                let outdated = ov.machines.iter().filter(|m| m.online && !m.up_to_date && !m.updating).count();
                let mut update_all = false;
                if ov.role == "admin" && outdated > 0 {
                    let text = format!("{outdated} helper{} out of date · Update all", if outdated == 1 { " is" } else { "s are" });
                    update_all = ui
                        .add_enabled(self.job_update.is_none(), egui::Link::new(RichText::new(text).size(12.0).color(AMBER)))
                        .on_hover_text(format!("Install {} on every connected computer with an older helper", ov.release_version.as_deref().unwrap_or("?")))
                        .clicked();
                }
                ui.add_space(2.0);
                let mut clicked = None;
                let mut rename_start: Option<(String, String)> = None;
                let mut notes_start: Option<NotesEdit> = None;
                let can_rename = ov.role == "admin";
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for m in ov.machines.iter().filter(|m| {
                        q.is_empty()
                            || format!("{} {} {} {}", m.machine.name, m.machine.hostname, m.machine.os, m.machine.notes).to_lowercase().contains(&q)
                    }) {
                        let selected = self.selected.as_deref() == Some(m.machine.id.as_str());
                        let in_session = self.sessions.iter().any(|s| s.machine_id.as_deref() == Some(m.machine.id.as_str()));
                        let mut resp = machine_row(ui, m, selected, in_session, ov.now);
                        if !m.up_to_date {
                            let state = if m.updating { "updating" } else if m.update_error.is_some() { "update failed" } else { "out of date" };
                            resp = resp.on_hover_text(format!("Helper {}: {state}", m.machine.version));
                        }
                        if resp.clicked() {
                            clicked = Some(m.machine.id.clone());
                        }
                        // Right click: rename (administrator only, as on the server).
                        if can_rename {
                            if resp.double_clicked() {
                                rename_start = Some((m.machine.id.clone(), m.machine.name.clone()));
                            }
                            resp.context_menu(|ui| {
                                if ui.button("Rename...").clicked() {
                                    rename_start = Some((m.machine.id.clone(), m.machine.name.clone()));
                                    ui.close();
                                }
                                if ui.button("Notes...").clicked() {
                                    notes_start = Some(NotesEdit {
                                        machine_id: m.machine.id.clone(),
                                        name: m.machine.name.clone(),
                                        text: m.machine.notes.clone(),
                                        saved: m.machine.notes.clone(),
                                    });
                                    ui.close();
                                }
                            });
                        }
                    }
                    if ov.machines.is_empty() {
                        ui.label(
                            RichText::new("No computers yet. Send the install link from the web admin to a computer to add it.")
                                .color(MUTED),
                        );
                    }
                });
                if let Some(id) = clicked {
                    if self.selected.as_ref() != Some(&id) {
                        self.grant = GrantForm::default();
                    }
                    self.selected = Some(id);
                }
                if notes_start.is_some() {
                    self.notes_edit = notes_start;
                }
                if let Some((id, name)) = rename_start {
                    self.renaming = Some(Renaming { machine_id: id, text: name, focus: true });
                }
                if update_all {
                    self.start_update(ui.ctx(), None);
                }
            });
    }

    /// Helper version vs. the published one, with "Update now" for the administrator.
    fn helper_update_row(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, mv: &MachineView) {
        let Some(ov) = &self.overview else { return };
        let Some(release) = ov.release_version.clone() else { return };
        let is_admin = ov.role == "admin";
        let m = &mv.machine;
        if m.version == release {
            return;
        }
        ui.horizontal(|ui| {
            if mv.updating {
                ui.add(egui::Spinner::new().size(12.0));
                ui.label(RichText::new(format!("Updating the helper to {release}...")).color(ACCENT));
            } else if let Some(e) = &mv.update_error {
                ui.label(RichText::new(format!("Update to {release} failed: {e}")).color(DANGER));
            } else if !mv.up_to_date {
                ui.label(RichText::new(format!("Helper {} is older than the published {release}", m.version)).color(AMBER));
            } else {
                ui.label(RichText::new(format!("Helper {} is newer than the published {release}", m.version)).color(MUTED));
            }
            if is_admin && mv.online && !mv.updating {
                let text = if mv.update_error.is_some() { "Try again" } else { "Update now" };
                let button = ui
                    .add_enabled(self.job_update.is_none(), egui::Button::new(text).small())
                    .on_hover_text(format!("Install {release} on this computer now; the helper restarts (open sessions reconnect)"));
                if button.clicked() {
                    self.start_update(ctx, Some(m.id.clone()));
                }
            }
        });
    }

    /// Ask the server to update one helper (`Some(id)`) or every outdated one.
    fn start_update(&mut self, ctx: &egui::Context, machine_id: Option<String>) {
        if self.demo {
            self.toast("Sample computers can't be updated");
            return;
        }
        let Some(api) = self.api() else { return };
        self.job_update = Some(spawn_job(ctx, move || match machine_id {
            Some(id) => api.update_machine(&id).map(|v| format!("Updating to {v}; the helper restarts in a moment")),
            None => api.update_all().map(|n| match n {
                0 => "Every connected helper is up to date".to_string(),
                1 => "Updating 1 computer".to_string(),
                n => format!("Updating {n} computers"),
            }),
        }));
    }

    /// The "Rename computer" dialog.
    fn rename_dialog(&mut self, ctx: &egui::Context) {
        let Some(r) = self.renaming.as_mut() else { return };
        let mut save = false;
        let mut cancel = false;
        let modal = egui::Modal::new(egui::Id::unique("rename-computer")).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.label(RichText::new("Rename computer").size(17.0).strong());
            ui.label(RichText::new("The new name shows everywhere: web page, every app and temporary links.").color(MUTED));
            ui.add_space(6.0);
            let edit = ui.add(egui::TextEdit::singleline(&mut r.text).desired_width(f32::INFINITY));
            if r.focus {
                edit.request_focus();
                r.focus = false;
            }
            let valid = crate::grants::valid_machine_name(&r.text);
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && valid {
                save = true;
            }
            if !valid {
                ui.label(RichText::new("Use 1 to 80 characters.").color(DANGER).size(12.0));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(valid, egui::Button::new(RichText::new("Save").strong().color(Color32::from_rgb(6, 32, 25))).fill(ACCENT)).clicked() {
                    save = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
        if modal.should_close() {
            cancel = true;
        }
        if save {
            let Some(r) = self.renaming.take() else { return };
            let name = r.text.trim().to_string();
            // Show the new name right away; the server confirms on the next refresh.
            if let Some(m) = self.overview.as_mut().and_then(|o| o.machines.iter_mut().find(|m| m.machine.id == r.machine_id)) {
                m.machine.name = name.clone();
            }
            for s in self.sessions.iter_mut().filter(|s| s.machine_id.as_deref() == Some(r.machine_id.as_str())) {
                s.name = name.clone();
            }
            if !self.demo
                && let Some(api) = self.api()
            {
                let id = r.machine_id.clone();
                self.job_rename = Some(spawn_job(ctx, move || api.rename(&id, &name)));
            }
        } else if cancel {
            self.renaming = None;
        }
    }

    // -------------------------------------------------------------- central

    fn central(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(Margin::same(20))).show(ui, |ui| {
            if !self.signed_in() && !self.demo {
                self.login_card(ui);
                return;
            }
            let machine = self
                .overview
                .as_ref()
                .and_then(|o| o.machines.iter().find(|m| Some(&m.machine.id) == self.selected.as_ref()).cloned());
            let Some(m) = machine else {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("Choose a computer on the left").color(MUTED).size(16.0));
                });
                return;
            };
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                self.machine_details(&ctx, ui, &m);
            });
        });
    }

    fn login_card(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.vertical_centered(|ui| {
            ui.add_space(50.0);
            card(ui, 420.0, |ui| {
                ui.label(RichText::new("Connect to your Farlink server").size(20.0).strong());
                ui.label(RichText::new("The address of your server and your sign-in. People see only their own computers.").color(MUTED));
                ui.add_space(10.0);
                ui.label(RichText::new("Server address").color(MUTED));
                ui.add(egui::TextEdit::singleline(&mut self.server_input).hint_text("remote.example.com").desired_width(f32::INFINITY));
                ui.label(RichText::new("Your sign-in name (leave empty for the administrator)").color(MUTED));
                ui.add(egui::TextEdit::singleline(&mut self.cfg.user_name).hint_text("e.g. anna").desired_width(f32::INFINITY));
                ui.label(RichText::new("Password").color(MUTED));
                let pw = ui.add(egui::TextEdit::singleline(&mut self.password_input).password(true).desired_width(f32::INFINITY));
                ui.add_space(8.0);
                let busy = self.job_login.is_some();
                let go = primary_button(ui, if busy { "Signing in..." } else { "Sign in" }).clicked()
                    || (pw.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if go && !busy {
                    let server = normalize_server_url(&self.server_input);
                    self.server_input = server.clone();
                    self.cfg.server_url = server.clone();
                    let pwd = self.password_input.clone();
                    let user = self.cfg.user_name.trim().to_lowercase();
                    self.job_login = Some(spawn_job(&ctx, move || Api::login(&server, &user, &pwd)));
                }
                if let Some(e) = &self.overview_err {
                    ui.label(RichText::new(e).color(DANGER));
                }
                ui.add_space(12.0);
                ui.separator();
                ui.label(RichText::new("Got a temporary link? Paste it here:").color(MUTED));
                ui.horizontal(|ui| {
                    let resp = ui.add(egui::TextEdit::singleline(&mut self.link_input).hint_text("https://.../g/...").desired_width(300.0));
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.button("Open link").clicked() || enter {
                        let l = self.link_input.clone();
                        self.open_link(&ctx, &l);
                    }
                });
                if ui.button("Try with sample computers").clicked() {
                    self.demo = true;
                    self.overview = Some(sample_overview());
                    self.selected = Some("demo-office".into());
                }
                if !self.cfg.recent_links.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new("Recent links").color(MUTED));
                    for l in self.cfg.recent_links.clone() {
                        if ui.link(&l).clicked() {
                            self.open_link(&ctx, &l);
                        }
                    }
                }
            });
        });
    }

    fn machine_details(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, mv: &MachineView) {
        let m = &mv.machine;
        ui.horizontal(|ui| {
            os_glyph(ui, &m.os, 44.0);
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&m.name).size(24.0).strong());
                    // Same dot as in the list: green online, grey offline.
                    let (c, t) = if mv.online { (OK, "Online") } else { (Color32::from_rgb(89, 97, 109), "Offline") };
                    let (r, resp) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 5.5, c);
                    resp.on_hover_text(t);
                });
                let seen = if mv.online {
                    String::new()
                } else {
                    let now = self.overview.as_ref().map_or(now_secs(), |o| o.now);
                    format!(" · last seen {} ago", human_duration(now.saturating_sub(m.last_seen)))
                };
                ui.label(RichText::new(format!("{} · {} · helper {}{seen}", m.hostname, m.os, m.version)).color(MUTED));
                if let Some(owner) = &mv.owner_name {
                    ui.label(RichText::new(format!("Belongs to {owner}: only {owner} and the administrator can reach it")).color(AMBER));
                }
                self.helper_update_row(ctx, ui, mv);
            });
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            // An open connection to this computer: one "Disconnect" instead of
            // the two connect buttons (mode is switched in the bottom bar).
            if let Some(idx) = self.sessions.iter().position(|s| s.machine_id.as_deref() == Some(m.id.as_str())) {
                let b = egui::Button::new(RichText::new("Disconnect").size(15.0).strong().color(Color32::from_rgb(40, 8, 8)))
                    .fill(DANGER)
                    .min_size(Vec2::new(130.0, 34.0));
                if ui.add(b).clicked() {
                    self.sessions.remove(idx);
                    ctx.request_repaint();
                }
            } else {
                ui.add_enabled_ui(mv.online, |ui| {
                    if primary_button(ui, "Control").clicked() {
                        self.connect_machine(ctx, m, false);
                    }
                    if ui.add(egui::Button::new(RichText::new("View only").size(15.0)).min_size(Vec2::new(130.0, 34.0))).clicked() {
                        self.connect_machine(ctx, m, true);
                    }
                    if ui
                        .add(egui::Button::new(RichText::new("Files").size(15.0)).min_size(Vec2::new(100.0, 34.0)))
                        .on_hover_text("Copy files to and from this computer without opening its screens")
                        .clicked()
                    {
                        self.files_machine(ctx, m);
                    }
                });
            }
            let is_admin = self.overview.as_ref().is_none_or(|o| o.role == "admin");
            if is_admin
                && ui.add(egui::Button::new(RichText::new("Temporary link...").size(15.0)).min_size(Vec2::new(160.0, 34.0))).clicked()
            {
                self.grant.open = !self.grant.open;
                self.grant.result = None;
            }
        });
        if !mv.online {
            ui.label(RichText::new("This computer is switched off or not connected to the internet.").color(MUTED));
        }

        if self.grant.open {
            ui.add_space(10.0);
            self.grant_card(ctx, ui, m);
        }

        ui.add_space(14.0);
        // Double-clicking the screens picture does what "Control" does.
        if self.screens_card(ui, m, mv.online) {
            self.connect_machine(ctx, m, false);
        }

        if let Some(s) = mv.stats.as_ref().filter(|_| mv.online) {
            ui.add_space(14.0);
            stats_card(ui, s);
        }
    }

    fn grant_card(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, m: &MachineSummary) {
        card(ui, f32::INFINITY, |ui| {
            ui.label(RichText::new("Temporary access").size(16.0).strong());
            ui.label(RichText::new("Someone opens the link in a browser or in this app. You can withdraw it at any time.").color(MUTED));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("For");
                ui.add(egui::TextEdit::singleline(&mut self.grant.label).hint_text("e.g. Support for Ana").desired_width(200.0));
                ui.label("Allowed to");
                ui.radio_value(&mut self.grant.view_only, true, "only watch");
                ui.radio_value(&mut self.grant.view_only, false, "watch and control");
            });
            ui.horizontal(|ui| {
                ui.label("Works for");
                for (mins, label) in [(30, "30 min"), (120, "2 hours"), (480, "8 hours"), (1440, "1 day"), (10080, "1 week")] {
                    let sel = self.grant.minutes == mins;
                    if ui.add(egui::Button::new(label).selected(sel)).clicked() {
                        self.grant.minutes = mins;
                    }
                }
                ui.add_space(10.0);
                let busy = self.job_grant.is_some();
                if primary_button(ui, if busy { "Creating..." } else { "Create link" }).clicked() && !busy {
                    if self.demo {
                        self.grant.result = Some(format!("https://remote.example.com/g/{}", crate::util::random_token(10)));
                    } else if let Some(api) = self.api() {
                        let (id, mins, vo, label) = (m.id.clone(), self.grant.minutes, self.grant.view_only, self.grant.label.clone());
                        self.job_grant = Some(spawn_job(ctx, move || api.create_grant(&id, mins, vo, &label)));
                    }
                }
            });
            if let Some(url) = self.grant.result.clone() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&url).monospace().color(ACCENT));
                    if ui.button("Copy").clicked() {
                        ctx.copy_text(url.clone());
                        self.toast("Link copied");
                    }
                });
            }
            let active: Vec<_> = self
                .overview
                .as_ref()
                .map(|o| o.grants.iter().filter(|g| g.grant.machine_id == m.id && g.status == "active").cloned().collect())
                .unwrap_or_default();
            if !active.is_empty() {
                ui.add_space(6.0);
                ui.label(RichText::new("ACTIVE LINKS").size(11.0).color(MUTED));
                let now = self.overview.as_ref().map_or(now_secs(), |o| o.now);
                for g in active {
                    ui.horizontal(|ui| {
                        badge(ui, g.grant.view_only);
                        ui.label(if g.grant.label.is_empty() { "-" } else { &g.grant.label });
                        ui.label(RichText::new(format!("ends in {}", human_duration(g.grant.expires.saturating_sub(now)))).color(MUTED));
                        if ui.button("Copy").clicked() {
                            ctx.copy_text(g.url.clone());
                        }
                        if ui.add(egui::Button::new(RichText::new("Withdraw").color(DANGER))).clicked()
                            && let Some(api) = self.api()
                        {
                            let t = g.grant.token.clone();
                            self.job_revoke = Some(spawn_job(ctx, move || api.revoke(&t)));
                        }
                    });
                }
            }
        });
    }

    /// The administrator's notes about a computer in their own window (from
    /// the list's right-click menu), saved on the relay: the web admin shows
    /// and edits the same text. While nothing is typed the text follows the relay.
    fn notes_window(&mut self, ctx: &egui::Context) {
        let Some(e) = self.notes_edit.as_mut() else { return };
        let current = self.overview.as_ref().and_then(|o| o.machines.iter().find(|m| m.machine.id == e.machine_id));
        let Some(current) = current else {
            self.notes_edit = None; // the computer was removed
            return;
        };
        e.name = current.machine.name.clone();
        if e.text == e.saved && current.machine.notes != e.saved {
            e.text = current.machine.notes.clone();
            e.saved = current.machine.notes.clone();
        }
        let saving = self.job_notes.is_some();
        let (mut save, mut close) = (false, false);
        let vid = ViewportId::from_hash_of(("farlink-notes", &e.machine_id));
        let builder = ViewportBuilder::default()
            .with_title(format!("{} - Notes", e.name))
            .with_inner_size([520.0, 360.0])
            .with_min_inner_size([360.0, 240.0]);
        ctx.show_viewport_immediate(vid, builder, |ui, _| {
            if ui.ctx().input(|i| i.viewport().close_requested()) {
                close = true;
            }
            egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(Margin::same(16))).show(ui, |ui| {
                ui.label(RichText::new(format!("Notes about {}", e.name)).size(17.0).strong());
                ui.label(RichText::new("Only administrators see them. The web admin shows the same notes.").color(MUTED));
                ui.add_space(8.0);
                let dirty = e.text != e.saved;
                egui::Panel::bottom("notes-buttons").frame(egui::Frame::new().inner_margin(Margin::symmetric(0, 8))).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let b = egui::Button::new(RichText::new("Save").strong().color(Color32::from_rgb(6, 32, 25))).fill(ACCENT);
                        if ui.add_enabled(dirty && !saving, b).clicked() {
                            save = true;
                        }
                        if ui.button(if dirty { "Cancel" } else { "Close" }).clicked() {
                            close = true;
                        }
                    });
                });
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.add_sized(
                        ui.available_size(),
                        egui::TextEdit::multiline(&mut e.text)
                            .hint_text("Where it is, who uses it, anything to remember...")
                            .char_limit(crate::grants::MAX_NOTES),
                    );
                });
            });
        });
        if save {
            let id = e.machine_id.clone();
            let text = e.text.trim_end().to_string();
            self.notes_edit = None;
            if let Some(mv) = self.overview.as_mut().and_then(|o| o.machines.iter_mut().find(|x| x.machine.id == id)) {
                mv.machine.notes = text.clone();
            }
            if !self.demo
                && let Some(api) = self.api()
            {
                self.job_notes = Some(spawn_job(ctx, move || api.set_notes(&id, &text)));
            }
        } else if close {
            self.notes_edit = None;
        }
    }

    /// Returns true when the picture was double-clicked to connect (online, no open connection).
    fn screens_card(&mut self, ui: &mut egui::Ui, m: &MachineSummary, online: bool) -> bool {
        let mut connect = false;
        let session_idx = self.sessions.iter().position(|s| s.machine_id.as_deref() == Some(m.id.as_str()));
        let monitors = session_idx
            .map(|i| self.sessions[i].handle.shared.lock().unwrap().monitors.clone())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| m.monitors.clone());
        card(ui, f32::INFINITY, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Screens").size(16.0).strong());
                ui.label(RichText::new(format!("{} on this computer · each opens in its own window", monitors.len())).color(MUTED));
                if let Some(e) = self.preview.as_ref().filter(|p| p.machine_id == m.id).and_then(|p| p.error.as_ref()) {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(format!("Preview: {e}")).color(AMBER).size(12.5));
                    });
                }
            });
            ui.add_space(6.0);
            if monitors.is_empty() {
                ui.label(RichText::new("The screen layout appears after the first connection.").color(MUTED));
                return;
            }
            // Arrangement diagram with live pictures when connected.
            let (vx, vy, vw, vh) = crate::desktop::virtual_bounds(&monitors);
            let avail_w = ui.available_width().min(760.0);
            let scale = (avail_w / vw as f32).min(260.0 / vh as f32);
            let can_connect = online && session_idx.is_none();
            let (area, _) = ui.allocate_exact_size(Vec2::new(avail_w, vh as f32 * scale + 8.0), Sense::hover());
            let key = m.id.clone();
            let mut forget: Option<String> = None;
            let painter = ui.painter_at(area);
            let off = Vec2::new((avail_w - vw as f32 * scale) / 2.0, 4.0);
            for mon in &monitors {
                let r = Rect::from_min_size(
                    area.min + off + Vec2::new((mon.x - vx) as f32 * scale, (mon.y - vy) as f32 * scale),
                    Vec2::new(mon.width as f32 * scale, mon.height as f32 * scale),
                )
                .shrink(3.0);
                painter.rect_filled(r, CornerRadius::same(4), PANEL2);
                let tex = session_idx.and_then(|i| self.sessions[i].textures.get(&mon.index).map(|t| t.0.id())).or_else(|| {
                    let p = self.preview.as_ref().filter(|p| p.machine_id == m.id)?;
                    p.textures.get(&mon.index).map(|t| t.id())
                });
                if let Some(tex) = tex {
                    painter.image(tex, r.shrink(2.0), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                } else {
                    painter.rect_filled(r.shrink(2.0), CornerRadius::same(3), Color32::from_rgb(28, 50, 74));
                }
                let border = Stroke::new(1.5, if mon.primary { ACCENT } else { LINE });
                painter.rect_stroke(r, CornerRadius::same(4), border, StrokeKind::Inside);
                let label = format!("{}{}", mon.label(), if mon.primary { " · main" } else { "" });
                let galley = painter.layout_no_wrap(label, FontId::proportional(12.5), TEXT);
                let lp = r.left_bottom() + Vec2::new(8.0, -8.0 - galley.size().y);
                painter.rect_filled(
                    Rect::from_min_size(lp - Vec2::new(5.0, 2.0), galley.size() + Vec2::new(10.0, 4.0)),
                    CornerRadius::same(4),
                    Color32::from_black_alpha(180),
                );
                painter.galley(lp, galley, TEXT);
                // Double-click: connect with control. Right-click: where its window opens.
                let resp = ui.interact(r, ui.scope_id().with(("screen-preview", mon.index)), Sense::click());
                if can_connect && resp.double_clicked() {
                    connect = true;
                }
                let resp = if can_connect { resp.on_hover_text("Double-click to control this computer") } else { resp };
                let fp = monitor_fingerprint(mon);
                let place = self.layouts.get(&key, &fp);
                resp.context_menu(|ui| {
                    let text = match place {
                        Some(p) => format!(
                            "Window remembered at {:.0}, {:.0} · {:.0}×{:.0}{}",
                            p.x,
                            p.y,
                            p.w,
                            p.h,
                            if p.fullscreen { " · full screen" } else { "" }
                        ),
                        None => "Window not placed yet: opens side by side".to_string(),
                    };
                    ui.label(RichText::new(text).color(MUTED).size(12.0));
                    if ui.add_enabled(place.is_some(), egui::Button::new("Forget window position")).clicked() {
                        forget = Some(fp.clone());
                        ui.close();
                    }
                });
            }
            if let Some(fp) = forget {
                self.layouts.forget_monitor(&key, &fp);
                self.layouts_dirty = Some(Instant::now());
            }
        });
        connect
    }

    // -------------------------------------------------------------- monitor windows

    fn monitor_windows(&mut self, ctx: &egui::Context) {
        let local = ctx.input(|i| i.viewport().monitor_size).unwrap_or(Vec2::new(1920.0, 1080.0));
        let mut dirty = false;
        let mut to_close = Vec::new();
        let mut new_scale = None;
        let mut new_quality = None;
        let mut new_hidden = None;
        let mut files_moved = false;
        for s in &mut self.sessions {
            if std::mem::take(&mut s.controls_changed) {
                new_hidden = Some(s.controls_hidden);
            }
            if std::mem::take(&mut s.scale_changed) {
                new_scale = Some(s.scale);
            }
            if std::mem::take(&mut s.quality_changed) {
                new_quality = Some(s.quality);
            }
            let (monitors, live) = {
                let mut sh = s.handle.shared.lock().unwrap();
                // Text copied on the remote computer goes on this one's clipboard.
                if let Some(text) = sh.clipboard_in.take() {
                    ctx.copy_text(text);
                }
                (sh.monitors.clone(), sh.state == ConnState::Live)
            };
            if !live {
                continue;
            }
            let Some(key) = s.key.clone() else { continue };
            for mon in &monitors {
                if s.files_only || s.closed.contains(&mon.index) {
                    continue;
                }
                let fp = monitor_fingerprint(mon);
                let builder = s
                    .builders
                    .entry(mon.index)
                    .or_insert_with(|| {
                        let mut place = self.layouts.get(&key, &fp).unwrap_or_else(|| {
                            default_place(mon.index as usize, monitors.len(), mon.width, mon.height, (local.x, local.y))
                        });
                        if s.scale == ScaleMode::Original {
                            let size = original_window_size(mon, ctx.pixels_per_point(), Some(local), !s.controls_hidden);
                            (place.w, place.h) = (size.x, size.y);
                        }
                        // Full screen and maximised at once can't be left on GNOME:
                        // open in full screen only and maximise when leaving it.
                        if place.fullscreen && place.maximized {
                            place.maximized = false;
                            s.max_before_fullscreen.insert(mon.index);
                        }
                        ViewportBuilder::default()
                            .with_title(format!("{} - Screen {} ({}x{})", s.name, mon.index + 1, mon.width, mon.height))
                            .with_position([place.x, place.y])
                            .with_inner_size([place.w, place.h])
                            .with_min_inner_size([320.0, 200.0])
                            .with_fullscreen(place.fullscreen)
                            .with_maximized(place.maximized)
                    })
                    .clone();
                let vid = ViewportId::from_hash_of(("farlink-monitor", s.id, mon.index));
                let layouts = &mut self.layouts;
                let result = ctx.show_viewport_immediate(vid, builder, |ui, _class| monitor_window(ui, s, mon, &key, &fp, layouts));
                match result {
                    MonitorResult::Moved => dirty = true,
                    MonitorResult::Closed => {
                        dirty = true;
                        s.closed.insert(mon.index);
                    }
                    MonitorResult::Disconnect => to_close.push(s.id),
                    MonitorResult::Nothing => {}
                }
            }
            // Tell the helper which screens are actually visible here; it stops
            // capturing closed or minimised ones.
            let visible: Vec<u8> =
                monitors.iter().map(|m| m.index).filter(|i| !s.files_only && !s.closed.contains(i) && !s.minimized.contains(i)).collect();
            if s.last_watch.as_ref() != Some(&visible) {
                s.handle.send(SessionMsg::Watch { monitors: visible.clone() });
                s.last_watch = Some(visible);
            }
            if s.files.is_some() {
                let vid = ViewportId::from_hash_of(("farlink-files", s.id));
                let builder = ViewportBuilder::default()
                    .with_title(format!("{} - Files", s.name))
                    .with_inner_size([980.0, 620.0])
                    .with_min_inner_size([640.0, 360.0]);
                let closed = ctx.show_viewport_immediate(vid, builder, |ui, _| files_window(ui, s));
                // Remember where the browser is, here and on that computer.
                let (local_changed, remote_changed) = s.note_files();
                if local_changed {
                    self.cfg.files_local.clone_from(&s.files_local);
                    files_moved = true;
                }
                if remote_changed && let Some(key) = &s.key {
                    self.cfg.files_remote.insert(key.clone(), s.files_remote.clone());
                    files_moved = true;
                }
                if closed {
                    s.files = None;
                }
            } else {
                // Another connection's browser may have moved on since.
                s.files_local.clone_from(&self.cfg.files_local);
            }
            if (s.files_only || (!monitors.is_empty() && monitors.iter().all(|m| s.closed.contains(&m.index)))) && s.files.is_none() {
                to_close.push(s.id);
            }
        }
        if !to_close.is_empty() {
            self.sessions.retain(|s| !to_close.contains(&s.id));
        }
        if files_moved {
            self.save_cfg();
        }
        if let Some(q) = new_quality
            && q != self.cfg.quality
        {
            self.cfg.quality = q;
            self.save_cfg();
        }
        if let Some(h) = new_hidden {
            for s in &mut self.sessions {
                s.controls_hidden = h;
            }
            if h != self.cfg.hide_controls {
                self.cfg.hide_controls = h;
                self.save_cfg();
            }
        }
        if let Some(m) = new_scale
            && m != self.cfg.scale_mode
        {
            self.cfg.scale_mode = m;
            self.save_cfg();
        }
        if dirty && self.layouts_dirty.is_none() {
            self.layouts_dirty = Some(Instant::now());
        }
        if let Some(t) = self.layouts_dirty
            && t.elapsed() > Duration::from_millis(600)
        {
            if let Some(p) = &self.layouts_path
                && let Err(e) = self.layouts.save(p)
            {
                log::warn!("saving window positions: {e}");
            }
            self.layouts_dirty = None;
        }
        if self.layouts_dirty.is_some() {
            ctx.request_repaint_after(Duration::from_millis(700));
        }
    }

    fn screenshot_step(&mut self, ctx: &egui::Context) {
        let Some(path) = self.opts.screenshot.clone() else { return };
        self.frames += 1;
        ctx.request_repaint();
        if self.frames >= 120 && !self.shot_requested {
            self.shot_requested = true;
            let done = self.shot_done.clone();
            let c = ctx.clone();
            ctx.send_viewport_cmd_to(
                ViewportId::ROOT,
                ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                    let [w, h] = img.size;
                    let bytes: Vec<u8> = img.pixels.iter().flat_map(|p| p.to_array()).collect();
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    if let Err(e) = image::save_buffer(&path, &bytes, w as u32, h as u32, image::ExtendedColorType::Rgba8) {
                        eprintln!("screenshot failed: {e}");
                    }
                    done.store(true, Ordering::SeqCst);
                    c.request_repaint();
                })),
            );
        }
        if self.shot_done.load(Ordering::SeqCst) || self.frames > 600 {
            self.sessions.clear();
            ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Close);
        }
    }
}

/// Keeps this app up to date from the server: checks at start-up, every
/// [`UPDATE_EVERY`] and when asked, and installs a newer version (it is used
/// after a restart).
#[derive(Default)]
struct Updater {
    shared: Arc<(Mutex<UpdaterState>, std::sync::Condvar)>,
}

const UPDATE_EVERY: Duration = Duration::from_secs(3600);

#[derive(Default, Clone)]
struct UpdaterState {
    server: String,
    started: bool,
    /// Wake the thread for a check now.
    wanted: bool,
    checking: bool,
    /// Short result of the last check ("up to date", an error...).
    text: String,
    error: bool,
    last_check: Option<Instant>,
    /// Version installed next to the running one, waiting for a restart.
    installed: Option<String>,
    /// App installed from the .deb: a newer package downloaded and waiting
    /// for "Install" (it asks for the password, so never on its own).
    deb: Option<(String, PathBuf)>,
    /// The .deb is being installed (password dialog / apt running).
    installing: bool,
}

impl Updater {
    fn start(&self, ctx: &egui::Context, server: &str, auto: bool) {
        {
            let mut st = self.shared.0.lock().unwrap();
            st.server = server.to_string();
            st.started = true;
            st.wanted = auto; // first check right away (unless switched off)
        }
        let shared = self.shared.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let (lock, cv) = &*shared;
            loop {
                let server = {
                    let mut st = lock.lock().unwrap();
                    loop {
                        let due = auto && st.last_check.is_none_or(|t| t.elapsed() >= UPDATE_EVERY);
                        if st.wanted || due {
                            break;
                        }
                        let wait = st.last_check.map_or(UPDATE_EVERY, |t| UPDATE_EVERY.saturating_sub(t.elapsed()));
                        st = cv.wait_timeout(st, wait.max(Duration::from_secs(1))).unwrap().0;
                    }
                    st.wanted = false;
                    st.checking = true;
                    st.server.clone()
                };
                ctx.request_repaint();
                let (text, error, installed) = if server.is_empty() {
                    ("sign in to check for updates".to_string(), false, None)
                } else {
                    let already = {
                        let st = lock.lock().unwrap();
                        st.installed.clone().or_else(|| st.deb.as_ref().map(|d| d.0.clone()))
                    };
                    match check_and_install(&server, already.as_deref()) {
                        Ok(Checked::Installed(v)) => (String::new(), false, Some(v)),
                        Ok(Checked::DebReady(v, path)) => {
                            lock.lock().unwrap().deb = Some((v, path));
                            (String::new(), false, None)
                        }
                        // Nothing to say when up to date (the link's hover tells when it last looked).
                        Ok(Checked::UpToDate) => (String::new(), false, None),
                        Err(e) => {
                            log::warn!("update check: {e:#}");
                            ("could not check for updates".to_string(), true, None)
                        }
                    }
                };
                {
                    let mut st = lock.lock().unwrap();
                    st.checking = false;
                    st.text = text;
                    st.error = error;
                    st.last_check = Some(Instant::now());
                    if installed.is_some() {
                        st.installed = installed;
                    }
                }
                ctx.request_repaint();
            }
        });
    }

    fn started(&self) -> bool {
        self.shared.0.lock().unwrap().started
    }

    fn status(&self) -> UpdaterState {
        self.shared.0.lock().unwrap().clone()
    }

    fn set_server(&self, server: &str) {
        self.shared.0.lock().unwrap().server = server.to_string();
    }

    fn check_now(&self) {
        let mut st = self.shared.0.lock().unwrap();
        st.wanted = true;
        st.checking = true; // shown at once, before the thread wakes
        self.shared.1.notify_all();
    }

    /// Install the downloaded .deb (asks for the password).
    fn install_deb(&self, ctx: &egui::Context) {
        let (version, path) = {
            let mut st = self.shared.0.lock().unwrap();
            let Some(deb) = st.deb.clone() else { return };
            if st.installing {
                return;
            }
            st.installing = true;
            st.text.clear();
            deb
        };
        let shared = self.shared.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = crate::update::install_deb(&path);
            let mut st = shared.0.lock().unwrap();
            st.installing = false;
            match result {
                Ok(()) => {
                    log::info!("installed farlink {} -> {version} (.deb)", crate::VERSION);
                    let _ = std::fs::remove_file(&path);
                    st.deb = None;
                    st.installed = Some(version);
                }
                Err(e) => {
                    log::warn!("installing {}: {e:#}", path.display());
                    st.text = format!("not installed: {e:#}");
                    st.error = true;
                }
            }
            drop(st);
            ctx.request_repaint();
        });
    }
}

enum Checked {
    UpToDate,
    /// The program file was replaced: restart to use it.
    Installed(String),
    /// App from the .deb: the newer package is downloaded, see [`Updater::install_deb`].
    DebReady(String, PathBuf),
}

/// Install the server's version if it is newer than the running one and not
/// installed (or downloaded) already. An app installed from the .deb gets the
/// newer .deb instead of its bare program file.
fn check_and_install(server: &str, already: Option<&str>) -> anyhow::Result<Checked> {
    let target = crate::util::current_target();
    let installed = already.unwrap_or(crate::VERSION);
    let exe = app_exe()?;
    if crate::update::installed_from_deb(&exe) {
        let Some(info) = crate::update::check(server, crate::util::DEB_APP, target, installed)? else {
            return Ok(Checked::UpToDate);
        };
        let path = crate::update::download_deb(server, &info)?;
        log::info!("farlink {} downloaded: {}", info.version, path.display());
        return Ok(Checked::DebReady(info.version, path));
    }
    let Some(info) = crate::update::check(server, "farlink", target, installed)? else {
        return Ok(Checked::UpToDate);
    };
    let bytes = crate::update::download(server, &info)?; // checks size and SHA-256
    crate::update::replace_exe(&exe, &bytes)?;
    log::info!("updated farlink {} -> {}", crate::VERSION, info.version);
    Ok(Checked::Installed(info.version))
}

/// Start the (updated) program again and close this one. The restart is
/// asked for in the open window, so the new one starts shown too.
fn restart_app(ctx: &egui::Context) {
    let args = std::env::args_os().skip(1).filter(|a| a != crate::autostart::TRAY_SWITCH);
    match app_exe().and_then(|exe| std::process::Command::new(exe).args(args).spawn()) {
        Ok(_) => quit_app(ctx),
        Err(e) => log::warn!("restart: {e}"),
    }
}

/// Really close the app (not just hide it to the tray).
fn quit_app(ctx: &egui::Context) {
    QUITTING.store(true, Ordering::SeqCst);
    ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Close);
}

enum MonitorResult {
    Nothing,
    Moved,
    Closed,
    Disconnect,
}

/// Contents of one remote-monitor window.
fn monitor_window(ui: &mut egui::Ui, s: &mut OpenSession, mon: &MonitorInfo, key: &str, fp: &str, layouts: &mut LayoutStore) -> MonitorResult {
    let ctx = ui.ctx().clone();
    let mut result = MonitorResult::Nothing;
    let info = ctx.input(|i| i.viewport().clone());
    let ppp = ctx.pixels_per_point();
    let bar = !s.controls_hidden;
    let applied = s.applied_scale.insert(mon.index, (s.scale, bar));
    if applied.is_some_and(|a| a != (s.scale, bar))
        && s.scale == ScaleMode::Original
        && !info.fullscreen.unwrap_or(false)
        && !info.maximized.unwrap_or(false)
    {
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(original_window_size(mon, ppp, info.monitor_size, bar)));
    }
    if let (Some(outer), Some(inner)) = (info.outer_rect, info.inner_rect) {
        let fullscreen = info.fullscreen.unwrap_or(false);
        let maximized = info.maximized.unwrap_or(false);
        let place = match layouts.get(key, fp) {
            // Keep the normal size/position while full screen or maximised
            // (and that it goes back to maximised after full screen).
            Some(old) if fullscreen || maximized => {
                let maximized = if fullscreen { s.max_before_fullscreen.contains(&mon.index) } else { maximized };
                WindowPlace { fullscreen, maximized, ..old }
            }
            _ => WindowPlace { x: outer.min.x, y: outer.min.y, w: inner.width(), h: inner.height(), fullscreen, maximized },
        };
        if layouts.set(key, fp, place) {
            result = MonitorResult::Moved;
        }
    }
    if info.minimized.unwrap_or(false) {
        s.minimized.insert(mon.index);
    } else {
        s.minimized.remove(&mon.index);
    }
    if info.close_requested() {
        return MonitorResult::Closed;
    }
    let (view_only, locked, fps, kbps) = {
        let sh = s.handle.shared.lock().unwrap();
        (sh.view_only, sh.view_locked, sh.fps, sh.kbps)
    };
    let screen = ui.max_rect();
    let overlay = if s.controls_hidden {
        let r = s.reveal.entry(mon.index).or_default();
        let now = Instant::now();
        r.step(ctx.pointer_hover_pos().map(|p| p.y - screen.min.y), screen.height(), egui::Popup::is_any_open(&ctx), now);
        if let Some(wait) = r.wait(now) {
            ctx.request_repaint_after(wait);
        }
        r.shown.then(|| Rect::from_min_size(screen.min, Vec2::new(screen.width(), BAR_HEIGHT)))
    } else {
        s.reveal.remove(&mon.index);
        None
    };
    if !s.controls_hidden {
        egui::Panel::top("bar")
            .exact_size(BAR_HEIGHT)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(10, 5)))
            .show(ui, |ui| monitor_bar(ui, s, mon, view_only, locked, fps, kbps, &mut result));
    } else if let Some(bar) = overlay {
        egui::Area::new(egui::Id::unique(("farlink-bar", mon.index))).order(egui::Order::Foreground).fixed_pos(bar.min).show(&ctx, |ui| {
            egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(10, 5)).show(ui, |ui| {
                let size = Vec2::new(bar.width() - 20.0, BAR_HEIGHT - 10.0);
                ui.allocate_ui_with_layout(size, Layout::left_to_right(Align::Center), |ui| {
                    ui.set_min_size(size);
                    monitor_bar(ui, s, mon, view_only, locked, fps, kbps, &mut result);
                });
            });
        });
    }
    egui::CentralPanel::default().frame(egui::Frame::new().fill(Color32::BLACK)).show(ui, |ui| {
        let avail = ui.available_rect_before_wrap();
        let Some((tex, _)) = s.textures.get(&mon.index) else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Waiting for the picture...").color(MUTED));
            });
            return;
        };
        let (tex, size) = (tex.id(), tex.size_vec2());
        let sense = if view_only { Sense::hover() } else { Sense::click_and_drag() };
        // One remote pixel = one physical pixel here.
        let original = Vec2::new(mon.width.max(1) as f32, mon.height.max(1) as f32) / ppp;
        match s.scale {
            ScaleMode::Fit => {
                let rect = fit_rect(avail, size.x, size.y);
                let resp = ui.allocate_rect(avail, sense);
                screen_picture(ui, s, mon, tex, rect, below(avail, overlay), resp.hovered(), view_only);
            }
            ScaleMode::Original if original.x <= avail.width() + 0.5 && original.y <= avail.height() + 0.5 => {
                let rect = snap_rect(Rect::from_center_size(avail.center(), original), ppp);
                let resp = ui.allocate_rect(avail, sense);
                screen_picture(ui, s, mon, tex, rect, below(avail, overlay), resp.hovered(), view_only);
            }
            ScaleMode::Original => {
                // Bigger than the window: scroll bars (not the wheel — that goes to
                // the remote computer), kept beside the picture rather than over it.
                ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
                egui::ScrollArea::both()
                    .scroll_source(egui::containers::scroll_area::ScrollSource::SCROLL_BAR)
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        let (outer, resp) = ui.allocate_exact_size(original.max(ui.available_size()), sense);
                        let rect = snap_rect(Rect::from_center_size(outer.center(), original), ppp);
                        let visible = below(ui.clip_rect(), overlay);
                        screen_picture(ui, s, mon, tex, rect, visible, resp.hovered(), view_only);
                    });
            }
        }
    });
    result
}

/// The controls of a screen window (top bar, or shown over the picture
/// while hidden).
#[allow(clippy::too_many_arguments)]
fn monitor_bar(ui: &mut egui::Ui, s: &mut OpenSession, mon: &MonitorInfo, view_only: bool, locked: bool, fps: f32, kbps: f32, result: &mut MonitorResult) {
    let ctx = ui.ctx().clone();
    ui.horizontal_centered(|ui| {
        if let Some(v) = view_only_toggle(ui, view_only, locked) {
            s.handle.set_view_only(v);
        }
        ui.label(RichText::new(format!("{} · {}", s.name, mon.label())).strong());
        ui.label(RichText::new(format!("{fps:.0} fps · {kbps:.0} KB/s")).color(MUTED).size(12.0));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("Disconnect").clicked() {
                *result = MonitorResult::Disconnect;
            }
            let (label, hint) = if s.controls_hidden {
                ("Keep controls", "Keep these controls shown")
            } else {
                ("Hide controls", "Show only the remote screen. To get the controls back, rest the mouse near the top edge for a second")
            };
            if ui.button(label).on_hover_text(hint).clicked() {
                s.controls_hidden = !s.controls_hidden;
                s.controls_changed = true;
            }
            if !view_only && ui.button("Files").clicked() {
                s.open_files();
            }
            let (fs, maximized) = ctx.input(|i| (i.viewport().fullscreen.unwrap_or(false), i.viewport().maximized.unwrap_or(false)));
            if ui.button(if fs { "Exit full screen" } else { "Full screen" }).clicked() {
                for cmd in fullscreen_commands(fs, maximized, mon.index, &mut s.max_before_fullscreen) {
                    ctx.send_viewport_cmd(cmd);
                }
            }
            if !view_only && ui.button("Ctrl+Alt+Del").clicked() {
                s.handle.send(SessionMsg::Input { event: InputEvent::CtrlAltDel });
            }
            let before = s.quality;
            egui::ComboBox::from_id_salt("colors").selected_text(s.quality.color.short_label()).width(132.0).show_ui(ui, |ui| {
                for c in ColorDepth::ALL {
                    if ui.add(egui::Button::new(c.label()).selected(s.quality.color == c).frame(false)).clicked() {
                        s.quality.color = c;
                    }
                }
            });
            // Every picture setting, in a popup that stays open while sliders move.
            let current = s.quality.matching_preset();
            let button = dropdown_button(ui, current.label()).on_hover_text("Picture quality");
            egui::Popup::menu(&button).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                quality_controls(ui, &mut s.quality);
            });
            if s.quality != before {
                s.handle.send(SessionMsg::Quality { quality: s.quality });
                s.quality_changed = true;
            }
            egui::ComboBox::from_id_salt("scale").selected_text(s.scale.label()).width(110.0).show_ui(ui, |ui| {
                for m in [ScaleMode::Original, ScaleMode::Fit] {
                    if ui.add(egui::Button::new(m.label()).selected(s.scale == m).frame(false)).clicked() && s.scale != m {
                        s.scale = m;
                        s.scale_changed = true;
                    }
                }
            });
        });
    });
}

/// A button with a small down arrow (drawn: the font has no arrow glyph).
fn dropdown_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let resp = ui.button(format!("{text}     "));
    let c = Pos2::new(resp.rect.right() - 13.0, resp.rect.center().y);
    let color = ui.style().interact(&resp).fg_stroke.color;
    let tri = vec![c + Vec2::new(-4.0, -2.0), c + Vec2::new(4.0, -2.0), c + Vec2::new(0.0, 3.0)];
    ui.painter().add(egui::Shape::convex_polygon(tri, color, Stroke::NONE));
    resp
}

/// Picture settings of a connection (the quality popup of a screen window).
fn quality_controls(ui: &mut egui::Ui, q: &mut Quality) {
    ui.set_min_width(380.0);
    ui.label(RichText::new("Picture quality").strong());
    ui.label(RichText::new("Changes apply right away and are remembered for next time.").color(MUTED).size(12.0));
    ui.add_space(4.0);
    let current = q.matching_preset();
    ui.horizontal(|ui| {
        for p in Preset::BUTTONS {
            if ui.add(egui::Button::new(p.label()).selected(current == p)).on_hover_text(p.hint()).clicked() {
                *q = q.with_preset(p);
            }
        }
    });
    ui.label(RichText::new(q.matching_preset().hint()).color(MUTED).size(12.0));
    ui.add_space(6.0);
    egui::Grid::new("quality").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
        ui.label("Picture size");
        ui.add(egui::Slider::new(&mut q.scale_pct, 25..=100).suffix(" %"));
        ui.end_row();
        ui.label("Pictures per second");
        ui.add(egui::Slider::new(&mut q.max_fps, 1..=30));
        ui.end_row();
        ui.label("Compression");
        ui.horizontal(|ui| {
            for c in Codec::ALL {
                ui.radio_value(&mut q.codec, c, c.label());
            }
        });
        ui.end_row();
        if q.codec != Codec::Png {
            ui.label(if q.codec == Codec::Auto { "Photo quality" } else { "JPEG quality" });
            ui.add(egui::Slider::new(&mut q.jpeg_quality, 10..=95));
            ui.end_row();
        }
    });
    ui.label(
        RichText::new("Auto sends text and windows lossless and photos/video as JPEG, then sharpens them once they stop moving.")
            .color(MUTED)
            .size(12.0),
    );
}

/// Commands for the "Full screen" button of a screen window that is now in
/// full screen (`fs`) or maximised. GNOME ignores "leave full screen" for a
/// window that is also maximised, so a maximised window is un-maximised first
/// and maximised again when it leaves full screen.
fn fullscreen_commands(fs: bool, maximized: bool, index: u8, max_before: &mut HashSet<u8>) -> Vec<ViewportCommand> {
    if fs {
        let mut cmds = vec![ViewportCommand::Fullscreen(false)];
        if max_before.remove(&index) {
            cmds.push(ViewportCommand::Maximized(true));
        }
        cmds
    } else if maximized {
        max_before.insert(index);
        vec![ViewportCommand::Maximized(false), ViewportCommand::Fullscreen(true)]
    } else {
        max_before.remove(&index);
        vec![ViewportCommand::Fullscreen(true)]
    }
}

/// Height of a screen window's controls.
const BAR_HEIGHT: f32 = 36.0;
/// Hidden controls come back after the mouse rests this long...
const REVEAL_AFTER: Duration = Duration::from_secs(1);
/// ...in the top part of the window this big (share of its height)...
const REVEAL_EDGE: f32 = 0.03;
/// ...but never thinner than this (points).
const REVEAL_EDGE_MIN: f32 = 4.0;

/// Hidden controls of one screen window, brought back by resting the mouse
/// at the top edge for [`REVEAL_AFTER`]; they go again when it moves away.
#[derive(Default, Debug)]
struct Reveal {
    at_top_since: Option<Instant>,
    shown: bool,
    /// Last mouse height seen in the window.
    last_y: Option<f32>,
    /// The mouse left through the top: it is on the window's title bar
    /// (the app sees no mouse there), which counts as the top edge.
    on_title: bool,
}

/// Leaving the window this close to its top means going onto the title bar (points).
const TITLE_EXIT: f32 = 60.0;

impl Reveal {
    /// `y`: mouse height in the window (`None`: outside it); `height`: the
    /// window's height; `popup`: a menu of the controls is open (they stay
    /// while it is).
    fn step(&mut self, y: Option<f32>, height: f32, popup: bool, now: Instant) {
        let edge = (height * REVEAL_EDGE).max(REVEAL_EDGE_MIN);
        match y {
            Some(_) => self.on_title = false,
            None => self.on_title |= self.last_y.is_some_and(|l| l <= TITLE_EXIT),
        }
        self.last_y = y;
        if self.shown {
            if !popup && !self.on_title && !y.is_some_and(|y| y <= BAR_HEIGHT + 12.0) {
                *self = Reveal::default();
            }
        } else if self.on_title || y.is_some_and(|y| y <= edge) {
            let since = *self.at_top_since.get_or_insert(now);
            self.shown = now.duration_since(since) >= REVEAL_AFTER;
        } else {
            self.at_top_since = None;
        }
    }

    /// How long until the controls are due to show (to look again then).
    fn wait(&self, now: Instant) -> Option<Duration> {
        let since = self.at_top_since.filter(|_| !self.shown)?;
        Some(REVEAL_AFTER.saturating_sub(now.duration_since(since)) + Duration::from_millis(10))
    }
}

/// The part of `visible` not covered by controls shown over the picture
/// (clicks there are for the controls, not the remote computer).
fn below(visible: Rect, overlay: Option<Rect>) -> Rect {
    match overlay {
        Some(bar) => Rect::from_min_max(Pos2::new(visible.min.x, visible.min.y.max(bar.max.y)), visible.max),
        None => visible,
    }
}

/// Window size (in points) showing a remote screen at its original size,
/// limited to the local screen.
fn original_window_size(mon: &MonitorInfo, ppp: f32, screen: Option<Vec2>, bar: bool) -> Vec2 {
    let bar = if bar { BAR_HEIGHT } else { 0.0 };
    let mut size = Vec2::new(mon.width.max(1) as f32 / ppp, mon.height.max(1) as f32 / ppp + bar);
    if let Some(sc) = screen {
        size = size.min(Vec2::new(sc.x, sc.y - 60.0));
    }
    size.max(Vec2::new(320.0, 200.0)).round()
}

/// Move a rect onto whole physical pixels so a 1:1 picture stays sharp.
fn snap_rect(r: Rect, ppp: f32) -> Rect {
    let min = Pos2::new((r.min.x * ppp).round() / ppp, (r.min.y * ppp).round() / ppp);
    Rect::from_min_size(min, r.size())
}

/// Draw a remote screen in `rect` (only `visible` of it on screen) and send
/// this frame's input to the remote computer.
#[allow(clippy::too_many_arguments)]
fn screen_picture(
    ui: &mut egui::Ui,
    s: &mut OpenSession,
    mon: &MonitorInfo,
    tex: egui::TextureId,
    rect: Rect,
    visible: Rect,
    hovering: bool,
    view_only: bool,
) {
    let ctx = ui.ctx().clone();
    ui.painter().image(tex, rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
    let local = ctx.pointer_hover_pos().filter(|p| hovering && !view_only && rect.contains(*p) && visible.contains(*p));
    if local.is_some() {
        // Our own pointer is hidden: the remote one is drawn instead.
        ctx.set_cursor_icon(egui::CursorIcon::None);
    }
    draw_remote_pointer(ui, s, mon, rect, visible, local);
    if view_only {
        return;
    }
    let focused = ctx.input(|i| i.viewport().focused.unwrap_or(false));
    let (events, mods) = ctx.input(|i| (i.events.clone(), i.modifiers));
    // Text copied here reaches the remote clipboard before the keys that
    // paste it: on Ctrl+V, and when this window gets the keyboard (so the
    // remote right-click "Paste" works too).
    if focused && s.focused.insert(mon.index) && let Some(text) = crate::clipboard::system_get() {
        s.handle.send_clipboard(&text);
    } else if !focused {
        s.focused.remove(&mon.index);
    }
    if focused {
        for ev in &events {
            if let egui::Event::Paste(text) = ev {
                s.handle.send_clipboard(text);
            }
        }
    }
    // Alt+Tab & co. for the remote computer: while Alt is held the keys come
    // from the grab (see crate::keygrab), not from here.
    if let Some(held) = s.alt_grab.as_ref().and_then(|g| g.finished()) {
        s.alt_grab = None;
        // Alt is still down over there: let go of it as soon as egui has.
        s.mods = egui::Modifiers { alt: true, shift: held.shift, ctrl: held.ctrl, ..s.mods };
    }
    let grabbed = s.alt_grab.is_some();
    let alt_pressed = focused && !grabbed && mods.alt && !s.mods.alt && local.is_some();
    for ev in map_events(&events, rect, visible, mon.index, &mut s.mods, mods, focused && !grabbed) {
        match ev {
            InputEvent::Scroll { dx, dy } => {
                if !hovering {
                    continue;
                }
                s.scroll_acc += Vec2::new(dx, dy);
                let (sx, sy) = (s.scroll_acc.x.trunc(), s.scroll_acc.y.trunc());
                if sx != 0.0 || sy != 0.0 {
                    s.scroll_acc -= Vec2::new(sx, sy);
                    s.handle.send(SessionMsg::Input { event: InputEvent::Scroll { dx: sx, dy: sy } });
                }
            }
            other => s.handle.send(SessionMsg::Input { event: other }),
        }
    }
    if alt_pressed {
        let held = crate::keygrab::Held { shift: s.mods.shift, ctrl: s.mods.ctrl };
        s.alt_grab = crate::keygrab::AltGrab::start(s.handle.input_sender(), held);
    }
    if s.alt_grab.is_some() {
        // Look again for the grab's end (nothing else wakes the window then).
        ctx.request_repaint_after(Duration::from_millis(50));
    }
}

/// Draw the remote pointer, if the helper sends it separately (otherwise it
/// is part of the picture). While the mouse is over the picture in control
/// mode it is drawn right where the local mouse is, so it moves with no
/// delay at all; otherwise where the remote computer says it is.
fn draw_remote_pointer(ui: &egui::Ui, s: &OpenSession, mon: &MonitorInfo, rect: Rect, visible: Rect, local: Option<Pos2>) {
    let Some((rx, ry, shape)) = s.remote_pointer else { return };
    let Some((tex, [w, h, hx, hy])) = s.pointer_textures.get(&shape) else { return };
    let at = match local {
        Some(p) => p,
        None => {
            let (lx, ly) = (rx - mon.x, ry - mon.y);
            if lx < 0 || ly < 0 || lx >= mon.width as i32 || ly >= mon.height as i32 {
                return; // on another screen
            }
            rect.min + Vec2::new(lx as f32 / mon.width.max(1) as f32 * rect.width(), ly as f32 / mon.height.max(1) as f32 * rect.height())
        }
    };
    // Same size as on the remote screen, whatever the picture's scale.
    let ppp = ui.ctx().pixels_per_point();
    let size = Vec2::new(*w as f32, *h as f32) / ppp;
    let min = at - Vec2::new(*hx as f32, *hy as f32) / ppp;
    let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    ui.painter().with_clip_rect(visible).image(tex.id(), Rect::from_min_size(min, size), uv, Color32::WHITE);
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.tray_step(&ctx);
        self.poll_jobs(&ctx);
        self.preview_step(&ctx);
        if self.signed_in() && !self.demo && self.last_refresh.is_none_or(|t| t.elapsed() > Duration::from_secs(5)) {
            self.refresh(&ctx);
        }
        if self.signed_in() || !self.sessions.is_empty() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        for s in &mut self.sessions {
            s.update_textures(&ctx);
        }
        // Hidden switch for checking the file browser's look in a screenshot.
        if self.opts.screenshot.is_some() && std::env::var_os("FARLINK_SHOT_FILES").is_some() && !self.sessions.is_empty() {
            files_window(ui, &mut self.sessions[0]);
            self.screenshot_step(&ctx);
            return;
        }
        self.bottom_bar(ui);
        if self.signed_in() || self.demo {
            self.machine_list(ui);
        }
        self.central(ui);
        self.rename_dialog(&ctx);
        self.notes_window(&ctx);
        self.monitor_windows(&ctx);
        self.screenshot_step(&ctx);
    }
}

// ------------------------------------------------------------------ widgets

/// Small "● text" for the status bar, painted as one piece so it reads the
/// same inside right-to-left rows.
fn status_text(ui: &mut egui::Ui, dot: Color32, text: &str) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(12.0), MUTED);
    let size = Vec2::new(14.0 + galley.size().x, galley.size().y.max(16.0));
    let (r, resp) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().circle_filled(Pos2::new(r.min.x + 4.0, r.center().y), 3.5, dot);
    ui.painter().galley(Pos2::new(r.min.x + 14.0, r.center().y - galley.size().y / 2.0), galley, MUTED);
    resp
}

fn status_dot(ui: &mut egui::Ui, c: Color32) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    ui.painter().circle_filled(r.center(), 4.5, c);
}

fn pill(ui: &mut egui::Ui, c: Color32, text: &str) {
    egui::Frame::new()
        .stroke(Stroke::new(1.0, c.linear_multiply(0.6)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(8, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(12.0).color(c));
        });
}

fn badge(ui: &mut egui::Ui, view_only: bool) {
    if view_only { pill(ui, AMBER, "View only") } else { pill(ui, ACCENT, "Control") }
}

/// "View only" switch inside an open connection. Locked view-only links show
/// a plain badge instead. Returns the new choice when clicked.
fn view_only_toggle(ui: &mut egui::Ui, view_only: bool, locked: bool) -> Option<bool> {
    if locked {
        pill(ui, AMBER, "View only (link)");
        return None;
    }
    let (color, text) = if view_only { (AMBER, "View only: on") } else { (ACCENT, "View only: off") };
    let b = egui::Button::new(RichText::new(text).size(12.5).color(color))
        .stroke(Stroke::new(1.0, color.linear_multiply(0.6)))
        .corner_radius(CornerRadius::same(10));
    let hint = if view_only { "Click to control mouse and keyboard again" } else { "Click to only watch (mouse and keyboard stay yours)" };
    ui.add(b).on_hover_text(hint).clicked().then_some(!view_only)
}

fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).size(15.0).strong().color(Color32::from_rgb(6, 32, 25)))
            .fill(ACCENT)
            .min_size(Vec2::new(130.0, 34.0)),
    )
}

fn card(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            if width.is_finite() {
                ui.set_max_width(width);
            } else {
                ui.set_width(ui.available_width());
            }
            add(ui);
        });
}

/// Small drawn operating-system mark (simple shapes, no logos).
fn os_glyph(ui: &mut egui::Ui, os: &str, size: f32) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    paint_os_glyph(ui.painter(), r, os);
}

fn paint_os_glyph(p: &egui::Painter, r: Rect, os: &str) {
    let size = r.width();
    let o = os.to_lowercase();
    p.rect_filled(r, CornerRadius::same((size / 4.0) as u8), PANEL2);
    let inner = r.shrink(size * 0.24);
    if o.contains("windows") {
        let c = if o.contains("server") { Color32::from_rgb(130, 165, 230) } else { Color32::from_rgb(70, 150, 240) };
        let h = inner.width() / 2.0 - 1.0;
        for (dx, dy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
            let min = inner.min + Vec2::new(dx * (h + 2.0), dy * (h + 2.0));
            p.rect_filled(Rect::from_min_size(min, Vec2::splat(h)), CornerRadius::same(1), c);
        }
    } else if o.contains("ubuntu") || o.contains("linux") {
        let c = Color32::from_rgb(233, 95, 50);
        p.circle_stroke(inner.center(), inner.width() * 0.40, Stroke::new(size * 0.08, c));
        for k in 0..3 {
            let a = k as f32 * std::f32::consts::TAU / 3.0 - 0.5;
            p.circle_filled(inner.center() + Vec2::angled(a) * inner.width() * 0.46, size * 0.08, c);
        }
    } else {
        p.rect_stroke(inner, CornerRadius::same(2), Stroke::new(2.0, MUTED), StrokeKind::Inside);
    }
}

/// One computer in the list: name, OS, load and disks while online, and
/// the first line of the administrator's notes.
fn machine_row(ui: &mut egui::Ui, m: &MachineView, selected: bool, in_session: bool, now: u64) -> egui::Response {
    let stats = m.stats.as_ref().filter(|_| m.online);
    let notes = m.machine.notes.lines().map(str::trim).find(|l| !l.is_empty());
    let extra = stats.map_or(0, |s| 1 + usize::from(!s.disks.is_empty())) + usize::from(notes.is_some());
    let height = 54.0 + 17.0 * extra as f32;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
    let p = ui.painter();
    let fill = if selected {
        Color32::from_rgb(31, 50, 52)
    } else if resp.hovered() {
        PANEL2
    } else {
        Color32::TRANSPARENT
    };
    p.rect_filled(rect, CornerRadius::same(8), fill);
    if selected {
        let bar = Rect::from_min_size(rect.min + Vec2::new(0.0, 10.0), Vec2::new(3.0, rect.height() - 20.0));
        p.rect_filled(bar, CornerRadius::same(2), ACCENT);
    }
    paint_os_glyph(p, Rect::from_min_size(rect.min + Vec2::new(10.0, 11.0), Vec2::splat(32.0)), &m.machine.os);
    let name_color = if m.online { TEXT } else { MUTED };
    let text_w = rect.width() - 52.0 - 40.0;
    let x = rect.min.x + 52.0;
    one_line(p, Pos2::new(x, rect.min.y + 18.0), &m.machine.name, 14.5, name_color, text_w);
    let screens = m.machine.monitors.len();
    let owner = m.owner_name.as_ref().map(|o| format!("{o} · ")).unwrap_or_default();
    let sub = if m.online {
        format!("{owner}{} · {} screen{}", m.machine.os, screens, if screens == 1 { "" } else { "s" })
    } else {
        format!("offline · {} ago · {}", human_duration(now.saturating_sub(m.machine.last_seen)), m.machine.os)
    };
    one_line(p, Pos2::new(x, rect.min.y + 37.0), &sub, 12.0, MUTED, rect.max.x - x - 10.0);
    let mut y = rect.min.y + 54.0;
    if let Some(s) = stats {
        let mut job = egui::text::LayoutJob::default();
        let font = FontId::proportional(12.0);
        let mut part = |text: String, color: Color32| {
            job.append(&text, 0.0, egui::TextFormat::simple(font.clone(), color));
        };
        part(format!("CPU {:.0}%", s.cpu), load_color(s.cpu));
        part("  ·  ".into(), MUTED);
        let mem = if s.mem_total > 0 { s.mem_used as f32 * 100.0 / s.mem_total as f32 } else { 0.0 };
        part(format!("RAM {} of {}", crate::sysstat::human_bytes(s.mem_used), crate::sysstat::human_bytes(s.mem_total)), load_color(mem));
        paint_job(p, Pos2::new(x, y), job, rect.max.x - x - 10.0);
        y += 17.0;
        if !s.disks.is_empty() {
            let mut job = egui::text::LayoutJob::default();
            for (i, d) in s.disks.iter().enumerate() {
                if i > 0 {
                    job.append("  ·  ", 0.0, egui::TextFormat::simple(font.clone(), MUTED));
                }
                let used = 100.0 - d.free as f32 * 100.0 / d.total.max(1) as f32;
                let text = format!("{} {} free", d.name, crate::sysstat::human_bytes(d.free));
                job.append(&text, 0.0, egui::TextFormat::simple(font.clone(), load_color(used)));
            }
            paint_job(p, Pos2::new(x, y), job, rect.max.x - x - 10.0);
            y += 17.0;
        }
    }
    if let Some(n) = notes {
        let job = egui::text::LayoutJob::single_section(
            n.to_string(),
            egui::TextFormat { font_id: FontId::proportional(12.0), color: AMBER.linear_multiply(0.85), italics: true, ..Default::default() },
        );
        paint_job(p, Pos2::new(x, y), job, rect.max.x - x - 10.0);
    }
    let dot = if in_session {
        ACCENT
    } else if m.online {
        OK
    } else {
        Color32::from_rgb(89, 97, 109)
    };
    let mid = rect.min.y + 27.0;
    p.circle_filled(Pos2::new(rect.max.x - 14.0, mid), 4.5, dot);
    if !m.up_to_date {
        // Small "arrow up": this helper is older than the published version.
        let c = Pos2::new(rect.max.x - 30.0, mid);
        let color = if m.update_error.is_some() { DANGER } else { AMBER };
        let tri = vec![c + Vec2::new(0.0, -5.0), c + Vec2::new(5.0, 2.0), c + Vec2::new(-5.0, 2.0)];
        p.add(egui::Shape::convex_polygon(tri, color, Stroke::NONE));
        p.rect_filled(Rect::from_center_size(c + Vec2::new(0.0, 4.0), Vec2::new(3.0, 4.0)), CornerRadius::ZERO, color);
    }
    let resp = match stats {
        Some(s) if !s.disks.is_empty() => resp.on_hover_text(disk_details(s)),
        _ => resp,
    };
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// CPU, memory and every disk of an online computer.
fn stats_card(ui: &mut egui::Ui, s: &crate::sysstat::SystemStats) {
    use crate::sysstat::human_bytes;
    card(ui, f32::INFINITY, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("System").size(16.0).strong());
            ui.label(RichText::new("updated every 30 s").color(MUTED));
        });
        ui.add_space(6.0);
        let mem = s.mem_used as f32 * 100.0 / s.mem_total.max(1) as f32;
        let mut rows = vec![
            (format!("CPU · {} cores", s.cores), s.cpu, format!("{:.0}% busy", s.cpu)),
            ("Memory".to_string(), mem, format!("{} of {} used", human_bytes(s.mem_used), human_bytes(s.mem_total))),
        ];
        for d in &s.disks {
            let used = 100.0 - d.free as f32 * 100.0 / d.total.max(1) as f32;
            rows.push((format!("Disk {}", d.name), used, format!("{} free of {}", human_bytes(d.free), human_bytes(d.total))));
        }
        egui::Grid::new("stats").num_columns(3).spacing(Vec2::new(14.0, 6.0)).show(ui, |ui| {
            for (name, pct, text) in rows {
                ui.label(name);
                let color = match load_color(pct) {
                    MUTED => ACCENT,
                    c => c,
                };
                ui.add(egui::ProgressBar::new(pct / 100.0).desired_width(220.0).desired_height(8.0).fill(color));
                ui.label(RichText::new(text).color(MUTED));
                ui.end_row();
            }
        });
    });
}

/// Normal below 75 %, amber up to 90 %, red above.
fn load_color(percent: f32) -> Color32 {
    if percent >= 90.0 {
        DANGER
    } else if percent >= 75.0 {
        AMBER
    } else {
        MUTED
    }
}

/// "C:  7.0 GB free of 120 GB (94% used)", one disk per line.
fn disk_details(s: &crate::sysstat::SystemStats) -> String {
    use crate::sysstat::human_bytes;
    s.disks
        .iter()
        .map(|d| {
            let used = 100 - (d.free * 100 / d.total.max(1));
            format!("{}  {} free of {} ({used}% used)", d.name, human_bytes(d.free), human_bytes(d.total))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Paint text on one line, cut with "…" when wider than `max_w`; `pos` is the left centre.
fn one_line(p: &egui::Painter, pos: Pos2, text: &str, size: f32, color: Color32, max_w: f32) {
    paint_job(p, pos, egui::text::LayoutJob::simple_singleline(text.to_string(), FontId::proportional(size), color), max_w);
}

fn paint_job(p: &egui::Painter, pos: Pos2, mut job: egui::text::LayoutJob, max_w: f32) {
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w.max(10.0));
    let galley = p.layout_job(job);
    let at = Pos2::new(pos.x, pos.y - galley.size().y / 2.0);
    p.galley(at, galley, TEXT);
}

/// What hovering the relay's address in the status bar tells: version, its
/// own updates, and what could be better. The flag: something needs a look
/// (update check failed, connections slow down after pauses).
fn relay_details(o: &Overview) -> (String, bool) {
    let u = &o.server_update;
    let ago = |t: u64| format!("{} ago", crate::util::human_duration(o.now.saturating_sub(t)));
    let mut warn = false;
    let mut tip = format!("Relay server version {}", o.server_version);
    if let Some(fix) = &o.slow_after_pause_fix {
        warn = true;
        tip.push_str(&format!(
            "\nCan be faster: the relay's Linux slows connections down after every pause, so the picture after a click arrives later.\nRun once on the server (also shown in the web admin):\n{fix}"
        ));
    }
    match &u.source {
        Some(repo) => tip.push_str(&format!("\nUpdates itself from github.com/{repo} (hourly)")),
        None => tip.push_str("\nAutomatic updates are off"),
    }
    if u.checking {
        tip.push_str("\nChecking for updates…");
    } else if let Some(t) = u.last_check {
        warn |= !u.last_check_ok;
        tip.push_str(&format!("\nLast check: {} — {}{}", ago(t), if u.last_check_ok { "" } else { "failed: " }, u.last_check_result));
    }
    match (u.last_update, &u.last_update_from) {
        (Some(t), Some(from)) => tip.push_str(&format!("\nLast update: {} ({from} → {})", ago(t), u.version)),
        (Some(t), None) => tip.push_str(&format!("\nLast update: {}", ago(t))),
        _ => tip.push_str("\nNot updated since it was installed"),
    }
    (tip, warn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect::from_min_size(Pos2::new(100.0, 50.0), Vec2::new(200.0, 100.0))
    }

    #[test]
    fn pointer_maps_to_monitor_fractions() {
        let mut prev = egui::Modifiers::default();
        let evs = vec![
            egui::Event::PointerMoved(Pos2::new(150.0, 75.0)),
            egui::Event::PointerMoved(Pos2::new(200.0, 100.0)),
            egui::Event::PointerMoved(Pos2::new(900.0, 900.0)), // outside: ignored
        ];
        let out = map_events(&evs, rect(), rect(), 1, &mut prev, egui::Modifiers::default(), true);
        assert_eq!(out, vec![InputEvent::MouseMove { mon: 1, x: 0.5, y: 0.5 }], "only the last move inside is sent");
    }

    #[test]
    fn clicks_outside_are_ignored_but_releases_kept() {
        let mut prev = egui::Modifiers::default();
        let press_out = egui::Event::PointerButton {
            pos: Pos2::new(0.0, 0.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        };
        let release_out = egui::Event::PointerButton {
            pos: Pos2::new(0.0, 0.0),
            button: egui::PointerButton::Secondary,
            pressed: false,
            modifiers: Default::default(),
        };
        let out = map_events(&[press_out, release_out], rect(), rect(), 0, &mut prev, Default::default(), true);
        assert_eq!(out, vec![InputEvent::MouseButton { mon: 0, x: 0.0, y: 0.0, button: MouseButton::Right, down: false }]);
    }

    #[test]
    fn keys_text_and_modifiers() {
        let mut prev = egui::Modifiers::default();
        let ctrl = egui::Modifiers { ctrl: true, command: true, ..Default::default() };
        let key = |k, m| egui::Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers: m };
        // Plain letter: arrives as text, not as a key.
        let out = map_events(
            &[key(egui::Key::A, Default::default()), egui::Event::Text("a".into()), key(egui::Key::Enter, Default::default())],
            rect(),
            rect(),
            0,
            &mut prev,
            Default::default(),
            true,
        );
        assert_eq!(out, vec![InputEvent::Text { text: "a".into() }, InputEvent::Key { key: "Enter".into(), down: true }]);
        // Ctrl pressed: modifier goes down, Ctrl+S sent as key.
        let out = map_events(&[key(egui::Key::S, ctrl)], rect(), rect(), 0, &mut prev, ctrl, true);
        assert_eq!(out, vec![InputEvent::Key { key: "Control".into(), down: true }, InputEvent::Key { key: "S".into(), down: true }]);
        // Ctrl released.
        let out = map_events(&[], rect(), rect(), 0, &mut prev, Default::default(), true);
        assert_eq!(out, vec![InputEvent::Key { key: "Control".into(), down: false }]);
        // Copy shortcut becomes C.
        let out = map_events(&[egui::Event::Copy], rect(), rect(), 0, &mut prev, Default::default(), true);
        assert_eq!(out.len(), 2);
        // No keyboard when window not focused.
        assert!(map_events(&[egui::Event::Text("x".into())], rect(), rect(), 0, &mut prev, Default::default(), false).is_empty());
    }

    #[test]
    fn arrows_and_wheel() {
        assert_eq!(remote_key(egui::Key::ArrowLeft), Some(("ArrowLeft", false)));
        assert_eq!(remote_key(egui::Key::Num5), Some(("5", true)));
        assert_eq!(remote_key(egui::Key::F5), Some(("F5", false)));
        let mut prev = egui::Modifiers::default();
        let wheel = egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: Vec2::new(0.0, 2.0),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Wheel,
            modifiers: Default::default(),
        };
        let out = map_events(&[wheel], rect(), rect(), 0, &mut prev, Default::default(), true);
        assert_eq!(out, vec![InputEvent::Scroll { dx: 0.0, dy: 2.0 }]);
    }

    #[test]
    fn scrolled_away_parts_take_no_clicks() {
        // Picture scrolled so only its right half is on screen.
        let visible = Rect::from_min_max(Pos2::new(200.0, 50.0), Pos2::new(300.0, 150.0));
        let mut prev = egui::Modifiers::default();
        let press = |x: f32| egui::Event::PointerButton {
            pos: Pos2::new(x, 100.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        };
        let out = map_events(&[press(150.0), press(250.0)], rect(), visible, 0, &mut prev, Default::default(), true);
        assert_eq!(out, vec![InputEvent::MouseButton { mon: 0, x: 0.75, y: 0.5, button: MouseButton::Left, down: true }]);
    }

    #[test]
    fn original_size_window_fits_the_screen() {
        let mon = MonitorInfo { index: 0, name: String::new(), x: 0, y: 0, width: 1280, height: 720, primary: true };
        assert_eq!(original_window_size(&mon, 1.0, Some(Vec2::new(1920.0, 1080.0)), true), Vec2::new(1280.0, 756.0));
        assert_eq!(original_window_size(&mon, 2.0, None, true), Vec2::new(640.0, 396.0));
        let big = MonitorInfo { width: 3840, height: 2160, ..mon };
        assert_eq!(original_window_size(&big, 1.0, Some(Vec2::new(1920.0, 1080.0)), true), Vec2::new(1920.0, 1020.0));
    }

    #[test]
    fn original_size_window_without_controls_is_just_the_screen() {
        let mon = MonitorInfo { index: 0, name: String::new(), x: 0, y: 0, width: 1280, height: 720, primary: true };
        assert_eq!(original_window_size(&mon, 1.0, Some(Vec2::new(1920.0, 1080.0)), false), Vec2::new(1280.0, 720.0));
    }

    #[test]
    fn the_file_browser_starts_where_it_was_or_somewhere_sensible() {
        use crate::net::RemoteDir;
        use crate::proto::FsEntry;
        let entry = |p: &str| FsEntry { name: p.into(), path: p.into(), is_dir: true, size: 0, modified: 0 };
        // This computer: the remembered folder while it exists, else home (or the root).
        let tmp = std::env::temp_dir().display().to_string();
        assert_eq!(local_start(Some(&tmp)), tmp);
        let fallback = local_start(Some("/no/such/folder/anywhere"));
        assert_eq!(fallback, local_start(None));
        assert!(fallback.is_empty() || std::path::Path::new(&fallback).is_dir());

        // The primary disk: C: on Windows (else the first drive), / elsewhere.
        assert_eq!(primary_disk(&[entry("A:\\"), entry("C:\\"), entry("D:\\")]).as_deref(), Some("C:\\"));
        assert_eq!(primary_disk(&[entry("D:\\"), entry("E:\\")]).as_deref(), Some("D:\\"));
        assert_eq!(primary_disk(&[entry("/home/anna"), entry("/")]).as_deref(), Some("/"));
        assert_eq!(primary_disk(&[]), None);

        let listing = |path: &str, entries: Vec<FsEntry>, error: Option<&str>| RemoteDir {
            path: path.into(),
            parent: None,
            entries,
            error: error.map(str::to_string),
            loading: false,
        };
        // Nothing remembered: the drives are asked for, then the primary disk.
        let side = |dir: &str| FilesSide { dir: dir.into(), show_hidden: true };
        let mut fv = FilesView::new(&FilesSide::default(), &FilesSide::default());
        assert!(!fv.local_hidden && !fv.remote_hidden);
        assert_eq!(fv.first_remote_path(), "");
        assert_eq!(fv.remote_start_step(&RemoteDir { loading: true, ..Default::default() }), None, "still waiting");
        assert_eq!(fv.remote_start_step(&listing("", vec![entry("C:\\"), entry("D:\\")], None)).as_deref(), Some("C:\\"));
        assert_eq!(fv.remote_start, RemoteStart::Done);
        assert_eq!(fv.remote_start_step(&listing("", vec![entry("C:\\")], None)), None, "going to the drives later stays there");

        // A remembered folder that still exists is simply opened.
        let mut fv = FilesView::new(&FilesSide::default(), &side("D:\\Work"));
        assert!(fv.remote_hidden && !fv.local_hidden, "hidden files are remembered per side");
        assert_eq!(fv.first_remote_path(), "D:\\Work");
        assert_eq!(fv.remote_start_step(&listing("D:\\Work", vec![], None)), None);
        assert_eq!(fv.remote_start, RemoteStart::Done);

        // One that is gone: drives, then the primary disk.
        let mut fv = FilesView::new(&FilesSide::default(), &side("D:\\Gone"));
        assert_eq!(fv.remote_start_step(&listing("D:\\Gone", vec![], Some("not found"))).as_deref(), Some(""));
        assert_eq!(fv.remote_start_step(&listing("", vec![entry("C:\\")], None)).as_deref(), Some("C:\\"));
    }

    #[test]
    fn maximised_windows_leave_full_screen_maximised() {
        let mut max = HashSet::new();
        assert_eq!(fullscreen_commands(false, true, 1, &mut max), vec![ViewportCommand::Maximized(false), ViewportCommand::Fullscreen(true)]);
        assert_eq!(fullscreen_commands(true, false, 1, &mut max), vec![ViewportCommand::Fullscreen(false), ViewportCommand::Maximized(true)]);
        assert!(max.is_empty());
        assert_eq!(fullscreen_commands(false, false, 1, &mut max), vec![ViewportCommand::Fullscreen(true)]);
        assert_eq!(fullscreen_commands(true, false, 1, &mut max), vec![ViewportCommand::Fullscreen(false)]);
    }

    #[test]
    fn hidden_controls_show_after_a_second_at_the_top_edge() {
        const H: f32 = 1000.0;
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut r = Reveal::default();
        r.step(Some(300.0), H, false, at(0));
        assert!(!r.shown && r.wait(at(0)).is_none(), "nothing while the mouse is on the picture");
        r.step(Some(1.0), H, false, at(100));
        assert!(!r.shown);
        assert_eq!(r.wait(at(600)), Some(Duration::from_millis(510)), "looks again when the second is up");
        r.step(Some(2.0), H, false, at(1099));
        assert!(!r.shown, "not before a whole second");
        r.step(Some(0.0), H, false, at(1100));
        assert!(r.shown);
        assert!(r.wait(at(1100)).is_none());

        // The edge is 3 % of the window's height (30 of 1000), at least 4 points.
        let mut r = Reveal::default();
        r.step(Some(31.0), H, false, at(0));
        assert!(r.at_top_since.is_none(), "below the edge");
        r.step(Some(29.0), H, false, at(0));
        r.step(Some(29.0), H, false, at(1000));
        assert!(r.shown);
        let mut r = Reveal::default();
        r.step(Some(3.5), 50.0, false, at(0));
        assert!(r.at_top_since.is_some(), "small windows keep 4 points");
    }

    #[test]
    fn leaving_the_top_edge_restarts_the_wait() {
        const H: f32 = 1000.0;
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut r = Reveal::default();
        r.step(Some(0.0), H, false, at(0));
        r.step(Some(50.0), H, false, at(900));
        r.step(Some(0.0), H, false, at(1000));
        assert!(!r.shown);
        r.step(Some(0.0), H, false, at(1500));
        assert!(!r.shown, "only 0.5 s since it came back");
        r.step(Some(300.0), H, false, at(1550));
        r.step(None, H, false, at(1600));
        r.step(Some(0.0), H, false, at(2700));
        assert!(!r.shown, "leaving the window elsewhere counts as leaving");
    }

    #[test]
    fn the_title_bar_counts_as_the_top_edge() {
        const H: f32 = 1000.0;
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut r = Reveal::default();
        r.step(Some(200.0), H, false, at(0));
        r.step(Some(40.0), H, false, at(10));
        r.step(None, H, false, at(20)); // left through the top: on the title bar
        assert!(!r.shown && r.wait(at(20)).is_some());
        r.step(None, H, false, at(1020));
        assert!(r.shown, "a second on the title bar");
        r.step(None, H, false, at(3000));
        assert!(r.shown, "stays while the mouse is on the title bar");
        r.step(Some(20.0), H, false, at(3100));
        assert!(r.shown, "back onto the controls");
        r.step(Some(500.0), H, false, at(3200));
        assert!(!r.shown, "gone once the mouse is on the picture");
        r.step(None, H, false, at(3300));
        r.step(None, H, false, at(5000));
        assert!(!r.shown, "left from the middle of the picture: not the title bar");
    }

    #[test]
    fn shown_controls_stay_while_used_and_go_when_the_mouse_leaves() {
        const H: f32 = 1000.0;
        let t0 = Instant::now();
        let mut r = Reveal { at_top_since: Some(t0), shown: true, ..Default::default() };
        r.step(Some(BAR_HEIGHT - 5.0), H, false, t0);
        assert!(r.shown, "mouse on the controls");
        r.step(Some(400.0), H, true, t0);
        assert!(r.shown, "a menu of the controls is open");
        r.step(Some(400.0), H, false, t0);
        assert!(!r.shown && r.at_top_since.is_none());
        r.step(None, H, false, t0);
        assert!(!r.shown);
    }

    #[test]
    fn clicks_under_shown_controls_are_not_for_the_remote() {
        let visible = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        assert_eq!(below(visible, None), visible);
        let bar = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, BAR_HEIGHT));
        let cut = below(visible, Some(bar));
        assert_eq!(cut, Rect::from_min_max(Pos2::new(0.0, BAR_HEIGHT), Pos2::new(800.0, 600.0)));
        let press = egui::Event::PointerButton {
            pos: Pos2::new(400.0, 10.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        };
        let mut prev = egui::Modifiers::default();
        assert!(map_events(&[press], visible, cut, 0, &mut prev, Default::default(), true).is_empty());
    }

    #[test]
    fn fit_keeps_aspect() {
        let r = fit_rect(Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 1000.0)), 1920.0, 1080.0);
        assert!((r.width() / r.height() - 1920.0 / 1080.0).abs() < 0.01);
        assert_eq!(r.width(), 1000.0);
        assert_eq!(r.center(), Pos2::new(500.0, 500.0));
    }

    #[test]
    fn sample_data_is_consistent() {
        let o = sample_overview();
        assert!(o.machines.iter().any(|m| m.machine.monitors.len() == 2));
        assert!(o.machines.iter().any(|m| !m.online));
    }
}
