use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    Read,
    Create,
    Modify,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Error)]
#[allow(dead_code)]
pub(crate) enum PolicyError {
    #[error("invalid deny glob: {0}")]
    InvalidGlob(String),
    #[error("approval is missing, stale, mismatched, or already consumed")]
    InvalidApproval,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct OperationBinding<'a> {
    pub descriptor_version: u32,
    pub run_revision: u64,
    pub effect_id: &'a str,
    pub attempt: u64,
    pub tool: &'a str,
    pub target: &'a str,
    pub input: &'a serde_json::Value,
    pub precondition: Option<&'a str>,
    pub timeout_ms: u64,
    pub output_cap: usize,
    pub policy_version: u64,
    pub lease_owner: &'a str,
    pub lease_token: u64,
}
pub(crate) fn digest(binding: &OperationBinding<'_>) -> String {
    let bytes = serde_json::to_vec(binding).expect("operation binding is serializable");
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug)]
#[cfg(test)]
pub(crate) struct PendingApproval {
    digest: String,
    consumed: bool,
}
#[cfg(test)]
impl PendingApproval {
    pub(crate) fn new(digest: String) -> Self {
        Self {
            digest,
            consumed: false,
        }
    }
    pub(crate) fn consume(&mut self, exact: &str) -> Result<(), PolicyError> {
        if self.consumed || self.digest != exact {
            return Err(PolicyError::InvalidApproval);
        }
        self.consumed = true;
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct PermissionPolicy {
    denies: GlobSet,
    pub(crate) version: u64,
}
impl PermissionPolicy {
    pub(crate) fn new(deny: &[String], version: u64) -> Result<Self, PolicyError> {
        let mut builder = GlobSetBuilder::new();
        for pattern in deny {
            builder.add(Glob::new(pattern).map_err(|e| PolicyError::InvalidGlob(e.to_string()))?);
        }
        Ok(Self {
            denies: builder
                .build()
                .map_err(|e| PolicyError::InvalidGlob(e.to_string()))?,
            version,
        })
    }
    pub(crate) fn decide(&self, effect: EffectClass, target: &str) -> PolicyDecision {
        if self.denies.is_match(target) {
            PolicyDecision::Deny
        } else if effect == EffectClass::Read {
            PolicyDecision::Allow
        } else {
            PolicyDecision::Ask
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_policy_rejects_invalid_glob() {
        let result = PermissionPolicy::new(&["[invalid".to_string()], 1);
        assert!(matches!(result, Err(PolicyError::InvalidGlob(_))));
    }

    #[test]
    fn permission_policy_decide_covers_deny_allow_and_ask() {
        let policy = PermissionPolicy::new(&["**/secret/**".to_string()], 1).expect("valid glob");
        assert_eq!(
            policy.decide(EffectClass::Read, "workspace/secret/file.txt"),
            PolicyDecision::Deny
        );
        assert_eq!(
            policy.decide(EffectClass::Read, "workspace/normal/file.txt"),
            PolicyDecision::Allow
        );
        assert_eq!(
            policy.decide(EffectClass::Modify, "workspace/normal/file.txt"),
            PolicyDecision::Ask
        );
        assert_eq!(policy.version, 1);
    }
}
