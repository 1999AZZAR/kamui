use crate::commands;
use crate::compaction;
use crate::config::{Config, Profile};
use crate::context::ProjectContext;
use crate::markdown;
use crate::mcp::ConnectionStatus;
use crate::pricing::Prices;
use crate::prompt;
use crate::provider::{ChatRequest, Message, Provider, Role, StreamEvent, ToolCall, Usage};
use crate::render;
use crate::storage;
use crate::storage::{Database, Session};
use crate::terminal::{Style, Ui};
use crate::tools;
use crate::tools::ToolRegistry;
use crate::ui::{ChatUi, HubEvent, InputHub};
use anyhow::{Context, Result};
use chrono::{Local, TimeZone};
use dialoguer::console::{Key, Term};
use futures_util::future::join_all;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{path::Path, process::Command};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

const RESUME_PREVIEW_MESSAGES: usize = 6;
/// Upper bound on model/tool round-trips within a single user turn, to stop runaway tool loops.
/// Generous enough for multi-file edits while still bounding a stuck loop.
const MAX_TOOL_ROUNDS: usize = 25;
const MAX_CONCURRENT_SUB_AGENTS: usize = 4;
const EMBEDDING_BATCH_SIZE: usize = 64;
/// Settings key for the persisted active provider profile.
const ACTIVE_PROFILE_KEY: &str = "active_profile";

