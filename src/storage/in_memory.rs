use std::collections::{BTreeMap, HashMap};
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

use async_trait::async_trait;

use super::{AsyncFileReader, DirMetadata, FileMetadata, FileStorage, Resource};

pub struct InMemoryStorage {
    data: RwLock<BTreeMap<PathBuf, (Vec<u8>, SystemTime)>>,
}

impl InMemoryStorage {
    pub fn new() -> Self {
        Self {
            data: RwLock::new(BTreeMap::new()),
        }
    }
}

#[async_trait]
impl FileStorage for InMemoryStorage {
    async fn put(&self, path: &Path, mut content: AsyncFileReader) -> io::Result<u64> {
        let mut buffer = Vec::new();
        // Drain the reader into our local buffer
        let bytes_written = tokio::io::copy(&mut content, &mut buffer).await?;

        let mut map = self.data.write().unwrap();
        map.insert(path.to_path_buf(), (buffer, SystemTime::now()));

        Ok(bytes_written)
    }

    async fn get(&self, path: &Path) -> io::Result<Option<AsyncFileReader>> {
        let map = self.data.read().unwrap();

        if let Some((bytes, _)) = map.get(path) {
            // We clone the bytes to give the caller their own owned reader.
            // In an in-memory mock, this is usually acceptable.
            let cursor = Cursor::new(bytes.clone());
            return Ok(Some(Box::pin(cursor)));
        }

        Ok(None)
    }

    async fn delete(&self, path: &Path) -> io::Result<()> {
        let mut map = self.data.write().unwrap();
        map.remove(path);
        Ok(())
    }

    async fn metadata(&self, path: &Path) -> io::Result<Option<FileMetadata>> {
        let map = self.data.read().unwrap();
        Ok(map.get(path).map(|(bytes, modified)| FileMetadata {
            size: bytes.len() as u64,
            modified: *modified,
        }))
    }

    async fn list(&self, prefix: &Path) -> io::Result<Vec<Resource>> {
        let map = self.data.read().unwrap();
        let mut files = Vec::new();
        let mut dirs: HashMap<PathBuf, SystemTime> = HashMap::new();

        for (fpath, (bytes, modified)) in map.iter() {
            if fpath.starts_with(prefix) && fpath != prefix {
                // Find the immediate child component after the prefix
                // e.g. prefix: "a", fpath: "a/b/c.txt" -> child: "b"
                let relative = fpath.strip_prefix(prefix).unwrap();
                let mut components = relative.components();

                if let Some(std::path::Component::Normal(first_part)) = components.next() {
                    let immediate_child = prefix.join(first_part);

                    if components.next().is_some() {
                        // It's a directory (there are more parts after this one)
                        let entry = dirs.entry(immediate_child).or_insert(*modified);
                        if *modified > *entry {
                            *entry = *modified;
                        }
                    } else {
                        // It's a direct file in this directory
                        files.push(Resource::File {
                            path: fpath.clone(),
                            metadata: FileMetadata {
                                size: bytes.len() as u64,
                                modified: *modified,
                            },
                        });
                    }
                }
            }
        }

        let mut results = files;
        for (dpath, latest_mod) in dirs {
            results.push(Resource::Directory {
                path: dpath,
                metadata: DirMetadata {
                    modified: latest_mod,
                },
            });
        }
        Ok(results)
    }
}
