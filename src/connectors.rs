use crate::{
    config::{Platform, RepositoryConfig},
    store::Store,
};
use anyhow::{ensure, Context, Result};
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone, Serialize)]
pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub secret: bool,
    pub required: bool,
}
#[derive(Serialize)]
pub struct Definition {
    pub kind: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub fields: Vec<Field>,
}
fn field(key: &'static str, label: &'static str, secret: bool, required: bool) -> Field {
    Field {
        key,
        label,
        secret,
        required,
    }
}
/// New adapters register their fields here; the portal renders this schema.
pub fn definitions() -> Vec<Definition> {
    vec![
        Definition {kind:"ado",name:"Azure DevOps",description:"Access Azure Repos repositories permitted by these credentials.",fields:vec![field("base_url","Organization URL (https://dev.azure.com/your-org)",false,true),field("token","Personal access token (read)",true,true),field("write_token","Write token (optional; read token used if blank)",true,false)]},
        Definition {kind:"github",name:"GitHub",description:"Repository and issue credentials, plus optional webhook verification.",fields:vec![field("base_url","Web URL (https://github.com)",false,true),field("api_url","API URL (https://api.github.com)",false,true),field("token","Access token (read)",true,true),field("write_token","Write token (optional; read token used if blank)",true,false),field("webhook_secret","Webhook signing secret",true,false)]},
        Definition {kind:"jira",name:"Jira Cloud",description:"Fetch issue details using a personal or service-account API token. For scoped tokens, enter the site's Cloud ID.",fields:vec![field("base_url","Site URL (https://your-team.atlassian.net)",false,true),field("issue_fields","Additional issue fields",false,false),field("cloud_id","Cloud ID (required for scoped tokens; blank for unscoped tokens)",false,false),field("email","Account email (service-account email when applicable)",false,true),field("token","API token",true,true),field("webhook_secret","Webhook bearer secret",true,false)]},
        Definition {kind:"slack",name:"Slack",description:"Send approval notifications and verify /factory callbacks. Configure the Slack app separately.",fields:vec![field("token","Bot token",true,true),field("signing_secret","Signing secret",true,true),field("channel","Default approval channel ID",false,true)]},
    ]
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub enabled: bool,
    #[serde(default)]
    pub repositories: Vec<String>,
    pub values: BTreeMap<String, String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub revision: i64,
    pub enabled: bool,
    #[serde(default)]
    pub repositories: Vec<String>,
    pub values: BTreeMap<String, String>,
    #[serde(default)]
    pub clear_secrets: Vec<String>,
}
fn key(master: &str) -> Result<aead::LessSafeKey> {
    let bytes = Sha256::digest(format!("factories/connector-encryption/v1:{master}"));
    let key = aead::UnboundKey::new(&aead::AES_256_GCM, &bytes)
        .map_err(|_| anyhow::anyhow!("connector encryption unavailable"))?;
    Ok(aead::LessSafeKey::new(key))
}
fn seal(master: &str, kind: &str, connection: &Connection) -> Result<Vec<u8>> {
    let mut nonce = [0u8; 12];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow::anyhow!("secure randomness unavailable"))?;
    let mut body = serde_json::to_vec(connection)?;
    key(master)?
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(kind.as_bytes()),
            &mut body,
        )
        .map_err(|_| anyhow::anyhow!("connector encryption failed"))?;
    Ok([nonce.to_vec(), body].concat())
}
fn open(master: &str, kind: &str, ciphertext: &[u8]) -> Result<Connection> {
    ensure!(ciphertext.len() >= 28, "invalid encrypted connector");
    let nonce: [u8; 12] = ciphertext[..12].try_into()?;
    let mut body = ciphertext[12..].to_vec();
    let plain = key(master)?
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(kind.as_bytes()),
            &mut body,
        )
        .map_err(|_| {
            anyhow::anyhow!("cannot decrypt connector; restore the original server master secret")
        })?;
    Ok(serde_json::from_slice(plain)?)
}
impl Connection {
    fn jira_cloud_id(&self) -> Result<Option<uuid::Uuid>> {
        self.values
            .get("cloud_id")
            .filter(|id| !id.trim().is_empty())
            .map(|id| {
                let id =
                    uuid::Uuid::parse_str(id.trim()).context("Jira Cloud ID must be a UUID")?;
                ensure!(!id.is_nil(), "Jira Cloud ID must not be empty");
                Ok(id)
            })
            .transpose()
    }

