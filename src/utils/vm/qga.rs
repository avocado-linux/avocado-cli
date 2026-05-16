//! Minimal qemu-guest-agent client.
//!
//! qga speaks the same line-delimited JSON-RPC dialect as QMP, but over a
//! separate Unix socket that's bridged into the guest via a virtio-serial
//! port (`org.qemu.guest_agent.0`). Unlike QMP it has no greeting or
//! capabilities negotiation — you connect and start sending commands.
//!
//! We use it for the boot handshake (`guest-sync` returns once the agent is
//! ready inside the guest) and as a low-level fallback when sshd is wedged.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::{timeout, Duration};

#[allow(dead_code)]
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct QgaClient {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl QgaClient {
    /// Connect to the qga socket. Does not require any handshake.
    pub async fn connect(socket: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("connect to qga {}", socket.display()))?;
        let (rh, wh) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(rh),
            writer: wh,
        })
    }

    /// Execute a qga command. Returns the `"return"` value or an error.
    /// Reserved for Phase 4+ guest-exec use cases.
    #[allow(dead_code)]
    pub async fn execute(&mut self, cmd: &str, args: Option<Value>) -> Result<Value> {
        self.execute_with_timeout(cmd, args, DEFAULT_TIMEOUT).await
    }

    pub async fn execute_with_timeout(
        &mut self,
        cmd: &str,
        args: Option<Value>,
        dur: Duration,
    ) -> Result<Value> {
        let mut msg = serde_json::Map::new();
        msg.insert("execute".to_string(), Value::String(cmd.to_string()));
        if let Some(a) = args {
            msg.insert("arguments".to_string(), a);
        }
        let raw = serde_json::to_vec(&Value::Object(msg))?;
        self.writer.write_all(&raw).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;

        let mut line = String::new();
        let n = timeout(dur, self.reader.read_line(&mut line))
            .await
            .with_context(|| format!("qga timeout on `{cmd}`"))?
            .with_context(|| format!("qga read error on `{cmd}`"))?;
        if n == 0 {
            bail!("qga connection closed during `{cmd}`");
        }
        let v: Value = serde_json::from_str(&line)
            .with_context(|| format!("malformed qga response: {line:?}"))?;
        if let Some(err) = v.get("error") {
            bail!("qga error from `{cmd}`: {err}");
        }
        Ok(v.get("return").cloned().unwrap_or(Value::Null))
    }

    /// Probe liveness. `guest-sync` echoes back a token of our choice; we
    /// pick a random one each call so stale responses can't fool us.
    pub async fn ping(&mut self) -> Result<()> {
        let token: u32 = rand::random();
        let resp = self
            .execute_with_timeout(
                "guest-sync",
                Some(serde_json::json!({"id": token})),
                Duration::from_secs(5),
            )
            .await?;
        let got = resp.as_u64().unwrap_or(0);
        if got != token as u64 {
            bail!("guest-sync echoed {got}, expected {token}");
        }
        Ok(())
    }

    /// `guest-info` — qga version + supported commands. Cheap, useful for
    /// confirming the guest agent is fully wired. Reserved for `vm doctor`.
    #[allow(dead_code)]
    pub async fn info(&mut self) -> Result<Value> {
        self.execute("guest-info", None).await
    }
}
