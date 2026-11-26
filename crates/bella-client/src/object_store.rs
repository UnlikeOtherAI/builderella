//! The controller's object store over HTTP.

use crate::blocking::OwnedRuntime;
use crate::client::{Call, Client};
use crate::error::ClientError;
use crate::retry::Attempts;
use crate::spool::{Spool, SpoolContent};
use bella_core::api::MissingObjectsRequest;
use bella_core::ids::DigestWriter;
use bella_core::store::{ObjectStore, StoreError};
use bella_core::{Digest, ErrorCode};
use bella_store::format;
use bytes::Bytes;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue};
use reqwest::{Method, Response, Url};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Handle;

/// Content type of an object in the `bella_store::format` encoding: the
/// 16-byte header followed by one zstd frame.
pub const OBJECT_CONTENT_TYPE: &str = "application/vnd.builderella.object";

/// Response header of `HEAD /objects/{digest}` carrying the uncompressed
/// length of the object in decimal.
pub const OBJECT_LENGTH_HEADER: &str = "x-bella-object-length";

/// Digests asked about in one `POST /objects/missing`.
const MISSING_BATCH: usize = 1000;
/// Bytes read from a spooled file per body chunk of an upload.
const UPLOAD_CHUNK: usize = 64 * 1024;

const IN_RUNTIME: &str = "the HTTP object store blocks its calling thread and was called from a \
     thread that belongs to a Tokio runtime (an async task or a spawn_blocking closure); call it \
     from a plain thread such as std::thread::spawn or std::thread::scope";

/// Limits and scratch space of an [`HttpObjectStore`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectStoreOptions {
    /// An object being transferred is held in memory up to this many bytes
    /// and in a self-deleting temporary file beyond that. Default 4 MiB.
    pub memory_spool_bytes: usize,
    /// Largest object, by uncompressed length, that is uploaded or
    /// downloaded. Default `bella_core::defaults::MAX_OBJECT_BYTES`.
    pub max_object_bytes: u64,
    /// Directory for temporary files; the system's temporary directory when
    /// `None`.
    pub spool_dir: Option<PathBuf>,
}

impl Default for ObjectStoreOptions {
    fn default() -> Self {
        Self {
            memory_spool_bytes: 4 * 1024 * 1024,
            max_object_bytes: bella_core::defaults::MAX_OBJECT_BYTES,
            spool_dir: None,
        }
    }
}

/// [`ObjectStore`] backed by the controller's `/objects` routes.
///
/// # Calling context
///
/// `ObjectStore` is a blocking interface. Every method runs its requests on
/// the runtime behind the [`Handle`] given at construction and blocks the
/// calling thread until they finish. It must therefore be called from a
/// thread that is **not** part of a Tokio runtime: a thread made with
/// `std::thread::spawn` or `std::thread::scope`, or the main thread of a
/// synchronous program. Called from an async task, or from a
/// `spawn_blocking` closure (which also carries the runtime's context),
/// every method returns [`StoreError::Unavailable`] with a message that
/// says so. It never panics and never blocks a runtime thread. The runtime
/// must stay alive for as long as the store is used.
///
/// # Guarantees
///
/// * **Upload.** `put` compresses the reader's bytes into a spool while
///   hashing them, so what is uploaded is exactly what was hashed. With
///   `expected`, a mismatch fails before anything is sent. The controller
///   is asked whether it already has the object and the upload is skipped
///   when it does. An interrupted upload is repeated within the client's
///   retry policy.
/// * **Download.** `open` receives the stored encoding, decodes it with
///   `bella_store::format::decode` into a spool and checks the length and
///   the SHA-256 of the content against the digest before it returns. The
///   reader it returns therefore only ever yields verified bytes; a
///   damaged, truncated, over-long or wrong object is an error from `open`
///   itself.
/// * **Bounds.** Nothing larger than `max_object_bytes` is sent or
///   accepted, and a download is cut off once it is longer than any valid
///   encoding of the length it announced.
#[derive(Clone)]
pub struct HttpObjectStore {
    client: Client,
    handle: Handle,
    options: ObjectStoreOptions,
    /// Keeps the runtime of a blocking client alive while the store exists.
    _runtime: Option<Arc<OwnedRuntime>>,
}

