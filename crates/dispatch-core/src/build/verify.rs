use super::*;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParcelLock {
    pub format_version: u32,
    pub digest: String,
    pub manifest: String,
    pub context_dir: String,
    pub files: Vec<ParcelFileRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationReport {
    pub digest: String,
    pub manifest_digest_matches: bool,
    pub lockfile_digest_matches: bool,
    pub lockfile_layout_matches: bool,
    pub lockfile_files_match: bool,
    pub verified_files: usize,
    pub missing_files: Vec<String>,
    pub modified_files: Vec<String>,
}

impl VerificationReport {
    pub fn is_ok(&self) -> bool {
        self.manifest_digest_matches
            && self.lockfile_digest_matches
            && self.lockfile_layout_matches
            && self.lockfile_files_match
            && self.missing_files.is_empty()
            && self.modified_files.is_empty()
    }
}

pub fn verify_parcel(parcel_path: &Path) -> Result<VerificationReport, BuildError> {
    let manifest_path = resolve_manifest_path(parcel_path);
    let parcel_dir =
        manifest_path
            .parent()
            .map(PathBuf::from)
            .ok_or_else(|| BuildError::MissingPath {
                path: manifest_path.display().to_string(),
            })?;
    let parcel: ParcelManifest =
        serde_json::from_slice(&fs::read(&manifest_path).map_err(|source| {
            BuildError::ReadFile {
                path: manifest_path.display().to_string(),
                source,
            }
        })?)?;
    let lockfile_path = parcel_dir.join("parcel.lock");
    let lockfile: ParcelLock =
        serde_json::from_slice(&fs::read(&lockfile_path).map_err(|source| {
            BuildError::ReadFile {
                path: lockfile_path.display().to_string(),
                source,
            }
        })?)?;

    let expected_digest = provisional_digest(&parcel)?;
    let mut missing_files = Vec::new();
    let mut modified_files = Vec::new();
    for file in &parcel.files {
        let path = parcel_dir.join("context").join(&file.packaged_as);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing_files.push(file.packaged_as.clone());
                continue;
            }
            Err(source) => {
                return Err(BuildError::ReadFile {
                    path: path.display().to_string(),
                    source,
                });
            }
        };

        if hex_digest(&bytes) != file.sha256 || bytes.len() as u64 != file.size_bytes {
            modified_files.push(file.packaged_as.clone());
        }
    }

    Ok(VerificationReport {
        digest: parcel.digest.clone(),
        manifest_digest_matches: parcel.digest == expected_digest,
        lockfile_digest_matches: lockfile.digest == parcel.digest,
        lockfile_layout_matches: lockfile.format_version == parcel.format_version
            && lockfile.manifest == "manifest.json"
            && lockfile.context_dir == "context",
        lockfile_files_match: lockfile.files == parcel.files,
        verified_files: parcel.files.len(),
        missing_files,
        modified_files,
    })
}

pub(super) fn resolve_manifest_path(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.join("manifest.json")
    } else {
        path.to_path_buf()
    }
}
