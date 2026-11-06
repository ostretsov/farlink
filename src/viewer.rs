//! Desktop viewer (egui): machine list, details, quality, temporary links and
//! one native window per remote monitor, reopened where it was left.

use crate::config::{ViewerConfig, load_json, save_json, viewer_dir};
use crate::layout::{LayoutStore, WindowPlace, default_place};
use crate::net::{Api, ConnState, SessionHandle, Target, TransferState, parse_link};
use crate::proto::{InputEvent, MonitorInfo, MouseButton, SessionMsg, monitor_fingerprint};
use crate::quality::{Codec, ColorDepth, Preset, Quality};
use crate::server::{MachineSummary, MachineView, Overview};
use crate::util::{human_duration, normalize_server_url, now_secs};
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2,
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
}

pub fn run(opts: ViewerOptions) -> eframe::Result<()> {
    let native = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("Farlink")
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([820.0, 520.0]),
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
    fullscreen: HashSet<u8>,
    minimized: HashSet<u8>,
    /// Last "watching these screens" sent to the helper.
    last_watch: Option<Vec<u8>>,
    /// Open file browser window, if any.
    files: Option<FilesView>,
}

/// Two-pane file browser: this computer on the left, the remote one on the right.
pub struct FilesView {
    /// "" = starting places (drives / home).
    local_dir: String,
    local: crate::net::RemoteDir,
    local_sel: Option<crate::proto::FsEntry>,
    remote_sel: Option<crate::proto::FsEntry>,
    new_folder: String,
    show_hidden: bool,
}

