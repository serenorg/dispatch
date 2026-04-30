use anyhow::{Context, Result, bail};
use dispatch_core::{
    DeploymentPluginManifest, load_deployment_registry, resolve_deployment_plugin,
};
use dispatch_deployment_protocol::{
    DEPLOYMENT_PLUGIN_PROTOCOL_VERSION, Deployment, DeploymentRevision, PluginRequest,
    PluginRequestEnvelope, PluginRequestId, PluginResponse, parse_jsonrpc_message,
    request_to_jsonrpc,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::{Command, Stdio},
};

use crate::{DeploymentCommand, DeploymentIdArgs, DeploymentPluginOptions};

pub(crate) fn deployment_command(command: DeploymentCommand) -> Result<()> {
    match command {
        DeploymentCommand::List { json, registry } => list_deployments(json, registry.as_deref()),
        DeploymentCommand::Inspect {
            name,
            json,
            registry,
        } => inspect_deployment_plugin(&name, json, registry.as_deref()),
        DeploymentCommand::Capabilities(args) => {
            let json = args.options.json;
            let response =
                call_deployment_plugin(&args.name, &args.options, PluginRequest::Capabilities)?;
            print_deployment_response(response, json)
        }
        DeploymentCommand::Validate(args) => {
            let spec = load_json_file(&args.spec)?;
            let json = args.options.json;
            let response = call_deployment_plugin(
                &args.name,
                &args.options,
                PluginRequest::Validate { spec },
            )?;
            print_deployment_response(response, json)
        }
        DeploymentCommand::Deploy(args) => {
            let spec = load_json_file(&args.spec)?;
            let json = args.options.json;
            let response =
                call_deployment_plugin(&args.name, &args.options, PluginRequest::Deploy { spec })?;
            print_deployment_response(response, json)
        }
        DeploymentCommand::TestRun(args) => {
            let spec = load_json_file(&args.spec)?;
            let json = args.options.json;
            let response = call_deployment_plugin(
                &args.name,
                &args.options,
                PluginRequest::TestRun {
                    spec,
                    sample_input: args.sample_input,
                },
            )?;
            print_deployment_response(response, json)
        }
        DeploymentCommand::Deployments(args) => {
            let filters =
                load_optional_json(args.filters_json.as_deref(), args.filters_file.as_deref())?;
            let json = args.options.json;
            let response =
                call_deployment_plugin(&args.name, &args.options, PluginRequest::List { filters })?;
            print_deployment_response(response, json)
        }
        DeploymentCommand::Get(args) => {
            deployment_id_request(args, |deployment_id| PluginRequest::Get { deployment_id })
        }
        DeploymentCommand::Revisions(args) => deployment_id_request(args, |deployment_id| {
            PluginRequest::ListRevisions { deployment_id }
        }),
        DeploymentCommand::Rollback(args) => {
            let json = args.options.json;
            let response = call_deployment_plugin(
                &args.name,
                &args.options,
                PluginRequest::Rollback {
                    deployment_id: args.deployment_id,
                    revision_id: args.revision_id,
                },
            )?;
            print_deployment_response(response, json)
        }
        DeploymentCommand::Start(args) => {
            deployment_id_request(args, |deployment_id| PluginRequest::Start { deployment_id })
        }
        DeploymentCommand::Stop(args) => {
            deployment_id_request(args, |deployment_id| PluginRequest::Stop { deployment_id })
        }
        DeploymentCommand::Delete(args) => deployment_id_request(args, |deployment_id| {
            PluginRequest::Delete { deployment_id }
        }),
    }
}

fn deployment_id_request(
    args: DeploymentIdArgs,
    build: impl FnOnce(String) -> PluginRequest,
) -> Result<()> {
    let json = args.options.json;
    let response = call_deployment_plugin(&args.name, &args.options, build(args.deployment_id))?;
    print_deployment_response(response, json)
}

fn list_deployments(emit_json: bool, registry: Option<&Path>) -> Result<()> {
    let registry = load_deployment_registry(registry)?;
    if emit_json {
        println!("{}", serde_json::to_string_pretty(&registry)?);
        return Ok(());
    }

    for plugin in registry.plugins {
        let description = plugin.description.as_deref().unwrap_or("");
        println!(
            "{}\tprotocol-v{}\t{}\t{}",
            plugin.name, plugin.protocol_version, plugin.exec.command, description
        );
    }
    Ok(())
}

