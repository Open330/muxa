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
        required_group: Some("operator".into()),
        groups_claim: None,
        scopes: vec!["groups".into()],
        enrollment: None,
    }
}

fn state(issuer: &str) -> AppState {
    state_with(login_config(issuer))
}

fn state_with(login: LoginConfig) -> AppState {
    let mut config = DashboardConfig::loopback_default();
    config.token = Some("operator-token".into());
    config.login = Some(login);
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
    body: Option<Value>,
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
    let body = match req.body {
        Some(body) => Body::from(body.to_string()),
        None if method == "GET" => Body::empty(),
        None => Body::from("{}"),
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
            issuer: "https://issuer.example.com".into(),
            subject: "owner".into(),
            email: Some("owner@example.com".into()),
            via: Via::Group,
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
        .up_to_n_times(1)
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
    assert_eq!(access["login"]["via"], "group");
    assert_eq!(access["login"]["enrollment"], true);
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
async fn accounts_outside_the_group_are_refused_when_enrollment_is_disabled() {
    for groups in [
        None,
        Some(json!([])),
        Some(json!("operators")),
        Some(json!(["Operator", "admin"])),
        Some(json!([{"name": "operator"}, 7])),
        Some(json!({"operator": true})),
    ] {
        let provider = provider().await;
        let mut config = login_config(&provider.uri());
        config.enrollment = Some(false);
        let state = state_with(config);
        // Enrolled entries are not honored either once enrollment is off.
        state
            .operator
            .registry()
            .enrolled
            .push(enrolled(&provider.uri(), "owner-subject"));
        let flow = begin(&state, "/").await;
        mount_token(&provider, &claims(&provider, &flow.nonce, groups.clone())).await;
        let response = complete(&state, &flow).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{groups:?}");
        assert!(set_cookie(&response, "__Host-muxa-op").is_none());
        assert!(set_cookie(&response, "__Host-muxa-op-enroll").is_none());
        assert!(state.operator.registry().sessions.is_empty());
        assert!(state.operator.registry().enrollments.is_empty());
        let page = call(&state, "GET", "/auth/enroll", Req::default()).await;
        assert_eq!(page.status(), StatusCode::NOT_FOUND);
        let status = json_body(call(&state, "GET", "/auth/session", Req::default()).await).await;
        assert_eq!(status["enrollment"], false);
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
        |c| c.required_group = Some(String::new()),
        |c| c.required_group = Some("  ".into()),
        |c| c.required_group = Some("two words".into()),
        |c| {
            c.required_group = None;
            c.enrollment = Some(false);
        },
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
    let mut groupless = base.clone();
    groupless.required_group = None;
    assert!(groupless.validate().is_ok());
    assert!(groupless.enrollment_enabled());
    let mut loopback = base.clone();
    loopback.public_url = "http://127.0.0.1:7878".into();
    assert!(loopback.validate().is_ok());
    assert_eq!(
        loopback.cookie_names(),
        ("muxa-op-dev", "muxa-op-login-dev")
    );
    assert_eq!(loopback.enroll_cookie_name(), "muxa-op-enroll-dev");
    assert_eq!(base.enroll_cookie_name(), "__Host-muxa-op-enroll");
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
fn login_toml_rejects_unknown_keys_and_makes_group_optional() {
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
    assert_eq!(parsed.required_group.as_deref(), Some("operator"));
    assert!(parsed.enrollment_enabled());
    assert!(parse(&format!("{valid}\nallowed_emails = [\"x\"]")).is_err());
    let groupless = parse(&valid.replace("required_group = \"operator\"", ""))
        .unwrap()
        .login
        .unwrap();
    assert!(groupless.required_group.is_none() && groupless.validate().is_ok());
    let disabled = parse(&format!("{valid}\nenrollment = false"))
        .unwrap()
        .login
        .unwrap();
    assert!(!disabled.enrollment_enabled() && disabled.validate().is_ok());
    let neither = parse(&format!("{valid}\nenrollment = false"))
        .unwrap()
        .login
        .map(|mut login| {
            login.required_group = None;
            login
        })
        .unwrap();
    assert!(neither.validate().is_err());
}

// ── Enrollment ─────────────────────────────────────────────────────

const ENROLL_COOKIE: &str = "__Host-muxa-op-enroll";

fn enrolled(issuer: &str, subject: &str) -> Enrolled {
    Enrolled {
        id: uuid::Uuid::new_v4().simple().to_string(),
        issuer: issuer.into(),
        subject: subject.into(),
        email: Some("owner@example.com".into()),
        created_at: 1,
        last_seen_at: 1,
    }
}

fn groupless(issuer: &str) -> AppState {
    let mut config = login_config(issuer);
    config.required_group = None;
    state_with(config)
}

/// Sign in at the provider as `owner-subject` with `groups`, returning the
/// callback response.
async fn sign_in(
    state: &AppState,
    provider: &MockServer,
    return_to: &str,
    groups: Option<Value>,
) -> Response {
    let flow = begin(state, return_to).await;
    mount_token(provider, &claims(provider, &flow.nonce, groups)).await;
    complete(state, &flow).await
}

/// A sign-in that lands on the enrollment page; returns the pending cookie.
async fn pending(state: &AppState, provider: &MockServer, return_to: &str) -> String {
    let response = sign_in(state, provider, return_to, None).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/auth/enroll");
    assert!(set_cookie(&response, "__Host-muxa-op").is_none());
    assert!(set_cookie(&response, "__Host-muxa-op-login")
        .unwrap()
        .contains("Max-Age=0"));
    let issued = set_cookie(&response, ENROLL_COOKIE).unwrap();
    for attribute in [
        "HttpOnly",
        "Secure",
        "SameSite=Lax",
        "Path=/",
        "Max-Age=300",
    ] {
        assert!(issued.contains(attribute), "{issued}");
    }
    cookie_pair(issued)
}

async fn submit(state: &AppState, cookie: Option<&str>, token: &str) -> Response {
    call(
        state,
        "POST",
        "/auth/enroll",
        Req {
            cookie,
            origin: Some(PUBLIC),
            operator_header: true,
            body: Some(json!({"token": token})),
            ..Req::default()
        },
    )
    .await
}

async fn status_of(state: &AppState, uri: &str, cookie: &str) -> StatusCode {
    call(
        state,
        "GET",
        uri,
        Req {
            cookie: Some(cookie),
            ..Req::default()
        },
    )
    .await
    .status()
}

async fn text_body(response: Response) -> String {
    String::from_utf8(
        to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One end-to-end enrollment scenario.
async fn groupless_account_enrolls_with_the_token_then_signs_in_directly() {
    let provider = provider().await;
    // No required_group at all: only enrolled accounts are operators.
    let state = groupless(&provider.uri());
    let cookie = pending(&state, &provider, "/?tab=work").await;

    // The pending enrollment grants no API access by itself.
    assert_eq!(
        status_of(&state, "/api/agents", &cookie).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status_of(&state, "/api/operators", &cookie).await,
        StatusCode::UNAUTHORIZED
    );

    let page = call(
        &state,
        "GET",
        "/auth/enroll",
        Req {
            cookie: Some(&cookie),
            ..Req::default()
        },
    )
    .await;
    assert_eq!(page.status(), StatusCode::OK);
    let csp = page.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(csp.contains("script-src 'self'") && csp.contains("form-action 'none'"));
    assert!(!csp.contains("unsafe-inline"));
    assert_eq!(page.headers()["cache-control"], "no-store");
    let html = text_body(page).await;
    for needle in [
        "owner@example.com",
        "owner-subject",
        "type=\"password\"",
        "Enter this dashboard's access token to register this account as an operator",
        "/auth/enroll/cancel",
        "/static/operator-enroll.mjs",
    ] {
        assert!(html.contains(needle), "{needle}");
    }
    assert_eq!(
        call(&state, "GET", "/static/operator-enroll.mjs", Req::default())
            .await
            .status(),
        StatusCode::OK
    );

    let response = submit(&state, Some(&cookie), "operator-token").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(set_cookie(&response, ENROLL_COOKIE)
        .unwrap()
        .contains("Max-Age=0"));
    let issued = set_cookie(&response, "__Host-muxa-op").unwrap();
    for attribute in ["HttpOnly", "Secure", "SameSite=Strict", "Path=/"] {
        assert!(issued.contains(attribute), "{issued}");
    }
    let session = cookie_pair(issued);
    assert_eq!(json_body(response).await["redirect"], "/?tab=work");
    assert_eq!(
        status_of(&state, "/api/agents", &session).await,
        StatusCode::OK
    );
    let access = json_body(
        call(
            &state,
            "GET",
            "/api/access",
            Req {
                cookie: Some(&session),
                ..Req::default()
            },
        )
        .await,
    )
    .await;
    assert_eq!(access["write_authorized"], true);
    assert_eq!(access["login"]["via"], "enrollment");

    // The pending enrollment was consumed.
    let replay = submit(&state, Some(&cookie), "operator-token").await;
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(json_body(replay).await["restart"], true);

    let list = json_body(
        call(
            &state,
            "GET",
            "/api/operators",
            Req {
                cookie: Some(&session),
                ..Req::default()
            },
        )
        .await,
    )
    .await;
    let operators = list["operators"].as_array().unwrap();
    assert_eq!(operators.len(), 1);
    assert_eq!(operators[0]["email"], "owner@example.com");
    assert_eq!(operators[0]["subject"], "owner-subject");
    assert_eq!(operators[0]["issuer"], provider.uri());
    assert_eq!(operators[0]["current"], true);
    assert_eq!(operators[0]["active"], true);
    // A bearer client is not "the current account".
    let list = json_body(
        call(
            &state,
            "GET",
            "/api/operators",
            Req {
                bearer: true,
                ..Req::default()
            },
        )
        .await,
    )
    .await;
    assert_eq!(list["operators"][0]["current"], false);

    // Signing in again goes straight in, replacing the previous session.
    let flow = begin(&state, "/next").await;
    mount_token(&provider, &claims(&provider, &flow.nonce, None)).await;
    let again = call(
        &state,
        "GET",
        &format!("/auth/callback?code=test-code&state={}", flow.state),
        Req {
            cookie: Some(&format!("{}; {session}", flow.browser_cookie)),
            ..Req::default()
        },
    )
    .await;
    assert_eq!(again.status(), StatusCode::SEE_OTHER);
    assert_eq!(again.headers()[header::LOCATION], "/next");
    assert!(set_cookie(&again, ENROLL_COOKIE).is_none());
    let renewed = cookie_pair(set_cookie(&again, "__Host-muxa-op").unwrap());
    assert_eq!(
        status_of(&state, "/api/agents", &session).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status_of(&state, "/api/agents", &renewed).await,
        StatusCode::OK
    );
    assert!(state.operator.registry().enrollments.is_empty());
}

#[tokio::test]
async fn wrong_tokens_are_counted_backed_off_and_discard_the_enrollment() {
    let provider = provider().await;
    let state = state(&provider.uri());
    let cookie = pending(&state, &provider, "/").await;
    let reset = || state.operator.registry().backoff = Backoff::default();

    let first = submit(&state, Some(&cookie), "wrong").await;
    assert_eq!(first.status(), StatusCode::UNAUTHORIZED);
    assert!(set_cookie(&first, "__Host-muxa-op").is_none());
    assert_eq!(json_body(first).await["restart"], false);
    // The global backoff refuses an immediate retry without counting it,
    // even with the right token.
    let throttled = submit(&state, Some(&cookie), "operator-token").await;
    assert_eq!(throttled.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(throttled.headers().contains_key(header::RETRY_AFTER));
    for _ in 2..MAX_ENROLL_ATTEMPTS {
        reset();
        let response = submit(&state, Some(&cookie), "operator-token-").await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(json_body(response).await["restart"], false);
    }
    reset();
    let last = submit(&state, Some(&cookie), "").await;
    assert_eq!(last.status(), StatusCode::UNAUTHORIZED);
    assert!(set_cookie(&last, ENROLL_COOKIE)
        .unwrap()
        .contains("Max-Age=0"));
    assert_eq!(json_body(last).await["restart"], true);
    reset();
    // Discarded: even the right token no longer enrolls this sign-in.
    let late = submit(&state, Some(&cookie), "operator-token").await;
    assert_eq!(late.status(), StatusCode::UNAUTHORIZED);
    let registry = state.operator.registry();
    assert!(registry.enrolled.is_empty() && registry.sessions.is_empty());
    assert!(registry.enrollments.is_empty());
}

#[test]
fn enrollment_backoff_grows_and_is_capped() {
    let mut backoff = Backoff::default();
    assert!(backoff.retry_at().is_none());
    backoff.fail();
    let last = backoff.last.unwrap();
    assert_eq!(backoff.retry_at(), Some(last + Duration::from_secs(1)));
    for _ in 0..100 {
        backoff.fail();
    }
    let last = backoff.last.unwrap();
    assert_eq!(backoff.retry_at(), Some(last + MAX_ENROLL_BACKOFF));
    // A long quiet period starts the count over.
    backoff.last = Instant::now().checked_sub(ENROLL_BACKOFF_RESET);
    assert!(backoff.retry_at().is_none());
    backoff.fail();
    assert_eq!(backoff.failures, 1);
}

#[tokio::test]
async fn enrollment_needs_same_origin_proof_and_a_pending_cookie() {
    let provider = provider().await;
    let state = state(&provider.uri());
    let cookie = pending(&state, &provider, "/").await;
    let secret = cookie.split_once('=').unwrap().1.to_owned();
    let token = || Some(json!({"token": "operator-token"}));

    // CSRF: the right token and cookie, without the exact Origin + header.
    for (origin, operator_header) in [
        (None, true),
        (Some("https://evil.example.com"), true),
        (Some("http://dash.example.com"), true),
        (Some(PUBLIC), false),
    ] {
        let response = call(
            &state,
            "POST",
            "/auth/enroll",
            Req {
                cookie: Some(&cookie),
                origin,
                operator_header,
                body: token(),
                ..Req::default()
            },
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{origin:?} {operator_header}"
        );
    }
    // No pending cookie, a forged one, the dev name on HTTPS, an operator
    // session secret presented as a pending enrollment, or a duplicate.
    let live_session = session(&state);
    let session_secret = live_session.split_once('=').unwrap().1;
    for forged in [
        None,
        Some(format!("{ENROLL_COOKIE}=0123456789abcdef")),
        Some(format!("muxa-op-enroll-dev={secret}")),
        Some(format!("{ENROLL_COOKIE}={session_secret}")),
        Some(format!("{cookie}; {ENROLL_COOKIE}=other")),
    ] {
        let response = submit(&state, forged.as_deref(), "operator-token").await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{forged:?}");
        assert!(set_cookie(&response, "__Host-muxa-op").is_none());
        if let Some(forged) = &forged {
            assert_eq!(
                status_of(&state, "/auth/enroll", forged).await,
                StatusCode::UNAUTHORIZED
            );
        }
    }
    assert_eq!(
        call(&state, "GET", "/auth/enroll", Req::default())
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    // A bearer token is not a pending enrollment either.
    let bearer = call(
        &state,
        "POST",
        "/auth/enroll",
        Req {
            bearer: true,
            origin: Some(PUBLIC),
            operator_header: true,
            body: token(),
            ..Req::default()
        },
    )
    .await;
    assert_eq!(bearer.status(), StatusCode::UNAUTHORIZED);
    // Malformed bodies are refused without consuming an attempt.
    let malformed = call(
        &state,
        "POST",
        "/auth/enroll",
        Req {
            cookie: Some(&cookie),
            origin: Some(PUBLIC),
            operator_header: true,
            body: Some(json!({"password": "operator-token"})),
            ..Req::default()
        },
    )
    .await;
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    // None of that consumed an attempt or tripped the backoff.
    assert_eq!(state.operator.registry().enrollments[&secret].attempts, 0);
    let response = submit(&state, Some(&cookie), "operator-token").await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn cancelling_discards_the_pending_enrollment() {
    let provider = provider().await;
    let state = state(&provider.uri());
    let cookie = pending(&state, &provider, "/").await;
    let response = call(
        &state,
        "GET",
        "/auth/enroll/cancel",
        Req {
            cookie: Some(&cookie),
            ..Req::default()
        },
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/");
    assert!(set_cookie(&response, ENROLL_COOKIE)
        .unwrap()
        .contains("Max-Age=0"));
    assert_eq!(
        submit(&state, Some(&cookie), "operator-token")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One removal scenario across sessions.
async fn removal_ends_live_sessions_and_the_next_sign_in_enrolls_again() {
    let provider = provider().await;
    let state = state(&provider.uri());
    let cookie = pending(&state, &provider, "/").await;
    let first = submit(&state, Some(&cookie), "operator-token").await;
    let first = cookie_pair(set_cookie(&first, "__Host-muxa-op").unwrap());
    // A second browser signed in as the same enrolled account.
    let response = sign_in(&state, &provider, "/", None).await;
    let second = cookie_pair(set_cookie(&response, "__Host-muxa-op").unwrap());
    // And another operator, in the group, who stays signed in.
    let other = session(&state);
    let list = json_body(
        call(
            &state,
            "GET",
            "/api/operators",
            Req {
                cookie: Some(&first),
                ..Req::default()
            },
        )
        .await,
    )
    .await;
    let id = list["operators"][0]["id"].as_str().unwrap().to_owned();
    let remove = format!("/api/operators/{id}/remove");

    // Cookie-authorized removal needs the CSRF proof.
    for (origin, operator_header) in [(None, true), (Some(PUBLIC), false)] {
        let response = call(
            &state,
            "POST",
            &remove,
            Req {
                cookie: Some(&other),
                origin,
                operator_header,
                ..Req::default()
            },
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    // Unauthenticated callers cannot list or remove.
    assert_eq!(
        call(&state, "GET", "/api/operators", Req::default())
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&state, "POST", &remove, Req::default()).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let response = call(
        &state,
        "POST",
        &remove,
        Req {
            cookie: Some(&other),
            origin: Some(PUBLIC),
            operator_header: true,
            ..Req::default()
        },
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["sessions_ended"], 2);
    for ended in [&first, &second] {
        assert_eq!(
            status_of(&state, "/api/agents", ended).await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        status_of(&state, "/api/agents", &other).await,
        StatusCode::OK
    );
    // Removing again, or an unknown id, is a 404 (bearer needs no CSRF).
    for missing in [remove.as_str(), "/api/operators/not-an-id/remove"] {
        let response = call(
            &state,
            "POST",
            missing,
            Req {
                bearer: true,
                ..Req::default()
            },
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{missing}");
    }
    // The next sign-in lands on the enrollment page again.
    pending(&state, &provider, "/").await;
}

#[tokio::test]
async fn enrollment_is_rechecked_on_every_request() {
    let provider = provider().await;
    let state = state(&provider.uri());
    let cookie = pending(&state, &provider, "/").await;
    let response = submit(&state, Some(&cookie), "operator-token").await;
    let session = cookie_pair(set_cookie(&response, "__Host-muxa-op").unwrap());
    assert_eq!(
        status_of(&state, "/api/agents", &session).await,
        StatusCode::OK
    );
    // The entry vanishing (not through the removal route) is enough.
    state.operator.registry().enrolled.clear();
    assert_eq!(
        status_of(&state, "/api/agents", &session).await,
        StatusCode::UNAUTHORIZED
    );
    assert!(state.operator.registry().sessions.is_empty());
}

#[tokio::test]
async fn entries_for_another_issuer_never_match() {
    let provider = provider().await;
    let state = state(&provider.uri());
    let foreign = enrolled("https://other-issuer.example.com", "owner-subject");
    state.operator.registry().enrolled.push(foreign.clone());
    // Same subject, other issuer: still needs enrollment.
    pending(&state, &provider, "/").await;
    // A session claiming that issuer through enrollment is not honored.
    let secret = oidc::random_secret();
    state.operator.registry().sessions.insert(
        secret.clone(),
        Session {
            issuer: foreign.issuer.clone(),
            subject: foreign.subject.clone(),
            email: None,
            via: Via::Enrollment,
            expires: Instant::now() + SESSION_TTL,
        },
    );
    assert_eq!(
        status_of(&state, "/api/agents", &format!("__Host-muxa-op={secret}")).await,
        StatusCode::UNAUTHORIZED
    );
    // It is listed (so it can be removed) but marked inactive.
    let list = json_body(
        call(
            &state,
            "GET",
            "/api/operators",
            Req {
                bearer: true,
                ..Req::default()
            },
        )
        .await,
    )
    .await;
    assert_eq!(list["operators"][0]["active"], false);
}

#[tokio::test]
async fn group_members_skip_enrollment_and_pending_enrollments_are_bounded() {
    let provider = provider().await;
    let state = state(&provider.uri());
    let response = sign_in(&state, &provider, "/", Some(json!(["operator"]))).await;
    assert_eq!(response.headers()[header::LOCATION], "/");
    assert!(set_cookie(&response, ENROLL_COOKIE).is_none());
    assert!(state.operator.registry().enrolled.is_empty());
    for _ in 0..MAX_PENDING_ENROLLMENTS {
        pending(&state, &provider, "/").await;
    }
    let response = sign_in(&state, &provider, "/", None).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(set_cookie(&response, ENROLL_COOKIE).is_none());
}

#[tokio::test]
async fn enrolled_operators_are_bounded() {
    let provider = provider().await;
    let state = state(&provider.uri());
    for n in 0..MAX_ENROLLED {
        state
            .operator
            .registry()
            .enrolled
            .push(enrolled(&provider.uri(), &format!("someone-{n}")));
    }
    let cookie = pending(&state, &provider, "/").await;
    let response = submit(&state, Some(&cookie), "operator-token").await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(set_cookie(&response, "__Host-muxa-op").is_none());
    assert!(set_cookie(&response, ENROLL_COOKIE)
        .unwrap()
        .contains("Max-Age=0"));
}

#[tokio::test]
async fn enrollments_are_durable_private_and_exclusive() {
    use std::os::unix::fs::PermissionsExt;
    let provider = provider().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("operators/operators.sqlite3");
    let config = login_config(&provider.uri());
    {
        let mut state = state(&provider.uri());
        state.operator = OperatorLogin::open(Some(config.clone()), Some(path.clone()))
            .await
            .unwrap();
        // A second daemon cannot open the same store concurrently.
        assert!(
            OperatorLogin::open(Some(config.clone()), Some(path.clone()))
                .await
                .is_err()
        );
        let cookie = pending(&state, &provider, "/").await;
        assert_eq!(
            submit(&state, Some(&cookie), "operator-token")
                .await
                .status(),
            StatusCode::OK
        );
    }
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let dir_mode = std::fs::metadata(path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700);

    // After a restart the account signs straight in, and removal persists.
    let mut state = state(&provider.uri());
    state.operator = OperatorLogin::open(Some(config.clone()), Some(path.clone()))
        .await
        .unwrap();
    let entry = state.operator.registry().enrolled[0].clone();
    assert_eq!(entry.subject, "owner-subject");
    let response = sign_in(&state, &provider, "/", None).await;
    assert!(set_cookie(&response, "__Host-muxa-op").is_some());
    assert!(state.operator.registry().enrolled[0].last_seen_at >= entry.created_at);
    let response = call(
        &state,
        "POST",
        &format!("/api/operators/{}/remove", entry.id),
        Req {
            bearer: true,
            ..Req::default()
        },
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    drop(state);
    let reopened = OperatorLogin::open(Some(config.clone()), Some(path.clone()))
        .await
        .unwrap();
    assert!(reopened.registry().enrolled.is_empty());
    drop(reopened);

    // With enrollment off nothing is opened or created.
    let mut disabled = config;
    disabled.enrollment = Some(false);
    let other = dir.path().join("disabled/operators.sqlite3");
    let login = OperatorLogin::open(Some(disabled), Some(other.clone()))
        .await
        .unwrap();
    assert!(login.storage.is_none() && !other.exists());
    // A file that is not an operator store is refused rather than adopted.
    let foreign = dir.path().join("foreign.sqlite3");
    rusqlite::Connection::open(&foreign)
        .unwrap()
        .execute_batch("CREATE TABLE unrelated (x INTEGER);")
        .unwrap();
    assert!(
        OperatorLogin::open(Some(login_config(&provider.uri())), Some(foreign))
            .await
            .is_err()
    );
}
