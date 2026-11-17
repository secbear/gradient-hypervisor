"""Exercise prepared workers against disposable, matched Gradient and Kubernetes."""

import argparse
import base64
import json
import os
from pathlib import Path
import secrets
import subprocess
import sys
import tempfile
import time
from urllib.parse import urlsplit
import uuid

NAMESPACE = "gradient-prepared-test"
GRADIENT_URL = f"http://gradient.{NAMESPACE}.svc:3000"
TEST_IMAGE = "gradient-kubernetes-test:fixture"
WORKER_IMAGE = "gradient-worker:2.0.1"
SERVICE_ACCOUNT = "gradient-autoscaler-fixture"
FEATURES = ["fixture$$feature", "fixture$(NODE)"]
PROJECT_ID = "10000000-0000-0000-0000-000000000001"
WORKER_SELECTOR = (
    "app.kubernetes.io/managed-by=gradient-autoscaler,gradient.wavelens.io/connection=fixture"
)


class Fixture:
    def __init__(self, args, directory):
        self.args = args
        self.directory = directory
        self.name = "gradient-prepared-" + uuid.uuid4().hex[:12]
        self.kubeconfig = directory / "kubeconfig"
        self.environment = {**os.environ, "KIND_EXPERIMENTAL_PROVIDER": args.provider,
                            "KUBECONFIG": str(self.kubeconfig)}
        self.redactions = set()
        self.guarded = False

    def safe(self, text):
        for value in sorted(self.redactions, key=len, reverse=True):
            text = text.replace(value, "<redacted>")
        return text

    def run(self, command, data=None, timeout=120):
        result = subprocess.run(command, input=data, env=self.environment, text=True,
                                capture_output=True, timeout=timeout)
        if result.returncode:
            raise RuntimeError(
                f"{command[0]} failed ({result.returncode}): "
                + self.safe(result.stderr)[-2000:]
            )
        return result.stdout

    def kubectl(self, *args, data=None):
        return self.run(["kubectl", "--kubeconfig", str(self.kubeconfig),
                         "--context", "kind-" + self.name, "--request-timeout=20s",
                         "-n", NAMESPACE, *args], data=data)

    def guard(self):
        config = json.loads(self.kubectl("config", "view", "--minify", "-o", "json"))
        endpoint = urlsplit(config["clusters"][0]["cluster"]["server"])
        if (endpoint.scheme != "https" or endpoint.hostname != "127.0.0.1"
                or not endpoint.port or endpoint.username is not None
                or endpoint.password is not None):
            raise RuntimeError("refusing a non-loopback Kubernetes API")
        nodes = json.loads(self.kubectl("get", "nodes", "-o", "json"))["items"]
        if [node["metadata"]["name"] for node in nodes] != [self.name + "-control-plane"]:
            raise RuntimeError("refusing unexpected cluster nodes")

    def start(self):
        config = self.directory / "kind.json"
        config.write_text(json.dumps({"kind": "Cluster", "apiVersion": "kind.x-k8s.io/v1alpha4",
                                      "nodes": [{"role": "control-plane"}]}))
        self.run(["kind", "create", "cluster", "--name", self.name,
                  "--config", str(config), "--kubeconfig", str(self.kubeconfig),
                  "--wait", "120s"], timeout=240)
        self.guard()
        self.guarded = True
        for archive in (self.args.test_image_archive, self.args.worker_image_archive):
            self.run(["kind", "load", "image-archive", str(archive),
                      "--name", self.name], timeout=240)
        for image in (TEST_IMAGE, WORKER_IMAGE):
            self.run([self.args.provider, "exec", self.name + "-control-plane",
                      "crictl", "inspecti", image])
        self.kubectl("create", "namespace", NAMESPACE)

    def apply(self, kind, name, **body):
        api = {"Deployment": "apps/v1", "Job": "batch/v1"}.get(kind, "v1")
        self.kubectl("apply", "-f", "-", data=json.dumps({
            "apiVersion": api, "kind": kind,
            "metadata": {"name": name, "namespace": NAMESPACE}, **body,
        }))

    def redact_credentials(self, values):
        for value in values:
            if not value:
                continue
            self.redactions.update(part for part in value.splitlines() if len(part) >= 8)
            self.redactions.add(value)
            self.redactions.add(base64.b64encode(value.encode()).decode())

    def secret(self, name, values):
        self.redact_credentials(values.values())
        self.apply("Secret", name, type="Opaque", stringData=values)

    def deploy(self, name, container, volumes, user, cpu, memory):
        resources = {"cpu": cpu, "memory": memory}
        container = {"name": name, "image": TEST_IMAGE, "imagePullPolicy": "Never",
                     "resources": {"requests": resources, "limits": resources}, **container}
        self.apply("Deployment", name, spec={
            "replicas": 1, "selector": {"matchLabels": {"app": name}},
            "template": {"metadata": {"labels": {"app": name}}, "spec": {
                "automountServiceAccountToken": False,
                "securityContext": {"runAsUser": user, "runAsGroup": user, "fsGroup": user},
                "containers": [container], "volumes": volumes,
            }},
        })
        self.kubectl("rollout", "status", "deployment/" + name, "--timeout=90s")

    def service(self, name, port):
        self.apply("Service", name, spec={"selector": {"app": name},
                                         "ports": [{"port": port, "targetPort": port}]})

    def api(self, path, body=None, token=None):
        # Curl configuration on stdin keeps passwords and tokens out of arguments.
        config = ['silent', 'show-error', 'fail',
                  "url = " + json.dumps(GRADIENT_URL + "/api/v1/" + path)]
        if token:
            config.append("header = " + json.dumps("Authorization: Bearer " + token))
        if body is not None:
            config.extend(['header = "Content-Type: application/json"',
                           "data = " + json.dumps(json.dumps(body))])
        response = self.kubectl("exec", "-i", "deployment/postgres", "--",
                               "/bin/curl", "--config", "-", data="\n".join(config) + "\n")
        result = json.loads(response)
        if result.get("error"):
            raise RuntimeError("Gradient API request failed: " + path)
        return result["message"]

    def wait_api(self):
        self.kubectl("exec", "deployment/postgres", "--", "/bin/curl",
                     "--silent", "--show-error", "--fail", "--max-time", "3",
                     "--retry", "20", "--retry-connrefused", "--retry-delay", "1",
                     "--retry-max-time", "30", GRADIENT_URL + "/api/v1/health")

    def driver_credential(self, session):
        if self.args.auth == "session":
            print("SESSION TEST MODE: lifecycle checks only; API-key compatibility is not validated.")
            credential = session
        else:
            credential = self.api("user/keys", {
                "name": "prepared-fixture", "expires_in_days": 1,
                "permissions": ["viewProject", "manageWorkers"],
            }, session)
        self.secret("driver-key", {"api-key": credential})

    def bootstrap(self):
        password, pg_password = secrets.token_urlsafe(24), secrets.token_urlsafe(32)
        self.redactions.add(password)
        self.secret("database", {"password": pg_password})
        self.deploy("postgres", {
            "command": ["/bin/bash", "-euc", """
if [ ! -f /data/pg/PG_VERSION ]; then
  initdb -D /data/pg -U postgres --pwfile=/credentials/password --auth=scram-sha-256
  printf 'host all all 0.0.0.0/0 scram-sha-256\\n' >> /data/pg/pg_hba.conf
fi
exec postgres -D /data/pg -h 0.0.0.0 -k /data \
  -c max_connections=200 -c max_locks_per_transaction=1024
"""],
            "env": [{"name": "HOME", "value": "/data/pg"},
                    {"name": "PGPASSWORD", "valueFrom": {
                "secretKeyRef": {"name": "database", "key": "password"}}}],
            "volumeMounts": [{"name": "data", "mountPath": "/data"},
                             {"name": "credentials", "mountPath": "/credentials", "readOnly": True}],
            "readinessProbe": {"exec": {"command": ["/bin/pg_isready", "-h", "127.0.0.1"]}},
        }, [{"name": "data", "emptyDir": {}},
            {"name": "credentials", "secret": {"secretName": "database"}}], 1000, "1", "1Gi")
        self.service("postgres", 5432)
        self.kubectl("exec", "deployment/postgres", "--", "/bin/createdb",
                     "-h", "127.0.0.1", "-U", "postgres", "gradient")
        hashed = self.kubectl("exec", "-i", "deployment/postgres", "--", "/bin/argon2",
                              secrets.token_hex(16), "-id", "-e", data=password).strip()
        private_key = self.kubectl("exec", "deployment/postgres", "--", "/bin/bash", "-euc",
                                  'd=$(mktemp -d); trap \'rm -rf "$d"\' EXIT; '
                                  'ssh-keygen -q -t ed25519 -N "" -f "$d/key"; cat "$d/key"')
        cache_key = self.kubectl("exec", "deployment/postgres", "--", "/bin/bash", "-euc",
                                'd=$(mktemp -d); trap \'rm -rf "$d"\' EXIT; '
                                'nix-store --store dummy:// --generate-binary-cache-key fixture "$d/key" "$d/pub"; '
                                'cat "$d/key"')
        self.secret("credentials", {
            "gradient_user_fixture_password": hashed,
            "gradient_project_fixture_private_key": private_key,
            "gradient_cache_fixture_signing_key": cache_key.split(":", 1)[1].strip(),
            "jwt": base64.b64encode(secrets.token_bytes(48)).decode(),
            "crypt": base64.b64encode(secrets.token_bytes(48)).decode(),
            "database-url": f"postgresql://postgres:{pg_password}@postgres:5432/gradient",
        })
        state = {
            "users": {"fixture": {"username": "fixture", "name": "Fixture",
                                  "email": "fixture@example.invalid", "password_file": "mounted"}},
            "teams": {"fixture": {"name": "fixture", "display_name": "Fixture",
                                  "members": [{"user": "fixture", "role": "Admin"}]}},
            "projects": {"fixture": {"name": "fixture", "display_name": "Fixture",
                                     "id": PROJECT_ID, "created_by": "fixture",
                                     "private_key_file": "mounted", "public": False,
                                     "teams": [{"team": "fixture", "role": "Admin",
                                                "users": True, "workers": True}]}},
            "caches": {"fixture": {"name": "fixture", "display_name": "Fixture",
                                  "public": False, "created_by": "fixture",
                                  "signing_key_file": "mounted", "projects": ["fixture"],
                                  "upstream_caches": []}},
        }
        self.apply("ConfigMap", "server-state", data={"state.json": json.dumps(state)})
        self.deploy("gradient", {
            "command": ["/bin/gradient-server", "--database-url-file", "/credentials/database-url",
                        "--state-file", "/fixture/state.json", "--credentials-dir", "/credentials",
                        "--secrets-jwt-file", "/credentials/jwt", "--secrets-crypt-file", "/credentials/crypt",
                        "--base-dir", "/data", "--listen-addr", "0.0.0.0", "--port", "3000",
                        "--serve-url", GRADIENT_URL, "--frontend-url", GRADIENT_URL],
            "env": [{"name": "GRADIENT_USE_TLS", "value": "false"},
                    {"name": "GRADIENT_SENTRY_ENABLE", "value": "false"}],
            "volumeMounts": [{"name": "credentials", "mountPath": "/credentials", "readOnly": True},
                             {"name": "config", "mountPath": "/fixture", "readOnly": True},
                             {"name": "data", "mountPath": "/data"}, {"name": "tmp", "mountPath": "/tmp"}],
            "readinessProbe": {"httpGet": {"path": "/api/v1/health", "port": 3000}},
        }, [{"name": "credentials", "secret": {"secretName": "credentials"}},
            {"name": "config", "configMap": {"name": "server-state"}},
            {"name": "data", "emptyDir": {}}, {"name": "tmp", "emptyDir": {}}], 65532, "2", "2Gi")
        self.service("gradient", 3000)
        self.wait_api()
        token = self.api("auth/basic/login", {"loginname": "fixture", "password": password})
        self.redactions.add(token)
        self.driver_credential(token)

    def driver(self):
        capabilities = dict(build=True, network=True, eval=False, secrets=False, exec=False)
        resources = dict(architectures=["x86_64-linux"], system_features=FEATURES,
                         max_concurrent_builds=2, cpu_count=2, ram_total_mb=1024,
                         cpu_core_score=4000, build_cores=1)
        pool = dict(system="x86_64-linux", features=FEATURES, maxJobs=2, cpu="2", memory="2Gi",
                    privileged=True, buildResources=dict(cpuCount=2, ramTotalMb=1024,
                                                        cpuCoreScore=4000, buildCores=1),
                    podOptions={"env": [{"name": "GRADIENT_SENTRY_ENABLE", "value": "false"}]})
        config = dict(listen="127.0.0.1:8080", publicUrl="http://fixture:8080", stateDir="/state",
                      connections=[dict(name="fixture", gradientUrl=GRADIENT_URL, team="fixture",
                                        projects=[PROJECT_ID], apiKeyFile="/credentials/api-key",
                                        maxLaunches=1, maxLaunchesPerKind=1, idleTimeout=600,
                                        maxLifetime=1800, workerCapabilities=capabilities,
                                        kubernetes=dict(namespace=NAMESPACE, image=WORKER_IMAGE,
                                                        pools={"x86": pool}))])
        profile = dict(name="x86", worker_id=str(uuid.uuid4()),
                       capabilities={**capabilities, "core": False, "cache": False, "federate": False},
                       resources=resources)
        self.apply("ConfigMap", "driver-config", data={
            "config.json": json.dumps(config), "profile.json": json.dumps(profile)})
        # Helm also evaluates sibling templates when selecting only RBAC.
        rbac = self.run(["helm", "template", SERVICE_ACCOUNT, str(self.args.chart),
                         "--namespace", NAMESPACE, "--show-only", "templates/rbac.yaml",
                         "--values", "-"], data=json.dumps({
                             "connections": {"fixture": {
                                 "apiKeySecret": {"name": "driver-key", "key": "api-key"},
                                 "kubernetes": {"namespace": NAMESPACE}}}}))
        self.kubectl("apply", "-f", "-", data=rbac)
        driver_resources = {"cpu": "1", "memory": "512Mi"}
        self.apply("Job", "prepared", spec={
            "backoffLimit": 0, "activeDeadlineSeconds": 300,
            "template": {"spec": {
                "restartPolicy": "Never", "serviceAccountName": SERVICE_ACCOUNT,
                "automountServiceAccountToken": True,
                "securityContext": {"runAsNonRoot": True, "runAsUser": 65532,
                                    "runAsGroup": 65532, "fsGroup": 65532},
                "containers": [{"name": "driver", "image": TEST_IMAGE, "imagePullPolicy": "Never",
                                "resources": {"requests": driver_resources, "limits": driver_resources},
                                "readinessProbe": {"exec": {"command": ["/bin/test", "-f", "/state/ready"]},
                                                   "periodSeconds": 1},
                                "command": ["/bin/prepared-kubernetes", "--config", "/fixture/config.json",
                                            "--profile", "/fixture/profile.json"], "workingDir": "/state",
                                "env": [{"name": "HOME", "value": "/tmp"}],
                                "volumeMounts": [{"name": "config", "mountPath": "/fixture", "readOnly": True},
                                                 {"name": "key", "mountPath": "/credentials", "readOnly": True},
                                                 {"name": "state", "mountPath": "/state"},
                                                 {"name": "tmp", "mountPath": "/tmp"}]}],
                "volumes": [{"name": "config", "configMap": {"name": "driver-config"}},
                            {"name": "key", "secret": {"secretName": "driver-key"}},
                            {"name": "state", "emptyDir": {}}, {"name": "tmp", "emptyDir": {}}],
            }},
        })
        self.job_deadline = time.monotonic() + 330
        self.wait_job(ready=True)

    def wait_job(self, ready=False):
        while time.monotonic() < self.job_deadline:
            job = json.loads(self.kubectl("get", "job", "prepared", "-o", "json"))
            status = job.get("status", {})
            if status.get("failed") or any(
                    condition.get("type") == "Failed" and condition.get("status") == "True"
                    for condition in status.get("conditions", [])):
                raise RuntimeError("prepared driver Job failed")
            if status.get("succeeded"):
                if ready:
                    raise RuntimeError("prepared driver finished before runtime inspection")
                return
            if ready:
                pods = json.loads(self.kubectl("get", "pods", "-l", "job-name=prepared",
                                              "-o", "json"))["items"]
                if len(pods) > 1:
                    raise RuntimeError("expected only one driver Job Pod")
                if pods:
                    pod = pods[0]
                    if not any(owner.get("kind") == "Job" and owner.get("name") == "prepared"
                               and owner.get("uid") == job["metadata"]["uid"]
                               for owner in pod["metadata"].get("ownerReferences", [])):
                        raise RuntimeError("driver Pod does not belong to the fixture Job")
                    if any(condition.get("type") == "Ready" and condition.get("status") == "True"
                           for condition in pod.get("status", {}).get("conditions", [])):
                        return
            time.sleep(1)
        raise RuntimeError("prepared driver Job readiness or completion timed out")

    def finish(self):
        self.kubectl("exec", "job/prepared", "--", "/bin/touch", "/state/inspected")
        self.wait_job()

    def check_worker(self):
        pods = json.loads(self.kubectl("get", "pods", "-l", WORKER_SELECTOR, "-o", "json"))["items"]
        if len(pods) != 1:
            raise RuntimeError("expected exactly one prepared worker Pod")
        names = ["GRADIENT_WORKER_SYSTEM_ARCHITECTURES", "GRADIENT_WORKER_SYSTEM_FEATURES",
                 "GRADIENT_WORKER_SYSTEM_CPU_CORE_SCORE", "GRADIENT_WORKER_BUILD_MAX_CORES"]
        pod = pods[0]["metadata"]["name"]
        actual = self.kubectl("exec", pod, "--", "/bin/printenv", *names).splitlines()
        if actual != ["x86_64-linux", ",".join(FEATURES), "4000", "1"]:
            raise RuntimeError("worker native environment differs from the literal profile")
        settings = json.loads(self.kubectl("exec", pod, "--", "/bin/nix",
                                          "--extra-experimental-features", "nix-command",
                                          "show-config", "--json"))
        if not set(FEATURES).issubset(settings["system-features"]["value"]):
            raise RuntimeError("worker Nix system features lost literal profile names")
        print("PASS: native worker environment and Nix preserve literal profile settings")

    def logs(self, failed=False):
        if not self.guarded:
            return
        for target in ("job/prepared", "deployment/gradient", "deployment/postgres"):
            try:
                text = self.kubectl("logs", target, "--tail=80", "--limit-bytes=8000")
                print(f"{target}:\n{self.safe(text)}")
            except Exception:
                print(f"{target}: logs unavailable", file=sys.stderr)
        if failed:
            try:
                pods = json.loads(self.kubectl("get", "pods", "-l", WORKER_SELECTOR, "-o", "json"))["items"]
                print(f"worker Pod count: {len(pods)}")
                for pod in pods[:4]:
                    status = pod.get("status", {})
                    diagnostics = {
                        "name": pod["metadata"]["name"], "phase": status.get("phase"),
                        "conditions": [{key: condition.get(key) for key in ("type", "status", "reason")}
                                       for condition in status.get("conditions", [])[:5]],
                        "containers": [{
                            "name": container["name"],
                            "waiting": container.get("state", {}).get("waiting", {}).get("reason"),
                            "terminated": container.get("state", {}).get("terminated", {}).get("reason"),
                            "exit_code": container.get("state", {}).get("terminated", {}).get("exitCode"),
                        } for container in status.get("containerStatuses", [])[:3]],
                    }
                    print("worker status: " + self.safe(json.dumps(diagnostics))[:2000])
                    try:
                        name = pod["metadata"]["name"]
                        secret = json.loads(self.kubectl("get", "secret", name, "-o", "json"))
                        peers = base64.b64decode(secret["data"]["peers"]).decode()
                        self.redact_credentials([
                            peers, *[line.partition(":")[2] for line in peers.splitlines()],
                        ])
                        text = self.kubectl("logs", name, "--tail=80", "--limit-bytes=8000")
                        print("worker logs:\n" + self.safe(text))
                    except Exception:
                        print("worker logs unavailable", file=sys.stderr)
            except Exception:
                print("worker status unavailable", file=sys.stderr)

    def close(self):
        self.run(["kind", "delete", "cluster", "--name", self.name], timeout=180)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--test-image-archive", required=True, type=Path)
    parser.add_argument("--worker-image-archive", required=True, type=Path)
    parser.add_argument("--provider", choices=("podman", "docker"), default="podman")
    parser.add_argument("--auth", choices=("api-key", "session"), default="api-key",
                        help="Authentication for the disposable test driver.")
    parser.add_argument("--chart", type=Path,
                        default=Path(__file__).resolve().parents[3] / "helm/gradient-autoscaler")
    args = parser.parse_args(argv)
    with tempfile.TemporaryDirectory(prefix="gradient-prepared-") as temporary:
        fixture, status = Fixture(args, Path(temporary)), 0
        try:
            fixture.start()
            fixture.bootstrap()
            fixture.driver()
            fixture.check_worker()
            fixture.finish()
        except Exception as error:
            print(fixture.safe(str(error)), file=sys.stderr)
            status = 1
        finally:
            try:
                fixture.logs(failed=bool(status))
            except Exception as error:
                print("log collection failed: " + fixture.safe(str(error)), file=sys.stderr)
                status = 1
            finally:
                try:
                    fixture.close()
                except Exception as error:
                    print("cluster cleanup failed: " + fixture.safe(str(error)), file=sys.stderr)
                    status = 1
        return status


if __name__ == "__main__":
    sys.exit(main())
