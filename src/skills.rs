//! Skills: folder-based `SKILL.md` bundles (Agent Skills open standard) with progressive
//! disclosure. Only `name`+`description` are eagerly injected into the system prompt;
//! the full body is loaded lazily on `/<skill-name>` or `/skill:<name>`.
//!
//! Discovery checks four locations in priority order:
//!   1. `<project>/.kamui/skills/`
//!   2. `<project>/.agents/skills/` (compat)
//!   3. `<global>/.kamui/skills/`  (OS config dir, e.g. `~/.config/kamui/skills`)
//!   4. `<global>/.agents/skills/` (compat, `~/.config/agents/skills`)
//!
//! Project shadows global; `.kamui` wins over `.agents` at the same level.

use anyhow::Context;
use std::path::{Path, PathBuf};

/// Where a skill was discovered. Ordered by precedence (lower index = higher priority).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    ProjectKamui,
    ProjectAgents,
    GlobalKamui,
    GlobalAgents,
}

impl SkillSource {
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            SkillSource::ProjectKamui => "project:.kamui",
            SkillSource::ProjectAgents => "project:.agents",
            SkillSource::GlobalKamui => "global:.kamui",
            SkillSource::GlobalAgents => "global:.agents",
        }
    }

    pub fn badge(self) -> &'static str {
        match self {
            SkillSource::ProjectKamui => "[project]",
            SkillSource::ProjectAgents => "[project:.agents]",
            SkillSource::GlobalKamui => "[global]",
            SkillSource::GlobalAgents => "[global:.agents]",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub source: SkillSource,
    pub allowed_tools: Option<String>,
    #[allow(dead_code)]
    pub when_to_use: Option<String>,
    #[allow(dead_code)]
    pub argument_hint: Option<String>,
}

/// All discovered skills, deduplicated by precedence, plus warnings for invalid ones.
#[derive(Debug, Default)]
pub struct SkillLibrary {
    skills: Vec<Skill>,
    warnings: Vec<String>,
}

impl SkillLibrary {
    /// Discover skills from the four locations. Missing/unreadable directories are silently
    /// ignored; invalid skills are skipped with a warning (never a crash).
    pub fn load(project_root: &Path) -> Self {
        Self::load_with_global(project_root, true)
    }

    fn load_with_global(project_root: &Path, include_global: bool) -> Self {
        let mut skills: Vec<Skill> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        // Project locations (highest priority first).
        load_dir(
            &project_root.join(".kamui/skills"),
            SkillSource::ProjectKamui,
            &mut skills,
            &mut warnings,
        );
        load_dir(
            &project_root.join(".agents/skills"),
            SkillSource::ProjectAgents,
            &mut skills,
            &mut warnings,
        );

        // Global locations.
        if include_global {
            if let Ok(dir) = global_kamui_skills_dir() {
                load_dir(&dir, SkillSource::GlobalKamui, &mut skills, &mut warnings);
            }
            if let Ok(dir) = global_agents_skills_dir() {
                load_dir(&dir, SkillSource::GlobalAgents, &mut skills, &mut warnings);
            }
        }

        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Self { skills, warnings }
    }

