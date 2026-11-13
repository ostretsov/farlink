//! Farlink — a small self-hosted remote desktop system.
//!
//! Three programs share this library:
//! * `farlink`        — desktop viewer (egui): one window per remote monitor,
//!   remembered window positions, quality settings, view-only mode.
//! * `farlink-server` — relay + web admin + install links + update feed.
//! * `farlink-host`   — helper on every remote machine. It can only be
//!   controlled (never controls others) and only connects out to the relay.

pub mod autostart;
pub mod clipboard;
pub mod codec;
pub mod config;
pub mod desktop;
pub mod files;
pub mod grants;
pub mod host;
pub mod install;
pub mod keygrab;
pub mod keys;
pub mod layout;
pub mod net;
pub mod palette;
pub mod ping;
pub mod proto;
pub mod quality;
pub mod server;
pub mod server_update;
pub mod sysstat;
pub mod tls;
pub mod tray;
pub mod update;
pub mod util;
pub mod version;
pub mod viewer;

/// Version of all three programs (they are always released together).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Product name shown in windows, services and firewall rules.
pub const PRODUCT: &str = "Farlink";
