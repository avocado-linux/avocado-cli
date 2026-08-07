//! Whether this process can offer a human at the keyboard — and what that
//! means for the stdio flags handed to a container.
//!
//! # The trap this exists to close
//!
//! Every command that runs a container builds a `RunConfig` and writes
//! `interactive: <bool>` by hand. There are ~100 such sites. That field is a
//! statement of *intent* ("this command wants to prompt"), but it was being
//! used directly as a *capability* ("a terminal is attached"), so any command
//! declaring intent became unusable wherever no terminal exists — agents, CI,
//! pipes, the desktop app driver. Docker refuses outright:
//!
//! ```text
//! cannot attach stdin to a TTY-enabled container because stdin is not a terminal
//! ```
//!
//! The output layer already gets this right: [`crate::utils::output::should_use_tui`]
//! ANDs intent (no explicit opt-out) with capability (`stderr().is_terminal()`,
//! not CI). Container stdio simply never asked the same question — and it has
//! to ask it of **stdin**, not stderr. That asymmetry is the whole bug.
//!
//! # The shape of the fix
//!
//! Intent stays where it is; call sites are unchanged. Capability is detected
//! once, here, and the two are combined by one pure function whose entire
//! truth table is tested. A new command can set `interactive: true` and still
//! work headless, because the runner downgrades instead of failing.
//!
//! Adding a command that runs a container? Say what it *wants* and let this
//! decide what it *gets*. Never hand-roll `-i` / `-t`.

use std::io::IsTerminal;

/// What the environment can actually support, detected once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdioCapabilities {
    /// A real terminal is attached to stdin. The one signal the old code
    /// never consulted.
    pub stdin_is_tty: bool,
    /// NDJSON event stream — the desktop app driver, not a person. Its stdin
    /// is a pipe, and a dnf prompt would deadlock against it.
    pub json_active: bool,
    /// A TUI renderer owns the terminal; container output is piped to it.
    pub tui_active: bool,
    /// Explicit opt-out for scripted and agent-driven runs, independent of
    /// whether a tty happens to be attached.
    pub forced_noninteractive: bool,
}

impl StdioCapabilities {
    pub fn detect() -> Self {
        Self {
            stdin_is_tty: std::io::stdin().is_terminal(),
            json_active: crate::utils::output_format::is_json_output_active(),
            tui_active: crate::utils::tui::get_active_renderer().is_some(),
            // `CI` is deliberately *not* consulted here. `should_use_tui`
            // checks it to avoid painting a TUI into a build log, but a CI job
            // may still legitimately pipe answers to a prompt. The tty check
            // already covers the case that matters.
            forced_noninteractive: std::env::var("AVOCADO_NONINTERACTIVE").is_ok(),
        }
    }
}

/// Whether this process can meaningfully prompt the user *itself* — as
/// opposed to handing stdio to a container.
///
/// Host-side prompts fail differently and more quietly than the container
/// case. `stdin().read_line()` at EOF returns `Ok(0)` and leaves the buffer
/// empty: no error, no signal, just an empty answer. A confirmation that
/// treats empty as "no" degrades safely, but a `loop { read_line; complain }`
/// spins forever printing to a log nobody is reading.
///
/// Anything that blocks on the user must check this first and take a defined
/// path when it is false — usually erroring with a flag to pass instead.
///
/// Currently consulted only from the macOS-only disk writer, hence the
/// targeted allow: it is genuinely unused elsewhere today, and a blanket
/// `allow(dead_code)` would hide it going unused on macOS too.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn can_prompt_user() -> bool {
    let caps = StdioCapabilities::detect();
    caps.stdin_is_tty && !caps.json_active && !caps.forced_noninteractive
}

/// The stdio arrangement a container should be given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerStdio {
    /// Neither `-i` nor `-t`: output is piped and captured, nothing will be
    /// read from stdin.
    Piped,
    /// `-i` only: stdin stays open so prompts can be answered from a pipe or
    /// heredoc, but no PTY is allocated. dnf prompts still work here — they
    /// just lose the progress-bar rendering.
    StdinOnly,
    /// `-i -t`: a real person at a real terminal.
    StdinAndTty,
}

