CREATE TABLE launch_new (
    id            TEXT    PRIMARY KEY,
    connection    TEXT    NOT NULL,
    external_id   TEXT    NOT NULL,
    label         TEXT    NOT NULL,
    state         TEXT    NOT NULL,
    worker_id     TEXT,
    swapped       INTEGER NOT NULL DEFAULT 0,
    dispatched_at INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    UNIQUE (connection, external_id)
);

INSERT INTO launch_new (id, connection, external_id, label, state, worker_id, swapped, dispatched_at, updated_at)
SELECT id, connection, CAST(run_id AS TEXT), label, state, worker_id, swapped, dispatched_at, updated_at FROM launch;

DROP TABLE launch;
ALTER TABLE launch_new RENAME TO launch;
CREATE INDEX launch_open ON launch (connection, state);
