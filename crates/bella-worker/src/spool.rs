//! The log spool: output of managed processes on local disk, and what of it
//! still has to reach the controller.
//!
//! Every `(step, stream)` of an attempt is a sequence of segment files
//! `spool/<attempt>/<step>.<stream>.<start offset>.log`. Offsets are byte
//! offsets within the stream, so a chunk is identified by where it starts
//! and resending it is idempotent. A segment is deleted once the controller
//! acknowledged everything in it.
//!
//! The spool is the source of truth for how much was produced. The journal
//! is told (`record_log_produced`) before bytes are handed to the
//! connection, never after.

use crate::b64;
use bella_core::AttemptId;
use bella_core::defaults::LOG_CHUNK_BYTES;
use bella_core::protocol::{AttemptRef, LogChunk, LogStream};
use bella_journal::Journal;
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Instant;
use tokio::sync::Notify;

/// Bytes a pump reads from a pipe at a time.
const PUMP_BUFFER_BYTES: usize = 16 * 1024;

fn stream_name(stream: LogStream) -> &'static str {
    match stream {
        LogStream::Stdout => "stdout",
        LogStream::Stderr => "stderr",
        LogStream::System => "system",
    }
}

fn stream_from_name(name: &str) -> Option<LogStream> {
    match name {
        "stdout" => Some(LogStream::Stdout),
        "stderr" => Some(LogStream::Stderr),
        "system" => Some(LogStream::System),
        _ => None,
    }
}

type StreamKey = (String, LogStream);

struct Segment {
    start: u64,
    len: u64,
    path: PathBuf,
}

#[derive(Default)]
struct Stream {
    segments: VecDeque<Segment>,
    /// Open handle on the last segment.
    writer: Option<File>,
    /// End of the stream: bytes written to the spool so far.
    produced: u64,
    /// Bytes the controller confirmed.
    acked: u64,
    /// Bytes handed to the current connection.
    sent: u64,
    /// The producer hit the allowance; `produced` is final.
    truncated: bool,
    /// The final, truncated chunk went out on the current connection.
    truncation_sent: bool,
    /// What the journal was told.
    journal_produced: u64,
    journal_truncated: bool,
}

struct Inner {
    streams: BTreeMap<StreamKey, Stream>,
    /// Captured output still allowed for the whole attempt.
    remaining: u64,
    /// The truncation notice was written.
    noticed: bool,
    /// No further output is accepted.
    closed: bool,
    /// The stream that was served last, so streams take turns.
    turn: Option<StreamKey>,
}

/// The spool of one attempt.
pub(crate) struct AttemptLogs {
    attempt: AttemptRef,
    dir: PathBuf,
    segment_bytes: u64,
    max_log_bytes: u64,
    inner: Mutex<Inner>,
    wake: Arc<Notify>,
}

impl AttemptLogs {
    /// A new, empty spool in `dir` with `max_log_bytes` of captured output
    /// allowed across all its `Stdout` and `Stderr` streams.
    pub(crate) fn create(
        attempt: AttemptRef,
        dir: PathBuf,
        max_log_bytes: u64,
        segment_bytes: u64,
        wake: Arc<Notify>,
    ) -> std::io::Result<Arc<Self>> {
        fs::create_dir_all(&dir)?;
        Ok(Arc::new(Self {
            attempt,
            dir,
            segment_bytes: segment_bytes.max(1),
            max_log_bytes,
            inner: Mutex::new(Inner {
                streams: BTreeMap::new(),
                remaining: max_log_bytes,
                noticed: false,
                closed: false,
                turn: None,
            }),
            wake,
        }))
    }

