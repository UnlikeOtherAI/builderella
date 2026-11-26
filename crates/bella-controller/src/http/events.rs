//! `GET /events`: the ledger's event log as server-sent events.
//!
//! Each event is written as
//!
//! ```text
//! id: <seq>
//! event: <kind>
//! data: <EventRecord JSON>
//! ```
//!
//! followed by a blank line. With `?after=<seq>` or a `last-event-id`
//! header the stream first replays everything after that sequence; without
//! either it carries only events recorded from now on. A comment line keeps
//! an idle stream open.

use super::ApiQuery;
use crate::auth::Viewer;
use crate::error::{ApiResult, Failure};
use crate::state::App;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use bella_core::api::EventRecord;
use bella_ledger::LedgerError;
use bytes::Bytes;
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc};

/// Events read from the ledger per query while replaying.
const REPLAY_PAGE: u32 = 500;
const KEEP_ALIVE: &[u8] = b": keep-alive\n\n";

#[derive(Debug, Default, Deserialize)]
pub(crate) struct EventsQuery {
    after: Option<u64>,
}

/// One event in the wire format. The JSON has no raw line breaks and event
/// kinds are fixed dotted names, so every field fits on its line.
fn frame(event: &EventRecord) -> Option<Bytes> {
    let data = serde_json::to_string(event).ok()?;
    let kind: String = event
        .kind
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    Some(Bytes::from(format!(
        "id: {}\nevent: {kind}\ndata: {data}\n\n",
        event.seq
    )))
}

type Out = mpsc::Sender<Result<Bytes, Infallible>>;

/// Send everything after `last` from the ledger. `Ok(false)` means the
/// client went away.
async fn replay(app: &App, out: &Out, last: &mut u64) -> Result<bool, LedgerError> {
    loop {
        let after = *last;
        let page = app
            .db(move |ledger| ledger.events_after(after, REPLAY_PAGE))
            .await?;
        if page.is_empty() {
            return Ok(true);
        }
        for event in page {
            *last = event.seq;
            if let Some(bytes) = frame(&event)
                && out.send(Ok(bytes)).await.is_err()
            {
                return Ok(false);
            }
        }
    }
}

async fn pump(
    app: App,
    out: Out,
    mut live: broadcast::Receiver<Arc<EventRecord>>,
    mut last: u64,
    replay_first: bool,
) {
    if replay_first {
        match replay(&app, &out, &mut last).await {
            Ok(true) => {}
            Ok(false) | Err(_) => return,
        }
    }
    let mut keep_alive = tokio::time::interval(app.tuning.sse_keepalive);
    keep_alive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keep_alive.reset();
    loop {
        tokio::select! {
            () = out.closed() => return,
            () = app.shutdown.cancelled() => return,
            _ = keep_alive.tick() => {
                if out.send(Ok(Bytes::from_static(KEEP_ALIVE))).await.is_err() {
                    return;
                }
            }
            received = live.recv() => match received {
                Ok(event) => {
                    if event.seq > last {
                        last = event.seq;
                        if let Some(bytes) = frame(&event)
                            && out.send(Ok(bytes)).await.is_err()
                        {
                            return;
                        }
                    }
                }
                // Fell too far behind the broadcast: catch up from the
                // ledger, which has every event.
                Err(RecvError::Lagged(_)) => match replay(&app, &out, &mut last).await {
                    Ok(true) => {}
                    Ok(false) | Err(_) => return,
                },
                Err(RecvError::Closed) => return,
            },
        }
    }
}

/// `GET /events`.
pub(crate) async fn stream(
    State(app): State<App>,
    _viewer: Viewer,
    ApiQuery(query): ApiQuery<EventsQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let resumed = match headers.get("last-event-id") {
        None => None,
        Some(value) => Some(
            value
                .to_str()
                .ok()
                .and_then(|text| text.trim().parse::<u64>().ok())
                .ok_or_else(|| Failure::bad_request("last-event-id must be an event sequence"))?,
        ),
    };
    // A reconnecting client repeats its original `after` and adds the last
    // id it saw; the later of the two is where it stopped.
    let after = match (query.after, resumed) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    // Subscribed before the starting point is read and before any replay,
    // so nothing recorded in between is missed.
    let live = app.events.subscribe();
    let start = match after {
        Some(after) => after,
        // Only events from now on: start at the newest one recorded.
        None => app.db(|ledger| ledger.last_event_seq()).await?,
    };

    let (sender, mut receiver) = mpsc::channel(256);
    tokio::spawn(pump(app, sender, live, start, after.is_some()));
    let body = Body::from_stream(futures_util::stream::poll_fn(move |cx| {
        receiver.poll_recv(cx)
    }));
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    // Proxies that buffer would hold events back.
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bella_core::UnixMillis;
    use bella_core::api::EntityKind;

    #[test]
    fn frames_follow_the_wire_format() {
        let event = EventRecord {
            seq: 42,
            at: UnixMillis(7),
            entity: EntityKind::Job,
            entity_id: "job_x".into(),
            kind: "job.queued".into(),
            job_id: None,
            data: serde_json::json!({ "note": "two\nlines" }),
        };
        let text = String::from_utf8(frame(&event).unwrap().to_vec()).unwrap();
        let mut lines = text.split('\n');
        assert_eq!(lines.next(), Some("id: 42"));
        assert_eq!(lines.next(), Some("event: job.queued"));
        let data = lines.next().unwrap().strip_prefix("data: ").unwrap();
        assert_eq!(serde_json::from_str::<EventRecord>(data).unwrap(), event);
        assert_eq!(lines.next(), Some(""));
        assert_eq!(lines.next(), Some(""));
        assert_eq!(lines.next(), None);
    }
}
