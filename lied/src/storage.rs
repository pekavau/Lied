//! MinIO (S3) object streaming for file upload/download (issue #7).
//!
//! Both directions are bounded by a fixed buffer, never the file size
//! (docs/design.md NFR: "Uploads stream end-to-end … server memory per upload is
//! fixed at the chunk buffer size"), for `/v1`, the console and WebDAV alike:
//!   - **Upload** goes through an [`ObjectWriter`]: incoming bytes fill one
//!     ~8 MiB part at a time, each uploaded via `UploadPart` as it fills; the
//!     byte ceiling is enforced as it goes and a started multipart upload is
//!     aborted on any error. A file below one part is one `PutObject`.
//!   - **Download** returns the `GetObject` `ByteStream` piped straight into
//!     the response body, honoring HTTP `Range` for tablet seeking.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::Client as S3Client;
use bytes::{Bytes, BytesMut};

/// S3 requires every part except the last to be at least 5 MiB. We flush at
/// 8 MiB so the buffered-in-memory ceiling per upload is ~8 MiB regardless of
/// file size.
const PART_SIZE: usize = 8 * 1024 * 1024;

#[derive(thiserror::Error, Debug)]
pub enum StorageError {
    #[error("upload exceeds the maximum allowed size of {limit} bytes")]
    TooLarge { limit: u64 },
    #[error("error reading the upload stream")]
    Upstream,
    #[error("object not found")]
    NotFound,
    #[error("invalid Range header")]
    InvalidRange,
    #[error("S3 error: {0}")]
    S3(String),
}

/// What [`ensure_bucket`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketStatus {
    Existed,
    Created,
}

/// Make sure `bucket` exists, creating it only when `HeadBucket` says it is
/// missing (issue #91) — so credentials that may not create buckets still
/// work against one the operator made. A concurrent start that creates it
/// first is not an error.
pub async fn ensure_bucket(s3: &S3Client, bucket: &str) -> Result<BucketStatus, StorageError> {
    match s3.head_bucket().bucket(bucket).send().await {
        Ok(_) => return Ok(BucketStatus::Existed),
        Err(e) => {
            let svc = e.into_service_error();
            if !svc.is_not_found() {
                return Err(StorageError::S3(format!("head_bucket: {svc}")));
            }
        }
    }
    match s3.create_bucket().bucket(bucket).send().await {
        Ok(_) => Ok(BucketStatus::Created),
        Err(e) => {
            let svc = e.into_service_error();
            if svc.is_bucket_already_owned_by_you() {
                Ok(BucketStatus::Existed)
            } else {
                Err(StorageError::S3(format!("create_bucket: {svc}")))
            }
        }
    }
}

/// The id of the lifecycle rule Lied sets on its bucket.
pub const VERSION_RETENTION_RULE_ID: &str = "lied-noncurrent-version-expiry";

/// Turn bucket versioning on and set how long old versions are kept (#59).
///
/// With versioning on, overwriting an object (a replaced part, an app
/// saving over an annotation) or deleting one keeps the previous content as
/// a noncurrent version. A lifecycle rule expires noncurrent versions
/// `retention_days` after they were superseded, and removes delete markers
/// that no longer hide anything; `retention_days == 0` keeps them forever, so
/// any lifecycle configuration is removed instead.
///
/// **Lied owns the bucket's lifecycle configuration:** the S3 API replaces
/// the whole configuration at once, so a rule an operator added by hand is
/// overwritten on the next start. Idempotent; runs on every start.
pub async fn ensure_versioning(
    s3: &S3Client,
    bucket: &str,
    retention_days: u32,
) -> Result<(), StorageError> {
    use aws_sdk_s3::types::{
        BucketLifecycleConfiguration, BucketVersioningStatus, ExpirationStatus,
        LifecycleExpiration, LifecycleRule, LifecycleRuleFilter, NoncurrentVersionExpiration,
        VersioningConfiguration,
    };

    s3.put_bucket_versioning()
        .bucket(bucket)
        .versioning_configuration(
            VersioningConfiguration::builder()
                .status(BucketVersioningStatus::Enabled)
                .build(),
        )
        .send()
        .await
        .map_err(|e| {
            StorageError::S3(format!("put_bucket_versioning: {}", e.into_service_error()))
        })?;

    if retention_days == 0 {
        // Keep every version: no rule may expire them.
        s3.delete_bucket_lifecycle()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| {
                StorageError::S3(format!(
                    "delete_bucket_lifecycle: {}",
                    e.into_service_error()
                ))
            })?;
        return Ok(());
    }

    let days = i32::try_from(retention_days).unwrap_or(i32::MAX);
    let rule = LifecycleRule::builder()
        .id(VERSION_RETENTION_RULE_ID)
        .status(ExpirationStatus::Enabled)
        .filter(LifecycleRuleFilter::builder().prefix("").build())
        .noncurrent_version_expiration(
            NoncurrentVersionExpiration::builder()
                .noncurrent_days(days)
                .build(),
        )
        .expiration(
            LifecycleExpiration::builder()
                .expired_object_delete_marker(true)
                .build(),
        )
        .build()
        .map_err(|e| StorageError::S3(format!("lifecycle rule: {e}")))?;
    let configuration = BucketLifecycleConfiguration::builder()
        .rules(rule)
        .build()
        .map_err(|e| StorageError::S3(format!("lifecycle configuration: {e}")))?;
    s3.put_bucket_lifecycle_configuration()
        .bucket(bucket)
        .lifecycle_configuration(configuration)
        .send()
        .await
        .map_err(|e| {
            StorageError::S3(format!(
                "put_bucket_lifecycle_configuration: {}",
                e.into_service_error()
            ))
        })?;
    Ok(())
}

