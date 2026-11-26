use crate::common::{Fixture, base64, exited, fixture, lost, proof, released, spec};
use bella_core::api::{LogPage, LogQuery};
use bella_core::ids::{JobId, MachineId};
use bella_core::job::Outcome;
use bella_core::protocol::{AttemptRef, LogChunk, LogStream};
use bella_ledger::{LedgerError, LogAppend, LogAppendStatus};

fn chunk(
    attempt: &AttemptRef,
    step: &str,
    stream: LogStream,
    offset: u64,
    data: &[u8],
) -> LogChunk {
    LogChunk {
        attempt: attempt.clone(),
        step: step.into(),
        stream,
        offset,
        data_b64: base64(data),
        truncated: false,
    }
}

fn out(attempt: &AttemptRef, offset: u64, data: &[u8]) -> LogChunk {
    chunk(attempt, "compile", LogStream::Stdout, offset, data)
}

fn running(f: &Fixture) -> (MachineId, JobId, AttemptRef) {
    let machine = f.machine("builder");
    let job = f.submit("app");
    let attempt = f.start(&job, &machine);
    (machine, job, attempt)
}

fn page(f: &Fixture, job: &JobId, query: LogQuery) -> LogPage {
    f.ledger.read_logs(job, &query).unwrap()
}

fn text(page: &LogPage) -> String {
    page.entries.iter().map(|e| e.text.as_str()).collect()
}

#[test]
fn chunks_append_in_order_and_duplicates_are_ignored() {
    let f = fixture();
    let (_, job, attempt) = running(&f);
    let appended = |through_offset| LogAppend {
        through_offset,
        status: LogAppendStatus::Appended,
    };
    let duplicate = LogAppend {
        through_offset: 11,
        status: LogAppendStatus::Duplicate,
    };
    assert_eq!(
        f.ledger.append_log(&out(&attempt, 0, b"hello ")).unwrap(),
        appended(6)
    );
    assert_eq!(
        f.ledger.append_log(&out(&attempt, 6, b"world")).unwrap(),
        appended(11)
    );

    // Retransmissions: an exact duplicate, an older chunk and an overlap.
    assert_eq!(
        f.ledger.append_log(&out(&attempt, 6, b"world")).unwrap(),
        duplicate
    );
    assert_eq!(
        f.ledger.append_log(&out(&attempt, 0, b"hello ")).unwrap(),
        duplicate
    );
    assert_eq!(
        f.ledger
            .append_log(&out(&attempt, 8, b"rld and more"))
            .unwrap(),
        duplicate
    );

    // A gap says where to resume, and stores nothing.
    let error = f
        .ledger
        .append_log(&out(&attempt, 20, b"lost"))
        .unwrap_err();
    assert!(matches!(
        error,
        LedgerError::LogGap {
            expected: 11,
            got: 20
        }
    ));
    assert_eq!(error.to_api_error().details["expected"], 11);

    // Each stream of each step has its own offsets.
    let stderr = chunk(&attempt, "compile", LogStream::Stderr, 0, b"warning\n");
    assert_eq!(f.ledger.append_log(&stderr).unwrap(), appended(8));
    let other_step = chunk(&attempt, "test", LogStream::Stdout, 0, b"ok\n");
    assert_eq!(f.ledger.append_log(&other_step).unwrap(), appended(3));

    let all = page(&f, &job, LogQuery::default());
    assert_eq!(text(&all), "hello worldwarning\nok\n");
    let entries: Vec<_> = all
        .entries
        .iter()
        .map(|e| (e.step.as_str(), e.stream, e.text.as_str()))
        .collect();
    assert_eq!(
        entries,
        [
            ("compile", LogStream::Stdout, "hello world"),
            ("compile", LogStream::Stderr, "warning\n"),
            ("test", LogStream::Stdout, "ok\n")
        ]
    );
    assert!(!all.complete && !all.truncated);
    assert_eq!(f.ledger.attempt(&attempt.attempt_id).unwrap().log_bytes, 22);
}

