//! Handlers for `dispatch extension ...` - catalog discovery and install.
//!
//! These commands operate on the user-level catalog registry
//! (`~/.config/dispatch/catalogs.toml`) and the on-disk catalog cache
//! (`~/.config/dispatch/catalog-cache/`). Install-by-name is intentionally
//! limited to catalog entries that publish machine-installable source metadata.
//! See `docs/plugin-ecosystem.md`.

use anyhow::{Context, Result, anyhow, bail};
use dispatch_core::{
    CatalogConfig, CatalogEntry, CatalogError, CatalogExtensionKind, CatalogInstallSource,
    CatalogSearchHit, CatalogSource, GithubReleaseBinary, default_catalog_cache_dir,
    default_catalog_config_path, default_channel_registry_path, default_courier_registry_path,
    find_cached_entry, install_channel_plugin, install_courier_plugin, refresh_catalog,
    search_cached,
};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::{fs, io, io::IsTerminal as _, io::Read as _};
use tempfile::tempdir;
use url::Url;

pub(crate) fn extension_command(command: crate::ExtensionCommand) -> Result<()> {
    match command {
        crate::ExtensionCommand::Catalog { command } => match command {
            crate::ExtensionCatalogCommand::Add { url, name, config } => {
                catalog_add(&url, name.as_deref(), config.as_deref())
            }
            crate::ExtensionCatalogCommand::Ls { json, config } => {
                catalog_ls(config.as_deref(), json)
            }
            crate::ExtensionCatalogCommand::Rm { name, config } => {
                catalog_rm(&name, config.as_deref())
            }
            crate::ExtensionCatalogCommand::Refresh {
                name,
                config,
                cache_dir,
            } => catalog_refresh(name.as_deref(), config.as_deref(), cache_dir.as_deref()),
        },
        crate::ExtensionCommand::Install {
            name,
            yes,
            config,
            cache_dir,
            courier_registry,
            channel_registry,
        } => extension_install(
            &name,
            yes,
            config.as_deref(),
            cache_dir.as_deref(),
            courier_registry.as_deref(),
            channel_registry.as_deref(),
            None,
        ),
        crate::ExtensionCommand::Search {
            query,
            kind,
            json,
            config,
            cache_dir,
        } => extension_search(
            query.as_deref().unwrap_or(""),
            kind,
            json,
            config.as_deref(),
            cache_dir.as_deref(),
        ),
        crate::ExtensionCommand::Show {
            name,
            json,
            config,
            cache_dir,
        } => extension_show(&name, json, config.as_deref(), cache_dir.as_deref()),
    }
}

const DEFAULT_EXTENSION_BIN_RELATIVE: &str = ".config/dispatch/bin";
const EXTENSION_DOWNLOAD_MAX_BYTES: u64 = 256 * 1024 * 1024;

struct ExtensionInstallPaths<'a> {
    install_root: &'a Path,
    courier_registry: Option<&'a Path>,
    channel_registry: Option<&'a Path>,
}

fn default_extension_bin_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .map(|home| home.join(DEFAULT_EXTENSION_BIN_RELATIVE))
        .ok_or_else(|| {
            anyhow!("HOME and USERPROFILE are not set; cannot resolve extension bin dir")
        })
}

fn resolve_config_path(override_path: Option<&Path>) -> Result<PathBuf> {
    match override_path {
        Some(path) => Ok(path.to_path_buf()),
        None => default_catalog_config_path().map_err(Into::into),
    }
}

fn resolve_cache_dir(override_path: Option<&Path>) -> Result<PathBuf> {
    match override_path {
        Some(path) => Ok(path.to_path_buf()),
        None => default_catalog_cache_dir().map_err(Into::into),
    }
}

fn resolve_extension_bin_dir(override_path: Option<&Path>) -> Result<PathBuf> {
    match override_path {
        Some(path) => Ok(path.to_path_buf()),
        None => default_extension_bin_dir(),
    }
}

fn resolve_courier_registry_path(override_path: Option<&Path>) -> Result<PathBuf> {
    match override_path {
        Some(path) => Ok(path.to_path_buf()),
        None => default_courier_registry_path().map_err(Into::into),
    }
}

fn resolve_channel_registry_path(override_path: Option<&Path>) -> Result<PathBuf> {
    match override_path {
        Some(path) => Ok(path.to_path_buf()),
        None => default_channel_registry_path().map_err(Into::into),
    }
}

