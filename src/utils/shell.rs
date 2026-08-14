//! Shell quoting helpers.
//!
//! Several commands splice values into a shell script that runs inside the SDK
//! container. Anything user-supplied has to be quoted on the way in or the
//! container shell re-splits and expands it.

/// Shell escape a string for safe use in a shell command
pub(crate) fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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
}