#[test]
fn malformed_and_misaddressed_chunks_are_refused() {
    let f = fixture();
    let (_, _, attempt) = running(&f);
    let mut bad = out(&attempt, 0, b"x");
    bad.data_b64 = "not base64!".into();
    assert!(matches!(
        f.ledger.append_log(&bad),
        Err(LedgerError::Invalid(_))
    ));
    let nameless = chunk(&attempt, "", LogStream::Stdout, 0, b"x");
    assert!(matches!(
        f.ledger.append_log(&nameless),
        Err(LedgerError::Invalid(_))
    ));
    let stale = AttemptRef {
        generation: 9,
        ..attempt.clone()
    };
    assert!(matches!(
        f.ledger.append_log(&out(&stale, 0, b"x")),
        Err(LedgerError::StaleGeneration { .. })
    ));
    assert!(matches!(
        f.ledger
            .append_log(&out(&crate::common::unknown_attempt(), 0, b"x")),
        Err(LedgerError::NotFound(_))
    ));
    assert!(matches!(
        f.ledger.read_logs(&JobId::generate(), &LogQuery::default()),
        Err(LedgerError::NotFound(_))
    ));
    assert_eq!(f.ledger.attempt(&attempt.attempt_id).unwrap().log_bytes, 0);
}

#[test]
fn pages_are_bounded_and_continue_from_the_cursor() {
    let f = fixture();
    let (_, job, attempt) = running(&f);
    let mut expected = String::new();
    let mut offset = 0;
    for line in 0..40 {
        let data = format!("line {line:03}\n");
        f.ledger
            .append_log(&out(&attempt, offset, data.as_bytes()))
            .unwrap();
        offset += data.len() as u64;
        expected.push_str(&data);
    }

    // 9-byte lines read 25 bytes at a time: pages cut chunks in the middle.
    let mut collected = String::new();
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let query = LogQuery {
            cursor: cursor.clone(),
            limit_bytes: Some(25),
            ..LogQuery::default()
        };
        let page = page(&f, &job, query);
        let bytes: usize = page.entries.iter().map(|e| e.text.len()).sum();
        assert!(bytes <= 25);
        if bytes == 0 {
            // At the end the cursor stays put, ready for more output.
            assert_eq!(Some(page.next_cursor.clone()), cursor);
            break;
        }
        collected.push_str(&text(&page));
        cursor = Some(page.next_cursor);
        pages += 1;
        assert!(pages < 100);
    }
    assert_eq!(collected, expected);
    assert_eq!(pages, 15, "360 bytes in pages of 25");

    // New output is picked up from the same cursor.
    f.ledger
        .append_log(&out(&attempt, offset, b"tail\n"))
        .unwrap();
    let more = page(
        &f,
        &job,
        LogQuery {
            cursor: cursor.clone(),
            ..LogQuery::default()
        },
    );
    assert_eq!(text(&more), "tail\n");

    assert!(matches!(
        f.ledger.read_logs(
            &job,
            &LogQuery {
                cursor: Some("garbage".into()),
                ..LogQuery::default()
            }
        ),
        Err(LedgerError::Invalid(_))
    ));
}

#[test]
fn tail_and_step_filters_select_the_output() {
    let f = fixture();
    let (_, job, attempt) = running(&f);
    f.ledger
        .append_log(&out(&attempt, 0, b"compiling\n"))
        .unwrap();
    f.ledger
        .append_log(&chunk(
            &attempt,
            "test",
            LogStream::Stdout,
            0,
            b"test one\n",
        ))
        .unwrap();
    f.ledger
        .append_log(&out(&attempt, 10, b"linking\n"))
        .unwrap();
    f.ledger
        .append_log(&chunk(
            &attempt,
            "test",
            LogStream::Stdout,
            9,
            b"test two\n",
        ))
        .unwrap();

    let only_tests = LogQuery {
        step: Some("test".into()),
        ..LogQuery::default()
    };
    assert_eq!(text(&page(&f, &job, only_tests)), "test one\ntest two\n");

    let tail = LogQuery {
        tail: Some(true),
        limit_bytes: Some(12),
        ..LogQuery::default()
    };
    let last = page(&f, &job, tail);
    assert_eq!(text(&last), "ng\ntest two\n");
    // The tail's cursor continues at the end.
    f.ledger.append_log(&out(&attempt, 18, b"done\n")).unwrap();
    let next = LogQuery {
        cursor: Some(last.next_cursor),
        ..LogQuery::default()
    };
    assert_eq!(text(&page(&f, &job, next)), "done\n");

    let tail_of_step = LogQuery {
        step: Some("compile".into()),
        tail: Some(true),
        limit_bytes: Some(5),
        ..LogQuery::default()
    };
    assert_eq!(text(&page(&f, &job, tail_of_step)), "done\n");
    let whole_tail = LogQuery {
        tail: Some(true),
        ..LogQuery::default()
    };
    assert_eq!(
        text(&page(&f, &job, whole_tail)),
        "compiling\ntest one\nlinking\ntest two\ndone\n"
    );
}

