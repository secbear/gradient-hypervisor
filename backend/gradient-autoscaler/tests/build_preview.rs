mod common;

use common::KubernetesService;
use gradient_autoscaler::build_planner::{BuildWaitReason, PlannedBuildWorker};
use gradient_autoscaler::build_preview::preview_build_capacity;
use gradient_autoscaler::capacity::{BuildCapacityProfile, BuilderResources, CapacityCapabilities};
use gradient_autoscaler::catch_up::{catch_up, run_connection};
use gradient_autoscaler::config::{BuildScalingConfig, BuildScalingMode, LauncherConfig};
use gradient_autoscaler::events::{advance, finish_launch, record_connected_workers};
use gradient_autoscaler::scaler::scale;
use gradient_autoscaler::setup::setup;
use gradient_autoscaler::state::{Launch, LaunchState, SecretKind};
use serde_json::{Value, json};
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const ROUTE: &str = "/api/v1/teams/ops/workers/capacity-assessment";

fn policy() -> BuildScalingConfig {
    BuildScalingConfig {
        mode: BuildScalingMode::Preview,
        profiles: vec!["x86".into()],
        bootstrap_profiles: vec![],
        queue_wait: 60,
        startup_grace: 120,
        max_new_workers: 2,
        fallback_ram_mb: 2048,
    }
}

async fn service(server: &MockServer) -> KubernetesService {
    let mut config = common::kubernetes();
    let pool = config.pools.get_mut("x86").unwrap();
    pool.max_jobs = 2;
    pool.build_resources = Some(
        serde_json::from_value(json!({
            "cpuCount": 4, "ramTotalMb": 8192, "cpuCoreScore": 4000, "buildCores": 2
        }))
        .unwrap(),
    );
    common::kubernetes_service_with_config(&server.uri(), config).await
}

fn enable_preview(fixture: &mut KubernetesService) {
    Arc::get_mut(&mut fixture.svc).unwrap().conn.build_scaling = Some(policy());
}

struct Assessment {
    builds: usize,
    connected: bool,
    restrict_network: bool,
}

impl Respond for Assessment {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let request: Value = serde_json::from_slice(&request.body).unwrap();
        let profiles = request["profiles"].as_array().unwrap();
        let tracked = request["tracked_profiles"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let resources = json!({
            "cores": 2, "reserved_ram_mb": 2048, "predicted_ram_mb": 2048,
            "ram_fit": "fits", "memory_assumption": "predicted",
            "own_samples": 1, "build_time_known": true,
            "time": {
                "download_secs": 0.0, "path_secs": 0.0, "build_secs": 10.0,
                "upload_secs": 0.0, "eval_secs": 0.0, "nar_bytes": 0.0,
                "paths": 0.0, "output_nar_bytes": 0.0, "cores": 2, "fallbacks": []
            }
        });
        let assessment = json!({"resources": resources, "prediction_fit": "fits"});
        let builds = (0..self.builds).map(|_| json!({
            "architecture": "x86_64-linux", "required_features": ["kvm"],
            "is_fixed_output": false, "ready_at": "2026-01-01T00:00:00Z",
            "workers": [],
            "profiles": vec![assessment.clone(); profiles.len()],
            "tracked_workers": vec![if self.connected { Value::Null } else { assessment.clone() }; tracked.len()]
        })).collect::<Vec<_>>();
        ResponseTemplate::new(200).set_body_json(json!({
            "error": false, "message": {
                "captured_at": "2026-11-11T22:00:00Z",
                "server_draining": false, "total_ready_builds": self.builds,
                "builds_truncated": false, "workers": [],
                "profiles": profiles.iter().map(|p| p["profile"].clone()).collect::<Vec<_>>(),
                "tracked_workers": tracked.iter().map(|p| {
                    let mut profile = p["profile"].clone();
                    if self.restrict_network {
                        profile["capabilities"]["network"] = json!(false);
                    }
                    json!({
                        "profile": profile, "connected": self.connected, "worker_index": null
                    })
                }).collect::<Vec<_>>(),
                "builds": builds
            }
        }))
    }
}

async fn assessment(server: &MockServer, builds: usize, connected: bool) {
    Mock::given(method("POST"))
        .and(path(ROUTE))
        .and(header("Authorization", "Bearer GRADkey"))
        .respond_with(Assessment {
            builds,
            connected,
            restrict_network: false,
        })
        .mount(server)
        .await;
}

