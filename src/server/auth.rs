use axum::{
    extract::{Query, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    Json,
};
use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::server::AppState;

const SESSION_COOKIE: &str = "flowz_session";
const STATE_COOKIE: &str = "flowz_oidc_state";
const NEXT_COOKIE: &str = "flowz_oidc_next";

/// OIDC / OAuth 2.0 provider configuration (ZID and any OIDC-compatible provider).
/// When absent from server config, authentication is disabled (server runs open).
#[derive(Debug, Clone, Deserialize)]
pub struct OidcConfig {
    /// Public provider URL (issuer) — used for browser redirects.
    pub issuer: String,
    /// Provider URL for backend requests (discovery, token exchange, userinfo).
    /// Falls back to `issuer` when unset.
    #[serde(default)]
    pub base_url: Option<String>,
    pub client_id: String,
    pub client_secret: String,
    /// Full callback URL; must match the provider's registered redirect_uris byte-for-byte.
    pub redirect_uri: String,
    /// Skip TLS verification when talking to the provider (self-signed only).
    #[serde(default)]
    pub tls_insecure: bool,
    /// Path to a PEM file with a trusted CA certificate.
    #[serde(default)]
    pub ca_file: Option<String>,
    /// Where to send the user after a successful login (default "/").
    #[serde(default = "default_post_login")]
    pub post_login_redirect: String,
}

fn default_post_login() -> String {
    "/".to_string()
}

impl OidcConfig {
    /// Build from OIDC_* env vars (fallback ZID_*). Returns None if client_id/secret are unset.
    pub fn from_env() -> Option<Self> {
        let get = |oidc: &str, zid: &str| -> Option<String> {
            std::env::var(oidc)
                .ok()
                .filter(|s| !s.is_empty())
                .or_else(|| std::env::var(zid).ok().filter(|s| !s.is_empty()))
        };
        let client_id = get("OIDC_CLIENT_ID", "ZID_CLIENT_ID")?;
        let client_secret = get("OIDC_CLIENT_SECRET", "ZID_CLIENT_SECRET")?;
        Some(Self {
            issuer: get("OIDC_ISSUER", "ZID_ISSUER")?,
            base_url: get("OIDC_BASE_URL", "ZID_BASE_URL"),
            client_id,
            client_secret,
            redirect_uri: get("OIDC_REDIRECT_URI", "ZID_REDIRECT_URI")?,
            tls_insecure: get("OIDC_TLS_INSECURE", "ZID_TLS_INSECURE")
                .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(false),
            ca_file: get("OIDC_CA_FILE", "ZID_CA_FILE"),
            post_login_redirect: std::env::var("OIDC_POST_LOGIN_REDIRECT")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(default_post_login),
        })
    }
}

/// JWT session claims. No users table — identity lives entirely in the token.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionClaims {
    pub sub: String,
    #[serde(default)]
    pub email: Option<String>,
    pub exp: i64,
    pub iat: i64,
}

pub fn create_session_token(
    secret: &str,
    sub: &str,
    email: Option<&str>,
) -> anyhow::Result<String> {
    let now = Utc::now();
    let claims = SessionClaims {
        sub: sub.to_string(),
        email: email.map(String::from),
        exp: (now + Duration::hours(24)).timestamp(),
        iat: now.timestamp(),
    };
    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_ref()),
    )?;
    Ok(token)
}

pub fn verify_session_token(secret: &str, token: &str) -> anyhow::Result<SessionClaims> {
    let data = decode::<SessionClaims>(
        token,
        &DecodingKey::from_secret(secret.as_ref()),
        &Validation::default(),
    )?;
    Ok(data.claims)
}

// ── middleware ──────────────────────────────────────────────────────────────

/// Gate: require a valid session cookie. HTML routes redirect to the login flow,
/// `/api/*` routes get a 401. Only registered on protected routes when OIDC is configured.
pub async fn require_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let secret = state.jwt_secret.as_str();
    let cookie_header = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if let Some(token) = get_cookie_value(cookie_header, SESSION_COOKIE) {
        if verify_session_token(secret, &token).is_ok() {
            return next.run(req).await;
        }
    }

    let path = req.uri().path();
    if path.starts_with("/api/") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "authentication required" })),
        )
            .into_response();
    }

    let next_target = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    Redirect::temporary(&format!(
        "/auth/zid/login?next={}",
        urlencoding::encode(next_target)
    ))
    .into_response()
}

// ── handlers ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct LoginQuery {
    pub next: Option<String>,
}

