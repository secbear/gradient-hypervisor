mod common;

use gradient_autoscaler::config::{Connection, LauncherConfig, WorkerCapabilities};
use gradient_autoscaler::github::oidc::{AUDIENCE, OidcVerifier, VerifyError};
use gradient_autoscaler::github::swap::{SwapError, swap};
use gradient_autoscaler::gradient::GradientClient;
use gradient_autoscaler::state::{Launch, State};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PRIVATE_KEY: &str = include_str!("data/oidc-key.pem");
const JWKS: &str = include_str!("data/oidc-jwks.json");

struct Fixture {
    issuer: MockServer,
    gradient: MockServer,
    state: State,
    launch: Launch,
}

async fn fixture() -> Fixture {
    let issuer = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": issuer.uri(),
            "jwks_uri": format!("{}/jwks", issuer.uri()),
        })))
        .mount(&issuer)
        .await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(JWKS, "application/json"))
        .mount(&issuer)
        .await;

    let gradient = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/teams/ops/workers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": false,
            "message": { "team": Uuid::nil(), "token": "worker-token" }
        })))
        .expect(1)
        .mount(&gradient)
        .await;

    let state = State::memory().await.unwrap();
    let launch = Launch::new("ops", "4242", "ubuntu-latest");
    state.insert_launch(&launch).await.unwrap();
    Fixture {
        issuer,
        gradient,
        state,
        launch,
    }
}

fn github_connection(gradient_url: &str) -> Connection {
    let launcher = LauncherConfig::GitHubActions(common::github_actions("acme/runners"));
    common::connection("ops", gradient_url, launcher)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn claims(issuer: &str, run_id: &str, exp: u64) -> Value {
    json!({
        "iss": issuer,
        "aud": AUDIENCE,
        "exp": exp,
        "iat": now(),
        "repository": "acme/runners",
        "run_id": run_id,
        "run_attempt": "1",
    })
}

fn signed(claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test-key".into());
    let key = EncodingKey::from_rsa_pem(PRIVATE_KEY.as_bytes()).unwrap();
    encode(&header, claims, &key).unwrap()
}

impl Fixture {
    fn verifier(&self) -> OidcVerifier {
        OidcVerifier::new(&self.issuer.uri())
    }

    fn gradient(&self) -> GradientClient {
        GradientClient::new(Url::parse(&self.gradient.uri()).unwrap(), "GRADkey".into())
    }

    fn valid_token(&self) -> String {
        signed(&claims(&self.issuer.uri(), "4242", now() + 300))
    }

    async fn swap_as(&self, repository: &str, token: &str) -> Result<(), SwapError> {
        let conn = github_connection(&self.gradient.uri());
        let claims = self.verifier().verify(token, AUDIENCE).await.unwrap();
        swap(
            &self.state,
            &conn,
            repository,
            &self.gradient(),
            &claims,
            self.launch.id,
        )
        .await
        .map(|_| ())
    }
}

#[tokio::test]
async fn valid_token_swaps_once() {
    let fixture = fixture().await;
    let mut conn = github_connection(&fixture.gradient.uri());
    conn.worker_capabilities = WorkerCapabilities {
        eval: false,
        secrets: false,
        ..Default::default()
    };
    let claims = fixture
        .verifier()
        .verify(&fixture.valid_token(), AUDIENCE)
        .await
        .unwrap();
    assert_eq!((claims.run_id, claims.run_attempt), (4242, 1));

    let response = swap(
        &fixture.state,
        &conn,
        "acme/runners",
        &fixture.gradient(),
        &claims,
        fixture.launch.id,
    )
    .await
    .unwrap();
    assert_eq!(response.token, "worker-token");
    assert_eq!(response.team, Uuid::nil());
    assert!(response.server_url.ends_with("/proto") && response.server_url.starts_with("ws://"));

    let stored = fixture
        .state
        .launch("ops", fixture.launch.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.worker_id, Some(response.worker_id));

    let registration = &fixture.gradient.received_requests().await.unwrap()[0];
    let body: Value = serde_json::from_slice(&registration.body).unwrap();
    assert_eq!(body["worker_id"], response.worker_id.to_string());
    assert_eq!(body["display_name"], "gha-4242-1");
    assert_eq!(body["enable_build"], true);
    assert_eq!(body["enable_network"], true);
    assert_eq!(body["enable_eval"], false);
    assert_eq!(body["enable_secrets"], false);
}

#[tokio::test]
async fn second_swap_refused() {
    let fixture = fixture().await;
    let token = fixture.valid_token();
    fixture.swap_as("acme/runners", &token).await.unwrap();

    let replay = fixture.swap_as("acme/runners", &token).await;
    assert!(
        matches!(replay, Err(SwapError::AlreadySwapped)),
        "{replay:?}"
    );
}

#[tokio::test]
async fn wrong_repository_refused() {
    let fixture = fixture().await;
    fixture.gradient.reset().await;
    let result = fixture.swap_as("acme/other", &fixture.valid_token()).await;
    assert!(
        matches!(result, Err(SwapError::WrongRepository(_))),
        "{result:?}"
    );
    assert!(
        fixture
            .gradient
            .received_requests()
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn unknown_run_refused() {
    let fixture = fixture().await;
    fixture.gradient.reset().await;
    let token = signed(&claims(&fixture.issuer.uri(), "9999", now() + 300));
    let result = fixture.swap_as("acme/runners", &token).await;
    assert!(
        matches!(result, Err(SwapError::UnknownLaunch)),
        "{result:?}"
    );
    assert!(
        fixture
            .gradient
            .received_requests()
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn expired_token_refused() {
    let fixture = fixture().await;
    fixture.gradient.reset().await;
    let token = signed(&claims(&fixture.issuer.uri(), "4242", now() - 3600));
    let result = fixture.verifier().verify(&token, AUDIENCE).await;
    assert!(matches!(result, Err(VerifyError::Invalid(_))), "{result:?}");
}

#[tokio::test]
async fn hs256_token_refused() {
    let fixture = fixture().await;
    fixture.gradient.reset().await;
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some("test-key".into());
    let claims = claims(&fixture.issuer.uri(), "4242", now() + 300);
    let token = encode(&header, &claims, &EncodingKey::from_secret(JWKS.as_bytes())).unwrap();
    let result = fixture.verifier().verify(&token, AUDIENCE).await;
    assert!(matches!(result, Err(VerifyError::Invalid(_))), "{result:?}");
}

#[tokio::test]
async fn wrong_audience_refused() {
    let fixture = fixture().await;
    fixture.gradient.reset().await;
    let result = fixture
        .verifier()
        .verify(&fixture.valid_token(), "someone-else")
        .await;
    assert!(matches!(result, Err(VerifyError::Invalid(_))), "{result:?}");
}
