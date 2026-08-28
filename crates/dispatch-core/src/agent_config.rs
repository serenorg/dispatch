//! Authored agent source: the `[agent]` table of `dispatch.toml`.
//!
//! This is the declarative source of truth for what an agent is. `dispatch
//! parcel build` compiles this table into the canonical parcel manifest and
//! ignores every other table in the file, so deployment wiring never reaches
//! the signed artifact.

use crate::manifest::{
    A2aAuthConfig, A2aEndpointMode, MountKind, ToolApprovalPolicy, ToolRiskLevel, Visibility,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use thiserror::Error;

/// Name of the file that carries the `[agent]` table.
pub const AGENT_CONFIG_FILE: &str = "dispatch.toml";

#[derive(Debug, Error)]
pub enum AgentConfigError {
    #[error("failed to read `{path}`: {source}")]
    ReadFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse `{path}`: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error(
        "`{path}` has no `[agent]` table; add one to build a parcel from this file, or point the build at a file that declares an agent"
    )]
    MissingAgentTable { path: String },
    #[error("`{path}` declares both `parcel` and `[agent]`; use one or the other")]
    ParcelAndAgent { path: String },
}

/// Top level of `dispatch.toml`, from the build's point of view.
///
/// Only `parcel` and `agent` are read here. Deployment tables are owned by the
/// CLI project loader and are deliberately not modeled, so adding a channel
/// binding can never affect the parcel digest.
#[derive(Debug, Deserialize)]
struct ConfigDocument {
    #[serde(default)]
    parcel: Option<toml::Value>,
    #[serde(default)]
    agent: Option<AgentConfig>,
}

