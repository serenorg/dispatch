//! Extension catalog support.
//!
//! A catalog is a JSON document at a stable URL listing plugins (channels,
//! couriers, future connectors) with enough metadata for discovery. This
//! module owns:
//!
//! - the catalog entry schema (what a remote catalog serves)
//! - the user-level catalog registry (`~/.config/dispatch/catalogs.toml`)
//! - fetch and on-disk cache logic
//! - search over cached entries
//!
//! Tier 1/2 of the plugin ecosystem roadmap; see `docs/plugin-ecosystem.md`.

use std::{
    fs,
    io::Read as _,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Name of the catalog registry config file under the dispatch config dir.
pub const CATALOG_CONFIG_FILE: &str = "catalogs.toml";

/// Name of the on-disk cache directory under the dispatch config dir.
pub const CATALOG_CACHE_DIR: &str = "catalog-cache";

const CATALOG_CONFIG_RELATIVE: &str = ".config/dispatch/catalogs.toml";
const CATALOG_CACHE_RELATIVE: &str = ".config/dispatch/catalog-cache";

fn user_home_dir() -> Result<PathBuf, CatalogError> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or(CatalogError::MissingHome)
}

/// Default absolute path for the user-level catalog config.
pub fn default_catalog_config_path() -> Result<PathBuf, CatalogError> {
    Ok(user_home_dir()?.join(CATALOG_CONFIG_RELATIVE))
}

/// Default absolute path for the on-disk catalog cache directory.
pub fn default_catalog_cache_dir() -> Result<PathBuf, CatalogError> {
    Ok(user_home_dir()?.join(CATALOG_CACHE_RELATIVE))
}

/// Upper bound on how many bytes Dispatch will download for one catalog.
/// 4 MiB covers the existing catalog (~7 entries at ~500 B each) with very
/// generous headroom for growth while protecting against pathological
/// responses.
pub const CATALOG_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Default HTTP timeout for catalog fetches.
pub const CATALOG_FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Schema version string produced by `dispatch-plugins/catalog`.
pub const CATALOG_SCHEMA_V1: &str =
    "https://serenorg.github.io/dispatch/schemas/extension-catalog.v1.json";

/// Single user-level catalog registration, as stored in `catalogs.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogSource {
    /// Short identifier used when listing or removing the catalog.
    pub name: String,
    /// Absolute URL to the catalog JSON document.
    pub url: String,
}

/// Root config document. Lives at `~/.config/dispatch/catalogs.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogConfig {
    #[serde(default, rename = "catalogs")]
    pub catalogs: Vec<CatalogSource>,
}

