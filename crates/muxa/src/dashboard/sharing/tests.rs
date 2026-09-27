use super::*;
use crate::backend::{BackendCaps, PaneBackend};
use crate::dashboard::{
    server::{router, AppState},
    DashboardConfig,
};
use crate::tmux::PaneInfo;
use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    response::Response,
};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Backend {
    panes: std::sync::Mutex<Vec<PaneInfo>>,
    sent: std::sync::Mutex<Vec<(String, String, String)>>,
    send_gate: std::sync::Mutex<Option<Arc<std::sync::Barrier>>>,
    degraded: std::sync::atomic::AtomicBool,
}
impl PaneBackend for Backend {
    fn kind(&self) -> crate::HostKind {
        crate::HostKind::Tmux
    }
    fn list_panes(&self) -> Vec<PaneInfo> {
        self.panes.lock().unwrap().clone()
    }
    fn observe_panes(&self) -> crate::backend::PaneObservation {
        if self.degraded.load(std::sync::atomic::Ordering::Relaxed) {
            crate::backend::PaneObservation::incomplete(Vec::new())
        } else {
            crate::backend::PaneObservation::complete(self.list_panes())
        }
    }
    fn resolve_pane(&self, id: &str) -> Option<PaneInfo> {
        self.list_panes()
            .into_iter()
            .find(|pane| pane.pane_id == id)
    }
    fn capture_pane(&self, _: &str) -> Option<String> {
        panic!("capture must pin the socket")
    }
    fn capture_pane_on(&self, socket: Option<&str>, pane: &str) -> Option<String> {
        Some(format!("{socket:?}:{pane}:<script>not HTML</script>"))
    }
    fn pane_pid_map(&self) -> HashMap<u32, String> {
        HashMap::new()
    }
    fn current_pane(&self) -> Option<String> {
        None
    }
    fn focus_pane(&self, _: &str) -> bool {
        false
    }
    fn send_text(&self, _: &str, _: &str) -> bool {
        panic!("send must pin the socket")
    }
    fn send_text_on(&self, socket: Option<&str>, pane: &str, text: &str) -> bool {
        self.sent
            .lock()
            .unwrap()
            .push((socket.unwrap().into(), pane.into(), text.into()));
        let gate = self.send_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.wait();
        }
        true
    }
    fn caps(&self) -> BackendCaps {
        BackendCaps::default()
    }
}

fn pane(socket: &str) -> PaneInfo {
    serde_json::from_value(json!({"pane_id":"%1", "session_id":"$1", "session":"test", "window_id":"@1", "window_index":"0", "pane_index":"0", "tty":"test-tty", "current_command":"shell", "title":"test", "pane_pid":std::process::id(), "socket": socket})).unwrap()
}

fn state() -> (AppState, Arc<Backend>) {
    let mut config = DashboardConfig::loopback_default();
    config.token = Some("operator".into());
    config.sharing = Some(SharingConfig {
        public_url: "https://share.example.com".into(),
        issuer_url: "https://issuer.example.com".into(),
        client_id: "muxa".into(),
        client_secret_env: None,
        storage_path: None,
    });
    let backend = Arc::new(Backend {
        panes: std::sync::Mutex::new(vec![pane("one"), pane("two")]),
        sent: std::sync::Mutex::new(Vec::new()),
        send_gate: std::sync::Mutex::new(None),
        degraded: std::sync::atomic::AtomicBool::new(false),
    });
    let state = AppState::new(
        crate::Store::shared(),
        Arc::new(config),
        Arc::new(crate::tmux::scanner::PaneCache::new(Duration::from_secs(1))),
        crate::session::PtySessionBackend::shared(),
    )
    .with_backend(backend.clone());
    (state, backend)
}

