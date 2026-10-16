mod common;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use gradient_gha_autoscaler::catch_up::catch_up;
use gradient_gha_autoscaler::github::GitHubClient;
use gradient_gha_autoscaler::gradient::GradientClient;
use gradient_gha_autoscaler::service::ConnectionService;
use gradient_gha_autoscaler::setup::setup;
use gradient_gha_autoscaler::state::{Launch, LaunchState, SecretKind, State};
use gradient_gha_autoscaler::workflow::render;
use serde_json::{Value, json};
use url::Url;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PUBLIC_URL: &str = "https://scaler.example.com";
const WORKFLOW_CONTENTS: &str =
    "/repos/acme/runners/contents/.github/workflows/gradient-worker.yml";

async fn service(server: &MockServer) -> ConnectionService {
    let url = Url::parse(&server.uri()).unwrap();
    ConnectionService::new(
        common::connection("ops", "acme/runners", &server.uri()),
        Url::parse(PUBLIC_URL).unwrap(),
        GradientClient::new(url.clone(), "GRADkey".into()),
        GitHubClient::with_api_url(url, "ghp".into(), "acme/runners".into()),
        State::memory().await.unwrap(),
    )
}

async fn mount(
    server: &MockServer,
    verb: &str,
    route: &str,
    response: ResponseTemplate,
    expected: Option<u64>,
) {
    let mock = Mock::given(method(verb))
        .and(path(route))
        .respond_with(response);
    match expected {
        Some(times) => mock.expect(times).mount(server).await,
        None => mock.mount(server).await,
    }
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

fn envelope(message: Value) -> ResponseTemplate {
    ok(json!({ "error": false, "message": message }))
}

async fn repository_and_runners(server: &MockServer) {
    let repo = json!({ "default_branch": "main", "private": true, "owner": { "type": "User" } });
    mount(server, "GET", "/repos/acme/runners", ok(repo), None).await;
    mount(
        server,
        "GET",
        "/repos/acme/runners/actions/runners",
        ok(json!({ "runners": [] })),
        None,
    )
    .await;
}

async fn body_of(server: &MockServer, verb: &str, route: &str) -> Value {
    let requests = server.received_requests().await.unwrap();
    let request = requests
        .iter()
        .find(|r| r.method.as_str() == verb && r.url.path() == route)
        .unwrap();
    serde_json::from_slice(&request.body).unwrap()
}

#[tokio::test]
async fn setup_creates_hooks_and_workflow() {
    let server = MockServer::start().await;
    mount(
        &server,
        "GET",
        "/api/v1/teams/ops/webhooks",
        envelope(json!([])),
        None,
    )
    .await;
    let created = envelope(json!({ "webhook": {}, "secret": "gradient-secret" }));
    mount(
        &server,
        "POST",
        "/api/v1/teams/ops/webhooks",
        created,
        Some(1),
    )
    .await;
    mount(
        &server,
        "GET",
        "/repos/acme/runners/hooks",
        ok(json!([])),
        None,
    )
    .await;
    mount(
        &server,
        "POST",
        "/repos/acme/runners/hooks",
        ok(json!({ "id": 1 })),
        Some(1),
    )
    .await;
    mount(
        &server,
        "GET",
        WORKFLOW_CONTENTS,
        ResponseTemplate::new(404),
        None,
    )
    .await;
    mount(&server, "PUT", WORKFLOW_CONTENTS, ok(json!({})), Some(1)).await;
    repository_and_runners(&server).await;
    let svc = service(&server).await;

    setup(&svc).await;

    let gradient_hook = body_of(&server, "POST", "/api/v1/teams/ops/webhooks").await;
    assert_eq!(gradient_hook["url"], format!("{PUBLIC_URL}/c/ops/gradient"));
    assert_eq!(gradient_hook["events"], json!(["worker.demand"]));
    let stored = svc
        .state
        .secret("ops", SecretKind::GradientHook)
        .await
        .unwrap();
    assert_eq!(stored.as_deref(), Some("gradient-secret"));

    let github_hook = body_of(&server, "POST", "/repos/acme/runners/hooks").await;
    assert_eq!(
        github_hook["config"]["url"],
        format!("{PUBLIC_URL}/c/ops/github")
    );
    let stored = svc
        .state
        .secret("ops", SecretKind::GitHubHook)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(github_hook["config"]["secret"], stored);

    let workflow = body_of(&server, "PUT", WORKFLOW_CONTENTS).await;
    assert!(workflow.get("sha").is_none());
    let content = STANDARD
        .decode(workflow["content"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        String::from_utf8(content).unwrap(),
        render(env!("CARGO_PKG_VERSION"))
    );
}

#[tokio::test]
async fn setup_rotates_lost_gradient_secret() {
    let server = MockServer::start().await;
    let hook_id = Uuid::new_v4();
    let existing = json!([{
        "id": hook_id, "name": "gradient-gha-autoscaler",
        "url": format!("{PUBLIC_URL}/c/ops/gradient"), "events": ["worker.demand"]
    }]);
    mount(
        &server,
        "GET",
        "/api/v1/teams/ops/webhooks",
        envelope(existing),
        None,
    )
    .await;
    mount(
        &server,
        "POST",
        "/api/v1/teams/ops/webhooks",
        ok(json!({})),
        Some(0),
    )
    .await;
    let rotate = format!("/api/v1/teams/ops/webhooks/{hook_id}/rotate-secret");
    mount(
        &server,
        "POST",
        &rotate,
        envelope(json!({ "secret": "rotated" })),
        Some(1),
    )
    .await;
    let svc = service(&server).await;

    setup(&svc).await;

    let stored = svc
        .state
        .secret("ops", SecretKind::GradientHook)
        .await
        .unwrap();
    assert_eq!(stored.as_deref(), Some("rotated"));
}

#[tokio::test]
async fn setup_leaves_user_owned_workflow_alone() {
    let server = MockServer::start().await;
    let edited = "name: my worker\non: workflow_dispatch\n";
    let file = json!({ "content": STANDARD.encode(edited), "sha": "abc", "encoding": "base64" });
    mount(&server, "GET", WORKFLOW_CONTENTS, ok(file), None).await;
    mount(&server, "PUT", WORKFLOW_CONTENTS, ok(json!({})), Some(0)).await;
    let svc = service(&server).await;

    setup(&svc).await;
}

#[tokio::test]
async fn catch_up_closes_launch_missed_by_webhook() {
    let server = MockServer::start().await;
    mount(
        &server,
        "GET",
        "/api/v1/teams/ops/workers/demand",
        envelope(json!({ "builds": [], "fetch": 0, "eval": 0 })),
        None,
    )
    .await;
    let svc = service(&server).await;
    let never_connected = Launch::new("ops", 31, "ubuntu-latest");
    let finished = Launch::new("ops", 32, "ubuntu-latest");
    let still_queued = Launch::new("ops", 33, "ubuntu-latest");
    for launch in [&never_connected, &finished, &still_queued] {
        svc.state.insert_launch(launch).await.unwrap();
    }
    let worker = Uuid::new_v4();
    svc.state.mark_swapped(finished.id, worker).await.unwrap();
    for (run_id, status) in [(31, "completed"), (32, "completed"), (33, "queued")] {
        let run = ok(json!({ "id": run_id, "status": status, "conclusion": null }));
        mount(
            &server,
            "GET",
            &format!("/repos/acme/runners/actions/runs/{run_id}"),
            run,
            None,
        )
        .await;
    }
    let delete = format!("/api/v1/teams/ops/workers/{worker}");
    mount(
        &server,
        "DELETE",
        &delete,
        envelope(json!("deleted")),
        Some(1),
    )
    .await;
    mount(
        &server,
        "POST",
        "/repos/acme/runners/actions/runs/33/cancel",
        ResponseTemplate::new(202),
        None,
    )
    .await;

    catch_up(&svc).await;

    let state = |id| {
        let svc = &svc;
        async move { svc.state.launch("ops", id).await.unwrap().unwrap().state }
    };
    assert_eq!(state(never_connected.id).await, LaunchState::Failed);
    assert_eq!(state(finished.id).await, LaunchState::Completed);
    assert_eq!(state(still_queued.id).await, LaunchState::Cancelled);
}

#[tokio::test]
async fn catch_up_ignores_unrecorded_team_workers() {
    let server = MockServer::start().await;
    let human_worker = json!([{
        "worker_id": Uuid::new_v4(), "display_name": "gha-1-1", "connected": false, "live": null
    }]);
    mount(
        &server,
        "GET",
        "/api/v1/teams/ops/workers",
        envelope(human_worker),
        None,
    )
    .await;
    mount(
        &server,
        "GET",
        "/api/v1/teams/ops/workers/demand",
        envelope(json!({ "builds": [], "fetch": 0, "eval": 0 })),
        None,
    )
    .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let svc = service(&server).await;

    catch_up(&svc).await;
}
