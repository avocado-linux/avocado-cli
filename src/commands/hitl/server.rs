//! `avocado hitl` -- a managed NFS server that serves extension sysroots to a
//! device, in place of the images installed there.
//!
//! The server is a detached, named, labelled container. Named so a second
//! `start` finds the first; labelled so `status`/`stop`/`logs` find it by
//! role without knowing the name; detached so the terminal is not a log tail.
//! The first cut ran it in the foreground under a random `avocado-run-<uuid>`
//! name: indistinguishable from any other container, unmanageable, and its
//! terminal showed ganesha internals with the one line that mattered -- the
//! export failing to load -- scrolling past in them.
//!
//! Two things it does that the first cut did not, both found on hardware:
//!
//! - **Export paths are resolved.** `$AVOCADO_EXT_SYSROOTS` is a symlink to
//!   the per-runtime extensions directory. ganesha's VFS FSAL will not
//!   traverse a symlinked export root; it fails `init_export_root` with an
//!   ELOOP mapped to "Undefined server error", the client sees "reason given
//!   by server: No such file or directory", and nothing works. Since that
//!   directory is always a symlink, HITL had never worked on a project in
//!   this shape.
//! - **Startup is verified.** ganesha prints `NFS SERVER INITIALIZED` and keeps
//!   running with zero working exports. `start` waits for that line and fails
//!   if any `:CRIT :` line preceded it, quoting them.

use crate::utils::config::{ComposedConfig, Config};
use crate::utils::container::{is_docker_desktop, RunConfig, SdkContainer};
use crate::utils::nfs_server::{NfsExport, HITL_DEFAULT_PORT};
use crate::utils::output::{print_debug, print_error, print_info, print_success, OutputLevel};
use crate::utils::stamps::{
    generate_batch_read_stamps_script, validate_stamps_batch, StampRequirement,
};
use crate::utils::target::validate_and_log_target;
use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Label every HITL server carries; `status`/`stop` select on it.
pub const LABEL_ROLE: &str = "avocado.role=hitl";

/// How long `start` waits for ganesha to report initialised before giving up.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Which project + target a server belongs to. Everything else -- name,
/// labels, the connect hint -- derives from this.
pub struct HitlIdentity {
    pub target: String,
    pub project_dir: PathBuf,
}

impl HitlIdentity {
    pub fn new(target: &str, config_path: &str) -> Self {
        let project_dir = Path::new(config_path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let project_dir = std::fs::canonicalize(&project_dir).unwrap_or(project_dir);
        Self {
            target: target.to_string(),
            project_dir,
        }
    }

    /// `avocado-hitl-<target>-<8 hex of the project path>`: stable across
    /// invocations, distinct across projects, readable in `docker ps`.
    pub fn container_name(&self) -> String {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in self.project_dir.to_string_lossy().bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!(
            "avocado-hitl-{}-{:08x}",
            self.target,
            (h >> 32) as u32 ^ h as u32
        )
    }

    pub fn labels(&self, port: u16, extensions: &[String]) -> Vec<String> {
        let mut v = vec![
            "--label".into(),
            LABEL_ROLE.into(),
            "--label".into(),
            format!("avocado.target={}", self.target),
            "--label".into(),
            format!("avocado.project={}", self.project_dir.display()),
            "--label".into(),
            format!("avocado.port={port}"),
        ];
        if !extensions.is_empty() {
            v.push("--label".into());
            v.push(format!("avocado.extensions={}", extensions.join(",")));
        }
        v
    }
}

#[derive(Args, Debug)]
pub struct HitlServerCommand {
    /// Path to the avocado.yaml configuration file
    #[arg(short, long, default_value = "avocado.yaml")]
    pub config_path: String,

    /// Extensions to create NFS exports for
    #[arg(short, long = "extension")]
    pub extensions: Vec<String>,

    /// Additional container arguments
    #[arg(long)]
    pub container_args: Option<Vec<String>>,

    /// Additional DNF arguments
    #[arg(long)]
    pub dnf_args: Option<Vec<String>>,

