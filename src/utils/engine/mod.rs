//! Built-in stand-in for the `docker` CLI, speaking the Docker Engine API.
//!
//! Every container operation in this crate is spelled as a `docker …` argv and
//! run as a child process. Rather than require a `docker` binary on the host to
//! run those, [`command`] re-invokes this executable as `avocado __docker …`,
//! and [`main`] answers the slice of the docker CLI the crate actually uses
//! over the Engine API. A host with an avocado-vm (or any reachable daemon)
//! then needs no docker client installed at all.
//!
//! It is a re-exec rather than direct API calls from each call site so that
//! those sites keep ordinary child-process semantics: inherited or piped stdio,
//! exit codes, process groups, kill.
//!
//! Anything that is not literally `docker` (podman, an explicit path, a test's
//! fake tool script) is still spawned as given, and `AVOCADO_DOCKER_CLI=1`
//! sends `docker` back to the real CLI too.
// ponytail: one process spawn and one daemon connection per container
// operation, the same as the docker CLI this replaces. Call the API from the
// main process if that ever shows up in a profile.

use anyhow::{bail, Context, Result};
use bollard::auth::DockerCredentials;
use bollard::container::LogOutput;
use bollard::errors::Error as ApiError;
use bollard::query_parameters as q;
use bollard::Docker;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// `println!` that ends the process quietly when the reader has gone away
/// (`… | head`), as the docker CLI does, where `println!` would panic.
macro_rules! out {
    ($($arg:tt)*) => {
        if writeln!(std::io::stdout(), $($arg)*).is_err() {
            std::process::exit(0)
        }
    };
}

mod cp;
mod run;

/// First argument of the re-exec; never a real subcommand.
pub const SHIM_ARG: &str = "__docker";

/// Set to route `docker` to the host's docker CLI instead of the built-in
/// Engine API client.
pub const USE_CLI_ENV: &str = "AVOCADO_DOCKER_CLI";

/// The docker CLI has no request timeout: `stop`, `wait` and a pull all block
/// for as long as the daemon takes. bollard defaults to two minutes.
const NO_TIMEOUT: Duration = Duration::from_secs(60 * 60 * 24 * 365);

fn use_builtin(tool: &str) -> bool {
    tool == "docker"
        // A unit-test binary has no `__docker` entry point to re-invoke.
        && !cfg!(test)
        // ponytail: Windows keeps the docker CLI until this has been run
        // there: console input and half-closing a named pipe are unproven.
        && !cfg!(windows)
        && std::env::var_os(USE_CLI_ENV).is_none()
        // bollard is built without its TLS and ssh transports.
        && std::env::var_os("DOCKER_TLS_VERIFY").is_none()
        && reachable(
            std::env::var("DOCKER_HOST").ok().as_deref(),
            context_host,
        )
}

/// Whether the daemon is one this can talk to: bollard is built without its
/// TLS and ssh transports. The context is read only without `DOCKER_HOST`.
fn reachable(docker_host: Option<&str>, context: impl FnOnce() -> Option<String>) -> bool {
    match docker_host {
        Some(host) => !(host.starts_with("ssh://") || host.starts_with("https://")),
        // A context's TLS settings live beside its endpoint, so only trust
        // the local sockets it can name.
        None => {
            context().is_none_or(|host| host.starts_with("unix://") || host.starts_with("npipe://"))
        }
    }
}

/// The program to spawn for `tool`, and the argument that must lead its argv.
fn program(tool: &str) -> (std::ffi::OsString, Option<&'static str>) {
    if use_builtin(tool) {
        // On Linux this link is still the running binary after its file has
        // been replaced, by an upgrade or a rebuild, part-way through a run.
        let own = std::path::Path::new("/proc/self/exe");
        let exe = match own.exists() {
            true => Some(own.into()),
            false => std::env::current_exe().ok(),
        };
        if let Some(exe) = exe {
            return (exe.into_os_string(), Some(SHIM_ARG));
        }
    }
    (tool.into(), None)
}

