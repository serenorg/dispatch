use anyhow::{Context, Result, bail};
use dispatch_core::{BuildOptions, BuiltParcel, BuiltinCourier, build_agentfile};
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

pub(crate) fn run_skill(args: crate::RunSkillArgs) -> Result<()> {
    let has_digest_changing_overrides =
        args.model.is_some() || args.provider.is_some() || args.entrypoint.is_some();
    let warned_about_resume = args
        .exec
        .session_file
        .as_ref()
        .is_some_and(|path| path.exists() || has_digest_changing_overrides);
    if warned_about_resume {
        eprintln!(
            "warning: `dispatch skill run --session-file` only resumes cleanly when the synthesized parcel digest stays stable across invocations"
        );
    }
    let synthesized = synthesize_skill_parcel(&args)?;
    for warning in &synthesized.built.warnings {
        eprintln!("warning: {warning}");
    }
    crate::run::run(crate::RunArgs {
        path: synthesized.built.parcel_dir.clone(),
        exec: args.exec.clone(),
    })?;
    Ok(())
}

struct SynthesizedSkillParcel {
    _workspace: TempDir,
    built: BuiltParcel,
}

fn synthesize_skill_parcel(args: &crate::RunSkillArgs) -> Result<SynthesizedSkillParcel> {
    if args.provider.is_some() && args.model.is_none() {
        bail!("`dispatch skill run --provider` requires `--model`");
    }
    let courier = parse_skill_courier(&args.exec.courier)?;
    let workspace = tempfile::tempdir().context("failed to create temporary skill workspace")?;
    let source = resolve_skill_source(&args.path)?;
    let copied_rel = copy_skill_source(&source.root, workspace.path(), &source.copied_name)?;
    let agentfile_path = workspace.path().join("Agentfile");
    let output_root = workspace.path().join(".dispatch/parcels");
    let agentfile = render_skill_agentfile(courier, &copied_rel, args);
    fs::write(&agentfile_path, agentfile)
        .with_context(|| format!("failed to write {}", agentfile_path.display()))?;
    let built = build_agentfile(
        &agentfile_path,
        &BuildOptions {
            output_root: output_root.clone(),
        },
    )
    .with_context(|| {
        format!(
            "failed to build synthesized skill parcel for {}",
            args.path.display()
        )
    })?;
    Ok(SynthesizedSkillParcel {
        _workspace: workspace,
        built,
    })
}

struct ResolvedSkillSource {
    root: PathBuf,
    copied_name: String,
}

fn parse_skill_courier(name: &str) -> Result<BuiltinCourier> {
    match name {
        "native" => Ok(BuiltinCourier::Native),
        "docker" => Ok(BuiltinCourier::Docker),
        "wasm" => bail!(
            "`dispatch skill run` does not support `--courier wasm`; use an Agentfile with `FROM dispatch/wasm:...` and `COMPONENT ...`"
        ),
        other => bail!(
            "`dispatch skill run` currently supports only built-in `native` and `docker` couriers, got `{other}`"
        ),
    }
}

fn resolve_skill_source(path: &Path) -> Result<ResolvedSkillSource> {
    let source = path
        .canonicalize()
        .with_context(|| format!("failed to access skill source {}", path.display()))?;
    let file_name = source
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("skill source must have a final path component"))?;
    let copied_name = file_name.to_string();
    if source.is_dir() {
        return Ok(ResolvedSkillSource {
            root: source,
            copied_name,
        });
    }

    if file_name == "SKILL.md"
        && let Some(parent) = source.parent()
    {
        let copied_name = parent
            .file_name()
            .and_then(OsStr::to_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| anyhow::anyhow!("skill bundle must have a final path component"))?
            .to_string();
        return Ok(ResolvedSkillSource {
            root: parent.to_path_buf(),
            copied_name,
        });
    }

    Ok(ResolvedSkillSource {
        root: source,
        copied_name,
    })
}

