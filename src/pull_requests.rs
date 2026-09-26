//! Provider context is data, never instructions. All authenticated requests use
//! connector-derived endpoints, not links returned in PR descriptions/comments.
use crate::{api::App, connectors, model::Issue};
use anyhow::{bail, ensure, Context, Result};
use reqwest::{Client, Method, RequestBuilder, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const MAX_BYTES: usize = 2_000_000;
const MAX_PAGES: usize = 100;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSubmission {
    pub pr_url: String,
    pub ticket: Option<String>,
    #[serde(default = "review_workflow")]
    pub workflow: String,
}
fn review_workflow() -> String {
    "pr-review".into()
}

/// Parse a web link without ever making a request to its host. The resulting
/// repository must still be resolved through a configured connector.
pub fn parse_link(link: &str) -> Result<(String, String, String)> {
    let mut url = Url::parse(link).context("Enter a complete pull request URL")?;
    ensure!(
        link.len() <= 2048
            && url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "PR URL must be HTTPS without credentials, query or fragment"
    );
    let path = url.path().trim_end_matches('/').to_owned();
    let parts: Vec<_> = path.split('/').collect();
    ensure!(
        parts.len() >= 5,
        "Expected a GitHub /pull/NUMBER or Azure /pullrequest/NUMBER URL"
    );
    let number = parts[parts.len() - 1];
    ensure!(
        number.parse::<u32>().is_ok_and(|n| n > 0),
        "PR number must be a positive integer"
    );
    let kind = match parts[parts.len() - 2] {
        "pull" => "github_pr",
        "pullrequest" if parts.iter().any(|p| *p == "_git") => "ado_pr",
        _ => bail!("Expected a GitHub /pull/NUMBER or Azure /pullrequest/NUMBER URL"),
    };
    let repository = parts[..parts.len() - 2].join("/");
    url.set_path(&repository);
    Ok((url.to_string(), kind.into(), number.into()))
}

pub struct Provider<'a> {
    pub http: &'a Client,
    pub kind: &'a str,
    pub token: &'a str,
    pub api: &'a str,
}
impl Provider<'_> {
    pub fn request(&self, method: Method, url: &str) -> RequestBuilder {
        let request = self.http.request(method, url);
        if self.kind == "ado" {
            request.basic_auth("", Some(self.token))
        } else {
            request
                .bearer_auth(self.token)
                .header("Accept", "application/vnd.github+json")
        }
    }
    pub fn pr_url(&self, number: &str) -> String {
        if self.kind == "ado" {
            format!("{}/pullrequests/{number}", self.api.trim_end_matches('/'))
        } else {
            format!("{}/pulls/{number}", self.api.trim_end_matches('/'))
        }
    }
    pub async fn metadata(&self, number: &str) -> Result<Value> {
        ensure!(
            number.parse::<u32>().is_ok_and(|n| n > 0),
            "Invalid PR number"
        );
        let request = self.request(Method::GET, &self.pr_url(number));
        read_json(if self.kind == "ado" {
            request.query(&[("api-version", "7.1")])
        } else {
            request
        })
        .await
    }
    pub async fn pages(&self, url: &str) -> Result<Vec<Value>> {
        let mut all = Vec::new();
        let mut size = 0;
        let mut continuation = String::new();
        for page in 1..=MAX_PAGES {
            let mut request = self.request(Method::GET, url);
            if self.kind == "ado" {
                request =
                    request.query(&[("api-version", "7.1"), ("continuationToken", &continuation)]);
            } else {
                request = request.query(&[("per_page", 100), ("page", page)]);
            }
            let response = request
                .send()
                .await
                .context("PR discussion request failed")?;
            let next = response
                .headers()
                .get("x-ms-continuationtoken")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let has_next = response
                .headers()
                .get("link")
                .and_then(|h| h.to_str().ok())
                .is_some_and(|h| h.contains("rel=\"next\""));
            let value = response_json(response).await?;
            size += serde_json::to_vec(&value)?.len();
            ensure!(
                size <= MAX_BYTES,
                "PR discussion exceeds 2 MB; review stopped rather than omitting comments"
            );
            let items = if self.kind == "ado" {
                &value["value"]
            } else {
                &value
            };
            all.extend(
                items
                    .as_array()
                    .context("Invalid PR discussion response")?
                    .iter()
                    .cloned(),
            );
            if self.kind == "ado" {
                if next.is_empty() {
                    return Ok(all);
                }
                ensure!(next != continuation, "Provider repeated a discussion page");
                continuation = next;
            } else if !has_next {
                return Ok(all);
            }
        }
        bail!("PR discussion exceeds pagination limit; no partial review was performed")
    }
    pub async fn discussion(&self, number: &str) -> Result<Value> {
        let pr = self.pr_url(number);
        if self.kind == "ado" {
            return Ok(json!({"threads":self.pages(&format!("{pr}/threads")).await?}));
        }
        let comments = self
            .pages(&format!(
                "{}/issues/{number}/comments",
                self.api.trim_end_matches('/')
            ))
            .await?;
        let reviews = self.pages(&format!("{pr}/reviews")).await?;
        let inline = self.pages(&format!("{pr}/comments")).await?;
        let threads = self.github_threads(number).await?;
        Ok(
            json!({"comments":comments,"reviews":reviews,"inline_comments":inline,"threads":threads}),
        )
    }
    async fn github_threads(&self, number: &str) -> Result<Vec<Value>> {
        let mut endpoint = Url::parse(self.api)?;
        let path = endpoint.path().to_owned();
        let (prefix, repo) = path
            .split_once("/repos/")
            .context("Invalid GitHub API repository URL")?;
        let (owner, name) = repo.split_once('/').context("Invalid GitHub repository")?;
        endpoint.set_path(&format!(
            "{}/graphql",
            prefix.strip_suffix("/v3").unwrap_or(prefix)
        ));
        let mut cursor = Value::Null;
        let mut threads = Vec::new();
        for _ in 0..MAX_PAGES {
            // Replies are fetched in full through the paginated REST comments
            // endpoint. The first comment links each thread's resolution metadata.
            let value = read_json(self.request(Method::POST, endpoint.as_str()).json(&json!({
                "query":"query($owner:String!,$name:String!,$number:Int!,$after:String){repository(owner:$owner,name:$name){pullRequest(number:$number){reviewThreads(first:100,after:$after){nodes{id isResolved isOutdated path line originalLine diffSide comments(first:1){nodes{fullDatabaseId}}} pageInfo{hasNextPage endCursor}}}}}",
                "variables":{"owner":owner,"name":name,"number":number.parse::<u32>()?,"after":cursor}
            }))).await?;
            ensure!(
                value.get("errors").is_none(),
                "Could not fetch PR thread resolution status; check GitHub GraphQL access"
            );
            let connection = &value["data"]["repository"]["pullRequest"]["reviewThreads"];
            threads.extend(
                connection["nodes"]
                    .as_array()
                    .context("Missing PR review threads")?
                    .iter()
                    .cloned(),
            );
            if connection["pageInfo"]["hasNextPage"] == false {
                return Ok(threads);
            }
            let next = connection["pageInfo"]["endCursor"].clone();
            ensure!(
                !next.is_null() && next != cursor,
                "Invalid PR thread pagination cursor"
            );
            cursor = next;
        }
        bail!("PR threads exceed pagination limit; no partial review was performed")
    }
}
pub async fn response_json(mut response: reqwest::Response) -> Result<Value> {
    ensure!(
        response.status().is_success(),
        "Provider request failed (HTTP {}); check connector permissions and resource access",
        response.status()
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= MAX_BYTES,
            "Provider response exceeds 2 MB; context was not truncated"
        );
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).context("Invalid provider JSON response")
}
pub async fn read_json(request: RequestBuilder) -> Result<Value> {
    response_json(request.send().await.context("Provider request failed")?).await
}
pub fn metadata_issue(
    kind: &str,
    number: &str,
    url: &str,
    metadata: &Value,
    ticket: Option<Value>,
) -> Result<Issue> {
    let title = metadata["title"]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("PR title missing")?;
    let body = metadata[if kind == "github" {
        "body"
    } else {
        "description"
    }]
    .as_str()
    .unwrap_or("");
    ensure!(
        title.len() <= 500 && body.len() <= 100_000,
        "PR title or description exceeds supported size"
    );
    Ok(Issue {
        provider: format!("{kind}_pr"),
        key: number.into(),
        title: title.into(),
        body: body.into(),
        url: Some(url.into()),
        ticket,
    })
}
pub fn base_revision(kind: &str, metadata: &Value) -> Result<String> {
    let value = if kind == "github" {
        &metadata["base"]["sha"]
    } else {
        &metadata["lastMergeTargetCommit"]["commitId"]
    };
    let sha = value.as_str().context("PR base commit missing")?;
    ensure!(
        sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid PR base commit"
    );
    Ok(sha.into())
}

