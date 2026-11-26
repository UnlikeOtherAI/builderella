//! The controller's event stream: `GET /events` as server-sent events.

use crate::client::{Call, Client};
use crate::error::ClientError;
use crate::retry::{exponential, jittered};
use bella_core::api::EventRecord;
use futures_util::Stream;
use reqwest::header::CONTENT_TYPE;
use reqwest::{Method, Response};
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

const EVENT_STREAM: &str = "text/event-stream";

/// Longest single line of the stream that is buffered.
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
/// Largest `data` payload of one event.
const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// One parsed server-sent event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SseEvent {
    /// The `id` field in effect for this event.
    pub(crate) id: Option<String>,
    /// The `event` field; empty when the event has none.
    pub(crate) event: String,
    /// The `data` lines joined with `\n`.
    pub(crate) data: String,
}

/// The stream broke the limits of the format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SseTooLarge(&'static str);

/// Incremental parser for `text/event-stream`.
///
/// Bytes arrive in arbitrary pieces; an event is produced when its
/// terminating blank line has been seen. Lines may end in LF, CR or CRLF,
/// and a CRLF may be split between two pieces. Comment lines (starting with
/// `:`) are dropped, which is how keep-alives disappear. Several `data`
/// lines form one payload joined by newlines. An event without any `data`
/// line is not dispatched.
#[derive(Debug, Default)]
pub(crate) struct SseParser {
    line: Vec<u8>,
    data: String,
    has_data: bool,
    event: String,
    last_id: Option<String>,
    /// The previous byte was CR, so a following LF belongs to it.
    after_cr: bool,
    /// Nothing has been parsed yet, so a byte-order mark may still come.
    at_start: bool,
}

impl SseParser {
    pub(crate) fn new() -> Self {
        Self {
            at_start: true,
            ..Self::default()
        }
    }

    /// Consumes `chunk` and appends every completed event to `out`. After
    /// an error the parser must be discarded with the connection.
    pub(crate) fn feed(
        &mut self,
        chunk: &[u8],
        out: &mut Vec<SseEvent>,
    ) -> Result<(), SseTooLarge> {
        for &byte in chunk {
            if self.after_cr {
                self.after_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => self.end_line(out)?,
                b'\r' => {
                    self.after_cr = true;
                    self.end_line(out)?;
                }
                _ => {
                    if self.line.len() >= MAX_LINE_BYTES {
                        return Err(SseTooLarge("a line of the event stream is too long"));
                    }
                    self.line.push(byte);
                }
            }
        }
        Ok(())
    }

    fn end_line(&mut self, out: &mut Vec<SseEvent>) -> Result<(), SseTooLarge> {
        let raw = std::mem::take(&mut self.line);
        let mut bytes = raw.as_slice();
        if self.at_start {
            self.at_start = false;
            bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
        }
        if bytes.is_empty() {
            self.dispatch(out);
            return Ok(());
        }
        if bytes[0] == b':' {
            return Ok(());
        }
        let (field, value) = match bytes.iter().position(|&b| b == b':') {
            Some(colon) => {
                let value = &bytes[colon + 1..];
                (&bytes[..colon], value.strip_prefix(b" ").unwrap_or(value))
            }
            None => (bytes, &[][..]),
        };
        match field {
            b"data" => {
                if self.data.len() + value.len() + 1 > MAX_EVENT_BYTES {
                    return Err(SseTooLarge("an event of the event stream is too large"));
                }
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(&String::from_utf8_lossy(value));
                self.has_data = true;
            }
            b"event" => self.event = String::from_utf8_lossy(value).into_owned(),
            // An id containing NUL is ignored, as the format requires.
            b"id" if !value.contains(&0) => {
                self.last_id = Some(String::from_utf8_lossy(value).into_owned());
            }
            // `retry` and unknown fields are ignored.
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        let event = std::mem::take(&mut self.event);
        let data = std::mem::take(&mut self.data);
        if std::mem::take(&mut self.has_data) {
            out.push(SseEvent {
                id: self.last_id.clone(),
                event,
                data,
            });
        }
    }
}

type Item = Result<EventRecord, ClientError>;

/// Events from the controller, in sequence order, across reconnects.
///
/// Returned by [`Client::events`]. The stream never ends by itself: drop it
/// to stop. See [`Client::events`] for what the error items mean.
pub struct EventStream {
    inner: Pin<Box<dyn Stream<Item = Item> + Send>>,
}

impl Stream for EventStream {
    type Item = Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl std::fmt::Debug for EventStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventStream").finish_non_exhaustive()
    }
}

impl EventStream {
    /// The next event or error. Never `None`.
    pub async fn next(&mut self) -> Option<Item> {
        futures_util::StreamExt::next(self).await
    }
}

struct Connection {
    response: Response,
    parser: SseParser,
}

