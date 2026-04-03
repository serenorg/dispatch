use crate::ast::{Instruction, ParsedAgentfile, Value};
use serde::Serialize;
use std::collections::HashSet;

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
            "NAME" | "VERSION" | "MODEL" | "ENTRYPOINT" | "VISIBILITY" => {
                require_exact_args(instruction, 1, &mut diagnostics)
            }
            "FRAMEWORK" => require_min_args(instruction, 1, &mut diagnostics),
            "COMPONENT" => require_min_args(instruction, 1, &mut diagnostics),
            "IDENTITY" | "SOUL" | "SKILL" | "AGENTS" | "USER" | "TOOLS" | "EVAL" => {
                require_exact_args(instruction, 1, &mut diagnostics)
            }
            "MEMORY" => require_min_args(instruction, 2, &mut diagnostics),
            "HEARTBEAT" | "TOOL" | "MOUNT" | "TIMEOUT" | "LIMIT" | "ENV" | "SECRET" | "NETWORK"
            | "LABEL" | "COPY" | "ADD" | "FALLBACK" | "ROUTING" | "PROMPT" => {
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
        "MOUNT",
        "EVAL",
        "TEST",
        "ENTRYPOINT",
    ]
    .into_iter()
    .collect()
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
