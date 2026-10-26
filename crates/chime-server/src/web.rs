use crate::admin::{
    AddFinishRequest, AdminError, AdminFinishRequest, AdminRequest, AdminResponse, AdminState,
    SetupFinishRequest, SetupStartRequest,
};
use crate::notify::NotifyState;
use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, FromRequest, Path, State},
    http::{HeaderName, HeaderValue, Request, StatusCode, Uri, header},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use include_dir::{Dir, include_dir};
use serde::Serialize;

static WEB_ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../web");

const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; base-uri 'none'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; manifest-src 'self'; img-src 'self'; script-src 'self'; style-src 'self'";
const MAX_JSON_BODY_BYTES: usize = 16 * 1024;

#[derive(Clone)]
struct AppState {
    admin: AdminState,
    notify: NotifyState,
}

pub fn router(admin: AdminState, notifications: NotifyState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/setup/status", get(setup_status))
        .route("/api/push-key", get(push_key))
        .route("/api/notify", post(notify))
        .route("/api/setup/start", post(setup_start))
        .route("/api/setup/finish", post(setup_finish))
        .route("/api/admin/challenge", post(admin_challenge))
        .route("/api/admin/finish", post(admin_finish))
        .route("/api/admin/add/finish", post(add_finish))
        .route("/{*path}", get(asset))
        .fallback(not_found)
        .with_state(AppState {
            admin,
            notify: notifications,
        })
        .layer(DefaultBodyLimit::max(MAX_JSON_BODY_BYTES))
        .layer(middleware::from_fn(security_headers))
}

async fn index() -> Response {
    asset_response("index.html")
}

async fn asset(Path(path): Path<String>) -> Response {
    asset_response(&path)
}

async fn not_found(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if WEB_ASSETS.get_file(path).is_some() {
        return asset_response(path);
    }
    error_response(StatusCode::NOT_FOUND, "Not found.")
}

async fn setup_status(State(state): State<AppState>) -> Response {
    match state.admin.setup_status().await {
        Ok(status) => api_json(StatusCode::OK, status),
        Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Chime could not load setup status.",
        ),
    }
}

async fn push_key(State(state): State<AppState>) -> Response {
    api_json(
        StatusCode::OK,
        serde_json::json!({ "public_key": state.admin.push_public_key() }),
    )
}

async fn notify(State(state): State<AppState>, request: Request<Body>) -> Response {
    let Some(secret) = bearer_secret(&request) else {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "A valid Chime send key is required.",
        );
    };
    let body = match parse_json::<chime_protocol::NotificationRequest>(request).await {
        Ok(body) => body,
        Err(status) => return error_response(status, "The request body is invalid."),
    };
    match state.notify.send(secret, body).await {
        Ok(summary) => api_json(StatusCode::OK, summary),
        Err(error) => error_response(error.status, error.message),
    }
}

async fn setup_start(State(state): State<AppState>, request: Request<Body>) -> Response {
    if !origin_matches(&request, &state.admin) {
        return origin_error();
    }
    let body = match parse_json::<SetupStartRequest>(request).await {
        Ok(body) => body,
        Err(status) => return error_response(status, "The request body is invalid."),
    };
    match state.admin.start_setup(body).await {
        Ok(challenge) => api_json(StatusCode::OK, challenge),
        Err(error) => admin_error(error),
    }
}

async fn setup_finish(State(state): State<AppState>, request: Request<Body>) -> Response {
    if !origin_matches(&request, &state.admin) {
        return origin_error();
    }
    let body = match parse_json::<SetupFinishRequest>(request).await {
        Ok(body) => body,
        Err(status) => return error_response(status, "The request body is invalid."),
    };
    match state.admin.finish_setup(body).await {
        Ok(()) => api_json(StatusCode::OK, serde_json::json!({ "ok": true })),
        Err(error) => admin_error(error),
    }
}

async fn admin_challenge(State(state): State<AppState>, request: Request<Body>) -> Response {
    if !origin_matches(&request, &state.admin) {
        return origin_error();
    }
    let body = match parse_json::<AdminRequest>(request).await {
        Ok(body) => body,
        Err(status) => return error_response(status, "The request body is invalid."),
    };
    match state.admin.start_admin_operation(body).await {
        Ok(challenge) => api_json(StatusCode::OK, challenge),
        Err(error) => admin_error(error),
    }
}

async fn admin_finish(State(state): State<AppState>, request: Request<Body>) -> Response {
    if !origin_matches(&request, &state.admin) {
        return origin_error();
    }
    let body = match parse_json::<AdminFinishRequest>(request).await {
        Ok(body) => body,
        Err(status) => return error_response(status, "The request body is invalid."),
    };
    match state.admin.finish_admin_operation(body).await {
        Ok(response) => api_json(StatusCode::OK, response),
        Err(error) => admin_error(error),
    }
}

