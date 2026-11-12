ALTER TABLE launch ADD COLUMN build_intent TEXT;
ALTER TABLE launch ADD COLUMN registration_started INTEGER NOT NULL DEFAULT 0;
ALTER TABLE launch ADD COLUMN secret_uid TEXT;
ALTER TABLE launch ADD COLUMN pod_uid TEXT;
CREATE INDEX launch_worker_id ON launch (worker_id);

CREATE TRIGGER build_launch_identity_insert
BEFORE INSERT ON launch
WHEN NEW.worker_id IS NOT NULL AND EXISTS (
    SELECT 1 FROM launch WHERE worker_id = NEW.worker_id
    AND (NEW.build_intent IS NOT NULL OR build_intent IS NOT NULL)
)
BEGIN
    SELECT RAISE(ABORT, 'build worker identity is already reserved');
END;

CREATE TRIGGER build_launch_identity_update
BEFORE UPDATE OF worker_id ON launch
WHEN NEW.worker_id IS NOT NULL AND EXISTS (
    SELECT 1 FROM launch WHERE id != OLD.id AND worker_id = NEW.worker_id
    AND (NEW.build_intent IS NOT NULL OR build_intent IS NOT NULL)
)
BEGIN
    SELECT RAISE(ABORT, 'build worker identity is already reserved');
END;

CREATE TRIGGER build_launch_intent_immutable
BEFORE UPDATE ON launch
WHEN OLD.build_intent IS NOT NULL AND (
    NEW.build_intent IS NOT OLD.build_intent OR
    NEW.worker_id IS NOT OLD.worker_id OR
    NEW.connection IS NOT OLD.connection OR
    NEW.external_id IS NOT OLD.external_id OR
    NEW.label IS NOT OLD.label OR
    NEW.dispatched_at IS NOT OLD.dispatched_at OR
    (OLD.secret_uid IS NOT NULL AND NEW.secret_uid IS NOT OLD.secret_uid) OR
    (OLD.pod_uid IS NOT NULL AND NEW.pod_uid IS NOT OLD.pod_uid)
)
BEGIN
    SELECT RAISE(ABORT, 'build launch intent is immutable');
END;
