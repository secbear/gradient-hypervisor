"""Check fixture locality and failure cleanup without starting tools or clusters."""

import base64
import json
import tempfile
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from prepared import Fixture, GRADIENT_URL, NAMESPACE, main


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
                        patch.object(subject, "secret") as secret, \
                        patch("builtins.print") as output:
                    subject.driver_credential("temporary-session")
                credential = "temporary-session" if mode == "session" else "issued-api-key"
                secret.assert_called_once_with("driver-key", {"api-key": credential})
                if mode == "session":
                    api.assert_not_called()
                    self.assertIn("API-key compatibility is not validated", output.call_args.args[0])
                else:
                    api.assert_called_once_with("user/keys", {
                        "name": "prepared-fixture", "expires_in_days": 1,
                        "permissions": ["viewProject", "manageWorkers"],
                    }, "temporary-session")
                    output.assert_not_called()

    def test_api_key_failure_does_not_fall_back_to_a_session(self):
        subject = fixture()
        subject.args.auth = "api-key"
        with patch.object(subject, "api", side_effect=RuntimeError("key failed")), \
                patch.object(subject, "secret") as secret:
            with self.assertRaisesRegex(RuntimeError, "key failed"):
                subject.driver_credential("temporary-session")
        secret.assert_not_called()

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
        with patch.object(subject, "kubectl", return_value='{"message":"ok"}') as command:
            subject.api("user/keys", {"name": "fixture"}, "disposable-token")
        self.assertNotIn("retry", command.call_args.kwargs["data"])
        self.assertNotIn("--retry", command.call_args.args)

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
