use crate::{
    ParsedAgentfile, Value,
    manifest::{
        A2aAuthConfig, A2aEndpointMode, A2aToolConfig, BuiltinToolConfig, CommandSpec,
        CompactionConfig, CourierTarget, DISPATCH_WASM_ABI, EnvVar, FrameworkProvenance,
        InstructionConfig, InstructionKind, LimitSpec, LocalToolConfig, McpToolConfig, ModelPolicy,
        ModelReference, MountConfig, MountKind, NetworkRule, PARCEL_FORMAT_VERSION,
        PARCEL_SCHEMA_URL, ParcelFileRecord, ParcelManifest, SecretSpec, TimeoutSpec,
        ToolApprovalPolicy, ToolConfig, ToolInputSchemaRef, ToolRiskLevel, Visibility,
        WasmComponentConfig,
    },
    parse_agentfile,
    skill::{
        DispatchSkillManifest, DispatchSkillTool, allowed_tool_warnings,
        dispatch_skill_manifest_path, parse_skill_markdown, validate_agent_skill_frontmatter,
    },
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
    pub warnings: Vec<String>,
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
    compaction: Option<CompactionConfig>,
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
    compaction: Option<CompactionConfig>,
    limits: Vec<LimitSpec>,
    timeouts: Vec<TimeoutSpec>,
    network: Vec<NetworkRule>,
    labels: BTreeMap<String, String>,
    entrypoint_declared: bool,
    skill_tool_aliases: BTreeMap<String, Vec<String>>,
    warnings: Vec<String>,
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
            "ENTRYPOINT" => {
                if let Some(entrypoint) = first_scalar(instruction.args.first()) {
                    resolved.entrypoint = Some(validate_entrypoint_value(
                        &entrypoint,
                        "Agentfile ENTRYPOINT",
                    )?);
                    resolved.entrypoint_declared = true;
                }
            }
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
                if let Some(mut tool) = parse_tool(&instruction.args)? {
                    package_tool_config(&context_dir, &mut packaged, &mut files, &mut tool)?;
                    insert_resolved_tool(&mut resolved.tools, &mut resolved.warnings, tool)?;
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
            "COMPACTION" => resolved.compaction = parse_compaction(&instruction.args),
            "LIMIT" => {
                if let Some(limit) = parse_limit(&instruction.args)? {
                    resolved.limits.push(limit);
                }
            }
            "TIMEOUT" => {
                if let Some(timeout) = parse_timeout(&instruction.args)? {
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
            "SKILL" => {
                let source_path = scalar_at(&instruction.args, 0);
                process_skill_instruction(
                    &context_dir,
                    &source_path,
                    &mut packaged,
                    &mut files,
                    &mut resolved,
                )?;
            }
            "IDENTITY" | "SOUL" | "AGENTS" | "USER" | "TOOLS" | "EVAL" => {
                let source_path = scalar_at(&instruction.args, 0);
                let resolved_path = resolve_path(&context_dir, &source_path)?;
                let file_record = package_path(&context_dir, &resolved_path, &mut packaged)?;
                resolved.instructions.push(InstructionConfig {
                    kind: instruction_kind_from_keyword(&instruction.keyword),
                    packaged_path: source_path,
                    sha256: file_record.sha256.clone(),
                    skill_name: None,
                    allowed_tools: None,
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
                            skill_name: None,
                            allowed_tools: None,
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
                        skill_name: None,
                        allowed_tools: None,
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

    resolved.warnings.extend(skill_allowed_tool_build_warnings(
        &resolved.instructions,
        &resolved.skill_tool_aliases,
        &resolved.tools,
    ));

    files.sort_by(|left, right| left.packaged_as.cmp(&right.packaged_as));
    for pair in files.windows(2) {
        if pair[0].packaged_as == pair[1].packaged_as && pair[0].sha256 != pair[1].sha256 {
            return Err(BuildError::Validation(format!(
                "packaged file `{}` was recorded more than once with conflicting content hashes",
                pair[0].packaged_as
            )));
        }
    }
    files.dedup_by(|left, right| left.packaged_as == right.packaged_as);

    for tool in &resolved.tools {
        if let ToolConfig::A2a(tool) = tool
            && let Some(auth) = &tool.auth
        {
            for secret_name in a2a_auth_secret_names(auth) {
                if !resolved
                    .secrets
                    .iter()
                    .any(|secret| secret.name == secret_name)
                {
                    return Err(BuildError::Validation(format!(
                        "TOOL A2A `{}` references auth secret `{}` which is not declared via `SECRET`",
                        tool.alias, secret_name
                    )));
                }
            }
        }
    }

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
        compaction: resolved.compaction,
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
        compaction: provisional.compaction,
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
        warnings: resolved.warnings,
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
        compaction: parcel.compaction.clone(),
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
        if entry.file_type().is_symlink() {
            return Err(BuildError::Validation(format!(
                "packaged directory `{}` contains symlink `{}`; symlinks are not allowed in parcel content",
                resolved.display(),
                entry.path().display()
            )));
        }
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

fn process_skill_instruction(
    context_dir: &Path,
    source_path: &str,
    packaged: &mut BTreeMap<String, Vec<u8>>,
    files: &mut Vec<ParcelFileRecord>,
    resolved: &mut ResolvedAgentSpec,
) -> Result<(), BuildError> {
    let resolved_path = resolve_path(context_dir, source_path)?;
    if resolved_path.is_file() {
        let file_record = package_path(context_dir, &resolved_path, packaged)?;
        resolved.instructions.push(InstructionConfig {
            kind: InstructionKind::Skill,
            packaged_path: source_path.to_string(),
            sha256: file_record.sha256.clone(),
            skill_name: None,
            allowed_tools: None,
        });
        files.extend(file_record.expand());
        return Ok(());
    }

    let skill_dir = resolved_path;
    let skill_md_path = skill_dir.join("SKILL.md");
    if !skill_md_path.exists() {
        return Err(BuildError::Validation(format!(
            "SKILL directory `{source_path}` must contain `SKILL.md`"
        )));
    }

    let skill_source =
        fs::read_to_string(&skill_md_path).map_err(|source| BuildError::ReadFile {
            path: skill_md_path.display().to_string(),
            source,
        })?;
    let parsed_skill = parse_skill_markdown(&skill_source).map_err(|source| {
        BuildError::Validation(format!(
            "failed to parse Agent Skills frontmatter in `{}`: {source}",
            skill_md_path.display()
        ))
    })?;
    validate_agent_skill_frontmatter(&skill_dir, &parsed_skill.frontmatter)
        .map_err(BuildError::Validation)?;

    let bundle_record = package_path(context_dir, &skill_dir, packaged)?;
    let skill_md_packaged_path = relative_display(context_dir, &skill_md_path);
    let skill_file_record = bundle_record
        .entries
        .iter()
        .find(|entry| entry.packaged_as == skill_md_packaged_path)
        .cloned()
        .ok_or_else(|| {
            BuildError::Validation(format!(
                "packaged skill bundle `{}` did not include `{}`",
                skill_dir.display(),
                skill_md_packaged_path
            ))
        })?;
    files.extend(bundle_record.expand());

    resolved.instructions.push(InstructionConfig {
        kind: InstructionKind::Skill,
        packaged_path: skill_md_packaged_path.clone(),
        sha256: skill_file_record.sha256.clone(),
        skill_name: Some(parsed_skill.frontmatter.name.clone()),
        allowed_tools: parsed_skill.frontmatter.allowed_tools.clone(),
    });

    if let Some(sidecar) =
        resolve_skill_dispatch_manifest_path(&skill_dir, &parsed_skill.frontmatter)?
    {
        let sidecar_path = sidecar.path();
        let sidecar_source =
            fs::read_to_string(sidecar_path).map_err(|source| BuildError::ReadFile {
                path: sidecar_path.display().to_string(),
                source,
            })?;
        let skill_manifest: DispatchSkillManifest =
            toml::from_str(&sidecar_source).map_err(|source| {
                let mut message = format!(
                    "failed to parse Dispatch skill manifest `{}`: {source}",
                    sidecar_path.display()
                );
                if sidecar.is_auto_detected() {
                    message.push_str(
                        "; `dispatch.toml` is reserved for skill sidecars and is auto-detected inside skill directories. Rename the file or set `metadata.dispatch-manifest` to an explicit sidecar path."
                    );
                }
                BuildError::Validation(message)
            })?;

        if let Some(entrypoint) = skill_manifest.entrypoint.as_deref() {
            let entrypoint = validate_entrypoint_value(
                entrypoint,
                &format!("skill sidecar `{}` entrypoint", sidecar_path.display()),
            )?;
            if !resolved.entrypoint_declared {
                match resolved.entrypoint.as_deref() {
                    None => resolved.entrypoint = Some(entrypoint),
                    Some(existing) if existing == entrypoint => {}
                    Some(existing) => {
                        return Err(BuildError::Validation(format!(
                            "skill sidecar `{}` entrypoint `{}` conflicts with previously resolved entrypoint `{existing}`",
                            sidecar_path.display(),
                            entrypoint
                        )));
                    }
                }
            }
        }

        resolved
            .skill_tool_aliases
            .entry(parsed_skill.frontmatter.name.clone())
            .or_default()
            .extend(skill_manifest.tools.iter().map(|tool| tool.name.clone()));

        for skill_tool in skill_manifest.tools {
            let mut tool = synthesize_skill_tool(
                context_dir,
                &skill_dir,
                &parsed_skill.frontmatter.name,
                &skill_tool,
            )?;
            package_tool_config(context_dir, packaged, files, &mut tool)?;
            insert_resolved_tool(&mut resolved.tools, &mut resolved.warnings, tool)?;
        }
    }

    Ok(())
}

enum SkillDispatchManifestSource {
    Explicit(PathBuf),
    AutoDetected(PathBuf),
}

impl SkillDispatchManifestSource {
    fn path(&self) -> &Path {
        match self {
            Self::Explicit(path) | Self::AutoDetected(path) => path,
        }
    }

    fn is_auto_detected(&self) -> bool {
        matches!(self, Self::AutoDetected(_))
    }
}

fn resolve_skill_dispatch_manifest_path(
    skill_dir: &Path,
    frontmatter: &crate::skill::AgentSkillFrontmatter,
) -> Result<Option<SkillDispatchManifestSource>, BuildError> {
    if let Some(path) = dispatch_skill_manifest_path(frontmatter) {
        return Ok(Some(SkillDispatchManifestSource::Explicit(
            resolve_skill_member_path(skill_dir, path)?,
        )));
    }
    let default = skill_dir.join("dispatch.toml");
    if default.is_file() {
        return Ok(Some(SkillDispatchManifestSource::AutoDetected(
            resolve_skill_member_path(skill_dir, "dispatch.toml")?,
        )));
    }
    Ok(None)
}

fn resolve_skill_member_path(skill_dir: &Path, relative: &str) -> Result<PathBuf, BuildError> {
    debug_assert!(skill_dir.is_absolute());
    let joined = skill_dir.join(relative);
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
    if !resolved.starts_with(skill_dir) {
        return Err(BuildError::Validation(format!(
            "skill path `{relative}` escapes skill directory `{}`",
            skill_dir.display()
        )));
    }
    Ok(resolved)
}

fn synthesize_skill_tool(
    context_dir: &Path,
    skill_dir: &Path,
    skill_source: &str,
    skill_tool: &DispatchSkillTool,
) -> Result<ToolConfig, BuildError> {
    let resolved_script = resolve_skill_member_path(skill_dir, &skill_tool.script)?;
    let packaged_path = relative_display(context_dir, &resolved_script);
    let runner = match &skill_tool.runner {
        Some(runner) => CommandSpec {
            command: runner.clone(),
            args: skill_tool.args.clone(),
        },
        None => {
            let mut inferred = infer_runner(&packaged_path);
            inferred.args = skill_tool.args.clone();
            inferred
        }
    };

    let input_schema = if let Some(schema_path) = &skill_tool.schema {
        let resolved_schema = resolve_skill_member_path(skill_dir, schema_path)?;
        validate_tool_schema(&resolved_schema, &skill_tool.name)?;
        let schema_bytes = fs::read(&resolved_schema).map_err(|source| BuildError::ReadFile {
            path: resolved_schema.display().to_string(),
            source,
        })?;
        Some(ToolInputSchemaRef {
            packaged_path: relative_display(context_dir, &resolved_schema),
            sha256: hex_digest(&schema_bytes),
        })
    } else {
        None
    };

    Ok(ToolConfig::Local(LocalToolConfig {
        alias: skill_tool.name.clone(),
        packaged_path,
        runner,
        approval: skill_tool.approval,
        risk: skill_tool.risk,
        description: skill_tool.description.clone(),
        input_schema,
        skill_source: Some(skill_source.to_string()),
    }))
}

fn package_tool_config(
    context_dir: &Path,
    packaged: &mut BTreeMap<String, Vec<u8>>,
    files: &mut Vec<ParcelFileRecord>,
    tool: &mut ToolConfig,
) -> Result<(), BuildError> {
    match tool {
        ToolConfig::Local(local) => {
            let resolved_path = resolve_path(context_dir, &local.packaged_path)?;
            let file_record = package_path(context_dir, &resolved_path, packaged)?;
            files.extend(file_record.expand());
            if let Some(schema) = &local.input_schema {
                let resolved_schema_path = resolve_path(context_dir, &schema.packaged_path)?;
                validate_tool_schema(&resolved_schema_path, &local.alias)?;
                let schema_record = package_path(context_dir, &resolved_schema_path, packaged)?;
                local.input_schema = Some(ToolInputSchemaRef {
                    packaged_path: schema.packaged_path.clone(),
                    sha256: schema_record.sha256.clone(),
                });
                files.extend(schema_record.expand());
            }
        }
        ToolConfig::A2a(tool) => {
            if let Some(schema) = &tool.input_schema {
                let resolved_schema_path = resolve_path(context_dir, &schema.packaged_path)?;
                validate_tool_schema(&resolved_schema_path, &tool.alias)?;
                let schema_record = package_path(context_dir, &resolved_schema_path, packaged)?;
                tool.input_schema = Some(ToolInputSchemaRef {
                    packaged_path: schema.packaged_path.clone(),
                    sha256: schema_record.sha256.clone(),
                });
                files.extend(schema_record.expand());
            }
        }
        ToolConfig::Builtin(_) | ToolConfig::Mcp(_) => {}
    }
    Ok(())
}

fn insert_resolved_tool(
    tools: &mut Vec<ToolConfig>,
    warnings: &mut Vec<String>,
    tool: ToolConfig,
) -> Result<(), BuildError> {
    let alias = tool_alias(&tool).map(ToOwned::to_owned);
    if let Some(alias) = alias
        && let Some(existing) = tools
            .iter_mut()
            .find(|existing| tool_alias(existing) == Some(alias.as_str()))
    {
        match (tool_skill_source(existing), tool_skill_source(&tool)) {
            (Some(previous_skill_source), Some(new_skill_source)) => {
                let message = if previous_skill_source == new_skill_source {
                    format!(
                        "tool `{alias}` is declared more than once by skill `{previous_skill_source}`"
                    )
                } else {
                    format!(
                        "tool `{alias}` is declared by multiple skills (`{previous_skill_source}` and `{new_skill_source}`)"
                    )
                };
                return Err(BuildError::Validation(message));
            }
            (Some(previous_skill_source), None) => {
                warnings.push(format!(
                    "tool `{alias}` from skill `{previous_skill_source}` overridden by an explicit Agentfile tool declaration"
                ));
                *existing = tool;
                return Ok(());
            }
            (None, Some(new_skill_source)) => {
                warnings.push(format!(
                    "tool `{alias}` from skill `{new_skill_source}` was shadowed by an explicit Agentfile tool declaration"
                ));
                return Ok(());
            }
            (None, None) => {
                return Err(BuildError::Validation(format!(
                    "tool `{alias}` is declared more than once in the Agentfile"
                )));
            }
        }
    }
    tools.push(tool);
    Ok(())
}

fn tool_alias(tool: &ToolConfig) -> Option<&str> {
    match tool {
        ToolConfig::Local(local) => Some(local.alias.as_str()),
        ToolConfig::A2a(tool) => Some(tool.alias.as_str()),
        ToolConfig::Builtin(_) | ToolConfig::Mcp(_) => None,
    }
}

fn tool_skill_source(tool: &ToolConfig) -> Option<&str> {
    match tool {
        ToolConfig::Local(local) => local.skill_source.as_deref(),
        ToolConfig::Builtin(_) | ToolConfig::Mcp(_) | ToolConfig::A2a(_) => None,
    }
}

fn skill_allowed_tool_build_warnings(
    instructions: &[InstructionConfig],
    skill_tool_aliases: &BTreeMap<String, Vec<String>>,
    tools: &[ToolConfig],
) -> Vec<String> {
    let parcel_tool_names = tools
        .iter()
        .filter_map(model_visible_tool_name)
        .map(ToOwned::to_owned)
        .collect::<std::collections::BTreeSet<_>>();

    instructions
        .iter()
        .filter_map(|instruction| {
            instruction
                .skill_name
                .as_ref()
                .map(|skill_name| (skill_name, instruction.allowed_tools.as_deref()))
        })
        .flat_map(|(skill_name, allowed_tools)| {
            let own_aliases = skill_tool_aliases
                .get(skill_name)
                .map(|aliases| aliases.as_slice())
                .unwrap_or(&[]);
            allowed_tool_warnings(skill_name, allowed_tools, own_aliases, &parcel_tool_names)
        })
        .collect()
}

fn model_visible_tool_name(tool: &ToolConfig) -> Option<&str> {
    match tool {
        ToolConfig::Local(local) => Some(local.alias.as_str()),
        ToolConfig::Builtin(builtin) => Some(builtin.capability.as_str()),
        ToolConfig::A2a(tool) => Some(tool.alias.as_str()),
        ToolConfig::Mcp(_) => None,
    }
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

fn validate_entrypoint_value(value: &str, context: &str) -> Result<String, BuildError> {
    match value {
        "chat" | "job" | "heartbeat" => Ok(value.to_string()),
        _ => Err(BuildError::Validation(format!(
            "{context} must be one of `chat`, `job`, or `heartbeat`, got `{value}`"
        ))),
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

fn parse_tool(args: &[Value]) -> Result<Option<ToolConfig>, BuildError> {
    let tokens = scalars(args);
    let Some(first) = tokens.first() else {
        return Ok(None);
    };
    match first.as_str() {
        "LOCAL" => parse_local_tool(&tokens).map(|tool| tool.map(ToolConfig::Local)),
        "BUILTIN" => tokens
            .get(1)
            .map(|capability| {
                Ok(ToolConfig::Builtin(BuiltinToolConfig {
                    capability: capability.clone(),
                    approval: parse_tool_approval(&tokens)?,
                    risk: parse_tool_risk(&tokens)?,
                    description: parse_named_value(&tokens, "DESCRIPTION"),
                }))
            })
            .transpose(),
        "MCP" => tokens
            .get(1)
            .map(|server| {
                Ok(ToolConfig::Mcp(McpToolConfig {
                    server: server.clone(),
                    approval: parse_tool_approval(&tokens)?,
                    risk: parse_tool_risk(&tokens)?,
                    description: parse_named_value(&tokens, "DESCRIPTION"),
                }))
            })
            .transpose(),
        "A2A" => parse_a2a_tool(&tokens).map(|tool| tool.map(ToolConfig::A2a)),
        _ => Ok(None),
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

fn parse_local_tool(tokens: &[String]) -> Result<Option<LocalToolConfig>, BuildError> {
    let Some(packaged_path) = tokens.get(1).cloned() else {
        return Ok(None);
    };
    let alias = parse_named_value(tokens, "AS").unwrap_or_else(|| {
        Path::new(&packaged_path)
            .file_stem()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| packaged_path.clone())
    });
    let approval = parse_tool_approval(tokens)?;
    let risk = parse_tool_risk(tokens)?;
    let description = parse_named_value(tokens, "DESCRIPTION");
    let input_schema =
        parse_named_value(tokens, "SCHEMA").map(|packaged_path| ToolInputSchemaRef {
            packaged_path,
            sha256: String::new(),
        });
    let runner = parse_tool_runner(tokens, &packaged_path);

    Ok(Some(LocalToolConfig {
        alias,
        packaged_path,
        runner,
        approval,
        risk,
        description,
        input_schema,
        skill_source: None,
    }))
}

fn parse_a2a_tool(tokens: &[String]) -> Result<Option<A2aToolConfig>, BuildError> {
    let Some(alias) = tokens.get(1).cloned() else {
        return Ok(None);
    };
    let url = parse_named_value(tokens, "URL").ok_or_else(|| {
        BuildError::Validation(format!("TOOL A2A `{alias}` requires `URL <endpoint>`"))
    })?;

    let input_schema =
        parse_named_value(tokens, "SCHEMA").map(|packaged_path| ToolInputSchemaRef {
            packaged_path,
            sha256: String::new(),
        });

    let endpoint_mode = parse_a2a_endpoint_mode(tokens)?;
    let expected_agent_name = parse_named_value(tokens, "EXPECT_AGENT_NAME");
    let expected_card_sha256 = parse_a2a_card_sha256(tokens)?;
    if matches!(endpoint_mode, Some(A2aEndpointMode::Direct))
        && (expected_agent_name.is_some() || expected_card_sha256.is_some())
    {
        return Err(BuildError::Validation(format!(
            "TOOL A2A `{alias}` cannot use `DISCOVERY direct` with `EXPECT_AGENT_NAME` or `EXPECT_CARD_SHA256`"
        )));
    }

    Ok(Some(A2aToolConfig {
        alias,
        url,
        endpoint_mode,
        auth: parse_a2a_auth(tokens)?,
        expected_agent_name,
        expected_card_sha256,
        approval: parse_tool_approval(tokens)?,
        risk: parse_tool_risk(tokens)?,
        description: parse_named_value(tokens, "DESCRIPTION"),
        input_schema,
    }))
}

fn parse_a2a_card_sha256(tokens: &[String]) -> Result<Option<String>, BuildError> {
    let Some(value) = parse_named_value(tokens, "EXPECT_CARD_SHA256") else {
        return Ok(None);
    };
    let valid = value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit());
    if !valid {
        return Err(BuildError::Validation(
            "EXPECT_CARD_SHA256 must be a 64-character lowercase or uppercase hex SHA256 digest"
                .to_string(),
        ));
    }
    Ok(Some(value.to_ascii_lowercase()))
}

fn parse_a2a_endpoint_mode(tokens: &[String]) -> Result<Option<A2aEndpointMode>, BuildError> {
    let Some(value) = parse_named_value(tokens, "DISCOVERY") else {
        return Ok(None);
    };
    match value.to_ascii_lowercase().as_str() {
        "auto" => Ok(Some(A2aEndpointMode::Auto)),
        "card" => Ok(Some(A2aEndpointMode::Card)),
        "direct" => Ok(Some(A2aEndpointMode::Direct)),
        other => Err(BuildError::Validation(format!(
            "invalid A2A discovery mode `{other}`; expected one of auto, card, direct"
        ))),
    }
}

fn parse_a2a_auth(tokens: &[String]) -> Result<Option<A2aAuthConfig>, BuildError> {
    let Some(auth_index) = tokens.iter().position(|token| token == "AUTH") else {
        return Ok(None);
    };
    let Some(scheme) = tokens.get(auth_index + 1) else {
        return Err(BuildError::Validation(
            "TOOL A2A `AUTH` requires `bearer <secret_name>`, `header <header_name> <secret_name>`, or `basic <username_secret_name> <password_secret_name>`".to_string(),
        ));
    };
    let auth = match scheme.to_ascii_lowercase().as_str() {
        "bearer" => {
            let Some(secret_name) = tokens.get(auth_index + 2) else {
                return Err(BuildError::Validation(
                    "TOOL A2A `AUTH bearer` requires `<secret_name>`".to_string(),
                ));
            };
            A2aAuthConfig::Bearer {
                secret_name: secret_name.clone(),
            }
        }
        "header" => {
            let Some(header_name) = tokens.get(auth_index + 2) else {
                return Err(BuildError::Validation(
                    "TOOL A2A `AUTH header` requires `<header_name> <secret_name>`".to_string(),
                ));
            };
            let Some(secret_name) = tokens.get(auth_index + 3) else {
                return Err(BuildError::Validation(
                    "TOOL A2A `AUTH header` requires `<header_name> <secret_name>`".to_string(),
                ));
            };
            validate_http_header_name(header_name)?;
            A2aAuthConfig::Header {
                header_name: header_name.clone(),
                secret_name: secret_name.clone(),
            }
        }
        "basic" => {
            let Some(username_secret_name) = tokens.get(auth_index + 2) else {
                return Err(BuildError::Validation(
                    "TOOL A2A `AUTH basic` requires `<username_secret_name> <password_secret_name>`".to_string(),
                ));
            };
            let Some(password_secret_name) = tokens.get(auth_index + 3) else {
                return Err(BuildError::Validation(
                    "TOOL A2A `AUTH basic` requires `<username_secret_name> <password_secret_name>`".to_string(),
                ));
            };
            A2aAuthConfig::Basic {
                username_secret_name: username_secret_name.clone(),
                password_secret_name: password_secret_name.clone(),
            }
        }
        other => {
            return Err(BuildError::Validation(format!(
                "invalid A2A auth scheme `{other}`; expected `bearer`, `header`, or `basic`"
            )));
        }
    };
    Ok(Some(auth))
}

fn a2a_auth_secret_names(auth: &A2aAuthConfig) -> Vec<&str> {
    match auth {
        A2aAuthConfig::Bearer { secret_name } => vec![secret_name.as_str()],
        A2aAuthConfig::Header { secret_name, .. } => vec![secret_name.as_str()],
        A2aAuthConfig::Basic {
            username_secret_name,
            password_secret_name,
        } => vec![username_secret_name.as_str(), password_secret_name.as_str()],
    }
}

fn validate_http_header_name(header_name: &str) -> Result<(), BuildError> {
    if header_name.is_empty()
        || !header_name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Err(BuildError::Validation(format!(
            "invalid A2A auth header name `{header_name}`; expected ASCII letters, digits, or `-`"
        )));
    }
    Ok(())
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
            if token == "AS"
                || token == "APPROVAL"
                || token == "RISK"
                || token == "DESCRIPTION"
                || token == "SCHEMA"
            {
                break;
            }
            args.push(token.clone());
            index += 1;
        }

        return CommandSpec { command, args };
    }

    infer_runner(packaged_path)
}

fn parse_tool_approval(tokens: &[String]) -> Result<Option<ToolApprovalPolicy>, BuildError> {
    let Some(value) = parse_named_value(tokens, "APPROVAL") else {
        return Ok(None);
    };
    match value.to_ascii_lowercase().as_str() {
        "never" => Ok(Some(ToolApprovalPolicy::Never)),
        "always" => Ok(Some(ToolApprovalPolicy::Always)),
        "confirm" | "required" => Ok(Some(ToolApprovalPolicy::Confirm)),
        "audit" => Ok(Some(ToolApprovalPolicy::Audit)),
        other => Err(BuildError::Validation(format!(
            "invalid tool approval policy `{other}`; expected one of never, always, confirm, audit"
        ))),
    }
}

fn parse_tool_risk(tokens: &[String]) -> Result<Option<ToolRiskLevel>, BuildError> {
    let Some(value) = parse_named_value(tokens, "RISK") else {
        return Ok(None);
    };
    match value.to_ascii_lowercase().as_str() {
        "low" => Ok(Some(ToolRiskLevel::Low)),
        "medium" => Ok(Some(ToolRiskLevel::Medium)),
        "high" => Ok(Some(ToolRiskLevel::High)),
        other => Err(BuildError::Validation(format!(
            "invalid tool risk level `{other}`; expected one of low, medium, high"
        ))),
    }
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

fn parse_limit(args: &[Value]) -> Result<Option<LimitSpec>, BuildError> {
    let tokens = scalars(args);
    if tokens.len() < 2 {
        return Ok(None);
    }
    let scope = tokens[0].clone();
    if !matches!(
        scope.as_str(),
        "ITERATIONS" | "TOOL_CALLS" | "TOOL_OUTPUT" | "CONTEXT_TOKENS"
    ) {
        return Err(BuildError::Validation(format!(
            "invalid limit scope `{scope}`; expected one of ITERATIONS, TOOL_CALLS, TOOL_OUTPUT, CONTEXT_TOKENS"
        )));
    }
    Ok(Some(LimitSpec {
        scope,
        value: tokens[1].clone(),
        qualifiers: tokens[2..].to_vec(),
    }))
}

fn parse_compaction(args: &[Value]) -> Option<CompactionConfig> {
    let tokens = scalars(args);
    let interval = tokens.first()?.clone();
    let overlap = tokens
        .windows(2)
        .find(|window| window[0] == "OVERLAP")
        .and_then(|window| window[1].parse::<u32>().ok());
    Some(CompactionConfig { interval, overlap })
}

fn parse_timeout(args: &[Value]) -> Result<Option<TimeoutSpec>, BuildError> {
    let tokens = scalars(args);
    if tokens.len() < 2 {
        return Ok(None);
    }
    if !timeout_duration_is_valid(&tokens[1]) {
        return Err(BuildError::Validation(format!(
            "invalid timeout duration `{}`; expected a positive integer ending in ms, s, m, or h",
            tokens[1]
        )));
    }
    Ok(Some(TimeoutSpec {
        scope: tokens[0].clone(),
        duration: tokens[1].clone(),
        qualifiers: tokens[2..].to_vec(),
    }))
}

fn timeout_duration_is_valid(raw: &str) -> bool {
    let trimmed = raw.trim();
    let value = if let Some(value) = trimmed.strip_suffix("ms") {
        value
    } else if let Some(value) = trimmed.strip_suffix('s') {
        value
    } else if let Some(value) = trimmed.strip_suffix('m') {
        value
    } else if let Some(value) = trimmed.strip_suffix('h') {
        value
    } else {
        return false;
    };
    value.trim().parse::<u64>().is_ok()
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
    use crate::A2aAuthScheme;
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
TOOL LOCAL tools/demo.py AS demo USING python3 -u RISK low DESCRIPTION \"Look up a record by id and print JSON.\"
TOOL BUILTIN web_search APPROVAL required RISK medium DESCRIPTION \"Search the web for live information.\"
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
                assert_eq!(local.risk, Some(ToolRiskLevel::Low));
                assert_eq!(
                    local.description.as_deref(),
                    Some("Look up a record by id and print JSON.")
                );
            }
            other => panic!("expected local tool, got {other:?}"),
        }
        match &parcel.tools[1] {
            ToolConfig::Builtin(tool) => {
                assert_eq!(tool.approval, Some(ToolApprovalPolicy::Confirm));
                assert_eq!(tool.risk, Some(ToolRiskLevel::Medium));
                assert_eq!(
                    tool.description.as_deref(),
                    Some("Search the web for live information.")
                );
            }
            other => panic!("expected builtin tool, got {other:?}"),
        }
    }

    #[test]
    fn build_supports_agent_skill_directory_bundles() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::create_dir_all(skill_dir.join("schemas")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: file-analyst\ndescription: Analyze files.\nlicense: MIT\nmetadata:\n  dispatch-manifest: dispatch.toml\n---\nUse the bundled tools.\n",
        )
        .unwrap();
        fs::write(
            skill_dir.join("dispatch.toml"),
            "[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\nrisk = \"low\"\ndescription = \"Read a file\"\n\n[[tools]]\nname = \"find_files\"\nscript = \"scripts/find_files.sh\"\nschema = \"schemas/find_files.json\"\napproval = \"confirm\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("scripts/read_file.sh"), "cat \"$1\"\n").unwrap();
        fs::write(skill_dir.join("scripts/find_files.sh"), "echo ok\n").unwrap();
        fs::write(
            skill_dir.join("schemas/find_files.json"),
            "{\n  \"type\": \"object\",\n  \"properties\": {\n    \"pattern\": { \"type\": \"string\" }\n  },\n  \"required\": [\"pattern\"]\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "FROM dispatch/native:latest\nSKILL file-analyst\nENTRYPOINT chat\n",
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
        assert_eq!(parcel.instructions.len(), 1);
        assert_eq!(
            parcel.instructions[0].packaged_path,
            "file-analyst/SKILL.md"
        );
        assert_eq!(
            parcel.instructions[0].skill_name.as_deref(),
            Some("file-analyst")
        );
        assert_eq!(parcel.instructions[0].allowed_tools, None);
        assert_eq!(parcel.tools.len(), 2);
        match &parcel.tools[0] {
            ToolConfig::Local(local) => {
                assert_eq!(local.alias, "read_file");
                assert_eq!(local.packaged_path, "file-analyst/scripts/read_file.sh");
                assert_eq!(local.risk, Some(ToolRiskLevel::Low));
                assert_eq!(local.skill_source.as_deref(), Some("file-analyst"));
            }
            other => panic!("expected local tool, got {other:?}"),
        }
        match &parcel.tools[1] {
            ToolConfig::Local(local) => {
                assert_eq!(local.alias, "find_files");
                assert_eq!(
                    local
                        .input_schema
                        .as_ref()
                        .map(|schema| schema.packaged_path.as_str()),
                    Some("file-analyst/schemas/find_files.json")
                );
                assert_eq!(local.approval, Some(ToolApprovalPolicy::Confirm));
                assert_eq!(local.skill_source.as_deref(), Some("file-analyst"));
            }
            other => panic!("expected local tool, got {other:?}"),
        }
    }

    #[test]
    fn build_skill_directory_records_allowed_tools_metadata() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: file-analyst\ndescription: Analyze files.\nallowed-tools:\n  - Bash\n  - Grep\n---\nUse the bundled tools.\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "FROM dispatch/native:latest\nSKILL file-analyst\nENTRYPOINT chat\n",
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
        assert_eq!(
            parcel.instructions[0].allowed_tools.as_deref(),
            Some(&["Bash".to_string(), "Grep".to_string()][..])
        );
    }

    #[test]
    fn build_warns_on_skill_allowed_tools_mismatches() {
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
        fs::write(
            dir.path().join("Agentfile"),
            "FROM dispatch/native:latest\nSKILL file-analyst\nENTRYPOINT chat\n",
        )
        .unwrap();

        let built = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

        assert_eq!(
            built.warnings,
            vec![
                "skill `file-analyst` declares allowed-tools entry `Bash` but no tool with that name exists in the built parcel"
                    .to_string(),
                "skill `file-analyst` synthesizes tool `read_file` but its allowed-tools list does not include that alias"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn build_skill_directory_autodetects_dispatch_sidecar_and_sets_entrypoint_default() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: file-analyst\ndescription: Analyze files.\n---\nUse the bundled tools.\n",
        )
        .unwrap();
        fs::write(
            skill_dir.join("dispatch.toml"),
            "entrypoint = \"job\"\n\n[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("scripts/read_file.sh"), "cat \"$1\"\n").unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "FROM dispatch/native:latest\nSKILL file-analyst\n",
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
        assert_eq!(parcel.entrypoint.as_deref(), Some("job"));
        assert_eq!(parcel.tools.len(), 1);
    }

    #[test]
    fn build_agentfile_entrypoint_overrides_skill_sidecar_entrypoint() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: file-analyst\ndescription: Analyze files.\n---\nUse the bundled tools.\n",
        )
        .unwrap();
        fs::write(
            skill_dir.join("dispatch.toml"),
            "entrypoint = \"job\"\n\n[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("scripts/read_file.sh"), "cat \"$1\"\n").unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "FROM dispatch/native:latest\nSKILL file-analyst\nENTRYPOINT chat\n",
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
        assert_eq!(parcel.entrypoint.as_deref(), Some("chat"));
    }

    #[test]
    fn build_rejects_conflicting_skill_sidecar_entrypoints() {
        let dir = tempdir().unwrap();
        for (name, entrypoint) in [("file-analyst", "job"), ("web-search", "heartbeat")] {
            let skill_dir = dir.path().join(name);
            fs::create_dir_all(skill_dir.join("scripts")).unwrap();
            fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: Skill.\n---\nBody\n"),
            )
            .unwrap();
            fs::write(
                skill_dir.join("dispatch.toml"),
                format!(
                    "entrypoint = \"{entrypoint}\"\n\n[[tools]]\nname = \"{name}_tool\"\nscript = \"scripts/run.sh\"\n"
                ),
            )
            .unwrap();
            fs::write(skill_dir.join("scripts/run.sh"), "echo ok\n").unwrap();
        }
        fs::write(
            dir.path().join("Agentfile"),
            "FROM dispatch/native:latest\nSKILL file-analyst\nSKILL web-search\n",
        )
        .unwrap();

        let error = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("conflicts with previously resolved entrypoint")
        );
    }

    #[test]
    fn build_rejects_conflicting_skill_tool_aliases() {
        let dir = tempdir().unwrap();
        for name in ["file-analyst", "web-search"] {
            let skill_dir = dir.path().join(name);
            fs::create_dir_all(skill_dir.join("scripts")).unwrap();
            fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: Skill.\n---\nBody\n"),
            )
            .unwrap();
            fs::write(
                skill_dir.join("dispatch.toml"),
                "[[tools]]\nname = \"shared\"\nscript = \"scripts/run.sh\"\n",
            )
            .unwrap();
            fs::write(skill_dir.join("scripts/run.sh"), "echo ok\n").unwrap();
        }
        fs::write(
            dir.path().join("Agentfile"),
            "FROM dispatch/native:latest\nSKILL file-analyst\nSKILL web-search\nENTRYPOINT chat\n",
        )
        .unwrap();

        let error = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("declared by multiple skills"));
    }

    #[test]
    fn build_explicit_tool_overrides_skill_generated_alias() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: file-analyst\ndescription: Analyze files.\n---\nUse the bundled tools.\n",
        )
        .unwrap();
        fs::write(
            skill_dir.join("dispatch.toml"),
            "[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\nrisk = \"low\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("scripts/read_file.sh"), "cat \"$1\"\n").unwrap();
        fs::create_dir_all(dir.path().join("tools")).unwrap();
        fs::write(dir.path().join("tools/read_file.py"), "print('override')\n").unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "FROM dispatch/native:latest\nSKILL file-analyst\nTOOL LOCAL tools/read_file.py AS read_file RISK high\nENTRYPOINT chat\n",
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
        assert_eq!(parcel.tools.len(), 1);
        match &parcel.tools[0] {
            ToolConfig::Local(local) => {
                assert_eq!(local.alias, "read_file");
                assert_eq!(local.packaged_path, "tools/read_file.py");
                assert_eq!(local.risk, Some(ToolRiskLevel::High));
                assert_eq!(local.skill_source, None);
            }
            other => panic!("expected local tool, got {other:?}"),
        }
        assert_eq!(
            built.warnings,
            vec![
                "tool `read_file` from skill `file-analyst` overridden by an explicit Agentfile tool declaration"
                    .to_string()
            ]
        );
    }

    #[test]
    fn build_explicit_tool_declared_before_skill_still_wins() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: file-analyst\ndescription: Analyze files.\n---\nUse the bundled tools.\n",
        )
        .unwrap();
        fs::write(
            skill_dir.join("dispatch.toml"),
            "[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\nrisk = \"low\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("scripts/read_file.sh"), "cat \"$1\"\n").unwrap();
        fs::create_dir_all(dir.path().join("tools")).unwrap();
        fs::write(dir.path().join("tools/read_file.py"), "print('override')\n").unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "FROM dispatch/native:latest\nTOOL LOCAL tools/read_file.py AS read_file RISK high\nSKILL file-analyst\nENTRYPOINT chat\n",
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
        match &parcel.tools[0] {
            ToolConfig::Local(local) => {
                assert_eq!(local.alias, "read_file");
                assert_eq!(local.packaged_path, "tools/read_file.py");
                assert_eq!(local.risk, Some(ToolRiskLevel::High));
                assert_eq!(local.skill_source, None);
            }
            other => panic!("expected local tool, got {other:?}"),
        }
        assert_eq!(
            built.warnings,
            vec![
                "tool `read_file` from skill `file-analyst` was shadowed by an explicit Agentfile tool declaration"
                    .to_string()
            ]
        );
    }

    #[test]
    fn build_rejects_duplicate_explicit_tool_aliases() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("tools")).unwrap();
        fs::write(dir.path().join("tools/read_file.py"), "print('one')\n").unwrap();
        fs::write(dir.path().join("tools/read_file.sh"), "echo two\n").unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "FROM dispatch/native:latest\nTOOL LOCAL tools/read_file.py AS read_file\nTOOL LOCAL tools/read_file.sh AS read_file\nENTRYPOINT chat\n",
        )
        .unwrap();

        let error = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("tool `read_file` is declared more than once in the Agentfile")
        );
    }

    #[test]
    fn build_rejects_duplicate_tool_aliases_within_single_skill_sidecar() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: file-analyst\ndescription: Analyze files.\n---\nUse the bundled tools.\n",
        )
        .unwrap();
        fs::write(
            skill_dir.join("dispatch.toml"),
            "[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\n[[tools]]\nname = \"read_file\"\nscript = \"scripts/other.sh\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("scripts/read_file.sh"), "cat \"$1\"\n").unwrap();
        fs::write(skill_dir.join("scripts/other.sh"), "echo other\n").unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "FROM dispatch/native:latest\nSKILL file-analyst\nENTRYPOINT chat\n",
        )
        .unwrap();

        let error = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("tool `read_file` is declared more than once by skill `file-analyst`")
        );
    }

    #[test]
    fn build_reports_reserved_dispatch_toml_on_autodetect_parse_failure() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: file-analyst\ndescription: Analyze files.\n---\nUse the bundled tools.\n",
        )
        .unwrap();
        fs::write(skill_dir.join("dispatch.toml"), "this is not toml\n").unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "FROM dispatch/native:latest\nSKILL file-analyst\nENTRYPOINT chat\n",
        )
        .unwrap();

        let error = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("failed to parse Dispatch skill manifest"));
        assert!(message.contains("`dispatch.toml` is reserved for skill sidecars"));
        assert!(message.contains("metadata.dispatch-manifest"));
    }

    #[test]
    fn build_deduplicates_file_records_for_skill_and_explicit_tool_overlap() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: file-analyst\ndescription: Analyze files.\n---\nUse the bundled tools.\n",
        )
        .unwrap();
        fs::write(
            skill_dir.join("dispatch.toml"),
            "[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("scripts/read_file.sh"), "cat \"$1\"\n").unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "FROM dispatch/native:latest\nSKILL file-analyst\nTOOL LOCAL file-analyst/scripts/read_file.sh AS read_file_override\nENTRYPOINT chat\n",
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
        let read_file_records = parcel
            .files
            .iter()
            .filter(|file| file.packaged_as == "file-analyst/scripts/read_file.sh")
            .count();
        assert_eq!(read_file_records, 1);
    }

    #[test]
    fn build_rejects_skill_directory_with_mismatched_agent_skill_name() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: wrong-name\ndescription: Analyze files.\n---\nBody\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "FROM dispatch/native:latest\nSKILL file-analyst\nENTRYPOINT chat\n",
        )
        .unwrap();

        let error = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("must match skill directory"));
    }

    #[test]
    fn build_rejects_invalid_skill_sidecar_entrypoint() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: file-analyst\ndescription: Analyze files.\n---\nUse the bundled tools.\n",
        )
        .unwrap();
        fs::write(
            skill_dir.join("dispatch.toml"),
            "entrypoint = \"unsupported\"\n\n[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("scripts/read_file.sh"), "cat \"$1\"\n").unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "FROM dispatch/native:latest\nSKILL file-analyst\n",
        )
        .unwrap();

        let error = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("must be one of `chat`, `job`, or `heartbeat`")
        );
    }

    #[test]
    #[cfg(unix)]
    fn build_rejects_skill_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: file-analyst\ndescription: Analyze files.\n---\nUse the bundled tools.\n",
        )
        .unwrap();
        fs::write(dir.path().join("outside.txt"), "secret\n").unwrap();
        symlink(
            dir.path().join("outside.txt"),
            skill_dir.join("scripts/exfil"),
        )
        .unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "FROM dispatch/native:latest\nSKILL file-analyst\nENTRYPOINT chat\n",
        )
        .unwrap();

        let error = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("symlinks are not allowed"));
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
    fn build_records_compaction_config() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
