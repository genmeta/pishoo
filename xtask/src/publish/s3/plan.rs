#![allow(dead_code)]

use std::path::PathBuf;

use snafu::Snafu;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteArtifactState {
    Missing,
    Present { sha256: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadCondition {
    IfMissing,
    IfMatch(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedUpload {
    pub path: PathBuf,
    pub key: String,
    pub entry: bool,
    pub condition: Option<UploadCondition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionedImmutablePayloadPlan {
    metadata_sha256: String,
    upload_condition: Option<UploadCondition>,
    remote_sha256_matches_local: Option<bool>,
}

impl VersionedImmutablePayloadPlan {
    pub fn metadata_sha256(&self) -> &str {
        &self.metadata_sha256
    }

    pub fn upload_condition(&self) -> Option<UploadCondition> {
        self.upload_condition.clone()
    }

    pub fn reuses_remote_payload(&self) -> bool {
        self.remote_sha256_matches_local.is_some()
    }

    pub fn remote_sha256_matches_local(&self) -> bool {
        self.remote_sha256_matches_local.unwrap_or(false)
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ImmutableCollisionError {
    #[snafu(display(
        "remote immutable artifact {artifact_path} already exists with different sha256 {sha256}"
    ))]
    DifferentHash {
        artifact_path: String,
        sha256: String,
    },
}

pub fn verify_immutable_collision(
    artifact_path: &str,
    actual_sha256: &str,
    remote: RemoteArtifactState,
) -> Result<(), ImmutableCollisionError> {
    plan_immutable_upload(artifact_path, actual_sha256, remote).map(|_| ())
}

pub fn plan_immutable_upload(
    artifact_path: &str,
    actual_sha256: &str,
    remote: RemoteArtifactState,
) -> Result<Option<UploadCondition>, ImmutableCollisionError> {
    match remote {
        RemoteArtifactState::Missing => Ok(Some(UploadCondition::IfMissing)),
        RemoteArtifactState::Present { sha256 } if sha256 == actual_sha256 => Ok(None),
        RemoteArtifactState::Present { sha256 } => Err(ImmutableCollisionError::DifferentHash {
            artifact_path: artifact_path.to_string(),
            sha256,
        }),
    }
}

pub fn plan_versioned_immutable_payload(
    _artifact_path: &str,
    actual_sha256: &str,
    remote: RemoteArtifactState,
) -> VersionedImmutablePayloadPlan {
    match remote {
        RemoteArtifactState::Missing => VersionedImmutablePayloadPlan {
            metadata_sha256: actual_sha256.to_string(),
            upload_condition: Some(UploadCondition::IfMissing),
            remote_sha256_matches_local: None,
        },
        RemoteArtifactState::Present { sha256 } => {
            let matches_local = sha256 == actual_sha256;
            VersionedImmutablePayloadPlan {
                metadata_sha256: sha256,
                upload_condition: None,
                remote_sha256_matches_local: Some(matches_local),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RemoteArtifactState, UploadCondition, plan_immutable_upload,
        plan_versioned_immutable_payload,
    };

    #[test]
    fn immutable_missing_remote_uploads_only_when_absent() {
        let condition =
            plan_immutable_upload("brew/file.tar.gz", "abc", RemoteArtifactState::Missing)
                .expect("missing remote should be publishable");

        assert_eq!(condition, Some(UploadCondition::IfMissing));
    }

    #[test]
    fn immutable_same_hash_remote_is_skipped() {
        let condition = plan_immutable_upload(
            "brew/file.tar.gz",
            "abc",
            RemoteArtifactState::Present {
                sha256: "abc".to_string(),
            },
        )
        .expect("same hash should be accepted");

        assert_eq!(condition, None);
    }

    #[test]
    fn immutable_collision_different_hash_fails() {
        let error = plan_immutable_upload(
            "brew/file.tar.gz",
            "abc",
            RemoteArtifactState::Present {
                sha256: "def".to_string(),
            },
        )
        .expect_err("different hash should fail");
        assert_eq!(
            error.to_string(),
            "remote immutable artifact brew/file.tar.gz already exists with different sha256 def"
        );
    }

    #[test]
    fn versioned_immutable_missing_remote_uploads_local_payload() {
        let plan = plan_versioned_immutable_payload(
            "brew/file.tar.gz",
            "local-sha",
            RemoteArtifactState::Missing,
        );

        assert_eq!(plan.metadata_sha256(), "local-sha");
        assert_eq!(plan.upload_condition(), Some(UploadCondition::IfMissing));
        assert!(!plan.reuses_remote_payload());
    }

    #[test]
    fn versioned_immutable_matching_remote_reuses_remote_payload() {
        let plan = plan_versioned_immutable_payload(
            "brew/file.tar.gz",
            "same-sha",
            RemoteArtifactState::Present {
                sha256: "same-sha".to_string(),
            },
        );

        assert_eq!(plan.metadata_sha256(), "same-sha");
        assert_eq!(plan.upload_condition(), None);
        assert!(plan.reuses_remote_payload());
        assert!(plan.remote_sha256_matches_local());
    }

    #[test]
    fn versioned_immutable_different_remote_reuses_remote_sha_for_metadata() {
        let plan = plan_versioned_immutable_payload(
            "brew/file.tar.gz",
            "new-local-sha",
            RemoteArtifactState::Present {
                sha256: "published-sha".to_string(),
            },
        );

        assert_eq!(plan.metadata_sha256(), "published-sha");
        assert_eq!(plan.upload_condition(), None);
        assert!(plan.reuses_remote_payload());
        assert!(!plan.remote_sha256_matches_local());
    }
}