    /// Target to build for
    #[arg(short, long)]
    pub target: Option<String>,

    /// Enable verbose output
    #[arg(short, long)]
    pub verbose: bool,

    /// NFS port number to use
    pub port: Option<u16>,

    /// Disable stamp validation
    #[arg(long)]
    pub no_stamps: bool,

    /// Stay attached and stream the server log (the old `hitl server` behaviour)
    #[arg(long)]
    pub foreground: bool,

    /// SDK container architecture for cross-arch emulation
    #[arg(skip)]
    pub sdk_arch: Option<String>,

    /// Pre-composed configuration to avoid reloading
    #[arg(skip)]
    pub composed_config: Option<Arc<ComposedConfig>>,
}

impl HitlServerCommand {
    #[allow(dead_code)]
    pub fn with_composed_config(mut self, config: Arc<ComposedConfig>) -> Self {
        self.composed_config = Some(config);
        self
    }

    pub async fn execute(&self) -> Result<()> {
        let composed = match &self.composed_config {
            Some(cc) => Arc::clone(cc),
            None => Arc::new(
                Config::load_composed(&self.config_path, self.target.as_deref())
                    .with_context(|| format!("Failed to load config from {}", self.config_path))?,
            ),
        };
        let config = &composed.config;
        let container_helper =
            SdkContainer::from_config(&self.config_path, config)?.verbose(self.verbose);
        let tool = container_helper.container_tool.clone();
        let target = validate_and_log_target(self.target.as_deref(), config)?;

        let (container_image, repo_url, repo_release) = if let Some(sdk_config) = &config.sdk {
            (
                sdk_config
                    .image
                    .clone()
                    .unwrap_or_else(|| "docker.io/avocadolinux/sdk:apollo-edge".to_string()),
                sdk_config.repo_url.clone(),
                sdk_config.repo_release.clone(),
            )
        } else {
            bail!("No SDK configuration found in config file");
        };

        if self.extensions.is_empty() {
            bail!("No extensions given. Pass one or more with -e <name>; there is nothing to serve otherwise.");
        }
        self.validate_extension_names()?;

        let identity = HitlIdentity::new(&target, &self.config_path);
        let name = identity.container_name();
        let nfs_port = self.port.unwrap_or(HITL_DEFAULT_PORT);

        // One server per project+target. A running one is reported, not
        // duplicated; a dead one (crashed, or stopped without `hitl stop`) is
        // cleared so its name is free.
        match container_state(&tool, &name)? {
            Some(state) if state == "running" => {
                print_info(
                    &format!("HITL server '{name}' is already running."),
                    OutputLevel::Normal,
                );
                // The hint describes the server that is running, not the
                // options this invocation was given.
                match running_port_and_extensions(&tool, &name) {
                    Some((port, extensions)) => {
                        if self.port.is_some_and(|p| p != port) || extensions != self.extensions {
                            print_info(
                                &format!(
                                    "It serves {} on port {port}; `avocado hitl stop` first to change that.",
                                    extensions.join(", ")
                                ),
                                OutputLevel::Normal,
                            );
                        }
                        print_connect_hint(port, &extensions);
                    }
                    None => print_info(
                        "Could not read its labels; `avocado hitl status` describes it.",
                        OutputLevel::Normal,
                    ),
                }
                return Ok(());
            }
            Some(state) => {
                if self.verbose {
                    print_debug(
                        &format!("Removing previous HITL server '{name}' ({state})"),
                        OutputLevel::Normal,
                    );
                }
                let _ = Command::new(&tool).args(["rm", "-f", &name]).output();
            }
            None => {}
        }

        if !self.no_stamps {
            self.validate_stamps(
                &container_helper,
                &container_image,
                &target,
                &repo_url,
                &repo_release,
            )
            .await?;
        }

        // Network: host networking on Linux so the device reaches the port
        // directly. Docker Desktop's VM does not expose host-networked ports,
        // so publish the port there.
        let mut container_args = if is_docker_desktop() {
            vec!["-p".to_string(), format!("0.0.0.0:{nfs_port}:{nfs_port}")]
        } else {
            vec!["--net=host".to_string()]
        };
        container_args.extend(["--cap-add".to_string(), "DAC_READ_SEARCH".to_string()]);
        container_args.push("--init".to_string());
        container_args.extend(identity.labels(nfs_port, &self.extensions));
        if let Some(additional) = Config::process_container_args(self.container_args.as_ref()) {
            container_args.extend(additional);
        }

        let setup_command = format!(
            "if [ -f \"${{AVOCADO_SDK_PREFIX}}/environment-setup\" ]; then \
             source \"${{AVOCADO_SDK_PREFIX}}/environment-setup\"; \
             fi && \
             ln -sf ${{AVOCADO_SDK_PREFIX}}/etc/netconfig /etc/netconfig && \
             mkdir -p /tmp/hitl && \
             ln -sf ${{AVOCADO_SDK_PREFIX}}/usr/var/lib/nfs/ganesha /tmp/hitl && \
             {} \
             exec avocado-hitl-server -c ${{AVOCADO_SDK_PREFIX}}/etc/avocado/hitl-nfs.conf",
            self.generate_export_setup_commands()
        );

        if self.verbose {
            print_debug(&format!("Container: {name}"), OutputLevel::Normal);
            print_debug(
                &format!("Container args: {container_args:?}"),
                OutputLevel::Normal,
            );
            print_debug(
                &format!("Setup command: {setup_command}"),
                OutputLevel::Normal,
            );
        }

        let run = RunConfig {
            container_image,
            target: target.clone(),
            command: setup_command,
            container_name: Some(name.clone()),
            // Detached, and kept after exit: a crashed server's log is the
            // only thing that says why it crashed. `hitl stop` removes it.
            detach: !self.foreground,
            rm: self.foreground,
            verbose: self.verbose,
            source_environment: true,
            interactive: self.foreground,
            repo_url,
            repo_release,
            container_args: Some(container_args),
            dnf_args: self.dnf_args.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };

        let started = container_helper.run_in_container(run).await?;
        if self.foreground {
            if !started {
                bail!("HITL server '{name}' exited with an error.");
            }
            return Ok(());
        }
        if !started {
            bail!("Could not launch HITL server '{name}'.");
        }

        // The container is up; ganesha may still fail. Wait for it to say it
        // is serving, and refuse to call that success if an export did not load.
        wait_for_ready(&tool, &name)?;
        print_success(
            &format!(
                "HITL server '{name}' is serving {} on port {nfs_port}.",
                self.extensions.join(", ")
            ),
            OutputLevel::Normal,
        );
        print_connect_hint(nfs_port, &self.extensions);
        print_info(
            "`avocado hitl status` lists servers, `avocado hitl logs` shows this one, `avocado hitl stop` removes it.",
            OutputLevel::Normal,
        );
        Ok(())
    }

