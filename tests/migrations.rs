use sha2::{Digest, Sha384};

#[test]
fn historical_migration_bytes_match_applied_database_checksums() {
    // SQLx checks bytes, including line endings. These are the original applied
    // migrations; schema changes belong in new files, not edits to these files.
    for (file, expected) in [
        ("0001_control_plane.sql", "efb5c63f2463eb6abfc9096c286ddc48368b9cff81356f693385d0881e8db0d16036c095c2d0cd863f0156dcbbcb631d"),
        ("0002_connectors.sql", "adfdaddae850c4cbbfe042b5f2438f38fddb5e82eb94a92fedd0620c9f2e25d52b02a2827eba9509e1cdaa3111dc290c"),
        ("0003_releases.sql", "05626cd3eed469b536cb232b3ae5b6266bb85ad41a5431de432bd8cb356572d2eed401001bb188d90f18c2a11b4cb15b"),
        ("0004_design_workspace.sql", "2c51feb9e6a33d38978de25c4cb612627614b244aeeefd4acbd962b0ba0b2869a5f2c3c75c15e1233b9d416861e3ed29"),
    ] {
        let bytes = std::fs::read(format!("migrations/{file}")).unwrap();
        assert_eq!(hex::encode(Sha384::digest(bytes)), expected, "{file} changed, possibly through line-ending conversion");
    }
}
