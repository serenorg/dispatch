use super::*;
#[cfg(test)]
use std::sync::{Mutex, OnceLock};
use std::{fs, io::Cursor};
use tar::{Archive, Builder};

pub(super) fn push_http_parcel(
    parcel: &LoadedParcel,
    reference: &DepotReference,
) -> Result<PushedParcel, DepotError> {
    let blob_location = reference.blob_location(&parcel.config.digest);
    let tag_location = reference.tag_location();
    let archive = archive_parcel_tree(&parcel.parcel_dir)?;
    let mut blob_request = ureq::put(&blob_location).header("content-type", "application/x-tar");
    if let Some(token) = depot_auth_token() {
        blob_request = blob_request.header("authorization", &format!("Bearer {token}"));
    }
    let response = blob_request
        .send(&archive[..])
        .map_err(|source| DepotError::HttpRequest {
            url: blob_location.clone(),
            source,
        })?;
    ensure_http_success(response, &blob_location)?;

    let tag_record = DepotTagRecord {
        repository: reference.repository.clone(),
        tag: reference.tag.clone(),
        digest: parcel.config.digest.clone(),
    };
    let mut tag_request = ureq::put(&tag_location).header("content-type", "application/json");
    if let Some(token) = depot_auth_token() {
        tag_request = tag_request.header("authorization", &format!("Bearer {token}"));
    }
    let response = tag_request
        .send_json(tag_record)
        .map_err(|source| DepotError::HttpRequest {
            url: tag_location.clone(),
            source,
        })?;
    ensure_http_success(response, &tag_location)?;

    Ok(PushedParcel {
        digest: parcel.config.digest.clone(),
        blob_location,
        tag_location,
    })
}

pub(super) fn pull_http_parcel(
    reference: &DepotReference,
    output_root: &Path,
    public_keys: &[PathBuf],
    require_signatures: bool,
) -> Result<PulledParcel, DepotError> {
    let tag_location = reference.tag_location();
    let mut tag_request = ureq::get(&tag_location);
    if let Some(token) = depot_auth_token() {
        tag_request = tag_request.header("authorization", &format!("Bearer {token}"));
    }
    let tag_response = tag_request
        .call()
        .map_err(|source| DepotError::HttpRequest {
            url: tag_location.clone(),
            source,
        })?;
    let tag_response = ensure_http_success(tag_response, &tag_location)?;
    let tag_bytes = read_http_body(tag_response, &tag_location)?;
    let tag_record: DepotTagRecord =
        serde_json::from_slice(&tag_bytes).map_err(|source| DepotError::ParseTagRecord {
            path: tag_location.clone(),
            source,
        })?;

    let blob_location = reference.blob_location(&tag_record.digest);
    let mut blob_request = ureq::get(&blob_location);
    if let Some(token) = depot_auth_token() {
        blob_request = blob_request.header("authorization", &format!("Bearer {token}"));
    }
    let blob_response = blob_request
        .call()
        .map_err(|source| DepotError::HttpRequest {
            url: blob_location.clone(),
            source,
        })?;
    let blob_response = ensure_http_success(blob_response, &blob_location)?;
    let blob_bytes = read_http_body(blob_response, &blob_location)?;

    let parcel_dir = output_root.join(&tag_record.digest);
    if parcel_dir.exists() {
        verify_pulled_parcel(&parcel_dir, public_keys, require_signatures)?;
    } else {
        let staging_dir = staging_parcel_dir(output_root, &tag_record.digest);
        if staging_dir.exists() {
            fs::remove_dir_all(&staging_dir).map_err(|source| DepotError::WriteFile {
                path: staging_dir.display().to_string(),
                source,
            })?;
        }
        unpack_parcel_archive(&blob_bytes, &staging_dir)?;
        if let Err(error) = verify_pulled_parcel(&staging_dir, public_keys, require_signatures) {
            let _ = fs::remove_dir_all(&staging_dir);
            return Err(error);
        }
        if let Some(parent) = parcel_dir.parent() {
            fs::create_dir_all(parent).map_err(|source| DepotError::CreateDir {
                path: parent.display().to_string(),
                source,
            })?;
        }
        fs::rename(&staging_dir, &parcel_dir).map_err(|source| DepotError::WriteFile {
            path: parcel_dir.display().to_string(),
            source,
        })?;
    }

    let loaded = load_parcel(&parcel_dir)?;
    Ok(PulledParcel {
        digest: loaded.config.digest,
        parcel_dir: loaded.parcel_dir.clone(),
        manifest_path: loaded.manifest_path.clone(),
        source_tag_location: tag_location,
    })
}