#[tokio::test]
async fn preview_uses_real_client_and_bounded_planner_without_persisting_proposals() {
    let server = MockServer::start().await;
    let mut fixture = service(&server).await;
    enable_preview(&mut fixture);
    let conn = &mut Arc::get_mut(&mut fixture.svc).unwrap().conn;
    conn.projects = Some(vec!["70000000-0000-0000-0000-000000000001".into()]);
    conn.worker_capabilities.eval = false;
    conn.worker_capabilities.secrets = false;
    assessment(&server, 5, false).await;

    for _ in 0..2 {
        let preview = preview_build_capacity(&fixture.svc).await.unwrap();
        assert_eq!(preview.total_ready_builds, 5);
        assert_eq!(preview.assessed_builds, 5);
        assert!(preview.tracked_workers.is_empty());
        assert_eq!(preview.plan.proposed_workers.len(), 2);
        assert_eq!(preview.plan.deferred_builds.len(), 1);
        assert_eq!(
            preview.plan.deferred_builds[0].reason,
            BuildWaitReason::LaunchLimit
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
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(request.url.path(), ROUTE);
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            body["projects"],
            json!(["70000000-0000-0000-0000-000000000001"])
        );
        assert_eq!(body["include_waiting_builds"], true);
        assert_eq!(body["profiles"].as_array().unwrap().len(), 2);
        assert!(body.get("tracked_profiles").is_none());
        let profile = &body["profiles"][0]["profile"];
        assert_eq!(
            profile["capabilities"],
            json!({
                "core": false, "cache": false, "federate": false, "eval": false,
                "secrets": false, "exec": false, "build": true, "network": true
            })
        );
        assert_eq!(
            profile["resources"],
            json!({
                "architectures": ["x86_64-linux"], "system_features": ["kvm"],
                "max_concurrent_builds": 2, "cpu_count": 4, "ram_total_mb": 8192,
                "cpu_core_score": 4000, "build_cores": 2
            })
        );
        assert_ne!(
            body["profiles"][0]["profile"]["worker_id"],
            body["profiles"][1]["profile"]["worker_id"]
        );
    }
}