#[allow(clippy::too_many_arguments)]
pub async fn start_chat<F>(
    config: Config,
    tools: ToolRegistry,
    mcp_statuses: Vec<ConnectionStatus>,
    database: &Database,
    project: &ProjectContext,
    resume_id: Option<String>,
    auto_approve: bool,
    build_provider: F,
) -> Result<()>
where
    F: Fn(&Profile) -> Box<dyn Provider>,
{
    // Pick the active profile: a persisted choice if it still exists, otherwise the default.
    let active_name = database
        .get_setting(ACTIVE_PROFILE_KEY)?
        .filter(|name| config.find(name).is_some())
        .unwrap_or_else(|| config.default_profile.clone());
    let mut active = config
        .find(&active_name)
        .cloned()
        .unwrap_or_else(|| config.default().clone());
    let mut provider = build_provider(&active);
    let mut context_window = active.context_window;
    let job_registry = tools.jobs();
    let command_library = commands::CommandLibrary::load(project.root());
    let skill_library = crate::skills::SkillLibrary::load(project.root());
    let use_tui = crate::tui::is_interactive();
    let mut chat_ui = crate::ui::ChatUi::new(
        use_tui,
        format!(
            "Kamui v{} · {} · {}",
            env!("CARGO_PKG_VERSION"),
            active.model,
            display_path(project.root())
        ),
    )?;
    // The keyboard hub owns all TUI input for the session: editor always live, Enter queues
    // while the agent runs, Esc interrupts.
    let mut hub = chat_ui.screen_handle().map(InputHub::spawn);
    let interrupt = hub.as_ref().map(|h| h.interrupt.clone());
    if let Some(hub) = hub.as_ref() {
        hub.set_models(
            config
                .profiles
                .iter()
                .map(|profile| {
                    (
                        profile.name.clone(),
                        format!("{} · {}", profile.name, profile.model),
                    )
                })
                .collect(),
        );
        refresh_session_source(database, hub);
    }
    let ui = Ui::stdio();
    // One tidy startup line instead of a wall of per-skill warnings; /skills still lists every
    // individual reason.
    let skill_warning_count = skill_library.warnings().len();
    if skill_warning_count > 0 {
        if use_tui {
            chat_ui.warning(&format!(
                "{skill_warning_count} skill folder(s) skipped (invalid name or frontmatter) — /skills for details; /warnings hides"
            ))?;
        } else {
            for warning in skill_library.warnings() {
                eprintln!("warning: {warning}");
            }
        }
    }

    if !use_tui {
        print_status(
            project,
            &active,
            &tools,
            &mcp_statuses,
            &config.allow_commands,
        );
        println!("Data: {}", database.path().display());
    }
    // A hint, not an action: an out-of-date index is only worth mentioning to someone who already
    // runs `/index`, and refreshing it costs the user's own embedding budget, so Kamui reports the
    // drift and leaves the decision to them. Interactive chat only — `-p` output is script input.
    // A failed check is not worth interrupting startup over; semantic search still works without it.
    if active.embedding_model.is_some()
        && let Ok(Some(staleness)) = index_staleness(database, project)
        && !staleness.is_fresh()
    {
        chat_ui.notice(&format!(
            "Index: {} since last /index — run /index to refresh.",
            staleness.describe()
        ))?;
    }
    if auto_approve {
        chat_ui
            .warning("--auto-approve is active: commands and file edits will run without asking")?;
    }
    chat_ui.notice("Type / for commands · Tab completes · Ctrl+C cancels a turn")?;

    let (mut session, mut messages) = match resume_id {
        Some(id) => {
            let session = resolve_session(database, &id)?;
            if session.provider != provider.name() {
                anyhow::bail!(
                    "session uses provider '{}', but '{}' is active",
                    session.provider,
                    provider.name()
                );
            }
            let messages = database.load_messages(&session.id)?;
            chat_ui.notice(&format!(
                "Resuming: {} ({})",
                session.title,
                short_id(&session.id)
            ))?;
            if !use_tui {
                print_history_preview(&messages);
            }
            (Some(session), messages)
        }
        None => {
            chat_ui.notice("New chat")?;
            (None, Vec::new())
        }
    };
    update_sidebar(
        &mut chat_ui,
        session.as_ref(),
        &active.model,
        project,
        None,
        context_window,
        None,
    );
    // Restore pending plan on resume/startup.
    let mut plan_mode: Option<PlanModeState> = session
        .as_ref()
        .and_then(|s| database.get_plan(&s.id).ok().flatten())
        .and_then(|(json, status)| {
            let status = match status.as_str() {
                "pending" => PlanStatus::Pending,
                "approved" => PlanStatus::Approved,
                _ => return None,
            };
            Some(PlanModeState {
                status,
                plan_json: Some(json),
            })
        });
    if let Some(state) = plan_mode.as_ref()
        && state.status == PlanStatus::Pending
        && let Some(json) = state.plan_json.as_deref()
        && let Some(rendered) = tools::render_plan(json)
    {
        chat_ui.notice(&format!("Plan Mode — pending plan\n{rendered}"))?;
    }
    let mut input_rx = if use_tui { None } else { Some(input_channel()) };
    let mut disabled_skills = crate::settings::load_disabled_skills(project.root());

    // Rolling context compaction: `summary` folds in messages before `summarized_upto`; the rest of
    // `messages` is sent verbatim. Both reset whenever a command replaces the loaded history.
    let mut summary: Option<String> = None;
    let mut summarized_upto: usize = 0;
    // The most recently completed turn's pre-edit file snapshot, if it touched any files, so
    // `/undo` can revert it. `None` once nothing is left to undo.
    let mut last_turn_snapshot: Option<HashMap<PathBuf, Option<String>>> = None;
    // Tool names granted a standing "always allow" for the rest of this session (ported from
    // Kumo's "Always allow" approval button). Session-scoped: cleared whenever a chat effectively
    // restarts (`/new`, or `/delete` of the active session), same as `last_turn_snapshot`.
    let mut always_allowed: HashSet<String> = HashSet::new();
    // `/plan` forces the next turn into Plan Mode even for a small task.
    let mut plan_requested = false;
    // `/warnings` flips this; the transcript only renders the warning rail when it is set.
    let mut show_warnings = true;

    'chat: loop {
        let input = if use_tui {
            let hub = hub.as_mut().expect("tui implies hub");
            let cmds: Vec<crate::commands::CustomCommand> = command_library.list().to_vec();
            let sks: Vec<crate::skills::Skill> = skill_library.list().to_vec();
            hub.set_candidates(crate::tui::slash_candidates(&cmds, &sks, &disabled_skills));
            chat_ui.prompt()?;
            // Queued lines typed while the agent ran are consumed first, in order.
            if let Some(queued) = hub.pop_queue() {
                queued
            } else {
                match hub.next().await {
                    Some(HubEvent::Line(line)) => line,
                    Some(HubEvent::Quit) | None => {
                        shutdown(
                            database,
                            session.as_ref(),
                            context_window,
                            &job_registry,
                            &config.prices,
                        )?;
                        break;
                    }
                }
            }
        } else {
            print!("{}", ui.style("\u{276f} ", &[Style::Cyan, Style::Bold]));
            io::stdout().flush()?;
            let rx = input_rx.as_mut().expect("plain mode has input channel");
            let line = tokio::select! {
                input = rx.recv() => match input {
                    Some(input) => input,
                    None => {
                        shutdown(database, session.as_ref(), context_window, &job_registry, &config.prices)?;
                        break;
                    }
                },
                signal = tokio::signal::ctrl_c() => {
                    signal.context("failed to listen for Ctrl+C")?;
                    println!();
                    shutdown(database, session.as_ref(), context_window, &job_registry, &config.prices)?;
                    break;
                }
            };
            line
        };
        let input = input.trim();

        if input.eq_ignore_ascii_case("exit") || input == "/exit" {
            shutdown(
                database,
                session.as_ref(),
                context_window,
                &job_registry,
                &config.prices,
            )?;
            break;
        }
        if input.is_empty() {
            continue;
        }
        // `!cmd` runs a shell command directly — no model, no approval, the human typed it.
        if let Some(direct) = input.strip_prefix('!') {
            let direct = direct.trim();
            if direct.is_empty() {
                chat_ui.notice("usage: !<command> runs it in the shell (e.g. !git status)")?;
                continue;
            }
            let output = tools::run_direct_command(
                project.root(),
                direct,
                Duration::from_secs(config.command_timeout_secs),
            )
            .await;
            chat_ui.tool_call("shell", direct)?;
            chat_ui.tool_output(&output)?;
            continue;
        }
        // Slash commands are UI operations, not conversation turns — opencode hides them
        // from the transcript too.
        if !input.starts_with('/') {
            chat_ui.user(input)?;
        }

        // A custom command (`/review`, ...) or skill (`/my-skill`, `/skill:my-skill`) expands
        // into this turn's prompt and then takes the ordinary path below. Built-in commands and
        // custom commands win over a same-named skill on bare `/<name>`; use `/skill:<name>` to
        // force the skill. The original line is kept for titling.
        let expanded_command = command_library.expand(input);
        let expanded_skill = if expanded_command.is_none() {
            skill_library.expand_filtered(input, &disabled_skills)
        } else {
            None
        };
        let expanded = expanded_command.as_deref().or(expanded_skill.as_deref());
        let title_source = input;
        let input: &str = expanded.unwrap_or(input);

        if expanded.is_none() && input.starts_with('/') && use_tui {
            chat_ui.leave_intro()?;
        }
        if expanded.is_none() && input.starts_with('/') {
            let (command, argument) = input.split_once(' ').unwrap_or((input, ""));
            if command == "/help" && use_tui {
                chat_ui.toggle_help()?;
                continue;
            }
            if command == "/sessions" && use_tui {
                let opened = hub
                    .as_ref()
                    .map(|h| h.open_sessions_dialog())
                    .unwrap_or(false);
                if !opened {
                    chat_ui.notice("No saved sessions yet.")?;
                }
                continue;
            }
            if command == "/models" && use_tui {
                let opened = hub
                    .as_ref()
                    .map(|h| h.open_models_dialog())
                    .unwrap_or(false);
                if !opened {
                    chat_ui.notice("No provider profiles configured.")?;
                }
                continue;
            }
            if command == "/commands" {
                {
                    let mut buf = String::new();
                    if chat_ui.is_fullscreen() && command_library.is_empty() {
                        chat_ui.notice(
                            "Custom commands are listed in .kamui/commands and global kamui/commands.",
                        )?;
                        continue;
                    }
                    if chat_ui.is_fullscreen() {
                        // Fullscreen: list through the sink so it lands in the transcript.
                        let mut out_buf2 = String::new();
                        print_commands(&command_library, &mut out_buf2);
                        chat_ui.notice(out_buf2.trim_end())?;
                    } else if !command_library.is_empty() {
                        print_commands(&command_library, &mut buf);
                        print!("{buf}");
                    } else {
                        let mut out_buf2 = String::new();
                        print_commands(&command_library, &mut out_buf2);
                        print!("{out_buf2}");
                    }
                }
                continue;
            }
            if command == "/warnings" || command == "/warning" {
                show_warnings = match argument.to_ascii_lowercase().as_str() {
                    "" => !show_warnings,
                    "on" | "show" => true,
                    "off" | "hide" => false,
                    other => {
                        chat_ui.notice(&format!("usage: /warnings [on|off] (got \"{other}\")"))?;
                        continue;
                    }
                };
                chat_ui.set_warnings_visible(show_warnings)?;
                chat_ui.notice(if show_warnings {
                    "Warnings shown."
                } else {
                    "Warnings hidden. /warnings to show again."
                })?;
                continue;
            }
            if command == "/expand" {
                if !chat_ui.is_fullscreen() || !chat_ui.expand_last()? {
                    chat_ui.notice("Nothing to expand.")?;
                }
                continue;
            }
            if command == "/collapse" {
                if !chat_ui.is_fullscreen() || !chat_ui.collapse_last()? {
                    chat_ui.notice("Nothing to collapse.")?;
                }
                continue;
            }
            if command == "/skills" {
                // Non-interactive (piped) fallback: plain list.
                if !Ui::stdio().interactive() {
                    let mut buf = String::new();
                    print_skills(&skill_library, &disabled_skills, &mut buf);
                    print!("{buf}");
                    continue;
                }
                let is_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
                if !is_tty {
                    let mut buf = String::new();
                    print_skills(&skill_library, &disabled_skills, &mut buf);
                    print!("{buf}");
                    continue;
                }
                // Run popup on a blocking thread so the tokio runtime is not blocked.
                // Clone the library's data for 'static; the popup needs no live borrow.
                let skills_snapshot = skill_library.list().to_vec();
                let warnings_snapshot = skill_library.warnings().to_vec();
                let root = project.root().to_path_buf();
                let root2 = root.clone();
                let mut ds = disabled_skills.clone();
                let changed = tokio::task::spawn_blocking(move || {
                    let lib =
                        crate::skills::SkillLibrary::from_parts(skills_snapshot, warnings_snapshot);
                    run_skills_popup(&lib, &root, &mut ds)
                })
                .await
                .unwrap_or(Ok(false))
                .unwrap_or(false);
                if changed {
                    disabled_skills = crate::settings::load_disabled_skills(&root2);
                }
                continue;
            }
            if command == "/model" {
                let tui_sink = if use_tui { Some(&mut chat_ui) } else { None };
                if let Err(error) = switch_profile(
                    argument.trim(),
                    &config,
                    &mut active,
                    &mut provider,
                    &mut context_window,
                    database,
                    &build_provider,
                    tui_sink,
                ) {
                    if chat_ui.is_fullscreen() {
                        chat_ui.error(&format!("Command failed: {error:#}"))?;
                    } else {
                        eprintln!(
                            "{}",
                            ui.style(&format!("Command failed: {error:#}\n"), &[Style::Red])
                        );
                    }
                }
                if chat_ui.is_fullscreen() {
                    chat_ui.set_header(format!(
                        "Kamui v{} · {} · {}",
                        env!("CARGO_PKG_VERSION"),
                        active.model,
                        display_path(project.root())
                    ))?;
                }
                continue;
            }
            if command == "/status" {
                if chat_ui.is_fullscreen() {
                    chat_ui.notice(&format!(
                        "Project: {} · Model: {} · Tools: {} · MCP: {}",
                        display_path(project.root()),
                        active.model,
                        tools.len(),
                        mcp_statuses.len()
                    ))?;
                } else {
                    print_status(
                        project,
                        &active,
                        &tools,
                        &mcp_statuses,
                        &config.allow_commands,
                    );
                }
                continue;
            }
            if command == "/compact" {
                let outcome = tokio::select! {
                    result = run_compaction(
                        provider.as_ref(), &active.model, &messages, summary.as_deref(), summarized_upto,
                    ) => result,
                    signal = tokio::signal::ctrl_c() => {
                        signal.context("failed to listen for Ctrl+C")?;
                        chat_ui.notice("interrupted — back to prompt")?;
                        continue;
                    }
                };
                match outcome {
                    Ok(Some((new_summary, new_upto, count))) => {
                        summary = Some(new_summary);
                        summarized_upto = new_upto;
                        chat_ui.notice(&format!(
                            "Compacted {count} earlier messages into the summary."
                        ))?;
                    }
                    Ok(None) => chat_ui.notice("Not enough history to compact yet.")?,
                    Err(error) => {
                        if chat_ui.is_fullscreen() {
                            chat_ui.error(&format!("Compaction failed: {error:#}"))?;
                        } else {
                            eprintln!(
                                "{}",
                                ui.style(&format!("Compaction failed: {error:#}\n"), &[Style::Red])
                            );
                        }
                    }
                }
                continue;
            }
            if command == "/plan" {
                plan_requested = true;
                chat_ui.notice("Plan Mode requested — next turn will require a plan.")?;
                continue;
            }
            if command == "/undo" {
                match last_turn_snapshot.take() {
                    Some(snapshot) => {
                        let reverted = revert_snapshot(&snapshot);
                        chat_ui
                            .notice(&format!("Reverted {reverted} file(s) from the last turn."))?;
                    }
                    None => chat_ui.notice("Nothing to undo.")?,
                }
                continue;
            }
            if command == "/jobs" {
                let text = format!(
                    "Session jobs:\n{}\n\nScheduled jobs:\n{}",
                    tools::describe_jobs(&job_registry),
                    crate::jobs::format_jobs(&database.list_scheduled_jobs()?)
                );
                chat_ui.notice(&text)?;
                continue;
            }
            if command == "/index" {
                let outcome = tokio::select! {
                    result = run_index(provider.as_ref(), &active, database, project) => result,
                    signal = tokio::signal::ctrl_c() => {
                        signal.context("failed to listen for Ctrl+C")?;
                        chat_ui.notice("interrupted — back to prompt")?;
                        continue;
                    }
                };
                match outcome {
                    Ok(summary) => chat_ui.notice(&summary)?,
                    Err(error) => {
                        if chat_ui.is_fullscreen() {
                            chat_ui.error(&format!("Index failed: {error:#}"))?;
                        } else {
                            eprintln!(
                                "{}",
                                ui.style(&format!("Index failed: {error:#}\n"), &[Style::Red])
                            );
                        }
                    }
                }
                continue;
            }
            let prev_session_id = session.as_ref().map(|s| s.id.clone());
            let messages_before = messages.len();
            if use_tui {
                chat_ui.leave_intro()?;
            }
            let tui_sink = if use_tui { Some(&mut chat_ui) } else { None };
            if let Err(error) = handle_command(
                input,
                provider.as_ref(),
                context_window,
                database,
                &mut session,
                &mut messages,
                &mut always_allowed,
                &mut last_turn_snapshot,
                &config.prices,
                tui_sink,
            ) {
                if chat_ui.is_fullscreen() {
                    chat_ui.error(&format!("Command failed: {error:#}"))?;
                } else {
                    eprintln!(
                        "{}",
                        ui.style(&format!("Command failed: {error:#}\n"), &[Style::Red])
                    );
                }
            }
            // Sidebar follows /new //resume: session title, id, and context reset.
            if use_tui {
                update_sidebar(
                    &mut chat_ui,
                    session.as_ref(),
                    &active.model,
                    project,
                    None,
                    context_window,
                    None,
                );
            }
            // Sync Plan Mode with session changes from /new /resume /delete.
            let new_session_id = session.as_ref().map(|s| s.id.clone());
            if prev_session_id != new_session_id {
                if let Some(id) = new_session_id {
                    plan_mode = database
                        .get_plan(&id)
                        .ok()
                        .flatten()
                        .and_then(|(json, status)| {
                            let status = match status.as_str() {
                                "pending" => PlanStatus::Pending,
                                "approved" => PlanStatus::Approved,
                                _ => return None,
                            };
                            Some(PlanModeState {
                                status,
                                plan_json: Some(json),
                            })
                        });
                    if let Some(state) = plan_mode.as_ref()
                        && state.status == PlanStatus::Pending
                        && let Some(json) = state.plan_json.as_deref()
                        && let Some(rendered) = tools::render_plan(json)
                    {
                        chat_ui.notice(&format!("Plan Mode — pending plan\n{rendered}"))?;
                    }
                } else {
                    plan_mode = None;
                    plan_requested = false;
                }
            }
            // Compaction state is tied to the current history; reset it if a command replaced it.
            if messages.len() != messages_before {
                summary = None;
                summarized_upto = 0;
            }
            continue;
        }

        let user_message = Message::user(input);
        let expanded = match project.expand_file_references(input) {
            Ok(expanded) => expanded,
            Err(error) => {
                if chat_ui.is_fullscreen() {
                    chat_ui.error(&format!("Could not attach file: {error:#}"))?;
                } else {
                    eprintln!(
                        "{}",
                        ui.style(
                            &format!("\nCould not attach file: {error:#}\n"),
                            &[Style::Red]
                        )
                    );
                }
                continue;
            }
        };

        let model = active.model.clone();
        // Plan Mode: auto-enter for ≥3-step tasks (heuristic on prompt) or manual /plan.
        // Also auto-enter when model first calls update_plan with ≥3 steps (prompt heuristic
        // is not the only signal — Q1=B covers both).
        let should_enter_plan =
            plan_requested || (plan_mode.is_none() && looks_like_multi_step(input));
        if should_enter_plan && active.tools {
            plan_mode = Some(PlanModeState {
                status: PlanStatus::Pending,
                plan_json: None,
            });
            plan_requested = false;
            if let Some(session) = session.as_ref() {
                let _ = database.set_plan(&session.id, "{}", "pending");
            }
            chat_ui.notice("Plan Mode — only read-only tools + update_plan until approved")?;
        } else if plan_requested {
            plan_requested = false;
        }
        // Some models/endpoints reject the `tools` field; a profile can opt out so plain chat works.
        // In Plan Mode (pending), only read-only + update_plan + ask_user/search_code/spawn_agent.
        let is_plan_pending = plan_mode
            .as_ref()
            .is_some_and(|s| s.status == PlanStatus::Pending);
        let tool_definitions = if active.tools {
            if is_plan_pending {
                plan_mode_definitions(project.root(), active.embedding_model.is_some())
            } else {
                let mut defs = tools.definitions();
                if active.embedding_model.is_some() {
                    defs.push(tools::search_code_definition());
                }
                defs
            }
        } else {
            Vec::new()
        };

        // Auto-compact older history once the recent portion grows past the threshold.
        summarized_upto = summarized_upto.min(messages.len());
        if compaction::total_bytes(&messages[summarized_upto..])
            > compaction::threshold(active.context_window)
        {
            let outcome = tokio::select! {
                result = run_compaction(
                    provider.as_ref(), &active.model, &messages, summary.as_deref(), summarized_upto,
                ) => result,
                signal = tokio::signal::ctrl_c() => {
                    signal.context("failed to listen for Ctrl+C")?;
                    chat_ui.notice("interrupted — back to prompt")?;
                    continue 'chat;
                }
            };
            match outcome {
                Ok(Some((new_summary, new_upto, count))) => {
                    summary = Some(new_summary);
                    summarized_upto = new_upto;
                    chat_ui.notice(&format!(
                        "Compacted {count} earlier messages into a running summary."
                    ))?;
                }
                Ok(None) => {}
                Err(error) => {
                    if chat_ui.is_fullscreen() {
                        chat_ui.error(&format!("Could not compact history: {error:#}"))?;
                    } else {
                        eprintln!(
                            "{}",
                            ui.style(
                                &format!("(could not compact history: {error:#})\n"),
                                &[Style::Red]
                            )
                        );
                    }
                }
            }
        }

        // Working conversation for this turn: the agentic system prompt (plus project instructions
        // and any running summary), the un-summarized recent history, and the expanded prompt.
        // Intermediate tool messages live here only; they are not persisted. Memory is read fresh
        // every turn (unlike Kumo's frozen-at-startup snapshot): Kamui is a single interactive
        // process, not a server shared across many chats, so a fact remembered in this turn should
        // be visible on the very next one without needing a restart.
        let skills_eager = skill_library.eager_block_filtered(&disabled_skills);
        let mut system = prompt::build(
            active.tools,
            project.system_message().as_deref(),
            skills_eager.as_deref(),
        );
        let memory_snapshot = render_memory_snapshot(&database.list_memory()?);
        if !memory_snapshot.is_empty() {
            system.push_str("\n\n");
            system.push_str(&memory_snapshot);
        }
        if let Some(summary) = &summary {
            system.push_str("\n\nSummary of the earlier conversation so far:\n\n");
            system.push_str(summary);
        }
        let mut turn_messages = vec![Message::system(system)];
        turn_messages.extend(messages[summarized_upto..].iter().cloned());
        turn_messages.push(Message::user_with_images(expanded.text, expanded.images));

        // Agent loop: stream a turn, run any tools it requests, and repeat until a plain answer.
        // `tool_trail` collects this turn's intermediate tool-request and tool-result messages so
        // they can be persisted alongside the prompt and final answer.
        let mut final_usage = Usage::default();
        let mut final_finish = String::new();
        let mut last_content = String::new();
        let mut tool_trail: Vec<Message> = Vec::new();
        // Pre-edit snapshot of every file an approved patch_file call touches this turn, so an
        // interrupted multi-file edit can be reverted instead of left half-applied (see
        // `snapshot_patch_target`/`revert_on_cancel`).
        let mut turn_snapshot: HashMap<PathBuf, Option<String>> = HashMap::new();
        let mut round = 0usize;
        // Esc / Ctrl+C now interrupt; the editor stays live for queueing.
        let _busy = hub.as_ref().map(|h| h.busy_guard());
        let assistant_message = 'agent: loop {
            round += 1;
            if round > MAX_TOOL_ROUNDS {
                chat_ui.notice(&format!(
                    "Stopped after {MAX_TOOL_ROUNDS} tool rounds without a final answer."
                ))?;
                break 'agent Message::assistant(if last_content.is_empty() {
                    "(stopped: reached the tool-call round limit)".to_string()
                } else {
                    last_content.clone()
                });
            }

            let started = Instant::now();
            let request = provider.chat_stream(ChatRequest {
                model: model.clone(),
                messages: turn_messages.clone(),
                tools: tool_definitions.clone(),
            });
            if !chat_ui.is_fullscreen() {
                println!();
            }
            // Animate a spinner from the moment the request is sent until the first token (or a
            // terminal event) arrives, so the wait for the model does not look frozen.
            let mut spinner = start_spinner("Thinking...", ui, &mut chat_ui);
            let mut stream = tokio::select! {
                response = request => match response {
                    Ok(stream) => stream,
                    Err(error) => {
                        stop_spinner(&mut spinner, &mut chat_ui).await;
                        chat_ui.error(&format!("Request failed: {error:#}"))?;
                        revert_on_cancel(&turn_snapshot);
                        continue 'chat;
                    }
                },
                signal = tokio::signal::ctrl_c() => {
                    stop_spinner(&mut spinner, &mut chat_ui).await;
                    signal.context("failed to listen for Ctrl+C")?;
                    revert_on_cancel(&turn_snapshot);
                    chat_ui.notice("interrupted — back to prompt")?;
                    continue 'chat;
                },
                () = wait_interrupt(&interrupt) => {
                    stop_spinner(&mut spinner, &mut chat_ui).await;
                    revert_on_cancel(&turn_snapshot);
                    chat_ui.notice("interrupted — back to prompt")?;
                    continue 'chat;
                }
            };

            let mut content = String::new();
            let mut ttft: Option<Duration> = None;
            // Styles the streamed text a line at a time. `content` keeps the raw markdown, since
            // that is what gets persisted and re-sent to the model.
            let mut renderer = markdown::Renderer::for_stdout();
            let (usage, finish_reason, tool_calls) = loop {
                let event = tokio::select! {
                    event = stream.recv() => event,
                    signal = tokio::signal::ctrl_c() => {
                        stop_spinner(&mut spinner, &mut chat_ui).await;
                        signal.context("failed to listen for Ctrl+C")?;
                        // Show the partial line still held by the line buffer before leaving.
                        if chat_ui.is_fullscreen() {
                            chat_ui.assistant_update(&content)?;
                        } else {
                            print!("{}", renderer.finish());
                        }
                        revert_on_cancel(&turn_snapshot);
                        chat_ui.notice("interrupted — back to prompt")?;
                        continue 'chat;
                    }
                    () = wait_interrupt(&interrupt) => {
                        stop_spinner(&mut spinner, &mut chat_ui).await;
                        if chat_ui.is_fullscreen() {
                            chat_ui.assistant_update(&content)?;
                        } else {
                            print!("{}", renderer.finish());
                        }
                        revert_on_cancel(&turn_snapshot);
                        chat_ui.notice("interrupted — back to prompt")?;
                        continue 'chat;
                    }
                };
                match event {
                    Some(Ok(StreamEvent::Delta(delta))) => {
                        stop_spinner(&mut spinner, &mut chat_ui).await;
                        if ttft.is_none() {
                            ttft = Some(started.elapsed());
                        }
                        let rendered = renderer.push(&delta);
                        if chat_ui.is_fullscreen() {
                            content.push_str(&delta);
                            chat_ui.assistant_update(&content)?;
                        } else {
                            print!("{rendered}");
                            io::stdout().flush()?;
                            content.push_str(&delta);
                        }
                    }
                    Some(Ok(StreamEvent::Done {
                        usage,
                        finish_reason,
                        tool_calls,
                    })) => {
                        stop_spinner(&mut spinner, &mut chat_ui).await;
                        if chat_ui.is_fullscreen() {
                            chat_ui.assistant_update(&content)?;
                            chat_ui.assistant_done()?;
                        } else {
                            print!("{}", renderer.finish());
                            println!();
                        }
                        break (usage, finish_reason, tool_calls);
                    }
                    Some(Err(error)) => {
                        stop_spinner(&mut spinner, &mut chat_ui).await;
                        if chat_ui.is_fullscreen() {
                            chat_ui.error(&format!("Request failed: {error:#}"))?;
                        } else {
                            print!("{}", renderer.finish());
                            eprintln!(
                                "\n{}",
                                render::render_error(&format!("Request failed: {error:#}"), ui)
                            );
                        }
                        revert_on_cancel(&turn_snapshot);
                        continue 'chat;
                    }
                    None => {
                        stop_spinner(&mut spinner, &mut chat_ui).await;
                        if chat_ui.is_fullscreen() {
                            chat_ui.error("Request failed: provider stream closed unexpectedly")?;
                        } else {
                            print!("{}", renderer.finish());
                            eprintln!(
                                "\n{}",
                                render::render_error(
                                    "Request failed: provider stream closed unexpectedly",
                                    ui
                                )
                            );
                        }
                        revert_on_cancel(&turn_snapshot);
                        continue 'chat;
                    }
                }
            };
            let usage_line = format_usage(
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.total_tokens,
                usage.cached_tokens,
                &finish_reason,
                ttft,
                started.elapsed(),
                context_window,
            );
            if chat_ui.is_fullscreen() {
                // The usage report lives in the sidebar rail; the transcript stays readable.
                update_sidebar(
                    &mut chat_ui,
                    session.as_ref(),
                    &active.model,
                    project,
                    Some(usage.prompt_tokens),
                    context_window,
                    Some(usage_line),
                );
            } else {
                println!("{usage_line}");
                update_sidebar(
                    &mut chat_ui,
                    session.as_ref(),
                    &active.model,
                    project,
                    Some(usage.prompt_tokens),
                    context_window,
                    None,
                );
            }
            accumulate_usage(&mut final_usage, &usage);
            final_finish = finish_reason;
            last_content = content.clone();

            if tool_calls.is_empty() {
                break 'agent Message::assistant(content);
            }

            // The model requested tools. Record the request, run each tool, feed the results back.
            let request_message = Message::tool_request(content, tool_calls.clone());
            turn_messages.push(request_message.clone());
            tool_trail.push(request_message);
            let spawn_calls: Vec<&ToolCall> = tool_calls
                .iter()
                .filter(|call| call.name == tools::SPAWN_AGENT_TOOL)
                .collect();
            let spawned_outputs = if spawn_calls.is_empty() {
                HashMap::new()
            } else {
                chat_ui.notice(&format!(
                    "running {} sub-agent(s), up to {MAX_CONCURRENT_SUB_AGENTS} concurrently",
                    spawn_calls.len()
                ))?;
                tokio::select! {
                    output = dispatch_spawn_agents(
                        provider.as_ref(), &active.model, project, &spawn_calls,
                    ) => output,
                    signal = tokio::signal::ctrl_c() => {
                        signal.context("failed to listen for Ctrl+C")?;
                        revert_on_cancel(&turn_snapshot);
                        chat_ui.notice("interrupted — back to prompt")?;
                        continue 'chat;
                    }
                }
            };
            // Plan Mode: auto-enter on first update_plan with ≥3 steps (Q1=B).
            if plan_mode.is_none() {
                for call in &tool_calls {
                    if call.name == tools::UPDATE_PLAN_TOOL
                        && let Some(count) = tools::plan_step_count(&call.arguments)
                        && count >= 3
                    {
                        plan_mode = Some(PlanModeState {
                            status: PlanStatus::Pending,
                            plan_json: None,
                        });
                        if let Some(session) = session.as_ref() {
                            let _ = database.set_plan(&session.id, "{}", "pending");
                        }
                        chat_ui.notice(
                            "Plan Mode — only read-only tools + update_plan until approved",
                        )?;
                        break;
                    }
                }
            }
            for call in &tool_calls {
                let tool_started = Instant::now();
                if call.name == tools::UPDATE_PLAN_TOOL
                    && let Some(rendered) = tools::render_plan(&call.arguments)
                {
                    chat_ui.tool_call(&call.name, &call.arguments)?;
                    if chat_ui.is_fullscreen() {
                        chat_ui.notice(&rendered)?;
                    } else {
                        println!("{rendered}");
                    }
                    // Persist plan: pending stays pending, approved stays tracker.
                    if let Some(state) = plan_mode.as_mut() {
                        state.plan_json = Some(call.arguments.clone());
                        let status = match state.status {
                            PlanStatus::Pending => "pending",
                            PlanStatus::Approved => "approved",
                        };
                        if let Some(session) = session.as_ref() {
                            let _ = database.set_plan(&session.id, &call.arguments, status);
                        }
                    }
                    // Inline approval prompt when a plan is pending and this is the plan call.
                    if plan_mode
                        .as_ref()
                        .is_some_and(|s| s.status == PlanStatus::Pending)
                    {
                        let plan_title = "Approve plan?";
                        let plan_body = tools::render_plan(
                            plan_mode
                                .as_ref()
                                .and_then(|state| state.plan_json.as_deref())
                                .unwrap_or(""),
                        )
                        .unwrap_or_default();
                        let answer = tokio::select! {
                            answer = read_approval_line(&mut input_rx, use_tui, hub.as_mut(), plan_title, plan_body) => answer,
                        () = wait_interrupt(&interrupt) => None,
                            signal = tokio::signal::ctrl_c() => {
                                signal.context("failed to listen for Ctrl+C")?;
                                revert_on_cancel(&turn_snapshot);
                                chat_ui.notice("interrupted — back to prompt")?;
                                continue 'chat;
                            }
                        };
                        let approved = matches!(
                            answer.as_deref().map(str::trim),
                            Some("y" | "Y" | "yes" | "Yes")
                        );
                        if approved {
                            if let Some(state) = plan_mode.as_mut() {
                                state.status = PlanStatus::Approved;
                                if let Some(session) = session.as_ref()
                                    && let Some(json) = state.plan_json.clone()
                                {
                                    let _ = database.set_plan(&session.id, &json, "approved");
                                }
                            }
                            chat_ui.notice("plan approved — gate open for this session")?;
                        } else {
                            chat_ui.notice(
                                "plan not approved — still in Plan Mode; propose a revised plan",
                            )?;
                        }
                    }
                } else {
                    chat_ui.tool_call(&call.name, &call.arguments)?;
                }
                // In pending Plan Mode, hold mutating tools.
                let is_mutating_held = plan_mode
                    .as_ref()
                    .is_some_and(|s| s.status == PlanStatus::Pending)
                    && is_mutating_tool(&call.name);
                let output = if is_mutating_held {
                    "Plan Mode is active — propose a plan with update_plan and wait for approval before mutating tools.".to_string()
                } else if call.name == tools::ASK_USER_TOOL {
                    tokio::select! {
                        output = ask_user(&mut input_rx, use_tui, &call.arguments, hub.as_mut()) => output?,
                        signal = tokio::signal::ctrl_c() => {
                            signal.context("failed to listen for Ctrl+C")?;
                            revert_on_cancel(&turn_snapshot);
                            chat_ui.notice("interrupted — back to prompt")?;
                            continue 'chat;
                        }
                        () = wait_interrupt(&interrupt) => {
                            revert_on_cancel(&turn_snapshot);
                            chat_ui.notice("interrupted — back to prompt")?;
                            continue 'chat;
                        }
                    }
                } else if call.name == tools::SPAWN_AGENT_TOOL {
                    spawned_outputs
                        .get(&call.id)
                        .map(|(output, _)| output.clone())
                        .unwrap_or_else(|| "Error: sub-agent result was missing".to_string())
                } else if call.name == tools::SEARCH_CODE_TOOL {
                    match active.embedding_model.as_deref() {
                        Some(embedding_model) => {
                            tokio::select! {
                                output = dispatch_search_code(
                                    provider.as_ref(),
                                    embedding_model,
                                    database,
                                    project,
                                    &call.arguments,
                                ) => output,
                                signal = tokio::signal::ctrl_c() => {
                                    signal.context("failed to listen for Ctrl+C")?;
                                    revert_on_cancel(&turn_snapshot);
                                    chat_ui.notice("interrupted — back to prompt")?;
                                    continue 'chat;
                                }
                            }
                        }
                        None => "Error: this profile has no embedding_model configured; \
                                 search_code is unavailable"
                            .to_string(),
                    }
                } else if is_memory_tool(&call.name) {
                    dispatch_memory_tool(database, &call.name, &call.arguments)
                } else if tools.requires_confirmation_for(&call.name, &call.arguments)
                    && !auto_approve
                    && !always_allowed.contains(&call.name)
                {
                    let preview = tools.preview(call);
                    if !use_tui {
                        if let Some(preview) = &preview {
                            chat_ui.notice(preview)?;
                        }
                        chat_ui.notice("approve? [y/N/a]")?;
                    }
                    let modal_title = format!("Allow {}?", call.name);
                    let modal_body =
                        preview.unwrap_or_else(|| format!("{} {}", call.name, call.arguments));
                    let answer = tokio::select! {
                        answer = read_approval_line(&mut input_rx, use_tui, hub.as_mut(), &modal_title, modal_body) => answer,
                        () = wait_interrupt(&interrupt) => None,
                        signal = tokio::signal::ctrl_c() => {
                            signal.context("failed to listen for Ctrl+C")?;
                            revert_on_cancel(&turn_snapshot);
                            chat_ui.notice("interrupted — back to prompt")?;
                            continue 'chat;
                        }
                    };
                    let trimmed = answer.as_deref().map(str::trim);
                    let always = matches!(trimmed, Some("a" | "A" | "always" | "Always"));
                    let approved = always || matches!(trimmed, Some("y" | "Y" | "yes" | "Yes"));
                    if always {
                        always_allowed.insert(call.name.clone());
                        chat_ui.notice(&format!(
                            "always allowing {} for the rest of this session — /new clears this",
                            call.name
                        ))?;
                    }
                    if approved {
                        if call.name == tools::PATCH_FILE_TOOL {
                            snapshot_patch_target(
                                project.root(),
                                &call.arguments,
                                &mut turn_snapshot,
                            );
                        }
                        tokio::select! {
                            output = tools.dispatch(call) => output,
                            signal = tokio::signal::ctrl_c() => {
                                signal.context("failed to listen for Ctrl+C")?;
                                revert_on_cancel(&turn_snapshot);
                                chat_ui.notice("interrupted — back to prompt")?;
                                continue 'chat;
                            }
                        }
                    } else {
                        chat_ui.notice("skipped")?;
                        "The user declined to run this command.".to_string()
                    }
                } else {
                    // Reached because the tool never needs confirmation, --auto-approve overrode
                    // one that normally would, or it was granted a standing "always allow" this
                    // session; either way, still snapshot a patch so it can be reverted like an
                    // approved one.
                    if call.name == tools::PATCH_FILE_TOOL {
                        snapshot_patch_target(project.root(), &call.arguments, &mut turn_snapshot);
                    }
                    tokio::select! {
                        output = tools.dispatch(call) => output,
                        signal = tokio::signal::ctrl_c() => {
                            signal.context("failed to listen for Ctrl+C")?;
                            revert_on_cancel(&turn_snapshot);
                            chat_ui.notice("interrupted — back to prompt")?;
                            continue 'chat;
                        }
                    }
                };
                let elapsed = spawned_outputs
                    .get(&call.id)
                    .map(|(_, elapsed)| *elapsed)
                    .unwrap_or_else(|| tool_started.elapsed());
                if !output.starts_with("Error: ") && !output.is_empty() {
                    chat_ui.tool_output(&preview_output(&output))?;
                } else if chat_ui.is_fullscreen() {
                    chat_ui.tool_error(output.trim_start_matches("Error: "))?;
                }
                let outcome = format_tool_outcome(&output, elapsed);
                if chat_ui.is_fullscreen() {
                    chat_ui.notice(&outcome)?;
                } else {
                    println!("{outcome}");
                }
                let result_message = Message::tool_result(&call.id, output);
                turn_messages.push(result_message.clone());
                tool_trail.push(result_message);
            }
        };

        // The turn completed normally (not cancelled): keep its file snapshot around for /undo.
        last_turn_snapshot = (!turn_snapshot.is_empty()).then_some(turn_snapshot);

        // Assemble the full turn: the original prompt, any tool trail, then the final answer.
        let final_answer = assistant_message.content.clone();
        let mut turn_record = Vec::with_capacity(tool_trail.len() + 2);
        turn_record.push(user_message);
        turn_record.append(&mut tool_trail);
        turn_record.push(assistant_message);

        let is_first_exchange = session.is_none();
        let active_session = match session.as_mut() {
            Some(session) => session,
            None => session.insert(database.create_session(provider.name(), &active.model)?),
        };
        database.save_turn(
            &active_session.id,
            &turn_record,
            &final_usage,
            &active.model,
            &final_finish,
        )?;
        // Persist plan state after save (session now exists). Approved clears pending.
        if let Some(state) = plan_mode.as_ref() {
            match state.status {
                PlanStatus::Approved => {
                    if let Some(json) = state.plan_json.as_deref() {
                        let _ = database.set_plan(&active_session.id, json, "approved");
                    }
                }
                PlanStatus::Pending => {
                    if let Some(json) = state.plan_json.as_deref() {
                        let _ = database.set_plan(&active_session.id, json, "pending");
                    }
                }
            }
        }
        if active_session.title == "New chat" {
            active_session.title = make_title(title_source);
        }
        messages.extend(turn_record);

        // Only after the turn is safely persisted: a refresh failure must never cost the exchange.
        if let Some(snapshot) = last_turn_snapshot.as_ref() {
            let edited: Vec<PathBuf> = snapshot.keys().cloned().collect();
            report_index_refresh(tokio::select! {
                result = refresh_index_for_paths(
                    provider.as_ref(), &active, database, project, edited,
                ) => result,
                signal = tokio::signal::ctrl_c() => {
                    signal.context("failed to listen for Ctrl+C")?;
                    println!("\n(interrupted — the index may be stale; /index refreshes it)");
                    Ok(0)
                }
            });
        }

        if is_first_exchange {
            let title_request = provider.chat(ChatRequest {
                model: active.model.clone(),
                messages: vec![
                    Message::system(
                        "Create a concise title of at most 6 words for this conversation. Return only the title without quotes or punctuation.",
                    ),
                    Message::user(title_source),
                    Message::assistant(final_answer),
                ],
                tools: Vec::new(),
            });
            let title_response = tokio::select! {
                response = title_request => response,
                signal = tokio::signal::ctrl_c() => {
                    signal.context("failed to listen for Ctrl+C")?;
                    println!();
                    shutdown(database, session.as_ref(), context_window, &job_registry, &config.prices)?;
                    break;
                }
            };
            match title_response {
                Ok(response) => {
                    let title = clean_title(&response.content);
                    if !title.is_empty() {
                        let session = session.as_mut().expect("session was just persisted");
                        database.save_generated_title(
                            &session.id,
                            &title,
                            &response.usage,
                            &active.model,
                            &response.finish_reason,
                        )?;
                        session.title = title;
                    }
                }
                Err(error) => eprintln!(
                    "{}",
                    ui.style(
                        &format!("Could not generate session title: {error:#}\n"),
                        &[Style::Red]
                    )
                ),
            }
        }
    }

    Ok(())
}

