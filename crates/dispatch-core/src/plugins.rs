use crate::courier::CourierKind;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use thiserror::Error;

const REGISTRY_RELATIVE_PATH: &str = ".config/dispatch/couriers.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginTransport {
    Jsonl,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CourierPluginExec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CourierPluginManifest {
    pub name: String,
    pub version: String,
    pub protocol_version: u32,
    pub transport: PluginTransport,
    pub description: Option<String>,
    pub exec: CourierPluginExec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CourierPluginRegistry {
    #[serde(default)]
    pub plugins: Vec<CourierPluginManifest>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum CourierCatalogEntry {
    Builtin {
        name: String,
        kind: CourierKind,
        description: String,
    },
    Plugin {
        name: String,
        description: Option<String>,
        protocol_version: u32,
        transport: PluginTransport,
        command: String,
        args: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedCourier {
    Builtin(BuiltinCourier),
    Plugin(CourierPluginManifest),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinCourier {
    Native,
    Docker,
    Wasm,
}

#[derive(Debug, Error)]
pub enum PluginRegistryError {
    #[error("HOME is not set; cannot determine default courier registry path")]
    MissingHome,
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
    #[error("failed to parse `{path}`: {source}")]
    ParseJson {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("courier plugin manifest `{path}` is invalid: {message}")]
    InvalidManifest { path: String, message: String },
    #[error("courier `{name}` is reserved by a built-in Dispatch backend")]
    BuiltinNameConflict { name: String },
    #[error("courier `{name}` is not installed")]
    UnknownCourier { name: String },
}

impl BuiltinCourier {
    pub fn name(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Docker => "docker",
            Self::Wasm => "wasm",
        }
    }

    pub fn kind(self) -> CourierKind {
        match self {
            Self::Native => CourierKind::Native,
            Self::Docker => CourierKind::Docker,
            Self::Wasm => CourierKind::Wasm,
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Native => {
                "Built-in host-process Dispatch courier with model-backed chat and local tools."
            }
            Self::Docker => {
                "Built-in Docker courier for declared local tool execution via the Docker CLI."
            }
            Self::Wasm => "Built-in stub courier reserved for WASM-backed execution.",
        }
    }

    pub fn all() -> &'static [BuiltinCourier] {
        &[Self::Native, Self::Docker, Self::Wasm]
    }
}

pub fn default_courier_registry_path() -> Result<PathBuf, PluginRegistryError> {
    let home = std::env::var_os("HOME").ok_or(PluginRegistryError::MissingHome)?;
    Ok(PathBuf::from(home).join(REGISTRY_RELATIVE_PATH))
}

pub fn load_courier_registry(
    path: Option<&Path>,
) -> Result<CourierPluginRegistry, PluginRegistryError> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => default_courier_registry_path()?,
    };
    if !path.exists() {
        return Ok(CourierPluginRegistry::default());
    }

    let body = fs::read_to_string(&path).map_err(|source| PluginRegistryError::ReadFile {
        path: path.display().to_string(),
        source,
    })?;
    serde_json::from_str(&body).map_err(|source| PluginRegistryError::ParseJson {
        path: path.display().to_string(),
        source,
    })
}

pub fn install_courier_plugin(
    manifest_path: &Path,
    registry_path: Option<&Path>,
) -> Result<CourierPluginManifest, PluginRegistryError> {
    let body =
        fs::read_to_string(manifest_path).map_err(|source| PluginRegistryError::ReadFile {
            path: manifest_path.display().to_string(),
            source,
        })?;
    let manifest: CourierPluginManifest =
        serde_json::from_str(&body).map_err(|source| PluginRegistryError::ParseJson {
            path: manifest_path.display().to_string(),
            source,
        })?;
    validate_plugin_manifest(manifest_path, &manifest)?;

    if BuiltinCourier::all()
        .iter()
        .any(|builtin| builtin.name() == manifest.name)
    {
        return Err(PluginRegistryError::BuiltinNameConflict {
            name: manifest.name.clone(),
        });
    }

    let registry_path = match registry_path {
        Some(path) => path.to_path_buf(),
        None => default_courier_registry_path()?,
    };
    let mut registry = load_courier_registry(Some(&registry_path))?;
    registry
        .plugins
        .retain(|plugin| plugin.name != manifest.name);
    registry.plugins.push(manifest.clone());
    registry
        .plugins
        .sort_by(|left, right| left.name.cmp(&right.name));

    if let Some(parent) = registry_path.parent() {
        fs::create_dir_all(parent).map_err(|source| PluginRegistryError::WriteFile {
            path: parent.display().to_string(),
            source,
        })?;
    }
    let payload = serde_json::to_string_pretty(&registry).map_err(|source| {
        PluginRegistryError::ParseJson {
            path: registry_path.display().to_string(),
            source,
        }
    })?;
    fs::write(&registry_path, payload).map_err(|source| PluginRegistryError::WriteFile {
        path: registry_path.display().to_string(),
        source,
    })?;

    Ok(manifest)
}

pub fn list_courier_catalog(
    registry_path: Option<&Path>,
) -> Result<Vec<CourierCatalogEntry>, PluginRegistryError> {
    let registry = load_courier_registry(registry_path)?;
    let mut entries = BuiltinCourier::all()
        .iter()
        .map(|builtin| CourierCatalogEntry::Builtin {
            name: builtin.name().to_string(),
            kind: builtin.kind(),
            description: builtin.description().to_string(),
        })
        .collect::<Vec<_>>();

    entries.extend(
        registry
            .plugins
            .into_iter()
            .map(|plugin| CourierCatalogEntry::Plugin {
                name: plugin.name,
                description: plugin.description,
                protocol_version: plugin.protocol_version,
                transport: plugin.transport,
                command: plugin.exec.command,
                args: plugin.exec.args,
            }),
    );
    entries.sort_by(|left, right| catalog_name(left).cmp(catalog_name(right)));

    Ok(entries)
}

pub fn resolve_courier(
    name: &str,
    registry_path: Option<&Path>,
) -> Result<ResolvedCourier, PluginRegistryError> {
    if let Some(builtin) = BuiltinCourier::all()
        .iter()
        .copied()
        .find(|builtin| builtin.name() == name)
    {
        return Ok(ResolvedCourier::Builtin(builtin));
    }

    let registry = load_courier_registry(registry_path)?;
    let plugin = registry
        .plugins
        .into_iter()
        .find(|plugin| plugin.name == name)
        .ok_or_else(|| PluginRegistryError::UnknownCourier {
            name: name.to_string(),
        })?;
    Ok(ResolvedCourier::Plugin(plugin))
}

fn validate_plugin_manifest(
    path: &Path,
    manifest: &CourierPluginManifest,
) -> Result<(), PluginRegistryError> {
    if manifest.name.trim().is_empty() {
        return Err(PluginRegistryError::InvalidManifest {
            path: path.display().to_string(),
            message: "name must not be empty".to_string(),
        });
    }
    if manifest.version.trim().is_empty() {
        return Err(PluginRegistryError::InvalidManifest {
            path: path.display().to_string(),
            message: "version must not be empty".to_string(),
        });
    }
    if manifest.protocol_version == 0 {
        return Err(PluginRegistryError::InvalidManifest {
            path: path.display().to_string(),
            message: "protocol_version must be greater than zero".to_string(),
        });
    }
    if manifest.exec.command.trim().is_empty() {
        return Err(PluginRegistryError::InvalidManifest {
            path: path.display().to_string(),
            message: "exec.command must not be empty".to_string(),
        });
    }

    Ok(())
}

fn catalog_name(entry: &CourierCatalogEntry) -> &str {
    match entry {
        CourierCatalogEntry::Builtin { name, .. } => name,
        CourierCatalogEntry::Plugin { name, .. } => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn install_courier_plugin_round_trips_registry() {
        let dir = tempdir().unwrap();
        let manifest_path = dir.path().join("courier-plugin.json");
        let registry_path = dir.path().join("plugins.json");
        fs::write(
            &manifest_path,
            "{\n\
\"name\": \"demo-plugin\",\n\
\"version\": \"0.1.0\",\n\
\"protocol_version\": 1,\n\
\"transport\": \"jsonl\",\n\
\"description\": \"Demo courier plugin\",\n\
\"exec\": {\n\
\"command\": \"/usr/local/bin/dispatch-courier-demo\",\n\
\"args\": [\"--stdio\"]\n\
}\n\
}",
        )
        .unwrap();

        let installed = install_courier_plugin(&manifest_path, Some(&registry_path)).unwrap();
        assert_eq!(installed.name, "demo-plugin");

        let registry = load_courier_registry(Some(&registry_path)).unwrap();
        assert_eq!(registry.plugins.len(), 1);
        assert_eq!(registry.plugins[0].name, "demo-plugin");
        assert_eq!(registry.plugins[0].transport, PluginTransport::Jsonl);
    }

    #[test]
    fn resolve_courier_prefers_builtins() {
        let resolved = resolve_courier("docker", None).unwrap();
        assert_eq!(resolved, ResolvedCourier::Builtin(BuiltinCourier::Docker));
    }

    #[test]
    fn install_rejects_builtin_name_conflicts() {
        let dir = tempdir().unwrap();
        let manifest_path = dir.path().join("courier-plugin.json");
        fs::write(
            &manifest_path,
            "{\n\
\"name\": \"docker\",\n\
\"version\": \"0.1.0\",\n\
\"protocol_version\": 1,\n\
\"transport\": \"jsonl\",\n\
\"description\": \"conflict\",\n\
\"exec\": {\n\
\"command\": \"dispatch-courier-docker\",\n\
\"args\": []\n\
}\n\
}",
        )
        .unwrap();

        let error = install_courier_plugin(&manifest_path, Some(&dir.path().join("plugins.json")))
            .unwrap_err();
        assert!(matches!(
            error,
            PluginRegistryError::BuiltinNameConflict { name } if name == "docker"
        ));
    }
}
