use crate::events::{
    WorkflowJobEvent, WorkflowRunEvent, learn_profile, on_workflow_job, on_workflow_run,
};
use crate::oidc::{AUDIENCE, OidcVerifier, VerifyError};
use crate::service::ConnectionService;
use crate::state::SecretKind;
use crate::swap::{SwapError, SwapRequest, swap};
use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DELIVERY_MEMORY: Duration = Duration::from_secs(3600);

pub struct App {
    services: HashMap<String, Arc<ConnectionService>>,
    oidc: OidcVerifier,
    deliveries: Mutex<HashMap<String, Instant>>,
}

type Shared = State<Arc<App>>;

impl App {
    pub fn new(services: Vec<Arc<ConnectionService>>, oidc: OidcVerifier) -> App {
        App {
            services: services
                .into_iter()
                .map(|svc| (svc.conn.name.clone(), svc))
                .collect(),
            oidc,
            deliveries: Mutex::new(HashMap::new()),
        }
    }

    fn service(&self, name: &str) -> Result<Arc<ConnectionService>, StatusCode> {
        self.services
            .get(name)
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)
    }

    fn first_delivery(&self, key: String) -> bool {
        let mut seen = self.deliveries.lock().expect("deliveries lock");
        let now = Instant::now();
        seen.retain(|_, at| now.duration_since(*at) < DELIVERY_MEMORY);
        seen.insert(key, now).is_none()
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/c/{connection}/github", post(github_webhook))
        .route("/c/{connection}/gradient", post(gradient_webhook))
        .route("/c/{connection}/swap", post(token_swap))
        .with_state(app)
}

async fn github_webhook(
    State(app): Shared,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let svc = match app.service(&name) {
        Ok(svc) => svc,
        Err(status) => return status.into_response(),
    };
    if let Err(response) = check_signature(
        &svc,
        SecretKind::GitHubHook,
        &headers,
        "X-Hub-Signature-256",
        &body,
    )
    .await
    {
        return response.into_response();
    }
    let result = match header(&headers, "X-GitHub-Event") {
        "workflow_run" => match serde_json::from_slice::<WorkflowRunEvent>(&body) {
            Ok(event) => on_workflow_run(&svc, &event).await,
            Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        },
        "workflow_job" => match serde_json::from_slice::<WorkflowJobEvent>(&body) {
            Ok(event) => on_workflow_job(&svc, &event).await,
            Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        },
        _ => Ok(()),
    };
    handled(&name, result)
}

async fn gradient_webhook(
    State(app): Shared,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let svc = match app.service(&name) {
        Ok(svc) => svc,
        Err(status) => return status.into_response(),
    };
    if let Err(response) = check_signature(
        &svc,
        SecretKind::GradientHook,
        &headers,
        "X-Gradient-Signature",
        &body,
    )
    .await
    {
        return response.into_response();
    }
    let delivery = header(&headers, "X-Gradient-Delivery");
    if !delivery.is_empty() && !app.first_delivery(format!("{name}:{delivery}")) {
        return StatusCode::OK.into_response();
    }
    if header(&headers, "X-Gradient-Event") == "worker.demand" {
        svc.wake.notify_one();
    }
    StatusCode::OK.into_response()
}

async fn token_swap(
    State(app): Shared,
    Path(name): Path<String>,
    Json(request): Json<SwapRequest>,
) -> Response {
    let svc = match app.service(&name) {
        Ok(svc) => svc,
        Err(status) => return status.into_response(),
    };
    let claims = match app.oidc.verify(&request.token, AUDIENCE).await {
        Ok(claims) => claims,
        Err(error @ VerifyError::Invalid(_)) => {
            return (StatusCode::UNAUTHORIZED, error.to_string()).into_response();
        }
        Err(error) => {
            tracing::error!(connection = %name, "{error}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    match swap(
        &svc.state,
        &svc.conn,
        &svc.gradient,
        &claims,
        request.launch_id,
    )
    .await
    {
        Ok(response) => {
            let learner = svc.clone();
            let worker_id = response.worker_id;
            tokio::spawn(
                async move { learn_profile(&learner, request.launch_id, worker_id).await },
            );
            Json(response).into_response()
        }
        Err(SwapError::Failed(error)) => {
            tracing::error!(connection = %name, "token swap: {error:#}");
            StatusCode::BAD_GATEWAY.into_response()
        }
        Err(error @ SwapError::AlreadySwapped) => {
            (StatusCode::CONFLICT, error.to_string()).into_response()
        }
        Err(error) => (StatusCode::FORBIDDEN, error.to_string()).into_response(),
    }
}

async fn check_signature(
    svc: &ConnectionService,
    kind: SecretKind,
    headers: &HeaderMap,
    header_name: &str,
    body: &[u8],
) -> Result<(), StatusCode> {
    let secret = match svc.state.secret(&svc.conn.name, kind).await {
        Ok(Some(secret)) => secret,
        Ok(None) => return Err(StatusCode::UNAUTHORIZED),
        Err(error) => {
            tracing::error!(connection = %svc.conn.name, "reading webhook secret: {error:#}");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    if signature_matches(&secret, body, header(headers, header_name)) {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

pub fn signature_matches(secret: &str, body: &[u8], header: &str) -> bool {
    let Some(signature) = header
        .strip_prefix("sha256=")
        .and_then(|hex| hex::decode(hex).ok())
    else {
        return false;
    };
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key length");
    mac.update(body);
    mac.verify_slice(&signature).is_ok()
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

fn handled(connection: &str, result: anyhow::Result<()>) -> Response {
    match result {
        Ok(()) => StatusCode::OK.into_response(),
        Err(error) => {
            tracing::error!(connection, "handling webhook: {error:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
