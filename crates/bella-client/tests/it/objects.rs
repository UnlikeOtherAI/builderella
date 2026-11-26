//! The HTTP object store against an in-memory object server.

use crate::support::{Server, TOKEN, fast_options};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{post, put};
use bella_client::{
    ClientOptions, HttpObjectStore, OBJECT_CONTENT_TYPE, OBJECT_LENGTH_HEADER, ObjectStoreOptions,
    RetryPolicy,
};
use bella_core::api::{MissingObjectsRequest, MissingObjectsResponse};
use bella_core::store::{ObjectStore, StoreError, get_bytes, put_bytes};
use bella_core::{ApiError, Digest, ErrorCode};
use bella_store::format;
use std::collections::HashMap;
use std::io::{self, Cursor, Read};
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;

/// Deterministic bytes that do not compress.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn encode(content: &[u8]) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    let mut reader = content;
    format::encode(&mut reader, &mut out, u64::MAX).unwrap();
    out.into_inner()
}

fn decode(stored: &[u8], digest: &Digest) -> io::Result<Vec<u8>> {
    let (mut reader, _) = format::decode(stored, digest)?;
    let mut out = Vec::new();
    reader.read_to_end(&mut out)?;
    Ok(out)
}

#[derive(Default)]
struct ObjectsState {
    /// Objects in the stored encoding, by digest.
    stored: HashMap<String, Vec<u8>>,
    missing_requests: Vec<usize>,
    puts: usize,
    gets: usize,
    heads: usize,
    raw_gets: usize,
    /// Answer this many uploads with 503 before accepting one.
    fail_puts: usize,
    /// Abort this many downloads halfway through the body.
    break_gets: usize,
    /// Leave the length header out of HEAD answers.
    omit_length: bool,
    unauthorized: usize,
    wrong_content_type: usize,
}

#[derive(Clone, Default)]
struct Objects {
    state: Arc<Mutex<ObjectsState>>,
}

impl Objects {
    fn router(&self) -> Router {
        Router::new()
            .route("/api/v1/objects/missing", post(missing))
            .route(
                "/api/v1/objects/{digest}",
                put(put_object).get(get_object).head(head_object),
            )
            .layer(DefaultBodyLimit::disable())
            .with_state(self.clone())
    }

    fn with<T>(&self, f: impl FnOnce(&mut ObjectsState) -> T) -> T {
        f(&mut self.state.lock().unwrap())
    }

    /// Puts bytes under a digest without any check.
    fn plant(&self, digest: &Digest, stored: Vec<u8>) {
        self.with(|state| state.stored.insert(digest.to_string(), stored));
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        let ok = headers.get("authorization").and_then(|v| v.to_str().ok())
            == Some(format!("Bearer {TOKEN}").as_str());
        if !ok {
            self.with(|state| state.unauthorized += 1);
        }
        ok
    }
}

fn api_error(code: ErrorCode, message: &str) -> Response {
    let status = StatusCode::from_u16(code.http_status()).unwrap();
    (status, axum::Json(ApiError::new(code, message))).into_response()
}

async fn missing(
    State(objects): State<Objects>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<MissingObjectsRequest>,
) -> Response {
    if !objects.authorized(&headers) {
        return api_error(ErrorCode::Unauthorized, "no token");
    }
    let missing = objects.with(|state| {
        state.missing_requests.push(request.digests.len());
        request
            .digests
            .iter()
            .filter(|digest| !state.stored.contains_key(digest.as_str()))
            .cloned()
            .collect()
    });
    axum::Json(MissingObjectsResponse { missing }).into_response()
}

async fn put_object(
    State(objects): State<Objects>,
    Path(digest): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !objects.authorized(&headers) {
        return api_error(ErrorCode::Unauthorized, "no token");
    }
    let fail = objects.with(|state| {
        state.puts += 1;
        if state.fail_puts > 0 {
            state.fail_puts -= 1;
            true
        } else {
            false
        }
    });
    if fail {
        return api_error(ErrorCode::Unavailable, "try again");
    }
    let content_type = headers.get("content-type").and_then(|v| v.to_str().ok());
    if content_type != Some(OBJECT_CONTENT_TYPE) {
        objects.with(|state| state.wrong_content_type += 1);
        return api_error(ErrorCode::BadRequest, "unexpected content type");
    }
    let declared = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared != Some(body.len()) {
        return api_error(ErrorCode::BadRequest, "content-length does not match");
    }
    let Ok(digest) = Digest::parse(&digest) else {
        return api_error(ErrorCode::BadRequest, "bad digest");
    };
    // Verify like the controller does before publishing.
    if decode(&body, &digest).is_err() {
        return api_error(ErrorCode::Conflict, "the object does not match its digest");
    }
    objects.plant(&digest, body.to_vec());
    StatusCode::NO_CONTENT.into_response()
}