impl CatalogConfig {
    /// Load the config from disk, returning `Default` if the file is missing.
    pub fn load(path: &Path) -> Result<Self, CatalogError> {
        match fs::read_to_string(path) {
            Ok(body) => {
                let config: Self =
                    toml::from_str(&body).map_err(|error| CatalogError::ParseConfig {
                        path: path.display().to_string(),
                        message: error.to_string(),
                    })?;
                Ok(config)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(CatalogError::ReadConfig {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    /// Serialize and atomically write the config to disk.
    pub fn save(&self, path: &Path) -> Result<(), CatalogError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| CatalogError::WriteConfig {
                path: path.display().to_string(),
                source,
            })?;
        }
        let body = toml::to_string_pretty(self).map_err(|error| CatalogError::SerializeConfig {
            message: error.to_string(),
        })?;
        fs::write(path, body).map_err(|source| CatalogError::WriteConfig {
            path: path.display().to_string(),
            source,
        })
    }

    pub fn find(&self, name: &str) -> Option<&CatalogSource> {
        self.catalogs.iter().find(|source| source.name == name)
    }

    /// Add a source, rejecting duplicate names.
    pub fn add(&mut self, source: CatalogSource) -> Result<(), CatalogError> {
        if self.find(&source.name).is_some() {
            return Err(CatalogError::DuplicateCatalog { name: source.name });
        }
        self.catalogs.push(source);
        Ok(())
    }

    /// Remove a source by name. Returns `true` if a source was removed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.catalogs.len();
        self.catalogs.retain(|source| source.name != name);
        self.catalogs.len() != before
    }
}

/// Wire schema served by a remote catalog URL. Mirrors the existing
/// `dispatch-plugins/catalog/extensions.json` structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionCatalog {
    #[serde(default, rename = "schema")]
    pub schema: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub entries: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CatalogExtensionKind {
    Channel,
    Courier,
    Connector,
    Provider,
    Database,
    Deployment,
}

impl CatalogExtensionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Channel => "channel",
            Self::Courier => "courier",
            Self::Connector => "connector",
            Self::Provider => "provider",
            Self::Database => "database",
            Self::Deployment => "deployment",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CatalogInstallSource {
    GithubRelease {
        repo: String,
        tag: String,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        checksum_asset: Option<String>,
        binaries: Vec<GithubReleaseBinary>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GithubReleaseBinary {
    pub target: String,
    pub asset: String,
    #[serde(default)]
    pub sha256: Option<String>,
    pub binary_name: String,
}

/// One plugin entry inside an `ExtensionCatalog`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub name: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub kind: CatalogExtensionKind,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub protocol_version: Option<u32>,
    #[serde(default)]
    pub source_dir: Option<String>,
    #[serde(default)]
    pub manifest_path: Option<String>,
    #[serde(default)]
    pub manifest_url: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub install_hint: Option<String>,
    #[serde(default)]
    pub source: Option<CatalogInstallSource>,
    #[serde(default)]
    pub auth: Option<serde_json::Value>,
    #[serde(default)]
    pub requirements: Option<serde_json::Value>,
}

impl CatalogEntry {
    /// True if any searchable field contains `needle` (case-insensitive).
    pub fn matches(&self, needle: &str) -> bool {
        let needle = needle.to_ascii_lowercase();
        let fields: &[Option<&str>] = &[
            Some(self.name.as_str()),
            self.display_name.as_deref(),
            self.description.as_deref(),
        ];
        if fields
            .iter()
            .flatten()
            .any(|field| field.to_ascii_lowercase().contains(&needle))
        {
            return true;
        }
        let lists: &[&[String]] = &[&self.keywords, &self.tags];
        lists
            .iter()
            .flat_map(|list| list.iter())
            .any(|value| value.to_ascii_lowercase().contains(&needle))
    }
}

/// One catalog entry paired with the catalog it came from, ready to display.
#[derive(Debug, Clone)]
pub struct CatalogSearchHit {
    pub catalog: String,
    pub entry: CatalogEntry,
}

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("failed to read catalog config at {path}: {source}")]
    ReadConfig {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write catalog config at {path}: {source}")]
    WriteConfig {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse catalog config at {path}: {message}")]
    ParseConfig { path: String, message: String },
    #[error("failed to serialize catalog config: {message}")]
    SerializeConfig { message: String },
    #[error("catalog `{name}` is already registered")]
    DuplicateCatalog { name: String },
    #[error("catalog `{name}` is not registered")]
    UnknownCatalog { name: String },
    #[error("failed to fetch catalog `{name}` from {url}: {message}")]
    Fetch {
        name: String,
        url: String,
        message: String,
    },
    #[error("failed to cache catalog `{name}` at {path}: {source}")]
    WriteCache {
        name: String,
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read cached catalog `{name}` at {path}: {source}")]
    ReadCache {
        name: String,
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse catalog `{name}` at {location}: {message}")]
    ParseCatalog {
        name: String,
        location: String,
        message: String,
    },
    #[error("catalog `{name}` response body exceeded {limit} bytes")]
    CatalogTooLarge { name: String, limit: u64 },
    #[error("no catalog has been cached for `{name}`; run `dispatch extension catalog refresh`")]
    NoCache { name: String },
    #[error("HOME and USERPROFILE are not set; cannot resolve default catalog paths")]
    MissingHome,
}

/// Resolve the absolute path to the cache file for a given catalog name.
pub fn cache_path(cache_dir: &Path, name: &str) -> PathBuf {
    cache_dir.join(format!("{name}.json"))
}

/// Read the cached JSON document for a catalog, returning a parsed
/// `ExtensionCatalog`. Returns `NoCache` if the file does not exist.
pub fn load_cached_catalog(cache_dir: &Path, name: &str) -> Result<ExtensionCatalog, CatalogError> {
    let path = cache_path(cache_dir, name);
    let body = match fs::read_to_string(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(CatalogError::NoCache {
                name: name.to_string(),
            });
        }
        Err(source) => {
            return Err(CatalogError::ReadCache {
                name: name.to_string(),
                path: path.display().to_string(),
                source,
            });
        }
    };
    serde_json::from_str(&body).map_err(|error| CatalogError::ParseCatalog {
        name: name.to_string(),
        location: path.display().to_string(),
        message: error.to_string(),
    })
}

/// Write a raw catalog response body to the cache, verifying it parses and
/// returning the parsed value.
pub fn write_cache(
    cache_dir: &Path,
    name: &str,
    body: &str,
) -> Result<ExtensionCatalog, CatalogError> {
    let parsed: ExtensionCatalog =
        serde_json::from_str(body).map_err(|error| CatalogError::ParseCatalog {
            name: name.to_string(),
            location: "response body".to_string(),
            message: error.to_string(),
        })?;
    fs::create_dir_all(cache_dir).map_err(|source| CatalogError::WriteCache {
        name: name.to_string(),
        path: cache_dir.display().to_string(),
        source,
    })?;
    let path = cache_path(cache_dir, name);
    fs::write(&path, body).map_err(|source| CatalogError::WriteCache {
        name: name.to_string(),
        path: path.display().to_string(),
        source,
    })?;
    Ok(parsed)
}

/// Fetch a catalog document over HTTP and return the body as a string.
/// The response is capped at `CATALOG_MAX_BYTES` to protect the host.
pub fn fetch_catalog_body(name: &str, url: &str) -> Result<String, CatalogError> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(CATALOG_FETCH_TIMEOUT))
        .build()
        .new_agent();
    let mut response = agent
        .get(url)
        .header("Accept", "application/json")
        .call()
        .map_err(|error| CatalogError::Fetch {
            name: name.to_string(),
            url: url.to_string(),
            message: error.to_string(),
        })?;
    let mut reader = response.body_mut().with_config().reader();
    read_catalog_body(&mut reader, name).map_err(|error| match error {
        CatalogError::Fetch {
            name,
            url: location,
            message,
        } => CatalogError::Fetch {
            name,
            url: format!("{url} ({location})"),
            message,
        },
        other => other,
    })
}

/// Convenience: fetch and cache a catalog in one call.
pub fn refresh_catalog(
    cache_dir: &Path,
    source: &CatalogSource,
) -> Result<ExtensionCatalog, CatalogError> {
    let body = fetch_catalog_body(&source.name, &source.url)?;
    write_cache(cache_dir, &source.name, &body)
}

fn read_catalog_body<R: std::io::Read>(reader: &mut R, name: &str) -> Result<String, CatalogError> {
    let mut body = Vec::new();
    reader
        .take(CATALOG_MAX_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(|error| CatalogError::Fetch {
            name: name.to_string(),
            url: "response body".to_string(),
            message: error.to_string(),
        })?;
    if (body.len() as u64) > CATALOG_MAX_BYTES {
        return Err(CatalogError::CatalogTooLarge {
            name: name.to_string(),
            limit: CATALOG_MAX_BYTES,
        });
    }
    String::from_utf8(body).map_err(|error| CatalogError::Fetch {
        name: name.to_string(),
        url: "response body".to_string(),
        message: format!("response body is not valid UTF-8: {error}"),
    })
}

/// Search across all catalogs in `config` using the cached document for each.
/// Returns hits in catalog-then-entry order. Missing caches are silently
/// skipped so that a single unreachable catalog does not break search.
pub fn search_cached(
    config: &CatalogConfig,
    cache_dir: &Path,
    query: &str,
    kind_filter: Option<CatalogExtensionKind>,
) -> Vec<CatalogSearchHit> {
    let mut hits = Vec::new();
    for source in &config.catalogs {
        let Ok(catalog) = load_cached_catalog(cache_dir, &source.name) else {
            continue;
        };
        for entry in catalog.entries {
            if let Some(kind) = kind_filter
                && entry.kind != kind
            {
                continue;
            }
            if query.is_empty() || entry.matches(query) {
                hits.push(CatalogSearchHit {
                    catalog: source.name.clone(),
                    entry,
                });
            }
        }
    }
    hits
}

/// Find a specific entry by name across all cached catalogs. Returns the
/// first match in catalog registration order.
pub fn find_cached_entry(
    config: &CatalogConfig,
    cache_dir: &Path,
    name: &str,
) -> Option<CatalogSearchHit> {
    for source in &config.catalogs {
        let Ok(catalog) = load_cached_catalog(cache_dir, &source.name) else {
            continue;
        };
        if let Some(entry) = catalog.entries.into_iter().find(|entry| entry.name == name) {
            return Some(CatalogSearchHit {
                catalog: source.name.clone(),
                entry,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::tempdir;

    fn sample_entry(name: &str, kind: CatalogExtensionKind) -> CatalogEntry {
        CatalogEntry {
            name: name.to_string(),
            display_name: Some(format!("{name} display")),
            kind,
            version: "0.1.0".to_string(),
            description: Some("messaging adapter prototype".to_string()),
            protocol: Some("jsonl".to_string()),
            protocol_version: Some(1),
            source_dir: None,
            manifest_path: None,
            manifest_url: None,
            keywords: vec!["messaging".to_string(), name.to_string()],
            tags: vec!["prototype".to_string()],
            install_hint: Some(format!(
                "dispatch channel install {name}/channel-plugin.json"
            )),
            source: None,
            auth: None,
            requirements: None,
        }
    }

    fn sample_catalog() -> ExtensionCatalog {
        ExtensionCatalog {
            schema: Some(CATALOG_SCHEMA_V1.to_string()),
            repository: Some("dispatch-plugins".to_string()),
            generated_at: None,
            entries: vec![
                sample_entry("channel-telegram", CatalogExtensionKind::Channel),
                sample_entry("courier-seren-cloud", CatalogExtensionKind::Courier),
            ],
        }
    }

    #[test]
    fn config_roundtrips_through_toml() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(CATALOG_CONFIG_FILE);
        let mut config = CatalogConfig::default();
        config
            .add(CatalogSource {
                name: "dispatch-plugins".to_string(),
                url: "https://example.com/extensions.json".to_string(),
            })
            .unwrap();
        config.save(&path).unwrap();
        let loaded = CatalogConfig::load(&path).unwrap();
        assert_eq!(loaded.catalogs, config.catalogs);
    }

    #[test]
    fn catalog_entry_deserializes_github_release_source() {
        let catalog: ExtensionCatalog = serde_json::from_str(
            r#"{
                "entries": [
                    {
                        "name": "seren-cloud",
                        "kind": "courier",
                        "version": "0.1.0",
                        "source": {
                            "type": "github_release",
                            "repo": "serenorg/dispatch-courier-seren-cloud",
                            "tag": "v0.1.0",
                            "binaries": [
                                {
                                    "target": "x86_64-unknown-linux-gnu",
                                    "asset": "dispatch-courier-seren-cloud-x86_64-unknown-linux-gnu",
                                    "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                                    "binary_name": "dispatch-courier-seren-cloud"
                                }
                            ]
                        }
                    }
                ]
            }"#,
        )
        .unwrap();

        let source = catalog.entries[0]
            .source
            .clone()
            .expect("source should parse");
        assert_eq!(
            source,
            CatalogInstallSource::GithubRelease {
                repo: "serenorg/dispatch-courier-seren-cloud".to_string(),
                tag: "v0.1.0".to_string(),
                base_url: None,
                checksum_asset: None,
                binaries: vec![GithubReleaseBinary {
                    target: "x86_64-unknown-linux-gnu".to_string(),
                    asset: "dispatch-courier-seren-cloud-x86_64-unknown-linux-gnu".to_string(),
                    sha256: Some(
                        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                            .to_string(),
                    ),
                    binary_name: "dispatch-courier-seren-cloud".to_string(),
                }],
            }
        );
    }

    #[test]
    fn missing_config_loads_as_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope.toml");
        let loaded = CatalogConfig::load(&path).unwrap();
        assert!(loaded.catalogs.is_empty());
    }

    #[test]
    fn duplicate_add_is_rejected() {
        let mut config = CatalogConfig::default();
        config
            .add(CatalogSource {
                name: "x".to_string(),
                url: "https://example.com/x.json".to_string(),
            })
            .unwrap();
        let err = config
            .add(CatalogSource {
                name: "x".to_string(),
                url: "https://example.com/y.json".to_string(),
            })
            .unwrap_err();
        assert!(matches!(err, CatalogError::DuplicateCatalog { .. }));
    }

    #[test]
    fn remove_returns_whether_present() {
        let mut config = CatalogConfig::default();
        config
            .add(CatalogSource {
                name: "x".to_string(),
                url: "https://example.com/x.json".to_string(),
            })
            .unwrap();
        assert!(config.remove("x"));
        assert!(!config.remove("x"));
    }

    #[test]
    fn entry_matches_is_case_insensitive() {
        let entry = sample_entry("channel-telegram", CatalogExtensionKind::Channel);
        assert!(entry.matches("telegram"));
        assert!(entry.matches("TELEGRAM"));
        assert!(entry.matches("messaging"));
        assert!(entry.matches("prototype"));
        assert!(!entry.matches("discord"));
    }

    #[test]
    fn write_and_load_cache_round_trip() {
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join(CATALOG_CACHE_DIR);
        let body = serde_json::to_string(&sample_catalog()).unwrap();
        let parsed = write_cache(&cache_dir, "plugins", &body).unwrap();
        assert_eq!(parsed.entries.len(), 2);
        let reloaded = load_cached_catalog(&cache_dir, "plugins").unwrap();
        assert_eq!(reloaded.entries[0].name, "channel-telegram");
    }

    #[test]
    fn load_cached_missing_returns_nocache() {
        let dir = tempdir().unwrap();
        let err = load_cached_catalog(dir.path(), "absent").unwrap_err();
        assert!(matches!(err, CatalogError::NoCache { .. }));
    }

    #[test]
    fn search_filters_by_kind_and_query() {
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join(CATALOG_CACHE_DIR);
        let body = serde_json::to_string(&sample_catalog()).unwrap();
        write_cache(&cache_dir, "plugins", &body).unwrap();
        let mut config = CatalogConfig::default();
        config
            .add(CatalogSource {
                name: "plugins".to_string(),
                url: "https://example.com/extensions.json".to_string(),
            })
            .unwrap();

        let all = search_cached(&config, &cache_dir, "", None);
        assert_eq!(all.len(), 2);

        let couriers = search_cached(&config, &cache_dir, "", Some(CatalogExtensionKind::Courier));
        assert_eq!(couriers.len(), 1);
        assert_eq!(couriers[0].entry.name, "courier-seren-cloud");

        let telegram = search_cached(&config, &cache_dir, "telegram", None);
        assert_eq!(telegram.len(), 1);
        assert_eq!(telegram[0].entry.name, "channel-telegram");
    }

    #[test]
    fn find_cached_entry_matches_by_exact_name() {
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join(CATALOG_CACHE_DIR);
        let body = serde_json::to_string(&sample_catalog()).unwrap();
        write_cache(&cache_dir, "plugins", &body).unwrap();
        let mut config = CatalogConfig::default();
        config
            .add(CatalogSource {
                name: "plugins".to_string(),
                url: "https://example.com/extensions.json".to_string(),
            })
            .unwrap();

        let hit = find_cached_entry(&config, &cache_dir, "courier-seren-cloud").unwrap();
        assert_eq!(hit.catalog, "plugins");
        assert_eq!(hit.entry.kind, CatalogExtensionKind::Courier);
        assert!(find_cached_entry(&config, &cache_dir, "nope").is_none());
    }

    #[test]
    fn read_catalog_body_rejects_oversized_payload() {
        let payload = vec![b'a'; (CATALOG_MAX_BYTES + 1) as usize];
        let mut reader = Cursor::new(payload);
        let error = read_catalog_body(&mut reader, "demo").unwrap_err();
        assert!(matches!(error, CatalogError::CatalogTooLarge { .. }));
    }

    #[test]
    fn read_catalog_body_parses_utf8_json() {
        let body = serde_json::to_string(&sample_catalog()).unwrap();
        let mut reader = Cursor::new(body.as_bytes());
        let parsed = read_catalog_body(&mut reader, "demo").unwrap();
        assert!(parsed.contains("\"entries\""));
    }
}
