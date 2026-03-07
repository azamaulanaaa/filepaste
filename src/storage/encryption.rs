use std::io;
use std::path::Path;

use async_trait::async_trait;
use chacha20poly1305::{
    ChaCha20Poly1305, Key, KeyInit,
    aead::stream::{DecryptorBE32, EncryptorBE32},
};
use futures::{StreamExt, stream};
use tokio::io::AsyncReadExt;
use tokio_util::{bytes::Bytes, io::StreamReader};

use super::{AsyncFileReader, FileMetadata, Resource, StorageProvider};

pub const CHUNK_SIZE: usize = 64 * 1024; // 64KB chunks
pub const TAG_SIZE: usize = 16; // Poly1305 tag
pub const NONCE_SIZE: usize = 7; // ChaCha20 stream nonce

#[derive(Clone, Default)]
pub struct EncryptedContext<C> {
    pub inner: C,
    pub key: [u8; 32],
}

pub struct EncryptedStorage<S: StorageProvider> {
    inner: S,
}

impl<S: StorageProvider> EncryptedStorage<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }

    /// Mathematically reconstructs the original plaintext size from the encrypted size.
    fn plaintext_size(encrypted_size: u64) -> u64 {
        let min_size = (NONCE_SIZE + TAG_SIZE) as u64;

        // If it's smaller than a nonce + 1 tag, it's incomplete/corrupted. We default to 0.
        if encrypted_size < min_size {
            return 0;
        }

        let ciphertext_size = encrypted_size - NONCE_SIZE as u64;
        let chunk_with_tag = (CHUNK_SIZE + TAG_SIZE) as u64;

        let full_chunks = ciphertext_size / chunk_with_tag;
        let last_chunk_ciphertext = ciphertext_size % chunk_with_tag;

        // Safety check against corruption (a chunk can't be smaller than its tag)
        let last_chunk_plaintext = if last_chunk_ciphertext >= TAG_SIZE as u64 {
            last_chunk_ciphertext - TAG_SIZE as u64
        } else {
            0
        };

        (full_chunks * CHUNK_SIZE as u64) + last_chunk_plaintext
    }
}

#[async_trait]
impl<S: StorageProvider> StorageProvider for EncryptedStorage<S> {
    type Context = EncryptedContext<S::Context>;

    async fn put(
        &self,
        path: &Path,
        content: AsyncFileReader,
        ctx: &Self::Context,
    ) -> io::Result<u64> {
        let mut nonce = [0u8; NONCE_SIZE];
        rand::fill(&mut nonce);

        let key = Key::from_slice(&ctx.key);
        let cipher = ChaCha20Poly1305::new(key);
        let encryptor = EncryptorBE32::from_aead(cipher, &nonce.into());

        let nonce_stream = stream::once(async move { Ok(Bytes::from(nonce.to_vec())) });

        // Wrap encryptor in Some()
        let encrypt_stream = stream::unfold(
            (content, Some(encryptor), false),
            |(mut reader, mut encryptor_opt, is_done)| async move {
                if is_done {
                    return None;
                }

                // Take the encryptor out of the option
                let mut encryptor = encryptor_opt.take().expect("Encryptor missing");

                let mut buf = vec![0u8; CHUNK_SIZE];
                let mut total_read = 0;

                while total_read < CHUNK_SIZE {
                    match reader.read(&mut buf[total_read..]).await {
                        Ok(0) => break,
                        Ok(n) => total_read += n,
                        Err(e) => return Some((Err(e), (reader, Some(encryptor), true))),
                    }
                }

                let chunk = &buf[..total_read];

                if total_read < CHUNK_SIZE {
                    let res = encryptor
                        .encrypt_last(chunk)
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()));

                    match res {
                        // encryptor is consumed, return None
                        Ok(ciphertext) => Some((Ok(Bytes::from(ciphertext)), (reader, None, true))),
                        Err(e) => Some((Err(e), (reader, None, true))),
                    }
                } else {
                    let res = encryptor
                        .encrypt_next(chunk)
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()));

