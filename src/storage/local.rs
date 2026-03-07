use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::fs::{self, File};

use super::{AsyncFileReader, DirMetadata, FileMetadata, FileStorage, Resource};

#[derive(Default, Clone)]
pub struct LocalContext {}

pub struct LocalStorage {
    root: PathBuf,
}

impl LocalStorage {
    pub fn new(config: super::config::StorageConfig) -> io::Result<Self> {
        // Ensure the root directory exists
        if !config.root.exists() {
            std::fs::create_dir_all(&config.root)?;
        }
        Ok(Self { root: config.root })
    }

    /// Helper to join the root with the relative path provided by the trait
    fn full_path(&self, path: &Path) -> PathBuf {
        self.root.join(path)
    }

    /// Recursively deletes empty parent directories up to (but not including) the root.
    async fn cleanup_empty_parents(&self, path: &Path) -> io::Result<()> {
        let mut current = path;
        while let Some(parent) = current.parent() {
            // Stop if we've reached the root of our storage
            if parent == self.root || !parent.starts_with(&self.root) {
                break;
            }

            // Check if directory is empty
            let mut entries = fs::read_dir(parent).await?;
            if entries.next_entry().await?.is_none() {
                fs::remove_dir(parent).await?;
                current = parent;
            } else {
                break; // Parent is not empty, stop climbing
            }
        }
        Ok(())
    }
}

#[async_trait]
impl FileStorage for LocalStorage {
    type Context = LocalContext;

    async fn put(
        &self,
        path: &Path,
        mut content: AsyncFileReader,
        _ctx: &Self::Context,
    ) -> io::Result<u64> {
        let full_path = self.full_path(path);

        if full_path.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Cannot overwrite a directory with a file",
            ));
        }

        if let Some(parent) = full_path.parent() {
            if parent.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Parent path component is a file, not a directory",
                ));
            }
            fs::create_dir_all(parent).await?;
        }

        // Create the file and use tokio::io::copy to stream data from the reader
        let mut file = File::create(&full_path).await?;
        let bytes_written = tokio::io::copy(&mut content, &mut file).await?;

        Ok(bytes_written)
    }

    async fn get(&self, path: &Path, _ctx: &Self::Context) -> io::Result<Option<AsyncFileReader>> {
        let full_path = self.full_path(path);

        match File::open(full_path).await {
            Ok(file) => Ok(Some(Box::pin(file))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn delete(&self, path: &Path, _ctx: &Self::Context) -> io::Result<()> {
        let full_path = self.full_path(path);

        // 1. REJECT manual directory deletion
        if full_path.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Manual deletion of directories is not allowed",
            ));
        }

        // 2. Delete file if it exists
        if full_path.exists() {
            fs::remove_file(&full_path).await?;
        } else {
            return Ok(()); // Already gone
        }

        // 3. System-led auto-cleanup of empty parents
        self.cleanup_empty_parents(&full_path).await?;

        Ok(())
    }

    async fn metadata(
        &self,
        path: &Path,
        _ctx: &Self::Context,
    ) -> io::Result<Option<FileMetadata>> {
        let full_path = self.full_path(path);
        match fs::metadata(full_path).await {
            Ok(meta) => Ok(Some(FileMetadata {
                size: meta.len(),
                modified: meta.modified()?,
            })),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn list(&self, path: &Path, _ctx: &Self::Context) -> io::Result<Vec<Resource>> {
        let full_path = self.full_path(path);
        let mut entries = fs::read_dir(full_path).await?;
        let mut resources = Vec::new();

        while let Some(entry) = entries.next_entry().await? {
            let meta = entry.metadata().await?;
            let relative_path = entry
                .path()
                .strip_prefix(&self.root)
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|_| entry.path());

            if meta.is_dir() {
                resources.push(Resource::Directory {
                    path: relative_path,
                    metadata: DirMetadata {
                        modified: meta.modified()?,
                    },
                });
            } else {
                resources.push(Resource::File {
                    path: relative_path,
                    metadata: FileMetadata {
                        size: meta.len(),
                        modified: meta.modified()?,
                    },
                });
            }
        }
        Ok(resources)
    }
}
