use std::env;
use std::process::Command;

fn main() {
    // This runs only during build
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .unwrap();
    let git_hash = String::from_utf8(output.stdout).unwrap();
    println!(
        "cargo:rustc-env=AVOCADO_CLI_VERSION={} {}",
        env!("CARGO_PKG_VERSION"),
        git_hash
    );
    println!("cargo:rustc-env=TARGET={}", env::var("TARGET").unwrap());
}