    pub fn jira_api_base(&self) -> Result<String> {
        match self.jira_cloud_id()? {
            Some(id) => Ok(format!("https://api.atlassian.com/ex/jira/{id}")),
            None => self.value("base_url"),
        }
    }

    pub fn jira_issue_url(&self, key: &str) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&self.jira_api_base()?)?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid Jira URL"))?
            .pop_if_empty()
            .extend(["rest", "api", "3", "issue", key]);
        Ok(url)
    }

    pub fn value(&self, key: &str) -> Result<String> {
        self.values
            .get(key)
            .filter(|v| !v.is_empty())
            .cloned()
            .context("required connector field is not configured")
    }
    pub fn authorize(&self, _repo: &str) -> Result<()> {
        ensure!(self.enabled, "connector is disabled");
        Ok(())
    }
}
pub async fn load(store: &Store, master: &str, kind: &str) -> Result<Option<Connection>> {
    let data: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT ciphertext FROM connectors WHERE kind=$1")
            .bind(kind)
            .fetch_optional(&store.pool)
            .await?;
    data.map(|data| open(master, kind, &data)).transpose()
}
pub async fn list(store: &Store, master: &str) -> Result<Value> {
    let mut items = vec![];
    for definition in definitions() {
        let row: Option<(Vec<u8>, i64, String)> =
            sqlx::query_as("SELECT ciphertext,revision,updated_by FROM connectors WHERE kind=$1")
                .bind(definition.kind)
                .fetch_optional(&store.pool)
                .await?;
        let (connection, revision, updated_by) = match row {
            Some((data, revision, actor)) => {
                (open(master, definition.kind, &data)?, revision, Some(actor))
            }
            None => (Connection::default(), 0, None),
        };
        let values: BTreeMap<_, _> = connection
            .values
            .iter()
            .filter(|(k, _)| {
                definition
                    .fields
                    .iter()
                    .any(|f| f.key == k.as_str() && !f.secret)
            })
            .collect();
        let secrets: BTreeMap<_, _> = definition
            .fields
            .iter()
            .filter(|f| f.secret)
            .map(|f| {
                (
                    f.key,
                    connection.values.get(f.key).is_some_and(|v| !v.is_empty()),
                )
            })
            .collect();
        items.push(json!({"definition":definition,"configured":revision>0,"revision":revision,"enabled":connection.enabled,"repositories":connection.repositories,"values":values,"secrets":secrets,"updated_by":updated_by}));
    }
    Ok(json!({"connectors":items}))
}
fn validate(c: &Connection, definition: &Definition, _platform: &Platform) -> Result<()> {
    if definition.kind == "jira" {
        c.jira_cloud_id()?;
        crate::jira::mappings(c)?;
    }
    ensure!(
        c.values.len() <= definition.fields.len(),
        "unknown connector field"
    );
    for (k, v) in &c.values {
        ensure!(
            definition.fields.iter().any(|f| f.key == k),
            "unknown connector field"
        );
        ensure!(
            v.len() <= 8192 && !v.contains(['\r', '\n', '\0']),
            "invalid connector field"
        );
    }
    for f in &definition.fields {
        if c.enabled && f.required {
            c.value(f.key)?;
        }
        if f.key.ends_with("url") {
            if let Some(v) = c.values.get(f.key).filter(|s| !s.is_empty()) {
                let url = reqwest::Url::parse(v).context("invalid connector URL")?;
                ensure!(
                    url.scheme() == "https"
                        && url.host_str().is_some()
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.query().is_none()
                        && url.fragment().is_none(),
                    "connector URLs require HTTPS without credentials, query or fragment"
                );
            }
        }
    }
    Ok(())
}
fn merge_update(
    old: Connection,
    input: Update,
    definition: &Definition,
    platform: &Platform,
) -> Result<Connection> {
    let mut c = Connection {
        enabled: input.enabled,
        repositories: vec![], // Legacy selections are accepted but no longer restrict access.
        values: input.values,
    };
    for f in &definition.fields {
        if f.secret && c.values.get(f.key).is_none_or(|v| v.is_empty()) {
            if let Some(value) = old.values.get(f.key) {
                c.values.insert(f.key.into(), value.clone());
            }
        }
    }
    for k in input.clear_secrets {
        ensure!(
            definition.fields.iter().any(|f| f.secret && f.key == k),
            "invalid secret field"
        );
        c.values.remove(&k);
    }
    validate(&c, definition, platform)?;
    Ok(c)
}
pub async fn save(
    store: &Store,
    master: &str,
    platform: &Platform,
    kind: &str,
    input: Update,
    actor: &str,
) -> Result<()> {
    let definition = definitions()
        .into_iter()
        .find(|d| d.kind == kind)
        .context("unknown connector type")?;
    let mut tx = store.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,2))")
        .bind(kind)
        .execute(&mut *tx)
        .await?;
    let row: Option<(Vec<u8>, i64)> =
        sqlx::query_as("SELECT ciphertext,revision FROM connectors WHERE kind=$1 FOR UPDATE")
            .bind(kind)
            .fetch_optional(&mut *tx)
            .await?;
    let (old, revision) = match row {
        Some((data, r)) => (open(master, kind, &data)?, r),
        None => (Connection::default(), 0),
    };
    ensure!(
        revision == input.revision,
        "connector was changed by another administrator; reload before saving"
    );
    let c = merge_update(old, input, &definition, platform)?;
    let encrypted = seal(master, kind, &c)?;
    sqlx::query("INSERT INTO connectors(kind,ciphertext,revision,updated_by) VALUES($1,$2,$3,$4) ON CONFLICT(kind) DO UPDATE SET ciphertext=$2,revision=$3,updated_by=$4,updated_at=now()").bind(kind).bind(encrypted).bind(revision+1).bind(actor).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO connector_audit(kind,revision,actor) VALUES($1,$2,$3)")
        .bind(kind)
        .bind(revision + 1)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
