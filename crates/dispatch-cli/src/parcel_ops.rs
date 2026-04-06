use anyhow::{Context, Result, bail};
use dispatch_core::{
    PullTrustPolicy, SignatureVerification, VerificationReport, generate_keypair_files,
    load_parcel, parse_depot_reference, pull_parcel_verified, push_parcel, sign_parcel,
    verify_parcel, verify_parcel_signature,
};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ParcelListEntry {
    pub digest: String,
    pub name: Option<String>,
    pub version: Option<String>,
    pub courier: String,
    pub entrypoint: Option<String>,
    pub parcel_dir: PathBuf,
    pub manifest_path: PathBuf,
}

pub(crate) fn list(path: PathBuf, emit_json: bool) -> Result<()> {
    let parcels_root = crate::resolve_parcels_root(&path);
    let entries = collect_parcel_entries(&parcels_root)?;

    if emit_json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        print_parcel_list(&parcels_root, &entries);
    }

    Ok(())
}

pub(crate) fn collect_parcel_entries(parcels_root: &Path) -> Result<Vec<ParcelListEntry>> {
    if !parcels_root.exists() {
        return Ok(Vec::new());
    }
    if !parcels_root.is_dir() {
        bail!("parcel store {} is not a directory", parcels_root.display());
    }

    let mut entries = fs::read_dir(parcels_root)
        .with_context(|| format!("failed to read {}", parcels_root.display()))?
        .map(|entry| -> Result<Option<ParcelListEntry>> {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() || !path.join("manifest.json").exists() {
                return Ok(None);
            }

            let parcel = match load_parcel(&path) {
                Ok(parcel) => parcel,
                Err(_) => return Ok(None),
            };
            Ok(Some(ParcelListEntry {
                digest: parcel.config.digest.clone(),
                name: parcel.config.name.clone(),
                version: parcel.config.version.clone(),
                courier: parcel.config.courier.reference().to_string(),
                entrypoint: parcel.config.entrypoint.clone(),
                parcel_dir: parcel.parcel_dir,
                manifest_path: parcel.manifest_path,
            }))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    entries.sort_by(|left, right| left.digest.cmp(&right.digest));
    Ok(entries)
}

pub(crate) fn resolve_parcel_prefix(parcels_root: &Path, prefix: &str) -> Result<PathBuf> {
    let prefix = prefix.to_ascii_lowercase();
    if prefix.len() < 8 {
        bail!("parcel id prefix `{prefix}` is too short; use at least 8 characters");
    }
    if !prefix.chars().all(|ch| ch.is_ascii_hexdigit()) {
        bail!("parcel id prefix `{prefix}` must be hexadecimal");
    }
    if !parcels_root.exists() {
        bail!("parcel store {} does not exist", parcels_root.display());
    }
    if !parcels_root.is_dir() {
        bail!("parcel store {} is not a directory", parcels_root.display());
    }

    let mut matches = fs::read_dir(parcels_root)
        .with_context(|| format!("failed to read {}", parcels_root.display()))?
        .map(|entry| -> Result<Option<PathBuf>> {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                return Ok(None);
            };
            if path.is_dir() && path.join("manifest.json").exists() && name.starts_with(&prefix) {
                return Ok(Some(path));
            }
            Ok(None)
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    matches.sort();
    match matches.as_slice() {
        [] => bail!(
            "no parcel matching id prefix `{prefix}` found in {}",
            parcels_root.display()
        ),
        [path] => Ok(path.clone()),
        _ => bail!(
            "parcel id prefix `{prefix}` is ambiguous in {}",
            parcels_root.display()
        ),
    }
}

pub(crate) fn verify(path: PathBuf, public_keys: Vec<PathBuf>, emit_json: bool) -> Result<()> {
    let report =
        verify_parcel(&path).with_context(|| format!("failed to verify {}", path.display()))?;
    let signature_checks = public_keys
        .iter()
        .map(|public_key| {
            verify_parcel_signature(&path, public_key).with_context(|| {
                format!(
                    "failed to verify detached signature for {} with {}",
                    path.display(),
                    public_key.display()
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;

    if emit_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "integrity": report,
                "signatures": signature_checks,
            }))?
        );
    } else {
        print_verification_report(&report);
        if !signature_checks.is_empty() {
            print_signature_verifications(&signature_checks);
        }
    }

    let signatures_ok = signature_checks.iter().all(SignatureVerification::is_ok);
    if report.is_ok() && signatures_ok {
        Ok(())
    } else {
        bail!("verification failed")
    }
}

pub(crate) fn keygen(key_id: &str, output_dir: Option<PathBuf>) -> Result<()> {
    let output_dir = output_dir.unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".dispatch/keys")
    });
    let generated = generate_keypair_files(&output_dir, key_id)?;
    println!("Secret key: {}", generated.secret_key_path.display());
    println!("Public key: {}", generated.public_key_path.display());
    Ok(())
}

