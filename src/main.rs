//! farlink — desktop app to view and control your computers.
//!
//! ```text
//! farlink                         # normal start
//! farlink https://…/g/TOKEN       # open a temporary link
//! farlink --demo                  # sample computers, no server needed
//! farlink --screenshot out.png    # draw ~120 frames, save the window, exit
//! farlink --tray                  # start hidden in the tray (at login)
//! farlink mcp [--server URL]      # MCP server for AI assistants (stdin/stdout)
//! ```

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use farlink::util::Args;
use farlink::viewer::{ViewerOptions, run};

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args = Args::parse(std::env::args().skip(1), &["screenshot", "server", "user"]);
    if args.command() == Some("mcp") {
        // stdout carries the protocol: messages go to stderr only.
        if let Err(e) = farlink::mcp::run(&args) {
            eprintln!("farlink mcp: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let opts = ViewerOptions {
        screenshot: args.get("screenshot").map(Into::into),
        demo: args.has("demo"),
        open_link: args.positional.iter().find(|a| a.contains("/g/")).cloned(),
        start_hidden: args.has("tray"),
    };
    run(opts)
}
