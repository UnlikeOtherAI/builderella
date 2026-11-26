//! Attempt logs: idempotent append by stream offset and bounded reads in
//! arrival order.
//!
//! SQLite holds the index. The bytes live in one append-only file per
//! attempt in the log directory, or inline in the index for an in-memory
//! ledger. A chunk's bytes are written and synced before the transaction
//! that indexes them commits, so an indexed chunk is always readable; a
//! crash in between leaves only unreferenced bytes at the end of a file.

use crate::db::{
    Ctx, all_rows, count, enum_text, enumeration, exec, flag, int, opt_row, opt_uint, text, uint,
};
use crate::events::emit;
use crate::{
    AttemptRecord, Ledger, LedgerError, LogAppend, LogAppendStatus, Result, attempts, jobs,
};
use bella_core::api::{EntityKind, LogEntry, LogPage, LogQuery};
use bella_core::ids::JobId;
use bella_core::job::JobPhase;
use bella_core::protocol::{LogChunk, LogStream};
use rusqlite::params;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const DEFAULT_PAGE_BYTES: u64 = 64 * 1024;
const MAX_PAGE_BYTES: u64 = 1024 * 1024;
/// Index rows fetched per query while assembling a page.
const INDEX_BATCH: i64 = 64;

/// Decode standard-alphabet base64, with or without `=` padding.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let body = bytes
        .strip_suffix(b"==")
        .or_else(|| bytes.strip_suffix(b"="))
        .unwrap_or(bytes);
    if body.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() / 4 * 3 + 2);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for &byte in body {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    // Bits left over must be zero padding.
    (buffer == 0).then_some(out)
}

/// Length of an unfinished UTF-8 sequence at the end of `bytes`, if any.
fn incomplete_utf8_suffix(bytes: &[u8]) -> usize {
    let len = bytes.len();
    for back in 1..=len.min(3) {
        let byte = bytes[len - back];
        if byte & 0b1100_0000 == 0b1000_0000 {
            continue;
        }
        let needed = match byte {
            0b1111_0000.. => 4,
            0b1110_0000.. => 3,
            0b1100_0000.. => 2,
            _ => 1,
        };
        return if needed > back { back } else { 0 };
    }
    0
}

fn log_file(dir: &Path, attempt: &str) -> std::path::PathBuf {
    dir.join(format!("{attempt}.log"))
}

/// Make a newly created entry of `dir` durable. Where a directory cannot
/// be opened for syncing (Windows), the file system's own metadata
/// journaling is what makes the entry durable.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Append bytes to an attempt's log file, durably, and return where they
/// start. Only called with the ledger lock held, so there is one writer.
fn append_to_file(dir: &Path, attempt: &str, bytes: &[u8]) -> std::io::Result<u64> {
    if !dir.is_dir() {
        fs::create_dir_all(dir)?;
        if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
            sync_dir(parent)?;
        }
    }
    let path = log_file(dir, attempt);
    let is_new = !path.exists();
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let position = file.metadata()?.len();
    file.write_all(bytes)?;
    file.sync_data()?;
    if is_new {
        // The index must never point at a file whose name a crash could lose.
        sync_dir(dir)?;
    }
    Ok(position)
}

fn store(
    c: &Ctx<'_>,
    attempt: &AttemptRecord,
    step: &str,
    stream: &str,
    stream_offset: u64,
    bytes: &[u8],
) -> Result<()> {
    let (file_offset, inline) = match c.log_dir {
        Some(dir) => {
            let position = append_to_file(dir, attempt.id.as_str(), bytes)?;
            (Some(int(position)?), None)
        }
        None => (None, Some(bytes)),
    };
    exec(
        c.conn,
        "INSERT INTO log_chunks
             (job_id, attempt_id, step, stream, stream_offset, len, file_offset, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            attempt.job_id.as_str(),
            attempt.id.as_str(),
            step,
            stream,
            int(stream_offset)?,
            int(bytes.len() as u64)?,
            file_offset,
            inline
        ],
    )?;
    Ok(())
}