#[derive(serde::Deserialize)]
struct GetQuery {
    raw: Option<String>,
}

async fn get_object(
    State(objects): State<Objects>,
    Path(digest): Path<String>,
    Query(query): Query<GetQuery>,
    headers: HeaderMap,
) -> Response {
    if !objects.authorized(&headers) {
        return api_error(ErrorCode::Unauthorized, "no token");
    }
    let raw = query.raw.as_deref() == Some("1");
    let (stored, interrupt) = objects.with(|state| {
        if raw {
            state.raw_gets += 1;
        } else {
            state.gets += 1;
        }
        let interrupt = state.break_gets > 0;
        if interrupt {
            state.break_gets -= 1;
        }
        (state.stored.get(&digest).cloned(), interrupt)
    });
    let Some(stored) = stored else {
        return api_error(ErrorCode::NotFound, "object not found");
    };
    let (content_type, bytes) = if raw {
        // Planted garbage is served as it is, so a test can make the raw
        // route lie.
        let parsed = Digest::parse(&digest).unwrap();
        let bytes = decode(&stored, &parsed).unwrap_or(stored);
        ("application/octet-stream", bytes)
    } else {
        (OBJECT_CONTENT_TYPE, stored)
    };
    if interrupt {
        let half = bytes.len() / 2;
        let chunks: Vec<Result<Bytes, io::Error>> = vec![
            Ok(Bytes::copy_from_slice(&bytes[..half])),
            Err(io::Error::other("connection lost")),
        ];
        return Response::builder()
            .header("content-type", content_type)
            .body(Body::from_stream(futures_util::stream::iter(chunks)))
            .unwrap();
    }
    Response::builder()
        .header("content-type", content_type)
        .body(Body::from(bytes))
        .unwrap()
}

async fn head_object(
    State(objects): State<Objects>,
    Path(digest): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !objects.authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let (stored, omit) = objects.with(|state| {
        state.heads += 1;
        (state.stored.get(&digest).cloned(), state.omit_length)
    });
    let Some(stored) = stored else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut header = [0u8; format::HEADER_LEN];
    header.copy_from_slice(&stored[..format::HEADER_LEN]);
    let length = format::Header::parse(&header).unwrap().len;
    let mut response = Response::builder().header("content-type", OBJECT_CONTENT_TYPE);
    if !omit {
        response = response.header(OBJECT_LENGTH_HEADER, length.to_string());
    }
    response.body(Body::empty()).unwrap()
}

/// A fresh in-memory object server, for tests in other modules.
pub fn object_router() -> Router {
    Objects::default().router()
}

/// Runs blocking store calls where they are allowed: on a plain thread.
async fn on_thread<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    crate::support::within(rx)
        .await
        .expect("the store thread panicked")
}

struct Fixture {
    objects: Objects,
    server: Server,
    store: HttpObjectStore,
}

async fn fixture(tls: bool) -> Fixture {
    fixture_with(tls, fast_options(), ObjectStoreOptions::default()).await
}

async fn fixture_with(tls: bool, client: ClientOptions, store: ObjectStoreOptions) -> Fixture {
    let objects = Objects::default();
    let server = Server::start(tls, objects.router()).await;
    let store = HttpObjectStore::with_options(server.client_with(client), Handle::current(), store);
    Fixture {
        objects,
        server,
        store,
    }
}