impl ContainerStdio {
    /// Combine what the command wants with what the environment can give.
    ///
    /// `wants_interactive` is the caller's intent — `RunConfig::interactive`.
    ///
    /// The only way to reach [`ContainerStdio::StdinAndTty`] is for the caller
    /// to want it *and* for stdin to actually be a terminal. Everything else
    /// degrades rather than failing, which is what makes the command usable
    /// headless.
    pub fn decide(wants_interactive: bool, caps: &StdioCapabilities) -> Self {
        // JSON mode: a machine is driving. Prose and prompts both break the
        // event stream, and a prompt would block forever on a pipe nobody is
        // writing to.
        if caps.json_active {
            return Self::Piped;
        }

        if wants_interactive && !caps.forced_noninteractive {
            return if caps.stdin_is_tty {
                Self::StdinAndTty
            } else {
                // Wanted a terminal, hasn't got one. Keep stdin open so a
                // piped answer still lands; do NOT ask for a PTY, which is
                // the error Docker refuses on.
                Self::StdinOnly
            };
        }

        // Non-interactive commands: a TUI owns the terminal, so its output is
        // captured; otherwise leave stdin open for the occasional prompt.
        if caps.tui_active {
            Self::Piped
        } else {
            Self::StdinOnly
        }
    }

    /// Whether a PTY is allocated. Prefer this to inspecting [`Self::flags`]
    /// so callers and tests never have to spell `-t` themselves — which also
    /// keeps them clear of the source guard that forbids those literals.
    ///
    /// Consumed by tests today; `allow(dead_code)` matches how the crate
    /// already handles lib-public items the binary happens not to call.
    #[allow(dead_code)]
    pub fn allocates_tty(&self) -> bool {
        matches!(self, Self::StdinAndTty)
    }

    /// Whether stdin is left open, with or without a PTY.
    #[allow(dead_code)]
    pub fn keeps_stdin(&self) -> bool {
        matches!(self, Self::StdinOnly | Self::StdinAndTty)
    }

