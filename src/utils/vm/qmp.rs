//! Minimal QMP (QEMU Machine Protocol) client.
//!
//! QMP is line-delimited JSON over a Unix socket. Sessions begin with the
//! server sending a greeting, then the client sends
//! `{"execute":"qmp_capabilities"}` once to leave Capabilities Negotiation
//! mode. After that, command/response pairs flow. Asynchronous events from
//! the server (e.g. `RESET`, `SHUTDOWN`) interleave with responses; we drop
//! them by default since we only need request/reply for hot-plug ops.
//!
//! This client is intentionally small. It opens, negotiates, and exposes
//! `execute(cmd, args)`. Long-running monitoring of events is out of scope.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::{timeout, Duration};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

pub struct QmpClient {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl QmpClient {
    /// Connect to the QMP socket and complete capability negotiation.
    pub async fn connect(socket: &Path) -> Result<Self> {
        let stream = timeout(HANDSHAKE_TIMEOUT, UnixStream::connect(socket))
            .await
            .with_context(|| format!("timeout connecting to QMP {}", socket.display()))?
            .with_context(|| format!("connect to QMP {}", socket.display()))?;
        let (rh, wh) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(rh),
            writer: wh,
        };

        // Drain the greeting (`{"QMP":{...}}`)
        let _ = client.read_line_with(HANDSHAKE_TIMEOUT).await?;

        // Send qmp_capabilities to leave Capabilities Negotiation mode.
        let neg = serde_json::json!({"execute": "qmp_capabilities"});
        let raw = serde_json::to_vec(&neg)?;
        client.writer.write_all(&raw).await?;
        client.writer.write_all(b"\n").await?;
        client.writer.flush().await?;
        let _ack = client.read_until_response(HANDSHAKE_TIMEOUT).await?;
        Ok(client)
    }

    /// Execute one command (e.g. `"device_add"`) with arguments.
    /// Returns the `"return"` payload of the response.
    pub async fn execute(&mut self, cmd: &str, args: Option<Value>) -> Result<Value> {
        let mut msg = serde_json::Map::new();
        msg.insert("execute".to_string(), Value::String(cmd.to_string()));
        if let Some(a) = args {
            msg.insert("arguments".to_string(), a);
        }
        let raw = serde_json::to_vec(&Value::Object(msg))?;
        self.writer.write_all(&raw).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;
        let v = self.read_until_response(COMMAND_TIMEOUT).await?;
        if let Some(err) = v.get("error") {
            bail!("QMP error from `{cmd}`: {err}");
        }
        Ok(v.get("return").cloned().unwrap_or(Value::Null))
    }

    /// Run an HMP (human monitor) command via `human-monitor-command`.
    /// HMP errors come back *inside* the returned string, not the QMP
    /// `error` field, so callers must inspect the returned text.
    pub async fn human_monitor_command(&mut self, command_line: &str) -> Result<String> {
        let v = self
            .execute(
                "human-monitor-command",
                Some(serde_json::json!({ "command-line": command_line })),
            )
            .await?;
        Ok(v.as_str().unwrap_or_default().to_string())
    }

    /// Add a slirp host-forwarding rule to a `user` netdev at runtime:
    /// `host_addr:host_port` on the host → `:guest_port` in the guest.
    /// Use `host_addr = "0.0.0.0"` to make the forward reachable from the
    /// LAN (not just loopback). TCP only — that's all deploy needs.
    pub async fn hostfwd_add(
        &mut self,
        netdev: &str,
        host_addr: &str,
        host_port: u16,
        guest_port: u16,
    ) -> Result<()> {
        // qemu hostfwd_add form: `<netdev> tcp:<hostaddr>:<hostport>-:<guestport>`
        let out = self
            .human_monitor_command(&format!(
                "hostfwd_add {netdev} tcp:{host_addr}:{host_port}-:{guest_port}"
            ))
            .await?;
        let out = out.trim();
        if !out.is_empty() {
            bail!("hostfwd_add failed: {out}");
        }
        Ok(())
    }

    /// Remove a previously-added slirp host-forwarding rule. The remove
    /// form omits the guest side: `<netdev> tcp:<hostaddr>:<hostport>`.
    /// A "not found" reply is tolerated so cleanup is idempotent.
    pub async fn hostfwd_remove(
        &mut self,
        netdev: &str,
        host_addr: &str,
        host_port: u16,
    ) -> Result<()> {
        let out = self
            .human_monitor_command(&format!(
                "hostfwd_remove {netdev} tcp:{host_addr}:{host_port}"
            ))
            .await?;
        let out = out.trim();
        if !out.is_empty() && !out.contains("not found") {
            bail!("hostfwd_remove failed: {out}");
        }
        Ok(())
    }

