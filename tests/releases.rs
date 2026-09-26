use anyhow::Result;
use factories::{config::Platform, releases::Bundle};
use std::path::Path;
fn fixture() -> (Platform, Bundle) {
    let p = Platform::load(Path::new("config/platform.yaml")).unwrap();
    let s = p
        .snapshot(Path::new("workflows"), Path::new("prompts"))
        .unwrap();
    let b = Bundle::from_snapshot(&s, "research-plan").unwrap();
    (p, b)
}
#[test]
fn bundles_are_complete_scoped_and_hash_content_not_export_metadata() -> Result<()> {
    let (p, b) = fixture();
    let s = b.resolve(&p)?;
    assert_eq!(s.workflows.len(), 1);
    assert_eq!(s.prompts.len(), 3);
    assert!(!s.agents.contains_key("fixture"));
    let mut edited = b.clone();
    edited.base_generation = 99;
    assert_eq!(b.digest()?, edited.digest()?);
    edited
        .workflows
        .get_mut("research-plan")
        .unwrap()
        .push_str("\n# comment\n");
    assert_eq!(b.digest()?, edited.digest()?);
    edited
        .prompts
        .get_mut("research@3")
        .unwrap()
        .push_str(" Changed instruction.");
    assert_ne!(b.digest()?, edited.digest()?);
    Ok(())
}
#[test]
fn missing_extra_and_unsafe_dependencies_are_rejected() {
    let (p, b) = fixture();
    let mut broken = b.clone();
    broken.prompts.remove("research@3");
    assert!(broken.resolve(&p).is_err());
    let mut broken = b.clone();
    broken.prompts.insert("unused@1".into(), "extra".into());
    assert!(broken.resolve(&p).is_err());
    let mut broken = b.clone();
    broken
        .workflows
        .get_mut("research-plan")
        .unwrap()
        .push_str("\nunknownField: true\n");
    assert!(broken.resolve(&p).is_err());
    let mut broken = b.clone();
    broken
        .workflows
        .get_mut("research-plan")
        .unwrap()
        .push_str("\nfollowUps:\n  - workflow: missing\n    when: always\n    maxDepth: 2\n");
    assert!(broken.resolve(&p).is_err());
}
#[test]
fn follow_up_closure_is_exported_even_for_cycles() -> Result<()> {
    let (p, _) = fixture();
    let s = p.snapshot(Path::new("workflows"), Path::new("prompts"))?;
    let b = Bundle::from_snapshot(&s, "pr-review")?;
    assert!(b.workflows.contains_key("pr-review-fix"));
    b.resolve(&p)?;
    Ok(())
}
#[test]
fn offline_roundtrip_and_locked_change_detection() -> Result<()> {
    let (_, b) = fixture();
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("bundle.json");
    std::fs::write(&source, serde_json::to_vec(&b)?)?;
    let output = dir.path().join("local");
    let binary = env!("CARGO_BIN_EXE_factory-bundle");
    assert!(std::process::Command::new(binary)
        .args(["unpack", "config/platform.yaml"])
        .arg(&source)
        .arg(&output)
        .status()?
        .success());
    let packed = dir.path().join("packed.json");
    let pack = || {
        std::process::Command::new(binary)
            .args(["pack", "config/platform.yaml", "research-plan"])
            .arg(&output)
            .arg(&packed)
            .arg("--locked")
            .output()
    };
    assert!(pack()?.status.success());
    let roundtrip: Bundle = serde_json::from_slice(&std::fs::read(&packed)?)?;
    assert_eq!(b.digest()?, roundtrip.digest()?);
    std::fs::write(output.join("prompts/research@3.md"), "changed")?;
    assert!(!pack()?.status.success());
    Ok(())
}
