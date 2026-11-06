//! OIDC authorization-code flow with PKCE (S27), against the one configured issuer.
//!
//! Discovery and the JWKS are fetched on every login: logins are rare, and it means key rotation and
//! a restarted provider need no cache handling. ID tokens are verified here (RS256 and ES256 only);
//! the userinfo endpoint is not used, so the provider must put `email` and `groups` in the ID token.
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, Jwk, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::auth::{hash_token, random_secret};
use crate::{Db, Error, OidcIdentity, Result};

/// Settings for the one configured issuer.
pub struct OidcConfig {
    /// The issuer's `.well-known/openid-configuration` URL.
    pub config_url: String,
    pub client_id: String,
    /// Read once at startup from `OIDC_CLIENT_SECRET_FILE`; never logged.
    pub client_secret: String,
    pub user_group: String,
    pub admin_group: String,
}

/// A login must come back within this long.
const LOGIN_MINUTES: u32 = 10;
/// Logins in flight at once. Starting one needs no sign-in, so the table needs a ceiling.
const MAX_IN_FLIGHT: i64 = 500;
const SCOPES: &str = "openid profile email";

struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

/// Refusals that carry a message the user may read. Everything else is logged and shown generically.
fn failed(what: impl std::fmt::Display) -> Error {
    Error::Validation(format!("sign-in failed: {what}"))
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(10)).user_agent("mealier").build().map_err(failed)
}

// ponytail: the provider is the operator's own and trusted, so its responses are read whole with no size cap.
async fn json_of(resp: reqwest::Response, what: &str) -> Result<Value> {
    let status = resp.status();
    if !status.is_success() {
        return Err(failed(format!("{what} answered {status}")));
    }
    let bytes = resp.bytes().await.map_err(|e| failed(format!("{what}: {e}")))?;
    serde_json::from_slice(&bytes).map_err(|e| failed(format!("{what} sent bad JSON: {e}")))
}

fn endpoint(v: &Value, key: &str) -> Result<String> {
    let s = v[key].as_str().ok_or_else(|| failed(format!("discovery has no {key}")))?;
    match url::Url::parse(s) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.has_host() => Ok(s.to_string()),
        _ => Err(failed(format!("discovery {key} is not an http(s) URL"))),
    }
}

fn s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

impl OidcConfig {
    async fn discover(&self, http: &reqwest::Client) -> Result<Discovery> {
        let v = json_of(http.get(&self.config_url).send().await.map_err(|e| failed(format!("discovery: {e}")))?, "discovery").await?;
        let issuer = v["issuer"].as_str().filter(|i| !i.is_empty()).ok_or_else(|| failed("discovery has no issuer"))?.to_string();
        // The issuer must be the place the document was served from (OIDC Discovery, section 4.3).
        let expected = self.config_url.strip_suffix("/.well-known/openid-configuration");
        if expected.is_some_and(|e| e.trim_end_matches('/') != issuer.trim_end_matches('/')) {
            return Err(failed("the discovery document names a different issuer than the one configured"));
        }
        Ok(Discovery { issuer, authorization_endpoint: endpoint(&v, "authorization_endpoint")?, token_endpoint: endpoint(&v, "token_endpoint")?, jwks_uri: endpoint(&v, "jwks_uri")? })
    }