/// Run a single prompt non-interactively and exit: no REPL loop, no stdin reader, no spinner.
/// Reuses the same profile selection, tool registry, agent loop, and persistence as interactive
/// chat, so a `-p` session can later be resumed with `-r` like any other.
pub async fn run_once<F>(
    config: Config,
    tools: ToolRegistry,
    database: &Database,
    project: &ProjectContext,
    prompt: &str,
    auto_approve: bool,
    build_provider: F,
) -> Result<()>
where
    F: Fn(&Profile) -> Box<dyn Provider>,
{
    let ui = Ui::stdio();
    let active_name = database
        .get_setting(ACTIVE_PROFILE_KEY)?
        .filter(|name| config.find(name).is_some())
        .unwrap_or_else(|| config.default_profile.clone());
    let active = config
        .find(&active_name)
        .cloned()
        .unwrap_or_else(|| config.default().clone());
    let provider = build_provider(&active);

    // `kamui -p /review` or `/skill:my-skill` expands the same way interactive chat does.
    let command_library = commands::CommandLibrary::load(project.root());
    let skill_library = crate::skills::SkillLibrary::load(project.root());
    for warning in skill_library.warnings() {
        eprintln!("warning: {warning}");
    }
    let disabled_skills = crate::settings::load_disabled_skills(project.root());
    let expanded_command = command_library.expand(prompt);
    let expanded_skill = if expanded_command.is_none() {
        skill_library.expand_filtered(prompt, &disabled_skills)
    } else {
        None
    };
    let expanded = expanded_command.as_deref().or(expanded_skill.as_deref());
    let title_source = prompt;
    let prompt: &str = expanded.unwrap_or(prompt);

    let expanded = project
        .expand_file_references(prompt)
        .context("could not attach file")?;
    let mut tool_definitions = if active.tools {
        tools.definitions()
    } else {
        Vec::new()
    };
    if active.tools && active.embedding_model.is_some() {
        tool_definitions.push(tools::search_code_definition());
    }

    let skills_eager = skill_library.eager_block_filtered(&disabled_skills);
    let mut system = prompt::build(
        active.tools,
        project.system_message().as_deref(),
        skills_eager.as_deref(),
    );
    let memory_snapshot = render_memory_snapshot(&database.list_memory()?);
    if !memory_snapshot.is_empty() {
        system.push_str("\n\n");
        system.push_str(&memory_snapshot);
    }
    let mut turn_messages = vec![Message::system(system)];
    turn_messages.push(Message::user_with_images(expanded.text, expanded.images));

    let user_message = Message::user(prompt);
    let mut tool_trail: Vec<Message> = Vec::new();
    // Files `patch_file` targeted this turn, so the code index can be refreshed once at the end.
    // Interactive chat reads the same set out of its revert snapshot, which `-p` has no use for.
    let mut edited: Vec<PathBuf> = Vec::new();
    let mut round = 0usize;
    let (assistant_message, final_usage, final_finish) = loop {
        round += 1;
        if round > MAX_TOOL_ROUNDS {
            anyhow::bail!("stopped after {MAX_TOOL_ROUNDS} tool rounds without a final answer");
        }

        let response = provider
            .chat(ChatRequest {
                model: active.model.clone(),
                messages: turn_messages.clone(),
                tools: tool_definitions.clone(),
            })
            .await
            .context("request failed")?;

        if response.tool_calls.is_empty() {
            break (
                Message::assistant(response.content),
                response.usage,
                response.finish_reason,
            );
        }

        let request_message = Message::tool_request(response.content, response.tool_calls.clone());
        turn_messages.push(request_message.clone());
        tool_trail.push(request_message);
        let spawn_calls: Vec<&ToolCall> = response
            .tool_calls
            .iter()
            .filter(|call| call.name == tools::SPAWN_AGENT_TOOL)
            .collect();
        let spawned_outputs =
            dispatch_spawn_agents(provider.as_ref(), &active.model, project, &spawn_calls).await;
        for call in &response.tool_calls {
            let tool_started = Instant::now();
            if call.name == tools::UPDATE_PLAN_TOOL
                && let Some(rendered) = tools::render_plan(&call.arguments)
            {
                print!(
                    "{}",
                    render::render_tool_call(&call.name, &call.arguments, ui)
                );
                println!("{rendered}");
            } else {
                print!(
                    "{}",
                    render::render_tool_call(&call.name, &call.arguments, ui)
                );
            }
            let output = if call.name == tools::ASK_USER_TOOL {
                println!("    skipped: ask_user is not available in non-interactive mode");
                "There is no user to ask in non-interactive mode. Proceed using your best \
                 judgment, or state your assumption in the final answer."
                    .to_string()
            } else if call.name == tools::SPAWN_AGENT_TOOL {
                spawned_outputs
                    .get(&call.id)
                    .map(|(output, _)| output.clone())
                    .unwrap_or_else(|| "Error: sub-agent result was missing".to_string())
            } else if call.name == tools::SEARCH_CODE_TOOL {
                match active.embedding_model.as_deref() {
                    Some(embedding_model) => {
                        dispatch_search_code(
                            provider.as_ref(),
                            embedding_model,
                            database,
                            project,
                            &call.arguments,
                        )
                        .await
                    }
                    None => "Error: this profile has no embedding_model configured; search_code \
                             is unavailable"
                        .to_string(),
                }
            } else if is_memory_tool(&call.name) {
                dispatch_memory_tool(database, &call.name, &call.arguments)
            } else if tools.requires_confirmation_for(&call.name, &call.arguments) && !auto_approve
            {
                println!("    denied: non-interactive mode (pass --auto-approve to allow)");
                "The user declined to run this command (non-interactive mode).".to_string()
            } else {
                if call.name == tools::PATCH_FILE_TOOL
                    && let Some(target) = tools::patch_target(project.root(), &call.arguments)
                    && !edited.contains(&target)
                {
                    edited.push(target);
                }
                tools.dispatch(call).await
            };
            let elapsed = spawned_outputs
                .get(&call.id)
                .map(|(_, elapsed)| *elapsed)
                .unwrap_or_else(|| tool_started.elapsed());
            if !output.starts_with("Error: ") && !output.is_empty() {
                print!(
                    "{}",
                    render::render_tool_output(&preview_output(&output), ui)
                );
            }
            println!("{}", ui.tool_outcome(&output, elapsed));
            let result_message = Message::tool_result(&call.id, output);
            turn_messages.push(result_message.clone());
            tool_trail.push(result_message);
        }
    };

    println!(
        "\n{}",
        markdown::Renderer::for_stdout().render_block(&assistant_message.content)
    );

    let final_answer = assistant_message.content.clone();
    let mut turn_record = Vec::with_capacity(tool_trail.len() + 2);
    turn_record.push(user_message);
    turn_record.append(&mut tool_trail);
    turn_record.push(assistant_message);

    let mut session = database.create_session(provider.name(), &active.model)?;
    database.save_turn(
        &session.id,
        &turn_record,
        &final_usage,
        &active.model,
        &final_finish,
    )?;
    database.rename_session(&session.id, &make_title(title_source))?;
    session.title = make_title(title_source);

    // Only after the turn is safely persisted: a refresh failure must never cost the exchange.
    report_index_refresh(
        refresh_index_for_paths(provider.as_ref(), &active, database, project, edited).await,
    );

    let title_response = provider
        .chat(ChatRequest {
            model: active.model.clone(),
            messages: vec![
                Message::system(
                    "Create a concise title of at most 6 words for this conversation. Return only the title without quotes or punctuation.",
                ),
                Message::user(title_source),
                Message::assistant(final_answer),
            ],
            tools: Vec::new(),
        })
        .await;
    match title_response {
        Ok(response) => {
            let title = clean_title(&response.content);
            if !title.is_empty() {
                database.save_generated_title(
                    &session.id,
                    &title,
                    &response.usage,
                    &active.model,
                    &response.finish_reason,
                )?;
            }
        }
        Err(error) => eprintln!(
            "{}",
            ui.style(
                &format!("Could not generate session title: {error:#}"),
                &[Style::Red]
            )
        ),
    }

    eprintln!(
        "\nTo resume this session: kamui -r {}",
        short_id(&session.id)
    );

    // Nothing outlives a single -p invocation: a background job has no way to be checked on or
    // stopped once the process exits.
    tools::kill_all_jobs(&tools.jobs());

    Ok(())
}

/// A background task that animates a single-line braille spinner until told to stop.
struct PlainSpinner {
    stop: Arc<Notify>,
    handle: JoinHandle<()>,
    width: usize,
}

/// The waiting indicator for a turn: inline on the scrollback, a footer animation in the
/// fullscreen TUI (the frea-inspired loading state), or nothing when output is piped.
enum Spinner {
    None,
    Plain(PlainSpinner),
    Tui,
}

fn start_spinner(label: &'static str, ui: Ui, chat_ui: &mut ChatUi) -> Spinner {
    if !ui.interactive() {
        return Spinner::None;
    }
    if chat_ui.is_fullscreen() {
        if chat_ui.thinking_start(label).is_ok() {
            return Spinner::Tui;
        }
        return Spinner::None;
    }
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let stop = Arc::new(Notify::new());
    let stop_task = stop.clone();
    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(80));
        let mut frame = 0usize;
        loop {
            tokio::select! {
                _ = stop_task.notified() => break,
                _ = interval.tick() => {
                    print!(
                        "\r{} {}",
                        FRAMES[frame % FRAMES.len()],
                        ui.style(label, &[Style::Dim])
                    );
                    let _ = io::stdout().flush();
                    frame += 1;
                }
            }
        }
    });
    Spinner::Plain(PlainSpinner {
        stop,
        handle,
        width: label.chars().count() + 2,
    })
}

impl Spinner {
    async fn finish(self, chat_ui: &mut ChatUi) {
        match self {
            Spinner::None => {}
            Spinner::Plain(spinner) => {
                spinner.stop.notify_one();
                let _ = spinner.handle.await;
                // Erase the spinner line so the response starts on a clean line.
                print!("\r{}\r", " ".repeat(spinner.width));
                let _ = io::stdout().flush();
            }
            Spinner::Tui => chat_ui.thinking_stop().await,
        }
    }
}

/// Resolves when the keyboard hub raises an interrupt; never fires in plain mode.
async fn wait_interrupt(interrupt: &Option<Arc<tokio::sync::Notify>>) {
    match interrupt {
        Some(notify) => notify.notified().await,
        None => std::future::pending().await,
    }
}

/// Stop the spinner if it is still running. Safe to call repeatedly.
async fn stop_spinner(spinner: &mut Spinner, chat_ui: &mut ChatUi) {
    let spinner = std::mem::replace(spinner, Spinner::None);
    spinner.finish(chat_ui).await;
}

fn input_channel() -> mpsc::UnboundedReceiver<String> {
    let (sender, receiver) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        loop {
            let mut input = String::new();
            match io::stdin().read_line(&mut input) {
                Ok(0) | Err(_) => break,
                Ok(_) if sender.send(input).is_err() => break,
                Ok(_) => {}
            }
        }
    });
    receiver
}

