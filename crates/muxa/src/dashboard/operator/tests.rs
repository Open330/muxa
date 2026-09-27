use super::*;
use crate::config::{DashboardAuthMode, DashboardTomlConfig};
use crate::dashboard::oidc::testing::{provider, signed_token};
use crate::dashboard::{
    server::{router, AppState},
    DashboardConfig, DashboardOverrides,
};
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    response::Response,
};
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

const PUBLIC: &str = "https://dash.example.com";

fn login_config(issuer: &str) -> LoginConfig {
    LoginConfig {
        public_url: PUBLIC.into(),
        issuer_url: issuer.into(),
        client_id: "muxa".into(),
        client_secret_env: None,
        required_group: "operator".into(),
        groups_claim: None,
        scopes: vec!["groups".into()],
    }
}

fn state(issuer: &str) -> AppState {
    let mut config = DashboardConfig::loopback_default();
    config.token = Some("operator-token".into());
    config.login = Some(login_config(issuer));
    AppState::new(
        crate::Store::shared(),
        Arc::new(config),
        Arc::new(crate::tmux::scanner::PaneCache::new(Duration::from_secs(1))),
        crate::session::PtySessionBackend::shared(),
    )
}

#[derive(Default)]
struct Req<'a> {
    cookie: Option<&'a str>,
    bearer: bool,
    origin: Option<&'a str>,
    operator_header: bool,
    host: Option<&'a str>,
}

async fn call(state: &AppState, method: &str, uri: &str, req: Req<'_>) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(cookie) = req.cookie {
        request = request.header(header::COOKIE, cookie);
    }
    if req.bearer {
        request = request.header(header::AUTHORIZATION, "Bearer operator-token");
    }
    if let Some(origin) = req.origin {
        request = request.header(header::ORIGIN, origin);
    }
    if req.operator_header {
        request = request.header("x-muxa-operator", "1");
    }
    if let Some(host) = req.host {
        request = request.header(header::HOST, host);
    }
    let body = if method == "GET" {
        Body::empty()
    } else {
        Body::from("{}")
    };
    router(state.clone())
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap()
}

async fn json_body(response: Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
}

fn set_cookie<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with(&format!("{name}=")))
}

fn cookie_pair(set_cookie: &str) -> String {
    set_cookie.split(';').next().unwrap().to_owned()
}

/// A signed-in operator session without going through the provider.
fn session(state: &AppState) -> String {
    let secret = oidc::random_secret();
    state.operator.registry().sessions.insert(
        secret.clone(),
        Session {
            subject: "owner".into(),
            email: Some("owner@example.com".into()),
            expires: Instant::now() + SESSION_TTL,
        },
    );
    format!("__Host-muxa-op={secret}")
}

struct Flow {
    browser_cookie: String,
    state: String,
    nonce: String,
    authorize: url::Url,
}

async fn begin(state: &AppState, return_to: &str) -> Flow {
    let response = call(
        state,
        "GET",
        &format!("/auth/login?return_to={}", urlencode(return_to)),
        Req::default(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let flow = set_cookie(&response, "__Host-muxa-op-login").unwrap();
    assert!(flow.contains("HttpOnly") && flow.contains("Secure") && flow.contains("SameSite=Lax"));
    let authorize =
        url::Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    let query: HashMap<_, _> = authorize.query_pairs().into_owned().collect();
    Flow {
        browser_cookie: cookie_pair(flow),
        state: query["state"].clone(),
        nonce: query["nonce"].clone(),
        authorize,
    }
}

fn urlencode(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

async fn mount_token(provider: &MockServer, claims: &Value) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"access_token":"test-access", "token_type":"Bearer", "id_token":signed_token(claims)}),
        ))
        .mount(provider)
        .await;
}

fn claims(provider: &MockServer, nonce: &str, groups: Option<Value>) -> Value {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let mut claims = json!({"iss":provider.uri(), "sub":"owner-subject", "aud":"muxa", "iat":now, "exp":now+300, "nonce":nonce, "email":"owner@example.com", "email_verified":false});
    if let Some(groups) = groups {
        claims["groups"] = groups;
    }
    claims
}