impl FilesView {
    fn new() -> FilesView {
        let start = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
        let mut v = FilesView {
            local_dir: start,
            local: Default::default(),
            local_sel: None,
            remote_sel: None,
            new_folder: String::new(),
            show_hidden: false,
        };
        v.reload_local();
        v
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
                let text = if e.is_dir { format!("[folder]  {}", e.name) } else { format!("{}    {}", e.name, format_size(e.size)) };
                let resp = ui.add(egui::Button::new(RichText::new(text).color(if e.is_dir { ACCENT } else { TEXT })).selected(selected).frame(selected).min_size(Vec2::new(ui.available_width(), 20.0)));
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
    egui::Panel::top("files-bar").frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(12, 8))).show(ui, |ui| {
        ui.horizontal(|ui| {
            let up_ok = fv.local_sel.as_ref().is_some_and(|e| !e.is_dir) && !remote.path.is_empty();
            if ui.add_enabled(up_ok, egui::Button::new(RichText::new("Upload  ->").strong())).on_hover_text("Copy the selected file to the remote folder").clicked()
                && let Some(e) = &fv.local_sel
            {
                s.handle.upload(PathBuf::from(&e.path), remote.path.clone());
            }
            let down_ok = fv.remote_sel.as_ref().is_some_and(|e| !e.is_dir) && !fv.local_dir.is_empty();
            if ui.add_enabled(down_ok, egui::Button::new(RichText::new("<-  Download").strong())).on_hover_text("Copy the selected remote file here").clicked()
                && let Some(e) = &fv.remote_sel
            {
                s.handle.download(e, PathBuf::from(&fv.local_dir));
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut fv.new_folder).hint_text("New remote folder name").desired_width(180.0));
            let name_ok = crate::files::safe_name(&fv.new_folder).is_some() && !remote.path.is_empty();
            ui.checkbox(&mut fv.show_hidden, "Show hidden files");
            if ui.add_enabled(name_ok, egui::Button::new("Create folder")).clicked() {
                let p = PathBuf::from(&remote.path).join(fv.new_folder.trim());
                s.handle.mkdir_remote(&p.display().to_string());
                fv.new_folder.clear();
            }
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
            if let Some(a) = file_pane(&mut cols[0], "local", "This computer", &fv.local, fv.local_sel.as_ref(), fv.show_hidden) {
                match a {
                    PaneAction::Open(p) => {
                        fv.local_dir = p;
                        fv.reload_local();
                    }
                    PaneAction::Select(e) => fv.local_sel = Some(e),
                }
            }
            let title = format!("Remote: {}", s.name);
            if let Some(a) = file_pane(&mut cols[1], "remote", &title, &remote, fv.remote_sel.as_ref(), fv.show_hidden) {
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
            self.files = Some(FilesView::new());
            self.handle.list_remote("");
        }
    }
}

impl OpenSession {
    fn update_textures(&mut self, ctx: &egui::Context) {
        let shared = self.handle.shared.lock().unwrap();
        for (mon, canvas) in &shared.canvases {
            let Some(frame) = &canvas.frame else { continue };
            let current = self.textures.get(mon).map(|t| t.1);
            if current == Some(canvas.generation) {
                continue;
            }
            let img = egui::ColorImage::from_rgba_unmultiplied([frame.width as usize, frame.height as usize], &frame.rgba);
            match self.textures.get_mut(mon) {
                Some((tex, generation)) => {
                    tex.set(img, egui::TextureOptions::LINEAR);
                    *generation = canvas.generation;
                }
                None => {
                    let tex = ctx.load_texture(format!("s{}m{}", self.id, mon), img, egui::TextureOptions::LINEAR);
                    self.textures.insert(*mon, (tex, canvas.generation));
                }
            }
        }
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
/// in `rect`. `keyboard` is false when the window isn't focused.
pub fn map_events(
    events: &[egui::Event],
    rect: Rect,
    mon: u8,
    prev_mods: &mut egui::Modifiers,
    mods: egui::Modifiers,
    keyboard: bool,
) -> Vec<InputEvent> {
    let norm = |p: Pos2| ((p.x - rect.min.x) / rect.width(), (p.y - rect.min.y) / rect.height());
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
            egui::Event::PointerMoved(p) if rect.contains(*p) => {
                let (x, y) = norm(*p);
                last_move = Some(InputEvent::MouseMove { mon, x, y });
            }
            egui::Event::PointerButton { pos, button, pressed, .. } => {
                if *pressed && !rect.contains(*pos) {
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
    renaming: Option<Renaming>,
    last_refresh: Option<Instant>,
    selected: Option<String>,
    search: String,
    sessions: Vec<OpenSession>,
    next_session: u64,
    grant: GrantForm,
    toast: Option<(String, Instant)>,
    update_status: Arc<Mutex<String>>,
    frames: u64,
    shot_requested: bool,
    shot_done: Arc<AtomicBool>,
    demo: bool,
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
        },
        online,
        sessions: 0,
        up_to_date: true,
        owner_name: None,
    };
    let mut anna = m("demo-anna", "Anna's laptop", "ANNA-NB", "Windows 11 Home", vec![mon(0, 0, 1920, 1080)], true, 0);
    anna.machine.owner = Some("demo-user-anna".into());
    anna.owner_name = Some("Anna".into());
    Overview {
        machines: vec![
            m("demo-office", "Office PC", "OFFICE-PC", "Windows 11 Pro", vec![mon(0, 0, 1600, 900), mon(1, 1600, 1280, 1024)], true, 0),
            m("demo-srv", "Accounting server", "ACC-SRV16", "Windows Server 2016", vec![mon(0, 0, 1920, 1080)], true, 0),
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
            renaming: None,
            last_refresh: None,
            selected: None,
            search: String::new(),
            sessions: Vec::new(),
            next_session: 1,
            grant: GrantForm::default(),
            toast: None,
            update_status: Arc::new(Mutex::new(String::new())),
            frames: 0,
            shot_requested: false,
            shot_done: Arc::new(AtomicBool::new(false)),
            demo,
            opts,
        };
        if demo {
            app.overview = Some(sample_overview());
            app.selected = Some("demo-office".into());
            if screenshot {
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
        if !screenshot && app.cfg.auto_update && !app.cfg.server_url.is_empty() {
            let server = app.cfg.server_url.clone();
            let status = app.update_status.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                if let Ok(exe) = std::env::current_exe() {
                    crate::update::cleanup_old(&exe);
                }
                let text = match crate::update::update_self(&server, "farlink") {
                    Ok(Some(v)) => format!("Updated to {v}, restart Farlink to use it"),
                    Ok(None) => "Up to date".into(),
                    Err(_) => String::new(),
                };
                *status.lock().unwrap() = text;
                ctx.request_repaint();
            });
        }
        app
    }

    fn signed_in(&self) -> bool {
        self.cfg.admin_token.is_some() && !self.cfg.server_url.is_empty()
    }

    fn api(&self) -> Option<Api> {
        Some(Api { server: self.cfg.server_url.clone(), token: self.cfg.admin_token.clone()? })
    }

    fn save_cfg(&self) {
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
            fullscreen: HashSet::new(),
            minimized: HashSet::new(),
            last_watch: None,
            files: None,
        });
        self.next_session += 1;
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
        if let Some(r) = take_job(&mut self.job_revoke) {
            match r {
                Ok(()) => self.toast("Access withdrawn, open sessions were ended"),
                Err(e) => self.toast(e),
            }
            self.refresh(ctx);
        }
    }

    // -------------------------------------------------------------- top bar

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::Panel::top("top")
            .exact_size(50.0)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(14, 8)).stroke(Stroke::new(1.0, LINE)))
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    logo(ui);
                    ui.add_space(10.0);
                    let (dot, text) = if self.demo {
                        (AMBER, "Sample computers (demo)".to_string())
                    } else if self.signed_in() {
                        let who = self.overview.as_ref().map(|o| o.user_name.clone()).unwrap_or_default();
                        let text = if who.is_empty() { self.cfg.server_url.clone() } else { format!("{} · {who}", self.cfg.server_url) };
                        (if self.overview_err.is_some() { DANGER } else { OK }, text)
                    } else {
                        (MUTED, "Not connected".to_string())
                    };
                    chip(ui, dot, &text);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if self.signed_in() && !self.demo && ui.button("Sign out").clicked() {
                            if let Some(api) = self.api() {
                                std::thread::spawn(move || api.logout());
                            }
                            self.cfg.admin_token = None;
                            self.overview = None;
                            self.save_cfg();
                        }
                        if self.demo && !self.opts.demo && self.opts.screenshot.is_none() && ui.button("Leave demo").clicked() {
                            self.demo = false;
                            self.overview = None;
                            self.selected = None;
                        }
                        if self.signed_in() && !self.demo && ui.button("Refresh").clicked() {
                            self.refresh(&ctx);
                        }
                        ui.add_space(6.0);
                        if ui.button("Open link").clicked() {
                            let l = self.link_input.clone();
                            self.open_link(&ctx, &l);
                        }
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.link_input).hint_text("Paste a temporary link").desired_width(250.0),
                        );
                        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            let l = self.link_input.clone();
                            self.open_link(&ctx, &l);
                        }
                    });
                });
            });
    }

    // -------------------------------------------------------------- bottom

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::Panel::bottom("status")
            .exact_size(28.0)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(14, 4)).stroke(Stroke::new(1.0, LINE)))
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(RichText::new(format!("Farlink {}", crate::VERSION)).color(MUTED).size(12.0));
                    let upd = self.update_status.lock().unwrap().clone();
                    if !upd.is_empty() {
                        ui.label(RichText::new(format!("· {upd}")).color(MUTED).size(12.0));
                    }
                    ui.label(RichText::new(format!("· picture: {}", self.cfg.quality.summary())).color(MUTED).size(12.0));
                    if let Some((t, at)) = &self.toast {
                        if at.elapsed() < Duration::from_secs(5) {
                            let t = t.clone();
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(RichText::new(t).color(AMBER).size(12.0));
                            });
                        } else {
                            self.toast = None;
                        }
                    }
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
                            if !s.closed.is_empty() && mons > 0 && ui.button("Show screens").clicked() {
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
            .exact_size(300.0)
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
                let title = if ov.role == "admin" { "COMPUTERS" } else { "MY COMPUTERS" };
                ui.label(RichText::new(format!("{title} · {online} ONLINE OF {}", ov.machines.len())).size(11.0).color(MUTED));
                ui.add_space(2.0);
                let mut clicked = None;
                let mut rename_start: Option<(String, String)> = None;
                let can_rename = ov.role == "admin";
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for m in ov.machines.iter().filter(|m| {
                        q.is_empty() || format!("{} {} {}", m.machine.name, m.machine.hostname, m.machine.os).to_lowercase().contains(&q)
                    }) {
                        let selected = self.selected.as_deref() == Some(m.machine.id.as_str());
                        let in_session = self.sessions.iter().any(|s| s.machine_id.as_deref() == Some(m.machine.id.as_str()));
                        let resp = machine_row(ui, m, selected, in_session, ov.now);
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
                if let Some((id, name)) = rename_start {
                    self.renaming = Some(Renaming { machine_id: id, text: name, focus: true });
                }
            });
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
                ui.label(RichText::new("Got a temporary link? Paste it at the top right and press Open link.").color(MUTED));
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
                    let (c, t) = if mv.online { (OK, "online") } else { (MUTED, "offline") };
                    pill(ui, c, t);
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
            });
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.add_enabled_ui(mv.online, |ui| {
                if primary_button(ui, "Control").clicked() {
                    self.connect_machine(ctx, m, false);
                }
                if ui.add(egui::Button::new(RichText::new("View only").size(15.0)).min_size(Vec2::new(130.0, 34.0))).clicked() {
                    self.connect_machine(ctx, m, true);
                }
            });
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
        self.screens_card(ui, m);
        ui.add_space(14.0);
        self.quality_card(ui);
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

    fn screens_card(&mut self, ui: &mut egui::Ui, m: &MachineSummary) {
        let session_idx = self.sessions.iter().position(|s| s.machine_id.as_deref() == Some(m.id.as_str()));
        let monitors = session_idx
            .map(|i| self.sessions[i].handle.shared.lock().unwrap().monitors.clone())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| m.monitors.clone());
        card(ui, f32::INFINITY, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Screens").size(16.0).strong());
                ui.label(RichText::new(format!("{} on this computer · each opens in its own window", monitors.len())).color(MUTED));
            });
            ui.add_space(6.0);
            if monitors.is_empty() {
                ui.label(RichText::new("The screen layout appears after the first connection.").color(MUTED));
                return;
            }
            // Arrangement diagram with live pictures when connected.
            let (vx, vy, vw, vh) = crate::desktop::virtual_bounds(&monitors);
            let avail_w = ui.available_width().min(760.0);
            let scale = (avail_w / vw as f32).min(200.0 / vh as f32);
            let (area, _) = ui.allocate_exact_size(Vec2::new(avail_w, vh as f32 * scale + 8.0), Sense::hover());
            let painter = ui.painter_at(area);
            let off = Vec2::new((avail_w - vw as f32 * scale) / 2.0, 4.0);
            for mon in &monitors {
                let r = Rect::from_min_size(
                    area.min + off + Vec2::new((mon.x - vx) as f32 * scale, (mon.y - vy) as f32 * scale),
                    Vec2::new(mon.width as f32 * scale, mon.height as f32 * scale),
                )
                .shrink(3.0);
                painter.rect_filled(r, CornerRadius::same(4), PANEL2);
                let tex = session_idx.and_then(|i| self.sessions[i].textures.get(&mon.index).map(|t| t.0.id()));
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
            }
            ui.add_space(6.0);
            let key = m.id.clone();
            for mon in &monitors {
                let fp = monitor_fingerprint(mon);
                let text = match self.layouts.get(&key, &fp) {
                    Some(p) => format!(
                        "Screen {} window: remembered at {:.0}, {:.0} · {:.0}×{:.0}{}",
                        mon.index + 1,
                        p.x,
                        p.y,
                        p.w,
                        p.h,
                        if p.fullscreen { " · full screen" } else { "" }
                    ),
                    None => format!("Screen {} window: not placed yet, opens side by side", mon.index + 1),
                };
                ui.label(RichText::new(text).color(MUTED));
            }
            if self.layouts.count(&key) > 0 && ui.button("Forget window positions").clicked() {
                self.layouts.forget(&key);
                self.layouts_dirty = Some(Instant::now());
            }
        });
    }

    fn quality_card(&mut self, ui: &mut egui::Ui) {
        let before = self.cfg.quality;
        card(ui, f32::INFINITY, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Picture quality").size(16.0).strong());
                ui.label(RichText::new("changes apply right away, also to open connections").color(MUTED));
            });
            ui.add_space(4.0);
            let current = self.cfg.quality.matching_preset();
            ui.horizontal(|ui| {
                for p in Preset::BUTTONS {
                    let b = egui::Button::new(RichText::new(p.label()).size(14.0)).selected(current == p).min_size(Vec2::new(96.0, 30.0));
                    if ui.add(b).on_hover_text(p.hint()).clicked() {
                        self.cfg.quality = Quality::preset(p);
                    }
                }
                ui.label(RichText::new(current.hint()).color(MUTED));
            });
            ui.add_space(6.0);
            let q = &mut self.cfg.quality;
            egui::Grid::new("quality").num_columns(2).spacing([18.0, 8.0]).show(ui, |ui| {
                ui.label("Picture size");
                ui.add(egui::Slider::new(&mut q.scale_pct, 25..=100).suffix(" %"));
                ui.end_row();
                ui.label("Pictures per second");
                ui.add(egui::Slider::new(&mut q.max_fps, 1..=30));
                ui.end_row();
                ui.label("Compression");
                ui.horizontal(|ui| {
                    for c in [Codec::Jpeg, Codec::Png] {
                        ui.radio_value(&mut q.codec, c, c.label());
                    }
                });
                ui.end_row();
                if q.codec == Codec::Jpeg {
                    ui.label("JPEG quality");
                    ui.add(egui::Slider::new(&mut q.jpeg_quality, 10..=95));
                    ui.end_row();
                }
                ui.label("Colours");
                egui::ComboBox::from_id_salt("colors").selected_text(q.color.label()).width(220.0).show_ui(ui, |ui| {
                    for c in ColorDepth::ALL {
                        ui.selectable_value(&mut q.color, c, c.label());
                    }
                });
                ui.end_row();
            });
            ui.label(
                RichText::new("Fewer colours and a smaller picture use less internet. Colour depth matters most with Sharp text (PNG).")
                    .color(MUTED)
                    .size(12.0),
            );
        });
        if self.cfg.quality != before {
            self.save_cfg();
            let q = self.cfg.quality;
            for s in &mut self.sessions {
                s.quality = q;
                s.handle.send(SessionMsg::Quality { quality: q });
            }
        }
    }

    // -------------------------------------------------------------- monitor windows

    fn monitor_windows(&mut self, ctx: &egui::Context) {
        let local = ctx.input(|i| i.viewport().monitor_size).unwrap_or(Vec2::new(1920.0, 1080.0));
        let mut dirty = false;
        let mut to_close = Vec::new();
        for s in &mut self.sessions {
            let (monitors, live) = {
                let sh = s.handle.shared.lock().unwrap();
                (sh.monitors.clone(), sh.state == ConnState::Live)
            };
            if !live {
                continue;
            }
            let Some(key) = s.key.clone() else { continue };
            for mon in &monitors {
                if s.closed.contains(&mon.index) {
                    continue;
                }
                let fp = monitor_fingerprint(mon);
                let builder = s
                    .builders
                    .entry(mon.index)
                    .or_insert_with(|| {
                        let place = self.layouts.get(&key, &fp).unwrap_or_else(|| {
                            default_place(mon.index as usize, monitors.len(), mon.width, mon.height, (local.x, local.y))
                        });
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
                monitors.iter().map(|m| m.index).filter(|i| !s.closed.contains(i) && !s.minimized.contains(i)).collect();
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
                if ctx.show_viewport_immediate(vid, builder, |ui, _| files_window(ui, s)) {
                    s.files = None;
                }
            }
            if !monitors.is_empty() && monitors.iter().all(|m| s.closed.contains(&m.index)) && s.files.is_none() {
                to_close.push(s.id);
            }
        }
        if !to_close.is_empty() {
            self.sessions.retain(|s| !to_close.contains(&s.id));
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
    if let (Some(outer), Some(inner)) = (info.outer_rect, info.inner_rect) {
        let fullscreen = info.fullscreen.unwrap_or(false);
        let maximized = info.maximized.unwrap_or(false);
        let place = match layouts.get(key, fp) {
            // Keep the normal size/position while full screen or maximised.
            Some(old) if fullscreen || maximized => WindowPlace { fullscreen, maximized, ..old },
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
    egui::Panel::top("bar")
        .exact_size(36.0)
        .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(10, 5)))
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                if let Some(v) = view_only_toggle(ui, view_only, locked) {
                    s.handle.set_view_only(v);
                }
                ui.label(RichText::new(format!("{} · {}", s.name, mon.label())).strong());
                ui.label(RichText::new(format!("{fps:.0} fps · {kbps:.0} KB/s")).color(MUTED).size(12.0));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Disconnect").clicked() {
                        result = MonitorResult::Disconnect;
                    }
                    if !view_only && ui.button("Files").clicked() {
                        s.open_files();
                    }
                    let fs = s.fullscreen.contains(&mon.index);
                    if ui.button(if fs { "Exit full screen" } else { "Full screen" }).clicked() {
                        if fs {
                            s.fullscreen.remove(&mon.index);
                        } else {
                            s.fullscreen.insert(mon.index);
                        }
                        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(!fs));
                    }
                    if !view_only && ui.button("Ctrl+Alt+Del").clicked() {
                        s.handle.send(SessionMsg::Input { event: InputEvent::CtrlAltDel });
                    }
                    let current = s.quality.matching_preset();
                    egui::ComboBox::from_id_salt("q").selected_text(current.label()).width(96.0).show_ui(ui, |ui| {
                        for p in Preset::BUTTONS {
                            if ui.add(egui::Button::new(p.label()).selected(current == p).frame(false)).clicked() {
                                s.quality = Quality::preset(p);
                                s.handle.send(SessionMsg::Quality { quality: s.quality });
                            }
                        }
                    });
                });
            });
        });
    egui::CentralPanel::default().frame(egui::Frame::new().fill(Color32::from_rgb(8, 9, 12))).show(ui, |ui| {
        let avail = ui.available_rect_before_wrap();
        let Some((tex, _)) = s.textures.get(&mon.index) else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Waiting for the picture...").color(MUTED));
            });
            return;
        };
        let size = tex.size_vec2();
        let rect = fit_rect(avail, size.x, size.y);
        let resp = ui.allocate_rect(avail, if view_only { Sense::hover() } else { Sense::click_and_drag() });
        ui.painter().image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        if view_only {
            return;
        }
        let hovering = resp.hovered();
        if hovering && ctx.pointer_hover_pos().is_some_and(|p| rect.contains(p)) {
            // The remote pointer is drawn in the picture.
            ctx.set_cursor_icon(egui::CursorIcon::None);
        }
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(false));
        let (events, mods) = ctx.input(|i| (i.events.clone(), i.modifiers));
        for ev in map_events(&events, rect, mon.index, &mut s.mods, mods, focused) {
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
    });
    result
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_jobs(&ctx);
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
        self.top_bar(ui);
        self.bottom_bar(ui);
        if self.signed_in() || self.demo {
            self.machine_list(ui);
        }
        self.central(ui);
        self.rename_dialog(&ctx);
        self.monitor_windows(&ctx);
        self.screenshot_step(&ctx);
    }
}

