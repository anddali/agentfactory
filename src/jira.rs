use crate::connectors::Connection;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    pub id: String,
    pub heading: String,
}
pub fn mappings(c: &Connection) -> Result<Vec<Mapping>> {
    let raw = c.values.get("issue_fields").filter(|s| !s.is_empty());
    let fields: Vec<Mapping> = serde_json::from_str(raw.map(String::as_str).unwrap_or("[]"))
        .context("Invalid Jira additional fields")?;
    ensure!(
        fields.len() <= 20,
        "At most 20 additional Jira fields are allowed"
    );
    let mut seen = std::collections::HashSet::new();
    for f in &fields {
        ensure!(
            !f.id.is_empty()
                && f.id.len() <= 100
                && f.id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && f.id != "summary"
                && f.id != "description"
                && seen.insert(&f.id),
            "Invalid or duplicate Jira field ID"
        );
        ensure!(
            !f.heading.trim().is_empty()
                && f.heading.len() <= 120
                && !f.heading.chars().any(char::is_control),
            "Invalid Jira field heading"
        );
    }
    Ok(fields)
}
pub fn text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(_) | Value::Number(_) => v.to_string(),
        Value::Array(items) => items
            .iter()
            .map(text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        Value::Object(_) => {
            if let Some(kind) = v["type"].as_str() {
                if kind == "text" {
                    return v["text"].as_str().unwrap_or("").into();
                }
                if kind == "hardBreak" {
                    return "\n".into();
                }
                if kind == "mention" {
                    return text(&v["attrs"]["text"]);
                }
                if kind == "inlineCard" {
                    return text(&v["attrs"]["url"]);
                }
                if let Some(nodes) = v["content"].as_array() {
                    let body: String = nodes.iter().map(text).collect();
                    return match kind {
                        "listItem" => format!("- {}\n", body.trim()),
                        "paragraph" | "heading" | "codeBlock" | "blockquote" | "tableRow" => {
                            format!("{body}\n")
                        }
                        "tableCell" | "tableHeader" => format!("{}\t", body.trim()),
                        _ => body,
                    };
                }
            }
            for key in ["displayName", "value", "name", "key"] {
                if !v[key].is_null() {
                    return text(&v[key]);
                }
            }
            // Unknown structured fields remain available to the agent without losing data.
            v.to_string()
        }
    }
}
pub fn compose(c: &Connection, data: &Value) -> Result<Value> {
    let fields = &data["fields"];
    let mut body = text(&fields["description"]).trim().to_owned();
    let mut statuses = vec![];
    for f in mappings(c)? {
        let value = fields.get(&f.id);
        let content = value.map(text).unwrap_or_default();
        let status = if value.is_none() {
            "unavailable"
        } else if content.trim().is_empty() {
            "empty"
        } else {
            "included"
        };
        if status == "included" {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&format!("## {}\n{}", f.heading.trim(), content.trim()));
        }
        statuses.push(json!({"id":f.id,"heading":f.heading,"status":status}));
    }
    ensure!(
        body.len() <= 100_000,
        "Combined Jira task details exceed 100 KB"
    );
    Ok(
        json!({"key":data["key"],"summary":fields["summary"],"description":body,"field_statuses":statuses}),
    )
}
pub async fn get(c: &Connection, url: reqwest::Url) -> Result<Value> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut response = client
        .get(url)
        .basic_auth(c.value("email")?, Some(c.value("token")?))
        .send()
        .await
        .context("Jira request failed")?;
    ensure!(
        response.status().is_success(),
        "Jira request rejected; check credentials, scopes and issue access"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("Could not read Jira response")?
    {
        ensure!(
            bytes.len() + chunk.len() <= 2_000_000,
            "Jira response is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).context("Invalid Jira response")
}
pub async fn issue(c: &Connection, key: &str) -> Result<Value> {
    ensure!(
        !key.is_empty() && key.len() <= 120,
        "Invalid Jira issue key"
    );
    let mut fields = vec!["summary".to_owned(), "description".to_owned()];
    fields.extend(mappings(c)?.into_iter().map(|f| f.id));
    let mut url = c.jira_issue_url(key)?;
    url.query_pairs_mut()
        .append_pair("fields", &fields.join(","));
    compose(c, &get(c, url).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn connection(fields: Value) -> Connection {
        Connection {
            values: [("issue_fields".into(), fields.to_string())].into(),
            ..Default::default()
        }
    }
    #[test]
    fn composes_ordered_fields_and_reports_empty_and_missing() {
        let c = connection(json!([
            {"id":"customfield_1","heading":"Acceptance criteria"},
            {"id":"labels","heading":"Labels"},
            {"id":"customfield_2","heading":"Empty"},
            {"id":"customfield_3","heading":"Missing"}
        ]));
        let issue = json!({"key":"TEST-1","fields":{
            "summary":"Test", "description":{"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Build this."}]}]},
            "customfield_1":{"type":"doc","content":[{"type":"bulletList","content":[{"type":"listItem","content":[{"type":"paragraph","content":[{"type":"text","text":"Works correctly"}]}]}]}]},
            "labels":["one","two"],"customfield_2":null
        }});
        let out = compose(&c, &issue).unwrap();
        assert_eq!(
            out["description"],
            "Build this.\n\n## Acceptance criteria\n- Works correctly\n\n## Labels\none, two"
        );
        assert_eq!(out["field_statuses"][2]["status"], "empty");
        assert_eq!(out["field_statuses"][3]["status"], "unavailable");
        assert_eq!(text(&json!({"displayName":"Some User"})), "Some User");
        assert_eq!(
            text(&json!([{"value":"Option"},42,false])),
            "Option, 42, false"
        );
        assert_eq!(
            compose(
                &Connection::default(),
                &json!({"fields":{"description":null}})
            )
            .unwrap()["description"],
            ""
        );
    }
    #[test]
    fn rejects_duplicate_reserved_and_malformed_mappings_and_large_content() {
        for fields in [
            json!([{"id":"description","heading":"Duplicate"}]),
            json!([{"id":"x","heading":""}]),
            json!([{"id":"x","heading":"a\nb"}]),
            json!([{"id":"*all","heading":"Bad"}]),
            json!([{"id":"x","heading":"One"},{"id":"x","heading":"Two"}]),
        ] {
            assert!(mappings(&connection(fields)).is_err());
        }
        assert!(compose(
            &Connection::default(),
            &json!({"fields":{"description":"a".repeat(100001)}})
        )
        .is_err());
    }
}