#[allow(clippy::too_many_arguments)]
fn handle_command(
    input: &str,
    provider: &dyn Provider,
    context_window: Option<u64>,
    database: &Database,
    session: &mut Option<Session>,
    messages: &mut Vec<Message>,
    always_allowed: &mut HashSet<String>,
    last_turn_snapshot: &mut Option<HashMap<PathBuf, Option<String>>>,
    prices: &Prices,
    mut tui: Option<&mut ChatUi>,
) -> Result<()> {
    let (command, argument) = input.split_once(' ').unwrap_or((input, ""));
    let argument = argument.trim();
    // Command output is buffered and flushed once at the end: fullscreen mode renders it as
    // a single transcript notice, plain mode keeps direct stdout. Helpers append to the same
    // buffer so nothing ever prints raw into a frame ratatui owns.
    let mut out_buf = String::new();
    macro_rules! out {
        () => { out_buf.push('\n') };
        ($($arg:tt)*) => { out_buf.push_str(&format!($($arg)*)) };
    }

    match command {
        "/help" => print_help(&mut out_buf),
        "/new" => {
            *session = None;
            messages.clear();
            always_allowed.clear();
            *last_turn_snapshot = None;
            out!("Started a new chat. It will be saved after the first response.\n");
        }
        "/sessions" => {
            let sessions = database.list_sessions()?;
            if sessions.is_empty() {
                out!("No saved sessions.\n");
            } else {
                for item in sessions {
                    let marker = if session
                        .as_ref()
                        .is_some_and(|session| item.id == session.id)
                    {
                        "*"
                    } else {
                        " "
                    };
                    out!(
                        "{marker} {}  {}  {:<40} {:>3} messages  {:>8} tokens",
                        short_id(&item.id),
                        format_timestamp(item.updated_at),
                        item.title,
                        item.message_count,
                        item.total_tokens
                    );
                }
                out!();
            }
        }
        "/resume" => {
            let resumed = resolve_session(database, argument)?;
            if resumed.provider != provider.name() {
                anyhow::bail!(
                    "session uses provider '{}', but '{}' is active",
                    resumed.provider,
                    provider.name()
                );
            }
            *messages = database.load_messages(&resumed.id)?;
            out!("Resumed: {} ({})\n", resumed.title, short_id(&resumed.id));
            // Note: Plan Mode restore is handled by the main loop's plan_mode state;
            // /resume via handle_command is not the startup resume path, so we don't
            // rehydrate here — the caller would need &mut plan_mode.
            *session = Some(resumed);
            if let Some(ui) = tui.as_deref_mut() {
                // Replay the last few user/assistant turns as transcript cards; raw text
                // would print straight into the frame the TUI owns.
                let skip = messages.len().saturating_sub(10);
                for message in &messages[skip..] {
                    match message.role {
                        Role::User => ui.user(&message.content)?,
                        Role::Assistant if !message.content.is_empty() => {
                            ui.assistant_update(&message.content)?
                        }
                        _ => {}
                    }
                }
            } else {
                print_history_preview(messages);
            }
        }
        "/delete" => {
            let target = resolve_session(database, argument)?;
            database.delete_session(&target.id)?;
            out!("Deleted: {}\n", target.title);
            if session
                .as_ref()
                .is_some_and(|session| target.id == session.id)
            {
                *session = None;
                messages.clear();
                always_allowed.clear();
                *last_turn_snapshot = None;
                out!("Started a new chat. It will be saved after the first response.\n");
            }
        }
        "/rename" => {
            let (id_prefix, new_title) =
                argument.split_once(char::is_whitespace).unwrap_or(("", ""));
            let new_title = new_title.trim();
            if id_prefix.is_empty() || new_title.is_empty() {
                anyhow::bail!("usage: /rename <id> <new title>");
            }
            let target = resolve_session(database, id_prefix.trim())?;
            database.rename_session(&target.id, new_title)?;
            if let Some(active) = session.as_mut()
                && active.id == target.id
            {
                active.title = new_title.to_string();
            }
            out!("Renamed {} to: {new_title}\n", short_id(&target.id));
        }
        "/search" => {
            if argument.is_empty() {
                anyhow::bail!("usage: /search <text>");
            }
            let hits = database.search_messages(argument, 20)?;
            if hits.is_empty() {
                out!("No messages matched \"{argument}\".\n");
            } else {
                for hit in hits {
                    let speaker = match hit.role.as_str() {
                        "user" => "You",
                        "assistant" => "Assistant",
                        "system" => "System",
                        _ => "?",
                    };
                    out!(
                        "{}  {}  {:<30}  {speaker}: {}",
                        short_id(&hit.session_id),
                        format_timestamp(hit.created_at),
                        truncate(&hit.title, 30),
                        make_snippet(&hit.content, argument),
                    );
                }
                println!();
            }
        }
        "/stats" => match session.as_ref() {
            Some(session) => print_stats(database, session, context_window, prices, &mut out_buf)?,
            None => out!("This chat has no saved messages yet.\n"),
        },
        "/usage" => print_usage_report(database, prices, &mut out_buf)?,
        "/memory" => {
            let entries = database.list_memory()?;
            if entries.is_empty() {
                out!("Nothing remembered yet.\n");
            } else {
                out!("Remembered facts:");
                for entry in &entries {
                    out!("- {}", entry.content);
                }
                out!("\nUse /forget <text> or /forget all.\n");
            }
        }
        "/forget" => {
            if argument.is_empty() {
                anyhow::bail!("usage: /forget <text> or /forget all");
            }
            if argument.eq_ignore_ascii_case("all") {
                let count = database.clear_memory()?;
                out!("Forgot all {count} remembered fact(s).\n");
            } else if database.forget(argument)? {
                out!("Forgot the fact matching \"{argument}\".\n");
            } else {
                out!(
                    "No remembered fact matches \"{argument}\", or the text matches more than \
                     one. Use /memory to see exact wording.\n"
                );
            }
        }
        _ => out!("Unknown command. Type /help for available commands.\n"),
    }

    if !out_buf.is_empty() {
        std::fs::write(
            "/tmp/opencode/flush_debug.txt",
            format!(
                "len={} nl={} tui={}",
                out_buf.len(),
                out_buf.matches('\n').count(),
                tui.is_some()
            ),
        )
        .ok();
        match tui {
            Some(ui) => ui.notice(out_buf.trim_end())?,
            None => print!("{out_buf}"),
        }
    }

    Ok(())
}

fn resolve_session(database: &Database, id_prefix: &str) -> Result<Session> {
    if id_prefix.is_empty() {
        anyhow::bail!("a session ID is required");
    }
    database
        .find_session(id_prefix)?
        .with_context(|| format!("session '{id_prefix}' was not found or is ambiguous"))
}

fn print_stats(
    database: &Database,
    session: &Session,
    context_window: Option<u64>,
    prices: &Prices,
    out: &mut String,
) -> Result<()> {
    let stats = database.session_stats(&session.id)?;
    let _ = writeln!(out, "\nSession:       {}", session.title);
    let _ = writeln!(out, "Requests:      {}", stats.request_count);
    let _ = writeln!(out, "Input tokens:  {}", stats.input_tokens);
    let _ = writeln!(out, "Output tokens: {}", stats.output_tokens);
    let _ = writeln!(out, "Total tokens:  {}", stats.total_tokens);
    // Cost is opt-in end to end: with no `[pricing]` configured there is no line, no zero, and not
    // even an extra query — the report stays exactly what it has always been.
    let mut unpriced = false;
    if let Some((cost, has_unpriced)) = session_cost(database, &session.id, prices)? {
        unpriced |= has_unpriced;
        let _ = writeln!(out, "Cost:          {cost}");
    }
    if stats.cached_tokens > 0 {
        let percent = if stats.input_tokens > 0 {
            (stats.cached_tokens as f64 / stats.input_tokens as f64 * 100.0).min(100.0)
        } else {
            0.0
        };
        let _ = writeln!(
            out,
            "Cached tokens: {} ({percent:.0}%)",
            stats.cached_tokens
        );
    }
    if let (Some(last_input), Some(window)) = (stats.last_input_tokens, context_window) {
        let percent = last_input as f64 / window as f64 * 100.0;
        print!("Last context:  {last_input}/{window} ({percent:.1}%)");
        if let Some(cached) = stats.last_cached_tokens.filter(|cached| *cached > 0) {
            let cached_percent = (cached as f64 / last_input as f64 * 100.0).min(100.0);
            print!(" | Cached: {cached} ({cached_percent:.0}%)");
        }
        let _ = writeln!(out,);
    }
    let by_model = database.model_stats(&session.id)?;
    if by_model.len() > 1 {
        let _ = writeln!(out, "\n--- Per model ---");
        for m in &by_model {
            // Priced from this row's own (chat-only) tokens, so each line is honest about exactly
            // the numbers standing beside it.
            let cell = cost_cell(
                prices,
                [(Some(m.model.as_str()), m.input_tokens, m.output_tokens)],
            );
            if let Some((_, has_unpriced)) = &cell {
                unpriced |= has_unpriced;
            }
            let _ = writeln!(
                out,
                "{}",
                model_row(m, cell.as_ref().map(|(cost, _)| cost.as_str()))
            );
        }
    }
    if unpriced {
        let _ = writeln!(out, "\n{UNPRICED_NOTE}");
    }
    let _ = writeln!(out,);
    Ok(())
}

/// Explains a `+` or `unpriced` cell, printed only under a report that produced one.
const UNPRICED_NOTE: &str =
    "Some usage came from a model with no price in [pricing.models]; it is excluded, not free.";

/// This session's total cost, or `None` when no prices are configured. Sums every usage kind, not
/// just `kind = 'chat'`: the token totals it sits under include title generation, and so does the
/// bill. The query itself is skipped when there is nothing to price.
fn session_cost(
    database: &Database,
    session_id: &str,
    prices: &Prices,
) -> Result<Option<(String, bool)>> {
    if prices.is_empty() {
        return Ok(None);
    }
    let tokens = database.session_model_tokens(session_id)?;
    Ok(cost_cell(prices, model_token_rows(&tokens)))
}

/// The cost cell for one report row, plus whether any of that row's tokens could not be priced.
/// `None` when the user configured no prices at all — which is how the column stays absent
/// entirely, rather than showing a blank or a zero that would read as free.
fn cost_cell<'a>(
    prices: &Prices,
    rows: impl IntoIterator<Item = (Option<&'a str>, i64, i64)>,
) -> Option<(String, bool)> {
    if prices.is_empty() {
        return None;
    }
    let tally = prices.tally(rows);
    Some((prices.format(&tally), tally.has_unpriced()))
}

/// Adapt stored per-model token sums to what `cost_cell` takes.
fn model_token_rows(
    tokens: &[storage::ModelTokens],
) -> impl Iterator<Item = (Option<&str>, i64, i64)> {
    tokens
        .iter()
        .map(|row| (row.model.as_deref(), row.input_tokens, row.output_tokens))
}

/// The per-model token rows recorded for one period, or nothing when that period has none.
fn tokens_for<'a>(
    models: &'a HashMap<String, Vec<storage::ModelTokens>>,
    period: &str,
) -> &'a [storage::ModelTokens] {
    models.get(period).map_or(&[][..], Vec::as_slice)
}

/// One `/stats` per-model line. The cost cell is appended only when prices are configured, so the
/// line is unchanged for a user who configured none.
fn model_row(stat: &storage::ModelStat, cost: Option<&str>) -> String {
    let mut line = format!(
        "  {:<24} {:>3} req  {:>8} in  {:>8} out  {:>8} total",
        stat.model, stat.request_count, stat.input_tokens, stat.output_tokens, stat.total_tokens
    );
    if stat.cached_tokens > 0 {
        line.push_str(&format!("  {:>8} cached", stat.cached_tokens));
    }
    if let Some(cost) = cost {
        line.push_str(&format!("  {cost:>12}"));
    }
    line
}

/// One `/usage` line, with the same opt-in cost cell as `model_row`.
fn usage_row(period: &storage::UsagePeriod, cost: Option<&str>) -> String {
    let mut line = format!(
        "  {:<10} {:>4} req  {:>10} in  {:>10} out  {:>10} total",
        period.period,
        period.request_count,
        period.input_tokens,
        period.output_tokens,
        period.total_tokens
    );
    if period.cached_tokens > 0 {
        let percent = if period.input_tokens > 0 {
            (period.cached_tokens as f64 / period.input_tokens as f64 * 100.0).min(100.0)
        } else {
            0.0
        };
        line.push_str(&format!(
            "  {:>8} cached ({percent:.0}%)",
            period.cached_tokens
        ));
    }
    if let Some(cost) = cost {
        line.push_str(&format!("  {cost:>12}"));
    }
    line
}

/// Fold the older, un-summarized messages into a fresh running summary via a non-streaming request.
/// Returns the new summary, the new summarized-up-to index, and how many messages were folded in, or
/// `None` when there is nothing new worth summarizing.
async fn run_compaction(
    provider: &dyn Provider,
    model: &str,
    messages: &[Message],
    summary: Option<&str>,
    summarized_upto: usize,
) -> Result<Option<(String, usize, usize)>> {
    let Some(cutoff) = compaction::cutoff(messages.len(), summarized_upto) else {
        return Ok(None);
    };
    let rendered = compaction::render(&messages[summarized_upto..cutoff]);
    let request = compaction::summary_request(model, summary, &rendered);
    let response = provider.chat(request).await?;
    Ok(Some((
        response.content.trim().to_string(),
        cutoff,
        cutoff - summarized_upto,
    )))
}

/// List profiles, or switch to a named one and persist the choice. Rebuilding the provider swaps the
/// base URL and API key; the model and context window follow the profile.
#[allow(clippy::too_many_arguments)]
fn switch_profile<F>(
    name: &str,
    config: &Config,
    active: &mut Profile,
    provider: &mut Box<dyn Provider>,
    context_window: &mut Option<u64>,
    database: &Database,
    build_provider: &F,
    mut tui: Option<&mut ChatUi>,
) -> Result<()>
where
    F: Fn(&Profile) -> Box<dyn Provider>,
{
    macro_rules! out {
        ($($arg:tt)*) => {
            if let Some(ui) = tui.as_deref_mut() {
                ui.notice(&format!($($arg)*))?;
            } else {
                println!($($arg)*);
            }
        };
    }
    if name.is_empty() {
        out!("Profiles:");
        for profile in &config.profiles {
            let marker = if profile.name == active.name {
                "*"
            } else {
                " "
            };
            let tools = if profile.tools { "" } else { "  [no tools]" };
            out!(
                "{marker} {:<16} {:<22} {}{tools}",
                profile.name,
                profile.model,
                profile.base_url
            );
        }
        println!();
        return Ok(());
    }

    match config.find(name) {
        Some(profile) => {
            *active = profile.clone();
            *provider = build_provider(profile);
            *context_window = profile.context_window;
            database.set_setting(ACTIVE_PROFILE_KEY, &profile.name)?;
            out!("Now using {} ({}).\n", profile.model, profile.name);
        }
        None => out!("Unknown profile '{name}'. Type /model to list profiles.\n"),
    }
    Ok(())
}

fn shutdown(
    database: &Database,
    session: Option<&Session>,
    context_window: Option<u64>,
    jobs: &tools::JobRegistry,
    prices: &Prices,
) -> Result<()> {
    // Nothing should outlive the process: a still-running background job has no way to be
    // checked on or stopped once Kamui exits.
    tools::kill_all_jobs(jobs);
    let mut buf = String::new();
    if let Some(session) = session {
        print_stats(database, session, context_window, prices, &mut buf)?;
        buf.push_str(&format!(
            "To resume this session: kamui -r {}\n",
            short_id(&session.id)
        ));
    }
    buf.push_str("Goodbye\n");
    // In fullscreen this is the final frame teardown companion; printing raw after leaving
    // the alt screen is correct here because shutdown drops the TUI before returning.
    print!("{buf}");
    Ok(())
}

/// Snapshot a `patch_file` call's target before it is dispatched, so the turn can be reverted if
/// cancelled. First-touch-wins: if this path was already snapshotted earlier in the turn, the
/// existing entry (the file's state *before this turn*) is kept, not overwritten with an
/// intermediate edit. If the file exists but cannot be read as UTF-8, it is left unsnapshotted
/// rather than guessing — `patch_file` itself will fail the same way, so nothing gets written and
/// there is nothing to revert for that path.
fn snapshot_patch_target(
    root: &Path,
    arguments: &str,
    snapshot: &mut HashMap<PathBuf, Option<String>>,
) {
    let Some(target) = tools::patch_target(root, arguments) else {
        return;
    };
    if snapshot.contains_key(&target) {
        return;
    }
    if target.is_file() {
        if let Ok(content) = std::fs::read_to_string(&target) {
            snapshot.insert(target, Some(content));
        }
    } else {
        snapshot.insert(target, None);
    }
}

/// Revert every file in a turn's patch snapshot back to its pre-turn state: restore the original
/// content, or delete a file that did not exist before the turn. Best-effort — a failure on one
/// file is reported but does not stop the rest from being reverted. Returns how many files were
/// successfully reverted.
fn revert_snapshot(snapshot: &HashMap<PathBuf, Option<String>>) -> usize {
    let mut reverted = 0;
    for (path, original) in snapshot {
        let result = match original {
            Some(content) => tools::write_atomic(path, content),
            None => match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            },
        };
        match result {
            Ok(()) => reverted += 1,
            Err(error) => eprintln!("    ! could not revert {}: {error:#}", display_path(path)),
        }
    }
    reverted
}

/// If this turn touched any files, revert them and report how many. Called right before
/// abandoning a cancelled or failed turn so a Ctrl+C (or a dropped request) never leaves a
/// multi-file edit half-applied with no trace in session history.
fn revert_on_cancel(snapshot: &HashMap<PathBuf, Option<String>>) {
    if snapshot.is_empty() {
        return;
    }
    let reverted = revert_snapshot(snapshot);
    println!("(reverted {reverted} file(s) changed before this turn was interrupted)");
}

fn print_history_preview(messages: &[Message]) {
    if messages.is_empty() {
        println!("No previous messages.\n");
        return;
    }

    let start = messages.len().saturating_sub(RESUME_PREVIEW_MESSAGES);
    if start > 0 {
        println!("... {start} earlier messages omitted\n");
    }
    for message in &messages[start..] {
        let speaker = match message.role_name() {
            "user" => "You",
            "assistant" => "Assistant",
            "system" => "System",
            "tool" => "Tool",
            _ => "?",
        };
        let body = if message.content.is_empty() && !message.tool_calls.is_empty() {
            let names: Vec<&str> = message
                .tool_calls
                .iter()
                .map(|call| call.name.as_str())
                .collect();
            format!("(requested tools: {})", names.join(", "))
        } else {
            message.content.clone()
        };
        println!("{speaker}:\n{body}\n");
    }
    println!("--- End of history ---\n");
}

/// Refresh the fullscreen sidebar rail (opencode-style): session identity on top, model and
/// context usage beneath. Called at startup and after every completed round so the context
/// figure tracks the live conversation.
/// Pushes recent sessions into the Ctrl+S switcher (id -> title labels).
fn refresh_session_source(database: &Database, hub: &InputHub) {
    if let Ok(sessions) = database.list_sessions() {
        hub.set_sessions(
            sessions
                .into_iter()
                .take(15)
                .map(|session| (session.id.clone(), session.title))
                .collect(),
        );
    }
}

