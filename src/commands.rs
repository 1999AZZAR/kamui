//! User-defined prompt commands: markdown files invoked as `/<name>` that expand into the prompt
//! for that turn. Purely text expansion — a command grants no new capability and changes no
//! permission, so it carries the same risk as the `KAMUI.md` project instructions that already
//! ship. There is deliberately no chaining or orchestration between commands: each one stands
//! alone and can be invoked at any time, in any order.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Directory name holding a project's own commands, alongside a project `kamui.toml`.
const PROJECT_DIR: &str = ".kamui/commands";
/// Directory name holding global commands, inside Kamui's OS config directory.
const GLOBAL_DIR: &str = "commands";

/// Built-in chat commands a custom command may not shadow, so a stray file can never take over
/// `/new` or `/exit`. Kept in sync with `chat::print_help` and the chat loop's own dispatch.
const RESERVED: [&str; 20] = [
    "help", "new", "sessions", "resume", "model", "rename", "search", "compact", "undo", "jobs",
    "index", "delete", "stats", "status", "memory", "forget", "exit", "commands", "skills",
    "usage",
];

/// Where a command was loaded from. A project command shadows a global one of the same name, the
/// same way a project `kamui.toml` overrides the global file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Global,
    Project,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Global => "global",
            Source::Project => "project",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CustomCommand {
    pub name: String,
    pub description: Option<String>,
    pub body: String,
    pub source: Source,
}

/// Every custom command available in the current project, global and project-local merged.
#[derive(Debug, Default)]
pub struct CommandLibrary {
    commands: Vec<CustomCommand>,
}

impl CommandLibrary {
    /// Load global commands then project ones, letting a project command replace a global command
    /// of the same name. A missing or unreadable directory simply contributes nothing — custom
    /// commands are optional, so their absence must never stop a chat from starting.
    pub fn load(project_root: &Path) -> Self {
        let mut commands: Vec<CustomCommand> = Vec::new();
        if let Ok(dir) = global_dir() {
            load_dir(&dir, Source::Global, &mut commands);
        }
        load_dir(
            &project_root.join(PROJECT_DIR),
            Source::Project,
            &mut commands,
        );
        commands.sort_by(|a, b| a.name.cmp(&b.name));
        Self { commands }
    }

    pub fn find(&self, name: &str) -> Option<&CustomCommand> {
        self.commands.iter().find(|command| command.name == name)
    }

    pub fn list(&self) -> &[CustomCommand] {
        &self.commands
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Expand a `/<name> [extra text]` line into the prompt to send. Returns `None` when the input
    /// is not a custom command, so the caller falls through to built-in command handling. Trailing
    /// text is appended below the command body rather than replacing it, so `/review @src/a.rs`
    /// keeps working through the normal `@file` expansion the chat loop already performs.
    pub fn expand(&self, input: &str) -> Option<String> {
        let rest = input.strip_prefix('/')?;
        let (name, argument) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let command = self.find(name)?;
        let argument = argument.trim();
        Some(if argument.is_empty() {
            command.body.clone()
        } else {
            format!("{}\n\n{argument}", command.body)
        })
    }
}

/// The global commands directory, beside `kamui.toml` in the OS config directory.
fn global_dir() -> Result<PathBuf> {
    crate::config::global_config_dir().map(|dir| dir.join(GLOBAL_DIR))
}

fn load_dir(dir: &Path, source: Source, commands: &mut Vec<CustomCommand>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_ascii_lowercase)
        else {
            continue;
        };
        if !is_valid_name(&name) || RESERVED.contains(&name.as_str()) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (description, body) = split_frontmatter(&content);
        if body.trim().is_empty() {
            continue;
        }
        let command = CustomCommand {
            name,
            description,
            body: body.trim().to_string(),
            source,
        };
        // A project command replaces a global one of the same name rather than adding a duplicate.
        match commands
            .iter()
            .position(|existing| existing.name == command.name)
        {
            Some(index) => commands[index] = command,
            None => commands.push(command),
        }
    }
}

/// Command names are restricted to what can be typed unambiguously after a slash.
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}