async fn call(
    state: &AppState,
    method: &str,
    path: &str,
    body: Value,
    cookie: Option<&str>,
    admin: bool,
    origin: Option<&str>,
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-muxa-share", "1");
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    if let Some(origin) = origin {
        request = request.header(header::ORIGIN, origin);
    }
    if admin {
        request = request.header(header::AUTHORIZATION, "Bearer operator");
    }
    router(state.clone())
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

async fn body(response: Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
}

async fn create(state: &AppState, permission: &str) -> String {
    let response = call(state, "POST", "/api/shares", json!({"pane":"%1", "socket":"two", "email":"guest@example.com", "permission":permission, "ttl_seconds":3600}), None, true, None).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    body(response).await["id"].as_str().unwrap().to_owned()
}

async fn session(state: &AppState, email: &str, subject: &str) -> String {
    let secret = random_secret();
    state.sharing.registry.lock().await.sessions.insert(
        secret.clone(),
        Session {
            subject: subject.into(),
            email: email.into(),
            expires: Instant::now() + SESSION_TTL,
        },
    );
    format!("__Host-muxa-share={secret}")
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One end-to-end access isolation scenario.
async fn recipient_cannot_escape_share_or_escalate_to_operator() {
    let (state, backend) = state();
    let id = create(&state, "prompt").await;
    let path = format!("/share/api/{id}");
    let cookie = session(&state, "guest@example.com", "guest-1").await;
    assert_eq!(
        call(&state, "GET", &path, json!(null), None, true, None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &state,
            "GET",
            "/api/panes",
            json!(null),
            Some(&cookie),
            false,
            None
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &state,
            "POST",
            "/api/shares",
            json!({}),
            Some(&cookie),
            false,
            None
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let response = call(
        &state,
        "GET",
        &path,
        json!(null),
        Some(&cookie),
        false,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let data = body(response).await;
    assert!(data["output"].as_str().unwrap().contains("two"));
    assert!(data.get("agents").is_none());
    for origin in [None, Some("https://evil.example")] {
        assert_eq!(
            call(
                &state,
                "POST",
                &format!("{path}/prompt"),
                json!({"text":"hello"}),
                Some(&cookie),
                false,
                origin
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        call(
            &state,
            "POST",
            &format!("{path}/prompt"),
            json!({"text":"hello", "pane":"%2"}),
            Some(&cookie),
            false,
            Some("https://share.example.com")
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert!(backend.sent.lock().unwrap().is_empty());
    assert_eq!(
        call(
            &state,
            "POST",
            &format!("{path}/prompt"),
            json!({"text":"hello"}),
            Some(&cookie),
            false,
            Some("https://share.example.com")
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        *backend.sent.lock().unwrap(),
        vec![
            (
                crate::tmux::socket_path_or_default("two")
                    .to_string_lossy()
                    .into_owned(),
                "%1".into(),
                "hello".into()
            ),
            (
                crate::tmux::socket_path_or_default("two")
                    .to_string_lossy()
                    .into_owned(),
                "%1".into(),
                "\r".into()
            )
        ]
    );
    let outsider = session(&state, "other@example.com", "other").await;
    assert_eq!(
        call(
            &state,
            "GET",
            &path,
            json!(null),
            Some(&outsider),
            false,
            None
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let recycled_email = session(&state, "guest@example.com", "different-subject").await;
    assert_eq!(
        call(
            &state,
            "GET",
            &path,
            json!(null),
            Some(&recycled_email),
            false,
            None
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn view_only_revocation_expiry_and_logout_are_enforced() {
    let (state, backend) = state();
    let id = create(&state, "view").await;
    let path = format!("/share/api/{id}");
    let cookie = session(&state, "guest@example.com", "guest").await;
    assert_eq!(
        call(
            &state,
            "POST",
            &format!("{path}/prompt"),
            json!({"text":"no"}),
            Some(&cookie),
            false,
            Some("https://share.example.com")
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert!(backend.sent.lock().unwrap().is_empty());
    assert_eq!(
        call(
            &state,
            "POST",
            &format!("/api/shares/{id}/revoke"),
            json!({}),
            None,
            true,
            None
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(
            &state,
            "GET",
            &path,
            json!(null),
            Some(&cookie),
            false,
            None
        )
        .await
        .status(),
        StatusCode::GONE
    );
    let id = create(&state, "view").await;
    state.sharing.registry.lock().await.grants[&id]
        .lock()
        .await
        .expires = Instant::now();
    assert_eq!(
        call(
            &state,
            "GET",
            &format!("/share/api/{id}"),
            json!(null),
            Some(&cookie),
            false,
            None
        )
        .await
        .status(),
        StatusCode::GONE
    );
    assert_eq!(
        call(
            &state,
            "POST",
            "/share/auth/logout",
            json!({}),
            Some(&cookie),
            false,
            Some("https://share.example.com")
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(
            &state,
            "GET",
            &path,
            json!(null),
            Some(&cookie),
            false,
            None
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn changed_pane_incarnation_and_ambiguous_socket_are_refused() {
    let (state, backend) = state();
    let response = call(
        &state,
        "POST",
        "/api/shares",
        json!({"pane":"%1", "email":"guest@example.com", "permission":"view", "ttl_seconds":3600}),
        None,
        true,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let id = create(&state, "prompt").await;
    let cookie = session(&state, "guest@example.com", "guest").await;
    backend.panes.lock().unwrap()[1].pane_pid += 1;
    assert_eq!(
        call(
            &state,
            "POST",
            &format!("/share/api/{id}/prompt"),
            json!({"text":"must not send"}),
            Some(&cookie),
            false,
            Some("https://share.example.com")
        )
        .await
        .status(),
        StatusCode::GONE
    );
    assert!(backend.sent.lock().unwrap().is_empty());
}

#[test]
fn sharing_config_requires_safe_origins_and_private_dashboard() {
    let (state, _) = state();
    let config = state.config.sharing.as_ref().unwrap();
    assert!(config.validate().is_ok());
    assert!(config.matches_host("share.example.com"));
    assert!(!config.matches_host("share.example.com.evil"));
    for invalid in [
        "http://public.example",
        "https://user:password@example.com",
        "https://example.com/path",
        "https://example.com/#fragment",
    ] {
        let mut config = config.clone();
        config.public_url = invalid.into();
        assert!(config.validate().is_err());
    }
    let toml = crate::config::DashboardTomlConfig {
        sharing: Some(Box::new(config.clone())),
        auth: Some(crate::config::DashboardAuthMode::PublicRead),
        token: Some("operator".into()),
        ..Default::default()
    };
    assert!(
        DashboardConfig::resolve(&toml, &crate::dashboard::DashboardOverrides::default()).is_err()
    );
}

// A publicly known test-only Ed25519 seed encoded as PKCS#8, never a provider key.
fn test_key() -> openidconnect::core::CoreEdDsaPrivateSigningKey {
    use base64::Engine;
    let mut der = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    der.extend_from_slice(&[42; 32]);
    let pem = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        base64::engine::general_purpose::STANDARD.encode(der)
    );
    openidconnect::core::CoreEdDsaPrivateSigningKey::from_ed25519_pem(
        &pem,
        Some(openidconnect::JsonWebKeyId::new("test".into())),
    )
    .unwrap()
}

fn signed_token(claims: &Value) -> String {
    use base64::Engine;
    use openidconnect::PrivateSigningKey;
    let encode = |data: Vec<u8>| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data);
    let input = format!(
        "{}.{}",
        encode(serde_json::to_vec(&json!({"alg":"EdDSA","kid":"test"})).unwrap()),
        encode(serde_json::to_vec(claims).unwrap())
    );
    let signature = test_key()
        .sign(
            &openidconnect::core::CoreJwsSigningAlgorithm::EdDsa,
            input.as_bytes(),
        )
        .unwrap();
    format!("{input}.{}", encode(signature))
}

#[allow(clippy::too_many_lines)] // Provider fixture and complete authorization-code exchange.
async fn oidc_flow(invalid: Option<&str>) {
    use openidconnect::PrivateSigningKey;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };
    let provider = MockServer::start().await;
    Mock::given(method("GET")).and(path("/.well-known/openid-configuration")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "issuer":provider.uri(), "authorization_endpoint":format!("{}/authorize",provider.uri()), "token_endpoint":format!("{}/token",provider.uri()), "jwks_uri":format!("{}/jwks",provider.uri()),
        "response_types_supported":["code"], "subject_types_supported":["public"], "id_token_signing_alg_values_supported":["EdDSA"]
    }))).mount(&provider).await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"keys":[test_key().as_verification_key()]})),
        )
        .mount(&provider)
        .await;
    let (mut state, _) = state();
    let mut config = state.config.sharing.clone().unwrap();
    config.issuer_url = provider.uri();
    state.sharing = Sharing::new(Some(config));
    let id = create(&state, "view").await;
    let response = call(
        &state,
        "GET",
        &format!("/share/auth/login?share={id}"),
        json!(null),
        None,
        false,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let flow_cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let authorize =
        url::Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    let query: HashMap<_, _> = authorize.query_pairs().into_owned().collect();
    assert_eq!(query["code_challenge_method"], "S256");
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let mut claims = json!({"iss":provider.uri(), "sub":"guest-subject", "aud":"muxa", "iat":now, "exp":now+300, "nonce":query["nonce"], "email":"guest@example.com", "email_verified":true});
    match invalid {
        Some("nonce") => claims["nonce"] = json!("wrong"),
        Some("issuer") => claims["iss"] = json!("https://wrong.example"),
        Some("audience") => claims["aud"] = json!("wrong-client"),
        Some("expiry") => claims["exp"] = json!(now - 3600),
        Some("email") => claims["email_verified"] = json!(false),
        Some("recipient") => claims["email"] = json!("uninvited@example.com"),
        None | Some("signature") => {}
        Some(other) => panic!("invalid test case {other}"),
    }
    let mut token = signed_token(&claims);
    if invalid == Some("signature") {
        let start = token.rfind('.').unwrap() + 1;
        let replacement = if &token[start..=start] == "A" {
            "B"
        } else {
            "A"
        };
        token.replace_range(start..=start, replacement);
    }
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"access_token":"test-access", "token_type":"Bearer", "id_token":token}),
        ))
        .expect(1)
        .mount(&provider)
        .await;
    let callback = format!(
        "/share/auth/callback?code=test-code&state={}",
        query["state"]
    );
    assert_eq!(
        call(&state, "GET", &callback, json!(null), None, false, None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let response = call(
        &state,
        "GET",
        &callback,
        json!(null),
        Some(&flow_cookie),
        false,
        None,
    )
    .await;
    if invalid.is_some() {
        assert!(
            [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN].contains(&response.status()),
            "{invalid:?}: {}",
            response.status()
        );
        assert!(!response.headers().contains_key(header::SET_COOKIE));
    } else {
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], format!("/share/{id}"));
        let session_cookie = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .find(|h| h.to_str().unwrap().starts_with("__Host-muxa-share="))
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            session_cookie.contains("Secure")
                && session_cookie.contains("HttpOnly")
                && session_cookie.contains("SameSite=Lax")
        );
        let session_cookie = session_cookie.split(';').next().unwrap();
        assert_eq!(
            call(
                &state,
                "GET",
                &format!("/share/api/{id}"),
                json!(null),
                Some(session_cookie),
                false,
                None
            )
            .await
            .status(),
            StatusCode::OK
        );
    }
    // Callback replay cannot exchange a second authorization code.
    assert_eq!(
        call(
            &state,
            "GET",
            &callback,
            json!(null),
            Some(&flow_cookie),
            false,
            None
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let requests = provider.received_requests().await.unwrap();
    let exchange = requests
        .iter()
        .find(|request| request.url.path() == "/token")
        .unwrap();
    let form: HashMap<_, _> = url::form_urlencoded::parse(&exchange.body)
        .into_owned()
        .collect();
    let verifier = openidconnect::PkceCodeVerifier::new(form["code_verifier"].clone());
    assert_eq!(
        openidconnect::PkceCodeChallenge::from_code_verifier_sha256(&verifier).as_str(),
        query["code_challenge"]
    );
}

#[tokio::test]
async fn oidc_code_flow_verifies_identity_and_pkce_and_rejects_replay() {
    oidc_flow(None).await;
}

#[tokio::test]
async fn oidc_rejects_untrusted_identity_claims() {
    for invalid in [
        "signature",
        "nonce",
        "issuer",
        "audience",
        "expiry",
        "email",
        "recipient",
    ] {
        oidc_flow(Some(invalid)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoke_waits_for_admitted_send_even_after_handler_cancellation() {
    let (state, backend) = state();
    let id = create(&state, "prompt").await;
    let cookie = session(&state, "guest@example.com", "guest").await;
    let gate = Arc::new(std::sync::Barrier::new(2));
    *backend.send_gate.lock().unwrap() = Some(gate.clone());
    let send = tokio::spawn({
        let state = state.clone();
        let id = id.clone();
        let cookie = cookie.clone();
        async move {
            call(
                &state,
                "POST",
                &format!("/share/api/{id}/prompt"),
                json!({"text":"hello"}),
                Some(&cookie),
                false,
                Some("https://share.example.com"),
            )
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while backend.sent.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    send.abort();
    let mut revoke = tokio::spawn({
        let state = state.clone();
        let id = id.clone();
        async move {
            call(
                &state,
                "POST",
                &format!("/api/shares/{id}/revoke"),
                json!(null),
                None,
                true,
                None,
            )
            .await
        }
    });
    let waited = tokio::time::timeout(Duration::from_millis(50), &mut revoke)
        .await
        .is_err();
    // Always release the blocking worker, including on assertion failure.
    tokio::task::spawn_blocking(move || gate.wait())
        .await
        .unwrap();
    assert!(waited, "revocation must wait for the admitted send");
    assert_eq!(revoke.await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(backend.sent.lock().unwrap().len(), 2);
    assert_eq!(
        call(
            &state,
            "POST",
            &format!("/share/api/{id}/prompt"),
            json!({"text":"again"}),
            Some(&cookie),
            false,
            Some("https://share.example.com")
        )
        .await
        .status(),
        StatusCode::GONE
    );
    assert_eq!(backend.sent.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn dashboard_absolute_socket_is_pinned_and_mismatched_path_is_rejected() {
    let (state, _) = state();
    let socket = crate::tmux::socket_path_or_default("two")
        .to_string_lossy()
        .into_owned();
    let body = json!({"pane":"%1", "socket":socket, "email":"guest@example.com", "permission":"view", "ttl_seconds":3600});
    let response = call(
        &state,
        "POST",
        "/api/shares",
        body.clone(),
        None,
        true,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let result: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    let grant = state.sharing.registry.lock().await.grants[result["id"].as_str().unwrap()].clone();
    assert_eq!(grant.lock().await.target.socket, socket);
    let mut wrong = body;
    wrong["socket"] = json!("/unrelated/server/two");
    assert_eq!(
        call(&state, "POST", "/api/shares", wrong, None, true, None)
            .await
            .status(),
        StatusCode::CONFLICT
    );
}

async fn durable_state(path: &std::path::Path) -> (AppState, Arc<Backend>) {
    let (mut state, backend) = state();
    let mut config = state.sharing.config.clone().unwrap();
    config.storage_path = Some(path.to_owned());
    state.sharing = Sharing::open(Some(config)).await.unwrap();
    (state, backend)
}

async fn submit(
    state: &AppState,
    id: &str,
    cookie: &str,
    request_id: &str,
    text: &str,
) -> Response {
    call(
        state,
        "POST",
        &format!("/share/api/{id}/prompt"),
        json!({"text":text,"request_id":request_id}),
        Some(cookie),
        false,
        Some("https://share.example.com"),
    )
    .await
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One lifecycle across two daemon restarts.
async fn restart_keeps_identity_receipts_and_revocations_but_requires_login() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shares.sqlite3");
    let (state, backend) = durable_state(&path).await;
    let id = create(&state, "prompt").await;
    let cookie = session(&state, "guest@example.com", "guest").await;
    assert_eq!(
        submit(&state, &id, &cookie, "receipt-1", "private-prompt-marker")
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        submit(&state, &id, &cookie, "receipt-1", "private-prompt-marker")
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        submit(&state, &id, &cookie, "receipt-1", "changed")
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(backend.sent.lock().unwrap().len(), 2);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let bytes = std::fs::read(&path).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("private-prompt-marker"));
    drop(state);
    let (state, backend) = durable_state(&path).await;
    let route = format!("/share/api/{id}");
    assert_eq!(
        call(
            &state,
            "GET",
            &route,
            json!(null),
            Some(&cookie),
            false,
            None
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let stranger = session(&state, "guest@example.com", "recycled-account").await;
    assert_eq!(
        call(
            &state,
            "GET",
            &route,
            json!(null),
            Some(&stranger),
            false,
            None
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let cookie = session(&state, "guest@example.com", "guest").await;
    assert_eq!(
        call(
            &state,
            "GET",
            &route,
            json!(null),
            Some(&cookie),
            false,
            None
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        submit(&state, &id, &cookie, "receipt-1", "private-prompt-marker")
            .await
            .status(),
        StatusCode::OK
    );
    assert!(backend.sent.lock().unwrap().is_empty());
    assert_eq!(
        call(
            &state,
            "POST",
            &format!("/api/shares/{id}/revoke"),
            json!(null),
            None,
            true,
            None
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    drop(state);
    let (state, _) = durable_state(&path).await;
    let cookie = session(&state, "guest@example.com", "guest").await;
    assert_eq!(
        call(
            &state,
            "GET",
            &route,
            json!(null),
            Some(&cookie),
            false,
            None
        )
        .await
        .status(),
        StatusCode::GONE
    );
}

#[tokio::test]
async fn unfinished_delivery_is_never_replayed_after_restart() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shares.sqlite3");
    let (state, _) = durable_state(&path).await;
    let id = create(&state, "prompt").await;
    let grant = state.sharing.registry.lock().await.grants[&id].clone();
    let mut guard = grant.lock_owned().await;
    guard.deliveries.insert(
        "interrupted".into(),
        Delivery {
            digest: format!("{:x}", Sha256::digest(b"%1\0command")),
            outcome: "pending".into(),
        },
    );
    drop(state.sharing.persist(guard).await.unwrap());
    drop(state);
    let (state, backend) = durable_state(&path).await;
    let cookie = session(&state, "guest@example.com", "guest").await;
    assert_eq!(
        submit(&state, &id, &cookie, "interrupted", "command")
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert!(backend.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn storage_lock_identity_and_corruption_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shares.sqlite3");
    let (state, _) = durable_state(&path).await;
    let mut config = state.sharing.config.clone().unwrap();
    assert!(Sharing::open(Some(config.clone())).await.is_err());
    drop(state);
    config.client_id = "another-app".into();
    assert!(Sharing::open(Some(config.clone())).await.is_err());
    std::fs::write(&path, "corrupt state").unwrap();
    assert!(Sharing::open(Some(config)).await.is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "corrupt state");
}

#[tokio::test]
async fn failed_storage_never_acknowledges_binding_or_executes_commands() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shares.sqlite3");
    let (state, backend) = durable_state(&path).await;
    let id = create(&state, "prompt").await;
    let cookie = session(&state, "guest@example.com", "guest").await;
    state.sharing.storage.as_ref().unwrap().test_read_only(true);
    for _ in 0..2 {
        assert_eq!(
            call(
                &state,
                "GET",
                &format!("/share/api/{id}"),
                json!(null),
                Some(&cookie),
                false,
                None
            )
            .await
            .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
    assert_eq!(
        submit(&state, &id, &cookie, "write-failed", "command")
            .await
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(backend.sent.lock().unwrap().is_empty());
    assert_eq!(
        call(
            &state,
            "POST",
            &format!("/api/shares/{id}/revoke"),
            json!(null),
            None,
            true,
            None
        )
        .await
        .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    state
        .sharing
        .storage
        .as_ref()
        .unwrap()
        .test_read_only(false);
    assert_eq!(
        call(
            &state,
            "POST",
            &format!("/api/shares/{id}/revoke"),
            json!(null),
            None,
            true,
            None
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn logout_all_only_ends_the_authenticated_accounts_sessions() {
    let (state, _) = state();
    let id = create(&state, "view").await;
    let cookie = session(&state, "guest@example.com", "guest").await;
    let second = session(&state, "guest@example.com", "guest").await;
    let other = session(&state, "other@example.com", "other").await;
    assert_eq!(
        call(
            &state,
            "POST",
            "/share/auth/logout-all",
            json!(null),
            Some(&cookie),
            false,
            Some("https://share.example.com")
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    let path = format!("/share/api/{id}");
    assert_eq!(
        call(
            &state,
            "GET",
            &path,
            json!(null),
            Some(&second),
            false,
            None
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&state, "GET", &path, json!(null), Some(&other), false, None)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separate_invitations_to_one_pane_cannot_interleave_commands() {
    let (state, backend) = state();
    let first = create(&state, "prompt").await;
    let second = create(&state, "prompt").await;
    let cookie = session(&state, "guest@example.com", "guest").await;
    let gate = Arc::new(std::sync::Barrier::new(2));
    *backend.send_gate.lock().unwrap() = Some(gate.clone());
    let send = tokio::spawn({
        let state = state.clone();
        let cookie = cookie.clone();
        async move { submit(&state, &first, &cookie, "first", "first").await }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while backend.sent.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let mut next = tokio::spawn({
        let state = state.clone();
        async move { submit(&state, &second, &cookie, "second", "second").await }
    });
    let waited = tokio::time::timeout(Duration::from_millis(50), &mut next)
        .await
        .is_err();
    let count = backend.sent.lock().unwrap().len();
    tokio::task::spawn_blocking(move || gate.wait())
        .await
        .unwrap();
    assert!(waited);
    assert_eq!(count, 1);
    assert_eq!(send.await.unwrap().status(), StatusCode::OK);
    assert_eq!(next.await.unwrap().status(), StatusCode::OK);
    assert_eq!(
        backend
            .sent
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, text)| text.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "\r", "second", "\r"]
    );
}

#[tokio::test]
async fn transient_inventory_failure_is_recoverable_without_a_new_invitation() {
    let (state, backend) = state();
    let id = create(&state, "view").await;
    let cookie = session(&state, "guest@example.com", "guest").await;
    backend
        .degraded
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let path = format!("/share/api/{id}");
    assert_eq!(
        call(
            &state,
            "GET",
            &path,
            json!(null),
            Some(&cookie),
            false,
            None
        )
        .await
        .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    backend
        .degraded
        .store(false, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        call(
            &state,
            "GET",
            &path,
            json!(null),
            Some(&cookie),
            false,
            None
        )
        .await
        .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn window_shares_pin_members_and_reject_new_panes_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shares.sqlite3");
    let (state, backend) = durable_state(&path).await;
    let mut peer = pane("two");
    peer.pane_id = "%2".into();
    backend.panes.lock().unwrap().push(peer.clone());
    let response = call(&state,"POST","/api/shares",json!({"pane":"%1","socket":"two","scope":"window","email":"guest@example.com","permission":"prompt","ttl_seconds":3600}),None,true,None).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let id = body(response).await["id"].as_str().unwrap().to_owned();
    drop(state);
    let (state, backend) = durable_state(&path).await;
    let mut newer = peer.clone();
    newer.pane_id = "%3".into();
    backend.panes.lock().unwrap().extend([peer, newer]);
    let cookie = session(&state, "guest@example.com", "guest").await;
    let route = format!("/share/api/{id}");
    let response = call(
        &state,
        "GET",
        &format!("{route}?pane=%252"),
        json!(null),
        Some(&cookie),
        false,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let data = body(response).await;
    assert_eq!(data["panes"], json!(["%1", "%2"]));
    assert_eq!(data["pane"], "%2");
    let command = json!({"text":"hello", "request_id":"one", "pane":"%2"});
    assert_eq!(
        call(
            &state,
            "POST",
            &format!("{route}/prompt"),
            command,
            Some(&cookie),
            false,
            Some("https://share.example.com")
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(backend.sent.lock().unwrap()[0].1, "%2");
    assert_eq!(
        submit(&state, &id, &cookie, "one", "hello").await.status(),
        StatusCode::CONFLICT
    );
    let invalid = json!({"text":"hello", "request_id":"two", "pane":"%3"});
    assert_eq!(
        call(
            &state,
            "POST",
            &format!("{route}/prompt"),
            invalid,
            Some(&cookie),
            false,
            Some("https://share.example.com")
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(backend.sent.lock().unwrap().len(), 2);
    backend
        .panes
        .lock()
        .unwrap()
        .retain(|pane| pane.pane_id != "%1");
    let response = call(
        &state,
        "GET",
        &route,
        json!(null),
        Some(&cookie),
        false,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body(response).await["pane_available"], false);
    assert_eq!(
        call(
            &state,
            "GET",
            &format!("{route}?pane=%252"),
            json!(null),
            Some(&cookie),
            false,
            None
        )
        .await
        .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn foreign_sqlite_database_is_not_modified() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("other.sqlite3");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch("CREATE TABLE unrelated(value TEXT); INSERT INTO unrelated VALUES ('keep');")
        .unwrap();
    drop(connection);
    let before = std::fs::read(&path).unwrap();
    let (state, _) = state();
    let mut config = state.sharing.config.clone().unwrap();
    config.storage_path = Some(path.clone());
    assert!(Sharing::open(Some(config)).await.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_creation_keeps_the_committed_invitation_registered() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shares.sqlite3");
    let (state, _) = durable_state(&path).await;
    let gate = state
        .sharing
        .storage
        .as_ref()
        .unwrap()
        .test_block_next_write();
    let request = tokio::spawn({
        let state = state.clone();
        async move { create(&state, "view").await }
    });
    let entered = gate.clone();
    tokio::task::spawn_blocking(move || entered.0.wait())
        .await
        .unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    let entries: Vec<_> = state
        .sharing
        .registry
        .lock()
        .await
        .grants
        .values()
        .cloned()
        .collect();
    tokio::task::spawn_blocking(move || gate.1.wait())
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    let id = entries[0].lock().await.id.clone();
    drop(entries);
    drop(state);
    let (state, _) = durable_state(&path).await;
    assert!(state.sharing.registry.lock().await.grants.contains_key(&id));
}
