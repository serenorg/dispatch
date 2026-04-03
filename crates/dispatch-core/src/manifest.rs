use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const PARCEL_SCHEMA_URL: &str = "https://dispatch.run/schemas/parcel.v1.json";
pub const DISPATCH_WASM_ABI: &str = "dispatch:courier@0.1.0/courier-guest";
pub const DISPATCH_WASM_WORLD: &str = "courier-guest";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParcelManifest {
    #[serde(rename = "$schema")]
    pub schema: String,
    pub format_version: u32,
    pub digest: String,
    pub source_agentfile: String,
    pub courier: CourierTarget,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub framework: Option<FrameworkProvenance>,
    pub name: Option<String>,
    pub version: Option<String>,
    pub entrypoint: Option<String>,
    pub instructions: Vec<InstructionConfig>,
    pub inline_prompts: Vec<String>,
    pub env: Vec<EnvVar>,
    pub secrets: Vec<SecretSpec>,
    pub visibility: Option<Visibility>,
    pub mounts: Vec<MountConfig>,
    pub tools: Vec<ToolConfig>,
    pub models: ModelPolicy,
    pub limits: Vec<LimitSpec>,
    pub timeouts: Vec<TimeoutSpec>,
    pub network: Vec<NetworkRule>,
    pub labels: BTreeMap<String, String>,
    pub files: Vec<ParcelFileRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FrameworkProvenance {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CourierTarget {
    pub reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component: Option<WasmComponentConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WasmComponentConfig {
    pub source: String,
    pub sha256: String,
    pub abi: String,
    pub world: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstructionKind {
    Identity,
    Soul,
    Skill,
    Agents,
    User,
    Tools,
    Memory,
    Heartbeat,
    Eval,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstructionConfig {
    pub kind: InstructionKind,
    pub source: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretSpec {
    pub name: String,
    pub required: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Open,
    Opaque,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MountKind {
    Session,
    Memory,
    Artifacts,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MountConfig {
    pub kind: MountKind,
    pub driver: String,
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandSpec {
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolInputSchemaRef {
    pub source: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolConfig {
    Local(LocalToolConfig),
    Builtin(BuiltinToolConfig),
    Mcp(McpToolConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalToolConfig {
    pub alias: String,
    pub packaged_path: String,
    pub runner: CommandSpec,
    pub approval: Option<String>,
    pub description: Option<String>,
    pub input_schema: Option<ToolInputSchemaRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuiltinToolConfig {
    pub capability: String,
    pub approval: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpToolConfig {
    pub server: String,
    pub approval: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ModelPolicy {
    pub primary: Option<ModelReference>,
    pub fallbacks: Vec<ModelReference>,
    pub routing: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelReference {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LimitSpec {
    pub scope: String,
    pub value: String,
    pub qualifiers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimeoutSpec {
    pub scope: String,
    pub duration: String,
    pub qualifiers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkRule {
    pub action: String,
    pub target: String,
    pub qualifiers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParcelFileRecord {
    pub source: String,
    pub packaged_as: String,
    pub sha256: String,
    pub size_bytes: u64,
}
