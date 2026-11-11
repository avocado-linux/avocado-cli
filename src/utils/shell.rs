//! Shell quoting helpers.
//!
//! Several commands splice values into a shell script that runs inside the SDK
//! container. Anything user-supplied has to be quoted on the way in or the
//! container shell re-splits and expands it.

/// Shell escape a string for safe use in a shell command
pub(crate) fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Escape a string for use inside a double-quoted shell word.
///
/// Inside double quotes only `\`, `"`, `$` and the backtick are special, so a
/// value with none of them comes back unchanged. The caller supplies the
/// surrounding quotes.
pub(crate) fn shell_escape_double_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '"' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn test_shell_escape_with_spaces() {
        assert_eq!(shell_escape("hello world"), "'hello world'");
    }

    #[test]
    fn test_shell_escape_with_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_shell_escape_complex() {
        assert_eq!(
            shell_escape("echo 'hello' && rm -rf /"),
            "'echo '\\''hello'\\'' && rm -rf /'"
        );
    }

    #[test]
    fn test_shell_escape_double_quoted_plain_is_unchanged() {
        assert_eq!(
            shell_escape_double_quoted("modprobe nvidia NVreg_X=1 'a b'"),
            "modprobe nvidia NVreg_X=1 'a b'"
        );
    }

    #[test]
    fn test_shell_escape_double_quoted_specials() {
        assert_eq!(
            shell_escape_double_quoted(r#"a"b$c`d\e"#),
            r#"a\"b\$c\`d\\e"#
        );
    }

    /// The escaped value round-trips through a real shell unchanged.
    #[cfg(unix)]
    #[test]
    fn test_shell_escape_double_quoted_round_trips_through_sh() {
        let value = r#"x "q" $(id) `id` \n $HOME 'single' ${X:-y}"#;
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                r#"printf '%s' "{}""#,
                shell_escape_double_quoted(value)
            ))
            .output()
            .expect("run sh");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), value);
    }
}
