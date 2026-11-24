use std::io;
use std::path::Path;

use actix_web::{Error, FromRequest, HttpRequest, HttpResponse, dev::Payload, error, http::header};
use actix_web_httpauth::extractors::basic::BasicAuth;
use async_trait::async_trait;
use chacha20poly1305::{
    ChaCha20Poly1305, Key, KeyInit,
    aead::stream::{DecryptorBE32, EncryptorBE32},
};
use futures::{StreamExt, stream};
use futures_util::future::LocalBoxFuture;
use pbkdf2::pbkdf2_hmac;
use sha2::Sha256;
use tokio::io::AsyncReadExt;
use tokio_util::{bytes::Bytes, io::StreamReader};

use super::{AsyncFileReader, FileMetadata, Resource, StorageProvider};

pub const CHUNK_SIZE: usize = 64 * 1024; // 64KB chunks
pub const TAG_SIZE: usize = 16; // Poly1305 tag
pub const NONCE_SIZE: usize = 7; // ChaCha20 stream nonce

/// Returns the encrypted size of a plaintext length.
fn encrypted_size(plaintext_len: u64) -> u64 {
    let chunks = plaintext_len / CHUNK_SIZE as u64 + 1;
    NONCE_SIZE as u64 + plaintext_len + TAG_SIZE as u64 * chunks
}

#[derive(Clone, Default)]
pub struct EncryptedContext<C> {
    pub inner: C,
    pub password: String,
}

impl<C> EncryptedContext<C> {
    /// Creates a new context storing the plaintext password.
    pub fn new(inner: C, password: impl Into<String>) -> Self {
        Self {
            inner,
            password: password.into(),
        }
    }
}

impl<C> FromRequest for EncryptedContext<C>
where
    C: FromRequest + 'static,
    C::Error: Into<Error>,
{
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self, Self::Error>>;

    fn from_request(req: &HttpRequest, payload: &mut Payload) -> Self::Future {
        // Clone the request for the async block
        let req_clone = req.clone();

        // Capture the path to create a unique Auth Realm for this specific URL
        let path = req.path().to_string();

        // 1. Trigger the extraction of `C` *outside* the async block
        // This avoids borrowing issues with `&mut Payload` inside the static future.
        let c_future = C::from_request(req, payload);

        Box::pin(async move {
            // 2. Await the previous extractor (`C`) first
            let c_instance = c_future.await.map_err(|e| e.into())?; // Convert C's error into a standard Actix error

            // 3. Extract Basic Auth as normal
            let auth = match BasicAuth::extract(&req_clone).await {
                Ok(auth) => auth,
                Err(_) => {
                    let realm = format!("Basic realm=\"Access to {}\"", path);

                    return Err(error::InternalError::from_response(
                        "Unauthorized",
                        HttpResponse::Unauthorized()
                            .insert_header((header::WWW_AUTHENTICATE, realm))
                            .insert_header((
                                header::CACHE_CONTROL,
                                "no-store, no-cache, must-revalidate",
                            ))
                            .insert_header((header::PRAGMA, "no-cache"))
                            .finish(),
                    )
                    .into());
                }
            };

            let password = auth.password().unwrap_or("").to_string();

            // 6. Inject the extracted `C` instance instead of C::default()
            let ctx = EncryptedContext::new(c_instance, password);

            Ok(ctx)
        })
    }
}

pub struct EncryptedStorage<S: StorageProvider> {
    inner: S,
    salt: String,
}

impl<S: StorageProvider> EncryptedStorage<S> {
    pub fn new(inner: S, salt: String) -> Self {
        Self { inner, salt }
    }

    fn derive_key(&self, password: &str) -> io::Result<[u8; 32]> {
        let mut key = [0u8; 32];
        let salt = self.salt.as_bytes();

        // 600,000 iterations is the OWASP recommendation for SHA-256
        // This is CPU-intensive but uses virtually NO extra RAM.
        pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, 600_000, &mut key);

