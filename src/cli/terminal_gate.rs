use clap::{CommandFactory, error::ErrorKind};

use super::{Cli, Command};

/// Reject bare TUI commands unless both standard streams are terminals.
///
/// The terminal state is injected so command selection remains a pure,
/// deterministic check. The process entry point owns the actual terminal
/// detection and handles the returned Clap error before logging or TUI setup.
pub fn validate_bare_command_terminal(
    command: Option<&Command>,
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
) -> Result<(), clap::Error> {
    let (display_name, clap_name) = match command {
        Some(Command::Provider {
            select: None,
            command: None,
        }) => ("provider", "provider"),
        Some(Command::Provider {
            select: Some(_),
            command: None,
        }) => ("provider --select", "provider"),
        Some(Command::Model {
            select: None,
            command: None,
        }) => ("model", "model"),
        Some(Command::Model {
            select: Some(_),
            command: None,
        }) => ("model --select", "model"),
        _ => return Ok(()),
    };

    if stdin_is_terminal && stdout_is_terminal {
        return Ok(());
    }

    let mut root = Cli::command().color(clap::ColorChoice::Never);
    let message = format!(
        "'{display_name}' requires interactive stdin and stdout to open the TUI; \
         use an explicit subcommand or 'llm-proxy {clap_name} --help'"
    );
    let subcommand = root
        .find_subcommand_mut(clap_name)
        .expect("bare TUI command must exist in the Clap command tree");

    Err(subcommand.error(ErrorKind::MissingSubcommand, message))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn parse_command(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("test command should parse")
    }

    #[test]
    fn bare_provider_and_model_require_both_terminals() {
        for args in [
            ["llm-proxy", "provider"].as_slice(),
            ["llm-proxy", "provider", "--select", "deepseek"].as_slice(),
            ["llm-proxy", "model"].as_slice(),
            ["llm-proxy", "model", "--select", "deepseek-v4-pro-lp"].as_slice(),
        ] {
            let cli = parse_command(args);

            for (stdin_is_terminal, stdout_is_terminal) in
                [(false, false), (false, true), (true, false)]
            {
                let error = validate_bare_command_terminal(
                    cli.command.as_ref(),
                    stdin_is_terminal,
                    stdout_is_terminal,
                )
                .expect_err("bare TUI command should require both terminals");
                let rendered = error.render().to_string();

                assert_eq!(error.exit_code(), 2);
                assert!(rendered.contains("Usage:"));
                assert!(rendered.contains("explicit subcommand"));
                assert!(!rendered.contains('\u{1b}'));
            }

            validate_bare_command_terminal(cli.command.as_ref(), true, true)
                .expect("both terminals should permit the bare TUI command");
        }
    }

    #[test]
    fn gate_does_not_change_other_command_forms() {
        for args in [
            ["llm-proxy"].as_slice(),
            ["llm-proxy", "connect"].as_slice(),
            ["llm-proxy", "provider", "list"].as_slice(),
            ["llm-proxy", "provider", "info", "deepseek"].as_slice(),
            ["llm-proxy", "model", "list"].as_slice(),
        ] {
            let cli = parse_command(args);
            validate_bare_command_terminal(cli.command.as_ref(), false, false)
                .unwrap_or_else(|error| panic!("{args:?} unexpectedly rejected: {error}"));
        }
    }
}
