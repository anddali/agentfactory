use crate::model::hash;
use anyhow::{ensure, Result};
use std::path::PathBuf;

#[derive(Clone)]
pub struct Blobs {
    pub root: PathBuf,
    pub bucket: Option<String>,
}
impl Blobs {
    fn key(digest: &str) -> Result<String> {
        ensure!(
            digest.len() == 64 && digest.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid content hash"
        );
        Ok(format!("sha256/{}/{}", &digest[..2], digest))
    }
    pub async fn put(&self, content: &[u8]) -> Result<String> {
        ensure!(
            content.len() <= 10 * 1024 * 1024,
            "artifact exceeds 10 MiB limit"
        );
        let digest = hash(content);
        let key = Self::key(&digest)?;
        let path = self.root.join(&key);
        tokio::fs::create_dir_all(path.parent().unwrap()).await?;
        // A unique temporary file plus atomic rename prevents a reader seeing a partial upload.
        let temp = path.with_extension(uuid::Uuid::new_v4().to_string());
        tokio::fs::write(&temp, content).await?;
        if let Some(bucket) = &self.bucket {
            let output = tokio::process::Command::new("aws")
                .args([
                    "s3api",
                    "put-object",
                    "--bucket",
                    bucket,
                    "--key",
                    &key,
                    "--body",
                ])
                .arg(&temp)
                .args(["--server-side-encryption", "AES256"])
                .output()
                .await?;
            tokio::fs::remove_file(&temp).await?;
            ensure!(output.status.success(), "S3 artifact upload failed");
        } else if path.exists() {
            tokio::fs::remove_file(temp).await?;
        } else {
            match tokio::fs::rename(&temp, &path).await {
                Ok(()) => (),
                Err(_) if path.exists() => {
                    let _ = tokio::fs::remove_file(temp).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(digest)
    }
    pub async fn get(&self, digest: &str) -> Result<Vec<u8>> {
        let key = Self::key(digest)?;
        let bytes = if let Some(bucket) = &self.bucket {
            tokio::fs::create_dir_all(&self.root).await?;
            let path = self.root.join(uuid::Uuid::new_v4().to_string());
            let output = tokio::process::Command::new("aws")
                .args(["s3api", "get-object", "--bucket", bucket, "--key", &key])
                .arg(&path)
                .output()
                .await?;
            if !output.status.success() {
                let _ = tokio::fs::remove_file(&path).await;
                anyhow::bail!("S3 artifact download failed");
            }
            let result = tokio::fs::read(&path).await;
            let _ = tokio::fs::remove_file(path).await;
            result?
        } else {
            tokio::fs::read(self.root.join(key)).await?
        };
        ensure!(hash(&bytes) == digest, "artifact integrity check failed");
        Ok(bytes)
    }
}