/// `tokio::process::Command` for a container tool. Use in place of
/// `Command::new(tool)` wherever the argv that follows is a docker command.
pub fn command(tool: impl AsRef<str>) -> tokio::process::Command {
    let (program, lead) = program(tool.as_ref());
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(lead);
    cmd
}

/// Blocking counterpart of [`command`].
pub fn std_command(tool: impl AsRef<str>) -> std::process::Command {
    let (program, lead) = program(tool.as_ref());
    let mut cmd = std::process::Command::new(program);
    cmd.args(lead);
    cmd
}

/// Entry point for `avocado __docker <args>`. Returns the process exit code.
pub async fn main(args: Vec<String>) -> i32 {
    // `docker container <verb>` is the long spelling of `docker <verb>`.
    let args = match args.split_first() {
        Some((first, rest)) if first == "container" => rest,
        _ => &args[..],
    };
    let result = match connect().await {
        Ok(docker) => dispatch(&docker, args).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{}", describe(&e));
            // What the docker CLI exits with when `run` fails before the
            // container's own command does.
            match args.first().map(String::as_str) {
                Some("run" | "create") => 125,
                _ => 1,
            }
        }
    }
}

async fn dispatch(docker: &Docker, args: &[String]) -> Result<i32> {
    let (sub, rest) = args.split_first().context("no docker command given")?;
    match sub.as_str() {
        "run" => run::run(docker, rest, true).await,
        "create" => run::run(docker, rest, false).await,
        "exec" => run::exec(docker, rest).await,
        "cp" => cp::cp(docker, rest).await,
        "ps" => ps(docker, rest).await,
        "inspect" => inspect(docker, rest).await,
        "logs" => logs(docker, rest).await,
        "stop" => stop(docker, rest).await,
        "kill" => kill(docker, rest).await,
        "rm" => rm(docker, rest).await,
        "volume" => volume(docker, rest).await,
        "pull" => {
            let a = parse(rest, &["--platform"], &["-q|--quiet"], true)?;
            let image = a.rest.first().context("docker pull: no image given")?;
            pull(docker, image, a.get("--platform")).await?;
            Ok(0)
        }
        other => bail!(
            "unsupported docker command '{other}' (set {USE_CLI_ENV}=1 to use the docker CLI)"
        ),
    }
}