async fn round_trips(tls: bool) {
    let fx = fixture(tls).await;
    // Empty, small, and larger than the 4 MiB memory spool even after
    // compression, so both the memory and the file path carry an object.
    let contents = vec![
        Vec::new(),
        b"hello object".to_vec(),
        noise(5 * 1024 * 1024 + 123, 7),
        vec![b'a'; 3 * 1024 * 1024],
    ];
    for content in contents {
        let digest = Digest::of_bytes(&content);
        let len = content.len() as u64;
        let store = fx.store.clone();
        let (expected, input) = (digest.clone(), content.clone());
        let result = on_thread(move || {
            assert!(!store.contains(&expected).unwrap());
            assert_eq!(store.size(&expected).unwrap(), None);
            let put = store.put(&mut input.as_slice(), None).unwrap();
            assert!(store.contains(&expected).unwrap());
            let size = store.size(&expected).unwrap();
            let read = get_bytes(&store, &expected).unwrap();
            (put, size, read)
        })
        .await;
        assert_eq!(result.0, (digest.clone(), len));
        assert_eq!(result.1, Some(len));
        assert_eq!(result.2, content, "{len} bytes");

        // The server holds a valid stored-format object with that content.
        let stored = fx
            .objects
            .with(|state| state.stored.get(digest.as_str()).cloned())
            .unwrap();
        assert_eq!(decode(&stored, &digest).unwrap(), content);
    }
    fx.objects.with(|state| {
        assert_eq!(state.puts, 4);
        assert_eq!(state.gets, 4);
        assert_eq!(state.unauthorized, 0);
        assert_eq!(state.wrong_content_type, 0);
    });
    drop(fx.server);
}

#[tokio::test]
async fn objects_round_trip_over_plain_http() {
    round_trips(false).await;
}

#[tokio::test]
async fn objects_round_trip_over_pinned_tls() {
    round_trips(true).await;
}

#[tokio::test]
async fn an_upload_is_skipped_when_the_controller_has_the_object() {
    let fx = fixture(false).await;
    let content = noise(100_000, 3);
    let digest = Digest::of_bytes(&content);
    let store = fx.store.clone();
    let (first, second) = on_thread(move || {
        let first = store.put(&mut content.as_slice(), None).unwrap();
        let second = store.put(&mut content.as_slice(), Some(&first.0)).unwrap();
        (first, second)
    })
    .await;
    assert_eq!(first, (digest, 100_000));
    assert_eq!(second, first);
    fx.objects.with(|state| {
        assert_eq!(state.puts, 1, "the second put uploaded nothing");
        assert_eq!(state.missing_requests, vec![1, 1]);
    });
}

#[tokio::test]
async fn a_wrong_expected_digest_fails_before_anything_is_sent() {
    let fx = fixture(false).await;
    let store = fx.store.clone();
    let wrong = Digest::of_bytes(b"something else");
    let expected = wrong.clone();
    let error = on_thread(move || {
        store
            .put(&mut b"the real content".as_slice(), Some(&expected))
            .unwrap_err()
    })
    .await;
    match error {
        StoreError::DigestMismatch { expected, actual } => {
            assert_eq!(expected, wrong);
            assert_eq!(actual, Digest::of_bytes(b"the real content"));
        }
        other => panic!("expected a digest mismatch, got {other:?}"),
    }
    fx.objects.with(|state| {
        assert_eq!(state.puts, 0);
        assert!(state.missing_requests.is_empty(), "not even a lookup");
        assert!(state.stored.is_empty());
    });
}

#[tokio::test]
async fn an_object_over_the_limit_is_refused_locally() {
    let fx = fixture_with(
        false,
        fast_options(),
        ObjectStoreOptions {
            max_object_bytes: 1000,
            ..ObjectStoreOptions::default()
        },
    )
    .await;
    let store = fx.store.clone();
    let error =
        on_thread(move || store.put(&mut noise(1001, 1).as_slice(), None).unwrap_err()).await;
    assert!(
        matches!(error, StoreError::TooLarge { limit: 1000 }),
        "{error:?}"
    );
    fx.objects.with(|state| assert_eq!(state.puts, 0));

    // And a download that announces more than the limit is cut off.
    let content = noise(3000, 2);
    let digest = Digest::of_bytes(&content);
    fx.objects.plant(&digest, encode(&content));
    let store = fx.store.clone();
    let error = on_thread(move || store.open(&digest).err().unwrap()).await;
    assert!(
        matches!(error, StoreError::TooLarge { limit: 1000 }),
        "{error:?}"
    );
}

/// Opens `digest` and reads it to the end.
async fn fetch(store: &HttpObjectStore, digest: &Digest) -> Result<Vec<u8>, StoreError> {
    let (store, digest) = (store.clone(), digest.clone());
    on_thread(move || {
        let mut reader = store.open(&digest)?;
        let mut out = Vec::new();
        reader.read_to_end(&mut out)?;
        Ok(out)
    })
    .await
}