    /// Starts a login: records state, nonce and PKCE verifier, and returns `(authorization URL, state)`.
    pub(crate) async fn begin(&self, db: &Db, redirect_uri: &str) -> Result<(String, String)> {
        let d = self.discover(&client()?).await?;
        let (state, nonce, verifier) = (random_secret("")?, random_secret("")?, random_secret("")?);
        db.oidc_login_start(&state, &nonce, &verifier).await?;
        let mut url = url::Url::parse(&d.authorization_endpoint).map_err(failed)?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", SCOPES)
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &s256(&verifier))
            .append_pair("code_challenge_method", "S256");
        Ok((url.into(), state))
    }

    /// Completes a login: spends the state, trades the code for an ID token, verifies it and applies the group rules.
    pub(crate) async fn finish(&self, db: &Db, redirect_uri: &str, code: &str, state: &str) -> Result<OidcIdentity> {
        let (nonce, verifier) = db.oidc_login_take(state).await?.ok_or_else(|| failed("unknown, expired or already used login"))?;
        let http = client()?;
        let d = self.discover(&http).await?;

        let body = serde_urlencoded::to_string([
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
            ("code_verifier", &verifier),
        ])
        .map_err(failed)?;
        let resp = http
            .post(&d.token_endpoint)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| failed(format!("token request: {e}")))?;
        let tokens = json_of(resp, "token endpoint").await?;
        let id_token = tokens["id_token"].as_str().ok_or_else(|| failed("the token response has no id_token"))?;

        let jwks = json_of(http.get(&d.jwks_uri).send().await.map_err(|e| failed(format!("jwks: {e}")))?, "jwks").await?;
        let claims = self.verify(id_token, &d.issuer, &jwks)?;
        if claims.nonce.as_deref() != Some(nonce.as_str()) {
            return Err(failed("the ID token's nonce does not match this login"));
        }
        self.identity(&d.issuer, claims)
    }

    /// Signature, issuer, audience, expiry. Only RS256 and ES256, only with a key of the matching type from the JWKS.
    fn verify(&self, token: &str, issuer: &str, jwks: &Value) -> Result<Claims> {
        let header = jsonwebtoken::decode_header(token).map_err(|e| failed(format!("bad ID token: {e}")))?;
        let alg = match header.alg {
            a @ (Algorithm::RS256 | Algorithm::ES256) => a,
            other => return Err(failed(format!("ID token algorithm {other:?} is not accepted"))),
        };
        let mut v = Validation::new(alg);
        v.set_issuer(&[issuer]);
        v.set_audience(&[&self.client_id]);
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        v.validate_nbf = true;
        v.leeway = 30;

        let mut last = failed("no usable key in the provider's JWKS");
        for key in candidate_keys(jwks, header.kid.as_deref(), alg) {
            match jsonwebtoken::decode::<Claims>(token, &key, &v) {
                Ok(t) => return Ok(t.claims),
                Err(e) => last = failed(format!("ID token rejected: {e}")),
            }
        }
        Err(last)
    }

    /// Claims to a login decision: an email, membership of the users group, and admin if in the admin group.
    fn identity(&self, issuer: &str, c: Claims) -> Result<OidcIdentity> {
        if c.sub.is_empty() {
            return Err(failed("the ID token has no subject"));
        }
        // With several audiences the token must say it was issued to us (OIDC Core, section 3.1.3.7).
        if c.aud.as_array().is_some_and(|a| a.len() > 1) && c.azp.as_deref() != Some(self.client_id.as_str()) {
            return Err(failed("the ID token's authorized party is not this client"));
        }
        let groups: Vec<&str> = c.groups.as_ref().and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        if !groups.contains(&self.user_group.as_str()) {
            return Err(Error::Forbidden("Your account is not in the group that may use mealier.".into()));
        }
        let email = c.email.as_deref().map(str::trim).filter(|e| e.contains('@') && !e.starts_with('@') && !e.ends_with('@'));
        let Some(email) = email else {
            return Err(Error::Forbidden("Your identity provider did not send an email address, which mealier needs.".into()));
        };
        let username = c.preferred_username.as_deref().map(str::trim).filter(|u| !u.is_empty()).unwrap_or_else(|| email.split('@').next().unwrap_or("user"));
        Ok(OidcIdentity {
            issuer: issuer.into(),
            subject: c.sub.clone(),
            email: email.into(),
            name: c.name.clone().unwrap_or_default(),
            username: username.chars().take(64).collect(),
            admin: groups.contains(&self.admin_group.as_str()),
        })
    }
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    nonce: Option<String>,
    azp: Option<String>,
    aud: Value,
    email: Option<String>,
    name: Option<String>,
    preferred_username: Option<String>,
    groups: Option<Value>,
}

/// The JWKS keys that may have signed a token with this header: same `kid` when it names one, a key type that
/// fits the algorithm, and not an encryption key. A key that does not parse is skipped, not fatal.
fn candidate_keys(jwks: &Value, kid: Option<&str>, alg: Algorithm) -> Vec<DecodingKey> {
    let keys = jwks["keys"].as_array().map(Vec::as_slice).unwrap_or_default();
    keys.iter()
        .filter_map(|k| serde_json::from_value::<Jwk>(k.clone()).ok())
        .filter(|jwk| jwk.common.public_key_use != Some(PublicKeyUse::Encryption))
        .filter(|jwk| kid.is_none_or(|k| jwk.common.key_id.as_deref() == Some(k)))
        .filter(|jwk| jwk.common.key_algorithm.is_none_or(|ka| ka.to_string() == format!("{alg:?}")))
        .filter(|jwk| match (&jwk.algorithm, alg) {
            (AlgorithmParameters::RSA(_), Algorithm::RS256) => true,
            (AlgorithmParameters::EllipticCurve(p), Algorithm::ES256) => p.curve == EllipticCurve::P256,
            _ => false,
        })
        .filter_map(|jwk| DecodingKey::from_jwk(&jwk).ok())
        .take(8)
        .collect()
}

impl Db {
    /// Records a login in flight (state stored hashed). Fails once too many are waiting.
    pub(crate) async fn oidc_login_start(&self, state: &str, nonce: &str, verifier: &str) -> Result<()> {
        sqlx::query("DELETE FROM oidc_logins WHERE expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ', 'now')").execute(&self.pool).await?;
        let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM oidc_logins").fetch_one(&self.pool).await?;
        if waiting >= MAX_IN_FLIGHT {
            return Err(Error::TooMany("Too many sign-ins are in progress. Try again in a few minutes.".into()));
        }
        sqlx::query(&format!("INSERT INTO oidc_logins (state_hash, nonce, verifier, expires_at) VALUES (?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '+{LOGIN_MINUTES} minutes'))"))
            .bind(hash_token(state))
            .bind(nonce)
            .bind(verifier)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Spends a login: returns `(nonce, verifier)` if the state is known and unexpired. The row is gone either way.
    pub(crate) async fn oidc_login_take(&self, state: &str) -> Result<Option<(String, String)>> {
        let row: Option<(String, String, bool)> = sqlx::query_as("DELETE FROM oidc_logins WHERE state_hash = ? RETURNING nonce, verifier, expires_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now')")
            .bind(hash_token(state))
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.filter(|r| r.2).map(|r| (r.0, r.1)))
    }
}
