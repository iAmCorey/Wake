//! Command Prompt command construction, kept independent of Win32 so its
//! rejection rules can be tested on every platform.

use crate::adapters::EnvVar;

/// cmd always needs double quotes here; callers must reject inputs containing
/// quotes, percent expansion or line/control delimiters before using this.
fn quote(s: &str) -> String {
    format!("\"{s}\"")
}

/// Unlike paths, database-backed session ids can contain `"`. It breaks out of
/// quotes; `%VAR%` expands even inside them. Reject instead of rewriting either
/// value, including in the clipboard fallback. Newlines and NUL are not valid
/// parts of the single command passed to cmd.exe either.
fn hostile(cli: &str, args: &[String], cwd: Option<&str>, env: &[EnvVar]) -> bool {
    std::iter::once(cli)
        .chain(args.iter().map(|s| s.as_str()))
        .chain(cwd)
        .chain(env.iter().map(|(_, value)| value.as_str()))
        .any(|s| s.contains(['%', '"', '\r', '\n', '\0']))
}

/// `call` keeps the first word unquoted, avoiding cmd's legacy `/K` rule that
/// strips the first and last quotes when the payload starts with a quote.
/// `/d` makes cd switch drives as well as directories. Environment variables
/// use the quoted `set "NAME=value"` form, which keeps `&` and `^` literal.
pub(super) fn command_line(
    cli: &str,
    args: &[String],
    cwd: Option<&str>,
    env: &[EnvVar],
) -> anyhow::Result<String> {
    if hostile(cli, args, cwd, env) {
        anyhow::bail!("path or argument contains a double quote, '%', or a control delimiter that Command Prompt cannot safely quote — pick another terminal");
    }
    let mut line = String::new();
    if let Some(dir) = cwd {
        line.push_str(&format!("cd /d {} && ", quote(dir)));
    }
    for (name, value) in env {
        line.push_str(&format!("set {} && ", quote(&format!("{name}={value}"))));
    }
    line.push_str("call ");
    line.push_str(&quote(cli));
    for arg in args {
        line.push(' ');
        line.push_str(&quote(arg));
    }
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::command_line;

    #[test]
    fn rejects_unsafe_inputs_before_producing_a_command() {
        let id = ["--resume".to_string(), "abc-123".to_string()];
        for value in [
            r#"x" & echo BREAKOUT_MARKER & rem"#,
            "a\"b",
            "%COMSPEC%",
            "a\rb",
            "a\nb",
            "a\0b",
        ] {
            assert!(command_line("claude", &["--resume".into(), value.into()], None, &[]).is_err());
            assert!(command_line(value, &id, None, &[]).is_err());
            assert!(command_line("claude", &[], Some(value), &[]).is_err());
            assert!(command_line("codex", &id, None, &[("CODEX_HOME", value.into())]).is_err());
        }
    }

    #[test]
    fn preserves_quoted_paths_and_arguments() {
        assert_eq!(
            command_line(
                r"C:\Program Files\agent.cmd",
                &["--resume".into(), "abc-123".into()],
                Some(r"D:\工作 & (test)^"),
                &[],
            )
            .unwrap(),
            r#"cd /d "D:\工作 & (test)^" && call "C:\Program Files\agent.cmd" "--resume" "abc-123""#
        );
        assert_eq!(
            command_line("copilot", &["--resume=abc-123".into()], None, &[]).unwrap(),
            r#"call "copilot" "--resume=abc-123""#
        );
    }

    /// Resume variables (#83) are set right before the call; `&` and `^` stay
    /// literal inside the quotes
    #[test]
    fn sets_resume_environment_before_the_call() {
        assert_eq!(
            command_line(
                "codex",
                &["resume".into(), "abc-123".into()],
                Some(r"D:\work"),
                &[("CODEX_HOME", r"C:\Users\me\.codex & api^".into())],
            )
            .unwrap(),
            r#"cd /d "D:\work" && set "CODEX_HOME=C:\Users\me\.codex & api^" && call "codex" "resume" "abc-123""#
        );
    }
}