/// GET /auth/zid/login — start OAuth 2.0 Authorization Code flow.
pub async fn login(State(state): State<AppState>, Query(q): Query<LoginQuery>) -> Response {
    let oidc = match &state.oidc {
        Some(o) => o,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "OIDC not configured").into_response(),
    };

    let csrf = uuid::Uuid::new_v4().to_string();
    let next = q.next.unwrap_or_else(|| oidc.post_login_redirect.clone());
    let issuer = normalize_base(&oidc.issuer);
    let url = format!(
        "{}/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&scope=openid%20profile%20email&state={}",
        issuer,
        urlencoding::encode(&oidc.client_id),
        urlencoding::encode(&oidc.redirect_uri),
        urlencoding::encode(&csrf),
    );

    let secure = oidc.redirect_uri.starts_with("https://");
    let mut headers = HeaderMap::new();
    append_cookie(&mut headers, &set_cookie(STATE_COOKIE, &csrf, 300, secure));
    append_cookie(
        &mut headers,
        &set_cookie(NEXT_COOKIE, &urlencoding::encode(&next), 300, secure),
    );
    (headers, Redirect::temporary(&url)).into_response()
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// GET /auth/zid/callback — exchange code, fetch identity, issue session cookie.
pub async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let oidc = match &state.oidc {
        Some(o) => o,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "OIDC not configured").into_response(),
    };
    let secure = oidc.redirect_uri.starts_with("https://");

    if let Some(err) = &q.error {
        let msg = q.error_description.as_deref().unwrap_or(err);
        return error_redirect(msg, secure);
    }

    let code = match q.code.as_deref().filter(|c| !c.is_empty()) {
        Some(c) => c.to_string(),
        None => return error_redirect("missing code", secure),
    };
    let state_q = match q.state.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => return error_redirect("missing state", secure),
    };

    let cookie_header = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    match get_cookie_value(cookie_header, STATE_COOKIE) {
        Some(c) if c == state_q => {}
        Some(_) => return error_redirect("state mismatch (CSRF)", secure),
        None => return error_redirect("missing or expired state", secure),
    }

    let next = get_cookie_value(cookie_header, NEXT_COOKIE)
        .and_then(|s| urlencoding::decode(&s).ok().map(|c| c.into_owned()))
        .filter(|s| s.starts_with('/'))
        .unwrap_or_else(|| oidc.post_login_redirect.clone());

    let client = build_http_client(oidc);
    let backend_base = oidc.base_url.as_deref().unwrap_or(&oidc.issuer);
    let (token_endpoint, userinfo_endpoint) =
        endpoints(backend_base, oidc.base_url.as_deref(), &client).await;

    let token_body = format!(
        "grant_type=authorization_code&client_id={}&client_secret={}&redirect_uri={}&code={}",
        urlencoding::encode(&oidc.client_id),
        urlencoding::encode(&oidc.client_secret),
        urlencoding::encode(&oidc.redirect_uri),
        urlencoding::encode(&code),
    );
    let token_resp = match client
        .post(&token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(token_body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return error_redirect(&format!("token request failed: {e}"), secure),
    };
    if !token_resp.status().is_success() {
        let text = token_resp.text().await.unwrap_or_default();
        return error_redirect(&format!("code exchange rejected: {text}"), secure);
    }
    let token_data: TokenResponse = match token_resp.json().await {
        Ok(t) => t,
        Err(e) => return error_redirect(&format!("bad token response: {e}"), secure),
    };

    let (mut sub, mut email) = (String::new(), None);

    // userinfo: Bearer access_token, then Bearer id_token on 401.
    let info = fetch_userinfo(&client, &userinfo_endpoint, &token_data).await;
    if let Some(u) = info {
        sub = u.sub;
        email = u.email;
    }
    // Fallback: unverified id_token payload.
    if sub.is_empty() {
        if let Some(c) = token_data.id_token.as_deref().and_then(parse_id_token) {
            sub = c.0;
            email = email.or(c.1);
        }
    }

    if sub.is_empty() {
        return error_redirect("provider returned no subject", secure);
    }

    let session_token = match create_session_token(&state.jwt_secret, &sub, email.as_deref()) {
        Ok(t) => t,
        Err(e) => return error_redirect(&format!("session error: {e}"), secure),
    };

    let mut out = HeaderMap::new();
    append_cookie(
        &mut out,
        &set_cookie(SESSION_COOKIE, &session_token, 86400, secure),
    );
    append_cookie(&mut out, &clear_cookie(STATE_COOKIE, secure));
    append_cookie(&mut out, &clear_cookie(NEXT_COOKIE, secure));
    (out, Redirect::temporary(&next)).into_response()
}

/// POST or GET /auth/logout — clear the session cookie.
pub async fn logout(State(state): State<AppState>) -> Response {
    let secure = state
        .oidc
        .as_ref()
        .map(|o| o.redirect_uri.starts_with("https://"))
        .unwrap_or(false);
    let mut headers = HeaderMap::new();
    append_cookie(&mut headers, &clear_cookie(SESSION_COOKIE, secure));
    (headers, Redirect::temporary("/")).into_response()
}

