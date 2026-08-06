//! Guard: container stdio flags are decided in exactly one place.
//!
//! `-i` / `-t` must come from `utils::interactivity::ContainerStdio`, which
//! combines what a command *wants* with what the environment can actually
//! provide. A site that pushes them directly has, by construction, skipped the
//! "is stdin even a terminal?" question — and that is the bug this guards:
//!
//! ```text
//! cannot attach stdin to a TTY-enabled container because stdin is not a terminal
//! ```
//!
//! Which makes the command unusable for agents, CI, pipes, and the desktop
//! app driver. Reviewing for this by eye does not scale across ~100
//! `RunConfig` construction sites, so it is checked here instead.
//!
//! If this fails: don't add an exemption. Call `ContainerStdio::for_intent`
//! (or `::decide`) and push `.flags()`.

use std::fs;
use std::path::{Path, PathBuf};

/// The one module allowed to spell these flags.
const SANCTIONED: &str = "interactivity.rs";

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Flag spellings that hand a PTY or stdin to a container.
fn offending_snippets(line: &str) -> Option<&'static str> {
    // Only flag string literals that *are* the switch, not prose mentioning
    // it, and not unrelated uses like `docker stop -t 2`.
    const NEEDLES: [&str; 3] = ["\"-it\"", "push(\"-t\"", "push(\"-i\""];
    NEEDLES.into_iter().find(|n| line.contains(n))
}

#[test]
fn container_stdio_flags_come_only_from_the_interactivity_module() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);
    assert!(
        !files.is_empty(),
        "found no sources under {}",
        src.display()
    );

    let mut violations = Vec::new();

    for file in files {
        if file.file_name().is_some_and(|n| n == SANCTIONED) {
            continue;
        }
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            // Comments and doc comments may legitimately discuss the flags.
            if trimmed.starts_with("//") {
                continue;
            }
            if let Some(needle) = offending_snippets(line) {
                violations.push(format!(
                    "{}:{}: {} — use ContainerStdio::for_intent(..).flags()",
                    file.display(),
                    i + 1,
                    needle
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "container stdio flags must be decided by utils::interactivity, \
         so that intent is combined with whether stdin is actually a \
         terminal. Offending sites:\n  {}",
        violations.join("\n  ")
    );
}