    /// Rebuild the spool of an attempt from an earlier worker process from
    /// its segment files and the journal's offsets. It accepts `System`
    /// lines but no further captured output.
    pub(crate) fn load(
        attempt: AttemptRef,
        dir: PathBuf,
        journal: &Journal,
        segment_bytes: u64,
        wake: Arc<Notify>,
    ) -> Arc<Self> {
        let mut streams: BTreeMap<StreamKey, Stream> = BTreeMap::new();
        let mut files: Vec<(StreamKey, u64, PathBuf, u64)> = Vec::new();
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let mut parts = name.split('.');
            let (Some(step), Some(stream), Some(start), Some("log"), None) = (
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
            ) else {
                continue;
            };
            let (Some(stream), Ok(start)) = (stream_from_name(stream), start.parse::<u64>()) else {
                continue;
            };
            let len = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            files.push(((step.to_string(), stream), start, path, len));
        }
        files.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        for (key, start, path, len) in files {
            let stream = streams.entry(key).or_default();
            // Segments are contiguous by construction; a hole means files
            // were lost, and only what follows the hole can still be read.
            if stream.produced != start {
                stream.segments.clear();
            }
            stream.segments.push_back(Segment { start, len, path });
            stream.produced = start + len;
        }
        for cursor in journal.log_cursors(&attempt.attempt_id).unwrap_or_default() {
            let stream = streams.entry((cursor.step, cursor.stream)).or_default();
            stream.acked = cursor.offsets.acked.min(stream.produced);
            stream.journal_produced = cursor.offsets.produced;
            stream.journal_truncated = cursor.offsets.truncated;
            stream.truncated = cursor.offsets.truncated;
        }
        for stream in streams.values_mut() {
            stream.sent = stream.acked;
        }
        Arc::new(Self {
            attempt,
            dir,
            segment_bytes: segment_bytes.max(1),
            max_log_bytes: 0,
            inner: Mutex::new(Inner {
                streams,
                remaining: 0,
                noticed: true,
                closed: false,
                turn: None,
            }),
            wake,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn segment_path(&self, key: &StreamKey, start: u64) -> PathBuf {
        self.dir
            .join(format!("{}.{}.{start:020}.log", key.0, stream_name(key.1)))
    }

    /// Append to one stream's spool. Returns `false` when the write failed.
    fn write(&self, key: &StreamKey, stream: &mut Stream, mut data: &[u8]) -> bool {
        while !data.is_empty() {
            let room = match stream.segments.back() {
                Some(last) if stream.writer.is_some() && last.len < self.segment_bytes => {
                    self.segment_bytes - last.len
                }
                _ => {
                    let path = self.segment_path(key, stream.produced);
                    let file = OpenOptions::new().create(true).append(true).open(&path);
                    let Ok(file) = file else {
                        return false;
                    };
                    stream.writer = Some(file);
                    stream.segments.push_back(Segment {
                        start: stream.produced,
                        len: 0,
                        path,
                    });
                    self.segment_bytes
                }
            };
            let take = data.len().min(usize::try_from(room).unwrap_or(usize::MAX));
            let (now, rest) = data.split_at(take);
            let written = stream
                .writer
                .as_mut()
                .is_some_and(|file| file.write_all(now).is_ok());
            if !written {
                return false;
            }
            if let Some(last) = stream.segments.back_mut() {
                last.len += take as u64;
            }
            stream.produced += take as u64;
            data = rest;
        }
        true
    }

    /// Append captured output of a step. Bytes beyond the attempt's
    /// allowance are discarded; the first time that happens a `System` line
    /// saying so is written for that step and the stream is marked
    /// truncated. The caller keeps reading its pipe either way.
    pub(crate) fn append(&self, step: &str, stream: LogStream, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut inner = self.lock();
        if inner.closed {
            return;
        }
        let key = (step.to_string(), stream);
        let take = data
            .len()
            .min(usize::try_from(inner.remaining).unwrap_or(usize::MAX));
        let mut entry = inner.streams.remove(&key).unwrap_or_default();
        let mut failed = false;
        if take > 0 && !entry.truncated {
            failed = !self.write(&key, &mut entry, &data[..take]);
            inner.remaining -= take as u64;
        }
        let over = take < data.len() || failed;
        if over {
            entry.truncated = true;
        }
        inner.streams.insert(key, entry);
        if over && !inner.noticed {
            inner.noticed = true;
            let notice = if failed {
                "[builderella] log capture stopped: the log spool could not be written\n"
                    .to_string()
            } else {
                format!(
                    "[builderella] log truncated at {} bytes: the attempt reached its log allowance and further output is discarded\n",
                    self.max_log_bytes
                )
            };
            self.write_system(&mut inner, step, notice.as_bytes());
        }
        drop(inner);
        self.wake.notify_one();
    }

    fn write_system(&self, inner: &mut Inner, step: &str, data: &[u8]) {
        let key = (step.to_string(), LogStream::System);
        let mut entry = inner.streams.remove(&key).unwrap_or_default();
        let _ = self.write(&key, &mut entry, data);
        inner.streams.insert(key, entry);
    }

    /// Append a line written by the worker itself. It does not count
    /// against the allowance. A newline is added.
    pub(crate) fn system(&self, step: &str, line: &str) {
        let mut inner = self.lock();
        if inner.closed {
            return;
        }
        let mut text = String::with_capacity(line.len() + 1);
        text.push_str(line);
        text.push('\n');
        self.write_system(&mut inner, step, text.as_bytes());
        drop(inner);
        self.wake.notify_one();
    }

    /// Was captured output discarded because the allowance was reached?
    pub(crate) fn truncated(&self) -> bool {
        self.lock().streams.values().any(|stream| stream.truncated)
    }

    /// Stop accepting output and close the segment files.
    pub(crate) fn close(&self) {
        let mut inner = self.lock();
        inner.closed = true;
        for stream in inner.streams.values_mut() {
            stream.writer = None;
        }
    }

    /// Forget what was sent: the next connection resends from the
    /// acknowledged offsets.
    fn reset_sent(&self) {
        for stream in self.lock().streams.values_mut() {
            stream.sent = stream.acked;
            stream.truncation_sent = false;
        }
    }

    fn outstanding(&self) -> u64 {
        self.lock()
            .streams
            .values()
            .map(|stream| stream.sent.saturating_sub(stream.acked))
            .sum()
    }

    fn read_at(stream: &Stream, offset: u64, max: usize) -> Option<Vec<u8>> {
        let segment = stream
            .segments
            .iter()
            .find(|segment| segment.start <= offset && offset < segment.start + segment.len)?;
        let within = offset - segment.start;
        let len = max.min(usize::try_from(segment.len - within).unwrap_or(usize::MAX));
        let mut file = File::open(&segment.path).ok()?;
        file.seek(SeekFrom::Start(within)).ok()?;
        let mut data = vec![0u8; len];
        file.read_exact(&mut data).ok()?;
        Some(data)
    }

    /// The next chunk to send, at most `max` bytes, from the stream after
    /// the one served last (in key order, wrapping) that has unsent bytes
    /// or an unsent truncation mark. The journal learns of the bytes
    /// first. Returns the chunk and the number of bytes it carries.
    fn next_chunk(&self, journal: &Journal, max: usize) -> Option<(LogChunk, u64)> {
        let mut inner = self.lock();
        let pending = |stream: &Stream| {
            stream.sent < stream.produced || (stream.truncated && !stream.truncation_sent)
        };
        let key = {
            let turn = inner.turn.as_ref();
            let later = inner
                .streams
                .iter()
                .filter(|(key, stream)| turn.is_some_and(|turn| *key > turn) && pending(stream));
            let wrapped = inner.streams.iter().filter(|(_, stream)| pending(stream));
            later.chain(wrapped).next().map(|(key, _)| key.clone())?
        };
        inner.turn = Some(key.clone());
        let id = &self.attempt.attempt_id;
        let stream = inner.streams.get_mut(&key)?;
        if stream.journal_produced < stream.produced {
            match journal.record_log_produced(id, &key.0, key.1, stream.produced) {
                Ok(_) => stream.journal_produced = stream.produced,
                Err(error) => {
                    tracing::warn!(attempt = %id, %error, "could not journal a log offset");
                    return None;
                }
            }
        }
        if stream.truncated && !stream.journal_truncated {
            match journal.mark_log_truncated(id, &key.0, key.1) {
                Ok(_) => stream.journal_truncated = true,
                Err(error) => {
                    tracing::warn!(attempt = %id, %error, "could not journal a log truncation");
                    return None;
                }
            }
        }
        let offset = stream.sent;
        let data = if stream.sent < stream.produced {
            match Self::read_at(stream, offset, max) {
                Some(data) => data,
                None => {
                    // The bytes are gone from disk; nothing more of this
                    // stream can be delivered.
                    tracing::warn!(attempt = %id, step = %key.0, offset, "log spool segment is unreadable");
                    stream.sent = stream.produced;
                    return None;
                }
            }
        } else {
            Vec::new()
        };
        let len = data.len() as u64;
        stream.sent += len;
        let last = stream.truncated && stream.sent == stream.produced;
        if last {
            stream.truncation_sent = true;
        }
        let chunk = LogChunk {
            attempt: self.attempt.clone(),
            step: key.0.clone(),
            stream: key.1,
            offset,
            data_b64: b64::encode(&data),
            truncated: last,
        };
        Some((chunk, len))
    }

    /// Record an acknowledgement and delete the segments it covers.
    fn ack(&self, journal: &Journal, step: &str, stream: LogStream, through: u64) {
        let id = &self.attempt.attempt_id;
        let mut inner = self.lock();
        let closed = inner.closed;
        let Some(entry) = inner.streams.get_mut(&(step.to_string(), stream)) else {
            return;
        };
        let through = through.min(entry.produced);
        if through <= entry.acked {
            return;
        }
        if entry.journal_produced < through {
            match journal.record_log_produced(id, step, stream, entry.produced) {
                Ok(_) => entry.journal_produced = entry.produced,
                Err(_) => return,
            }
        }
        if let Err(error) = journal.ack_log(id, step, stream, through) {
            tracing::warn!(attempt = %id, %error, "could not journal a log acknowledgement");
            return;
        }
        entry.acked = through;
        entry.sent = entry.sent.max(through);
        while let Some(first) = entry.segments.front() {
            let end = first.start + first.len;
            let active = entry.segments.len() == 1 && entry.writer.is_some() && !closed;
            if end > through || active {
                break;
            }
            let _ = fs::remove_file(&first.path);
            entry.segments.pop_front();
        }
    }

    /// Delete the spool directory.
    fn remove(&self) {
        self.close();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Every attempt's spool, and the send window of the current connection.
pub(crate) struct LogHub {
    root: PathBuf,
    segment_bytes: u64,
    attempts: Mutex<BTreeMap<AttemptId, Arc<AttemptLogs>>>,
    /// The attempt whose turn it is next, so attempts take turns.
    turn: Mutex<usize>,
    wake: Arc<Notify>,
}

impl LogHub {
    pub(crate) fn new(root: PathBuf, segment_bytes: u64, wake: Arc<Notify>) -> Self {
        Self {
            root,
            segment_bytes,
            attempts: Mutex::new(BTreeMap::new()),
            turn: Mutex::new(0),
            wake,
        }
    }

    fn attempts(&self) -> MutexGuard<'_, BTreeMap<AttemptId, Arc<AttemptLogs>>> {
        self.attempts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn dir(&self, attempt: &AttemptId) -> PathBuf {
        self.root.join(attempt.as_str())
    }

    /// Start the spool of an attempt that is about to run.
    pub(crate) fn create(
        &self,
        attempt: &AttemptRef,
        max_log_bytes: u64,
    ) -> std::io::Result<Arc<AttemptLogs>> {
        let logs = AttemptLogs::create(
            attempt.clone(),
            self.dir(&attempt.attempt_id),
            max_log_bytes,
            self.segment_bytes,
            self.wake.clone(),
        )?;
        self.attempts()
            .insert(attempt.attempt_id.clone(), logs.clone());
        Ok(logs)
    }

    /// The spool of an attempt, loading what an earlier worker process
    /// left on disk if this process has not seen it yet.
    pub(crate) fn open(&self, attempt: &AttemptRef, journal: &Journal) -> Arc<AttemptLogs> {
        let mut attempts = self.attempts();
        if let Some(logs) = attempts.get(&attempt.attempt_id) {
            return logs.clone();
        }
        let dir = self.dir(&attempt.attempt_id);
        let _ = fs::create_dir_all(&dir);
        let logs = AttemptLogs::load(
            attempt.clone(),
            dir,
            journal,
            self.segment_bytes,
            self.wake.clone(),
        );
        attempts.insert(attempt.attempt_id.clone(), logs.clone());
        logs
    }

    pub(crate) fn get(&self, attempt: &AttemptId) -> Option<Arc<AttemptLogs>> {
        self.attempts().get(attempt).cloned()
    }

    /// Drop an attempt's spool, files included.
    pub(crate) fn remove(&self, attempt: &AttemptId) {
        let logs = self.attempts().remove(attempt);
        match logs {
            Some(logs) => logs.remove(),
            None => {
                let _ = fs::remove_dir_all(self.dir(attempt));
            }
        }
    }

    /// Remove spool directories that belong to no attempt in `known`.
    pub(crate) fn remove_orphans(&self, known: &dyn Fn(&str) -> bool) {
        for entry in fs::read_dir(&self.root).into_iter().flatten().flatten() {
            let name = entry.file_name();
            if !name.to_str().is_some_and(known) {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }

    /// A new connection starts: everything unacknowledged is sent again.
    pub(crate) fn reset_sent(&self) {
        for logs in self.attempts().values() {
            logs.reset_sent();
        }
    }

    /// The next log chunks for the connection, at most `limit`, keeping the
    /// bytes sent and not yet acknowledged at or below `window`. Attempts
    /// take turns, and within an attempt its streams do.
    pub(crate) fn next_chunks(
        &self,
        journal: &Journal,
        window: u64,
        limit: usize,
    ) -> Vec<LogChunk> {
        let attempts: Vec<Arc<AttemptLogs>> = self.attempts().values().cloned().collect();
        if attempts.is_empty() {
            return Vec::new();
        }
        let mut outstanding: u64 = attempts.iter().map(|logs| logs.outstanding()).sum();
        let mut turn = self.turn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut chunks = Vec::new();
        let mut idle = 0usize;
        while chunks.len() < limit && idle < attempts.len() && outstanding < window {
            let room = window - outstanding;
            let max = LOG_CHUNK_BYTES.min(usize::try_from(room).unwrap_or(usize::MAX));
            let logs = &attempts[*turn % attempts.len()];
            *turn = (*turn % attempts.len()) + 1;
            match logs.next_chunk(journal, max) {
                Some((chunk, len)) => {
                    outstanding = outstanding.saturating_add(len);
                    chunks.push(chunk);
                    idle = 0;
                }
                None => idle += 1,
            }
        }
        chunks
    }

    /// The controller confirmed a stream up to `through`.
    pub(crate) fn ack(
        &self,
        journal: &Journal,
        attempt: &AttemptId,
        step: &str,
        stream: LogStream,
        through: u64,
    ) {
        if let Some(logs) = self.get(attempt) {
            logs.ack(journal, step, stream, through);
        }
        self.wake.notify_one();
    }
}

/// Looks for a text in a stream of output, across read boundaries.
pub(crate) struct LogWatch {
    needle: Vec<u8>,
    found: Arc<AtomicBool>,
    tail: Vec<u8>,
}

impl LogWatch {
    /// A watch for `needle` that sets `found` on the first match.
    pub(crate) fn new(needle: &str, found: Arc<AtomicBool>) -> Self {
        Self {
            needle: needle.as_bytes().to_vec(),
            found,
            tail: Vec::new(),
        }
    }

    fn scan(&mut self, data: &[u8]) {
        if self.needle.is_empty() {
            self.found.store(true, Ordering::SeqCst);
            return;
        }
        if self.found.load(Ordering::SeqCst) {
            return;
        }
        self.tail.extend_from_slice(data);
        if self
            .tail
            .windows(self.needle.len())
            .any(|window| window == self.needle.as_slice())
        {
            self.found.store(true, Ordering::SeqCst);
            self.tail.clear();
            return;
        }
        let keep = self.needle.len() - 1;
        if self.tail.len() > keep {
            let cut = self.tail.len() - keep;
            self.tail.drain(..cut);
        }
    }
}

/// A thread that reads one pipe of a managed process to its end.
pub(crate) struct Pump {
    done: Arc<(Mutex<bool>, Condvar)>,
}

impl Pump {
    /// Read `reader` until end-of-file into the `(step, stream)` spool of
    /// `logs`, or into nothing when `logs` is `None`. The thread ends when
    /// the last process holding the pipe's write end has exited.
    pub(crate) fn spawn(
        logs: Option<Arc<AttemptLogs>>,
        step: &str,
        stream: LogStream,
        mut reader: Box<dyn Read + Send>,
        mut watch: Option<LogWatch>,
    ) -> Self {
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let signal = done.clone();
        let step = step.to_string();
        let spawned = std::thread::Builder::new()
            .name(format!("bella-log-{step}"))
            .spawn(move || {
                let mut buffer = vec![0u8; PUMP_BUFFER_BYTES];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Some(watch) = watch.as_mut() {
                                watch.scan(&buffer[..n]);
                            }
                            if let Some(logs) = &logs {
                                logs.append(&step, stream, &buffer[..n]);
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                let (flag, condvar) = &*signal;
                *flag.lock().unwrap_or_else(PoisonError::into_inner) = true;
                condvar.notify_all();
            });
        if spawned.is_err() {
            // Without a thread nothing reads the pipe; the reader was
            // dropped with the closure, which closes it.
            let (flag, _) = &*done;
            *flag.lock().unwrap_or_else(PoisonError::into_inner) = true;
        }
        Self { done }
    }

    /// Wait until the pipe reached end-of-file, at most until `deadline`.
    /// Returns whether it did.
    pub(crate) fn wait_until(&self, deadline: Instant) -> bool {
        let (flag, condvar) = &*self.done;
        let mut finished = flag.lock().unwrap_or_else(PoisonError::into_inner);
        while !*finished {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, _) = condvar
                .wait_timeout(finished, deadline - now)
                .unwrap_or_else(PoisonError::into_inner);
            finished = guard;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bella_core::protocol::Offer;
    use bella_core::{BootId, Clock, SystemClock};

    fn fixture(max: u64, segment: u64) -> (tempfile::TempDir, Journal, LogHub, Arc<AttemptLogs>) {
        let dir = tempfile::tempdir().unwrap();
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let journal = Journal::open_in_memory(clock).unwrap();
        journal.recover(&BootId::derive(b"boot")).unwrap();
        let offer: Offer = crate::core::tests_support::offer("spool");
        journal.hold_offer(&offer).unwrap();
        let hub = LogHub::new(dir.path().to_path_buf(), segment, Arc::new(Notify::new()));
        let logs = hub.create(&offer.attempt, max).unwrap();
        (dir, journal, hub, logs)
    }

    fn decode(text: &str) -> Vec<u8> {
        crate::core::tests_support::b64_decode(text)
    }

    #[test]
    fn chunks_carry_contiguous_offsets_and_acks_trim_segments() {
        let (dir, journal, hub, logs) = fixture(1 << 20, 10);
        logs.append("build", LogStream::Stdout, b"0123456789abcdefghij-tail");
        let attempt_dir = dir.path().join(logs.attempt.attempt_id.as_str());
        assert_eq!(fs::read_dir(&attempt_dir).unwrap().count(), 3);

        let chunks = hub.next_chunks(&journal, 1 << 20, 16);
        let mut joined = Vec::new();
        let mut expected_offset = 0;
        for chunk in &chunks {
            assert_eq!(chunk.offset, expected_offset);
            let data = decode(&chunk.data_b64);
            expected_offset += data.len() as u64;
            joined.extend(data);
        }
        assert_eq!(joined, b"0123456789abcdefghij-tail");
        assert!(hub.next_chunks(&journal, 1 << 20, 16).is_empty());

        hub.ack(
            &journal,
            &logs.attempt.attempt_id,
            "build",
            LogStream::Stdout,
            20,
        );
        assert_eq!(fs::read_dir(&attempt_dir).unwrap().count(), 1);
        let offsets = journal
            .log_offsets(&logs.attempt.attempt_id, "build", LogStream::Stdout)
            .unwrap();
        assert_eq!((offsets.produced, offsets.acked), (25, 20));

        // A new connection resends only what was not acknowledged.
        hub.reset_sent();
        let again = hub.next_chunks(&journal, 1 << 20, 16);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].offset, 20);
        assert_eq!(decode(&again[0].data_b64), b"-tail");
    }

    #[test]
    fn the_window_bounds_unacknowledged_bytes() {
        let (_dir, journal, hub, logs) = fixture(1 << 20, 1 << 20);
        logs.append("build", LogStream::Stdout, &[b'x'; 100]);
        let first = hub.next_chunks(&journal, 40, 16);
        let sent: usize = first.iter().map(|c| decode(&c.data_b64).len()).sum();
        assert_eq!(sent, 40);
        assert!(hub.next_chunks(&journal, 40, 16).is_empty());
        hub.ack(
            &journal,
            &logs.attempt.attempt_id,
            "build",
            LogStream::Stdout,
            40,
        );
        let second = hub.next_chunks(&journal, 40, 16);
        assert_eq!(second[0].offset, 40);
    }

    #[test]
    fn the_allowance_truncates_with_a_system_notice() {
        let (_dir, journal, hub, logs) = fixture(8, 1 << 20);
        logs.append("build", LogStream::Stdout, b"12345");
        logs.append("build", LogStream::Stderr, b"abcdef");
        logs.append("build", LogStream::Stdout, b"dropped");
        assert!(logs.truncated());
        let chunks = hub.next_chunks(&journal, 1 << 20, 16);
        let text = |stream: LogStream| -> Vec<u8> {
            chunks
                .iter()
                .filter(|c| c.stream == stream)
                .flat_map(|c| decode(&c.data_b64))
                .collect()
        };
        assert_eq!(text(LogStream::Stdout), b"12345");
        assert_eq!(text(LogStream::Stderr), b"abc");
        let notice = String::from_utf8(text(LogStream::System)).unwrap();
        assert!(notice.contains("log truncated at 8 bytes"), "{notice}");
        assert!(
            chunks
                .iter()
                .any(|c| c.stream == LogStream::Stderr && c.truncated)
        );
        assert!(
            chunks
                .iter()
                .any(|c| c.stream == LogStream::Stdout && c.truncated)
        );
        let offsets = journal
            .log_offsets(&logs.attempt.attempt_id, "build", LogStream::Stderr)
            .unwrap();
        assert!(offsets.truncated);
    }

    #[test]
    fn a_spool_is_rebuilt_from_disk_and_the_journal() {
        let (dir, journal, hub, logs) = fixture(1 << 20, 8);
        logs.append("build", LogStream::Stdout, b"0123456789abcdef-rest");
        hub.next_chunks(&journal, 1 << 20, 16);
        hub.ack(
            &journal,
            &logs.attempt.attempt_id,
            "build",
            LogStream::Stdout,
            16,
        );
        let attempt = logs.attempt.clone();
        drop(logs);

        let fresh = LogHub::new(dir.path().to_path_buf(), 8, Arc::new(Notify::new()));
        let loaded = fresh.open(&attempt, &journal);
        drop(loaded);
        let chunks = fresh.next_chunks(&journal, 1 << 20, 16);
        assert_eq!(chunks[0].offset, 16);
        assert_eq!(decode(&chunks[0].data_b64), b"-rest");
        fresh.remove(&attempt.attempt_id);
        assert!(!dir.path().join(attempt.attempt_id.as_str()).exists());
    }

    #[test]
    fn a_watch_finds_text_split_across_reads() {
        let found = Arc::new(AtomicBool::new(false));
        let mut watch = LogWatch::new("ready to accept", found.clone());
        watch.scan(b"starting\nready to ");
        assert!(!found.load(Ordering::SeqCst));
        watch.scan(b"accept connections\n");
        assert!(found.load(Ordering::SeqCst));
    }
}
