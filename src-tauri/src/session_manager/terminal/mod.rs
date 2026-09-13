//! Formatting helpers for session metadata. This module does not launch terminals.

/// Quote a path as a single POSIX shell argument for Pi's resume-command metadata.
/// Single quotes prevent expansion of `$`, backticks and command substitutions.
/// Embedded single quotes are represented by closing, escaping and reopening.
pub(crate) fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_escape_neutralizes_command_substitution_in_directory_names() {
        assert_eq!(shell_escape("/tmp/$(id -un)"), "'/tmp/$(id -un)'");
        assert_eq!(shell_escape("/tmp/`id -un`"), "'/tmp/`id -un`'");
        assert_eq!(shell_escape("/tmp/$HOME"), "'/tmp/$HOME'");
    }

    #[test]
    fn shell_escape_handles_embedded_single_quote() {
        assert_eq!(shell_escape("/tmp/it's"), r"'/tmp/it'\''s'");
    }

    #[test]
    fn shell_escape_preserves_spaces_and_empty_arguments() {
        assert_eq!(shell_escape("/tmp/project dir"), "'/tmp/project dir'");
        assert_eq!(shell_escape(""), "''");
    }
}
