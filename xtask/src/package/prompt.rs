#![allow(dead_code)]

use std::io::IsTerminal;

use snafu::Snafu;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwriteDecision {
    Write,
    Skip,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum OverwriteManifestError {
    #[snafu(display("package manifest already exists; pass --overwrite-manifest to replace it"))]
    NotInteractive,
    #[snafu(display("failed to read overwrite confirmation"))]
    Prompt { source: inquire::InquireError },
}

pub fn decide_manifest_overwrite(
    manifest_exists: bool,
    overwrite_manifest: bool,
    interactive: bool,
    prompt: impl FnOnce() -> Result<bool, inquire::InquireError>,
) -> Result<OverwriteDecision, OverwriteManifestError> {
    if !manifest_exists || overwrite_manifest {
        return Ok(OverwriteDecision::Write);
    }
    if !interactive {
        return Err(OverwriteManifestError::NotInteractive);
    }
    if prompt().map_err(|source| OverwriteManifestError::Prompt { source })? {
        Ok(OverwriteDecision::Write)
    } else {
        Ok(OverwriteDecision::Skip)
    }
}

pub async fn confirm_manifest_overwrite(
    manifest_exists: bool,
    overwrite_manifest: bool,
) -> Result<OverwriteDecision, OverwriteManifestError> {
    let interactive = std::io::stdin().is_terminal();
    decide_manifest_overwrite(manifest_exists, overwrite_manifest, interactive, || {
        inquire::Confirm::new("package manifest already exists; overwrite it?")
            .with_default(false)
            .prompt()
    })
}

#[cfg(test)]
mod tests {
    use super::{OverwriteDecision, decide_manifest_overwrite};

    #[test]
    fn missing_manifest_writes_without_prompt() {
        let decision = decide_manifest_overwrite(false, false, false, || unreachable!())
            .expect("missing manifest should write");
        assert_eq!(decision, OverwriteDecision::Write);
    }

    #[test]
    fn overwrite_flag_writes_existing_manifest() {
        let decision = decide_manifest_overwrite(true, true, false, || unreachable!())
            .expect("overwrite flag should write");
        assert_eq!(decision, OverwriteDecision::Write);
    }

    #[test]
    fn non_interactive_existing_manifest_fails_without_flag() {
        let error = decide_manifest_overwrite(true, false, false, || unreachable!())
            .expect_err("non-interactive overwrite should fail");
        assert_eq!(
            error.to_string(),
            "package manifest already exists; pass --overwrite-manifest to replace it"
        );
    }

    #[test]
    fn interactive_decline_skips_write() {
        let decision = decide_manifest_overwrite(true, false, true, || Ok(false))
            .expect("decline should be a decision");
        assert_eq!(decision, OverwriteDecision::Skip);
    }
}
