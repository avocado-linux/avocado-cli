//! Engine-driver trait for the Container Dev Mode watcher (design D4).
//!
//! The host watches its container engine for image *tag* events and, on a
//! rebuild, re-tags and syncs the changed layers to the device. This module
//! defines the engine abstraction those tasks build on: a driver per engine
//! (docker + podman) that
//!
//! 1. streams tag events over the engine **CLI subprocess** (`docker events` /
//!    `podman events --format json`), NEVER the API socket — so a rootless
//!    podman with no `podman.socket` still works (design D4, assumption A4);
//! 2. parses one engine-specific JSON event line into a structured
//!    [`TagEvent`]; and
//! 3. describes the per-engine write-credential injection used on push (docker:
//!    an ephemeral `DOCKER_CONFIG`; podman: `--creds`), because A10 couples
//!    credential injection to the engine (design M-3).
//!
//! Podman *conformance* is a droppable Phase 0 gate outcome (design D4): the
//! trait ships with both drivers regardless; the podman driver is a real
//! CLI-event path, not a stub. The subprocess plumbing ([`watch_tag_events`])
//! is engine-agnostic — it drives whichever driver it is handed through
//! [`EngineDriver::events_argv`] and [`EngineDriver::parse_tag_event`], so the
//! push wiring and watcher orchestration (tasks 4.2/4.3) reuse it unchanged.

use std::process::Stdio;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::auth::{WriteToken, WRITE_USERNAME};

/// A parsed image *tag* event from the engine's CLI event stream.
///
/// This is the engine-agnostic shape both drivers normalize their
/// (structurally different) JSON events into: docker carries the name under
/// `Actor.Attributes.name`, podman under `Name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagEvent {
    /// The image reference that was (re)tagged, e.g. `my-app:dev` (docker) or
    /// `localhost/my-app:dev` (podman qualifies the registry). Reported
    /// verbatim as the engine emitted it; ref normalization/matching against a
    /// configured `ref` is the watcher's concern (task 4.2), not the parser's.
    pub image: String,
    /// The image content id (digest) the event carried, when present.
    pub image_id: Option<String>,
}

/// How an engine receives a per-invocation, non-persisted write credential on
/// push (design D2/A10, M-3).
///
/// This is the per-engine credential-injection *shape*. The actual mechanics —
/// writing the ephemeral `DOCKER_CONFIG` dir 0600 under the per-project
/// directory and deleting it after the push (docker), or threading `--creds`
/// into the push argv (podman) — land with the push wiring in task 4.2. Neither
/// path ever runs `docker login` against the user's real `~/.docker/config.json`
/// (design M-E).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteCredential {
    /// docker: point `DOCKER_CONFIG` at an ephemeral dir whose `auths` entry is
    /// keyed byte-identically to `registry` (the tagged `host:port`, H-3), so
    /// the docker CLI resolves it locally and forwards it as `X-Registry-Auth`
    /// — surviving `DOCKER_HOST`/VM routing. A key that does not byte-match the
    /// tag host makes docker attach no auth and the push 401s with no prompt.
    DockerConfigEnv {
        /// The registry `host:port`, byte-identical to the image tag host (H-3).
        registry: String,
        /// Fixed Basic username paired with the write token.
        username: String,
        /// The host-only write token (Basic password).
        token: String,
    },
    /// podman: pass `--creds <username>:<token>` per push invocation.
    PodmanCreds {
        /// Fixed Basic username paired with the write token.
        username: String,
        /// The host-only write token.
        token: String,
    },
}

/// An engine driver: everything engine-specific about watching for tag events
/// and injecting a push credential.
///
/// Implementors MUST drive events through the engine CLI subprocess only; a
/// driver that reaches for the API socket violates the design (D4) and the
/// falsifier for task 4.1.
pub trait EngineDriver: Send + Sync {
    /// The engine CLI binary name (`docker` / `podman`).
    fn binary(&self) -> &'static str;

    /// The argv (after the binary) that streams image tag events as
    /// newline-delimited JSON over the engine CLI subprocess.
    ///
    /// This is `<binary> events …` in every case — never a socket dial — which
    /// is precisely what lets a rootless podman with no `podman.socket` work
    /// (A4). The stream is filtered to image tag events so the watcher does not
    /// have to discard unrelated container/network/volume traffic.
    fn events_argv(&self) -> Vec<String>;

    /// Parse a single JSON event line emitted by [`Self::events_argv`] into a
    /// [`TagEvent`], or `None` when the line is not an image tag event
    /// (a different event type/action, or an unparseable line).
    fn parse_tag_event(&self, line: &str) -> Option<TagEvent>;

