use std::{io::Write, path::Path};

use snafu::{OptionExt, ResultExt, Whatever};
use tracing::info;

use super::{TapOptions, paths::common_paths};

pub async fn publish(options: TapOptions) -> Result<(), Whatever> {
    validate_options(options.commit, options.push, options.dry_run)?;
    ensure_git_repo(&options.repo).await?;
    let homebrew = common_paths()?.homebrew;
    snafu::ensure_whatever!(
        tokio::fs::try_exists(&homebrew)
            .await
            .whatever_context(format!("failed to inspect {}", homebrew.display()))?,
        "staged homebrew directory {} is missing",
        homebrew.display()
    );

    let formulae = collect_formulae(&homebrew).await?;
    if options.dry_run {
        for formula in formulae {
            info!(
                source = %homebrew.join(&formula).display(),
                destination = %options.repo.join(&formula).display(),
                "would copy homebrew formula to tap"
            );
        }
        return Ok(());
    }

    copy_formulae(&homebrew, &options.repo, &formulae).await?;
    print_git_diff(&options.repo, &formulae).await?;
    if options.commit {
        run_git_with_formulae(&options.repo, &["add"], &formulae).await?;
        run_git(
            &options.repo,
            &["commit", "-m", "release: update Homebrew formulae"],
        )
        .await?;
    }
    if options.push {
        run_git(&options.repo, &["push"]).await?;
    }
    Ok(())
}

fn validate_options(commit: bool, push: bool, dry_run: bool) -> Result<(), Whatever> {
    snafu::ensure_whatever!(commit || !push, "tap push requires --commit");
    snafu::ensure_whatever!(
        !dry_run || (!commit && !push),
        "tap dry-run cannot be combined with --commit or --push"
    );
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

async fn collect_formulae(homebrew: &Path) -> Result<Vec<String>, Whatever> {
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
        formulae.push(filename);
    }
    snafu::ensure_whatever!(!formulae.is_empty(), "no staged homebrew formulae found");
    formulae.sort();
    Ok(formulae)
}

async fn copy_formulae(homebrew: &Path, repo: &Path, formulae: &[String]) -> Result<(), Whatever> {
    for formula in formulae {
        let source = homebrew.join(formula);
        let destination = repo.join(formula);
        tokio::fs::copy(&source, &destination)
            .await
            .whatever_context(format!(
                "failed to copy {} to {}",
                source.display(),
                destination.display()
            ))?;
        info!(formula = %formula, "copied homebrew formula to tap");
    }
    Ok(())
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
        let error =
            validate_options(false, true, false).expect_err("push without commit should fail");

        assert_eq!(error.to_string(), "tap push requires --commit");
    }

    #[test]
    fn commit_with_push_is_valid() {
        validate_options(true, true, false).expect("push with commit should be valid");
    }

    #[test]
    fn dry_run_rejects_mutating_flags() {
        let error =
            validate_options(true, false, true).expect_err("dry run with commit should fail");

        assert_eq!(
            error.to_string(),
            "tap dry-run cannot be combined with --commit or --push"
        );
    }
}