fn update_sidebar(
    chat_ui: &mut ChatUi,
    session: Option<&Session>,
    model: &str,
    project: &ProjectContext,
    last_input_tokens: Option<u64>,
    context_window: Option<u64>,
    last_turn: Option<String>,
) {
    if !chat_ui.is_fullscreen() {
        return;
    }
    let mut entries = vec![(
        "Session".to_string(),
        match session {
            Some(session) => session.title.clone(),
            None => "New chat".to_string(),
        },
    )];
    if let Some(session) = session {
        entries.push(("Id".to_string(), short_id(&session.id).to_string()));
    }
    entries.push(("Model".to_string(), model.to_string()));
    entries.push(("Project".to_string(), display_path(project.root())));
    let context_line = match (last_input_tokens, context_window) {
        (Some(tokens), Some(window)) => {
            format!(
                "{tokens} tokens ({:.1}% of {window})",
                tokens as f64 / window as f64 * 100.0
            )
        }
        (Some(tokens), None) => format!("{tokens} tokens"),
        (None, _) => "\u{2014}".to_string(),
    };
    entries.push(("Context".to_string(), context_line));
    // Status-bar badge: compact token count with context pressure for the amber threshold.
    match last_input_tokens {
        Some(tokens) => {
            let pct: u8 = context_window
                .map(|window| ((tokens as f64 / window as f64) * 100.0).min(100.0).round() as u8)
                .unwrap_or(0);
            let text = if tokens >= 1000 {
                format!("{:.1}k tok", tokens as f64 / 1000.0)
            } else {
                format!("{tokens} tok")
            };
            let _ = chat_ui.set_token_badge(Some((text, pct)));
        }
        None => {
            let _ = chat_ui.set_token_badge(None);
        }
    }
    if let Some(last_turn) = last_turn {
        // One metric per line reads better in the narrow rail than a pipe-separated row.
        entries.push(("Last turn".to_string(), last_turn.replace(" | ", "\n")));
    }
    let _ = chat_ui.set_sidebar(entries);
}

#[allow(clippy::too_many_arguments)]
fn format_usage(
    input: u64,
    output: u64,
    total: u64,
    cached: u64,
    finish_reason: &str,
    ttft: Option<Duration>,
    elapsed: Duration,
    context_window: Option<u64>,
) -> String {
    let mut line = format!("Tokens: {input} input + {output} output = {total} total");
    if cached > 0 {
        let percent = if input > 0 {
            (cached as f64 / input as f64 * 100.0).min(100.0)
        } else {
            0.0
        };
        line.push_str(&format!(" | Cached: {cached} ({percent:.0}%)"));
    }
    if let Some(window) = context_window {
        let percent = input as f64 / window as f64 * 100.0;
        line.push_str(&format!(" | Context: {percent:.1}%"));
    }
    if let Some(ttft) = ttft {
        line.push_str(&format!(" | TTFT: {}", format_duration(ttft)));
    }
    line.push_str(&format!(
        " | Time: {} | Finish: {finish_reason}",
        format_duration(elapsed)
    ));
    line
}

fn format_tool_outcome(output: &str, elapsed: Duration) -> String {
    match output.strip_prefix("Error: ") {
        Some(error) => format!("failed · {} · {error}", format_duration(elapsed)),
        None => format!(
            "completed · {} · {} chars",
            format_duration(elapsed),
            output.chars().count()
        ),
    }
}

/// Fold one agent-loop round's usage into the turn total: output tokens accumulate across every
/// round, while the input count tracks the final round so it still reflects the context that was
/// sent. Total is the last input plus all output generated during the turn.
fn accumulate_usage(total: &mut Usage, round: &Usage) {
    total.completion_tokens += round.completion_tokens;
    total.prompt_tokens = round.prompt_tokens;
    total.cached_tokens = round.cached_tokens;
    total.total_tokens = total.prompt_tokens + total.completion_tokens;
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();
    if seconds < 1.0 {
        format!("{}ms", duration.as_millis())
    } else {
        format!("{seconds:.1}s")
    }
}

fn make_title(input: &str) -> String {
    let mut title: String = input.chars().take(40).collect();
    if input.chars().count() > 40 {
        title.push_str("...");
    }
    title
}

fn clean_title(title: &str) -> String {
    title
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .trim_matches(['"', '\'', '.', ':'])
        .chars()
        .take(60)
        .collect()
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Render a path for display, trimming the Windows verbatim prefix that `canonicalize` adds
/// (`\\?\C:\...` and `\\?\UNC\server\share`). The canonical form stays in use internally for
/// path-safety checks.
pub(crate) fn display_path(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(plain) = text.strip_prefix(r"\\?\") {
        plain.to_string()
    } else {
        text
    }
}

#[derive(serde::Deserialize)]
struct AskUserArguments {
    question: String,
    #[serde(default)]
    options: Vec<String>,
}

/// Print an `ask_user` question (with numbered options, if any) and read one line of stdin as the
/// answer. If the user types a number that matches an offered option, the option's text is
/// returned instead of the raw digit, so the model gets a proper answer either way; any other
/// text (a number out of range, or free-form text when no options were offered) is returned as
/// typed. Returns an `Error: ...` string, not an `Err`, for bad JSON — same convention as
/// `ToolRegistry::dispatch` — so the model can recover on the next round.
async fn read_approval_line(
    input_rx: &mut Option<mpsc::UnboundedReceiver<String>>,
    use_tui: bool,
    hub: Option<&mut InputHub>,
    title: &str,
    body: String,
) -> Option<String> {
    if use_tui {
        let hub = hub.expect("tui implies hub");
        hub.open_permission_modal(title, body);
        let answer = hub.request_line().await;
        hub.close_permission_modal();
        answer
    } else {
        input_rx.as_mut().unwrap().recv().await
    }
}

async fn ask_user(
    input_rx: &mut Option<mpsc::UnboundedReceiver<String>>,
    use_tui: bool,
    arguments: &str,
    hub: Option<&mut InputHub>,
) -> Result<String> {
    let arguments: AskUserArguments = match serde_json::from_str(arguments) {
        Ok(arguments) => arguments,
        Err(error) => return Ok(format!("Error: invalid ask_user arguments: {error}")),
    };
    if arguments.question.trim().is_empty() {
        return Ok("Error: ask_user requires a non-empty 'question' argument".to_string());
    }

    println!("  ? {}", arguments.question);
    for (index, option) in arguments.options.iter().enumerate() {
        println!("    {}. {option}", index + 1);
    }
    print!("    > ");
    io::stdout().flush()?;

    let answer = if use_tui {
        let hub = hub.expect("tui implies hub");
        hub.request_line().await.unwrap_or_default()
    } else {
        input_rx.as_mut().unwrap().recv().await.unwrap_or_default()
    };
    let answer = answer.trim();
    let resolved = answer
        .parse::<usize>()
        .ok()
        .and_then(|number| number.checked_sub(1))
        .and_then(|index| arguments.options.get(index))
        .map(String::as_str)
        .unwrap_or(answer);
    Ok(resolved.to_string())
}

#[derive(serde::Deserialize)]
struct SpawnAgentArguments {
    prompt: String,
}

/// Dispatch `spawn_agent`, converting any failure to an `Error: ...` string (same convention as
/// `ToolRegistry::dispatch`) so a misbehaving sub-agent fails the tool call, not the whole turn.
async fn dispatch_spawn_agent(
    provider: &dyn Provider,
    model: &str,
    project: &ProjectContext,
    arguments: &str,
) -> String {
    match run_spawned_agent(provider, model, project, arguments).await {
        Ok(output) => output,
        Err(error) => format!("Error: {error:#}"),
    }
}

/// Run independent `spawn_agent` calls concurrently while preserving the original tool-call order
/// in the map consumed by the parent loop. Batching caps provider fan-out and rate-limit pressure.
async fn dispatch_spawn_agents(
    provider: &dyn Provider,
    model: &str,
    project: &ProjectContext,
    calls: &[&ToolCall],
) -> HashMap<String, (String, Duration)> {
    let mut outputs = HashMap::with_capacity(calls.len());
    for batch in calls.chunks(MAX_CONCURRENT_SUB_AGENTS) {
        let futures = batch.iter().map(|call| async move {
            let started = Instant::now();
            (
                call.id.clone(),
                (
                    dispatch_spawn_agent(provider, model, project, &call.arguments).await,
                    started.elapsed(),
                ),
            )
        });
        outputs.extend(join_all(futures).await);
    }
    outputs
}

/// Run an isolated sub-agent to completion and return just its final answer: a fresh system
/// prompt and no shared history with the parent conversation, so the parent's context is not
/// polluted by the sub-agent's own exploration trace. Scoped to `ToolRegistry::read_only` — none
/// of those tools ever require confirmation, so there is no approval flow to reproduce here, and
/// `tool_definitions_only` omits `spawn_agent` itself, so a sub-agent cannot recurse.
async fn run_spawned_agent(
    provider: &dyn Provider,
    model: &str,
    project: &ProjectContext,
    arguments: &str,
) -> Result<String> {
    let arguments: SpawnAgentArguments = serde_json::from_str(arguments)
        .context("spawn_agent requires a 'prompt' string argument")?;
    if arguments.prompt.trim().is_empty() {
        anyhow::bail!("spawn_agent requires a non-empty 'prompt' argument");
    }

    let sub_tools = tools::ToolRegistry::read_only(project.root().to_path_buf());
    let tool_definitions = sub_tools.tool_definitions_only();
    let system = prompt::build(true, project.system_message().as_deref(), None);
    let mut messages = vec![Message::system(system), Message::user(&arguments.prompt)];

    let mut round = 0usize;
    loop {
        round += 1;
        if round > MAX_TOOL_ROUNDS {
            anyhow::bail!(
                "sub-agent stopped after {MAX_TOOL_ROUNDS} tool rounds without a final answer"
            );
        }

        let response = provider
            .chat(ChatRequest {
                model: model.to_string(),
                messages: messages.clone(),
                tools: tool_definitions.clone(),
            })
            .await
            .context("sub-agent request failed")?;

        if response.tool_calls.is_empty() {
            return Ok(response.content);
        }

        messages.push(Message::tool_request(
            response.content,
            response.tool_calls.clone(),
        ));
        for call in &response.tool_calls {
            let output = sub_tools.dispatch(call).await;
            messages.push(Message::tool_result(&call.id, output));
        }
    }
}

/// Rebuild the semantic-search index: walk the project the same `.gitignore`-aware way `grep`/
/// `glob` do, skip any file whose content hash matches what was indexed last time, chunk and embed
/// the rest, and drop entries for files that no longer exist. Returns a one-line summary.
async fn run_index(
    provider: &dyn Provider,
    active: &Profile,
    database: &Database,
    project: &ProjectContext,
) -> Result<String> {
    let embedding_model = active.embedding_model.as_deref().context(
        "this profile has no embedding_model configured; set one under [provider] or \
         [profiles.*] in kamui.toml to use semantic search",
    )?;

    let root = project.root();
    let key = project.key();
    let mut seen = std::collections::HashSet::new();
    let mut indexed = 0usize;
    let mut skipped = 0usize;
    let mut chunk_total = 0usize;

    for path in tools::walk(root) {
        let relative = tools::relative_slug(root, &path);
        seen.insert(relative.clone());
        // Binary or otherwise unreadable-as-text files are simply not indexable, same as grep.
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let hash = content_hash(&content);
        if database.indexed_file_is_current(&key, &relative, &hash, embedding_model)? {
            skipped += 1;
            continue;
        }

        chunk_total += index_file(
            provider,
            embedding_model,
            database,
            &key,
            &relative,
            &content,
            &hash,
        )
        .await?;
        indexed += 1;
    }

    // Anything indexed before but not seen on this walk no longer exists (or is now ignored).
    let mut removed = 0usize;
    for file in database.indexed_files(&key)? {
        if !seen.contains(&file.path) {
            database.delete_chunks_for_path(&key, &file.path)?;
            database.delete_indexed_file(&key, &file.path)?;
            removed += 1;
        }
    }

    Ok(format!(
        "Indexed {indexed} file(s) ({chunk_total} new chunks), skipped {skipped} unchanged, \
         removed {removed} deleted. {} chunk(s) total.",
        database.chunk_count(&key)?
    ))
}

/// Chunk, embed, and store one file's content for a project, replacing whatever was indexed for
/// that path before, and record the hash so a later run can skip it while it stays unchanged.
/// Returns how many chunks were written. Shared by `/index` and the post-turn refresh so both
/// store chunks the same way.
#[allow(clippy::too_many_arguments)]
async fn index_file(
    provider: &dyn Provider,
    embedding_model: &str,
    database: &Database,
    project_key: &str,
    relative: &str,
    content: &str,
    hash: &str,
) -> Result<usize> {
    let chunks = tools::chunk_text(content);
    let mut prepared = Vec::new();
    for batch in chunks.chunks(EMBEDDING_BATCH_SIZE) {
        let texts: Vec<String> = batch.iter().map(|(_, _, text)| text.clone()).collect();
        let embeddings = provider
            .embed(embedding_model, texts)
            .await
            .with_context(|| format!("failed to embed {relative}"))?;
        if embeddings.len() != batch.len() {
            anyhow::bail!(
                "embedding provider returned {} vector(s) for {} chunk(s) in {relative}",
                embeddings.len(),
                batch.len()
            );
        }
        for ((start, end, text), embedding) in batch.iter().cloned().zip(embeddings) {
            prepared.push(storage::NewCodeChunk {
                start_line: start,
                end_line: end,
                content: text,
                embedding,
            });
        }
    }
    let written = prepared.len();
    database.replace_file_index(project_key, relative, hash, embedding_model, &prepared)?;
    Ok(written)
}

/// Re-embed the files a completed turn edited, so `search_code` cannot answer with text that no
/// longer exists at the lines it reports.
///
/// Refresh only: a file the turn *created* is not added to the index on Kamui's own initiative.
/// Stale content in an already-indexed file is actively misleading, while a file missing from the
/// index is merely incomplete — and the startup staleness hint already surfaces it — so only the
/// former justifies spending the user's embedding budget unasked. Having run `/index` at least
/// once is what opts a project in; without that, or without an `embedding_model`, this does
/// nothing and costs nothing.
///
/// Best-effort by contract: callers report a failure and carry on, since the turn's real work is
/// already done and persisted, and `/index` can always rebuild.
async fn refresh_index_for_paths(
    provider: &dyn Provider,
    active: &Profile,
    database: &Database,
    project: &ProjectContext,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<usize> {
    let Some(embedding_model) = active.embedding_model.as_deref() else {
        return Ok(0);
    };
    let key = project.key();
    let root = project.root();
    let mut refreshed = 0;
    for path in paths {
        let relative = tools::relative_slug(root, &path);
        let Some(stored_hash) = database.indexed_file_hash(&key, &relative)? else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            // Gone, or no longer readable as text: dropping its chunks is cheaper than embedding
            // and safer than serving text nothing can be checked against.
            database.delete_chunks_for_path(&key, &relative)?;
            database.delete_indexed_file(&key, &relative)?;
            refreshed += 1;
            continue;
        };
        // A turn can edit a file and end up back at the indexed content — patched then reverted,
        // or an edit that cancels out. Nothing to re-embed then.
        let hash = content_hash(&content);
        if hash == stored_hash
            && database.indexed_file_is_current(&key, &relative, &hash, embedding_model)?
        {
            continue;
        }
        index_file(
            provider,
            embedding_model,
            database,
            &key,
            &relative,
            &content,
            &hash,
        )
        .await?;
        refreshed += 1;
    }
    Ok(refreshed)
}

/// Report the outcome of a post-turn index refresh on one line, saying nothing when there was
/// nothing to refresh.
fn report_index_refresh(outcome: Result<usize>) {
    match outcome {
        Ok(0) => {}
        Ok(count) => println!("(refreshed {count} file(s) in the code index)"),
        Err(error) => eprintln!("(index refresh failed: {error:#} — /index rebuilds it)"),
    }
}

/// How far the stored index has drifted from what is on disk, as counts of files that changed,
/// appeared, or disappeared since they were last indexed.
#[derive(Default, PartialEq, Eq, Debug)]
struct IndexStaleness {
    changed: usize,
    added: usize,
    removed: usize,
}

impl IndexStaleness {
    fn is_fresh(&self) -> bool {
        *self == Self::default()
    }

    /// A one-line summary listing only the non-zero counts, e.g. `3 changed, 1 new`.
    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.changed > 0 {
            parts.push(format!("{} changed", self.changed));
        }
        if self.added > 0 {
            parts.push(format!("{} new", self.added));
        }
        if self.removed > 0 {
            parts.push(format!("{} removed", self.removed));
        }
        parts.join(", ")
    }
}

/// Compare the stored index against the project tree, returning `None` when this project has never
/// been indexed (so nothing is reported to a user who has not opted into semantic search).
///
/// Deliberately cheap: it walks the tree and compares each file's mtime against when it was
/// indexed, rather than reading and hashing every file the way `/index` does. That makes it a hint
/// — a checkout can bump an mtime without changing content — but it costs no file reads, no
/// network, and no embedding spend at startup, and `/index` still does the authoritative hash
/// comparison before re-embedding anything.
fn index_staleness(
    database: &Database,
    project: &ProjectContext,
) -> Result<Option<IndexStaleness>> {
    let indexed: HashMap<String, i64> = database
        .indexed_files(&project.key())?
        .into_iter()
        .map(|file| (file.path, file.indexed_at))
        .collect();
    if indexed.is_empty() {
        return Ok(None);
    }

    let root = project.root();
    let mut staleness = IndexStaleness::default();
    let mut seen = HashSet::new();
    for path in tools::walk(root) {
        let relative = tools::relative_slug(root, &path);
        match indexed.get(&relative) {
            Some(indexed_at) => {
                if modified_at(&path).is_some_and(|modified| modified > *indexed_at) {
                    staleness.changed += 1;
                }
            }
            None => staleness.added += 1,
        }
        seen.insert(relative);
    }
    staleness.removed = indexed.keys().filter(|path| !seen.contains(*path)).count();

    Ok(Some(staleness))
}

/// A file's modification time as a Unix timestamp, or `None` when the platform or filesystem does
/// not report one — treated as "unchanged" rather than guessed at.
fn modified_at(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs() as i64)
}