/// Render an error the way the docker CLI words it. Callers match on these
/// strings (`no such volume`, `volume is in use`, `Cannot connect to the
/// Docker daemon`), so the daemon's message has to come through verbatim.
fn describe(e: &anyhow::Error) -> String {
    match e.downcast_ref::<ApiError>() {
        Some(ApiError::DockerResponseServerError { message, .. }) => {
            format!("Error response from daemon: {message}")
        }
        _ => format!("{e:#}"),
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

async fn connect() -> Result<Docker> {
    // Same order as the docker CLI: DOCKER_HOST, then the selected context,
    // then the platform's default socket.
    let host = std::env::var("DOCKER_HOST")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(context_host);
    let unreachable = || {
        let at = host.as_deref().unwrap_or("the default socket");
        format!("Cannot connect to the Docker daemon at {at}. Is the docker daemon running?")
    };
    match &host {
        Some(host) => Docker::connect_with_host(host),
        None => Docker::connect_with_socket_defaults(),
    }
    .with_context(unreachable)?
    .with_timeout(NO_TIMEOUT)
    .negotiate_version()
    .await
    .with_context(unreachable)
}

fn docker_config_dir() -> Option<PathBuf> {
    std::env::var_os("DOCKER_CONFIG")
        .map(PathBuf::from)
        .or_else(|| Some(directories::BaseDirs::new()?.home_dir().join(".docker")))
}

fn read_json(path: &std::path::Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Endpoint of the docker context in effect (`docker context use`), which is
/// how Docker Desktop and rootless docker point clients at their sockets.
fn context_host() -> Option<String> {
    use sha2::Digest;
    let dir = docker_config_dir()?;
    let name = std::env::var("DOCKER_CONTEXT")
        .ok()
        .filter(|c| !c.is_empty())
        .or_else(|| {
            let config = read_json(&dir.join("config.json"))?;
            Some(config.get("currentContext")?.as_str()?.to_string())
        })?;
    if name == "default" {
        return None;
    }
    let id: String = sha2::Sha256::digest(name.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let meta = read_json(&dir.join("contexts/meta").join(id).join("meta.json"))?;
    Some(
        meta.pointer("/Endpoints/docker/Host")?
            .as_str()?
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

/// Flags and positionals of one docker subcommand. Each flag is keyed by the
/// last alias of its spec (`"-f|--format"` is stored as `--format`).
#[derive(Debug, Default)]
struct Parsed {
    flags: Vec<(&'static str, String)>,
    rest: Vec<String>,
}

impl Parsed {
    fn all<'a>(&'a self, flag: &'a str) -> impl Iterator<Item = &'a str> {
        self.flags
            .iter()
            .filter(move |(f, _)| *f == flag)
            .map(|(_, v)| v.as_str())
    }

    fn get<'a>(&'a self, flag: &'a str) -> Option<&'a str> {
        self.all(flag).last()
    }

    fn has(&self, flag: &str) -> bool {
        self.get(flag).is_some_and(|v| v != "false")
    }
}

fn unsupported(flag: &str) -> anyhow::Error {
    anyhow::anyhow!("unsupported docker flag '{flag}' (set {USE_CLI_ENV}=1 to use the docker CLI)")
}

/// Parse docker-style flags: `--flag value`, `--flag=value`, `-f value`,
/// `-fvalue`, and clustered booleans (`-it`). `valued` and `bools` list the
/// flags the subcommand knows, as `alias|alias`; anything else is an error
/// rather than a flag silently dropped. With `interspersed` false, parsing
/// stops at the first positional, which is how `run` and `exec` keep the
/// container command's own flags out of it.
fn parse(
    args: &[String],
    valued: &[&'static str],
    bools: &[&'static str],
    interspersed: bool,
) -> Result<Parsed> {
    fn spec(name: &str, specs: &[&'static str]) -> Option<&'static str> {
        let spec = specs.iter().find(|s| s.split('|').any(|a| a == name))?;
        spec.rsplit('|').next()
    }

    let mut out = Parsed::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--" {
            out.rest.extend(it.cloned());
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            out.rest.push(arg.clone());
            if !interspersed {
                out.rest.extend(it.cloned());
                break;
            }
            continue;
        }
        let mut value_for = |name: &str, inline: Option<&str>| match inline {
            Some(v) => Ok(v.to_string()),
            None => it
                .next()
                .cloned()
                .with_context(|| format!("flag needs an argument: {name}")),
        };
        if arg.starts_with("--") {
            let (name, inline) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (arg.as_str(), None),
            };
            if let Some(flag) = spec(name, valued) {
                out.flags.push((flag, value_for(name, inline)?));
            } else if let Some(flag) = spec(name, bools) {
                out.flags.push((flag, inline.unwrap_or("true").to_string()));
            } else {
                return Err(unsupported(name));
            }
            continue;
        }
        let shorts = &arg[1..];
        for (i, c) in shorts.char_indices() {
            let name = format!("-{c}");
            if let Some(flag) = spec(&name, valued) {
                let tail = &shorts[i + c.len_utf8()..];
                let inline = (!tail.is_empty()).then(|| tail.strip_prefix('=').unwrap_or(tail));
                out.flags.push((flag, value_for(&name, inline)?));
                break;
            } else if let Some(flag) = spec(&name, bools) {
                out.flags.push((flag, "true".to_string()));
            } else {
                return Err(unsupported(&name));
            }
        }
    }
    Ok(out)
}

/// `key=value` pairs into a map; a bare `key` maps to the empty string.
fn pairs<'a>(items: impl Iterator<Item = &'a str>) -> HashMap<String, String> {
    items
        .map(|item| {
            let (k, v) = item.split_once('=').unwrap_or((item, ""));
            (k.to_string(), v.to_string())
        })
        .collect()
}

/// `--filter key=value` flags in the shape the list endpoints take.
fn filters(a: &Parsed) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for filter in a.all("--filter") {
        let (k, v) = filter.split_once('=').unwrap_or((filter, ""));
        out.entry(k.to_string()).or_default().push(v.to_string());
    }
    out
}

// ---------------------------------------------------------------------------
// --format
// ---------------------------------------------------------------------------

/// Render a `--format` template against one object.
///
/// This is the slice of Go templates the crate's own format strings use, not a
/// Go template engine: `{{.A.B}}`, `{{index .A.B "key"}}`, `{{json .A}}` and
/// `{{.Label "key"}}`. Anything else is an error, not a guess.
fn render(template: &str, ctx: &Value) -> Result<String> {
    // The docker CLI expands these two escapes in `--format` itself.
    let template = template.replace("\\t", "\t").replace("\\n", "\n");
    let mut out = String::new();
    let mut rest = template.as_str();
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let end = rest[start..]
            .find("}}")
            .with_context(|| format!("unclosed action in template: {template}"))?;
        out.push_str(&eval(&rest[start + 2..start + end], ctx)?);
        rest = &rest[start + end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn eval(action: &str, ctx: &Value) -> Result<String> {
    // Whitespace-separated words; a "quoted" word may hold spaces.
    let mut words: Vec<String> = Vec::new();
    let mut quoted = false;
    let mut word = String::new();
    for c in action.trim().chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }

    fn lookup<'a>(ctx: &'a Value, path: &str) -> &'a Value {
        path.split('.')
            .filter(|field| !field.is_empty())
            .fold(ctx, |value, field| value.get(field).unwrap_or(&Value::Null))
    }
    fn show(value: &Value, missing: &str) -> String {
        match value {
            Value::Null => missing.to_string(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }

    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    Ok(match words.as_slice() {
        [".Label", key] => show(lookup(ctx, ".Labels").get(key).unwrap_or(&Value::Null), ""),
        [path] if path.starts_with('.') => show(lookup(ctx, path), "<no value>"),
        ["json", path] if path.starts_with('.') => lookup(ctx, path).to_string(),
        ["index", path, keys @ ..] if path.starts_with('.') => {
            let value = keys.iter().fold(lookup(ctx, path), |value, key| {
                value.get(key).unwrap_or(&Value::Null)
            });
            // A missing map key is Go's zero value, the empty string.
            show(value, "")
        }
        _ => bail!("unsupported --format action '{{{{{action}}}}}'"),
    })
}

// ---------------------------------------------------------------------------
// Containers
// ---------------------------------------------------------------------------

async fn ps(docker: &Docker, args: &[String]) -> Result<i32> {
    let a = parse(
        args,
        &["-f|--filter", "--format"],
        &["-a|--all", "-q|--quiet", "--no-trunc"],
        true,
    )?;
    let options = q::ListContainersOptionsBuilder::new()
        .all(a.has("--all"))
        .filters(&filters(&a))
        .build();
    let default = if a.has("--quiet") {
        "{{.ID}}"
    } else {
        "{{.ID}}\t{{.Image}}\t{{.Status}}\t{{.Names}}"
    };
    for c in docker.list_containers(Some(options)).await? {
        let mut id = c.id.unwrap_or_default();
        if !a.has("--no-trunc") {
            id.truncate(12);
        }
        let names: Vec<&str> = c
            .names
            .iter()
            .flatten()
            .map(|n| n.trim_start_matches('/'))
            .collect();
        let ctx = json!({
            "ID": id,
            "Names": names.join(","),
            "Image": c.image,
            "Status": c.status,
            "State": c.state.map(|s| s.to_string()),
            "Labels": c.labels,
        });
        out!("{}", render(a.get("--format").unwrap_or(default), &ctx)?);
    }
    Ok(0)
}

/// Print each object through `--format`, or all of them as the JSON array
/// `docker inspect` prints.
fn print_inspected(objects: Vec<Value>, format: Option<&str>) -> Result<()> {
    match format {
        Some(format) => {
            for object in &objects {
                out!("{}", render(format, object)?);
            }
        }
        None => out!("{}", serde_json::to_string_pretty(&objects)?),
    }
    Ok(())
}

async fn inspect(docker: &Docker, args: &[String]) -> Result<i32> {
    let a = parse(args, &["-f|--format", "--type"], &["-s|--size"], true)?;
    if a.get("--type").is_some_and(|t| t != "container") {
        bail!("docker inspect: only containers are supported here");
    }
    let mut objects = Vec::new();
    for name in &a.rest {
        objects.push(serde_json::to_value(
            docker.inspect_container(name, None).await?,
        )?);
    }
    print_inspected(objects, a.get("--format"))?;
    Ok(0)
}

async fn logs(docker: &Docker, args: &[String]) -> Result<i32> {
    let a = parse(args, &["-n|--tail"], &["-f|--follow", "--timestamps"], true)?;
    let name = a.rest.first().context("docker logs: no container given")?;
    let options = q::LogsOptionsBuilder::new()
        .stdout(true)
        .stderr(true)
        .follow(a.has("--follow"))
        .timestamps(a.has("--timestamps"))
        .tail(a.get("--tail").unwrap_or("all"))
        .build();
    let mut stream = docker.logs(name, Some(options));
    while let Some(frame) = stream.next().await {
        write_frame(frame?)?;
    }
    Ok(0)
}

/// Write one frame of a container's output to the matching stream of this
/// process, unbuffered so prompts and progress lines appear as they are sent.
fn write_frame(frame: LogOutput) -> std::io::Result<()> {
    match frame {
        LogOutput::StdErr { message } => {
            let mut err = std::io::stderr();
            err.write_all(&message)?;
            err.flush()
        }
        LogOutput::StdOut { message } | LogOutput::Console { message } => {
            let mut out = std::io::stdout();
            out.write_all(&message)?;
            out.flush()
        }
        LogOutput::StdIn { .. } => Ok(()),
    }
}

/// Run `op` for each name, printing the name on success and the daemon's
/// error on failure, as `docker stop|kill|rm a b c` do. With `missing_ok`, an
/// object that does not exist is passed over in silence — what `docker rm -f`
/// does.
async fn each<'a, F>(names: &'a [String], missing_ok: bool, op: impl Fn(&'a str) -> F) -> i32
where
    F: Future<Output = Result<(), ApiError>>,
{
    let mut code = 0;
    for name in names {
        match op(name).await {
            Ok(()) => out!("{name}"),
            Err(e) => {
                let missing = matches!(
                    e,
                    ApiError::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }
                );
                if !(missing && missing_ok) {
                    eprintln!("{}", describe(&e.into()));
                    code = 1;
                }
            }
        }
    }
    code
}

async fn stop(docker: &Docker, args: &[String]) -> Result<i32> {
    let a = parse(args, &["-t|--time|--timeout", "-s|--signal"], &[], true)?;
    let mut options = q::StopContainerOptionsBuilder::new();
    if let Some(t) = a.get("--timeout") {
        options = options.t(t.parse().context("docker stop: invalid timeout")?);
    }
    if let Some(signal) = a.get("--signal") {
        options = options.signal(signal);
    }
    let options = options.build();
    Ok(each(&a.rest, false, |name| {
        docker.stop_container(name, Some(options.clone()))
    })
    .await)
}

async fn kill(docker: &Docker, args: &[String]) -> Result<i32> {
    let a = parse(args, &["-s|--signal"], &[], true)?;
    let options = q::KillContainerOptionsBuilder::new()
        .signal(a.get("--signal").unwrap_or("KILL"))
        .build();
    Ok(each(&a.rest, false, |name| {
        docker.kill_container(name, Some(options.clone()))
    })
    .await)
}

async fn rm(docker: &Docker, args: &[String]) -> Result<i32> {
    let a = parse(args, &[], &["-f|--force", "-v|--volumes"], true)?;
    let options = q::RemoveContainerOptionsBuilder::new()
        .force(a.has("--force"))
        .v(a.has("--volumes"))
        .build();
    let force = a.has("--force");
    Ok(each(&a.rest, force, |name| {
        docker.remove_container(name, Some(options.clone()))
    })
    .await)
}

// ---------------------------------------------------------------------------
// Volumes
// ---------------------------------------------------------------------------

async fn volume(docker: &Docker, args: &[String]) -> Result<i32> {
    let (sub, rest) = args
        .split_first()
        .context("docker volume: no command given")?;
    match sub.as_str() {
        "ls" => {
            let a = parse(rest, &["-f|--filter", "--format"], &["-q|--quiet"], true)?;
            let options = q::ListVolumesOptionsBuilder::new()
                .filters(&filters(&a))
                .build();
            let listed = docker.list_volumes(Some(options)).await?;
            for volume in listed.volumes.unwrap_or_default() {
                let ctx = serde_json::to_value(volume)?;
                out!("{}", render(a.get("--format").unwrap_or("{{.Name}}"), &ctx)?);
            }
            Ok(0)
        }
        "inspect" => {
            let a = parse(rest, &["-f|--format"], &[], true)?;
            let mut objects = Vec::new();
            for name in &a.rest {
                objects.push(serde_json::to_value(docker.inspect_volume(name).await?)?);
            }
            print_inspected(objects, a.get("--format"))?;
            Ok(0)
        }
        "create" => {
            let a = parse(rest, &["--label", "-d|--driver", "-o|--opt"], &[], true)?;
            let created = docker
                .create_volume(bollard::models::VolumeCreateRequest {
                    name: a.rest.first().cloned(),
                    driver: a.get("--driver").map(String::from),
                    driver_opts: Some(pairs(a.all("--opt"))),
                    labels: Some(pairs(a.all("--label"))),
                    ..Default::default()
                })
                .await?;
            out!("{}", created.name);
            Ok(0)
        }
        "rm" | "remove" => {
            let a = parse(rest, &[], &["-f|--force"], true)?;
            let options = q::RemoveVolumeOptionsBuilder::new()
                .force(a.has("--force"))
                .build();
            Ok(each(&a.rest, false, |name| {
                docker.remove_volume(name, Some(options.clone()))
            })
            .await)
        }
        other => bail!(
            "unsupported docker command 'volume {other}' (set {USE_CLI_ENV}=1 to use the docker CLI)"
        ),
    }
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

/// Pull `image`, reporting progress on stderr so a caller reading the
/// command's stdout never sees it.
pub(crate) async fn pull(docker: &Docker, image: &str, platform: Option<&str>) -> Result<()> {
    // The endpoint pulls every tag of a repository unless told which one, so
    // default it the way the docker CLI does.
    let (repo, tag) = match image.split_once('@') {
        Some((repo, digest)) => (repo, digest),
        None => match image.rsplit_once(':') {
            Some((repo, tag)) if !tag.contains('/') => (repo, tag),
            _ => (image, "latest"),
        },
    };
    let mut options = q::CreateImageOptionsBuilder::new()
        .from_image(repo)
        .tag(tag);
    if let Some(platform) = platform {
        options = options.platform(platform);
    }

    let mut stream = docker.create_image(Some(options.build()), None, registry_credentials(image));
    // ponytail: one aggregate progress line on a terminal, not the docker
    // CLI's per-layer bars.
    let tty = std::io::stderr().is_terminal();
    let mut layers: HashMap<String, (String, i64, i64)> = HashMap::new();
    let mut drawn = Instant::now();
    let mut line_open = false;
    while let Some(info) = stream.next().await {
        let info = info?;
        if let Some(message) = info.error_detail.and_then(|e| e.message) {
            bail!("Error response from daemon: {message}");
        }
        let status = info.status.unwrap_or_default();
        let Some(id) = info.id.filter(|_| !status.starts_with("Pulling from")) else {
            if line_open {
                eprintln!();
                line_open = false;
            }
            eprintln!("{status}");
            continue;
        };
        let progress = info.progress_detail.unwrap_or_default();
        let layer = layers.entry(id.clone()).or_default();
        let changed = layer.0 != status;
        layer.0 = status.clone();
        if status == "Downloading" {
            layer.1 = progress.current.unwrap_or(layer.1);
            layer.2 = progress.total.unwrap_or(layer.2);
        }
        if !tty {
            if changed && status != "Downloading" && status != "Extracting" {
                eprintln!("{id}: {status}");
            }
        } else if changed || drawn.elapsed() > Duration::from_millis(200) {
            drawn = Instant::now();
            let done = layers
                .values()
                .filter(|l| l.0 == "Pull complete" || l.0 == "Already exists")
                .count();
            let (current, total) = layers
                .values()
                .fold((0, 0), |(c, t), l| (c + l.1.min(l.2), t + l.2));
            eprint!(
                "\r{image}: {done}/{} layers, {} / {} MB\x1b[K",
                layers.len(),
                current / 1_000_000,
                total / 1_000_000
            );
            line_open = true;
        }
    }
    if line_open {
        eprintln!();
    }
    Ok(())
}

/// Credentials `docker login` stored for the registry `image` lives on, from
/// the docker config file or its credential helper. `None` pulls anonymously.
fn registry_credentials(image: &str) -> Option<DockerCredentials> {
    use base64::Engine;
    let config = read_json(&docker_config_dir()?.join("config.json"))?;
    let first = image.split('/').next().unwrap_or_default();
    let named =
        image.contains('/') && (first.contains('.') || first.contains(':') || first == "localhost");
    // `docker login` files Docker Hub under its v1 index URL, however the
    // image spells the registry.
    let hub = matches!(
        first,
        "docker.io" | "index.docker.io" | "registry-1.docker.io"
    );
    let server = if named && !hub {
        first
    } else {
        "https://index.docker.io/v1/"
    };

    let helper = config
        .get("credHelpers")
        .and_then(|helpers| helpers.get(server))
        .or_else(|| config.get("credsStore"))
        .and_then(Value::as_str);
    let (username, secret) = if let Some(helper) = helper {
        let mut child = std::process::Command::new(format!("docker-credential-{helper}"))
            .arg("get")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        child.stdin.take()?.write_all(server.as_bytes()).ok()?;
        let found: Value = serde_json::from_slice(&child.wait_with_output().ok()?.stdout).ok()?;
        (
            found.get("Username")?.as_str()?.to_string(),
            found.get("Secret")?.as_str()?.to_string(),
        )
    } else {
        let entry = config.get("auths")?.get(server)?;
        if let Some(token) = entry.get("identitytoken").and_then(Value::as_str) {
            ("<token>".to_string(), token.to_string())
        } else {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(entry.get("auth")?.as_str()?)
                .ok()?;
            let decoded = String::from_utf8(decoded).ok()?;
            let (user, password) = decoded.split_once(':')?;
            (user.to_string(), password.to_string())
        }
    };

    let mut credentials = DockerCredentials {
        serveraddress: Some(server.to_string()),
        ..Default::default()
    };
    // Helpers and `docker login` both mark an identity token this way.
    if username == "<token>" {
        credentials.identitytoken = Some(secret);
    } else {
        credentials.username = Some(username);
        credentials.password = Some(secret);
    }
    Some(credentials)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn remote_and_tls_daemons_stay_on_the_docker_cli() {
        let none = || None;
        let ctx = |host: &str| {
            let host = host.to_string();
            move || Some(host)
        };
        assert!(reachable(None, none));
        assert!(reachable(Some("unix:///run/docker.sock"), ctx("ssh://far")));
        assert!(reachable(Some("tcp://10.0.0.2:2375"), none));
        assert!(!reachable(Some("ssh://user@far"), none));
        assert!(reachable(
            None,
            ctx("unix:///home/u/.docker/run/docker.sock")
        ));
        assert!(!reachable(None, ctx("ssh://user@far")));
        assert!(!reachable(None, ctx("tcp://far:2376")));
    }

    #[test]
    fn docker_hub_logins_are_found_however_the_image_names_the_hub() {
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let auth = base64::engine::general_purpose::STANDARD.encode("hubuser:pw");
        std::fs::write(
            dir.path().join("config.json"),
            format!(r#"{{"auths":{{"https://index.docker.io/v1/":{{"auth":"{auth}"}},"ghcr.io":{{"auth":"{auth}"}}}}}}"#),
        )
        .unwrap();
        std::env::set_var("DOCKER_CONFIG", dir.path());
        for image in [
            "busybox",
            "avocadolinux/sdk:2026",
            "docker.io/avocadolinux/sdk:2026",
            "index.docker.io/library/busybox",
            "ghcr.io/org/img",
        ] {
            let found = registry_credentials(image).unwrap_or_else(|| panic!("{image}"));
            assert_eq!(found.username.as_deref(), Some("hubuser"), "{image}");
        }
        assert!(registry_credentials("quay.io/org/img").is_none());
        std::env::remove_var("DOCKER_CONFIG");
    }

    #[test]
    fn only_the_bare_docker_tool_is_replaced() {
        // Under `cfg(test)` nothing is rerouted, so this pins the half that
        // matters for every fake-tool test: other tools are spawned as given.
        for tool in ["docker", "podman", "/usr/bin/docker", "/tmp/x/fake-tool"] {
            assert_eq!(program(tool), (tool.into(), None));
        }
    }

    #[test]
    fn parse_takes_every_docker_flag_spelling() {
        let a = parse(
            &argv(&[
                "--name=n",
                "-e",
                "A=1",
                "-eB=2",
                "-e=C=3",
                "--env",
                "D=4",
                "-di",
                "--rm=false",
                "img",
                "-e",
                "not-a-flag",
            ]),
            &["--name", "-e|--env"],
            &["-d|--detach", "-i|--interactive", "--rm"],
            false,
        )
        .unwrap();
        assert_eq!(a.get("--name"), Some("n"));
        assert_eq!(
            a.all("--env").collect::<Vec<_>>(),
            ["A=1", "B=2", "C=3", "D=4"]
        );
        assert!(a.has("--detach") && a.has("--interactive") && !a.has("--rm"));
        // Everything after the image belongs to the container's command.
        assert_eq!(a.rest, ["img", "-e", "not-a-flag"]);
    }

    #[test]
    fn parse_allows_flags_after_positionals_when_interspersed() {
        let a = parse(
            &argv(&["vol", "--format", "{{.Mountpoint}}"]),
            &["-f|--format"],
            &[],
            true,
        )
        .unwrap();
        assert_eq!(a.rest, ["vol"]);
        assert_eq!(a.get("--format"), Some("{{.Mountpoint}}"));
    }

    #[test]
    fn parse_rejects_a_flag_it_does_not_know() {
        let err = parse(&argv(&["--gpus-typo", "all", "img"]), &[], &[], false).unwrap_err();
        assert!(err.to_string().contains("'--gpus-typo'"), "{err}");
        assert!(parse(&argv(&["--name"]), &["--name"], &[], false).is_err());
    }

    /// Every `--format` string the crate passes to a docker command.
    #[test]
    fn render_covers_the_format_strings_in_use() {
        let container = json!({
            "ID": "0123456789ab",
            "Names": "avocado-explorer-1",
            "Status": "Up 2 minutes",
            "Labels": {"avocado.target": "qemux86-64", "avocado.port": "2049"},
            "State": {"Running": true, "Status": "running"},
            "Config": {"Labels": {"avocado.port": "2049"}},
            "Mountpoint": "/var/lib/docker/volumes/v/_data",
        });
        for (template, want) in [
            ("{{.ID}}\t{{.Names}}", "0123456789ab\tavocado-explorer-1"),
            ("{{.State.Running}}", "true"),
            ("{{.State.Status}}", "running"),
            ("{{.Mountpoint}}", "/var/lib/docker/volumes/v/_data"),
            (
                "{{index .Config.Labels \"avocado.port\"}}\\t{{index .Config.Labels \"avocado.extensions\"}}",
                "2049\t",
            ),
            (
                "{{.Names}}\t{{.Label \"avocado.target\"}}\t{{.Label \"avocado.project\"}}\t{{.Status}}",
                "avocado-explorer-1\tqemux86-64\t\tUp 2 minutes",
            ),
            ("{{.Missing.Field}}", "<no value>"),
            ("{{json .State}}", r#"{"Running":true,"Status":"running"}"#),
        ] {
            assert_eq!(render(template, &container).unwrap(), want, "{template}");
        }
        assert!(render("{{range .Mounts}}x{{end}}", &container).is_err());
        assert!(render("{{.ID", &container).is_err());
    }
}
