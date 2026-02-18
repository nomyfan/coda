use std::collections::HashMap;

use unicode_normalization::UnicodeNormalization;

use crate::error::{Result, SkillError};

pub const MAX_SKILL_NAME_LENGTH: usize = 64;
pub const MAX_DESCRIPTION_LENGTH: usize = 1024;
pub const MAX_COMPATIBILITY_LENGTH: usize = 500;

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

fn validate_name(name: &str) -> Result<String> {
    let name: String = name.trim().nfkc().collect();

    if name.is_empty() {
        return Err(SkillError::Parse(
            "name must be a non-empty string".to_string(),
        ));
    }

    if name.chars().count() > MAX_SKILL_NAME_LENGTH {
        return Err(SkillError::Parse(format!(
            "name must be at most {} characters",
            MAX_SKILL_NAME_LENGTH
        )));
    }

    if name != name.to_lowercase() {
        return Err(SkillError::Parse(format!(
            "name '{}' must be lowercase",
            name
        )));
    }

    if name.starts_with('-') || name.ends_with('-') {
        return Err(SkillError::Parse(
            "name cannot start or end with a hyphen".to_string(),
        ));
    }

    if name.contains("--") {
        return Err(SkillError::Parse(
            "name cannot contain consecutive hyphens".to_string(),
        ));
    }

    if !name.chars().all(|c| c.is_alphanumeric() || c == '-') {
        return Err(SkillError::Parse(format!(
            "name '{}' contains invalid characters. Only letters, digits, and hyphens are allowed.",
            name
        )));
    }

    Ok(name)
}

fn validate_description(description: &str) -> Result<String> {
    if description.trim().is_empty() {
        return Err(SkillError::Parse(
            "description must be a non-empty string".to_string(),
        ));
    }

    if description.chars().count() > MAX_DESCRIPTION_LENGTH {
        return Err(SkillError::Parse(format!(
            "description must be at most {} characters",
            MAX_DESCRIPTION_LENGTH
        )));
    }

    Ok(description.to_string())
}

#[derive(Debug, Clone)]
pub struct SkillProperties {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub allowed_tools: Option<String>,
    pub metadata: HashMap<String, String>,
}

impl SkillProperties {
    pub fn new(
        name: String,
        description: String,
        license: Option<String>,
        compatibility: Option<String>,
        allowed_tools: Option<String>,
        metadata: HashMap<String, String>,
    ) -> Result<Self> {
        let name = validate_name(&name)?;
        let description = validate_description(&description)?;

        if let Some(ref compat) = compatibility
            && compat.chars().count() > MAX_COMPATIBILITY_LENGTH
        {
            return Err(SkillError::Parse(format!(
                "compatibility must be at most {} characters",
                MAX_COMPATIBILITY_LENGTH
            )));
        }

        Ok(Self {
            name,
            description,
            license,
            compatibility,
            allowed_tools,
            metadata,
        })
    }

    pub fn to_xml(&self, location: &str) -> String {
        let mut xml = String::from("<skill>\n");
        xml.push_str(&format!("  <name>{}</name>\n", html_escape(&self.name)));
        xml.push_str(&format!(
            "  <description>{}</description>\n",
            html_escape(&self.description)
        ));
        xml.push_str(&format!(
            "  <location>{}</location>\n",
            html_escape(location)
        ));
        xml.push_str("</skill>");
        xml
    }
}

#[derive(Debug, Clone)]
pub struct SkillPropertiesWithLocation {
    pub properties: SkillProperties,
    pub location: String,
}

impl SkillPropertiesWithLocation {
    pub fn to_xml(&self) -> String {
        self.properties.to_xml(&self.location)
    }
}

#[derive(Debug, Default)]
pub struct Skills(pub Vec<SkillPropertiesWithLocation>);

impl Skills {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_skill(&mut self, skill: SkillPropertiesWithLocation) {
        self.0.push(skill);
    }