impl std::fmt::Debug for HttpObjectStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpObjectStore")
            .field("client", &self.client)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl HttpObjectStore {
    /// A store that sends its requests through `client` on the runtime
    /// behind `handle`, with the default [`ObjectStoreOptions`].
    pub fn new(client: Client, handle: Handle) -> Self {
        Self::with_options(client, handle, ObjectStoreOptions::default())
    }

    /// [`HttpObjectStore::new`] with explicit limits.
    pub fn with_options(client: Client, handle: Handle, options: ObjectStoreOptions) -> Self {
        Self {
            client,
            handle,
            options,
            _runtime: None,
        }
    }

    pub(crate) fn with_runtime(client: Client, runtime: Arc<OwnedRuntime>) -> Self {
        Self {
            client,
            handle: runtime.handle(),
            options: ObjectStoreOptions::default(),
            _runtime: Some(runtime),
        }
    }

    /// Replaces the limits.
    pub fn options(mut self, options: ObjectStoreOptions) -> Self {
        self.options = options;
        self
    }

    /// The client requests go through.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Which of `digests` the controller does not have, in the order given
    /// and without duplicates. Asks in batches, so any number of digests
    /// can be passed. Blocking; see the type documentation.
    pub fn missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, StoreError> {
        self.guard()?;
        let mut missing = BTreeSet::new();
        for batch in digests.chunks(MISSING_BATCH) {
            let request = MissingObjectsRequest {
                digests: batch.to_vec(),
            };
            let response = self
                .handle
                .block_on(self.client.missing_objects(&request))
                .map_err(|e| store_error(e, None))?;
            missing.extend(response.missing);
        }
        let mut seen = BTreeSet::new();
        Ok(digests
            .iter()
            .filter(|digest| missing.contains(*digest) && seen.insert(*digest))
            .cloned()
            .collect())
    }

    /// Downloads the raw bytes of an object into a new file at `path` and
    /// returns their number.
    ///
    /// The file is created exclusively: an existing `path` fails with
    /// `AlreadyExists` and is left untouched. The bytes are hashed as they
    /// are written; if the transfer fails or the content does not match
    /// `digest`, the file is removed again and an error is returned, so a
    /// file that this call leaves behind is complete and verified. While
    /// the call is running the file is incomplete. Blocking; see the type
    /// documentation.
    pub fn fetch_raw_to_file(&self, digest: &Digest, path: &Path) -> Result<u64, StoreError> {
        self.guard()?;
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let result = self
            .download(digest, true, &mut file)
            .and_then(|len| file.sync_all().map(|()| len).map_err(StoreError::Io));
        drop(file);
        if result.is_err() {
            // Best effort: the error being returned is the one that matters.
            let _ = std::fs::remove_file(path);
        }
        result
    }

    /// Refuses to block a thread that belongs to a runtime.
    fn guard(&self) -> Result<(), StoreError> {
        if Handle::try_current().is_ok() {
            return Err(StoreError::Unavailable(IN_RUNTIME.to_owned()));
        }
        Ok(())
    }

    fn object_url(&self, digest: &Digest) -> Result<Url, StoreError> {
        self.client
            .endpoint(&["objects", digest.as_str()])
            .map_err(|e| store_error(e, None))
    }

    fn spool(&self) -> Spool {
        Spool::new(
            self.options.memory_spool_bytes,
            self.options.spool_dir.as_deref(),
        )
    }

    /// Sends a stored-format object, repeating the upload after a transient
    /// failure.
    fn upload(
        &self,
        digest: &Digest,
        stored_len: u64,
        content: SpoolContent,
    ) -> Result<(), StoreError> {
        let url = self.object_url(digest)?;
        let source = match content {
            SpoolContent::Memory(bytes) => UploadSource::Memory(Bytes::from(bytes)),
            SpoolContent::File(file) => UploadSource::File(Arc::new(Mutex::new(file))),
        };
        let client = &self.client;
        let upload = async {
            let mut attempts = Attempts::new(&client.options().retry);
            loop {
                let call = Call::new(Method::PUT, url.clone())
                    .header(CONTENT_TYPE, HeaderValue::from_static(OBJECT_CONTENT_TYPE))
                    .header(CONTENT_LENGTH, HeaderValue::from(stored_len));
                let failure = match client.upload_once(&call, source.body(stored_len)).await {
                    Ok(_) => return Ok(()),
                    Err(failure) => failure,
                };
                if !failure.transient {
                    return Err(failure.error);
                }
                let Some(delay) = attempts.next_delay() else {
                    return Err(failure.error);
                };
                tracing::debug!(error = %failure.error, %digest, "repeating an object upload");
                tokio::time::sleep(delay).await;
                // The upload may have arrived although its answer did not.
                let request = MissingObjectsRequest {
                    digests: vec![digest.clone()],
                };
                if let Ok(response) = client.missing_objects(&request).await
                    && !response.missing.contains(digest)
                {
                    return Ok(());
                }
            }
        };
        self.handle
            .block_on(upload)
            .map_err(|e| store_error(e, Some(digest)))
    }

    /// Downloads an object into `sink`, verified, and returns the number of
    /// content bytes written. With `raw` the controller sends the content
    /// itself; otherwise the stored encoding, which is decoded here.
    ///
    /// `sink` holds exactly the verified content when this returns `Ok`.
    /// After a transient failure the sink is emptied and the download
    /// starts again, within the client's retry policy.
    fn download(&self, digest: &Digest, raw: bool, sink: &mut dyn Sink) -> Result<u64, StoreError> {
        let mut url = self.object_url(digest)?;
        if raw {
            url.query_pairs_mut().append_pair("raw", "1");
        }
        let mut attempts = Attempts::new(&self.client.options().retry);
        loop {
            let error = match self.download_once(&url, digest, raw, sink) {
                Ok(len) => return Ok(len),
                Err(Interrupted::Store(error)) => return Err(error),
                Err(Interrupted::Transfer(error)) => error,
            };
            if !error.is_transient() {
                return Err(store_error(error, Some(digest)));
            }
            let Some(delay) = attempts.next_delay() else {
                return Err(store_error(error, Some(digest)));
            };
            tracing::debug!(%error, %digest, "repeating an object download");
            self.handle.block_on(tokio::time::sleep(delay));
            sink.reset()?;
        }
    }

    fn download_once(
        &self,
        url: &Url,
        digest: &Digest,
        raw: bool,
        sink: &mut dyn Sink,
    ) -> Result<u64, Interrupted> {
        let mut call = Call::new(Method::GET, url.clone()).accept(if raw {
            "application/octet-stream"
        } else {
            OBJECT_CONTENT_TYPE
        });
        // `download` repeats the whole transfer, so one try is one request.
        call.retry = false;
        let response = self
            .handle
            .block_on(self.client.open_once(&call))
            .map_err(|failure| Interrupted::Transfer(failure.error))?;

        let max = self.options.max_object_bytes;
        let limit = Cell::new(if raw {
            max
        } else {
            format::max_stored_len(max)
        });
        let failure = RefCell::new(None);
        let mut body = ChunkReader {
            response,
            handle: &self.handle,
            idle: self.client.options().stream_idle_timeout,
            current: Bytes::new(),
            received: 0,
            limit: &limit,
            failure: &failure,
        };

        let copied = if raw {
            let mut hasher = DigestWriter::new();
            copy(&mut body, sink, Some(&mut hasher)).and_then(|len| {
                let (actual, _) = hasher.finish();
                if actual == *digest {
                    Ok(len)
                } else {
                    Err(StoreError::DigestMismatch {
                        expected: digest.clone(),
                        actual,
                    })
                }
            })
        } else {
            format::decode(&mut body, digest)
                .map_err(StoreError::Io)
                .and_then(|(mut content, len)| {
                    if len > max {
                        return Err(StoreError::TooLarge { limit: max });
                    }
                    // Past this point the stream cannot be valid.
                    limit.set(format::max_stored_len(len));
                    // The digest and the length are checked by `content`
                    // when the copy reaches the end of the stream.
                    copy(&mut content, sink, None)
                })
        };
        match copied {
            Ok(len) => Ok(len),
            Err(error) => match failure.take() {
                // The decoder only saw the stream end; the cause was the
                // transfer.
                Some(transfer) => Err(Interrupted::Transfer(transfer)),
                None => Err(Interrupted::Store(error)),
            },
        }
    }
}