fn append(c: &Ctx<'_>, chunk: &LogChunk) -> Result<LogAppend> {
    let attempt = attempts::load(c, &chunk.attempt.attempt_id)?;
    if chunk.attempt.generation != attempt.generation {
        return Err(LedgerError::StaleGeneration {
            attempt: attempt.id,
            got: chunk.attempt.generation,
            current: attempt.generation,
        });
    }
    if chunk.step.is_empty() {
        return Err(LedgerError::Invalid("a log chunk needs a step name".into()));
    }
    let bytes = decode_base64(&chunk.data_b64)
        .ok_or_else(|| LedgerError::Invalid("log chunk data is not valid base64".into()))?;
    let stream = enum_text(&chunk.stream)?;
    let (next_offset, stream_truncated) = opt_row(
        c.conn,
        "SELECT next_offset, truncated FROM log_streams
         WHERE attempt_id = ?1 AND step = ?2 AND stream = ?3",
        params![attempt.id.as_str(), chunk.step, stream],
        |row| Ok((uint(row, 0)?, flag(row, 1)?)),
    )?
    .unwrap_or((0, false));

    if !attempt.held {
        return Ok(LogAppend {
            through_offset: next_offset,
            status: LogAppendStatus::Sealed,
        });
    }
    if chunk.offset > next_offset {
        return Err(LedgerError::LogGap {
            expected: next_offset,
            got: chunk.offset,
        });
    }
    if chunk.offset < next_offset {
        return Ok(LogAppend {
            through_offset: next_offset,
            status: LogAppendStatus::Duplicate,
        });
    }

    let len = bytes.len() as u64;
    let end = next_offset
        .checked_add(len)
        .ok_or_else(|| LedgerError::Invalid("log offset overflow".into()))?;
    let job = jobs::load_row(c, &attempt.job_id)?;
    let allowance = job.max_log_bytes.saturating_sub(attempt.log_bytes);
    let keep = len.min(allowance);
    if keep > 0 {
        // `keep <= len`, the length of a slice, so it fits in `usize`.
        store(
            c,
            &attempt,
            &chunk.step,
            &stream,
            next_offset,
            &bytes[..keep as usize],
        )?;
        exec(
            c.conn,
            "UPDATE attempts SET log_bytes = log_bytes + ?1 WHERE id = ?2",
            params![int(keep)?, attempt.id.as_str()],
        )?;
    }
    let over_allowance = keep < len;
    if (over_allowance || chunk.truncated) && !attempt.log_truncated {
        exec(
            c.conn,
            "UPDATE attempts SET log_truncated = 1 WHERE id = ?1",
            params![attempt.id.as_str()],
        )?;
        emit(
            c,
            EntityKind::Attempt,
            attempt.id.as_str(),
            Some(&attempt.job_id),
            "attempt.log_truncated",
            json!({
                "step": chunk.step,
                "stream": chunk.stream,
                "max_log_bytes": job.max_log_bytes,
                "by_producer": chunk.truncated,
            }),
        )?;
    }
    exec(
        c.conn,
        "INSERT INTO log_streams (attempt_id, step, stream, next_offset, truncated)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (attempt_id, step, stream)
         DO UPDATE SET next_offset = excluded.next_offset, truncated = excluded.truncated",
        params![
            attempt.id.as_str(),
            chunk.step,
            stream,
            int(end)?,
            stream_truncated || chunk.truncated || over_allowance
        ],
    )?;
    Ok(LogAppend {
        through_offset: end,
        status: if over_allowance {
            LogAppendStatus::Truncated
        } else {
            LogAppendStatus::Appended
        },
    })
}

/// Where a page starts: the first index row to read, and how many of its
/// bytes earlier pages already returned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Cursor {
    chunk: u64,
    skip: u64,
}

impl Cursor {
    fn parse(text: &str) -> Result<Self> {
        let parsed = text
            .split_once('.')
            .and_then(|(chunk, skip)| Some((chunk.parse().ok()?, skip.parse().ok()?)));
        match parsed {
            Some((chunk, skip)) => Ok(Self { chunk, skip }),
            None => Err(LedgerError::Invalid(format!(
                "malformed log cursor `{text}`"
            ))),
        }
    }

    fn encode(self) -> String {
        format!("{}.{}", self.chunk, self.skip)
    }
}

struct IndexRow {
    id: u64,
    attempt_id: String,
    step: String,
    stream: LogStream,
    len: u64,
    file_offset: Option<u64>,
    data: Option<Vec<u8>>,
}

fn index_rows(c: &Ctx<'_>, job: &JobId, step: Option<&str>, from: u64) -> Result<Vec<IndexRow>> {
    all_rows(
        c.conn,
        "SELECT id, attempt_id, step, stream, len, file_offset, data FROM log_chunks
         WHERE job_id = ?1 AND id >= ?2 AND (?3 IS NULL OR step = ?3)
         ORDER BY id LIMIT ?4",
        params![job.as_str(), int(from)?, step, INDEX_BATCH],
        |row| {
            Ok(IndexRow {
                id: uint(row, "id")?,
                attempt_id: text(row, "attempt_id")?,
                step: text(row, "step")?,
                stream: enumeration(row, "stream")?,
                len: uint(row, "len")?,
                file_offset: opt_uint(row, "file_offset")?,
                data: row.get::<_, Option<Vec<u8>>>("data")?,
            })
        },
    )
}