#[tokio::test]
async fn a_download_that_does_not_verify_is_never_returned() {
    let fx = fixture(false).await;
    let content = noise(300_000, 11);
    let digest = Digest::of_bytes(&content);
    let good = encode(&content);

    // The untouched object reads back.
    fx.objects.plant(&digest, good.clone());
    assert_eq!(fetch(&fx.store, &digest).await.unwrap(), content);

    // The content of another object under this name.
    let other = noise(300_000, 12);
    fx.objects.plant(&digest, encode(&other));
    let error = fetch(&fx.store, &digest).await.unwrap_err();
    assert!(error.to_string().contains("digest mismatch"), "{error}");

    // One flipped byte in the compressed frame.
    let mut corrupt = good.clone();
    let middle = corrupt.len() / 2;
    corrupt[middle] ^= 0x55;
    fx.objects.plant(&digest, corrupt);
    assert!(matches!(
        fetch(&fx.store, &digest).await.unwrap_err(),
        StoreError::Io(_)
    ));

    // Truncated: inside the frame, inside the header, and empty.
    for keep in [good.len() - 1, good.len() / 3, 10, 0] {
        fx.objects.plant(&digest, good[..keep].to_vec());
        let error = fetch(&fx.store, &digest).await.unwrap_err();
        assert!(matches!(error, StoreError::Io(_)), "kept {keep}: {error:?}");
    }

    // A recorded length that is too short or too long for the frame.
    for wrong in [content.len() as u64 - 1, content.len() as u64 + 1, 0] {
        let mut bad = good.clone();
        bad[8..16].copy_from_slice(&wrong.to_le_bytes());
        fx.objects.plant(&digest, bad);
        let error = fetch(&fx.store, &digest).await.unwrap_err();
        assert!(
            matches!(error, StoreError::Io(_)),
            "length {wrong}: {error:?}"
        );
    }

    // Bytes after the end of the object.
    let mut extended = good.clone();
    extended.extend_from_slice(b"trailing");
    fx.objects.plant(&digest, extended);
    assert!(fetch(&fx.store, &digest).await.is_err());

    // Not a stored object at all.
    fx.objects.plant(&digest, content.clone());
    assert!(fetch(&fx.store, &digest).await.is_err());

    // None of these were retried: a bad object does not get better.
    fx.objects.with(|state| assert_eq!(state.gets, 12));
}

#[tokio::test]
async fn an_absent_object_is_not_found() {
    let fx = fixture(false).await;
    let digest = Digest::of_bytes(b"never stored");
    let store = fx.store.clone();
    let wanted = digest.clone();
    let (open, size, contains) = on_thread(move || {
        (
            store.open(&wanted).err(),
            store.size(&wanted).unwrap(),
            store.contains(&wanted).unwrap(),
        )
    })
    .await;
    assert!(matches!(open, Some(StoreError::NotFound(found)) if found == digest));
    assert_eq!(size, None);
    assert!(!contains);
}

