//! MinIO (S3) object streaming for file upload/download (issue #7).
//!
//! Both directions are bounded by a fixed buffer, never the file size
//! (CLAUDE.md NFR: "Uploads stream end-to-end … server memory per upload is
//! fixed at the chunk buffer size"):
//!   - **Upload** accumulates incoming chunks into ~8 MiB parts and flushes
//!     each via the S3 `UploadPart` API (`MULTIPART`), enforcing the byte
//!     ceiling as it goes and aborting the multipart upload if exceeded.
//!   - **Download** returns the `GetObject` `ByteStream` piped straight into
//!     the response body, honoring HTTP `Range` for tablet seeking.

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

/// A single chunk source for [`upload_streaming`]: yields the next chunk of
/// the body, `Ok(None)` at end of stream. Implemented for axum's
/// `Multipart` field in the route layer so this module stays HTTP-agnostic.
#[allow(async_fn_in_trait)]
pub trait ChunkSource {
    async fn next_chunk(&mut self) -> Result<Option<Bytes>, StorageError>;
}

/// Stream `source` to `bucket/key` via S3 multipart upload, enforcing
/// `max_bytes`. Returns the total number of bytes written. On any error
/// (including exceeding `max_bytes`) the multipart upload is aborted so no
/// partial object is left behind.
pub async fn upload_streaming<S: ChunkSource>(
    s3: &S3Client,
    bucket: &str,
    key: &str,
    content_type: &str,
    max_bytes: u64,
    mut source: S,
) -> Result<u64, StorageError> {
    let create = s3
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .content_type(content_type)
        .send()
        .await
        .map_err(|e| StorageError::S3(format!("create_multipart_upload: {e}")))?;

    let upload_id = create
        .upload_id()
        .ok_or_else(|| StorageError::S3("missing upload_id".into()))?
        .to_string();

    // Run the part loop; on any error, abort and propagate.
    match upload_parts(s3, bucket, key, &upload_id, max_bytes, &mut source).await {
        Ok(total) => Ok(total),
        Err(e) => {
            let _ = s3
                .abort_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .send()
                .await;
            Err(e)
        }
    }
}

async fn upload_parts<S: ChunkSource>(
    s3: &S3Client,
    bucket: &str,
    key: &str,
    upload_id: &str,
    max_bytes: u64,
    source: &mut S,
) -> Result<u64, StorageError> {
    let mut buf = BytesMut::with_capacity(PART_SIZE);
    let mut parts: Vec<CompletedPart> = Vec::new();
    let mut part_number: i32 = 1;
    let mut total: u64 = 0;

    while let Some(chunk) = source.next_chunk().await? {
        total += chunk.len() as u64;
        if total > max_bytes {
            return Err(StorageError::TooLarge { limit: max_bytes });
        }
        buf.extend_from_slice(&chunk);
        while buf.len() >= PART_SIZE {
            let part = buf.split_to(PART_SIZE).freeze();
            let completed = put_part(s3, bucket, key, upload_id, part_number, part).await?;
            parts.push(completed);
            part_number += 1;
        }
    }

    // Flush the trailing remainder as the final part. S3 forbids a
    // multipart upload with zero parts, so a totally empty body still needs
    // one (possibly empty) part.
    if !buf.is_empty() || parts.is_empty() {
        let part = buf.freeze();
        let completed = put_part(s3, bucket, key, upload_id, part_number, part).await?;
        parts.push(completed);
    }

    s3.complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(upload_id)
        .multipart_upload(
            CompletedMultipartUpload::builder()
                .set_parts(Some(parts))
                .build(),
        )
        .send()
        .await
        .map_err(|e| StorageError::S3(format!("complete_multipart_upload: {e}")))?;

    Ok(total)
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
/// Soft delete keeps the object per CLAUDE.md, so phase-1 #7 does not call
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