fn copy_skill_source(source: &Path, workspace: &Path, source_name: &str) -> Result<String> {
    let destination = workspace.join(source_name);
    if source.is_dir() {
        copy_dir_all(source, &destination)?;
    } else {
        fs::copy(source, &destination).with_context(|| {
            format!(
                "failed to copy skill source {} to {}",
                source.display(),
                destination.display()
            )
        })?;
    }
    Ok(source_name.to_string())
}

fn copy_dir_all(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("failed to enumerate {}", source.display()))?;
        let src_path = entry.path();
        let dest_path = destination.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", src_path.display()))?;
        if file_type.is_dir() {
            copy_dir_all(&src_path, &dest_path)?;
        } else if file_type.is_file() {
            fs::copy(&src_path, &dest_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    src_path.display(),
                    dest_path.display()
                )
            })?;
        } else {
            bail!(
                "unsupported non-file entry `{}` in synthesized skill workspace; symlinks are rejected to match Dispatch parcel packaging rules",
                src_path.display()
            );
        }
    }
    Ok(())
}

fn render_skill_agentfile(
    courier: BuiltinCourier,
    skill_path: &str,
    args: &crate::RunSkillArgs,
) -> String {
    let mut lines = vec![format!("FROM {}", synthesized_from_reference(courier))];
    lines.push(format!("SKILL {}", quote_agentfile_scalar(skill_path)));
    if let Some(model) = args.model.as_deref() {
        let mut line = format!("MODEL {}", quote_agentfile_scalar(model));
        if let Some(provider) = args.provider.as_deref() {
            line.push_str(&format!(" PROVIDER {}", quote_agentfile_scalar(provider)));
        }
        lines.push(line);
    }
    if let Some(entrypoint) = args.entrypoint.as_deref() {
        lines.push(format!("ENTRYPOINT {}", quote_agentfile_scalar(entrypoint)));
    }
    lines.push(String::new());
    lines.join("\n")
}

