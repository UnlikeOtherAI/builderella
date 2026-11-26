//! The object routes: content-addressed bytes in and out.
//!
//! `PUT /objects/{digest}` takes either the store's own encoding
//! (`application/vnd.builderella.object`: header plus one zstd frame) or raw
//! bytes (`application/octet-stream`). Either way the content is verified
//! against the digest before anything is published. `GET` answers with the
//! stored encoding, or with the decoded bytes for `?raw=1`.

use super::{ApiJson, ApiPath, ApiQuery, parse_id};
use crate::auth::{Reader, Writer};
use crate::error::{ApiResult, Failure};
use crate::state::App;
use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bella_core::api::{MissingObjectsRequest, MissingObjectsResponse};
use bella_core::store::{ObjectStore, StoreError};
use bella_core::{Digest, ErrorCode};
use bytes::Bytes;
use futures_util::StreamExt;
use serde::Deserialize;
use std::io::{self, Read};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::io::{ReaderStream, StreamReader, SyncIoBridge};

/// Media type of the store's encoding: a 16-byte header and one zstd frame.
pub(crate) const STORED_OBJECT: &str = "application/vnd.builderella.object";
const RAW_OBJECT: &str = "application/octet-stream";
/// Response header carrying an object's uncompressed length on `HEAD`.
pub(crate) const OBJECT_LENGTH_HEADER: &str = "x-bella-object-length";
/// Most digests one `POST /objects/missing` may ask about.
const MAX_MISSING_QUERY: usize = 250_000;
/// When an uploaded object already exists its body is not needed. A body up
/// to this size is still read and discarded, so that an HTTP/1.1 client
/// finishes sending before it reads the answer; a larger one is left unread
/// and the connection closes after the answer.
const DISCARD_LIMIT: u64 = 8 * 1024 * 1024;
const DISCARD_TIMEOUT: Duration = Duration::from_secs(10);
const CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Stored,
    Raw,
}

fn media_type(headers: &HeaderMap) -> ApiResult<Option<String>> {
    let Some(value) = headers.get(CONTENT_TYPE) else {
        return Ok(None);
    };
    let text = value
        .to_str()
        .map_err(|_| Failure::bad_request("the content-type header is not valid"))?;
    let essence = text.split(';').next().unwrap_or("").trim();
    Ok(Some(essence.to_ascii_lowercase()))
}

fn encoding(headers: &HeaderMap) -> ApiResult<Encoding> {
    match media_type(headers)?.as_deref() {
        Some(STORED_OBJECT) => Ok(Encoding::Stored),
        Some(RAW_OBJECT) | None => Ok(Encoding::Raw),
        Some(other) => Err(Failure::bad_request(format!(
            "unsupported content type `{other}`; send `{STORED_OBJECT}` or `{RAW_OBJECT}`"
        ))),
    }
}

fn declared_length(headers: &HeaderMap) -> Option<u64> {
    headers.get(CONTENT_LENGTH)?.to_str().ok()?.parse().ok()
}

/// What went wrong while storing an upload. Faults of the request (a body
/// that is malformed, cut short or stalled) are the client's; everything
/// else is the store's.
fn upload_failure(error: StoreError) -> Failure {
    match &error {
        StoreError::Io(io) => match io.kind() {
            io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => {
                Failure::bad_request(format!("the object is not well formed: {io}"))
            }
            io::ErrorKind::ConnectionAborted | io::ErrorKind::TimedOut => {
                Failure::bad_request(format!("the upload did not complete: {io}"))
            }
            _ => Failure::internal(error),
        },
        _ => error.into(),
    }
}

