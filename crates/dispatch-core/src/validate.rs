use crate::{
    ast::{Instruction, ParsedAgentfile, Value},
    skill::{
        DispatchSkillManifest, allowed_tool_warnings, dispatch_skill_manifest_path,
        parse_skill_markdown, validate_agent_skill_frontmatter,
    },
};
use serde::Serialize;
use std::{
    collections::{BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: Level,
    pub message: String,
    pub line: usize,
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

pub fn validate_agentfile(agentfile: &ParsedAgentfile) -> ValidationReport {
    validate_agentfile_base(agentfile)
}

pub fn validate_agentfile_at_path(
    agentfile_path: &Path,
    agentfile: &ParsedAgentfile,
) -> ValidationReport {
    let mut report = validate_agentfile_base(agentfile);
    let Some(context_dir) = agentfile_path.parent() else {
        return report;
    };

    let mut parcel_tool_names = collect_declared_tool_names(agentfile);
    let mut skill_specs = Vec::new();

    for instruction in &agentfile.instructions {
        if instruction.keyword != "SKILL" {
            continue;
        }
        let Some(skill_path) = first_scalar(instruction.args.first()) else {
            continue;
        };
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
            line: instruction.span.line_start,
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
            report.diagnostics.push(Diagnostic {
                level: Level::Warning,
                message,
                line: skill.line,
            });
        }
    }

    report
}

fn validate_agentfile_base(agentfile: &ParsedAgentfile) -> ValidationReport {
    let mut diagnostics = Vec::new();
    let mut seen = HashSet::new();

    let allowed = allowed_instructions();

    for instruction in &agentfile.instructions {
        if !allowed.contains(instruction.keyword.as_str()) {
            diagnostics.push(Diagnostic {
                level: Level::Error,
                message: format!("unknown instruction `{}`", instruction.keyword),
                line: instruction.span.line_start,
            });
            continue;
        }

        match instruction.keyword.as_str() {
            "FROM" => require_min_args(instruction, 1, &mut diagnostics),
            "NAME" | "VERSION" | "ENTRYPOINT" | "VISIBILITY" => {
                require_exact_args(instruction, 1, &mut diagnostics)
            }
            "MODEL" | "FALLBACK" => require_min_args(instruction, 1, &mut diagnostics),
            "FRAMEWORK" => require_min_args(instruction, 1, &mut diagnostics),
            "COMPONENT" => require_min_args(instruction, 1, &mut diagnostics),
            "IDENTITY" | "SOUL" | "SKILL" | "AGENTS" | "USER" | "TOOLS" | "EVAL" => {
                require_exact_args(instruction, 1, &mut diagnostics)
            }
            "TEST" => require_exact_args(instruction, 1, &mut diagnostics),
            "MEMORY" => require_min_args(instruction, 2, &mut diagnostics),
            "HEARTBEAT" | "TOOL" | "MOUNT" | "TIMEOUT" | "LIMIT" | "COMPACTION" | "ENV"
            | "SECRET" | "NETWORK" | "LABEL" | "COPY" | "ADD" | "ROUTING" | "PROMPT" => {
                require_min_args(instruction, 1, &mut diagnostics)
            }
            _ => {}
        }

        seen.insert(instruction.keyword.as_str());
    }

    if !seen.contains("FROM") {
        diagnostics.push(Diagnostic {
            level: Level::Error,
            message: "missing required `FROM` instruction".to_string(),
            line: 1,
        });
    }

    if !seen.contains("ENTRYPOINT") {
        diagnostics.push(Diagnostic {
            level: Level::Warning,
            message: "no `ENTRYPOINT` declared".to_string(),
            line: 1,
        });
    }

    ValidationReport { diagnostics }
}

#[derive(Debug)]
struct SkillValidationSpec {
    line: usize,
    skill_name: String,
    allowed_tools: Option<Vec<String>>,
    own_tool_aliases: Vec<String>,
}

fn allowed_instructions() -> HashSet<&'static str> {
    [
        "FROM",
        "NAME",
        "VERSION",
        "FRAMEWORK",
        "COMPONENT",
        "LABEL",
        "IDENTITY",
        "SOUL",
        "SKILL",
        "AGENTS",
        "USER",
        "TOOLS",
        "HEARTBEAT",
        "MEMORY",
        "PROMPT",
        "MODEL",
        "FALLBACK",
        "ROUTING",
        "TOOL",
        "COPY",
        "ADD",
        "ENV",
        "SECRET",
        "NETWORK",
        "VISIBILITY",
        "TIMEOUT",
        "LIMIT",
        "COMPACTION",
        "MOUNT",
        "EVAL",
        "TEST",
        "ENTRYPOINT",
    ]
    .into_iter()
    .collect()
}

