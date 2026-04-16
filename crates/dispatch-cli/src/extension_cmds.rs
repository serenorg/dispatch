//! Handlers for `dispatch extension ...` — Tier 1 catalog discovery.
//!
//! These commands operate on the user-level catalog registry
//! (`~/.config/dispatch/catalogs.toml`) and the on-disk catalog cache
//! (`~/.config/dispatch/catalog-cache/`). Nothing here installs plugins; that
//! lives in Tier 2. See `docs/plugin-ecosystem.md`.

use anyhow::{Context, Result, anyhow};
use dispatch_core::{
    CatalogConfig, CatalogError, CatalogExtensionKind, CatalogSearchHit, CatalogSource,
    default_catalog_cache_dir, default_catalog_config_path, find_cached_entry, refresh_catalog,
    search_cached,
};
use std::path::{Path, PathBuf};
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
    let segments: Vec<&str> = url.path_segments()?.collect();

    if host.eq_ignore_ascii_case("raw.githubusercontent.com") && segments.len() >= 2 {
        return Some(sanitize_name(segments[1]));
    }

    if host.eq_ignore_ascii_case("github.com") && segments.len() >= 2 {
        return Some(sanitize_name(segments[1]));
    }

    if let Some(last) = segments.iter().rev().find(|segment| !segment.is_empty()) {
        let trimmed = last
            .strip_suffix(".json")
            .or_else(|| last.strip_suffix(".toml"))
            .unwrap_or(last);
        if !trimmed.is_empty() && trimmed != "extensions" && trimmed != "catalog" {
            return Some(sanitize_name(trimmed));
        }
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
}
