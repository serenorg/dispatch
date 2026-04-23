pub mod ast;
pub mod build;
pub mod catalog;
pub mod channel_plugin_protocol;
pub mod channel_plugins;
pub mod courier;
pub mod database_plugins;
pub mod depot;
pub mod eval;
pub mod manifest;
pub mod parse;
pub mod plugin_protocol;
pub mod plugins;
pub mod provider_plugins;
pub mod secrets;
pub mod signing;
mod skill;
pub mod trace;
pub mod trust;
pub mod validate;

pub use ast::{Instruction, ParsedAgentfile, Value};
pub use build::{
    BuildError, BuildOptions, BuiltParcel, ParcelLock, VerificationReport, build_agentfile,
    verify_parcel,
};
pub use catalog::{
    CATALOG_CACHE_DIR, CATALOG_CONFIG_FILE, CATALOG_FETCH_TIMEOUT, CATALOG_MAX_BYTES,
    CATALOG_SCHEMA_V1, CatalogConfig, CatalogEntry, CatalogError, CatalogExtensionKind,
    CatalogInstallSource, CatalogSearchHit, CatalogSource, ExtensionCatalog, GithubReleaseBinary,
    cache_path, default_catalog_cache_dir, default_catalog_config_path, fetch_catalog_body,
    find_cached_entry, load_cached_catalog, refresh_catalog, search_cached, write_cache,
};
pub use channel_plugin_protocol::{
    AttachmentSource, CHANNEL_PLUGIN_PROTOCOL_VERSION, ChannelCapabilities,
    ChannelEventNotification, ChannelPluginRequest, ChannelPluginRequestEnvelope,
    ChannelPluginResponse, ChannelPolicy, ConfiguredChannel, DeliveryReceipt, HealthReport,
    InboundActor, InboundAttachment, InboundConversationRef, InboundEventEnvelope, InboundMessage,
    IngressCallbackReply, IngressMode, IngressPayload, IngressState, OutboundAttachment,
    OutboundMessageEnvelope, PluginMessage as ChannelPluginMessage, PluginNotificationEnvelope,
    PluginRequestId, RuntimeStateSnapshot, StatusAcceptance, StatusFrame, StatusKind,
    ThreadingModel, notification_to_jsonrpc as channel_notification_to_jsonrpc,
    parse_jsonrpc_message as parse_channel_jsonrpc_message,
    parse_jsonrpc_request as parse_channel_jsonrpc_request,
    parse_jsonrpc_response as parse_channel_jsonrpc_response,
    request_to_jsonrpc as channel_request_to_jsonrpc,
    response_to_jsonrpc as channel_response_to_jsonrpc,
};
pub use channel_plugins::{
    ChannelCatalogEntry, ChannelIngressEndpoint, ChannelIngressTrust, ChannelIngressTrustFailure,
    ChannelPluginCallError, ChannelPluginExec, ChannelPluginIngress, ChannelPluginManifest,
    ChannelPluginRegistry, PersistentChannelPluginProcess, build_channel_reply_envelope,
    build_channel_reply_message, call_channel_plugin, call_channel_plugin_with_timeout,
    call_persistent_channel_plugin, channel_event_session_file, default_channel_registry_path,
    drain_pending_channel_notifications, extract_assistant_channel_reply, extract_assistant_reply,
    install_channel_plugin, list_channel_catalog, load_channel_registry,
    match_channel_ingress_endpoint, recv_persistent_channel_notification,
    render_inbound_event_chat_input, resolve_channel_plugin, resolve_channel_plugin_for_ingress,
    shutdown_persistent_channel_plugin, spawn_persistent_channel_plugin,
    validate_channel_plugin_manifest, verify_host_managed_ingress_trust,
};
pub use database_plugins::{
    DatabasePluginExec, DatabasePluginManifest, DatabasePluginRegistry,
    default_database_registry_path, install_database_plugin, load_database_registry,
    resolve_database_plugin, validate_database_plugin_manifest,
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
    EvalDatasetCase, EvalDatasetDocument, EvalDocument, EvalError, EvalSpec,
    ToolA2aEndpointExpectation, ToolExitExpectation, ToolSchemaExpectation, ToolTextExpectation,
    load_eval_dataset, load_parcel_evals, load_parcel_tests,
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
    PluginResponse, parse_jsonrpc_message, parse_jsonrpc_request, request_to_jsonrpc,
    response_to_jsonrpc,
};
pub use plugins::{
    BuiltinCourier, CourierCatalogEntry, CourierPluginExec, CourierPluginManifest,
    CourierPluginRegistry, PluginRegistryError, PluginTransport, ResolvedCourier,
    default_courier_registry_path, install_courier_plugin, list_courier_catalog,
    load_courier_registry, resolve_courier,
};
pub use provider_plugins::{
    ProviderPluginExec, ProviderPluginManifest, ProviderPluginRegistry,
    default_provider_registry_path, install_provider_plugin, load_provider_registry,
    resolve_provider_plugin, validate_provider_plugin_manifest,
};
pub use secrets::{
    SecretStoreError, SecretStorePaths, init_secret_store, list_secret_names,
    maybe_secret_store_paths, remove_secret, resolve_secret_from_store, resolve_secret_with_env,
    secret_store_paths, set_secret,
};
pub use signing::{
    DISPATCH_SIGNATURE_ALGORITHM, GeneratedKeyPair, PARCEL_SIGNATURES_DIR, ParcelSignature,
    PublicKeyFile, SecretKeyFile, SignatureVerification, SigningError, generate_keypair_files,
    sign_parcel, verify_parcel_signature,
};
pub use trace::{DISPATCH_TRACE_VERSION, DispatchTraceArtifact, DispatchTraceStep};
pub use trust::{
    A2aTrustPolicy, A2aTrustRequirement, A2aTrustRule, PullTrustPolicy, PullTrustRequirement,
    PullTrustRule, TrustPolicyError,
};
pub use validate::{
    Diagnostic, Level, ValidationReport, validate_agentfile, validate_agentfile_at_path,
};
