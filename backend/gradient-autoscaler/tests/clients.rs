use gradient_autoscaler::github::client::{DispatchInputs, GitHubClient};
use gradient_autoscaler::gradient::{GradientClient, UnmetRequirement};
use serde_json::json;
use url::Url;
use uuid::Uuid;
use wiremock::matchers::{
    body_json, body_partial_json, header, method, path, query_param, query_param_is_missing,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DISPATCH_PATH: &str = "/repos/acme/runners/actions/workflows/gradient-worker.yml/dispatches";

fn github(server: &MockServer) -> GitHubClient {
    GitHubClient::with_api_url(
        Url::parse(&server.uri()).unwrap(),
        "ghp_test".into(),
        "acme/runners".into(),
    )
}

fn gradient(server: &MockServer) -> GradientClient {
    GradientClient::new(Url::parse(&server.uri()).unwrap(), "GRADkey".into())
}

fn inputs() -> DispatchInputs {
    DispatchInputs {
        runs_on: r#"["ubuntu-latest"]"#.into(),
        autoscaler_url: "https://scaler.example.com".into(),
        connection: "ops".into(),
        launch_id: Uuid::nil(),
        worker_flake: "github:wavelens/gradient/v2.0.0#gradient".into(),
        idle_timeout: 120,
        max_lifetime: 18000,
        substituters: String::new(),
        trusted_public_keys: String::new(),
    }
}

#[tokio::test]
async fn dispatch_returns_run_id_from_200() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(DISPATCH_PATH))
        .and(header("X-GitHub-Api-Version", "2026-03-10"))
        .and(header("Authorization", "Bearer ghp_test"))
        .and(body_partial_json(json!({
            "ref": "main",
            "inputs": { "connection": "ops", "idle_timeout": "120", "launch_id": Uuid::nil().to_string() }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "workflow_run_id": 4242,
            "run_url": "https://api.github.com/repos/acme/runners/actions/runs/4242",
            "html_url": "https://github.com/acme/runners/actions/runs/4242"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let run_id = github(&server)
        .dispatch("gradient-worker.yml", "main", &inputs())
        .await
        .unwrap();
    assert_eq!(run_id, 4242);
}

#[tokio::test]
async fn dispatch_errors_on_204_or_4xx() {
    for status in [204, 422] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(DISPATCH_PATH))
            .respond_with(
                ResponseTemplate::new(status).set_body_string("Unexpected inputs provided"),
            )
            .mount(&server)
            .await;

        let error = github(&server)
            .dispatch("gradient-worker.yml", "main", &inputs())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(DISPATCH_PATH), "{status}: {error}");
        if status == 422 {
            assert!(
                error.contains("422") && error.contains("Unexpected inputs"),
                "{error}"
            );
        }
    }
}

#[tokio::test]
async fn register_team_worker_sends_uuid_and_display_name() {
    let server = MockServer::start().await;
    let worker_id = Uuid::new_v4();
    let team_id = Uuid::new_v4();
    Mock::given(method("POST"))
        .and(path("/api/v1/teams/ops/workers"))
        .and(header("Authorization", "Bearer GRADkey"))
        .and(body_partial_json(json!({
            "worker_id": worker_id.to_string(),
            "display_name": "gha-4242-1",
            "enable_build": true,
            "enable_network": true,
            "enable_eval": true,
            "enable_secrets": true,
            "enable_exec": false,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": false,
            "message": { "team": team_id, "token": "secret-token" }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let registered = gradient(&server)
        .register_team_worker("ops", worker_id, "gha-4242-1")
        .await
        .unwrap();
    assert_eq!(registered.team, team_id);
    assert_eq!(registered.token, "secret-token");
}

#[tokio::test]
async fn team_demand_parses_builds_and_eval_gaps() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/teams/ops/workers/demand"))
        .and(query_param("projects", "web,api"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": false,
            "message": {
                "builds": [{ "architecture": "aarch64-linux", "required_features": ["kvm"], "build_count": 3 }],
                "network": 0,
                "secrets": 0,
                "eval": 2
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let projects = vec!["web".to_string(), "api".to_string()];
    let demand = gradient(&server)
        .team_demand("ops", Some(&projects))
        .await
        .unwrap();
    assert_eq!(
        demand.builds,
        [UnmetRequirement {
            architecture: "aarch64-linux".into(),
            required_features: vec!["kvm".into()],
            build_count: 3,
        }]
    );
    assert_eq!((demand.network, demand.secrets, demand.eval), (0, 0, 2));
}

#[tokio::test]
async fn team_demand_without_allowlist_sends_no_projects_parameter() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/teams/ops/workers/demand"))
        .and(query_param_is_missing("projects"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": false,
            "message": { "builds": [], "network": 1, "secrets": 0, "eval": 0 }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let demand = gradient(&server).team_demand("ops", None).await.unwrap();
    assert_eq!(demand.network, 1);
}

#[tokio::test]
async fn put_file_sends_sha_when_updating() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path(
            "/repos/acme/runners/contents/.github/workflows/gradient-worker.yml",
        ))
        .and(body_json(json!({
            "message": "update gradient worker workflow",
            "content": "aGVsbG8=",
            "sha": "abc123"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    github(&server)
        .put_file(
            ".github/workflows/gradient-worker.yml",
            "hello",
            Some("abc123"),
            "update gradient worker workflow",
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn file_decodes_content_and_treats_404_as_missing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/runners/contents/present.yml"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "encoding": "base64",
            "content": "aGVs\nbG8=\n",
            "sha": "abc123"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/runners/contents/missing.yml"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let client = github(&server);
    assert_eq!(
        client.file("present.yml").await.unwrap(),
        Some(("hello".into(), "abc123".into()))
    );
    assert_eq!(client.file("missing.yml").await.unwrap(), None);
}
