use factories::providers::{pull_request_context, safe_branch};
use serde_json::json;
#[test]
fn github_pr_pins_the_source_revision_and_rejects_fork_authority() {
    let mut value = json!({"head":{"sha":"a".repeat(40),"ref":"feature/fix","repo":{"clone_url":"https://github.com/example/repo.git"}},"base":{"ref":"main"}});
    let context =
        pull_request_context("github", "https://github.com/example/repo.git", &value).unwrap();
    assert_eq!(
        context,
        ("a".repeat(40), "feature/fix".into(), "main".into())
    );
    value["head"]["repo"]["clone_url"] = json!("https://github.com/someone/fork.git");
    assert!(pull_request_context("github", "https://github.com/example/repo.git", &value).is_err());
}
#[test]
fn github_pr_accepts_web_urls_without_weakening_repository_authority() {
    let value = json!({"head":{"sha":"a".repeat(40),"ref":"feature/fix","repo":{"clone_url":"https://github.com/example/repo.git"}},"base":{"ref":"main"}});
    for url in [
        "https://github.com/example/repo",
        "https://github.com/example/repo/",
        "https://github.com/example/repo.git",
        "https://github.com/example/repo.git/",
        "https://GitHub.com/Example/Repo",
    ] {
        assert!(pull_request_context("github", url, &value).is_ok(), "{url}");
    }
    for url in [
        "https://github.com/other/repo",
        "https://github.com/example/repo-other",
        "https://other.example/example/repo",
        "https://github.com/example/repo?extra=1",
        "https://github.com/example/repo#fragment",
        "https://token@github.com/example/repo",
        "https://github.com/example%2frepo",
    ] {
        assert!(
            pull_request_context("github", url, &value).is_err(),
            "{url}"
        );
    }
    let mut missing = value;
    missing["head"]["repo"] = serde_json::Value::Null;
    assert!(pull_request_context("github", "https://github.com/example/repo", &missing).is_err());
}
#[test]
fn ado_pr_normalizes_refs_and_rejects_the_base_branch() {
    let mut value = json!({"lastMergeSourceCommit":{"commitId":"b".repeat(40)},"sourceRefName":"refs/heads/feature/fix","targetRefName":"refs/heads/main"});
    assert_eq!(
        pull_request_context(
            "ado",
            "https://dev.azure.com/example/project/_git/repo",
            &value
        )
        .unwrap()
        .1,
        "feature/fix"
    );
    value["sourceRefName"] = json!("refs/heads/main");
    assert!(pull_request_context(
        "ado",
        "https://dev.azure.com/example/project/_git/repo",
        &value
    )
    .is_err());
}
#[test]
fn unsafe_ref_names_are_rejected() {
    for value in [
        "",
        "--upload-pack=evil",
        "../../main",
        "branch@{1}",
        "branch:main",
    ] {
        assert!(!safe_branch(value));
    }
}
