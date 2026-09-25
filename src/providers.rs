use anyhow::{ensure, Context, Result};
use serde_json::Value;

/// Resolve only same-repository PRs. Fork writes require a separate registered authority policy.
pub fn pull_request_context(
    provider: &str,
    repository_url: &str,
    value: &Value,
) -> Result<(String, String, String)> {
    let (revision, source, target) =
        if provider == "github" {
            ensure!(
                value["head"]["repo"]["clone_url"].as_str().is_some_and(
                    |url| github_repository_url(url) == github_repository_url(repository_url)
                ),
                "fork PRs require a separately registered repository"
            );
            (
                value["head"]["sha"].as_str(),
                value["head"]["ref"].as_str(),
                value["base"]["ref"].as_str(),
            )
        } else {
            ensure!(
                value.get("forkSource").is_none_or(Value::is_null),
                "fork PRs require a separately registered repository"
            );
            (
                value["lastMergeSourceCommit"]["commitId"].as_str(),
                value["sourceRefName"]
                    .as_str()
                    .and_then(|s| s.strip_prefix("refs/heads/")),
                value["targetRefName"]
                    .as_str()
                    .and_then(|s| s.strip_prefix("refs/heads/")),
            )
        };
    let revision = revision.context("PR revision missing")?;
    let source = source.context("PR source branch missing")?;
    let target = target.context("PR target branch missing")?;
    ensure!(
        revision.len() == 40 && revision.bytes().all(|c| c.is_ascii_hexdigit()),
        "PR revision must be a commit SHA"
    );
    ensure!(
        source != target && safe_branch(source) && safe_branch(target),
        "invalid PR branch context"
    );
    Ok((revision.into(), source.into(), target.into()))
}
// GitHub returns clone URLs with .git, while portal users commonly paste web URLs.
// Normalize only these suffixes; keep the complete host and repository path comparison.
fn github_repository_url(url: &str) -> &str {
    let url = url.trim_end_matches('/');
    url.strip_suffix(".git").unwrap_or(url)
}
pub fn safe_branch(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.contains("..")
        && !value.contains("@{")
        && !value.ends_with('.')
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/_-.".contains(&c))
}
