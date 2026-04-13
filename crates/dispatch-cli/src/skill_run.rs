use anyhow::{Context, Result, bail};
use dispatch_core::{BuildOptions, BuiltParcel, BuiltinCourier, build_agentfile};
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

struct SkillSynthesisArgs<'a> {
    path: &'a Path,
    courier: &'a str,
    model: Option<&'a str>,
    provider: Option<&'a str>,
    entrypoint: Option<&'a str>,
}

pub(crate) fn run_skill(args: crate::RunSkillArgs) -> Result<()> {
    let has_digest_changing_overrides = args.synthesis.model.is_some()
        || args.synthesis.provider.is_some()
        || args.synthesis.entrypoint.is_some();
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
    let synthesized = synthesize_skill_parcel(&SkillSynthesisArgs {
        path: &args.path,
        courier: &args.exec.courier,
        model: args.synthesis.model.as_deref(),
        provider: args.synthesis.provider.as_deref(),
        entrypoint: args.synthesis.entrypoint.as_deref(),
    })?;
    for warning in &synthesized.built.warnings {
        eprintln!("warning: {warning}");
    }
    crate::run::run(crate::RunArgs {
        path: synthesized.built.parcel_dir.clone(),
        exec: args.exec.clone(),
    })?;
    Ok(())
}

pub(crate) fn validate_skill(args: crate::ValidateSkillArgs) -> Result<()> {
    let synthesized = synthesize_skill_parcel(&SkillSynthesisArgs {
        path: &args.path,
        courier: &args.courier,
        model: args.synthesis.model.as_deref(),
        provider: args.synthesis.provider.as_deref(),
        entrypoint: args.synthesis.entrypoint.as_deref(),
    })?;
    for warning in &synthesized.built.warnings {
        eprintln!("warning: {warning}");
    }
    if synthesized.source.escalated_from_skill_md {
        println!(
            "OK {} (resolved from {})",
            synthesized.source.root.display(),
            args.path.display()
        );
    } else {
        println!("OK {}", args.path.display());
    }
    Ok(())
}

struct SynthesizedSkillParcel {
    _workspace: TempDir,
    built: BuiltParcel,
    source: ResolvedSkillSource,
}

fn synthesize_skill_parcel(args: &SkillSynthesisArgs<'_>) -> Result<SynthesizedSkillParcel> {
    if args.provider.is_some() && args.model.is_none() {
        bail!("`dispatch skill --provider` requires `--model`");
    }
    let courier = parse_skill_courier(args.courier)?;
    let workspace = tempfile::tempdir().context("failed to create temporary skill workspace")?;
    let source = resolve_skill_source(args.path)?;
    let copied_rel = copy_skill_source(&source.root, workspace.path(), &source.copied_name)?;
    let agentfile_path = workspace.path().join("Agentfile");
    let output_root = workspace.path().join(".dispatch/parcels");
    let agentfile = render_skill_agentfile(courier, &copied_rel, args)?;
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
        source,
    })
}

struct ResolvedSkillSource {
    root: PathBuf,
    copied_name: String,
    escalated_from_skill_md: bool,
}