fn derive_catalog_name(url: &str) -> Result<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("catalog URL must not be empty"));
    }

    if let Ok(parsed) = Url::parse(trimmed)
        && let Some(name) = derive_catalog_name_from_url(&parsed)
    {
        return Ok(name);
    }

    let without_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let host = without_scheme.split('/').next().unwrap_or(without_scheme);
    if !host.is_empty() {
        return Ok(sanitize_name(host));
    }

    Err(anyhow!(
        "could not derive a catalog name from `{url}`; pass --name explicitly"
    ))
}

fn derive_catalog_name_from_url(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let segments: Vec<&str> = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect();

    if host.eq_ignore_ascii_case("raw.githubusercontent.com") && segments.len() >= 2 {
        return Some(sanitize_name(segments[1]));
    }

    if host.eq_ignore_ascii_case("github.com") && segments.len() >= 2 {
        return Some(sanitize_name(segments[1]));
    }

    // Walk path segments from the last toward the front, skipping filename-like
    // segments that only name the catalog file itself (e.g. `extensions.json`,
    // `catalog.json`). This lets a URL like
    // `https://example.com/my-catalog/extensions.json` derive `my-catalog` instead
    // of falling through to `example-com`.
    for segment in segments.iter().rev() {
        let trimmed = segment
            .strip_suffix(".json")
            .or_else(|| segment.strip_suffix(".toml"))
            .unwrap_or(segment);
        if trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("extensions")
            || trimmed.eq_ignore_ascii_case("catalog")
        {
            continue;
        }
        return Some(sanitize_name(trimmed));
    }

    Some(sanitize_name(host))
}

fn sanitize_name(raw: &str) -> String {
    raw.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_ascii_lowercase()
}

fn catalog_add(url: &str, explicit_name: Option<&str>, config_path: Option<&Path>) -> Result<()> {
    let path = resolve_config_path(config_path)?;
    let mut config = CatalogConfig::load(&path)?;
    let name = match explicit_name {
        Some(name) => name.to_string(),
        None => derive_catalog_name(url)?,
    };
    config
        .add(CatalogSource {
            name: name.clone(),
            url: url.to_string(),
        })
        .with_context(|| format!("failed to register catalog `{name}`"))?;
    config.save(&path)?;
    println!("Registered catalog `{name}` -> {url}");
    println!("Config: {}", path.display());
    println!("Run `dispatch extension catalog refresh {name}` to populate the local cache.");
    Ok(())
}

fn catalog_ls(config_path: Option<&Path>, emit_json: bool) -> Result<()> {
    let path = resolve_config_path(config_path)?;
    let config = CatalogConfig::load(&path)?;
    if emit_json {
        println!("{}", serde_json::to_string_pretty(&config.catalogs)?);
        return Ok(());
    }
    if config.catalogs.is_empty() {
        println!("No catalogs registered.");
        println!("Add one with `dispatch extension catalog add <url>`.");
        return Ok(());
    }
    for source in &config.catalogs {
        println!("{}\t{}", source.name, source.url);
    }
    Ok(())
}

fn catalog_rm(name: &str, config_path: Option<&Path>) -> Result<()> {
    let path = resolve_config_path(config_path)?;
    let mut config = CatalogConfig::load(&path)?;
    if !config.remove(name) {
        return Err(CatalogError::UnknownCatalog {
            name: name.to_string(),
        }
        .into());
    }
    config.save(&path)?;
    println!("Removed catalog `{name}`");
    Ok(())
}

fn catalog_refresh(
    name: Option<&str>,
    config_path: Option<&Path>,
    cache_dir: Option<&Path>,
) -> Result<()> {
    let path = resolve_config_path(config_path)?;
    let config = CatalogConfig::load(&path)?;
    let cache_dir = resolve_cache_dir(cache_dir)?;

    let targets: Vec<&CatalogSource> = match name {
        Some(name) => match config.find(name) {
            Some(source) => vec![source],
            None => {
                return Err(CatalogError::UnknownCatalog {
                    name: name.to_string(),
                }
                .into());
            }
        },
        None => config.catalogs.iter().collect(),
    };

    if targets.is_empty() {
        println!("No catalogs registered.");
        return Ok(());
    }

    let mut failures = Vec::new();
    for source in targets {
        match refresh_catalog(&cache_dir, source) {
            Ok(catalog) => {
                println!(
                    "Refreshed `{}`: {} entries cached",
                    source.name,
                    catalog.entries.len()
                );
            }
            Err(error) => {
                eprintln!("Failed to refresh `{}`: {error}", source.name);
                failures.push((source.name.clone(), error));
            }
        }
    }

    if failures.is_empty() {
        return Ok(());
    }

    if failures.len() == 1 {
        let (_, error) = failures.into_iter().next().expect("checked length above");
        return Err(error.into());
    }

    Err(anyhow!(
        "{} catalogs failed to refresh; see stderr for details",
        failures.len()
    ))
}