    pub fn to_xml(&self) -> String {
        let mut xml = String::from("<available_skills>\n");
        for skill in &self.0 {
            xml.push_str(&skill.to_xml());
            xml.push('\n');
        }
        xml.push_str("</available_skills>");
        xml
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- validate_name ---

    #[test]
    fn name_valid() {
        assert_eq!(validate_name("my-skill").unwrap(), "my-skill");
    }

    #[test]
    fn name_trims_whitespace() {
        assert_eq!(validate_name("  my-skill  ").unwrap(), "my-skill");
    }

    #[test]
    fn name_nfkc_fullwidth() {
        // fullwidth ASCII letters → ASCII
        assert_eq!(validate_name("ａｂｃ").unwrap(), "abc");
    }

    #[test]
    fn name_empty_is_err() {
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
    }

    #[test]
    fn name_uppercase_is_err() {
        assert!(validate_name("MySkill").is_err());
    }

    #[test]
    fn name_leading_hyphen_is_err() {
        assert!(validate_name("-skill").is_err());
    }

    #[test]
    fn name_trailing_hyphen_is_err() {
        assert!(validate_name("skill-").is_err());
    }

    #[test]
    fn name_consecutive_hyphens_is_err() {
        assert!(validate_name("my--skill").is_err());
    }

    #[test]
    fn name_invalid_chars_is_err() {
        assert!(validate_name("my skill").is_err());
        assert!(validate_name("my_skill").is_err());
    }

    #[test]
    fn name_too_long_is_err() {
        let long = "a".repeat(MAX_SKILL_NAME_LENGTH + 1);
        assert!(validate_name(&long).is_err());
    }

    #[test]
    fn name_exactly_max_length_is_ok() {
        let name = "a".repeat(MAX_SKILL_NAME_LENGTH);
        assert!(validate_name(&name).is_ok());
    }

    // --- validate_description ---

    #[test]
    fn description_valid() {
        assert!(validate_description("A useful skill").is_ok());
    }

    #[test]
    fn description_empty_is_err() {
        assert!(validate_description("").is_err());
        assert!(validate_description("   ").is_err());
    }

    #[test]
    fn description_too_long_is_err() {
        let long = "x".repeat(MAX_DESCRIPTION_LENGTH + 1);
        assert!(validate_description(&long).is_err());
    }

    // --- html_escape ---

    #[test]
    fn html_escape_special_chars() {
        assert_eq!(html_escape("&"), "&amp;");
        assert_eq!(html_escape("<"), "&lt;");
        assert_eq!(html_escape(">"), "&gt;");
        assert_eq!(html_escape("\""), "&quot;");
        assert_eq!(html_escape("'"), "&#x27;");
    }

    #[test]
    fn html_escape_plain_string_unchanged() {
        assert_eq!(html_escape("hello world"), "hello world");
    }

    #[test]
    fn html_escape_combined() {
        assert_eq!(
            html_escape("<a href=\"x\">O'clock & more</a>"),
            "&lt;a href=&quot;x&quot;&gt;O&#x27;clock &amp; more&lt;/a&gt;"
        );
    }

    // --- SkillProperties::to_xml ---

    fn make_props() -> SkillProperties {
        SkillProperties::new(
            "my-skill".to_string(),
            "Does something useful".to_string(),
            None,
            None,
            None,
            HashMap::new(),
        )
        .unwrap()
    }

    #[test]
    fn skill_properties_to_xml_structure() {
        let xml = make_props().to_xml("/path/to/SKILL.md");
        assert!(xml.starts_with("<skill>"));
        assert!(xml.ends_with("</skill>"));
        assert!(xml.contains("<name>my-skill</name>"));
        assert!(xml.contains("<description>Does something useful</description>"));
        assert!(xml.contains("<location>/path/to/SKILL.md</location>"));
    }

    #[test]
    fn skill_properties_to_xml_escapes_location() {
        let xml = make_props().to_xml("/path/<weird>/SKILL.md");
        assert!(xml.contains("<location>/path/&lt;weird&gt;/SKILL.md</location>"));
    }

    // --- Skills::to_xml ---

    #[test]
    fn skills_to_xml_empty() {
        let skills = Skills::new();
        assert_eq!(skills.to_xml(), "<available_skills>\n</available_skills>");
    }

    #[test]
    fn skills_to_xml_multiple() {
        let mut skills = Skills::new();
        skills.add_skill(SkillPropertiesWithLocation {
            properties: make_props(),
            location: "/a/SKILL.md".to_string(),
        });
        skills.add_skill(SkillPropertiesWithLocation {
            properties: make_props(),
            location: "/b/SKILL.md".to_string(),
        });
        let xml = skills.to_xml();
        assert!(xml.starts_with("<available_skills>"));
        assert!(xml.ends_with("</available_skills>"));
        assert_eq!(xml.matches("<skill>").count(), 2);
    }
}