pub async fn ticket(app: &App, reference: &str, repository: &str) -> Result<Value> {
    ensure!(
        !reference.is_empty() && reference.len() <= 2048,
        "Invalid ticket reference"
    );
    for kind in ["jira", "github", "ado"] {
        let Some(c) = connectors::load(&app.store, &app.executor.secret, kind).await? else {
            continue;
        };
        if !c.enabled {
            continue;
        }
        c.authorize(repository)?;
        let base = Url::parse(&c.value("base_url")?)?;
        let target = Url::parse(reference).ok();
        let relative = if let Some(target) = &target {
            if target.origin() != base.origin()
                || target.scheme() != "https"
                || !target.username().is_empty()
                || target.password().is_some()
                || target.query().is_some()
                || target.fragment().is_some()
            {
                continue;
            }
            target
                .path()
                .strip_prefix(&format!("{}/", base.path().trim_end_matches('/')))
                .unwrap_or("")
        } else if kind == "jira" {
            reference
        } else {
            continue;
        };
        if kind == "jira" {
            let key = relative.strip_prefix("browse/").unwrap_or(relative);
            if key.contains('/')
                || !key.split_once('-').is_some_and(|(p, n)| {
                    !p.is_empty()
                        && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                        && n.parse::<u64>().is_ok()
                })
            {
                continue;
            }
            let issue = crate::jira::issue(&c, key).await?;
            return Ok(
                json!({"reference":reference,"provider":kind,"title":issue["summary"],"description":issue["description"]}),
            );
        }
        let parts: Vec<_> = relative.split('/').collect();
        let token = c.value("token")?;
        let api = c.value(if kind == "github" {
            "api_url"
        } else {
            "base_url"
        })?;
        let provider = Provider {
            http: &app.http,
            kind,
            token: &token,
            api: &api,
        };
        let mut endpoint = Url::parse(&api)?;
        if kind == "github"
            && parts.len() == 4
            && parts[2] == "issues"
            && parts[3].parse::<u32>().is_ok_and(|n| n > 0)
        {
            endpoint
                .path_segments_mut()
                .map_err(|_| anyhow::anyhow!("Invalid connector API URL"))?
                .pop_if_empty()
                .extend(["repos", parts[0], parts[1], "issues", parts[3]]);
            let value = read_json(provider.request(Method::GET, endpoint.as_str())).await?;
            ensure!(
                value.get("pull_request").is_none(),
                "Optional ticket must be an issue, not another PR"
            );
            return Ok(
                json!({"reference":reference,"provider":kind,"title":value["title"],"description":value["body"]}),
            );
        }
        if kind == "ado"
            && parts.len() == 4
            && parts[1] == "_workitems"
            && parts[2] == "edit"
            && parts[3].parse::<u32>().is_ok_and(|n| n > 0)
        {
            endpoint
                .path_segments_mut()
                .map_err(|_| anyhow::anyhow!("Invalid connector API URL"))?
                .pop_if_empty()
                .extend([parts[0], "_apis", "wit", "workitems", parts[3]]);
            let value = read_json(
                provider
                    .request(Method::GET, endpoint.as_str())
                    .query(&[("api-version", "7.1")]),
            )
            .await?;
            return Ok(
                json!({"reference":reference,"provider":kind,"title":value["fields"]["System.Title"],"description":value["fields"]["System.Description"],"acceptance_criteria":value["fields"]["Microsoft.VSTS.Common.AcceptanceCriteria"]}),
            );
        }
    }
    bail!("Ticket must be a Jira key/link, GitHub issue link, or Azure work item link matching an enabled connector")
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub file: String,
    pub line: u32,
    pub severity: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing_comment_id: Option<u64>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    pub findings: Vec<Finding>,
}
pub fn right_line_in_diff(diff: &str, target: u32) -> bool {
    let mut line = 0u32;
    let mut remaining = 0u32;
    for text in diff.lines() {
        if text.starts_with("@@ ") {
            if let Some(range) = text.split_whitespace().find_map(|s| s.strip_prefix('+')) {
                let (start, count) = range.split_once(',').unwrap_or((range, "1"));
                line = start.parse().unwrap_or(0);
                remaining = count.parse().unwrap_or(0);
            }
        } else if remaining > 0 && (text.starts_with('+') || text.starts_with(' ')) {
            if line == target {
                return true;
            }
            line += 1;
            remaining -= 1;
        }
    }
    false
}
impl Review {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let report: Self = serde_json::from_slice(bytes).context("Invalid review.json")?;
        ensure!(
            report.findings.len() <= 10,
            "Review may contain at most 10 findings"
        );
        for finding in &report.findings {
            ensure!(
                crate::workflow::relative_file(&finding.file)
                    && finding.line > 0
                    && matches!(finding.severity.as_str(), "high" | "medium" | "low")
                    && !finding.message.trim().is_empty()
                    && finding.message.len() <= 5000,
                "Invalid review finding location, severity or message"
            );
        }
        Ok(report)
    }
}
