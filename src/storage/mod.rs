pub mod in_memory;

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub size: u64,
    pub modified: SystemTime,
}

#[derive(Debug, Clone)]
pub struct DirMetadata {
    pub modified: SystemTime,
}

#[derive(Debug, Clone)]
pub enum Resource {
    File {
        path: PathBuf,
        metadata: FileMetadata,
    },
    Directory {
        path: PathBuf,
        metadata: DirMetadata,
    },
}

impl Resource {
    pub fn path(&self) -> &Path {
        match self {
            Resource::File { path, .. } => path,
            Resource::Directory { path, .. } => path,
        }
    }
}

#[async_trait]
pub trait FileStorage: Send + Sync {
    async fn put(&self, path: &Path, content: Vec<u8>) -> io::Result<()>;
    async fn get(&self, path: &Path) -> io::Result<Option<Vec<u8>>>;
    async fn delete(&self, path: &Path) -> io::Result<()>;
    async fn metadata(&self, path: &Path) -> io::Result<Option<FileMetadata>>;
    async fn list(&self, path: &Path) -> io::Result<Vec<Resource>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    pub async fn run_consistency_test_suite<S: FileStorage>(storage: S) {
        let path = Path::new("consistency_test.txt");
        let content = b"universal data".to_vec();

        // 1. Test Put & Get
        storage
            .put(path, content.clone())
            .await
            .expect("Put failed");
        let retrieved = storage.get(path).await.expect("Get failed");
        assert_eq!(retrieved, Some(content), "Data mismatch after Put/Get");

        // 2. Test Metadata
        let meta = storage
            .metadata(path)
            .await
            .expect("Metadata failed")
            .expect("Metadata should be Some after put");
        assert_eq!(meta.size, 14);

        // 3. Test Listing
        let dir = Path::new("test_dir");
        let file1 = dir.join("file1.tmp");
        let file2 = dir.join("file2.tmp");

        storage.put(&file1, b"1".to_vec()).await.unwrap();
        storage.put(&file2, b"2".to_vec()).await.unwrap();

        let list = storage.list(dir).await.expect("List failed");
        assert!(list.iter().any(|r| r.path() == file1));
        assert!(list.iter().any(|r| r.path() == file2));

        // 4. Test Delete & Not Found
        storage.delete(path).await.expect("Delete failed");
        let after_delete = storage.get(path).await.expect("Get after delete failed");
        assert!(
            after_delete.is_none(),
            "Resource should be gone after delete"
        );
    }

    #[tokio::test]
    async fn test_in_memory_consistency() {
        let storage = in_memory::InMemoryStorage::new();
        run_consistency_test_suite(storage).await;
    }
}