/// Split an optional leading `---` frontmatter block off the body, returning any `description`
/// found in it. Only `description` is read; everything else in the block is ignored, so files
/// written for another tool (with `tags:`, `agents:`, and so on) can be used unchanged.
fn split_frontmatter(content: &str) -> (Option<String>, &str) {
    let normalized = content.strip_prefix('\u{feff}').unwrap_or(content);
    let Some(rest) = normalized.strip_prefix("---") else {
        return (None, normalized);
    };
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"));
    let Some(rest) = rest else {
        return (None, normalized);
    };
    // The block ends at the first line that is exactly `---`.
    let Some((front, body)) = split_at_closing_fence(rest) else {
        return (None, normalized);
    };
    let description = front.lines().find_map(|line| {
        let value = line.trim().strip_prefix("description:")?.trim();
        (!value.is_empty()).then(|| value.trim_matches(['"', '\'']).to_string())
    });
    (description, body)
}

fn split_at_closing_fence(rest: &str) -> Option<(&str, &str)> {
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    fn project() -> PathBuf {
        let path = std::env::temp_dir().join(format!("kamui-commands-{}", Uuid::new_v4()));
        fs::create_dir_all(path.join(PROJECT_DIR)).unwrap();
        path
    }

    fn write_command(root: &Path, name: &str, content: &str) {
        fs::write(root.join(PROJECT_DIR).join(format!("{name}.md")), content).unwrap();
    }

    #[test]
    fn loads_a_project_command_with_its_description() {
        let root = project();
        write_command(
            &root,
            "review",
            "---\ndescription: Pragmatic reviewer\n---\n\nYou are a reviewer.\n",
        );

        let library = CommandLibrary::load(&root);
        let command = library.find("review").unwrap();

        assert_eq!(command.description.as_deref(), Some("Pragmatic reviewer"));
        assert_eq!(command.body, "You are a reviewer.");
        assert_eq!(command.source, Source::Project);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_file_without_frontmatter_is_used_verbatim() {
        let root = project();
        write_command(
            &root,
            "test",
            "# Testing Agent\n\nYou are a test engineer.\n",
        );

        let library = CommandLibrary::load(&root);
        let command = library.find("test").unwrap();

        assert!(command.description.is_none());
        assert!(command.body.starts_with("# Testing Agent"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn frontmatter_written_for_another_tool_is_tolerated() {
        let root = project();
        // The shape used by the user's existing Obsidian prompt library: no `description` key.
        write_command(
            &root,
            "execution",
            "---\ntags:\n  - agent\nagents:\n  - claude-code\n---\n\nYou are a builder.\n",
        );

        let library = CommandLibrary::load(&root);
        let command = library.find("execution").unwrap();

        assert!(command.description.is_none());
        assert_eq!(command.body, "You are a builder.");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expand_returns_the_body_and_appends_trailing_text() {
        let root = project();
        write_command(&root, "review", "You are a reviewer.");
        let library = CommandLibrary::load(&root);

        assert_eq!(library.expand("/review").unwrap(), "You are a reviewer.");
        assert_eq!(
            library.expand("/review @src/a.rs").unwrap(),
            "You are a reviewer.\n\n@src/a.rs"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expand_ignores_unknown_and_built_in_commands() {
        let root = project();
        write_command(&root, "review", "You are a reviewer.");
        let library = CommandLibrary::load(&root);

        assert!(library.expand("/nope").is_none());
        assert!(library.expand("/new").is_none());
        assert!(library.expand("plain text").is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_file_named_after_a_built_in_command_is_skipped() {
        let root = project();
        write_command(&root, "new", "This must never shadow /new.");
        write_command(&root, "compact", "Nor /compact.");

        let library = CommandLibrary::load(&root);

        assert!(library.find("new").is_none());
        assert!(library.find("compact").is_none());
        assert!(library.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn non_markdown_empty_and_oddly_named_files_are_skipped() {
        let root = project();
        fs::write(root.join(PROJECT_DIR).join("notes.txt"), "not a command").unwrap();
        write_command(&root, "blank", "---\ndescription: nothing\n---\n\n   \n");
        write_command(&root, "has space", "body");

        let library = CommandLibrary::load(&root);

        assert!(library.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_directories_yield_no_commands() {
        let root = std::env::temp_dir().join(format!("kamui-nonexistent-{}", Uuid::new_v4()));
        assert!(CommandLibrary::load(&root).is_empty());
    }

    #[test]
    fn command_names_are_lowercased_from_the_file_stem() {
        let root = project();
        write_command(&root, "Review-Agent", "You are a reviewer.");

        let library = CommandLibrary::load(&root);

        assert!(library.find("review-agent").is_some());
        fs::remove_dir_all(root).unwrap();
    }
}
