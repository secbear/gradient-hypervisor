use anyhow::Context;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use url::Url;

pub const GITHUB_ISSUER: &str = "https://token.actions.githubusercontent.com";
pub const AUDIENCE: &str = "gradient-autoscaler";
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

pub struct OidcVerifier {
    http: Client,
    issuer: String,
    keys: Mutex<KeyCache>,
}

#[derive(Default)]
struct KeyCache {
    keys: HashMap<String, DecodingKey>,
    refreshed_at: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    pub repository: String,
    pub run_id: u64,
    pub run_attempt: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("invalid token: {0}")]
    Invalid(String),
    #[error("loading signing keys: {0:#}")]
    Keys(anyhow::Error),
}

#[derive(Deserialize)]
struct RawClaims {
    repository: String,
    run_id: String,
    run_attempt: String,
}

#[derive(Deserialize)]
struct Discovery {
    jwks_uri: Url,
}

impl OidcVerifier {
    pub fn github() -> OidcVerifier {
        Self::new(GITHUB_ISSUER)
    }

    pub fn new(issuer: &str) -> OidcVerifier {
        OidcVerifier {
            http: crate::rest::http_client(),
            issuer: issuer.trim_end_matches('/').to_owned(),
            keys: Mutex::new(KeyCache::default()),
        }
    }

    pub async fn verify(&self, token: &str, audience: &str) -> Result<Claims, VerifyError> {
        let invalid = |error: &dyn std::fmt::Display| VerifyError::Invalid(error.to_string());
        let header = decode_header(token).map_err(|e| invalid(&e))?;
        if header.alg != Algorithm::RS256 {
            return Err(VerifyError::Invalid(format!(
                "algorithm {:?} is not RS256",
                header.alg
            )));
        }
        let kid = header
            .kid
            .ok_or_else(|| VerifyError::Invalid("token has no key ID".into()))?;
        let key = self.key(&kid).await?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        let raw = decode::<RawClaims>(token, &key, &validation)
            .map_err(|e| invalid(&e))?
            .claims;
        Ok(Claims {
            repository: raw.repository,
            run_id: raw.run_id.parse().map_err(|e| invalid(&e))?,
            run_attempt: raw.run_attempt.parse().map_err(|e| invalid(&e))?,
        })
    }

    async fn key(&self, kid: &str) -> Result<DecodingKey, VerifyError> {
        let mut cache = self.keys.lock().await;
        if let Some(key) = cache.keys.get(kid) {
            return Ok(key.clone());
        }
        let may_refresh = cache
            .refreshed_at
            .is_none_or(|at| at.elapsed() >= MIN_REFRESH_INTERVAL);
        if may_refresh {
            cache.refreshed_at = Some(Instant::now());
            cache.keys = self.fetch_keys().await.map_err(VerifyError::Keys)?;
        }
        cache
            .keys
            .get(kid)
            .cloned()
            .ok_or_else(|| VerifyError::Invalid(format!("unknown signing key {kid:?}")))
    }

    async fn fetch_keys(&self) -> anyhow::Result<HashMap<String, DecodingKey>> {
        let issuer = Url::parse(&self.issuer)?;
        let discovery: Discovery = self
            .http
            .get(format!("{}/.well-known/openid-configuration", self.issuer))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("reading OIDC discovery document")?;
        if discovery.jwks_uri.origin() != issuer.origin() {
            anyhow::bail!(
                "JWKS URI {} is outside the issuer {}",
                discovery.jwks_uri,
                self.issuer
            );
        }
        let set: JwkSet = self
            .http
            .get(discovery.jwks_uri)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("reading JWKS")?;
        let mut keys = HashMap::new();
        for jwk in &set.keys {
            if let Some(kid) = &jwk.common.key_id {
                keys.insert(kid.clone(), DecodingKey::from_jwk(jwk)?);
            }
        }
        Ok(keys)
    }
}
