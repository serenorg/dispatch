use crate::{
    ParsedAgentfile, Value,
    manifest::{
        BuiltinToolConfig, CommandSpec, CourierTarget, DISPATCH_WASM_ABI, EnvVar,
        FrameworkProvenance, InstructionConfig, InstructionKind, LimitSpec, LocalToolConfig,
        McpToolConfig, ModelPolicy, ModelReference, MountConfig, MountKind, NetworkRule,
        PARCEL_FORMAT_VERSION, PARCEL_SCHEMA_URL, ParcelFileRecord, ParcelManifest, SecretSpec,
        TimeoutSpec, ToolConfig, ToolInputSchemaRef, Visibility, WasmComponentConfig,
    },
    parse_agentfile,
    validate::{Level, validate_agentfile},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use thiserror::Error;
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub output_root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuiltParcel {
    pub digest: String,
    pub parcel_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub lockfile_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParcelLock {
    pub format_version: u32,
    pub digest: String,
    pub manifest: String,
    pub context_dir: String,
    pub files: Vec<ParcelFileRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationReport {
    pub digest: String,
    pub manifest_digest_matches: bool,
    pub lockfile_digest_matches: bool,
    pub lockfile_layout_matches: bool,
    pub lockfile_files_match: bool,
    pub verified_files: usize,
    pub missing_files: Vec<String>,
    pub modified_files: Vec<String>,
}

impl VerificationReport {
    pub fn is_ok(&self) -> bool {
        self.manifest_digest_matches
            && self.lockfile_digest_matches
            && self.lockfile_layout_matches
            && self.lockfile_files_match
            && self.missing_files.is_empty()
            && self.modified_files.is_empty()
    }
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("failed to read `{path}`: {source}")]
    ReadFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write `{path}`: {source}")]
    WriteFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create directory `{path}`: {source}")]
    CreateDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse `{path}`: {source}")]
    Parse {
        path: String,
        #[source]
        source: crate::ParseError,
    },
    #[error("validation failed:\n{0}")]
    Validation(String),
    #[error("missing referenced file or directory `{path}`")]
    MissingPath { path: String },
    #[error("path `{path}` escapes the build context")]
    EscapesContext { path: String },
    #[error("walk error for `{path}`: {source}")]
    Walk {
        path: String,
        #[source]
        source: walkdir::Error,
    },
    #[error("failed to serialize parcel manifest: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("tool `{tool}` schema `{path}` is invalid: {message}")]
    InvalidToolSchema {
        tool: String,
        path: String,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize)]
struct ProvisionalParcelManifest {
    #[serde(rename = "$schema")]
    schema: String,
    format_version: u32,
    source_agentfile: String,
    courier: CourierTarget,
    framework: Option<FrameworkProvenance>,
    name: Option<String>,
    version: Option<String>,
    entrypoint: Option<String>,
    instructions: Vec<InstructionConfig>,
    inline_prompts: Vec<String>,
    env: Vec<EnvVar>,
    secrets: Vec<SecretSpec>,
    visibility: Option<Visibility>,
    mounts: Vec<MountConfig>,
    tools: Vec<ToolConfig>,
    models: ModelPolicy,
    limits: Vec<LimitSpec>,
    timeouts: Vec<TimeoutSpec>,
    network: Vec<NetworkRule>,
    labels: BTreeMap<String, String>,
    files: Vec<ParcelFileRecord>,
}

#[derive(Debug, Clone, Default)]
struct ResolvedAgentSpec {
    courier: Option<CourierTarget>,
    framework: Option<FrameworkProvenance>,
    name: Option<String>,
    version: Option<String>,
    entrypoint: Option<String>,
    instructions: Vec<InstructionConfig>,
    inline_prompts: Vec<String>,
    env: Vec<EnvVar>,
    secrets: Vec<SecretSpec>,
    visibility: Option<Visibility>,
    mounts: Vec<MountConfig>,
    tools: Vec<ToolConfig>,
    models: ModelPolicy,
    limits: Vec<LimitSpec>,
    timeouts: Vec<TimeoutSpec>,
    network: Vec<NetworkRule>,
    labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
struct PackagedPath {
    entries: Vec<ParcelFileRecord>,
    sha256: String,
}

impl PackagedPath {
    fn expand(self) -> Vec<ParcelFileRecord> {
        self.entries
    }
}

pub fn build_agentfile(
    agentfile_path: &Path,
    options: &BuildOptions,
) -> Result<BuiltParcel, BuildError> {
    let agentfile_path = agentfile_path
        .canonicalize()
        .map_err(|source| BuildError::ReadFile {
            path: agentfile_path.display().to_string(),
            source,
        })?;
    let context_dir = agentfile_path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| BuildError::MissingPath {
            path: agentfile_path.display().to_string(),
        })?;

    let source = fs::read_to_string(&agentfile_path).map_err(|source| BuildError::ReadFile {
        path: agentfile_path.display().to_string(),
        source,
    })?;
    let parsed = parse_agentfile(&source).map_err(|source| BuildError::Parse {
        path: agentfile_path.display().to_string(),
        source,
    })?;
    validate_for_build(&parsed)?;

    let mut packaged = BTreeMap::<String, Vec<u8>>::new();
    let mut files = Vec::new();
    let mut resolved = ResolvedAgentSpec::default();

    for instruction in &parsed.instructions {
        match instruction.keyword.as_str() {
            "FROM" => {
                resolved.courier =
                    first_scalar(instruction.args.first()).map(CourierTarget::from_reference);
            }
            "COMPONENT" => {
                let component = parse_component(&instruction.args);
                let source_path = component.packaged_path.clone();
                let resolved_path = resolve_path(&context_dir, &source_path)?;
                let file_record = package_path(&context_dir, &resolved_path, &mut packaged)?;
                let component_sha256 = file_record.sha256.clone();
                files.extend(file_record.expand());

                let courier = resolved.courier.as_mut().ok_or_else(|| {
                    BuildError::Validation(format!(
                        "line {}: `COMPONENT` requires a preceding `FROM` instruction",
                        instruction.span.line_start
                    ))
                })?;
                if !courier.is_wasm() {
                    return Err(BuildError::Validation(
                        "`COMPONENT` is only supported for `dispatch/wasm` courier targets"
                            .to_string(),
                    ));
                }
                courier.set_component(WasmComponentConfig {
                    packaged_path: source_path,
                    sha256: component_sha256,
                    abi: DISPATCH_WASM_ABI.to_string(),
                });
            }
            "NAME" => resolved.name = first_scalar(instruction.args.first()),
            "VERSION" => resolved.version = first_scalar(instruction.args.first()),
            "FRAMEWORK" => resolved.framework = parse_framework(&instruction.args),
            "ENTRYPOINT" => resolved.entrypoint = first_scalar(instruction.args.first()),
            "VISIBILITY" => {
                resolved.visibility = first_scalar(instruction.args.first())
                    .and_then(|value| parse_visibility(&value))
            }
            "ENV" => {
                if let Some(env_var) = parse_env_var(&instruction.args) {
                    resolved.env.push(env_var);
                }
            }
            "SECRET" => {
                if let Some(name) = first_scalar(instruction.args.first()) {
                    resolved.secrets.push(SecretSpec {
                        name,
                        required: true,
                    });
                }
            }
            "MOUNT" => {
                if let Some(mount) = parse_mount(&instruction.args) {
                    resolved.mounts.push(mount);
                }
            }
            "TOOL" => {
                if let Some(mut tool) = parse_tool(&instruction.args) {
                    if let ToolConfig::Local(local) = &mut tool {
                        let resolved_path = resolve_path(&context_dir, &local.packaged_path)?;
                        let file_record =
                            package_path(&context_dir, &resolved_path, &mut packaged)?;
                        files.extend(file_record.expand());
                        if let Some(schema) = &local.input_schema {
                            let resolved_schema_path =
                                resolve_path(&context_dir, &schema.packaged_path)?;
                            validate_tool_schema(&resolved_schema_path, &local.alias)?;
                            let schema_record =
                                package_path(&context_dir, &resolved_schema_path, &mut packaged)?;
                            local.input_schema = Some(ToolInputSchemaRef {
                                packaged_path: schema.packaged_path.clone(),
                                sha256: schema_record.sha256.clone(),
                            });
                            files.extend(schema_record.expand());
                        }
                    }
                    resolved.tools.push(tool);
                }
            }
            "MODEL" => {
                if let Some(model) = parse_model_reference(&instruction.args) {
                    resolved.models.primary = Some(model);
                }
            }
            "FALLBACK" => {
                if let Some(model) = parse_model_reference(&instruction.args) {
                    resolved.models.fallbacks.push(model);
                }
            }
            "ROUTING" => resolved.models.routing = first_scalar(instruction.args.first()),
            "LIMIT" => {
                if let Some(limit) = parse_limit(&instruction.args) {
                    resolved.limits.push(limit);
                }
            }
            "TIMEOUT" => {
                if let Some(timeout) = parse_timeout(&instruction.args) {
                    resolved.timeouts.push(timeout);
                }
            }
            "NETWORK" => {
                if let Some(rule) = parse_network_rule(&instruction.args) {
                    resolved.network.push(rule);
                }
            }
            "LABEL" => {
                if let Some((key, value)) = parse_label(&instruction.args) {
                    resolved.labels.insert(key, value);
                }
            }
            "PROMPT" => {
                for value in &instruction.args {
                    match value {
                        Value::String(value) => resolved.inline_prompts.push(value.clone()),
                        Value::Heredoc(doc) => resolved.inline_prompts.push(doc.body.clone()),
                        Value::Token(value) => resolved.inline_prompts.push(value.clone()),
                    }
                }
            }
            "IDENTITY" | "SOUL" | "SKILL" | "AGENTS" | "USER" | "TOOLS" | "EVAL" => {
                let source_path = scalar_at(&instruction.args, 0);
                let resolved_path = resolve_path(&context_dir, &source_path)?;
                let file_record = package_path(&context_dir, &resolved_path, &mut packaged)?;
                resolved.instructions.push(InstructionConfig {
                    kind: instruction_kind_from_keyword(&instruction.keyword),
                    packaged_path: source_path,
                    sha256: file_record.sha256.clone(),
                });
                files.extend(file_record.expand());
            }
            "MEMORY" => {
                if instruction.args.len() >= 2 {
                    let maybe_path = scalar_at(&instruction.args, instruction.args.len() - 1);
                    if looks_like_path(&maybe_path) {
                        let resolved_path = resolve_path(&context_dir, &maybe_path)?;
                        let file_record =
                            package_path(&context_dir, &resolved_path, &mut packaged)?;
                        resolved.instructions.push(InstructionConfig {
                            kind: InstructionKind::Memory,
                            packaged_path: maybe_path,
                            sha256: file_record.sha256.clone(),
                        });
                        files.extend(file_record.expand());
                    }
                }
            }
            "HEARTBEAT" => {
                let tokens = scalars(&instruction.args);
                if let Some(file_index) = tokens.iter().position(|value| value == "FILE")
                    && let Some(path_value) = instruction.args.get(file_index + 1)
                {
                    let source_path = scalar_value(path_value);
                    let resolved_path = resolve_path(&context_dir, &source_path)?;
                    let file_record = package_path(&context_dir, &resolved_path, &mut packaged)?;
                    resolved.instructions.push(InstructionConfig {
                        kind: InstructionKind::Heartbeat,
                        packaged_path: source_path,
                        sha256: file_record.sha256.clone(),
                    });
                    files.extend(file_record.expand());
                }
            }
            "COPY" | "ADD" => {
                let source_path = scalar_at(&instruction.args, 0);
                let resolved_path = resolve_path(&context_dir, &source_path)?;
                let file_record = package_path(&context_dir, &resolved_path, &mut packaged)?;
                files.extend(file_record.expand());
            }
            _ => {}
        }
    }

    files.sort_by(|left, right| left.packaged_as.cmp(&right.packaged_as));
    files.dedup_by(|left, right| left.packaged_as == right.packaged_as);

    let provisional = ProvisionalParcelManifest {
        schema: PARCEL_SCHEMA_URL.to_string(),
        format_version: PARCEL_FORMAT_VERSION,
        source_agentfile: relative_display(&context_dir, &agentfile_path),
        courier: resolved.courier.ok_or_else(|| {
            BuildError::Validation("line 1: missing required `FROM` instruction".to_string())
        })?,
        framework: resolved.framework,
        name: resolved.name,
        version: resolved.version,
        entrypoint: resolved.entrypoint,
        instructions: resolved.instructions,
        inline_prompts: resolved.inline_prompts,
        env: resolved.env,
        secrets: resolved.secrets,
        visibility: resolved.visibility,
        mounts: resolved.mounts,
        tools: resolved.tools,
        models: resolved.models,
        limits: resolved.limits,
        timeouts: resolved.timeouts,
        network: resolved.network,
        labels: resolved.labels,
        files: files.clone(),
    };

    validate_courier_requirements(&provisional.courier)?;

    let serialized = serde_json::to_vec_pretty(&provisional)?;
    let digest = hex_digest(&serialized);

    let parcel_dir = options.output_root.join(&digest);
    let package_root = parcel_dir.join("context");
    fs::create_dir_all(&package_root).map_err(|source| BuildError::CreateDir {
        path: package_root.display().to_string(),
        source,
    })?;

    for (relative, bytes) in packaged {
        let output = package_root.join(&relative);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent).map_err(|source| BuildError::CreateDir {
                path: parent.display().to_string(),
                source,
            })?;
        }
        fs::write(&output, bytes).map_err(|source| BuildError::WriteFile {
            path: output.display().to_string(),
            source,
        })?;
    }

    let parcel_manifest = ParcelManifest {
        schema: provisional.schema,
        format_version: provisional.format_version,
        digest: digest.clone(),
        source_agentfile: provisional.source_agentfile,
        courier: provisional.courier,
        framework: provisional.framework,
        name: provisional.name,
        version: provisional.version,
        entrypoint: provisional.entrypoint,
        instructions: provisional.instructions,
        inline_prompts: provisional.inline_prompts,
        env: provisional.env,
        secrets: provisional.secrets,
        visibility: provisional.visibility,
        mounts: provisional.mounts,
        tools: provisional.tools,
        models: provisional.models,
        limits: provisional.limits,
        timeouts: provisional.timeouts,
        network: provisional.network,
        labels: provisional.labels,
        files,
    };

    let manifest_path = parcel_dir.join("manifest.json");
    let lockfile_path = parcel_dir.join("parcel.lock");
    fs::create_dir_all(&parcel_dir).map_err(|source| BuildError::CreateDir {
        path: parcel_dir.display().to_string(),
        source,
    })?;

    fs::write(&manifest_path, serde_json::to_vec_pretty(&parcel_manifest)?).map_err(|source| {
        BuildError::WriteFile {
            path: manifest_path.display().to_string(),
            source,
        }
    })?;

    let lockfile = ParcelLock {
        format_version: PARCEL_FORMAT_VERSION,
        digest,
        manifest: "manifest.json".to_string(),
        context_dir: "context".to_string(),
        files: parcel_manifest.files.clone(),
    };
    fs::write(&lockfile_path, serde_json::to_vec_pretty(&lockfile)?).map_err(|source| {
        BuildError::WriteFile {
            path: lockfile_path.display().to_string(),
            source,
        }
    })?;

    Ok(BuiltParcel {
        digest: parcel_manifest.digest.clone(),
        parcel_dir,
        manifest_path,
        lockfile_path,
    })
}