/// A single chunk source for [`upload_streaming`]: yields the next chunk of
/// the body, `Ok(None)` at end of stream. Implemented for axum's
/// `Multipart` field in the route layer so this module stays HTTP-agnostic.
#[allow(async_fn_in_trait)]
pub trait ChunkSource {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>, StorageError>;
}

/// Stream `source` to `bucket/key`, enforcing `max_bytes`, through an
/// [`ObjectWriter`]. Returns the total number of bytes written. On any error
/// (including exceeding `max_bytes`) a started multipart upload is aborted so
/// no partial object is left behind.
pub async fn upload_streaming<S: ChunkSource>(
    s3: &S3Client,
    bucket: &str,
    key: &str,
    content_type: &str,
    max_bytes: u64,
    mut source: S,
) -> Result<u64, StorageError> {
    let mut writer = ObjectWriter::new(s3, bucket, key, content_type, max_bytes);
    loop {
        match source.next_chunk().await {
            Ok(Some(chunk)) => writer.write(chunk).await?,
            Ok(None) => break,
            Err(e) => {
                writer.abort().await;
                return Err(e);
            }
        }
    }
    writer.finish().await
}

/// The largest part buffer any [`ObjectWriter`] in this process has held, in
/// bytes: a high-water mark, so a test can check that uploads stay bounded by
/// [`PART_SIZE`] whatever the file size (issue #26).
pub fn peak_upload_buffer() -> usize {
    PEAK_BUFFERED.load(Ordering::Relaxed)
}

static PEAK_BUFFERED: AtomicUsize = AtomicUsize::new(0);

/// The size of one multipart part, and so the most an upload holds in memory.
pub const UPLOAD_PART_SIZE: usize = PART_SIZE;

/// An object being written in pieces, holding at most one part
/// ([`PART_SIZE`]) in memory (docs/design.md NFR; issue #26).
///
/// - [`write`](Self::write) appends bytes and uploads every part as it fills.
///   The S3 multipart upload starts with the first full part, so a file
///   smaller than one part never starts one.
/// - [`finish`](Self::finish) makes the object visible: a single `PutObject`
///   for a small file, otherwise the last part plus `CompleteMultipartUpload`.
///   Until then nothing is readable at `key`, which is what lets a caller
///   write its database row first and roll it back if `finish` fails.
/// - `max_bytes` is enforced while writing (→ [`StorageError::TooLarge`]).
/// - Any error aborts a started multipart upload, and so does
///   [`abort`](Self::abort). A writer dropped without either (a client that
///   disconnects mid-body) aborts it in the background.
pub struct ObjectWriter {
    s3: S3Client,
    bucket: String,
    key: String,
    content_type: String,
    max_bytes: u64,
    buf: BytesMut,
    total: u64,
    /// The started multipart upload: its id and the parts uploaded so far.
    multipart: Option<(String, Vec<CompletedPart>)>,
}

impl std::fmt::Debug for ObjectWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectWriter")
            .field("key", &self.key)
            .field("total", &self.total)
            .field("buffered", &self.buf.len())
            .field("multipart", &self.multipart.is_some())
            .finish()
    }
}

impl ObjectWriter {
    pub fn new(s3: &S3Client, bucket: &str, key: &str, content_type: &str, max_bytes: u64) -> Self {
        ObjectWriter {
            s3: s3.clone(),
            bucket: bucket.to_string(),
            key: key.to_string(),
            content_type: content_type.to_string(),
            max_bytes,
            buf: BytesMut::new(),
            total: 0,
            multipart: None,
        }
    }

