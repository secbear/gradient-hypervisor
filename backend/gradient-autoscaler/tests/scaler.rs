mod common;

use gradient_autoscaler::config::WorkerCapabilities;
use gradient_autoscaler::joint_preview::preview_joint_capacity;
use gradient_autoscaler::scaler::scale;
use gradient_autoscaler::state::{Launch, LaunchState};
use serde_json::{Value, json};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

async fn demand(server: &MockServer, builds: Value) {
    Mock::given(method("GET"))
        .and(path("/api/v1/teams/ops/workers/demand"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": false,
            "message": { "builds": builds, "network": 0, "secrets": 0, "eval": 0 }
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn scale_dispatches_and_records_the_launch() {
    let server = MockServer::start().await;
    demand(
        &server,
        json!([{ "architecture": "x86_64-linux", "required_features": [], "build_count": 2 }]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/runners"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "default_branch": "trunk", "private": true, "owner": { "type": "Organization" }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(
            "/repos/acme/runners/actions/workflows/gradient-worker.yml/dispatches",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "workflow_run_id": 77 })))
        .expect(1)
        .mount(&server)
        .await;
    let svc = common::github_service(&server.uri()).await;

    scale(&svc).await.unwrap();

    let launch = svc
        .state
        .launch_by_external_id("ops", "77")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(launch.label, "ubuntu-latest");
    let requests = server.received_requests().await.unwrap();
    let dispatch = requests
        .iter()
        .find(|r| r.url.path().ends_with("/dispatches"))
        .unwrap();
    let body: Value = serde_json::from_slice(&dispatch.body).unwrap();
    assert_eq!(body["ref"], "trunk");
    assert_eq!(body["inputs"]["launch_id"], launch.id.to_string());
    assert_eq!(body["inputs"]["runs_on"], r#"["ubuntu-latest"]"#);
    assert_eq!(body["inputs"]["substituters"], "https://cache.example.com");
    assert_eq!(
        body["inputs"]["autoscaler_url"],
        "https://scaler.example.com"
    );
}

#[tokio::test]
async fn scale_cancels_queued_run_once_demand_is_gone() {
    let server = MockServer::start().await;
    demand(&server, json!([])).await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/runners/actions/runs/55/cancel"))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    let svc = common::github_service(&server.uri()).await;
    let launch = Launch {
        state: LaunchState::Queued,
        ..Launch::new("ops", "55", "ubuntu-latest")
    };
    svc.state.insert_launch(&launch).await.unwrap();

    scale(&svc).await.unwrap();

    let stored = svc.state.launch("ops", launch.id).await.unwrap().unwrap();
    assert_eq!(stored.state, LaunchState::Cancelled);
}

#[tokio::test]
async fn scale_does_not_launch_for_only_disabled_roles() {
    for capabilities in [
        WorkerCapabilities {
            eval: false,
            secrets: false,
            ..Default::default()
        },
        WorkerCapabilities {
            eval: false,
            network: false,
            ..Default::default()
        },
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/teams/ops/workers/demand"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": { "builds": [], "network": 0, "secrets": 1, "eval": 2 }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let mut svc = common::github_service(&server.uri()).await;
        Arc::get_mut(&mut svc).unwrap().conn.worker_capabilities = capabilities;
        scale(&svc).await.unwrap();
        assert!(svc.state.open_launches("ops").await.unwrap().is_empty());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

const CAPACITY_ROUTE: &str = "/api/v1/teams/ops/workers/capacity-assessment";
const NEXT_BUILD_PAGE: &str = "after-older-builds";

async fn joint_service(server: &MockServer) -> common::KubernetesService {
    let mut kubernetes = common::kubernetes();
    kubernetes.pools.get_mut("x86").unwrap().build_resources = Some(
        serde_json::from_value(json!({
            "cpuCount": 4, "ramTotalMb": 8192, "cpuCoreScore": 4000
        }))
        .unwrap(),
    );
    let mut fixture = common::kubernetes_service_with_config(&server.uri(), kubernetes).await;
    Arc::get_mut(&mut fixture.svc).unwrap().conn.build_scaling = Some(
        serde_json::from_value(json!({
            "mode": "joint_preview", "profiles": ["x86"], "maxNewWorkers": 2,
            "fallbackRamMb": 2048,
            "bootstrapProfiles": [{
                "pool": "arm",
                "capabilities": {
                    "build": false, "network": false, "eval": true, "secrets": false, "exec": false
                }
            }]
        }))
        .unwrap(),
    );
    fixture
}

#[derive(Default)]
struct JointAssessment {
    omit_bootstrap: bool,
    paginated: bool,
    invalid_page: bool,
}

impl Respond for JointAssessment {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let input: Value = serde_json::from_slice(&request.body).unwrap();
        let profiles = input["profiles"].as_array().unwrap();
        let extras = input["bootstrap"]["profiles"].as_array().unwrap();
        let after = input["build_page"].get("after").and_then(Value::as_str);
        let (build_count, total_ready, remaining, next, fits) = if self.paginated {
            match after {
                None => (128, 129, 1, Some(NEXT_BUILD_PAGE), false),
                Some(NEXT_BUILD_PAGE) => (1, 129, 0, None, true),
                Some(_) => return ResponseTemplate::new(409),
            }
        } else {
            (1, 1, 0, None, true)
        };
        let ram_mb = if fits { 2048 } else { 10_000 };
        let fit = if fits { "fits" } else { "does_not_fit" };
        let mut roster = profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| {
                json!({
                    "profile": profile["profile"], "build_profile_index": index
                })
            })
            .collect::<Vec<_>>();
        roster.extend(extras.iter().map(|profile| {
            json!({
                "profile": profile["profile"], "build_profile_index": null
            })
        }));
        let mut response = json!({
            "captured_at": if after.is_some() { "2026-11-11T22:01:00Z" } else { "2026-11-11T22:00:00Z" },
            "server_draining": false,
            "total_ready_builds": total_ready, "builds_truncated": total_ready > build_count, "workers": [],
            "profiles": profiles.iter().map(|profile| profile["profile"].clone()).collect::<Vec<_>>(),
            "tracked_workers": [],
            "build_page": { "remaining_ready_builds": remaining, "next": next },
            "builds": vec![json!({
                "architecture": "x86_64-linux", "required_features": ["kvm"],
                "is_fixed_output": false,
                "ready_at": if after.is_some() { "2026-01-01T00:01:00Z" } else { "2026-01-01T00:00:00Z" },
                "workers": [], "tracked_workers": [],
                "profiles": vec![json!({
                    "prediction_fit": fit,
                    "resources": {
                        "cores": 2, "reserved_ram_mb": ram_mb, "predicted_ram_mb": ram_mb,
                        "ram_fit": fit, "memory_assumption": "predicted",
                        "own_samples": 1, "build_time_known": true,
                        "time": {
                            "download_secs": 0.0, "path_secs": 0.0, "build_secs": 10.0,
                            "upload_secs": 0.0, "eval_secs": 0.0, "nar_bytes": 0.0, "paths": 0.0,
                            "output_nar_bytes": 0.0, "cores": 2, "fallbacks": []
                        }
                    }
                }); profiles.len()]
            }); build_count]
        });
        if self.invalid_page {
            response["build_page"]["remaining_ready_builds"] = json!(129);
        }
        if !self.omit_bootstrap {
            let mut compatible = vec![false; profiles.len()];
            compatible.extend(vec![true; extras.len()]);
            response["bootstrap"] = json!({
                "observed_at": response["captured_at"], "complete": self.paginated,
                "profiles": roster, "tracked_workers": [],
                "demands": if self.paginated { json!([]) } else { json!([{
                    "kind": "evaluation", "available": false,
                    "profiles": compatible, "tracked_workers": []
                }]) }
            });
        }
        ResponseTemplate::new(200).set_body_json(json!({"error": false, "message": response}))
    }
}

#[tokio::test]
async fn joint_preview_uses_one_fresh_assessment_and_never_mutates_workers() {
    let server = MockServer::start().await;
    let fixture = joint_service(&server).await;
    Mock::given(method("POST"))
        .and(path(CAPACITY_ROUTE))
        .respond_with(JointAssessment {
            omit_bootstrap: false,
            ..Default::default()
        })
        .mount(&server)
        .await;
    let before = fixture.svc.state.open_launches("ops").await.unwrap();
    for _ in 0..2 {
        let preview = preview_joint_capacity(&fixture.svc).await.unwrap();
        assert_eq!(preview.plan.build.proposed_workers.len(), 1);
        assert_eq!(preview.plan.capability_workers.len(), 1);
        assert!(!preview.plan.bootstrap_complete);
    }
    scale(&fixture.svc).await.unwrap();
    gradient_autoscaler::setup::setup(&fixture.svc).await;
    gradient_autoscaler::catch_up::catch_up(&fixture.svc).await;
    assert!(fixture.svc.ensure_mutations_allowed().is_err());
    assert_eq!(
        fixture.svc.state.open_launches("ops").await.unwrap(),
        before
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    let mut ids = std::collections::HashSet::new();
    for request in requests {
        assert_eq!(request.url.path(), CAPACITY_ROUTE);
        let input: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(input["build_page"], json!({}));
        assert!(input["bootstrap"].get("tracked_profiles").is_none());
        for profile in input["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .chain(input["bootstrap"]["profiles"].as_array().unwrap())
        {
            assert!(ids.insert(profile["profile"]["worker_id"].as_str().unwrap().to_owned()));
        }
        assert_eq!(
            input["profiles"][0]["registration"],
            json!({
                "enable_build": true, "enable_network": true, "enable_eval": true,
                "enable_secrets": true, "enable_exec": false
            })
        );
        assert_eq!(
            input["bootstrap"]["profiles"][0]["registration"],
            json!({
                "enable_build": false, "enable_network": false, "enable_eval": true,
                "enable_secrets": false, "enable_exec": false
            })
        );
        assert_eq!(
            input["bootstrap"]["profiles"][0]["profile"]["resources"],
            json!({
                "architectures": ["aarch64-linux"], "system_features": [], "max_concurrent_builds": 4,
                "cpu_count": 0, "ram_total_mb": 0, "cpu_core_score": 0, "build_cores": null
            })
        );
    }
}

#[tokio::test]
async fn missing_joint_bootstrap_does_not_fall_back_to_legacy_or_cleanup() {
    let server = MockServer::start().await;
    let fixture = joint_service(&server).await;
    let launch = Launch::new("ops", "existing-pod", "x86");
    fixture.svc.state.insert_launch(&launch).await.unwrap();
    let before = fixture.svc.state.open_launches("ops").await.unwrap();
    Mock::given(method("POST"))
        .and(path(CAPACITY_ROUTE))
        .respond_with(JointAssessment {
            omit_bootstrap: true,
            ..Default::default()
        })
        .expect(1)
        .mount(&server)
        .await;
    let error = scale(&fixture.svc).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("bootstrap capacity assessment is missing")
    );
    assert_eq!(
        fixture.svc.state.open_launches("ops").await.unwrap(),
        before
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn joint_preview_advances_pages_without_combining_builds_or_persisting_proposals() {
    let server = MockServer::start().await;
    let fixture = joint_service(&server).await;
    let before = fixture.svc.state.open_launches("ops").await.unwrap();
    Mock::given(method("POST"))
        .and(path(CAPACITY_ROUTE))
        .respond_with(JointAssessment {
            paginated: true,
            ..Default::default()
        })
        .expect(3)
        .mount(&server)
        .await;

    let older = preview_joint_capacity(&fixture.svc).await.unwrap();
    assert_eq!(older.assessed_builds, 128);
    assert!(older.plan.build.proposed_workers.is_empty());
    assert!(older.plan.capability_workers.is_empty());
    assert_eq!(older.plan.build.deferred_builds.len(), 128);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    let younger = preview_joint_capacity(&fixture.svc).await.unwrap();
    assert_eq!(younger.assessed_builds, 1);
    assert_eq!(younger.plan.build.proposed_workers.len(), 1);
    assert!(younger.plan.capability_workers.is_empty());
    assert!(younger.plan.build.deferred_builds.is_empty());
    assert_ne!(older.captured_at, younger.captured_at);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);

    let restarted = preview_joint_capacity(&fixture.svc).await.unwrap();
    assert_eq!(restarted.assessed_builds, 128);
    assert!(restarted.plan.build.proposed_workers.is_empty());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    for (request, expected_page) in
        requests
            .iter()
            .zip([json!({}), json!({"after": NEXT_BUILD_PAGE}), json!({})])
    {
        assert_eq!(request.url.path(), CAPACITY_ROUTE);
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["build_page"], expected_page);
    }
    assert_eq!(
        fixture.svc.state.open_launches("ops").await.unwrap(),
        before
    );
}

#[tokio::test]
async fn concurrent_joint_preview_cycles_consume_distinct_pages() {
    let server = MockServer::start().await;
    let fixture = joint_service(&server).await;
    Mock::given(method("POST"))
        .and(path(CAPACITY_ROUTE))
        .respond_with(JointAssessment {
            paginated: true,
            ..Default::default()
        })
        .expect(2)
        .mount(&server)
        .await;
    let (first, second) = tokio::join!(
        preview_joint_capacity(&fixture.svc),
        preview_joint_capacity(&fixture.svc)
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    let mut sizes = [first.assessed_builds, second.assessed_builds];
    sizes.sort_unstable();
    assert_eq!(sizes, [1, 128]);
    assert_eq!(
        first.plan.build.proposed_workers.len() + second.plan.build.proposed_workers.len(),
        1
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for (request, expected_page) in requests
        .iter()
        .zip([json!({}), json!({"after": NEXT_BUILD_PAGE})])
    {
        assert_eq!(request.url.path(), CAPACITY_ROUTE);
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["build_page"], expected_page);
    }
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
async fn joint_assessment_errors_reset_the_cursor_without_retry_fallback_or_cleanup() {
    for failure in ["scope", "unavailable", "malformed", "bootstrap", "page"] {
        let server = MockServer::start().await;
        let fixture = joint_service(&server).await;
        let launch = Launch::new("ops", "existing-pod", "x86");
        fixture.svc.state.insert_launch(&launch).await.unwrap();
        let before = fixture.svc.state.open_launches("ops").await.unwrap();
        Mock::given(method("POST"))
            .and(path(CAPACITY_ROUTE))
            .respond_with(JointAssessment {
                paginated: true,
                ..Default::default()
            })
            .expect(1)
            .mount(&server)
            .await;
        preview_joint_capacity(&fixture.svc).await.unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), 1);

        server.reset().await;
        let mock = Mock::given(method("POST")).and(path(CAPACITY_ROUTE));
        let mock = match failure {
            "scope" => mock.respond_with(ResponseTemplate::new(409)),
            "unavailable" => mock.respond_with(ResponseTemplate::new(503)),
            "malformed" => mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({}))),
            "bootstrap" => mock.respond_with(JointAssessment {
                paginated: true,
                omit_bootstrap: true,
                ..Default::default()
            }),
            "page" => mock.respond_with(JointAssessment {
                paginated: true,
                invalid_page: true,
                ..Default::default()
            }),
            _ => unreachable!(),
        };
        mock.expect(1).mount(&server).await;
        assert!(scale(&fixture.svc).await.is_err(), "{failure}");
        let failed = server.received_requests().await.unwrap();
        assert_eq!(failed.len(), 1, "{failure}");
        assert_eq!(failed[0].url.path(), CAPACITY_ROUTE);
        let body: Value = serde_json::from_slice(&failed[0].body).unwrap();
        assert_eq!(body["build_page"], json!({"after": NEXT_BUILD_PAGE}));
        assert_eq!(
            fixture.svc.state.open_launches("ops").await.unwrap(),
            before
        );

        server.reset().await;
        Mock::given(method("POST"))
            .and(path(CAPACITY_ROUTE))
            .respond_with(JointAssessment {
                paginated: true,
                ..Default::default()
            })
            .expect(1)
            .mount(&server)
            .await;
        let fresh = preview_joint_capacity(&fixture.svc).await.unwrap();
        assert_eq!(fresh.assessed_builds, 128, "{failure}");
        let retried = server.received_requests().await.unwrap();
        assert_eq!(retried.len(), 1);
        let body: Value = serde_json::from_slice(&retried[0].body).unwrap();
        assert_eq!(body["build_page"], json!({}), "{failure}");
        assert_eq!(
            fixture.svc.state.open_launches("ops").await.unwrap(),
            before
        );
    }
}