pub fn verify_parcel(parcel_path: &Path) -> Result<VerificationReport, BuildError> {
    let manifest_path = resolve_manifest_path(parcel_path);
    let parcel_dir =
        manifest_path
            .parent()
            .map(PathBuf::from)
            .ok_or_else(|| BuildError::MissingPath {
                path: manifest_path.display().to_string(),
            })?;
    let parcel: ParcelManifest =
        serde_json::from_slice(&fs::read(&manifest_path).map_err(|source| {
            BuildError::ReadFile {
                path: manifest_path.display().to_string(),
                source,
            }
        })?)?;
    let lockfile_path = parcel_dir.join("parcel.lock");
    let lockfile: ParcelLock =
        serde_json::from_slice(&fs::read(&lockfile_path).map_err(|source| {
            BuildError::ReadFile {
                path: lockfile_path.display().to_string(),
                source,
            }
        })?)?;

    let expected_digest = provisional_digest(&parcel)?;
    let mut missing_files = Vec::new();
    let mut modified_files = Vec::new();
    for file in &parcel.files {
        let path = parcel_dir.join("context").join(&file.packaged_as);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing_files.push(file.packaged_as.clone());
                continue;
            }
            Err(source) => {
                return Err(BuildError::ReadFile {
                    path: path.display().to_string(),
                    source,
                });
            }
        };

        if hex_digest(&bytes) != file.sha256 || bytes.len() as u64 != file.size_bytes {
            modified_files.push(file.packaged_as.clone());
        }
    }

    Ok(VerificationReport {
        digest: parcel.digest.clone(),
        manifest_digest_matches: parcel.digest == expected_digest,
        lockfile_digest_matches: lockfile.digest == parcel.digest,
        lockfile_layout_matches: lockfile.format_version == parcel.format_version
            && lockfile.manifest == "manifest.json"
            && lockfile.context_dir == "context",
        lockfile_files_match: lockfile.files == parcel.files,
        verified_files: parcel.files.len(),
        missing_files,
        modified_files,
    })
}