/// `PUT /objects/{digest}`: 204 once the object is stored (or already was),
/// 409 when the content does not match the digest.
pub(crate) async fn upload(
    State(app): State<App>,
    _: Writer,
    ApiPath(digest): ApiPath,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<StatusCode> {
    let digest: Digest = parse_id(&digest)?;
    let encoding = encoding(&headers)?;
    let declared = declared_length(&headers);
    let mut stream = body.into_data_stream();

    let known = digest.clone();
    if app
        .blocking(move |app| Ok(app.store.contains(&known)?))
        .await?
    {
        if declared.is_none_or(|length| length <= DISCARD_LIMIT) {
            let discard = async {
                let mut seen = 0u64;
                while let Some(Ok(chunk)) = stream.next().await {
                    seen += chunk.len() as u64;
                    if seen > DISCARD_LIMIT {
                        break;
                    }
                }
            };
            let _ = tokio::time::timeout(DISCARD_TIMEOUT, discard).await;
        }
        return Ok(StatusCode::NO_CONTENT);
    }

    if encoding == Encoding::Raw
        && declared.is_some_and(|length| length > app.tuning.max_object_bytes)
    {
        return Err(Failure::new(
            ErrorCode::TooLarge,
            format!(
                "object exceeds the {}-byte limit",
                app.tuning.max_object_bytes
            ),
        ));
    }

    let idle = app.tuning.upload_idle_timeout;
    let chunks = futures_util::stream::unfold(stream, move |mut stream| async move {
        let item = match tokio::time::timeout(idle, stream.next()).await {
            Ok(Some(Ok(chunk))) => Ok(chunk),
            Ok(Some(Err(error))) => Err(io::Error::new(io::ErrorKind::ConnectionAborted, error)),
            Ok(None) => return None,
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no data arrived for too long",
            )),
        };
        Some((item, stream))
    });
    let reader = StreamReader::new(Box::pin(chunks));
    app.blocking(move |app| {
        let mut reader = SyncIoBridge::new(reader);
        let stored = match encoding {
            Encoding::Stored => app.store.put_compressed(&mut reader, &digest).map(|_| ()),
            Encoding::Raw => app.store.put(&mut reader, Some(&digest)).map(|_| ()),
        };
        stored.map_err(upload_failure)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct DownloadQuery {
    /// `1` or `true`: answer with the decoded bytes.
    raw: Option<String>,
}

impl DownloadQuery {
    fn raw(&self) -> ApiResult<bool> {
        match self.raw.as_deref() {
            None | Some("0" | "false") => Ok(false),
            Some("1" | "true" | "") => Ok(true),
            Some(other) => Err(Failure::bad_request(format!(
                "`raw` is `1` or `0`, not `{other}`"
            ))),
        }
    }
}

/// Read until `buffer` is full or the reader ends.
fn fill(reader: &mut dyn Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Feed a verifying reader into a channel, always holding back the newest
/// chunk until the reader has produced something after it. The store's
/// reader reports a damaged object only at its end, so the final chunk is
/// released only once the content has been verified: a client can never
/// receive all of a corrupt object as if it were sound.
fn pump_verified(mut reader: Box<dyn Read + Send>, sender: mpsc::Sender<io::Result<Bytes>>) {
    let mut held: Option<Bytes> = None;
    let mut buffer = vec![0u8; CHUNK];
    loop {
        match fill(reader.as_mut(), &mut buffer) {
            Ok(0) => {
                if let Some(last) = held.take() {
                    let _ = sender.blocking_send(Ok(last));
                }
                return;
            }
            Ok(n) => {
                let chunk = Bytes::copy_from_slice(&buffer[..n]);
                if let Some(previous) = held.replace(chunk)
                    && sender.blocking_send(Ok(previous)).is_err()
                {
                    return;
                }
            }
            Err(error) => {
                let _ = sender.blocking_send(Err(error));
                return;
            }
        }
    }
}

/// `GET /objects/{digest}`.
pub(crate) async fn download(
    State(app): State<App>,
    _: Reader,
    ApiPath(digest): ApiPath,
    ApiQuery(query): ApiQuery<DownloadQuery>,
) -> ApiResult<Response> {
    let digest: Digest = parse_id(&digest)?;
    if query.raw()? {
        let (reader, length) = app
            .blocking(move |app| {
                let length = app
                    .store
                    .size(&digest)?
                    .ok_or_else(|| StoreError::NotFound(digest.clone()))?;
                Ok((app.store.open(&digest)?, length))
            })
            .await?;
        let (sender, mut receiver) = mpsc::channel(4);
        tokio::task::spawn_blocking(move || pump_verified(reader, sender));
        let body = Body::from_stream(futures_util::stream::poll_fn(move |cx| {
            receiver.poll_recv(cx)
        }));
        return Ok(object_response(RAW_OBJECT, length, body));
    }
    let (file, length) = app
        .blocking(move |app| Ok(app.store.open_compressed(&digest)?))
        .await?;
    let body = Body::from_stream(ReaderStream::with_capacity(
        tokio::fs::File::from_std(file),
        CHUNK,
    ));
    Ok(object_response(STORED_OBJECT, length, body))
}

fn object_response(media_type: &'static str, length: u64, body: Body) -> Response {
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(media_type));
    headers.insert(CONTENT_LENGTH, HeaderValue::from(length));
    response
}

/// `HEAD /objects/{digest}`: 200 with the uncompressed length in
/// `x-bella-object-length`, or 404.
pub(crate) async fn head(
    State(app): State<App>,
    _: Reader,
    ApiPath(digest): ApiPath,
) -> ApiResult<Response> {
    let digest: Digest = parse_id(&digest)?;
    let wanted = digest.clone();
    let length = app
        .blocking(move |app| Ok(app.store.size(&wanted)?))
        .await?
        .ok_or_else(|| Failure::not_found(format!("object {digest}")))?;
    let mut response = StatusCode::OK.into_response();
    response
        .headers_mut()
        .insert(OBJECT_LENGTH_HEADER, HeaderValue::from(length));
    Ok(response)
}

/// `POST /objects/missing`: which of these digests still need uploading.
pub(crate) async fn missing(
    State(app): State<App>,
    _: Reader,
    ApiJson(request): ApiJson<MissingObjectsRequest>,
) -> ApiResult<Json<MissingObjectsResponse>> {
    if request.digests.len() > MAX_MISSING_QUERY {
        return Err(Failure::new(
            ErrorCode::TooLarge,
            format!("ask about at most {MAX_MISSING_QUERY} digests at a time"),
        ));
    }
    let missing = app
        .blocking(move |app| crate::sweeps::missing_objects(app.store.as_ref(), request.digests))
        .await?;
    Ok(Json(MissingObjectsResponse { missing }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Yields its data, then fails the way a verifying reader does when the
    /// content does not match its digest.
    struct Corrupt {
        data: io::Cursor<Vec<u8>>,
    }

    impl Read for Corrupt {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            match self.data.read(buffer)? {
                0 => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "digest mismatch",
                )),
                n => Ok(n),
            }
        }
    }

    fn drain(reader: Box<dyn Read + Send>) -> (Vec<u8>, bool) {
        let (sender, mut receiver) = mpsc::channel(1024);
        pump_verified(reader, sender);
        let mut bytes = Vec::new();
        let mut failed = false;
        while let Ok(item) = receiver.try_recv() {
            match item {
                Ok(chunk) => bytes.extend_from_slice(&chunk),
                Err(_) => failed = true,
            }
        }
        (bytes, failed)
    }

    #[test]
    fn a36_a_corrupt_object_is_never_delivered_whole() {
        for length in [1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK] {
            let data = vec![7u8; length];
            let (sent, failed) = drain(Box::new(Corrupt {
                data: io::Cursor::new(data.clone()),
            }));
            assert!(failed, "length {length}");
            assert!(sent.len() < length, "length {length}: all bytes were sent");
        }
    }

    #[test]
    fn a_sound_object_is_delivered_completely() {
        for length in [0, 1, CHUNK, CHUNK + 1, 3 * CHUNK + 5] {
            let data: Vec<u8> = (0..length).map(|i| i as u8).collect();
            let (sent, failed) = drain(Box::new(io::Cursor::new(data.clone())));
            assert!(!failed);
            assert_eq!(sent, data, "length {length}");
        }
    }

    #[test]
    fn content_types_select_the_encoding() {
        let with = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static(value));
            headers
        };
        assert!(encoding(&HeaderMap::new()).unwrap() == Encoding::Raw);
        assert!(encoding(&with("application/octet-stream")).unwrap() == Encoding::Raw);
        assert!(encoding(&with("Application/Vnd.Builderella.Object")).unwrap() == Encoding::Stored);
        assert!(
            encoding(&with("application/vnd.builderella.object; q=1")).unwrap() == Encoding::Stored
        );
        assert!(encoding(&with("text/plain")).is_err());
    }
}