fn extension_install(
    name: &str,
    yes: bool,
    config_path: Option<&Path>,
    cache_dir: Option<&Path>,
    courier_registry: Option<&Path>,
    channel_registry: Option<&Path>,
    install_root: Option<&Path>,
) -> Result<()> {
    let config_path = resolve_config_path(config_path)?;
    let config = CatalogConfig::load(&config_path)?;
    let cache_dir = resolve_cache_dir(cache_dir)?;
    let install_root = resolve_extension_bin_dir(install_root)?;

    let hit = find_cached_entry(&config, &cache_dir, name).ok_or_else(|| {
        anyhow!(
            "extension `{name}` not found in any cached catalog; run `dispatch extension catalog refresh`"
        )
    })?;
    let source = hit.entry.source.clone().ok_or_else(|| {
        anyhow!(
            "extension `{name}` does not publish machine-installable source metadata yet; follow its install_hint instead"
        )
    })?;
    if config.find(&hit.catalog).is_none() {
        return Err(anyhow!(
            "catalog `{}` is no longer registered; run `dispatch extension catalog refresh` after re-adding it",
            hit.catalog
        ));
    }
    if !yes {
        prompt_extension_install_confirmation(&hit, &source)?;
    }

    match source {
        CatalogInstallSource::GithubRelease {
            repo,
            tag,
            base_url,
            checksum_asset,
            binaries,
        } => install_github_release_extension(
            &hit,
            &repo,
            &tag,
            base_url.as_deref(),
            checksum_asset.as_deref(),
            &binaries,
            ExtensionInstallPaths {
                install_root: &install_root,
                courier_registry,
                channel_registry,
            },
        ),
    }
}

fn install_github_release_extension(
    hit: &CatalogSearchHit,
    repo: &str,
    tag: &str,
    base_url: Option<&str>,
    checksum_asset: Option<&str>,
    binaries: &[GithubReleaseBinary],
    paths: ExtensionInstallPaths<'_>,
) -> Result<()> {
    let asset = select_release_binary(binaries, host_target_triple()?)?;
    let asset_url = release_asset_url(repo, tag, base_url, &asset.asset);
    let binary_bytes = fetch_bytes(
        &format!("release asset `{}`", asset.asset),
        &asset_url,
        EXTENSION_DOWNLOAD_MAX_BYTES,
    )?;
    let actual_sha256 = encode_hex(Sha256::digest(&binary_bytes));
    let expected_sha256 = expected_sha256_for_binary(repo, tag, base_url, checksum_asset, asset)?;
    if actual_sha256 != expected_sha256 {
        bail!(
            "downloaded asset `{}` for `{}` had sha256 `{}`, expected `{}`",
            asset.asset,
            hit.entry.name,
            actual_sha256,
            expected_sha256
        );
    }

    let staged_binary = stage_binary(
        paths.install_root,
        &hit.entry.name,
        &hit.entry.version,
        &asset.binary_name,
        &binary_bytes,
    )?;
    let manifest_url = resolve_install_manifest_url(&hit.entry)?;
    let mut manifest_body = fetch_bytes(
        &format!("manifest for `{}`", hit.entry.name),
        &manifest_url,
        1024 * 1024,
    )?;
    let (manifest_dir, manifest_path) =
        rewrite_manifest_for_staged_binary(&hit.entry, &staged_binary, &mut manifest_body)?;

    match hit.entry.kind {
        CatalogExtensionKind::Courier => {
            let registry = resolve_courier_registry_path(paths.courier_registry)?;
            let installed =
                install_courier_plugin(&manifest_path, Some(&registry)).with_context(|| {
                    format!("failed to install courier plugin `{}`", hit.entry.name)
                })?;
            println!(
                "Installed courier `{}` {} from catalog `{}`",
                installed.name, installed.version, hit.catalog
            );
            println!("Binary: {}", staged_binary.display());
            println!("Registry: {}", registry.display());
        }
        CatalogExtensionKind::Channel => {
            let registry = resolve_channel_registry_path(paths.channel_registry)?;
            let installed =
                install_channel_plugin(&manifest_path, Some(&registry)).with_context(|| {
                    format!("failed to install channel plugin `{}`", hit.entry.name)
                })?;
            println!(
                "Installed channel `{}` {} from catalog `{}`",
                installed.name, installed.version, hit.catalog
            );
            println!("Binary: {}", staged_binary.display());
            println!("Registry: {}", registry.display());
        }
        CatalogExtensionKind::Connector => {
            bail!(
                "extension `{}` is a connector; install-by-name is not implemented for connectors yet",
                hit.entry.name
            );
        }
    }

    drop(manifest_dir);

    Ok(())
}

