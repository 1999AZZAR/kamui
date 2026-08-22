//! User vs project `settings.json` for `disabledSkills`.
//!
//! Two files are consulted (union):
//!   - user:    `<global_config_dir>/settings.json`  (e.g. `~/.config/kamui/settings.json`)
//!   - project: `<project>/.kamui/settings.json`
//!
//! Each file stores `{ "disabledSkills": ["skill-a", ...] }` and preserves any other keys.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::skills::{Skill, SkillSource};

const KEY: &str = "disabledSkills";

pub fn user_settings_path() -> Result<PathBuf> {
    Ok(crate::config::global_config_dir()?.join("settings.json"))
}

pub fn project_settings_path(project_root: &Path) -> PathBuf {
    project_root.join(".kamui/settings.json")
}

/// Union of user + project disabled skills. Missing or unreadable files are treated as empty.
pub fn load_disabled_skills(project_root: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Ok(path) = user_settings_path() {
        out.extend(read_disabled_from_file(&path).unwrap_or_default());
    }
    out.extend(read_disabled_from_file(&project_settings_path(project_root)).unwrap_or_default());
    out
}

fn read_disabled_from_file(path: &Path) -> Result<HashSet<String>> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(e) => return Err(e.into()),
    };
    if content.trim().is_empty() {
        return Ok(HashSet::new());
    }
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Ok(HashSet::new()),
    };
    let mut set = HashSet::new();
    if let Some(arr) = value.get(KEY).and_then(|v| v.as_array()) {
        for item in arr {
            if let Some(s) = item.as_str() {
                let s = s.trim().to_ascii_lowercase();
                if !s.is_empty() {
                    set.insert(s);
                }
            }
        }
    }
    Ok(set)
}

fn write_disabled_to_file(path: &Path, set: &HashSet<String>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut value = match std::fs::read_to_string(path) {
        Ok(c) if !c.trim().is_empty() => serde_json::from_str::<serde_json::Value>(&c)
            .unwrap_or(serde_json::Value::Object(Default::default())),
        _ => serde_json::Value::Object(Default::default()),
    };
    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings.json must be a JSON object"))?;

    if set.is_empty() {
        obj.remove(KEY);
        // If the file would become empty, remove it rather than leaving `{}`.
        if obj.is_empty() {
            let _ = std::fs::remove_file(path);
            return Ok(());
        }
    } else {
        let mut sorted: Vec<String> = set.iter().cloned().collect();
        sorted.sort();
        obj.insert(
            KEY.to_string(),
            serde_json::Value::Array(sorted.into_iter().map(serde_json::Value::String).collect()),
        );
    }

    let pretty = serde_json::to_string_pretty(&value)?;
    std::fs::write(path, pretty + "\n")?;
    Ok(())
}

/// Persist a toggle. Project skills go to the project file, global skills to the user file.
/// When enabling, the name is removed from *both* files so a stale entry in the other scope
/// cannot keep the skill disabled via the union.
pub fn set_skill_disabled(project_root: &Path, skill: &Skill, disabled: bool) -> Result<()> {
    let project_path = project_settings_path(project_root);
    let user_path = user_settings_path()?;

    let is_project = matches!(
        skill.source,
        SkillSource::ProjectKamui | SkillSource::ProjectAgents
    );

    if disabled {
        // Add to the owning scope.
        let target = if is_project {
            &project_path
        } else {
            &user_path
        };
        let mut set = read_disabled_from_file(target).unwrap_or_default();
        set.insert(skill.name.clone());
        write_disabled_to_file(target, &set)?;
    } else {
        // Remove from both scopes — union means either file can keep it disabled.
        for path in [&project_path, &user_path] {
            let mut set = read_disabled_from_file(path).unwrap_or_default();
            if set.remove(&skill.name) {
                write_disabled_to_file(path, &set)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    fn tmp_project() -> PathBuf {
        let p = std::env::temp_dir().join(format!("kamui-settings-{}", Uuid::new_v4()));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn read_missing_file_is_empty() {
        let p = tmp_project();
        let set = read_disabled_from_file(&p.join("nope.json")).unwrap();
        assert!(set.is_empty());
        fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn write_and_read_round_trips_and_preserves_other_keys() {
        let p = tmp_project();
        let path = p.join("settings.json");
        fs::write(&path, r#"{"other": 123}"#).unwrap();
        let mut set = HashSet::new();
        set.insert("my-skill".to_string());
        write_disabled_to_file(&path, &set).unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["other"], 123);
        assert_eq!(raw["disabledSkills"][0], "my-skill");
        let back = read_disabled_from_file(&path).unwrap();
        assert!(back.contains("my-skill"));
        fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn empty_set_removes_key_and_file_when_alone() {
        let p = tmp_project();
        let path = p.join("settings.json");
        let mut set = HashSet::new();
        set.insert("a".to_string());
        write_disabled_to_file(&path, &set).unwrap();
        assert!(path.exists());
        write_disabled_to_file(&path, &HashSet::new()).unwrap();
        assert!(!path.exists());
        fs::remove_dir_all(p).unwrap();
    }
}