    /// Read lines until we see one that has a `return` or `error` key
    /// (i.e. a command response). Events are skipped.
    async fn read_until_response(&mut self, dur: Duration) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + dur;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                bail!("QMP timeout waiting for response");
            }
            let line = self.read_line_with(remaining).await?;
            let v: Value = serde_json::from_str(&line)
                .with_context(|| format!("malformed QMP line: {line:?}"))?;
            if v.get("event").is_some() {
                // Drop asynchronous events for now.
                continue;
            }
            return Ok(v);
        }
    }

    async fn read_line_with(&mut self, dur: Duration) -> Result<String> {
        let mut buf = String::new();
        let n = timeout(dur, self.reader.read_line(&mut buf))
            .await
            .context("QMP read timeout")?
            .context("QMP read error")?;
        if n == 0 {
            bail!("QMP connection closed");
        }
        Ok(buf)
    }

    /// Shut down QEMU cleanly via QMP.
    pub async fn quit(&mut self) -> Result<()> {
        let _ = self.execute("quit", None).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;

    /// Spin up a mock QMP server on a temp socket. Handler is async-fn-like:
    /// it gets a reader half and writer half and does whatever scripted I/O.
    async fn spawn_mock<F, Fut>(handler: F) -> std::path::PathBuf
    where
        F: FnOnce(
                tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
                tokio::net::unix::OwnedWriteHalf,
            ) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let tmp = tempfile::tempdir().unwrap().keep();
        let sock_path = tmp.join("qmp.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let sp = sock_path.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (rh, wh) = stream.into_split();
            handler(tokio::io::BufReader::new(rh), wh).await;
            let _ = sp;
        });
        sock_path
    }

    #[tokio::test]
    async fn negotiates_and_executes_query_status() {
        let socket = spawn_mock(|mut rh, mut wh| async move {
            // Greeting
            wh.write_all(
                b"{\"QMP\":{\"version\":{\"qemu\":{\"major\":10}},\"capabilities\":[]}}\n",
            )
            .await
            .unwrap();
            // Wait for qmp_capabilities, ack it
            let mut line = String::new();
            rh.read_line(&mut line).await.unwrap();
            assert!(line.contains("qmp_capabilities"));
            wh.write_all(b"{\"return\":{}}\n").await.unwrap();
            // Now expect a real command
            line.clear();
            rh.read_line(&mut line).await.unwrap();
            assert!(line.contains("\"query-status\""));
            wh.write_all(b"{\"return\":{\"running\":true}}\n")
                .await
                .unwrap();
        })
        .await;

        let mut client = QmpClient::connect(&socket).await.unwrap();
        let v = client.execute("query-status", None).await.unwrap();
        assert_eq!(v["running"], Value::Bool(true));
    }

    #[tokio::test]
    async fn surfaces_qmp_error() {
        let socket = spawn_mock(|mut rh, mut wh| async move {
            wh.write_all(b"{\"QMP\":{\"version\":{}}}\n").await.unwrap();
            let mut line = String::new();
            rh.read_line(&mut line).await.unwrap();
            wh.write_all(b"{\"return\":{}}\n").await.unwrap();
            line.clear();
            rh.read_line(&mut line).await.unwrap();
            wh.write_all(b"{\"error\":{\"class\":\"GenericError\",\"desc\":\"nope\"}}\n")
                .await
                .unwrap();
        })
        .await;

        let mut client = QmpClient::connect(&socket).await.unwrap();
        let err = client.execute("device_add", None).await.unwrap_err();
        assert!(format!("{err:#}").contains("nope"));
    }

    #[tokio::test]
    async fn hostfwd_add_ok_on_empty_return() {
        let socket = spawn_mock(|mut rh, mut wh| async move {
            wh.write_all(b"{\"QMP\":{\"version\":{}}}\n").await.unwrap();
            let mut line = String::new();
            rh.read_line(&mut line).await.unwrap();
            wh.write_all(b"{\"return\":{}}\n").await.unwrap();
            line.clear();
            rh.read_line(&mut line).await.unwrap();
            // The HMP command is carried inside human-monitor-command.
            assert!(line.contains("human-monitor-command"));
            assert!(line.contains("hostfwd_add net0 tcp:0.0.0.0:8585-:8585"));
            // Success: hostfwd_add prints nothing.
            wh.write_all(b"{\"return\":\"\"}\n").await.unwrap();
        })
        .await;

        let mut client = QmpClient::connect(&socket).await.unwrap();
        client
            .hostfwd_add("net0", "0.0.0.0", 8585, 8585)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn hostfwd_add_errors_on_nonempty_return() {
        let socket = spawn_mock(|mut rh, mut wh| async move {
            wh.write_all(b"{\"QMP\":{\"version\":{}}}\n").await.unwrap();
            let mut line = String::new();
            rh.read_line(&mut line).await.unwrap();
            wh.write_all(b"{\"return\":{}}\n").await.unwrap();
            line.clear();
            rh.read_line(&mut line).await.unwrap();
            // HMP errors come back in the return string, not the QMP error field.
            wh.write_all(b"{\"return\":\"Could not set up host forwarding rule\\n\"}\n")
                .await
                .unwrap();
        })
        .await;

        let mut client = QmpClient::connect(&socket).await.unwrap();
        let err = client
            .hostfwd_add("net0", "0.0.0.0", 8585, 8585)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("hostfwd_add failed"));
    }

    #[tokio::test]
    async fn hostfwd_remove_tolerates_not_found() {
        let socket = spawn_mock(|mut rh, mut wh| async move {
            wh.write_all(b"{\"QMP\":{\"version\":{}}}\n").await.unwrap();
            let mut line = String::new();
            rh.read_line(&mut line).await.unwrap();
            wh.write_all(b"{\"return\":{}}\n").await.unwrap();
            line.clear();
            rh.read_line(&mut line).await.unwrap();
            assert!(line.contains("hostfwd_remove net0 tcp:0.0.0.0:8585"));
            // A stale-cleanup "not found" reply must not be treated as an error.
            wh.write_all(
                b"{\"return\":\"host forwarding rule for tcp:0.0.0.0:8585 not found\\n\"}\n",
            )
            .await
            .unwrap();
        })
        .await;

        let mut client = QmpClient::connect(&socket).await.unwrap();
        client
            .hostfwd_remove("net0", "0.0.0.0", 8585)
            .await
            .unwrap();
    }
}
