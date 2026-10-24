CREATE TABLE launch (
    id            TEXT    PRIMARY KEY,
    connection    TEXT    NOT NULL,
    run_id        INTEGER NOT NULL,
    label         TEXT    NOT NULL,
    state         TEXT    NOT NULL,
    worker_id     TEXT,
    swapped       INTEGER NOT NULL DEFAULT 0,
    dispatched_at INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    UNIQUE (connection, run_id)
);

CREATE INDEX launch_open ON launch (connection, state);

CREATE TABLE profile (
    connection TEXT NOT NULL,
    label      TEXT NOT NULL,
    profile    TEXT NOT NULL,
    PRIMARY KEY (connection, label)
);

CREATE TABLE secret (
    connection TEXT NOT NULL,
    kind       TEXT NOT NULL,
    value      TEXT NOT NULL,
    PRIMARY KEY (connection, kind)
);