fn beneath(base: &str, target: &str) -> Result<bool> {
    let base = reqwest::Url::parse(base)?;
    let target = reqwest::Url::parse(target)?;
    Ok(base.origin() == target.origin()
        && (target.path() == base.path().trim_end_matches('/')
            || target
                .path()
                .starts_with(&format!("{}/", base.path().trim_end_matches('/')))))
}
pub async fn repository_token(
    store: &Store,
    master: &str,
    id: &str,
    repo: &RepositoryConfig,
    write: bool,
) -> Result<Option<String>> {
    if let Some(c) = load(store, master, &repo.provider).await? {
        c.authorize(id)?;
        ensure!(
            beneath(&c.value("base_url")?, &repo.url)?,
            "repository URL is outside connector scope"
        );
        let api = c
            .values
            .get("api_url")
            .cloned()
            .unwrap_or(c.value("base_url")?);
        ensure!(
            beneath(&api, &repo.api_url)?,
            "repository API is outside connector scope"
        );
        return Ok(Some(if write {
            c.values
                .get("write_token")
                .filter(|v| !v.is_empty())
                .cloned()
                .unwrap_or(c.value("token")?)
        } else {
            c.value("token")?
        }));
    }
    let env = if write {
        &repo.write_token_env
    } else {
        &repo.read_token_env
    };
    env.as_ref()
        .map(|name| std::env::var(name).context("repository credential is not configured"))
        .transpose()
}
pub async fn credential(
    store: &Store,
    master: &str,
    kind: &str,
    field: &str,
    env: &str,
) -> Result<String> {
    if let Some(c) = load(store, master, kind).await? {
        ensure!(c.enabled, "connector is disabled");
        c.value(field)
    } else {
        std::env::var(env).context("connector credential is not configured")
    }
}
pub async fn slack_channel(
    store: &Store,
    master: &str,
    platform: &Platform,
    repo: &str,
) -> Result<String> {
    if let Some(c) = load(store, master, "slack").await? {
        c.authorize(repo)?;
        Ok(platform
            .repositories
            .get(repo)
            .and_then(|r| r.slack_channel.clone())
            .unwrap_or(c.value("channel")?))
    } else {
        platform
            .repositories
            .get(repo)
            .and_then(|r| r.slack_channel.clone())
            .or_else(|| std::env::var("FACTORY_SLACK_CHANNEL").ok())
            .context("Slack channel not configured")
    }
}

