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

pub(crate) fn validate_skill_name_matches_dir(skill_dir: &Path, name: &str) -> Result<(), String> {
    let actual = skill_dir
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if name != actual {
        return Err(format!(
            "Agent Skills `name` `{name}` must match skill directory `{actual}`"
        ));
    }
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
        || !name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        return Err(format!(
            "Agent Skills `name` `{name}` must use lowercase letters, digits, and single hyphens only"
        ));
    }
    Ok(())
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
}
