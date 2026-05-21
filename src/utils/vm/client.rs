//! Fire-and-forget IPC notifications to Avocado.app.
//!
//! The CLI is authoritative for the qemu lifecycle (spawn, signal, pidfile).
//! These notifications exist purely to keep the desktop app's dashboard in
//! sync — `.starting` / `.stopping` are visible the instant the CLI begins
//! the transition rather than waiting for the desktop's pidfile reconciler
//! to notice ~2 s later.
//!
//! Crucially this is **never load-bearing**:
//! - All timeouts are aggressive (100 ms). If the desktop is bogged down
//!   rendering, the CLI moves on without blocking.
//! - Connect failure (app not running, app not installed, socket missing)
//!   is a silent no-op.
//! - The desktop has a pidfile reconciler as a backstop, so a dropped
//!   notification self-heals within ~2 s.
//!
//! Wire format (mirrors `macos/AvocadoApp/IPCServer.swift`):
//!   Request:  `{"method": "...", "params": {...}, "id": <any>}\n`
//!   Response: `{"result": {...}, "id": <any>}\n`
//!
//! `#[cfg(target_os = "macos")]` lives on `pub mod client;` in `mod.rs`.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// Aggressive cap on the total time any one notification can steal from
/// the CLI. Connect, write, and the brief response wait are each bounded
/// by this; the user-visible worst case is ~3× this when the socket
/// exists but the desktop's main actor is stuck.
const NOTIFY_TIMEOUT: Duration = Duration::from_millis(100);

/// Where Avocado.app advertises its IPC socket.
fn socket_path() -> Option<PathBuf> {
    let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
    Some(home.join("Library/Application Support/Avocado/avocado.sock"))
}

/// Best-effort fire-and-forget notification to the desktop app. Never
/// blocks the CLI for more than ~3× [`NOTIFY_TIMEOUT`]; never returns an
/// error (failure modes are all logged-and-swallowed). Safe to call
/// regardless of whether the desktop is installed or reachable.
pub fn notify(method: &str, params: Value) {
    let Some(path) = socket_path() else { return };
    let Ok(stream) = UnixStream::connect(&path) else {
        return;
    };
    // Apply timeouts up-front; without these `write_all` and `read_line`
    // can stall on a hung peer for far longer than we're willing to wait.
    let _ = stream.set_write_timeout(Some(NOTIFY_TIMEOUT));
    let _ = stream.set_read_timeout(Some(NOTIFY_TIMEOUT));

    let req = json!({ "method": method, "params": params, "id": 1 });
    let Ok(mut buf) = serde_json::to_vec(&req) else {
        return;
    };
    buf.push(b'\n');

    let mut stream = stream;
    if stream.write_all(&buf).is_err() {
        return;
    }
    // Drain the response if the desktop is quick enough. We don't care
    // about the contents; this just prevents the desktop from logging a
    // "peer closed mid-response" warning when we drop the stream below.
    // If it times out, fine — the request was already written.
    let mut line = String::new();
    let _ = BufReader::new(&stream).read_line(&mut line);
}