// ------------------------------------------------------------------ widgets

fn logo(ui: &mut egui::Ui) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(r, CornerRadius::same(6), Color32::from_rgb(42, 127, 214));
    p.rect_filled(Rect::from_min_max(r.min, r.center() + Vec2::new(4.0, 4.0)), CornerRadius::same(5), ACCENT);
    p.rect_stroke(r.shrink(6.0), CornerRadius::same(2), Stroke::new(2.0, Color32::WHITE), StrokeKind::Inside);
    ui.label(RichText::new("Farlink").size(18.0).strong());
}

fn chip(ui: &mut egui::Ui, dot: Color32, text: &str) {
    egui::Frame::new()
        .fill(BG)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(10, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                status_dot(ui, dot);
                ui.label(RichText::new(text).size(12.5).color(MUTED));
            });
        });
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

fn machine_row(ui: &mut egui::Ui, m: &MachineView, selected: bool, in_session: bool, now: u64) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 54.0), Sense::click());
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
    p.text(rect.min + Vec2::new(52.0, 18.0), Align2::LEFT_CENTER, &m.machine.name, FontId::proportional(14.5), name_color);
    let screens = m.machine.monitors.len();
    let owner = m.owner_name.as_ref().map(|o| format!("{o} · ")).unwrap_or_default();
    let sub = if m.online {
        format!("{owner}{} · {} screen{}", short_os(&m.machine.os), screens, if screens == 1 { "" } else { "s" })
    } else {
        format!("offline · {} ago", human_duration(now.saturating_sub(m.machine.last_seen)))
    };
    p.text(rect.min + Vec2::new(52.0, 37.0), Align2::LEFT_CENTER, sub, FontId::proportional(12.0), MUTED);
    let dot = if in_session {
        ACCENT
    } else if m.online {
        OK
    } else {
        Color32::from_rgb(89, 97, 109)
    };
    p.circle_filled(Pos2::new(rect.max.x - 14.0, rect.center().y), 4.5, dot);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn short_os(os: &str) -> String {
    os.split_whitespace().take(3).collect::<Vec<_>>().join(" ")
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
        let out = map_events(&evs, rect(), 1, &mut prev, egui::Modifiers::default(), true);
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
        let out = map_events(&[press_out, release_out], rect(), 0, &mut prev, Default::default(), true);
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
            0,
            &mut prev,
            Default::default(),
            true,
        );
        assert_eq!(out, vec![InputEvent::Text { text: "a".into() }, InputEvent::Key { key: "Enter".into(), down: true }]);
        // Ctrl pressed: modifier goes down, Ctrl+S sent as key.
        let out = map_events(&[key(egui::Key::S, ctrl)], rect(), 0, &mut prev, ctrl, true);
        assert_eq!(out, vec![InputEvent::Key { key: "Control".into(), down: true }, InputEvent::Key { key: "S".into(), down: true }]);
        // Ctrl released.
        let out = map_events(&[], rect(), 0, &mut prev, Default::default(), true);
        assert_eq!(out, vec![InputEvent::Key { key: "Control".into(), down: false }]);
        // Copy shortcut becomes C.
        let out = map_events(&[egui::Event::Copy], rect(), 0, &mut prev, Default::default(), true);
        assert_eq!(out.len(), 2);
        // No keyboard when window not focused.
        assert!(map_events(&[egui::Event::Text("x".into())], rect(), 0, &mut prev, Default::default(), false).is_empty());
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
        let out = map_events(&[wheel], rect(), 0, &mut prev, Default::default(), true);
        assert_eq!(out, vec![InputEvent::Scroll { dx: 0.0, dy: 2.0 }]);
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