fn validate_for_build(parsed: &ParsedAgentfile) -> Result<(), BuildError> {
    let report = validate_agentfile(parsed);
    if report.is_ok() {
        return Ok(());
    }

    let details = report
        .diagnostics
        .into_iter()
        .filter(|diagnostic| diagnostic.level == Level::Error)
        .map(|diagnostic| format!("line {}: {}", diagnostic.line, diagnostic.message))
        .collect::<Vec<_>>()
        .join("\n");

    Err(BuildError::Validation(details))
}

fn validate_courier_requirements(courier: &CourierTarget) -> Result<(), BuildError> {
    if courier.is_wasm() && courier.component().is_none() {
        return Err(BuildError::Validation(
            "line 1: `FROM dispatch/wasm...` requires a `COMPONENT <path>` instruction".to_string(),
        ));
    }

    Ok(())
}

fn provisional_digest(parcel: &ParcelManifest) -> Result<String, BuildError> {
    let provisional = ProvisionalParcelManifest {
        schema: parcel.schema.clone(),
        format_version: parcel.format_version,
        source_agentfile: parcel.source_agentfile.clone(),
        courier: parcel.courier.clone(),
        framework: parcel.framework.clone(),
        name: parcel.name.clone(),
        version: parcel.version.clone(),
        entrypoint: parcel.entrypoint.clone(),
        instructions: parcel.instructions.clone(),
        inline_prompts: parcel.inline_prompts.clone(),
        env: parcel.env.clone(),
        secrets: parcel.secrets.clone(),
        visibility: parcel.visibility,
        mounts: parcel.mounts.clone(),
        tools: parcel.tools.clone(),
        models: parcel.models.clone(),
        limits: parcel.limits.clone(),
        timeouts: parcel.timeouts.clone(),
        network: parcel.network.clone(),
        labels: parcel.labels.clone(),
        files: parcel.files.clone(),
    };
    let serialized = serde_json::to_vec_pretty(&provisional)?;
    Ok(hex_digest(&serialized))
}

