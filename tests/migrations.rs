use sha2::{Digest, Sha384};

#[test]
fn historical_migration_bytes_match_applied_database_checksums() {
    // SQLx checks bytes, including line endings. These are the original applied
    // migrations; schema changes belong in new files, not edits to these files.
    for (file, expected) in [
        ("0001_control_plane.sql", "efb5c63f2463eb6abfc9096c286ddc48368b9cff81356f693385d0881e8db0d16036c095c2d0cd863f0156dcbbcb631d"),
        ("0002_connectors.sql", "f4305cee62e3b070e1694ab89e176e85d384e834d3050d7aed38116a841b1d334552d9b8398cf97b314da7a0a700c1de"),
        ("0003_releases.sql", "4116ce050769eadc0dec47618cead3dc897e99a0d0a802b6ae62b9172775401dea4e618686bf9b29c9a45761d3fde7cc"),
        ("0004_design_workspace.sql", "514089f6c0d4bf56abf2979f65864aeb9f076c970bd8987e0b771fdc9e4705e9b6e67b7215b885d53d2816e197e85a94"),
    ] {
        let bytes = std::fs::read(format!("migrations/{file}")).unwrap();
        assert_eq!(hex::encode(Sha384::digest(bytes)), expected, "{file} changed, possibly through line-ending conversion");
    }
}