#[test]
fn the_log_allowance_truncates_with_a_marker() {
    let f = fixture();
    let machine = f.machine("builder");
    let mut small = spec();
    small.outputs.max_log_bytes = 10;
    let job = f.submit_spec("app", small);
    let attempt = f.start(&job, &machine);

    assert_eq!(
        f.ledger
            .append_log(&out(&attempt, 0, b"123456"))
            .unwrap()
            .status,
        LogAppendStatus::Appended
    );
    assert!(!page(&f, &job, LogQuery::default()).truncated);
    // Four of these six bytes fit; all six are acknowledged.
    assert_eq!(
        f.ledger.append_log(&out(&attempt, 6, b"789abc")).unwrap(),
        LogAppend {
            through_offset: 12,
            status: LogAppendStatus::Truncated
        }
    );
    // Later output is acknowledged and dropped, on every stream.
    assert_eq!(
        f.ledger.append_log(&out(&attempt, 12, b"more")).unwrap(),
        LogAppend {
            through_offset: 16,
            status: LogAppendStatus::Truncated
        }
    );
    let stderr = chunk(&attempt, "compile", LogStream::Stderr, 0, b"err");
    assert_eq!(
        f.ledger.append_log(&stderr).unwrap().status,
        LogAppendStatus::Truncated
    );

    let all = page(&f, &job, LogQuery::default());
    assert_eq!(text(&all), "123456789a");
    assert!(all.truncated);
    let record = f.ledger.attempt(&attempt.attempt_id).unwrap();
    assert_eq!(record.log_bytes, 10);
    assert!(record.log_truncated);
    let markers = f
        .job_event_kinds(&job)
        .iter()
        .filter(|kind| *kind == "attempt.log_truncated")
        .count();
    assert_eq!(markers, 1);
}

#[test]
fn a_producer_side_truncation_is_recorded() {
    let f = fixture();
    let (_, job, attempt) = running(&f);
    let mut last = out(&attempt, 0, b"[output truncated]\n");
    last.truncated = true;
    assert_eq!(
        f.ledger.append_log(&last).unwrap().status,
        LogAppendStatus::Appended
    );
    let all = page(&f, &job, LogQuery::default());
    assert!(all.truncated);
    assert_eq!(text(&all), "[output truncated]\n");
}

#[test]
fn a_log_is_complete_and_sealed_once_the_attempt_is_released() {
    let f = fixture();
    let (_, job, attempt) = running(&f);
    f.ledger
        .append_log(&out(&attempt, 0, b"building\n"))
        .unwrap();
    f.events(&attempt, 1, vec![exited(Outcome::Succeeded)]);
    // Cleanup output still arrives after the result is known.
    f.ledger
        .append_log(&out(&attempt, 9, b"cleaning\n"))
        .unwrap();
    let before = page(&f, &job, LogQuery::default());
    assert!(!before.complete);

    f.events(&attempt, 2, vec![released()]);
    let sealed = f
        .ledger
        .append_log(&out(&attempt, 18, b"too late\n"))
        .unwrap();
    assert_eq!(
        sealed,
        LogAppend {
            through_offset: 18,
            status: LogAppendStatus::Sealed
        }
    );
    let all = page(&f, &job, LogQuery::default());
    assert_eq!(text(&all), "building\ncleaning\n");
    assert!(all.complete);
    // A page that stops short of the end is not complete.
    let partial = page(
        &f,
        &job,
        LogQuery {
            limit_bytes: Some(5),
            ..LogQuery::default()
        },
    );
    assert!(!partial.complete);
    let rest = page(
        &f,
        &job,
        LogQuery {
            cursor: Some(partial.next_cursor.clone()),
            ..LogQuery::default()
        },
    );
    assert!(rest.complete);
    assert_eq!(
        format!("{}{}", text(&partial), text(&rest)),
        "building\ncleaning\n"
    );
}