    /// The docker/podman flags this arrangement implies.
    ///
    /// The single place `-i` and `-t` are spelled. Anything else pushing them
    /// onto a container command line is a bug.
    pub fn flags(&self) -> &'static [&'static str] {
        match self {
            Self::Piped => &[],
            Self::StdinOnly => &["-i"],
            Self::StdinAndTty => &["-i", "-t"],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every capability combination, so the matrix is documented and locked
    /// rather than living in one branch of a 400-line function.
    fn caps(
        stdin_is_tty: bool,
        json_active: bool,
        tui_active: bool,
        forced: bool,
    ) -> StdioCapabilities {
        StdioCapabilities {
            stdin_is_tty,
            json_active,
            tui_active,
            forced_noninteractive: forced,
        }
    }

    #[test]
    fn interactive_with_a_terminal_gets_a_pty() {
        let c = caps(true, false, false, false);
        assert_eq!(
            ContainerStdio::decide(true, &c),
            ContainerStdio::StdinAndTty
        );
        assert_eq!(ContainerStdio::decide(true, &c).flags(), &["-i", "-t"]);
    }

    #[test]
    fn interactive_without_a_terminal_degrades_instead_of_failing() {
        // The regression this module exists for: an agent, a pipe, or a CI
        // job running a command that declares `interactive: true`. Docker
        // rejects `-i -t` here, so we must not ask for it — but stdin stays
        // open so a piped answer still works.
        let c = caps(false, false, false, false);
        assert_eq!(ContainerStdio::decide(true, &c), ContainerStdio::StdinOnly);
        assert_eq!(ContainerStdio::decide(true, &c).flags(), &["-i"]);
    }

    #[test]
    fn json_mode_never_takes_stdin_even_when_interactive_is_wanted() {
        // A prompt would deadlock against the driver's pipe.
        for stdin_tty in [true, false] {
            let c = caps(stdin_tty, true, false, false);
            assert_eq!(ContainerStdio::decide(true, &c), ContainerStdio::Piped);
            assert_eq!(ContainerStdio::decide(false, &c), ContainerStdio::Piped);
        }
    }

    #[test]
    fn forced_noninteractive_suppresses_the_pty_even_on_a_terminal() {
        let c = caps(true, false, false, true);
        assert_eq!(ContainerStdio::decide(true, &c), ContainerStdio::StdinOnly);
    }

    #[test]
    fn tui_mode_pipes_noninteractive_commands() {
        let c = caps(true, false, true, false);
        assert_eq!(ContainerStdio::decide(false, &c), ContainerStdio::Piped);
    }

    #[test]
    fn tui_mode_still_yields_to_an_explicitly_interactive_command() {
        // `avocado shell` under a TUI still needs the terminal.
        let c = caps(true, false, true, false);
        assert_eq!(
            ContainerStdio::decide(true, &c),
            ContainerStdio::StdinAndTty
        );
    }

    #[test]
    fn noninteractive_without_tui_keeps_stdin_open_for_prompts() {
        let c = caps(false, false, false, false);
        assert_eq!(ContainerStdio::decide(false, &c), ContainerStdio::StdinOnly);
    }

    #[test]
    fn a_pty_requires_both_intent_and_capability() {
        // Exhaustive: the only route to StdinAndTty is (wants && stdin_tty &&
        // !json && !forced). Anything else must degrade.
        for wants in [true, false] {
            for stdin_tty in [true, false] {
                for json in [true, false] {
                    for tui in [true, false] {
                        for forced in [true, false] {
                            let c = caps(stdin_tty, json, tui, forced);
                            let got = ContainerStdio::decide(wants, &c);
                            let expected_pty = wants && stdin_tty && !json && !forced;
                            assert_eq!(
                                got == ContainerStdio::StdinAndTty,
                                expected_pty,
                                "wants={wants} stdin_tty={stdin_tty} json={json} \
                                 tui={tui} forced={forced} -> {got:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    // ---- the environment bridge ----------------------------------------
    //
    // `detect`, `for_intent`, and `can_prompt_user` read the real process
    // environment, which is exactly where the original bug lived — the pure
    // decision was never wrong, it simply was not asked about stdin. These
    // assert invariants that hold whether the suite runs from a terminal or
    // from CI, rather than hard-coding either, so they cannot pass in one
    // place and fail in the other.

    #[test]
    fn for_intent_actually_consults_stdin() {
        let caps = StdioCapabilities::detect();
        let got = ContainerStdio::decide(true, &StdioCapabilities::detect());
        let expect_pty = caps.stdin_is_tty && !caps.json_active && !caps.forced_noninteractive;
        assert_eq!(
            got == ContainerStdio::StdinAndTty,
            expect_pty,
            "for_intent disagreed with detected capabilities {caps:?} -> {got:?}"
        );
    }

    #[test]
    fn a_noninteractive_command_never_gets_a_pty_in_any_environment() {
        assert_ne!(
            ContainerStdio::decide(false, &StdioCapabilities::detect()),
            ContainerStdio::StdinAndTty
        );
    }

    #[test]
    fn can_prompt_user_agrees_with_the_container_decision() {
        // If we would give a container a PTY, we can also prompt ourselves,
        // and vice versa. Divergence would mean the two halves of the module
        // disagree about what "interactive" means.
        assert_eq!(
            can_prompt_user(),
            ContainerStdio::decide(true, &StdioCapabilities::detect())
                == ContainerStdio::StdinAndTty
        );
    }

    #[test]
    #[serial_test::serial]
    fn forced_noninteractive_env_var_is_honored_end_to_end() {
        let prev = std::env::var("AVOCADO_NONINTERACTIVE").ok();

        std::env::set_var("AVOCADO_NONINTERACTIVE", "1");
        assert!(StdioCapabilities::detect().forced_noninteractive);
        assert_ne!(
            ContainerStdio::decide(true, &StdioCapabilities::detect()),
            ContainerStdio::StdinAndTty,
            "AVOCADO_NONINTERACTIVE must suppress the PTY even on a terminal"
        );
        assert!(!can_prompt_user());

        std::env::remove_var("AVOCADO_NONINTERACTIVE");
        assert!(!StdioCapabilities::detect().forced_noninteractive);

        if let Some(v) = prev {
            std::env::set_var("AVOCADO_NONINTERACTIVE", v);
        }
    }

    #[test]
    fn no_arrangement_asks_for_a_tty_without_stdin() {
        // `-t` without `-i` is never valid for our use; assert the flag sets
        // stay well-formed.
        for s in [
            ContainerStdio::Piped,
            ContainerStdio::StdinOnly,
            ContainerStdio::StdinAndTty,
        ] {
            let f = s.flags();
            if f.contains(&"-t") {
                assert!(f.contains(&"-i"), "{s:?} asks for -t without -i");
            }
        }
    }
}
