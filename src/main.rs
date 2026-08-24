mod benchmark;
mod chat;
mod commands;
mod compaction;
mod config;
mod context;
mod jobs;
mod markdown;
mod mcp;
mod onboarding;
mod prompt;
mod provider;
mod render;
mod settings;
mod skills;
mod storage;
mod terminal;
mod tools;
mod tui;

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
        Command::Status => {
            return run_status().await;
        }
        Command::Jobs(command) => {
            return jobs::run(command).await;
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
    let command_limits = tools::CommandLimits {
        timeout: std::time::Duration::from_secs(config.command_timeout_secs),
        background_max: std::time::Duration::from_secs(config.background_max_secs),
    };
    let tools = tools::ToolRegistry::with_defaults(
        project.root().to_path_buf(),
        mcp.tools,
        config.allow_commands.clone(),
        command_limits,
    );
    let build_provider = |profile: &config::Profile| {
        Box::new(OpenAIProvider::new(
            profile.api_key.clone(),
            profile.base_url.clone(),
        )) as Box<dyn Provider>
    };

    match command {
        Command::Chat {
            resume_id,
            auto_approve,
        } => {
            chat::start_chat(
                config,
                tools,
                mcp.statuses,
                &database,
                &project,
                resume_id,
                auto_approve,
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
        Command::Benchmark {
            suite,
            profile,
            runs,
        } => {
            benchmark::run(&config, &suite, profile.as_deref(), runs, build_provider).await?;
        }
        Command::Help | Command::Version | Command::Doctor | Command::Status | Command::Jobs(_) => {
            unreachable!("handled above")
        }
    }

    Ok(())
}

enum Command {
    Chat {
        resume_id: Option<String>,
        auto_approve: bool,
    },
    Once {
        prompt: String,
        auto_approve: bool,
    },
    Doctor,
    Status,
    Benchmark {
        suite: std::path::PathBuf,
        profile: Option<String>,
        runs: usize,
    },
    Jobs(jobs::Command),
    Help,
    Version,
}

fn parse_command() -> Result<Command> {
    parse_command_from(std::env::args().skip(1))
}

fn parse_command_from(arguments: impl Iterator<Item = String>) -> Result<Command> {
    let tokens: Vec<String> = arguments.collect();
    const CHAT_USAGE: &str = "usage: kamui [-r <session-id>] [--auto-approve]";

    match tokens.first().map(String::as_str) {
        None => Ok(Command::Chat {
            resume_id: None,
            auto_approve: false,
        }),
        Some("-p" | "--print") => {
            let prompt = tokens
                .get(1)
                .cloned()
                .context("usage: kamui -p <prompt> [--auto-approve]")?;
            let mut auto_approve = false;
            for rest in &tokens[2..] {
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
        Some("status") => Ok(Command::Status),
        Some("benchmark") => parse_benchmark_command(&tokens[1..]),
        Some("jobs") => Ok(Command::Jobs(jobs::parse(&tokens[1..])?)),
        Some("-h" | "--help") => Ok(Command::Help),
        Some("-V" | "--version") => Ok(Command::Version),
        Some(_) => {
            // Chat mode: -r/--resume <id> and --auto-approve are both accepted, in any order.
            let mut resume_id = None;
            let mut auto_approve = false;
            let mut rest = tokens.iter();
            while let Some(token) = rest.next() {
                match token.as_str() {
                    "-r" | "--resume" => {
                        resume_id = Some(rest.next().context(CHAT_USAGE)?.clone());
                    }
                    "--auto-approve" => auto_approve = true,
                    _ => anyhow::bail!(CHAT_USAGE),
                }
            }
            Ok(Command::Chat {
                resume_id,
                auto_approve,
            })
        }
    }
}

fn parse_benchmark_command(tokens: &[String]) -> Result<Command> {
    const USAGE: &str = "usage: kamui benchmark <suite.json> [--profile <name>] [--runs <count>]";
    let suite = tokens.first().context(USAGE)?;
    if suite.starts_with('-') {
        anyhow::bail!(USAGE);
    }
    let mut profile = None;
    let mut runs = 1usize;
    let mut rest = tokens[1..].iter();
    while let Some(token) = rest.next() {
        match token.as_str() {
            "--profile" => profile = Some(rest.next().context(USAGE)?.clone()),
            "--runs" => {
                runs = rest
                    .next()
                    .context(USAGE)?
                    .parse()
                    .context("--runs must be a positive integer")?;
                if runs == 0 {
                    anyhow::bail!("--runs must be greater than zero");
                }
            }
            _ => anyhow::bail!(USAGE),
        }
    }
    Ok(Command::Benchmark {
        suite: suite.into(),
        profile,
        runs,
    })
}

/// `kamui status`: read configuration and the local database directly and print a summary,
/// without connecting to a provider or any MCP server — unlike `doctor`, this makes no network
/// calls. Useful for checking on a Kamui setup from a terminal without starting a chat session.
/// Ported from Kumo's `kumo status` (a sibling project, a Telegram gateway that delegates coding
/// work to Kamui), adapted for Kamui's per-project rather than per-process configuration.
async fn run_status() -> Result<()> {
    println!("Kamui v{}", env!("CARGO_PKG_VERSION"));
    println!();

    let config = match Config::load() {
        Ok(config::Loaded::Ready(config)) => config,
        Ok(config::Loaded::NeedsSetup(path)) => {
            println!(
                "Config:    not set up yet at {} (run `kamui` to finish onboarding)",
                path.display()
            );
            return Ok(());
        }
        Err(error) => {
            println!("Config:    invalid: {error:#}");
            return Ok(());
        }
    };

    let default = config.default();
    println!("Profile:   {} ({})", default.name, default.model);
    println!("Base URL:  {}", default.base_url);
    if config.profiles.len() > 1 {
        let names: Vec<&str> = config.profiles.iter().map(|p| p.name.as_str()).collect();
        println!(
            "Profiles:  {} configured ({})",
            config.profiles.len(),
            names.join(", ")
        );
    }
    match &default.embedding_model {
        Some(model) => println!("Embedding: {model}"),
        None => println!("Embedding: not configured (search_code unavailable)"),
    }
    if config.mcp_servers.is_empty() {
        println!("MCP:       none configured");
    } else {
        println!(
            "MCP:       {} server(s) configured",
            config.mcp_servers.len()
        );
        for server in &config.mcp_servers {
            println!("             - {}", server.name);
        }
    }
    if !config.allow_commands.is_empty() {
        println!(
            "Allowlist: {} command(s) auto-approved",
            config.allow_commands.len()
        );
    }

    let project = ProjectContext::discover()?;
    println!("Project:   {}", chat::display_path(project.root()));

    let database = Database::open()?;
    println!("Database:  {}", chat::display_path(database.path()));
    println!("Sessions:  {}", database.list_sessions()?.len());
    println!("Memory:    {} fact(s)", database.list_memory()?.len());
    if default.embedding_model.is_some() {
        println!(
            "Index:     {} chunk(s) for this project",
            database.chunk_count(&project.key())?
        );
    }

    Ok(())
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

        if let Some(embedding_model) = &profile.embedding_model {
            match provider
                .embed(embedding_model, vec!["health check".to_string()])
                .await
            {
                Ok(vectors) if vectors.len() == 1 && !vectors[0].is_empty() => check_ok(&format!(
                    "Embedding model '{embedding_model}' responds ({} dimensions)",
                    vectors[0].len()
                )),
                Ok(_) => {
                    check_fail(&format!(
                        "Embedding model '{embedding_model}' returned an empty or malformed vector"
                    ));
                    failures += 1;
                }
                Err(error) => {
                    check_fail(&format!(
                        "Embedding model '{embedding_model}' failed: {error:#}"
                    ));
                    failures += 1;
                }
            }
        } else {
            check_ok("No embedding model configured (semantic search is disabled)");
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
        Ok(database) => match database.integrity_check() {
            Ok(result) if result == "ok" => check_ok(&format!(
                "Database opens and passes integrity check (schema v{})",
                database.schema_version().unwrap_or_default()
            )),
            Ok(result) => {
                check_fail(&format!("Database integrity check reported: {result}"));
                failures += 1;
            }
            Err(error) => {
                check_fail(&format!("Database integrity check failed: {error:#}"));
                failures += 1;
            }
        },
        Err(error) => {
            check_fail(&format!("Database could not be opened: {error:#}"));
            failures += 1;
        }
    }

    match ProjectContext::discover() {
        Ok(project) => {
            check_ok(&format!(
                "Project root resolves to {}",
                chat::display_path(project.root())
            ));
            if let Some(name) = project.instruction_name() {
                check_ok(&format!("Project instructions loaded from {name}"));
            }
        }
        Err(error) => {
            check_fail(&format!("Project could not be discovered: {error:#}"));
            failures += 1;
        }
    }

    match std::process::Command::new("rtk").arg("--version").output() {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            check_ok(&format!(
                "RTK available ({})",
                version.lines().next().unwrap_or("version unknown").trim()
            ));
        }
        Ok(_) | Err(_) => {
            check_ok("RTK not installed (optional; direct commands remain available)")
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
    println!("      --auto-approve  Approve tool calls without prompting (with -p, or standalone)");
    println!("  doctor              Check configuration, provider, and MCP servers");
    println!("  status              Print a config/database summary (no network calls)");
    println!("  benchmark <FILE>    Run a repeatable JSON benchmark suite");
    println!("  jobs <COMMAND>      Manage persistent scheduled command jobs");
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
            Command::Chat {
                resume_id: None,
                auto_approve: false,
            }
        ));
    }

    #[test]
    fn resume_flag_carries_the_session_id() {
        let command = parse_command_from(args(&["-r", "abc123"])).unwrap();
        assert!(matches!(
            command,
            Command::Chat { resume_id: Some(id), auto_approve: false } if id == "abc123"
        ));
    }

    #[test]
    fn resume_flag_rejects_trailing_arguments() {
        assert!(parse_command_from(args(&["-r", "abc123", "extra"])).is_err());
    }

    #[test]
    fn auto_approve_flag_works_standalone_and_with_resume_in_either_order() {
        let standalone = parse_command_from(args(&["--auto-approve"])).unwrap();
        assert!(matches!(
            standalone,
            Command::Chat {
                resume_id: None,
                auto_approve: true
            }
        ));

        let after_resume = parse_command_from(args(&["-r", "abc123", "--auto-approve"])).unwrap();
        assert!(matches!(
            after_resume,
            Command::Chat { resume_id: Some(id), auto_approve: true } if id == "abc123"
        ));

        let before_resume = parse_command_from(args(&["--auto-approve", "-r", "abc123"])).unwrap();
        assert!(matches!(
            before_resume,
            Command::Chat { resume_id: Some(id), auto_approve: true } if id == "abc123"
        ));
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

    #[test]
    fn status_flag_is_recognized() {
        assert!(matches!(
            parse_command_from(args(&["status"])).unwrap(),
            Command::Status
        ));
    }

    #[test]
    fn benchmark_arguments_are_parsed() {
        let command = parse_command_from(args(&[
            "benchmark",
            "suite.json",
            "--profile",
            "fast",
            "--runs",
            "3",
        ]))
        .unwrap();
        assert!(matches!(
            command,
            Command::Benchmark { suite, profile: Some(profile), runs: 3 }
                if *suite == *"suite.json" && profile == "fast"
        ));
    }

    #[test]
    fn benchmark_rejects_zero_runs() {
        assert!(parse_command_from(args(&["benchmark", "suite.json", "--runs", "0"])).is_err());
    }
}