    /// Bytes written so far.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// The object key being written.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Append `data`, uploading each part as it fills. The buffer never
    /// grows past one part: `data` is copied in at most the room left.
    pub async fn write(&mut self, mut data: Bytes) -> Result<(), StorageError> {
        self.total += data.len() as u64;
        if self.total > self.max_bytes {
            let limit = self.max_bytes;
            self.abort_started().await;
            return Err(StorageError::TooLarge { limit });
        }
        while !data.is_empty() {
            let room = PART_SIZE - self.buf.len();
            let take = room.min(data.len());
            self.buf.extend_from_slice(&data.split_to(take));
            PEAK_BUFFERED.fetch_max(self.buf.len(), Ordering::Relaxed);
            if self.buf.len() == PART_SIZE {
                let part = std::mem::replace(&mut self.buf, BytesMut::new()).freeze();
                if let Err(e) = self.upload_part(part).await {
                    self.abort_started().await;
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Make the object visible at its key. Returns the bytes written.
    pub async fn finish(mut self) -> Result<u64, StorageError> {
        let last = std::mem::take(&mut self.buf).freeze();
        if self.multipart.is_none() {
            self.s3
                .put_object()
                .bucket(&self.bucket)
                .key(&self.key)
                .content_type(&self.content_type)
                .body(ByteStream::from(last))
                .send()
                .await
                .map_err(|e| StorageError::S3(format!("put_object: {e}")))?;
            return Ok(self.total);
        }
        // A multipart upload's last part may be empty only if it is the only
        // one; ours has at least one full part already, so skip an empty tail.
        if !last.is_empty() {
            if let Err(e) = self.upload_part(last).await {
                self.abort_started().await;
                return Err(e);
            }
        }
        let Some((upload_id, parts)) = self.multipart.take() else {
            return Err(StorageError::S3("multipart upload vanished".into()));
        };
        let completed = self
            .s3
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(&self.key)
            .upload_id(&upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build(),
            )
            .send()
            .await;
        if let Err(e) = completed {
            abort_upload(&self.s3, &self.bucket, &self.key, &upload_id).await;
            return Err(StorageError::S3(format!("complete_multipart_upload: {e}")));
        }
        Ok(self.total)
    }

    /// Give up: abort a started multipart upload. Nothing becomes visible.
    pub async fn abort(mut self) {
        self.abort_started().await;
    }

    async fn upload_part(&mut self, body: Bytes) -> Result<(), StorageError> {
        if self.multipart.is_none() {
            let created = self
                .s3
                .create_multipart_upload()
                .bucket(&self.bucket)
                .key(&self.key)
                .content_type(&self.content_type)
                .send()
                .await
                .map_err(|e| StorageError::S3(format!("create_multipart_upload: {e}")))?;
            let upload_id = created
                .upload_id()
                .ok_or_else(|| StorageError::S3("missing upload_id".into()))?
                .to_string();
            self.multipart = Some((upload_id, Vec::new()));
        }
        let Some((upload_id, parts)) = self.multipart.as_mut() else {
            return Err(StorageError::S3("multipart upload vanished".into()));
        };
        let part_number = i32::try_from(parts.len() + 1).unwrap_or(i32::MAX);
        let completed = put_part(
            &self.s3,
            &self.bucket,
            &self.key,
            upload_id,
            part_number,
            body,
        )
        .await?;
        parts.push(completed);
        Ok(())
    }

    async fn abort_started(&mut self) {
        self.buf = BytesMut::new();
        if let Some((upload_id, _)) = self.multipart.take() {
            abort_upload(&self.s3, &self.bucket, &self.key, &upload_id).await;
        }
    }
}

impl Drop for ObjectWriter {
    /// A writer dropped mid-upload (the client went away before the body
    /// ended) must not leave a multipart upload behind: abort it on the
    /// runtime, since `Drop` cannot await.
    fn drop(&mut self) {
        if let Some((upload_id, _)) = self.multipart.take() {
            let (s3, bucket, key) = (self.s3.clone(), self.bucket.clone(), self.key.clone());
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move { abort_upload(&s3, &bucket, &key, &upload_id).await });
            } else {
                tracing::warn!(key = %key, "multipart upload left open: no runtime to abort it");
            }
        }
    }
}

/// Abort a multipart upload, logging (not returning) a failure: the caller is
/// already on an error path, and an unaborted upload only costs storage until
/// the bucket's lifecycle rule (or an operator) clears it.
async fn abort_upload(s3: &S3Client, bucket: &str, key: &str, upload_id: &str) {
    if let Err(error) = s3
        .abort_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(upload_id)
        .send()
        .await
    {
        tracing::warn!(%error, key = %key, "failed to abort a multipart upload");
    }
}

async fn put_part(
    s3: &S3Client,
    bucket: &str,
    key: &str,
    upload_id: &str,
    part_number: i32,
    body: Bytes,
) -> Result<CompletedPart, StorageError> {
    let out = s3
        .upload_part()
        .bucket(bucket)
        .key(key)
        .upload_id(upload_id)
        .part_number(part_number)
        .body(ByteStream::from(body))
        .send()
        .await
        .map_err(|e| StorageError::S3(format!("upload_part {part_number}: {e}")))?;

    Ok(CompletedPart::builder()
        .set_e_tag(out.e_tag().map(str::to_string))
        .part_number(part_number)
        .build())
}

/// The result of a (possibly ranged) download: a streaming body plus the
/// response metadata the handler needs to set headers.
pub struct ObjectStream {
    pub body: ByteStream,
    /// Bytes in *this* response (the ranged length when a Range was honored).
    pub content_length: Option<i64>,
    /// `Content-Range` value when the request was ranged (→ 206).
    pub content_range: Option<String>,
    pub content_type: Option<String>,
}

/// Fetch `bucket/key`, optionally honoring an HTTP `Range` header value
/// (e.g. `bytes=0-1023`). The returned [`ObjectStream::body`] streams without
/// buffering the whole object.
pub async fn get_object(
    s3: &S3Client,
    bucket: &str,
    key: &str,
    range: Option<&str>,
) -> Result<ObjectStream, StorageError> {
    let mut req = s3.get_object().bucket(bucket).key(key);
    if let Some(r) = range {
        req = req.range(r);
    }

    let out = req.send().await.map_err(|e| {
        use aws_sdk_s3::error::ProvideErrorMetadata;
        // `GetObjectError` doesn't model 416; S3/MinIO surface it as an
        // unhandled error whose code is "InvalidRange". Capture the code
        // before consuming the error into the service variant.
        let code = e.code().map(str::to_string);
        let svc = e.into_service_error();
        if svc.is_no_such_key() {
            StorageError::NotFound
        } else if code.as_deref() == Some("InvalidRange") {
            StorageError::InvalidRange
        } else {
            StorageError::S3(format!("get_object: {svc}"))
        }
    })?;

    Ok(ObjectStream {
        content_range: out.content_range().map(str::to_string),
        content_type: out.content_type().map(str::to_string),
        content_length: out.content_length(),
        body: out.body,
    })
}

/// Metadata about a stored object, from a `HeadObject`.
pub struct ObjectHead {
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// `HeadObject` for size + last-modified, used to fill WebDAV
/// `getcontentlength`/`getlastmodified` without downloading the body.
pub async fn head_object(
    s3: &S3Client,
    bucket: &str,
    key: &str,
) -> Result<ObjectHead, StorageError> {
    let out = s3
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|e| {
            let svc = e.into_service_error();
            if svc.is_not_found() {
                StorageError::NotFound
            } else {
                StorageError::S3(format!("head_object: {svc}"))
            }
        })?;
    Ok(ObjectHead {
        size: out.content_length().unwrap_or(0).max(0) as u64,
        modified: out.last_modified().and_then(|d| {
            let secs = d.secs();
            if secs < 0 {
                None
            } else {
                Some(UNIX_EPOCH + Duration::new(secs as u64, d.subsec_nanos()))
            }
        }),
    })
}

/// A single object in a directory listing.
pub struct ListedObject {
    /// Full object key.
    pub key: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// One level of a "directory" listing under `prefix` (which must end in `/`,
/// or be empty): immediate child objects plus the immediate sub-directory
/// prefixes (via the `/` delimiter). Used for the files-by-convention trees
/// (personal annotations, user libraries) that have no DB rows.
pub struct DirListing {
    pub objects: Vec<ListedObject>,
    /// Immediate child directory prefixes (each ends in `/`).
    pub dirs: Vec<String>,
}

/// List one level under `prefix` using the `/` delimiter. Non-recursive.
pub async fn list_objects(
    s3: &S3Client,
    bucket: &str,
    prefix: &str,
) -> Result<DirListing, StorageError> {
    let mut objects = Vec::new();
    let mut dirs = Vec::new();
    let mut continuation: Option<String> = None;

    loop {
        let mut req = s3
            .list_objects_v2()
            .bucket(bucket)
            .prefix(prefix)
            .delimiter("/");
        if let Some(token) = &continuation {
            req = req.continuation_token(token);
        }
        let out = req
            .send()
            .await
            .map_err(|e| StorageError::S3(format!("list_objects_v2: {e}")))?;

        for cp in out.common_prefixes() {
            if let Some(p) = cp.prefix() {
                dirs.push(p.to_string());
            }
        }
        for obj in out.contents() {
            if let Some(key) = obj.key() {
                // S3 has no real directories; a zero-length key equal to the
                // prefix (a "directory marker") is not a real file — skip it.
                if key == prefix {
                    continue;
                }
                objects.push(ListedObject {
                    key: key.to_string(),
                    size: obj.size().unwrap_or(0).max(0) as u64,
                    modified: obj.last_modified().and_then(|d| {
                        let secs = d.secs();
                        (secs >= 0)
                            .then(|| UNIX_EPOCH + Duration::new(secs as u64, d.subsec_nanos()))
                    }),
                });
            }
        }

        if out.is_truncated().unwrap_or(false) {
            continuation = out.next_continuation_token().map(str::to_string);
            if continuation.is_none() {
                break;
            }
        } else {
            break;
        }
    }

    Ok(DirListing { objects, dirs })
}

/// Upload an in-memory buffer to `bucket/key`, enforcing `max_bytes`. A
/// convenience over [`upload_streaming`] for the files-by-convention writes
/// (annotations, library) where the whole body is already buffered by the
/// WebDAV `DavFile` before flush.
pub async fn upload_bytes(
    s3: &S3Client,
    bucket: &str,
    key: &str,
    content_type: &str,
    max_bytes: u64,
    data: Bytes,
) -> Result<u64, StorageError> {
    struct Once(Option<Bytes>);
    impl ChunkSource for Once {
        async fn next_chunk(&mut self) -> Result<Option<Bytes>, StorageError> {
            Ok(self.0.take())
        }
    }
    upload_streaming(s3, bucket, key, content_type, max_bytes, Once(Some(data))).await
}

/// Best-effort delete of every version of an object (hard delete only).
/// Soft delete keeps the object per docs/design.md, so phase-1 #7 does not call
/// this; it exists for the admin hard-delete flow.
pub async fn delete_object(s3: &S3Client, bucket: &str, key: &str) -> Result<(), StorageError> {
    s3.delete_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|e| StorageError::S3(format!("delete_object: {e}")))?;
    Ok(())
}

/// Copy `from` to `to` inside `bucket` (server side; no bytes pass through
/// Lied). Used by WebDAV MOVE and COPY in the private library (#26).
pub async fn copy_object(
    s3: &S3Client,
    bucket: &str,
    from: &str,
    to: &str,
) -> Result<(), StorageError> {
    s3.copy_object()
        .bucket(bucket)
        .copy_source(format!("{bucket}/{}", encode_key(from)))
        .key(to)
        .send()
        .await
        .map_err(|e| {
            let svc = e.into_service_error();
            StorageError::S3(format!("copy_object: {svc}"))
        })?;
    Ok(())
}

/// Every object key under `prefix`, recursively (no delimiter), including a
/// directory marker equal to `prefix` itself.
pub async fn list_keys_recursive(
    s3: &S3Client,
    bucket: &str,
    prefix: &str,
) -> Result<Vec<String>, StorageError> {
    let mut keys = Vec::new();
    let mut continuation: Option<String> = None;
    loop {
        let mut req = s3.list_objects_v2().bucket(bucket).prefix(prefix);
        if let Some(token) = &continuation {
            req = req.continuation_token(token);
        }
        let out = req
            .send()
            .await
            .map_err(|e| StorageError::S3(format!("list_objects_v2: {e}")))?;
        keys.extend(
            out.contents()
                .iter()
                .filter_map(|o| o.key().map(str::to_string)),
        );
        continuation = out
            .is_truncated()
            .unwrap_or(false)
            .then(|| out.next_continuation_token().map(str::to_string))
            .flatten();
        if continuation.is_none() {
            break;
        }
    }
    Ok(keys)
}

/// Percent-encode an object key for `x-amz-copy-source`, keeping `/`.
fn encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for b in key.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::encode_key;

    #[test]
    fn copy_source_keys_are_percent_encoded_except_slashes() {
        assert_eq!(
            encode_key("users/anna/library/a b.pdf"),
            "users/anna/library/a%20b.pdf"
        );
        assert_eq!(
            encode_key("users/anna/library/Märsche/1.pdf"),
            "users/anna/library/M%C3%A4rsche/1.pdf"
        );
        assert_eq!(encode_key("a+b&c"), "a%2Bb%26c");
    }
}
