use std::io::IsTerminal;
use std::time::Duration;

const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";

#[derive(Clone, Copy)]
pub struct Ui {
    interactive: bool,
    color: bool,
}

impl Ui {
    pub fn stdio() -> Self {
        let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        Self {
            interactive,
            color: interactive && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    pub fn interactive(self) -> bool {
        self.interactive
    }

    pub fn tool_outcome(self, output: &str, elapsed: Duration) -> String {
        if !self.interactive {
            return match output.strip_prefix("Error: ") {
                Some(error) => format!("    ! {error}"),
                None => format!("    ok ({} chars)", output.chars().count()),
            };
        }

        match output.strip_prefix("Error: ") {
            Some(error) => format!(
                "    {} failed · {} · {error}",
                self.paint("x", RED),
                format_duration(elapsed)
            ),
            None => format!(
                "    {} completed · {} · {} chars",
                self.paint("✓", GREEN),
                format_duration(elapsed),
                output.chars().count()
            ),
        }
    }

    fn paint(self, text: &str, color: &str) -> String {
        if self.color {
            format!("{color}{text}{RESET}")
        } else {
            text.to_string()
        }
    }
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() > 0 {
        format!("{:.1}s", duration.as_secs_f64())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_tool_outcomes_match_script_friendly_output() {
        let ui = Ui {
            interactive: false,
            color: false,
        };
        assert_eq!(
            ui.tool_outcome("hello", Duration::from_millis(4)),
            "    ok (5 chars)"
        );
        assert_eq!(
            ui.tool_outcome("Error: nope", Duration::from_millis(4)),
            "    ! nope"
        );
    }

    #[test]
    fn interactive_outcome_includes_lifecycle_metadata() {
        let ui = Ui {
            interactive: true,
            color: false,
        };
        assert_eq!(
            ui.tool_outcome("done", Duration::from_millis(12)),
            "    ✓ completed · 12ms · 4 chars"
        );
    }
}