#[test]
fn a_retried_job_keeps_the_output_of_every_attempt() {
    let f = fixture();
    let (machine, job, first) = running(&f);
    f.ledger
        .append_log(&out(&first, 0, b"first try\n"))
        .unwrap();
    f.events(&first, 1, vec![exited(lost())]);
    f.ledger.record_release(&first, &proof()).unwrap();
    // Queued for a retry: more output will come.
    assert!(!page(&f, &job, LogQuery::default()).complete);

    let second = f.start(&job, &machine);
    f.ledger
        .append_log(&out(&second, 0, b"second try\n"))
        .unwrap();
    f.finish(&second, Outcome::Succeeded);
    let all = page(&f, &job, LogQuery::default());
    assert_eq!(text(&all), "first try\nsecond try\n");
    assert!(all.complete);
}

#[test]
fn a_page_does_not_split_a_character() {
    let f = fixture();
    let (_, job, attempt) = running(&f);
    let message = "naïve café ☕ 😀 done";
    // Send it in 4-byte chunks, which cut through multi-byte characters.
    let mut offset = 0;
    for piece in message.as_bytes().chunks(4) {
        f.ledger.append_log(&out(&attempt, offset, piece)).unwrap();
        offset += piece.len() as u64;
    }
    // Any page that can hold a whole character ends between characters.
    for limit in 4..12 {
        let mut collected = String::new();
        let mut cursor = None;
        for _ in 0..100 {
            let page = page(
                &f,
                &job,
                LogQuery {
                    cursor: cursor.clone(),
                    limit_bytes: Some(limit),
                    ..LogQuery::default()
                },
            );
            if page.entries.is_empty() {
                break;
            }
            collected.push_str(&text(&page));
            cursor = Some(page.next_cursor);
        }
        assert_eq!(collected, message, "page limit {limit}");
    }
}

#[test]
fn log_bytes_live_in_files_next_to_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let clock = bella_core::ManualClock::at(crate::common::START);
    let open = || bella_ledger::Ledger::open(&path, std::sync::Arc::new(clock.clone())).unwrap();
    let f = crate::common::with_principals(open(), clock.clone());
    let (_, job, attempt) = running(&f);
    let binary = [0u8, 159, 146, 150, 255, b'\n'];
    f.ledger
        .append_log(&out(&attempt, 0, b"from a file\n"))
        .unwrap();
    f.ledger
        .append_log(&chunk(&attempt, "compile", LogStream::Stderr, 0, &binary))
        .unwrap();
    f.ledger
        .append_log(&out(&attempt, 12, b"second chunk\n"))
        .unwrap();

    let file = dir
        .path()
        .join("logs")
        .join(format!("{}.log", attempt.attempt_id));
    let mut stored = b"from a file\n".to_vec();
    stored.extend_from_slice(&binary);
    stored.extend_from_slice(b"second chunk\n");
    assert_eq!(std::fs::read(&file).unwrap(), stored);

    let read = |ledger: &bella_ledger::Ledger| {
        let all = ledger.read_logs(&job, &LogQuery::default()).unwrap();
        let entries: Vec<_> = all
            .entries
            .into_iter()
            .map(|e| (e.stream, e.text))
            .collect();
        entries
    };
    let expected = vec![
        (LogStream::Stdout, "from a file\n".to_string()),
        (
            LogStream::Stderr,
            String::from_utf8_lossy(&binary).into_owned(),
        ),
        (LogStream::Stdout, "second chunk\n".to_string()),
    ];
    assert_eq!(read(&f.ledger), expected);

    // The index and the bytes survive a restart.
    drop(f);
    let reopened = open();
    assert_eq!(read(&reopened), expected);
    let duplicate = reopened
        .append_log(&out(&attempt, 12, b"second chunk\n"))
        .unwrap();
    assert_eq!(duplicate.status, LogAppendStatus::Duplicate);
    assert_eq!(std::fs::read(&file).unwrap(), stored);
}
