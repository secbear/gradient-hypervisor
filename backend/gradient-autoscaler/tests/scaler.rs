mod common;

use gradient_autoscaler::scaler::scale;
use gradient_autoscaler::state::{Launch, LaunchState};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