fn host_target_triple() -> Result<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => Ok("aarch64-apple-darwin"),
        ("x86_64", "macos") => Ok("x86_64-apple-darwin"),
        ("aarch64", "linux") => Ok("aarch64-unknown-linux-gnu"),
        ("x86_64", "linux") => Ok("x86_64-unknown-linux-gnu"),
        ("aarch64", "windows") => Ok("aarch64-pc-windows-msvc"),
        ("x86_64", "windows") => Ok("x86_64-pc-windows-msvc"),
        (arch, os) => bail!(
            "dispatch extension install does not have a default target mapping for {arch}-{os}; choose a manual install path for now"
        ),
    }
}

fn select_release_binary<'a>(
    binaries: &'a [GithubReleaseBinary],
    target: &str,
) -> Result<&'a GithubReleaseBinary> {
    binaries
        .iter()
        .find(|binary| binary.target == target)
        .ok_or_else(|| anyhow!("no release asset is published for target `{target}`"))
}

fn release_asset_url(repo: &str, tag: &str, base_url: Option<&str>, asset: &str) -> String {
    format!(
        "{}/{repo}/releases/download/{tag}/{asset}",
        base_url
            .unwrap_or("https://github.com")
            .trim_end_matches('/'),
    )
}

fn expected_sha256_for_binary(
    repo: &str,
    tag: &str,
    base_url: Option<&str>,
    checksum_asset: Option<&str>,
    asset: &GithubReleaseBinary,
) -> Result<String> {
    if let Some(sha256) = &asset.sha256 {
        return Ok(sha256.clone());
    }

    let checksum_asset = checksum_asset.ok_or_else(|| {
        anyhow!(
            "release asset `{}` does not declare sha256 and no checksum_asset was provided",
            asset.asset
        )
    })?;
    let checksum_url = release_asset_url(repo, tag, base_url, checksum_asset);
    let checksum_bytes = fetch_bytes(
        &format!("checksum asset `{checksum_asset}`"),
        &checksum_url,
        1024 * 1024,
    )?;
    let checksum_text = std::str::from_utf8(&checksum_bytes)
        .with_context(|| format!("checksum asset `{checksum_asset}` was not valid UTF-8"))?;
    parse_checksum_asset(checksum_text, &asset.asset)
}

fn parse_checksum_asset(checksum_text: &str, asset_name: &str) -> Result<String> {
    for line in checksum_text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else {
            continue;
        };
        let normalized_name = name.trim_start_matches('*');
        if normalized_name == asset_name {
            return Ok(hash.to_string());
        }
    }

    bail!("checksum asset did not include a sha256 entry for `{asset_name}`")
}

fn resolve_install_manifest_url(entry: &CatalogEntry) -> Result<String> {
    let manifest_url = entry.manifest_url.as_deref().ok_or_else(|| {
        anyhow!(
            "extension `{}` publishes machine-installable source metadata but does not declare manifest_url; install-by-name requires an absolute, version-pinned manifest URL",
            entry.name
        )
    })?;
    Url::parse(manifest_url).with_context(|| {
        format!(
            "extension `{}` declares invalid manifest_url `{manifest_url}`",
            entry.name
        )
    })?;
    Ok(manifest_url.to_string())
}

