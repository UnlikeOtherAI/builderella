-- 0001: controller schema.
--
-- Structured values are JSON text. Columns that are filtered, joined or
-- used to enforce an invariant are real columns.

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

-- Every durable state transition, in commit order.
CREATE TABLE events (
    seq       INTEGER PRIMARY KEY AUTOINCREMENT,
    at        INTEGER NOT NULL,
    entity    TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    job_id    TEXT,
    kind      TEXT NOT NULL,
    data      TEXT NOT NULL
);
CREATE INDEX events_by_job ON events (job_id, seq) WHERE job_id IS NOT NULL;

CREATE TABLE principals (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    role       TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
) WITHOUT ROWID;

CREATE TABLE pairing_codes (
    code_hash   TEXT PRIMARY KEY,
    profile     TEXT NOT NULL,
    created_by  TEXT REFERENCES principals (id),
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    consumed_at INTEGER
) WITHOUT ROWID;

CREATE TABLE machines (
    id                    TEXT PRIMARY KEY,
    name                  TEXT NOT NULL,
    -- NULL once the machine is retired or revoked.
    token_hash            TEXT UNIQUE,
    lifecycle             TEXT NOT NULL,
    inventory             TEXT NOT NULL,
    policy                TEXT NOT NULL,
    boot_id               TEXT,
    connected             INTEGER NOT NULL DEFAULT 0,
    connection_generation INTEGER NOT NULL DEFAULT 0,
    -- Controller epoch in which this machine last reconciled since it
    -- connected. NULL means it must reconcile before it is offered work.
    reconciled_epoch      INTEGER,
    heartbeat_seq         INTEGER NOT NULL DEFAULT 0,
    last_seen_at          INTEGER,
    telemetry             TEXT,
    effective_budget      TEXT,
    admitting             INTEGER NOT NULL DEFAULT 0,
    worker_flags          TEXT NOT NULL DEFAULT '[]',
    controller_flags      TEXT NOT NULL DEFAULT '[]',
    local_control         TEXT NOT NULL,
    enrolled_at           INTEGER NOT NULL,
    retired_at            INTEGER
) WITHOUT ROWID;

