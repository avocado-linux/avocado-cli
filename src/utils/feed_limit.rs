//! Saying so when a package repository answers HTTP 429.
//!
//! Any repository or CDN may answer `429 Too Many Requests`, usually with
//! `Retry-After`. dnf honours neither: it marks the only mirror failed and
//! stops with "No more mirrors to try", which reads as a broken repository and
//! hides the one fact that matters, that waiting would fix it. So when a
//! container run fails, the CLI asks the repository once whether it is
//! answering 429 and, if so, says so. It does not wait and retry on its own:
//! an install that silently sleeps in CI is harder to diagnose than one that
//! fails with the reason.

use std::time::Duration;

use reqwest::header::{HeaderMap, RETRY_AFTER};

use crate::utils::feeds::FeedEndpoint;
use reqwest::StatusCode;

/// `Retry-After` in seconds. The edge sends delta-seconds; an HTTP-date or
/// anything unparsable is treated as absent rather than guessed at.
pub(crate) fn retry_after_secs(headers: &HeaderMap) -> Option<u64> {
    headers.get(RETRY_AFTER)?.to_str().ok()?.trim().parse().ok()
}

/// The message for a 429. `identified` is whether requests carried a Connect
/// key id (logged in, or `AVOCADO_CONNECT_TOKEN`). Only a client that is not
/// is pointed at logging in: identified requests let a repository tell
/// machines behind one shared address apart.
pub(crate) fn rate_limit_message(retry_after: Option<u64>, identified: bool) -> String {
    let when = match retry_after {
        Some(s) => format!("Try again in {s} s."),
        None => "Try again in a few minutes.".to_string(),
    };
    let base = format!(
        "The package repository answered HTTP 429 (too many requests), so dnf \
         could not download from it. {when}"
    );
    if identified {
        base
    } else {
        format!(
            "{base} If this machine shares a network address with other builds, \
             `avocado login` (or AVOCADO_CONNECT_TOKEN in CI) identifies it to the \
             repository."
        )
    }
}

/// Whether the repository is answering this client 429 right now: one HEAD
/// with the User-Agent dnf sent, the repo TLS settings dnf uses
/// (`AVOCADO_REPO_CA`, `AVOCADO_REPO_INSECURE`), and a named feed's own CA,
/// TLS setting and credential, so a feed that authenticates before it rate
/// limits answers the probe as it answered dnf. `Some(retry_after)` on a 429,
/// `None` for any other answer or no answer. Rate limits are counted per client
/// rather than per object, so the repository root is as good a probe as the
/// object dnf failed on.
// ponytail: probes from the CLI's host, so it sees what dnf saw only when both
// leave through the same address (the normal case). With `--runs-on` or a VM
// on another egress, an anonymous per-address 429 goes unexplained and dnf's
// own message stands. Probe from inside the container if that case matters.
pub(crate) async fn probe(feed: &FeedEndpoint, user_agent: &str) -> Option<Option<u64>> {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .user_agent(user_agent);
    if !feed.tls_verify
        || std::env::var("AVOCADO_REPO_INSECURE")
            .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
    {
        builder = builder.danger_accept_invalid_certs(true);
    }
    if let Some(path) = std::env::var("AVOCADO_REPO_CA")
        .ok()
        .filter(|p| !p.is_empty())
    {
        for cert in reqwest::Certificate::from_pem_bundle(&std::fs::read(path).ok()?).ok()? {
            builder = builder.add_root_certificate(cert);
        }
    }
    let client = crate::utils::feeds::trust_feed_ca(builder, feed.ca.as_deref(), "feed")
        .ok()?
        .build()
        .ok()?;
    let req = client.head(format!("{}/", feed.url.trim_end_matches('/')));
    let resp = crate::utils::feeds::authorize_feed(req, feed.credential.as_ref())
        .send()
        .await
        .ok()?;
    (resp.status() == StatusCode::TOO_MANY_REQUESTS).then(|| retry_after_secs(resp.headers()))
}

