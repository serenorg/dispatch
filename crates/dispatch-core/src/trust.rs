use crate::DepotReference;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PullTrustPolicy {
    #[serde(default)]
    pub rules: Vec<PullTrustRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct A2aTrustPolicy {
    #[serde(default)]
    pub rules: Vec<A2aTrustRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct A2aTrustRule {
    #[serde(default)]
    pub origin_prefix: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub expected_agent_name: Option<String>,
    #[serde(default)]
    pub expected_card_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aTrustRequirement {
    pub matched: bool,
    pub expected_agent_name: Option<String>,
    pub expected_card_sha256: Option<String>,
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
        source: toml::de::Error,
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
    #[error("A2A trust policy rule {index} must set `origin_prefix`, `hostname`, or both")]
    MissingA2aRuleMatcher { index: usize },
    #[error("A2A trust policy rule {index} has invalid `expected_card_sha256` `{digest}`")]
    InvalidA2aCardDigest { index: usize, digest: String },
    #[error("A2A trust policy matched conflicting `{field}` values: `{left}` vs `{right}`")]
    ConflictingA2aRequirement {
        field: &'static str,
        left: String,
        right: String,
    },
}

impl PullTrustPolicy {
    pub fn from_path(path: &Path) -> Result<Self, TrustPolicyError> {
        let source = fs::read_to_string(path).map_err(|source| TrustPolicyError::ReadPolicy {
            path: path.display().to_string(),
            source,
        })?;
        let mut policy: PullTrustPolicy =
            toml::from_str(&source).map_err(|source| TrustPolicyError::ParsePolicy {
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

impl A2aTrustPolicy {
    pub fn from_path(path: &Path) -> Result<Self, TrustPolicyError> {
        let source = fs::read_to_string(path).map_err(|source| TrustPolicyError::ReadPolicy {
            path: path.display().to_string(),
            source,
        })?;
        let policy: A2aTrustPolicy =
            toml::from_str(&source).map_err(|source| TrustPolicyError::ParsePolicy {
                path: path.display().to_string(),
                source,
            })?;
        for (index, rule) in policy.rules.iter().enumerate() {
            if rule.origin_prefix.is_none() && rule.hostname.is_none() {
                return Err(TrustPolicyError::MissingA2aRuleMatcher { index });
            }
            if let Some(digest) = &rule.expected_card_sha256
                && !is_lower_hex_sha256(digest)
            {
                return Err(TrustPolicyError::InvalidA2aCardDigest {
                    index,
                    digest: digest.clone(),
                });
            }
        }
        Ok(policy)
    }

    pub fn resolve_for_url(&self, url: &url::Url) -> Result<A2aTrustRequirement, TrustPolicyError> {
        let mut requirement = A2aTrustRequirement {
            matched: false,
            expected_agent_name: None,
            expected_card_sha256: None,
        };

        for rule in &self.rules {
            if !rule_matches_a2a(rule, url) {
                continue;
            }
            requirement.matched = true;
            merge_optional_requirement(
                &mut requirement.expected_agent_name,
                rule.expected_agent_name.as_ref(),
                "expected_agent_name",
            )?;
            merge_optional_requirement(
                &mut requirement.expected_card_sha256,
                rule.expected_card_sha256.as_ref(),
                "expected_card_sha256",
            )?;
        }

        Ok(requirement)
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

fn rule_matches_a2a(rule: &A2aTrustRule, url: &url::Url) -> bool {
    if let Some(prefix) = &rule.origin_prefix
        && !crate::courier::a2a_origin_for_trust(url)
            .as_deref()
            .is_some_and(|origin| origin.starts_with(prefix))
    {
        return false;
    }
    if let Some(hostname) = &rule.hostname {
        let actual = url
            .host_str()
            .map(|value| value.to_ascii_lowercase())
            .unwrap_or_default();
        if actual != hostname.to_ascii_lowercase() {
            return false;
        }
    }
    true
}

fn merge_optional_requirement(
    current: &mut Option<String>,
    incoming: Option<&String>,
    field: &'static str,
) -> Result<(), TrustPolicyError> {
    let Some(incoming) = incoming else {
        return Ok(());
    };
    match current {
        Some(existing) if existing != incoming => {
            Err(TrustPolicyError::ConflictingA2aRequirement {
                field,
                left: existing.clone(),
                right: incoming.clone(),
            })
        }
        Some(_) => Ok(()),
        None => {
            *current = Some(incoming.clone());
            Ok(())
        }
    }
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DepotLocator, DepotReference};
    use tempfile::tempdir;

    #[test]
    fn trust_policy_requires_a_matcher() {
        let dir = tempdir().unwrap();
        let policy_path = dir.path().join("trust.toml");
        fs::write(&policy_path, "[[rules]]\nrequire_signatures = true\n").unwrap();

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
        let policy_path = dir.path().join("trust.toml");
        fs::write(
            &policy_path,
            "[[rules]]\nrepository_prefix = \"acme/demo\"\nrequire_signatures = true\npublic_keys = [\"keys/release.pub\"]\n",
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

    #[test]
    fn a2a_trust_policy_requires_a_matcher() {
        let dir = tempdir().unwrap();
        let policy_path = dir.path().join("a2a-trust.toml");
        fs::write(
            &policy_path,
            "[[rules]]\nexpected_agent_name = \"planner\"\n",
        )
        .unwrap();

        let error = A2aTrustPolicy::from_path(&policy_path).unwrap_err();
        assert!(matches!(
            error,
            TrustPolicyError::MissingA2aRuleMatcher { index: 0 }
        ));
    }

    #[test]
    fn a2a_trust_policy_validates_card_digests() {
        let dir = tempdir().unwrap();
        let policy_path = dir.path().join("a2a-trust.toml");
        fs::write(
            &policy_path,
            "[[rules]]\nhostname = \"planner.example.com\"\nexpected_card_sha256 = \"ABCD\"\n",
        )
        .unwrap();

        let error = A2aTrustPolicy::from_path(&policy_path).unwrap_err();
        assert!(matches!(
            error,
            TrustPolicyError::InvalidA2aCardDigest { index: 0, .. }
        ));
    }

    #[test]
    fn trust_policy_rejects_unknown_fields() {
        let dir = tempdir().unwrap();
        let policy_path = dir.path().join("trust.toml");
        fs::write(
            &policy_path,
            "[[rules]]\nrepository_prefix = \"acme/demo\"\norigin_prefx = \"typo\"\n",
        )
        .unwrap();

        let error = PullTrustPolicy::from_path(&policy_path).unwrap_err();
        assert!(matches!(error, TrustPolicyError::ParsePolicy { .. }));
    }

    #[test]
    fn a2a_trust_policy_rejects_unknown_fields() {
        let dir = tempdir().unwrap();
        let policy_path = dir.path().join("a2a-trust.toml");
        fs::write(
            &policy_path,
            "[[rules]]\nhostname = \"planner.example.com\"\norigin_prefx = \"typo\"\n",
        )
        .unwrap();

        let error = A2aTrustPolicy::from_path(&policy_path).unwrap_err();
        assert!(matches!(error, TrustPolicyError::ParsePolicy { .. }));
    }

    #[test]
    fn a2a_trust_policy_resolves_operator_requirements() {
        let policy = A2aTrustPolicy {
            rules: vec![
                A2aTrustRule {
                    origin_prefix: Some("https://planner.example.com".to_string()),
                    hostname: None,
                    expected_agent_name: Some("planner-agent".to_string()),
                    expected_card_sha256: None,
                },
                A2aTrustRule {
                    origin_prefix: None,
                    hostname: Some("planner.example.com".to_string()),
                    expected_agent_name: None,
                    expected_card_sha256: Some(
                        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                            .to_string(),
                    ),
                },
            ],
        };
        let url = url::Url::parse("https://planner.example.com/a2a").unwrap();

        let requirement = policy.resolve_for_url(&url).unwrap();
        assert!(requirement.matched);
        assert_eq!(
            requirement.expected_agent_name.as_deref(),
            Some("planner-agent")
        );
        assert_eq!(
            requirement.expected_card_sha256.as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn a2a_trust_policy_rejects_conflicting_operator_requirements() {
        let policy = A2aTrustPolicy {
            rules: vec![
                A2aTrustRule {
                    origin_prefix: Some("https://planner.example.com".to_string()),
                    hostname: None,
                    expected_agent_name: Some("planner-a".to_string()),
                    expected_card_sha256: None,
                },
                A2aTrustRule {
                    origin_prefix: None,
                    hostname: Some("planner.example.com".to_string()),
                    expected_agent_name: Some("planner-b".to_string()),
                    expected_card_sha256: None,
                },
            ],
        };
        let url = url::Url::parse("https://planner.example.com/a2a").unwrap();

        let error = policy.resolve_for_url(&url).unwrap_err();
        assert!(matches!(
            error,
            TrustPolicyError::ConflictingA2aRequirement {
                field: "expected_agent_name",
                ..
            }
        ));
    }
}