async fn prepare(svc: &gradient_autoscaler::service::ConnectionService, age: Duration) -> Launch {
    let profile = BuildCapacityProfile {
        name: Uuid::new_v4().to_string(),
        worker_id: Uuid::new_v4(),
        capabilities: CapacityCapabilities {
            build: true,
            network: true,
            ..Default::default()
        },
        resources: BuilderResources {
            architectures: vec!["x86_64-linux".into()],
            system_features: vec!["kvm".into()],
            max_concurrent_builds: 2,
            cpu_count: 4,
            ram_total_mb: 8192,
            cpu_core_score: 4000,
            build_cores: Some(2),
        },
    };
    svc.launcher
        .prepare_build_launch(
            svc,
            &PlannedBuildWorker {
                profile_index: 0,
                label: "x86".into(),
                worker_id: profile.worker_id,
            },
            &profile,
            &policy().policy(),
            OffsetDateTime::now_utc() - age,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn preview_credits_only_fresh_registered_intents_and_keeps_all_open_launch_limits() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/teams/ops"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": false, "message": {"id": Uuid::nil()}
        })))
        .mount(&server)
        .await;
    let mut fixture = service(&server).await;
    let fresh = prepare(&fixture.svc, Duration::seconds(1)).await;
    let expired = prepare(&fixture.svc, Duration::minutes(3)).await;
    let not_started = prepare(&fixture.svc, Duration::seconds(1)).await;
    let connected = prepare(&fixture.svc, Duration::seconds(1)).await;
    for launch in [&fresh, &expired, &connected] {
        assert!(
            fixture
                .svc
                .state
                .claim_build_registration(launch)
                .await
                .unwrap()
        );
    }
    fixture
        .svc
        .state
        .advance_launch(connected.id, LaunchState::Connected)
        .await
        .unwrap();
    let before = fixture.svc.state.open_launches("ops").await.unwrap();
    server.reset().await;
    enable_preview(&mut fixture);
    assert!(
        fixture
            .svc
            .launcher
            .prepare_build_launch(
                &fixture.svc,
                &PlannedBuildWorker {
                    profile_index: 0,
                    label: "x86".into(),
                    worker_id: fresh.worker_id.unwrap()
                },
                &fresh.build_intent.as_ref().unwrap().profile,
                &policy().policy(),
                OffsetDateTime::now_utc(),
            )
            .await
            .is_err()
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    let LauncherConfig::Kubernetes(config) =
        &mut Arc::get_mut(&mut fixture.svc).unwrap().conn.launcher
    else {
        panic!()
    };
    config
        .pools
        .get_mut("x86")
        .unwrap()
        .build_resources
        .as_mut()
        .unwrap()
        .ram_total_mb = 4096;
    assessment(&server, 3, false).await;

    let preview = preview_build_capacity(&fixture.svc).await.unwrap();
    assert_eq!(preview.tracked_workers.len(), 1);
    let tracked = &preview.tracked_workers[0];
    assert_eq!(
        &tracked.profile,
        &fresh.build_intent.as_ref().unwrap().profile
    );
    assert!(!tracked.connected);
    assert_eq!(tracked.worker_index, None);
    assert_eq!(
        preview.plan.credited_starting_workers,
        vec![fresh.worker_id.unwrap()]
    );
    assert!(preview.plan.proposed_workers.is_empty());
    assert_eq!(preview.plan.deferred_builds.len(), 1);
    assert_eq!(
        preview.plan.deferred_builds[0].reason,
        BuildWaitReason::LaunchLimit
    );
    assert_eq!(
        fixture.svc.state.open_launches("ops").await.unwrap(),
        before
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["tracked_profiles"].as_array().unwrap().len(), 1);
    assert_eq!(
        body["tracked_profiles"][0]["profile"]["worker_id"],
        fresh.worker_id.unwrap().to_string()
    );
    assert_eq!(
        body["tracked_profiles"][0]["profile"]["resources"]["ram_total_mb"],
        8192
    );
    assert_eq!(
        body["profiles"][0]["profile"]["resources"]["ram_total_mb"],
        4096
    );
    assert!(before.iter().any(|launch| launch.id == not_started.id));

    server.reset().await;
    assessment(&server, 3, true).await;
    let preview = preview_build_capacity(&fixture.svc).await.unwrap();
    assert_eq!(preview.tracked_workers.len(), 1);
    let tracked = &preview.tracked_workers[0];
    assert_eq!(
        &tracked.profile,
        &fresh.build_intent.as_ref().unwrap().profile
    );
    assert!(tracked.connected);
    assert_eq!(tracked.worker_index, None);
    assert!(preview.plan.credited_starting_workers.is_empty());
    assert_eq!(preview.plan.deferred_builds.len(), 3);

    server.reset().await;
    Mock::given(method("POST"))
        .and(path(ROUTE))
        .respond_with(Assessment {
            builds: 2,
            connected: false,
            restrict_network: true,
        })
        .mount(&server)
        .await;
    let preview = preview_build_capacity(&fixture.svc).await.unwrap();
    assert_eq!(preview.tracked_workers.len(), 1);
    assert!(!preview.tracked_workers[0].profile.capabilities.network);
    assert_eq!(
        preview.plan.credited_starting_workers,
        vec![fresh.worker_id.unwrap()]
    );
    assert!(preview.plan.deferred_builds.is_empty());
    fixture
        .svc
        .state
        .begin_build_cleanup(&fresh, LaunchState::Failed)
        .await
        .unwrap();
    let cleaning = fixture.svc.state.open_launches("ops").await.unwrap();
    server.reset().await;
    assessment(&server, 3, false).await;
    let preview = preview_build_capacity(&fixture.svc).await.unwrap();
    assert!(preview.tracked_workers.is_empty());
    assert!(preview.plan.credited_starting_workers.is_empty());
    assert!(preview.plan.proposed_workers.is_empty());
    assert_eq!(preview.plan.deferred_builds.len(), 3);
    assert!(
        preview
            .plan
            .deferred_builds
            .iter()
            .all(|build| { build.reason == BuildWaitReason::LaunchLimit })
    );
    assert_eq!(
        fixture.svc.state.open_launches("ops").await.unwrap(),
        cleaning
    );
}

