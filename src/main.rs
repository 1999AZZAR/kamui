mod chat;
mod compaction;
mod config;
mod context;
mod mcp;
mod onboarding;
mod prompt;
mod provider;
mod storage;
mod tools;

use anyhow::{Context, Result};
use config::Config;
use context::ProjectContext;
use provider::Provider;
use provider::openai::OpenAIProvider;
use storage::Database;

#[tokio::main]
async fn main() -> Result<()> {
    let command = match parse_command()? {
        Command::Help => {
            print_help();
            return Ok(());
        }
        Command::Version => {
            println!("kamui {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Command::Doctor => {
            return run_doctor().await;
        }
        command => command,
    };

    let config = match Config::load()? {
        config::Loaded::Ready(config) => config,
        config::Loaded::NeedsSetup(path) => {
            onboarding::run(&path).await?;
            match Config::load()? {
                config::Loaded::Ready(config) => config,
                config::Loaded::NeedsSetup(_) => {
                    anyhow::bail!("configuration is still incomplete after onboarding")
                }
            }
        }
    };
    let database = Database::open()?;
    let project = ProjectContext::discover()?;

    // Connect MCP servers before the chat starts so their tools are offered from the first turn.
    let mcp = mcp::connect_all(&config.mcp_servers).await;
    let tools = tools::ToolRegistry::with_defaults(project.root().to_path_buf(), mcp.tools);
    let build_provider = |profile: &config::Profile| {
        Box::new(OpenAIProvider::new(
            profile.api_key.clone(),
            profile.base_url.clone(),
        )) as Box<dyn Provider>
    };

    match command {
        Command::Chat(resume_id) => {
            chat::start_chat(
                config,
                tools,
                mcp.statuses,
                &database,
                &project,
                resume_id,
                build_provider,
            )
            .await?;
        }
        Command::Once {
            prompt,
            auto_approve,
        } => {
            chat::run_once(
                config,
                tools,
                &database,
                &project,
                &prompt,
                auto_approve,
                build_provider,
            )
            .await?;
        }
        Command::Help | Command::Version | Command::Doctor => unreachable!("handled above"),
    }

    Ok(())
}

enum Command {
    Chat(Option<String>),
    Once { prompt: String, auto_approve: bool },
    Doctor,
    Help,
    Version,
}

fn parse_command() -> Result<Command> {
    parse_command_from(std::env::args().skip(1))
}

fn parse_command_from(arguments: impl Iterator<Item = String>) -> Result<Command> {
    let mut arguments = arguments;
    match arguments.next().as_deref() {
        None => Ok(Command::Chat(None)),
        Some("-r" | "--resume") => {
            let id = arguments.next().context("usage: kamui -r <session-id>")?;
            if arguments.next().is_some() {
                anyhow::bail!("usage: kamui -r <session-id>");
            }
            Ok(Command::Chat(Some(id)))
        }
        Some("-p" | "--print") => {
            let prompt = arguments
                .next()
                .context("usage: kamui -p <prompt> [--auto-approve]")?;
            let mut auto_approve = false;
            for rest in arguments {
                match rest.as_str() {
                    "--auto-approve" => auto_approve = true,
                    _ => anyhow::bail!("usage: kamui -p <prompt> [--auto-approve]"),
                }
            }
            Ok(Command::Once {
                prompt,
                auto_approve,
            })
        }
        Some("doctor") => Ok(Command::Doctor),
        Some("-h" | "--help") => Ok(Command::Help),
        Some("-V" | "--version") => Ok(Command::Version),
        Some(_) => anyhow::bail!("usage: kamui [-r <session-id>] [-p <prompt> [--auto-approve]]"),
    }
}

/// `kamui doctor`: check configuration, provider connectivity, and MCP servers one at a time,
/// printing a pass/fail line for each with actionable guidance on failure, rather than crashing on
/// the first problem the way starting a normal chat would. Exits with an error if anything failed,
/// so it is usable as a pre-flight check in a script.
async fn run_doctor() -> Result<()> {
    println!("Kamui doctor");
    println!();
    let mut failures = 0usize;

    let config = match Config::load() {
        Ok(config::Loaded::Ready(config)) => {
            check_ok("Config file parses and is complete");
            Some(config)
        }
        Ok(config::Loaded::NeedsSetup(path)) => {
            check_fail(&format!(
                "Config at {} is incomplete — run `kamui` once to finish onboarding",
                path.display()
            ));
            failures += 1;
            None
        }
        Err(error) => {
            check_fail(&format!("Config file is invalid: {error:#}"));
            failures += 1;
            None
        }
    };

    if let Some(config) = &config {
        let profile = config.default();
        check_ok(&format!(
            "Default profile '{}' uses model '{}'",
            profile.name, profile.model
        ));
        let provider = OpenAIProvider::new(profile.api_key.clone(), profile.base_url.clone());
        match provider
            .chat(provider::ChatRequest {
                model: profile.model.clone(),
                messages: vec![provider::Message::user("ping")],
                tools: Vec::new(),
            })
            .await
        {
            Ok(_) => check_ok("Provider responds to a test request"),
            Err(error) => {
                check_fail(&format!("Provider request failed: {error:#}"));
                failures += 1;
            }
        }

        if config.mcp_servers.is_empty() {
            check_ok("No MCP servers configured (nothing to check)");
        } else {
            let mcp = mcp::connect_all(&config.mcp_servers).await;
            for status in &mcp.statuses {
                match &status.error {
                    Some(error) => {
                        check_fail(&format!("MCP server '{}' failed: {error}", status.name));
                        failures += 1;
                    }
                    None => check_ok(&format!(
                        "MCP server '{}' connected ({} tool(s))",
                        status.name, status.tool_count
                    )),
                }
            }
        }
    }

    match Database::open() {
        Ok(_) => check_ok("Database opens successfully"),
        Err(error) => {
            check_fail(&format!("Database could not be opened: {error:#}"));
            failures += 1;
        }
    }

    println!();
    if failures == 0 {
        println!("All checks passed.");
        Ok(())
    } else {
        anyhow::bail!("{failures} check(s) failed");
    }
}

fn check_ok(message: &str) {
    println!("  \u{2713} {message}");
}

fn check_fail(message: &str) {
    println!("  \u{2717} {message}");
}

fn print_help() {
    println!("Kamui - provider-agnostic LLM chat CLI\n");
    println!("Usage: kamui [OPTIONS]\n");
    println!("Options:");
    println!("  -r, --resume <ID>   Resume a saved session");
    println!("  -p, --print <TEXT>  Run one prompt non-interactively and exit");
    println!("      --auto-approve  With -p, approve tool calls without prompting");
    println!("  doctor              Check configuration, provider, and MCP servers");
    println!("  -h, --help          Print help");
    println!("  -V, --version       Print version");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> impl Iterator<Item = String> {
        values
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn no_arguments_starts_a_new_chat() {
        assert!(matches!(
            parse_command_from(args(&[])).unwrap(),
            Command::Chat(None)
        ));
    }

    #[test]
    fn resume_flag_carries_the_session_id() {
        let command = parse_command_from(args(&["-r", "abc123"])).unwrap();
        assert!(matches!(command, Command::Chat(Some(id)) if id == "abc123"));
    }

    #[test]
    fn resume_flag_rejects_trailing_arguments() {
        assert!(parse_command_from(args(&["-r", "abc123", "extra"])).is_err());
    }

    #[test]
    fn print_flag_carries_the_prompt_without_auto_approve() {
        let command = parse_command_from(args(&["-p", "hello there"])).unwrap();
        match command {
            Command::Once {
                prompt,
                auto_approve,
            } => {
                assert_eq!(prompt, "hello there");
                assert!(!auto_approve);
            }
            _ => panic!("expected Command::Once"),
        }
    }

    #[test]
    fn print_flag_accepts_the_auto_approve_flag() {
        let command = parse_command_from(args(&["--print", "do it", "--auto-approve"])).unwrap();
        match command {
            Command::Once {
                prompt,
                auto_approve,
            } => {
                assert_eq!(prompt, "do it");
                assert!(auto_approve);
            }
            _ => panic!("expected Command::Once"),
        }
    }

    #[test]
    fn print_flag_rejects_unknown_trailing_arguments() {
        assert!(parse_command_from(args(&["-p", "hi", "--bogus"])).is_err());
    }

    #[test]
    fn print_flag_requires_a_prompt() {
        assert!(parse_command_from(args(&["-p"])).is_err());
    }

    #[test]
    fn help_and_version_flags_are_recognized() {
        assert!(matches!(
            parse_command_from(args(&["-h"])).unwrap(),
            Command::Help
        ));
        assert!(matches!(
            parse_command_from(args(&["--help"])).unwrap(),
            Command::Help
        ));
        assert!(matches!(
            parse_command_from(args(&["-V"])).unwrap(),
            Command::Version
        ));
        assert!(matches!(
            parse_command_from(args(&["--version"])).unwrap(),
            Command::Version
        ));
    }

    #[test]
    fn unknown_flag_is_rejected() {
        assert!(parse_command_from(args(&["--bogus"])).is_err());
    }

    #[test]
    fn doctor_flag_is_recognized() {
        assert!(matches!(
            parse_command_from(args(&["doctor"])).unwrap(),
            Command::Doctor
        ));
    }
}
