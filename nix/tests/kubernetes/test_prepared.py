"""Check fixture locality, auth requests and cleanup without starting tools or clusters."""

import base64
import json
import tempfile
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import call, patch

from prepared import Fixture, GRADIENT_URL, NAMESPACE, PROJECT_ID, main


def fixture():
    return Fixture(SimpleNamespace(provider="podman"), Path("/unused"))


def endpoint(url):
    return json.dumps({"clusters": [{"cluster": {"server": url}}]})


def nodes(*names):
    return json.dumps({"items": [{"metadata": {"name": name}} for name in names]})


class FixtureGuards(unittest.TestCase):
    def test_api_key_auth_is_the_default(self):
        with patch("prepared.Fixture") as factory, \
                patch("prepared.tempfile.TemporaryDirectory") as temporary:
            temporary.return_value.__enter__.return_value = "/unused"
            self.assertEqual(main(["--test-image-archive", "/unused/test.tar",
                                   "--worker-image-archive", "/unused/worker.tar"]), 0)
        self.assertEqual(factory.call_args.args[0].auth, "api-key")

    def test_driver_credential_modes_are_explicit(self):
        for mode in ("api-key", "session"):
            with self.subTest(mode=mode):
                subject = fixture()
                subject.args.auth = mode
                with patch.object(subject, "api", return_value="issued-api-key") as api, \
                        patch.object(subject, "validate_auth") as validate, \
                        patch.object(subject, "secret") as secret, \
                        patch("builtins.print") as output:
                    subject.driver_credential("temporary-session")
                credential = "temporary-session" if mode == "session" else "issued-api-key"
                secret.assert_called_once_with("driver-key", {"api-key": credential})
                if mode == "session":
                    api.assert_not_called()
                    validate.assert_not_called()
                    self.assertIn("API-key compatibility is not validated", output.call_args.args[0])
                else:
                    api.assert_called_once_with("user/keys", {
                        "name": "prepared-fixture", "expires_in_days": 1,
                        "team": "fixture", "permissions": ["manageWorkers"],
                    }, "temporary-session")
                    validate.assert_called_once_with("temporary-session", "issued-api-key")
                    output.assert_not_called()

    def test_api_key_failure_does_not_fall_back_to_a_session(self):
        subject = fixture()
        subject.args.auth = "api-key"
        for failed_step in ("api", "validate_auth"):
            with self.subTest(failed_step=failed_step), \
                    patch.object(subject, "api", return_value="issued-api-key") as api, \
                    patch.object(subject, "validate_auth") as validate, \
                    patch.object(subject, "secret") as secret:
                (api if failed_step == "api" else validate).side_effect = RuntimeError("key failed")
                with self.assertRaisesRegex(RuntimeError, "key failed"):
                    subject.driver_credential("temporary-session")
            secret.assert_not_called()

    def test_only_api_key_mode_adds_an_empty_mismatched_team(self):
        for mode in ("api-key", "session"):
            with self.subTest(mode=mode):
                subject = fixture()
                subject.args.auth = mode
                with patch.object(subject, "kubectl", side_effect=[
                    "", "password-hash", "private-key", "fixture:signing-key",
                ]), patch.object(subject, "secret"), patch.object(subject, "deploy"), \
                        patch.object(subject, "service"), patch.object(subject, "apply") as apply, \
                        patch.object(subject, "wait_api"), patch.object(subject, "api"), \
                        patch.object(subject, "driver_credential"):
                    subject.bootstrap()
                state = json.loads(apply.call_args.kwargs["data"]["state.json"])
                if mode == "api-key":
                    self.assertEqual(state["teams"]["other"],
                                     {"name": "other", "display_name": "Other", "members": []})
                else:
                    self.assertEqual(set(state["teams"]), {"fixture"})

    def test_rejects_nonlocal_endpoints_before_reading_nodes(self):
        for url in ("https://remote.example:443", "http://127.0.0.1:1234",
                    "https://user:password@127.0.0.1:1234", "https://127.0.0.1"):
            with self.subTest(url=url):
                subject = fixture()
                with patch.object(subject, "kubectl", return_value=endpoint(url)) as command:
                    with self.assertRaisesRegex(RuntimeError, "non-loopback"):
                        subject.guard()
                command.assert_called_once()

    def test_rejects_wrong_node_or_additional_nodes(self):
        subject = fixture()
        for wrong in (["other-control-plane"], [],
                      [subject.name + "-control-plane", "another-worker"]):
            with patch.object(subject, "kubectl", side_effect=[
                endpoint("https://127.0.0.1:1234"), nodes(*wrong),
            ]):
                with self.assertRaisesRegex(RuntimeError, "unexpected cluster"):
                    subject.guard()

    def test_uses_only_the_generated_context_and_private_kubeconfig(self):
        subject = fixture()
        with patch.object(subject, "kubectl", side_effect=[
            endpoint("https://127.0.0.1:1234"), nodes(subject.name + "-control-plane"),
        ]):
            subject.guard()
        with patch.object(subject, "run", return_value="") as command:
            subject.kubectl("get", "pods")
        arguments = command.call_args.args[0]
        self.assertIn(str(subject.kubeconfig), arguments)
        self.assertIn("kind-" + subject.name, arguments)
        self.assertIn(NAMESPACE, arguments)
        self.assertEqual(subject.environment["KIND_EXPERIMENTAL_PROVIDER"], "podman")
        self.assertEqual(subject.environment["KUBECONFIG"], str(subject.kubeconfig))
        self.assertNotEqual(subject.name, fixture().name)

    def test_no_diagnostics_before_locality_guard_passes(self):
        subject = fixture()
        with patch.object(subject, "kubectl") as command:
            subject.logs(failed=True)
        command.assert_not_called()

    def test_deletes_only_its_generated_cluster(self):
        subject = fixture()
        with patch.object(subject, "run") as command:
            subject.close()
        command.assert_called_once_with(
            ["kind", "delete", "cluster", "--name", subject.name], timeout=180)

    def test_readiness_retries_only_an_unauthenticated_health_get(self):
        subject = fixture()
        with patch.object(subject, "kubectl") as command:
            subject.wait_api()
        arguments = command.call_args.args
        self.assertEqual(arguments[-1], GRADIENT_URL + "/api/v1/health")
        self.assertIn("--retry-connrefused", arguments)
        self.assertIn("--retry-max-time", arguments)
        for forbidden in ("--data", "--request", "--header", "--user"):
            self.assertNotIn(forbidden, arguments)
        with patch.object(subject, "kubectl", return_value='{"message":"ok"}\n200') as command:
            subject.api("user/keys", {"name": "fixture"}, "disposable-token")
        self.assertNotIn("retry", command.call_args.kwargs["data"])
        self.assertNotIn("--retry", command.call_args.args)

    def test_api_checks_exact_status_and_keeps_credentials_on_stdin(self):
        for method, body, status, response in (
                (None, None, 200, '{"message":"ok"}\n200'),
                (None, {"name": "fixture"}, 200, '{"message":"ok"}\n200'),
                ("PATCH", {"active": False}, 200, '{"message":"ok"}\n200'),
                ("DELETE", None, 403, '{"error":true}\n403'),
                ("POST", None, 404, '{"error":true}\n404')):
            with self.subTest(method=method, status=status):
                subject = fixture()
                with patch.object(subject, "kubectl", return_value=response) as command:
                    result = subject.api("teams/fixture", body, "sensitive-token",
                                         method=method, expected_status=status)
                self.assertEqual(result, None if status >= 400 else "ok")
                self.assertEqual(command.call_args.args,
                                 ("exec", "-i", "deployment/postgres", "--",
                                  "/bin/curl", "--config", "-"))
                config = command.call_args.kwargs["data"]
                expected_method = method or ("POST" if body is not None else "GET")
                self.assertIn('request = "' + expected_method + '"', config)
                self.assertIn('write-out = "\\n%{http_code}"', config)
                self.assertIn("max-time = 20", config)
                self.assertIn("Authorization: Bearer sensitive-token", config)
                self.assertNotIn("fail", config)
                self.assertNotIn("retry", config)
                self.assertNotIn("sensitive-token", subject.safe("sensitive-token"))

    def test_api_does_not_accept_transport_server_or_parse_failures_as_denials(self):
        for response in ('{"error":true}\n401', '{"error":true}\n404',
                         '{"error":true}\n500', '{"message":"ok"}\n200',
                         '{"error":true}', '{"error":true}\n000'):
            with self.subTest(response=response):
                subject = fixture()
                with patch.object(subject, "kubectl", return_value=response):
                    with self.assertRaises(RuntimeError):
                        subject.api("teams/fixture", expected_status=403)
        subject = fixture()
        with patch("prepared.subprocess.run", return_value=SimpleNamespace(
                returncode=7, stderr="curl failed sensitive-token")):
            with self.assertRaisesRegex(RuntimeError, "curl failed <redacted>"):
                subject.api("teams/fixture", token="sensitive-token", expected_status=403)
        for response in ('not-json\n200', '{"error":true,"message":"failed"}\n200', '[]\n200'):
            with self.subTest(response=response), \
                    patch.object(subject, "kubectl", return_value=response):
                with self.assertRaises(RuntimeError):
                    subject.api("teams/fixture")

    def test_live_auth_case_table_uses_native_pins_and_exact_denials(self):
        subject = fixture()

        def response(path, body=None, token=None, **kwargs):
            if path == "user/keys":
                return body["name"]
            if kwargs.get("expected_status", 200) >= 400:
                return None
            if path == "teams/fixture":
                return {"name": "fixture", "id": "10000000-0000-0000-0000-000000000002"}
            if "/demand?" in path:
                return {"builds": [], "network": 0, "secrets": 0, "eval": 0}
            return []

        with patch.object(subject, "api", side_effect=response) as api, \
                patch.object(subject, "validate_webhooks") as webhooks, patch("builtins.print"):
            subject.validate_auth("session", "worker-key")
        calls = api.call_args_list
        for name, permissions, pin in (
                ("reader", ["viewProject"], {"team": "fixture"}),
                ("unpinned", ["manageWorkers", "manageWebhooks"], {}),
                ("project", ["manageWorkers", "manageWebhooks"], {"project": "fixture"}),
                ("cache", ["readStore"], {"cache": "fixture"}),
                ("webhooks", ["manageWebhooks"], {"team": "fixture"})):
            self.assertIn(call("user/keys", {
                "name": "prepared-" + name, "expires_in_days": 1,
                "permissions": permissions, **pin,
            }, "session"), calls)
            for path in ("teams/fixture", f"teams/fixture/workers/demand?projects={PROJECT_ID}",
                         "teams/fixture/workers"):
                self.assertIn(call(path, None, "prepared-" + name,
                                   method="GET", expected_status=403), calls)
        for path in ("teams/other", f"teams/other/workers/demand?projects={PROJECT_ID}",
                     "teams/other/workers"):
            self.assertIn(call(path, None, "worker-key", method="GET", expected_status=404), calls)
        registrations = [request for request in calls
                         if request.kwargs.get("method") == "POST"]
        self.assertEqual({(request.args[0], request.args[2], request.kwargs["expected_status"])
                          for request in registrations}, {
            *[("teams/fixture/workers", "prepared-" + name, 403)
              for name in ("reader", "unpinned", "project", "cache", "webhooks")],
            ("teams/other/workers", "worker-key", 404),
        })
        for path in ("teams", "teams/fixture/members", "teams/fixture/grants",
                     "teams/fixture/requests", "teams/fixture/invitations"):
            self.assertIn(call(path, token="session"), calls)
            for key in ("worker-key", "prepared-webhooks"):
                self.assertIn(call(path, None, key, method="GET", expected_status=403), calls)
        webhooks.assert_called_once_with("prepared-webhooks", [
            ("prepared-reader", 403), ("prepared-unpinned", 403),
            ("prepared-project", 403), ("prepared-cache", 403), ("worker-key", 403),
        ])

    def test_native_webhook_lifecycle_stays_inactive_and_redacts_rotated_secrets(self):
        path = "teams/fixture/webhooks"
        item = path + "/10000000-0000-0000-0000-000000000003"
        body = {"name": "prepared-fixture", "url": "https://fixture.example.invalid/gradient",
                "events": ["worker.*"], "active": False}
        hook = {"id": item.rsplit("/", 1)[1], **body}
        for rotated in ("whs_new-fixture-secret", "whs_old-fixture-secret"):
            with self.subTest(rotated=rotated):
                subject = fixture()
                with patch.object(subject, "api", side_effect=[
                    {"webhook": hook, "secret": "whs_old-fixture-secret"},
                    None, None, None, None,
                    [hook], hook, {**hook, "name": "prepared-updated"}, {"secret": rotated},
                    {"deleted": True}, [], None,
                ]) as api, patch("builtins.print") as output:
                    if rotated == "whs_old-fixture-secret":
                        with self.assertRaisesRegex(RuntimeError, "new signing secret"):
                            subject.validate_webhooks("webhook-key", [("worker-key", 403)])
                        output.assert_not_called()
                    else:
                        subject.validate_webhooks("webhook-key", [("worker-key", 403)])
                        self.assertEqual(api.call_args_list, [
                            call(path, body, "webhook-key"),
                            call(path, None, "worker-key", method="GET", expected_status=403),
                            call(path, body, "worker-key", method="POST", expected_status=403),
                            call("teams/other/webhooks", None, "webhook-key",
                                 method="GET", expected_status=404),
                            call("teams/other/webhooks", body, "webhook-key",
                                 method="POST", expected_status=404),
                            call(path, token="webhook-key"),
                            call(item, token="webhook-key"),
                            call(item, {"name": "prepared-updated"}, "webhook-key", method="PATCH"),
                            call(item + "/rotate-secret", token="webhook-key", method="POST"),
                            call(item, token="webhook-key", method="DELETE"),
                            call(path, token="webhook-key"),
                            call(item, token="webhook-key", expected_status=404),
                        ])
                for secret in ("whs_old-fixture-secret", rotated):
                    self.assertNotIn(secret, subject.safe(secret))

    def test_missing_loaded_image_fails_before_namespace_creation(self):
        with tempfile.TemporaryDirectory() as directory:
            subject = Fixture(SimpleNamespace(
                provider="podman", test_image_archive=Path("/unused/test.tar"),
                worker_image_archive=Path("/unused/worker.tar"),
            ), Path(directory))

            def command(arguments, **kwargs):
                if "crictl" in arguments:
                    raise RuntimeError("image is missing")
                return ""

            with patch.object(subject, "guard"), \
                    patch.object(subject, "run", side_effect=command) as run:
                with self.assertRaisesRegex(RuntimeError, "image is missing"):
                    subject.start()
            self.assertIn(
                ["podman", "exec", subject.name + "-control-plane",
                 "crictl", "inspecti", "gradient-kubernetes-test:fixture"],
                [call.args[0] for call in run.call_args_list],
            )
            self.assertFalse(any(
                "namespace" in call.args[0] for call in run.call_args_list
            ))

    def test_cleanup_runs_on_failure_and_preserves_nonzero_results(self):
        arguments = ["--test-image-archive", "/unused/test.tar",
                     "--worker-image-archive", "/unused/worker.tar"]
        for failed_step in ("start", "bootstrap", "driver", "check_worker", "finish", "logs", "close", None):
            with self.subTest(failed_step=failed_step), patch("prepared.Fixture") as factory, \
                    patch("prepared.tempfile.TemporaryDirectory") as temporary, \
                    patch("builtins.print"):
                temporary.return_value.__enter__.return_value = "/unused"
                subject = factory.return_value
                subject.safe.side_effect = lambda text: text
                if failed_step:
                    getattr(subject, failed_step).side_effect = RuntimeError("fixture failure")
                self.assertEqual(main(arguments), int(failed_step is not None))
                subject.close.assert_called_once()
                if failed_step is None:
                    self.assertEqual([call[0] for call in subject.method_calls],
                                     ["start", "bootstrap", "driver", "check_worker", "finish", "logs", "close"])

    def test_driver_failure_survives_cleanup_failure(self):
        with patch("prepared.Fixture") as factory, \
                patch("prepared.tempfile.TemporaryDirectory") as temporary, \
                patch("builtins.print") as output:
            temporary.return_value.__enter__.return_value = "/unused"
            subject = factory.return_value
            subject.safe.side_effect = lambda text: text
            subject.driver.side_effect = RuntimeError("driver failed")
            subject.close.side_effect = RuntimeError("delete failed")
            self.assertEqual(main(["--test-image-archive", "/unused/test.tar",
                                   "--worker-image-archive", "/unused/worker.tar"]), 1)
            self.assertEqual(output.call_args_list[0].args[0], "driver failed")
            self.assertIn("delete failed", output.call_args_list[1].args[0])

    def test_redacts_raw_and_encoded_credentials(self):
        subject = fixture()
        with patch.object(subject, "apply"):
            subject.secret("key", {"api-key": "sensitive-token"})
        self.assertNotIn("sensitive-token", subject.safe("sensitive-token"))
        self.assertNotIn("c2Vuc2l0aXZlLXRva2Vu", subject.safe("c2Vuc2l0aXZlLXRva2Vu"))
        subject.redact_credentials(["", "temporary-peer-token"])
        self.assertNotIn("temporary-peer-token", subject.safe("temporary-peer-token"))
        self.assertEqual(subject.safe("unchanged"), "unchanged")
        subject.guarded = True
        pod = {"metadata": {"name": "worker"}, "spec": {"containers": [
            {"name": "worker", "env": [{"name": "TOKEN", "value": "unknown-worker-token"}]}]},
               "status": {"phase": "Pending", "conditions": [{
                   "type": "PodScheduled", "status": "False", "reason": "Unschedulable",
                   "message": "unknown-worker-token",
               }]}}
        with patch.object(subject, "kubectl", side_effect=[
            "sensitive-token", "", "", json.dumps({"items": [pod]}),
            json.dumps({"data": {"peers": base64.b64encode(
                b"peer:unknown-worker-token\n").decode()}}),
            "worker output unknown-worker-token",
        ]), patch("builtins.print") as output:
            subject.logs(failed=True)
        text = "\n".join(str(call.args[0]) for call in output.call_args_list)
        self.assertIn("Unschedulable", text)
        self.assertIn("worker logs:", text)
        self.assertNotIn("sensitive-token", text)
        self.assertNotIn("unknown-worker-token", text)


if __name__ == "__main__":
    unittest.main()
