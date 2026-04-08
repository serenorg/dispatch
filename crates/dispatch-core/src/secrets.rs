use base64::{Engine as _, engine::general_purpose::STANDARD};
use ring::{
    aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};
use thiserror::Error;

const SECRET_STORE_DIR: &str = "secrets";
const SECRET_STORE_FILE: &str = "store.dispatch-secrets.json";
const SECRET_KEY_FILE: &str = "key";
const SECRET_STORE_VERSION: u32 = 1;
const SECRET_KEY_BYTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretStorePaths {
    pub dispatch_root: PathBuf,
    pub secrets_dir: PathBuf,
    pub store_path: PathBuf,
    pub key_path: PathBuf,
}

#[derive(Debug, Error)]
pub enum SecretStoreError {
    #[error("could not determine Dispatch root from `{path}`")]
    MissingDispatchRoot { path: String },
    #[error("failed to create secrets directory `{path}`: {source}")]
    CreateSecretsDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("secret store already exists at `{path}`")]
    AlreadyInitialized { path: String },
    #[error("failed to read secret key `{path}`: {source}")]
    ReadKey {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid secret key file `{path}`: {message}")]
    InvalidKey { path: String, message: String },
    #[error("failed to write secret key `{path}`: {source}")]
    WriteKey {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read secret store `{path}`: {source}")]
    ReadStore {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse secret store `{path}`: {source}")]
    ParseStore {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("secret store `{path}` has unsupported version `{version}`")]
    UnsupportedStoreVersion { path: String, version: u32 },
    #[error("failed to write secret store `{path}`: {source}")]
    WriteStore {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to initialize encryption for `{path}`: {message}")]
    EncryptionInit { path: String, message: String },
    #[error("failed to encrypt secret store `{path}`")]
    EncryptStore { path: String },
    #[error("failed to decrypt secret store `{path}`")]
    DecryptStore { path: String },
    #[error("failed to generate random bytes for `{path}`")]
    Random { path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct StoredSecretEnvelope {
    version: u32,
    nonce_b64: String,
    ciphertext_b64: String,
}

pub fn init_secret_store(
    path_hint: &Path,
    force: bool,
) -> Result<SecretStorePaths, SecretStoreError> {
    let paths = secret_store_paths_for_init(path_hint)?;
    fs::create_dir_all(&paths.secrets_dir).map_err(|source| {
        SecretStoreError::CreateSecretsDir {
            path: paths.secrets_dir.display().to_string(),
            source,
        }
    })?;
    if !force && (paths.key_path.exists() || paths.store_path.exists()) {
        return Err(SecretStoreError::AlreadyInitialized {
            path: paths.secrets_dir.display().to_string(),
        });
    }
    let mut key = [0u8; SECRET_KEY_BYTES];
    SystemRandom::new()
        .fill(&mut key)
        .map_err(|_| SecretStoreError::Random {
            path: paths.key_path.display().to_string(),
        })?;
    atomic_write_text(&paths.key_path, &(STANDARD.encode(key) + "\n")).map_err(|source| {
        SecretStoreError::WriteKey {
            path: paths.key_path.display().to_string(),
            source,
        }
    })?;
    set_secret_key_permissions(&paths.key_path).map_err(|source| SecretStoreError::WriteKey {
        path: paths.key_path.display().to_string(),
        source,
    })?;
    write_store_entries(&paths, &BTreeMap::new())?;
    Ok(paths)
}

pub fn list_secret_names(path_hint: &Path) -> Result<Vec<String>, SecretStoreError> {
    let paths = secret_store_paths(path_hint)?;
    let entries = read_store_entries(&paths)?;
    Ok(entries.into_keys().collect())
}

pub fn set_secret(
    path_hint: &Path,
    name: &str,
    value: &str,
) -> Result<SecretStorePaths, SecretStoreError> {
    let paths = ensure_secret_store(path_hint)?;
    let mut entries = read_store_entries(&paths)?;
    entries.insert(name.to_string(), value.to_string());
    write_store_entries(&paths, &entries)?;
    Ok(paths)
}

pub fn remove_secret(
    path_hint: &Path,
    name: &str,
) -> Result<(SecretStorePaths, bool), SecretStoreError> {
    let paths = secret_store_paths(path_hint)?;
    let mut entries = read_store_entries(&paths)?;
    let removed = entries.remove(name).is_some();
    write_store_entries(&paths, &entries)?;
    Ok((paths, removed))
}

pub fn resolve_secret_with_env<F>(
    path_hint: &Path,
    name: &str,
    mut env_lookup: F,
) -> Result<Option<String>, SecretStoreError>
where
    F: FnMut(&str) -> Option<String>,
{
    if let Some(value) = env_lookup(name) {
        return Ok(Some(value));
    }
    let Some(paths) = maybe_secret_store_paths(path_hint)? else {
        return Ok(None);
    };
    let entries = read_store_entries(&paths)?;
    Ok(entries.get(name).cloned())
}

pub fn resolve_secret_from_store(
    path_hint: &Path,
    name: &str,
) -> Result<Option<String>, SecretStoreError> {
    let Some(paths) = maybe_secret_store_paths(path_hint)? else {
        return Ok(None);
    };
    let entries = read_store_entries(&paths)?;
    Ok(entries.get(name).cloned())
}

pub fn secret_store_paths(path_hint: &Path) -> Result<SecretStorePaths, SecretStoreError> {
    let dispatch_root = dispatch_root_from_hint(path_hint).ok_or_else(|| {
        SecretStoreError::MissingDispatchRoot {
            path: path_hint.display().to_string(),
        }
    })?;
    Ok(paths_for_dispatch_root(dispatch_root))
}

pub fn maybe_secret_store_paths(
    path_hint: &Path,
) -> Result<Option<SecretStorePaths>, SecretStoreError> {
    Ok(dispatch_root_from_hint(path_hint).map(paths_for_dispatch_root))
}

fn ensure_secret_store(path_hint: &Path) -> Result<SecretStorePaths, SecretStoreError> {
    let paths = secret_store_paths(path_hint)?;
    if paths.key_path.exists() && paths.store_path.exists() {
        return Ok(paths);
    }
    init_secret_store(path_hint, false)
}

fn secret_store_paths_for_init(path_hint: &Path) -> Result<SecretStorePaths, SecretStoreError> {
    if let Some(dispatch_root) = dispatch_root_from_hint(path_hint) {
        return Ok(paths_for_dispatch_root(dispatch_root));
    }

    let dispatch_root = if path_hint.is_file() {
        path_hint.parent().unwrap_or(path_hint)
    } else {
        path_hint
    };
    Ok(paths_for_dispatch_root(dispatch_root.to_path_buf()))
}

fn dispatch_root_from_hint(path_hint: &Path) -> Option<PathBuf> {
    if let Ok(explicit_root) = env::var("DISPATCH_SECRET_STORE_ROOT") {
        let root = PathBuf::from(explicit_root);
        if root.exists() {
            return Some(root);
        }
    }

    let start = if path_hint.is_file() {
        path_hint.parent().unwrap_or(path_hint)
    } else {
        path_hint
    };

    for ancestor in start.ancestors() {
        if ancestor.file_name().is_some_and(|name| name == ".dispatch")
            && let Some(parent) = ancestor.parent()
        {
            return Some(parent.to_path_buf());
        }
        if ancestor.join(".dispatch").is_dir() {
            return Some(ancestor.to_path_buf());
        }
    }

    None
}

fn paths_for_dispatch_root(dispatch_root: PathBuf) -> SecretStorePaths {
    let secrets_dir = dispatch_root.join(".dispatch").join(SECRET_STORE_DIR);
    SecretStorePaths {
        dispatch_root,
        store_path: secrets_dir.join(SECRET_STORE_FILE),
        key_path: secrets_dir.join(SECRET_KEY_FILE),
        secrets_dir,
    }
}

fn read_store_entries(
    paths: &SecretStorePaths,
) -> Result<BTreeMap<String, String>, SecretStoreError> {
    if !paths.key_path.exists() || !paths.store_path.exists() {
        return Ok(BTreeMap::new());
    }
    let key = read_key(paths)?;
    let source =
        fs::read_to_string(&paths.store_path).map_err(|source| SecretStoreError::ReadStore {
            path: paths.store_path.display().to_string(),
            source,
        })?;
    let envelope: StoredSecretEnvelope =
        serde_json::from_str(&source).map_err(|source| SecretStoreError::ParseStore {
            path: paths.store_path.display().to_string(),
            source,
        })?;
    if envelope.version != SECRET_STORE_VERSION {
        return Err(SecretStoreError::UnsupportedStoreVersion {
            path: paths.store_path.display().to_string(),
            version: envelope.version,
        });
    }
    let plaintext = decrypt_envelope(paths, &key, &envelope)?;
    serde_json::from_slice(&plaintext).map_err(|source| SecretStoreError::ParseStore {
        path: paths.store_path.display().to_string(),
        source,
    })
}

fn write_store_entries(
    paths: &SecretStorePaths,
    entries: &BTreeMap<String, String>,
) -> Result<(), SecretStoreError> {
    fs::create_dir_all(&paths.secrets_dir).map_err(|source| {
        SecretStoreError::CreateSecretsDir {
            path: paths.secrets_dir.display().to_string(),
            source,
        }
    })?;
    let key = read_key(paths)?;
    let plaintext =
        serde_json::to_vec_pretty(entries).map_err(|source| SecretStoreError::ParseStore {
            path: paths.store_path.display().to_string(),
            source,
        })?;
    let envelope = encrypt_entries(paths, &key, &plaintext)?;
    let json =
        serde_json::to_string_pretty(&envelope).map_err(|source| SecretStoreError::ParseStore {
            path: paths.store_path.display().to_string(),
            source,
        })?;
    atomic_write_text(&paths.store_path, &(json + "\n")).map_err(|source| {
        SecretStoreError::WriteStore {
            path: paths.store_path.display().to_string(),
            source,
        }
    })
}

fn read_key(paths: &SecretStorePaths) -> Result<[u8; SECRET_KEY_BYTES], SecretStoreError> {
    let source =
        fs::read_to_string(&paths.key_path).map_err(|source| SecretStoreError::ReadKey {
            path: paths.key_path.display().to_string(),
            source,
        })?;
    let decoded = STANDARD
        .decode(source.trim())
        .map_err(|error| SecretStoreError::InvalidKey {
            path: paths.key_path.display().to_string(),
            message: error.to_string(),
        })?;
    if decoded.len() != SECRET_KEY_BYTES {
        return Err(SecretStoreError::InvalidKey {
            path: paths.key_path.display().to_string(),
            message: format!(
                "expected {SECRET_KEY_BYTES} decoded bytes, got {}",
                decoded.len()
            ),
        });
    }
    let mut key = [0u8; SECRET_KEY_BYTES];
    key.copy_from_slice(&decoded);
    Ok(key)
}

fn encrypt_entries(
    paths: &SecretStorePaths,
    key_bytes: &[u8; SECRET_KEY_BYTES],
    plaintext: &[u8],
) -> Result<StoredSecretEnvelope, SecretStoreError> {
    let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key_bytes).map_err(|error| {
        SecretStoreError::EncryptionInit {
            path: paths.store_path.display().to_string(),
            message: error.to_string(),
        }
    })?);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| SecretStoreError::Random {
            path: paths.store_path.display().to_string(),
        })?;
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let mut in_out = plaintext.to_vec();
    key.seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| SecretStoreError::EncryptStore {
            path: paths.store_path.display().to_string(),
        })?;
    Ok(StoredSecretEnvelope {
        version: SECRET_STORE_VERSION,
        nonce_b64: STANDARD.encode(nonce_bytes),
        ciphertext_b64: STANDARD.encode(in_out),
    })
}

fn decrypt_envelope(
    paths: &SecretStorePaths,
    key_bytes: &[u8; SECRET_KEY_BYTES],
    envelope: &StoredSecretEnvelope,
) -> Result<Vec<u8>, SecretStoreError> {
    let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key_bytes).map_err(|error| {
        SecretStoreError::EncryptionInit {
            path: paths.store_path.display().to_string(),
            message: error.to_string(),
        }
    })?);
    let nonce_raw =
        STANDARD
            .decode(&envelope.nonce_b64)
            .map_err(|_| SecretStoreError::DecryptStore {
                path: paths.store_path.display().to_string(),
            })?;
    if nonce_raw.len() != NONCE_LEN {
        return Err(SecretStoreError::DecryptStore {
            path: paths.store_path.display().to_string(),
        });
    }
    let mut nonce_bytes = [0u8; NONCE_LEN];
    nonce_bytes.copy_from_slice(&nonce_raw);
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let mut ciphertext =
        STANDARD
            .decode(&envelope.ciphertext_b64)
            .map_err(|_| SecretStoreError::DecryptStore {
                path: paths.store_path.display().to_string(),
            })?;
    let plaintext = key
        .open_in_place(nonce, Aad::empty(), &mut ciphertext)
        .map_err(|_| SecretStoreError::DecryptStore {
            path: paths.store_path.display().to_string(),
        })?;
    Ok(plaintext.to_vec())
}

