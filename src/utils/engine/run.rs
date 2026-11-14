//! `docker run`, `create` and `exec`.

use super::{describe, pairs, parse, pull, write_frame, Parsed, USE_CLI_ENV};
use anyhow::{bail, Context, Result};
use bollard::container::LogOutput;
use bollard::errors::Error as ApiError;
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::models::{
    ContainerCreateBody, DeviceMapping, DeviceRequest, HostConfig, Mount, MountType, PortBinding,
    ResourcesUlimits, RestartPolicy,
};
use bollard::query_parameters as q;
use bollard::Docker;
use futures_util::{Stream, StreamExt};
use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::pin::Pin;
use std::time::Duration;
use tokio::io::{AsyncWrite, AsyncWriteExt};

const VALUED: &[&str] = &[
    "--name",
    "--platform",
    "--pull",
    "-v|--volume",
    "--mount",
    "--tmpfs",
    "--volumes-from",
    "-e|--env",
    "--env-file",
    "-w|--workdir",
    "-u|--user",
    "--group-add",
    "-h|--hostname",
    "--entrypoint",
    "-l|--label",
    "--stop-signal",
    "-p|--publish",
    "--net|--network",
    "--add-host",
    "--dns",
    "--device",
    "--device-cgroup-rule",
    "--gpus",
    "--runtime",
    "--cap-add",
    "--cap-drop",
    "--security-opt",
    "--pid",
    "--ipc",
    "--uts",
    "--userns",
    "--cgroupns",
    "--sysctl",
    "--ulimit",
    "--shm-size",
    "-m|--memory",
    "--cpus",
    "--cpuset-cpus",
    "--pids-limit",
    "--restart",
];

const BOOLS: &[&str] = &[
    "--rm",
    "-d|--detach",
    "-i|--interactive",
    "-t|--tty",
    "--privileged",
    "--init",
    "--read-only",
    "-P|--publish-all",
    "-q|--quiet",
];

type Output = Pin<Box<dyn Stream<Item = Result<LogOutput, ApiError>> + Send>>;
type Input = Pin<Box<dyn AsyncWrite + Send>>;

/// `docker run` (`start`) or `docker create`.
pub(super) async fn run(docker: &Docker, args: &[String], start: bool) -> Result<i32> {
    let a = parse(args, VALUED, BOOLS, false)?;
    let (image, cmd) = a.rest.split_first().context("docker run: no image given")?;
    let body = container(&a, image, cmd)?;
    let mut options = q::CreateContainerOptionsBuilder::new();
    if let Some(name) = a.get("--name") {
        options = options.name(name);
    }
    if let Some(platform) = a.get("--platform") {
        options = options.platform(platform);
    }
    let options = options.build();

    let policy = a.get("--pull").unwrap_or("missing");
    if policy == "always" {
        pull(docker, image, a.get("--platform")).await?;
    }
    let created = match docker
        .create_container(Some(options.clone()), body.clone())
        .await
    {
        Err(ApiError::DockerResponseServerError {
            status_code: 404,
            message,
        }) if policy == "missing" && message.contains("No such image") => {
            eprintln!("Unable to find image '{image}' locally");
            pull(docker, image, a.get("--platform")).await?;
            docker.create_container(Some(options), body).await?
        }
        other => other?,
    };
    let id = created.id;

    let tty = a.has("--tty");
    let stdin = a.has("--interactive");
    if !start || a.has("--detach") {
        if start {
            docker.start_container(&id, None).await?;
        }
        out!("{id}");
        return Ok(0);
    }

    // A `--rm` container is gone the moment it exits, so ask how it exited
    // before it can start. Otherwise the container is still there to be asked
    // afterwards.
    // ponytail: nothing confirms the daemon has registered this wait before
    // `start` goes out; it has the attach round trip to do so. Losing that
    // race reports "no such container" instead of the exit code. Close it by
    // not using the daemon's auto-remove if it is ever seen.
    let removed = a
        .has("--rm")
        .then(|| tokio::spawn(wait(docker.clone(), id.clone(), "removed")));
    let attach = q::AttachContainerOptionsBuilder::new()
        .stream(true)
        .stdout(true)
        .stderr(true)
        .stdin(stdin)
        .build();
    let attached = docker.attach_container(&id, Some(attach)).await?;
    if let Err(e) = docker.start_container(&id, None).await {
        // The docker CLI's codes for a command that could not be started.
        let code = match e.to_string().to_lowercase() {
            m if m.contains("executable file not found")
                || m.contains("no such file or directory") =>
            {
                127
            }
            m if m.contains("permission denied") || m.contains("not a directory") => 126,
            _ => 125,
        };
        eprintln!("{}", describe(&e.into()));
        return Ok(code);
    }

    let size = tty.then(|| tokio::spawn(follow_size(docker.clone(), Tty::Container(id.clone()))));
    tokio::select! {
        streamed = stream(attached.output, attached.input, stdin, tty) => streamed?,
        // Only without a terminal: with one, ^C is a byte the container's own
        // terminal turns into the signal.
        never = forward_signals(docker, &id), if !tty => match never {},
    }
    if let Some(size) = size {
        size.abort();
    }
    match removed {
        Some(removed) => removed.await?,
        None => wait(docker.clone(), id, "not-running").await,
    }
}