    /// Extension names are interpolated into the container's setup shell, a
    /// file name and ganesha's config, so they are held to the grammar the
    /// fetch script already enforces rather than quoted at every site.
    fn validate_extension_names(&self) -> Result<()> {
        for ext in &self.extensions {
            crate::utils::ext_fetch::validate_shell_safe("name", ext)?;
            // `,` separates names in the `avocado.extensions` label.
            if ext.contains(',') {
                bail!("Extension name '{ext}' contains ','.");
            }
        }
        Ok(())
    }

    async fn validate_stamps(
        &self,
        container_helper: &SdkContainer,
        container_image: &str,
        target: &str,
        repo_url: &Option<String>,
        repo_release: &Option<String>,
    ) -> Result<()> {
        print_info("Validating extension stamps...", OutputLevel::Normal);
        let mut requirements = vec![StampRequirement::sdk_install()];
        for ext_name in &self.extensions {
            requirements.push(StampRequirement::ext_install(ext_name));
            requirements.push(StampRequirement::ext_build(ext_name));
        }
        let batch_script = generate_batch_read_stamps_script(&requirements);
        let validation_config = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: batch_script,
            verbose: false,
            source_environment: true,
            interactive: false,
            repo_url: repo_url.clone(),
            repo_release: repo_release.clone(),
            sdk_arch: self.sdk_arch.clone(),
            ..Default::default()
        };
        let output = container_helper
            .run_in_container_with_output(validation_config)
            .await?;
        let validation = validate_stamps_batch(&requirements, output.as_deref().unwrap_or(""), &[]);
        if !validation.is_satisfied() {
            validation
                .into_error("Cannot start HITL server")
                .with_search_root(crate::utils::stamps::StampSearchRoot::for_container(
                    container_helper,
                    target,
                ))
                .print_and_exit();
        }
        Ok(())
    }