fn inspect_deployment_plugin(name: &str, emit_json: bool, registry: Option<&Path>) -> Result<()> {
    let plugin = resolve_deployment_plugin(name, registry)?;
    if emit_json {
        println!("{}", serde_json::to_string_pretty(&plugin)?);
        return Ok(());
    }

    println!("Deployment Plugin: {}", plugin.name);
    println!("Version: {}", plugin.version);
    println!("Protocol: v{}", plugin.protocol_version);
    println!("Transport: {:?}", plugin.transport);
    println!("Command: {}", plugin.exec.command);
    if !plugin.exec.args.is_empty() {
        println!("Args: {}", plugin.exec.args.join(" "));
    }
    if let Some(description) = plugin.description.as_deref() {
        println!("Description: {description}");
    }
    if let Some(sha256) = plugin.installed_sha256.as_deref() {
        println!("Installed SHA256: {sha256}");
    }
    Ok(())
}

fn call_deployment_plugin(
    name: &str,
    options: &DeploymentPluginOptions,
    request: PluginRequest,
) -> Result<PluginResponse> {
    let plugin = resolve_deployment_plugin(name, options.registry.as_deref())?;
    let configure = build_config(options)?;
    invoke_deployment_plugin(&plugin, configure, request)
}

fn invoke_deployment_plugin(
    plugin: &DeploymentPluginManifest,
    configure: Option<Value>,
    request: PluginRequest,
) -> Result<PluginResponse> {
    if plugin.protocol_version != DEPLOYMENT_PLUGIN_PROTOCOL_VERSION {
        bail!(
            "deployment plugin `{}` uses unsupported protocol_version {}; expected {}",
            plugin.name,
            plugin.protocol_version,
            DEPLOYMENT_PLUGIN_PROTOCOL_VERSION
        );
    }
    verify_installed_sha256(plugin)?;

    let mut child = Command::new(&plugin.exec.command)
        .args(&plugin.exec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn deployment plugin `{}`", plugin.name))?;
    let mut stdin = child
        .stdin
        .take()
        .context("deployment plugin stdin was not captured")?;
    let stdout = child
        .stdout
        .take()
        .context("deployment plugin stdout was not captured")?;
    let mut reader = BufReader::new(stdout);
    let mut next_id = 1_i64;

    if let Some(config) = configure {
        write_deployment_request(
            &mut stdin,
            plugin,
            next_id,
            PluginRequest::Configure { config },
        )?;
        let configured = read_deployment_response(&mut reader, plugin, next_id)?;
        next_id += 1;
        match configured {
            PluginResponse::Configured { .. } => {}
            PluginResponse::Error { error } => {
                bail!(
                    "deployment plugin configure failed: {}: {}",
                    error.code,
                    error.message
                )
            }
            other => bail!("deployment plugin returned unexpected configure response: {other:?}"),
        }
    }

    write_deployment_request(&mut stdin, plugin, next_id, request)?;
    drop(stdin);
    let response = read_deployment_response(&mut reader, plugin, next_id)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "deployment plugin `{}` exited with {}; stderr: {}",
            plugin.name,
            output.status,
            stderr.trim()
        );
    }
    Ok(response)
}

fn verify_installed_sha256(plugin: &DeploymentPluginManifest) -> Result<()> {
    let Some(expected) = plugin.installed_sha256.as_deref() else {
        return Ok(());
    };
    let body = fs::read(&plugin.exec.command).with_context(|| {
        format!(
            "failed to read deployment plugin `{}` executable",
            plugin.name
        )
    })?;
    let actual = encode_hex(Sha256::digest(body));
    if actual != expected {
        bail!(
            "deployment plugin `{}` executable changed; expected sha256 {}, got {}",
            plugin.name,
            expected,
            actual
        );
    }
    Ok(())
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

fn write_deployment_request(
    writer: &mut impl std::io::Write,
    plugin: &DeploymentPluginManifest,
    request_id: i64,
    request: PluginRequest,
) -> Result<()> {
    let rpc_request = request_to_jsonrpc(
        PluginRequestId::Integer(request_id),
        &PluginRequestEnvelope {
            protocol_version: plugin.protocol_version,
            request,
        },
    )?;
    serde_json::to_writer(&mut *writer, &rpc_request)?;
    std::io::Write::write_all(writer, b"\n")?;
    std::io::Write::flush(writer)?;
    Ok(())
}

fn read_deployment_response(
    reader: &mut impl BufRead,
    plugin: &DeploymentPluginManifest,
    expected_id: i64,
) -> Result<PluginResponse> {
    let mut line = String::new();
    let bytes = reader.read_line(&mut line).with_context(|| {
        format!(
            "failed to read deployment plugin `{}` response",
            plugin.name
        )
    })?;
    if bytes == 0 {
        bail!("deployment plugin `{}` produced no response", plugin.name);
    }
    let (actual_id, response) = parse_jsonrpc_message(line.trim_end())?;
    match actual_id {
        Some(PluginRequestId::Integer(id)) if id == expected_id => Ok(response),
        Some(other) => bail!(
            "deployment plugin `{}` returned response id {:?}, expected {}",
            plugin.name,
            other,
            expected_id
        ),
        None => bail!("deployment plugin `{}` response omitted an id", plugin.name),
    }
}

fn build_config(options: &DeploymentPluginOptions) -> Result<Option<Value>> {
    let mut config = match (&options.config_json, &options.config_file) {
        (Some(raw), None) => {
            Some(serde_json::from_str::<Value>(raw).context("invalid --config-json")?)
        }
        (None, Some(path)) => Some(load_json_file(path)?),
        (None, None) => None,
        (Some(_), Some(_)) => unreachable!("clap enforces config conflicts"),
    };

    if options.api_origin.is_some() || options.api_key.is_some() {
        let object = config
            .get_or_insert_with(|| Value::Object(Default::default()))
            .as_object_mut()
            .context(
                "deployment config must be a JSON object when using --api-origin or --api-key",
            )?;
        if let Some(api_origin) = &options.api_origin {
            object.insert("api_origin".to_string(), Value::String(api_origin.clone()));
        }
        if let Some(api_key) = &options.api_key {
            object.insert("api_key".to_string(), Value::String(api_key.clone()));
        }
    }

    Ok(config)
}

fn load_json_file(path: &Path) -> Result<Value> {
    let body =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&body).with_context(|| format!("failed to parse {}", path.display()))
}