    pub fn find(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    #[allow(dead_code)]
    pub fn list(&self) -> &[Skill] {
        &self.skills
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Expand `/<name> [extra]` or `/skill:<name> [extra]` into the skill body.
    /// Returns `None` if the input does not match a known skill.
    /// Trailing text is appended below the body (same as `CommandLibrary::expand`).
    /// Bare `/<name>` does not shadow a built-in (use `/skill:<name>` for collisions).
    pub fn expand(&self, input: &str) -> Option<String> {
        let rest = input.strip_prefix('/')?;
        // Namespaced form: /skill:<name> [args] — always works, even for collisions.
        if let Some(after) = rest.strip_prefix("skill:") {
            let (name, argument) = after.split_once(char::is_whitespace).unwrap_or((after, ""));
            let name = name.trim().to_ascii_lowercase();
            let skill = self.find(&name)?;
            let argument = argument.trim();
            return Some(if argument.is_empty() {
                skill.body.clone()
            } else {
                format!("{}\n\n{argument}", skill.body)
            });
        }
        // Bare form: /<name> [args] — must not shadow a built-in.
        let (name, argument) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        if crate::tui::is_builtin_command(name) {
            return None;
        }
        let name = name.trim().to_ascii_lowercase();
        let skill = self.find(&name)?;
        let argument = argument.trim();
        Some(if argument.is_empty() {
            skill.body.clone()
        } else {
            format!("{}\n\n{argument}", skill.body)
        })
    }

    /// Like `load` but filters out skills whose names are in `disabled`.
    /// The full library is still returned as `self` for the popup; this is only for the
    /// two injection points (eager prompt + lazy expand).
    pub fn enabled_only(&self, disabled: &std::collections::HashSet<String>) -> Vec<&Skill> {
        self.skills
            .iter()
            .filter(|s| !disabled.contains(&s.name))
            .collect()
    }

    /// Render the eager block for the system prompt: `name: description` per skill.
    /// `allowed-tools` is included as a hint when present (not a filter).
    /// Disabled skills are omitted.
    #[allow(dead_code)]
    pub fn eager_block(&self) -> Option<String> {
        self.eager_block_filtered(&std::collections::HashSet::new())
    }

    pub fn eager_block_filtered(
        &self,
        disabled: &std::collections::HashSet<String>,
    ) -> Option<String> {
        let enabled: Vec<&Skill> = self.enabled_only(disabled);
        if enabled.is_empty() {
            return None;
        }
        let mut block =
            String::from("Available skills (invoke with /<skill-name> or /skill:<name>):");
        for skill in enabled {
            block.push_str(&format!("\n- {}: {}", skill.name, skill.description));
            if let Some(tools) = &skill.allowed_tools {
                block.push_str(&format!(" (tools: {tools})"));
            }
            block.push_str(&format!(" {}", skill.source.badge()));
        }
        Some(block)
    }

    /// Filtered expand: disabled skills do not expand (both bare and namespaced).
    pub fn expand_filtered(
        &self,
        input: &str,
        disabled: &std::collections::HashSet<String>,
    ) -> Option<String> {
        let rest = input.strip_prefix('/')?;
        let name = if let Some(after) = rest.strip_prefix("skill:") {
            after
                .split_once(char::is_whitespace)
                .unwrap_or((after, ""))
                .0
        } else {
            rest.split_once(char::is_whitespace).unwrap_or((rest, "")).0
        };
        if disabled.contains(name.trim().to_ascii_lowercase().as_str()) {
            return None;
        }
        self.expand(input)
    }
}

fn global_kamui_skills_dir() -> anyhow::Result<PathBuf> {
    crate::config::global_config_dir().map(|dir| dir.join("skills"))
}

fn global_agents_skills_dir() -> anyhow::Result<PathBuf> {
    // Compat: global .agents is at HOME/.agents/skills (e.g. ~/.agents/skills),
    // not beside the OS config dir (which is ~/Library/Application Support/kamui on macOS).
    let home = directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .context("could not determine home directory")?;
    Ok(home.join(".agents/skills"))
}

fn load_dir(dir: &Path, source: SkillSource, skills: &mut Vec<Skill>, warnings: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(folder_name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_ascii_lowercase)
        else {
            continue;
        };
        // Lenient like opencode: any visible directory may host a skill. Hidden dot-folders
        // are tooling noise (.git, caches) and are skipped silently.
        if folder_name.starts_with('.') {
            continue;
        }
        // Skip if a higher-priority skill with same name already exists.
        if skills.iter().any(|skill| skill.name == folder_name) {
            continue;
        }
        let skill_file = path.join("SKILL.md");
        if !skill_file.is_file() {
            warnings.push(format!(
                "skill '{}' in {} is missing SKILL.md",
                folder_name,
                dir.display()
            ));
            continue;
        }
        let content = match std::fs::read_to_string(&skill_file) {
            Ok(content) => content,
            Err(error) => {
                warnings.push(format!(
                    "skill '{}' in {} could not be read: {error}",
                    folder_name,
                    dir.display()
                ));
                continue;
            }
        };
        match parse_skill(&folder_name, &content) {
            Ok((name, description, body, allowed_tools, when_to_use, argument_hint)) => {
                skills.push(Skill {
                    name,
                    description,
                    body,
                    source,
                    allowed_tools,
                    when_to_use,
                    argument_hint,
                });
            }
            Err(reason) => {
                warnings.push(format!(
                    "skill '{}' in {} is invalid: {reason}",
                    folder_name,
                    dir.display()
                ));
            }
        }
    }
}

/// Skill names: lowercase alphanumeric + hyphens only (no underscores, per spec).
type ParsedSkill = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn parse_skill(folder_name: &str, content: &str) -> Result<ParsedSkill, String> {
    let (frontmatter, body) = split_frontmatter(content).ok_or_else(|| {
        "missing frontmatter (expected --- block with name and description)".to_string()
    })?;

    // Lenient parsing (mirrors opencode): a skill only needs SKILL.md. Missing frontmatter
    // fields fall back to the folder name / first body line so real-world skills from other
    // harnesses load instead of warning.
    // Missing fields fall back to the folder name / first body line.
    let name = frontmatter
        .get("name")
        .map(|n| n.trim().to_ascii_lowercase())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| folder_name.to_ascii_lowercase());
    let description = frontmatter
        .get("description")
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| {
            body.lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("Skill")
                .trim()
                .chars()
                .take(120)
                .collect::<String>()
        });
    let body = body.trim().to_string();
    if body.is_empty() {
        return Err("SKILL.md body is empty".to_string());
    }

    let allowed_tools = frontmatter.get("allowed-tools").cloned();
    let when_to_use = frontmatter.get("when_to_use").cloned();
    let argument_hint = frontmatter.get("argument-hint").cloned();

    Ok((
        name,
        description.trim().to_string(),
        body,
        allowed_tools,
        when_to_use,
        argument_hint,
    ))
}

