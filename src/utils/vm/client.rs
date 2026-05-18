//! JSON-line client for talking to Avocado.app's IPC socket.
//!
//! On macOS, `avocado vm *` delegates VM lifecycle to a long-lived
//! `Avocado.app` (com.peridio.avocadodesktop) that owns the qemu process.
//! This module is the CLI side of that conversation: it connects to the
//! app's Unix socket, sends one-shot requests, and reads one-line responses.
//!
//! The app auto-launches if it isn't already running (Finder/Dock-style via
//! `open`). On Linux this whole module is unused.
//!
//! Protocol (mirrors `macos/AvocadoApp/IPCServer.swift`):
//!   Request:  `{"method": "...", "params": {...}, "id": <any>}\n`
//!   Response: `{"result": {...}, "id": <any>}\n`  or
//!             `{"error": "...",   "id": <any>}\n`
//!
//! Module-level `#[cfg(target_os = "macos")]` lives on the `pub mod`
//! declaration in `super::mod.rs`; we don't repeat it here.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Connected JSON-line client. One round trip per `request()` call.
pub struct Client {
    stream: UnixStream,
}

impl Client {
    /// Where Avocado.app advertises its IPC socket.
    pub fn socket_path() -> Result<PathBuf> {
        let home = directories::BaseDirs::new()
            .context("could not determine home directory")?
            .home_dir()
            .to_path_buf();
        Ok(home.join("Library/Application Support/Avocado/avocado.sock"))
    }

    /// Connect once. Errors if Avocado.app isn't running. Use
    /// [`Self::connect_or_launch`] for the user-facing path.
    pub fn connect() -> Result<Self> {
        let path = Self::socket_path()?;
        let stream =
            UnixStream::connect(&path).with_context(|| format!("connect to {}", path.display()))?;
        // Snug, not heroic — these requests are tiny and the app responds
        // synchronously. `vm.wait_ready` is the only long-runner and it sets
        // its own per-request timeout via the `timeout_sec` param.
        let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
        Ok(Self { stream })
    }

    /// Connect, auto-launching Avocado.app if its socket isn't there yet.
    /// Polls for up to ~6 s after launching for the socket to appear.
    pub fn connect_or_launch() -> Result<Self> {
        if let Ok(c) = Self::connect() {
            return Ok(c);
        }
        Self::launch_app().context("launching Avocado.app")?;
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut last_err: Option<anyhow::Error> = None;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            match Self::connect() {
                Ok(c) => return Ok(c),
                Err(e) => last_err = Some(e),
            }
        }
        bail!(
            "Avocado.app did not become reachable on {} within 6s ({})",
            Self::socket_path()?.display(),
            last_err.map(|e| format!("{e:#}")).unwrap_or_default()
        )
    }

    /// Locate Avocado.app on disk and launch it via `open -gja`. Search order:
    ///   1. `$AVOCADO_APP_PATH` env override
    ///   2. `/Applications/Avocado.app`
    ///   3. dev build under `avocado-vm/macos/build/Build/Products/Debug/Avocado.app`
    ///      (relative to this crate's manifest dir)
    fn launch_app() -> Result<()> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(p) = std::env::var("AVOCADO_APP_PATH") {
            candidates.push(PathBuf::from(p));
        }
        candidates.push(PathBuf::from("/Applications/Avocado.app"));
        if let Some(crate_parent) = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent() {
            candidates
                .push(crate_parent.join("avocado-vm/macos/build/Build/Products/Debug/Avocado.app"));
        }
        let app = candidates.iter().find(|p| p.exists()).cloned()
            .with_context(|| format!(
                "could not find Avocado.app (checked {candidates:?}). Build it with `make -C avocado-vm/macos build` or set AVOCADO_APP_PATH."
            ))?;
        // -g: don't bring to foreground. -j: hide. -a: target by bundle path.
        let status = std::process::Command::new("open")
            .args(["-gja"])
            .arg(&app)
            .status()
            .context("spawn `open`")?;
        if !status.success() {
            bail!("`open -gja {}` exited {status}", app.display());
        }
        Ok(())
    }

    /// One JSON-line request → one JSON-line response. Returns the `result`
    /// payload; surfaces server-side `error` strings as anyhow errors.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let req = json!({
            "method": method,
            "params": params,
            "id": 1,
        });
        let mut buf = serde_json::to_vec(&req).context("encode request")?;
        buf.push(b'\n');
        self.stream.write_all(&buf).context("write request")?;

        let mut reader = BufReader::new(&self.stream);
        let mut line = String::new();
        let n = reader.read_line(&mut line).context("read response line")?;
        if n == 0 {
            bail!("app closed the connection without responding");
        }
        let resp: Value = serde_json::from_str(line.trim())
            .with_context(|| format!("parse response: {line:?}"))?;
        if let Some(err) = resp.get("error").and_then(|v| v.as_str()) {
            bail!("Avocado.app error: {err}");
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }
}
