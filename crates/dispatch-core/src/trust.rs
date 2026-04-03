use crate::DepotReference;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PullTrustPolicy {
    #[serde(default)]
    pub rules: Vec<PullTrustRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PullTrustRule {
    #[serde(default)]
    pub reference_prefix: Option<String>,
    #[serde(default)]
    pub repository_prefix: Option<String>,
    #[serde(default)]
    pub public_keys: Vec<PathBuf>,
    #[serde(default)]
    pub require_signatures: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullTrustRequirement {
    pub public_keys: Vec<PathBuf>,
    pub require_signatures: bool,
}

#[derive(Debug, Error)]
pub enum TrustPolicyError {
    #[error("failed to read trust policy `{path}`: {source}")]
    ReadPolicy {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse trust policy `{path}`: {source}")]
    ParsePolicy {
        path: String,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("trust policy rule {index} must set `reference_prefix`, `repository_prefix`, or both")]
    MissingRuleMatcher { index: usize },
    #[error("trust policy rule {index} requires signatures but declares no public keys")]
    MissingRuleKeys { index: usize },
    #[error("trust policy public key `{path}` does not exist")]
    MissingPublicKey { path: String },
    #[error("failed to read trust policy public key `{path}`: {source}")]
    ReadPublicKey {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl PullTrustPolicy {
    pub fn from_path(path: &Path) -> Result<Self, TrustPolicyError> {
        let source = fs::read_to_string(path).map_err(|source| TrustPolicyError::ReadPolicy {
            path: path.display().to_string(),
            source,
        })?;
        let mut policy: PullTrustPolicy =
            serde_yaml::from_str(&source).map_err(|source| TrustPolicyError::ParsePolicy {
                path: path.display().to_string(),
                source,
            })?;
        let root = path.parent().unwrap_or_else(|| Path::new("."));
        for (index, rule) in policy.rules.iter_mut().enumerate() {
            if rule.reference_prefix.is_none() && rule.repository_prefix.is_none() {
                return Err(TrustPolicyError::MissingRuleMatcher { index });
            }
            if rule.require_signatures && rule.public_keys.is_empty() {
                return Err(TrustPolicyError::MissingRuleKeys { index });
            }
            for public_key in &mut rule.public_keys {
                if !public_key.is_absolute() {
                    *public_key = root.join(&*public_key);
                }
                if !public_key.exists() {
                    return Err(TrustPolicyError::MissingPublicKey {
                        path: public_key.display().to_string(),
                    });
                }
                fs::read(&*public_key).map_err(|source| TrustPolicyError::ReadPublicKey {
                    path: public_key.display().to_string(),
                    source,
                })?;
            }
        }
        Ok(policy)
    }

    pub fn resolve_for_reference(
        &self,
        raw_reference: &str,
        reference: &DepotReference,
    ) -> PullTrustRequirement {
        let mut public_keys = Vec::new();
        let mut require_signatures = false;

        for rule in &self.rules {
            if !rule_matches_reference(rule, raw_reference, reference) {
                continue;
            }
            require_signatures |= rule.require_signatures;
            public_keys.extend(rule.public_keys.iter().cloned());
        }

        PullTrustRequirement {
            public_keys,
            require_signatures,
        }
    }
}

fn rule_matches_reference(
    rule: &PullTrustRule,
    raw_reference: &str,
    reference: &DepotReference,
) -> bool {
    if let Some(prefix) = &rule.reference_prefix
        && !raw_reference.starts_with(prefix)
    {
        return false;
    }
    if let Some(prefix) = &rule.repository_prefix
        && !reference.repository.starts_with(prefix)
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DepotLocator, DepotReference};
    use tempfile::tempdir;

    #[test]
    fn trust_policy_requires_a_matcher() {
        let dir = tempdir().unwrap();
        let policy_path = dir.path().join("trust.yaml");
        fs::write(&policy_path, "rules:\n  - require_signatures: true\n").unwrap();

        let error = PullTrustPolicy::from_path(&policy_path).unwrap_err();
        assert!(matches!(
            error,
            TrustPolicyError::MissingRuleMatcher { index: 0 }
        ));
    }

    #[test]
    fn trust_policy_resolves_relative_public_keys() {
        let dir = tempdir().unwrap();
        let keys_dir = dir.path().join("keys");
        fs::create_dir_all(&keys_dir).unwrap();
        fs::write(keys_dir.join("release.pub"), "public-key").unwrap();
        let policy_path = dir.path().join("trust.yaml");
        fs::write(
            &policy_path,
            "rules:\n  - repository_prefix: \"acme/demo\"\n    require_signatures: true\n    public_keys:\n      - keys/release.pub\n",
        )
        .unwrap();

        let policy = PullTrustPolicy::from_path(&policy_path).unwrap();
        assert_eq!(
            policy.rules[0].public_keys,
            vec![keys_dir.join("release.pub")]
        );
    }

    #[test]
    fn trust_policy_uses_and_semantics_for_dual_prefixes() {
        let policy = PullTrustPolicy {
            rules: vec![PullTrustRule {
                reference_prefix: Some("https://depot.example.com::".to_string()),
                repository_prefix: Some("acme/demo".to_string()),
                public_keys: vec![PathBuf::from("/tmp/demo.pub")],
                require_signatures: true,
            }],
        };
        let reference = DepotReference {
            locator: DepotLocator::Http {
                base_url: "https://depot.example.com".to_string(),
            },
            repository: "acme/demo".to_string(),
            tag: "v1".to_string(),
        };
        let requirement =
            policy.resolve_for_reference("https://depot.example.com::acme/demo:v1", &reference);
        assert!(requirement.require_signatures);
        assert_eq!(
            requirement.public_keys,
            vec![PathBuf::from("/tmp/demo.pub")]
        );

        let no_match =
            policy.resolve_for_reference("https://other.example.com::acme/demo:v1", &reference);
        assert!(!no_match.require_signatures);
        assert!(no_match.public_keys.is_empty());
    }
}
