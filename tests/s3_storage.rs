use std::path::Path;

use filepaste::storage::StorageProvider;
use filepaste::storage::s3::S3Storage;
use tokio::io::AsyncReadExt;

#[tokio::test]
#[ignore = "requires a real S3 bucket"]
async fn s3_round_trip_uses_a_real_bucket() {
    let Ok(bucket) = std::env::var("FILEPASTE_S3_BUCKET") else {
        return;
    };
    let Ok(region) = std::env::var("FILEPASTE_S3_REGION") else {
        return;
    };
    let prefix =
        std::env::var("FILEPASTE_S3_PREFIX").unwrap_or_else(|_| "filepaste-test".to_owned());
    let endpoint = std::env::var("FILEPASTE_S3_ENDPOINT").ok();

    let storage = S3Storage::new(bucket, region, prefix, endpoint)
        .await
        .expect("create S3 storage");
    let ctx = Default::default();
    let path = Path::new("integration-test.txt");
    let content = b"s3 integration";

    let written = storage
        .put(path, Box::pin(&content[..]), &ctx)
        .await
        .expect("put object");
    assert_eq!(written, content.len() as u64);

    let mut reader = storage
        .get(path, &ctx)
        .await
        .expect("get object")
        .expect("object exists");
    let mut received = Vec::new();
    reader
        .read_to_end(&mut received)
        .await
        .expect("read object");
    assert_eq!(received, content.to_vec());

    let metadata = storage
        .metadata(path, &ctx)
        .await
        .expect("read metadata")
        .expect("metadata exists");
    assert_eq!(metadata.size, content.len() as u64);

    let listing = storage
        .list(Path::new(""), &ctx)
        .await
        .expect("list objects");
    assert!(listing.iter().any(|item| item.path() == path));

    storage.delete(path, &ctx).await.expect("delete object");
    assert!(
        storage
            .get(path, &ctx)
            .await
            .expect("get after delete")
            .is_none()
    );
}