/// A fast, non-cryptographic change-detection hash — good enough to decide whether a file needs
/// re-embedding, not a security primitive.
fn content_hash(content: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

#[derive(serde::Deserialize)]
struct SearchCodeArguments {
    query: String,
}

/// How many of the highest-scoring chunks `search_code` returns.
const SEARCH_CODE_RESULTS: usize = 8;

/// Dispatch `search_code`, converting any failure to an `Error: ...` string (same convention as
/// `ToolRegistry::dispatch`) so a bad query or a missing index fails the tool call, not the turn.
async fn dispatch_search_code(
    provider: &dyn Provider,
    embedding_model: &str,
    database: &Database,
    project: &ProjectContext,
    arguments: &str,
) -> String {
    match run_search_code(provider, embedding_model, database, project, arguments).await {
        Ok(output) => output,
        Err(error) => format!("Error: {error:#}"),
    }
}

async fn run_search_code(
    provider: &dyn Provider,
    embedding_model: &str,
    database: &Database,
    project: &ProjectContext,
    arguments: &str,
) -> Result<String> {
    let arguments: SearchCodeArguments = serde_json::from_str(arguments)
        .context("search_code requires a 'query' string argument")?;
    if arguments.query.trim().is_empty() {
        anyhow::bail!("search_code requires a non-empty 'query' argument");
    }
    if database.chunk_count(&project.key())? == 0 {
        anyhow::bail!("no code index found for this project; run /index first");
    }

    let mut query_embedding = provider
        .embed(embedding_model, vec![arguments.query.clone()])
        .await
        .context("failed to embed the query")?;
    let query_vector = query_embedding
        .pop()
        .context("provider returned no embedding for the query")?;
    let buckets = storage::lsh_probe_buckets(storage::embedding_signature(&query_vector));
    let chunks = database.candidate_chunks(&project.key(), &arguments.query, &buckets)?;
    if chunks.is_empty() {
        anyhow::bail!("no code index found for this project; run /index first");
    }

    let mut scored: Vec<(f32, storage::CodeChunk)> = chunks
        .into_iter()
        .map(|chunk| (cosine_similarity(&query_vector, &chunk.embedding), chunk))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.truncate(SEARCH_CODE_RESULTS);

    Ok(scored
        .into_iter()
        .map(|(score, chunk)| {
            format!(
                "{}:{}-{} (score={score:.2})\n{}",
                chunk.path, chunk.start_line, chunk.end_line, chunk.content
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n"))
}

/// Standard cosine similarity, in `[-1.0, 1.0]` for non-zero vectors (`0.0` for a mismatched or
/// zero vector, which should not occur for embeddings from the same model but is handled rather
/// than panicking on a division by zero).
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

/// Total bytes of stored memory content allowed before `remember` refuses to add more, keeping
/// the system prompt (which carries every entry on every request) from growing unbounded.
const MAX_MEMORY_BYTES: i64 = 4 * 1024;

/// Render every remembered fact as a system-prompt block, or an empty string when there is
/// nothing remembered yet (so callers can skip adding an empty section).
fn render_memory_snapshot(entries: &[storage::MemoryEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut text =
        "Remembered facts about the user (persist across sessions and projects):".to_string();
    for entry in entries {
        text.push_str("\n- ");
        text.push_str(&entry.content);
    }
    text
}

fn is_memory_tool(name: &str) -> bool {
    matches!(
        name,
        tools::REMEMBER_TOOL | tools::UPDATE_MEMORY_TOOL | tools::FORGET_TOOL
    )
}

// --- Plan Mode (ticket #9) ---

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlanStatus {
    Pending,
    Approved,
}

#[derive(Debug, Clone)]
struct PlanModeState {
    status: PlanStatus,
    plan_json: Option<String>,
}

fn looks_like_multi_step(input: &str) -> bool {
    // Heuristic: ≥3 bullet/numbered lines or explicit "step" mentions.
    let lines: Vec<&str> = input.lines().collect();
    let mut hits = 0usize;
    for line in &lines {
        let t = line.trim();
        if t.starts_with("- ")
            || t.starts_with("* ")
            || t.starts_with("- [")
            || (t.chars().next().is_some_and(|c| c.is_ascii_digit()) && t.contains(". "))
        {
            hits += 1;
        }
        if t.to_ascii_lowercase().contains("step") {
            hits += 1;
        }
    }
    hits >= 3
}

fn is_mutating_tool(name: &str) -> bool {
    matches!(
        name,
        tools::PATCH_FILE_TOOL | "run_command" | "command_status" | "stop_command"
    )
}

fn plan_mode_definitions(
    root: &std::path::Path,
    has_embedding: bool,
) -> Vec<crate::provider::ToolDefinition> {
    let mut defs = tools::ToolRegistry::plan_mode(root.to_path_buf()).definitions();
    // plan_mode() already includes update_plan; add ask_user/spawn_agent/memory via definitions()
    // which plan_mode's definitions() already includes. Add search_code if available.
    if has_embedding {
        defs.push(tools::search_code_definition());
    }
    defs
}

#[derive(serde::Deserialize)]
struct FactArguments {
    fact: String,
}

#[derive(serde::Deserialize)]
struct UpdateMemoryArguments {
    matching: String,
    fact: String,
}

#[derive(serde::Deserialize)]
struct ForgetArguments {
    matching: String,
}

/// Dispatch one of the memory pseudo-tools (`remember`/`update_memory`/`forget`), all of which
/// need `Database` directly rather than going through `ToolRegistry`. Synchronous and infallible
/// at the call site (it always returns a tool-result string, `Error: ...` on failure) so it slots
/// into the same position `tools.dispatch(call).await` would.
fn dispatch_memory_tool(database: &Database, name: &str, arguments: &str) -> String {
    let result = (|| -> Result<String> {
        match name {
            tools::REMEMBER_TOOL => {
                let arguments: FactArguments = serde_json::from_str(arguments)
                    .context("tool arguments were not valid JSON")?;
                let fact = arguments.fact.trim();
                if fact.is_empty() {
                    anyhow::bail!("remember requires a non-empty 'fact' argument");
                }
                let existing = database.total_memory_bytes()?;
                if existing + fact.len() as i64 > MAX_MEMORY_BYTES {
                    anyhow::bail!(
                        "memory is full ({existing}/{MAX_MEMORY_BYTES} bytes); use update_memory \
                         or forget to make room before adding more"
                    );
                }
                database.remember(fact)?;
                Ok(format!("remembered: {fact}"))
            }
            tools::UPDATE_MEMORY_TOOL => {
                let arguments: UpdateMemoryArguments = serde_json::from_str(arguments)
                    .context("tool arguments were not valid JSON")?;
                let matching = arguments.matching.trim();
                let fact = arguments.fact.trim();
                if matching.is_empty() || fact.is_empty() {
                    anyhow::bail!(
                        "update_memory requires non-empty 'matching' and 'fact' arguments"
                    );
                }
                if database.update_memory(matching, fact)? {
                    Ok(format!("updated memory matching \"{matching}\" to: {fact}"))
                } else {
                    anyhow::bail!(
                        "no single remembered fact matches \"{matching}\"; it may not exist, or \
                         the substring matches more than one entry"
                    )
                }
            }
            tools::FORGET_TOOL => {
                let arguments: ForgetArguments = serde_json::from_str(arguments)
                    .context("tool arguments were not valid JSON")?;
                let matching = arguments.matching.trim();
                if matching.is_empty() {
                    anyhow::bail!("forget requires a non-empty 'matching' argument");
                }
                if database.forget(matching)? {
                    Ok(format!("forgot the fact matching \"{matching}\""))
                } else {
                    anyhow::bail!(
                        "no single remembered fact matches \"{matching}\"; it may not exist, or \
                         the substring matches more than one entry"
                    )
                }
            }
            _ => unreachable!("is_memory_tool already filtered to a known name"),
        }
    })();
    result.unwrap_or_else(|error| format!("Error: {error:#}"))
}

fn truncate(text: &str, max: usize) -> String {
    let mut result: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        result.push('…');
    }
    result
}

/// Collapsed preview: head/tail trimmed, expand hint — box truncates rows to width.
fn preview_output(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let preview = if total <= 20 {
        let clipped = lines.join("\n");
        let mut out: String = clipped.chars().take(1000).collect();
        if clipped.chars().count() > 1000 {
            out.push_str(" … (truncated, collapsed)");
        }
        out
    } else {
        let head = lines[..10].join("\n");
        let tail = lines[total - 10..].join("\n");
        let hidden = total - 20;
        format!("{head}\n… ({hidden} lines hidden, collapsed) …\n{tail}")
    };
    let mut out: String = preview.chars().take(1000).collect();
    if preview.chars().count() > 1000 {
        out.push('…');
    }
    out
}

/// Build a single-line preview of `content` centered on the first match of `query`.
fn make_snippet(content: &str, query: &str) -> String {
    const WINDOW: usize = 80;
    const LEAD: usize = 24;

    let normalized: Vec<char> = content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .collect();
    // ASCII-fold both sides so indexing stays aligned one-to-one with `normalized`.
    let haystack: Vec<char> = normalized.iter().map(|c| c.to_ascii_lowercase()).collect();
    let needle: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();

    let start = match haystack
        .windows(needle.len().max(1))
        .position(|window| window == needle.as_slice())
    {
        Some(position) => position.saturating_sub(LEAD),
        None => 0,
    };

    let mut snippet = String::new();
    if start > 0 {
        snippet.push('…');
    }
    snippet.extend(normalized[start..].iter().take(WINDOW));
    if normalized.len() - start > WINDOW {
        snippet.push('…');
    }
    snippet
}

fn format_timestamp(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|value| value.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown time".to_string())
}

pub(crate) fn print_help(out: &mut String) {
    let _ = writeln!(
        out,
        "!<command>        Run a shell command directly (no model involvement)"
    );
    let _ = writeln!(
        out,
        "/plan             Enter Plan Mode (gate mutating tools until plan approved)"
    );
    let _ = writeln!(out, "/skills           List discovered skills");
    let _ = writeln!(out, "/warnings         Hide or show warning messages");
    let _ = writeln!(out, "/new              Start a new session");
    let _ = writeln!(out, "/sessions         List saved sessions");
    let _ = writeln!(out, "/resume <id>      Resume a session");
    let _ = writeln!(
        out,
        "/model [name]     List provider profiles, or switch to one"
    );
    let _ = writeln!(out, "/rename <id> <t>  Rename a session");
    let _ = writeln!(out, "/search <text>    Search saved messages");
    let _ = writeln!(
        out,
        "/compact          Summarize older messages to free up context"
    );
    let _ = writeln!(
        out,
        "/undo             Revert the files patched by the last turn"
    );
    let _ = writeln!(
        out,
        "/jobs             List session and persistent scheduled jobs"
    );
    let _ = writeln!(
        out,
        "/index            Rebuild the semantic-search index (needs embedding_model)"
    );
    let _ = writeln!(out, "/commands         List your own prompt commands");
    let _ = writeln!(out, "/delete <id>      Delete a session");
    let _ = writeln!(out, "/stats            Show current session usage");
    let _ = writeln!(
        out,
        "/usage            Show token usage by day and month, across all sessions"
    );
    let _ = writeln!(out, "/status           Show project and connection status");
    let _ = writeln!(
        out,
        "/memory           List facts Kamui remembers across sessions and projects"
    );
    let _ = writeln!(
        out,
        "/forget <text>    Forget one remembered fact, or /forget all"
    );
    let _ = writeln!(out, "/exit             Save and quit\n");
}

/// How much history `/usage` reports before summarizing everything into the lifetime total.
const USAGE_REPORT_DAYS: usize = 14;
const USAGE_REPORT_MONTHS: usize = 6;

/// Token usage across every session, by day and by month. Unlike `/stats`, which is scoped to the
/// active session, this answers "how much have I spent lately" over the whole database.
fn print_usage_report(database: &Database, prices: &Prices, out: &mut String) -> Result<()> {
    let daily = database.usage_by_day(USAGE_REPORT_DAYS)?;
    if daily.is_empty() {
        let _ = writeln!(out, "No usage recorded yet.\n");
        return Ok(());
    }

    // Per-model sums exist only to be priced, so they are not even queried without prices; the
    // report then runs exactly the queries, and prints exactly the columns, that it always has.
    let priced = !prices.is_empty();
    let daily_models = if priced {
        database.usage_model_tokens_by_day()?
    } else {
        HashMap::new()
    };
    let monthly_models = if priced {
        database.usage_model_tokens_by_month()?
    } else {
        HashMap::new()
    };

    let mut unpriced = false;
    #[allow(clippy::too_many_arguments)]
    fn row(
        out: &mut String,
        unpriced: &mut bool,
        prices: &Prices,
        period: &storage::UsagePeriod,
        tokens: &[storage::ModelTokens],
    ) {
        let cell = cost_cell(prices, model_token_rows(tokens));
        if let Some((_, has_unpriced)) = &cell {
            *unpriced |= has_unpriced;
        }
        let _ = writeln!(
            out,
            "{}",
            usage_row(period, cell.as_ref().map(|(cost, _)| cost.as_str()))
        );
    }

    let _ = writeln!(out, "\nLast {USAGE_REPORT_DAYS} days");
    for period in &daily {
        row(
            out,
            &mut unpriced,
            prices,
            period,
            tokens_for(&daily_models, &period.period),
        );
    }

    let monthly = database.usage_by_month(USAGE_REPORT_MONTHS)?;
    if monthly.len() > 1 {
        let _ = writeln!(out, "\nBy month");
        for period in &monthly {
            row(
                out,
                &mut unpriced,
                prices,
                period,
                tokens_for(&monthly_models, &period.period),
            );
        }
    }

    let total = database.usage_total()?;
    let total_models = if priced {
        database.usage_model_tokens()?
    } else {
        Vec::new()
    };
    let _ = writeln!(out, "\nAll time");
    row(out, &mut unpriced, prices, &total, &total_models);
    let _ = writeln!(
        out,
        "\nRequests count chat turns only; tokens include title generation."
    );
    if unpriced {
        let _ = writeln!(out, "{UNPRICED_NOTE}");
    }
    let _ = writeln!(out,);
    Ok(())
}

fn print_skills(
    library: &crate::skills::SkillLibrary,
    disabled: &std::collections::HashSet<String>,
    out: &mut String,
) {
    if library.list().is_empty() {
        let _ = writeln!(out, "No skills discovered.");
        let _ = writeln!(out, "Create a skill as a folder with SKILL.md:");
        let _ = writeln!(
            out,
            "  <project>/.kamui/skills/my-skill/SKILL.md  ->  /my-skill  (project)"
        );
        let _ = writeln!(
            out,
            "  <config dir>/kamui/skills/my-skill/SKILL.md ->  /my-skill  (global)"
        );
        let _ = writeln!(
            out,
            "Compat: .agents/skills is also scanned. Use /skill:<name> if a skill collides with a built-in or command.\n"
        );
        for warning in library.warnings() {
            let _ = writeln!(out, "  warning: {warning}");
        }
        if !library.warnings().is_empty() {
            let _ = writeln!(out,);
        }
        return;
    }
    let _ = writeln!(
        out,
        "Skills (eager: name+description in prompt, body on /<skill> or /skill:<name>):"
    );
    let term_w = Term::stdout().size().1 as usize;
    let max_desc = term_w.saturating_sub(40).clamp(20, 60);
    for skill in library.list() {
        let state = if disabled.contains(&skill.name) {
            "[disabled]"
        } else {
            "[enabled] "
        };
        let tools_hint = skill
            .allowed_tools
            .as_deref()
            .map(|tools| format!(" [tools: {tools}]"))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "  {state} /{:<18} {:<18} {}{tools_hint}",
            skill.name,
            skill.source.badge(),
            truncate(&skill.description, max_desc)
        );
    }
    if !library.warnings().is_empty() {
        let _ = writeln!(out, "\nWarnings (invalid skills skipped):");
        for warning in library.warnings() {
            let _ = writeln!(out, "  - {warning}");
        }
    }
    let _ = writeln!(
        out,
        "\nInvoke with /<skill-name> or /skill:<name> (namespaced, wins over collisions).\n"
    );
}

/// Interactive popup for `/skills`: grouped by location, arrow keys navigate, Enter toggles
/// enable/disable (persisted to user vs project settings.json), Esc closes.
/// Returns `Ok(true)` if any toggle was made (caller should reload `disabled_skills`).
fn source_rank(source: crate::skills::SkillSource) -> u8 {
    match source {
        crate::skills::SkillSource::ProjectKamui => 0,
        crate::skills::SkillSource::ProjectAgents => 1,
        crate::skills::SkillSource::GlobalKamui => 2,
        crate::skills::SkillSource::GlobalAgents => 3,
    }
}

fn source_label(source: crate::skills::SkillSource) -> &'static str {
    match source {
        crate::skills::SkillSource::ProjectKamui => "project .kamui",
        crate::skills::SkillSource::ProjectAgents => "project .agents",
        crate::skills::SkillSource::GlobalKamui => "global .kamui",
        crate::skills::SkillSource::GlobalAgents => "global .agents",
    }
}

fn run_skills_popup(
    library: &crate::skills::SkillLibrary,
    project_root: &std::path::Path,
    disabled: &mut std::collections::HashSet<String>,
) -> anyhow::Result<bool> {
    if library.list().is_empty() {
        let mut buf = String::new();
        print_skills(library, disabled, &mut buf);
        print!("{buf}");
        return Ok(false);
    }

    // Build display order: grouped by SkillSource priority, then name.
    let mut order: Vec<usize> = (0..library.list().len()).collect();
    order.sort_by_key(|&i| {
        let s = &library.list()[i];
        (source_rank(s.source), s.name.clone())
    });

    let mut selected: usize = 0;
    let mut changed = false;
    let term = Term::stdout();

    // Fall back to a plain list when not a TTY or NO_COLOR is set (no ANSI).
    if !Ui::stdio().interactive() || std::env::var_os("NO_COLOR").is_some() {
        let mut buf = String::new();
        print_skills(library, disabled, &mut buf);
        print!("{buf}");
        return Ok(false);
    }

    // Hide cursor
    let _ = term.hide_cursor();

    let (term_h, term_w) = term.size();
    let visible = (term_h as usize).saturating_sub(6).clamp(5, 10);
    let max_desc = (term_w as usize).saturating_sub(40).clamp(20, 60);

    let render = |selected: usize, disabled: &std::collections::HashSet<String>| -> String {
        let total = order.len();
        let start = selected
            .saturating_sub(visible / 2)
            .min(total.saturating_sub(visible));
        let mut out = String::new();
        out.push_str(
            "\x1b[1mSkills\x1b[0m · \x1b[2m↑/↓ navigate · Enter toggle · Esc close\x1b[0m\n",
        );
        let mut last_rank = if start > 0 {
            Some(source_rank(library.list()[order[start - 1]].source))
        } else {
            None
        };
        for (row, &idx) in order.iter().skip(start).take(visible).enumerate() {
            let pos = start + row;
            let skill = &library.list()[idx];
            let rank = source_rank(skill.source);
            if last_rank != Some(rank) {
                out.push_str(&format!(
                    "\n\x1b[2m─ {} ─\x1b[0m\n",
                    source_label(skill.source)
                ));
                last_rank = Some(rank);
            }
            let is_on = pos == selected;
            let enabled = !disabled.contains(&skill.name);
            let badge = if enabled { "●" } else { "○" };
            let state = if enabled { "enabled" } else { "disabled" };
            let prefix = if is_on { "\x1b[7m" } else { "" };
            let suffix = if is_on { "\x1b[0m" } else { "" };
            let dim = if enabled { "" } else { "\x1b[2m" };
            let dim_off = if enabled { "" } else { "\x1b[0m" };
            let desc = truncate(&skill.description, max_desc);
            out.push_str(&format!(
                "{prefix}{dim} {badge} /{:<18} {} [{state}]{dim_off}{suffix}\n",
                skill.name, desc
            ));
        }
        out
    };

    // Initial draw: clear below and print
    let mut last_lines: usize = 0;
    let draw =
        |selected: usize, disabled: &std::collections::HashSet<String>, last_lines: &mut usize| {
            let text = render(selected, disabled);
            let lines = text.matches('\n').count() + 1;
            if *last_lines > 0 {
                // Move up and clear
                let _ = term.write_str(&format!("\x1b[{}A\x1b[J", last_lines));
            }
            let _ = term.write_str(&text);
            let _ = term.flush();
            *last_lines = lines;
        };

    draw(selected, disabled, &mut last_lines);

    #[allow(clippy::while_let_loop)]
    loop {
        let key = match term.read_key() {
            Ok(k) => k,
            Err(_) => break,
        };
        match key {
            Key::ArrowUp => {
                if selected > 0 {
                    selected -= 1;
                } else {
                    selected = order.len() - 1;
                }
                draw(selected, disabled, &mut last_lines);
            }
            Key::ArrowDown => {
                selected = (selected + 1) % order.len();
                draw(selected, disabled, &mut last_lines);
            }
            Key::Enter => {
                let idx = order[selected];
                let skill = &library.list()[idx];
                let was_disabled = disabled.contains(&skill.name);
                let now_disabled = !was_disabled;
                // Persist
                if let Err(e) =
                    crate::settings::set_skill_disabled(project_root, skill, now_disabled)
                {
                    // Show error inline then continue
                    let _ = term.write_str(&format!("\n\x1b[31mFailed to save: {e}\x1b[0m\n"));
                    let _ = term.flush();
                } else {
                    if now_disabled {
                        disabled.insert(skill.name.clone());
                    } else {
                        disabled.remove(&skill.name);
                    }
                    changed = true;
                }
                draw(selected, disabled, &mut last_lines);
            }
            Key::Escape => break,
            Key::Char('q') | Key::Char('Q') => break,
            _ => {}
        }
    }

    // Restore cursor and move to next line
    let _ = term.show_cursor();
    let _ = term.write_line("");
    let _ = term.flush();

    if changed {
        println!("\nUpdated disabledSkills. Changes apply to the next turn.\n");
    }
    Ok(changed)
}

/// List the user's own prompt commands, or explain where to put one when there are none yet.
fn print_commands(library: &commands::CommandLibrary, out: &mut String) {
    if library.is_empty() {
        let _ = writeln!(out, "No custom commands yet.");
        let _ = writeln!(out, "Add a markdown file to create one:");
        let _ = writeln!(
            out,
            "  <project>/.kamui/commands/review.md  ->  /review   (this project only)"
        );
        let _ = writeln!(
            out,
            "  <config dir>/kamui/commands/review.md ->  /review   (every project)\n"
        );
        return;
    }
    let _ = writeln!(out, "Your commands:");
    for command in library.list() {
        let description = command.description.as_deref().unwrap_or("");
        let _ = writeln!(
            out,
            "  /{:<18} {:<9} {description}",
            command.name,
            command.source.label()
        );
    }
    let _ = writeln!(
        out,
        "\nInvoke one with /<name>; anything after it is appended to the prompt.\n"
    );
}

struct GitStatus {
    branch: String,
    changed: usize,
}

fn print_status(
    project: &ProjectContext,
    active: &Profile,
    tools: &ToolRegistry,
    mcp_statuses: &[ConnectionStatus],
    allow_commands: &[String],
) {
    let git = git_status(project.root());
    let project_name = project
        .root()
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project");

    println!(
        "╭─ Kamui v{} ─────────────────────────",
        env!("CARGO_PKG_VERSION")
    );
    println!(
        "│ Project  {project_name}  ({})",
        display_path(project.root())
    );
    match git {
        Some(git) => println!("│ Git      {}  ·  {} changed", git.branch, git.changed),
        None => println!("│ Git      not a repository"),
    }
    println!("│ Model    {}  ({})", active.model, active.name);
    println!("│ Tools    {} available", tools.len());
    if !allow_commands.is_empty() {
        println!("│ Allow    {}", allow_commands.join(", "));
    }
    if mcp_statuses.is_empty() {
        println!("│ MCP      none configured");
    } else {
        for server in mcp_statuses {
            match &server.error {
                Some(_) => println!("│ MCP      {}  unavailable", server.name),
                None => println!(
                    "│ MCP      {}  connected · {} tools{}",
                    server.name,
                    server.tool_count,
                    if server.trusted { " · trusted" } else { "" }
                ),
            }
        }
    }
    if let Some(name) = project.instruction_name() {
        println!("│ Rules    {name}");
    }
    println!("╰──────────────────────────────────────\n");
}

fn git_status(root: &Path) -> Option<GitStatus> {
    let branch = Command::new("git")
        .current_dir(root)
        .args(["branch", "--show-current"])
        .output()
        .ok()?;
    if !branch.status.success() {
        return None;
    }
    let mut branch = String::from_utf8(branch.stdout).ok()?.trim().to_string();
    if branch.is_empty() {
        let head = Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()?;
        if !head.status.success() {
            return None;
        }
        branch = format!("detached@{}", String::from_utf8(head.stdout).ok()?.trim());
    }

    let status = Command::new("git")
        .current_dir(root)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !status.status.success() {
        return None;
    }
    Some(GitStatus {
        branch,
        changed: String::from_utf8(status.stdout).ok()?.lines().count(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::ModelPrice;
    use std::fs;
    use uuid::Uuid;

    fn temporary_directory() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("kamui-status-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        path
    }

    /// The point of "optional": with no `[pricing]` in `kamui.toml`, `/stats` and `/usage` print
    /// exactly the lines they printed before cost tracking existed — no cost column, no zeroes —
    /// and never even reach the database for per-model sums.
    #[test]
    fn reports_are_unchanged_when_no_prices_are_configured() {
        let prices = Prices::default();
        let period = storage::UsagePeriod {
            period: "2026-08-20".to_string(),
            request_count: 3,
            input_tokens: 10,
            output_tokens: 4,
            total_tokens: 14,
            cached_tokens: 0,
        };
        let stat = storage::ModelStat {
            model: "gpt-5".to_string(),
            request_count: 3,
            input_tokens: 10,
            output_tokens: 4,
            total_tokens: 14,
            cached_tokens: 0,
        };

        assert!(cost_cell(&prices, [(Some("gpt-5"), 10, 4)]).is_none());
        assert_eq!(
            usage_row(&period, None),
            "  2026-08-20    3 req          10 in           4 out          14 total"
        );
        assert_eq!(
            model_row(&stat, None),
            "  gpt-5                      3 req        10 in         4 out        14 total"
        );

        // `UnreachableProvider`'s counterpart for storage: the session has usage, but with no
        // prices there is nothing to report and nothing to query.
        let database = Database::open_in_memory_for_tests();
        let session = database.create_session("test", "gpt-5").unwrap();
        database
            .save_turn(
                &session.id,
                &[Message::user("hi")],
                &Usage {
                    prompt_tokens: 10,
                    completion_tokens: 4,
                    total_tokens: 14,
                    cached_tokens: 0,
                },
                "gpt-5",
                "stop",
            )
            .unwrap();

        assert!(
            session_cost(&database, &session.id, &prices)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_configured_price_adds_a_cost_cell_to_a_report_row() {
        let prices = Prices::new(
            None,
            [(
                "gpt-5".to_string(),
                ModelPrice {
                    input_per_million: 1.0,
                    output_per_million: 2.0,
                },
            )],
        )
        .unwrap();
        let period = storage::UsagePeriod {
            period: "2026-08-20".to_string(),
            request_count: 1,
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            total_tokens: 1_500_000,
            cached_tokens: 0,
        };

        let (cost, unpriced) = cost_cell(&prices, [(Some("gpt-5"), 1_000_000, 500_000)]).unwrap();

        assert_eq!(cost, "$2.0000");
        assert!(!unpriced);
        assert!(usage_row(&period, Some(&cost)).ends_with("total       $2.0000"));
    }

    #[test]
    fn a_model_with_usage_but_no_price_is_never_reported_as_free() {
        let prices = Prices::new(
            None,
            [(
                "gpt-5".to_string(),
                ModelPrice {
                    input_per_million: 1.0,
                    output_per_million: 1.0,
                },
            )],
        )
        .unwrap();

        let (cost, unpriced) = cost_cell(&prices, [(Some("codeqwen:latest"), 500, 500)]).unwrap();

        assert_eq!(cost, "unpriced");
        assert!(unpriced);
    }

    /// A session's cost covers every usage kind, so a title generated by a second, unpriced model
    /// marks the total rather than quietly vanishing from it.
    #[test]
    fn session_cost_covers_title_generation_and_marks_an_unpriced_model() {
        let database = Database::open_in_memory_for_tests();
        let session = database.create_session("test", "gpt-5").unwrap();
        let million = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cached_tokens: 0,
        };
        database
            .save_turn(
                &session.id,
                &[Message::user("hi")],
                &million,
                "gpt-5",
                "stop",
            )
            .unwrap();
        database
            .save_generated_title(&session.id, "A title", &million, "cheap-titler", "stop")
            .unwrap();
        let prices = Prices::new(
            None,
            [(
                "gpt-5".to_string(),
                ModelPrice {
                    input_per_million: 1.0,
                    output_per_million: 1.0,
                },
            )],
        )
        .unwrap();

        let (cost, unpriced) = session_cost(&database, &session.id, &prices)
            .unwrap()
            .unwrap();

        assert_eq!(cost, "$1.0000+");
        assert!(unpriced);
    }

    #[test]
    fn git_status_reports_branch_and_changed_files() {
        let root = temporary_directory();
        assert!(
            Command::new("git")
                .args(["init", "-b", "status-test"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        fs::write(root.join("one.txt"), "one").unwrap();
        fs::write(root.join("two.txt"), "two").unwrap();

        let status = git_status(&root).unwrap();

        assert_eq!(status.branch, "status-test");
        assert_eq!(status.changed, 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_status_returns_none_outside_a_repository() {
        let root = temporary_directory();
        assert!(git_status(&root).is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn make_title_truncates_long_input() {
        assert_eq!(make_title("short"), "short");
        let title = make_title(&"a".repeat(45));
        assert_eq!(title.chars().count(), 43); // 40 characters plus "..."
        assert!(title.ends_with("..."));
    }

    #[test]
    fn clean_title_strips_wrapping_punctuation_and_extra_lines() {
        assert_eq!(clean_title("\"Rust Ownership\""), "Rust Ownership");
        assert_eq!(clean_title("Title:\nsecond line"), "Title");
        assert_eq!(clean_title("  spaced.  "), "spaced");
    }

    #[test]
    fn truncate_appends_ellipsis_only_when_needed() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hello…");
    }

    #[test]
    fn preview_output_caps_lines_and_chars() {
        let many_lines = (0..25)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let previewed = preview_output(&many_lines);
        assert!(previewed.contains("lines hidden, collapsed"));
        assert!(previewed.starts_with("line 0"));
        assert!(previewed.contains("line 24"));
        assert_eq!(preview_output("short"), "short");
        assert!(!preview_output(&"x".repeat(1200)).ends_with('x'));
    }

    #[test]
    fn short_id_takes_the_first_eight_characters() {
        assert_eq!(short_id("0123456789"), "01234567");
        assert_eq!(short_id("abc"), "abc");
    }

    #[test]
    fn display_path_trims_windows_verbatim_prefixes() {
        use std::path::Path;
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Users\dev\project")),
            r"C:\Users\dev\project"
        );
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\server\share\dir")),
            r"\\server\share\dir"
        );
        assert_eq!(
            display_path(Path::new("/home/dev/project")),
            "/home/dev/project"
        );
    }

    #[test]
    fn accumulate_usage_sums_output_and_keeps_the_last_input() {
        let mut total = Usage::default();
        accumulate_usage(
            &mut total,
            &Usage {
                prompt_tokens: 100,
                completion_tokens: 20,
                total_tokens: 120,
                cached_tokens: 10,
            },
        );
        accumulate_usage(
            &mut total,
            &Usage {
                prompt_tokens: 150,
                completion_tokens: 30,
                total_tokens: 180,
                cached_tokens: 40,
            },
        );

        assert_eq!(total.prompt_tokens, 150); // final round's context size
        assert_eq!(total.completion_tokens, 50); // output summed across rounds
        assert_eq!(total.total_tokens, 200); // last input + all output
        assert_eq!(total.cached_tokens, 40); // last round wins, like prompt_tokens
    }

    #[test]
    fn format_duration_switches_units_at_one_second() {
        assert_eq!(format_duration(Duration::from_millis(320)), "320ms");
        assert_eq!(format_duration(Duration::from_millis(999)), "999ms");
        assert_eq!(format_duration(Duration::from_millis(4200)), "4.2s");
        assert_eq!(format_duration(Duration::from_secs(1)), "1.0s");
    }

    #[test]
    fn make_snippet_centers_on_the_match_without_ellipsis_when_short() {
        let snippet = make_snippet("the quick brown fox jumps", "brown");
        assert!(snippet.contains("brown"));
        assert!(!snippet.contains('…'));
    }

    #[test]
    fn make_snippet_is_case_insensitive_and_normalizes_whitespace() {
        let snippet = make_snippet("Hello\n\n  WORLD   here", "world");
        assert!(snippet.contains("WORLD"));
        assert!(!snippet.contains('\n'));
    }

    #[test]
    fn make_snippet_marks_truncation_with_an_ellipsis() {
        let mut content = "x ".repeat(60); // pushes the match past the leading window
        content.push_str("NEEDLE tail");
        let snippet = make_snippet(&content, "needle");
        assert!(snippet.starts_with('…'));
        assert!(snippet.contains("NEEDLE"));
    }

    fn respond_with(text: &str) -> Option<mpsc::UnboundedReceiver<String>> {
        let (sender, receiver) = mpsc::unbounded_channel();
        sender.send(text.to_string()).unwrap();
        Some(receiver)
    }

    #[tokio::test]
    async fn ask_user_rejects_invalid_json_arguments() {
        let mut rx = respond_with("anything");
        let output = ask_user(&mut rx, false, "not json", None).await.unwrap();
        assert!(output.starts_with("Error:"));
    }

    #[tokio::test]
    async fn ask_user_rejects_a_blank_question() {
        let mut rx = respond_with("anything");
        let output = ask_user(&mut rx, false, r#"{"question":"   "}"#, None)
            .await
            .unwrap();
        assert!(output.starts_with("Error:"));
    }

    #[tokio::test]
    async fn ask_user_returns_free_text_when_no_options_are_offered() {
        let mut rx = respond_with("Tuesday works better");
        let output = ask_user(&mut rx, false, r#"{"question":"When?"}"#, None)
            .await
            .unwrap();
        assert_eq!(output, "Tuesday works better");
    }

    #[tokio::test]
    async fn ask_user_resolves_a_numbered_choice_to_its_option_text() {
        let mut rx = respond_with("2");
        let output = ask_user(
            &mut rx,
            false,
            r#"{"question":"Pick one","options":["red","green","blue"]}"#,
            None,
        )
        .await
        .unwrap();
        assert_eq!(output, "green");
    }

    #[tokio::test]
    async fn ask_user_falls_back_to_raw_text_for_an_out_of_range_number() {
        let mut rx = respond_with("99");
        let output = ask_user(
            &mut rx,
            false,
            r#"{"question":"Pick one","options":["red","green"]}"#,
            None,
        )
        .await
        .unwrap();
        assert_eq!(output, "99");
    }

    #[tokio::test]
    async fn ask_user_accepts_free_text_even_when_options_are_offered() {
        let mut rx = respond_with("actually neither");
        let output = ask_user(
            &mut rx,
            false,
            r#"{"question":"Pick one","options":["red","green"]}"#,
            None,
        )
        .await
        .unwrap();
        assert_eq!(output, "actually neither");
    }

    #[test]
    fn render_memory_snapshot_is_empty_with_no_entries() {
        assert_eq!(render_memory_snapshot(&[]), "");
    }

    #[test]
    fn render_memory_snapshot_lists_every_fact() {
        let entries = vec![
            storage::MemoryEntry {
                content: "Prefers bun over node.".to_owned(),
            },
            storage::MemoryEntry {
                content: "Prefers uv over pip.".to_owned(),
            },
        ];

        let rendered = render_memory_snapshot(&entries);

        assert!(rendered.contains("Prefers bun over node."));
        assert!(rendered.contains("Prefers uv over pip."));
    }

    #[test]
    fn dispatch_memory_tool_remembers_a_fact() {
        let database = Database::open_in_memory_for_tests();
        let output = dispatch_memory_tool(&database, "remember", r#"{"fact":"Prefers bun."}"#);
        assert!(output.starts_with("remembered:"), "{output}");
        assert_eq!(database.list_memory().unwrap()[0].content, "Prefers bun.");
    }

    #[test]
    fn dispatch_memory_tool_rejects_a_blank_fact() {
        let database = Database::open_in_memory_for_tests();
        let output = dispatch_memory_tool(&database, "remember", r#"{"fact":"  "}"#);
        assert!(output.starts_with("Error:"), "{output}");
    }

    #[test]
    fn dispatch_memory_tool_refuses_once_the_memory_cap_is_reached() {
        let database = Database::open_in_memory_for_tests();
        database
            .remember(&"x".repeat(MAX_MEMORY_BYTES as usize))
            .unwrap();

        let output = dispatch_memory_tool(&database, "remember", r#"{"fact":"one more"}"#);

        assert!(
            output.starts_with("Error:") && output.contains("full"),
            "{output}"
        );
    }

    #[test]
    fn dispatch_memory_tool_updates_a_matched_fact() {
        let database = Database::open_in_memory_for_tests();
        database.remember("Prefers node over bun.").unwrap();

        let output = dispatch_memory_tool(
            &database,
            "update_memory",
            r#"{"matching":"node over bun","fact":"bun over node."}"#,
        );

        assert!(output.starts_with("updated memory"), "{output}");
        assert_eq!(database.list_memory().unwrap()[0].content, "bun over node.");
    }

    #[test]
    fn dispatch_memory_tool_update_reports_an_error_when_nothing_matches() {
        let database = Database::open_in_memory_for_tests();
        let output = dispatch_memory_tool(
            &database,
            "update_memory",
            r#"{"matching":"nonexistent","fact":"x"}"#,
        );
        assert!(output.starts_with("Error:"), "{output}");
    }

    #[test]
    fn dispatch_memory_tool_forgets_a_matched_fact() {
        let database = Database::open_in_memory_for_tests();
        database.remember("Prefers bun.").unwrap();

        let output = dispatch_memory_tool(&database, "forget", r#"{"matching":"bun"}"#);

        assert!(output.starts_with("forgot"), "{output}");
        assert!(database.list_memory().unwrap().is_empty());
    }

    #[test]
    fn dispatch_memory_tool_forget_reports_an_error_when_nothing_matches() {
        let database = Database::open_in_memory_for_tests();
        let output = dispatch_memory_tool(&database, "forget", r#"{"matching":"nonexistent"}"#);
        assert!(output.starts_with("Error:"), "{output}");
    }

    #[test]
    fn snapshot_patch_target_keeps_the_pre_turn_content_on_repeated_edits() {
        // patch_target canonicalizes internally, so the root must be canonical too for the
        // returned keys to compare equal to `root.join(...)`.
        let root = temporary_directory().canonicalize().unwrap();
        fs::write(root.join("a.txt"), "original").unwrap();
        let mut snapshot = HashMap::new();

        snapshot_patch_target(
            &root,
            r#"{"path":"a.txt","old_text":"original","new_text":"first edit"}"#,
            &mut snapshot,
        );
        // A second, later call for the same path must not overwrite the pre-turn baseline.
        snapshot_patch_target(
            &root,
            r#"{"path":"a.txt","old_text":"first edit","new_text":"second edit"}"#,
            &mut snapshot,
        );

        assert_eq!(
            snapshot.get(&root.join("a.txt")).unwrap().as_deref(),
            Some("original")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_patch_target_records_none_for_a_new_file() {
        let root = temporary_directory().canonicalize().unwrap();
        let mut snapshot = HashMap::new();

        snapshot_patch_target(
            &root,
            r#"{"path":"new.txt","old_text":"","new_text":"hello"}"#,
            &mut snapshot,
        );

        assert_eq!(snapshot.get(&root.join("new.txt")), Some(&None));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn revert_snapshot_restores_edited_files_and_deletes_created_ones() {
        let root = temporary_directory();
        fs::write(root.join("edited.txt"), "changed").unwrap();
        fs::write(root.join("created.txt"), "new content").unwrap();
        let mut snapshot = HashMap::new();
        snapshot.insert(root.join("edited.txt"), Some("original".to_string()));
        snapshot.insert(root.join("created.txt"), None);

        let reverted = revert_snapshot(&snapshot);

        assert_eq!(reverted, 2);
        assert_eq!(
            fs::read_to_string(root.join("edited.txt")).unwrap(),
            "original"
        );
        assert!(!root.join("created.txt").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn revert_snapshot_treats_an_already_missing_file_as_reverted() {
        let root = temporary_directory();
        let mut snapshot = HashMap::new();
        snapshot.insert(root.join("never-written.txt"), None);

        assert_eq!(revert_snapshot(&snapshot), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn is_memory_tool_recognizes_only_the_three_memory_tools() {
        assert!(is_memory_tool("remember"));
        assert!(is_memory_tool("update_memory"));
        assert!(is_memory_tool("forget"));
        assert!(!is_memory_tool("ask_user"));
        assert!(!is_memory_tool("run_command"));
    }

    #[test]
    fn new_command_clears_always_allowed_and_undo_state() {
        let database = Database::open_in_memory_for_tests();
        let mut session: Option<Session> = None;
        let mut messages: Vec<Message> = vec![Message::user("hi")];
        let mut always_allowed: HashSet<String> = HashSet::from(["run_command".to_string()]);
        let mut last_turn_snapshot: Option<HashMap<PathBuf, Option<String>>> = Some(HashMap::from(
            [(PathBuf::from("a.txt"), Some("x".to_string()))],
        ));

        handle_command(
            "/new",
            &UnreachableProvider,
            None,
            &database,
            &mut session,
            &mut messages,
            &mut always_allowed,
            &mut last_turn_snapshot,
            &Prices::default(),
            None,
        )
        .unwrap();

        assert!(session.is_none());
        assert!(messages.is_empty());
        assert!(always_allowed.is_empty());
        assert!(last_turn_snapshot.is_none());
    }

    #[test]
    fn delete_command_clears_state_when_deleting_the_active_session() {
        let database = Database::open_in_memory_for_tests();
        let created = database.create_session("test", "m").unwrap();
        let id = created.id.clone();
        let mut session = Some(created);
        let mut messages: Vec<Message> = vec![Message::user("hi")];
        let mut always_allowed: HashSet<String> = HashSet::from(["patch_file".to_string()]);
        let mut last_turn_snapshot: Option<HashMap<PathBuf, Option<String>>> = Some(HashMap::new());

        handle_command(
            &format!("/delete {id}"),
            &UnreachableProvider,
            None,
            &database,
            &mut session,
            &mut messages,
            &mut always_allowed,
            &mut last_turn_snapshot,
            &Prices::default(),
            None,
        )
        .unwrap();

        assert!(session.is_none());
        assert!(messages.is_empty());
        assert!(always_allowed.is_empty());
        assert!(last_turn_snapshot.is_none());
    }

    /// A `Provider` that panics if actually called — for tests asserting that some check rejects
    /// or skips its input before ever making a request.
    struct UnreachableProvider;

    #[async_trait::async_trait]
    impl Provider for UnreachableProvider {
        fn name(&self) -> &'static str {
            "unreachable"
        }
        async fn chat(&self, _request: ChatRequest) -> Result<crate::provider::ChatResponse> {
            panic!("the provider should not have been called");
        }
        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<mpsc::UnboundedReceiver<Result<crate::provider::StreamEvent>>> {
            panic!("the provider should not have been called");
        }
        async fn embed(&self, _model: &str, _input: Vec<String>) -> Result<Vec<Vec<f32>>> {
            panic!("the provider should not have been asked to embed anything");
        }
    }

    /// A `Provider` that answers `embed` with a deterministic vector per input, so index writes can
    /// be asserted without a network call.
    struct StubEmbeddingProvider;

    #[async_trait::async_trait]
    impl Provider for StubEmbeddingProvider {
        fn name(&self) -> &'static str {
            "stub-embeddings"
        }
        async fn chat(&self, _request: ChatRequest) -> Result<crate::provider::ChatResponse> {
            panic!("these tests only exercise embedding");
        }
        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<mpsc::UnboundedReceiver<Result<crate::provider::StreamEvent>>> {
            panic!("these tests only exercise embedding");
        }
        async fn embed(&self, _model: &str, input: Vec<String>) -> Result<Vec<Vec<f32>>> {
            Ok(input.iter().map(|text| vec![text.len() as f32]).collect())
        }
    }

    fn profile_with_embedding(embedding_model: Option<&str>) -> Profile {
        Profile {
            name: "default".to_string(),
            model: "gpt-5".to_string(),
            base_url: "https://api.example.com/v1".to_string(),
            api_key: "k".to_string(),
            context_window: None,
            tools: true,
            embedding_model: embedding_model.map(str::to_string),
        }
    }

    /// Seed the index for a project-relative path without going through a provider, so a test can
    /// start from "already indexed" and assert what a refresh does next.
    fn seed_index(database: &Database, project: &ProjectContext, relative: &str, content: &str) {
        let key = project.key();
        database
            .replace_file_index(
                &key,
                relative,
                &content_hash(content),
                "embed-1",
                &[storage::NewCodeChunk {
                    start_line: 1,
                    end_line: 1,
                    content: content.to_string(),
                    embedding: vec![1.0],
                }],
            )
            .unwrap();
    }

    struct ConcurrentProvider {
        active: std::sync::atomic::AtomicUsize,
        maximum: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for ConcurrentProvider {
        fn name(&self) -> &'static str {
            "concurrent"
        }

        async fn chat(&self, request: ChatRequest) -> Result<crate::provider::ChatResponse> {
            use std::sync::atomic::Ordering;
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(crate::provider::ChatResponse {
                content: request.messages.last().unwrap().content.clone(),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                finish_reason: "stop".to_string(),
            })
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<mpsc::UnboundedReceiver<Result<crate::provider::StreamEvent>>> {
            panic!("the concurrent sub-agent test uses non-streaming chat");
        }

        async fn embed(&self, _model: &str, _input: Vec<String>) -> Result<Vec<Vec<f32>>> {
            panic!("the concurrent sub-agent test does not embed");
        }
    }

    #[tokio::test]
    async fn spawn_agents_run_concurrently_with_a_four_agent_cap() {
        use std::sync::atomic::Ordering;
        let provider = ConcurrentProvider {
            active: std::sync::atomic::AtomicUsize::new(0),
            maximum: std::sync::atomic::AtomicUsize::new(0),
        };
        let project = temporary_project();
        let calls = (0..6)
            .map(|index| ToolCall {
                id: format!("c{index}"),
                name: tools::SPAWN_AGENT_TOOL.to_string(),
                arguments: format!(r#"{{"prompt":"task {index}"}}"#),
            })
            .collect::<Vec<_>>();
        let references = calls.iter().collect::<Vec<_>>();

        let outputs = dispatch_spawn_agents(&provider, "model", &project, &references).await;

        assert_eq!(outputs.len(), 6);
        assert_eq!(outputs["c0"].0, "task 0");
        assert_eq!(outputs["c5"].0, "task 5");
        assert_eq!(provider.maximum.load(Ordering::SeqCst), 4);
        fs::remove_dir_all(project.root()).unwrap();
    }

    #[tokio::test]
    async fn refresh_re_embeds_an_indexed_file_that_changed() {
        let database = Database::open_in_memory_for_tests();
        let project = temporary_project();
        let path = project.root().join("a.rs");
        seed_index(&database, &project, "a.rs", "fn old() {}");
        fs::write(&path, "fn new() {}").unwrap();

        let refreshed = refresh_index_for_paths(
            &StubEmbeddingProvider,
            &profile_with_embedding(Some("embed-1")),
            &database,
            &project,
            vec![path],
        )
        .await
        .unwrap();

        assert_eq!(refreshed, 1);
        let chunks = database.all_chunks(&project.key()).unwrap();
        assert_eq!(chunks.len(), 1, "the old chunk should have been replaced");
        assert_eq!(chunks[0].content, "fn new() {}");
        assert_eq!(
            database.indexed_file_hash(&project.key(), "a.rs").unwrap(),
            Some(content_hash("fn new() {}")),
            "the stored hash should follow the new content"
        );
    }

    #[tokio::test]
    async fn refresh_skips_a_file_that_still_matches_the_index() {
        let database = Database::open_in_memory_for_tests();
        let project = temporary_project();
        let path = project.root().join("a.rs");
        fs::write(&path, "fn a() {}").unwrap();
        seed_index(&database, &project, "a.rs", "fn a() {}");

        // `UnreachableProvider` panics on `embed`, so reaching zero here proves nothing was spent.
        let refreshed = refresh_index_for_paths(
            &UnreachableProvider,
            &profile_with_embedding(Some("embed-1")),
            &database,
            &project,
            vec![path],
        )
        .await
        .unwrap();

        assert_eq!(refreshed, 0);
    }

    /// The deliberate boundary: a turn that creates a file does not grow the index on Kamui's own
    /// initiative — the startup staleness hint reports it and the user decides.
    #[tokio::test]
    async fn refresh_ignores_a_file_that_was_never_indexed() {
        let database = Database::open_in_memory_for_tests();
        let project = temporary_project();
        let path = project.root().join("new.rs");
        fs::write(&path, "fn brand_new() {}").unwrap();

        let refreshed = refresh_index_for_paths(
            &UnreachableProvider,
            &profile_with_embedding(Some("embed-1")),
            &database,
            &project,
            vec![path],
        )
        .await
        .unwrap();

        assert_eq!(refreshed, 0);
        assert!(database.all_chunks(&project.key()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn refresh_drops_the_index_entry_for_a_file_that_disappeared() {
        let database = Database::open_in_memory_for_tests();
        let project = temporary_project();
        seed_index(&database, &project, "gone.rs", "fn gone() {}");

        let refreshed = refresh_index_for_paths(
            &UnreachableProvider,
            &profile_with_embedding(Some("embed-1")),
            &database,
            &project,
            vec![project.root().join("gone.rs")],
        )
        .await
        .unwrap();

        assert_eq!(refreshed, 1);
        assert!(database.all_chunks(&project.key()).unwrap().is_empty());
        assert!(database.indexed_files(&project.key()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn refresh_does_nothing_without_an_embedding_model() {
        let database = Database::open_in_memory_for_tests();
        let project = temporary_project();
        let path = project.root().join("a.rs");
        seed_index(&database, &project, "a.rs", "fn old() {}");
        fs::write(&path, "fn new() {}").unwrap();

        let refreshed = refresh_index_for_paths(
            &UnreachableProvider,
            &profile_with_embedding(None),
            &database,
            &project,
            vec![path],
        )
        .await
        .unwrap();

        assert_eq!(refreshed, 0);
    }

    #[tokio::test]
    async fn spawn_agent_rejects_invalid_json_without_calling_the_provider() {
        let root = temporary_directory().canonicalize().unwrap();
        let project = ProjectContext::from_root(root.clone()).unwrap();

        let output =
            dispatch_spawn_agent(&UnreachableProvider, "gpt-5", &project, "not json").await;

        assert!(output.starts_with("Error:"), "{output}");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn spawn_agent_rejects_an_empty_prompt_without_calling_the_provider() {
        let root = temporary_directory().canonicalize().unwrap();
        let project = ProjectContext::from_root(root.clone()).unwrap();

        let output = dispatch_spawn_agent(
            &UnreachableProvider,
            "gpt-5",
            &project,
            r#"{"prompt":"   "}"#,
        )
        .await;

        assert!(output.starts_with("Error:"), "{output}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cosine_similarity_of_identical_vectors_is_one() {
        let vector = vec![1.0_f32, 2.0, 3.0];
        assert!((cosine_similarity(&vector, &vector) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_of_orthogonal_vectors_is_zero() {
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_handles_mismatched_or_empty_vectors() {
        assert_eq!(cosine_similarity(&[1.0, 2.0], &[1.0]), 0.0);
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn content_hash_is_stable_and_change_sensitive() {
        assert_eq!(content_hash("hello"), content_hash("hello"));
        assert_ne!(content_hash("hello"), content_hash("world"));
    }

    fn temporary_project() -> ProjectContext {
        ProjectContext::from_root(temporary_directory()).unwrap()
    }

    #[tokio::test]
    async fn search_code_rejects_invalid_json_without_calling_the_provider() {
        let database = Database::open_in_memory_for_tests();
        let output = dispatch_search_code(
            &UnreachableProvider,
            "text-embedding-3-small",
            &database,
            &temporary_project(),
            "not json",
        )
        .await;
        assert!(output.starts_with("Error:"), "{output}");
    }

    #[tokio::test]
    async fn search_code_rejects_an_empty_query_without_calling_the_provider() {
        let database = Database::open_in_memory_for_tests();
        let output = dispatch_search_code(
            &UnreachableProvider,
            "text-embedding-3-small",
            &database,
            &temporary_project(),
            r#"{"query":"   "}"#,
        )
        .await;
        assert!(output.starts_with("Error:"), "{output}");
    }

    #[tokio::test]
    async fn search_code_reports_a_missing_index_without_calling_the_provider() {
        let database = Database::open_in_memory_for_tests();
        let output = dispatch_search_code(
            &UnreachableProvider,
            "text-embedding-3-small",
            &database,
            &temporary_project(),
            r#"{"query":"how does auth work"}"#,
        )
        .await;
        assert!(output.contains("no code index found"), "{output}");
    }

    /// A project indexed elsewhere must not satisfy `search_code` here — the chunks exist, but not
    /// for this root, so the tool still reports a missing index rather than answering with another
    /// project's code.
    #[tokio::test]
    async fn search_code_ignores_another_projects_index() {
        let database = Database::open_in_memory_for_tests();
        database
            .insert_chunk("/somewhere/else", "src/main.rs", 1, 5, "other", &[0.1])
            .unwrap();

        let output = dispatch_search_code(
            &UnreachableProvider,
            "text-embedding-3-small",
            &database,
            &temporary_project(),
            r#"{"query":"how does auth work"}"#,
        )
        .await;

        assert!(output.contains("no code index found"), "{output}");
    }

    #[test]
    fn staleness_is_not_reported_for_an_unindexed_project() {
        let database = Database::open_in_memory_for_tests();
        let project = temporary_project();
        assert_eq!(index_staleness(&database, &project).unwrap(), None);
    }

    #[test]
    fn staleness_counts_changed_new_and_removed_files() {
        let database = Database::open_in_memory_for_tests();
        let project = temporary_project();
        let key = project.key();
        let root = project.root();

        // `indexed.rs` is indexed and untouched, `edited.rs` is indexed then modified, `new.rs`
        // appeared after indexing, and `gone.rs` was indexed but no longer exists on disk.
        fs::write(root.join("indexed.rs"), "fn a() {}").unwrap();
        fs::write(root.join("edited.rs"), "fn b() {}").unwrap();
        database.set_indexed_file(&key, "indexed.rs", "h1").unwrap();
        database.set_indexed_file(&key, "edited.rs", "h2").unwrap();
        database.set_indexed_file(&key, "gone.rs", "h3").unwrap();
        fs::write(root.join("new.rs"), "fn c() {}").unwrap();

        // Push the mtime forward instead of sleeping, so the test stays fast and deterministic.
        let edited = fs::OpenOptions::new()
            .write(true)
            .open(root.join("edited.rs"))
            .unwrap();
        edited
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(120))
            .unwrap();

        let staleness = index_staleness(&database, &project).unwrap().unwrap();

        assert_eq!(
            staleness,
            IndexStaleness {
                changed: 1,
                added: 1,
                removed: 1,
            }
        );
        assert!(!staleness.is_fresh());
        assert_eq!(staleness.describe(), "1 changed, 1 new, 1 removed");
    }

    #[test]
    fn staleness_is_fresh_when_every_indexed_file_is_untouched() {
        let database = Database::open_in_memory_for_tests();
        let project = temporary_project();
        fs::write(project.root().join("a.rs"), "fn a() {}").unwrap();
        database
            .set_indexed_file(&project.key(), "a.rs", "h1")
            .unwrap();

        let staleness = index_staleness(&database, &project).unwrap().unwrap();

        assert!(staleness.is_fresh(), "{staleness:?}");
        assert_eq!(staleness.describe(), "");
    }

    #[tokio::test]
    async fn run_index_requires_an_embedding_model() {
        let root = temporary_directory().canonicalize().unwrap();
        let project = ProjectContext::from_root(root.clone()).unwrap();
        let database = Database::open_in_memory_for_tests();
        let active = profile_with_embedding(None);

        let error = run_index(&UnreachableProvider, &active, &database, &project)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("embedding_model"));
        fs::remove_dir_all(root).unwrap();
    }
}
