from datetime import timedelta
import tempfile


PREVIEW_EXPECTED = {
    "startup-unready": "unready",
    "ready-busy": "ready",
    "periodic-unready": "unready",
    "reconnect-pending": "unready",
    "expired-connected": "unready",
    "reclaimed-unready": "unready",
}
PREVIEW_CHECKPOINTS = set()
PREVIEW_KEY_INSTALLED = False


def readiness_checkpoint(phase, deadline=None):
    global PREVIEW_KEY_INSTALLED
    expected = PREVIEW_EXPECTED[phase]
    assert phase not in PREVIEW_CHECKPOINTS, phase
    with subtest(f"Capacity preview: {phase}"):
        if not PREVIEW_KEY_INSTALLED:
            assert TOKEN is not None
            key = api("POST", "user/keys", {
                "name": "readiness-preview",
                "team": "readiness",
                "permissions": ["manageWorkers"],
                "expires_in_days": 1,
            })
            assert isinstance(key, str) and key.startswith("GTEAM")
            with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8") as source:
                source.write(key + "\n")
                source.flush()
                server.copy_from_host(source.name, "/run/readiness-preview-api-key")
            server.succeed("chmod 0600 /run/readiness-preview-api-key")
            PREVIEW_KEY_INSTALLED = True

        profile = assessment()["tracked_workers"][0]["profile"]
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8") as source:
            json.dump(profile, source)
            source.flush()
            server.copy_from_host(source.name, "/run/readiness-preview-profile.json")
        server.succeed("chmod 0600 /run/readiness-preview-profile.json")

        registration_query = (
            "SELECT md5(COALESCE((SELECT jsonb_agg(to_jsonb(t) ORDER BY t.id)::text "
            "FROM team_worker t), '[]')), "
            "md5(COALESCE((SELECT jsonb_agg(to_jsonb(t) ORDER BY t.id)::text "
            "FROM worker_registration t), '[]'))"
        )
        registrations_before = sql(registration_query)
        dispatches_before = sql("SELECT count(*) FROM dispatched_job WHERE kind = 1")
        allowed = 8 if deadline is None else min(8, deadline - time.monotonic() - 1)
        assert allowed > 0, "healthy observation budget exhausted before preview"
        receipt = json.loads(server.succeed(
            f"timeout {allowed}s readiness-preview --config /etc/readiness-preview.json "
            f"--profile /run/readiness-preview-profile.json --expect-live {expected}",
            timeout=timedelta(seconds=allowed),
        ))
        assert sql(registration_query) == registrations_before, phase
        assert sql("SELECT count(*) FROM dispatched_job WHERE kind = 1") == dispatches_before, phase
        assert receipt["connected"] is True, receipt
        assert (receipt["live_index"] is not None) == (expected == "ready"), receipt
        assert receipt["total_ready_builds"] > 0, receipt
        assert receipt["proposed_workers"] == int(expected == "unready"), receipt
        assert receipt["credited_starting_workers"] == 0, receipt
        PREVIEW_CHECKPOINTS.add(phase)