/// `docker exec`.
pub(super) async fn exec(docker: &Docker, args: &[String]) -> Result<i32> {
    let a = parse(
        args,
        &["-e|--env", "--env-file", "-w|--workdir", "-u|--user"],
        &[
            "-d|--detach",
            "-i|--interactive",
            "-t|--tty",
            "--privileged",
        ],
        false,
    )?;
    let (name, cmd) = a
        .rest
        .split_first()
        .context("docker exec: no container given")?;
    let (tty, stdin, detach) = (a.has("--tty"), a.has("--interactive"), a.has("--detach"));
    let created = docker
        .create_exec(
            name,
            CreateExecOptions {
                attach_stdin: Some(stdin),
                attach_stdout: Some(!detach),
                attach_stderr: Some(!detach),
                tty: Some(tty),
                env: Some(environment(&a)?),
                cmd: Some(cmd.to_vec()),
                privileged: Some(a.has("--privileged")),
                user: a.get("--user").map(String::from),
                working_dir: a.get("--workdir").map(String::from),
                detach_keys: None,
            },
        )
        .await?;
    let options = StartExecOptions {
        detach,
        tty,
        output_capacity: None,
    };
    match docker.start_exec(&created.id, Some(options)).await? {
        StartExecResults::Detached => Ok(0),
        StartExecResults::Attached { output, input } => {
            let size = tty
                .then(|| tokio::spawn(follow_size(docker.clone(), Tty::Exec(created.id.clone()))));
            stream(output, input, stdin, tty).await?;
            if let Some(size) = size {
                size.abort();
            }
            let code = docker.inspect_exec(&created.id).await?.exit_code;
            Ok(code.unwrap_or(0) as i32)
        }
    }
}

/// The container's exit code, once it reaches `condition`.
async fn wait(docker: Docker, id: String, condition: &str) -> Result<i32> {
    let options = q::WaitContainerOptionsBuilder::new()
        .condition(condition)
        .build();
    let mut waiting = docker.wait_container(&id, Some(options));
    match waiting.next().await {
        Some(Ok(exited)) => Ok(exited.status_code as i32),
        // bollard reports a non-zero exit as an error.
        Some(Err(ApiError::DockerContainerWaitError { code, .. })) => Ok(code as i32),
        Some(Err(e)) => Err(e.into()),
        None => bail!("the daemon closed the wait for container {id} without an exit status"),
    }
}

