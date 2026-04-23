use crate::plugins::{
    PluginRegistryError, PluginTransport, hash_file_sha256, resolve_plugin_exec_path,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

const DATABASE_REGISTRY_RELATIVE_PATH: &str = ".config/dispatch/databases.json";

fn user_home_dir() -> Result<PathBuf, PluginRegistryError> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or(PluginRegistryError::MissingHome)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabasePluginExec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabasePluginManifest {
    pub name: String,
    pub version: String,
    pub protocol_version: u32,
    pub transport: PluginTransport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub exec: DatabasePluginExec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_sha256: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OnDiskDatabaseManifestKind {
    Channel,
    Courier,
    Connector,
    Provider,
    Database,
}

#[derive(Debug, Clone, Deserialize)]
struct DatabasePluginOnDiskManifest {
    #[serde(default)]
    kind: Option<OnDiskDatabaseManifestKind>,
    name: String,
    version: String,
    protocol_version: u32,
    transport: PluginTransport,
    #[serde(default)]
    description: Option<String>,
    exec: DatabasePluginExec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct DatabasePluginRegistry {
    #[serde(default)]
    pub plugins: Vec<DatabasePluginManifest>,
}

pub fn default_database_registry_path() -> Result<PathBuf, PluginRegistryError> {
    Ok(user_home_dir()?.join(DATABASE_REGISTRY_RELATIVE_PATH))
}

pub fn load_database_registry(
    path: Option<&Path>,
) -> Result<DatabasePluginRegistry, PluginRegistryError> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => default_database_registry_path()?,
    };
    if !path.exists() {
        return Ok(DatabasePluginRegistry::default());
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

pub fn install_database_plugin(
    manifest_path: &Path,
    registry_path: Option<&Path>,
) -> Result<DatabasePluginManifest, PluginRegistryError> {
    let body =
        fs::read_to_string(manifest_path).map_err(|source| PluginRegistryError::ReadFile {
            path: manifest_path.display().to_string(),
            source,
        })?;
    let on_disk: DatabasePluginOnDiskManifest =
        serde_json::from_str(&body).map_err(|source| PluginRegistryError::ParseJson {
            path: manifest_path.display().to_string(),
            source,
        })?;
    validate_on_disk_database_plugin_manifest(manifest_path, &on_disk)?;

    let mut manifest = DatabasePluginManifest {
        name: on_disk.name,
        version: on_disk.version,
        protocol_version: on_disk.protocol_version,
        transport: on_disk.transport,
        description: on_disk.description,
        exec: on_disk.exec,
        installed_sha256: None,
    };
    validate_database_plugin_manifest(manifest_path, &manifest)?;

    let exec_path = resolve_plugin_exec_path(manifest_path, &manifest.exec.command)?;
    manifest.exec.command = exec_path.display().to_string();
    manifest.installed_sha256 = Some(hash_file_sha256(&exec_path)?);

    let registry_path = match registry_path {
        Some(path) => path.to_path_buf(),
        None => default_database_registry_path()?,
    };
    let mut registry = load_database_registry(Some(&registry_path))?;
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

pub fn resolve_database_plugin(
    name: &str,
    registry_path: Option<&Path>,
) -> Result<DatabasePluginManifest, PluginRegistryError> {
    let registry = load_database_registry(registry_path)?;
    registry
        .plugins
        .into_iter()
        .find(|plugin| plugin.name == name)
        .ok_or_else(|| PluginRegistryError::UnknownDatabase {
            name: name.to_string(),
        })
}

pub fn validate_database_plugin_manifest(
    path: &Path,
    manifest: &DatabasePluginManifest,
) -> Result<(), PluginRegistryError> {
    validate_manifest_fields(
        path,
        &manifest.name,
        &manifest.version,
        manifest.protocol_version,
        &manifest.exec.command,
    )
}

fn validate_on_disk_database_plugin_manifest(
    path: &Path,
    manifest: &DatabasePluginOnDiskManifest,
) -> Result<(), PluginRegistryError> {
    if let Some(kind) = &manifest.kind
        && *kind != OnDiskDatabaseManifestKind::Database
    {
        return Err(PluginRegistryError::InvalidManifest {
            path: path.display().to_string(),
            message: format!(
                "kind `{}` is invalid for a database plugin manifest; expected `database`",
                kind.as_str()
            ),
        });
    }
    Ok(())
}

fn validate_manifest_fields(
    path: &Path,
    name: &str,
    version: &str,
    protocol_version: u32,
    command: &str,
) -> Result<(), PluginRegistryError> {
    if name.trim().is_empty() {
        return Err(PluginRegistryError::InvalidManifest {
            path: path.display().to_string(),
            message: "name must not be empty".to_string(),
        });
    }
    if version.trim().is_empty() {
        return Err(PluginRegistryError::InvalidManifest {
            path: path.display().to_string(),
            message: "version must not be empty".to_string(),
        });
    }
    if protocol_version != 1 {
        return Err(PluginRegistryError::InvalidManifest {
            path: path.display().to_string(),
            message: format!("protocol_version `{protocol_version}` is unsupported; expected 1"),
        });
    }
    if command.trim().is_empty() {
        return Err(PluginRegistryError::InvalidManifest {
            path: path.display().to_string(),
            message: "exec.command must not be empty".to_string(),
        });
    }
    Ok(())
}

impl OnDiskDatabaseManifestKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Channel => "channel",
            Self::Courier => "courier",
            Self::Connector => "connector",
            Self::Provider => "provider",
            Self::Database => "database",
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn install_database_plugin_round_trips_registry() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let script_path = dir.path().join("dispatch-database-demo.sh");
        fs::write(&script_path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();
        let manifest_path = dir.path().join("database-plugin.json");
        let registry_path = dir.path().join("databases.json");
        fs::write(
            &manifest_path,
            format!(
                "{{\n\
\"kind\": \"database\",\n\
\"name\": \"seren-db\",\n\
\"version\": \"0.1.0\",\n\
\"protocol_version\": 1,\n\
\"transport\": \"jsonl\",\n\
\"description\": \"Demo database plugin\",\n\
\"exec\": {{\n\
\"command\": \"{}\",\n\
\"args\": [\"--stdio\"]\n\
}}\n\
}}",
                script_path.display()
            ),
        )
        .unwrap();

        let installed = install_database_plugin(&manifest_path, Some(&registry_path)).unwrap();
        assert_eq!(installed.name, "seren-db");
        assert!(installed.installed_sha256.is_some());

        let registry = load_database_registry(Some(&registry_path)).unwrap();
        assert_eq!(registry.plugins.len(), 1);
        assert_eq!(registry.plugins[0].name, "seren-db");
    }

    #[test]
    fn resolve_database_plugin_finds_installed() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let script_path = dir.path().join("dispatch-database-demo.sh");
        fs::write(&script_path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).unwrap();
        let manifest_path = dir.path().join("database-plugin.json");
        let registry_path = dir.path().join("databases.json");
        fs::write(
            &manifest_path,
            format!(
                "{{\n\
\"kind\": \"database\",\n\
\"name\": \"seren-db\",\n\
\"version\": \"0.1.0\",\n\
\"protocol_version\": 1,\n\
\"transport\": \"jsonl\",\n\
\"exec\": {{\n\
\"command\": \"{}\",\n\
\"args\": []\n\
}}\n\
}}",
                script_path.display()
            ),
        )
        .unwrap();

        install_database_plugin(&manifest_path, Some(&registry_path)).unwrap();
        let resolved = resolve_database_plugin("seren-db", Some(&registry_path)).unwrap();
        assert_eq!(resolved.name, "seren-db");
    }
}
