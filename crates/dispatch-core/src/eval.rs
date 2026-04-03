use crate::{InstructionKind, LoadedParcel};
use serde::{Deserialize, Serialize};
use std::fs;
use thiserror::Error;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum EvalDocument {
    Single(Box<EvalSpec>),
    Cases { cases: Vec<EvalSpec> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvalSpec {
    pub name: String,
    pub input: String,
    #[serde(default)]
    pub entrypoint: Option<String>,
    #[serde(default)]
    pub expects_tool: Option<String>,
    #[serde(
        default,
        alias = "expects_output_contains",
        alias = "expects_message_contains",
        alias = "expects_text_contains"
    )]
    pub expects_text: Option<String>,
    #[serde(default)]
    pub expects_text_exact: Option<String>,
    #[serde(default, alias = "rejects_text_contains")]
    pub expects_text_not_contains: Option<String>,
    #[serde(default)]
    pub expects_tool_count: Option<usize>,
    #[serde(default)]
    pub expects_tools: Vec<String>,
    #[serde(default)]
    pub expects_tool_stdout_contains: Option<String>,
    #[serde(default)]
    pub expects_tool_stderr_contains: Option<String>,
    #[serde(default)]
    pub expects_tool_exit_code: Option<i32>,
    #[serde(default)]
    pub expects_error_contains: Option<String>,
}

#[derive(Debug, Error)]
pub enum EvalError {
    #[error("failed to read eval file `{path}`: {source}")]
    ReadEval {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse eval file `{path}`: {source}")]
    ParseEval {
        path: String,
        #[source]
        source: serde_yaml::Error,
    },
}

pub fn load_parcel_evals(parcel: &LoadedParcel) -> Result<Vec<(String, EvalSpec)>, EvalError> {
    let mut evals = Vec::new();
    for instruction in &parcel.config.instructions {
        if !matches!(instruction.kind, InstructionKind::Eval) {
            continue;
        }
        let path = parcel
            .parcel_dir
            .join("context")
            .join(&instruction.packaged_path);
        let source = fs::read_to_string(&path).map_err(|source| EvalError::ReadEval {
            path: path.display().to_string(),
            source,
        })?;
        let parsed: EvalDocument =
            serde_yaml::from_str(&source).map_err(|source| EvalError::ParseEval {
                path: path.display().to_string(),
                source,
            })?;
        match parsed {
            EvalDocument::Single(spec) => evals.push((instruction.packaged_path.clone(), *spec)),
            EvalDocument::Cases { cases } => {
                for spec in cases {
                    evals.push((instruction.packaged_path.clone(), spec));
                }
            }
        }
    }
    Ok(evals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BuildOptions, build_agentfile, load_parcel};
    use tempfile::tempdir;

    #[test]
    fn load_parcel_evals_supports_single_and_cases_documents() {
        let dir = tempdir().unwrap();
        let context_dir = dir.path().join("image");
        fs::create_dir_all(context_dir.join("evals")).unwrap();
        fs::write(
            context_dir.join("Agentfile"),
            "FROM dispatch/native:latest\nEVAL evals/single.eval\nEVAL evals/multi.eval\nENTRYPOINT chat\n",
        )
        .unwrap();
        fs::write(
            context_dir.join("evals/single.eval"),
            "name: single\ninput: hi\n",
        )
        .unwrap();
        fs::write(
            context_dir.join("evals/multi.eval"),
            "cases:\n  - name: first\n    input: one\n  - name: second\n    input: two\n",
        )
        .unwrap();

        let built = build_agentfile(
            &context_dir.join("Agentfile"),
            &BuildOptions {
                output_root: context_dir.join(".dispatch/parcels"),
            },
        )
        .unwrap();
        let parcel = load_parcel(&built.parcel_dir).unwrap();
        let evals = load_parcel_evals(&parcel).unwrap();

        assert_eq!(evals.len(), 3);
        assert_eq!(evals[0].0, "evals/single.eval");
        assert_eq!(evals[0].1.name, "single");
        assert_eq!(evals[1].0, "evals/multi.eval");
        assert_eq!(evals[1].1.name, "first");
        assert_eq!(evals[2].1.name, "second");
    }
}
