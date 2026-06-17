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

#[cfg(test)]
mod tests {
    use super::{RemoteArtifactState, UploadCondition, plan_immutable_upload};

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
}