/// Send SIGINT and SIGTERM on to the container instead of dying of them, as
/// `docker run` does, so the container gets to shut down and this process
/// still reports how it exited.
async fn forward_signals(docker: &Docker, id: &str) -> std::convert::Infallible {
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
    loop {
        #[cfg(unix)]
        let signal = match &mut terminate {
            Ok(terminate) => tokio::select! {
                _ = tokio::signal::ctrl_c() => "SIGINT",
                _ = terminate.recv() => "SIGTERM",
            },
            Err(_) => tokio::signal::ctrl_c().await.map_or("", |()| "SIGINT"),
        };
        #[cfg(not(unix))]
        let signal = tokio::signal::ctrl_c().await.map_or("", |()| "SIGINT");
        if signal.is_empty() {
            // No handler could be installed; signals keep their default action.
            return std::future::pending().await;
        }
        let options = q::KillContainerOptionsBuilder::new().signal(signal).build();
        let _ = docker.kill_container(id, Some(options)).await;
    }
}

/// Copy an attached session's output to this process's stdout and stderr, and
/// this process's stdin to it, until the daemon closes the session.
// ponytail: bollard's decoder reads a terminal session's chunk as a stream
// frame header if it starts with byte 0, 1 or 2, which stalls the output.
// Binary data on a tty is the only way to get one; take over the upgraded
// connection ourselves if that ever matters.
async fn stream(mut output: Output, mut input: Input, stdin: bool, tty: bool) -> Result<()> {
    let _raw = (tty && std::io::stdin().is_terminal())
        .then(RawMode::enter)
        .flatten();
    if stdin {
        // tokio's stdin is a blocking thread too, but one the runtime then
        // waits on at shutdown, with a read nothing can interrupt.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 8192];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });
        tokio::spawn(async move {
            while let Some(chunk) = rx.recv().await {
                if input.write_all(&chunk).await.is_err() || input.flush().await.is_err() {
                    return;
                }
            }
            // End of our stdin is end of the container's: `tar xf -` and
            // friends wait for it.
            let _ = input.shutdown().await;
        });
    }
    while let Some(frame) = output.next().await {
        write_frame(frame?)?;
    }
    Ok(())
}

/// The local terminal in raw mode for as long as this lives, so every key
/// goes to the container's terminal untouched.
struct RawMode;

