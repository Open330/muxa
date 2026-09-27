//! OpenID Connect plumbing shared by pane-sharing recipient login and
//! operator sign-in: origin/URL validation, provider discovery, client
//! construction and cookie parsing. Each caller keeps its own session store
//! and authorization rule; nothing here decides who may do what.

use axum::http::{header, HeaderMap};
use openidconnect::{
    core::{
        CoreAuthDisplay, CoreAuthPrompt, CoreErrorResponseType, CoreGenderClaim, CoreJsonWebKey,
        CoreJweContentEncryptionAlgorithm, CoreJwsSigningAlgorithm, CoreProviderMetadata,
        CoreRevocableToken, CoreRevocationErrorResponse, CoreTokenIntrospectionResponse,
        CoreTokenType,
    },
    AccessTokenHash, AdditionalClaims, ClientId, ClientSecret, EmptyExtraTokenFields,
    EndpointMaybeSet, EndpointNotSet, EndpointSet, IdToken, IdTokenFields, IdTokenVerifier,
    IssuerUrl, OAuth2TokenResponse, RedirectUrl, StandardErrorResponse, StandardTokenResponse,
};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// How long discovered provider metadata (and its signing keys) are reused.
const METADATA_TTL: Duration = Duration::from_secs(300);

pub(super) type TokenResponse<AC> = StandardTokenResponse<
    IdTokenFields<
        AC,
        EmptyExtraTokenFields,
        CoreGenderClaim,
        CoreJweContentEncryptionAlgorithm,
        CoreJwsSigningAlgorithm,
    >,
    CoreTokenType,
>;

/// A discovered client with the authorization and token endpoints set.
/// Generic over the ID token's additional claims so each caller parses only
/// what it authorizes on.
pub(super) type Client<AC> = openidconnect::Client<
    AC,
    CoreAuthDisplay,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJsonWebKey,
    CoreAuthPrompt,
    StandardErrorResponse<CoreErrorResponseType>,
    TokenResponse<AC>,
    CoreTokenIntrospectionResponse,
    CoreRevocableToken,
    CoreRevocationErrorResponse,
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// Validate the provider fields every OIDC-backed feature shares. `public_url`
/// must be a bare origin: callbacks and cookie scope are derived from it.
pub(super) fn validate_provider(
    public_url: &str,
    issuer_url: &str,
    client_id: &str,
    client_secret_env: Option<&str>,
) -> Result<(), String> {
    let public = url::Url::parse(public_url).map_err(|_| "invalid public_url")?;
    let issuer = url::Url::parse(issuer_url).map_err(|_| "invalid issuer_url")?;
    for url in [&public, &issuer] {
        if !secure_url(url)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("public_url and issuer_url require HTTPS (HTTP is allowed only on loopback), without credentials/query/fragment".into());
        }
    }
    if public.path() != "/" || client_id.trim().is_empty() {
        return Err("public_url must be an origin; client_id is required".into());
    }
    if client_secret_env.is_some_and(|name| {
        name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    }) {
        return Err("client_secret_env must name an environment variable".into());
    }
    Ok(())
}

pub(super) fn secure_url(url: &url::Url) -> bool {
    url.scheme() == "https"
        || (url.scheme() == "http"
            && matches!(
                url.host_str(),
                Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
            ))
}

/// Serialized origin of a validated `public_url`.
pub(super) fn origin(public_url: &str) -> String {
    // Validated at configuration resolution, including for test fixtures.
    url::Url::parse(public_url)
        .expect("validated public_url")
        .origin()
        .ascii_serialization()
}

/// Whether cookies for this origin can carry `Secure` and the `__Host-` prefix.
pub(super) fn is_https(public_url: &str) -> bool {
    url::Url::parse(public_url).is_ok_and(|url| url.scheme() == "https")
}

/// Does a raw `Host` header name exactly the origin of `public_url`?
pub(super) fn matches_host(public_url: &str, host: &str) -> bool {
    let Ok(public) = url::Url::parse(public_url) else {
        return false;
    };
    let Ok(request) = url::Url::parse(&format!("{}://{host}", public.scheme())) else {
        return false;
    };
    request.origin() == public.origin()
        && request.path() == "/"
        && request.username().is_empty()
        && request.password().is_none()
}