/// The cursor that leaves about `limit` bytes before the end of the log.
fn tail_start(c: &Ctx<'_>, job: &JobId, step: Option<&str>, limit: u64) -> Result<Cursor> {
    let mut stmt = c.conn.prepare_cached(
        "SELECT id, len FROM log_chunks
         WHERE job_id = ?1 AND (?2 IS NULL OR step = ?2) ORDER BY id DESC",
    )?;
    let mut rows = stmt.query(params![job.as_str(), step])?;
    let mut start = Cursor::default();
    let mut total = 0u64;
    while let Some(row) = rows.next()? {
        let (chunk, len) = (uint(row, 0)?, uint(row, 1)?);
        total = total.saturating_add(len);
        start = Cursor {
            chunk,
            skip: total.saturating_sub(limit),
        };
        if total >= limit {
            break;
        }
    }
    Ok(start)
}

fn read_slice(
    c: &Ctx<'_>,
    files: &mut BTreeMap<String, File>,
    row: &IndexRow,
    skip: u64,
    take: u64,
) -> Result<Vec<u8>> {
    let corrupt = || LedgerError::Corrupt(format!("log chunk {} has no readable bytes", row.id));
    let take = usize::try_from(take).map_err(|_| corrupt())?;
    if let Some(data) = &row.data {
        let start = usize::try_from(skip).map_err(|_| corrupt())?;
        return data
            .get(start..start + take)
            .map(<[u8]>::to_vec)
            .ok_or_else(corrupt);
    }
    let (Some(dir), Some(offset)) = (c.log_dir, row.file_offset) else {
        return Err(corrupt());
    };
    if !files.contains_key(&row.attempt_id) {
        let file = File::open(log_file(dir, &row.attempt_id))?;
        files.insert(row.attempt_id.clone(), file);
    }
    let file = files.get_mut(&row.attempt_id).ok_or_else(corrupt)?;
    file.seek(SeekFrom::Start(offset + skip))?;
    let mut buffer = vec![0u8; take];
    file.read_exact(&mut buffer)?;
    Ok(buffer)
}

fn read(c: &Ctx<'_>, job: &JobId, query: &LogQuery) -> Result<LogPage> {
    let job_row = jobs::load_row(c, job)?;
    let limit = query
        .limit_bytes
        .unwrap_or(DEFAULT_PAGE_BYTES)
        .clamp(1, MAX_PAGE_BYTES);
    let step = query.step.as_deref();
    let mut cursor = match (&query.cursor, query.tail) {
        (Some(text), _) => Cursor::parse(text)?,
        (None, Some(true)) => tail_start(c, job, step, limit)?,
        (None, _) => Cursor::default(),
    };

    let mut entries: Vec<(String, LogStream, Vec<u8>)> = Vec::new();
    // The slices that make up the last entry: index row, offset within it
    // and length. Used to move the cursor back over an unfinished character.
    let mut last_entry_slices: Vec<(u64, u64, u64)> = Vec::new();
    let mut files = BTreeMap::new();
    let mut produced = 0u64;
    let mut reached_end = false;
    'page: loop {
        let rows = index_rows(c, job, step, cursor.chunk)?;
        if rows.is_empty() {
            reached_end = true;
            break;
        }
        for row in &rows {
            let skip = if row.id == cursor.chunk {
                cursor.skip.min(row.len)
            } else {
                0
            };
            let available = row.len - skip;
            let take = available.min(limit - produced);
            if take > 0 {
                let bytes = read_slice(c, &mut files, row, skip, take)?;
                match entries.last_mut() {
                    Some((last_step, last_stream, buffer))
                        if *last_step == row.step && *last_stream == row.stream =>
                    {
                        buffer.extend_from_slice(&bytes);
                    }
                    _ => {
                        entries.push((row.step.clone(), row.stream, bytes));
                        last_entry_slices.clear();
                    }
                }
                last_entry_slices.push((row.id, skip, take));
                produced += take;
            }
            cursor = if take == available {
                Cursor {
                    chunk: row.id + 1,
                    skip: 0,
                }
            } else {
                Cursor {
                    chunk: row.id,
                    skip: skip + take,
                }
            };
            if produced >= limit {
                break 'page;
            }
        }
    }
    let more =
        !reached_end && (cursor.skip > 0 || !index_rows(c, job, step, cursor.chunk)?.is_empty());
    let complete = !more && job_row.phase == JobPhase::Finished;

    // Do not cut a multi-byte character in half at the end of a page while
    // the rest of it may still follow: give those bytes back and let the
    // next page start with them. A page always keeps at least one byte.
    if !complete && let Some((_, _, buffer)) = entries.last_mut() {
        let dangling = incomplete_utf8_suffix(buffer);
        if dangling > 0 && (dangling as u64) < produced {
            buffer.truncate(buffer.len() - dangling);
            let mut to_return = dangling as u64;
            while let Some((chunk, skip, take)) = last_entry_slices.pop() {
                if take > to_return {
                    cursor = Cursor {
                        chunk,
                        skip: skip + take - to_return,
                    };
                    break;
                }
                cursor = Cursor { chunk, skip };
                to_return -= take;
                if to_return == 0 {
                    break;
                }
            }
        }
    }
    entries.retain(|(_, _, buffer)| !buffer.is_empty());

    let truncated = count(
        c.conn,
        "SELECT COUNT(*) FROM attempts WHERE job_id = ?1 AND log_truncated = 1",
        params![job.as_str()],
    )? > 0;
    Ok(LogPage {
        entries: entries
            .into_iter()
            .map(|(step, stream, bytes)| LogEntry {
                step,
                stream,
                text: String::from_utf8_lossy(&bytes).into_owned(),
            })
            .collect(),
        next_cursor: cursor.encode(),
        truncated,
        complete,
    })
}

