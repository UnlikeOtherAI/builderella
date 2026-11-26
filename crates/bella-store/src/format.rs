//! The stored-object format, version 1.
//!
//! The same bytes are used on disk and on the wire, so an object moves
//! between stores without being recompressed.
//!
//! ```text
//! offset  size  field
//!      0     4  magic, the ASCII bytes "BOBJ"
//!      4     1  format version, 1
//!      5     3  reserved, zero
//!      8     8  length of the uncompressed content, little-endian u64
//!     16     …  exactly one zstd frame holding the content, then end of stream
//! ```
//!
//! Rules a reader enforces:
//!
//! * magic, version and reserved bytes must match exactly;
//! * the frame must decode to exactly the recorded length, and nothing may
//!   follow it;
//! * the frame may not need a window larger than 2^[`WINDOW_LOG_MAX`] bytes
//!   and may not use a dictionary;
//! * the whole stored object may not be longer than
//!   [`max_stored_len`]`(length)`, which every zstd encoder stays below.
//!
//! The digest that names an object is the SHA-256 of the uncompressed
//! content. It is not stored in the object; a reader is told which digest it
//! expects and checks the content against it.

use bella_core::Digest;
use bella_core::ids::DigestWriter;
use bella_core::store::{StoreError, VerifyingReader};
use std::io::{self, Read, Seek, SeekFrom, Write};
use zstd::stream::raw::{DParameter, Decoder, InBuffer, Operation, OutBuffer};

/// First four bytes of every stored object.
pub const MAGIC: [u8; 4] = *b"BOBJ";
/// Format version written by this crate and the only one it reads.
pub const VERSION: u8 = 1;
/// Size of the fixed header in front of the zstd frame.
pub const HEADER_LEN: usize = 16;
/// zstd level used when this crate compresses.
pub const COMPRESSION_LEVEL: i32 = 3;
/// Largest zstd window a reader accepts, as a power of two (8 MiB). Level
/// [`COMPRESSION_LEVEL`] uses 2 MiB. The bound keeps the memory one reader
/// can be made to allocate independent of what the stream claims.
pub const WINDOW_LOG_MAX: u32 = 23;

const LEN_OFFSET: u64 = 8;
const CHUNK: usize = 64 * 1024;
const DECODE_INPUT: usize = 16 * 1024;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// The fixed header of a stored object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Length of the uncompressed content in bytes.
    pub len: u64,
}

impl Header {
    /// The 16 header bytes for this length.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[..4].copy_from_slice(&MAGIC);
        out[4] = VERSION;
        out[8..].copy_from_slice(&self.len.to_le_bytes());
        out
    }

    /// Parses a header. Any byte that is not what version 1 writes is
    /// `InvalidData`.
    pub fn parse(bytes: &[u8; HEADER_LEN]) -> io::Result<Self> {
        if bytes[..4] != MAGIC {
            return Err(invalid("not a stored object: bad magic"));
        }
        if bytes[4] != VERSION {
            return Err(invalid(format!(
                "unsupported stored-object version {}",
                bytes[4]
            )));
        }
        if bytes[5..8] != [0, 0, 0] {
            return Err(invalid("stored-object header has non-zero reserved bytes"));
        }
        let mut len = [0u8; 8];
        len.copy_from_slice(&bytes[8..]);
        Ok(Self {
            len: u64::from_le_bytes(len),
        })
    }

    /// Reads and parses a header. A stream that ends inside the header is
    /// `InvalidData`.
    pub fn read_from(reader: &mut (impl Read + ?Sized)) -> io::Result<Self> {
        let mut bytes = [0u8; HEADER_LEN];
        reader.read_exact(&mut bytes).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                invalid("stored object is shorter than its header")
            } else {
                e
            }
        })?;
        Self::parse(&bytes)
    }
}

