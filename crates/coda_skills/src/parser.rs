use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_yml::Value;

use crate::error::{Result, SkillError};
use crate::models::{SkillProperties, SkillPropertiesWithLocation, Skills};

fn find_skill_md(skill_dir: &Path) -> Option<PathBuf> {
    for name in &["SKILL.md", "skill.md"] {
        let path = skill_dir.join(name);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

fn parse_frontmatter(content: &str) -> Result<(HashMap<String, Value>, String)> {
    if !content.starts_with("---") {
        return Err(SkillError::Parse(
            "SKILL.md must start with YAML frontmatter (---)".to_string(),
        ));
    }

    let parts: Vec<&str> = content.splitn(3, "---").collect();
    if parts.len() < 3 {
        return Err(SkillError::Parse(
            "SKILL.md frontmatter not properly closed with ---".to_string(),
        ));
    }

    let frontmatter_str = parts[1];
    let body = parts[2].trim().to_string();

    let metadata: HashMap<String, Value> = serde_yml::from_str(frontmatter_str)
        .map_err(|e| SkillError::Parse(format!("Invalid YAML in frontmatter: {}", e)))?;

    Ok((metadata, body))
}

fn value_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

pub fn read_skills(skill_dir: &Path) -> Result<SkillPropertiesWithLocation> {
    let skill_md = find_skill_md(skill_dir).ok_or_else(|| {
        SkillError::Parse(format!("SKILL.md not found in {}", skill_dir.display()))
    })?;

    let content = std::fs::read_to_string(&skill_md)?;
    let (metadata, _body) = parse_frontmatter(&content)?;

    let name = metadata
        .get("name")
        .and_then(value_to_string)
        .ok_or_else(|| SkillError::Parse("missing required field: name".to_string()))?;

    let description = metadata
        .get("description")
        .and_then(value_to_string)
        .ok_or_else(|| SkillError::Parse("missing required field: description".to_string()))?;

    let license = metadata.get("license").and_then(value_to_string);
    let compatibility = metadata.get("compatibility").and_then(value_to_string);
    let allowed_tools = metadata.get("allowed-tools").and_then(value_to_string);

    let skill_metadata: HashMap<String, String> = match metadata.get("metadata") {
        Some(Value::Mapping(m)) => m
            .iter()
            .filter_map(|(k, v)| {
                let key = value_to_string(k)?;
                let val = value_to_string(v)?;
                Some((key, val))
            })
            .collect(),
        _ => HashMap::new(),
    };

    let props = SkillProperties::new(
        name,
        description,
        license,
        compatibility,
        allowed_tools,
        skill_metadata,
    )?;

    let location = skill_md
        .canonicalize()
        .unwrap_or(skill_md)
        .to_string_lossy()
        .into_owned();

    Ok(SkillPropertiesWithLocation {
        properties: props,
        location,
    })
}

impl Skills {
    /// Scans `dir` for subdirectories and builds a `Skills` from every
    /// subdirectory that contains a `SKILL.md` (or `skill.md`).
    ///
    /// Subdirectories without a skill file are silently skipped.
    /// Any other error (invalid SKILL.md content, IO failures) is returned immediately.
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let mut skills = Self::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if !path.is_dir() {
                continue;
            }
            if find_skill_md(&path).is_none() {
                continue;
            }
            skills.0.push(read_skills(&path)?);
        }
        Ok(skills)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_frontmatter ---

    #[test]
    fn frontmatter_valid_minimal() {
        let content = "---\nname: my-skill\ndescription: A skill\n---\n";
        let (meta, body) = parse_frontmatter(content).unwrap();
        assert_eq!(meta["name"], Value::String("my-skill".to_string()));
        assert_eq!(body, "");
    }

    #[test]
    fn frontmatter_body_is_trimmed() {
        let content = "---\nname: my-skill\n---\n\n  body text  \n";
        let (_meta, body) = parse_frontmatter(content).unwrap();
        assert_eq!(body, "body text");
    }

    #[test]
    fn frontmatter_missing_opener_is_err() {
        let content = "name: my-skill\n---\n";
        let err = parse_frontmatter(content).unwrap_err();
        assert!(matches!(err, SkillError::Parse(_)));
    }

    #[test]
    fn frontmatter_unclosed_is_err() {
        let content = "---\nname: my-skill\n";
        let err = parse_frontmatter(content).unwrap_err();
        assert!(matches!(err, SkillError::Parse(_)));
    }

    #[test]
    fn frontmatter_invalid_yaml_is_err() {
        let content = "---\n: : :\n---\n";
        assert!(parse_frontmatter(content).is_err());
    }

    #[test]
    fn frontmatter_with_metadata_mapping() {
        let content = "---\nname: s\ndescription: d\nmetadata:\n  key1: val1\n  key2: val2\n---\n";
        let (meta, _) = parse_frontmatter(content).unwrap();
        assert!(matches!(meta["metadata"], Value::Mapping(_)));
    }

    // --- read_skills ---

    fn write_skill_md(dir: &std::path::Path, filename: &str, content: &str) {
        std::fs::write(dir.join(filename), content).unwrap();
    }

    #[test]
    fn read_skills_minimal() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_md(
            dir.path(),
            "SKILL.md",
            "---\nname: my-skill\ndescription: Does something\n---\n",
        );
        let result = read_skills(dir.path()).unwrap();
        assert_eq!(result.properties.name, "my-skill");
        assert_eq!(result.properties.description, "Does something");
        assert!(result.properties.license.is_none());
        assert!(result.properties.metadata.is_empty());
    }

    #[test]
    fn read_skills_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_md(
            dir.path(),
            "SKILL.md",
            "---\nname: full-skill\ndescription: Full\nlicense: MIT\ncompatibility: claude-3\nallowed-tools: Bash\nmetadata:\n  author: alice\n---\n",
        );
        let result = read_skills(dir.path()).unwrap();
        assert_eq!(result.properties.license.as_deref(), Some("MIT"));
        assert_eq!(result.properties.compatibility.as_deref(), Some("claude-3"));
        assert_eq!(result.properties.allowed_tools.as_deref(), Some("Bash"));
        assert_eq!(result.properties.metadata["author"], "alice");
    }

    #[test]
    fn read_skills_falls_back_to_lowercase_skill_md() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_md(
            dir.path(),
            "skill.md",
            "---\nname: lower\ndescription: Lower\n---\n",
        );
        let result = read_skills(dir.path()).unwrap();
        assert_eq!(result.properties.name, "lower");
    }

    #[test]
    fn read_skills_no_file_is_err() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_skills(dir.path()).unwrap_err();
        assert!(matches!(err, SkillError::Parse(_)));
    }

    #[test]
    fn read_skills_missing_name_is_err() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_md(
            dir.path(),
            "SKILL.md",
            "---\ndescription: No name here\n---\n",
        );
        assert!(read_skills(dir.path()).is_err());
    }

    // --- Skills::from_dir ---

    fn make_skill_dir(parent: &std::path::Path, name: &str, skill_name: &str) {
        let dir = parent.join(name);
        std::fs::create_dir(&dir).unwrap();
        write_skill_md(
            &dir,
            "SKILL.md",
            &format!("---\nname: {}\ndescription: Desc\n---\n", skill_name),
        );
    }

    #[test]
    fn from_dir_finds_skills() {
        let dir = tempfile::tempdir().unwrap();
        make_skill_dir(dir.path(), "skill-a", "skill-a");
        make_skill_dir(dir.path(), "skill-b", "skill-b");
        // Plain directory without SKILL.md — must be skipped.
        std::fs::create_dir(dir.path().join("not-a-skill")).unwrap();

        let skills = Skills::from_dir(dir.path()).unwrap();

        assert_eq!(skills.0.len(), 2);
        let names: std::collections::HashSet<&str> = skills
            .0
            .iter()
            .map(|s| s.properties.name.as_str())
            .collect();
        assert!(names.contains("skill-a"));
        assert!(names.contains("skill-b"));
    }

    #[test]
    fn from_dir_skips_files() {
        let dir = tempfile::tempdir().unwrap();
        // A plain file at the top level must not be treated as a skill directory.
        std::fs::write(dir.path().join("README.md"), "hello").unwrap();
        make_skill_dir(dir.path(), "real-skill", "real-skill");

        let skills = Skills::from_dir(dir.path()).unwrap();

        assert_eq!(skills.0.len(), 1);
    }
}