fn resolve_manifest_path(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.join("manifest.json")
    } else {
        path.to_path_buf()
    }
}

fn package_path(
    context_dir: &Path,
    resolved: &Path,
    packaged: &mut BTreeMap<String, Vec<u8>>,
) -> Result<PackagedPath, BuildError> {
    if resolved.is_file() {
        let bytes = fs::read(resolved).map_err(|source| BuildError::ReadFile {
            path: resolved.display().to_string(),
            source,
        })?;
        let relative = relative_display(context_dir, resolved);
        packaged.insert(relative.clone(), bytes.clone());
        return Ok(PackagedPath {
            sha256: hex_digest(&bytes),
            entries: vec![ParcelFileRecord {
                source: relative.clone(),
                packaged_as: relative,
                sha256: hex_digest(&bytes),
                size_bytes: bytes.len() as u64,
            }],
        });
    }

    let mut entries = Vec::new();
    let mut hasher = Sha256::new();

    for entry in WalkDir::new(resolved) {
        let entry = entry.map_err(|source| BuildError::Walk {
            path: resolved.display().to_string(),
            source,
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let bytes = fs::read(path).map_err(|source| BuildError::ReadFile {
            path: path.display().to_string(),
            source,
        })?;
        let relative = relative_display(context_dir, path);
        hasher.update(relative.as_bytes());
        hasher.update(&bytes);
        packaged.insert(relative.clone(), bytes.clone());
        entries.push(ParcelFileRecord {
            source: relative.clone(),
            packaged_as: relative,
            sha256: hex_digest(&bytes),
            size_bytes: bytes.len() as u64,
        });
    }

    entries.sort_by(|left, right| left.packaged_as.cmp(&right.packaged_as));

    Ok(PackagedPath {
        sha256: encode_hex(hasher.finalize()),
        entries,
    })
}

fn resolve_path(context_dir: &Path, relative: &str) -> Result<PathBuf, BuildError> {
    let joined = context_dir.join(relative);
    if !joined.exists() {
        return Err(BuildError::MissingPath {
            path: relative.to_string(),
        });
    }
    let resolved = joined
        .canonicalize()
        .map_err(|source| BuildError::ReadFile {
            path: joined.display().to_string(),
            source,
        })?;
    if !resolved.starts_with(context_dir) {
        return Err(BuildError::EscapesContext {
            path: relative.to_string(),
        });
    }
    Ok(resolved)
}

fn instruction_kind_from_keyword(keyword: &str) -> InstructionKind {
    match keyword {
        "IDENTITY" => InstructionKind::Identity,
        "SOUL" => InstructionKind::Soul,
        "SKILL" => InstructionKind::Skill,
        "AGENTS" => InstructionKind::Agents,
        "USER" => InstructionKind::User,
        "TOOLS" => InstructionKind::Tools,
        "EVAL" => InstructionKind::Eval,
        "MEMORY" => InstructionKind::Memory,
        "HEARTBEAT" => InstructionKind::Heartbeat,
        _ => unreachable!("unexpected instruction keyword `{keyword}`"),
    }
}

fn parse_visibility(value: &str) -> Option<Visibility> {
    match value {
        "open" => Some(Visibility::Open),
        "opaque" => Some(Visibility::Opaque),
        _ => None,
    }
}

fn parse_framework(args: &[Value]) -> Option<FrameworkProvenance> {
    let tokens = scalars(args);
    let name = tokens.first()?.clone();
    let mut version = None;
    let mut target = None;

    let mut index = 1;
    while index < tokens.len() {
        match tokens[index].as_str() {
            "VERSION" if index + 1 < tokens.len() => {
                version = Some(tokens[index + 1].clone());
                index += 2;
            }
            "TARGET" if index + 1 < tokens.len() => {
                target = Some(tokens[index + 1].clone());
                index += 2;
            }
            _ => index += 1,
        }
    }

    Some(FrameworkProvenance {
        name,
        version,
        target,
    })
}

fn parse_model_reference(args: &[Value]) -> Option<ModelReference> {
    let tokens = scalars(args);
    let id = tokens.first()?.clone();
    let mut provider = None;

    let mut index = 1;
    while index < tokens.len() {
        match tokens[index].as_str() {
            "PROVIDER" if index + 1 < tokens.len() => {
                provider = Some(tokens[index + 1].clone());
                index += 2;
            }
            _ => index += 1,
        }
    }

    Some(ModelReference { id, provider })
}

fn parse_env_var(args: &[Value]) -> Option<EnvVar> {
    let joined = join_scalars(args);
    let (name, value) = joined.split_once('=')?;
    Some(EnvVar {
        name: name.to_string(),
        value: value.to_string(),
    })
}

fn parse_mount(args: &[Value]) -> Option<MountConfig> {
    let tokens = scalars(args);
    if tokens.len() < 2 {
        return None;
    }

    Some(MountConfig {
        kind: match tokens[0].as_str() {
            "SESSION" => MountKind::Session,
            "MEMORY" => MountKind::Memory,
            "ARTIFACTS" => MountKind::Artifacts,
            _ => return None,
        },
        driver: tokens[1].clone(),
        options: tokens[2..].to_vec(),
    })
}

fn parse_tool(args: &[Value]) -> Option<ToolConfig> {
    let tokens = scalars(args);
    match tokens.first()?.as_str() {
        "LOCAL" => parse_local_tool(&tokens).map(ToolConfig::Local),
        "BUILTIN" => tokens.get(1).map(|capability| {
            ToolConfig::Builtin(BuiltinToolConfig {
                capability: capability.clone(),
                approval: parse_named_value(&tokens, "APPROVAL"),
                description: parse_named_value(&tokens, "DESCRIPTION"),
            })
        }),
        "MCP" => tokens.get(1).map(|server| {
            ToolConfig::Mcp(McpToolConfig {
                server: server.clone(),
                approval: parse_named_value(&tokens, "APPROVAL"),
                description: parse_named_value(&tokens, "DESCRIPTION"),
            })
        }),
        _ => None,
    }
}

struct ParsedComponent {
    packaged_path: String,
}

fn parse_component(args: &[Value]) -> ParsedComponent {
    let tokens = scalars(args);
    let packaged_path = tokens.first().cloned().unwrap_or_default();
    ParsedComponent { packaged_path }
}

fn parse_local_tool(tokens: &[String]) -> Option<LocalToolConfig> {
    let packaged_path = tokens.get(1)?.clone();
    let alias = parse_named_value(tokens, "AS").unwrap_or_else(|| {
        Path::new(&packaged_path)
            .file_stem()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| packaged_path.clone())
    });
    let approval = parse_named_value(tokens, "APPROVAL");
    let description = parse_named_value(tokens, "DESCRIPTION");
    let input_schema =
        parse_named_value(tokens, "SCHEMA").map(|packaged_path| ToolInputSchemaRef {
            packaged_path,
            sha256: String::new(),
        });
    let runner = parse_tool_runner(tokens, &packaged_path);

    Some(LocalToolConfig {
        alias,
        packaged_path,
        runner,
        approval,
        description,
        input_schema,
    })
}

fn parse_tool_runner(tokens: &[String], packaged_path: &str) -> CommandSpec {
    if let Some(using_index) = tokens.iter().position(|token| token == "USING") {
        let mut args = Vec::new();
        let mut index = using_index + 1;
        let command = tokens
            .get(index)
            .cloned()
            .unwrap_or_else(|| infer_runner(packaged_path).command);
        index += 1;

        while let Some(token) = tokens.get(index) {
            if token == "AS" || token == "APPROVAL" || token == "DESCRIPTION" || token == "SCHEMA" {
                break;
            }
            args.push(token.clone());
            index += 1;
        }

        return CommandSpec { command, args };
    }

    infer_runner(packaged_path)
}

fn infer_runner(packaged_path: &str) -> CommandSpec {
    let extension = Path::new(packaged_path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();

    let command = match extension {
        "py" => "python3",
        "js" => "node",
        "ts" => "tsx",
        "sh" => "sh",
        _ => packaged_path,
    };

    CommandSpec {
        command: command.to_string(),
        args: Vec::new(),
    }
}

fn parse_limit(args: &[Value]) -> Option<LimitSpec> {
    let tokens = scalars(args);
    if tokens.len() < 2 {
        return None;
    }
    Some(LimitSpec {
        scope: tokens[0].clone(),
        value: tokens[1].clone(),
        qualifiers: tokens[2..].to_vec(),
    })
}

fn parse_timeout(args: &[Value]) -> Option<TimeoutSpec> {
    let tokens = scalars(args);
    if tokens.len() < 2 {
        return None;
    }
    Some(TimeoutSpec {
        scope: tokens[0].clone(),
        duration: tokens[1].clone(),
        qualifiers: tokens[2..].to_vec(),
    })
}

fn parse_network_rule(args: &[Value]) -> Option<NetworkRule> {
    let tokens = scalars(args);
    if tokens.len() < 2 {
        return None;
    }
    Some(NetworkRule {
        action: tokens[0].clone(),
        target: tokens[1].clone(),
        qualifiers: tokens[2..].to_vec(),
    })
}

fn parse_label(args: &[Value]) -> Option<(String, String)> {
    let joined = join_scalars(args);
    if let Some((key, value)) = joined.split_once('=') {
        return Some((key.to_string(), strip_matching_quotes(value)));
    }

    let tokens = scalars(args);
    if tokens.len() >= 2 {
        return Some((
            tokens[0].clone(),
            strip_matching_quotes(&tokens[1..].join(" ")),
        ));
    }

    None
}

fn parse_named_value(tokens: &[String], marker: &str) -> Option<String> {
    tokens
        .windows(2)
        .find(|window| window[0] == marker)
        .map(|window| window[1].clone())
}

fn strip_matching_quotes(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn scalar_at(args: &[Value], index: usize) -> String {
    args.get(index).map(scalar_value).unwrap_or_default()
}

fn scalar_value(value: &Value) -> String {
    match value {
        Value::Token(value) | Value::String(value) => value.clone(),
        Value::Heredoc(doc) => doc.body.clone(),
    }
}

fn first_scalar(value: Option<&Value>) -> Option<String> {
    value.map(scalar_value)
}

fn join_scalars(args: &[Value]) -> String {
    scalars(args).join(" ")
}

fn scalars(args: &[Value]) -> Vec<String> {
    args.iter().map(scalar_value).collect()
}

// Heuristic used when the MEMORY instruction's last argument might be either a
// file path (MEMORY POLICY policy.md) or a bare driver/keyword token
// (e.g. a future form that takes inline options). A value is treated as a path
// if it contains a path separator or any dot - this catches all common file
// extensions (.md, .txt, .json, .yaml, .toml, etc.) while excluding bare
// alphanumeric driver names such as "sqlite" or "pgvector".
fn looks_like_path(value: &str) -> bool {
    value.contains('/') || value.contains('.')
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    encode_hex(hasher.finalize())
}

fn encode_hex(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn validate_tool_schema(path: &Path, tool: &str) -> Result<(), BuildError> {
    let bytes = fs::read(path).map_err(|source| BuildError::ReadFile {
        path: path.display().to_string(),
        source,
    })?;
    let schema: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|source| BuildError::InvalidToolSchema {
            tool: tool.to_string(),
            path: path.display().to_string(),
            message: source.to_string(),
        })?;
    if !schema.is_object() {
        return Err(BuildError::InvalidToolSchema {
            tool: tool.to_string(),
            path: path.display().to_string(),
            message: "schema root must be a JSON object".to_string(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn build_emits_typed_manifest() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
NAME demo
VERSION 1.0.0
FRAMEWORK adk-rust VERSION 0.5.0 TARGET wasm
IDENTITY IDENTITY.md
SOUL SOUL.md
SKILL SKILL.md
AGENTS AGENTS.md
USER USER.md
TOOLS TOOLS.md
MODEL claude-sonnet-4 PROVIDER anthropic
FALLBACK gpt-5-nano PROVIDER openai
TOOL LOCAL tools/demo.py AS demo USING python3 -u DESCRIPTION \"Look up a record by id and print JSON.\"
TOOL BUILTIN web_search APPROVAL required DESCRIPTION \"Search the web for live information.\"
MOUNT SESSION sqlite
NETWORK allow api.example.com
ENV TZ=UTC
SECRET OPENAI_API_KEY
LABEL org.example.team=platform
ENTRYPOINT chat
",
        )
        .unwrap();
        fs::write(dir.path().join("IDENTITY.md"), "identity").unwrap();
        fs::write(dir.path().join("SOUL.md"), "soul").unwrap();
        fs::write(dir.path().join("SKILL.md"), "skill").unwrap();
        fs::write(dir.path().join("AGENTS.md"), "agents").unwrap();
        fs::write(dir.path().join("USER.md"), "user").unwrap();
        fs::write(dir.path().join("TOOLS.md"), "tools").unwrap();
        fs::create_dir_all(dir.path().join("tools")).unwrap();
        fs::write(dir.path().join("tools/demo.py"), "print('ok')").unwrap();

        let built = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let parcel: ParcelManifest =
            serde_json::from_slice(&fs::read(built.manifest_path).unwrap()).unwrap();

        assert_eq!(parcel.schema, PARCEL_SCHEMA_URL);
        assert_eq!(parcel.courier.reference(), "dispatch/native:latest");
        assert_eq!(
            parcel
                .framework
                .as_ref()
                .map(|framework| framework.name.as_str()),
            Some("adk-rust")
        );
        assert_eq!(
            parcel
                .framework
                .as_ref()
                .and_then(|framework| framework.version.as_deref()),
            Some("0.5.0")
        );
        assert_eq!(
            parcel
                .framework
                .as_ref()
                .and_then(|framework| framework.target.as_deref()),
            Some("wasm")
        );
        assert_eq!(
            parcel.models.primary.as_ref().unwrap().id,
            "claude-sonnet-4"
        );
        assert_eq!(
            parcel.models.primary.as_ref().unwrap().provider.as_deref(),
            Some("anthropic")
        );
        assert_eq!(parcel.models.fallbacks[0].id, "gpt-5-nano");
        assert_eq!(
            parcel.models.fallbacks[0].provider.as_deref(),
            Some("openai")
        );
        assert_eq!(parcel.env[0].name, "TZ");
        assert_eq!(parcel.secrets[0].name, "OPENAI_API_KEY");
        assert_eq!(parcel.labels["org.example.team"], "platform");
        assert_eq!(parcel.instructions.len(), 6);
        assert!(matches!(
            parcel.instructions[0].kind,
            InstructionKind::Identity
        ));
        assert!(matches!(parcel.instructions[1].kind, InstructionKind::Soul));
        assert!(matches!(
            parcel.instructions[2].kind,
            InstructionKind::Skill
        ));
        assert!(matches!(
            parcel.instructions[3].kind,
            InstructionKind::Agents
        ));
        assert!(matches!(parcel.instructions[4].kind, InstructionKind::User));
        assert!(matches!(
            parcel.instructions[5].kind,
            InstructionKind::Tools
        ));
        match &parcel.tools[0] {
            ToolConfig::Local(local) => {
                assert_eq!(local.alias, "demo");
                assert_eq!(local.runner.command, "python3");
                assert_eq!(local.runner.args, vec!["-u"]);
                assert_eq!(
                    local.description.as_deref(),
                    Some("Look up a record by id and print JSON.")
                );
            }
            other => panic!("expected local tool, got {other:?}"),
        }
        match &parcel.tools[1] {
            ToolConfig::Builtin(tool) => {
                assert_eq!(
                    tool.description.as_deref(),
                    Some("Search the web for live information.")
                );
            }
            other => panic!("expected builtin tool, got {other:?}"),
        }
    }

    #[test]
    fn build_preserves_heartbeat_mount_options_and_network_qualifiers() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
HEARTBEAT EVERY 30s FILE HEARTBEAT.md
MOUNT MEMORY pgvector tenant=acme namespace=agents
NETWORK allow api.example.com via-egress
LABEL org.example.display=\"Market Monitor\"
ENTRYPOINT heartbeat
",
        )
        .unwrap();
        fs::write(dir.path().join("HEARTBEAT.md"), "poll").unwrap();

        let built = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let parcel: ParcelManifest =
            serde_json::from_slice(&fs::read(built.manifest_path).unwrap()).unwrap();

        assert!(matches!(
            parcel.instructions[0].kind,
            InstructionKind::Heartbeat
        ));
        assert_eq!(parcel.mounts[0].driver, "pgvector");
        assert_eq!(
            parcel.mounts[0].options,
            vec!["tenant=acme", "namespace=agents"]
        );
        assert_eq!(parcel.network[0].action, "allow");
        assert_eq!(parcel.network[0].target, "api.example.com");
        assert_eq!(parcel.network[0].qualifiers, vec!["via-egress"]);
        assert_eq!(parcel.labels["org.example.display"], "Market Monitor");
    }

    #[test]
    fn build_records_framework_provenance_without_optional_fields() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
FRAMEWORK adk-rust
ENTRYPOINT chat
",
        )
        .unwrap();

        let built = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let parcel: ParcelManifest =
            serde_json::from_slice(&fs::read(built.manifest_path).unwrap()).unwrap();

        let framework = parcel
            .framework
            .expect("framework provenance should be present");
        assert_eq!(framework.name, "adk-rust");
        assert_eq!(framework.version, None);
        assert_eq!(framework.target, None);
    }

    #[test]
    fn build_supports_extended_workspace_instruction_files() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
IDENTITY IDENTITY.md
SOUL SOUL.md
AGENTS AGENTS.md
USER USER.md
TOOLS TOOLS.md
MEMORY POLICY MEMORY.md
ENTRYPOINT chat
",
        )
        .unwrap();
        fs::write(dir.path().join("IDENTITY.md"), "name: demo").unwrap();
        fs::write(dir.path().join("SOUL.md"), "soul").unwrap();
        fs::write(dir.path().join("AGENTS.md"), "procedures").unwrap();
        fs::write(dir.path().join("USER.md"), "prefs").unwrap();
        fs::write(dir.path().join("TOOLS.md"), "tool notes").unwrap();
        fs::write(dir.path().join("MEMORY.md"), "memory").unwrap();

        let built = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let parcel: ParcelManifest =
            serde_json::from_slice(&fs::read(built.manifest_path).unwrap()).unwrap();

        assert_eq!(
            parcel
                .instructions
                .iter()
                .map(|instruction| instruction.kind)
                .collect::<Vec<_>>(),
            vec![
                InstructionKind::Identity,
                InstructionKind::Soul,
                InstructionKind::Agents,
                InstructionKind::User,
                InstructionKind::Tools,
                InstructionKind::Memory,
            ]
        );
    }

    #[test]
    fn build_packages_tool_input_schema_and_records_hash() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