    /// Shell that writes the ganesha export files inside the container.
    fn generate_export_setup_commands(&self) -> String {
        let mut commands = vec![
            "mkdir -p ${AVOCADO_SDK_PREFIX}/etc/avocado/exports.d".to_string(),
            "rm -f ${AVOCADO_SDK_PREFIX}/etc/avocado/exports.d/*.conf".to_string(),
        ];
        let config_file = "${AVOCADO_SDK_PREFIX}/etc/avocado/hitl-nfs.conf";
        commands.push(format!(
            "touch {config_file} && \
             sed -i '/^%dir .*\\/etc\\/avocado\\/exports\\.d$/d' {config_file} && \
             echo \"%dir ${{AVOCADO_SDK_PREFIX}}/etc/avocado/exports.d\" >> {config_file}"
        ));
        let port = self.port.unwrap_or(HITL_DEFAULT_PORT);
        commands.push(format!(
            "sed -i '/NFS_Core_Param {{/,/}}/s/NFS_Port = [0-9]\\+;/NFS_Port = {port};/' {config_file}"
        ));

        for (index, extension) in self.extensions.iter().enumerate() {
            let export_id = (index + 1) as u32;
            // readlink -f: $AVOCADO_EXT_SYSROOTS is itself a symlink and
            // ganesha refuses a symlinked export root (see module doc).
            let export = NfsExport::new(
                export_id,
                PathBuf::from(format!(
                    "$(readlink -f ${{AVOCADO_EXT_SYSROOTS}}/{extension})"
                )),
                format!("/{extension}"),
            );
            let content = Self::generate_ganesha_export_block(&export)
                .replace('\\', "\\\\")
                .replace('"', "\\\"");
            commands.push(format!(
                "[ -d \"${{AVOCADO_EXT_SYSROOTS}}/{extension}\" ] || {{ echo \"ERROR: extension sysroot ${{AVOCADO_EXT_SYSROOTS}}/{extension} does not exist -- run avocado ext install/build {extension} first\" >&2; exit 1; }}"
            ));
            commands.push(format!(
                "echo -e \"{content}\" > ${{AVOCADO_SDK_PREFIX}}/etc/avocado/exports.d/{extension}.conf"
            ));
        }
        format!("{} &&", commands.join(" && "))
    }

    fn generate_ganesha_export_block(export: &NfsExport) -> String {
        format!(
            "EXPORT {{\n\
            \x20\x20Export_Id = {};\n\
            \x20\x20Path = {};\n\
            \x20\x20Pseudo = {};\n\
            \x20\x20FSAL {{\n\
            \x20\x20\x20\x20name = VFS;\n\
            \x20\x20}}\n\
            }}",
            export.export_id,
            export.local_path.display(),
            export.pseudo_path
        )
    }
}