fn fetch_bytes(label: &str, url: &str, limit: u64) -> Result<Vec<u8>> {
    let mut response = ureq::Agent::config_builder()
        .timeout_global(Some(dispatch_core::CATALOG_FETCH_TIMEOUT))
        .build()
        .new_agent()
        .get(url)
        .call()
        .with_context(|| format!("failed to fetch {label} from {url}"))?;
    let mut reader = response
        .body_mut()
        .with_config()
        .limit(limit.saturating_add(1))
        .reader();
    let mut body = Vec::new();
    reader
        .read_to_end(&mut body)
        .with_context(|| format!("failed to read {label} from {url}"))?;
    if (body.len() as u64) > limit {
        bail!("{label} from {url} exceeded {limit} bytes");
    }
    Ok(body)
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

fn stage_binary(
    install_root: &Path,
    extension_name: &str,
    version: &str,
    binary_name: &str,
    body: &[u8],
) -> Result<PathBuf> {
    let dir = install_root.join(extension_name).join(version);
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create install directory {}", dir.display()))?;
    let binary_path = dir.join(binary_name);
    fs::write(&binary_path, body)
        .with_context(|| format!("failed to write staged binary {}", binary_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&binary_path)
            .with_context(|| format!("failed to stat {}", binary_path.display()))?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&binary_path, permissions).with_context(|| {
            format!(
                "failed to mark staged binary `{}` executable",
                binary_path.display()
            )
        })?;
    }
    Ok(binary_path)
}

fn rewrite_manifest_for_staged_binary(
    entry: &CatalogEntry,
    staged_binary: &Path,
    manifest_body: &mut [u8],
) -> Result<(tempfile::TempDir, PathBuf)> {
    let mut manifest: serde_json::Value = serde_json::from_slice(manifest_body)
        .context("failed to parse downloaded manifest JSON")?;
    match entry.kind {
        CatalogExtensionKind::Courier => {
            let command = manifest
                .get_mut("exec")
                .and_then(|value| value.as_object_mut())
                .ok_or_else(|| anyhow!("downloaded courier manifest is missing exec"))?
                .get_mut("command")
                .ok_or_else(|| anyhow!("downloaded courier manifest is missing exec.command"))?;
            *command = serde_json::Value::String(staged_binary.display().to_string());
        }
        CatalogExtensionKind::Channel => {
            let command = manifest
                .get_mut("entrypoint")
                .and_then(|value| value.as_object_mut())
                .ok_or_else(|| anyhow!("downloaded channel manifest is missing entrypoint"))?
                .get_mut("command")
                .ok_or_else(|| {
                    anyhow!("downloaded channel manifest is missing entrypoint.command")
                })?;
            *command = serde_json::Value::String(staged_binary.display().to_string());
        }
        CatalogExtensionKind::Connector => {
            bail!(
                "extension `{}` is a connector; install-by-name is not implemented for connectors yet",
                entry.name
            );
        }
    }

    let dir = tempdir().context("failed to create temporary manifest directory")?;
    let filename = match entry.kind {
        CatalogExtensionKind::Courier => "courier-plugin.json",
        CatalogExtensionKind::Channel => "channel-plugin.json",
        CatalogExtensionKind::Connector => "connector-plugin.json",
    };
    let path = dir.path().join(filename);
    fs::write(
        &path,
        serde_json::to_vec_pretty(&manifest).context("failed to serialize rewritten manifest")?,
    )
    .with_context(|| format!("failed to write temporary manifest {}", path.display()))?;
    Ok((dir, path))
}

fn prompt_extension_install_confirmation(
    hit: &CatalogSearchHit,
    source: &CatalogInstallSource,
) -> Result<()> {
    if !io::stdin().is_terminal() {
        bail!(
            "install confirmation requires an interactive terminal; rerun with `--yes` to confirm non-interactively"
        );
    }

    let summary = match source {
        CatalogInstallSource::GithubRelease { repo, tag, .. } => {
            format!("GitHub release {repo} {tag}")
        }
    };
    println!(
        "Install `{}` {} from {}? [y/N]",
        hit.entry.name, hit.entry.version, summary
    );
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("failed to read install confirmation")?;
    let normalized = answer.trim().to_ascii_lowercase();
    if normalized == "y" || normalized == "yes" {
        return Ok(());
    }
    bail!("installation cancelled")
}