#[tokio::test]
async fn calls_from_inside_a_runtime_return_the_documented_error() {
    let fx = fixture(false).await;
    let digest = Digest::of_bytes(b"x");

    let check = |error: StoreError| match error {
        StoreError::Unavailable(message) => {
            assert!(message.contains("Tokio runtime"), "{message}");
            assert!(message.contains("plain thread"), "{message}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    };

    // Directly in an async task.
    check(fx.store.contains(&digest).unwrap_err());
    check(fx.store.size(&digest).unwrap_err());
    check(fx.store.open(&digest).err().unwrap());
    check(fx.store.put(&mut b"x".as_slice(), None).unwrap_err());
    check(fx.store.missing(std::slice::from_ref(&digest)).unwrap_err());
    let target = tempfile::tempdir().unwrap();
    let path = target.path().join("artifact");
    check(fx.store.fetch_raw_to_file(&digest, &path).unwrap_err());
    assert!(!path.exists(), "the refused call created no file");

    // A spawn_blocking closure carries the runtime context too.
    let store = fx.store.clone();
    let inner = digest.clone();
    let error = tokio::task::spawn_blocking(move || store.contains(&inner).unwrap_err())
        .await
        .unwrap();
    check(error);

    // Nothing reached the server.
    fx.objects.with(|state| {
        assert!(state.missing_requests.is_empty());
        assert_eq!(state.puts + state.gets + state.heads + state.raw_gets, 0);
    });
}

#[tokio::test]
async fn missing_is_asked_in_batches_and_keeps_the_callers_order() {
    let fx = fixture(false).await;
    let digests: Vec<Digest> = (0..2500u32)
        .map(|i| Digest::of_bytes(&i.to_le_bytes()))
        .collect();
    // Every third object exists.
    for digest in digests.iter().step_by(3) {
        fx.objects.plant(digest, encode(b"present"));
    }
    let mut asked = digests.clone();
    asked.push(digests[1].clone()); // a duplicate of a missing one
    let store = fx.store.clone();
    let missing = on_thread(move || store.missing(&asked).unwrap()).await;

    let expected: Vec<Digest> = digests
        .iter()
        .enumerate()
        .filter(|(index, _)| index % 3 != 0)
        .map(|(_, digest)| digest.clone())
        .collect();
    assert_eq!(missing, expected);
    fx.objects
        .with(|state| assert_eq!(state.missing_requests, vec![1000, 1000, 501]));

    let store = fx.store.clone();
    assert!(
        on_thread(move || store.missing(&[]).unwrap())
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn an_interrupted_upload_is_repeated() {
    for size in [10_000usize, 6 * 1024 * 1024] {
        let fx = fixture(false).await;
        fx.objects.with(|state| state.fail_puts = 2);
        let content = noise(size, 5);
        let digest = Digest::of_bytes(&content);
        let store = fx.store.clone();
        let input = content.clone();
        let put = on_thread(move || put_bytes(&store, &input)).await.unwrap();
        assert_eq!(put, digest);
        fx.objects
            .with(|state| assert_eq!(state.puts, 3, "{size} bytes"));
        // The third try carried the complete object again.
        let stored = fx
            .objects
            .with(|state| state.stored.get(digest.as_str()).cloned())
            .unwrap();
        assert_eq!(decode(&stored, &digest).unwrap(), content);
    }
}

#[tokio::test]
async fn an_upload_that_keeps_failing_gives_up_with_the_error() {
    let options = ClientOptions {
        retry: RetryPolicy {
            max_attempts: 2,
            ..fast_options().retry
        },
        ..fast_options()
    };
    let fx = fixture_with(false, options, ObjectStoreOptions::default()).await;
    fx.objects.with(|state| state.fail_puts = 100);
    let store = fx.store.clone();
    let error = on_thread(move || put_bytes(&store, b"content").unwrap_err()).await;
    assert!(matches!(error, StoreError::Unavailable(_)), "{error:?}");
    fx.objects.with(|state| {
        assert_eq!(state.puts, 2);
        assert!(state.stored.is_empty());
    });
}

#[tokio::test]
async fn an_interrupted_download_is_repeated_from_the_start() {
    let fx = fixture(false).await;
    let content = noise(2 * 1024 * 1024, 9);
    let digest = Digest::of_bytes(&content);
    fx.objects.plant(&digest, encode(&content));
    fx.objects.with(|state| state.break_gets = 2);
    assert_eq!(fetch(&fx.store, &digest).await.unwrap(), content);
    fx.objects.with(|state| assert_eq!(state.gets, 3));
}

#[tokio::test]
async fn spooled_objects_leave_nothing_behind() {
    let scratch = tempfile::tempdir().unwrap();
    let fx = fixture_with(
        false,
        fast_options(),
        ObjectStoreOptions {
            // Everything beyond a kilobyte goes through a temporary file.
            memory_spool_bytes: 1024,
            spool_dir: Some(scratch.path().to_path_buf()),
            ..ObjectStoreOptions::default()
        },
    )
    .await;
    let content = noise(400_000, 21);
    let digest = Digest::of_bytes(&content);
    let store = fx.store.clone();
    let input = content.clone();
    let read = on_thread(move || {
        let digest = put_bytes(&store, &input).unwrap();
        let mut reader = store.open(&digest).unwrap();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        out
    })
    .await;
    assert_eq!(read, content);
    assert!(
        fx.objects
            .with(|state| state.stored.contains_key(digest.as_str()))
    );
    assert_eq!(
        std::fs::read_dir(scratch.path()).unwrap().count(),
        0,
        "temporary files are anonymous and gone"
    );
}

#[tokio::test]
async fn size_needs_the_length_header() {
    let fx = fixture(false).await;
    let content = noise(1234, 4);
    let digest = Digest::of_bytes(&content);
    fx.objects.plant(&digest, encode(&content));
    let store = fx.store.clone();
    let wanted = digest.clone();
    assert_eq!(
        on_thread(move || store.size(&wanted).unwrap()).await,
        Some(1234)
    );

    fx.objects.with(|state| state.omit_length = true);
    let store = fx.store.clone();
    let error = on_thread(move || store.size(&digest).unwrap_err()).await;
    match error {
        StoreError::Unavailable(message) => {
            assert!(message.contains(OBJECT_LENGTH_HEADER), "{message}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    fx.objects.with(|state| {
        assert_eq!(state.heads, 2);
        assert_eq!(state.gets, 0, "size never downloads the object");
    });
}

#[tokio::test]
async fn raw_downloads_create_a_verified_file_exclusively() {
    let fx = fixture(false).await;
    let dir = tempfile::tempdir().unwrap();
    let content = noise(1_500_000, 31);
    let digest = Digest::of_bytes(&content);
    fx.objects.plant(&digest, encode(&content));

    // A new file with exactly the content.
    let path = dir.path().join("app.bin");
    let (store, wanted, target) = (fx.store.clone(), digest.clone(), path.clone());
    let written = on_thread(move || store.fetch_raw_to_file(&wanted, &target))
        .await
        .unwrap();
    assert_eq!(written, content.len() as u64);
    assert_eq!(std::fs::read(&path).unwrap(), content);
    fx.objects.with(|state| {
        assert_eq!(state.raw_gets, 1);
        assert_eq!(state.gets, 0, "the raw route was used");
    });

    // An existing file is not overwritten or removed.
    let existing = dir.path().join("existing.bin");
    std::fs::write(&existing, b"keep me").unwrap();
    let (store, wanted, target) = (fx.store.clone(), digest.clone(), existing.clone());
    let error = on_thread(move || store.fetch_raw_to_file(&wanted, &target).unwrap_err()).await;
    match error {
        StoreError::Io(error) => assert_eq!(error.kind(), io::ErrorKind::AlreadyExists),
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
    assert_eq!(std::fs::read(&existing).unwrap(), b"keep me");

    // An interrupted transfer is repeated and still ends in a whole file.
    fx.objects.with(|state| state.break_gets = 1);
    let retried = dir.path().join("retried.bin");
    let (store, wanted, target) = (fx.store.clone(), digest.clone(), retried.clone());
    on_thread(move || store.fetch_raw_to_file(&wanted, &target))
        .await
        .unwrap();
    assert_eq!(std::fs::read(&retried).unwrap(), content);

    // Content that does not match the digest leaves no file.
    let lying = Digest::of_bytes(b"what was asked for");
    fx.objects
        .plant(&lying, b"not a stored object, served raw as it is".to_vec());
    let bad = dir.path().join("bad.bin");
    let (store, wanted, target) = (fx.store.clone(), lying.clone(), bad.clone());
    let error = on_thread(move || store.fetch_raw_to_file(&wanted, &target).unwrap_err()).await;
    assert!(
        matches!(&error, StoreError::DigestMismatch { expected, .. } if *expected == lying),
        "{error:?}"
    );
    assert!(!bad.exists());

    // Neither does an absent object.
    let absent = Digest::of_bytes(b"absent");
    let gone = dir.path().join("gone.bin");
    let (store, wanted, target) = (fx.store.clone(), absent.clone(), gone.clone());
    let error = on_thread(move || store.fetch_raw_to_file(&wanted, &target).unwrap_err()).await;
    assert!(matches!(error, StoreError::NotFound(found) if found == absent));
    assert!(!gone.exists());
}

#[tokio::test]
async fn the_store_works_through_the_object_store_trait() {
    let fx = fixture(false).await;
    let store: Arc<dyn ObjectStore> = Arc::new(fx.store.clone());
    let digest = on_thread(move || {
        let digest = put_bytes(store.as_ref(), b"through the trait").unwrap();
        assert_eq!(
            get_bytes(store.as_ref(), &digest).unwrap(),
            b"through the trait"
        );
        digest
    })
    .await;
    assert_eq!(digest, Digest::of_bytes(b"through the trait"));
}