struct State {
    client: Client,
    /// Sequence of the newest event handed out, or the caller's starting
    /// point. The next connection asks for everything after it.
    last_seq: Option<u64>,
    connection: Option<Connection>,
    ready: VecDeque<Item>,
    /// Wait before the next connection attempt.
    delay: Option<Duration>,
    /// Consecutive connections that ended without delivering an event.
    failures: u32,
}

impl State {
    fn schedule_reconnect(&mut self) {
        self.connection = None;
        let options = self.client.options();
        let step = exponential(
            options.reconnect_initial_backoff,
            options.reconnect_max_backoff,
            self.failures,
        );
        self.delay = Some(jittered(step));
        self.failures = self.failures.saturating_add(1);
    }

    async fn connect(&self) -> Result<Connection, ClientError> {
        #[derive(serde::Serialize)]
        struct After {
            #[serde(skip_serializing_if = "Option::is_none")]
            after: Option<u64>,
        }
        // The stream does its own reconnecting with its own backoff, so
        // the request-level retry is not layered on top of it.
        let mut call = Call::new(Method::GET, self.client.endpoint(&["events"])?)
            .accept(EVENT_STREAM)
            .query(&After {
                after: self.last_seq,
            })?;
        call.retry = false;
        let response = self
            .client
            .open_once(&call)
            .await
            .map_err(|failure| failure.error)?;
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type
            .trim_start()
            .to_ascii_lowercase()
            .starts_with(EVENT_STREAM)
        {
            return Err(ClientError::decode(format!(
                "the event stream answered with content type `{content_type}`"
            )));
        }
        Ok(Connection {
            response,
            parser: SseParser::new(),
        })
    }

    /// Reads from the connection until at least one item is ready or the
    /// connection is given up.
    async fn pump(&mut self) {
        let idle = self.client.options().stream_idle_timeout;
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        let chunk = match idle {
            Some(idle) => match tokio::time::timeout(idle, connection.response.chunk()).await {
                Ok(chunk) => chunk,
                Err(_) => {
                    self.ready.push_back(Err(ClientError::Timeout));
                    self.schedule_reconnect();
                    return;
                }
            },
            None => connection.response.chunk().await,
        };
        match chunk {
            Ok(Some(bytes)) => {
                let mut events = Vec::new();
                let parsed = connection.parser.feed(&bytes, &mut events);
                for event in events {
                    self.accept(event);
                }
                if let Err(SseTooLarge(reason)) = parsed {
                    self.ready.push_back(Err(ClientError::decode(reason)));
                    self.schedule_reconnect();
                }
            }
            // The controller closed the stream, for example to restart.
            // That is not an error; the next connection resumes it.
            Ok(None) => self.schedule_reconnect(),
            Err(error) => {
                self.ready.push_back(Err(ClientError::from_reqwest(error)));
                self.schedule_reconnect();
            }
        }
    }

    fn accept(&mut self, event: SseEvent) {
        match serde_json::from_str::<EventRecord>(&event.data) {
            Ok(record) => {
                // A replay may overlap what was already delivered.
                if self.last_seq.is_some_and(|last| record.seq <= last) {
                    return;
                }
                self.last_seq = Some(record.seq);
                self.failures = 0;
                self.ready.push_back(Ok(record));
            }
            Err(error) => self.ready.push_back(Err(ClientError::decode(format!(
                "event `{}` (id {}) is not a valid event record: {error}",
                event.event,
                event.id.as_deref().unwrap_or("none"),
            )))),
        }
    }

    async fn next(&mut self) -> Item {
        loop {
            if let Some(item) = self.ready.pop_front() {
                return item;
            }
            if self.connection.is_none() {
                if let Some(delay) = self.delay.take() {
                    tokio::time::sleep(delay).await;
                }
                match self.connect().await {
                    Ok(connection) => self.connection = Some(connection),
                    Err(error) => {
                        self.schedule_reconnect();
                        return Err(error);
                    }
                }
            }
            self.pump().await;
        }
    }
}