#[derive(Serialize)]
pub struct Check {
    pub name: String,
    pub status: &'static str,
    pub message: String,
}
fn check(name: &str, status: &'static str, message: &str) -> Check {
    Check {
        name: name.into(),
        status,
        message: message.into(),
    }
}
fn probe_request(
    client: &reqwest::Client,
    kind: &str,
    c: &Connection,
    token: &str,
) -> Result<reqwest::RequestBuilder> {
    let (base, path) = match kind {
        "ado" => (c.value("base_url")?, "_apis/connectionData"),
        "github" => (c.value("api_url")?, "user"),
        "jira" => (c.jira_api_base()?, "rest/api/3/myself"),
        "slack" => ("https://slack.com/api".into(), "auth.test"),
        _ => anyhow::bail!("credential testing is not supported for this connector"),
    };
    let mut url = reqwest::Url::parse(&base).context("invalid connector URL")?;
    url.set_path(&format!("{}/{}", url.path().trim_end_matches('/'), path));
    let request = if kind == "slack" {
        client.post(url)
    } else {
        client.get(url)
    };
    let request = match kind {
        "ado" => request
            .basic_auth("", Some(token))
            .query(&[("api-version", "7.1-preview.1")])
            .header("X-TFS-FedAuthRedirect", "Suppress"),
        "jira" => request.basic_auth(c.value("email")?, Some(token)),
        _ => request.bearer_auth(token),
    };
    Ok(request.header("Accept", "application/json"))
}
fn interpret_probe(
    kind: &str,
    status: reqwest::StatusCode,
    body: &[u8],
) -> std::result::Result<(), &'static str> {
    if !status.is_success() {
        return Err(match status.as_u16() {
            401 => "Authentication rejected. Check the credential and whether it has expired.",
            403 => "Access denied. Check account access, token permissions, and organization policies.",
            404 => "Authentication endpoint not found. Check the configured service URL.",
            429 => "Provider rate limit reached. Retry later.",
            300..=399 => "Provider redirected the request. Check the service URL; redirects are not followed.",
            _ => "Provider returned an unsuccessful response. Retry or check service availability.",
        });
    }
    let data: Value = serde_json::from_slice(body)
        .map_err(|_| "Provider did not return valid JSON. Check the service URL.")?;
    if kind == "slack" && data["ok"] != true {
        return Err(match data["error"].as_str().unwrap_or("") {
            "invalid_auth" | "not_authed" | "token_revoked" | "token_expired"
            | "account_inactive" => {
                "Slack rejected the token. Check that the bot token is valid and active."
            }
            "missing_scope" | "not_allowed_token_type" => {
                "Slack rejected the token type or permissions. Use an installed bot token."
            }
            "ratelimited" => "Provider rate limit reached. Retry later.",
            _ => "Slack could not authenticate this token.",
        });
    }
    let identified = match kind {
        "ado" => {
            data["authenticatedUser"]["id"]
                .as_str()
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok_and(|id| !id.is_nil()))
                && data["authenticatedUser"]["isActive"] == true
                && !data["authenticatedUser"]["descriptor"]
                    .as_str()
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .contains("unauthenticated")
        }
        "github" => {
            data["id"].as_u64().is_some_and(|id| id > 0)
                && data["login"].as_str().is_some_and(|s| !s.is_empty())
        }
        "jira" => {
            data["accountId"].as_str().is_some_and(|s| !s.is_empty()) && data["active"] != false
        }
        "slack" => {
            data["ok"] == true
                && data["user_id"].as_str().is_some_and(|s| !s.is_empty())
                && data["team_id"].as_str().is_some_and(|s| !s.is_empty())
        }
        _ => false,
    };
    if identified {
        Ok(())
    } else {
        Err("Response did not identify an authenticated account. Check the endpoint and credential type.")
    }
}
async fn run_probe(
    kind: &str,
    request: reqwest::RequestBuilder,
) -> std::result::Result<(), &'static str> {
    let mut response=request.send().await.map_err(|e| if e.is_timeout() {"Provider request timed out. Retry or check connectivity."} else {"Could not connect securely to the provider. Check the URL, network, and TLS configuration."})?;
    let status = response.status();
    if !status.is_success() {
        return interpret_probe(kind, status, &[]);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Could not read the provider response.")?
    {
        if body.len() + chunk.len() > 65_536 {
            return Err("Provider response exceeded the test size limit.");
        }
        body.extend_from_slice(&chunk);
    }
    interpret_probe(kind, status, &body)
}
pub async fn jira_draft(
    store: &Store,
    master: &str,
    platform: &Platform,
    mut input: Update,
) -> Result<Connection> {
    let row: Option<(Vec<u8>, i64)> =
        sqlx::query_as("SELECT ciphertext,revision FROM connectors WHERE kind='jira'")
            .fetch_optional(&store.pool)
            .await?;
    let (old, revision) = match row {
        Some((data, r)) => (open(master, "jira", &data)?, r),
        None => (Connection::default(), 0),
    };
    ensure!(
        revision == input.revision,
        "connector was changed by another administrator; reload before previewing"
    );
    input.enabled = false;
    let definition = definitions()
        .into_iter()
        .find(|d| d.kind == "jira")
        .unwrap();
    merge_update(old, input, &definition, platform)
}
/// Test the form snapshot without saving, enabling, or changing any connector.
pub async fn test_connection(
    store: &Store,
    master: &str,
    platform: &Platform,
    kind: &str,
    mut input: Update,
) -> Result<Value> {
    let definition = definitions()
        .into_iter()
        .find(|d| d.kind == kind)
        .context("unknown connector type")?;
    let row: Option<(Vec<u8>, i64)> =
        sqlx::query_as("SELECT ciphertext,revision FROM connectors WHERE kind=$1")
            .bind(kind)
            .fetch_optional(&store.pool)
            .await?;
    let (old, revision) = match row {
        Some((data, r)) => (open(master, kind, &data)?, r),
        None => (Connection::default(), 0),
    };
    ensure!(
        revision == input.revision,
        "connector was changed by another administrator; reload before testing"
    );
    // Only authentication fields are required for a probe, not channel/signing configuration.
    input.enabled = false;
    let c = merge_update(old, input, &definition, platform)?;
    let client = reqwest::Client::builder()
        .user_agent("factories/connector-test")
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let mut checks = vec![];
    for (field, label) in [
        ("token", "Primary credential"),
        ("write_token", "Separate write credential"),
    ] {
        if field == "write_token" && !matches!(kind, "ado" | "github") {
            continue;
        }
        let Some(token) = c.values.get(field).filter(|s| !s.is_empty()) else {
            checks.push(check(label,if field=="token" {"failed"} else {"not_tested"},if field=="token" {"Enter a credential or keep an existing saved credential."} else {"No separate write credential configured; repository writes use the primary credential."}));
            continue;
        };
        match probe_request(&client, kind, &c, token) {
            Err(_) => checks.push(check(
                label,
                "failed",
                "Enter the required service URL and account fields before testing.",
            )),
            Ok(request) => match run_probe(kind, request).await {
                Ok(()) => checks.push(check(
                    label,
                    "passed",
                    "Provider authenticated the credential.",
                )),
                Err(message) => checks.push(check(label, "failed", message)),
            },
        }
    }
    checks.push(check("Operation permissions","not_tested",match kind {
        "slack" => "Authentication does not verify channel membership or permission to post messages. No message was sent.",
        "jira" => "Authentication does not verify access to individual projects or issues.",
        "github" => "Authentication does not verify repository access or write permissions. This check supports user tokens; GitHub App installation tokens are not verified.",
        _ => "Authentication does not verify access to each repository or permission to push or create pull requests.",
    }));
    if definition
        .fields
        .iter()
        .any(|f| f.key == "webhook_secret" || f.key == "signing_secret")
    {
        checks.push(check("Webhook signing/bearer secret","not_tested","A matching incoming provider callback is required to verify this secret; an authentication probe cannot verify it."));
    }
    Ok(
        json!({"ok":!checks.iter().any(|c|c.status=="failed"),"checked_at":chrono::Utc::now(),"checks":checks,"saved":false}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encrypted_credentials_are_bound_to_key_and_provider() {
        let c = Connection {
            values: BTreeMap::from([("token".into(), "test-secret".into())]),
            ..Default::default()
        };
        let sealed = seal("master", "jira", &c).unwrap();
        assert!(!sealed.windows(11).any(|w| w == b"test-secret"));
        assert_eq!(
            open("master", "jira", &sealed)
                .unwrap()
                .value("token")
                .unwrap(),
            "test-secret"
        );
        assert!(open("wrong", "jira", &sealed).is_err());
        assert!(open("master", "slack", &sealed).is_err());
        let mut tampered = sealed;
        tampered[15] ^= 1;
        assert!(open("master", "jira", &tampered).is_err());
    }
    #[test]
    fn endpoint_scope_uses_origin_and_path_boundaries() {
        assert!(beneath(
            "https://dev.azure.com/team",
            "https://dev.azure.com/team/project/repo"
        )
        .unwrap());
        assert!(!beneath(
            "https://dev.azure.com/team",
            "https://dev.azure.com/team-evil/repo"
        )
        .unwrap());
        assert!(!beneath("https://github.com", "https://evil.test/repo").unwrap());
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    #[test]
    fn jira_scoped_tokens_route_probes_and_issues_through_gateway() {
        use base64::Engine;
        let mut c = Connection {
            values: BTreeMap::from([
                ("base_url".into(), "https://team.atlassian.net/".into()),
                ("email".into(), "bot@serviceaccount.atlassian.com".into()),
            ]),
            ..Default::default()
        };
        let client = reqwest::Client::new();
        for cloud_id in [None, Some(""), Some("1a11d016-8984-4c3e-b9ab-142dd06acb1b")] {
            if let Some(id) = cloud_id {
                c.values.insert("cloud_id".into(), id.into());
            }
            let base = if cloud_id.is_some_and(|id| !id.is_empty()) {
                "https://api.atlassian.com/ex/jira/1a11d016-8984-4c3e-b9ab-142dd06acb1b"
            } else {
                "https://team.atlassian.net"
            };
            let request = probe_request(&client, "jira", &c, "test-token")
                .unwrap()
                .build()
                .unwrap();
            assert_eq!(request.url().as_str(), format!("{base}/rest/api/3/myself"));
            assert_eq!(
                request.headers()["authorization"],
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD
                        .encode("bot@serviceaccount.atlassian.com:test-token")
                )
            );
            assert_eq!(
                c.jira_issue_url("DEMO-42").unwrap().as_str(),
                format!("{base}/rest/api/3/issue/DEMO-42")
            );
            assert_eq!(c.value("base_url").unwrap(), "https://team.atlassian.net/");
        }
        for invalid in [
            "not-a-cloud-id",
            "../other",
            "00000000-0000-0000-0000-000000000000",
        ] {
            c.values.insert("cloud_id".into(), invalid.into());
            assert!(c.jira_issue_url("DEMO-42").is_err());
            assert!(probe_request(&client, "jira", &c, "test-token").is_err());
        }
    }

    #[test]
    fn provider_requests_use_expected_authentication_and_paths() {
        let client = reqwest::Client::new();
        let c = Connection {
            values: BTreeMap::from([
                ("base_url".into(), "https://example.test/org".into()),
                ("api_url".into(), "https://api.example.test/api/v3".into()),
                ("email".into(), "user@example.test".into()),
            ]),
            ..Default::default()
        };
        for (kind, path, auth) in [
            ("ado", "/org/_apis/connectionData", "Basic "),
            ("github", "/api/v3/user", "Bearer "),
            ("jira", "/org/rest/api/3/myself", "Basic "),
            ("slack", "/api/auth.test", "Bearer "),
        ] {
            let r = probe_request(&client, kind, &c, "private-token")
                .unwrap()
                .build()
                .unwrap();
            assert_eq!(r.url().path(), path);
            assert!(r.headers()["authorization"]
                .to_str()
                .unwrap()
                .starts_with(auth));
            assert!(!r.url().as_str().contains("private-token"));
            assert_eq!(
                r.method(),
                if kind == "slack" {
                    reqwest::Method::POST
                } else {
                    reqwest::Method::GET
                }
            );
        }
    }
    #[test]
    fn provider_responses_require_identity_and_never_echo_errors() {
        let ok = reqwest::StatusCode::OK;
        for (kind, data) in [
            (
                "ado",
                json!({"authenticatedUser":{"id":uuid::Uuid::new_v4(),"isActive":true}}),
            ),
            ("github", json!({"id":42,"login":"example"})),
            ("jira", json!({"accountId":"account","active":true})),
            (
                "slack",
                json!({"ok":true,"user_id":"U123","team_id":"T123"}),
            ),
        ] {
            assert!(interpret_probe(kind, ok, &serde_json::to_vec(&data).unwrap()).is_ok());
            assert!(interpret_probe(kind, ok, b"{}").is_err());
        }
        for code in [401, 403, 404, 429, 500, 302] {
            let error = interpret_probe(
                "github",
                reqwest::StatusCode::from_u16(code).unwrap(),
                b"secret-value",
            )
            .unwrap_err();
            assert!(!error.contains("secret-value"));
        }
        assert!(
            interpret_probe("slack", ok, br#"{"ok":false,"error":"invalid_auth"}"#)
                .unwrap_err()
                .contains("rejected")
        );
        assert!(
            !interpret_probe("slack", ok, br#"{"ok":false,"error":"secret-value"}"#)
                .unwrap_err()
                .contains("secret-value")
        );
        assert!(interpret_probe("github", ok, b"<html>Sign in</html>").is_err());
        assert!(interpret_probe("ado",ok,br#"{"authenticatedUser":{"id":"00000000-0000-0000-0000-000000000000","isActive":true}}"#).is_err());
    }
    #[tokio::test]
    async fn probe_http_transport_rejects_redirects_and_handles_provider_errors() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (status, body, expected) in [
            ("200 OK", r#"{"id":1,"login":"user"}"#, true),
            ("200 OK", r#"{"message":"private-token"}"#, false),
            ("401 Unauthorized", "private-token", false),
            ("302 Found", "", false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = [0; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                assert!(String::from_utf8_lossy(&buf[..n]).starts_with("GET /user "));
                let response=format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nLocation: http://127.0.0.1:1/should-not-follow\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            });
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(1))
                .build()
                .unwrap();
            let result = run_probe(
                "github",
                client
                    .get(format!("http://{addr}/user"))
                    .bearer_auth("private-token"),
            )
            .await;
            assert_eq!(result.is_ok(), expected);
            if let Err(message) = result {
                assert!(!message.contains("private-token"));
            }
            server.await.unwrap();
        }
    }
}