/// The single value of cookie `name`. Duplicates (e.g. a sibling-domain
/// cookie shadowing ours) and oversized values fail closed.
pub(super) fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut values = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|part| part.trim().split_once('='))
        .filter(|(key, _)| *key == name)
        .map(|(_, value)| value);
    let value = values.next()?;
    if value.len() > 128 || values.next().is_some() {
        return None;
    }
    Some(value.to_owned())
}

/// Opaque, unguessable identifier for sessions and browser bindings.
pub(super) fn random_secret() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// A relying-party registration at one issuer. Discovery is single-flight and
/// cached; the HTTP client never follows redirects so a compromised endpoint
/// cannot bounce the code exchange elsewhere.
pub(super) struct Provider {
    issuer_url: String,
    client_id: String,
    client_secret_env: Option<String>,
    redirect_url: String,
    http: std::sync::OnceLock<Option<reqwest::Client>>,
    metadata: Mutex<Option<(Instant, CoreProviderMetadata)>>,
}

impl Provider {
    pub(super) fn new(
        issuer_url: &str,
        client_id: &str,
        client_secret_env: Option<&str>,
        redirect_url: String,
    ) -> Self {
        Self {
            issuer_url: issuer_url.to_owned(),
            client_id: client_id.to_owned(),
            client_secret_env: client_secret_env.map(str::to_owned),
            redirect_url,
            http: std::sync::OnceLock::new(),
            metadata: Mutex::new(None),
        }
    }

    pub(super) async fn client<AC: AdditionalClaims>(
        &self,
    ) -> Result<(Client<AC>, reqwest::Client), ()> {
        let http = self
            .http
            .get_or_init(|| {
                reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(Duration::from_secs(10))
                    .build()
                    .ok()
            })
            .as_ref()
            .ok_or(())?
            .clone();
        let metadata = {
            // Single flight discovery and shared connections bound provider traffic.
            let mut cache = self.metadata.lock().await;
            if let Some((_, metadata)) =
                cache.as_ref().filter(|(at, _)| at.elapsed() < METADATA_TTL)
            {
                metadata.clone()
            } else {
                let metadata = CoreProviderMetadata::discover_async(
                    IssuerUrl::new(self.issuer_url.clone()).map_err(|_| ())?,
                    &http,
                )
                .await
                .map_err(|_| ())?;
                *cache = Some((Instant::now(), metadata.clone()));
                metadata
            }
        };
        if !secure_url(metadata.authorization_endpoint().url())
            || metadata
                .token_endpoint()
                .is_none_or(|endpoint| !secure_url(endpoint.url()))
        {
            return Err(());
        }
        let secret = match &self.client_secret_env {
            Some(name) => {
                let value = std::env::var(name).map_err(|_| ())?;
                if value.trim().is_empty() {
                    return Err(());
                }
                Some(ClientSecret::new(value))
            }
            None => None,
        };
        let client = openidconnect::Client::from_provider_metadata(
            metadata,
            ClientId::new(self.client_id.clone()),
            secret,
        )
        .set_redirect_uri(RedirectUrl::new(self.redirect_url.clone()).map_err(|_| ())?);
        Ok((client, http))
    }
}

/// Check `at_hash` when the provider included one. The ID token itself must
/// already have been verified with `verifier`.
pub(super) fn check_access_token_hash<AC: AdditionalClaims>(
    tokens: &TokenResponse<AC>,
    token: &IdToken<
        AC,
        CoreGenderClaim,
        CoreJweContentEncryptionAlgorithm,
        CoreJwsSigningAlgorithm,
    >,
    expected: Option<&AccessTokenHash>,
    verifier: &IdTokenVerifier<'_, CoreJsonWebKey>,
) -> Result<(), ()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let actual = AccessTokenHash::from_token(
        tokens.access_token(),
        token.signing_alg().map_err(|_| ())?,
        token.signing_key(verifier).map_err(|_| ())?,
    )
    .map_err(|_| ())?;
    if actual == *expected {
        Ok(())
    } else {
        Err(())
    }
}
