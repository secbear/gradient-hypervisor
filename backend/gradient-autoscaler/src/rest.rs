use anyhow::Context;
use reqwest::{Certificate, Client, ClientBuilder, RequestBuilder, Response, StatusCode};
use std::time::Duration;

const BODY_EXCERPT_BYTES: usize = 512;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
#[error("{method} {path} answered {status}: {body}")]
pub struct ApiError {
    pub method: String,
    pub path: String,
    pub status: StatusCode,
    pub body: String,
}

pub(crate) fn http_client() -> Client {
    client_builder().build().expect("TLS backend initializes")
}

pub(crate) fn http_client_trusting(ca_pem: &[u8]) -> anyhow::Result<Client> {
    let certificate = Certificate::from_pem(ca_pem).context("parsing the CA certificate")?;
    Ok(client_builder().add_root_certificate(certificate).build()?)
}

fn client_builder() -> ClientBuilder {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
}

pub(crate) async fn send(client: &Client, request: RequestBuilder) -> anyhow::Result<Response> {
    let request = request.build()?;
    let method = request.method().to_string();
    let path = request.url().path().to_owned();
    let response = client
        .execute(request)
        .await
        .with_context(|| format!("{method} {path}"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(ApiError {
        method,
        path,
        status,
        body: excerpt(&body).to_owned(),
    }
    .into())
}

pub(crate) fn status_of(error: &anyhow::Error) -> Option<StatusCode> {
    error.downcast_ref::<ApiError>().map(|error| error.status)
}

fn excerpt(body: &str) -> &str {
    let mut end = body.len().min(BODY_EXCERPT_BYTES);
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    &body[..end]
}