// ── provider HTTP helpers ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Deserialize)]
struct Userinfo {
    sub: String,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Deserialize)]
struct Discovery {
    token_endpoint: String,
    #[serde(default)]
    userinfo_endpoint: Option<String>,
}

fn build_http_client(oidc: &OidcConfig) -> reqwest::Client {
    let mut builder = reqwest::Client::builder();
    if let Some(path) = &oidc.ca_file {
        if let Ok(pem) = std::fs::read(path) {
            if let Ok(cert) = reqwest::Certificate::from_pem(&pem) {
                builder = builder.add_root_certificate(cert);
            }
        }
    }
    if oidc.tls_insecure {
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build().expect("build OIDC http client")
}

fn normalize_base(base: &str) -> String {
    let b = base.trim_end_matches('/');
    if b.is_empty() || (!b.starts_with("http://") && !b.starts_with("https://")) {
        "http://localhost:5555".to_string()
    } else {
        b.to_string()
    }
}

/// Resolve token & userinfo endpoints from discovery, falling back to conventional paths.
async fn endpoints(
    discovery_base: &str,
    backend_base: Option<&str>,
    client: &reqwest::Client,
) -> (String, String) {
    let base = normalize_base(discovery_base);
    let url = format!("{base}/.well-known/openid-configuration");
    if let Ok(resp) = client.get(&url).send().await {
        if resp.status().is_success() {
            if let Ok(d) = resp.json::<Discovery>().await {
                let userinfo = d
                    .userinfo_endpoint
                    .unwrap_or_else(|| format!("{base}/oauth/userinfo"));
                return match backend_base {
                    Some(b) => (
                        override_base(b, &d.token_endpoint),
                        override_base(b, &userinfo),
                    ),
                    None => (d.token_endpoint, userinfo),
                };
            }
        }
    }
    (
        format!("{base}/oauth/token"),
        format!("{base}/oauth/userinfo"),
    )
}

/// Replace the host of `url` with `base`, keeping the path.
fn override_base(base: &str, url: &str) -> String {
    let path = if let Some((_, rest)) = url.split_once("://") {
        rest.find('/').map(|i| &rest[i..]).unwrap_or("/")
    } else {
        url
    };
    let path = path.split_once('?').map(|(p, _)| p).unwrap_or(path);
    format!("{}{}", base.trim_end_matches('/'), path)
}

async fn fetch_userinfo(
    client: &reqwest::Client,
    endpoint: &str,
    token: &TokenResponse,
) -> Option<Userinfo> {
    let mut bearers = vec![token.access_token.clone()];
    if let Some(id) = token.id_token.as_deref().filter(|t| !t.is_empty()) {
        bearers.push(id.to_string());
    }
    for bearer in bearers {
        if let Ok(resp) = client
            .get(endpoint)
            .header("Authorization", format!("Bearer {bearer}"))
            .send()
            .await
        {
            if resp.status().is_success() {
                if let Ok(u) = resp.json::<Userinfo>().await {
                    return Some(u);
                }
            }
        }
    }
    None
}

/// Parse `sub`/`email` from an id_token payload without signature verification.
fn parse_id_token(id_token: &str) -> Option<(String, Option<String>)> {
    use base64::Engine;
    let payload = id_token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let sub = v.get("sub")?.as_str()?.to_string();
    let email = v.get("email").and_then(|e| e.as_str()).map(String::from);
    Some((sub, email))
}

// ── cookie helpers ────────────────────────────────────────────────────────────

fn get_cookie_value(cookie_header: &str, name: &str) -> Option<String> {
    for part in cookie_header.split(';') {
        let p = part.trim();
        if let Some(rest) = p.strip_prefix(&format!("{name}=")) {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

fn set_cookie(name: &str, value: &str, max_age: i64, secure: bool) -> String {
    let s = if secure { "; Secure" } else { "" };
    format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{s}")
}

fn clear_cookie(name: &str, secure: bool) -> String {
    set_cookie(name, "", 0, secure)
}

fn append_cookie(headers: &mut HeaderMap, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        headers.append(header::SET_COOKIE, v);
    }
}

fn error_redirect(message: &str, secure: bool) -> Response {
    tracing::warn!("oidc callback error: {message}");
    let mut headers = HeaderMap::new();
    append_cookie(&mut headers, &clear_cookie(STATE_COOKIE, secure));
    append_cookie(&mut headers, &clear_cookie(NEXT_COOKIE, secure));
    (
        headers,
        Redirect::temporary(&format!("/?auth_error={}", urlencoding::encode(message))),
    )
        .into_response()
}