pub(crate) fn sign(path: PathBuf, secret_key: &Path) -> Result<()> {
    let signature = sign_parcel(&path, secret_key).with_context(|| {
        format!(
            "failed to sign {} with {}",
            path.display(),
            secret_key.display()
        )
    })?;
    println!("Signature: {}", signature.display());
    Ok(())
}

pub(crate) fn push(path: PathBuf, reference: &str, emit_json: bool) -> Result<()> {
    let parcel =
        load_parcel(&path).with_context(|| format!("failed to load parcel {}", path.display()))?;
    let reference = parse_depot_reference(reference)
        .with_context(|| format!("invalid depot reference `{reference}`"))?;
    let pushed = push_parcel(&parcel, &reference)?;

    if emit_json {
        println!("{}", serde_json::to_string_pretty(&pushed)?);
    } else {
        println!("Pushed parcel {}", pushed.digest);
        println!("Blob: {}", pushed.blob_location);
        println!("Tag: {}", pushed.tag_location);
    }
    Ok(())
}

pub(crate) fn pull(
    reference: &str,
    output_dir: Option<PathBuf>,
    public_keys: Vec<PathBuf>,
    trust_policy: Option<PathBuf>,
    emit_json: bool,
) -> Result<()> {
    let raw_reference = reference.to_string();
    let reference = parse_depot_reference(reference)
        .with_context(|| format!("invalid depot reference `{reference}`"))?;
    let trust_policy = resolve_trust_policy_path(trust_policy, |name| std::env::var_os(name));
    let trust_policy = trust_policy
        .as_deref()
        .map(PullTrustPolicy::from_path)
        .transpose()?;
    let requirement = trust_policy
        .as_ref()
        .map(|policy| policy.resolve_for_reference(&raw_reference, &reference));
    let public_keys = merge_public_keys(
        public_keys,
        requirement
            .as_ref()
            .map(|requirement| requirement.public_keys.clone())
            .unwrap_or_default(),
    );
    let require_signatures = requirement
        .as_ref()
        .is_some_and(|requirement| requirement.require_signatures);
    let output_root = output_dir.unwrap_or_else(default_pull_output_root);
    let pulled = pull_parcel_verified(
        &reference,
        &raw_reference,
        &output_root,
        &public_keys,
        trust_policy.as_ref(),
    )?;
    let verification = if !public_keys.is_empty() || require_signatures {
        let integrity = verify_parcel(&pulled.parcel_dir).with_context(|| {
            format!(
                "failed to verify pulled parcel {}",
                pulled.parcel_dir.display()
            )
        })?;
        let signature_checks = public_keys
            .iter()
            .map(|public_key| {
                verify_parcel_signature(&pulled.parcel_dir, public_key).with_context(|| {
                    format!(
                        "failed to verify detached signature for {} with {}",
                        pulled.parcel_dir.display(),
                        public_key.display()
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let signatures_ok = signature_checks.iter().all(SignatureVerification::is_ok);
        if !integrity.is_ok() || !signatures_ok {
            print_verification_report(&integrity);
            print_signature_verifications(&signature_checks);
            bail!("pulled parcel failed verification");
        }
        Some((integrity, signature_checks))
    } else {
        None
    };

    if emit_json {
        let (integrity, signatures) = verification
            .map(|(integrity, signatures)| (Some(integrity), Some(signatures)))
            .unwrap_or((None, None));
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "pulled": pulled,
                "integrity": integrity,
                "signatures": signatures,
            }))?
        );
    } else {
        println!("Pulled parcel {}", pulled.digest);
        println!("Parcel dir: {}", pulled.parcel_dir.display());
        println!("Manifest: {}", pulled.manifest_path.display());
    }
    Ok(())
}

fn default_pull_output_root() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".dispatch/parcels")
}