    /// The per-engine write-credential injection shape for a push to
    /// `registry` (design D2/A10/M-3). The value describes HOW the credential
    /// is delivered; task 4.2 realizes it on the push subprocess.
    fn write_credential(&self, registry: &str, token: &WriteToken) -> WriteCredential;
}

/// The docker engine driver.
///
/// Events: `docker events --filter type=image --filter event=tag --format
/// {{json .}}`. docker's event JSON capitalizes `Type`/`Action` and nests the
/// image name under `Actor.Attributes.name`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DockerDriver;

/// docker's event JSON shape (the fields we read from `{{json .}}`).
#[derive(Debug, Deserialize)]
struct DockerEvent {
    #[serde(rename = "Type")]
    typ: Option<String>,
    #[serde(rename = "Action")]
    action: Option<String>,
    #[serde(rename = "Actor")]
    actor: Option<DockerActor>,
    /// Deprecated top-level id, retained by docker for compatibility; used as a
    /// fallback for the image digest when `Actor.ID` is absent.
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DockerActor {
    #[serde(rename = "ID")]
    id: Option<String>,
    #[serde(rename = "Attributes")]
    attributes: Option<std::collections::HashMap<String, String>>,
}

impl EngineDriver for DockerDriver {
    fn binary(&self) -> &'static str {
        "docker"
    }

    fn events_argv(&self) -> Vec<String> {
        [
            "events",
            "--filter",
            "type=image",
            "--filter",
            "event=tag",
            "--format",
            "{{json .}}",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn parse_tag_event(&self, line: &str) -> Option<TagEvent> {
        let event: DockerEvent = serde_json::from_str(line.trim()).ok()?;
        // Only an image `tag` action is a tag event.
        if event.typ.as_deref() != Some("image") || event.action.as_deref() != Some("tag") {
            return None;
        }
        let actor = event.actor.as_ref();
        let image = actor
            .and_then(|a| a.attributes.as_ref())
            .and_then(|attrs| attrs.get("name"))
            .cloned()?;
        let image_id = actor
            .and_then(|a| a.id.clone())
            .or(event.id)
            .filter(|s| !s.is_empty());
        Some(TagEvent { image, image_id })
    }

    fn write_credential(&self, registry: &str, token: &WriteToken) -> WriteCredential {
        WriteCredential::DockerConfigEnv {
            registry: registry.to_string(),
            username: WRITE_USERNAME.to_string(),
            token: token.secret().to_string(),
        }
    }
}

/// The podman engine driver.
///
/// Events: `podman events --filter type=image --filter event=tag --format
/// json`. podman's event JSON uses `Status` for the action and carries the
/// image name in `Name`. Rootless podman emits these over its `events_backend`
/// (journald or file) with NO API socket (A4).
#[derive(Debug, Clone, Copy, Default)]
pub struct PodmanDriver;

/// podman's event JSON shape (the fields we read from `--format json`).
#[derive(Debug, Deserialize)]
struct PodmanEvent {
    #[serde(rename = "Type")]
    typ: Option<String>,
    #[serde(rename = "Status")]
    status: Option<String>,
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "Image")]
    image: Option<String>,
    #[serde(rename = "ID")]
    id: Option<String>,
}

impl EngineDriver for PodmanDriver {
    fn binary(&self) -> &'static str {
        "podman"
    }

    fn events_argv(&self) -> Vec<String> {
        [
            "events",
            "--filter",
            "type=image",
            "--filter",
            "event=tag",
            "--format",
            "json",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn parse_tag_event(&self, line: &str) -> Option<TagEvent> {
        let event: PodmanEvent = serde_json::from_str(line.trim()).ok()?;
        if event.typ.as_deref() != Some("image") || event.status.as_deref() != Some("tag") {
            return None;
        }
        // podman reports the tagged reference under `Name`; fall back to `Image`.
        let image = event
            .name
            .filter(|s| !s.is_empty())
            .or(event.image)
            .filter(|s| !s.is_empty())?;
        let image_id = event.id.filter(|s| !s.is_empty());
        Some(TagEvent { image, image_id })
    }

    fn write_credential(&self, _registry: &str, token: &WriteToken) -> WriteCredential {
        WriteCredential::PodmanCreds {
            username: WRITE_USERNAME.to_string(),
            token: token.secret().to_string(),
        }
    }
}

/// Resolve an engine driver by CLI tool name (`docker` / `podman`).
///
/// Returns `None` for an unknown tool. Both drivers are real CLI-event paths;
/// podman is not a stub (design D4).
pub fn driver_for(tool: &str) -> Option<Box<dyn EngineDriver>> {
    match tool {
        "docker" => Some(Box::new(DockerDriver)),
        "podman" => Some(Box::new(PodmanDriver)),
        _ => None,
    }
}

/// Read newline-delimited JSON events from `reader`, parse each through
/// `driver.parse_tag_event`, and hand every recognized [`TagEvent`] to `sink`.
///
/// Non-tag and unparseable lines are skipped, so a driver that emits unfiltered
/// events (or a stray log line) never breaks the stream. This is the
/// engine-agnostic core of the CLI-subprocess event loop: [`watch_tag_events`]
/// pipes a live subprocess stdout in here, and tests drive it with captured
/// fixtures — the event source is the CLI byte stream either way, never an API
/// socket.
pub async fn forward_tag_events<R, F>(
    driver: &dyn EngineDriver,
    reader: R,
    mut sink: F,
) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin,
    F: FnMut(TagEvent),
{
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(event) = driver.parse_tag_event(&line) {
            sink(event);
        }
    }
    Ok(())
}

