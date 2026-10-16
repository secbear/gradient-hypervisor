mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradient_gha_autoscaler::github::GitHubClient;
use gradient_gha_autoscaler::gradient::GradientClient;
use gradient_gha_autoscaler::http::{App, router};
use gradient_gha_autoscaler::oidc::OidcVerifier;
use gradient_gha_autoscaler::service::ConnectionService;
use gradient_gha_autoscaler::state::{Launch, LaunchState, SecretKind, State};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use url::Url;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const GITHUB_SECRET: &str = "github-hook-secret";
const GRADIENT_SECRET: &str = "gradient-hook-secret";

struct Fixture {
    gradient: MockServer,
    svc: Arc<ConnectionService>,
    app: Arc<App>,
}

async fn fixture() -> Fixture {
    let gradient = MockServer::start().await;
    let state = State::memory().await.unwrap();
    state
        .put_secret("ops", SecretKind::GitHubHook, GITHUB_SECRET)
        .await
        .unwrap();
    state
        .put_secret("ops", SecretKind::GradientHook, GRADIENT_SECRET)
        .await
        .unwrap();
    let svc = Arc::new(ConnectionService::new(
        common::connection("ops", "acme/runners", &gradient.uri()),
        Url::parse("https://scaler.example.com").unwrap(),
        GradientClient::new(Url::parse(&gradient.uri()).unwrap(), "GRADkey".into()),
        GitHubClient::with_api_url(
            Url::parse(&gradient.uri()).unwrap(),
            "ghp".into(),
            "acme/runners".into(),
        ),
        state,
    ));
    let app = Arc::new(App::new(
        vec![svc.clone()],
        OidcVerifier::new("https://issuer.invalid"),
    ));
    Fixture { gradient, svc, app }
}

fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

fn run_completed(run_id: u64) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "action": "completed",
        "workflow_run": { "id": run_id, "status": "completed", "conclusion": "success" }
    }))
    .unwrap()
}

fn github_request(connection: &str, body: Vec<u8>, signature: Option<String>) -> Request<Body> {
    let mut request = Request::post(format!("/c/{connection}/github"))
        .header("X-GitHub-Event", "workflow_run")
        .header("Content-Type", "application/json");
    if let Some(signature) = signature {
        request = request.header("X-Hub-Signature-256", signature);
    }
    request.body(Body::from(body)).unwrap()
}

fn gradient_request(delivery: &str) -> Request<Body> {
    let body = br#"{"event":"worker.demand","at":"2026-10-16T12:00:00Z","content":{}}"#.to_vec();
    Request::post("/c/ops/gradient")
        .header("X-Gradient-Event", "worker.demand")
        .header("X-Gradient-Delivery", delivery)
        .header("X-Gradient-Signature", sign(GRADIENT_SECRET, &body))
        .body(Body::from(body))
        .unwrap()
}

impl Fixture {
    async fn send(&self, request: Request<Body>) -> StatusCode {
        router(self.app.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status()
    }

    async fn insert(&self, run_id: u64) -> Launch {
        let launch = Launch::new("ops", run_id, "ubuntu-latest");
        self.svc.state.insert_launch(&launch).await.unwrap();
        launch
    }

    async fn state_of(&self, launch: &Launch) -> LaunchState {
        self.svc
            .state
            .launch("ops", launch.id)
            .await
            .unwrap()
            .unwrap()
            .state
    }
}

#[tokio::test]
async fn github_bad_signature_401() {
    let fixture = fixture().await;
    let launch = fixture.insert(11).await;
    let body = run_completed(11);

    let forged = sign("wrong-secret", &body);
    assert_eq!(
        fixture
            .send(github_request("ops", body.clone(), Some(forged)))
            .await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        fixture.send(github_request("ops", body, None)).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(fixture.state_of(&launch).await, LaunchState::Dispatched);
}

#[tokio::test]
async fn unknown_connection_404() {
    let fixture = fixture().await;
    let body = run_completed(11);
    let signature = sign(GITHUB_SECRET, &body);
    assert_eq!(
        fixture
            .send(github_request("nobody", body, Some(signature)))
            .await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn run_completed_without_worker_marks_failed() {
    let fixture = fixture().await;
    let launch = fixture.insert(11).await;
    let body = run_completed(11);
    let signature = sign(GITHUB_SECRET, &body);

    assert_eq!(
        fixture
            .send(github_request("ops", body, Some(signature)))
            .await,
        StatusCode::OK
    );
    assert_eq!(fixture.state_of(&launch).await, LaunchState::Failed);
}

#[tokio::test]
async fn run_completed_deletes_recorded_worker_only() {
    let fixture = fixture().await;
    let finished = fixture.insert(11).await;
    let still_running = fixture.insert(12).await;
    let finished_worker = Uuid::new_v4();
    fixture
        .svc
        .state
        .mark_swapped(finished.id, finished_worker)
        .await
        .unwrap();
    fixture
        .svc
        .state
        .advance_launch(finished.id, LaunchState::Connected)
        .await
        .unwrap();
    fixture
        .svc
        .state
        .mark_swapped(still_running.id, Uuid::new_v4())
        .await
        .unwrap();

    Mock::given(method("DELETE"))
        .and(path(format!("/api/v1/teams/ops/workers/{finished_worker}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "error": false, "message": "deleted" })),
        )
        .expect(1)
        .with_priority(1)
        .mount(&fixture.gradient)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .with_priority(10)
        .mount(&fixture.gradient)
        .await;

    let body = run_completed(11);
    let signature = sign(GITHUB_SECRET, &body);
    assert_eq!(
        fixture
            .send(github_request("ops", body, Some(signature)))
            .await,
        StatusCode::OK
    );
    assert_eq!(fixture.state_of(&finished).await, LaunchState::Completed);
    assert_eq!(
        fixture.state_of(&still_running).await,
        LaunchState::Dispatched
    );
}

#[tokio::test]
async fn gradient_delivery_deduplicated() {
    let fixture = fixture().await;
    let wakes_now = || tokio::time::timeout(Duration::from_millis(50), fixture.svc.wake.notified());

    assert_eq!(fixture.send(gradient_request("d-1")).await, StatusCode::OK);
    assert!(wakes_now().await.is_ok());

    assert_eq!(fixture.send(gradient_request("d-1")).await, StatusCode::OK);
    assert!(wakes_now().await.is_err());

    assert_eq!(fixture.send(gradient_request("d-2")).await, StatusCode::OK);
    assert!(wakes_now().await.is_ok());
}