MODEL gpt-5-mini
TOOL LOCAL tools/demo.sh AS demo SCHEMA schemas/demo.json
ENTRYPOINT chat
",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("tools")).unwrap();
        fs::create_dir_all(dir.path().join("schemas")).unwrap();
        fs::write(dir.path().join("tools/demo.sh"), "printf ok").unwrap();
        let schema_body = "{\n  \"type\": \"object\",\n  \"properties\": {\n    \"id\": { \"type\": \"string\" }\n  },\n  \"required\": [\"id\"]\n}";
        fs::write(dir.path().join("schemas/demo.json"), schema_body).unwrap();

        let built = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let parcel: ParcelManifest =
            serde_json::from_slice(&fs::read(&built.manifest_path).unwrap()).unwrap();

        match &parcel.tools[0] {
            ToolConfig::Local(local) => {
                let schema = local
                    .input_schema
                    .as_ref()
                    .expect("expected packaged input schema");
                assert_eq!(schema.packaged_path, "schemas/demo.json");
                assert_eq!(schema.sha256, hex_digest(schema_body.as_bytes()));
            }
            other => panic!("expected local tool, got {other:?}"),
        }

        let packaged_schema = built.parcel_dir.join("context/schemas/demo.json");
        assert_eq!(fs::read_to_string(packaged_schema).unwrap(), schema_body);
    }

    #[test]
    fn build_records_wasm_component_in_courier_target() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/wasm:latest
