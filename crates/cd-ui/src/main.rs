//! Desktop shell: a native window that hosts the daemon console served over
//! HTTP. If the local daemon is not running, the shell starts one first; the
//! daemon keeps running after the window closes so cross-device sync survives.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::env;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

const DEFAULT_CONSOLE: &str = "http://127.0.0.1:7824/";
const DAEMON_ADDR: &str = "127.0.0.1:7824";

fn main() {
    let console = env::var("CROSSDEV_CONSOLE").unwrap_or_else(|_| DEFAULT_CONSOLE.to_owned());
    let local = console == DEFAULT_CONSOLE;
    let ready = !local || ensure_daemon();
    tauri::Builder::default()
        .setup(move |app| {
            use tauri::{Url, WebviewUrl, WebviewWindowBuilder};
            let url = match console.parse::<Url>() {
                Ok(url) if ready => WebviewUrl::External(url),
                _ => WebviewUrl::App("index.html".into()),
            };
            WebviewWindowBuilder::new(app, "main", url)
                .title("Crossdev · 跨设备协同")
                .inner_size(1180.0, 820.0)
                .min_inner_size(1024.0, 680.0)
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to run Crossdev desktop shell");
}

/// Returns true when the daemon answers on its HTTP port, starting it when
/// necessary. A missing `crossdev` binary is not fatal: the shell falls back to
/// the offline page with instructions.
fn ensure_daemon() -> bool {
    if daemon_ready() {
        return true;
    }
    let program = daemon_binary();
    let spawned = Command::new(&program)
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if spawned.is_err() {
        return false;
    }
    for _ in 0..50 {
        if daemon_ready() {
            return true;
        }
        sleep(Duration::from_millis(100));
    }
    false
}

fn daemon_ready() -> bool {
    let Ok(addr) = DAEMON_ADDR.parse::<SocketAddr>() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Prefers the `crossdev` shipped next to this executable (the Debian package
/// installs both in `/usr/bin`), then falls back to `PATH`.
fn daemon_binary() -> PathBuf {
    if let Ok(current) = env::current_exe() {
        if let Some(parent) = current.parent() {
            let sibling = parent.join("crossdev");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("crossdev")
}