impl Ledger {
    /// Store a slice of one log stream. Idempotent by
    /// `(attempt, step, stream, offset)`:
    ///
    /// * a chunk that starts at the stream's next expected offset is stored;
    /// * a chunk that starts before it, duplicate or overlapping, is
    ///   ignored and `through_offset` says where to continue;
    /// * a chunk that starts after it is [`LedgerError::LogGap`] carrying
    ///   the expected offset.
    ///
    /// At most the job's `outputs.max_log_bytes` are kept per attempt.
    /// Bytes beyond that are dropped but acknowledged, and the truncation
    /// is recorded. Once the attempt's reservation is released its log is
    /// sealed: later chunks are dropped (`Sealed`), so a completed log
    /// never grows. Stored bytes are durable when this returns.
    pub fn append_log(&self, chunk: &LogChunk) -> Result<LogAppend> {
        self.write(|c| append(c, chunk))
    }

    /// A bounded page of a job's log output, across its attempts, in the
    /// order the ledger received it.
    ///
    /// The page holds at most `limit_bytes` (default 64 KiB, capped at
    /// 1 MiB). `next_cursor` continues after it and is always present, so a
    /// reader can poll. `complete` is set once the job has finished and the
    /// page reaches the end: no further output will arrive. `truncated`
    /// says an attempt exceeded its log allowance. Text is lossy UTF-8;
    /// treat it as untrusted.
    pub fn read_logs(&self, job: &JobId, query: &LogQuery) -> Result<LogPage> {
        self.read(|c| read(c, job, query))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_known_vectors() {
        assert_eq!(decode_base64("").unwrap(), b"");
        assert_eq!(decode_base64("Zg==").unwrap(), b"f");
        assert_eq!(decode_base64("Zm8=").unwrap(), b"fo");
        assert_eq!(decode_base64("Zm9v").unwrap(), b"foo");
        assert_eq!(decode_base64("Zm9vYg").unwrap(), b"foob");
        assert_eq!(decode_base64("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(decode_base64("/+8=").unwrap(), [0xff, 0xef]);
        assert_eq!(decode_base64("AAECAwQF").unwrap(), [0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn base64_rejects_garbage() {
        assert!(decode_base64("Zm9v!").is_none());
        assert!(decode_base64("Z").is_none());
        assert!(decode_base64("Zh==").is_none(), "non-zero padding bits");
        assert!(decode_base64("Zm 9v").is_none());
    }

    #[test]
    fn finds_unfinished_utf8_at_the_end() {
        assert_eq!(incomplete_utf8_suffix(b"abc"), 0);
        assert_eq!(incomplete_utf8_suffix("é".as_bytes()), 0);
        assert_eq!(incomplete_utf8_suffix(&"é".as_bytes()[..1]), 1);
        assert_eq!(incomplete_utf8_suffix(&"€".as_bytes()[..2]), 2);
        assert_eq!(incomplete_utf8_suffix(&"😀".as_bytes()[..3]), 3);
        assert_eq!(incomplete_utf8_suffix("😀".as_bytes()), 0);
        assert_eq!(incomplete_utf8_suffix(&[0x80, 0x80, 0x80, 0x80]), 0);
        assert_eq!(incomplete_utf8_suffix(b""), 0);
    }

    #[test]
    fn cursors_round_trip() {
        let cursor = Cursor { chunk: 42, skip: 7 };
        assert_eq!(Cursor::parse(&cursor.encode()).unwrap(), cursor);
        assert!(Cursor::parse("nope").is_err());
        assert!(Cursor::parse("1.x").is_err());
    }
}
