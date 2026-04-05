use crate::{InstructionKind, LoadedParcel, TestSpec};
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
    #[serde(default, rename = "expects_text_contains")]
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
    pub expects_no_tool: bool,
    #[serde(default)]
    pub expects_tool_stdout_contains: Option<ToolTextExpectation>,
    #[serde(default)]
    pub expects_tool_stdout_matches_schema: Option<ToolSchemaExpectation>,
    #[serde(default)]
    pub expects_tool_stderr_contains: Option<ToolTextExpectation>,
    #[serde(default)]
    pub expects_tool_exit_code: Option<ToolExitExpectation>,
    #[serde(default)]
    pub expects_a2a_endpoint: Option<ToolA2aEndpointExpectation>,
    #[serde(default)]
    pub expects_error_contains: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ToolTextExpectation {
    Contains(String),
    Scoped { tool: String, contains: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ToolExitExpectation {
    ExitCode(i32),
    Scoped { tool: String, exit_code: i32 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ToolSchemaExpectation {
    Schema(String),
    Scoped { tool: String, schema: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ToolA2aEndpointExpectation {
    Url(String),
    Scoped { tool: String, url: String },
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
        source: toml::de::Error,
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
            toml::from_str(&source).map_err(|source| EvalError::ParseEval {
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

pub fn load_parcel_tests(parcel: &LoadedParcel) -> Vec<TestSpec> {
    parcel.config.tests.clone()
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
            "name = \"single\"\ninput = \"hi\"\n",
        )
        .unwrap();
        fs::write(
            context_dir.join("evals/multi.eval"),
            "[[cases]]\nname = \"first\"\ninput = \"one\"\n\n[[cases]]\nname = \"second\"\ninput = \"two\"\n",
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

    #[test]
    fn load_parcel_evals_supports_tool_scoped_assertions() {
        let dir = tempdir().unwrap();
        let context_dir = dir.path().join("image");
        fs::create_dir_all(context_dir.join("evals")).unwrap();
        fs::write(
            context_dir.join("Agentfile"),
            "FROM dispatch/native:latest\nEVAL evals/scoped.eval\nENTRYPOINT chat\n",
        )
        .unwrap();
        fs::write(
            context_dir.join("evals/scoped.eval"),
            "name = \"scoped\"\ninput = \"hi\"\nexpects_tool_stdout_contains = { tool = \"search\", contains = \"result\" }\nexpects_tool_exit_code = { tool = \"search\", exit_code = 0 }\n",
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

        assert_eq!(evals.len(), 1);
        assert_eq!(
            evals[0].1.expects_tool_stdout_contains,
            Some(ToolTextExpectation::Scoped {
                tool: "search".to_string(),
                contains: "result".to_string(),
            })
        );
        assert_eq!(
            evals[0].1.expects_tool_exit_code,
            Some(ToolExitExpectation::Scoped {
                tool: "search".to_string(),
                exit_code: 0,
            })
        );
    }

    #[test]
    fn load_parcel_evals_supports_expects_no_tool() {
        let dir = tempdir().unwrap();
        let context_dir = dir.path().join("image");
        fs::create_dir_all(context_dir.join("evals")).unwrap();
        fs::write(
            context_dir.join("Agentfile"),
            "FROM dispatch/native:latest\nEVAL evals/no-tool.eval\nENTRYPOINT chat\n",
        )
        .unwrap();
        fs::write(
            context_dir.join("evals/no-tool.eval"),
            "name = \"no-tool\"\ninput = \"hi\"\nexpects_no_tool = true\n",
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

        assert_eq!(evals.len(), 1);
        assert!(evals[0].1.expects_no_tool);
    }

    #[test]
    fn load_parcel_evals_supports_schema_and_a2a_expectations() {
        let dir = tempdir().unwrap();
        let context_dir = dir.path().join("image");
        fs::create_dir_all(context_dir.join("evals")).unwrap();
        fs::write(
            context_dir.join("Agentfile"),
            "FROM dispatch/native:latest\nEVAL evals/expectations.eval\nENTRYPOINT chat\n",
        )
        .unwrap();
        fs::write(
            context_dir.join("evals/expectations.eval"),
            concat!(
                "name = \"expectations\"\n",
                "input = \"hi\"\n",
                "expects_tool_stdout_matches_schema = { tool = \"broker\", schema = \"schemas/output.json\" }\n",
                "expects_a2a_endpoint = { tool = \"broker\", url = \"https://broker.example.com\" }\n",
            ),
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

        assert_eq!(
            evals[0].1.expects_tool_stdout_matches_schema,
            Some(ToolSchemaExpectation::Scoped {
                tool: "broker".to_string(),
                schema: "schemas/output.json".to_string(),
            })
        );
        assert_eq!(
            evals[0].1.expects_a2a_endpoint,
            Some(ToolA2aEndpointExpectation::Scoped {
                tool: "broker".to_string(),
                url: "https://broker.example.com".to_string(),
            })
        );
    }

    #[test]
    fn load_parcel_tests_returns_packaged_tool_tests() {
        let dir = tempdir().unwrap();
        let context_dir = dir.path().join("image");
        fs::create_dir_all(context_dir.join("scripts")).unwrap();
        fs::write(
            context_dir.join("Agentfile"),
            "FROM dispatch/native:latest\nTOOL LOCAL scripts/demo.sh AS demo\nTEST tool:demo\nENTRYPOINT chat\n",
        )
        .unwrap();
        fs::write(context_dir.join("scripts/demo.sh"), "#!/bin/sh\necho ok\n").unwrap();

        let built = build_agentfile(
            &context_dir.join("Agentfile"),
            &BuildOptions {
                output_root: context_dir.join(".dispatch/parcels"),
            },
        )
        .unwrap();
        let parcel = load_parcel(&built.parcel_dir).unwrap();
        let tests = load_parcel_tests(&parcel);

        assert_eq!(
            tests,
            vec![TestSpec::Tool {
                tool: "demo".to_string(),
            }]
        );
    }
}