fn split_frontmatter(content: &str) -> Option<(std::collections::HashMap<String, String>, &str)> {
    let normalized = content.strip_prefix('\u{feff}').unwrap_or(content);
    let rest = normalized.strip_prefix("---")?;
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))?;
    let (front, body) = split_at_closing_fence(rest)?;
    let mut map = std::collections::HashMap::new();
    for line in front.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().trim_matches(['"', '\'']).trim().to_string();
        if value.is_empty() {
            continue;
        }
        // Only keep known keys; unknown keys are ignored (compat with other tools).
        match key.as_str() {
            "name" | "description" | "allowed-tools" | "when_to_use" | "argument-hint" => {
                map.insert(key, value);
            }
            _ => {}
        }
    }
    Some((map, body))
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
        let path = std::env::temp_dir().join(format!("kamui-skills-{}", Uuid::new_v4()));
        fs::create_dir_all(path.join(".kamui/skills")).unwrap();
        fs::create_dir_all(path.join(".agents/skills")).unwrap();
        path
    }

    fn write_skill(root: &Path, subdir: &str, folder: &str, content: &str) {
        let dir = root.join(subdir).join(folder);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn loads_a_valid_skill() {
        let root = project();
        write_skill(
            &root,
            ".kamui/skills",
            "code-review",
            "---\nname: code-review\ndescription: Review code\n---\n\nYou are a reviewer.\n",
        );
        let library = SkillLibrary::load_with_global(&root, false);
        let skill = library.find("code-review").unwrap();
        assert_eq!(skill.description, "Review code");
        assert_eq!(skill.body, "You are a reviewer.");
        assert_eq!(skill.source, SkillSource::ProjectKamui);
        assert!(library.warnings().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn frontmatter_name_wins_when_it_differs_from_folder() {
        let root = project();
        write_skill(
            &root,
            ".kamui/skills",
            "my-skill",
            "---\nname: other-name\ndescription: desc\n---\n\nBody\n",
        );
        let library = SkillLibrary::load_with_global(&root, false);
        assert!(library.find("other-name").is_some());
        assert!(library.warnings().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_description_falls_back_to_first_body_line() {
        let root = project();
        write_skill(
            &root,
            ".kamui/skills",
            "bad-skill",
            "---\nname: bad-skill\n---\n\nBody first line here.\n",
        );
        let library = SkillLibrary::load_with_global(&root, false);
        let skill = library.find("bad-skill").unwrap();
        assert_eq!(skill.description, "Body first line here.");
        assert!(library.warnings().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn odd_folder_names_still_load_but_hidden_dirs_are_skipped() {
        let root = project();
        write_skill(
            &root,
            ".kamui/skills",
            "Bad_Name",
            "---\nname: Bad_Name\ndescription: desc\n---\n\nBody\n",
        );
        // Hidden dot-folders are tooling noise and are skipped silently.
        write_skill(
            &root,
            ".kamui/skills",
            ".system",
            "---\nname: system\ndescription: desc\n---\n\nBody\n",
        );
        let library = SkillLibrary::load_with_global(&root, false);
        assert!(library.find("bad_name").is_some());
        assert!(library.find("system").is_none());
        assert!(library.warnings().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_shadows_global_and_kamui_wins_over_agents() {
        let root = project();
        // Project .kamui should win over project .agents and both globals.
        write_skill(
            &root,
            ".kamui/skills",
            "my-skill",
            "---\nname: my-skill\ndescription: from kamui\n---\n\nKamui body\n",
        );
        write_skill(
            &root,
            ".agents/skills",
            "my-skill",
            "---\nname: my-skill\ndescription: from agents\n---\n\nAgents body\n",
        );
        let library = SkillLibrary::load_with_global(&root, false);
        assert_eq!(library.list().len(), 1);
        assert_eq!(library.find("my-skill").unwrap().description, "from kamui");
        assert_eq!(
            library.find("my-skill").unwrap().source,
            SkillSource::ProjectKamui
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn allowed_tools_is_parsed_as_hint() {
        let root = project();
        write_skill(
            &root,
            ".kamui/skills",
            "my-skill",
            "---\nname: my-skill\ndescription: desc\nallowed-tools: read_file, grep\n---\n\nBody\n",
        );
        let library = SkillLibrary::load_with_global(&root, false);
        let skill = library.find("my-skill").unwrap();
        assert_eq!(skill.allowed_tools.as_deref(), Some("read_file, grep"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expand_bare_and_namespaced() {
        let root = project();
        write_skill(
            &root,
            ".kamui/skills",
            "my-skill",
            "---\nname: my-skill\ndescription: desc\n---\n\nBody text\n",
        );
        let library = SkillLibrary::load_with_global(&root, false);
        assert_eq!(library.expand("/my-skill").unwrap(), "Body text");
        assert_eq!(library.expand("/skill:my-skill").unwrap(), "Body text");
        assert_eq!(
            library.expand("/my-skill extra args").unwrap(),
            "Body text\n\nextra args"
        );
        assert_eq!(
            library.expand("/skill:my-skill extra").unwrap(),
            "Body text\n\nextra"
        );
        assert!(library.expand("/unknown").is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn eager_block_contains_name_and_description() {
        let root = project();
        write_skill(
            &root,
            ".kamui/skills",
            "my-skill",
            "---\nname: my-skill\ndescription: Does stuff\n---\n\nBody\n",
        );
        let library = SkillLibrary::load_with_global(&root, false);
        let block = library.eager_block().unwrap();
        assert!(block.contains("my-skill"));
        assert!(block.contains("Does stuff"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_skill_md_is_warning() {
        let root = project();
        fs::create_dir_all(root.join(".kamui/skills/empty-skill")).unwrap();
        let library = SkillLibrary::load_with_global(&root, false);
        assert!(library.is_empty());
        assert!(
            library
                .warnings()
                .iter()
                .any(|warning| warning.contains("missing SKILL.md"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bare_expand_does_not_shadow_builtin_but_namespaced_does() {
        let root = project();
        write_skill(
            &root,
            ".kamui/skills",
            "help",
            "---\nname: help\ndescription: Help skill\n---\n\nHelp body\n",
        );
        let library = SkillLibrary::load_with_global(&root, false);
        // Bare /help must not expand the skill (reserved).
        assert!(library.expand("/help").is_none());
        // Namespaced form still works.
        assert_eq!(library.expand("/skill:help").unwrap(), "Help body");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skill_without_frontmatter_is_invalid() {
        let root = project();
        write_skill(
            &root,
            ".kamui/skills",
            "no-frontmatter",
            "Just body without frontmatter\n",
        );
        let library = SkillLibrary::load_with_global(&root, false);
        assert!(library.is_empty());
        assert!(!library.warnings().is_empty());
        fs::remove_dir_all(root).unwrap();
    }
}
