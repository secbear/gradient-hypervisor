mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradient_autoscaler::build_planner::{BuildScalingPolicy, PlannedBuildWorker};
use gradient_autoscaler::capacity::{BuildCapacityProfile, BuilderResources, CapacityCapabilities};
use gradient_autoscaler::catch_up::catch_up;
use gradient_autoscaler::config::{BuildScalingConfig, BuildScalingMode, WorkerCapabilities};
use gradient_autoscaler::github::oidc::OidcVerifier;
use gradient_autoscaler::http::{App, router};
use gradient_autoscaler::kubernetes::client::KubernetesClient;
use gradient_autoscaler::scaler::scale;
use gradient_autoscaler::setup::setup;
use gradient_autoscaler::state::{Launch, LaunchState, State};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::DeleteOptions;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;
use url::Url;
use uuid::Uuid;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request as Received, ResponseTemplate};

const PODS: &str = "/api/v1/namespaces/ci/pods";
const SECRETS: &str = "/api/v1/namespaces/ci/secrets";

fn envelope(message: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "error": false, "message": message }))
}

async fn mount(
    server: &MockServer,
    verb: &str,
    route: &str,
    response: ResponseTemplate,
    times: Option<u64>,
) {
    let mock = Mock::given(method(verb))
        .and(path(route))
        .respond_with(response);
    match times {
        Some(times) => mock.expect(times).mount(server).await,
        None => mock.mount(server).await,
    }
}

async fn demand(server: &MockServer, builds: Value) {
    let body = json!({ "builds": builds, "network": 0, "secrets": 0, "eval": 0 });
    mount(
        server,
        "GET",
        "/api/v1/teams/ops/workers/demand",
        envelope(body),
        None,
    )
    .await;
}

async fn received(server: &MockServer, verb: &str, route: &str) -> Vec<Received> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == verb && r.url.path() == route)
        .collect()
}

fn json_of(request: &Received) -> Value {
    serde_json::from_slice(&request.body).unwrap()
}

fn pod(name: &str, phase: &str, waiting: Option<&str>) -> ResponseTemplate {
    let state = match waiting {
        Some(reason) => json!({ "waiting": { "reason": reason } }),
        None => json!({ "running": {} }),
    };
    ResponseTemplate::new(200).set_body_json(json!({
        "metadata": { "name": name, "uid": "uid-1" },
        "status": { "phase": phase, "containerStatuses": [{ "state": state }] },
    }))
}

async fn open_launch(
    svc: &gradient_autoscaler::service::ConnectionService,
    state: LaunchState,
) -> Launch {
    open_launch_since(svc, state, OffsetDateTime::now_utc()).await
}

async fn open_launch_since(
    svc: &gradient_autoscaler::service::ConnectionService,
    state: LaunchState,
    dispatched_at: OffsetDateTime,
) -> Launch {
    let id = Uuid::new_v4();
    let launch = Launch {
        id,
        state,
        worker_id: Some(Uuid::new_v4()),
        dispatched_at,
        ..Launch::new("ops", &format!("gradient-worker-{id}"), "x86")
    };
    svc.state.insert_launch(&launch).await.unwrap();
    launch
}

fn build_profile() -> BuildCapacityProfile {
    BuildCapacityProfile {
        name: "x86".into(),
        worker_id: Uuid::new_v4(),
        capabilities: CapacityCapabilities {
            build: true,
            network: true,
            ..Default::default()
        },
        resources: BuilderResources {
            architectures: vec!["x86_64-linux".into()],
            system_features: vec!["kvm".into()],
            max_concurrent_builds: 4,
            cpu_count: 4,
            cpu_core_score: 4_000,
            ram_total_mb: 16_000,
            build_cores: None,
        },
    }
}

fn build_policy() -> BuildScalingPolicy {
    BuildScalingPolicy {
        queue_wait: Duration::seconds(30),
        startup_grace: Duration::minutes(2),
        max_new_workers: 2,
        fallback_ram_mb: 2_000,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LostResponse {
    None,
    Secret,
    Registration,
    Pod,
    Owner,
    PodDelete,
    SecretDelete,
    RegistrationDelete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedObjects {
    secret: Option<Value>,
    pod: Option<Value>,
    workers: Vec<Value>,
    lost: LostResponse,
    replace_on_delete: Option<LostResponse>,
    defer_pod_deletion: bool,
    defer_secret_deletion: bool,
    pod_absence_observed: bool,
    secret_absence_observed: bool,
}

async fn prepared_objects(server: &MockServer, lost: LostResponse) -> Arc<Mutex<PreparedObjects>> {
    use base64::Engine as _;
    let objects = Arc::new(Mutex::new(PreparedObjects {
        secret: None,
        pod: None,
        workers: vec![],
        lost,
        replace_on_delete: None,
        defer_pod_deletion: false,
        defer_secret_deletion: false,
        pod_absence_observed: false,
        secret_absence_observed: false,
    }));
    mount(
        server,
        "GET",
        "/api/v1/teams/ops",
        envelope(json!({"id": Uuid::from_u128(5)})),
        None,
    )
    .await;
    demand(server, json!([])).await;
    let hook_id = Uuid::from_u128(6);
    mount(
        server,
        "GET",
        "/api/v1/teams/ops/webhooks",
        envelope(json!([{
            "id": hook_id, "name": "gradient-autoscaler",
            "url": format!("{}/c/ops/gradient", common::PUBLIC_URL),
            "events": ["worker.demand"]
        }])),
        None,
    )
    .await;
    mount(
        server,
        "POST",
        &format!("/api/v1/teams/ops/webhooks/{hook_id}/rotate-secret"),
        envelope(json!({"secret": "hook-secret"})),
        None,
    )
    .await;
    for (verb, route, stage) in [
        ("POST", SECRETS, LostResponse::Secret),
        ("POST", PODS, LostResponse::Pod),
        (
            "POST",
            "/api/v1/teams/ops/workers",
            LostResponse::Registration,
        ),
    ] {
        let objects = Arc::clone(&objects);
        Mock::given(method(verb)).and(path(route)).respond_with(move |request: &Received| {
            let mut body = json_of(request);
            let mut objects = objects.lock().unwrap();
            match stage {
                LostResponse::Secret => {
                    assert!(objects.secret.is_none(), "credential secret created twice");
                    let peers = body["stringData"]["peers"].as_str().unwrap();
                    body["data"] = json!({"peers": base64::engine::general_purpose::STANDARD.encode(peers)});
                    body.as_object_mut().unwrap().remove("stringData");
                    body["metadata"]["uid"] = json!("secret-uid");
                    body["metadata"]["resourceVersion"] = json!("1");
                    objects.secret = Some(body.clone());
                    objects.secret_absence_observed = false;
                }
                LostResponse::Pod => {
                    assert!(objects.pod.is_none(), "worker pod created twice");
                    assert_eq!(objects.workers.len(), 1);
                    body["metadata"]["uid"] = json!("pod-uid");
                    body["spec"]["nodeName"] = json!("assigned-node");
                    body["status"] = json!({
                        "phase": "Pending",
                        "containerStatuses": [{"name": "worker", "state": {
                            "waiting": {"reason": "ContainerCreating"}
                        }}]
                    });
                    objects.pod = Some(body.clone());
                    objects.pod_absence_observed = false;
                }
                LostResponse::Registration => {
                    assert!(objects.workers.is_empty(), "worker registered twice");
                    assert!(objects.secret.is_some(), "registration preceded durable credential");
                    objects.workers.push(json!({
                        "worker_id": body["worker_id"], "display_name": body["display_name"],
                        "active": true, "managed": false, "gradient_ci": false, "url": null,
                        "enable_build": body["enable_build"], "enable_network": body["enable_network"],
                        "enable_secrets": body["enable_secrets"], "enable_exec": body["enable_exec"],
                        "enable_eval": body["enable_eval"], "connected": false
                    }));
                    body = json!({"error": false, "message": {"team": Uuid::from_u128(5)}});
                }
                _ => unreachable!(),
            }
            if objects.lost == stage {
                objects.lost = LostResponse::None;
                ResponseTemplate::new(500)
            } else {
                ResponseTemplate::new(if stage == LostResponse::Registration { 200 } else { 201 }).set_body_json(body)
            }
        }).mount(server).await;
    }
    let workers = Arc::clone(&objects);
    Mock::given(method("GET"))
        .and(path("/api/v1/teams/ops/workers"))
        .respond_with(move |_: &Received| envelope(json!(workers.lock().unwrap().workers)))
        .mount(server)
        .await;
    let pods = Arc::clone(&objects);
    Mock::given(method("GET"))
        .and(path(PODS))
        .and(query_param(
            "labelSelector",
            "gradient.wavelens.io/connection=ops",
        ))
        .respond_with(move |_: &Received| {
            let objects = pods.lock().unwrap();
            ResponseTemplate::new(200).set_body_json(json!({
                "apiVersion": "v1", "kind": "PodList", "metadata": {},
                "items": objects.pod.iter().collect::<Vec<_>>()
            }))
        })
        .mount(server)
        .await;
    let secret = Arc::clone(&objects);
    Mock::given(method("GET"))
        .respond_with(move |request: &Received| {
            let mut objects = secret.lock().unwrap();
            let is_secret = request.url.path().starts_with(&format!("{SECRETS}/"));
            let object = if is_secret {
                objects.secret.as_ref()
            } else if request.url.path().starts_with(&format!("{PODS}/")) {
                objects.pod.as_ref()
            } else {
                return ResponseTemplate::new(404);
            };
            if let Some(object) = object {
                if request.url.path().rsplit('/').next() == object["metadata"]["name"].as_str() {
                    return ResponseTemplate::new(200).set_body_json(object);
                }
                return ResponseTemplate::new(404);
            }
            if is_secret {
                objects.secret_absence_observed = true;
            } else {
                objects.pod_absence_observed = true;
            }
            ResponseTemplate::new(404)
        })
        .with_priority(10)
        .mount(server)
        .await;
    let deleted = Arc::clone(&objects);
    Mock::given(method("DELETE"))
        .respond_with(move |request: &Received| {
            let mut objects = deleted.lock().unwrap();
            let route = request.url.path();
            let stage = if route.starts_with(&format!("{PODS}/")) {
                LostResponse::PodDelete
            } else if route.starts_with(&format!("{SECRETS}/")) {
                LostResponse::SecretDelete
            } else if let Some(worker_id) = route.strip_prefix("/api/v1/teams/ops/workers/") {
                assert!(
                    objects.pod.is_none()
                        && objects.secret.is_none()
                        && objects.pod_absence_observed
                        && objects.secret_absence_observed,
                    "registration deleted before object absence was confirmed"
                );
                let Some(index) = objects
                    .workers
                    .iter()
                    .position(|worker| worker["worker_id"].as_str() == Some(worker_id))
                else {
                    return ResponseTemplate::new(404);
                };
                objects.workers.remove(index);
                if objects.lost == LostResponse::RegistrationDelete {
                    objects.lost = LostResponse::None;
                    return ResponseTemplate::new(500);
                }
                return envelope(json!("deleted"));
            } else {
                return ResponseTemplate::new(404);
            };
            let deferred = match stage {
                LostResponse::PodDelete => objects.defer_pod_deletion,
                LostResponse::SecretDelete => objects.defer_secret_deletion,
                _ => unreachable!(),
            };
            let replace = objects.replace_on_delete == Some(stage);
            if replace {
                objects.replace_on_delete = None;
            }
            let slot = match stage {
                LostResponse::PodDelete => &mut objects.pod,
                LostResponse::SecretDelete => &mut objects.secret,
                _ => unreachable!(),
            };
            let Some(object) = slot.as_mut() else {
                return ResponseTemplate::new(404);
            };
            assert_eq!(
                route.rsplit('/').next(),
                object["metadata"]["name"].as_str()
            );
            let Ok(options) = serde_json::from_slice::<DeleteOptions>(&request.body) else {
                return ResponseTemplate::new(400);
            };
            if replace {
                object["metadata"]["uid"] = json!("replaced-object");
            }
            if options
                .preconditions
                .as_ref()
                .and_then(|p| p.uid.as_deref())
                != object["metadata"]["uid"].as_str()
            {
                return ResponseTemplate::new(409).set_body_json(json!({
                    "apiVersion": "v1", "kind": "Status", "status": "Failure",
                    "reason": "Conflict", "code": 409
                }));
            }
            if deferred {
                object["metadata"]["deletionTimestamp"] = json!("2026-11-19T20:00:00Z");
            } else {
                *slot = None;
            }
            match stage {
                LostResponse::PodDelete => objects.pod_absence_observed = false,
                LostResponse::SecretDelete => objects.secret_absence_observed = false,
                _ => unreachable!(),
            }
            if objects.lost == stage {
                objects.lost = LostResponse::None;
                ResponseTemplate::new(500)
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "apiVersion": "v1", "kind": "Status", "status": "Success"
                }))
            }
        })
        .mount(server)
        .await;
    let owner = Arc::clone(&objects);
    Mock::given(method("PUT"))
        .respond_with(move |request: &Received| {
            assert!(request.url.path().starts_with(&format!("{SECRETS}/")));
            let mut objects = owner.lock().unwrap();
            let body = json_of(request);
            assert_eq!(body["data"], objects.secret.as_ref().unwrap()["data"]);
            objects.secret = Some(body.clone());
            if objects.lost == LostResponse::Owner {
                objects.lost = LostResponse::None;
                ResponseTemplate::new(500)
            } else {
                ResponseTemplate::new(200).set_body_json(body)
            }
        })
        .mount(server)
        .await;
    objects
}