/// Spawn `<binary> events …` as a subprocess and stream parsed [`TagEvent`]s
/// over the returned channel.
///
/// The events come ONLY from the engine CLI subprocess (design D4) — no API
/// socket is opened — so a rootless podman without `podman.socket` works. The
/// caller owns the returned [`tokio::process::Child`] and kills it to stop
/// watching (e.g. on `down`); dropping the receiver ends the forwarding task.
pub async fn watch_tag_events(
    driver: Box<dyn EngineDriver>,
) -> Result<(mpsc::Receiver<TagEvent>, tokio::process::Child)> {
    let argv = driver.events_argv();
    let mut child = Command::new(driver.binary())
        .args(&argv)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn `{} events`", driver.binary()))?;

    let stdout = child
        .stdout
        .take()
        .context("engine events subprocess produced no stdout handle")?;

    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let _ = forward_tag_events(driver.as_ref(), reader, |event| {
            // A closed receiver means the watcher stopped; blocking_send is not
            // available in async, so use try_send and drop on a full/closed
            // channel — the watcher (task 4.2) debounces, so a dropped burst
            // event is coalesced by the next one.
            let _ = tx.try_send(event);
        })
        .await;
    });

    Ok((rx, child))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // ---- docker fixtures (captured `docker events --format '{{json .}}'`) ----

    const DOCKER_TAG_EVENT: &str = r#"{"status":"tag","id":"sha256:1111aaaa","Type":"image","Action":"tag","Actor":{"ID":"sha256:1111aaaa","Attributes":{"name":"my-app:dev"}},"scope":"local","time":1718030000,"timeNano":1718030000000000000}"#;

    const DOCKER_CONTAINER_START: &str = r#"{"status":"start","id":"c0ffee","Type":"container","Action":"start","Actor":{"ID":"c0ffee","Attributes":{"image":"my-app:dev","name":"web"}},"scope":"local","time":1718030001}"#;

    // ---- podman fixtures (captured `podman events --format json`) ----

    const PODMAN_TAG_EVENT: &str = r#"{"ID":"2222bbbbcccc","Image":"localhost/my-app:dev","Name":"localhost/my-app:dev","Status":"tag","Time":"2024-06-10T12:00:00.000000000-06:00","Type":"image","Attributes":null}"#;

    const PODMAN_CONTAINER_START: &str = r#"{"ID":"deadbeef","Image":"localhost/my-app:dev","Name":"web","Status":"start","Time":"2024-06-10T12:00:01.000000000-06:00","Type":"container","Attributes":null}"#;

    #[test]
    fn docker_driver_parses_a_tag_event_from_the_cli_json_line() {
        let event = DockerDriver
            .parse_tag_event(DOCKER_TAG_EVENT)
            .expect("a docker image tag event parses");
        assert_eq!(event.image, "my-app:dev");
        assert_eq!(event.image_id.as_deref(), Some("sha256:1111aaaa"));
    }

    #[test]
    fn podman_driver_parses_a_tag_event_from_the_cli_json_line() {
        let event = PodmanDriver
            .parse_tag_event(PODMAN_TAG_EVENT)
            .expect("a podman image tag event parses");
        // podman qualifies the ref with the registry; the parser reports it
        // verbatim (matching/normalization is the watcher's job).
        assert_eq!(event.image, "localhost/my-app:dev");
        assert_eq!(event.image_id.as_deref(), Some("2222bbbbcccc"));
    }

    #[test]
    fn docker_driver_ignores_a_non_tag_event() {
        assert!(
            DockerDriver
                .parse_tag_event(DOCKER_CONTAINER_START)
                .is_none(),
            "a container start is not an image tag event"
        );
    }

    #[test]
    fn podman_driver_ignores_a_non_tag_event() {
        assert!(
            PodmanDriver
                .parse_tag_event(PODMAN_CONTAINER_START)
                .is_none(),
            "a container start is not an image tag event"
        );
    }

    #[test]
    fn a_driver_returns_none_on_an_unparseable_line() {
        assert!(DockerDriver.parse_tag_event("not json").is_none());
        assert!(PodmanDriver.parse_tag_event("").is_none());
    }

    #[test]
    fn docker_drives_events_over_the_cli_not_the_api_socket() {
        let driver = DockerDriver;
        assert_eq!(driver.binary(), "docker");
        let argv = driver.events_argv();
        // The event source is the `docker events` CLI subcommand — not a socket.
        assert_eq!(argv.first().map(String::as_str), Some("events"));
        assert!(
            !argv.iter().any(|a| a.contains("--host")
                || a.contains("-H")
                || a.contains(".sock")
                || a.contains("unix://")),
            "the driver must not dial the API socket: {argv:?}"
        );
    }

    #[test]
    fn podman_drives_events_over_the_cli_with_json_and_no_socket() {
        let driver = PodmanDriver;
        assert_eq!(driver.binary(), "podman");
        let argv = driver.events_argv();
        assert_eq!(argv.first().map(String::as_str), Some("events"));
        // The task pins `podman events --format json`.
        let format_idx = argv
            .iter()
            .position(|a| a == "--format")
            .expect("podman events must request an explicit format");
        assert_eq!(argv.get(format_idx + 1).map(String::as_str), Some("json"));
        assert!(
            !argv.iter().any(|a| a.contains("--url")
                || a.contains(".sock")
                || a.contains("unix://")
                || a.contains("--remote")),
            "rootless podman must be driven with no API socket: {argv:?}"
        );
    }

    #[test]
    fn both_docker_and_podman_drivers_resolve_and_podman_is_not_a_stub() {
        let docker = driver_for("docker").expect("docker driver exists");
        assert_eq!(docker.binary(), "docker");

        let podman = driver_for("podman").expect("podman driver exists");
        assert_eq!(podman.binary(), "podman");
        // podman is a real CLI-event path, not a bare stub: it both drives
        // `events` and parses a real tag event.
        assert_eq!(
            podman.events_argv().first().map(String::as_str),
            Some("events")
        );
        assert!(
            podman.parse_tag_event(PODMAN_TAG_EVENT).is_some(),
            "the podman driver must parse a real CLI tag event, not stub out"
        );

        assert!(driver_for("nerdctl").is_none());
    }

    #[tokio::test]
    async fn forward_tag_events_streams_only_tag_events_from_the_cli_byte_stream() {
        // A captured multi-line event stream, as it would arrive on the engine
        // subprocess stdout: two tag events interleaved with noise the driver
        // must skip.
        let stream = format!(
            "{DOCKER_TAG_EVENT}\n\
             {DOCKER_CONTAINER_START}\n\
             garbage-not-json\n\
             {}\n",
            DOCKER_TAG_EVENT.replace("my-app:dev", "sidecar:latest")
        );
        let reader = BufReader::new(Cursor::new(stream.into_bytes()));

        let mut collected: Vec<TagEvent> = Vec::new();
        forward_tag_events(&DockerDriver, reader, |event| collected.push(event))
            .await
            .expect("forwarding over a byte-stream reader succeeds");

        // Only the two image tag lines surface, in order; the container start
        // and the garbage line are dropped.
        assert_eq!(collected.len(), 2, "only tag events are forwarded");
        assert_eq!(collected[0].image, "my-app:dev");
        assert_eq!(collected[1].image, "sidecar:latest");
    }

    #[test]
    fn docker_write_credential_is_an_ephemeral_docker_config_keyed_to_the_registry() {
        let cred = DockerDriver.write_credential("127.0.0.1:5599", &WriteToken::new("wtok"));
        match cred {
            WriteCredential::DockerConfigEnv {
                registry,
                username,
                token,
            } => {
                // The auth-entry key must be byte-identical to the tagged
                // registry host:port (H-3).
                assert_eq!(registry, "127.0.0.1:5599");
                assert_eq!(username, WRITE_USERNAME);
                assert_eq!(token, "wtok");
            }
            other => panic!("docker must inject via an ephemeral DOCKER_CONFIG, got {other:?}"),
        }
    }

    #[test]
    fn podman_write_credential_is_per_invocation_creds() {
        let cred = PodmanDriver.write_credential("127.0.0.1:5599", &WriteToken::new("wtok"));
        match cred {
            WriteCredential::PodmanCreds { username, token } => {
                assert_eq!(username, WRITE_USERNAME);
                assert_eq!(token, "wtok");
            }
            other => panic!("podman must inject via --creds, got {other:?}"),
        }
    }
}
