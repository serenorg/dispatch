use anyhow::{Context, Result, bail};
use dispatch_core::{
    PullTrustPolicy, SignatureVerification, VerificationReport, generate_keypair_files,
    load_parcel, parse_depot_reference, pull_parcel_verified, push_parcel, sign_parcel,
    verify_parcel, verify_parcel_signature,
};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

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