COMPONENT components/assistant.wat
SOUL SOUL.md
ENTRYPOINT chat
",
        )
        .unwrap();
        fs::write(dir.path().join("SOUL.md"), "soul").unwrap();
        fs::create_dir_all(dir.path().join("components")).unwrap();
        fs::write(dir.path().join("components/assistant.wat"), "(component)").unwrap();

        let built = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let parcel: ParcelManifest =
            serde_json::from_slice(&fs::read(built.manifest_path).unwrap()).unwrap();
        let component = parcel
            .courier
            .component()
            .cloned()
            .expect("expected wasm component in courier target");

        assert_eq!(parcel.courier.reference(), "dispatch/wasm:latest");
        assert_eq!(component.packaged_path, "components/assistant.wat");
        assert_eq!(component.abi, DISPATCH_WASM_ABI);
        assert_eq!(component.sha256.len(), 64);
    }

    #[test]
    fn build_rejects_invalid_tool_input_schema() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
TOOL LOCAL tools/demo.sh AS demo SCHEMA schemas/demo.json
ENTRYPOINT chat
",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("tools")).unwrap();
        fs::create_dir_all(dir.path().join("schemas")).unwrap();
        fs::write(dir.path().join("tools/demo.sh"), "printf ok").unwrap();
        fs::write(dir.path().join("schemas/demo.json"), "[1,2,3]").unwrap();

        let error = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            BuildError::InvalidToolSchema { tool, .. } if tool == "demo"
        ));
    }

    #[test]
    fn verify_parcel_accepts_clean_built_parcel() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
