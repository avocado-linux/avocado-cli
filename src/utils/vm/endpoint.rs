//! Host-side address of a QEMU chardev socket (QMP, qga, control plane).
//!
//! On Unix hosts QEMU serves each chardev on a Unix-domain socket in the VM
//! state dir. Windows has no usable equivalent for us (tokio has no
//! `UnixStream` there, and QEMU's `pipe` chardev accepts one client and
//! never re-listens), so there QEMU serves the same chardevs on loopback
//! TCP and the chosen port is written to a small file next to where the
//! socket would have been. The JSON protocols on top are identical.
//!
//! The port file doubles as the "is the socket there?" marker, so callers
//! that used to test `qmp.sock` for existence test the endpoint instead.
//!
//! Loopback TCP has no filesystem permissions: any process of any local
//! account can connect, unlike a Unix socket in the user's state dir (or a
//! default-ACL named pipe, which other users can only read). Another local
//! account can therefore drive QMP with the rights of the account running
//! QEMU, and the VM's dockerd, which has the workspace mounted, through the
//! supervisor's docker listener. That is accepted for a single-user dev
//! box; a shared Windows host should not run the avocado-vm.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncRead, AsyncWrite};

pub type BoxedRead = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxedWrite = Box<dyn AsyncWrite + Unpin + Send>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// Unix-domain socket QEMU creates at this path.
    Unix(PathBuf),
    /// Loopback TCP port, recorded as decimal text in this file.
    TcpPortFile(PathBuf),
}

impl Endpoint {
    /// The endpoint that stands for `socket` on this host: the Unix socket
    /// itself, or on Windows the loopback port recorded in the file of the
    /// same stem (`qmp.sock` -> `qmp.port`). Lets code that is handed a
    /// socket path (the supervisor's argv) stay platform-neutral.
    pub fn for_socket(socket: &Path) -> Self {
        if cfg!(windows) {
            Endpoint::TcpPortFile(socket.with_extension("port"))
        } else {
            Endpoint::Unix(socket.to_path_buf())
        }
    }

    /// The file that represents this endpoint on disk: the socket itself,
    /// or the port file. Removed on VM shutdown.
    pub fn file(&self) -> &Path {
        match self {
            Endpoint::Unix(p) | Endpoint::TcpPortFile(p) => p,
        }
    }

    pub fn exists(&self) -> bool {
        self.file().exists()
    }

    /// Reserve the address before QEMU launches. A no-op for Unix sockets
    /// (QEMU creates them); for TCP this picks a free loopback port and
    /// records it.
    pub fn allocate(&self) -> Result<()> {
        match self {
            Endpoint::Unix(_) => Ok(()),
            Endpoint::TcpPortFile(file) => {
                let port = super::qemu::pick_free_port()?;
                std::fs::write(file, port.to_string())
                    .with_context(|| format!("writing {}", file.display()))
            }
        }
    }

    fn tcp_port(file: &Path) -> Result<u16> {
        let raw =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        raw.trim()
            .parse()
            .with_context(|| format!("port file {} has invalid content", file.display()))
    }

    /// The address part of a QEMU `-chardev socket,...` option.
    pub fn chardev_addr(&self) -> Result<String> {
        match self {
            Endpoint::Unix(path) => Ok(format!("path={}", path.display())),
            Endpoint::TcpPortFile(file) => {
                Ok(format!("host=127.0.0.1,port={}", Self::tcp_port(file)?))
            }
        }
    }

    /// Open a client connection.
    pub async fn connect(&self) -> Result<(BoxedRead, BoxedWrite)> {
        match self {
            Endpoint::Unix(path) => connect_unix(path).await,
            Endpoint::TcpPortFile(file) => {
                let port = Self::tcp_port(file)?;
                let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .with_context(|| format!("connect to 127.0.0.1:{port}"))?;
                let _ = stream.set_nodelay(true);
                let (r, w) = stream.into_split();
                Ok((Box::new(r), Box::new(w)))
            }
        }
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Endpoint::Unix(p) => write!(f, "{}", p.display()),
            Endpoint::TcpPortFile(p) => write!(f, "tcp port in {}", p.display()),
        }
    }
}

#[cfg(unix)]
async fn connect_unix(path: &Path) -> Result<(BoxedRead, BoxedWrite)> {
    let stream = tokio::net::UnixStream::connect(path)
        .await
        .with_context(|| format!("connect to {}", path.display()))?;
    let (r, w) = stream.into_split();
    Ok((Box::new(r), Box::new(w)))
}

#[cfg(not(unix))]
async fn connect_unix(path: &Path) -> Result<(BoxedRead, BoxedWrite)> {
    anyhow::bail!(
        "Unix-domain socket {} is not supported on this host",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn unix_endpoint_renders_a_path_chardev() {
        let e = Endpoint::Unix(PathBuf::from("/tmp/vm/qmp.sock"));
        assert_eq!(e.chardev_addr().unwrap(), "path=/tmp/vm/qmp.sock");
        assert!(e.allocate().is_ok());
    }

    #[test]
    fn tcp_endpoint_allocates_a_port_and_renders_it() {
        let tmp = tempfile::tempdir().unwrap();
        let e = Endpoint::TcpPortFile(tmp.path().join("qmp.port"));
        assert!(!e.exists());
        assert!(e.chardev_addr().is_err());
        e.allocate().unwrap();
        assert!(e.exists());
        let port: u16 = std::fs::read_to_string(e.file()).unwrap().parse().unwrap();
        assert!(port > 0);
        assert_eq!(
            e.chardev_addr().unwrap(),
            format!("host=127.0.0.1,port={port}")
        );
    }

    /// The QMP client speaks the same protocol over the TCP endpoint as it
    /// does over a Unix socket.
    #[tokio::test]
    async fn qmp_client_negotiates_over_tcp() {
        let tmp = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let e = Endpoint::TcpPortFile(tmp.path().join("qmp.port"));
        std::fs::write(e.file(), port.to_string()).unwrap();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (rh, mut wh) = stream.into_split();
            let mut rh = BufReader::new(rh);
            wh.write_all(b"{\"QMP\":{\"version\":{}}}\n").await.unwrap();
            let mut line = String::new();
            rh.read_line(&mut line).await.unwrap();
            assert!(line.contains("qmp_capabilities"));
            wh.write_all(b"{\"return\":{}}\n").await.unwrap();
            line.clear();
            rh.read_line(&mut line).await.unwrap();
            assert!(line.contains("query-status"));
            wh.write_all(b"{\"return\":{\"status\":\"running\"}}\n")
                .await
                .unwrap();
        });

        let mut client = crate::utils::vm::qmp::QmpClient::connect_endpoint(&e)
            .await
            .unwrap();
        let v = client.execute("query-status", None).await.unwrap();
        assert_eq!(v["status"], "running");
    }
}