fn parse_skill_courier(name: &str) -> Result<BuiltinCourier> {
    match name {
        "native" => Ok(BuiltinCourier::Native),
        "docker" => Ok(BuiltinCourier::Docker),
        "wasm" => bail!(
            "`dispatch skill` does not support `--courier wasm`; use an Agentfile with `FROM dispatch/wasm:...` and `COMPONENT ...`"
        ),
        other => bail!(
            "`dispatch skill` currently supports only built-in `native` and `docker` couriers, got `{other}`"
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
            escalated_from_skill_md: false,
        });
    }

    let canonical_skill_md = source.parent().map(|parent| parent.join("SKILL.md"));
    // Accept a case-insensitive alias only when `SKILL.md` resolves to the same file, so a
    // distinct lowercase markdown file on a case-sensitive filesystem still behaves like a file.
    let matches_skill_md = file_name == "SKILL.md"
        || file_name.eq_ignore_ascii_case("SKILL.md")
            && canonical_skill_md
                .as_ref()
                .and_then(|path| path.canonicalize().ok())
                .as_ref()
                == Some(&source);
    if matches_skill_md && let Some(parent) = source.parent() {
        let copied_name = parent
            .file_name()
            .and_then(OsStr::to_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| anyhow::anyhow!("skill bundle must have a final path component"))?
            .to_string();
        return Ok(ResolvedSkillSource {
            root: parent.to_path_buf(),
            copied_name,
            escalated_from_skill_md: true,
        });
    }

    Ok(ResolvedSkillSource {
        root: source,
        copied_name,
        escalated_from_skill_md: false,
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
    args: &SkillSynthesisArgs<'_>,
) -> Result<String> {
    let mut lines = vec![format!("FROM {}", synthesized_from_reference(courier))];
    lines.push(format!("SKILL {}", render_agentfile_scalar(skill_path)?));
    if let Some(model) = args.model {
        let mut line = format!("MODEL {}", render_agentfile_scalar(model)?);
        if let Some(provider) = args.provider {
            line.push_str(&format!(" PROVIDER {}", render_agentfile_scalar(provider)?));
        }
        lines.push(line);
    }
    if let Some(entrypoint) = args.entrypoint {
        lines.push(format!(
            "ENTRYPOINT {}",
            render_agentfile_scalar(entrypoint)?
        ));
    }
    lines.push(String::new());
    Ok(lines.join("\n"))
}

fn render_agentfile_scalar(value: &str) -> Result<String> {
    if value.contains(['\n', '\r', '\0']) {
        bail!(
            "cannot synthesize an Agentfile scalar containing newline, carriage return, or NUL bytes"
        );
    }
    // Keep this aligned with `dispatch_core::parse::tokenize`: quoted scalars are only delimited
    // by `"` and do not support backslash escapes, so unsupported values must be rejected here.
    if !value.is_empty() && value.chars().all(|ch| !ch.is_whitespace()) && !value.starts_with('"') {
        Ok(value.to_string())
    } else if value.contains('"') {
        bail!("cannot synthesize an Agentfile scalar containing `\"`")
    } else {
        Ok(format!("\"{value}\""))
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

    fn sample_synthesis_args<'a>(
        path: &'a Path,
        courier: &'a str,
        model: Option<&'a str>,
        provider: Option<&'a str>,
        entrypoint: Option<&'a str>,
    ) -> SkillSynthesisArgs<'a> {
        SkillSynthesisArgs {
            path,
            courier,
            model,
            provider,
            entrypoint,
        }
    }

    #[test]
    fn render_skill_agentfile_uses_matching_courier_reference() {
        let args = sample_synthesis_args(
            Path::new("skills/file-analyst"),
            "docker",
            Some("gpt-5-mini"),
            Some("openai"),
            Some("chat"),
        );
        let agentfile =
            render_skill_agentfile(BuiltinCourier::Docker, "file-analyst", &args).unwrap();
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
        let args = sample_synthesis_args(Path::new("skill.md"), "native", None, None, None);
        let agentfile =
            render_skill_agentfile(BuiltinCourier::Native, "My Skill.md", &args).unwrap();
        assert!(agentfile.contains("SKILL \"My Skill.md\""));
    }

    #[test]
    fn render_skill_agentfile_quotes_model_provider_and_entrypoint() {
        let args = sample_synthesis_args(
            Path::new("skill.md"),
            "native",
            Some("gpt 5"),
            Some("openai compatible"),
            Some("job runner"),
        );
        let agentfile = render_skill_agentfile(BuiltinCourier::Native, "skill.md", &args).unwrap();
        assert!(agentfile.contains("MODEL \"gpt 5\" PROVIDER \"openai compatible\""));
        assert!(agentfile.contains("ENTRYPOINT \"job runner\""));
    }

    #[test]
    fn render_skill_agentfile_rejects_control_characters_in_scalars() {
        let args = sample_synthesis_args(
            Path::new("skill.md"),
            "native",
            Some("gpt-5\nmini"),
            None,
            None,
        );
        let error = render_skill_agentfile(BuiltinCourier::Native, "skill.md", &args).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("newline, carriage return, or NUL bytes")
        );
    }

    #[test]
    fn render_skill_agentfile_rejects_quoted_scalars_that_cannot_round_trip() {
        let args = sample_synthesis_args(
            Path::new("skill.md"),
            "native",
            Some("gpt \"5\" mini"),
            None,
            None,
        );
        let error = render_skill_agentfile(BuiltinCourier::Native, "skill.md", &args).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot synthesize an Agentfile scalar containing `\"`")
        );
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
            skill_dir.join("skill.toml"),
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

        let synthesis_args = sample_synthesis_args(&skill_dir, "native", None, None, None);
        let built = synthesize_skill_parcel(&synthesis_args).unwrap();
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
        assert!(resolved.escalated_from_skill_md);
    }

    #[test]
    fn validate_skill_accepts_instructions_only_bundle_via_skill_md_path() {
        let root = tempfile::tempdir().unwrap();
        let skill_dir = root.path().join("docs-only-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\n\
name: docs-only-skill\n\
description: Documentation-only skill\n\
---\n\
\n\
Use careful reasoning.\n",
        )
        .unwrap();

        validate_skill(crate::ValidateSkillArgs {
            path: skill_dir.join("SKILL.md"),
            courier: "native".to_string(),
            synthesis: crate::SkillSynthesisOverrideArgs {
                model: None,
                provider: None,
                entrypoint: None,
            },
        })
        .unwrap();
    }

    #[test]
    fn validate_skill_rejects_provider_without_model() {
        let root = tempfile::tempdir().unwrap();
        let skill_dir = root.path().join("file-analyst");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\n\
name: file-analyst\n\
description: Analyze files\n\
---\n\
\n\
Read files carefully.\n",
        )
        .unwrap();

        let error = validate_skill(crate::ValidateSkillArgs {
            path: skill_dir,
            courier: "native".to_string(),
            synthesis: crate::SkillSynthesisOverrideArgs {
                model: None,
                provider: Some("openai".to_string()),
                entrypoint: None,
            },
        })
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("`dispatch skill --provider` requires `--model`")
        );
    }
}