async fn complete(state: &AppState, flow: &Flow) -> Response {
    call(
        state,
        "GET",
        &format!("/auth/callback?code=test-code&state={}", flow.state),
        Req {
            cookie: Some(&flow.browser_cookie),
            ..Req::default()
        },
    )
    .await
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One end-to-end sign-in, use and sign-out scenario.
async fn group_member_signs_in_operates_and_signs_out() {
    let provider = provider().await;
    let state = state(&provider.uri());
    let flow = begin(&state, "/?tab=terminals").await;
    let query: HashMap<_, _> = flow.authorize.query_pairs().into_owned().collect();
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["redirect_uri"], format!("{PUBLIC}/auth/callback"));
    assert_eq!(query["prompt"], "select_account");
    let scopes: Vec<_> = query["scope"].split(' ').collect();
    for scope in ["openid", "email", "groups"] {
        assert!(scopes.contains(&scope), "{scopes:?}");
    }
    // email_verified=false is fine: authorization is by group, not email.
    mount_token(
        &provider,
        &claims(&provider, &flow.nonce, Some(json!(["staff", "operator"]))),
    )
    .await;
    let response = complete(&state, &flow).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/?tab=terminals");
    assert_eq!(response.headers()["cache-control"], "no-store");
    let cleared = set_cookie(&response, "__Host-muxa-op-login").unwrap();
    assert!(cleared.contains("Max-Age=0"));
    let issued = set_cookie(&response, "__Host-muxa-op").unwrap();
    for attribute in ["HttpOnly", "Secure", "SameSite=Strict", "Path=/"] {
        assert!(issued.contains(attribute), "{issued}");
    }
    let cookie = cookie_pair(issued);

    // PKCE verifier sent at exchange matches the challenge sent to the IdP.
    let requests = provider.received_requests().await.unwrap();
    let exchange = requests.iter().find(|r| r.url.path() == "/token").unwrap();
    let form: HashMap<_, _> = url::form_urlencoded::parse(&exchange.body)
        .into_owned()
        .collect();
    let verifier = openidconnect::PkceCodeVerifier::new(form["code_verifier"].clone());
    assert_eq!(
        openidconnect::PkceCodeChallenge::from_code_verifier_sha256(&verifier).as_str(),
        query["code_challenge"]
    );

    let signed_in = Req {
        cookie: Some(&cookie),
        ..Req::default()
    };
    assert_eq!(
        call(&state, "GET", "/api/agents", signed_in).await.status(),
        StatusCode::OK
    );
    let access = json_body(
        call(
            &state,
            "GET",
            "/api/access",
            Req {
                cookie: Some(&cookie),
                ..Req::default()
            },
        )
        .await,
    )
    .await;
    assert_eq!(access["write_authorized"], true);
    assert_eq!(access["login"]["signed_in"], true);
    assert_eq!(access["login"]["email"], "owner@example.com");
    assert_eq!(access["login"]["login_url"], format!("{PUBLIC}/auth/login"));

    // Cookie-authorized writes need both the exact Origin and the header.
    for (origin, header) in [
        (None, true),
        (Some("https://evil.example.com"), true),
        (Some(PUBLIC), false),
    ] {
        let response = call(
            &state,
            "POST",
            "/api/shares/check",
            Req {
                cookie: Some(&cookie),
                origin,
                operator_header: header,
                ..Req::default()
            },
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{origin:?} {header}"
        );
    }
    let admitted = call(
        &state,
        "POST",
        "/api/shares/check",
        Req {
            cookie: Some(&cookie),
            origin: Some(PUBLIC),
            operator_header: true,
            ..Req::default()
        },
    )
    .await;
    // Past the auth layer; sharing itself is not configured in this fixture.
    assert_eq!(admitted.status(), StatusCode::NOT_FOUND);

    // Logout is CSRF-protected too, then ends the session.
    assert_eq!(
        call(
            &state,
            "POST",
            "/auth/logout",
            Req {
                cookie: Some(&cookie),
                ..Req::default()
            }
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let response = call(
        &state,
        "POST",
        "/auth/logout",
        Req {
            cookie: Some(&cookie),
            origin: Some(PUBLIC),
            operator_header: true,
            ..Req::default()
        },
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(set_cookie(&response, "__Host-muxa-op")
        .unwrap()
        .contains("Max-Age=0"));
    assert_eq!(
        call(
            &state,
            "GET",
            "/api/agents",
            Req {
                cookie: Some(&cookie),
                ..Req::default()
            }
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    // The consumed state cannot be replayed for a second session.
    assert_eq!(
        complete(&state, &flow).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn group_claim_accepts_a_single_string_and_a_custom_claim_name() {
    let provider = provider().await;
    let mut state = state(&provider.uri());
    let mut config = login_config(&provider.uri());
    config.groups_claim = Some("roles".into());
    let mut dashboard = (*state.config).clone();
    dashboard.login = Some(config.clone());
    state.config = Arc::new(dashboard);
    state.operator = OperatorLogin::new(Some(config));
    let flow = begin(&state, "/").await;
    let mut claims = claims(&provider, &flow.nonce, Some(json!(["operator"])));
    // The default claim name is ignored once another is configured.
    claims["roles"] = json!("operator");
    claims["groups"] = json!("nobody");
    mount_token(&provider, &claims).await;
    let response = complete(&state, &flow).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(set_cookie(&response, "__Host-muxa-op").is_some());
}

#[tokio::test]
async fn accounts_outside_the_group_are_refused() {
    for groups in [
        None,
        Some(json!([])),
        Some(json!("operators")),
        Some(json!(["Operator", "admin"])),
        Some(json!([{"name": "operator"}, 7])),
        Some(json!({"operator": true})),
    ] {
        let provider = provider().await;
        let state = state(&provider.uri());
        let flow = begin(&state, "/").await;
        mount_token(&provider, &claims(&provider, &flow.nonce, groups.clone())).await;
        let response = complete(&state, &flow).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{groups:?}");
        assert!(set_cookie(&response, "__Host-muxa-op").is_none());
        assert!(state.operator.registry().sessions.is_empty());
    }
}

#[tokio::test]
async fn forged_or_mismatched_login_responses_are_refused() {
    let provider = provider().await;
    let state = state(&provider.uri());

    // Wrong nonce in an otherwise valid, group-bearing token.
    let flow = begin(&state, "/").await;
    mount_token(
        &provider,
        &claims(&provider, "wrong-nonce", Some(json!(["operator"]))),
    )
    .await;
    assert_eq!(
        complete(&state, &flow).await.status(),
        StatusCode::UNAUTHORIZED
    );

    // Unknown state, or a known state without / with another browser's cookie.
    let flow = begin(&state, "/").await;
    let other = begin(&state, "/").await;
    for (query_state, cookie) in [
        ("unknown", Some(flow.browser_cookie.as_str())),
        (flow.state.as_str(), None),
        (flow.state.as_str(), Some(other.browser_cookie.as_str())),
    ] {
        let response = call(
            &state,
            "GET",
            &format!("/auth/callback?code=test-code&state={query_state}"),
            Req {
                cookie,
                ..Req::default()
            },
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(set_cookie(&response, "__Host-muxa-op").is_none());
    }
    // A provider error redirect carries no code.
    assert_eq!(
        call(
            &state,
            "GET",
            &format!("/auth/callback?error=access_denied&state={}", flow.state),
            Req {
                cookie: Some(&flow.browser_cookie),
                ..Req::default()
            }
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(state.operator.registry().sessions.is_empty());
}

#[tokio::test]
async fn return_to_must_be_a_same_origin_path() {
    let provider = provider().await;
    let state = state(&provider.uri());
    for bad in [
        "//evil.example.com/",
        "/\\evil.example.com",
        "https://evil.example.com/",
        "evil",
        "",
        "/\u{7f}x",
        "/ x",
    ] {
        let response = call(
            &state,
            "GET",
            &format!("/auth/login?return_to={}", urlencode(bad)),
            Req::default(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{bad:?}");
        assert!(set_cookie(&response, "__Host-muxa-op-login").is_none());
    }
    assert!(routes::safe_return_to("/"));
    assert!(routes::safe_return_to("/?tab=work#panel"));
    // Browsers render the failure page instead of JSON.
    let response = call(
        &state,
        "GET",
        "/auth/login?return_to=%2F%2Fevil",
        Req::default(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/auth/login?return_to=%2F%2Fevil")
                .header(header::ACCEPT, "text/html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.headers()[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .starts_with("text/html"));
}

#[tokio::test]
async fn bearer_token_keeps_working_and_needs_no_csrf_proof() {
    let state = state("https://issuer.example.com");
    assert_eq!(
        call(&state, "GET", "/api/agents", Req::default())
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let bearer = || Req {
        bearer: true,
        ..Req::default()
    };
    assert_eq!(
        call(&state, "GET", "/api/agents", bearer()).await.status(),
        StatusCode::OK
    );
    // No Origin, no X-Muxa-Operator: a bearer token is not ambient.
    assert_eq!(
        call(&state, "POST", "/api/shares/check", bearer())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let access = json_body(call(&state, "GET", "/api/access", bearer()).await).await;
    assert_eq!(access["write_authorized"], true);
    assert_eq!(access["login"]["available"], true);
    assert_eq!(access["login"]["signed_in"], false);
    // Signed-out browsers can still discover sign-in.
    let status = json_body(call(&state, "GET", "/auth/session", Req::default()).await).await;
    assert_eq!(status["available"], true);
    assert_eq!(status["signed_in"], false);
    // SSE reads are fine by cookie.
    let cookie = session(&state);
    assert_eq!(
        call(
            &state,
            "GET",
            "/api/events",
            Req {
                cookie: Some(&cookie),
                ..Req::default()
            }
        )
        .await
        .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn expired_sessions_and_foreign_cookies_do_not_authorize() {
    let state = state("https://issuer.example.com");
    let cookie = session(&state);
    let secret = cookie.split_once('=').unwrap().1.to_owned();
    for forged in [
        // Same value under the pane-sharing recipient cookie name.
        format!("__Host-muxa-share={secret}"),
        // The dev name is not accepted on an HTTPS deployment.
        format!("muxa-op-dev={secret}"),
        // Two values for one name fail closed.
        format!("{cookie}; __Host-muxa-op=other"),
    ] {
        assert_eq!(
            call(
                &state,
                "GET",
                "/api/agents",
                Req {
                    cookie: Some(&forged),
                    ..Req::default()
                }
            )
            .await
            .status(),
            StatusCode::UNAUTHORIZED,
            "{forged}"
        );
    }
    state
        .operator
        .registry()
        .sessions
        .get_mut(&secret)
        .unwrap()
        .expires = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    assert_eq!(
        call(
            &state,
            "GET",
            "/api/agents",
            Req {
                cookie: Some(&cookie),
                ..Req::default()
            }
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(state.operator.registry().sessions.is_empty());
}

#[tokio::test]
async fn operator_session_grants_nothing_on_recipient_routes() {
    let state = state("https://issuer.example.com");
    let cookie = session(&state);
    // Sharing is not configured here; with it configured, the recipient
    // routes read only their own cookie (covered in the sharing tests).
    let response = call(
        &state,
        "GET",
        "/share/api/0123456789abcdef0123456789abcdef",
        Req {
            cookie: Some(&cookie),
            ..Req::default()
        },
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn host_guard_admits_the_login_public_host_only() {
    let state = state("https://issuer.example.com");
    for (host, allowed) in [
        ("dash.example.com", true),
        ("127.0.0.1:7878", true),
        ("dash.example.com.evil.example", false),
        ("evil.example.com", false),
    ] {
        let status = call(
            &state,
            "GET",
            "/api/health",
            Req {
                host: Some(host),
                bearer: true,
                ..Req::default()
            },
        )
        .await
        .status();
        assert_eq!(status != StatusCode::FORBIDDEN, allowed, "{host}: {status}");
    }
}

#[tokio::test]
async fn pending_logins_and_sessions_are_bounded() {
    let provider = provider().await;
    let state = state(&provider.uri());
    for _ in 0..MAX_PENDING_LOGINS {
        begin(&state, "/").await;
    }
    assert_eq!(
        call(&state, "GET", "/auth/login", Req::default())
            .await
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    // Sessions evict the oldest rather than locking the owner out.
    state.operator.registry().logins.clear();
    for _ in 0..MAX_SESSIONS {
        session(&state);
    }
    let flow = begin(&state, "/").await;
    mount_token(
        &provider,
        &claims(&provider, &flow.nonce, Some(json!(["operator"]))),
    )
    .await;
    let response = complete(&state, &flow).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(state.operator.registry().sessions.len(), MAX_SESSIONS);
}

#[tokio::test]
async fn signing_in_again_replaces_the_previous_session() {
    let provider = provider().await;
    let state = state(&provider.uri());
    let previous = session(&state);
    let flow = begin(&state, "/").await;
    mount_token(
        &provider,
        &claims(&provider, &flow.nonce, Some(json!(["operator"]))),
    )
    .await;
    let response = call(
        &state,
        "GET",
        &format!("/auth/callback?code=test-code&state={}", flow.state),
        Req {
            cookie: Some(&format!("{}; {previous}", flow.browser_cookie)),
            ..Req::default()
        },
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let issued = cookie_pair(set_cookie(&response, "__Host-muxa-op").unwrap());
    assert_ne!(issued, previous);
    let registry = state.operator.registry();
    assert_eq!(registry.sessions.len(), 1);
    assert!(!registry
        .sessions
        .contains_key(previous.split_once('=').unwrap().1));
}

#[test]
fn login_config_validation() {
    let base = login_config("https://idp.example.com");
    assert!(base.validate().is_ok());
    assert_eq!(base.groups_claim(), "groups");
    let invalid: Vec<fn(&mut LoginConfig)> = vec![
        |c| c.required_group = String::new(),
        |c| c.required_group = "  ".into(),
        |c| c.required_group = "two words".into(),
        |c| c.groups_claim = Some(String::new()),
        |c| c.groups_claim = Some("email".into()),
        |c| c.scopes = vec!["a b".into()],
        |c| c.public_url = "http://dash.example.com".into(),
        |c| c.public_url = "https://dash.example.com/path".into(),
        |c| c.issuer_url = "http://idp.example.com".into(),
        |c| c.client_id = " ".into(),
        |c| c.client_secret_env = Some("NOT-A-NAME".into()),
    ];
    for mutate in invalid {
        let mut config = base.clone();
        mutate(&mut config);
        assert!(config.validate().is_err(), "{config:?}");
    }
    let mut loopback = base.clone();
    loopback.public_url = "http://127.0.0.1:7878".into();
    assert!(loopback.validate().is_ok());
    assert_eq!(
        loopback.cookie_names(),
        ("muxa-op-dev", "muxa-op-login-dev")
    );
}

#[test]
fn login_requires_token_auth_and_one_public_origin() {
    let resolve =
        |toml: &DashboardTomlConfig| DashboardConfig::resolve(toml, &DashboardOverrides::default());
    let login = Box::new(login_config("https://idp.example.com"));
    let ok = DashboardTomlConfig {
        login: Some(login.clone()),
        token: Some("t".into()),
        ..DashboardTomlConfig::default()
    };
    assert!(resolve(&ok).unwrap().login.is_some());
    for auth in [DashboardAuthMode::PublicRead, DashboardAuthMode::None] {
        let toml = DashboardTomlConfig {
            auth: Some(auth),
            ..ok.clone()
        };
        assert!(matches!(
            resolve(&toml),
            Err(crate::dashboard::DashboardConfigError::Login(_))
        ));
    }
    let tokenless = DashboardTomlConfig {
        token: None,
        ..ok.clone()
    };
    assert!(resolve(&tokenless).is_err());
    let sharing = |public_url: &str| {
        Some(Box::new(crate::dashboard::sharing::SharingConfig {
            public_url: public_url.into(),
            issuer_url: "https://idp.example.com".into(),
            client_id: "muxa".into(),
            client_secret_env: None,
            storage_path: None,
        }))
    };
    let same = DashboardTomlConfig {
        sharing: sharing(PUBLIC),
        ..ok.clone()
    };
    assert!(resolve(&same).is_ok());
    let different = DashboardTomlConfig {
        sharing: sharing("https://share.example.com"),
        ..ok.clone()
    };
    assert!(matches!(
        resolve(&different),
        Err(crate::dashboard::DashboardConfigError::Login(_))
    ));
}

#[test]
fn login_toml_rejects_unknown_keys_and_requires_group() {
    let parse = |text: &str| toml::from_str::<DashboardTomlConfig>(text);
    let valid = r#"
        [login]
        public_url = "https://dash.example.com"
        issuer_url = "https://idp.example.com"
        client_id = "muxa"
        client_secret_env = "MUXA_OIDC_CLIENT_SECRET"
        required_group = "operator"
        groups_claim = "groups"
        scopes = ["groups"]
    "#;
    let parsed = parse(valid).unwrap().login.unwrap();
    assert_eq!(parsed.required_group, "operator");
    assert!(parse(&format!("{valid}\nallowed_emails = [\"x\"]")).is_err());
    assert!(parse(&valid.replace("required_group = \"operator\"", "")).is_err());
}