fn extension_search(
    query: &str,
    kind: Option<crate::ExtensionKindFilter>,
    emit_json: bool,
    config_path: Option<&Path>,
    cache_dir: Option<&Path>,
) -> Result<()> {
    let path = resolve_config_path(config_path)?;
    let config = CatalogConfig::load(&path)?;
    let cache_dir = resolve_cache_dir(cache_dir)?;
    let kind_filter = kind.map(Into::into);
    let hits = search_cached(&config, &cache_dir, query, kind_filter);

    if emit_json {
        #[derive(serde::Serialize)]
        struct HitView<'a> {
            catalog: &'a str,
            entry: &'a dispatch_core::CatalogEntry,
        }
        let view: Vec<HitView<'_>> = hits
            .iter()
            .map(|hit| HitView {
                catalog: &hit.catalog,
                entry: &hit.entry,
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&view)?);
        return Ok(());
    }

    if hits.is_empty() {
        println!("No matching extensions found across cached catalogs.");
        println!("If you recently added a catalog, run `dispatch extension catalog refresh`.");
        return Ok(());
    }

    for hit in &hits {
        print_search_hit(hit);
    }
    Ok(())
}

fn extension_show(
    name: &str,
    emit_json: bool,
    config_path: Option<&Path>,
    cache_dir: Option<&Path>,
) -> Result<()> {
    let path = resolve_config_path(config_path)?;
    let config = CatalogConfig::load(&path)?;
    let cache_dir = resolve_cache_dir(cache_dir)?;

    let hit = find_cached_entry(&config, &cache_dir, name).ok_or_else(|| {
        anyhow!(
            "extension `{name}` not found in any cached catalog; run `dispatch extension catalog refresh`"
        )
    })?;

    if emit_json {
        #[derive(serde::Serialize)]
        struct ShowView<'a> {
            catalog: &'a str,
            entry: &'a dispatch_core::CatalogEntry,
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&ShowView {
                catalog: &hit.catalog,
                entry: &hit.entry,
            })?
        );
        return Ok(());
    }

    print_detail(&hit);
    // Surface where operators can actually reach the cached JSON if they want
    // to inspect fields not printed above.
    let cache_path = dispatch_core::cache_path(&cache_dir, &hit.catalog);
    println!("\nSource catalog cache: {}", cache_path.display());
    Ok(())
}

fn print_search_hit(hit: &CatalogSearchHit) {
    let kind = hit.entry.kind.as_str();
    let display = hit.entry.display_name.as_deref().unwrap_or(&hit.entry.name);
    println!(
        "{name}\t{kind}\t{version}\t{catalog}\t{display}",
        name = hit.entry.name,
        kind = kind,
        version = hit.entry.version,
        catalog = hit.catalog,
        display = display,
    );
}

fn print_detail(hit: &CatalogSearchHit) {
    let entry = &hit.entry;
    println!("name:         {}", entry.name);
    if let Some(display) = &entry.display_name {
        println!("display_name: {display}");
    }
    println!("kind:         {}", entry.kind.as_str());
    println!("version:      {}", entry.version);
    println!("catalog:      {}", hit.catalog);
    if let Some(description) = &entry.description {
        println!("description:  {description}");
    }
    if let Some(protocol) = &entry.protocol {
        match entry.protocol_version {
            Some(version) => println!("protocol:     {protocol} (v{version})"),
            None => println!("protocol:     {protocol}"),
        }
    }
    if let Some(hint) = &entry.install_hint {
        println!("install:      {hint}");
    }
    if let Some(source) = &entry.source {
        match source {
            CatalogInstallSource::GithubRelease { repo, tag, .. } => {
                println!("source:       github_release {repo} {tag}");
            }
        }
    }
    if !entry.keywords.is_empty() {
        println!("keywords:     {}", entry.keywords.join(", "));
    }
    if !entry.tags.is_empty() {
        println!("tags:         {}", entry.tags.join(", "));
    }
    if let Some(manifest_url) = &entry.manifest_url {
        println!("manifest_url: {manifest_url}");
    } else if let Some(manifest_path) = &entry.manifest_path {
        println!("manifest:     {manifest_path} (relative to catalog)");
    }
}