fn load_optional_json(raw: Option<&str>, path: Option<&Path>) -> Result<Option<Value>> {
    match (raw, path) {
        (Some(raw), None) => Ok(Some(
            serde_json::from_str(raw).context("invalid --filters-json")?,
        )),
        (None, Some(path)) => Ok(Some(load_json_file(path)?)),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => unreachable!("clap enforces filter conflicts"),
    }
}

fn print_deployment_response(response: PluginResponse, emit_json: bool) -> Result<()> {
    if emit_json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }

    match response {
        PluginResponse::Capabilities { capabilities } => {
            println!("Deployment Plugin: {}", capabilities.deployment_plugin_id);
            println!("Protocol: v{}", capabilities.protocol_version);
            println!("Templates: {}", capabilities.supported_templates.join(", "));
            println!(
                "Tool Presets: {}",
                capabilities.supported_tool_presets.join(", ")
            );
            println!(
                "Model Policies: {}",
                capabilities.supported_model_policies.join(", ")
            );
            println!("Supports Test Run: {}", capabilities.supports_test_run);
            println!("Supports Revisions: {}", capabilities.supports_revisions);
            println!("Supports Rollback: {}", capabilities.supports_rollback);
            println!("Supports Scheduled: {}", capabilities.supports_scheduled);
        }
        PluginResponse::Health { health } => {
            println!("Reachable: {}", health.reachable);
            if let Some(message) = health.message {
                println!("Message: {message}");
            }
        }
        PluginResponse::Validation { result } => {
            println!("Valid: {}", result.ok);
            for issue in result.issues {
                let field = issue.field.as_deref().unwrap_or("<root>");
                println!("  {field}: {}: {}", issue.code, issue.message);
            }
        }
        PluginResponse::TestRunResult { result } => {
            println!("Status: {}", result.status);
            if let Some(output) = result.output {
                println!("Output: {output}");
            }
            if let Some(error) = result.error {
                println!("Error: {error}");
            }
        }
        PluginResponse::Deployment { deployment }
        | PluginResponse::DeploymentDetail { deployment } => print_deployment(&deployment),
        PluginResponse::DeploymentList { deployments } => {
            for deployment in deployments {
                print_deployment_row(&deployment);
            }
        }
        PluginResponse::Revisions { revisions } => {
            for revision in revisions {
                print_revision_row(&revision);
            }
        }
        PluginResponse::Preview { preview } => {
            println!("Deployment ID: {}", preview.deployment_id);
            println!("{}", serde_json::to_string_pretty(&preview.diff)?);
        }
        PluginResponse::Ok => println!("ok"),
        PluginResponse::Configured { configuration } => {
            println!("Configured: {}", configuration.deployment_plugin_id);
        }
        PluginResponse::Error { error } => {
            bail!("deployment plugin error: {}: {}", error.code, error.message);
        }
    }
    Ok(())
}

fn print_deployment(deployment: &Deployment) {
    println!("Deployment ID: {}", deployment.deployment_id);
    println!("Status: {}", deployment.status);
    if let Some(revision_id) = deployment.revision_id.as_deref() {
        println!("Revision ID: {revision_id}");
    }
}

fn print_deployment_row(deployment: &Deployment) {
    let revision = deployment.revision_id.as_deref().unwrap_or("-");
    println!(
        "{}\t{}\t{}",
        deployment.deployment_id, deployment.status, revision
    );
}

fn print_revision_row(revision: &DeploymentRevision) {
    println!(
        "{}\t{}\t{}",
        revision.revision_id,
        revision.change_kind,
        revision.created_at.as_deref().unwrap_or("-")
    );
}