SOUL SOUL.md
ENTRYPOINT chat
",
        )
        .unwrap();
        fs::write(dir.path().join("SOUL.md"), "soul").unwrap();

        let built = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let report = verify_parcel(&built.parcel_dir).unwrap();

        assert!(report.is_ok());
        assert_eq!(report.verified_files, 1);
        assert!(report.missing_files.is_empty());
        assert!(report.modified_files.is_empty());
    }

    #[test]
    fn verify_parcel_detects_modified_packaged_file() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
SOUL SOUL.md
ENTRYPOINT chat
",
        )
        .unwrap();
        fs::write(dir.path().join("SOUL.md"), "soul").unwrap();

        let built = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();
        fs::write(built.parcel_dir.join("context/SOUL.md"), "tampered").unwrap();

        let report = verify_parcel(&built.parcel_dir).unwrap();

        assert!(!report.is_ok());
        assert_eq!(report.modified_files, vec!["SOUL.md"]);
    }

    #[test]
    fn verify_parcel_detects_lockfile_digest_mismatch() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
SOUL SOUL.md
ENTRYPOINT chat
",
        )
        .unwrap();
        fs::write(dir.path().join("SOUL.md"), "soul").unwrap();

        let built = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let mut lockfile: ParcelLock =
            serde_json::from_slice(&fs::read(&built.lockfile_path).unwrap()).unwrap();
        lockfile.digest = "bad-digest".to_string();
        fs::write(
            &built.lockfile_path,
            serde_json::to_vec_pretty(&lockfile).unwrap(),
        )
        .unwrap();

        let report = verify_parcel(&built.parcel_dir).unwrap();

        assert!(!report.is_ok());
        assert!(!report.lockfile_digest_matches);
    }

    #[test]
    fn provisional_digest_excludes_embedded_manifest_digest_field() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
SOUL SOUL.md
ENTRYPOINT chat
",
        )
        .unwrap();
        fs::write(dir.path().join("SOUL.md"), "soul").unwrap();

        let built = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let mut parcel: ParcelManifest =
            serde_json::from_slice(&fs::read(&built.manifest_path).unwrap()).unwrap();
        let expected_digest = parcel.digest.clone();
        parcel.digest = "f".repeat(64);

        assert_eq!(provisional_digest(&parcel).unwrap(), expected_digest);
    }
}
