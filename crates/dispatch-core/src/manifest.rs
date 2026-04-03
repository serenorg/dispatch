use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const PARCEL_SCHEMA_URL: &str = "https://schema.dispatch.run/parcel.v1.json";
pub const PARCEL_FORMAT_VERSION: u32 = 1;
pub const DISPATCH_WASM_ABI: &str = dispatch_wasm_abi::ABI;

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
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CourierTarget {
    Native {
        reference: String,
    },
    Docker {
        reference: String,
    },
    Wasm {
        reference: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        component: Option<WasmComponentConfig>,
    },
    Custom {
        reference: String,
    },
}

impl CourierTarget {
    pub fn from_reference(reference: String) -> Self {
        if reference == "native"
            || reference == "dispatch/native"
            || reference.starts_with("dispatch/native:")
            || reference.starts_with("dispatch/native@")
        {
            return Self::Native { reference };
        }
        if reference == "docker"
            || reference == "dispatch/docker"
            || reference.starts_with("dispatch/docker:")
            || reference.starts_with("dispatch/docker@")
        {
            return Self::Docker { reference };
        }
        if reference == "wasm"
            || reference == "dispatch/wasm"
            || reference.starts_with("dispatch/wasm:")
            || reference.starts_with("dispatch/wasm@")
        {
            return Self::Wasm {
                reference,
                component: None,
            };
        }
        Self::Custom { reference }
    }

    pub fn reference(&self) -> &str {
        match self {
            Self::Native { reference }
            | Self::Docker { reference }
            | Self::Wasm { reference, .. }
            | Self::Custom { reference } => reference,
        }
    }

    pub fn component(&self) -> Option<&WasmComponentConfig> {
        match self {
            Self::Wasm { component, .. } => component.as_ref(),
            _ => None,
        }
    }

    pub fn set_component(&mut self, component: WasmComponentConfig) {
        match self {
            Self::Wasm {
                component: slot, ..
            } => *slot = Some(component),
            _ => unreachable!("component only applies to wasm courier targets"),
        }
    }

    pub fn is_wasm(&self) -> bool {
        matches!(self, Self::Wasm { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WasmComponentConfig {
    pub packaged_path: String,
    pub sha256: String,
    pub abi: String,
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
    pub packaged_path: String,
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
    pub packaged_path: String,
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