fn collect_declared_tool_names(agentfile: &ParsedAgentfile) -> BTreeSet<String> {
    agentfile
        .instructions
        .iter()
        .filter(|instruction| instruction.keyword == "TOOL")
        .filter_map(tool_name_from_instruction)
        .collect()
}

fn tool_name_from_instruction(instruction: &Instruction) -> Option<String> {
    let tokens = scalars(&instruction.args);
    match tokens.first().map(String::as_str) {
        Some("LOCAL") => {
            let packaged_path = tokens.get(1)?;
            Some(parse_named_value(&tokens, "AS").unwrap_or_else(|| {
                Path::new(packaged_path)
                    .file_stem()
                    .map(|value| value.to_string_lossy().to_string())
                    .unwrap_or_else(|| packaged_path.clone())
            }))
        }
        Some("A2A") | Some("BUILTIN") => tokens.get(1).cloned(),
        _ => None,
    }
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
    let default = skill_dir.join("dispatch.toml");
    if default.is_file() {
        return resolve_skill_member_path_for_validation(skill_dir, "dispatch.toml");
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

fn parse_named_value(tokens: &[String], marker: &str) -> Option<String> {
    tokens
        .windows(2)
        .find(|window| window[0] == marker)
        .map(|window| window[1].clone())
}

fn first_scalar(value: Option<&Value>) -> Option<String> {
    value.map(scalar_value)
}

fn scalar_value(value: &Value) -> String {
    match value {
        Value::Token(value) | Value::String(value) => value.clone(),
        Value::Heredoc(doc) => doc.body.clone(),
    }
}

fn scalars(args: &[Value]) -> Vec<String> {
    args.iter().map(scalar_value).collect()
}

fn require_exact_args(
    instruction: &Instruction,
    expected: usize,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if instruction.args.len() != expected {
        diagnostics.push(Diagnostic {
            level: Level::Error,
            message: format!(
                "`{}` expects exactly {} argument(s), got {}",
                instruction.keyword,
                expected,
                instruction.args.len()
            ),
            line: instruction.span.line_start,
        });
    }
}

fn require_min_args(instruction: &Instruction, minimum: usize, diagnostics: &mut Vec<Diagnostic>) {
    if instruction.args.len() < minimum {
        diagnostics.push(Diagnostic {
            level: Level::Error,
            message: format!(
                "`{}` expects at least {} argument(s), got {}",
                instruction.keyword,
                minimum,
                instruction.args.len()
            ),
            line: instruction.span.line_start,
        });
    }

    if instruction.keyword == "PROMPT"
        && instruction
            .args
            .iter()
            .any(|value| matches!(value, Value::Token(token) if token.starts_with("<<")))
    {
        diagnostics.push(Diagnostic {
            level: Level::Error,
            message: "invalid heredoc usage".to_string(),
            line: instruction.span.line_start,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_agentfile;
    use tempfile::tempdir;

    #[test]
    fn validate_agentfile_at_path_warns_on_skill_allowed_tools_mismatches() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: file-analyst\ndescription: Analyze files.\nallowed-tools:\n  - Bash\n---\nUse the bundled tools.\n",
        )
        .unwrap();
        fs::write(
            skill_dir.join("dispatch.toml"),
            "[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("scripts/read_file.sh"), "cat \"$1\"\n").unwrap();
        let agentfile_path = dir.path().join("Agentfile");
        fs::write(
            &agentfile_path,
            "FROM dispatch/native:latest\nSKILL file-analyst\nENTRYPOINT chat\n",
        )
        .unwrap();

        let parsed = parse_agentfile(&fs::read_to_string(&agentfile_path).unwrap()).unwrap();
        let report = validate_agentfile_at_path(&agentfile_path, &parsed);

        let warnings = report
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.level == Level::Warning)
            .map(|diagnostic| diagnostic.message.clone())
            .collect::<Vec<_>>();

        assert_eq!(
            warnings,
            vec![
                "skill `file-analyst` declares allowed-tools entry `Bash` but no tool with that name exists in the built parcel"
                    .to_string(),
                "skill `file-analyst` synthesizes tool `read_file` but its allowed-tools list does not include that alias"
                    .to_string(),
            ]
        );
    }
}
