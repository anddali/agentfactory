use factories::storage::Blobs;
#[tokio::test]
async fn content_addressed_storage_detects_corruption_and_rejects_traversal() {
    let directory = tempfile::tempdir().unwrap();
    let blobs = Blobs {
        root: directory.path().into(),
        bucket: None,
    };
    let digest = blobs.put(b"approved research").await.unwrap();
    assert_eq!(digest, blobs.put(b"approved research").await.unwrap());
    assert_eq!(blobs.get(&digest).await.unwrap(), b"approved research");
    assert!(blobs.get("../../secret").await.is_err());
    tokio::fs::write(
        directory
            .path()
            .join(format!("sha256/{}/{}", &digest[..2], digest)),
        b"modified",
    )
    .await
    .unwrap();
    assert!(blobs.get(&digest).await.is_err());
}
