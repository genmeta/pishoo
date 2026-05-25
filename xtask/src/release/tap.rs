use std::{
    io::Write,
    path::{Path, PathBuf},
};

use snafu::{OptionExt, ResultExt, Whatever};
use tracing::info;

use super::paths::common_paths;

pub async fn update(repo: PathBuf, commit: bool, push: bool) -> Result<(), Whatever> {
    validate_options(commit, push)?;
    ensure_git_repo(&repo).await?;
    let homebrew = common_paths()?.homebrew;
    snafu::ensure_whatever!(
        tokio::fs::try_exists(&homebrew)
            .await
            .whatever_context(format!("failed to inspect {}", homebrew.display()))?,
        "staged homebrew directory {} is missing",
        homebrew.display()
    );

    let formulae = copy_formulae(&homebrew, &repo).await?;
    print_git_diff(&repo, &formulae).await?;
    if commit {
        run_git_with_formulae(&repo, &["add"], &formulae).await?;
        run_git(
            &repo,
            &["commit", "-m", "release: update Homebrew formulae"],
        )
        .await?;
    }
    if push {
        run_git(&repo, &["push"]).await?;
    }
    Ok(())
}

fn validate_options(commit: bool, push: bool) -> Result<(), Whatever> {
    snafu::ensure_whatever!(commit || !push, "tap push requires --commit");
    Ok(())
}

async fn ensure_git_repo(repo: &Path) -> Result<(), Whatever> {
    let git = repo.join(".git");
    snafu::ensure_whatever!(
        tokio::fs::try_exists(&git)
            .await
            .whatever_context(format!("failed to inspect {}", git.display()))?,
        "{} is not a git repository checkout",
        repo.display()
    );
    Ok(())
}

async fn copy_formulae(homebrew: &Path, repo: &Path) -> Result<Vec<String>, Whatever> {
    let mut formulae = Vec::new();
    let mut entries = tokio::fs::read_dir(homebrew)
        .await
        .whatever_context(format!("failed to read {}", homebrew.display()))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .whatever_context(format!("failed to read entry in {}", homebrew.display()))?
    {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .await
            .whatever_context(format!("failed to inspect {}", path.display()))?;
        if !file_type.is_file()
            || path.extension().and_then(|extension| extension.to_str()) != Some("rb")
        {
            continue;
        }
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .whatever_context("failed to read formula filename as utf-8")?
            .to_string();
        let destination = repo.join(&filename);
        tokio::fs::copy(&path, &destination)
            .await
            .whatever_context(format!(
                "failed to copy {} to {}",
                path.display(),
                destination.display()
            ))?;
        info!(formula = %filename, "copied homebrew formula to tap");
        formulae.push(filename);
    }
    snafu::ensure_whatever!(!formulae.is_empty(), "no staged homebrew formulae found");
    formulae.sort();
    Ok(formulae)
}

async fn print_git_diff(repo: &Path, formulae: &[String]) -> Result<(), Whatever> {
    let output = tokio::process::Command::new("git")
        .current_dir(repo)
        .arg("diff")
        .arg("--")
        .args(formulae)
        .output()
        .await
        .whatever_context("failed to run git diff")?;
    snafu::ensure_whatever!(output.status.success(), "git diff failed");
    std::io::stdout()
        .write_all(&output.stdout)
        .whatever_context("failed to write git diff stdout")?;
    std::io::stderr()
        .write_all(&output.stderr)
        .whatever_context("failed to write git diff stderr")?;
    Ok(())
}

async fn run_git(repo: &Path, args: &[&str]) -> Result<(), Whatever> {
    crate::run_cmd(
        tokio::process::Command::new("git")
            .current_dir(repo)
            .args(args),
    )
    .await
}

async fn run_git_with_formulae(
    repo: &Path,
    args: &[&str],
    formulae: &[String],
) -> Result<(), Whatever> {
    crate::run_cmd(
        tokio::process::Command::new("git")
            .current_dir(repo)
            .args(args)
            .arg("--")
            .args(formulae),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::validate_options;

    #[test]
    fn push_requires_commit() {
        let error = validate_options(false, true).expect_err("push without commit should fail");

        assert_eq!(error.to_string(), "tap push requires --commit");
    }

    #[test]
    fn commit_with_push_is_valid() {
        validate_options(true, true).expect("push with commit should be valid");
    }
}
