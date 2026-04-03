use crate::courier::load_parcel;
use ring::{
    rand::SystemRandom,
    signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use thiserror::Error;

pub const DISPATCH_SIGNATURE_ALGORITHM: &str = "ed25519";
pub const PARCEL_SIGNATURES_DIR: &str = "signatures";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretKeyFile {
    pub key_id: String,
    pub algorithm: String,
    pub pkcs8: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicKeyFile {
    pub key_id: String,
    pub algorithm: String,
    pub public_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParcelSignature {
    pub key_id: String,
    pub algorithm: String,
    pub digest: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignatureVerification {
    pub key_id: String,
    pub algorithm: String,
    pub signature_found: bool,
    pub digest_matches: bool,
    pub signature_matches: bool,
}

impl SignatureVerification {
    pub fn is_ok(&self) -> bool {
        self.signature_found && self.digest_matches && self.signature_matches
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedKeyPair {
    pub secret_key_path: PathBuf,
    pub public_key_path: PathBuf,
}

#[derive(Debug, Error)]
pub enum SigningError {
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
    #[error("failed to parse `{path}`: {source}")]
    ParseJson {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid signing key id `{key_id}`")]
    InvalidKeyId { key_id: String },
    #[error("unsupported signing algorithm `{algorithm}`")]
    UnsupportedAlgorithm { algorithm: String },
    #[error("invalid hex payload in `{path}`: {message}")]
    InvalidHex { path: String, message: String },
    #[error("invalid secret key file `{path}`: {message}")]
    InvalidSecretKey { path: String, message: String },
    #[error("invalid public key file `{path}`: {message}")]
    InvalidPublicKey { path: String, message: String },
    #[error("failed to generate signing keypair")]
    KeyGeneration,
    #[error("failed to load parcel at `{path}`: {message}")]
    LoadParcel { path: String, message: String },
    #[error("failed to serialize signing payload: {0}")]
    Serialize(#[from] serde_json::Error),
}

pub fn generate_keypair_files(
    output_dir: &Path,
    key_id: &str,
) -> Result<GeneratedKeyPair, SigningError> {
    validate_key_id(key_id)?;

    let rng = SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| SigningError::KeyGeneration)?;
    let keypair =
        Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|_| SigningError::KeyGeneration)?;

    fs::create_dir_all(output_dir).map_err(|source| SigningError::CreateDir {
        path: output_dir.display().to_string(),
        source,
    })?;

    let secret_key = SecretKeyFile {
        key_id: key_id.to_string(),
        algorithm: DISPATCH_SIGNATURE_ALGORITHM.to_string(),
        pkcs8: encode_hex(pkcs8.as_ref()),
    };
    let public_key = PublicKeyFile {
        key_id: key_id.to_string(),
        algorithm: DISPATCH_SIGNATURE_ALGORITHM.to_string(),
        public_key: encode_hex(keypair.public_key().as_ref()),
    };

    let secret_key_path = output_dir.join(format!("{key_id}.dispatch-secret.json"));
    let public_key_path = output_dir.join(format!("{key_id}.dispatch-public.json"));
    write_json(&secret_key_path, &secret_key)?;
    write_json(&public_key_path, &public_key)?;

    Ok(GeneratedKeyPair {
        secret_key_path,
        public_key_path,
    })
}

pub fn sign_parcel(parcel_path: &Path, secret_key_path: &Path) -> Result<PathBuf, SigningError> {
    let parcel = load_parcel(parcel_path).map_err(|error| SigningError::LoadParcel {
        path: parcel_path.display().to_string(),
        message: error.to_string(),
    })?;
    let secret_key = read_secret_key(secret_key_path)?;
    let pkcs8 = decode_hex(&secret_key.pkcs8, &secret_key_path.display().to_string())?;
    let keypair =
        Ed25519KeyPair::from_pkcs8(&pkcs8).map_err(|_| SigningError::InvalidSecretKey {
            path: secret_key_path.display().to_string(),
            message: "pkcs8 payload is invalid".to_string(),
        })?;

    let signature = ParcelSignature {
        key_id: secret_key.key_id.clone(),
        algorithm: secret_key.algorithm,
        digest: parcel.config.digest.clone(),
        signature: encode_hex(keypair.sign(parcel.config.digest.as_bytes()).as_ref()),
    };
    let signature_path = signature_path(&parcel.parcel_dir, &signature.key_id);
    if let Some(parent) = signature_path.parent() {
        fs::create_dir_all(parent).map_err(|source| SigningError::CreateDir {
            path: parent.display().to_string(),
            source,
        })?;
    }
    write_json(&signature_path, &signature)?;
    Ok(signature_path)
}

pub fn verify_parcel_signature(
    parcel_path: &Path,
    public_key_path: &Path,
) -> Result<SignatureVerification, SigningError> {
    let parcel = load_parcel(parcel_path).map_err(|error| SigningError::LoadParcel {
        path: parcel_path.display().to_string(),
        message: error.to_string(),
    })?;
    let public_key = read_public_key(public_key_path)?;
    let signature_path = signature_path(&parcel.parcel_dir, &public_key.key_id);
    if !signature_path.exists() {
        return Ok(SignatureVerification {
            key_id: public_key.key_id,
            algorithm: public_key.algorithm,
            signature_found: false,
            digest_matches: false,
            signature_matches: false,
        });
    }

    let signature = read_signature(&signature_path)?;
    let digest_matches = signature.digest == parcel.config.digest;
    let public_key_bytes = decode_hex(
        &public_key.public_key,
        &public_key_path.display().to_string(),
    )?;
    let signature_bytes = decode_hex(&signature.signature, &signature_path.display().to_string())?;
    let signature_matches = digest_matches
        && UnparsedPublicKey::new(&ED25519, &public_key_bytes)
            .verify(parcel.config.digest.as_bytes(), &signature_bytes)
            .is_ok();

    Ok(SignatureVerification {
        key_id: public_key.key_id,
        algorithm: public_key.algorithm,
        signature_found: true,
        digest_matches,
        signature_matches,
    })
}

fn read_secret_key(path: &Path) -> Result<SecretKeyFile, SigningError> {
    let key = read_json::<SecretKeyFile>(path)?;
    validate_key_id(&key.key_id)?;
    validate_algorithm(&key.algorithm)?;
    Ok(key)
}

fn read_public_key(path: &Path) -> Result<PublicKeyFile, SigningError> {
    let key = read_json::<PublicKeyFile>(path)?;
    validate_key_id(&key.key_id)?;
    validate_algorithm(&key.algorithm)?;
    Ok(key)
}

fn read_signature(path: &Path) -> Result<ParcelSignature, SigningError> {
    let signature = read_json::<ParcelSignature>(path)?;
    validate_key_id(&signature.key_id)?;
    validate_algorithm(&signature.algorithm)?;
    Ok(signature)
}

fn read_json<T>(path: &Path) -> Result<T, SigningError>
where
    T: for<'de> Deserialize<'de>,
{
    let body = fs::read_to_string(path).map_err(|source| SigningError::ReadFile {
        path: path.display().to_string(),
        source,
    })?;
    serde_json::from_str(&body).map_err(|source| SigningError::ParseJson {
        path: path.display().to_string(),
        source,
    })
}

fn write_json<T>(path: &Path, value: &T) -> Result<(), SigningError>
where
    T: Serialize,
{
    let body = serde_json::to_string_pretty(value)?;
    fs::write(path, body).map_err(|source| SigningError::WriteFile {
        path: path.display().to_string(),
        source,
    })
}

fn validate_key_id(key_id: &str) -> Result<(), SigningError> {
    if !key_id.is_empty()
        && key_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Ok(());
    }
    Err(SigningError::InvalidKeyId {
        key_id: key_id.to_string(),
    })
}

fn validate_algorithm(algorithm: &str) -> Result<(), SigningError> {
    if algorithm == DISPATCH_SIGNATURE_ALGORITHM {
        Ok(())
    } else {
        Err(SigningError::UnsupportedAlgorithm {
            algorithm: algorithm.to_string(),
        })
    }
}

fn signature_path(parcel_dir: &Path, key_id: &str) -> PathBuf {
    parcel_dir
        .join(PARCEL_SIGNATURES_DIR)
        .join(format!("{key_id}.json"))
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

fn decode_hex(value: &str, path: &str) -> Result<Vec<u8>, SigningError> {
    if !value.len().is_multiple_of(2) {
        return Err(SigningError::InvalidHex {
            path: path.to_string(),
            message: "hex payload must have an even length".to_string(),
        });
    }

    let mut bytes = Vec::with_capacity(value.len() / 2);
    let chars = value.as_bytes().chunks_exact(2);
    for pair in chars {
        let text = std::str::from_utf8(pair).map_err(|_| SigningError::InvalidHex {
            path: path.to_string(),
            message: "hex payload is not valid utf-8".to_string(),
        })?;
        let byte = u8::from_str_radix(text, 16).map_err(|_| SigningError::InvalidHex {
            path: path.to_string(),
            message: format!("invalid hex byte `{text}`"),
        })?;
        bytes.push(byte);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BuildOptions, build_agentfile};
    use tempfile::tempdir;

    #[test]
    fn generated_keypair_can_sign_and_verify_parcel() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("Agentfile"),
            "FROM dispatch/native:latest\nSOUL SOUL.md\nENTRYPOINT chat\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("SOUL.md"), "You are a signed test.\n").unwrap();
        let built = build_agentfile(
            &dir.path().join("Agentfile"),
            &BuildOptions {
                output_root: dir.path().join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let keys = generate_keypair_files(&dir.path().join("keys"), "release").unwrap();
        let signature_path = sign_parcel(&built.parcel_dir, &keys.secret_key_path).unwrap();
        assert!(signature_path.exists());

        let verification =
            verify_parcel_signature(&built.parcel_dir, &keys.public_key_path).unwrap();
        assert!(verification.is_ok());
    }
}
