use crate::storage::{Database, ScheduledJob};
use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

const JOB_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const JOB_LEASE_SECS: i64 = 31 * 60;
const MAX_OUTPUT: usize = 16 * 1024;

pub enum Command {
    List,
    Add {
        command: String,
        next_run_at: i64,
        interval_secs: Option<i64>,
    },
    Cancel(String),
    Pause(String),
    Resume(String),
    Worker {
        once: bool,
    },
}

pub fn parse(tokens: &[String]) -> Result<Command> {
    const USAGE: &str = "usage: kamui jobs <list|add|cancel|pause|resume|worker> ...";
    match tokens.first().map(String::as_str) {
        None | Some("list") if tokens.len() <= 1 => Ok(Command::List),
        Some("cancel") => Ok(Command::Cancel(single_argument(tokens, USAGE)?)),
        Some("pause") => Ok(Command::Pause(single_argument(tokens, USAGE)?)),
        Some("resume") => Ok(Command::Resume(single_argument(tokens, USAGE)?)),
        Some("worker") => match &tokens[1..] {
            [] => Ok(Command::Worker { once: false }),
            [flag] if flag == "--once" => Ok(Command::Worker { once: true }),
            _ => anyhow::bail!(USAGE),
        },
        Some("add") => parse_add(&tokens[1..]),
        _ => anyhow::bail!(USAGE),
    }
}

fn single_argument(tokens: &[String], usage: &str) -> Result<String> {
    match &tokens[1..] {
        [id] => Ok(id.clone()),
        _ => anyhow::bail!(usage.to_string()),
    }
}

fn parse_add(tokens: &[String]) -> Result<Command> {
    const USAGE: &str =
        "usage: kamui jobs add <--now|--at <RFC3339>|--every <duration>> -- <command>";
    let separator = tokens
        .iter()
        .position(|token| token == "--")
        .context(USAGE)?;
    let command = tokens[separator + 1..].join(" ");
    if command.trim().is_empty() {
        anyhow::bail!(USAGE);
    }
    let now = chrono::Utc::now().timestamp();
    let (next_run_at, interval_secs) = match &tokens[..separator] {
        [flag] if flag == "--now" => (now, None),
        [flag, value] if flag == "--at" => (parse_time(value)?, None),
        [flag, value] if flag == "--every" => {
            let interval = parse_duration(value)?;
            (now + interval, Some(interval))
        }
        _ => anyhow::bail!(USAGE),
    };
    Ok(Command::Add {
        command,
        next_run_at,
        interval_secs,
    })
}

fn parse_time(value: &str) -> Result<i64> {
    DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("invalid RFC3339 time '{value}'"))
        .map(|time| time.timestamp())
}

fn parse_duration(value: &str) -> Result<i64> {
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .context("duration requires a unit: s, m, h, or d")?;
    let amount: i64 = value[..split]
        .parse()
        .with_context(|| format!("invalid duration '{value}'"))?;
    let multiplier = match &value[split..] {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => anyhow::bail!("invalid duration unit in '{value}'; use s, m, h, or d"),
    };
    let seconds = amount
        .checked_mul(multiplier)
        .context("duration is too large")?;
    if seconds < 60 {
        anyhow::bail!("recurring jobs require an interval of at least 60 seconds");
    }
    Ok(seconds)
}

pub async fn run(command: Command) -> Result<()> {
    let database = Database::open()?;
    match command {
        Command::List => print_jobs(&database.list_scheduled_jobs()?),
        Command::Add {
            command,
            next_run_at,
            interval_secs,
        } => {
            let cwd = std::env::current_dir().context("could not determine working directory")?;
            let id = database.create_scheduled_job(
                &command,
                &cwd.to_string_lossy(),
                next_run_at,
                interval_secs,
            )?;
            println!("Scheduled job {id} for {}", format_time(next_run_at));
        }
        Command::Cancel(id) => change_job(database.cancel_scheduled_job(&id)?, &id, "cancelled")?,
        Command::Pause(id) => change_job(database.pause_scheduled_job(&id)?, &id, "paused")?,
        Command::Resume(id) => change_job(
            database.resume_scheduled_job(&id, chrono::Utc::now().timestamp())?,
            &id,
            "resumed",
        )?,
        Command::Worker { once } => run_worker(&database, once).await?,
    }
    Ok(())
}

fn change_job(changed: bool, id: &str, action: &str) -> Result<()> {
    if !changed {
        anyhow::bail!("job '{id}' was not found or cannot be {action}");
    }
    println!("Job {id} {action}.");
    Ok(())
}

