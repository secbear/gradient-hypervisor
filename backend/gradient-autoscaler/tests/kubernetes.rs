mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradient_autoscaler::build_planner::{BuildScalingPolicy, PlannedBuildWorker};
use gradient_autoscaler::capacity::{BuildCapacityProfile, BuilderResources, CapacityCapabilities};
use gradient_autoscaler::catch_up::catch_up;
use gradient_autoscaler::config::WorkerCapabilities;
use gradient_autoscaler::github::oidc::OidcVerifier;
use gradient_autoscaler::http::{App, router};
use gradient_autoscaler::kubernetes::client::KubernetesClient;
use gradient_autoscaler::scaler::scale;
use gradient_autoscaler::setup::setup;
use gradient_autoscaler::state::{Launch, LaunchState};
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum LostResponse {
    None,
    Secret,
    Registration,
    Pod,
    Owner,
}

struct PreparedObjects {
    secret: Option<Value>,
    pod: Option<Value>,
    workers: Vec<Value>,
    lost: LostResponse,
}

async fn prepared_objects(server: &MockServer, lost: LostResponse) -> Arc<Mutex<PreparedObjects>> {
    use base64::Engine as _;
    let objects = Arc::new(Mutex::new(PreparedObjects {
        secret: None,
        pod: None,
        workers: vec![],
        lost,
    }));
    mount(
        server,
        "GET",
        "/api/v1/teams/ops",
        envelope(json!({"id": Uuid::from_u128(5)})),
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
                }
                LostResponse::Pod => {
                    assert!(objects.pod.is_none(), "worker pod created twice");
                    assert_eq!(objects.workers.len(), 1);
                    body["metadata"]["uid"] = json!("pod-uid");
                    body["spec"]["nodeName"] = json!("assigned-node");
                    objects.pod = Some(body.clone());
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
    let secret = Arc::clone(&objects);
    Mock::given(method("GET"))
        .respond_with(move |request: &Received| {
            let objects = secret.lock().unwrap();
            let object = if request.url.path().starts_with(&format!("{SECRETS}/")) {
                &objects.secret
            } else if request.url.path().starts_with(&format!("{PODS}/")) {
                &objects.pod
            } else {
                return ResponseTemplate::new(404);
            };
            object.as_ref().map_or_else(
                || ResponseTemplate::new(404),
                |object| ResponseTemplate::new(200).set_body_json(object),
            )
        })
        .with_priority(10)
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
async fn prepared_launch_recovers_each_lost_response_with_the_same_worker_and_credential() {
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
        restarted
            .svc
            .launcher
            .launch_prepared(&restarted.svc, launch.id)
            .await
            .unwrap();
        restarted
            .svc
            .launcher
            .launch_prepared(&restarted.svc, launch.id)
            .await
            .unwrap();
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
async fn prepared_launch_uses_frozen_manifest_after_configuration_reload() {
    let server = MockServer::start().await;
    let _objects = prepared_objects(&server, LostResponse::None).await;
    let mut fixture = common::kubernetes_service(&server.uri()).await;
    let profile = build_profile();
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
    gradient_autoscaler::events::record_connected_workers(&fixture.svc)
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
    for corruption in [
        "owner",
        "worker",
        "permission",
        "credential",
        "secret_uid",
        "pod_uid",
        "immutable",
    ] {
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
        {
            let mut objects = objects.lock().unwrap();
            match corruption {
                "owner" => {
                    objects.secret.as_mut().unwrap()["metadata"]["ownerReferences"][0]["uid"] =
                        json!("foreign-uid")
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
                "credential" => objects.secret = None,
                "secret_uid" => {
                    objects.secret.as_mut().unwrap()["metadata"]["uid"] = json!("replaced-secret")
                }
                "pod_uid" => {
                    objects.pod.as_mut().unwrap()["metadata"]["uid"] = json!("replaced-pod")
                }
                "immutable" => objects.secret.as_mut().unwrap()["immutable"] = json!(false),
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