impl From<crate::ExtensionKindFilter> for CatalogExtensionKind {
    fn from(value: crate::ExtensionKindFilter) -> Self {
        match value {
            crate::ExtensionKindFilter::Channel => Self::Channel,
            crate::ExtensionKindFilter::Courier => Self::Courier,
            crate::ExtensionKindFilter::Connector => Self::Connector,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use dispatch_core::{cache_path, load_courier_registry, write_cache};
    #[cfg(unix)]
    use std::thread;
    #[cfg(unix)]
    use tiny_http::{Header, Response, Server, StatusCode};

    #[test]
    fn derives_name_from_github_raw_url() {
        let name = derive_catalog_name(
            "https://raw.githubusercontent.com/serenorg/dispatch-plugins/master/catalog/extensions.json",
        )
        .unwrap();
        assert_eq!(name, "dispatch-plugins");
    }

    #[test]
    fn derives_name_from_bare_host() {
        assert_eq!(
            derive_catalog_name("https://example.com/extensions.json").unwrap(),
            "example-com"
        );
    }

    #[test]
    fn rejects_empty_url() {
        assert!(derive_catalog_name("").is_err());
        assert!(derive_catalog_name("   ").is_err());
    }

    #[test]
    fn sanitize_name_produces_safe_ids() {
        assert_eq!(sanitize_name("Example.Com"), "example-com");
        assert_eq!(sanitize_name("/leading/"), "leading");
    }

    #[test]
    fn derives_name_from_github_repo_url() {
        let name = derive_catalog_name("https://github.com/serenorg/dispatch-courier-seren-cloud")
            .unwrap();
        assert_eq!(name, "dispatch-courier-seren-cloud");
    }

    #[test]
    fn derives_name_from_penultimate_segment_when_last_is_conventional() {
        assert_eq!(
            derive_catalog_name("https://example.com/my-catalog/extensions.json").unwrap(),
            "my-catalog"
        );
        assert_eq!(
            derive_catalog_name("https://example.com/vendor/catalog.json").unwrap(),
            "vendor"
        );
        assert_eq!(
            derive_catalog_name("https://example.com/extensions.json").unwrap(),
            "example-com"
        );
    }

    #[test]
    fn named_refresh_returns_error_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("catalogs.toml");
        let cache_dir = dir.path().join("cache");
        let mut config = CatalogConfig::default();
        config
            .add(CatalogSource {
                name: "broken".to_string(),
                url: "http://127.0.0.1:1/extensions.json".to_string(),
            })
            .unwrap();
        config.save(&config_path).unwrap();

        let error = catalog_refresh(Some("broken"), Some(&config_path), Some(&cache_dir))
            .expect_err("refresh should fail");
        assert!(
            error
                .to_string()
                .contains("failed to fetch catalog `broken`")
        );
    }

    #[test]
    fn select_release_binary_prefers_exact_target_match() {
        let binaries = vec![
            GithubReleaseBinary {
                target: "x86_64-unknown-linux-gnu".to_string(),
                asset: "linux".to_string(),
                sha256: Some("00".repeat(32)),
                binary_name: "dispatch-courier-demo".to_string(),
            },
            GithubReleaseBinary {
                target: "x86_64-pc-windows-msvc".to_string(),
                asset: "windows.exe".to_string(),
                sha256: Some("11".repeat(32)),
                binary_name: "dispatch-courier-demo.exe".to_string(),
            },
        ];

        let selected = select_release_binary(&binaries, "x86_64-pc-windows-msvc").unwrap();
        assert_eq!(selected.asset, "windows.exe");
    }

    #[test]
    fn resolve_install_manifest_url_requires_absolute_manifest_url() {
        let entry = CatalogEntry {
            name: "seren-cloud".to_string(),
            display_name: None,
            kind: CatalogExtensionKind::Courier,
            version: "0.1.0".to_string(),
            description: None,
            protocol: Some("jsonl".to_string()),
            protocol_version: Some(1),
            source_dir: None,
            manifest_path: Some("courier-plugin.json".to_string()),
            manifest_url: None,
            keywords: Vec::new(),
            tags: Vec::new(),
            install_hint: None,
            source: None,
            auth: None,
            requirements: None,
        };

        let error = resolve_install_manifest_url(&entry).unwrap_err();
        assert!(error.to_string().contains("does not declare manifest_url"));
    }

    #[test]
    fn parse_checksum_asset_reads_sha256sum_output() {
        let checksum = parse_checksum_asset(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  demo-binary\n",
            "demo-binary",
        )
        .unwrap();
        assert_eq!(
            checksum,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
    }

    #[cfg(unix)]
    #[test]
    fn extension_install_downloads_and_installs_courier_from_release_metadata() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("catalogs.toml");
        let cache_dir = dir.path().join("cache");
        let install_root = dir.path().join("bin");
        let registry_path = dir.path().join("couriers.json");
        let server = Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr();
        let base_url = format!("http://{}", addr);
        let target = host_target_triple().unwrap().to_string();

        let binary_name = "dispatch-courier-demo";
        let binary_body = b"#!/bin/sh\nexit 0\n".to_vec();
        let manifest_body = format!(
            "{{\n\
\"kind\": \"courier\",\n\
\"name\": \"seren-cloud\",\n\
\"version\": \"0.1.0\",\n\
\"protocol_version\": 1,\n\
\"transport\": \"jsonl\",\n\
\"description\": \"Demo courier plugin\",\n\
\"exec\": {{\n\
\"command\": \"./{binary_name}\",\n\
\"args\": []\n\
}}\n\
}}"
        );

        let server_thread = thread::spawn(move || {
            for _ in 0..3 {
                let request = server.recv().unwrap();
                let path = request.url().to_string();
                let response = match path.as_str() {
                    "/catalog/courier-plugin.json" => Response::from_string(manifest_body.clone())
                        .with_header(
                            Header::from_bytes(
                                b"Content-Type".as_slice(),
                                b"application/json".as_slice(),
                            )
                            .unwrap(),
                        ),
                    "/serenorg/dispatch-courier-seren-cloud/releases/download/v0.1.0/demo-binary" =>
                    {
                        let mut response = Response::from_data(binary_body.clone());
                        response.add_header(
                            Header::from_bytes(
                                b"Content-Type".as_slice(),
                                b"application/octet-stream".as_slice(),
                            )
                            .unwrap(),
                        );
                        response
                    }
                    "/serenorg/dispatch-courier-seren-cloud/releases/download/v0.1.0/SHA256SUMS.txt" => {
                        Response::from_string(format!(
                            "{}  demo-binary\n",
                            encode_hex(Sha256::digest(&binary_body))
                        ))
                        .with_header(
                            Header::from_bytes(
                                b"Content-Type".as_slice(),
                                b"text/plain".as_slice(),
                            )
                            .unwrap(),
                        )
                    }
                    _ => Response::from_string("not found").with_status_code(StatusCode(404)),
                };
                request.respond(response).unwrap();
            }
        });

        let mut config = CatalogConfig::default();
        config
            .add(CatalogSource {
                name: "seren-cloud".to_string(),
                url: format!("{base_url}/catalog/extensions.json"),
            })
            .unwrap();
        config.save(&config_path).unwrap();

        write_cache(
            &cache_dir,
            "seren-cloud",
            &format!(
                r#"{{
                    "entries": [
                        {{
                            "name": "seren-cloud",
                            "kind": "courier",
                            "version": "0.1.0",
                            "manifest_url": "{base_url}/catalog/courier-plugin.json",
                            "install_hint": "dispatch extension install seren-cloud",
                            "source": {{
                                "type": "github_release",
                                "repo": "serenorg/dispatch-courier-seren-cloud",
                                "tag": "v0.1.0",
                                "base_url": "{base_url}",
                                "checksum_asset": "SHA256SUMS.txt",
                                "binaries": [
                                    {{
                                        "target": "{target}",
                                        "asset": "demo-binary",
                                        "binary_name": "{binary_name}"
                                    }}
                                ]
                            }}
                        }}
                    ]
                }}"#,
                target = target
            ),
        )
        .unwrap();
        assert!(cache_path(&cache_dir, "seren-cloud").exists());

        extension_install(
            "seren-cloud",
            true,
            Some(&config_path),
            Some(&cache_dir),
            Some(&registry_path),
            None,
            Some(&install_root),
        )
        .unwrap();

        server_thread.join().unwrap();

        let staged_binary = install_root
            .join("seren-cloud")
            .join("0.1.0")
            .join(binary_name);
        assert!(staged_binary.exists());
        let mode = fs::metadata(&staged_binary).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);

        let registry = load_courier_registry(Some(&registry_path)).unwrap();
        assert_eq!(registry.plugins.len(), 1);
        assert_eq!(registry.plugins[0].name, "seren-cloud");
        let expected_command = fs::canonicalize(&staged_binary)
            .unwrap()
            .display()
            .to_string();
        let installed_command = fs::canonicalize(&registry.plugins[0].exec.command)
            .unwrap()
            .display()
            .to_string();
        assert_eq!(installed_command, expected_command);
    }
}
