use crate::{LoadedParcel, load_parcel};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Cursor, Read},
    path::{Path, PathBuf},
};
use tar::{Archive, Builder};
use thiserror::Error;
use urlencoding::encode;
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
    Http { base_url: String },
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
    pub blob_location: String,
    pub tag_location: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PulledParcel {
    pub digest: String,
    pub parcel_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub source_tag_location: String,
}

#[derive(Debug, Error)]
pub enum DepotError {
    #[error("invalid depot reference `{reference}`: expected `<locator>::<repository>:<tag>`")]
    InvalidReferenceFormat { reference: String },
    #[error(
        "unsupported depot locator `{locator}`; only file://, http://, and https:// locators are supported"
    )]
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
    #[error("failed to create archive for `{path}`: {source}")]
    ArchiveCreate {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to unpack archive into `{path}`: {source}")]
    ArchiveExtract {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("archive entry `{path}` is invalid")]
    InvalidArchivePath { path: String },
    #[error("request to `{url}` failed: {source}")]
    HttpRequest {
        url: String,
        #[source]
        source: ureq::Error,
    },
    #[error("request to `{url}` returned status {status}: {body}")]
    HttpStatus {
        url: String,
        status: u16,
        body: String,
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
    match &reference.locator {
        DepotLocator::File { .. } => push_file_parcel(parcel, reference),
        DepotLocator::Http { .. } => push_http_parcel(parcel, reference),
    }
}

pub fn pull_parcel(
    reference: &DepotReference,
    output_root: &Path,
) -> Result<PulledParcel, DepotError> {
    match &reference.locator {
        DepotLocator::File { .. } => pull_file_parcel(reference, output_root),
        DepotLocator::Http { .. } => pull_http_parcel(reference, output_root),
    }
}

fn push_file_parcel(
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
        blob_location: blob_dir.display().to_string(),
        tag_location: tag_path.display().to_string(),
    })
}

fn pull_file_parcel(
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
        source_tag_location: tag_path.display().to_string(),
    })
}

impl DepotReference {
    pub fn blob_dir(&self, digest: &str) -> PathBuf {
        match &self.locator {
            DepotLocator::File { root } => root.join("blobs/parcels").join(digest),
            DepotLocator::Http { .. } => PathBuf::from(self.blob_location(digest)),
        }
    }

    pub fn tag_path(&self) -> PathBuf {
        match &self.locator {
            DepotLocator::File { root } => root
                .join("refs")
                .join(Path::new(&self.repository))
                .join("tags")
                .join(format!("{}.json", self.tag)),
            DepotLocator::Http { .. } => PathBuf::from(self.tag_location()),
        }
    }

    pub fn blob_location(&self, digest: &str) -> String {
        match &self.locator {
            DepotLocator::File { root } => root
                .join("blobs/parcels")
                .join(digest)
                .display()
                .to_string(),
            DepotLocator::Http { base_url } => format!("{base_url}/v1/parcels/{digest}.tar"),
        }
    }

    pub fn tag_location(&self) -> String {
        match &self.locator {
            DepotLocator::File { root } => root
                .join("refs")
                .join(Path::new(&self.repository))
                .join("tags")
                .join(format!("{}.json", self.tag))
                .display()
                .to_string(),
            DepotLocator::Http { base_url } => format!(
                "{base_url}/v1/tags?repository={}&tag={}",
                encode(&self.repository),
                encode(&self.tag)
            ),
        }
    }
}

