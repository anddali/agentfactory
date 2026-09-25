use factories::config::Platform;
use std::path::Path;

#[test]
fn harness_settings_and_phase_overrides_are_validated_and_pinned() {
    let platform = Platform::load(Path::new("config/platform.yaml")).unwrap();
    let snapshot = platform
        .snapshot(Path::new("workflows"), Path::new("prompts"))
        .unwrap();
    let profile = &snapshot.agents["coding-default"];
    let settings = profile.openhands.as_ref().unwrap();
    assert_eq!(settings.sdk_version, "1.49.2");
    let mut changed = snapshot.clone();
    changed
        .agents
        .get_mut("coding-default")
        .unwrap()
        .openhands
        .as_mut()
        .unwrap()
        .model = "another/model".into();
    changed.refresh_hash().unwrap();
    assert_ne!(snapshot.definition_hash, changed.definition_hash);
    let mut settings = settings.clone();
    settings.max_iterations = 0;
    assert!(settings.validate(&profile.env_keys).is_err());
    settings.max_iterations = 10;
    settings.base_url = Some("https://user:secret@example.com".into());
    assert!(settings.validate(&profile.env_keys).is_err());
    settings.base_url = None;
    settings.tools.push("unapproved".into());
    assert!(settings.validate(&profile.env_keys).is_err());
}

#[test]
fn unknown_phase_agent_profile_is_rejected() {
    let platform = Platform::load(Path::new("config/platform.yaml")).unwrap();
    let directory = tempfile::tempdir().unwrap();
    for file in std::fs::read_dir("workflows").unwrap() {
        let file = file.unwrap();
        let text = std::fs::read_to_string(file.path()).unwrap().replace(
            "- id: research",
            "- id: research\n    agentProfile: nonexistent",
        );
        std::fs::write(directory.path().join(file.file_name()), text).unwrap();
    }
    assert!(platform
        .snapshot(directory.path(), Path::new("prompts"))
        .is_err());
}
