//! Behavior: installing and fetching commands refuse an unreachable feed before
//! any Connect token is minted.
//!
//! Each command runs for real against a project whose only plain feed points at
//! a closed loopback port, beside a Connect feed (`org:`) with no credentials.
//! The probe names the unreachable feed. If the command minted first, the
//! Connect feed would fail instead with a feed-token error naming `repos.conn`.
//! `PATH` is an empty directory, so a regression cannot reach docker either.
//!
//! `feed_preflight_wiring.rs` keeps the source-order table for the commands this
//! file does not run.

use std::path::Path;
use std::process::Command;

const CONFIG: &str = "default_target: qemux86-64
distro:
  release: 2026
  feeds: [conn, local]
repos:
  conn:
    org: acme
  local:
    url: http://127.0.0.1:1/feed
sdk:
  image: test-image
extensions:
  app:
    version: '1.0.0'
    source:
      type: package
      version: '*'
";

fn run(args: &[&str], project: &Path, empty_path: &Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_avocado"))
        .args(args)
        .arg("-C")
        .arg(project.join("avocado.yaml"))
        .current_dir(project)
        .env_clear()
        .env("PATH", empty_path)
        .env("HOME", project)
        .output()
        .expect("failed to run avocado");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "`avocado {}` succeeded against an unreachable feed:\n{text}",
        args.join(" ")
    );
    text
}

#[test]
fn installing_and_fetching_commands_probe_before_the_connect_mint() {
    let project = tempfile::tempdir().unwrap();
    let empty_path = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("avocado.yaml"), CONFIG).unwrap();

    let commands: &[&[&str]] = &[
        &["ext", "fetch"],
        &["fetch"],
        &["ext", "install"],
        &["install"],
        &["sdk", "install"],
        &["runtime", "install"],
        &["rootfs", "install"],
        &["initramfs", "install"],
    ];
    for args in commands {
        let text = run(args, project.path(), empty_path.path());
        let name = args.join(" ");
        assert!(
            text.contains("feed 'local': cannot connect to http://127.0.0.1:1/feed"),
            "`avocado {name}` did not name the unreachable feed:\n{text}"
        );
        assert!(
            !text.contains("repos.conn") && !text.contains("feed-token"),
            "`avocado {name}` reached the Connect mint before the probe:\n{text}"
        );
    }
}
