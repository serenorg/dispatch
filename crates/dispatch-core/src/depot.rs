use crate::{LoadedParcel, load_parcel};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use thiserror::Error;
use walkdir::WalkDir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepotReference {
    pub locator: DepotLocator,
    pub repository: String,
    pub tag: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepotLocator {
    File { root: PathBuf },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DepotTagRecord {
    pub repository: String,
    pub tag: String,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushedParcel {
    pub digest: String,
    pub parcel_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub tag_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PulledParcel {
    pub digest: String,
    pub parcel_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub source_tag_path: PathBuf,
}

#[derive(Debug, Error)]
pub enum DepotError {
    #[error("invalid depot reference `{reference}`: expected `<locator>::<repository>:<tag>`")]
    InvalidReferenceFormat { reference: String },
    #[error("unsupported depot locator `{locator}`; only file:// locators are supported")]
    UnsupportedLocator { locator: String },
    #[error("invalid parcel reference `{reference}`: repository and tag are required")]
    InvalidParcelReference { reference: String },
    #[error("depot tag `{path}` does not exist")]
    MissingTag { path: String },
    #[error("depot parcel blob `{path}` does not exist")]
    MissingBlob { path: String },
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
    #[error("failed to parse depot tag record `{path}`: {source}")]
    ParseTagRecord {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("walk error for `{path}`: {source}")]
    Walk {
        path: String,
        #[source]
        source: walkdir::Error,
    },
    #[error(transparent)]
    LoadParcel(#[from] crate::CourierError),
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
}

pub fn parse_depot_reference(reference: &str) -> Result<DepotReference, DepotError> {
    let (locator, parcel_ref) =
        reference
            .split_once("::")
            .ok_or_else(|| DepotError::InvalidReferenceFormat {
                reference: reference.to_string(),
            })?;

    let locator = parse_depot_locator(locator)?;
    let (repository, tag) = parse_parcel_ref(parcel_ref)?;

    Ok(DepotReference {
        locator,
        repository,
        tag,
    })
}

pub fn push_parcel(
    parcel: &LoadedParcel,
    reference: &DepotReference,
) -> Result<PushedParcel, DepotError> {
    let blob_dir = reference.blob_dir(&parcel.config.digest);
    if !blob_dir.exists() {
        copy_tree(&parcel.parcel_dir, &blob_dir)?;
    }

    let tag_path = reference.tag_path();
    if let Some(parent) = tag_path.parent() {
        fs::create_dir_all(parent).map_err(|source| DepotError::CreateDir {
            path: parent.display().to_string(),
            source,
        })?;
    }

    let tag_record = DepotTagRecord {
        repository: reference.repository.clone(),
        tag: reference.tag.clone(),
        digest: parcel.config.digest.clone(),
    };
    fs::write(&tag_path, serde_json::to_vec_pretty(&tag_record)?).map_err(|source| {
        DepotError::WriteFile {
            path: tag_path.display().to_string(),
            source,
        }
    })?;

    Ok(PushedParcel {
        digest: parcel.config.digest.clone(),
        parcel_dir: blob_dir.clone(),
        manifest_path: blob_dir.join("manifest.json"),
        tag_path,
    })
}

pub fn pull_parcel(
    reference: &DepotReference,
    output_root: &Path,
) -> Result<PulledParcel, DepotError> {
    let tag_path = reference.tag_path();
    if !tag_path.exists() {
        return Err(DepotError::MissingTag {
            path: tag_path.display().to_string(),
        });
    }

    let tag_record: DepotTagRecord =
        serde_json::from_slice(&fs::read(&tag_path).map_err(|source| DepotError::ReadFile {
            path: tag_path.display().to_string(),
            source,
        })?)
        .map_err(|source| DepotError::ParseTagRecord {
            path: tag_path.display().to_string(),
            source,
        })?;

    let source_blob = reference.blob_dir(&tag_record.digest);
    if !source_blob.exists() {
        return Err(DepotError::MissingBlob {
            path: source_blob.display().to_string(),
        });
    }

    let parcel_dir = output_root.join(&tag_record.digest);
    if !parcel_dir.exists() {
        copy_tree(&source_blob, &parcel_dir)?;
    }

    let loaded = load_parcel(&parcel_dir)?;
    Ok(PulledParcel {
        digest: loaded.config.digest,
        parcel_dir: loaded.parcel_dir.clone(),
        manifest_path: loaded.manifest_path.clone(),
        source_tag_path: tag_path,
    })
}

impl DepotReference {
    pub fn blob_dir(&self, digest: &str) -> PathBuf {
        match &self.locator {
            DepotLocator::File { root } => root.join("blobs/parcels").join(digest),
        }
    }

    pub fn tag_path(&self) -> PathBuf {
        match &self.locator {
            DepotLocator::File { root } => root
                .join("refs")
                .join(Path::new(&self.repository))
                .join("tags")
                .join(format!("{}.json", self.tag)),
        }
    }
}

fn parse_depot_locator(locator: &str) -> Result<DepotLocator, DepotError> {
    let Some(path) = locator.strip_prefix("file://") else {
        return Err(DepotError::UnsupportedLocator {
            locator: locator.to_string(),
        });
    };
    if path.is_empty() {
        return Err(DepotError::UnsupportedLocator {
            locator: locator.to_string(),
        });
    }
    Ok(DepotLocator::File {
        root: PathBuf::from(path),
    })
}

fn parse_parcel_ref(parcel_ref: &str) -> Result<(String, String), DepotError> {
    let last_slash = parcel_ref.rfind('/');
    let last_colon = parcel_ref.rfind(':');
    let Some(colon_index) = last_colon else {
        return Err(DepotError::InvalidParcelReference {
            reference: parcel_ref.to_string(),
        });
    };
    if last_slash.is_some_and(|slash| colon_index < slash) {
        return Err(DepotError::InvalidParcelReference {
            reference: parcel_ref.to_string(),
        });
    }
    let repository = &parcel_ref[..colon_index];
    let tag = &parcel_ref[colon_index + 1..];
    if repository.is_empty()
        || tag.is_empty()
        || repository.starts_with('/')
        || repository.ends_with('/')
        || repository.split('/').any(|segment| segment.is_empty())
    {
        return Err(DepotError::InvalidParcelReference {
            reference: parcel_ref.to_string(),
        });
    }
    Ok((repository.to_string(), tag.to_string()))
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), DepotError> {
    for entry in WalkDir::new(source) {
        let entry = entry.map_err(|source_error| DepotError::Walk {
            path: source.display().to_string(),
            source: source_error,
        })?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .expect("walk entry under source");
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).map_err(|source_error| DepotError::CreateDir {
                path: target.display().to_string(),
                source: source_error,
            })?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|source_error| DepotError::CreateDir {
                path: parent.display().to_string(),
                source: source_error,
            })?;
        }
        fs::copy(entry.path(), &target).map_err(|source_error| DepotError::WriteFile {
            path: target.display().to_string(),
            source: source_error,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BuildOptions, BuiltParcel, ParcelManifest, build_agentfile};
    use tempfile::tempdir;

    fn build_fixture(root: &Path) -> BuiltParcel {
        let context_dir = root.join("fixture");
        fs::create_dir_all(&context_dir).unwrap();
        fs::write(
            context_dir.join("Agentfile"),
            "FROM dispatch/native:latest\nNAME depot-test\nSKILL SKILL.md\nENTRYPOINT chat\n",
        )
        .unwrap();
        fs::write(context_dir.join("SKILL.md"), "You are a depot test.\n").unwrap();

        build_agentfile(
            &context_dir.join("Agentfile"),
            &BuildOptions {
                output_root: context_dir.join(".dispatch/parcels"),
            },
        )
        .unwrap()
    }

    #[test]
    fn parses_file_depot_reference() {
        let reference =
            parse_depot_reference("file:///tmp/dispatch-depot::acme/monitor:v1").unwrap();
        assert_eq!(
            reference,
            DepotReference {
                locator: DepotLocator::File {
                    root: PathBuf::from("/tmp/dispatch-depot"),
                },
                repository: "acme/monitor".to_string(),
                tag: "v1".to_string(),
            }
        );
    }

    #[test]
    fn rejects_invalid_depot_reference_shapes() {
        assert!(matches!(
            parse_depot_reference("file:///tmp/dispatch-depot"),
            Err(DepotError::InvalidReferenceFormat { .. })
        ));
        assert!(matches!(
            parse_depot_reference("file:///tmp/dispatch-depot::acme/monitor"),
            Err(DepotError::InvalidParcelReference { .. })
        ));
    }

    #[test]
    fn push_and_pull_round_trip_parcel() {
        let dir = tempdir().unwrap();
        let built = build_fixture(dir.path());
        let parcel = load_parcel(&built.parcel_dir).unwrap();
        let depot_root = dir.path().join("depot");
        let output_root = dir.path().join("pulled");
        let reference =
            parse_depot_reference(&format!("file://{}::acme/monitor:v1", depot_root.display()))
                .unwrap();

        let pushed = push_parcel(&parcel, &reference).unwrap();
        assert_eq!(pushed.digest, parcel.config.digest);
        assert!(pushed.manifest_path.exists());
        assert!(pushed.tag_path.exists());

        let pulled = pull_parcel(&reference, &output_root).unwrap();
        assert_eq!(pulled.digest, parcel.config.digest);
        assert!(pulled.manifest_path.exists());

        let pulled_manifest: ParcelManifest =
            serde_json::from_slice(&fs::read(&pulled.manifest_path).unwrap()).unwrap();
        assert_eq!(pulled_manifest.digest, parcel.config.digest);

        let tag_record: DepotTagRecord =
            serde_json::from_slice(&fs::read(reference.tag_path()).unwrap()).unwrap();
        assert_eq!(tag_record.repository, "acme/monitor");
        assert_eq!(tag_record.tag, "v1");
        assert_eq!(tag_record.digest, parcel.config.digest);
    }

    #[test]
    fn pull_reports_missing_tags() {
        let dir = tempdir().unwrap();
        let reference = parse_depot_reference(&format!(
            "file://{}::acme/missing:v1",
            dir.path().join("depot").display()
        ))
        .unwrap();
        let error = pull_parcel(&reference, &dir.path().join("out")).unwrap_err();
        assert!(matches!(error, DepotError::MissingTag { .. }));
    }
}