                    match res {
                        Ok(ciphertext) => Some((
                            Ok(Bytes::from(ciphertext)),
                            (reader, Some(encryptor), false),
                        )),
                        Err(e) => Some((Err(e), (reader, Some(encryptor), true))),
                    }
                }
            },
        );

        let combined_stream = nonce_stream.chain(encrypt_stream);
        let pipe_reader = StreamReader::new(combined_stream);

        self.inner
            .put(path, Box::pin(pipe_reader), &ctx.inner)
            .await
    }

    async fn get(&self, path: &Path, ctx: &Self::Context) -> io::Result<Option<AsyncFileReader>> {
        let raw_opt = self.inner.get(path, &ctx.inner).await?;
        let mut reader = match raw_opt {
            Some(r) => r,
            None => return Ok(None),
        };

        let mut nonce = [0u8; NONCE_SIZE];
        reader.read_exact(&mut nonce).await?;

        let key = Key::from_slice(&ctx.key);
        let cipher = ChaCha20Poly1305::new(key);
        let decryptor = DecryptorBE32::from_aead(cipher, &nonce.into());

        // Wrap decryptor in Some()
        let decrypt_stream = stream::unfold(
            (reader, Some(decryptor), false),
            |(mut reader, mut decryptor_opt, is_done)| async move {
                if is_done {
                    return None;
                }

                // Take the decryptor out of the option
                let mut decryptor = decryptor_opt.take().expect("Decryptor missing");

                let mut buf = vec![0u8; CHUNK_SIZE + TAG_SIZE];
                let mut total_read = 0;

                while total_read < buf.len() {
                    match reader.read(&mut buf[total_read..]).await {
                        Ok(0) => break,
                        Ok(n) => total_read += n,
                        Err(e) => return Some((Err(e), (reader, Some(decryptor), true))),
                    }
                }

                if total_read == 0 {
                    return None;
                }

                let chunk = &buf[..total_read];

                if total_read == CHUNK_SIZE + TAG_SIZE {
                    match decryptor.decrypt_next(chunk) {
                        // decryptor is still valid, return Some(decryptor)
                        Ok(pt) => Some((Ok(Bytes::from(pt)), (reader, Some(decryptor), false))),
                        Err(_) => Some((
                            Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "Decryption failure",
                            )),
                            (reader, Some(decryptor), true),
                        )),
                    }
                } else {
                    match decryptor.decrypt_last(chunk) {
                        // decryptor is consumed, return None
                        Ok(pt) => Some((Ok(Bytes::from(pt)), (reader, None, true))),
                        Err(_) => Some((
                            Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "Final decryption failure",
                            )),
                            (reader, None, true),
                        )),
                    }
                }
            },
        );

        let decrypted_reader = StreamReader::new(decrypt_stream);
        Ok(Some(Box::pin(decrypted_reader)))
    }

    // --- Standard Delegation ---
    async fn delete(&self, path: &Path, ctx: &Self::Context) -> io::Result<()> {
        self.inner.delete(path, &ctx.inner).await
    }

    async fn metadata(&self, path: &Path, ctx: &Self::Context) -> io::Result<Option<FileMetadata>> {
        let mut meta_opt = self.inner.metadata(path, &ctx.inner).await?;

        if let Some(meta) = &mut meta_opt {
            // Overwrite the encrypted size with the mathematically corrected plaintext size
            meta.size = Self::plaintext_size(meta.size);
        }

        Ok(meta_opt)
    }
    async fn list(&self, path: &Path, ctx: &Self::Context) -> io::Result<Vec<Resource>> {
        let mut resources = self.inner.list(path, &ctx.inner).await?;

        for resource in &mut resources {
            // Pattern match to only adjust sizes for actual files
            if let Resource::File { metadata, .. } = resource {
                metadata.size = Self::plaintext_size(metadata.size);
            }
        }

        Ok(resources)
    }
}