impl ObjectStore for HttpObjectStore {
    fn contains(&self, digest: &Digest) -> Result<bool, StoreError> {
        Ok(self.missing(std::slice::from_ref(digest))?.is_empty())
    }

    fn put(
        &self,
        reader: &mut dyn Read,
        expected: Option<&Digest>,
    ) -> Result<(Digest, u64), StoreError> {
        self.guard()?;
        let mut spool = self.spool();
        let encoded = format::encode(reader, &mut spool, self.options.max_object_bytes)?;
        if let Some(expected) = expected
            && *expected != encoded.digest
        {
            return Err(StoreError::DigestMismatch {
                expected: expected.clone(),
                actual: encoded.digest,
            });
        }
        if self.contains(&encoded.digest)? {
            return Ok((encoded.digest, encoded.len));
        }
        self.upload(&encoded.digest, encoded.stored_len, spool.into_content())?;
        Ok((encoded.digest, encoded.len))
    }

    fn open(&self, digest: &Digest) -> Result<Box<dyn Read + Send>, StoreError> {
        self.guard()?;
        let mut spool = self.spool();
        self.download(digest, false, &mut spool)?;
        spool.seek(SeekFrom::Start(0))?;
        Ok(Box::new(spool))
    }

    /// Uses `HEAD /objects/{digest}` and its [`OBJECT_LENGTH_HEADER`].
    fn size(&self, digest: &Digest) -> Result<Option<u64>, StoreError> {
        self.guard()?;
        let call = Call::new(Method::HEAD, self.object_url(digest)?);
        let response = match self.handle.block_on(self.client.open(&call)) {
            Ok(response) => response,
            Err(error) if is_not_found(&error) => return Ok(None),
            Err(error) => return Err(store_error(error, Some(digest))),
        };
        let length = response
            .headers()
            .get(HeaderName::from_static(OBJECT_LENGTH_HEADER))
            .and_then(|value| value.to_str().ok())
            .and_then(|text| text.trim().parse::<u64>().ok());
        match length {
            Some(length) => Ok(Some(length)),
            None => Err(StoreError::Unavailable(format!(
                "the controller did not report the length of object {digest} \
                 (no usable `{OBJECT_LENGTH_HEADER}` header)"
            ))),
        }
    }
}