fn parse_depot_locator(locator: &str) -> Result<DepotLocator, DepotError> {
    if let Some(path) = locator.strip_prefix("file://") {
        if path.is_empty() {
            return Err(DepotError::UnsupportedLocator {
                locator: locator.to_string(),
            });
        }
        return Ok(DepotLocator::File {
            root: PathBuf::from(path),
        });
    }
    if locator.starts_with("http://") || locator.starts_with("https://") {
        return Ok(DepotLocator::Http {
            base_url: locator.trim_end_matches('/').to_string(),
        });
    }
    Err(DepotError::UnsupportedLocator {
        locator: locator.to_string(),
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

fn push_http_parcel(
    parcel: &LoadedParcel,
    reference: &DepotReference,
) -> Result<PushedParcel, DepotError> {
    let blob_location = reference.blob_location(&parcel.config.digest);
    let tag_location = reference.tag_location();
    let archive = archive_parcel_tree(&parcel.parcel_dir)?;
    let response = ureq::put(&blob_location)
        .header("content-type", "application/x-tar")
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
    let response = ureq::put(&tag_location)
        .header("content-type", "application/json")
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

fn pull_http_parcel(
    reference: &DepotReference,
    output_root: &Path,
) -> Result<PulledParcel, DepotError> {
    let tag_location = reference.tag_location();
    let tag_response =
        ureq::get(&tag_location)
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
    let blob_response =
        ureq::get(&blob_location)
            .call()
            .map_err(|source| DepotError::HttpRequest {
                url: blob_location.clone(),
                source,
            })?;
    let blob_response = ensure_http_success(blob_response, &blob_location)?;
    let blob_bytes = read_http_body(blob_response, &blob_location)?;

    let parcel_dir = output_root.join(&tag_record.digest);
    if !parcel_dir.exists() {
        unpack_parcel_archive(&blob_bytes, &parcel_dir)?;
    }

    let loaded = load_parcel(&parcel_dir)?;
    Ok(PulledParcel {
        digest: loaded.config.digest,
        parcel_dir: loaded.parcel_dir.clone(),
        manifest_path: loaded.manifest_path.clone(),
        source_tag_location: tag_location,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BuildOptions, BuiltParcel, ParcelManifest, build_agentfile, verify_parcel};
    use std::{
        collections::HashMap,
        io::{BufRead, BufReader, Write},
        net::{TcpListener, TcpStream},
        sync::mpsc,
        thread,
        time::Duration,
    };
    use tempfile::tempdir;
    use urlencoding::decode;

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

    struct HttpDepotServer {
        base_url: String,
        shutdown: mpsc::Sender<()>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl Drop for HttpDepotServer {
        fn drop(&mut self) {
            let _ = self.shutdown.send(());
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn start_http_depot(root: PathBuf) -> HttpDepotServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
        let handle = thread::spawn(move || {
            loop {
                if shutdown_rx.try_recv().is_ok() {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => handle_http_depot_connection(stream, &root),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("failed to accept depot connection: {error}"),
                }
            }
        });
        HttpDepotServer {
            base_url,
            shutdown: shutdown_tx,
            handle: Some(handle),
        }
    }

    fn handle_http_depot_connection(stream: TcpStream, root: &Path) {
        stream.set_nonblocking(false).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap() == 0 {
            return;
        }
        let request_line = request_line.trim_end();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();

        let mut content_length = 0usize;
        loop {
            let mut header_line = String::new();
            reader.read_line(&mut header_line).unwrap();
            let header_line = header_line.trim_end();
            if header_line.is_empty() {
                break;
            }
            if let Some((name, value)) = header_line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap();
                }
            }
        }

        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).unwrap();

        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        match (method, path) {
            ("PUT", path) if path.starts_with("/v1/parcels/") && path.ends_with(".tar") => {
                let digest = path
                    .trim_start_matches("/v1/parcels/")
                    .trim_end_matches(".tar");
                let blob_path = root.join("blobs/parcels").join(format!("{digest}.tar"));
                if let Some(parent) = blob_path.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::write(blob_path, body).unwrap();
                write_http_response(&mut writer, 201, "created", "text/plain", b"ok");
            }
            ("GET", path) if path.starts_with("/v1/parcels/") && path.ends_with(".tar") => {
                let digest = path
                    .trim_start_matches("/v1/parcels/")
                    .trim_end_matches(".tar");
                let blob_path = root.join("blobs/parcels").join(format!("{digest}.tar"));
                match fs::read(blob_path) {
                    Ok(bytes) => {
                        write_http_response(&mut writer, 200, "ok", "application/x-tar", &bytes)
                    }
                    Err(_) => write_http_response(
                        &mut writer,
                        404,
                        "not found",
                        "text/plain",
                        b"missing blob",
                    ),
                }
            }
            ("PUT", "/v1/tags") => {
                let params = parse_query_string(query);
                let repository = params.get("repository").cloned().unwrap_or_default();
                let tag = params.get("tag").cloned().unwrap_or_default();
                let tag_path = root
                    .join("refs")
                    .join(Path::new(&repository))
                    .join("tags")
                    .join(format!("{tag}.json"));
                if let Some(parent) = tag_path.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::write(tag_path, body).unwrap();
                write_http_response(&mut writer, 201, "created", "application/json", b"{}");
            }
            ("GET", "/v1/tags") => {
                let params = parse_query_string(query);
                let repository = params.get("repository").cloned().unwrap_or_default();
                let tag = params.get("tag").cloned().unwrap_or_default();
                let tag_path = root
                    .join("refs")
                    .join(Path::new(&repository))
                    .join("tags")
                    .join(format!("{tag}.json"));
                match fs::read(tag_path) {
                    Ok(bytes) => {
                        write_http_response(&mut writer, 200, "ok", "application/json", &bytes)
                    }
                    Err(_) => write_http_response(
                        &mut writer,
                        404,
                        "not found",
                        "text/plain",
                        b"missing tag",
                    ),
                }
            }
            _ => write_http_response(&mut writer, 404, "not found", "text/plain", b"not found"),
        }
    }

    fn parse_query_string(query: &str) -> HashMap<String, String> {
        query
            .split('&')
            .filter(|segment| !segment.is_empty())
            .filter_map(|segment| {
                let (key, value) = segment.split_once('=').unwrap_or((segment, ""));
                let key = decode(key).ok()?.into_owned();
                let value = decode(value).ok()?.into_owned();
                Some((key, value))
            })
            .collect()
    }

    fn write_http_response(
        writer: &mut TcpStream,
        status: u16,
        reason: &str,
        content_type: &str,
        body: &[u8],
    ) {
        write!(
            writer,
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        writer.write_all(body).unwrap();
        writer.flush().unwrap();
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
    fn parses_http_depot_reference() {
        let reference =
            parse_depot_reference("https://depot.dispatch.run::acme/monitor:v1").unwrap();
        assert_eq!(
            reference,
            DepotReference {
                locator: DepotLocator::Http {
                    base_url: "https://depot.dispatch.run".to_string(),
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
        let output_root = dir.path().join("pulled/nested/output");
        let reference =
            parse_depot_reference(&format!("file://{}::acme/monitor:v1", depot_root.display()))
                .unwrap();

        let pushed = push_parcel(&parcel, &reference).unwrap();
        assert_eq!(pushed.digest, parcel.config.digest);
        assert!(
            Path::new(&pushed.blob_location)
                .join("manifest.json")
                .exists()
        );
        assert!(Path::new(&pushed.tag_location).exists());

        let pulled = pull_parcel(&reference, &output_root).unwrap();
        assert_eq!(pulled.digest, parcel.config.digest);
        assert!(pulled.manifest_path.exists());
        assert!(verify_parcel(&pulled.parcel_dir).unwrap().is_ok());

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
    fn push_and_pull_round_trip_http_depot() {
        let dir = tempdir().unwrap();
        let built = build_fixture(dir.path());
        let parcel = load_parcel(&built.parcel_dir).unwrap();
        let server_root = dir.path().join("http-depot");
        let server = start_http_depot(server_root.clone());
        let output_root = dir.path().join("pulled/http");
        let reference =
            parse_depot_reference(&format!("{}::acme/monitor:v1", server.base_url)).unwrap();

        let pushed = push_parcel(&parcel, &reference).unwrap();
        assert_eq!(pushed.digest, parcel.config.digest);
        assert!(pushed.blob_location.starts_with(&server.base_url));
        assert!(pushed.tag_location.starts_with(&server.base_url));

        let pulled = pull_parcel(&reference, &output_root).unwrap();
        assert_eq!(pulled.digest, parcel.config.digest);
        assert!(pulled.manifest_path.exists());
        assert!(verify_parcel(&pulled.parcel_dir).unwrap().is_ok());

        let tag_record: DepotTagRecord = serde_json::from_slice(
            &fs::read(server_root.join("refs/acme/monitor/tags/v1.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(tag_record.digest, parcel.config.digest);
        assert!(
            server_root
                .join("blobs/parcels")
                .join(format!("{}.tar", parcel.config.digest))
                .exists()
        );
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