pub fn format_jobs(jobs: &[ScheduledJob]) -> String {
    if jobs.is_empty() {
        return "no scheduled jobs".to_string();
    }
    jobs.iter()
        .map(|job| {
            let next = job
                .next_run_at
                .map(format_time)
                .unwrap_or_else(|| "-".to_string());
            let schedule = job
                .interval_secs
                .map(|seconds| format!("every {}", format_duration(seconds)))
                .unwrap_or_else(|| "once".to_string());
            let result = job
                .last_exit_code
                .map(|code| format!(" · exit {code}"))
                .unwrap_or_default();
            let error = if job.status == "failed" && !job.stderr.trim().is_empty() {
                format!(" · {}", job.stderr.lines().next().unwrap_or_default())
            } else {
                String::new()
            };
            let output = if job.status == "succeeded" && !job.stdout.trim().is_empty() {
                format!(" · {}", job.stdout.lines().next().unwrap_or_default())
            } else {
                String::new()
            };
            let disabled = if job.enabled { "" } else { " · disabled" };
            format!(
                "{}  {:<11}  {:<16}  {:<10}  {}{result}{error}{output}{disabled}",
                job.id, job.status, next, schedule, job.command
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn print_jobs(jobs: &[ScheduledJob]) {
    println!("{}", format_jobs(jobs));
}

async fn run_worker(database: &Database, once: bool) -> Result<()> {
    let worker_id = uuid::Uuid::new_v4().to_string();
    let recovered = database.recover_expired_jobs(chrono::Utc::now().timestamp())?;
    if recovered > 0 {
        println!("Marked {recovered} unfinished job(s) as interrupted.");
    }
    println!("Job worker started{}.", if once { " (once)" } else { "" });

    loop {
        let now = chrono::Utc::now().timestamp();
        match database.claim_due_job(now, &worker_id, now + JOB_LEASE_SECS)? {
            Some(job) => {
                println!("Running {}: {}", job.id, job.command);
                if execute_job(database, &job).await? {
                    return Ok(());
                }
            }
            None if once => return Ok(()),
            None => {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {},
                    signal = tokio::signal::ctrl_c() => {
                        signal.context("failed to listen for Ctrl+C")?;
                        println!("Worker stopped.");
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Returns true when Ctrl+C interrupted the worker.
async fn execute_job(database: &Database, job: &ScheduledJob) -> Result<bool> {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let child = tokio::process::Command::new(shell)
        .arg(flag)
        .arg(&job.command)
        .current_dir(PathBuf::from(&job.cwd))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to start job {}", job.id))?;

    let outcome = tokio::select! {
        result = tokio::time::timeout(JOB_TIMEOUT, child.wait_with_output()) => Some(result),
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for Ctrl+C")?;
            None
        }
    };

    let Some(outcome) = outcome else {
        database.interrupt_scheduled_job(&job.id, "worker interrupted by Ctrl+C")?;
        println!("Interrupted {}.", job.id);
        return Ok(true);
    };
    let (exit_code, stdout, stderr) = match outcome {
        Ok(Ok(output)) => (
            output.status.code().unwrap_or(-1),
            truncate_output(&String::from_utf8_lossy(&output.stdout)),
            truncate_output(&String::from_utf8_lossy(&output.stderr)),
        ),
        Ok(Err(error)) => (
            -1,
            String::new(),
            format!("failed to wait for job: {error}"),
        ),
        Err(_) => (
            -1,
            String::new(),
            "job timed out after 1800 seconds".to_string(),
        ),
    };
    database.finish_scheduled_job(
        job,
        exit_code,
        &stdout,
        &stderr,
        chrono::Utc::now().timestamp(),
    )?;
    println!("Finished {} with exit code {exit_code}.", job.id);
    Ok(false)
}

fn truncate_output(output: &str) -> String {
    if output.len() <= MAX_OUTPUT {
        return output.to_string();
    }
    let mut end = MAX_OUTPUT;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[output truncated]", &output[..end])
}

fn format_time(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|time| time.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| timestamp.to_string())
}

fn format_duration(seconds: i64) -> String {
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
        if seconds % size == 0 {
            return format!("{}{}", seconds / size, unit);
        }
    }
    format!("{seconds}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn duration_parser_supports_common_units() {
        assert_eq!(parse_duration("60s").unwrap(), 60);
        assert_eq!(parse_duration("15m").unwrap(), 900);
        assert_eq!(parse_duration("2h").unwrap(), 7200);
        assert_eq!(parse_duration("1d").unwrap(), 86400);
        assert!(parse_duration("30s").is_err());
    }

    #[test]
    fn parses_recurring_job_command() {
        let command = parse(&strings(&["add", "--every", "5m", "--", "cargo", "test"])).unwrap();
        assert!(matches!(
            command,
            Command::Add { command, interval_secs: Some(300), .. } if command == "cargo test"
        ));
    }

    #[test]
    fn list_output_contains_status_and_command() {
        let job = ScheduledJob {
            id: "abc12345".to_string(),
            command: "cargo test".to_string(),
            cwd: "/tmp".to_string(),
            interval_secs: None,
            next_run_at: Some(0),
            enabled: true,
            status: "scheduled".to_string(),
            last_exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            worker_id: None,
        };
        let output = format_jobs(&[job]);
        assert!(output.contains("scheduled"));
        assert!(output.contains("cargo test"));
    }
}