/// `docker inspect` state of a container: `Ok(None)` when it does not exist,
/// `Err` when the container tool itself failed -- a daemon that is down must
/// not read as "no server".
fn container_state(tool: &str, name: &str) -> Result<Option<String>> {
    let out = Command::new(tool)
        .args(["container", "inspect", "-f", "{{.State.Status}}", name])
        .output()
        .with_context(|| format!("running {tool} inspect"))?;
    if out.status.success() {
        return Ok(Some(
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ));
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if is_no_such(&err) {
        return Ok(None);
    }
    bail!("{tool} inspect {name} failed: {}", err.trim())
}

/// docker says `No such object` / `No such container`, podman `no such
/// container`: the container is absent, as opposed to the tool failing.
fn is_no_such(stderr: &str) -> bool {
    stderr.to_ascii_lowercase().contains("no such")
}

/// What a running server was started with, from its labels.
fn running_port_and_extensions(tool: &str, name: &str) -> Option<(u16, Vec<String>)> {
    let out = Command::new(tool)
        .args([
            "container",
            "inspect",
            "-f",
            "{{index .Config.Labels \"avocado.port\"}}\t{{index .Config.Labels \"avocado.extensions\"}}",
            name,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_port_and_extensions(&String::from_utf8_lossy(&out.stdout))
}

/// `<port>\t<ext,ext,...>` as `inspect` prints the two labels.
fn parse_port_and_extensions(text: &str) -> Option<(u16, Vec<String>)> {
    let (port, exts) = text.trim_end_matches('\n').split_once('\t')?;
    Some((
        port.parse().ok()?,
        exts.split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    ))
}

/// `docker ps -a` over every HITL server on this machine, in `format`.
fn list_servers(tool: &str, format: &str) -> Result<String> {
    let out = Command::new(tool)
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label={LABEL_ROLE}"),
            "--format",
            format,
        ])
        .output()
        .with_context(|| format!("running {tool} ps"))?;
    if !out.status.success() {
        bail!(
            "{tool} ps failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn logs(tool: &str, name: &str) -> String {
    Command::new(tool)
        .args(["logs", name])
        .output()
        .map(|o| {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            s
        })
        .unwrap_or_default()
}

/// Decide from ganesha's log whether it is serving what was asked. `Ok(())`
/// once `NFS SERVER INITIALIZED` appears with no `:CRIT :` before it; an
/// error quoting the CRIT lines otherwise; `None` if it has said neither yet.
pub fn judge_startup(log: &str) -> Option<Result<()>> {
    // Only what ganesha said BEFORE declaring itself initialised counts
    // against startup; a CRIT after that is a runtime event, not an export
    // that failed to load.
    const READY: &str = "NFS SERVER INITIALIZED";
    let startup = log.split(READY).next().unwrap_or(log);
    let crits: Vec<&str> = startup
        .lines()
        .filter(|l| l.contains(" :CRIT :") || l.contains(":FATAL :"))
        .collect();
    if !crits.is_empty() {
        let detail: Vec<String> = crits
            .iter()
            .map(|l| {
                // "... :EXPORT :CRIT :Lookup failed on path, ExportId=1 Path=..." -> after the level
                l.rsplit(" :CRIT :")
                    .next()
                    .or_else(|| l.rsplit(":FATAL :").next())
                    .unwrap_or(l)
                    .trim()
                    .to_string()
            })
            .collect();
        return Some(Err(anyhow::anyhow!(
            "the NFS server started but could not serve what was asked:\n  {}",
            detail.join("\n  ")
        )));
    }
    if log.contains(READY) {
        return Some(Ok(()));
    }
    None
}

fn wait_for_ready(tool: &str, name: &str) -> Result<()> {
    let start = Instant::now();
    loop {
        if let Some(verdict) = judge_startup(&logs(tool, name)) {
            if verdict.is_err() {
                let _ = Command::new(tool).args(["stop", name]).output();
            }
            return verdict.with_context(|| {
                format!(
                    "HITL server '{name}' failed to start; container kept for `{tool} logs {name}`"
                )
            });
        }
        // An inspect error here is "unknown": keep polling, the timeout is the
        // backstop. Aborting would leave an unverified server holding the port.
        if let Ok(Some(state)) = container_state(tool, name) {
            if state != "running" && state != "created" {
                bail!(
                    "HITL server '{name}' exited during startup ({state}). Last log lines:\n{}",
                    tail(&logs(tool, name), 15)
                );
            }
        }
        if start.elapsed() > STARTUP_TIMEOUT {
            // An unverified server must not be left holding the port.
            let _ = Command::new(tool).args(["stop", name]).output();
            bail!(
                "HITL server '{name}' did not report ready within {}s; stopped it, container kept for `{tool} logs {name}`. Last log lines:\n{}",
                STARTUP_TIMEOUT.as_secs(),
                tail(&logs(tool, name), 15)
            );
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// The addresses a device could use to reach this host. Best effort; the
/// hint is a starting point, not a promise about routing.
fn host_addresses() -> Vec<String> {
    let out = Command::new("ip")
        .args(["-4", "-o", "addr", "show", "scope", "global"])
        .output();
    let mut v = Vec::new();
    if let Ok(o) = out {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            // "2: eno2    inet 10.10.0.10/24 brd ..."
            let mut it = line.split_whitespace();
            let dev = it.nth(1).unwrap_or("");
            if dev.starts_with("docker") || dev.starts_with("br-") || dev.starts_with("veth") {
                continue;
            }
            if let Some(cidr) = it.find(|t| t.contains('.') && t.contains('/')) {
                v.push(cidr.split('/').next().unwrap_or(cidr).to_string());
            }
        }
    }
    if v.is_empty() {
        // No `ip` (macOS, Windows): the address the default route would send
        // from. connect() on UDP sends nothing; it only picks the local end.
        // TEST-NET-2 is unicast and never local, so the route is the default.
        let outbound = std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| s.connect("198.51.100.1:1").map(|_| s))
            .and_then(|s| s.local_addr());
        if let Ok(addr) = outbound {
            v.push(addr.ip().to_string());
        }
    }
    v
}

fn print_connect_hint(port: u16, extensions: &[String]) {
    let addrs = host_addresses();
    let ip = addrs
        .first()
        .cloned()
        .unwrap_or_else(|| "<this-host-ip>".to_string());
    let exts: Vec<String> = extensions.iter().map(|e| format!("-e {e}")).collect();
    print_info("On the device:", OutputLevel::Normal);
    println!(
        "    avocadoctl hitl mount -s {ip} -p {port} {}",
        exts.join(" ")
    );
    if addrs.len() > 1 {
        println!(
            "    (other addresses on this host: {})",
            addrs[1..].join(", ")
        );
    }
}

// ── status / stop / logs / sync ─────────────────────────────────────────────

/// One line per HITL server on this machine, any project.
pub fn status(tool: &str) -> Result<()> {
    let text = list_servers(
        tool,
        "{{.Names}}\t{{.Label \"avocado.target\"}}\t{{.Label \"avocado.port\"}}\t{{.Label \"avocado.extensions\"}}\t{{.Status}}\t{{.Label \"avocado.project\"}}",
    )?;
    if text.trim().is_empty() {
        print_info(
            "No HITL servers. Start one with `avocado hitl start -e <extension>`.",
            OutputLevel::Normal,
        );
        return Ok(());
    }
    println!(
        "{:<38} {:<12} {:<6} {:<28} {:<22} PROJECT",
        "NAME", "TARGET", "PORT", "EXTENSIONS", "STATUS"
    );
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() >= 6 {
            println!(
                "{:<38} {:<12} {:<6} {:<28} {:<22} {}",
                f[0], f[1], f[2], f[3], f[4], f[5]
            );
        }
    }
    Ok(())
}

/// Stop and remove this project's server, or every HITL server with `all`.
pub fn stop(tool: &str, identity: Option<&HitlIdentity>, all: bool) -> Result<()> {
    let names: Vec<String> = if all {
        list_servers(tool, "{{.Names}}")?
            .lines()
            .map(str::to_string)
            .collect()
    } else {
        let id = identity.context("no project identity and --all not given")?;
        let name = id.container_name();
        match container_state(tool, &name)? {
            Some(_) => vec![name],
            None => {
                print_info(
                    &format!("No HITL server for this project and target ({name})."),
                    OutputLevel::Normal,
                );
                return Ok(());
            }
        }
    };
    if names.is_empty() {
        print_info("No HITL servers to stop.", OutputLevel::Normal);
        return Ok(());
    }
    let mut failed = Vec::new();
    for name in names {
        let out = Command::new(tool)
            .args(["rm", "-f", &name])
            .output()
            .with_context(|| format!("running {tool} rm"))?;
        // Gone between `ps` and `rm` is the state that was asked for.
        if out.status.success() || is_no_such(&String::from_utf8_lossy(&out.stderr)) {
            print_success(&format!("Stopped and removed {name}"), OutputLevel::Normal);
        } else {
            print_error(
                &format!(
                    "Could not remove {name}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
                OutputLevel::Normal,
            );
            failed.push(name);
        }
    }
    if !failed.is_empty() {
        bail!(
            "{} HITL server(s) still present: {}",
            failed.len(),
            failed.join(", ")
        );
    }
    Ok(())
}

/// The server log, optionally followed.
pub fn show_logs(tool: &str, identity: &HitlIdentity, follow: bool) -> Result<()> {
    let name = identity.container_name();
    if container_state(tool, &name)?.is_none() {
        bail!("No HITL server for this project and target ({name}).");
    }
    let mut args = vec!["logs"];
    if follow {
        args.push("-f");
    }
    args.push(&name);
    let status = Command::new(tool).args(&args).status()?;
    if !status.success() {
        bail!("{tool} logs exited with {status}");
    }
    Ok(())
}

/// Tell a device the content it is serving has changed: re-run the extension
/// lifecycle there. Not a cache operation -- NFSv4 already shows the device
/// new and changed files -- but `on_merge`, `enable_services` and depmod only
/// run on a merge, and a rebuilt extension needs them again.
pub fn sync(device: &str) -> Result<()> {
    print_info(
        &format!("Refreshing extensions on {device} (avocadoctl ext refresh)..."),
        OutputLevel::Normal,
    );
    let status = Command::new("ssh")
        .args([
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            "LogLevel=ERROR",
            "-o",
            "ConnectTimeout=10",
            "--",
            device,
            "avocadoctl ext refresh",
        ])
        .status()
        .with_context(|| "running ssh")?;
    if !status.success() {
        bail!("refresh on {device} failed ({status})");
    }
    print_success(
        &format!("Extensions refreshed on {device}."),
        OutputLevel::Normal,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(extensions: &[&str]) -> HitlServerCommand {
        HitlServerCommand {
            config_path: "avocado.yaml".into(),
            extensions: extensions.iter().map(|s| s.to_string()).collect(),
            container_args: None,
            dnf_args: None,
            target: Some("rb3gen2".into()),
            verbose: false,
            port: None,
            no_stamps: true,
            foreground: false,
            sdk_arch: None,
            composed_config: None,
        }
    }

    #[test]
    fn export_path_is_resolved_not_symlinked() {
        let sh = cmd(&["vmm"]).generate_export_setup_commands();
        // ganesha refuses a symlinked export root; the shell resolves it first
        assert!(sh.contains("Path = $(readlink -f ${AVOCADO_EXT_SYSROOTS}/vmm)"));
        assert!(sh.contains("Pseudo = /vmm"));
        // a missing sysroot is an error before ganesha starts, not a CRIT in its log
        assert!(sh.contains("does not exist -- run avocado ext install/build vmm first"));
        // stale export files from an earlier run do not linger
        assert!(sh.contains("rm -f ${AVOCADO_SDK_PREFIX}/etc/avocado/exports.d/*.conf"));
    }

    /// The failure observed on hardware: initialised, and useless.
    #[test]
    fn startup_with_a_failed_export_is_not_success() {
        let log = "\
14/09/2026 11:12:51 : epoch 6aa7d6b3 : riptide : nfs-ganesha-7[main] posix2fsal_error :FSAL :CRIT :Default case mapping Too many levels of symbolic links (40) to ERR_FSAL_SERVERFAULT
14/09/2026 11:12:51 : epoch 6aa7d6b3 : riptide : nfs-ganesha-7[main] init_export_root :EXPORT :CRIT :Lookup failed on path, ExportId=1 Path=/opt/_avocado/rb3gen2/extensions/vmm FSAL_ERROR=(Undefined server error,40)
14/09/2026 11:12:51 : epoch 6aa7d6b3 : riptide : nfs-ganesha-7[main] nfs_start :NFS STARTUP :EVENT :      NFS SERVER INITIALIZED
";
        let verdict = judge_startup(log).expect("decided");
        let err = verdict.unwrap_err().to_string();
        assert!(err.contains("Lookup failed on path, ExportId=1"), "{err}");
        assert!(err.contains("Too many levels of symbolic links"), "{err}");
    }

    /// A CRIT after the ready line is a runtime event, not a failed export.
    #[test]
    fn a_crit_after_initialised_does_not_fail_startup() {
        let log = "x :NFS STARTUP :EVENT :      NFS SERVER INITIALIZED\n\
                   y :DISP :CRIT :client 10.0.0.9 went away\n";
        assert!(judge_startup(log).unwrap().is_ok());
    }

    /// The name lands in `bash -c`, a file name and ganesha's config.
    #[test]
    fn extension_names_that_could_alter_the_setup_shell_are_rejected() {
        for bad in ["vmm; rm -rf /", "$(id)", "a b", "../etc", "\"x\"", "a,b"] {
            assert!(cmd(&[bad]).validate_extension_names().is_err(), "{bad}");
        }
        assert!(cmd(&["vm-alpha", "vmm"]).validate_extension_names().is_ok());
    }

    #[test]
    fn startup_is_undecided_until_ganesha_speaks_and_ok_when_clean() {
        assert!(
            judge_startup("nfs-ganesha-7[main] nfs_Init :NFS STARTUP :EVENT :starting").is_none()
        );
        assert!(
            judge_startup("x :NFS STARTUP :EVENT :      NFS SERVER INITIALIZED")
                .unwrap()
                .is_ok()
        );
    }

    #[test]
    fn inspect_stderr_distinguishes_absent_from_broken() {
        assert!(is_no_such("Error: No such object: avocado-hitl-x\n"));
        assert!(is_no_such("Error: no such container\n"));
        assert!(!is_no_such(
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock\n"
        ));
    }

    #[test]
    fn running_server_labels_parse_back_to_what_start_was_given() {
        assert_eq!(
            parse_port_and_extensions("12049\tvmm,vm-alpha\n"),
            Some((12049, vec!["vmm".to_string(), "vm-alpha".to_string()]))
        );
        assert_eq!(parse_port_and_extensions("2049\t\n"), Some((2049, vec![])));
        assert_eq!(parse_port_and_extensions("\n"), None);
        assert_eq!(parse_port_and_extensions("nope\tvmm\n"), None);
    }

    #[test]
    fn container_name_is_stable_per_project_and_distinct_across_them() {
        let a = HitlIdentity {
            target: "rb3gen2".into(),
            project_dir: "/p/one".into(),
        };
        let b = HitlIdentity {
            target: "rb3gen2".into(),
            project_dir: "/p/two".into(),
        };
        let c = HitlIdentity {
            target: "rubikpi3".into(),
            project_dir: "/p/one".into(),
        };
        assert_eq!(a.container_name(), a.container_name());
        assert_ne!(a.container_name(), b.container_name());
        assert_ne!(a.container_name(), c.container_name());
        assert!(a.container_name().starts_with("avocado-hitl-rb3gen2-"));
    }

    #[test]
    fn labels_carry_what_status_needs() {
        let id = HitlIdentity {
            target: "rb3gen2".into(),
            project_dir: "/p".into(),
        };
        let l = id
            .labels(12049, &["vmm".into(), "vm-alpha".into()])
            .join(" ");
        assert!(l.contains("avocado.role=hitl"));
        assert!(l.contains("avocado.target=rb3gen2"));
        assert!(l.contains("avocado.port=12049"));
        assert!(l.contains("avocado.extensions=vmm,vm-alpha"));
    }
}