/// Set once the explanation has been printed. Concurrent installs (`sdk
/// install` runs several at once) all fail together; one message is enough.
static EXPLAINED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// After a failed run: if the repository is answering 429, say so, once.
/// `user_agent` is the run's `AVOCADO_FEED_UA`: the edge counts limits per
/// identity, so probing with any other one can read a different counter.
pub(crate) async fn explain_if_rate_limited(feed: &FeedEndpoint, user_agent: &str) {
    use std::sync::atomic::Ordering;
    if EXPLAINED.load(Ordering::Relaxed) {
        return;
    }
    if let Some(retry_after) = probe(feed, user_agent).await {
        if EXPLAINED.swap(true, Ordering::Relaxed) {
            return;
        }
        // Not print_error: that is silent under the TUI and --json, which is
        // where installs run.
        crate::utils::container::print_failure_notice(&rate_limit_message(
            retry_after,
            user_agent.contains(";key/"),
        ));
    }
}

/// A local HTTP server answering every request with `response`, for tests of
/// anything that talks to a feed.
#[cfg(test)]
pub(crate) fn serve(response: &'static str) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    const UA: &str = "avocado-cli/test";

    fn at(url: &str) -> FeedEndpoint {
        FeedEndpoint::plain(url.to_string())
    }

    fn headers(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, value.parse().unwrap());
        h
    }

    #[test]
    fn retry_after_reads_delta_seconds_only() {
        assert_eq!(retry_after_secs(&headers("300")), Some(300));
        assert_eq!(retry_after_secs(&headers(" 7 ")), Some(7));
        assert_eq!(
            retry_after_secs(&headers("Wed, 21 Oct 2026 07:28:00 GMT")),
            None
        );
        assert_eq!(retry_after_secs(&headers("-1")), None);
        assert_eq!(retry_after_secs(&HeaderMap::new()), None);
    }

    #[test]
    fn anonymous_message_names_the_wait_and_both_ways_to_identify() {
        let m = rate_limit_message(Some(300), false);
        assert!(m.contains("HTTP 429"), "{m}");
        assert!(m.contains("300 s"), "{m}");
        assert!(m.contains("avocado login"), "{m}");
        assert!(m.contains("AVOCADO_CONNECT_TOKEN"), "{m}");
    }

    #[test]
    fn identified_message_does_not_tell_a_logged_in_user_to_log_in() {
        let m = rate_limit_message(Some(60), true);
        assert!(m.contains("60 s"), "{m}");
        assert!(!m.contains("avocado login"), "{m}");
        assert!(!m.contains("AVOCADO_CONNECT_TOKEN"), "{m}");
    }

    #[test]
    fn missing_retry_after_still_gives_a_wait() {
        assert!(rate_limit_message(None, false).contains("a few minutes"));
    }

    #[tokio::test]
    async fn probe_reports_429_with_its_retry_after() {
        let url =
            serve("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 42\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(probe(&at(&url), UA).await, Some(Some(42)));
    }

    #[tokio::test]
    async fn probe_reports_429_without_retry_after() {
        let url = serve("HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(probe(&at(&url), UA).await, Some(None));
    }

    #[tokio::test]
    async fn probe_ignores_every_other_answer() {
        for resp in [
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 5\r\nContent-Length: 0\r\n\r\n",
        ] {
            assert_eq!(probe(&at(&serve(resp)), UA).await, None, "{resp}");
        }
    }

    #[tokio::test]
    async fn probe_asks_with_the_runs_user_agent() {
        // A repository limiting only one identity: the probe has to send the
        // run's User-Agent, not the default credential's, to see that 429.
        use std::io::{Read, Write};
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let limited = String::from_utf8_lossy(&buf[..n]).contains(";key/minted;tier/3");
                let status = if limited {
                    "429 Too Many Requests"
                } else {
                    "200 OK"
                };
                let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n");
            }
        });
        assert_eq!(
            probe(&at(&url), "avocado-cli/test;key/minted;tier/3").await,
            Some(None)
        );
        assert_eq!(
            probe(&at(&url), "avocado-cli/test;key/default;tier/1").await,
            None
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn probe_uses_the_repo_tls_settings_dnf_uses() {
        // An HTTPS feed under a private CA: without AVOCADO_REPO_CA or
        // AVOCADO_REPO_INSECURE the probe fails verification and stays silent
        // while dnf, which has them, got the 429.
        let tls = crate::utils::container_dev::tls::TlsMaterial::generate("feed", &[]).unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(tls.server_config());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((tcp, _)) = listener.accept().await {
                let Ok(mut s) = acceptor.accept(tcp).await else {
                    continue;
                };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\n\r\n")
                    .await;
                let _ = s.shutdown().await;
            }
        });
        let ca = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(ca.path(), tls.ca_cert_pem()).unwrap();

        std::env::remove_var("AVOCADO_REPO_CA");
        assert_eq!(
            probe(&at(&url), UA).await,
            None,
            "untrusted CA must not verify"
        );
        std::env::set_var("AVOCADO_REPO_CA", ca.path());
        let trusted = probe(&at(&url), UA).await;
        std::env::remove_var("AVOCADO_REPO_CA");
        std::env::set_var("AVOCADO_REPO_INSECURE", "1");
        let insecure = probe(&at(&url), UA).await;
        std::env::remove_var("AVOCADO_REPO_INSECURE");
        assert_eq!(trusted, Some(None));
        assert_eq!(insecure, Some(None));
    }

    #[tokio::test]
    async fn probe_is_silent_when_the_feed_is_unreachable() {
        // Bind then drop: a port nothing listens on.
        let port = TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert_eq!(
            probe(&at(&format!("http://127.0.0.1:{port}")), UA).await,
            None
        );
    }

    /// A named feed that authenticates before it rate limits: without its
    /// credential the probe reads a 401 and the 429 goes unexplained.
    #[tokio::test]
    async fn probe_sends_a_named_feeds_credential() {
        use std::io::{Read, Write};
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                // "user:pass" in base64.
                let authed = String::from_utf8_lossy(&buf[..n])
                    .to_ascii_lowercase()
                    .contains("authorization: basic dxnlcjpwyxnz");
                let status = if authed {
                    "429 Too Many Requests"
                } else {
                    "401 Unauthorized"
                };
                let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n");
            }
        });
        assert_eq!(probe(&at(&url), UA).await, None);
        let feed = FeedEndpoint {
            credential: Some(("user".into(), "pass".into())),
            ..at(&url)
        };
        assert_eq!(probe(&feed, UA).await, Some(None));
    }

    /// A named feed under its own private CA (`repos.<name>.ca`) is verified
    /// with that CA, not only the global AVOCADO_REPO_CA.
    #[tokio::test]
    #[serial_test::serial]
    async fn probe_trusts_a_named_feeds_own_ca() {
        let tls = crate::utils::container_dev::tls::TlsMaterial::generate("feed", &[]).unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(tls.server_config());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((tcp, _)) = listener.accept().await {
                let Ok(mut s) = acceptor.accept(tcp).await else {
                    continue;
                };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\n\r\n")
                    .await;
                let _ = s.shutdown().await;
            }
        });
        let ca = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(ca.path(), tls.ca_cert_pem()).unwrap();
        std::env::remove_var("AVOCADO_REPO_CA");
        std::env::remove_var("AVOCADO_REPO_INSECURE");
        assert_eq!(
            probe(&at(&url), UA).await,
            None,
            "untrusted CA must not verify"
        );
        let feed = FeedEndpoint {
            ca: Some(ca.path().to_path_buf()),
            ..at(&url)
        };
        assert_eq!(probe(&feed, UA).await, Some(None));
    }
}