COMPACTION 200 OVERLAP 32
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

        let compaction = parcel.compaction.expect("expected compaction config");
        assert_eq!(compaction.interval, "200");
        assert_eq!(compaction.overlap, Some(32));
    }

    #[test]
    fn build_rejects_invalid_tool_approval_policy() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
TOOL BUILTIN web_search APPROVAL bogus
ENTRYPOINT chat
",
        )
        .unwrap();

        let error = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid tool approval policy"));
    }

    #[test]
    fn build_rejects_invalid_tool_risk_level() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
TOOL BUILTIN web_search RISK dangerous
ENTRYPOINT chat
",
        )
        .unwrap();

        let error = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid tool risk level"));
    }

    #[test]
    fn build_rejects_invalid_limit_scope() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
LIMIT TOOL_CALL 5
ENTRYPOINT chat
",
        )
        .unwrap();

        let error = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid limit scope"));
    }

    #[test]
    fn build_rejects_invalid_timeout_duration() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
TIMEOUT TOOL sixty
ENTRYPOINT chat
",
        )
        .unwrap();

        let error = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid timeout duration"));
    }

    #[test]
    fn build_records_a2a_tool_metadata() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("schemas")).unwrap();
        fs::write(
            dir.path().join("schemas/a2a-input.json"),
            "{\n  \"type\": \"object\",\n  \"properties\": {\n    \"query\": { \"type\": \"string\" }\n  },\n  \"required\": [\"query\"]\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
SECRET A2A_TOKEN
TOOL A2A broker_agent URL https://broker.example.com DISCOVERY card AUTH bearer A2A_TOKEN EXPECT_AGENT_NAME remote-broker EXPECT_CARD_SHA256 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa APPROVAL confirm RISK medium SCHEMA schemas/a2a-input.json DESCRIPTION \"Delegate to a remote broker\"
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
            serde_json::from_slice(&fs::read(&built.manifest_path).unwrap()).unwrap();
        match &parcel.tools[0] {
            ToolConfig::A2a(tool) => {
                assert_eq!(tool.alias, "broker_agent");
                assert_eq!(tool.url, "https://broker.example.com");
                assert_eq!(tool.endpoint_mode, Some(A2aEndpointMode::Card));
                assert_eq!(tool.expected_agent_name.as_deref(), Some("remote-broker"));
                assert_eq!(
                    tool.expected_card_sha256.as_deref(),
                    Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                );
                let auth = tool.auth.as_ref().expect("expected auth config");
                assert_eq!(auth.scheme(), A2aAuthScheme::Bearer);
                assert!(matches!(
                    auth,
                    A2aAuthConfig::Bearer { secret_name } if secret_name == "A2A_TOKEN"
                ));
                assert_eq!(tool.approval, Some(ToolApprovalPolicy::Confirm));
                assert_eq!(tool.risk, Some(ToolRiskLevel::Medium));
                assert_eq!(
                    tool.description.as_deref(),
                    Some("Delegate to a remote broker")
                );
                let schema = tool
                    .input_schema
                    .as_ref()
                    .expect("expected packaged a2a input schema");
                assert_eq!(schema.packaged_path, "schemas/a2a-input.json");
                assert_eq!(schema.sha256.len(), 64);
            }
            other => panic!("expected a2a tool, got {other:?}"),
        }
    }

    #[test]
    fn build_rejects_a2a_tool_auth_secret_without_secret_declaration() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