        Ok(key)
    }

    /// Builds the encrypted reader for one upload.
    fn encrypted_reader(
        &self,
        content: AsyncFileReader,
        password: &str,
    ) -> io::Result<AsyncFileReader> {
        let derived_key = self.derive_key(password)?;

        let mut nonce = [0u8; NONCE_SIZE];
        rand::fill(&mut nonce);

        let key = Key::from_slice(&derived_key);
        let cipher = ChaCha20Poly1305::new(key);
        let encryptor = EncryptorBE32::from_aead(cipher, &nonce.into());
        let nonce_stream = stream::once(async move { Ok(Bytes::from(nonce.to_vec())) });

        let buffer = vec![0u8; CHUNK_SIZE];

        // Wrap encryptor in Some()
        let encrypt_stream = stream::unfold(
            (content, Some(encryptor), buffer, false),
            |(mut reader, mut encryptor_opt, mut buf, is_done)| async move {
                if is_done {
                    return None;
                }

                // Take the encryptor out of the option
                let mut encryptor = encryptor_opt.take().expect("Encryptor missing");

                let mut total_read = 0;

                while total_read < CHUNK_SIZE {
                    match reader.read(&mut buf[total_read..]).await {
                        Ok(0) => break,
                        Ok(n) => total_read += n,
                        Err(e) => return Some((Err(e), (reader, Some(encryptor), buf, true))),
                    }
                }

                let chunk = &buf[..total_read];

                if total_read < CHUNK_SIZE {
                    let res = encryptor
                        .encrypt_last(chunk)
                        .map_err(|e| io::Error::other(e.to_string()));

                    match res {
                        // encryptor is consumed, return None
                        Ok(ciphertext) => {
                            Some((Ok(Bytes::from(ciphertext)), (reader, None, buf, true)))
                        }
                        Err(e) => Some((Err(e), (reader, None, buf, true))),
                    }
                } else {
                    let res = encryptor
                        .encrypt_next(chunk)
                        .map_err(|e| io::Error::other(e.to_string()));

                    match res {
                        Ok(ciphertext) => Some((
                            Ok(Bytes::from(ciphertext)),
                            (reader, Some(encryptor), buf, false),
                        )),
                        Err(e) => Some((Err(e), (reader, Some(encryptor), buf, true))),
                    }
                }
            },
        );

        let combined_stream = nonce_stream.chain(encrypt_stream);
        Ok(Box::pin(StreamReader::new(combined_stream)))
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
        let reader = self.encrypted_reader(content, &ctx.password)?;
        self.inner.put(path, reader, &ctx.inner).await
    }

    async fn put_sized(
        &self,
        path: &Path,
        content: AsyncFileReader,
        length: u64,
        ctx: &Self::Context,
    ) -> io::Result<u64> {
        let reader = self.encrypted_reader(content, &ctx.password)?;
        let encrypted_length = encrypted_size(length);
        self.inner
            .put_sized(path, reader, encrypted_length, &ctx.inner)
            .await
    }

    async fn get(&self, path: &Path, ctx: &Self::Context) -> io::Result<Option<AsyncFileReader>> {
        let raw_opt = self.inner.get(path, &ctx.inner).await?;
        let mut reader = match raw_opt {
            Some(r) => r,
            None => return Ok(None),
        };

        // Derive key on the fly
        let derived_key = self.derive_key(&ctx.password)?;

        let mut nonce = [0u8; NONCE_SIZE];
        reader.read_exact(&mut nonce).await?;

        let key = Key::from_slice(&derived_key);
        let cipher = ChaCha20Poly1305::new(key);
        let decryptor = DecryptorBE32::from_aead(cipher, &nonce.into());

        let buffer = vec![0u8; CHUNK_SIZE + TAG_SIZE];

        // Wrap decryptor in Some()
        let decrypt_stream = stream::unfold(
            (reader, Some(decryptor), buffer, false),
            |(mut reader, mut decryptor_opt, mut buf, is_done)| async move {
                if is_done {
                    return None;
                }

                // Take the decryptor out of the option
                let mut decryptor = decryptor_opt.take().expect("Decryptor missing");

                let mut total_read = 0;
                while total_read < buf.len() {
                    match reader.read(&mut buf[total_read..]).await {
                        Ok(0) => break,
                        Ok(n) => total_read += n,
                        Err(e) => return Some((Err(e), (reader, Some(decryptor), buf, true))),
                    }
                }
                if total_read == 0 {
                    return None;
                }

                let chunk = &buf[..total_read];

                if total_read == CHUNK_SIZE + TAG_SIZE {
                    match decryptor.decrypt_next(chunk) {
                        // decryptor is still valid, return Some(decryptor)
                        Ok(pt) => {
                            Some((Ok(Bytes::from(pt)), (reader, Some(decryptor), buf, false)))
                        }
                        Err(_) => Some((
                            Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "Decryption failure",
                            )),
                            (reader, Some(decryptor), buf, true),
                        )),
                    }
                } else {
                    match decryptor.decrypt_last(chunk) {
                        // decryptor is consumed, return None
                        Ok(pt) => Some((Ok(Bytes::from(pt)), (reader, None, buf, true))),
                        Err(_) => Some((
                            Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "Final decryption failure",
                            )),
                            (reader, None, buf, true),
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

    async fn delete_partial(&self, path: &Path, ctx: &Self::Context) -> io::Result<()> {
        self.inner.delete_partial(path, &ctx.inner).await
    }

    async fn cleanup_stale_multipart_uploads(&self, ctx: &Self::Context) -> io::Result<u64> {
        self.inner.cleanup_stale_multipart_uploads(&ctx.inner).await
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

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use tokio::io::AsyncReadExt;

    use super::*;
    use crate::storage::in_memory::{InMemoryContext, InMemoryStorage};
    use crate::storage::test_support::RecordingStorage;

    #[test]
    fn encrypted_size_matches_the_stream_at_chunk_boundaries() {
        let chunk = CHUNK_SIZE as u64;
        let tag = TAG_SIZE as u64;
        let nonce = NONCE_SIZE as u64;
        let cases = [
            (0, nonce + tag),
            (1, nonce + 1 + tag),
            (chunk - 1, nonce + chunk - 1 + tag),
            (chunk, nonce + chunk + 2 * tag),
            (chunk + 1, nonce + chunk + 1 + 2 * tag),
            (2 * chunk, nonce + 2 * chunk + 3 * tag),
        ];

        for (plaintext, expected) in cases {
            assert_eq!(encrypted_size(plaintext), expected, "plaintext {plaintext}");
            assert_eq!(
                EncryptedStorage::<InMemoryStorage>::plaintext_size(expected),
                plaintext,
                "plaintext {plaintext}"
            );
        }
    }

    #[tokio::test]
    async fn encrypted_stream_length_matches_the_formula() {
        for length in [
            0,
            1,
            CHUNK_SIZE - 1,
            CHUNK_SIZE,
            CHUNK_SIZE + 1,
            2 * CHUNK_SIZE,
        ] {
            let storage = EncryptedStorage::new(InMemoryStorage::new(), "salt".to_owned());
            let ctx = EncryptedContext::<InMemoryContext>::default();

            storage
                .put_sized(
                    Path::new("file"),
                    Box::pin(Cursor::new(vec![0_u8; length])),
                    length as u64,
                    &ctx,
                )
                .await
                .expect("put");

            let mut reader = storage
                .inner
                .get(Path::new("file"), &ctx.inner)
                .await
                .expect("get")
                .expect("stored object");
            let mut ciphertext = Vec::new();
            reader.read_to_end(&mut ciphertext).await.expect("read");
            assert_eq!(ciphertext.len() as u64, encrypted_size(length as u64));
        }
    }

    #[tokio::test]
    async fn put_sized_passes_the_encrypted_length_to_the_inner_storage() {
        let storage = EncryptedStorage::new(RecordingStorage::new(), "salt".to_owned());
        let ctx = EncryptedContext::<InMemoryContext>::default();
        let length = 1024_u64;

        storage
            .put_sized(
                Path::new("file"),
                Box::pin(Cursor::new(vec![0_u8; length as usize])),
                length,
                &ctx,
            )
            .await
            .expect("put");

        assert_eq!(storage.inner.lengths(), vec![encrypted_size(length)]);
    }
}