async fn add_finish(State(state): State<AppState>, request: Request<Body>) -> Response {
    if !origin_matches(&request, &state.admin) {
        return origin_error();
    }
    let body = match parse_json::<AddFinishRequest>(request).await {
        Ok(body) => body,
        Err(status) => return error_response(status, "The request body is invalid."),
    };
    match state.admin.finish_add_passkey(body).await {
        Ok(passkeys) => api_json(StatusCode::OK, AdminResponse::Passkeys { passkeys }),
        Err(error) => admin_error(error),
    }
}

fn bearer_secret(request: &Request<Body>) -> Option<String> {
    let value = request
        .headers()
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = value.strip_prefix("Bearer ")?;
    let encoded = token.strip_prefix("chime_sk_")?;
    let decoded = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    (decoded.len() == 32 && URL_SAFE_NO_PAD.encode(&decoded) == encoded).then(|| token.to_owned())
}

async fn parse_json<T>(request: Request<Body>) -> Result<T, StatusCode>
where
    T: serde::de::DeserializeOwned,
{
    let (parts, body) = request.into_parts();
    let request = Request::from_parts(parts, body);
    match axum::Json::<T>::from_request(request, &()).await {
        Ok(axum::Json(value)) => Ok(value),
        Err(rejection) => Err(rejection.status()),
    }
}

fn origin_matches(request: &Request<Body>, state: &AdminState) -> bool {
    request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| origin == state.expected_origin())
}

fn origin_error() -> Response {
    error_response(
        StatusCode::FORBIDDEN,
        "This request must come from the configured Chime origin.",
    )
}

fn admin_error(error: AdminError) -> Response {
    error_response(error.status, error.message)
}

fn asset_response(path: &str) -> Response {
    let Some(file) = WEB_ASSETS.get_file(path) else {
        return error_response(StatusCode::NOT_FOUND, "Not found.");
    };

    let content_type = mime_guess::from_path(path)
        .first_or_octet_stream()
        .essence_str()
        .to_owned();
    let content_type = if content_type.starts_with("text/") {
        format!("{content_type}; charset=utf-8")
    } else {
        content_type
    };
    let mut response = Response::new(Body::from(file.contents().to_vec()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type).expect("MIME type is a valid header value"),
    );
    response
}

fn api_json<T: Serialize>(status: StatusCode, value: T) -> Response {
    match serde_json::to_vec(&value) {
        Ok(body) => {
            let mut response = Response::new(Body::from(body));
            *response.status_mut() = status;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json; charset=utf-8"),
            );
            response
        }
        Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Chime could not encode the response.",
        ),
    }
}

fn error_response(status: StatusCode, message: &'static str) -> Response {
    let mut response = Response::new(Body::from(
        serde_json::json!({ "error": message }).to_string(),
    ));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

async fn security_headers(request: Request<Body>, next: Next) -> Response {
    secure_response(next.run(request).await)
}

fn secure_response(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::{AdminState, CONTENT_SECURITY_POLICY, MAX_JSON_BODY_BYTES, StatusCode, router};
    use crate::notify::NotifyState;
    use crate::push::WebPushTransport;
    use crate::{config::Origin, store::Store};
    use axum::{body::Body, http::Request};
    use std::str::FromStr;
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn app() -> (axum::Router, tempfile::TempDir, String) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("state")).unwrap();
        let code = store.initialize_setup_code().await.unwrap().unwrap();
        let origin = Origin::from_str("https://chime.example-tailnet.ts.net").unwrap();
        (
            router(
                AdminState::new(store.clone(), &origin).unwrap(),
                NotifyState::new(store, Arc::new(WebPushTransport::new().unwrap())),
            ),
            directory,
            code,
        )
    }

    #[tokio::test]
    async fn embedded_assets_have_types_and_security_headers() {
        let (app, _directory, _code) = app().await;
        let response = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-type"],
            "text/html; charset=utf-8"
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.headers()["x-frame-options"], "DENY");
        assert_eq!(
            response.headers()["content-security-policy"],
            CONTENT_SECURITY_POLICY
        );
        assert!(CONTENT_SECURITY_POLICY.contains("manifest-src 'self'"));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/styles.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers()["content-type"],
            "text/css; charset=utf-8"
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
    }

    #[tokio::test]
    async fn unknown_assets_and_api_responses_are_not_cached() {
        let (app, _directory, code) = app().await;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/missing.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()["cache-control"], "no-store");

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/setup/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let body = axum::body::to_bytes(response.into_body(), MAX_JSON_BODY_BYTES)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains(&code));
    }

    #[tokio::test]
    async fn admin_posts_require_the_exact_configured_origin() {
        let (app, _directory, _code) = app().await;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/admin/challenge")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"kind":"list_passkeys"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response.headers()["cache-control"], "no-store");

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/admin/challenge")
                    .header("origin", "https://other.example")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"kind":"list_passkeys"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