TOOL A2A broker URL https://broker.example.com AUTH bearer MISSING_TOKEN
ENTRYPOINT chat
",
        )
        .unwrap();

        let error = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("references auth secret `MISSING_TOKEN`")
        );
    }

    #[test]
    fn build_rejects_invalid_a2a_card_sha256() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
TOOL A2A broker URL https://broker.example.com EXPECT_CARD_SHA256 not-a-digest
ENTRYPOINT chat
",
        )
        .unwrap();

        let error = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("EXPECT_CARD_SHA256"));
    }

    #[test]
    fn build_rejects_direct_a2a_with_identity_requirements() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
TOOL A2A broker URL https://broker.example.com DISCOVERY direct EXPECT_AGENT_NAME planner-agent
ENTRYPOINT chat
",
        )
        .unwrap();

        let error = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("cannot use `DISCOVERY direct`"));
    }

    #[test]
    fn build_parses_a2a_header_auth() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
SECRET API_KEY
TOOL A2A broker URL https://broker.example.com AUTH header X-Api-Key API_KEY
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
            serde_json::from_slice(&fs::read(&built.manifest_path).unwrap()).unwrap();
        match &parcel.tools[0] {
            ToolConfig::A2a(tool) => {
                let auth = tool.auth.as_ref().expect("expected auth config");
                assert_eq!(auth.scheme(), A2aAuthScheme::Header);
                assert!(matches!(
                    auth,
                    A2aAuthConfig::Header {
                        header_name,
                        secret_name,
                    } if header_name == "X-Api-Key" && secret_name == "API_KEY"
                ));
            }
            other => panic!("expected a2a tool, got {other:?}"),
        }
    }

    #[test]
    fn build_parses_a2a_basic_auth() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
SECRET A2A_USER
SECRET A2A_PASSWORD
TOOL A2A broker URL https://broker.example.com AUTH basic A2A_USER A2A_PASSWORD
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
            serde_json::from_slice(&fs::read(&built.manifest_path).unwrap()).unwrap();
        match &parcel.tools[0] {
            ToolConfig::A2a(tool) => {
                let auth = tool.auth.as_ref().expect("expected auth config");
                assert_eq!(auth.scheme(), A2aAuthScheme::Basic);
                assert!(matches!(
                    auth,
                    A2aAuthConfig::Basic {
                        username_secret_name,
                        password_secret_name,
                    } if username_secret_name == "A2A_USER"
                        && password_secret_name == "A2A_PASSWORD"
                ));
            }
            other => panic!("expected a2a tool, got {other:?}"),
        }
    }

    #[test]
    fn build_rejects_invalid_a2a_header_name() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Agentfile"),
            "\
FROM dispatch/native:latest
SECRET API_KEY
TOOL A2A broker URL https://broker.example.com AUTH header Bad:Header API_KEY
ENTRYPOINT chat
",
        )
        .unwrap();

        let error = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid A2A auth header name"));
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
