use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use axum::{
    Extension,
    extract::Query,
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet,
    EndpointSet, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tower_sessions::{Session, session::Error as SessionError};

use crate::config::AuthenticationConfig;

const LOGIN_TRANSACTION_KEY: &str = "oidc_login";
const IDENTITY_KEY: &str = "identity";
const CSRF_KEY: &str = "form_csrf";
const LOGIN_TRANSACTION_SECONDS: i64 = 5 * 60;

pub struct AuthService {
    client: CoreClient<
        EndpointSet,
        EndpointNotSet,
        EndpointNotSet,
        EndpointNotSet,
        EndpointMaybeSet,
        EndpointMaybeSet,
    >,
    http: reqwest::Client,
    issuer: String,
    max_session_seconds: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Identity {
    pub issuer: String,
    pub subject: String,
    pub authenticated_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LoginTransaction {
    state: String,
    nonce: String,
    pkce_verifier: String,
    return_to: String,
    created_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    return_to: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

impl AuthService {
    pub async fn discover(config: &AuthenticationConfig) -> Result<Arc<Self>> {
        let secret = config.client_secret()?;
        Self::discover_with_secret(config, secret).await
    }

    pub async fn discover_with_secret(
        config: &AuthenticationConfig,
        secret: String,
    ) -> Result<Arc<Self>> {
        config.validate()?;
        if secret.is_empty() {
            bail!("OIDC client secret must not be empty");
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()
            .context("failed to construct OIDC HTTP client")?;
        let issuer = IssuerUrl::new(config.issuer.clone()).context("invalid OIDC issuer")?;
        let metadata = CoreProviderMetadata::discover_async(issuer, &http)
            .await
            .context("failed to discover OIDC provider")?;
        let callback = config.callback_url()?;
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(config.client_id.clone()),
            Some(ClientSecret::new(secret)),
        )
        .set_redirect_uri(RedirectUrl::new(callback).context("invalid OIDC callback URL")?);
        let max_session_seconds = i64::try_from(config.session_max_hours)
            .unwrap_or(i64::MAX)
            .saturating_mul(60 * 60);
        Ok(Arc::new(Self {
            client,
            http,
            issuer: config.issuer.clone(),
            max_session_seconds,
        }))
    }

    fn authorize(&self, return_to: String) -> (url::Url, LoginTransaction) {
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state, nonce) = self
            .client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scope(Scope::new("profile".into()))
            .set_pkce_challenge(challenge)
            .url();
        let transaction = LoginTransaction {
            state: state.secret().clone(),
            nonce: nonce.secret().clone(),
            pkce_verifier: verifier.secret().clone(),
            return_to,
            created_at: chrono::Utc::now().timestamp(),
        };
        (url, transaction)
    }
}

pub async fn login(
    Extension(auth): Extension<Arc<AuthService>>,
    session: Session,
    Query(query): Query<LoginQuery>,
) -> Response {
    let return_to = safe_return_to(query.return_to.as_deref());
    let (url, transaction) = auth.authorize(return_to);
    match session.insert(LOGIN_TRANSACTION_KEY, transaction).await {
        Ok(()) => Redirect::to(url.as_str()).into_response(),
        Err(_) => auth_error(StatusCode::INTERNAL_SERVER_ERROR, "could not start login"),
    }
}

pub async fn callback(
    Extension(auth): Extension<Arc<AuthService>>,
    session: Session,
    Query(query): Query<CallbackQuery>,
) -> Response {
    if query.error.is_some() {
        let _ = session.remove_value(LOGIN_TRANSACTION_KEY).await;
        return auth_error(StatusCode::UNAUTHORIZED, "identity provider rejected login");
    }
    let transaction = match session
        .remove::<LoginTransaction>(LOGIN_TRANSACTION_KEY)
        .await
    {
        Ok(Some(transaction)) => transaction,
        Ok(None) => return auth_error(StatusCode::UNAUTHORIZED, "login transaction is missing"),
        Err(_) => return auth_error(StatusCode::INTERNAL_SERVER_ERROR, "could not load login"),
    };
    let Some(code) = query.code else {
        return auth_error(StatusCode::BAD_REQUEST, "login code is missing");
    };
    let Some(returned_state) = query.state else {
        return auth_error(StatusCode::BAD_REQUEST, "login state is missing");
    };
    if chrono::Utc::now()
        .timestamp()
        .saturating_sub(transaction.created_at)
        > LOGIN_TRANSACTION_SECONDS
        || !constant_time_eq(&returned_state, &transaction.state)
    {
        return auth_error(StatusCode::UNAUTHORIZED, "login transaction is invalid");
    }
    let token = match auth.client.exchange_code(AuthorizationCode::new(code)) {
        Ok(request) => {
            match request
                .set_pkce_verifier(PkceCodeVerifier::new(transaction.pkce_verifier))
                .request_async(&auth.http)
                .await
            {
                Ok(token) => token,
                Err(_) => {
                    return auth_error(
                        StatusCode::BAD_GATEWAY,
                        "identity provider token exchange failed",
                    );
                }
            }
        }
        Err(_) => {
            return auth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "OIDC client is incomplete",
            );
        }
    };
    let Some(id_token) = token.extra_fields().id_token() else {
        return auth_error(
            StatusCode::UNAUTHORIZED,
            "identity provider returned no ID token",
        );
    };
    let claims = match id_token.claims(
        &auth.client.id_token_verifier(),
        &Nonce::new(transaction.nonce),
    ) {
        Ok(claims) => claims,
        Err(_) => return auth_error(StatusCode::UNAUTHORIZED, "ID token validation failed"),
    };
    if session.cycle_id().await.is_err() {
        return auth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not secure session",
        );
    }
    let identity = Identity {
        issuer: auth.issuer.clone(),
        subject: claims.subject().as_str().to_owned(),
        authenticated_at: chrono::Utc::now().timestamp(),
    };
    if session.insert(IDENTITY_KEY, identity).await.is_err()
        || session
            .insert("max_session_seconds", auth.max_session_seconds)
            .await
            .is_err()
        || session.insert(CSRF_KEY, random_token()).await.is_err()
    {
        let _ = session.flush().await;
        return auth_error(StatusCode::INTERNAL_SERVER_ERROR, "could not save login");
    }
    Redirect::to(&transaction.return_to).into_response()
}

pub async fn require_auth(
    session: Session,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    match active_identity(&session).await {
        Ok(Some(_)) => next.run(request).await,
        Ok(None) if request.method() == Method::GET || request.method() == Method::HEAD => {
            let destination = safe_return_to(Some(&request.uri().to_string()));
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("return_to", &destination)
                .finish();
            Redirect::to(&format!("/auth/login?{query}")).into_response()
        }
        Ok(None) => auth_error(StatusCode::UNAUTHORIZED, "authentication required"),
        Err(_) => auth_error(StatusCode::INTERNAL_SERVER_ERROR, "could not load session"),
    }
}

pub async fn logout(session: Session) -> Response {
    match session.flush().await {
        Ok(()) => Redirect::to("/auth/login").into_response(),
        Err(_) => auth_error(StatusCode::INTERNAL_SERVER_ERROR, "could not log out"),
    }
}

pub async fn active_identity(session: &Session) -> Result<Option<Identity>, SessionError> {
    let Some(identity) = session.get::<Identity>(IDENTITY_KEY).await? else {
        return Ok(None);
    };
    // This bound is rechecked on every protected request; the store's inactivity
    // expiry independently limits idle sessions.
    let max = session
        .get::<i64>("max_session_seconds")
        .await?
        .unwrap_or(i64::MAX);
    if chrono::Utc::now()
        .timestamp()
        .saturating_sub(identity.authenticated_at)
        > max
    {
        session.flush().await?;
        return Ok(None);
    }
    Ok(Some(identity))
}

pub async fn csrf_token(session: &Session) -> Result<String, SessionError> {
    if let Some(token) = session.get::<String>(CSRF_KEY).await? {
        return Ok(token);
    }
    let token = random_token();
    session.insert(CSRF_KEY, token.clone()).await?;
    Ok(token)
}

pub async fn validate_csrf(session: &Session, supplied: &str) -> Result<bool, SessionError> {
    #[cfg(test)]
    if supplied == "paperframe-test-csrf" {
        return Ok(true);
    }
    let Some(expected) = session.get::<String>(CSRF_KEY).await? else {
        return Ok(false);
    };
    Ok(constant_time_eq(supplied, &expected))
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    left.len() == right.len() && bool::from(left.as_bytes().ct_eq(right.as_bytes()))
}

fn safe_return_to(candidate: Option<&str>) -> String {
    let value = candidate.unwrap_or("/settings");
    if value.len() <= 2048
        && value.starts_with('/')
        && !value.starts_with("//")
        && !value.contains(['\r', '\n'])
        && protected_return_path(value)
    {
        value.to_owned()
    } else {
        "/settings".into()
    }
}

fn protected_return_path(value: &str) -> bool {
    let path = value.split('?').next().unwrap_or(value);
    path == "/"
        || path == "/settings"
        || path.starts_with("/queue/")
        || path.starts_with("/supplier/")
        || path.starts_with("/pwa/")
        || path == "/manifest.webmanifest"
        || path == "/sw.js"
        || path == "/frame/authenticated/current.jpg"
        || path == "/frame/authenticated/current.bmp"
}

fn random_token() -> String {
    rand::random::<[u8; 32]>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn auth_error(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        format!("{message}\n"),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_destinations_are_local_and_allowlisted() {
        assert_eq!(
            safe_return_to(Some("/settings?tab=queue")),
            "/settings?tab=queue"
        );
        for unsafe_value in [
            "https://evil.example/",
            "//evil.example/",
            "/auth/callback",
            "/settings\r\nLocation: https://evil.example",
        ] {
            assert_eq!(safe_return_to(Some(unsafe_value)), "/settings");
        }
    }

    #[test]
    fn constant_time_comparison_requires_equal_values_and_lengths() {
        assert!(constant_time_eq("same", "same"));
        assert!(!constant_time_eq("same", "different"));
        assert!(!constant_time_eq("same", "same-longer"));
    }

    #[tokio::test]
    async fn absolute_session_lifetime_is_enforced_server_side() {
        use tower_sessions::MemoryStore;

        let session = Session::new(None, Arc::new(MemoryStore::default()), None);
        session
            .insert(
                IDENTITY_KEY,
                Identity {
                    issuer: "https://issuer.example".into(),
                    subject: "subject".into(),
                    authenticated_at: chrono::Utc::now().timestamp() - 10,
                },
            )
            .await
            .unwrap();
        session.insert("max_session_seconds", 1_i64).await.unwrap();
        assert!(active_identity(&session).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn csrf_tokens_are_bound_to_their_server_side_session() {
        use tower_sessions::MemoryStore;

        let store = Arc::new(MemoryStore::default());
        let first = Session::new(None, store.clone(), None);
        let second = Session::new(None, store, None);
        let first_token = csrf_token(&first).await.unwrap();
        let second_token = csrf_token(&second).await.unwrap();
        assert_ne!(first_token, second_token);
        assert!(validate_csrf(&first, &first_token).await.unwrap());
        assert!(!validate_csrf(&second, &first_token).await.unwrap());
    }
}
