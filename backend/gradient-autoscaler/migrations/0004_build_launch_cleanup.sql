ALTER TABLE launch ADD COLUMN cleanup_outcome TEXT CHECK (
    cleanup_outcome IS NULL OR (
        build_intent IS NOT NULL AND cleanup_outcome IN ('completed', 'failed', 'cancelled')
    )
);

CREATE INDEX launch_prepared_state ON launch (state, dispatched_at)
WHERE build_intent IS NOT NULL;

CREATE TRIGGER build_launch_cleanup_immutable
BEFORE UPDATE ON launch
WHEN OLD.cleanup_outcome IS NOT NULL AND NEW.cleanup_outcome IS NOT OLD.cleanup_outcome
BEGIN
    SELECT RAISE(ABORT, 'build launch cleanup outcome is immutable');
END;
