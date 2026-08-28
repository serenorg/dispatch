use crate::agent_config::{AgentConfig, AgentConfigError, ToolConfigEntry, load_agent_config};
use crate::skill::{
    DispatchSkillManifest, allowed_tool_warnings, dispatch_skill_manifest_path,
    parse_skill_markdown, validate_agent_skill_frontmatter,
};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: Level,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Error,
    Warning,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ValidationReport {
    pub diagnostics: Vec<Diagnostic>,
}

impl ValidationReport {
    pub fn is_ok(&self) -> bool {
        self.diagnostics
            .iter()
            .all(|diagnostic| diagnostic.level != Level::Error)
    }
}

/// Load and check the `[agent]` table of a `dispatch.toml` file.
///
/// Shape, required fields, and unknown keys are rejected by the loader, so a
/// returned report carries only the cross-reference checks serde cannot make.
pub fn validate_agent_config_at_path(
    config_path: &Path,
) -> Result<(AgentConfig, ValidationReport), AgentConfigError> {
    let config = load_agent_config(config_path)?;
    let report = validate_agent_config(&config, config_path);
    Ok((config, report))
}

/// Cross-reference checks over an already-loaded agent config.
pub fn validate_agent_config(config: &AgentConfig, config_path: &Path) -> ValidationReport {
    let mut diagnostics = Vec::new();

    if config.entrypoint.is_none() {
        diagnostics.push(Diagnostic {
            level: Level::Warning,
            message: "no `agent.entrypoint` declared".to_string(),
        });
    }

    let Some(context_dir) = config_path.parent() else {
        return ValidationReport { diagnostics };
    };

    let mut parcel_tool_names = collect_declared_tool_names(config);
    let mut skill_specs = Vec::new();

    for skill_path in &config.skills {
        let skill_dir = context_dir.join(skill_path);
        let Ok(metadata) = fs::metadata(&skill_dir) else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        let Ok(skill_dir) = skill_dir.canonicalize() else {
            continue;
        };
        let skill_md_path = skill_dir.join("SKILL.md");
        let Ok(skill_source) = fs::read_to_string(&skill_md_path) else {
            continue;
        };
        let Ok(parsed_skill) = parse_skill_markdown(&skill_source) else {
            continue;
        };
        if validate_agent_skill_frontmatter(&skill_dir, &parsed_skill.frontmatter).is_err() {
            continue;
        }

        let own_tool_aliases =
            resolve_skill_tool_aliases_for_validation(&skill_dir, &parsed_skill.frontmatter);
        parcel_tool_names.extend(own_tool_aliases.iter().cloned());
        skill_specs.push(SkillValidationSpec {
            skill_name: parsed_skill.frontmatter.name,
            allowed_tools: parsed_skill.frontmatter.allowed_tools,
            own_tool_aliases,
        });
    }

    for skill in skill_specs {
        for message in allowed_tool_warnings(
            &skill.skill_name,
            skill.allowed_tools.as_deref(),
            &skill.own_tool_aliases,
            &parcel_tool_names,
        ) {
            diagnostics.push(Diagnostic {
                level: Level::Warning,
                message,
            });
        }
    }

    ValidationReport { diagnostics }
}

#[derive(Debug)]
struct SkillValidationSpec {
    skill_name: String,
    allowed_tools: Option<Vec<String>>,
    own_tool_aliases: Vec<String>,
}

fn collect_declared_tool_names(config: &AgentConfig) -> BTreeSet<String> {
    config
        .tools
        .iter()
        .map(|tool| match tool {
            ToolConfigEntry::Builtin(tool) => tool.name.clone(),
            ToolConfigEntry::A2a(tool) => tool.alias.clone(),
            ToolConfigEntry::Mcp(tool) => tool.server.clone(),
            ToolConfigEntry::Local(tool) => tool.alias.clone().unwrap_or_else(|| {
                Path::new(&tool.path)
                    .file_stem()
                    .map(|value| value.to_string_lossy().to_string())
                    .unwrap_or_else(|| tool.path.clone())
            }),
        })
        .collect()
}

fn resolve_skill_tool_aliases_for_validation(
    skill_dir: &Path,
    frontmatter: &crate::skill::AgentSkillFrontmatter,
) -> Vec<String> {
    let Some(sidecar_path) =
        resolve_skill_dispatch_manifest_path_for_validation(skill_dir, frontmatter)
    else {
        return Vec::new();
    };
    let Ok(source) = fs::read_to_string(sidecar_path) else {
        return Vec::new();
    };
    let Ok(manifest) = toml::from_str::<DispatchSkillManifest>(&source) else {
        return Vec::new();
    };
    manifest.tools.into_iter().map(|tool| tool.name).collect()
}

fn resolve_skill_dispatch_manifest_path_for_validation(
    skill_dir: &Path,
    frontmatter: &crate::skill::AgentSkillFrontmatter,
) -> Option<PathBuf> {
    if let Some(path) = dispatch_skill_manifest_path(frontmatter) {
        return resolve_skill_member_path_for_validation(skill_dir, path);
    }
    let default = skill_dir.join("skill.toml");
    if default.is_file() {
        return resolve_skill_member_path_for_validation(skill_dir, "skill.toml");
    }
    None
}

fn resolve_skill_member_path_for_validation(skill_dir: &Path, relative: &str) -> Option<PathBuf> {
    let joined = skill_dir.join(relative);
    if !joined.exists() {
        return None;
    }
    let resolved = joined.canonicalize().ok()?;
    resolved.starts_with(skill_dir).then_some(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn validate_agent_config_at_path_reports_skill_allowed_tool_mismatches() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: file-analyst\ndescription: Analyze files.\nallowed-tools:\n  - Bash\n---\nUse the bundled tools.\n",
        )
        .unwrap();
        fs::write(
            skill_dir.join("skill.toml"),
            "[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("scripts/read_file.sh"), "printf ok\n").unwrap();
        let config_path = dir.path().join("dispatch.toml");
        fs::write(
            &config_path,
            "[agent]\ncourier_reference = \"native\"\nentrypoint = \"chat\"\nskills = [\"file-analyst\"]\n",
        )
        .unwrap();

        let (_, report) = validate_agent_config_at_path(&config_path).unwrap();
        let warnings = report
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.level == Level::Warning)
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();

        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().any(|message| message.contains("`Bash`")));
        assert!(
            warnings
                .iter()
                .any(|message| message.contains("`read_file`"))
        );
    }
}