async fn prepare(
    svc: &gradient_autoscaler::service::ConnectionService,
    profile: &BuildCapacityProfile,
    now: OffsetDateTime,
) -> Launch {
    svc.launcher
        .prepare_build_launch(
            svc,
            &PlannedBuildWorker {
                profile_index: 0,
                label: "x86".into(),
                worker_id: profile.worker_id,
            },
            profile,
            &build_policy(),
            now,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn catch_up_recovers_each_lost_creation_response_with_the_same_worker_and_credential() {
    use base64::Engine as _;
    for lost in [
        LostResponse::Secret,
        LostResponse::Registration,
        LostResponse::Pod,
        LostResponse::Owner,
    ] {
        let server = MockServer::start().await;
        let objects = prepared_objects(&server, lost).await;
        let directory = tempfile::tempdir().unwrap();
        let state_path = directory.path().join("launches.sqlite");
        let mut fixture = common::kubernetes_service(&server.uri()).await;
        Arc::get_mut(&mut fixture.svc).unwrap().state =
            gradient_autoscaler::state::State::open(&state_path)
                .await
                .unwrap();
        let profile = build_profile();
        let launch = prepare(&fixture.svc, &profile, OffsetDateTime::now_utc()).await;
        assert_eq!(launch.worker_id, Some(profile.worker_id));
        assert_eq!(received(&server, "POST", PODS).await.len(), 0);
        assert!(
            fixture
                .svc
                .launcher
                .launch_prepared(&fixture.svc, launch.id)
                .await
                .is_err()
        );
        let peers = objects.lock().unwrap().secret.as_ref().unwrap()["data"]["peers"]
            .as_str()
            .unwrap()
            .to_owned();
        drop(fixture);
        let mut restarted = common::kubernetes_service(&server.uri()).await;
        Arc::get_mut(&mut restarted.svc).unwrap().state =
            gradient_autoscaler::state::State::open(&state_path)
                .await
                .unwrap();
        let restored = restarted
            .svc
            .state
            .launch("ops", launch.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restored.build_intent, launch.build_intent);
        catch_up(&restarted.svc).await;
        catch_up(&restarted.svc).await;
        let recovered = restarted
            .svc
            .state
            .launch("ops", launch.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.state, LaunchState::Queued);
        assert_eq!(recovered.secret_uid.as_deref(), Some("secret-uid"));
        assert_eq!(recovered.pod_uid.as_deref(), Some("pod-uid"));
        assert_eq!(recovered.cleanup_outcome, None);
        assert_eq!(received(&server, "GET", PODS).await.len(), 2);
        {
            let objects = objects.lock().unwrap();
            assert_eq!(objects.secret.as_ref().unwrap()["data"]["peers"], peers);
            assert_eq!(objects.workers.len(), 1);
            assert_eq!(
                objects.workers[0]["worker_id"],
                profile.worker_id.to_string()
            );
            assert_eq!(
                objects.secret.as_ref().unwrap()["metadata"]["ownerReferences"][0]["uid"],
                "pod-uid"
            );
        }
        let token = std::str::from_utf8(
            &base64::engine::general_purpose::STANDARD
                .decode(&peers)
                .unwrap(),
        )
        .unwrap()
        .to_owned();
        let token = token.trim().split_once(':').unwrap().1;
        assert!(!format!("{restored:?}").contains(token));
        assert_eq!(received(&server, "POST", PODS).await.len(), 1);
        assert_eq!(received(&server, "POST", SECRETS).await.len(), 1);
        let registrations = received(&server, "POST", "/api/v1/teams/ops/workers").await;
        assert_eq!(registrations.len(), 1);
        assert_eq!(
            json_of(&registrations[0]),
            json!({
                "worker_id": profile.worker_id, "display_name": format!("k8s-x86-{}", launch.id),
                "enable_build": true, "enable_network": true, "enable_secrets": false,
                "enable_exec": false, "enable_eval": false, "token": token
            })
        );
    }
}

#[tokio::test]
async fn catch_up_records_connection_without_live_data_and_keeps_unready_workers() {
    let server = MockServer::start().await;
    let objects = prepared_objects(&server, LostResponse::None).await;
    let mut fixture = common::kubernetes_service(&server.uri()).await;
    let launch = prepare(&fixture.svc, &build_profile(), OffsetDateTime::now_utc()).await;
    catch_up(&fixture.svc).await;
    {
        let mut objects = objects.lock().unwrap();
        objects.pod.as_mut().unwrap()["status"] = json!({"phase": "Running"});
        objects.workers[0]["live"] = json!({
            "architectures": ["x86_64-linux"], "system_features": ["kvm"],
            "max_concurrent_builds": 4, "ram_total_mb": 16_000
        });
    }
    catch_up(&fixture.svc).await;
    let running = fixture
        .svc
        .state
        .launch("ops", launch.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(running.state, LaunchState::Running);
    {
        let mut objects = objects.lock().unwrap();
        objects.workers[0]["connected"] = json!(true);
        objects.workers[0]["live"] = Value::Null;
    }
    catch_up(&fixture.svc).await;
    let mut connected = fixture
        .svc
        .state
        .launch("ops", launch.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(connected.state, LaunchState::Connected);
    connected.dispatched_at = OffsetDateTime::now_utc() - Duration::minutes(3);
    connected.build_intent.as_mut().unwrap().startup_deadline =
        connected.dispatched_at + build_policy().startup_grace;
    let svc = Arc::get_mut(&mut fixture.svc).unwrap();
    svc.state = State::memory().await.unwrap();
    svc.state.insert_launch(&connected).await.unwrap();
    let before = fixture.svc.state.launch("ops", launch.id).await.unwrap();
    catch_up(&fixture.svc).await;
    assert_eq!(
        fixture.svc.state.launch("ops", launch.id).await.unwrap(),
        before
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.method != "DELETE")
    );
    assert_eq!(received(&server, "POST", PODS).await.len(), 1);
    assert_eq!(received(&server, "POST", SECRETS).await.len(), 1);
}

#[tokio::test]
async fn catch_up_closes_prepared_pods_using_phase_and_observed_connection() {
    for (phase, connected, outcome) in [
        (Some("Succeeded"), true, LaunchState::Completed),
        (Some("Succeeded"), false, LaunchState::Failed),
        (Some("Failed"), true, LaunchState::Failed),
        (None, true, LaunchState::Failed),
        (None, false, LaunchState::Failed),
    ] {
        let server = MockServer::start().await;
        let objects = prepared_objects(&server, LostResponse::None).await;
        let fixture = common::kubernetes_service(&server.uri()).await;
        let launch = prepare(&fixture.svc, &build_profile(), OffsetDateTime::now_utc()).await;
        catch_up(&fixture.svc).await;
        if connected {
            {
                let mut objects = objects.lock().unwrap();
                objects.pod.as_mut().unwrap()["status"] = json!({"phase": "Running"});
                objects.workers[0]["connected"] = json!(true);
            }
            catch_up(&fixture.svc).await;
            assert_eq!(
                fixture
                    .svc
                    .state
                    .launch("ops", launch.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                LaunchState::Connected
            );
        }
        {
            let mut objects = objects.lock().unwrap();
            objects.workers[0]["connected"] = json!(connected && phase.is_none());
            match phase {
                Some(phase) => objects.pod.as_mut().unwrap()["status"] = json!({"phase": phase}),
                None => objects.pod = None,
            }
        }
        catch_up(&fixture.svc).await;
        let saved = fixture
            .svc
            .state
            .launch("ops", launch.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.state, outcome, "{phase:?}, connected={connected}");
        assert_eq!(saved.cleanup_outcome, Some(outcome));
        assert!(
            fixture
                .svc
                .state
                .open_launches("ops")
                .await
                .unwrap()
                .is_empty()
        );
        {
            let objects = objects.lock().unwrap();
            assert!(objects.pod.is_none());
            assert!(objects.secret.is_none());
            assert!(objects.workers.is_empty());
        }
        catch_up(&fixture.svc).await;
        assert_eq!(received(&server, "POST", PODS).await.len(), 1);
        assert_eq!(received(&server, "POST", SECRETS).await.len(), 1);
        assert_eq!(
            received(&server, "POST", "/api/v1/teams/ops/workers")
                .await
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn catch_up_expires_startup_and_cleans_orphan_credentials_without_launching() {
    for lost in [
        None,
        Some(LostResponse::Secret),
        Some(LostResponse::Registration),
        Some(LostResponse::None),
    ] {
        let server = MockServer::start().await;
        let objects = prepared_objects(&server, lost.unwrap_or(LostResponse::None)).await;
        let mut fixture = common::kubernetes_service(&server.uri()).await;
        let launch = prepare(&fixture.svc, &build_profile(), OffsetDateTime::now_utc()).await;
        if let Some(lost) = lost {
            let result = fixture
                .svc
                .launcher
                .launch_prepared(&fixture.svc, launch.id)
                .await;
            assert_eq!(result.is_ok(), lost == LostResponse::None);
        }
        let mut expired = fixture
            .svc
            .state
            .launch("ops", launch.id)
            .await
            .unwrap()
            .unwrap();
        expired.dispatched_at = OffsetDateTime::now_utc() - Duration::minutes(3);
        expired.build_intent.as_mut().unwrap().startup_deadline =
            expired.dispatched_at + build_policy().startup_grace;
        let svc = Arc::get_mut(&mut fixture.svc).unwrap();
        svc.state = State::memory().await.unwrap();
        svc.state.insert_launch(&expired).await.unwrap();
        let pods_before = received(&server, "POST", PODS).await.len();
        let secrets_before = received(&server, "POST", SECRETS).await.len();
        let registrations_before = received(&server, "POST", "/api/v1/teams/ops/workers")
            .await
            .len();
        catch_up(&fixture.svc).await;
        let saved = fixture
            .svc
            .state
            .launch("ops", launch.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.state, LaunchState::Failed, "{lost:?}");
        assert_eq!(saved.cleanup_outcome, Some(LaunchState::Failed));
        assert!(
            fixture
                .svc
                .state
                .open_launches("ops")
                .await
                .unwrap()
                .is_empty()
        );
        {
            let objects = objects.lock().unwrap();
            assert!(objects.pod.is_none());
            assert!(objects.secret.is_none());
            assert!(objects.workers.is_empty());
        }
        assert_eq!(received(&server, "POST", PODS).await.len(), pods_before);
        assert_eq!(
            received(&server, "POST", SECRETS).await.len(),
            secrets_before
        );
        assert_eq!(
            received(&server, "POST", "/api/v1/teams/ops/workers")
                .await
                .len(),
            registrations_before
        );
    }
}

#[tokio::test]
async fn catch_up_retains_open_cleanup_across_restart_until_objects_disappear() {
    for deleting in [LostResponse::PodDelete, LostResponse::SecretDelete] {
        let server = MockServer::start().await;
        let objects = prepared_objects(&server, LostResponse::None).await;
        let directory = tempfile::tempdir().unwrap();
        let state_path = directory.path().join("launches.sqlite");
        let mut fixture = common::kubernetes_service(&server.uri()).await;
        Arc::get_mut(&mut fixture.svc).unwrap().state = State::open(&state_path).await.unwrap();
        Arc::get_mut(&mut fixture.svc).unwrap().conn.max_launches = 1;
        let launch = prepare(&fixture.svc, &build_profile(), OffsetDateTime::now_utc()).await;
        catch_up(&fixture.svc).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/teams/ops/workers/demand"))
            .respond_with(envelope(json!({
                "builds": [{
                    "architecture": "aarch64-linux", "required_features": [], "build_count": 1
                }],
                "network": 0, "secrets": 0, "eval": 0
            })))
            .with_priority(1)
            .up_to_n_times(2)
            .expect(2)
            .mount(&server)
            .await;
        {
            let mut objects = objects.lock().unwrap();
            objects.pod.as_mut().unwrap()["status"] = json!({"phase": "Failed"});
            match deleting {
                LostResponse::PodDelete => objects.defer_pod_deletion = true,
                LostResponse::SecretDelete => objects.defer_secret_deletion = true,
                _ => unreachable!(),
            }
        }
        catch_up(&fixture.svc).await;
        let cleaning = fixture
            .svc
            .state
            .launch("ops", launch.id)
            .await
            .unwrap()
            .unwrap();
        assert!(cleaning.state.is_open(), "{deleting:?}");
        assert_eq!(cleaning.cleanup_outcome, Some(LaunchState::Failed));
        assert_eq!(
            fixture.svc.state.open_launches("ops").await.unwrap(),
            vec![cleaning.clone()]
        );
        {
            let objects = objects.lock().unwrap();
            let object = match deleting {
                LostResponse::PodDelete => objects.pod.as_ref().unwrap(),
                LostResponse::SecretDelete => objects.secret.as_ref().unwrap(),
                _ => unreachable!(),
            };
            assert!(object["metadata"]["deletionTimestamp"].is_string());
            assert_eq!(objects.workers.len(), 1);
        }
        drop(fixture);
        let mut restarted = common::kubernetes_service(&server.uri()).await;
        Arc::get_mut(&mut restarted.svc).unwrap().state = State::open(&state_path).await.unwrap();
        Arc::get_mut(&mut restarted.svc).unwrap().conn.max_launches = 1;
        assert_eq!(
            restarted.svc.state.launch("ops", launch.id).await.unwrap(),
            Some(cleaning.clone())
        );
        catch_up(&restarted.svc).await;
        assert_eq!(
            restarted.svc.state.open_launches("ops").await.unwrap(),
            vec![cleaning]
        );
        {
            let mut objects = objects.lock().unwrap();
            match deleting {
                LostResponse::PodDelete => {
                    objects.pod = None;
                    objects.defer_pod_deletion = false;
                }
                LostResponse::SecretDelete => {
                    objects.secret = None;
                    objects.defer_secret_deletion = false;
                }
                _ => unreachable!(),
            }
        }
        catch_up(&restarted.svc).await;
        assert_eq!(
            restarted
                .svc
                .state
                .launch("ops", launch.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            LaunchState::Failed
        );
        assert!(
            restarted
                .svc
                .state
                .open_launches("ops")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(objects.lock().unwrap().workers.is_empty());
        assert_eq!(received(&server, "POST", PODS).await.len(), 1);
        assert_eq!(received(&server, "POST", SECRETS).await.len(), 1);
        assert_eq!(
            received(&server, "POST", "/api/v1/teams/ops/workers")
                .await
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn catch_up_recovers_lost_delete_responses_without_recreating_objects() {
    for lost in [
        LostResponse::PodDelete,
        LostResponse::SecretDelete,
        LostResponse::RegistrationDelete,
    ] {
        let server = MockServer::start().await;
        let objects = prepared_objects(&server, lost).await;
        let directory = tempfile::tempdir().unwrap();
        let state_path = directory.path().join("launches.sqlite");
        let mut fixture = common::kubernetes_service(&server.uri()).await;
        Arc::get_mut(&mut fixture.svc).unwrap().state = State::open(&state_path).await.unwrap();
        let launch = prepare(&fixture.svc, &build_profile(), OffsetDateTime::now_utc()).await;
        catch_up(&fixture.svc).await;
        objects.lock().unwrap().pod.as_mut().unwrap()["status"] = json!({"phase": "Failed"});
        catch_up(&fixture.svc).await;
        let cleaning = fixture
            .svc
            .state
            .launch("ops", launch.id)
            .await
            .unwrap()
            .unwrap();
        assert!(cleaning.state.is_open(), "{lost:?}");
        assert_eq!(cleaning.cleanup_outcome, Some(LaunchState::Failed));
        assert_eq!(objects.lock().unwrap().lost, LostResponse::None);
        drop(fixture);
        let mut restarted = common::kubernetes_service(&server.uri()).await;
        Arc::get_mut(&mut restarted.svc).unwrap().state = State::open(&state_path).await.unwrap();
        assert_eq!(
            restarted.svc.state.launch("ops", launch.id).await.unwrap(),
            Some(cleaning)
        );
        catch_up(&restarted.svc).await;
        let saved = restarted
            .svc
            .state
            .launch("ops", launch.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.state, LaunchState::Failed);
        assert_eq!(saved.cleanup_outcome, Some(LaunchState::Failed));
        assert!(
            restarted
                .svc
                .state
                .open_launches("ops")
                .await
                .unwrap()
                .is_empty()
        );
        {
            let objects = objects.lock().unwrap();
            assert!(objects.pod.is_none());
            assert!(objects.secret.is_none());
            assert!(objects.workers.is_empty());
        }
        assert_eq!(received(&server, "POST", PODS).await.len(), 1);
        assert_eq!(received(&server, "POST", SECRETS).await.len(), 1);
        assert_eq!(
            received(&server, "POST", "/api/v1/teams/ops/workers")
                .await
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn catch_up_refuses_cleanup_after_frozen_target_or_team_changes() {
    use std::io::Write as _;
    for change in ["team", "team_id", "gradient", "namespace", "api"] {
        let server = MockServer::start().await;
        let objects = prepared_objects(&server, LostResponse::None).await;
        let mut fixture = common::kubernetes_service(&server.uri()).await;
        let launch = prepare(&fixture.svc, &build_profile(), OffsetDateTime::now_utc()).await;
        catch_up(&fixture.svc).await;
        let launch = fixture
            .svc
            .state
            .launch("ops", launch.id)
            .await
            .unwrap()
            .unwrap();
        fixture
            .svc
            .state
            .begin_build_cleanup(&launch, LaunchState::Failed)
            .await
            .unwrap();
        objects.lock().unwrap().pod.as_mut().unwrap()["status"] = json!({"phase": "Failed"});
        let before = objects.lock().unwrap().clone();
        let mut token = tempfile::NamedTempFile::new().unwrap();
        writeln!(token, "service-account-token").unwrap();
        let svc = Arc::get_mut(&mut fixture.svc).unwrap();
        match change {
            "team" => svc.conn.team = "different".into(),
            "team_id" => {
                Mock::given(method("GET"))
                    .and(path("/api/v1/teams/ops"))
                    .respond_with(envelope(json!({"id": Uuid::from_u128(7)})))
                    .with_priority(1)
                    .mount(&server)
                    .await;
            }
            "gradient" => {
                svc.gradient = gradient_autoscaler::gradient::GradientClient::new(
                    Url::parse(&format!("{}/different/", server.uri())).unwrap(),
                    "GRADkey".into(),
                );
            }
            "namespace" | "api" => {
                if let gradient_autoscaler::launcher::Launcher::Kubernetes(launcher) =
                    &mut svc.launcher
                {
                    launcher.client = KubernetesClient::with_api_url(
                        Url::parse(&format!(
                            "{}{}",
                            server.uri(),
                            if change == "api" { "/different/" } else { "" }
                        ))
                        .unwrap(),
                        token.path().to_owned(),
                        if change == "namespace" {
                            "different"
                        } else {
                            "ci"
                        },
                    );
                }
            }
            _ => unreachable!(),
        }
        catch_up(&fixture.svc).await;
        let saved = fixture
            .svc
            .state
            .launch("ops", launch.id)
            .await
            .unwrap()
            .unwrap();
        assert!(saved.state.is_open(), "{change}");
        assert_eq!(saved.cleanup_outcome, Some(LaunchState::Failed));
        assert_eq!(*objects.lock().unwrap(), before, "{change}");
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|request| request.method != "DELETE"),
            "{change}"
        );
        assert_eq!(received(&server, "POST", PODS).await.len(), 1);
        assert_eq!(received(&server, "POST", SECRETS).await.len(), 1);
        assert_eq!(
            received(&server, "POST", "/api/v1/teams/ops/workers")
                .await
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn catch_up_never_destroys_connected_work_on_transient_read_errors() {
    for observed in [false, true] {
        for route in ["team", "workers", "pod", "secret"] {
            let server = MockServer::start().await;
            let objects = prepared_objects(&server, LostResponse::None).await;
            let mut fixture = common::kubernetes_service(&server.uri()).await;
            let launch = prepare(&fixture.svc, &build_profile(), OffsetDateTime::now_utc()).await;
            catch_up(&fixture.svc).await;
            {
                let mut objects = objects.lock().unwrap();
                objects.pod.as_mut().unwrap()["status"] = json!({"phase": "Running"});
                objects.workers[0]["connected"] = json!(true);
            }
            if observed {
                catch_up(&fixture.svc).await;
                assert_eq!(
                    fixture
                        .svc
                        .state
                        .launch("ops", launch.id)
                        .await
                        .unwrap()
                        .unwrap()
                        .state,
                    LaunchState::Connected
                );
            }
            let mut expired = fixture
                .svc
                .state
                .launch("ops", launch.id)
                .await
                .unwrap()
                .unwrap();
            expired.dispatched_at = OffsetDateTime::now_utc() - Duration::minutes(3);
            expired.build_intent.as_mut().unwrap().startup_deadline =
                expired.dispatched_at + build_policy().startup_grace;
            let svc = Arc::get_mut(&mut fixture.svc).unwrap();
            svc.state = State::memory().await.unwrap();
            svc.state.insert_launch(&expired).await.unwrap();
            let route = match route {
                "team" => "/api/v1/teams/ops".into(),
                "workers" => "/api/v1/teams/ops/workers".into(),
                "pod" => format!("{PODS}/{}", launch.external_id),
                "secret" => format!("{SECRETS}/{}", launch.external_id),
                _ => unreachable!(),
            };
            Mock::given(method("GET"))
                .and(path(&route))
                .respond_with(ResponseTemplate::new(500))
                .with_priority(1)
                .up_to_n_times(1)
                .expect(1)
                .mount(&server)
                .await;
            let before = objects.lock().unwrap().clone();
            catch_up(&fixture.svc).await;
            let saved = fixture
                .svc
                .state
                .launch("ops", launch.id)
                .await
                .unwrap()
                .unwrap();
            assert!(saved.state.is_open(), "{route}, observed={observed}");
            assert_eq!(saved.cleanup_outcome, None, "{route}, observed={observed}");
            assert_eq!(*objects.lock().unwrap(), before);
            catch_up(&fixture.svc).await;
            assert_eq!(
                fixture
                    .svc
                    .state
                    .launch("ops", launch.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                LaunchState::Connected
            );
            assert!(
                server
                    .received_requests()
                    .await
                    .unwrap()
                    .iter()
                    .all(|request| request.method != "DELETE")
            );
            assert_eq!(received(&server, "POST", PODS).await.len(), 1);
            assert_eq!(received(&server, "POST", SECRETS).await.len(), 1);
        }
    }
}

#[tokio::test]
async fn catch_up_does_not_advance_connected_registrations_outside_the_frozen_target() {
    use std::io::Write as _;
    for change in ["namespace", "api", "pod_uid"] {
        let server = MockServer::start().await;
        let objects = prepared_objects(&server, LostResponse::None).await;
        let mut fixture = common::kubernetes_service(&server.uri()).await;
        let launch = prepare(&fixture.svc, &build_profile(), OffsetDateTime::now_utc()).await;
        catch_up(&fixture.svc).await;
        {
            let mut objects = objects.lock().unwrap();
            objects.workers[0]["connected"] = json!(true);
            objects.workers[0]["live"] = json!({
                "architectures": ["x86_64-linux"], "system_features": ["kvm"],
                "max_concurrent_builds": 4, "ram_total_mb": 16_000
            });
            objects.pod.as_mut().unwrap()["status"] = json!({"phase": "Running"});
            if change == "pod_uid" {
                objects.pod.as_mut().unwrap()["metadata"]["uid"] = json!("foreign-pod");
            }
        }
        let mut token = tempfile::NamedTempFile::new().unwrap();
        writeln!(token, "service-account-token").unwrap();
        if change != "pod_uid"
            && let gradient_autoscaler::launcher::Launcher::Kubernetes(launcher) =
                &mut Arc::get_mut(&mut fixture.svc).unwrap().launcher
        {
            launcher.client = KubernetesClient::with_api_url(
                Url::parse(&format!(
                    "{}{}",
                    server.uri(),
                    if change == "api" { "/different/" } else { "" }
                ))
                .unwrap(),
                token.path().to_owned(),
                if change == "namespace" {
                    "different"
                } else {
                    "ci"
                },
            );
        }
        let before = fixture.svc.state.launch("ops", launch.id).await.unwrap();
        let objects_before = objects.lock().unwrap().clone();
        catch_up(&fixture.svc).await;
        assert_eq!(
            fixture.svc.state.launch("ops", launch.id).await.unwrap(),
            before,
            "{change}"
        );
        assert_eq!(*objects.lock().unwrap(), objects_before);
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|request| request.method != "DELETE"),
            "{change}"
        );
    }
}

#[tokio::test]
async fn catch_up_uid_conflicts_leave_replaced_objects_and_registration_open() {
    for replacing in [LostResponse::PodDelete, LostResponse::SecretDelete] {
        let server = MockServer::start().await;
        let objects = prepared_objects(&server, LostResponse::None).await;
        let fixture = common::kubernetes_service(&server.uri()).await;
        let launch = prepare(&fixture.svc, &build_profile(), OffsetDateTime::now_utc()).await;
        catch_up(&fixture.svc).await;
        {
            let mut objects = objects.lock().unwrap();
            objects.pod.as_mut().unwrap()["status"] = json!({"phase": "Failed"});
            objects.replace_on_delete = Some(replacing);
        }
        catch_up(&fixture.svc).await;
        let cleaning = fixture
            .svc
            .state
            .launch("ops", launch.id)
            .await
            .unwrap()
            .unwrap();
        assert!(cleaning.state.is_open());
        assert_eq!(cleaning.cleanup_outcome, Some(LaunchState::Failed));
        let (route, expected_uid) = match replacing {
            LostResponse::PodDelete => (PODS, "pod-uid"),
            LostResponse::SecretDelete => (SECRETS, "secret-uid"),
            _ => unreachable!(),
        };
        let deletions = received(
            &server,
            "DELETE",
            &format!("{route}/{}", launch.external_id),
        )
        .await;
        assert_eq!(deletions.len(), 1);
        assert_eq!(
            json_of(&deletions[0])["preconditions"],
            json!({"uid": expected_uid})
        );
        {
            let objects = objects.lock().unwrap();
            let object = match replacing {
                LostResponse::PodDelete => objects.pod.as_ref().unwrap(),
                LostResponse::SecretDelete => objects.secret.as_ref().unwrap(),
                _ => unreachable!(),
            };
            assert_eq!(object["metadata"]["uid"], "replaced-object");
            assert_eq!(objects.workers.len(), 1);
        }
        let before = objects.lock().unwrap().clone();
        let deletions_before = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.method == "DELETE")
            .count();
        catch_up(&fixture.svc).await;
        assert_eq!(
            fixture.svc.state.launch("ops", launch.id).await.unwrap(),
            Some(cleaning)
        );
        assert_eq!(*objects.lock().unwrap(), before);
        assert_eq!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| request.method == "DELETE")
                .count(),
            deletions_before
        );
        assert!(
            received(
                &server,
                "DELETE",
                &format!("/api/v1/teams/ops/workers/{}", launch.worker_id.unwrap())
            )
            .await
            .is_empty()
        );
        assert_eq!(received(&server, "POST", PODS).await.len(), 1);
        assert_eq!(received(&server, "POST", SECRETS).await.len(), 1);
    }
}

#[tokio::test]
async fn catch_up_continues_after_a_prepared_launch_refuses_reconciliation() {
    let server = MockServer::start().await;
    let objects = prepared_objects(&server, LostResponse::None).await;
    let fixture = common::kubernetes_service(&server.uri()).await;
    let broken = prepare(
        &fixture.svc,
        &build_profile(),
        OffsetDateTime::now_utc() - Duration::seconds(10),
    )
    .await;
    fixture
        .svc
        .launcher
        .launch_prepared(&fixture.svc, broken.id)
        .await
        .unwrap();
    let healthy = prepare(&fixture.svc, &build_profile(), OffsetDateTime::now_utc()).await;
    let mut healthy_pod = healthy
        .build_intent
        .as_ref()
        .unwrap()
        .kubernetes
        .as_ref()
        .unwrap()
        .pod
        .clone();
    healthy_pod["metadata"]["uid"] = json!("healthy-pod");
    healthy_pod["status"] = json!({"phase": "Running"});
    let healthy_secret = {
        let mut objects = objects.lock().unwrap();
        let mut secret = objects.secret.as_ref().unwrap().clone();
        secret["metadata"]["name"] = json!(healthy.external_id);
        secret["metadata"]["uid"] = json!("healthy-secret");
        secret["metadata"]["labels"]["gradient.wavelens.io/launch"] = json!(healthy.id);
        secret["metadata"]["ownerReferences"][0]["name"] = json!(healthy.external_id);
        secret["metadata"]["ownerReferences"][0]["uid"] = json!("healthy-pod");
        let mut worker = objects.workers[0].clone();
        worker["worker_id"] = json!(healthy.worker_id.unwrap());
        worker["display_name"] = json!(format!("k8s-x86-{}", healthy.id));
        worker["connected"] = json!(true);
        objects.workers.push(worker);
        objects.pod.as_mut().unwrap()["metadata"]["uid"] = json!("foreign-pod");
        secret
    };
    assert!(
        fixture
            .svc
            .state
            .claim_build_registration(&healthy)
            .await
            .unwrap()
    );
    fixture
        .svc
        .state
        .bind_build_objects(&healthy, "healthy-secret", Some("healthy-pod"))
        .await
        .unwrap();
    for (route, body) in [
        (format!("{PODS}/{}", healthy.external_id), healthy_pod),
        (format!("{SECRETS}/{}", healthy.external_id), healthy_secret),
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .with_priority(1)
            .mount(&server)
            .await;
    }
    let before = fixture.svc.state.open_launches("ops").await.unwrap();
    assert_eq!(
        before.iter().map(|launch| launch.id).collect::<Vec<_>>(),
        vec![broken.id, healthy.id]
    );
    catch_up(&fixture.svc).await;
    assert_eq!(
        fixture.svc.state.launch("ops", broken.id).await.unwrap(),
        Some(before[0].clone())
    );
    assert_eq!(
        fixture
            .svc
            .state
            .launch("ops", healthy.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        LaunchState::Connected
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.method != "DELETE")
    );
    assert_eq!(received(&server, "POST", PODS).await.len(), 1);
    assert_eq!(received(&server, "POST", SECRETS).await.len(), 1);
    assert_eq!(
        received(&server, "POST", "/api/v1/teams/ops/workers")
            .await
            .len(),
        1
    );
}

#[tokio::test]
async fn preview_catch_up_does_not_resume_prepared_cleanup() {
    let server = MockServer::start().await;
    let objects = prepared_objects(&server, LostResponse::None).await;
    let mut fixture = common::kubernetes_service(&server.uri()).await;
    let launch = prepare(&fixture.svc, &build_profile(), OffsetDateTime::now_utc()).await;
    catch_up(&fixture.svc).await;
    let launch = fixture
        .svc
        .state
        .launch("ops", launch.id)
        .await
        .unwrap()
        .unwrap();
    fixture
        .svc
        .state
        .begin_build_cleanup(&launch, LaunchState::Failed)
        .await
        .unwrap();
    objects.lock().unwrap().pod.as_mut().unwrap()["status"] = json!({"phase": "Failed"});
    let svc = Arc::get_mut(&mut fixture.svc).unwrap();
    if let gradient_autoscaler::config::LauncherConfig::Kubernetes(config) = &mut svc.conn.launcher
    {
        config.pools.get_mut("x86").unwrap().build_resources = Some(
            serde_json::from_value(json!({
                "cpuCount": 4, "ramTotalMb": 16_000, "cpuCoreScore": 4_000
            }))
            .unwrap(),
        );
    }
    svc.conn.build_scaling = Some(BuildScalingConfig {
        mode: BuildScalingMode::Preview,
        profiles: vec!["x86".into()],
        bootstrap_profiles: vec![],
        queue_wait: 30,
        startup_grace: 120,
        max_new_workers: 2,
        fallback_ram_mb: 2_000,
    });
    let assessment = "/api/v1/teams/ops/workers/capacity-assessment";
    Mock::given(method("POST"))
        .and(path(assessment))
        .respond_with(|request: &Received| {
            let request = json_of(request);
            assert!(request.get("tracked_profiles").is_none());
            envelope(json!({
                "captured_at": "2026-11-19T20:00:00Z",
                "server_draining": false, "total_ready_builds": 0,
                "builds_truncated": false, "workers": [], "tracked_workers": [],
                "profiles": request["profiles"].as_array().unwrap().iter()
                    .map(|profile| profile["profile"].clone()).collect::<Vec<_>>(),
                "builds": []
            }))
        })
        .expect(2)
        .mount(&server)
        .await;
    let before = fixture.svc.state.open_launches("ops").await.unwrap();
    let objects_before = objects.lock().unwrap().clone();
    let requests_before = server.received_requests().await.unwrap().len();
    assert!(
        fixture
            .svc
            .launcher
            .reconcile_prepared(&fixture.svc, launch.id)
            .await
            .is_err()
    );
    catch_up(&fixture.svc).await;
    catch_up(&fixture.svc).await;
    assert_eq!(
        fixture.svc.state.open_launches("ops").await.unwrap(),
        before
    );
    assert_eq!(*objects.lock().unwrap(), objects_before);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len() - requests_before, 2);
    for request in &requests[requests_before..] {
        assert_eq!(request.method, "POST");
        assert_eq!(request.url.path(), assessment);
    }
}

#[tokio::test]
async fn prepared_launch_uses_frozen_manifest_after_configuration_reload() {
    let server = MockServer::start().await;
    let _objects = prepared_objects(&server, LostResponse::None).await;
    let mut fixture = common::kubernetes_service(&server.uri()).await;
    let mut profile = build_profile();
    profile.resources.build_cores = Some(2);
    profile.resources.system_features.push("signing-key".into());
    if let gradient_autoscaler::launcher::Launcher::Kubernetes(launcher) =
        &mut Arc::get_mut(&mut fixture.svc).unwrap().launcher
    {
        launcher
            .config
            .pools
            .get_mut("x86")
            .unwrap()
            .features
            .push("signing-key".into());
    }
    let launch = prepare(&fixture.svc, &profile, OffsetDateTime::now_utc()).await;
    let svc = Arc::get_mut(&mut fixture.svc).unwrap();
    if let gradient_autoscaler::launcher::Launcher::Kubernetes(launcher) = &mut svc.launcher {
        launcher.config.image = "changed-image".into();
        launcher.config.pools.clear();
    }
    svc.launcher.launch_prepared(svc, launch.id).await.unwrap();
    let requests = received(&server, "POST", PODS).await;
    assert_eq!(
        json_of(&requests[0])["spec"]["containers"][0]["image"],
        "registry.example.com/gradient-worker:2.0.0"
    );
    let manifest = json_of(&requests[0]);
    assert_eq!(
        manifest["metadata"]["annotations"]["gradient.wavelens.io/profile-environment"],
        "1"
    );
    let env = manifest["spec"]["containers"][0]["env"].as_array().unwrap();
    for (name, value) in [
        ("GRADIENT_WORKER_SYSTEM_ARCHITECTURES", "x86_64-linux"),
        ("GRADIENT_WORKER_SYSTEM_FEATURES", "kvm,signing-key"),
        ("GRADIENT_WORKER_SYSTEM_CPU_CORE_SCORE", "4000"),
        ("GRADIENT_WORKER_BUILD_MAX_CORES", "2"),
    ] {
        assert!(
            env.contains(&json!({"name": name, "value": value})),
            "{name}"
        );
    }
    let nix_config = env
        .iter()
        .find(|entry| entry["name"] == "NIX_CONFIG")
        .unwrap();
    assert_eq!(
        nix_config["value"],
        "extra-substituters = https://cache.example.com\nextra-trusted-public-keys = cache.example.com-1:abc=\nextra-system-features = kvm signing-key"
    );
    assert_eq!(
        svc.state
            .launch("ops", launch.id)
            .await
            .unwrap()
            .unwrap()
            .build_intent,
        launch.build_intent
    );
}

#[tokio::test]
async fn prepared_manifest_rejects_changes_to_profile_settings() {
    let server = MockServer::start().await;
    let _objects = prepared_objects(&server, LostResponse::None).await;
    let fixture = common::kubernetes_service(&server.uri()).await;
    let mut profile = build_profile();
    profile.resources.build_cores = Some(2);
    let launch = prepare(&fixture.svc, &profile, OffsetDateTime::now_utc()).await;
    for (name, value) in [
        ("GRADIENT_WORKER_SYSTEM_ARCHITECTURES", "aarch64-linux"),
        ("GRADIENT_WORKER_SYSTEM_FEATURES", "other-feature"),
        ("GRADIENT_WORKER_SYSTEM_CPU_CORE_SCORE", "5000"),
        ("GRADIENT_WORKER_BUILD_MAX_CORES", "3"),
        ("NIX_CONFIG", "extra-system-features = other-feature"),
    ] {
        let mut changed = launch.clone();
        let spec = changed
            .build_intent
            .as_mut()
            .unwrap()
            .kubernetes
            .as_mut()
            .unwrap();
        let env = spec.pod["spec"]["containers"][0]["env"]
            .as_array_mut()
            .unwrap();
        env.iter_mut().find(|entry| entry["name"] == name).unwrap()["value"] = json!(value);
        let state = gradient_autoscaler::state::State::memory().await.unwrap();
        assert!(state.insert_launch(&changed).await.is_err(), "{name}");
        assert!(state.open_launches("ops").await.unwrap().is_empty());
    }
    let mut changed = launch.clone();
    let spec = changed
        .build_intent
        .as_mut()
        .unwrap()
        .kubernetes
        .as_mut()
        .unwrap();
    spec.pod["metadata"]["annotations"]["gradient.wavelens.io/profile-environment"] = json!("99");
    let state = gradient_autoscaler::state::State::memory().await.unwrap();
    assert!(state.insert_launch(&changed).await.is_err());
}

#[tokio::test]
async fn older_saved_manifests_keep_their_original_environment() {
    let server = MockServer::start().await;
    let _objects = prepared_objects(&server, LostResponse::None).await;
    let mut fixture = common::kubernetes_service(&server.uri()).await;
    let mut launch = prepare(&fixture.svc, &build_profile(), OffsetDateTime::now_utc()).await;
    let intent = launch.build_intent.as_mut().unwrap();
    let mut old = serde_json::to_value(intent.kubernetes.as_ref().unwrap()).unwrap();
    old["pod"]["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("annotations");
    let env = old["pod"]["spec"]["containers"][0]["env"]
        .as_array_mut()
        .unwrap();
    env.retain(|entry| {
        ![
            "GRADIENT_WORKER_SYSTEM_ARCHITECTURES",
            "GRADIENT_WORKER_SYSTEM_FEATURES",
            "GRADIENT_WORKER_SYSTEM_CPU_CORE_SCORE",
            "GRADIENT_WORKER_BUILD_MAX_CORES",
        ]
        .iter()
        .any(|name| entry["name"] == *name)
    });
    env.iter_mut()
        .find(|entry| entry["name"] == "NIX_CONFIG")
        .unwrap()["value"] = json!(
        "extra-substituters = https://cache.example.com\nextra-trusted-public-keys = cache.example.com-1:abc="
    );
    let old_pod = old["pod"].clone();
    let old_spec: gradient_autoscaler::kubernetes::KubernetesLaunchSpec =
        serde_json::from_value(old).unwrap();
    assert!(old_spec.pod["metadata"].get("annotations").is_none());
    intent.kubernetes = Some(old_spec);
    let svc = Arc::get_mut(&mut fixture.svc).unwrap();
    svc.state = gradient_autoscaler::state::State::memory().await.unwrap();
    svc.state.insert_launch(&launch).await.unwrap();
    let restored = svc.state.launch("ops", launch.id).await.unwrap().unwrap();
    assert_eq!(restored.build_intent, launch.build_intent);
    svc.launcher.launch_prepared(svc, launch.id).await.unwrap();
    let requests = received(&server, "POST", PODS).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(json_of(&requests[0])["spec"], old_pod["spec"]);
}

#[tokio::test]
async fn prepared_launch_rejects_capacity_environment_overrides_before_http() {
    let server = MockServer::start().await;
    let mut config = common::kubernetes();
    config.pools.get_mut("x86").unwrap().pod_options = serde_json::from_value(json!({
        "env": [{"name": "GRADIENT_WORKER_SYSTEM_FEATURES", "value": "other-feature"}]
    }))
    .unwrap();
    let fixture = common::kubernetes_service_with_config(&server.uri(), config).await;
    let profile = build_profile();
    let planned = PlannedBuildWorker {
        profile_index: 0,
        label: "x86".into(),
        worker_id: profile.worker_id,
    };
    assert!(
        fixture
            .svc
            .launcher
            .prepare_build_launch(
                &fixture.svc,
                &planned,
                &profile,
                &build_policy(),
                OffsetDateTime::now_utc(),
            )
            .await
            .is_err()
    );
    assert!(
        fixture
            .svc
            .state
            .open_launches("ops")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn prepared_connection_keeps_history_without_overwriting_legacy_profiles() {
    let server = MockServer::start().await;
    let objects = prepared_objects(&server, LostResponse::None).await;
    let fixture = common::kubernetes_service(&server.uri()).await;
    let launch = prepare(&fixture.svc, &build_profile(), OffsetDateTime::now_utc()).await;
    fixture
        .svc
        .launcher
        .launch_prepared(&fixture.svc, launch.id)
        .await
        .unwrap();
    let current = gradient_autoscaler::kinds::Profile {
        systems: vec!["x86_64-linux".into()],
        features: vec!["current-feature".into()],
        max_jobs: 8,
        ram_mb: Some(32_000),
    };
    fixture
        .svc
        .state
        .put_profile("ops", "x86", &current)
        .await
        .unwrap();
    {
        let mut objects = objects.lock().unwrap();
        objects.workers[0]["connected"] = json!(true);
        objects.workers[0]["live"] = json!({
            "architectures": ["x86_64-linux"], "system_features": ["kvm"],
            "max_concurrent_builds": 4, "ram_total_mb": 16_000
        });
    }
    fixture
        .svc
        .launcher
        .reconcile_prepared(&fixture.svc, launch.id)
        .await
        .unwrap();
    let saved = fixture
        .svc
        .state
        .launch("ops", launch.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.state, LaunchState::Connected);
    assert_eq!(saved.build_intent, launch.build_intent);
    assert_eq!(
        fixture.svc.state.profile("ops", "x86").await.unwrap(),
        Some(current)
    );
}

#[tokio::test]
async fn a_bound_secret_missing_before_registration_is_not_recreated() {
    let server = MockServer::start().await;
    let objects = prepared_objects(&server, LostResponse::None).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/teams/ops/workers"))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let fixture = common::kubernetes_service(&server.uri()).await;
    let launch = prepare(&fixture.svc, &build_profile(), OffsetDateTime::now_utc()).await;
    assert!(
        fixture
            .svc
            .launcher
            .launch_prepared(&fixture.svc, launch.id)
            .await
            .is_err()
    );
    let saved = fixture
        .svc
        .state
        .launch("ops", launch.id)
        .await
        .unwrap()
        .unwrap();
    assert!(saved.secret_uid.is_some());
    assert!(!saved.registration_started);
    objects.lock().unwrap().secret = None;
    assert!(
        fixture
            .svc
            .launcher
            .launch_prepared(&fixture.svc, launch.id)
            .await
            .is_err()
    );
    assert_eq!(received(&server, "POST", SECRETS).await.len(), 1);
    assert!(received(&server, "POST", PODS).await.is_empty());
    assert!(
        received(&server, "POST", "/api/v1/teams/ops/workers")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn prepared_launch_does_not_recreate_missing_credentials_or_foreign_owned_objects() {
    use base64::Engine as _;
    for corruption in [
        "owner",
        "secret_label",
        "pod_label",
        "worker",
        "permission",
        "registration_name",
        "team_credential",
        "credential",
        "secret_uid",
        "pod_uid",
        "immutable",
        "automatic_cores",
        "automatic_features",
        "automatic_score",
    ] {
        let server = MockServer::start().await;
        let objects = prepared_objects(&server, LostResponse::None).await;
        let mut fixture = common::kubernetes_service(&server.uri()).await;
        let mut profile = build_profile();
        if corruption == "automatic_features" {
            profile.resources.system_features.clear();
            if let gradient_autoscaler::launcher::Launcher::Kubernetes(launcher) =
                &mut Arc::get_mut(&mut fixture.svc).unwrap().launcher
            {
                launcher
                    .config
                    .pools
                    .get_mut("x86")
                    .unwrap()
                    .features
                    .clear();
            }
        }
        if corruption == "automatic_score" {
            profile.resources.cpu_core_score = 0;
        }
        let launch = prepare(&fixture.svc, &profile, OffsetDateTime::now_utc()).await;
        fixture
            .svc
            .launcher
            .launch_prepared(&fixture.svc, launch.id)
            .await
            .unwrap();
        {
            let mut objects = objects.lock().unwrap();
            match corruption {
                "owner" => {
                    objects.secret.as_mut().unwrap()["metadata"]["ownerReferences"][0]["uid"] =
                        json!("foreign-uid")
                }
                "secret_label" => {
                    objects.secret.as_mut().unwrap()["metadata"]["labels"]["gradient.wavelens.io/launch"] =
                        json!(Uuid::new_v4());
                }
                "pod_label" => {
                    objects.pod.as_mut().unwrap()["metadata"]["labels"]["gradient.wavelens.io/launch"] =
                        json!(Uuid::new_v4());
                }
                "worker" => {
                    let env = objects.pod.as_mut().unwrap()["spec"]["containers"][0]["env"]
                        .as_array_mut()
                        .unwrap();
                    env.iter_mut()
                        .find(|var| var["name"] == "GRADIENT_WORKER_ID")
                        .unwrap()["value"] = json!(Uuid::new_v4());
                }
                "permission" => objects.workers[0]["enable_network"] = json!(false),
                "registration_name" => objects.workers[0]["display_name"] = json!("foreign-worker"),
                "team_credential" => {
                    let secret = objects.secret.as_mut().unwrap();
                    let peers = base64::engine::general_purpose::STANDARD
                        .decode(secret["data"]["peers"].as_str().unwrap())
                        .unwrap();
                    let peers = std::str::from_utf8(&peers).unwrap();
                    let (_, token) = peers.split_once(':').unwrap();
                    secret["data"]["peers"] = json!(
                        base64::engine::general_purpose::STANDARD
                            .encode(format!("{}:{token}", Uuid::from_u128(7)))
                    );
                }
                "credential" => objects.secret = None,
                "secret_uid" => {
                    objects.secret.as_mut().unwrap()["metadata"]["uid"] = json!("replaced-secret")
                }
                "pod_uid" => {
                    objects.pod.as_mut().unwrap()["metadata"]["uid"] = json!("replaced-pod")
                }
                "immutable" => objects.secret.as_mut().unwrap()["immutable"] = json!(false),
                "automatic_cores" | "automatic_features" | "automatic_score" => {
                    let (name, value) = match corruption {
                        "automatic_cores" => ("GRADIENT_WORKER_BUILD_MAX_CORES", "1"),
                        "automatic_features" => {
                            ("GRADIENT_WORKER_SYSTEM_FEATURES", "other-feature")
                        }
                        "automatic_score" => ("GRADIENT_WORKER_SYSTEM_CPU_CORE_SCORE", "5000"),
                        _ => unreachable!(),
                    };
                    objects.pod.as_mut().unwrap()["spec"]["containers"][0]["env"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!({"name": name, "value": value}));
                }
                _ => unreachable!(),
            }
        }
        assert!(
            fixture
                .svc
                .launcher
                .launch_prepared(&fixture.svc, launch.id)
                .await
                .is_err(),
            "{corruption}"
        );
        if corruption != "credential" {
            objects.lock().unwrap().pod.as_mut().unwrap()["status"] = json!({"phase": "Failed"});
            let before = objects.lock().unwrap().clone();
            catch_up(&fixture.svc).await;
            let saved = fixture
                .svc
                .state
                .launch("ops", launch.id)
                .await
                .unwrap()
                .unwrap();
            assert!(saved.state.is_open(), "{corruption}");
            assert_eq!(saved.worker_id, launch.worker_id);
            assert_eq!(*objects.lock().unwrap(), before, "{corruption}");
            assert!(
                server
                    .received_requests()
                    .await
                    .unwrap()
                    .iter()
                    .all(|request| request.method != "DELETE"),
                "{corruption}"
            );
        }
        assert_eq!(received(&server, "POST", PODS).await.len(), 1);
        assert_eq!(received(&server, "POST", SECRETS).await.len(), 1);
        assert_eq!(
            received(&server, "POST", "/api/v1/teams/ops/workers")
                .await
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn prepared_launch_rejects_scope_or_permission_changes_before_creating_resources() {
    for change in ["team", "permissions", "namespace"] {
        let server = MockServer::start().await;
        let _objects = prepared_objects(&server, LostResponse::None).await;
        let mut fixture = common::kubernetes_service(&server.uri()).await;
        let launch = prepare(&fixture.svc, &build_profile(), OffsetDateTime::now_utc()).await;
        match change {
            "team" => {
                Mock::given(method("GET"))
                    .and(path("/api/v1/teams/ops"))
                    .respond_with(envelope(json!({"id": Uuid::from_u128(7)})))
                    .with_priority(1)
                    .mount(&server)
                    .await;
            }
            "permissions" => {
                Arc::get_mut(&mut fixture.svc)
                    .unwrap()
                    .conn
                    .worker_capabilities
                    .network = false
            }
            "namespace" => {
                if let gradient_autoscaler::launcher::Launcher::Kubernetes(launcher) =
                    &mut Arc::get_mut(&mut fixture.svc).unwrap().launcher
                {
                    launcher.client = KubernetesClient::with_api_url(
                        Url::parse(&server.uri()).unwrap(),
                        std::path::PathBuf::from("/missing"),
                        "different",
                    );
                }
            }
            _ => unreachable!(),
        }
        assert!(
            fixture
                .svc
                .launcher
                .launch_prepared(&fixture.svc, launch.id)
                .await
                .is_err(),
            "{change}"
        );
        assert!(received(&server, "POST", PODS).await.is_empty());
        assert!(received(&server, "POST", SECRETS).await.is_empty());
        assert!(
            received(&server, "POST", "/api/v1/teams/ops/workers")
                .await
                .is_empty()
        );
    }
}

#[tokio::test]
async fn prepared_github_launches_are_rejected_before_dispatch() {
    let server = MockServer::start().await;
    let svc = common::github_service(&server.uri()).await;
    let profile = build_profile();
    let planned = PlannedBuildWorker {
        profile_index: 0,
        label: "ubuntu-latest".into(),
        worker_id: profile.worker_id,
    };
    assert!(
        svc.launcher
            .prepare_build_launch(
                &svc,
                &planned,
                &profile,
                &build_policy(),
                OffsetDateTime::now_utc()
            )
            .await
            .is_err()
    );
    assert!(
        svc.launcher
            .launch_prepared(&svc, Uuid::new_v4())
            .await
            .is_err()
    );
    assert!(svc.state.open_launches("ops").await.unwrap().is_empty());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn expired_prepared_launch_neither_creates_resources_nor_enters_legacy_cleanup() {
    let server = MockServer::start().await;
    let _objects = prepared_objects(&server, LostResponse::None).await;
    let fixture = common::kubernetes_service(&server.uri()).await;
    let launch = prepare(
        &fixture.svc,
        &build_profile(),
        OffsetDateTime::now_utc() - Duration::minutes(3),
    )
    .await;
    assert!(
        fixture
            .svc
            .launcher
            .launch_prepared(&fixture.svc, launch.id)
            .await
            .is_err()
    );
    assert!(!fixture.svc.state.claim_cancel(launch.id).await.unwrap());
    assert!(
        gradient_autoscaler::events::finish_launch(&fixture.svc, &launch)
            .await
            .is_err()
    );
    assert_eq!(
        fixture.svc.state.open_launches("ops").await.unwrap().len(),
        1
    );
    assert!(received(&server, "POST", PODS).await.is_empty());
    assert!(received(&server, "POST", SECRETS).await.is_empty());
    assert!(
        received(&server, "POST", "/api/v1/teams/ops/workers")
            .await
            .is_empty()
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.method != "DELETE")
    );
}

#[tokio::test]
async fn scale_starts_a_pod_whose_secret_holds_the_registered_token() {
    let server = MockServer::start().await;
    let mut config = common::kubernetes();
    config.pools.get_mut("x86").unwrap().pod_options = serde_json::from_value(json!({
        "volumes": [{"name": "fetch-auth", "secret": {"secretName": "worker-fetch-auth"}}],
        "volumeMounts": [{"name": "fetch-auth", "mountPath": "/creds", "readOnly": true}],
        "env": [{"name": "FETCH_MODE", "valueFrom": {"configMapKeyRef": {"name": "worker-config", "key": "mode"}}}]
    })).unwrap();
    let mut fixture = common::kubernetes_service_with_config(&server.uri(), config).await;
    Arc::get_mut(&mut fixture.svc)
        .unwrap()
        .conn
        .worker_capabilities = WorkerCapabilities {
        eval: false,
        secrets: false,
        exec: true,
        ..Default::default()
    };
    let svc = &fixture.svc;
    demand(
        &server,
        json!([{ "architecture": "x86_64-linux", "required_features": ["kvm"], "build_count": 3 }]),
    )
    .await;
    let team = Uuid::from_u128(5);
    let registered = envelope(json!({ "team": team, "token": "worker-token" }));
    mount(
        &server,
        "POST",
        "/api/v1/teams/ops/workers",
        registered,
        Some(1),
    )
    .await;
    Mock::given(method("POST"))
        .and(path(PODS))
        .and(header("Authorization", "Bearer service-account-token"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "metadata": { "name": "created", "uid": "pod-uid", "labels": { "gradient.wavelens.io/connection": "ops" } },
        })))
        .expect(1)
        .mount(&server)
        .await;
    mount(
        &server,
        "POST",
        SECRETS,
        ResponseTemplate::new(201),
        Some(1),
    )
    .await;

    scale(svc).await.unwrap();

    let launches = svc.state.open_launches("ops").await.unwrap();
    assert_eq!(launches.len(), 1);
    let launch = &launches[0];
    assert_eq!(launch.label, "x86");
    assert!(!launch.swapped);

    let registration = json_of(&received(&server, "POST", "/api/v1/teams/ops/workers").await[0]);
    let worker_id = registration["worker_id"].as_str().unwrap();
    assert_eq!(launch.worker_id.unwrap().to_string(), worker_id);
    assert_eq!(registration["enable_build"], true);
    assert_eq!(registration["enable_network"], true);
    assert_eq!(registration["enable_eval"], false);
    assert_eq!(registration["enable_secrets"], false);
    assert_eq!(registration["enable_exec"], true);

    let manifest = json_of(&received(&server, "POST", PODS).await[0]);
    assert_eq!(manifest["metadata"]["name"], launch.external_id);
    assert_eq!(
        manifest["metadata"]["labels"]["gradient.wavelens.io/launch"],
        launch.id.to_string()
    );
    let env = manifest["spec"]["containers"][0]["env"].as_array().unwrap();
    assert!(
        env.iter()
            .any(|var| var["name"] == "GRADIENT_WORKER_ID" && var["value"] == worker_id)
    );
    assert!(!manifest.to_string().contains("worker-token"));
    for (role, enabled) in [
        ("BUILD", "true"),
        ("NETWORK", "true"),
        ("EVAL", "false"),
        ("SECRETS", "false"),
        ("EXEC", "true"),
    ] {
        assert!(env.iter().any(|var| {
            var["name"] == format!("GRADIENT_WORKER_CAPABILITIES_{role}") && var["value"] == enabled
        }));
    }
    assert!(
        manifest["spec"]["volumes"].as_array().unwrap().contains(
            &json!({"name": "fetch-auth", "secret": {"secretName": "worker-fetch-auth"}})
        )
    );
    assert!(
        manifest["spec"]["containers"][0]["volumeMounts"]
            .as_array()
            .unwrap()
            .contains(&json!({"name": "fetch-auth", "mountPath": "/creds", "readOnly": true}))
    );
    assert!(env.iter().any(|var| var
        == &json!({
            "name": "FETCH_MODE",
            "valueFrom": {"configMapKeyRef": {"name": "worker-config", "key": "mode"}}
        })));

    let secret = json_of(&received(&server, "POST", SECRETS).await[0]);
    assert_eq!(
        secret["stringData"]["peers"],
        format!("{team}:worker-token\n")
    );
    assert_eq!(secret["metadata"]["ownerReferences"][0]["uid"], "pod-uid");
}

#[tokio::test]
async fn scale_honors_build_and_transfer_capabilities_and_retains_pending_pods() {
    for (architecture, features, build, network, can_build, can_network, expected) in [
        ("builtin", vec!["kvm"], true, false, true, false, true),
        ("builtin", vec!["cuda"], true, false, true, true, false),
        ("builtin", vec![], true, false, false, true, false),
        ("builtin", vec![], false, true, true, false, false),
        ("builtin", vec![], true, true, true, false, false),
        ("x86_64-linux", vec![], true, true, false, true, false),
        ("x86_64-linux", vec![], true, true, true, false, false),
        ("x86_64-linux", vec!["kvm"], true, true, true, true, true),
        (
            "riscv64-linux",
            vec!["cuda"],
            false,
            true,
            false,
            true,
            true,
        ),
    ] {
        let server = MockServer::start().await;
        let mut fixture = common::kubernetes_service(&server.uri()).await;
        Arc::get_mut(&mut fixture.svc)
            .unwrap()
            .conn
            .worker_capabilities = WorkerCapabilities {
            build: can_build,
            network: can_network,
            eval: false,
            secrets: false,
            ..Default::default()
        };
        demand(
            &server,
            json!([{
                "architecture": architecture, "required_features": features, "build_count": 3,
                "build": build, "network": network
            }]),
        )
        .await;
        let times = u64::from(expected);
        mount(
            &server,
            "POST",
            "/api/v1/teams/ops/workers",
            envelope(json!({"team": Uuid::nil(), "token": "worker-token"})),
            Some(times),
        )
        .await;
        mount(
            &server,
            "POST",
            PODS,
            ResponseTemplate::new(201).set_body_json(json!({
                "metadata": {"name": "created", "uid": "pod-uid"}
            })),
            Some(times),
        )
        .await;
        mount(
            &server,
            "POST",
            SECRETS,
            ResponseTemplate::new(201),
            Some(times),
        )
        .await;

        scale(&fixture.svc).await.unwrap();
        let launches = fixture.svc.state.open_launches("ops").await.unwrap();
        assert_eq!(
            launches.len(),
            usize::from(expected),
            "{architecture}: requires {build}/{network}, supplies {can_build}/{can_network}"
        );
        if expected {
            let registration =
                json_of(&received(&server, "POST", "/api/v1/teams/ops/workers").await[0]);
            assert_eq!(registration["enable_build"], can_build);
            assert_eq!(registration["enable_network"], can_network);
            scale(&fixture.svc).await.unwrap();
            assert_eq!(
                fixture.svc.state.open_launches("ops").await.unwrap(),
                launches
            );
        }
    }
}

#[tokio::test]
async fn rejected_pod_unregisters_its_worker_and_records_no_launch() {
    let server = MockServer::start().await;
    let fixture = common::kubernetes_service(&server.uri()).await;
    demand(
        &server,
        json!([{ "architecture": "aarch64-linux", "required_features": [], "build_count": 1 }]),
    )
    .await;
    let registered = envelope(json!({ "team": Uuid::nil(), "token": "worker-token" }));
    mount(
        &server,
        "POST",
        "/api/v1/teams/ops/workers",
        registered,
        Some(1),
    )
    .await;
    mount(&server, "POST", PODS, ResponseTemplate::new(403), Some(1)).await;
    mount(
        &server,
        "POST",
        SECRETS,
        ResponseTemplate::new(201),
        Some(0),
    )
    .await;
    Mock::given(method("DELETE"))
        .and(wiremock::matchers::path_regex(
            "^/api/v1/teams/ops/workers/.+",
        ))
        .respond_with(envelope(json!("deleted")))
        .expect(1)
        .mount(&server)
        .await;

    scale(&fixture.svc).await.unwrap();

    assert!(
        fixture
            .svc
            .state
            .open_launches("ops")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn pending_pod_is_removed_once_demand_is_gone() {
    let server = MockServer::start().await;
    let fixture = common::kubernetes_service(&server.uri()).await;
    let svc = &fixture.svc;
    demand(&server, json!([])).await;
    let launch = open_launch(svc, LaunchState::Queued).await;
    let pod_path = format!("{PODS}/{}", launch.external_id);
    mount(
        &server,
        "DELETE",
        &pod_path,
        ResponseTemplate::new(200),
        Some(1),
    )
    .await;
    let worker_path = format!("/api/v1/teams/ops/workers/{}", launch.worker_id.unwrap());
    mount(
        &server,
        "DELETE",
        &worker_path,
        envelope(json!("deleted")),
        Some(1),
    )
    .await;

    scale(svc).await.unwrap();

    let stored = svc.state.launch("ops", launch.id).await.unwrap().unwrap();
    assert_eq!(stored.state, LaunchState::Cancelled);
}

#[tokio::test]
async fn catch_up_follows_pod_phases() {
    let server = MockServer::start().await;
    let fixture = common::kubernetes_service(&server.uri()).await;
    let svc = &fixture.svc;
    demand(&server, json!([])).await;
    let finished = open_launch(svc, LaunchState::Connected).await;
    let broken_image = open_launch(svc, LaunchState::Queued).await;
    let running = open_launch(svc, LaunchState::Running).await;
    let hour_ago = OffsetDateTime::now_utc() - Duration::hours(1);
    let never_scheduled = open_launch_since(svc, LaunchState::Queued, hour_ago).await;
    let open_pods: Vec<Value> = [&finished, &broken_image, &running, &never_scheduled]
        .iter()
        .map(|launch| json!({ "metadata": { "name": launch.external_id, "labels": { "gradient.wavelens.io/launch": launch.id.to_string() } } }))
        .collect();
    Mock::given(method("GET"))
        .and(path(PODS))
        .and(query_param(
            "labelSelector",
            "gradient.wavelens.io/connection=ops",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": open_pods })))
        .mount(&server)
        .await;
    mount(
        &server,
        "GET",
        "/api/v1/teams/ops/workers",
        envelope(json!([])),
        None,
    )
    .await;
    for (launch, phase, waiting) in [
        (&finished, "Succeeded", None),
        (&broken_image, "Pending", Some("ImagePullBackOff")),
        (&running, "Running", None),
        (&never_scheduled, "Pending", Some("ContainerCreating")),
    ] {
        let pod_path = format!("{PODS}/{}", launch.external_id);
        mount(
            &server,
            "GET",
            &pod_path,
            pod(&launch.external_id, phase, waiting),
            None,
        )
        .await;
        let removals = if launch.id == running.id { 0 } else { 1 };
        mount(
            &server,
            "DELETE",
            &pod_path,
            ResponseTemplate::new(200),
            Some(removals),
        )
        .await;
        let worker_path = format!("/api/v1/teams/ops/workers/{}", launch.worker_id.unwrap());
        mount(
            &server,
            "DELETE",
            &worker_path,
            envelope(json!("deleted")),
            Some(removals),
        )
        .await;
    }

    catch_up(svc).await;

    let state = |id| async move { svc.state.launch("ops", id).await.unwrap().unwrap().state };
    assert_eq!(state(finished.id).await, LaunchState::Completed);
    assert_eq!(state(broken_image.id).await, LaunchState::Failed);
    assert_eq!(state(running.id).await, LaunchState::Running);
    assert_eq!(state(never_scheduled.id).await, LaunchState::Failed);
}

#[tokio::test]
async fn api_server_path_prefix_is_kept() {
    let server = MockServer::start().await;
    let mut token = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut token, b"t").unwrap();
    let api = Url::parse(&format!("{}/k8s/clusters/c-1/", server.uri())).unwrap();
    let client = KubernetesClient::with_api_url(api, token.path().to_owned(), "ci");
    Mock::given(method("GET"))
        .and(path("/k8s/clusters/c-1/api/v1/namespaces/ci/pods/p"))
        .respond_with(pod("p", "Running", None))
        .expect(1)
        .mount(&server)
        .await;

    assert!(client.pod("p").await.unwrap().is_some());
}

#[tokio::test]
async fn catch_up_protects_other_connections_prepared_pods_after_connection_label_drift() {
    let server = MockServer::start().await;
    let objects = prepared_objects(&server, LostResponse::None).await;
    let fixture = common::kubernetes_service(&server.uri()).await;
    let launch = prepare(&fixture.svc, &build_profile(), OffsetDateTime::now_utc()).await;
    catch_up(&fixture.svc).await;
    let before = fixture.svc.state.launch("ops", launch.id).await.unwrap();
    objects.lock().unwrap().pod.as_mut().unwrap()["metadata"]["labels"]["gradient.wavelens.io/connection"] =
        json!("other");
    Mock::given(method("GET"))
        .and(path(PODS))
        .and(query_param(
            "labelSelector",
            "gradient.wavelens.io/connection=ops",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": []})))
        .with_priority(1)
        .mount(&server)
        .await;

    let mut other = common::kubernetes_service(&server.uri()).await;
    let svc = Arc::get_mut(&mut other.svc).unwrap();
    svc.conn.name = "other".into();
    svc.state = fixture.svc.state.clone();
    let pods = Arc::clone(&objects);
    Mock::given(method("GET"))
        .and(path(PODS))
        .and(query_param(
            "labelSelector",
            "gradient.wavelens.io/connection=other",
        ))
        .respond_with(move |_: &Received| {
            let objects = pods.lock().unwrap();
            ResponseTemplate::new(200).set_body_json(json!({
                "items": objects.pod.iter().collect::<Vec<_>>()
            }))
        })
        .mount(&server)
        .await;
    for svc in [&other.svc, &fixture.svc] {
        catch_up(svc).await;
    }
    assert_eq!(
        fixture.svc.state.launch("ops", launch.id).await.unwrap(),
        before
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| { request.method != "DELETE" })
    );
    assert!(objects.lock().unwrap().pod.is_some());
}

#[tokio::test]
async fn setup_deletes_pods_without_an_open_launch() {
    let server = MockServer::start().await;
    let fixture = common::kubernetes_service(&server.uri()).await;
    let svc = &fixture.svc;
    let kept = open_launch(svc, LaunchState::Running).await;
    let pods = json!({ "items": [
        { "metadata": { "name": kept.external_id, "labels": { "gradient.wavelens.io/launch": kept.id.to_string() } } },
        { "metadata": { "name": "gradient-worker-lost", "labels": { "gradient.wavelens.io/launch": Uuid::new_v4().to_string() } } },
    ] });
    mount(
        &server,
        "GET",
        PODS,
        ResponseTemplate::new(200).set_body_json(pods),
        None,
    )
    .await;
    mount(
        &server,
        "DELETE",
        &format!("{PODS}/gradient-worker-lost"),
        ResponseTemplate::new(200),
        Some(1),
    )
    .await;
    mount(
        &server,
        "DELETE",
        &format!("{PODS}/{}", kept.external_id),
        ResponseTemplate::new(200),
        Some(0),
    )
    .await;

    setup(svc).await;
}

#[tokio::test]
async fn github_endpoints_do_not_exist_for_a_kubernetes_connection() {
    let server = MockServer::start().await;
    let fixture = common::kubernetes_service(&server.uri()).await;
    let app = Arc::new(App::new(
        vec![fixture.svc.clone()],
        OidcVerifier::new("https://issuer.invalid"),
    ));

    for endpoint in ["github", "swap"] {
        let request = Request::post(format!("/c/ops/{endpoint}"))
            .header("Content-Type", "application/json")
            .body(Body::from(
                r#"{"token":"t","launch_id":"00000000-0000-0000-0000-000000000000"}"#,
            ))
            .unwrap();
        let status = router(app.clone()).oneshot(request).await.unwrap().status();
        assert_eq!(status, StatusCode::NOT_FOUND, "{endpoint}");
    }
}