impl Client {
    /// `GET /events`: the controller's durable events as a stream.
    ///
    /// With `after`, the stream starts with every event after that
    /// sequence number; without it, with events from now on.
    ///
    /// The stream remembers the sequence of the last event it yielded.
    /// When the connection ends for any reason it waits (exponential
    /// backoff with jitter, see [`ClientOptions`](crate::ClientOptions))
    /// and reconnects asking for everything after that sequence, so events
    /// are yielded exactly once and in order. A stream started without
    /// `after` that loses its connection before the first event resumes
    /// from "now" again, because it has no sequence to resume from.
    ///
    /// Items:
    ///
    /// * `Ok(event)`: the next event.
    /// * `Err(_)`: a connection attempt failed, the connection broke, went
    ///   silent for longer than `stream_idle_timeout`, or one event could
    ///   not be decoded. The stream keeps going after every error; an error
    ///   that will not go away by itself (a pin mismatch, `Unauthorized`)
    ///   repeats at the backoff interval until the stream is dropped.
    ///
    /// A connection the controller closes cleanly is resumed without an
    /// error item. The stream ends only when it is dropped.
    pub fn events(&self, after: Option<u64>) -> EventStream {
        let state = State {
            client: self.clone(),
            last_seq: after,
            connection: None,
            ready: VecDeque::new(),
            delay: None,
            failures: 0,
        };
        let stream = futures_util::stream::unfold(state, |mut state| async move {
            let item = state.next().await;
            Some((item, state))
        });
        EventStream {
            inner: Box::pin(stream),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(pieces: &[&[u8]]) -> Vec<SseEvent> {
        let mut parser = SseParser::new();
        let mut out = Vec::new();
        for piece in pieces {
            parser.feed(piece, &mut out).unwrap();
        }
        out
    }

    fn event(id: Option<&str>, name: &str, data: &str) -> SseEvent {
        SseEvent {
            id: id.map(str::to_owned),
            event: name.to_owned(),
            data: data.to_owned(),
        }
    }

    #[test]
    fn a_complete_event_is_parsed() {
        let events = parse_all(&[b"id: 7\nevent: job.queued\ndata: {\"seq\":7}\n\n"]);
        assert_eq!(events, vec![event(Some("7"), "job.queued", "{\"seq\":7}")]);
    }

    #[test]
    fn events_split_at_every_byte_boundary_parse_the_same() {
        let stream = b"id: 1\nevent: a\ndata: one\n\n: keep-alive\n\nid: 2\r\nevent: b\r\ndata: two\r\ndata: lines\r\n\r\ndata:three\rid:3\r\r";
        let whole = parse_all(&[stream]);
        assert_eq!(
            whole,
            vec![
                event(Some("1"), "a", "one"),
                event(Some("2"), "b", "two\nlines"),
                event(Some("3"), "", "three"),
            ]
        );
        for split in 1..stream.len() {
            let (head, tail) = stream.split_at(split);
            assert_eq!(parse_all(&[head, tail]), whole, "split at {split}");
        }
        let bytes: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(parse_all(&bytes), whole);
    }

    #[test]
    fn comments_and_empty_events_produce_nothing() {
        assert!(parse_all(&[b": keep-alive\n\n: another\n\n\n\n"]).is_empty());
        // An event name without data is not an event.
        assert!(parse_all(&[b"event: orphan\n\n"]).is_empty());
        // The discarded name does not leak into the next event.
        assert_eq!(
            parse_all(&[b"event: orphan\n\ndata: x\n\n"]),
            vec![event(None, "", "x")]
        );
    }

    #[test]
    fn multi_line_and_empty_data_are_joined_with_newlines() {
        assert_eq!(
            parse_all(&[b"data: a\ndata:\ndata: b\n\n"]),
            vec![event(None, "", "a\n\nb")]
        );
        assert_eq!(parse_all(&[b"data\n\n"]), vec![event(None, "", "")]);
    }

    #[test]
    fn only_one_leading_space_is_removed_and_colons_stay_in_values() {
        assert_eq!(
            parse_all(&[b"data:  two spaces\ndata: a: b\n\n"]),
            vec![event(None, "", " two spaces\na: b")]
        );
    }

    #[test]
    fn the_id_persists_until_replaced_and_unknown_fields_are_ignored() {
        assert_eq!(
            parse_all(&[b"id: 5\ndata: a\n\nretry: 100\nfoo: bar\ndata: b\n\nid: 6\ndata: c\n\n"]),
            vec![
                event(Some("5"), "", "a"),
                event(Some("5"), "", "b"),
                event(Some("6"), "", "c"),
            ]
        );
    }

    #[test]
    fn a_byte_order_mark_and_split_utf8_are_handled() {
        let text = "data: zażółć 🚀\n\n".as_bytes();
        let mut input = b"\xEF\xBB\xBF".to_vec();
        input.extend_from_slice(text);
        let whole = parse_all(&[&input]);
        assert_eq!(whole, vec![event(None, "", "zażółć 🚀")]);
        for split in 1..input.len() {
            let (head, tail) = input.split_at(split);
            assert_eq!(parse_all(&[head, tail]), whole, "split at {split}");
        }
    }

    #[test]
    fn an_unterminated_event_is_held_back() {
        let mut parser = SseParser::new();
        let mut out = Vec::new();
        parser.feed(b"id: 1\ndata: partial\n", &mut out).unwrap();
        assert!(out.is_empty());
        parser.feed(b"\n", &mut out).unwrap();
        assert_eq!(out, vec![event(Some("1"), "", "partial")]);
    }

    #[test]
    fn oversized_lines_are_refused() {
        let mut parser = SseParser::new();
        let mut out = Vec::new();
        let chunk = vec![b'x'; 1024 * 1024];
        let mut result = Ok(());
        for _ in 0..=(MAX_LINE_BYTES / chunk.len()) {
            result = parser.feed(&chunk, &mut out);
            if result.is_err() {
                break;
            }
        }
        assert!(result.is_err());
        assert!(out.is_empty());
    }
}
