//! Resolve repository URLs through trusted connector endpoints. Provider API calls
//! determine repository access and the default branch; no onboarding is required.
use crate::{
    config::{Platform, RepositoryConfig},
    connectors::{self, Connection},
    model::Repository,
    store::Store,
};
use anyhow::{ensure, Context, Result};

pub fn from_url(
    kind: &str,
    connection: &Connection,
    target: &str,
    workflow: &str,
) -> Result<RepositoryConfig> {
    let url =
        reqwest::Url::parse(target).context("Enter an HTTPS repository URL or configured alias")?;
    ensure!(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "repository URL must be HTTPS without credentials, query or fragment"
    );
    let base = reqwest::Url::parse(&connection.value("base_url")?)?;
    ensure!(
        url.origin() == base.origin(),
        "repository URL is outside connector scope"
    );
    let prefix = format!("{}/", base.path().trim_end_matches('/'));
    let path = url
        .path()
        .strip_prefix(&prefix)
        .context("repository URL is outside connector scope")?
        .trim_end_matches('/');
    let decoded = path
        .split('/')
        .map(|p| {
            percent_encoding::percent_decode_str(p)
                .decode_utf8()
                .map(|p| p.into_owned())
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let parts: Vec<_> = decoded.iter().map(String::as_str).collect();
    // Reject encoded separators/dot segments instead of interpreting them differently
    // in Git and the provider API.
    ensure!(
        parts.iter().all(|p| !p.is_empty()
            && !p.contains(['%', '/', '\\'])
            && !p.chars().any(char::is_control)
            && *p != "."
            && *p != ".."),
        "invalid repository path"
    );
    let endpoint = connection.value(if kind == "github" {
        "api_url"
    } else {
        "base_url"
    })?;
    let mut api = reqwest::Url::parse(&endpoint)?;
    match kind {
        "github" => {
            ensure!(
                parts.len() == 2,
                "GitHub repository URL must contain owner/repository"
            );
            let name = parts[1].strip_suffix(".git").unwrap_or(parts[1]);
            ensure!(!name.is_empty(), "repository name required");
            api.path_segments_mut()
                .map_err(|_| anyhow::anyhow!("invalid API URL"))?
                .pop_if_empty()
                .extend(["repos", parts[0], name]);
        }
        "ado" => {
            ensure!(parts.len() == 3 && parts[1] == "_git", "Azure repository URL must contain project/_git/repository beneath the organization URL");
            api.path_segments_mut()
                .map_err(|_| anyhow::anyhow!("invalid API URL"))?
                .pop_if_empty()
                .extend([parts[0], "_apis", "git", "repositories", parts[2]]);
        }
        _ => anyhow::bail!("unsupported repository connector"),
    }
    Ok(RepositoryConfig {
        provider: kind.into(),
        url: url.to_string(),
        api_url: api.to_string(),
        branch: String::new(),
        revision: None,
        workflow: workflow.into(),
        maintainers: vec![],
        read_token_env: None,
        write_token_env: None,
        slack_channel: None,
    })
}

pub async fn resolve(
    store: &Store,
    master: &str,
    platform: &Platform,
    http: &reqwest::Client,
    id: &str,
) -> Result<RepositoryConfig> {
    if let Some(repo) = platform.repositories.get(id) {
        return Ok(repo.clone());
    }
    ensure!(id.len() <= 2048, "repository URL is too long");
    for kind in ["github", "ado"] {
        let Some(c) = connectors::load(store, master, kind).await? else {
            continue;
        };
        let Ok(mut repo) = from_url(kind, &c, id, &platform.default_workflow) else {
            continue;
        };
        c.authorize(id)?;
        let credential = connectors::repository_token(store, master, id, &repo, false)
            .await?
            .context("repository credential required")?;
        let request = http.get(&repo.api_url);
        let request = if kind == "ado" {
            request
                .basic_auth("", Some(credential))
                .query(&[("api-version", "7.1")])
        } else {
            request.bearer_auth(credential)
        };
        let metadata: serde_json::Value = request
            .send()
            .await
            .context("repository lookup failed")?
            .error_for_status()
            .context("provider denied repository access or repository was not found")?
            .json()
            .await
            .context("invalid repository response")?;
        repo.branch = metadata[if kind == "github" {
            "default_branch"
        } else {
            "defaultBranch"
        }]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("repository has no default branch")?
        .trim_start_matches("refs/heads/")
        .into();
        return Ok(repo);
    }
    anyhow::bail!("Enter a repository URL matching a configured GitHub or Azure DevOps connector")
}

/// Worker destinations come from the pinned job, including after a restart.
pub fn pinned(platform: &Platform, repo: &Repository) -> RepositoryConfig {
    let mut config = platform
        .repositories
        .get(&repo.id)
        .cloned()
        .unwrap_or(RepositoryConfig {
            provider: repo.provider.clone(),
            url: repo.url.clone(),
            api_url: repo.api_url.clone(),
            branch: repo.base_branch.clone(),
            revision: None,
            workflow: String::new(),
            maintainers: vec![],
            read_token_env: None,
            write_token_env: None,
            slack_channel: None,
        });
    config.provider = repo.provider.clone();
    config.url = repo.url.clone();
    config.api_url = repo.api_url.clone();
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn platform_loads_without_repository_onboarding() {
        let mut value: serde_json::Value =
            serde_yaml::from_str(include_str!("../config/platform.yaml")).unwrap();
        value.as_object_mut().unwrap().remove("repositories");
        let platform: Platform = serde_json::from_value(value).unwrap();
        assert!(platform.repositories.is_empty());
        assert!(platform
            .snapshot(
                std::path::Path::new("workflows"),
                std::path::Path::new("prompts")
            )
            .is_ok());
    }
    fn connection(base: &str) -> Connection {
        Connection {
            enabled: true,
            values: [
                ("base_url".into(), base.into()),
                ("api_url".into(), "https://api.github.com".into()),
            ]
            .into(),
            ..Default::default()
        }
    }
    #[test]
    fn urls_resolve_without_registration_and_stay_inside_provider_scope() {
        let c = connection("https://github.com");
        let r = from_url(
            "github",
            &c,
            "https://github.com/team/new-repo.git",
            "research-plan",
        )
        .unwrap();
        assert_eq!(r.api_url, "https://api.github.com/repos/team/new-repo");
        for target in [
            "https://evil.test/team/repo",
            "https://token@github.com/team/repo",
            "https://github.com/team/repo?token=x",
            "https://github.com/team/repo/tree/main",
            "https://github.com/team/repo%2fother",
        ] {
            assert!(from_url("github", &c, target, "w").is_err());
        }
        let c = connection("https://dev.azure.com/org");
        assert_eq!(
            from_url(
                "ado",
                &c,
                "https://dev.azure.com/org/project/_git/repo",
                "w"
            )
            .unwrap()
            .api_url,
            "https://dev.azure.com/org/project/_apis/git/repositories/repo"
        );
        assert!(from_url(
            "ado",
            &c,
            "https://dev.azure.com/org-evil/project/_git/repo",
            "w"
        )
        .is_err());
        assert_eq!(
            from_url(
                "ado",
                &c,
                "https://dev.azure.com/org/My%20Project/_git/My%20Repo",
                "w"
            )
            .unwrap()
            .api_url,
            "https://dev.azure.com/org/My%20Project/_apis/git/repositories/My%20Repo"
        );
    }
}