impl RawMode {
    fn enter() -> Option<Self> {
        crossterm::terminal::enable_raw_mode()
            .ok()
            .map(|()| RawMode)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

enum Tty {
    Container(String),
    Exec(String),
}

/// Keep the remote terminal the size of the local one.
// ponytail: polls rather than listening for SIGWINCH, so one path serves every
// platform; a resize lands up to a quarter second late.
async fn follow_size(docker: Docker, target: Tty) {
    let mut last = None;
    loop {
        let size = crossterm::terminal::size().ok();
        if let (true, Some((w, h))) = (size != last, size) {
            let (w, h) = (w.into(), h.into());
            let _ = match &target {
                Tty::Container(id) => {
                    let options = q::ResizeContainerTTYOptionsBuilder::new().w(w).h(h);
                    docker.resize_container_tty(id, options.build()).await
                }
                Tty::Exec(id) => {
                    let options = q::ResizeExecOptionsBuilder::new().w(w).h(h);
                    docker.resize_exec(id, options.build()).await
                }
            };
        }
        last = size;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// `-e`/`--env-file` as the `NAME=value` list the daemon takes. A bare `NAME`
/// passes this process's value through, or nothing when it has none.
fn environment(a: &Parsed) -> Result<Vec<String>> {
    let mut env = Vec::new();
    for file in a.all("--env-file") {
        let text = std::fs::read_to_string(file)
            .with_context(|| format!("--env-file: cannot read {file}"))?;
        env.extend(
            text.lines()
                .map(str::trim_start)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(String::from),
        );
    }
    env.extend(a.all("--env").map(String::from));
    Ok(env
        .into_iter()
        .filter_map(|var| match var.contains('=') {
            true => Some(var),
            false => std::env::var(&var)
                .ok()
                .map(|value| format!("{var}={value}")),
        })
        .collect())
}

/// `512m`, `2g`, `1024` (bytes) as a byte count.
fn bytes(size: &str) -> Result<i64> {
    let lower = size.to_ascii_lowercase();
    let lower = lower.strip_suffix('b').unwrap_or(&lower);
    let (number, shift) = match lower.char_indices().last() {
        Some((i, 'k')) => (&lower[..i], 10),
        Some((i, 'm')) => (&lower[..i], 20),
        Some((i, 'g')) => (&lower[..i], 30),
        Some((i, 't')) => (&lower[..i], 40),
        _ => (lower, 0),
    };
    let number: f64 = number
        .parse()
        .with_context(|| format!("invalid size '{size}'"))?;
    Ok((number * (1u64 << shift) as f64) as i64)
}

/// The create request for a `docker run` command line.
fn container(a: &Parsed, image: &str, cmd: &[String]) -> Result<ContainerCreateBody> {
    // docker joins the container to every network named, and refuses most
    // pairs. Keeping the last one would run what docker itself rejects.
    if a.all("--network").count() > 1 {
        bail!("more than one --network is unsupported (set {USE_CLI_ENV}=1 to use the docker CLI)");
    }
    let text = |flag: &str| a.get(flag).map(String::from);
    let list = |flag: &str| -> Option<Vec<String>> {
        let items: Vec<String> = a.all(flag).map(String::from).collect();
        (!items.is_empty()).then_some(items)
    };
    let set = |flag: &str| a.has(flag).then_some(true);
    let nonempty = |items: Vec<String>| (!items.is_empty()).then_some(items);

    let mut binds = Vec::new();
    let mut volumes = Vec::new();
    for volume in a.all("--volume") {
        match volume.split_once(':') {
            // The daemon takes absolute host paths only; resolving `./dir`
            // against the working directory is the client's job.
            Some((source, rest)) if source.starts_with('.') => {
                binds.push(format!("{}:{rest}", std::path::absolute(source)?.display()))
            }
            Some(_) => binds.push(volume.to_string()),
            None => volumes.push(volume.to_string()),
        }
    }

    let mut mounts = Vec::new();
    for mount in a.all("--mount") {
        let mut parsed = Mount {
            typ: Some(MountType::VOLUME),
            ..Default::default()
        };
        for field in mount.split(',') {
            let (key, value) = field.split_once('=').unwrap_or((field, "true"));
            match key {
                "type" => {
                    let typ = value.parse().ok();
                    parsed.typ = Some(typ.with_context(|| format!("--mount: bad type '{value}'"))?)
                }
                "source" | "src" => parsed.source = Some(value.to_string()),
                "target" | "dst" | "destination" => parsed.target = Some(value.to_string()),
                "readonly" | "ro" => parsed.read_only = Some(value != "false"),
                _ => bail!(
                    "unsupported --mount option '{key}' (set {USE_CLI_ENV}=1 to use the docker CLI)"
                ),
            }
        }
        mounts.push(parsed);
    }

    let mut exposed = Vec::new();
    let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
    for publish in a.all("--publish") {
        // [[ip:]host-port:]container-port[/protocol]
        let (ports, protocol) = publish.split_once('/').unwrap_or((publish, "tcp"));
        let mut parts = ports.rsplitn(3, ':');
        let port = format!("{}/{protocol}", parts.next().unwrap_or_default());
        let host_port = parts.next().map(String::from);
        let host_ip = parts
            .next()
            .map(|ip| ip.trim_matches(['[', ']']).to_string());
        exposed.push(port.clone());
        port_bindings
            .entry(port)
            .or_default()
            .get_or_insert_with(Vec::new)
            .push(PortBinding { host_ip, host_port });
    }

    let devices: Vec<DeviceMapping> = a
        .all("--device")
        .map(|device| {
            // host[:container][:permissions]
            let mut parts = device.split(':');
            let host = parts.next().unwrap_or_default();
            let (inside, permissions) = match (parts.next(), parts.next()) {
                (Some(p), None) if p.chars().all(|c| "rwm".contains(c)) => (host, p),
                (inside, permissions) => (inside.unwrap_or(host), permissions.unwrap_or("rwm")),
            };
            DeviceMapping {
                path_on_host: Some(host.to_string()),
                path_in_container: Some(inside.to_string()),
                cgroup_permissions: Some(permissions.to_string()),
            }
        })
        .collect();

    let device_requests = match a.get("--gpus") {
        None => None,
        Some(gpus) => {
            let mut request = DeviceRequest {
                capabilities: Some(vec![vec!["gpu".to_string()]]),
                ..Default::default()
            };
            if gpus == "all" {
                request.count = Some(-1);
            } else if let Some(ids) = gpus.strip_prefix("device=") {
                request.device_ids = Some(ids.split(',').map(String::from).collect());
            } else {
                let count = gpus.parse().ok();
                request.count = Some(count.with_context(|| {
                    format!("unsupported --gpus value '{gpus}' (set {USE_CLI_ENV}=1 to use the docker CLI)")
                })?);
            }
            Some(vec![request])
        }
    };

    let mut security_opt = Vec::new();
    for opt in a.all("--security-opt") {
        match opt.strip_prefix("seccomp=") {
            // The daemon takes the profile itself; reading the file is the
            // client's job.
            Some(path) if path != "unconfined" && path != "builtin" => {
                let profile = std::fs::read(path)
                    .with_context(|| format!("--security-opt: cannot read {path}"))?;
                let profile: serde_json::Value = serde_json::from_slice(&profile)
                    .with_context(|| format!("--security-opt: {path} is not JSON"))?;
                security_opt.push(format!("seccomp={profile}"));
            }
            _ => security_opt.push(opt.to_string()),
        }
    }

    let mut ulimits = Vec::new();
    for ulimit in a.all("--ulimit") {
        // name=soft[:hard]
        let limit = |value: &str| match value {
            "unlimited" => Ok(-1),
            value => value
                .parse::<i64>()
                .with_context(|| format!("--ulimit: bad limit in '{ulimit}'")),
        };
        let (name, limits) = ulimit
            .split_once('=')
            .with_context(|| format!("--ulimit: expected name=soft[:hard], got '{ulimit}'"))?;
        let (soft, hard) = limits.split_once(':').unwrap_or((limits, limits));
        ulimits.push(ResourcesUlimits {
            name: Some(name.to_string()),
            soft: Some(limit(soft)?),
            hard: Some(limit(hard)?),
        });
    }

    let restart_policy = match a.get("--restart") {
        None => None,
        Some(restart) => {
            // name[:max-retries]
            let (name, retries) = restart.split_once(':').unwrap_or((restart, ""));
            let name = name.parse().ok();
            Some(RestartPolicy {
                name: Some(name.with_context(|| format!("--restart: bad policy '{restart}'"))?),
                maximum_retry_count: retries.parse().ok(),
            })
        }
    };
    let cgroupns_mode = match a.get("--cgroupns") {
        None => None,
        Some(mode) => {
            let parsed = mode.parse().ok();
            Some(parsed.with_context(|| format!("--cgroupns: bad mode '{mode}'"))?)
        }
    };
    let tmpfs: HashMap<String, String> = a
        .all("--tmpfs")
        .map(|mount| {
            let (path, options) = mount.split_once(':').unwrap_or((mount, ""));
            (path.to_string(), options.to_string())
        })
        .collect();
    let cpus = match a.get("--cpus") {
        None => None,
        Some(cpus) => {
            let cpus: f64 = cpus
                .parse()
                .with_context(|| format!("--cpus: bad value '{cpus}'"))?;
            Some((cpus * 1e9) as i64)
        }
    };
    let pids_limit = match a.get("--pids-limit") {
        None => None,
        Some(limit) => Some(
            limit
                .parse()
                .with_context(|| format!("--pids-limit: bad value '{limit}'"))?,
        ),
    };

    let stdin = a.has("--interactive");
    let detach = a.has("--detach");
    Ok(ContainerCreateBody {
        image: Some(image.to_string()),
        cmd: nonempty(cmd.to_vec()),
        entrypoint: a.get("--entrypoint").map(|e| vec![e.to_string()]),
        hostname: text("--hostname"),
        user: text("--user"),
        working_dir: text("--workdir"),
        env: nonempty(environment(a)?),
        labels: Some(pairs(a.all("--label"))),
        stop_signal: text("--stop-signal"),
        tty: Some(a.has("--tty")),
        open_stdin: Some(stdin),
        stdin_once: Some(stdin),
        attach_stdin: Some(stdin),
        attach_stdout: Some(!detach),
        attach_stderr: Some(!detach),
        exposed_ports: nonempty(exposed),
        volumes: nonempty(volumes),
        host_config: Some(HostConfig {
            // The size the terminal starts at; `follow_size` only tracks it
            // from there, and its first resize can land after the command
            // has already asked.
            console_size: a
                .has("--tty")
                .then(|| crossterm::terminal::size().ok())
                .flatten()
                .map(|(w, h)| vec![h.into(), w.into()]),
            binds: nonempty(binds),
            mounts: (!mounts.is_empty()).then_some(mounts),
            tmpfs: (!tmpfs.is_empty()).then_some(tmpfs),
            volumes_from: list("--volumes-from"),
            network_mode: text("--network"),
            port_bindings: (!port_bindings.is_empty()).then_some(port_bindings),
            publish_all_ports: set("--publish-all"),
            extra_hosts: list("--add-host"),
            dns: list("--dns"),
            devices: (!devices.is_empty()).then_some(devices),
            device_cgroup_rules: list("--device-cgroup-rule"),
            device_requests,
            runtime: text("--runtime"),
            privileged: set("--privileged"),
            cap_add: list("--cap-add"),
            cap_drop: list("--cap-drop"),
            security_opt: nonempty(security_opt),
            group_add: list("--group-add"),
            pid_mode: text("--pid"),
            ipc_mode: text("--ipc"),
            uts_mode: text("--uts"),
            userns_mode: text("--userns"),
            cgroupns_mode,
            sysctls: a.get("--sysctl").map(|_| pairs(a.all("--sysctl"))),
            ulimits: (!ulimits.is_empty()).then_some(ulimits),
            shm_size: a.get("--shm-size").map(bytes).transpose()?,
            memory: a.get("--memory").map(bytes).transpose()?,
            nano_cpus: cpus,
            cpuset_cpus: text("--cpuset-cpus"),
            pids_limit,
            restart_policy,
            init: set("--init"),
            readonly_rootfs: set("--read-only"),
            auto_remove: set("--rm"),
            ..Default::default()
        }),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(args: &[&str]) -> serde_json::Value {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let a = parse(&args, VALUED, BOOLS, false).unwrap();
        let (image, cmd) = a.rest.split_first().unwrap();
        serde_json::to_value(container(&a, image, cmd).unwrap()).unwrap()
    }

    /// The command line `SdkContainer` builds, plus the flags users put in
    /// `container_args`, as the request the daemon gets.
    #[test]
    fn run_flags_become_the_create_request() {
        std::env::set_var("AVOCADO_ENGINE_TEST_INHERITED", "from-env");
        let b = body(&[
            "--rm",
            "--interactive",
            "--name=sdk",
            "-v",
            "vol:/opt/_avocado:rw",
            "-v",
            "/src:/opt/src:z",
            "-v",
            "/anonymous",
            "--mount",
            "type=bind,src=/a,dst=/b,readonly",
            "-e",
            "A=1",
            "-e",
            "AVOCADO_ENGINE_TEST_INHERITED",
            "-e",
            "AVOCADO_ENGINE_TEST_UNSET",
            "--network=host",
            "--privileged",
            "--cap-add",
            "SYS_ADMIN",
            "--security-opt",
            "label=disable",
            "--device",
            "/dev/kvm",
            "--device",
            "/dev/ttyUSB0:/dev/ttyS9:rw",
            "-p",
            "127.0.0.1:8080:80",
            "-p",
            "2049:2049/udp",
            "--add-host",
            "host.docker.internal:host-gateway",
            "--ulimit",
            "nofile=1024:2048",
            "--shm-size",
            "1g",
            "--gpus",
            "all",
            "--restart",
            "on-failure:3",
            "--entrypoint",
            "/bin/sh",
            "image:tag",
            "-c",
            "echo --rm is the command's",
        ]);
        let host = &b["HostConfig"];
        assert_eq!(b["Image"], "image:tag");
        assert_eq!(
            b["Cmd"],
            serde_json::json!(["-c", "echo --rm is the command's"])
        );
        assert_eq!(b["Entrypoint"], serde_json::json!(["/bin/sh"]));
        assert_eq!(
            b["Env"],
            serde_json::json!(["A=1", "AVOCADO_ENGINE_TEST_INHERITED=from-env"])
        );
        assert_eq!(b["OpenStdin"], true);
        assert_eq!(b["Tty"], false);
        assert_eq!(b["Volumes"], serde_json::json!({"/anonymous": {}}));
        assert_eq!(
            host["Binds"],
            serde_json::json!(["vol:/opt/_avocado:rw", "/src:/opt/src:z"])
        );
        assert_eq!(
            host["Mounts"][0],
            serde_json::json!({"Type": "bind", "Source": "/a", "Target": "/b", "ReadOnly": true})
        );
        assert_eq!(host["AutoRemove"], true);
        assert_eq!(host["NetworkMode"], "host");
        assert_eq!(host["Privileged"], true);
        assert_eq!(host["CapAdd"], serde_json::json!(["SYS_ADMIN"]));
        assert_eq!(host["SecurityOpt"], serde_json::json!(["label=disable"]));
        assert_eq!(
            host["Devices"],
            serde_json::json!([
                {"PathOnHost": "/dev/kvm", "PathInContainer": "/dev/kvm", "CgroupPermissions": "rwm"},
                {"PathOnHost": "/dev/ttyUSB0", "PathInContainer": "/dev/ttyS9", "CgroupPermissions": "rw"},
            ])
        );
        assert_eq!(
            host["PortBindings"],
            serde_json::json!({
                "80/tcp": [{"HostIp": "127.0.0.1", "HostPort": "8080"}],
                "2049/udp": [{"HostPort": "2049"}],
            })
        );
        assert_eq!(
            host["ExtraHosts"],
            serde_json::json!(["host.docker.internal:host-gateway"])
        );
        assert_eq!(
            host["Ulimits"],
            serde_json::json!([{"Name": "nofile", "Soft": 1024, "Hard": 2048}])
        );
        assert_eq!(host["ShmSize"], 1 << 30);
        assert_eq!(host["DeviceRequests"][0]["Count"], -1);
        assert_eq!(
            host["RestartPolicy"],
            serde_json::json!({"Name": "on-failure", "MaximumRetryCount": 3})
        );
    }

    #[test]
    fn a_second_network_is_refused_rather_than_winning() {
        let args: Vec<String> = ["--network=bridge", "--net", "host", "image"]
            .map(String::from)
            .to_vec();
        let a = parse(&args, VALUED, BOOLS, false).unwrap();
        assert!(container(&a, "image", &[]).is_err());
    }

    #[test]
    fn relative_bind_sources_are_made_absolute() {
        let b = body(&["-v", "./out:/out:ro", "image"]);
        let bind = b["HostConfig"]["Binds"][0].as_str().unwrap();
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(bind, format!("{}:/out:ro", cwd.join("out").display()));
    }

    #[test]
    fn sizes_take_docker_units() {
        for (size, want) in [
            ("1024", 1024),
            ("64k", 64 << 10),
            ("512m", 512 << 20),
            ("2GB", 2 << 30),
        ] {
            assert_eq!(bytes(size).unwrap(), want, "{size}");
        }
        assert!(bytes("lots").is_err());
    }
}