/// Where an upload reads its bytes from on every try.
enum UploadSource {
    Memory(Bytes),
    File(Arc<Mutex<File>>),
}

impl UploadSource {
    /// A fresh body over the first `len` bytes.
    ///
    /// A file body reads at explicit offsets under the lock, one chunk per
    /// blocking task, so the body of an abandoned try that is still being
    /// torn down cannot move the position under a later try.
    fn body(&self, len: u64) -> reqwest::Body {
        match self {
            UploadSource::Memory(bytes) => reqwest::Body::from(bytes.clone()),
            UploadSource::File(file) => {
                let stream =
                    futures_util::stream::unfold((file.clone(), 0u64), move |(file, offset)| {
                        async move {
                            if offset >= len {
                                return None;
                            }
                            let want = usize::try_from(len - offset)
                                .unwrap_or(UPLOAD_CHUNK)
                                .min(UPLOAD_CHUNK);
                            let source = file.clone();
                            let read =
                                tokio::task::spawn_blocking(move || read_at(&source, offset, want))
                                    .await
                                    .unwrap_or_else(|e| Err(io::Error::other(e)));
                            match read {
                                Ok(chunk) => {
                                    let next = offset + chunk.len() as u64;
                                    Some((Ok(chunk), (file, next)))
                                }
                                // Ends the stream after reporting the error.
                                Err(error) => Some((Err(error), (file, len))),
                            }
                        }
                    });
                reqwest::Body::wrap_stream(stream)
            }
        }
    }
}

