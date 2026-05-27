#![allow(dead_code)]

use std::path::PathBuf;

use snafu::Snafu;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteArtifactState {
    Missing,
    Present { sha256: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedUpload {
    pub path: PathBuf,
    pub key: String,
    pub entry: bool,
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
    match remote {
        RemoteArtifactState::Missing => Ok(()),
        RemoteArtifactState::Present { sha256 } if sha256 == actual_sha256 => Ok(()),
        RemoteArtifactState::Present { sha256 } => Err(ImmutableCollisionError::DifferentHash {
            artifact_path: artifact_path.to_string(),
            sha256,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{RemoteArtifactState, verify_immutable_collision};

    #[test]
    fn immutable_collision_missing_passes() {
        verify_immutable_collision("brew/file.tar.gz", "abc", RemoteArtifactState::Missing)
            .expect("missing remote should pass");
    }

    #[test]
    fn immutable_collision_same_hash_passes() {
        verify_immutable_collision(
            "brew/file.tar.gz",
            "abc",
            RemoteArtifactState::Present {
                sha256: "abc".to_string(),
            },
        )
        .expect("same hash should pass");
    }

    #[test]
    fn immutable_collision_different_hash_fails() {
        let error = verify_immutable_collision(
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