fn depot_auth_token() -> Option<String> {
    #[cfg(test)]
    if let Some(override_token) = test_depot_auth_token_override() {
        return Some(override_token);
    }
    std::env::var("DISPATCH_DEPOT_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
static TEST_DEPOT_AUTH_TOKEN: OnceLock<Mutex<Option<String>>> = OnceLock::new();

#[cfg(test)]
fn test_depot_auth_token_override() -> Option<String> {
    TEST_DEPOT_AUTH_TOKEN
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|token| token.clone())
}

#[cfg(test)]
pub(super) fn set_test_depot_auth_token(token: Option<&str>) {
    let store = TEST_DEPOT_AUTH_TOKEN.get_or_init(|| Mutex::new(None));
    *store
        .lock()
        .expect("depot auth token override lock poisoned") = token.map(ToString::to_string);
}

fn archive_parcel_tree(source: &Path) -> Result<Vec<u8>, DepotError> {
    let mut builder = Builder::new(Vec::new());
    builder
        .append_dir_all(".", source)
        .map_err(|source_error| DepotError::ArchiveCreate {
            path: source.display().to_string(),
            source: source_error,
        })?;
    builder
        .into_inner()
        .map_err(|source_error| DepotError::ArchiveCreate {
            path: source.display().to_string(),
            source: source_error,
        })
}

fn unpack_parcel_archive(bytes: &[u8], destination: &Path) -> Result<(), DepotError> {
    validate_archive_entries(bytes)?;
    fs::create_dir_all(destination).map_err(|source| DepotError::CreateDir {
        path: destination.display().to_string(),
        source,
    })?;
    let mut archive = Archive::new(Cursor::new(bytes));
    archive
        .unpack(destination)
        .map_err(|source| DepotError::ArchiveExtract {
            path: destination.display().to_string(),
            source,
        })?;
    Ok(())
}

fn validate_archive_entries(bytes: &[u8]) -> Result<(), DepotError> {
    let mut archive = Archive::new(Cursor::new(bytes));
    for entry in archive
        .entries()
        .map_err(|source| DepotError::ArchiveExtract {
            path: "<archive>".to_string(),
            source,
        })?
    {
        let entry = entry.map_err(|source| DepotError::ArchiveExtract {
            path: "<archive>".to_string(),
            source,
        })?;
        let path = entry.path().map_err(|source| DepotError::ArchiveExtract {
            path: "<archive>".to_string(),
            source,
        })?;
        if path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(DepotError::InvalidArchivePath {
                path: path.display().to_string(),
            });
        }
    }
    Ok(())
}

fn ensure_http_success(
    mut response: ureq::http::Response<ureq::Body>,
    url: &str,
) -> Result<ureq::http::Response<ureq::Body>, DepotError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let body = response.body_mut().read_to_string().unwrap_or_default();
    Err(DepotError::HttpStatus {
        url: url.to_string(),
        status: response.status().as_u16(),
        body,
    })
}

fn read_http_body(
    mut response: ureq::http::Response<ureq::Body>,
    url: &str,
) -> Result<Vec<u8>, DepotError> {
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .read_to_end(&mut bytes)
        .map_err(|source| DepotError::ReadFile {
            path: url.to_string(),
            source,
        })?;
    Ok(bytes)
}