pub(crate) fn resolve_trust_policy_path(
    explicit: Option<PathBuf>,
    env_lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    explicit.or_else(|| env_lookup("DISPATCH_TRUST_POLICY").map(PathBuf::from))
}

pub(crate) fn merge_public_keys(
    explicit_keys: Vec<PathBuf>,
    policy_keys: Vec<PathBuf>,
) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    explicit_keys
        .into_iter()
        .chain(policy_keys)
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

pub(crate) fn print_verification_report(report: &VerificationReport) {
    println!("Digest: {}", report.digest);
    println!(
        "Manifest Digest Matches: {}",
        report.manifest_digest_matches
    );
    println!(
        "Lockfile Digest Matches: {}",
        report.lockfile_digest_matches
    );
    println!(
        "Lockfile Layout Matches: {}",
        report.lockfile_layout_matches
    );
    println!("Lockfile Files Match: {}", report.lockfile_files_match);
    println!("Verified Files: {}", report.verified_files);

    if !report.missing_files.is_empty() {
        println!("Missing Files:");
        for path in &report.missing_files {
            println!("  {path}");
        }
    }

    if !report.modified_files.is_empty() {
        println!("Modified Files:");
        for path in &report.modified_files {
            println!("  {path}");
        }
    }
}

pub(crate) fn print_signature_verifications(verifications: &[SignatureVerification]) {
    println!("Detached Signatures:");
    for verification in verifications {
        println!("  Key ID: {}", verification.key_id);
        println!("  Algorithm: {}", verification.algorithm);
        println!("  Signature Found: {}", verification.signature_found);
        println!("  Digest Matches: {}", verification.digest_matches);
        println!("  Signature Matches: {}", verification.signature_matches);
    }
}

fn print_parcel_list(parcels_root: &Path, entries: &[ParcelListEntry]) {
    if entries.is_empty() {
        println!("No local parcels found in {}", parcels_root.display());
        return;
    }

    println!(
        "{:<12}  {:<24}  {:<10}  {:<18}  PATH",
        "DIGEST", "NAME", "VERSION", "COURIER"
    );
    for entry in entries {
        let short_digest = entry.digest.chars().take(12).collect::<String>();
        println!(
            "{:<12}  {:<24}  {:<10}  {:<18}  {}",
            short_digest,
            entry.name.as_deref().unwrap_or("-"),
            entry.version.as_deref().unwrap_or("-"),
            entry.courier,
            entry.parcel_dir.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{collect_parcel_entries, resolve_parcel_prefix};
    use dispatch_core::{BuildOptions, build_agentfile};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn collect_parcel_entries_reads_local_store_metadata() {
        let dir = tempdir().unwrap();
        let source_dir = dir.path().join("agent");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(
            source_dir.join("Agentfile"),
            "\
FROM dispatch/native:latest
NAME parcel-list-test
VERSION 1.2.3
ENTRYPOINT chat
",
        )
        .unwrap();

        let built = build_agentfile(
            &source_dir.join("Agentfile"),
            &BuildOptions {
                output_root: source_dir.join(".dispatch/parcels"),
            },
        )
        .unwrap();

        let entries = collect_parcel_entries(&source_dir.join(".dispatch/parcels")).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].digest, built.digest);
        assert_eq!(entries[0].name.as_deref(), Some("parcel-list-test"));
        assert_eq!(entries[0].version.as_deref(), Some("1.2.3"));
        assert_eq!(entries[0].courier, "dispatch/native:latest");
    }

    #[test]
    fn resolve_parcel_prefix_returns_unique_match() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("parcels");
        fs::create_dir_all(&root).unwrap();
        let digest = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
        let parcel_dir = root.join(digest);
        fs::create_dir_all(&parcel_dir).unwrap();
        fs::write(parcel_dir.join("manifest.json"), "{}").unwrap();

        let resolved = resolve_parcel_prefix(&root, "abcdef12").unwrap();

        assert_eq!(resolved, parcel_dir);
    }

    #[test]
    fn collect_parcel_entries_skips_invalid_parcel_directories() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("parcels");
        let invalid = root.join("deadbeefdeadbeef");
        fs::create_dir_all(&invalid).unwrap();
        fs::write(invalid.join("manifest.json"), "{}").unwrap();

        let entries = collect_parcel_entries(&root).unwrap();

        assert!(entries.is_empty());
    }
}