fn atomic_write_text(path: &Path, contents: &str) -> Result<(), std::io::Error> {
    let temp_path = path.with_extension("tmp");
    fs::write(&temp_path, contents)?;
    #[cfg(unix)]
    {
        fs::rename(&temp_path, path)
    }
    #[cfg(not(unix))]
    {
        let _ = fs::remove_file(path);
        fs::rename(&temp_path, path)
    }
}

fn set_secret_key_permissions(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn secret_store_round_trip() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".dispatch")).unwrap();
        init_secret_store(dir.path(), false).unwrap();
        set_secret(dir.path(), "API_TOKEN", "secret-value").unwrap();

        let names = list_secret_names(dir.path()).unwrap();
        assert_eq!(names, vec!["API_TOKEN".to_string()]);

        let resolved = resolve_secret_with_env(dir.path(), "API_TOKEN", |_| None).unwrap();
        assert_eq!(resolved.as_deref(), Some("secret-value"));
    }

    #[test]
    fn env_override_wins_over_store() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".dispatch")).unwrap();
        init_secret_store(dir.path(), false).unwrap();
        set_secret(dir.path(), "API_TOKEN", "secret-value").unwrap();

        let resolved = resolve_secret_with_env(dir.path(), "API_TOKEN", |name| {
            (name == "API_TOKEN").then(|| "from-env".to_string())
        })
        .unwrap();
        assert_eq!(resolved.as_deref(), Some("from-env"));
    }

    #[test]
    fn remove_secret_updates_store() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".dispatch")).unwrap();
        init_secret_store(dir.path(), false).unwrap();
        set_secret(dir.path(), "API_TOKEN", "secret-value").unwrap();

        let (_, removed) = remove_secret(dir.path(), "API_TOKEN").unwrap();
        assert!(removed);
        let resolved = resolve_secret_with_env(dir.path(), "API_TOKEN", |_| None).unwrap();
        assert_eq!(resolved, None);
    }

    #[test]
    fn init_secret_store_bootstraps_dispatch_dir_for_source_root() {
        let dir = tempdir().unwrap();

        let paths = init_secret_store(dir.path(), false).unwrap();

        assert_eq!(paths.dispatch_root, dir.path());
        assert!(paths.secrets_dir.is_dir());
        assert!(paths.key_path.is_file());
        assert!(paths.store_path.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn init_secret_store_restricts_key_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let paths = init_secret_store(dir.path(), false).unwrap();
        let mode = fs::metadata(&paths.key_path).unwrap().permissions().mode() & 0o777;

        assert_eq!(mode, 0o600);
    }
}
