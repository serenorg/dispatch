use crate::{
    agent_config::load_agent_config,
    manifest::{
        CompactionConfig, CourierTarget, EnvVar, FrameworkProvenance, IngressPolicyConfig,
        InstructionConfig, LimitSpec, ModelPolicy, MountConfig, NetworkRule, PARCEL_FORMAT_VERSION,
        PARCEL_SCHEMA_URL, ParcelFileRecord, ParcelManifest, SecretSpec, TestSpec, TimeoutSpec,
        ToolConfig, Visibility,
    },
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

mod component;
mod lower;
mod parsing;
mod skill;
mod verify;

use lower::lower_agent_config;
use parsing::{
    a2a_auth_secret_names, validate_courier_requirements, validate_heartbeat_entrypoint,
    validate_test_specs,
};
use skill::skill_allowed_tool_build_warnings;
pub use verify::{ParcelLock, VerificationReport, verify_parcel};

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
    #[error(transparent)]
    AgentConfig(#[from] crate::agent_config::AgentConfigError),
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
    source: String,
    courier: CourierTarget,
    framework: Option<FrameworkProvenance>,
    name: Option<String>,
    version: Option<String>,
    entrypoint: Option<String>,
    schedules: Vec<String>,
    listeners: Vec<String>,
    ingress: Option<IngressPolicyConfig>,
    instructions: Vec<InstructionConfig>,
    inline_prompts: Vec<String>,
    env: Vec<EnvVar>,
    secrets: Vec<SecretSpec>,
    visibility: Option<Visibility>,
    mounts: Vec<MountConfig>,
    tools: Vec<ToolConfig>,
    tests: Vec<TestSpec>,
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
    schedules: Vec<String>,
    listeners: Vec<String>,
    ingress_path: Option<String>,
    ingress_methods: Vec<String>,
    ingress_secret_env: Option<String>,
    ingress_max_body_bytes: Option<usize>,
    ingress_max_header_bytes: Option<usize>,
    instructions: Vec<InstructionConfig>,
    inline_prompts: Vec<String>,
    env: Vec<EnvVar>,
    secrets: Vec<SecretSpec>,
    visibility: Option<Visibility>,
    mounts: Vec<MountConfig>,
    tools: Vec<ToolConfig>,
    tests: Vec<TestSpec>,
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

pub fn build_agent(config_path: &Path, options: &BuildOptions) -> Result<BuiltParcel, BuildError> {
    let config_path = config_path
        .canonicalize()
        .map_err(|source| BuildError::ReadFile {
            path: config_path.display().to_string(),
            source,
        })?;
    let context_dir =
        config_path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| BuildError::MissingPath {
                path: config_path.display().to_string(),
            })?;

    let config = load_agent_config(&config_path)?;

    let mut packaged = BTreeMap::<String, Vec<u8>>::new();
    let mut files = Vec::new();
    let mut resolved = ResolvedAgentSpec::default();

    lower_agent_config(
        &config,
        &context_dir,
        &config_path,
        &mut packaged,
        &mut files,
        &mut resolved,
    )?;

    resolved.warnings.extend(skill_allowed_tool_build_warnings(
        &resolved.instructions,
        &resolved.skill_tool_aliases,
        &resolved.tools,
    ));
    validate_test_specs(&resolved.tests, &resolved.tools)?;
    validate_heartbeat_entrypoint(resolved.entrypoint.as_deref(), &resolved.instructions)?;

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
                        "`agent.tools` A2A tool `{}` references auth secret `{}` which is not declared in `agent.secrets`",
                        tool.alias, secret_name
                    )));
                }
            }
        }
    }

    if let Some(secret_name) = &resolved.ingress_secret_env
        && !resolved
            .secrets
            .iter()
            .any(|secret| secret.name == *secret_name)
    {
        return Err(BuildError::Validation(format!(
            "`agent.ingress.secret_env` value `{secret_name}` is not declared in `agent.secrets`"
        )));
    }

    let ingress = resolved_ingress_policy(&resolved);

    let provisional = ProvisionalParcelManifest {
        schema: PARCEL_SCHEMA_URL.to_string(),
        format_version: PARCEL_FORMAT_VERSION,
        source: relative_display(&context_dir, &config_path),
        courier: resolved.courier.ok_or_else(|| {
            BuildError::Validation("missing required `agent.courier_reference`".to_string())
        })?,
        framework: resolved.framework,
        name: resolved.name,
        version: resolved.version,
        entrypoint: resolved.entrypoint,
        schedules: resolved.schedules,
        listeners: resolved.listeners,
        ingress,
        instructions: resolved.instructions,
        inline_prompts: resolved.inline_prompts,
        env: resolved.env,
        secrets: resolved.secrets,
        visibility: resolved.visibility,
        mounts: resolved.mounts,
        tools: resolved.tools,
        tests: resolved.tests,
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
        source: provisional.source,
        courier: provisional.courier,
        framework: provisional.framework,
        name: provisional.name,
        version: provisional.version,
        entrypoint: provisional.entrypoint,
        schedules: provisional.schedules,
        listeners: provisional.listeners,
        ingress: provisional.ingress,
        instructions: provisional.instructions,
        inline_prompts: provisional.inline_prompts,
        env: provisional.env,
        secrets: provisional.secrets,
        visibility: provisional.visibility,
        mounts: provisional.mounts,
        tools: provisional.tools,
        tests: provisional.tests,
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

fn provisional_digest(parcel: &ParcelManifest) -> Result<String, BuildError> {
    let provisional = ProvisionalParcelManifest {
        schema: parcel.schema.clone(),
        format_version: parcel.format_version,
        source: parcel.source.clone(),
        courier: parcel.courier.clone(),
        framework: parcel.framework.clone(),
        name: parcel.name.clone(),
        version: parcel.version.clone(),
        entrypoint: parcel.entrypoint.clone(),
        schedules: parcel.schedules.clone(),
        listeners: parcel.listeners.clone(),
        ingress: parcel.ingress.clone(),
        instructions: parcel.instructions.clone(),
        inline_prompts: parcel.inline_prompts.clone(),
        env: parcel.env.clone(),
        secrets: parcel.secrets.clone(),
        visibility: parcel.visibility,
        mounts: parcel.mounts.clone(),
        tools: parcel.tools.clone(),
        tests: parcel.tests.clone(),
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

fn resolved_ingress_policy(resolved: &ResolvedAgentSpec) -> Option<IngressPolicyConfig> {
    let has_ingress = resolved.ingress_path.is_some()
        || !resolved.ingress_methods.is_empty()
        || resolved.ingress_secret_env.is_some()
        || resolved.ingress_max_body_bytes.is_some()
        || resolved.ingress_max_header_bytes.is_some();
    if !has_ingress {
        return None;
    }
    Some(IngressPolicyConfig {
        path: resolved.ingress_path.clone(),
        methods: resolved.ingress_methods.clone(),
        shared_secret_env: resolved.ingress_secret_env.clone(),
        max_body_bytes: resolved.ingress_max_body_bytes,
        max_header_bytes: resolved.ingress_max_header_bytes,
    })
}

fn package_path(
    context_dir: &Path,
    config_path: &Path,
    resolved: &Path,
    packaged: &mut BTreeMap<String, Vec<u8>>,
) -> Result<PackagedPath, BuildError> {
    if resolved.is_file() {
        if resolved == config_path {
            return Err(BuildError::Validation(format!(
                "source config `{}` cannot be packaged into the parcel context",
                relative_display(context_dir, config_path)
            )));
        }
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
        if path == config_path {
            continue;
        }
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

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_config::AgentConfigError;
    use crate::{
        A2aAuthConfig, A2aAuthScheme, A2aEndpointMode, DISPATCH_WASM_ABI, InstructionKind,
        ToolApprovalPolicy, ToolRiskLevel,
    };
    use tempfile::tempdir;

    #[test]
    fn build_emits_typed_manifest() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nname = \"demo\"\nversion = \"1.0.0\"\nentrypoint = \"chat\"\n\n[agent.framework]\nname = \"adk-rust\"\nversion = \"0.5.0\"\ntarget = \"wasm\"\n\n[agent.instructions]\nidentity = \"IDENTITY.md\"\nsoul = \"SOUL.md\"\nskill = \"SKILL.md\"\nagents = \"AGENTS.md\"\nuser = \"USER.md\"\ntools = \"TOOLS.md\"\n\n[agent.model]\nid = \"claude-sonnet-4\"\nprovider = \"anthropic\"\n\n[[agent.model.fallbacks]]\nid = \"gpt-5-nano\"\nprovider = \"openai\"\n\n[agent.env]\n\"TZ\" = \"UTC\"\n\n[agent.labels]\n\"org.example.team\" = \"platform\"\n\n[[agent.secrets]]\nname = \"OPENAI_API_KEY\"\n\n[[agent.mounts]]\nkind = \"session\"\ndriver = \"sqlite\"\n\n[[agent.tools]]\nkind = \"local\"\npath = \"tools/demo.py\"\nalias = \"demo\"\nrunner = { command = \"python3\", args = [\"-u\"] }\nrisk = \"low\"\ndescription = \"Look up a record by id and print JSON.\"\n\n[[agent.tools]]\nkind = \"builtin\"\nname = \"web_search\"\napproval = \"confirm\"\nrisk = \"medium\"\ndescription = \"Search the web for live information.\"\n\n[[agent.network]]\naction = \"allow\"\ntarget = \"api.example.com\"\n",
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

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
        assert!(parcel.tests.is_empty());
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
    fn build_preserves_model_policy_without_a_primary_model() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"native\"\nentrypoint = \"chat\"\n\n[agent.model]\nrouting = \"balanced\"\n\n[[agent.model.fallbacks]]\nid = \"gpt-5-mini\"\nprovider = \"openai\"\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join("parcels"),
            },
        )
        .unwrap();
        let parcel: ParcelManifest =
            serde_json::from_slice(&fs::read(built.manifest_path).unwrap()).unwrap();

        assert!(parcel.models.primary.is_none());
        assert_eq!(parcel.models.routing.as_deref(), Some("balanced"));
        assert_eq!(parcel.models.fallbacks[0].id, "gpt-5-mini");
    }

    #[test]
    fn build_orders_skill_bundles_with_prompt_instruction_files() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("bundle")).unwrap();
        fs::write(
            dir.path().join("bundle/SKILL.md"),
            "---\nname: bundle\ndescription: Bundled skill.\n---\nbundle",
        )
        .unwrap();
        for (path, contents) in [
            ("IDENTITY.md", "identity"),
            ("SOUL.md", "soul"),
            ("SKILL.md", "skill"),
            ("AGENTS.md", "agents"),
            ("USER.md", "user"),
            ("TOOLS.md", "tools"),
            ("MEMORY.md", "memory"),
            ("HEARTBEAT.md", "heartbeat"),
            ("evals/smoke.eval", "name = \"smoke\"\ninput = \"ok\"\n"),
        ] {
            let path = dir.path().join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"native\"\nentrypoint = \"heartbeat\"\nskills = [\"bundle\"]\nevals = [\"evals/smoke.eval\"]\n\n[agent.instructions]\nidentity = \"IDENTITY.md\"\nsoul = \"SOUL.md\"\nskill = \"SKILL.md\"\nagents = \"AGENTS.md\"\nuser = \"USER.md\"\ntools = \"TOOLS.md\"\nmemory = \"MEMORY.md\"\nheartbeat = \"HEARTBEAT.md\"\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join("parcels"),
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
                InstructionKind::Skill,
                InstructionKind::Skill,
                InstructionKind::Agents,
                InstructionKind::User,
                InstructionKind::Tools,
                InstructionKind::Memory,
                InstructionKind::Heartbeat,
                InstructionKind::Eval,
            ]
        );
        assert_eq!(parcel.instructions[3].skill_name.as_deref(), Some("bundle"));
    }

    #[test]
    fn build_excludes_source_config_from_directory_assets_and_digest() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("source");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(source_dir.join("asset.txt"), "asset").unwrap();
        let config_path = source_dir.join("dispatch.toml");
        let config = |channel_id: &str| {
            format!(
                "[agent]\ncourier_reference = \"native\"\nentrypoint = \"chat\"\nfiles = [\".\"]\n\n[[channels]]\nplugin = \"channel-test\"\nmode = \"poll\"\nconfig = {{ channel_id = \"{channel_id}\", bot_token = \"must-not-package\" }}\n"
            )
        };

        fs::write(&config_path, config("one")).unwrap();
        let first = build_agent(
            &config_path,
            &BuildOptions {
                output_root: dir.path().join("parcels-one"),
            },
        )
        .unwrap();
        fs::write(&config_path, config("two")).unwrap();
        let second = build_agent(
            &config_path,
            &BuildOptions {
                output_root: dir.path().join("parcels-two"),
            },
        )
        .unwrap();

        assert_eq!(first.digest, second.digest);
        assert!(!first.parcel_dir.join("context/dispatch.toml").exists());
        let parcel: ParcelManifest =
            serde_json::from_slice(&fs::read(first.manifest_path).unwrap()).unwrap();
        assert!(
            parcel
                .files
                .iter()
                .all(|file| file.packaged_as != "dispatch.toml")
        );
    }

    #[test]
    fn build_rejects_direct_source_config_packaging() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"native\"\nfiles = [\"dispatch.toml\"]\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join("parcels"),
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "validation failed:\nsource config `dispatch.toml` cannot be packaged into the parcel context"
        );
    }

    #[test]
    fn build_supports_agent_skill_directory_bundles() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::create_dir_all(skill_dir.join("schemas")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: file-analyst\ndescription: Analyze files.\nlicense: MIT\nmetadata:\n  dispatch-manifest: skill.toml\n---\nUse the bundled tools.\n",
        )
        .unwrap();
        fs::write(
            skill_dir.join("skill.toml"),
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\nskills = [\"file-analyst\"]\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
    fn build_rejects_instruction_files_in_agent_skills() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("SKILL.md"), "skill").unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"native\"\nskills = [\"SKILL.md\"]\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join("parcels"),
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "validation failed:\n`agent.skills` entry `SKILL.md` must be a directory; use `agent.instructions.skill` for a standalone instruction file"
        );
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\nskills = [\"file-analyst\"]\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
            skill_dir.join("skill.toml"),
            "[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("scripts/read_file.sh"), "cat \"$1\"\n").unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\nskills = [\"file-analyst\"]\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
    fn build_infers_cmd_runner_for_windows_batch_tools() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("tools")).unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[[agent.tools]]\nkind = \"local\"\npath = \"tools/demo.cmd\"\nalias = \"demo\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("tools/demo.cmd"),
            "@echo off\r\necho ok\r\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let parcel: ParcelManifest =
            serde_json::from_slice(&fs::read(built.manifest_path).unwrap()).unwrap();
        match &parcel.tools[0] {
            ToolConfig::Local(local) => {
                assert_eq!(local.alias, "demo");
                assert_eq!(local.runner.command, "cmd");
                assert_eq!(local.runner.args, vec!["/C", ".\\tools\\demo.cmd"]);
            }
            other => panic!("expected local tool, got {other:?}"),
        }
    }

    #[test]
    fn build_skill_directory_autodetects_skill_sidecar_and_sets_entrypoint_default() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: file-analyst\ndescription: Analyze files.\n---\nUse the bundled tools.\n",
        )
        .unwrap();
        fs::write(
            skill_dir.join("skill.toml"),
            "entrypoint = \"job\"\n\n[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("scripts/read_file.sh"), "cat \"$1\"\n").unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nskills = [\"file-analyst\"]\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
    fn build_agent_entrypoint_overrides_skill_sidecar_entrypoint() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: file-analyst\ndescription: Analyze files.\n---\nUse the bundled tools.\n",
        )
        .unwrap();
        fs::write(
            skill_dir.join("skill.toml"),
            "entrypoint = \"job\"\n\n[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("scripts/read_file.sh"), "cat \"$1\"\n").unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\nskills = [\"file-analyst\"]\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
                skill_dir.join("skill.toml"),
                format!(
                    "entrypoint = \"{entrypoint}\"\n\n[[tools]]\nname = \"{name}_tool\"\nscript = \"scripts/run.sh\"\n"
                ),
            )
            .unwrap();
            fs::write(skill_dir.join("scripts/run.sh"), "echo ok\n").unwrap();
        }
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nskills = [\"file-analyst\", \"web-search\"]\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
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
                skill_dir.join("skill.toml"),
                "[[tools]]\nname = \"shared\"\nscript = \"scripts/run.sh\"\n",
            )
            .unwrap();
            fs::write(skill_dir.join("scripts/run.sh"), "echo ok\n").unwrap();
        }
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\nskills = [\"file-analyst\", \"web-search\"]\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
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
            skill_dir.join("skill.toml"),
            "[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\nrisk = \"low\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("scripts/read_file.sh"), "cat \"$1\"\n").unwrap();
        fs::create_dir_all(dir.path().join("tools")).unwrap();
        fs::write(dir.path().join("tools/read_file.py"), "print('override')\n").unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\nskills = [\"file-analyst\"]\n\n[[agent.tools]]\nkind = \"local\"\npath = \"tools/read_file.py\"\nalias = \"read_file\"\nrisk = \"high\"\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
                "tool `read_file` from skill `file-analyst` overridden by an explicit `agent.tools` declaration"
                    .to_string()
            ]
        );
    }

    #[test]
    fn build_explicit_tool_overrides_a_skill_tool_regardless_of_declaration_order() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: file-analyst\ndescription: Analyze files.\n---\nUse the bundled tools.\n",
        )
        .unwrap();
        fs::write(
            skill_dir.join("skill.toml"),
            "[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\nrisk = \"low\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("scripts/read_file.sh"), "cat \"$1\"\n").unwrap();
        fs::create_dir_all(dir.path().join("tools")).unwrap();
        fs::write(dir.path().join("tools/read_file.py"), "print('override')\n").unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\nskills = [\"file-analyst\"]\n\n[[agent.tools]]\nkind = \"local\"\npath = \"tools/read_file.py\"\nalias = \"read_file\"\nrisk = \"high\"\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
                "tool `read_file` from skill `file-analyst` overridden by an explicit `agent.tools` declaration"
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[[agent.tools]]\nkind = \"local\"\npath = \"tools/read_file.py\"\nalias = \"read_file\"\n\n[[agent.tools]]\nkind = \"local\"\npath = \"tools/read_file.sh\"\nalias = \"read_file\"\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("tool `read_file` is declared more than once in `agent.tools`")
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
            skill_dir.join("skill.toml"),
            "[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\n[[tools]]\nname = \"read_file\"\nscript = \"scripts/other.sh\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("scripts/read_file.sh"), "cat \"$1\"\n").unwrap();
        fs::write(skill_dir.join("scripts/other.sh"), "echo other\n").unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\nskills = [\"file-analyst\"]\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
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
    fn build_reports_reserved_skill_toml_on_autodetect_parse_failure() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join("file-analyst");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: file-analyst\ndescription: Analyze files.\n---\nUse the bundled tools.\n",
        )
        .unwrap();
        fs::write(skill_dir.join("skill.toml"), "this is not toml\n").unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\nskills = [\"file-analyst\"]\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("failed to parse Dispatch skill manifest"));
        assert!(message.contains("`skill.toml` is reserved for skill sidecars"));
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
            skill_dir.join("skill.toml"),
            "[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("scripts/read_file.sh"), "cat \"$1\"\n").unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\nskills = [\"file-analyst\"]\n\n[[agent.tools]]\nkind = \"local\"\npath = \"file-analyst/scripts/read_file.sh\"\nalias = \"read_file_override\"\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\nskills = [\"file-analyst\"]\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
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
            skill_dir.join("skill.toml"),
            "entrypoint = \"unsupported\"\n\n[[tools]]\nname = \"read_file\"\nscript = \"scripts/read_file.sh\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("scripts/read_file.sh"), "cat \"$1\"\n").unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nskills = [\"file-analyst\"]\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\nskills = [\"file-analyst\"]\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"heartbeat\"\n\n[agent.instructions]\nheartbeat = \"HEARTBEAT.md\"\n\n[agent.labels]\n\"org.example.display\" = \"Market Monitor\"\n\n[[agent.mounts]]\nkind = \"memory\"\ndriver = \"pgvector\"\noptions = [\"tenant=acme\", \"namespace=agents\"]\n\n[[agent.network]]\naction = \"allow\"\ntarget = \"api.example.com\"\nqualifiers = [\"via-egress\"]\n",
        )
        .unwrap();
        fs::write(dir.path().join("HEARTBEAT.md"), "poll").unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
    fn build_rejects_heartbeat_without_heartbeat_entrypoint() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[agent.instructions]\nheartbeat = \"HEARTBEAT.md\"\n",
        )
        .unwrap();
        fs::write(dir.path().join("HEARTBEAT.md"), "poll").unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "validation failed:\n`agent.instructions.heartbeat` requires `agent.entrypoint = \"heartbeat\"`"
        );
    }

    #[test]
    fn build_preserves_authored_schedules_in_manifest() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"heartbeat\"\nschedules = [\"*/5 * * * * * *\", \"0 */2 * * * * *\"]\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let parcel: ParcelManifest =
            serde_json::from_slice(&fs::read(built.manifest_path).unwrap()).unwrap();

        assert_eq!(parcel.schedules, vec!["*/5 * * * * * *", "0 */2 * * * * *"]);
    }

    #[test]
    fn build_preserves_authored_listeners_in_manifest() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"heartbeat\"\nlisteners = [\"127.0.0.1:0\", \"127.0.0.1:9000\"]\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let parcel: ParcelManifest =
            serde_json::from_slice(&fs::read(built.manifest_path).unwrap()).unwrap();

        assert_eq!(parcel.listeners, vec!["127.0.0.1:0", "127.0.0.1:9000"]);
    }

    #[test]
    fn build_preserves_authored_ingress_policy_in_manifest() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"heartbeat\"\nlisteners = [\"127.0.0.1:0\"]\n\n[agent.ingress]\npath = \"/hook\"\nsecret_env = \"DISPATCH_WEBHOOK_SECRET\"\nmax_body_bytes = 8192\nmax_header_bytes = 4096\nmethods = [\"POST\", \"PUT\"]\n\n[[agent.secrets]]\nname = \"DISPATCH_WEBHOOK_SECRET\"\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let parcel: ParcelManifest =
            serde_json::from_slice(&fs::read(built.manifest_path).unwrap()).unwrap();

        let ingress = parcel.ingress.expect("ingress policy should be preserved");
        assert_eq!(ingress.path.as_deref(), Some("/hook"));
        assert_eq!(ingress.methods, vec!["POST", "PUT"]);
        assert_eq!(
            ingress.shared_secret_env.as_deref(),
            Some("DISPATCH_WEBHOOK_SECRET")
        );
        assert_eq!(ingress.max_body_bytes, Some(8192));
        assert_eq!(ingress.max_header_bytes, Some(4096));
    }

    #[test]
    fn build_rejects_undeclared_listener_secret() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"heartbeat\"\n\n[agent.ingress]\nsecret_env = \"DISPATCH_WEBHOOK_SECRET\"\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "validation failed:\n`agent.ingress.secret_env` value `DISPATCH_WEBHOOK_SECRET` is not declared in `agent.secrets`"
        );
    }

    #[test]
    fn build_packages_tool_tests_into_manifest() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("scripts")).unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[[agent.tools]]\nkind = \"local\"\npath = \"scripts/demo.sh\"\nalias = \"demo\"\n\n[[agent.tests]]\ntool = \"demo\"\n",
        )
        .unwrap();
        fs::write(dir.path().join("scripts/demo.sh"), "#!/bin/sh\necho ok\n").unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let parcel: ParcelManifest =
            serde_json::from_slice(&fs::read(built.manifest_path).unwrap()).unwrap();

        assert_eq!(
            parcel.tests,
            vec![TestSpec::Tool {
                tool: "demo".to_string(),
            }]
        );
    }

    #[test]
    fn build_rejects_tool_tests_for_unknown_aliases() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("scripts")).unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[[agent.tools]]\nkind = \"local\"\npath = \"scripts/demo.sh\"\nalias = \"demo\"\n\n[[agent.tests]]\ntool = \"missing\"\n",
        )
        .unwrap();
        fs::write(dir.path().join("scripts/demo.sh"), "#!/bin/sh\necho ok\n").unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "validation failed:\n`agent.tests.tool = \"missing\"` references an unknown local or A2A tool alias"
        );
    }

    #[test]
    fn build_records_compaction_config() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[agent.compaction]\ninterval = \"200\"\noverlap = 32\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[[agent.tools]]\nkind = \"builtin\"\nname = \"web_search\"\napproval = \"bogus\"\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            BuildError::AgentConfig(AgentConfigError::Parse { .. })
        ));
    }

    #[test]
    fn build_rejects_invalid_tool_risk_level() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[[agent.tools]]\nkind = \"builtin\"\nname = \"web_search\"\nrisk = \"dangerous\"\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            BuildError::AgentConfig(AgentConfigError::Parse { .. })
        ));
    }

    #[test]
    fn build_rejects_invalid_limit_scope() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[agent.limits]\niteration = 5\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            BuildError::AgentConfig(AgentConfigError::Parse { .. })
        ));
    }

    #[test]
    fn build_accepts_tool_round_limit() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[agent.limits]\ntool_rounds = 4\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let parcel: ParcelManifest =
            serde_json::from_slice(&fs::read(built.manifest_path).unwrap()).unwrap();
        assert!(
            parcel
                .limits
                .iter()
                .any(|limit| limit.scope == "TOOL_ROUNDS" && limit.value == "4")
        );
    }

    #[test]
    fn build_rejects_invalid_timeout_duration() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[agent.timeouts]\ntool = \"sixty\"\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("invalid `agent.timeouts.tool` duration")
        );
    }

    #[test]
    fn build_rejects_zero_timeout_duration() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"native\"\n\n[agent.timeouts]\nrun = \"0s\"\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join("parcels"),
            },
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("invalid `agent.timeouts.run` duration"));
        assert!(message.contains("expected a positive integer"));
    }

    #[test]
    fn build_rejects_removed_required_tool_approval_alias_with_guidance() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"native\"\n\n[[agent.tools]]\nkind = \"builtin\"\nname = \"web_search\"\napproval = \"required\"\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join("parcels"),
            },
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("required"));
        assert!(message.contains("confirm"));
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[[agent.secrets]]\nname = \"A2A_TOKEN\"\n\n[[agent.tools]]\nkind = \"a2a\"\nalias = \"broker_agent\"\nurl = \"https://broker.example.com\"\ndiscovery = \"card\"\nexpect_agent_name = \"remote-broker\"\nexpect_card_sha256 = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\napproval = \"confirm\"\nrisk = \"medium\"\ndescription = \"Delegate to a remote broker\"\nschema = \"schemas/a2a-input.json\"\n\n[agent.tools.auth]\nscheme = \"bearer\"\nsecret_name = \"A2A_TOKEN\"\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[[agent.tools]]\nkind = \"a2a\"\nalias = \"broker\"\nurl = \"https://broker.example.com\"\n\n[agent.tools.auth]\nscheme = \"bearer\"\nsecret_name = \"MISSING_TOKEN\"\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[[agent.tools]]\nkind = \"a2a\"\nalias = \"broker\"\nurl = \"https://broker.example.com\"\nexpect_card_sha256 = \"not-a-digest\"\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("`expect_card_sha256` must be a 64-character hex SHA256 digest")
        );
    }

    #[test]
    fn build_rejects_direct_a2a_with_identity_requirements() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[[agent.tools]]\nkind = \"a2a\"\nalias = \"broker\"\nurl = \"https://broker.example.com\"\ndiscovery = \"direct\"\nexpect_agent_name = \"planner-agent\"\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("cannot use `discovery = \"direct\"`")
        );
    }

    #[test]
    fn build_parses_a2a_header_auth() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[[agent.secrets]]\nname = \"API_KEY\"\n\n[[agent.tools]]\nkind = \"a2a\"\nalias = \"broker\"\nurl = \"https://broker.example.com\"\n\n[agent.tools.auth]\nscheme = \"header\"\nheader_name = \"X-Api-Key\"\nsecret_name = \"API_KEY\"\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[[agent.secrets]]\nname = \"A2A_USER\"\n\n[[agent.secrets]]\nname = \"A2A_PASSWORD\"\n\n[[agent.tools]]\nkind = \"a2a\"\nalias = \"broker\"\nurl = \"https://broker.example.com\"\n\n[agent.tools.auth]\nscheme = \"basic\"\nusername_secret_name = \"A2A_USER\"\npassword_secret_name = \"A2A_PASSWORD\"\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[[agent.secrets]]\nname = \"API_KEY\"\n\n[[agent.tools]]\nkind = \"a2a\"\nalias = \"broker\"\nurl = \"https://broker.example.com\"\n\n[agent.tools.auth]\nscheme = \"header\"\nheader_name = \"Bad:Header\"\nsecret_name = \"API_KEY\"\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[agent.framework]\nname = \"adk-rust\"\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[agent.instructions]\nidentity = \"IDENTITY.md\"\nsoul = \"SOUL.md\"\nagents = \"AGENTS.md\"\nuser = \"USER.md\"\ntools = \"TOOLS.md\"\nmemory = \"MEMORY.md\"\n",
        )
        .unwrap();
        fs::write(dir.path().join("IDENTITY.md"), "name: demo").unwrap();
        fs::write(dir.path().join("SOUL.md"), "soul").unwrap();
        fs::write(dir.path().join("AGENTS.md"), "procedures").unwrap();
        fs::write(dir.path().join("USER.md"), "prefs").unwrap();
        fs::write(dir.path().join("TOOLS.md"), "tool notes").unwrap();
        fs::write(dir.path().join("MEMORY.md"), "memory").unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[agent.model]\nid = \"gpt-5-mini\"\n\n[[agent.tools]]\nkind = \"local\"\npath = \"tools/demo.sh\"\nalias = \"demo\"\nschema = \"schemas/demo.json\"\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("tools")).unwrap();
        fs::create_dir_all(dir.path().join("schemas")).unwrap();
        fs::write(dir.path().join("tools/demo.sh"), "printf ok").unwrap();
        let schema_body = "{\n  \"type\": \"object\",\n  \"properties\": {\n    \"id\": { \"type\": \"string\" }\n  },\n  \"required\": [\"id\"]\n}";
        fs::write(dir.path().join("schemas/demo.json"), schema_body).unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/wasm:latest\"\nentrypoint = \"chat\"\ncomponent = \"components/assistant.wat\"\n\n[agent.instructions]\nsoul = \"SOUL.md\"\n",
        )
        .unwrap();
        fs::write(dir.path().join("SOUL.md"), "soul").unwrap();
        fs::create_dir_all(dir.path().join("components")).unwrap();
        fs::write(dir.path().join("components/assistant.wat"), "(component)").unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[[agent.tools]]\nkind = \"local\"\npath = \"tools/demo.sh\"\nalias = \"demo\"\nschema = \"schemas/demo.json\"\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("tools")).unwrap();
        fs::create_dir_all(dir.path().join("schemas")).unwrap();
        fs::write(dir.path().join("tools/demo.sh"), "printf ok").unwrap();
        fs::write(dir.path().join("schemas/demo.json"), "[1,2,3]").unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[agent.instructions]\nsoul = \"SOUL.md\"\n",
        )
        .unwrap();
        fs::write(dir.path().join("SOUL.md"), "soul").unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
    fn build_records_model_persist_thread_setting() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[agent.model]\nid = \"gpt-5.4\"\nprovider = \"codex\"\n\n[agent.model.options]\n\"persist-thread\" = \"false\"\n\"reasoning-effort\" = \"high\"\n\n[[agent.model.fallbacks]]\nid = \"gpt-5.6-luna\"\nprovider = \"codex\"\n\n[agent.model.fallbacks.options]\n\"persist-thread\" = \"true\"\n",
        )
        .unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let parcel: ParcelManifest =
            serde_json::from_slice(&fs::read(&built.manifest_path).unwrap()).unwrap();

        let primary = parcel.models.primary.unwrap();
        assert_eq!(
            primary.options.get("persist-thread").map(String::as_str),
            Some("false")
        );
        assert_eq!(
            primary.options.get("reasoning-effort").map(String::as_str),
            Some("high")
        );
        assert_eq!(
            parcel.models.fallbacks[0]
                .options
                .get("persist-thread")
                .map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn build_rejects_invalid_model_persist_thread_setting() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[agent.model]\nid = \"gpt-5.4\"\nprovider = \"codex\"\n\n[agent.model.options]\n\"persist-thread\" = \"maybe\"\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("model option `persist-thread` must be `true` or `false`")
        );
    }

    #[test]
    fn build_rejects_an_unknown_model_option() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[agent.model]\nid = \"gpt-5.4\"\nprovider = \"codex\"\n\n[agent.model.options]\npersist-history = \"true\"\n",
        )
        .unwrap();

        let error = build_agent(
            &dir.path().join("dispatch.toml"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("unsupported model option `persist-history`")
        );
    }

    #[test]
    fn verify_parcel_detects_modified_packaged_file() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[agent.instructions]\nsoul = \"SOUL.md\"\n",
        )
        .unwrap();
        fs::write(dir.path().join("SOUL.md"), "soul").unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[agent.instructions]\nsoul = \"SOUL.md\"\n",
        )
        .unwrap();
        fs::write(dir.path().join("SOUL.md"), "soul").unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
            dir.path().join("dispatch.toml"),
            "[agent]\ncourier_reference = \"dispatch/native:latest\"\nentrypoint = \"chat\"\n\n[agent.instructions]\nsoul = \"SOUL.md\"\n",
        )
        .unwrap();
        fs::write(dir.path().join("SOUL.md"), "soul").unwrap();

        let built = build_agent(
            &dir.path().join("dispatch.toml"),
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
