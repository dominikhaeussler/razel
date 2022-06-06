use std::path::Path;

use sha2::Sha256;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, BufReader};

use crate::bazel_remote_exec::{ActionResult, Digest};

trait ActionCache {
    /// like rpc GetActionResult(GetActionResultRequest) returns (ActionResult)
    fn get(&self, digest: ActionDigest) -> Option<ActionResult>;

    /// like rpc UpdateActionResult(UpdateActionResultRequest) returns (ActionResult)
    fn push(&self, digest: ActionDigest, result: ActionResult);
}

trait ContentAddressableStorage {
    // like rpc BatchReadBlobs(BatchReadBlobsRequest) returns (BatchReadBlobsResponse)
    fn get(&self, digest: BlobDigest) -> Option<Vec<u8>>;

    /// like rpc BatchUpdateBlobs(BatchUpdateBlobsRequest) returns (BatchUpdateBlobsResponse)
    fn push(&self, digest: BlobDigest, blob: Vec<u8>);
}

type ActionDigest = Digest;
type BlobDigest = Digest;

impl Digest {
    pub async fn for_file(path: impl AsRef<Path>) -> Result<BlobDigest, anyhow::Error> {
        use sha2::Digest;
        let file = File::open(path).await?;
        let mut reader = BufReader::new(file);
        let mut hasher = Sha256::new();
        let mut buffer = [0; 1024];
        let mut len = 0;
        loop {
            let count = reader.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            len += count;
        }
        Ok(crate::bazel_remote_exec::Digest {
            hash: Self::hex(&hasher.finalize()),
            size_bytes: len as i64,
        })
    }

    pub fn for_action(_path: &Path) -> ActionDigest {
        todo!()
    }

    pub fn hex(input: &[u8]) -> String {
        base16ct::lower::encode_string(input)
    }
}

#[cfg(test)]
mod tests {
    use sha2::Digest;

    use super::*;

    fn digest_file_sha256_simple(path: impl AsRef<Path>) -> Result<super::Digest, anyhow::Error> {
        let bytes = std::fs::read(path)?;
        Ok(super::Digest {
            hash: super::Digest::hex(&Sha256::digest(&bytes)),
            size_bytes: bytes.len() as i64,
        })
    }

    #[tokio::test]
    async fn small_file() {
        let path = "test/data/a.csv";
        let act = super::Digest::for_file(&path).await.unwrap();
        let exp = digest_file_sha256_simple(&path).unwrap();
        assert_eq!(act, exp);
        assert_eq!(
            act,
            super::Digest {
                hash: "e0f702d446912234e5767af1db3f8b23b04beade5cdd1ea72d78c4f88c869b80".into(), // sha256sum test/data/a.csv
                size_bytes: 16,
            }
        );
    }

    #[tokio::test]
    async fn bigger_file() {
        let path = "Cargo.lock";
        let act = super::Digest::for_file(&path).await.unwrap();
        let exp = digest_file_sha256_simple(&path).unwrap();
        assert_eq!(act, exp);
    }
}