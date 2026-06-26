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