/// Upper bound on the stored size (header included) of an object whose
/// content is `len` bytes. zstd's worst case is well inside it; a stream
/// that exceeds it is rejected instead of being stored.
pub fn max_stored_len(len: u64) -> u64 {
    (HEADER_LEN as u64 + 4096)
        .saturating_add(len)
        .saturating_add(len >> 7)
}

/// What [`encode`] produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Encoded {
    /// SHA-256 of the content that was read.
    pub digest: Digest,
    /// Length of that content.
    pub len: u64,
    /// Bytes written to the output, header included.
    pub stored_len: u64,
}

/// Fills `buf` from `reader`, stopping early only at end of stream.
fn read_full(reader: &mut (impl Read + ?Sized), buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Reads `reader` to its end and writes it to `out` as one stored object,
/// starting at `out`'s current position and leaving `out` positioned after
/// it.
///
/// The digest and length returned describe exactly the bytes that were
/// compressed. Content longer than `max_len` fails with
/// [`StoreError::TooLarge`]; on any error `out` holds an incomplete object
/// that must be discarded.
pub fn encode<W: Write + Seek>(
    reader: &mut dyn Read,
    out: &mut W,
    max_len: u64,
) -> Result<Encoded, StoreError> {
    let start = out.stream_position()?;
    // The length is not known yet; the header is rewritten once it is.
    out.write_all(&Header { len: 0 }.encode())?;

    let mut buf = vec![0u8; CHUNK];
    let mut hasher = DigestWriter::new();
    let mut filled = read_full(reader, &mut buf)?;
    if filled as u64 > max_len {
        return Err(StoreError::TooLarge { limit: max_len });
    }

    let mut encoder = zstd::stream::write::Encoder::new(&mut *out, COMPRESSION_LEVEL)?;
    if filled < CHUNK {
        // The whole content is in hand, so zstd can size the frame to it and
        // a reader allocates no more than the content needs.
        encoder.set_pledged_src_size(Some(filled as u64))?;
    }
    loop {
        hasher.update(&buf[..filled]);
        encoder.write_all(&buf[..filled])?;
        if filled < CHUNK {
            break;
        }
        filled = read_full(reader, &mut buf)?;
        if hasher.len().saturating_add(filled as u64) > max_len {
            return Err(StoreError::TooLarge { limit: max_len });
        }
    }
    encoder.finish()?;

    let (digest, len) = hasher.finish();
    let end = out.stream_position()?;
    out.seek(SeekFrom::Start(start + LEN_OFFSET))?;
    out.write_all(&len.to_le_bytes())?;
    out.seek(SeekFrom::Start(end))?;
    Ok(Encoded {
        digest,
        len,
        stored_len: end - start,
    })
}

/// Reads a stored object from `reader` and returns its content reader and
/// recorded length.
///
/// The returned reader yields exactly the recorded number of bytes. It fails
/// with `InvalidData` if the stream is damaged, truncated, longer than
/// recorded, followed by other data, or if the content does not hash to
/// `digest`. The digest check happens when the end of the content is
/// reached, so a caller must read to end of stream before trusting what it
/// received.
pub fn decode<R: Read>(
    mut reader: R,
    digest: &Digest,
) -> io::Result<(VerifyingReader<BodyReader<R>>, u64)> {
    let header = Header::read_from(&mut reader)?;
    let body = BodyReader::new(reader, header.len)?;
    Ok((VerifyingReader::new(body, digest.clone()), header.len))
}

/// Decodes the zstd frame that follows a [`Header`].
///
/// Yields exactly `len` bytes and then checks that the frame and the
/// underlying stream both end there. Every violation is `InvalidData`, and
/// once an error has been returned every later read fails too.
pub struct BodyReader<R> {
    src: R,
    decoder: Decoder<'static>,
    input: Box<[u8]>,
    pos: usize,
    filled: usize,
    remaining: u64,
    frame_done: bool,
    finished: bool,
    failed: bool,
}

impl<R: Read> BodyReader<R> {
    /// `src` must be positioned at the first byte after the header, and
    /// `len` is the length that header recorded.
    pub fn new(src: R, len: u64) -> io::Result<Self> {
        let mut decoder = Decoder::new()?;
        decoder.set_parameter(DParameter::WindowLogMax(WINDOW_LOG_MAX))?;
        Ok(Self {
            src,
            decoder,
            input: vec![0u8; DECODE_INPUT].into_boxed_slice(),
            pos: 0,
            filled: 0,
            remaining: len,
            frame_done: false,
            finished: false,
            failed: false,
        })
    }

    fn fill(&mut self) -> io::Result<usize> {
        loop {
            match self.src.read(&mut self.input) {
                Ok(n) => {
                    self.pos = 0;
                    self.filled = n;
                    return Ok(n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn read_inner(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.frame_done {
                if self.remaining != 0 {
                    return Err(invalid("object is shorter than its recorded length"));
                }
                if self.pos < self.filled || self.fill()? != 0 {
                    return Err(invalid("unexpected data after the end of the object"));
                }
                self.finished = true;
                return Ok(0);
            }
            if self.pos == self.filled && self.fill()? == 0 {
                return Err(invalid("object is truncated"));
            }

            // Once the recorded length has been delivered the frame may
            // still have to consume its last bytes, but it must not produce
            // anything more: the probe byte catches a frame that would.
            let mut probe = [0u8; 1];
            let within_len = self.remaining > 0;
            let target: &mut [u8] = if within_len {
                let cap = usize::try_from(self.remaining)
                    .unwrap_or(usize::MAX)
                    .min(buf.len());
                &mut buf[..cap]
            } else {
                &mut probe
            };
            let mut input = InBuffer::around(&self.input[self.pos..self.filled]);
            let mut output = OutBuffer::around(target);
            let hint = self
                .decoder
                .run(&mut input, &mut output)
                .map_err(|e| invalid(format!("object is damaged: {e}")))?;
            let consumed = input.pos();
            let produced = output.pos();
            self.pos += consumed;
            if hint == 0 {
                self.frame_done = true;
            }
            if produced > 0 {
                if !within_len {
                    return Err(invalid("object is longer than its recorded length"));
                }
                self.remaining -= produced as u64;
                return Ok(produced);
            }
            if consumed == 0 && !self.frame_done {
                return Err(invalid("object is damaged: decoder made no progress"));
            }
        }
    }
}

impl<R: Read> Read for BodyReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.failed {
            return Err(invalid("object failed verification"));
        }
        if buf.is_empty() || self.finished {
            return Ok(0);
        }
        let result = self.read_inner(buf);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn encode_bytes(bytes: &[u8]) -> (Vec<u8>, Encoded) {
        let mut out = Cursor::new(Vec::new());
        let mut reader = bytes;
        let encoded = encode(&mut reader, &mut out, u64::MAX).unwrap();
        (out.into_inner(), encoded)
    }

    fn decode_bytes(stored: &[u8], digest: &Digest) -> io::Result<Vec<u8>> {
        let (mut reader, _) = decode(stored, digest)?;
        let mut out = Vec::new();
        reader.read_to_end(&mut out)?;
        Ok(out)
    }

    #[test]
    fn header_round_trips_and_rejects_foreign_bytes() {
        let header = Header {
            len: 0x0102_0304_0506_0708,
        };
        let bytes = header.encode();
        assert_eq!(&bytes[..4], b"BOBJ");
        assert_eq!(bytes[4], 1);
        assert_eq!(Header::parse(&bytes).unwrap(), header);

        for index in 0..8 {
            let mut bad = bytes;
            bad[index] ^= 0x01;
            let err = Header::parse(&bad).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "byte {index}");
        }
        let err = Header::read_from(&mut &bytes[..10]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn encode_then_decode_round_trips_across_chunk_boundaries() {
        for len in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK + 17] {
            let content: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
            let (stored, encoded) = encode_bytes(&content);
            assert_eq!(encoded.digest, Digest::of_bytes(&content));
            assert_eq!(encoded.len, len as u64);
            assert_eq!(encoded.stored_len, stored.len() as u64);
            assert!(encoded.stored_len <= max_stored_len(encoded.len));
            let (_, recorded) = decode(&stored[..], &encoded.digest).unwrap();
            assert_eq!(recorded, len as u64);
            assert_eq!(decode_bytes(&stored, &encoded.digest).unwrap(), content);
        }
    }

    #[test]
    fn encode_writes_at_the_current_position_and_enforces_the_limit() {
        let mut out = Cursor::new(b"prefix".to_vec());
        out.seek(SeekFrom::End(0)).unwrap();
        let mut reader: &[u8] = b"content";
        let encoded = encode(&mut reader, &mut out, 7).unwrap();
        let bytes = out.into_inner();
        assert_eq!(&bytes[..6], b"prefix");
        assert_eq!(bytes.len() as u64, 6 + encoded.stored_len);
        assert_eq!(
            decode_bytes(&bytes[6..], &encoded.digest).unwrap(),
            b"content"
        );

        let mut out = Cursor::new(Vec::new());
        let mut reader: &[u8] = b"content!";
        assert!(matches!(
            encode(&mut reader, &mut out, 7),
            Err(StoreError::TooLarge { limit: 7 })
        ));
        let big = vec![7u8; 2 * CHUNK + 1];
        let mut out = Cursor::new(Vec::new());
        assert!(matches!(
            encode(&mut &big[..], &mut out, 2 * CHUNK as u64),
            Err(StoreError::TooLarge { .. })
        ));
    }

    #[test]
    fn decode_rejects_wrong_digest_wrong_length_and_trailing_data() {
        let content = b"some content that is long enough to matter".repeat(20);
        let (stored, encoded) = encode_bytes(&content);

        let other = Digest::of_bytes(b"other");
        let err = decode_bytes(&stored, &other).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        for claimed in [encoded.len - 1, encoded.len + 1, 0] {
            let mut bad = stored.clone();
            bad[8..16].copy_from_slice(&claimed.to_le_bytes());
            let err = decode_bytes(&bad, &encoded.digest).unwrap_err();
            assert_eq!(
                err.kind(),
                io::ErrorKind::InvalidData,
                "claimed length {claimed}"
            );
        }

        let mut trailing = stored.clone();
        trailing.push(0);
        let err = decode_bytes(&trailing, &encoded.digest).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        // A second, perfectly valid frame is still data after the object.
        let (second, _) = encode_bytes(b"more");
        let mut two_frames = stored.clone();
        two_frames.extend_from_slice(&second[HEADER_LEN..]);
        let err = decode_bytes(&two_frames, &encoded.digest).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        for cut in [0, 7, HEADER_LEN, HEADER_LEN + 3, stored.len() - 1] {
            let err = decode_bytes(&stored[..cut], &encoded.digest).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "cut at {cut}");
        }
    }

    #[test]
    fn a_failed_reader_stays_failed() {
        let (mut stored, encoded) = encode_bytes(b"abc");
        stored.push(1);
        let (mut reader, _) = decode(&stored[..], &encoded.digest).unwrap();
        let mut out = Vec::new();
        assert!(reader.read_to_end(&mut out).is_err());
        let mut buf = [0u8; 8];
        assert!(reader.read(&mut buf).is_err());
    }

    #[test]
    fn an_oversized_window_is_refused() {
        let content = vec![0u8; 1024];
        let mut frame = Vec::new();
        let mut encoder = zstd::stream::write::Encoder::new(&mut frame, 1).unwrap();
        encoder.window_log(WINDOW_LOG_MAX + 1).unwrap();
        encoder.write_all(&content).unwrap();
        encoder.finish().unwrap();
        let mut stored = Header {
            len: content.len() as u64,
        }
        .encode()
        .to_vec();
        stored.extend_from_slice(&frame);
        let err = decode_bytes(&stored, &Digest::of_bytes(&content)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