/// The authored agent definition.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Courier target reference, for example `dispatch/native:latest`, `native`,
    /// `dispatch/docker:latest`, or `dispatch/wasm:latest`.
    pub courier_reference: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    /// One of `chat`, `job`, or `heartbeat`.
    #[serde(default)]
    pub entrypoint: Option<String>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
    /// WebAssembly component path. Required when `courier_reference` targets wasm.
    #[serde(default)]
    pub component: Option<String>,
    #[serde(default)]
    pub framework: Option<FrameworkConfig>,
    #[serde(default)]
    pub schedules: Vec<String>,
    #[serde(default)]
    pub listeners: Vec<String>,
    #[serde(default)]
    pub ingress: Option<IngressConfig>,
    #[serde(default)]
    pub instructions: InstructionsConfig,
    /// Agent Skills bundle directories, each containing a `SKILL.md`.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Inline prompt text. Prefer `instructions` files for anything sizable.
    #[serde(default)]
    pub prompts: Vec<String>,
    /// Eval documents to package and run against this agent.
    #[serde(default)]
    pub evals: Vec<String>,
    /// Extra files or directories to package into the parcel context.
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub model: Option<ModelConfig>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub secrets: Vec<SecretConfig>,
    #[serde(default)]
    pub mounts: Vec<MountConfigEntry>,
    #[serde(default)]
    pub tools: Vec<ToolConfigEntry>,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
    #[serde(default)]
    pub compaction: Option<CompactionConfigEntry>,
    #[serde(default)]
    pub network: Vec<NetworkRuleConfig>,
    #[serde(default)]
    pub tests: Vec<TestConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameworkConfig {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressConfig {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub methods: Vec<String>,
    /// Name of a declared secret holding the shared ingress secret.
    #[serde(default)]
    pub secret_env: Option<String>,
    #[serde(default)]
    pub max_body_bytes: Option<usize>,
    #[serde(default)]
    pub max_header_bytes: Option<usize>,
}

/// Instruction documents packaged into the parcel, each a relative file path.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct InstructionsConfig {
    #[serde(default)]
    pub identity: Option<String>,
    #[serde(default)]
    pub soul: Option<String>,
    #[serde(default)]
    pub skill: Option<String>,
    #[serde(default)]
    pub agents: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub tools: Option<String>,
    #[serde(default)]
    pub memory: Option<String>,
    #[serde(default)]
    pub heartbeat: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub routing: Option<String>,
    /// Backend-specific options, for example `persist-thread` or
    /// `reasoning-effort`.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    #[serde(default)]
    pub fallbacks: Vec<ModelFallbackConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelFallbackConfig {
    pub id: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretConfig {
    pub name: String,
    #[serde(default = "default_true")]
    pub required: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountConfigEntry {
    pub kind: MountKind,
    pub driver: String,
    #[serde(default)]
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolConfigEntry {
    Builtin(BuiltinToolEntry),
    Local(LocalToolEntry),
    Mcp(McpToolEntry),
    A2a(A2aToolEntry),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuiltinToolEntry {
    /// Builtin capability name, for example `web_search`.
    pub name: String,
    #[serde(default)]
    pub approval: Option<ToolApprovalPolicy>,
    #[serde(default)]
    pub risk: Option<ToolRiskLevel>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalToolEntry {
    /// Script or executable path, relative to the config file.
    pub path: String,
    /// Tool alias. Defaults to the file stem of `path`.
    #[serde(default)]
    pub alias: Option<String>,
    /// Explicit runner. Inferred from the file extension when absent.
    #[serde(default)]
    pub runner: Option<RunnerConfig>,
    #[serde(default)]
    pub approval: Option<ToolApprovalPolicy>,
    #[serde(default)]
    pub risk: Option<ToolRiskLevel>,
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema file describing the tool input.
    #[serde(default)]
    pub schema: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpToolEntry {
    pub server: String,
    #[serde(default)]
    pub approval: Option<ToolApprovalPolicy>,
    #[serde(default)]
    pub risk: Option<ToolRiskLevel>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct A2aToolEntry {
    pub alias: String,
    pub url: String,
    #[serde(default)]
    pub discovery: Option<A2aEndpointMode>,
    #[serde(default)]
    pub auth: Option<A2aAuthEntry>,
    #[serde(default)]
    pub expect_agent_name: Option<String>,
    #[serde(default)]
    pub expect_card_sha256: Option<String>,
    #[serde(default)]
    pub approval: Option<ToolApprovalPolicy>,
    #[serde(default)]
    pub risk: Option<ToolRiskLevel>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub schema: Option<String>,
}

/// A2A credential binding. Values are secret names, never secret values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "scheme", rename_all = "snake_case", deny_unknown_fields)]
pub enum A2aAuthEntry {
    Bearer {
        secret_name: String,
    },
    Header {
        header_name: String,
        secret_name: String,
    },
    Basic {
        username_secret_name: String,
        password_secret_name: String,
    },
}

impl A2aAuthEntry {
    pub fn into_manifest(self) -> A2aAuthConfig {
        match self {
            Self::Bearer { secret_name } => A2aAuthConfig::Bearer { secret_name },
            Self::Header {
                header_name,
                secret_name,
            } => A2aAuthConfig::Header {
                header_name,
                secret_name,
            },
            Self::Basic {
                username_secret_name,
                password_secret_name,
            } => A2aAuthConfig::Basic {
                username_secret_name,
                password_secret_name,
            },
        }
    }
}

/// Run limits. Each key maps to one manifest limit scope.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    #[serde(default)]
    pub iterations: Option<u64>,
    #[serde(default)]
    pub tool_calls: Option<u64>,
    #[serde(default)]
    pub tool_output: Option<u64>,
    #[serde(default)]
    pub context_tokens: Option<u64>,
    #[serde(default)]
    pub tool_rounds: Option<u64>,
}

impl LimitsConfig {
    /// Manifest scope name paired with each declared value, in a stable order.
    pub fn entries(&self) -> Vec<(&'static str, u64)> {
        [
            ("ITERATIONS", self.iterations),
            ("TOOL_CALLS", self.tool_calls),
            ("TOOL_OUTPUT", self.tool_output),
            ("CONTEXT_TOKENS", self.context_tokens),
            ("TOOL_ROUNDS", self.tool_rounds),
        ]
        .into_iter()
        .filter_map(|(scope, value)| value.map(|value| (scope, value)))
        .collect()
    }
}

/// Timeout budgets. Each value is a duration such as `300s`, `500ms`, or `2m`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TimeoutsConfig {
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub llm: Option<String>,
}

impl TimeoutsConfig {
    /// Manifest scope name paired with each declared duration, in a stable order.
    pub fn entries(&self) -> Vec<(&'static str, &str)> {
        [
            ("RUN", self.run.as_deref()),
            ("TOOL", self.tool.as_deref()),
            ("LLM", self.llm.as_deref()),
        ]
        .into_iter()
        .filter_map(|(scope, value)| value.map(|value| (scope, value)))
        .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionConfigEntry {
    pub interval: String,
    #[serde(default)]
    pub overlap: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkRuleConfig {
    /// Rule action, for example `allow` or `deny`.
    pub action: String,
    /// Rule target, for example a host, CIDR, or `*`.
    pub target: String,
    #[serde(default)]
    pub qualifiers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestConfig {
    /// Alias of a declared local or A2A tool.
    pub tool: String,
}

/// Read the `[agent]` table from a `dispatch.toml` file.
///
/// Rejects a document that declares both `parcel` and `[agent]`: a file either
/// defines an agent or references a built one, never both.
pub fn load_agent_config(path: &Path) -> Result<AgentConfig, AgentConfigError> {
    let source = std::fs::read_to_string(path).map_err(|source| AgentConfigError::ReadFile {
        path: path.display().to_string(),
        source,
    })?;
    parse_agent_config(&source, path)
}

/// Parse the `[agent]` table out of `dispatch.toml` source text.
pub fn parse_agent_config(source: &str, path: &Path) -> Result<AgentConfig, AgentConfigError> {
    let document: ConfigDocument =
        toml::from_str(source).map_err(|source| AgentConfigError::Parse {
            path: path.display().to_string(),
            source,
        })?;

    match (document.parcel, document.agent) {
        (Some(_), Some(_)) => Err(AgentConfigError::ParcelAndAgent {
            path: path.display().to_string(),
        }),
        (_, Some(agent)) => Ok(agent),
        (_, None) => Err(AgentConfigError::MissingAgentTable {
            path: path.display().to_string(),
        }),
    }
}

/// Whether a `dispatch.toml` document declares an `[agent]` table.
pub fn declares_agent(source: &str) -> bool {
    toml::from_str::<toml::Table>(source)
        .map(|document| matches!(document.get("agent"), Some(toml::Value::Table(_))))
        .unwrap_or_else(|_| {
            source.lines().any(|line| {
                let header = line
                    .split_once('#')
                    .map_or(line, |(before_comment, _)| before_comment)
                    .trim();
                header == "[agent]"
                    || header.starts_with("[agent.")
                    || header.starts_with("[[agent.")
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Result<AgentConfig, AgentConfigError> {
        parse_agent_config(source, Path::new("dispatch.toml"))
    }

    #[test]
    fn parses_a_minimal_agent() {
        let config = parse("[agent]\ncourier_reference = \"native\"\n").unwrap();
        assert_eq!(config.courier_reference, "native");
        assert!(config.name.is_none());
        assert!(config.tools.is_empty());
    }

    #[test]
    fn rejects_an_unknown_agent_field() {
        let error =
            parse("[agent]\ncourier_reference = \"native\"\nmodle = \"typo\"\n").unwrap_err();
        assert!(matches!(error, AgentConfigError::Parse { .. }));
    }

    #[test]
    fn rejects_an_unknown_nested_field() {
        let error = parse(
            "[agent]\ncourier_reference = \"native\"\n\n[agent.timeouts]\nrun = \"300s\"\nwall = \"1h\"\n",
        )
        .unwrap_err();
        assert!(matches!(error, AgentConfigError::Parse { .. }));
    }

    #[test]
    fn rejects_a_missing_required_field() {
        let error = parse("[agent]\nname = \"demo\"\n").unwrap_err();
        assert!(matches!(error, AgentConfigError::Parse { .. }));
    }

    #[test]
    fn rejects_parcel_and_agent_together() {
        let error =
            parse("parcel = \"./other\"\n\n[agent]\ncourier_reference = \"native\"\n").unwrap_err();
        assert!(matches!(error, AgentConfigError::ParcelAndAgent { .. }));
    }

    #[test]
    fn reports_a_document_with_no_agent_table() {
        let error = parse("parcel = \"./other\"\ncourier = \"native\"\n").unwrap_err();
        assert!(matches!(error, AgentConfigError::MissingAgentTable { .. }));
    }

    #[test]
    fn ignores_deployment_tables() {
        let config = parse(
            r#"
courier = "native"

[agent]
courier_reference = "dispatch/native:latest"

[[channels]]
name = "telegram"
plugin = "channel-telegram"
mode = "listen"
"#,
        )
        .unwrap();
        assert_eq!(config.courier_reference, "dispatch/native:latest");
    }

    #[test]
    fn serialized_agent_config_excludes_deployment_tables() {
        let config = parse(
            r#"
courier = "native"

[agent]
courier_reference = "dispatch/native:latest"
entrypoint = "chat"

[[channels]]
plugin = "channel-telegram"
mode = "listen"
config = { bot_token = "must-not-leak" }
"#,
        )
        .unwrap();
        let encoded = serde_json::to_value(&config).unwrap();
        let round_trip: AgentConfig = serde_json::from_value(encoded.clone()).unwrap();

        assert_eq!(
            encoded["courier_reference"],
            serde_json::Value::String("dispatch/native:latest".to_string())
        );
        assert!(encoded.get("channels").is_none());
        assert!(!encoded.to_string().contains("must-not-leak"));
        assert_eq!(round_trip.entrypoint.as_deref(), Some("chat"));
    }

    #[test]
    fn parses_each_tool_kind() {
        let config = parse(
            r#"
[agent]
courier_reference = "native"

[[agent.tools]]
kind = "builtin"
name = "web_search"

[[agent.tools]]
kind = "local"
path = "tools/lint.py"
alias = "lint"

[[agent.tools]]
kind = "mcp"
server = "filesystem"

[[agent.tools]]
kind = "a2a"
alias = "planner"
url = "https://planner.example.com"

[agent.tools.auth]
scheme = "bearer"
secret_name = "PLANNER_TOKEN"
"#,
        )
        .unwrap();
        assert_eq!(config.tools.len(), 4);
        match &config.tools[3] {
            ToolConfigEntry::A2a(tool) => {
                assert_eq!(tool.alias, "planner");
                assert!(matches!(tool.auth, Some(A2aAuthEntry::Bearer { .. })));
            }
            other => panic!("expected an A2A tool, got {other:?}"),
        }
    }

    #[test]
    fn limits_and_timeouts_lower_to_manifest_scopes() {
        let config = parse(
            r#"
[agent]
courier_reference = "native"

[agent.limits]
iterations = 20
tool_calls = 12

[agent.timeouts]
run = "300s"
llm = "120s"
"#,
        )
        .unwrap();
        assert_eq!(
            config.limits.entries(),
            vec![("ITERATIONS", 20), ("TOOL_CALLS", 12)]
        );
        assert_eq!(
            config.timeouts.entries(),
            vec![("RUN", "300s"), ("LLM", "120s")]
        );
    }

    #[test]
    fn secrets_default_to_required() {
        let config = parse(
            "[agent]\ncourier_reference = \"native\"\n\n[[agent.secrets]]\nname = \"TOKEN\"\n",
        )
        .unwrap();
        assert!(config.secrets[0].required);
    }

    #[test]
    fn detects_agent_tables_that_fail_strict_deserialization() {
        assert!(declares_agent(
            "[agent]\ncourier_reference = \"native\"\nunknown = true\n"
        ));
        assert!(declares_agent("[agent]\ncourier_reference = \"native\n"));
        assert!(declares_agent("[agent.model]\nid = \"unterminated\n"));
        assert!(declares_agent("[[agent.tools]]\nkind = \"builtin\n"));
        assert!(!declares_agent("parcel = \"./other\"\n"));
    }
}