fn quote_agentfile_scalar(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-'))
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn synthesized_from_reference(courier: BuiltinCourier) -> &'static str {
    match courier {
        BuiltinCourier::Native => "dispatch/native:latest",
        BuiltinCourier::Docker => "dispatch/docker:latest",
        BuiltinCourier::Wasm => unreachable!("wasm is rejected before synthesis"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dispatch_core::{InstructionKind, ToolConfig, load_parcel};

    fn sample_args(path: PathBuf) -> crate::RunSkillArgs {
        crate::RunSkillArgs {
            path,
            exec: crate::RunExecutionArgs {
                courier: "native".to_string(),
                registry: None,
                session_file: None,
                chat: None,
                job: None,
                heartbeat: None,
                interactive: false,
                print_prompt: false,
                list_tools: true,
                json: false,
                tool: None,
                input: None,
                tool_approval: None,
                a2a_allowed_origins: None,
                a2a_trust_policy: None,
            },
            model: None,
            provider: None,
            entrypoint: None,
        }
    }

    #[test]
    fn render_skill_agentfile_uses_matching_courier_reference() {
        let mut args = sample_args(PathBuf::from("skills/file-analyst"));
        args.exec.courier = "docker".to_string();
        args.exec.json = true;
        args.model = Some("gpt-5-mini".to_string());
        args.provider = Some("openai".to_string());
        args.entrypoint = Some("chat".to_string());
        let agentfile = render_skill_agentfile(BuiltinCourier::Docker, "file-analyst", &args);
        assert!(agentfile.contains("FROM dispatch/docker:latest"));
        assert!(agentfile.contains("SKILL file-analyst"));
        assert!(agentfile.contains("MODEL gpt-5-mini PROVIDER openai"));
        assert!(agentfile.contains("ENTRYPOINT chat"));
    }

    #[test]
    fn copy_skill_source_preserves_bundle_name() {
        let source_root = tempfile::tempdir().unwrap();
        let source = source_root.path().join("file-analyst");
        fs::create_dir_all(source.join("scripts")).unwrap();
        fs::write(source.join("SKILL.md"), "# demo\n").unwrap();
        fs::write(source.join("scripts/read.sh"), "echo hi\n").unwrap();
        let workspace = tempfile::tempdir().unwrap();

        let rel = copy_skill_source(&source, workspace.path(), "file-analyst").unwrap();

        assert_eq!(rel, "file-analyst");
        assert!(workspace.path().join("file-analyst/SKILL.md").exists());
        assert!(
            workspace
                .path()
                .join("file-analyst/scripts/read.sh")
                .exists()
        );
    }

    #[test]
    fn render_skill_agentfile_quotes_paths_with_spaces() {
        let args = sample_args(PathBuf::from("skill.md"));
        let agentfile = render_skill_agentfile(BuiltinCourier::Native, "My Skill.md", &args);
        assert!(agentfile.contains("SKILL \"My Skill.md\""));
    }

    #[test]
    fn render_skill_agentfile_quotes_model_provider_and_entrypoint() {
        let mut args = sample_args(PathBuf::from("skill.md"));
        args.model = Some("gpt 5".to_string());
        args.provider = Some("openai compatible".to_string());
        args.entrypoint = Some("job runner".to_string());
        let agentfile = render_skill_agentfile(BuiltinCourier::Native, "skill.md", &args);
        assert!(agentfile.contains("MODEL \"gpt 5\" PROVIDER \"openai compatible\""));
        assert!(agentfile.contains("ENTRYPOINT \"job runner\""));
    }

    #[test]
    fn synthesize_skill_bundle_builds_a_parcel_with_skill_metadata() {
        let root = tempfile::tempdir().unwrap();
        let skill_dir = root.path().join("file-analyst");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\n\
name: file-analyst\n\
description: Analyze files\n\
allowed-tools:\n\
    - read_file\n\
---\n\
\n\
Read files carefully.\n",
        )
        .unwrap();
        fs::write(
            skill_dir.join("dispatch.toml"),
            "entrypoint = \"chat\"\n\
\n\
[[tools]]\n\
name = \"read_file\"\n\
script = \"scripts/read_file.sh\"\n\
risk = \"low\"\n\
description = \"Read a file.\"\n",
        )
        .unwrap();
        fs::write(
            skill_dir.join("scripts/read_file.sh"),
            "#!/bin/sh\ncat \"$1\"\n",
        )
        .unwrap();

        let built = synthesize_skill_parcel(&sample_args(skill_dir)).unwrap();
        let parcel = load_parcel(&built.built.parcel_dir).unwrap();

        assert_eq!(parcel.config.courier.reference(), "dispatch/native:latest");
        let skill = parcel
            .config
            .instructions
            .iter()
            .find(|instruction| instruction.kind == InstructionKind::Skill)
            .expect("expected skill instruction");
        assert_eq!(skill.skill_name.as_deref(), Some("file-analyst"));
        assert_eq!(
            skill.allowed_tools.as_deref(),
            Some(vec!["read_file".to_string()].as_slice())
        );
        let tool = parcel
            .config
            .tools
            .iter()
            .find_map(|tool| match tool {
                ToolConfig::Local(local) if local.alias == "read_file" => Some(local),
                _ => None,
            })
            .expect("expected synthesized local tool");
        assert_eq!(tool.skill_source.as_deref(), Some("file-analyst"));
    }

    #[test]
    fn skill_md_input_escalates_to_parent_bundle_directory() {
        let root = tempfile::tempdir().unwrap();
        let skill_dir = root.path().join("file-analyst");
        fs::create_dir_all(skill_dir.join("references")).unwrap();
        fs::write(skill_dir.join("SKILL.md"), "# skill\n").unwrap();
        fs::write(skill_dir.join("references/README.md"), "context\n").unwrap();

        let resolved = resolve_skill_source(&skill_dir.join("SKILL.md")).unwrap();

        assert_eq!(
            resolved.root.canonicalize().unwrap(),
            skill_dir.canonicalize().unwrap()
        );
        assert_eq!(resolved.copied_name, "file-analyst");
    }
}
