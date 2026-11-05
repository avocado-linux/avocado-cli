//! `ext install` and `ext build` must agree on which runtime's sysroot they use.
//!
//! `ext build` resolves a runtime (the sole one counts, including the implicit
//! `default` a config without a `runtimes:` block gets) and requires that
//! runtime's extension sysroot to hold a package database. When `ext install`
//! ignored the runtime, a standalone `ext install` then `ext build` failed with
//! "its sysroot for runtime 'default' has no package database", which is the
//! sequence extension CI runs. Running either command for real needs a container
//! runtime and an SDK, so these tests pin the CLI surface that the fix added.

use std::process::Command;

fn help(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_avocado"))
        .args(args)
        .arg("--help")
        .output()
        .expect("failed to run avocado --help");
    assert!(
        output.status.success(),
        "`avocado {} --help` exited with {:?}",
        args.join(" "),
        output.status.code()
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn ext_install_accepts_a_runtime_like_ext_build() {
    let install = help(&["ext", "install"]);

    assert!(
        install.contains("-r, --runtime <RUNTIME>"),
        "`ext install` has no -r/--runtime, so it cannot target the runtime `ext build` checks:\n{install}"
    );
}

#[test]
fn ext_build_still_accepts_a_runtime() {
    let build = help(&["ext", "build"]);

    assert!(
        build.contains("--runtime <RUNTIME>"),
        "`ext build` lost --runtime:\n{build}"
    );
}