#[tokio::test]
async fn preview_catch_up_and_direct_mutation_entries_leave_state_and_external_systems_unchanged() {
    let server = MockServer::start().await;
    let mut fixture = service(&server).await;
    let launch = Launch::new("ops", "existing-pod", "x86");
    fixture.svc.state.insert_launch(&launch).await.unwrap();
    let before = fixture.svc.state.open_launches("ops").await.unwrap();
    enable_preview(&mut fixture);
    assessment(&server, 0, false).await;

    setup(&fixture.svc).await;
    assert!(fixture.svc.launcher.setup(&fixture.svc).await.is_none());
    catch_up(&fixture.svc).await;
    record_connected_workers(&fixture.svc).await.unwrap();
    assert!(
        advance(&fixture.svc, &launch, LaunchState::Running)
            .await
            .is_err()
    );
    assert!(finish_launch(&fixture.svc, &launch).await.is_err());
    let kinds = fixture.svc.kinds().await.unwrap();
    assert!(
        fixture
            .svc
            .launcher
            .launch(&fixture.svc, &[&kinds[0]])
            .await
            .is_err()
    );
    assert!(
        fixture
            .svc
            .launcher
            .reconcile_prepared(&fixture.svc, launch.id)
            .await
            .is_err()
    );
    assert!(
        fixture
            .svc
            .launcher
            .launch_prepared(&fixture.svc, launch.id)
            .await
            .is_err()
    );
    assert_eq!(
        fixture.svc.state.open_launches("ops").await.unwrap(),
        before
    );
    assert!(
        fixture
            .svc
            .state
            .secret("ops", SecretKind::GradientHook)
            .await
            .unwrap()
            .is_none()
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), ROUTE);
}

#[tokio::test]
async fn preview_errors_never_fall_back_to_legacy_scaling_or_cleanup() {
    for (status, body) in [
        (404, json!({})),
        (503, json!({})),
        (200, json!({"error": false, "message": {"profiles": []}})),
    ] {
        let server = MockServer::start().await;
        let mut fixture = service(&server).await;
        let launch = Launch::new("ops", "existing-pod", "x86");
        fixture.svc.state.insert_launch(&launch).await.unwrap();
        let before = fixture.svc.state.open_launches("ops").await.unwrap();
        enable_preview(&mut fixture);
        Mock::given(method("POST"))
            .and(path(ROUTE))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;
        assert!(scale(&fixture.svc).await.is_err());
        catch_up(&fixture.svc).await;
        assert_eq!(
            fixture.svc.state.open_launches("ops").await.unwrap(),
            before
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| request.url.path() == ROUTE));
    }
}

#[tokio::test]
async fn preview_rejects_project_names_before_sending_http() {
    let server = MockServer::start().await;
    let mut fixture = service(&server).await;
    enable_preview(&mut fixture);
    Arc::get_mut(&mut fixture.svc).unwrap().conn.projects = Some(vec!["website".into()]);
    assert!(preview_build_capacity(&fixture.svc).await.is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn preview_preserves_all_projects_vs_no_projects_on_the_wire() {
    for projects in [None, Some(vec![])] {
        let server = MockServer::start().await;
        let mut fixture = service(&server).await;
        enable_preview(&mut fixture);
        Arc::get_mut(&mut fixture.svc).unwrap().conn.projects = projects.clone();
        assessment(&server, 0, false).await;
        let preview = preview_build_capacity(&fixture.svc).await.unwrap();
        assert!(preview.plan.proposed_workers.is_empty());
        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        if projects.is_none() {
            assert!(body.get("projects").is_none());
        } else {
            assert_eq!(body["projects"], json!([]));
        }
    }
}

#[tokio::test]
async fn preview_wake_path_only_requests_assessment() {
    let server = MockServer::start().await;
    let mut fixture = service(&server).await;
    enable_preview(&mut fixture);
    assessment(&server, 5, false).await;
    let task = tokio::spawn(run_connection(
        fixture.svc.clone(),
        std::time::Duration::from_secs(3600),
    ));
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while server.received_requests().await.unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        fixture.svc.wake.notify_one();
        while server.received_requests().await.unwrap().len() < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    assert!(
        fixture
            .svc
            .state
            .open_launches("ops")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.url.path() == ROUTE)
    );
}
