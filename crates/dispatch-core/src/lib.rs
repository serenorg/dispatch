pub mod ast;
pub mod build;
pub mod courier;
pub mod depot;
pub mod eval;
pub mod manifest;
pub mod parse;
pub mod plugin_protocol;
pub mod plugins;
pub mod signing;
mod skill;
pub mod trust;
pub mod validate;

pub use ast::{Instruction, ParsedAgentfile, Value};
pub use build::{
    BuildError, BuildOptions, BuiltParcel, ParcelLock, VerificationReport, build_agentfile,
    verify_parcel,
};
// Keep the crate root focused on the primary parcel/courier entrypoints.
// Lower-level courier modeling types remain available under `dispatch_core::courier`.
pub use courier::{
    A2aOperatorPolicyOverrides, ConversationMessage, CourierBackend, CourierCapabilities,
    CourierError, CourierEvent, CourierInspection, CourierKind, CourierOperation, CourierRequest,
    CourierResponse, CourierSession, DockerCourier, JsonlCourierPlugin, LoadedParcel,
    LocalToolSpec, LocalToolTarget, NativeCourier, StubCourier, ToolApprovalDecision,
    ToolApprovalRequest, ToolInvocation, ToolRunResult, WasmCourier, collect_skill_allowed_tools,
    list_local_tools, list_native_builtin_tools, load_parcel, resolve_prompt_text, run_local_tool,
    with_a2a_operator_policy_overrides, with_tool_approval_handler,
};
pub use depot::{
    DepotError, DepotLocator, DepotReference, DepotTagRecord, PulledParcel, PushedParcel,
    parse_depot_reference, pull_parcel, pull_parcel_verified, push_parcel,
};
pub use eval::{
    EvalDocument, EvalError, EvalSpec, ToolA2aEndpointExpectation, ToolExitExpectation,
    ToolSchemaExpectation, ToolTextExpectation, load_parcel_evals, load_parcel_tests,
};
pub use manifest::{
    A2aAuthConfig, A2aAuthScheme, A2aEndpointMode, A2aToolConfig, BuiltinToolConfig, CommandSpec,
    CompactionConfig, CourierTarget, DISPATCH_WASM_ABI, EnvVar, IngressPolicyConfig,
    InstructionConfig, InstructionKind, LimitSpec, LocalToolConfig, McpToolConfig, ModelPolicy,
    ModelReference, MountConfig, MountKind, NetworkRule, PARCEL_FORMAT_VERSION, PARCEL_SCHEMA_URL,
    ParcelFileRecord, ParcelManifest, SecretSpec, TestSpec, TimeoutSpec, ToolApprovalPolicy,
    ToolConfig, ToolRiskLevel, Visibility, WasmComponentConfig,
};
pub use parse::{ParseError, parse_agentfile};
pub use plugin_protocol::{
    COURIER_PLUGIN_PROTOCOL_VERSION, PluginErrorPayload, PluginRequest, PluginRequestEnvelope,
    PluginResponse,
};
pub use plugins::{
    BuiltinCourier, CourierCatalogEntry, CourierPluginExec, CourierPluginManifest,
    CourierPluginRegistry, PluginRegistryError, PluginTransport, ResolvedCourier,
    default_courier_registry_path, install_courier_plugin, list_courier_catalog,
    load_courier_registry, resolve_courier,
};
pub use signing::{
    DISPATCH_SIGNATURE_ALGORITHM, GeneratedKeyPair, PARCEL_SIGNATURES_DIR, ParcelSignature,
    PublicKeyFile, SecretKeyFile, SignatureVerification, SigningError, generate_keypair_files,
    sign_parcel, verify_parcel_signature,
};
pub use trust::{
    A2aTrustPolicy, A2aTrustRequirement, A2aTrustRule, PullTrustPolicy, PullTrustRequirement,
    PullTrustRule, TrustPolicyError,
};
pub use validate::{
    Diagnostic, Level, ValidationReport, validate_agentfile, validate_agentfile_at_path,
};