CREATE TABLE projects (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    policy     TEXT NOT NULL,
    created_at INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE snapshots (
    id              TEXT PRIMARY KEY,
    project_id      TEXT NOT NULL REFERENCES projects (id),
    manifest_digest TEXT NOT NULL,
    file_count      INTEGER NOT NULL,
    total_bytes     INTEGER NOT NULL,
    meta            TEXT NOT NULL,
    registered_at   INTEGER NOT NULL
) WITHOUT ROWID;

-- `seq` is the submission order.
CREATE TABLE jobs (
    seq              INTEGER PRIMARY KEY AUTOINCREMENT,
    id               TEXT NOT NULL UNIQUE,
    project_id       TEXT NOT NULL REFERENCES projects (id),
    principal_id     TEXT NOT NULL REFERENCES principals (id),
    session_id       TEXT,
    session          TEXT,
    snapshot_id      TEXT NOT NULL REFERENCES snapshots (id),
    origin_platform  TEXT NOT NULL,
    recipe           TEXT NOT NULL,
    priority         TEXT NOT NULL,
    -- The frozen specification, and the parts of it transitions need.
    spec             TEXT NOT NULL,
    envelope         TEXT NOT NULL,
    retry_class      TEXT NOT NULL,
    max_attempts     INTEGER NOT NULL,
    max_log_bytes    INTEGER NOT NULL,
    phase            TEXT NOT NULL,
    outcome          TEXT,
    cleanup          TEXT NOT NULL,
    cancel_requested INTEGER NOT NULL DEFAULT 0,
    queue_reason     TEXT,
    -- Generation of the newest attempt; 0 before the first offer.
    generation       INTEGER NOT NULL DEFAULT 0,
    environment_id   TEXT,
    created_at       INTEGER NOT NULL,
    queued_at        INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    last_event       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX jobs_by_phase ON jobs (phase, seq);
CREATE INDEX jobs_by_project ON jobs (project_id, seq);
CREATE INDEX jobs_by_session ON jobs (session_id, seq) WHERE session_id IS NOT NULL;

CREATE TABLE idempotency_keys (
    principal_id   TEXT NOT NULL REFERENCES principals (id),
    key            TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    job_id         TEXT NOT NULL REFERENCES jobs (id),
    created_at     INTEGER NOT NULL,
    PRIMARY KEY (principal_id, key)
) WITHOUT ROWID;

-- One execution of a job together with its resource reservation.
-- `held = 1` means the reservation still counts against the machine.
CREATE TABLE attempts (
    id                    TEXT PRIMARY KEY,
    job_id                TEXT NOT NULL REFERENCES jobs (id),
    generation            INTEGER NOT NULL,
    machine_id            TEXT NOT NULL REFERENCES machines (id),
    phase                 TEXT NOT NULL,
    outcome               TEXT,
    cleanup               TEXT NOT NULL,
    uncertain             INTEGER NOT NULL DEFAULT 0,
    reserved              TEXT NOT NULL,
    held                  INTEGER NOT NULL DEFAULT 1,
    release_reason        TEXT,
    release_proof         TEXT,
    policy_revision       INTEGER NOT NULL,
    offer_epoch           INTEGER NOT NULL,
    offered_at            INTEGER NOT NULL,
    offer_expires_at      INTEGER NOT NULL,
    accepted_at           INTEGER,
    start_committed_at    INTEGER,
    start_epoch           INTEGER,
    start_delivered_at    INTEGER,
    authority             TEXT,
    authority_valid_until INTEGER,
    scope                 TEXT,
    usage                 TEXT,
    started_at            INTEGER,
    exited_at             INTEGER,
    released_at           INTEGER,
    last_event_seq        INTEGER NOT NULL DEFAULT 0,
    log_bytes             INTEGER NOT NULL DEFAULT 0,
    log_truncated         INTEGER NOT NULL DEFAULT 0,
    UNIQUE (job_id, generation)
) WITHOUT ROWID;
CREATE INDEX attempts_held_by_machine ON attempts (machine_id) WHERE held = 1;
-- A job has at most one live or unresolved attempt.
CREATE UNIQUE INDEX attempts_one_held_per_job ON attempts (job_id) WHERE held = 1;

CREATE TABLE attempt_steps (
    attempt_id  TEXT NOT NULL REFERENCES attempts (id),
    position    INTEGER NOT NULL,
    name        TEXT NOT NULL,
    kind        TEXT NOT NULL,
    state       TEXT NOT NULL,
    exit_code   INTEGER,
    started_at  INTEGER,
    finished_at INTEGER,
    PRIMARY KEY (attempt_id, name)
) WITHOUT ROWID;

CREATE TABLE environments (
    id                  TEXT PRIMARY KEY,
    job_id              TEXT NOT NULL REFERENCES jobs (id),
    attempt_id          TEXT NOT NULL UNIQUE REFERENCES attempts (id),
    project_id          TEXT NOT NULL REFERENCES projects (id),
    machine_id          TEXT NOT NULL REFERENCES machines (id),
    snapshot_id         TEXT NOT NULL,
    state               TEXT NOT NULL,
    endpoints           TEXT NOT NULL,
    lease               TEXT NOT NULL,
    effective_deadline  INTEGER NOT NULL,
    finish_requested_at INTEGER,
    ended_at            INTEGER,
    ended_reason        TEXT,
    created_at          INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX environments_live ON environments (effective_deadline) WHERE ended_at IS NULL;

-- Next expected byte offset of each log stream.
CREATE TABLE log_streams (
    attempt_id  TEXT NOT NULL REFERENCES attempts (id),
    step        TEXT NOT NULL,
    stream      TEXT NOT NULL,
    next_offset INTEGER NOT NULL,
    truncated   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (attempt_id, step, stream)
) WITHOUT ROWID;

-- Index of stored log bytes in arrival order. The bytes are either in the
-- attempt's append-only log file at `file_offset`, or inline in `data`.
CREATE TABLE log_chunks (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id        TEXT NOT NULL REFERENCES jobs (id),
    attempt_id    TEXT NOT NULL REFERENCES attempts (id),
    step          TEXT NOT NULL,
    stream        TEXT NOT NULL,
    stream_offset INTEGER NOT NULL,
    len           INTEGER NOT NULL,
    file_offset   INTEGER,
    data          BLOB
);
CREATE INDEX log_chunks_by_job ON log_chunks (job_id, id);

CREATE TABLE artifacts (
    id          TEXT PRIMARY KEY,
    job_id      TEXT NOT NULL REFERENCES jobs (id),
    attempt_id  TEXT NOT NULL REFERENCES attempts (id),
    snapshot_id TEXT NOT NULL,
    path        TEXT NOT NULL,
    kind        TEXT NOT NULL,
    digest      TEXT NOT NULL,
    size        INTEGER NOT NULL,
    created_at  INTEGER NOT NULL,
    UNIQUE (attempt_id, kind, path)
) WITHOUT ROWID;
CREATE INDEX artifacts_by_job ON artifacts (job_id);