/// Reads exactly `want` bytes at `offset`.
fn read_at(file: &Mutex<File>, offset: u64, want: usize) -> io::Result<Bytes> {
    let mut file = file
        .lock()
        .map_err(|_| io::Error::other("the spool file lock is poisoned"))?;
    file.seek(SeekFrom::Start(offset))?;
    let mut chunk = vec![0u8; want];
    file.read_exact(&mut chunk)?;
    Ok(Bytes::from(chunk))
}

/// Why one download try ended.
enum Interrupted {
    /// The connection failed; another try may succeed.
    Transfer(ClientError),
    /// The object, or writing it locally, is the problem.
    Store(StoreError),
}

/// Destination of a download that can be emptied for another try.
trait Sink: Write {
    fn reset(&mut self) -> io::Result<()>;
}

impl Sink for Spool {
    fn reset(&mut self) -> io::Result<()> {
        Spool::reset(self)
    }
}

impl Sink for File {
    fn reset(&mut self) -> io::Result<()> {
        self.set_len(0)?;
        self.seek(SeekFrom::Start(0))?;
        Ok(())
    }
}

/// Copies `reader` to its end into `sink`, hashing on the way if asked.
fn copy(
    reader: &mut dyn Read,
    sink: &mut dyn Sink,
    mut hasher: Option<&mut DigestWriter>,
) -> Result<u64, StoreError> {
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => return Ok(total),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(StoreError::Io(error)),
        };
        if let Some(hasher) = hasher.as_deref_mut() {
            hasher.update(&buffer[..read]);
        }
        sink.write_all(&buffer[..read])?;
        total += read as u64;
    }
}

/// A response body as a blocking reader.
///
/// Each read waits for the next chunk on the runtime. A transfer failure is
/// recorded in `failure` (the `io::Error` it returns loses the detail), and
/// a body longer than `limit` is `InvalidData`.
struct ChunkReader<'a> {
    response: Response,
    handle: &'a Handle,
    idle: Option<Duration>,
    current: Bytes,
    received: u64,
    limit: &'a Cell<u64>,
    failure: &'a RefCell<Option<ClientError>>,
}

impl ChunkReader<'_> {
    fn next_chunk(&mut self) -> Result<Option<Bytes>, ClientError> {
        let idle = self.idle;
        let response = &mut self.response;
        self.handle.block_on(async move {
            let chunk = match idle {
                Some(idle) => tokio::time::timeout(idle, response.chunk())
                    .await
                    .map_err(|_| ClientError::Timeout)?,
                None => response.chunk().await,
            };
            chunk.map_err(ClientError::from_reqwest)
        })
    }
}

impl Read for ChunkReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.current.is_empty() {
            match self.next_chunk() {
                Ok(Some(chunk)) => {
                    self.received = self.received.saturating_add(chunk.len() as u64);
                    if self.received > self.limit.get() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "the object is longer than its announced or permitted length",
                        ));
                    }
                    self.current = chunk;
                }
                Ok(None) => return Ok(0),
                Err(error) => {
                    self.failure.replace(Some(error.clone()));
                    return Err(io::Error::other(error));
                }
            }
        }
        let take = buf.len().min(self.current.len());
        buf[..take].copy_from_slice(&self.current[..take]);
        self.current = self.current.slice(take..);
        Ok(take)
    }
}

fn is_not_found(error: &ClientError) -> bool {
    match error {
        ClientError::Api(api) => api.code == ErrorCode::NotFound,
        ClientError::Http { status, .. } => *status == 404,
        _ => false,
    }
}

/// Expresses a client failure in the vocabulary of the store interface.
fn store_error(error: ClientError, digest: Option<&Digest>) -> StoreError {
    match digest {
        Some(digest) if is_not_found(&error) => StoreError::NotFound(digest.clone()),
        _ => StoreError::Unavailable(error.to_string()),
    }
}
