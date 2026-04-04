use crate::manifest::{ToolApprovalPolicy, ToolRiskLevel};
use serde::Deserialize;
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentSkillFrontmatter {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub compatibility: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default, rename = "allowed-tools")]
    pub allowed_tools: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct DispatchSkillManifest {
    #[serde(default)]
    pub entrypoint: Option<String>,
    #[serde(default)]
    pub tools: Vec<DispatchSkillTool>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DispatchSkillTool {
    pub name: String,
    pub script: String,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub risk: Option<ToolRiskLevel>,
    #[serde(default)]
    pub approval: Option<ToolApprovalPolicy>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub runner: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedSkillDocument {
    pub frontmatter: AgentSkillFrontmatter,
    pub body: String,
}

pub(crate) fn parse_skill_markdown(source: &str) -> Result<ParsedSkillDocument, serde_yaml::Error> {
    let (frontmatter, body) = split_skill_frontmatter(source);
    let frontmatter = serde_yaml::from_str::<AgentSkillFrontmatter>(frontmatter)?;
    Ok(ParsedSkillDocument {
        frontmatter,
        body: body.to_string(),
    })
}

pub(crate) fn strip_skill_frontmatter(source: &str) -> &str {
    if let Some((_, body)) = try_split_frontmatter(source) {
        body
    } else {
        source
    }
}

pub(crate) fn dispatch_skill_manifest_path(frontmatter: &AgentSkillFrontmatter) -> Option<&str> {
    frontmatter
        .metadata
        .get("dispatch-manifest")
        .map(String::as_str)
}

pub(crate) fn validate_agent_skill_frontmatter(
    skill_dir: &Path,
    frontmatter: &AgentSkillFrontmatter,
) -> Result<(), String> {
    validate_skill_name_matches_dir(skill_dir, &frontmatter.name)?;
    validate_skill_description(&frontmatter.description)?;
    validate_skill_compatibility(frontmatter.compatibility.as_deref())?;
    Ok(())
}

fn validate_skill_name_matches_dir(skill_dir: &Path, name: &str) -> Result<(), String> {
    let actual = skill_dir
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if name != actual {
        return Err(format!(
            "Agent Skills `name` `{name}` must match skill directory `{actual}`"
        ));
    }
    if name.is_empty() || name.chars().count() > 64 || name.starts_with('-') || name.ends_with('-')
    {
        return Err(format!(
            "Agent Skills `name` `{name}` must be 1-64 characters and must not start or end with a hyphen"
        ));
    }
    if name.contains("--") || !name.chars().all(is_skill_name_char) {
        return Err(format!(
            "Agent Skills `name` `{name}` must use lowercase alphanumeric characters and single hyphens only"
        ));
    }
    Ok(())
}

fn validate_skill_description(description: &str) -> Result<(), String> {
    let trimmed = description.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 1024 {
        return Err(
            "Agent Skills `description` must be non-empty and at most 1024 characters".to_string(),
        );
    }
    Ok(())
}

fn validate_skill_compatibility(compatibility: Option<&str>) -> Result<(), String> {
    if let Some(compatibility) = compatibility {
        let trimmed = compatibility.trim();
        if trimmed.is_empty() || trimmed.chars().count() > 500 {
            return Err(
                "Agent Skills `compatibility` must be non-empty and at most 500 characters"
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn is_skill_name_char(ch: char) -> bool {
    ch == '-' || ch.is_numeric() || ch.is_lowercase()
}

fn split_skill_frontmatter(source: &str) -> (&str, &str) {
    try_split_frontmatter(source).unwrap_or(("", source))
}

fn try_split_frontmatter(source: &str) -> Option<(&str, &str)> {
    let normalized = source
        .strip_prefix("---\n")
        .or_else(|| source.strip_prefix("---\r\n"))?;
    let close_idx = normalized
        .find("\n---\n")
        .or_else(|| normalized.find("\r\n---\r\n"))?;
    let frontmatter = &normalized[..close_idx];
    let body_start = close_idx
        + if normalized[close_idx..].starts_with("\n---\n") {
            5
        } else {
            7
        };
    Some((frontmatter, &normalized[body_start..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skill_markdown_extracts_frontmatter_and_body() {
        let parsed = parse_skill_markdown(
            "---\nname: file-analyst\ndescription: Analyze files\nmetadata:\n  dispatch-manifest: dispatch.toml\n---\nBody\n",
        )
        .unwrap();
        assert_eq!(parsed.frontmatter.name, "file-analyst");
        assert_eq!(
            dispatch_skill_manifest_path(&parsed.frontmatter),
            Some("dispatch.toml")
        );
        assert_eq!(parsed.body.trim(), "Body");
    }

    #[test]
    fn strip_skill_frontmatter_returns_body_only() {
        let body = strip_skill_frontmatter(
            "---\nname: file-analyst\ndescription: Analyze files\n---\nBody\n",
        );
        assert_eq!(body.trim(), "Body");
    }

    #[test]
    fn validate_agent_skill_frontmatter_rejects_long_description() {
        let skill_dir = Path::new("file-analyst");
        let frontmatter = AgentSkillFrontmatter {
            name: "file-analyst".to_string(),
            description: "x".repeat(1025),
            license: None,
            compatibility: None,
            metadata: BTreeMap::new(),
            allowed_tools: None,
        };

        let error = validate_agent_skill_frontmatter(skill_dir, &frontmatter).unwrap_err();
        assert!(error.contains("description"));
    }

    #[test]
    fn validate_agent_skill_frontmatter_rejects_empty_compatibility() {
        let skill_dir = Path::new("file-analyst");
        let frontmatter = AgentSkillFrontmatter {
            name: "file-analyst".to_string(),
            description: "Analyze files".to_string(),
            license: None,
            compatibility: Some("   ".to_string()),
            metadata: BTreeMap::new(),
            allowed_tools: None,
        };

        let error = validate_agent_skill_frontmatter(skill_dir, &frontmatter).unwrap_err();
        assert!(error.contains("compatibility"));
    }
}
