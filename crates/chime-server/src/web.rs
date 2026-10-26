use crate::admin::{
    AddFinishRequest, AdminError, AdminFinishRequest, AdminRequest, AdminResponse, AdminState,
    SetupFinishRequest, SetupStartRequest,
};
use crate::images::{ImageError, ImageStore, ImageType, MAX_IMAGE_BYTES};
use crate::notify::NotifyState;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{DefaultBodyLimit, FromRequest, Multipart, Path, State},
    http::{HeaderName, HeaderValue, Request, StatusCode, Uri, header},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use include_dir::{Dir, include_dir};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

static WEB_ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../web");

const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; base-uri 'none'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; manifest-src 'self'; img-src 'self' https:; script-src 'self'; style-src 'self'";
const MAX_JSON_BODY_BYTES: usize = 16 * 1024;
const MAX_MULTIPART_BODY_BYTES: usize = MAX_IMAGE_BYTES + MAX_JSON_BODY_BYTES;
const MAX_TEXT_PART_BYTES: usize = 2048;

#[derive(Clone)]
struct AppState {
    admin: AdminState,
    notify: NotifyState,
    images: ImageStore,
}

pub fn router(admin: AdminState, notifications: NotifyState, images: ImageStore) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/setup/status", get(setup_status))
        .route("/api/push-key", get(push_key))
        .route(
            "/api/notify",
            post(notify)
                .options(notify_options)
                .layer(DefaultBodyLimit::max(MAX_MULTIPART_BODY_BYTES)),
        )
        .route("/api/device/ack", post(device_acknowledge))
        .route("/api/images/{capability}", get(image))
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
            images,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AcknowledgeRequest {
    id: String,
    capability: String,
}

async fn device_acknowledge(State(state): State<AppState>, request: Request<Body>) -> Response {
    let Some(token) = bearer_device_token(&request) else {
        return device_token_error();
    };
    let body = match parse_json::<AcknowledgeRequest>(request).await {
        Ok(body) => body,
        Err(status) => return error_response(status, "The request body is invalid."),
    };
    match state
        .notify
        .acknowledge(token, body.id, body.capability)
        .await
    {
        Ok(()) => {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::NO_CONTENT;
            response
        }
        Err(error) => error_response(error.status, error.message),
    }
}

fn bearer_device_token(request: &Request<Body>) -> Option<String> {
    let value = request
        .headers()
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = value.strip_prefix("Bearer chime_dt_")?;
    Some(format!("chime_dt_{token}"))
}

fn device_token_error() -> Response {
    error_response(
        StatusCode::UNAUTHORIZED,
        "A valid Chime device token is required.",
    )
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
    with_notify_cors(handle_notify(state, request).await)
}

async fn handle_notify(state: AppState, request: Request<Body>) -> Response {
    let Some(secret) = bearer_secret(&request) else {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "A valid Chime send key is required.",
        );
    };
    let authorized = match state.notify.authorize(secret).await {
        Ok(authorized) => authorized,
        Err(error) => return error_response(error.status, error.message),
    };
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let parsed = if media_type(content_type) == Some("application/json") {
        parse_json_limited::<chime_protocol::NotificationRequest>(request, MAX_JSON_BODY_BYTES)
            .await
            .map(|body| (body, None))
    } else if media_type(content_type) == Some("multipart/form-data") {
        parse_multipart(request).await
    } else {
        return error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Use JSON or multipart form data.",
        );
    };
    let (mut body, upload) = match parsed {
        Ok(parsed) => parsed,
        Err(status) => return error_response(status, "The request body is invalid."),
    };
    if upload.is_some() && body.image_url.is_some() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "Use an uploaded image or image_url, not both.",
        );
    }

    let capability = match upload.as_ref() {
        Some(_) => match state.images.capability(state.admin.expected_origin()) {
            Ok(capability) => Some(capability),
            Err(error) => return error_response(error.status, error.message),
        },
        None => None,
    };
    if let Some(capability) = &capability {
        body.image_url = Some(capability.url.clone());
    }
    let plan = match state.notify.prepare(authorized, body) {
        Ok(plan) => plan,
        Err(error) => return error_response(error.status, error.message),
    };

    if let (Some((image_type, bytes)), Some(capability)) = (upload, capability)
        && matches!(&plan, crate::notify::SendPlan::Ready(_))
        && let Err(error) = state.images.save(&capability, image_type, bytes).await
    {
        return image_error(error);
    }
    match state.notify.dispatch(plan).await {
        Ok(summary) => api_json(StatusCode::OK, summary),
        Err(error) => error_response(error.status, error.message),
    }
}

async fn notify_options() -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    with_notify_cors(response)
}

async fn image(Path(capability): Path<String>, State(state): State<AppState>) -> Response {
    match state.images.load(capability).await {
        Ok(Some((bytes, content_type))) => {
            let mut response = Response::new(Body::from(bytes));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
            response
        }
        Ok(None) => error_response(StatusCode::NOT_FOUND, "Not found."),
        Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Chime could not load this image.",
        ),
    }
}

fn image_error(error: ImageError) -> Response {
    error_response(error.status, error.message)
}

fn with_notify_cors(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static("access-control-allow-origin"),
        HeaderValue::from_static("*"),
    );
    headers.insert(
        HeaderName::from_static("access-control-allow-methods"),
        HeaderValue::from_static("POST, OPTIONS"),
    );
    headers.insert(
        HeaderName::from_static("access-control-allow-headers"),
        HeaderValue::from_static("Authorization, Content-Type"),
    );
    response
}

fn media_type(value: &str) -> Option<&str> {
    value.split(';').next().map(str::trim)
}

struct UploadedImage {
    image_type: ImageType,
    bytes: Vec<u8>,
}

async fn parse_multipart(
    request: Request<Body>,
) -> Result<
    (
        chime_protocol::NotificationRequest,
        Option<(ImageType, Vec<u8>)>,
    ),
    StatusCode,
> {
    let mut multipart = Multipart::from_request(request, &())
        .await
        .map_err(|rejection| rejection.status())?;
    let mut seen = HashSet::new();
    let mut title = None;
    let mut body = None;
    let mut image_url = None;
    let mut url = None;
    let mut upload = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?
    {
        let name = field.name().ok_or(StatusCode::BAD_REQUEST)?.to_owned();
        if !seen.insert(name.clone()) {
            return Err(StatusCode::BAD_REQUEST);
        }
        let filename = field.file_name().map(str::to_owned);
        if name == "image" {
            let filename = filename.ok_or(StatusCode::BAD_REQUEST)?;
            if filename.is_empty()
                || filename == "."
                || filename == ".."
                || filename.contains('/')
                || filename.contains('\\')
            {
                return Err(StatusCode::BAD_REQUEST);
            }
            let content_type = field.content_type().ok_or(StatusCode::BAD_REQUEST)?;
            let image_type = ImageType::parse(content_type.split(';').next().unwrap_or_default())
                .ok_or(StatusCode::BAD_REQUEST)?;
            let mut bytes = Vec::new();
            let mut field = field;
            while let Some(chunk) = field.chunk().await.map_err(|_| StatusCode::BAD_REQUEST)? {
                if bytes.len().saturating_add(chunk.len()) > MAX_IMAGE_BYTES {
                    return Err(StatusCode::PAYLOAD_TOO_LARGE);
                }
                bytes.extend_from_slice(&chunk);
            }
            if !crate::images::validate_upload(&bytes, image_type) {
                return Err(StatusCode::BAD_REQUEST);
            }
            upload = Some(UploadedImage { image_type, bytes });
            continue;
        }
        if filename.is_some() {
            return Err(StatusCode::BAD_REQUEST);
        }
        if !matches!(name.as_str(), "title" | "body" | "image_url" | "url") {
            return Err(StatusCode::BAD_REQUEST);
        }
        let mut bytes = Vec::new();
        let mut field = field;
        while let Some(chunk) = field.chunk().await.map_err(|_| StatusCode::BAD_REQUEST)? {
            if bytes.len().saturating_add(chunk.len()) > MAX_TEXT_PART_BYTES {
                return Err(StatusCode::PAYLOAD_TOO_LARGE);
            }
            bytes.extend_from_slice(&chunk);
        }
        let value = String::from_utf8(bytes).map_err(|_| StatusCode::BAD_REQUEST)?;
        match name.as_str() {
            "title" => title = Some(value),
            "body" => body = Some(value),
            "image_url" => image_url = Some(value),
            "url" => url = Some(value),
            _ => return Err(StatusCode::BAD_REQUEST),
        }
    }
    let title = title.ok_or(StatusCode::BAD_REQUEST)?;
    let request = chime_protocol::NotificationRequest {
        title,
        body,
        image_url,
        url,
    };
    let upload = upload.map(|image| (image.image_type, image.bytes));
    Ok((request, upload))
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

async fn parse_json_limited<T>(request: Request<Body>, limit: usize) -> Result<T, StatusCode>
where
    T: serde::de::DeserializeOwned,
{
    let (_, body) = request.into_parts();
    let bytes = to_bytes(body, limit)
        .await
        .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;
    serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST)
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
    let public_image = request.uri().path().starts_with("/api/images/");
    secure_response(next.run(request).await, public_image)
}

fn secure_response(mut response: Response, public_image: bool) -> Response {
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
        HeaderValue::from_static(if public_image {
            "cross-origin"
        } else {
            "same-origin"
        }),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::{
        AdminState, CONTENT_SECURITY_POLICY, MAX_IMAGE_BYTES, MAX_JSON_BODY_BYTES, StatusCode,
        router,
    };
    use crate::images::ImageStore;
    use crate::notify::NotifyState;
    use crate::push::WebPushTransport;
    use crate::{config::Origin, store::Store};
    use axum::{body::Body, http::Request};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha2::{Digest, Sha256};
    use std::str::FromStr;
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn app() -> (
        axum::Router,
        tempfile::TempDir,
        String,
        String,
        ImageStore,
        Store,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("state")).unwrap();
        let code = store.initialize_setup_code().await.unwrap().unwrap();
        let secret = format!("chime_sk_{}", URL_SAFE_NO_PAD.encode([9_u8; 32]));
        let digest = Sha256::digest(secret.as_bytes()).to_vec();
        store
            .database()
            .run(move |connection| {
                connection.execute(
                    "INSERT INTO send_keys (id, name, secret_digest, created_at)
                     VALUES ('AAAAAAAAAAAAAAAAAAAAAA', 'browser sender', ?1, unixepoch())",
                    rusqlite::params![digest],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let origin = Origin::from_str("https://chime.example-tailnet.ts.net").unwrap();
        let images = ImageStore::new(store.clone()).unwrap();
        (
            router(
                AdminState::new(store.clone(), &origin).unwrap(),
                NotifyState::new(store.clone(), Arc::new(WebPushTransport::new().unwrap())),
                images.clone(),
            ),
            directory,
            code,
            secret,
            images,
            store,
        )
    }

    async fn acknowledgement_app() -> (axum::Router, tempfile::TempDir, Store, NotifyState) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("state")).unwrap();
        let token = format!("chime_dt_{}", URL_SAFE_NO_PAD.encode([6_u8; 32]));
        let digest = Sha256::digest(token.as_bytes()).to_vec();
        store
            .database()
            .run(move |connection| {
                connection.execute(
                    "INSERT INTO receiving_devices
                     (id, name, endpoint, p256dh, auth, token_digest,
                      created_at, updated_at, revision)
                     VALUES ('BBBBBBBBBBBBBBBBBBBBBB', 'test device',
                       'https://fcm.googleapis.com/test/device', '', '', ?1,
                       unixepoch(), unixepoch(), 1)",
                    rusqlite::params![digest],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let origin = Origin::from_str("https://chime.example-tailnet.ts.net").unwrap();
        let images = ImageStore::new(store.clone()).unwrap();
        let notify = NotifyState::new(store.clone(), Arc::new(WebPushTransport::new().unwrap()));
        let app = router(
            AdminState::new(store.clone(), &origin).unwrap(),
            notify.clone(),
            images,
        );
        (app, directory, store, notify)
    }

    #[tokio::test]
    async fn embedded_assets_have_types_and_security_headers() {
        let (app, _directory, _code, _secret, _images, _store) = app().await;
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
        assert!(CONTENT_SECURITY_POLICY.contains("img-src 'self' https:"));

        let response = app
            .clone()
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
        let (app, _directory, code, _secret, _images, _store) = app().await;
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
        let (app, _directory, _code, _secret, _images, _store) = app().await;
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
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-origin")
        );
    }

    #[tokio::test]
    async fn device_ack_requires_token_and_capability_and_stream_routes_are_absent() {
        let (app, _directory, store, notify) = acknowledgement_app().await;
        let token = format!("chime_dt_{}", URL_SAFE_NO_PAD.encode([6_u8; 32]));
        let id = URL_SAFE_NO_PAD.encode([7_u8; 16]);
        let capability = URL_SAFE_NO_PAD.encode([8_u8; 32]);
        let expires_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 3600;
        notify
            .remember_acknowledgement(
                id.clone(),
                &capability,
                expires_at,
                vec!["BBBBBBBBBBBBBBBBBBBBBB".to_owned()],
            )
            .await;

        let old_stream = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/device/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(old_stream.status(), StatusCode::NOT_FOUND);
        let old_reconciliation = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/device/reconcile")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(old_reconciliation.status(), StatusCode::METHOD_NOT_ALLOWED);

        let wrong_token = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/device/ack")
                    .header("authorization", "Bearer chime_dt_wrong")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"id":"{id}","capability":"{capability}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_token.status(), StatusCode::UNAUTHORIZED);

        let wrong_capability = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/device/ack")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"id":"{id}","capability":"{}"}}"#,
                        URL_SAFE_NO_PAD.encode([9_u8; 32])
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_capability.status(), StatusCode::NOT_FOUND);

        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/device/ack")
                        .header("authorization", format!("Bearer {token}"))
                        .header("content-type", "application/json")
                        .body(Body::from(format!(
                            r#"{{"id":"{id}","capability":"{capability}"}}"#
                        )))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }

        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/device/ack")
                        .header("authorization", format!("Bearer {token}"))
                        .header("content-type", "application/json")
                        .body(Body::from(format!(
                            r#"{{"id":"{id}","capability":"{capability}"}}"#
                        )))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }

        store
            .database()
            .run(|connection| {
                connection.execute(
                    "DELETE FROM receiving_devices WHERE id = 'BBBBBBBBBBBBBBBBBBBBBB'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let revoked = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/device/ack")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"id":"{id}","capability":"{capability}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn notify_cors_covers_preflight_and_authorized_and_unauthorized_responses_only() {
        let (app, _directory, _code, secret, _images, _store) = app().await;
        let preflight = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/api/notify")
                    .header("origin", "https://sender.example")
                    .header("access-control-request-method", "POST")
                    .header(
                        "access-control-request-headers",
                        "authorization,content-type",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
        assert_eq!(preflight.headers()["access-control-allow-origin"], "*");
        assert_eq!(
            preflight.headers()["access-control-allow-methods"],
            "POST, OPTIONS"
        );
        assert_eq!(
            preflight.headers()["access-control-allow-headers"],
            "Authorization, Content-Type"
        );
        assert!(
            !preflight
                .headers()
                .contains_key("access-control-allow-credentials")
        );

        for (authorization, expected_status) in [
            ("Bearer invalid".to_owned(), StatusCode::UNAUTHORIZED),
            (format!("Bearer {secret}"), StatusCode::OK),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/notify")
                        .header("origin", "https://sender.example")
                        .header("authorization", authorization)
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"title":"Browser send"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected_status);
            assert_eq!(response.headers()["access-control-allow-origin"], "*");
            assert!(
                !response
                    .headers()
                    .contains_key("access-control-allow-credentials")
            );
        }

        let admin = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/setup/status")
                    .header("origin", "https://sender.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(admin.status(), StatusCode::OK);
        assert!(!admin.headers().contains_key("access-control-allow-origin"));
    }

    #[tokio::test]
    async fn multipart_rejects_bad_fields_images_and_filenames_before_storage() {
        let (app, directory, _code, secret, _images, store) = app().await;
        let boundary = "chime-test-boundary";
        let png = b"\x89PNG\r\n\x1a\nsmall png";
        let bad_requests = [
            multipart_body(boundary, &[("title", b"First"), ("title", b"Second")], None),
            multipart_body(boundary, &[("title", b"Known"), ("unknown", b"x")], None),
            multipart_body(
                boundary,
                &[("title", b"Traversal")],
                Some(("../../escape.png", "image/png", png)),
            ),
            multipart_body(
                boundary,
                &[("title", b"Spoof")],
                Some(("image.png", "image/jpeg", png)),
            ),
            multipart_body(
                boundary,
                &[("title", b"Unsupported")],
                Some(("image.svg", "image/svg+xml", b"<svg/>")),
            ),
            multipart_body(
                boundary,
                &[
                    ("title", b"Conflict"),
                    ("image_url", b"https://images.example/a.png"),
                ],
                Some(("image.png", "image/png", png)),
            ),
        ];
        for body in bad_requests {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/notify")
                        .header("authorization", format!("Bearer {secret}"))
                        .header(
                            "content-type",
                            format!("multipart/form-data; boundary={boundary}"),
                        )
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(response.headers()["access-control-allow-origin"], "*");
        }

        let mut oversized_image = vec![b'x'; MAX_IMAGE_BYTES + 1];
        oversized_image[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        let overflow = multipart_body(
            boundary,
            &[("title", b"Overflow")],
            Some(("large.png", "image/png", &oversized_image)),
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/notify")
                    .header("authorization", format!("Bearer {secret}"))
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(overflow))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let unauthorized_upload = multipart_body(
            boundary,
            &[("title", b"Unauthorized")],
            Some(("image.png", "image/png", png)),
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/notify")
                    .header("authorization", "Bearer invalid")
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(unauthorized_upload))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()["access-control-allow-origin"], "*");

        let upload_body = multipart_body(
            boundary,
            &[("title", b"Upload"), ("url", b"https://example.com/result")],
            Some(("render.png", "image/png", png)),
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/notify")
                    .header("authorization", format!("Bearer {secret}"))
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(upload_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["access-control-allow-origin"], "*");
        let image_count = store
            .database()
            .run(|connection| {
                connection.query_row("SELECT COUNT(*) FROM uploaded_images", [], |row| {
                    row.get::<_, i64>(0)
                })
            })
            .await
            .unwrap();
        assert_eq!(image_count, 0);
        assert_eq!(
            std::fs::read_dir(directory.path().join("state/images"))
                .unwrap()
                .count(),
            0
        );

        for (body, status) in [
            (
                r#"{"title":"A","title":"B"}"#.to_owned(),
                StatusCode::BAD_REQUEST,
            ),
            (
                r#"{"title":"A","other":true}"#.to_owned(),
                StatusCode::BAD_REQUEST,
            ),
            (
                format!(
                    "{{\"title\":\"Hello\",\"image_url\":\"https://images.example/{}\",\"url\":\"https://example.com/{}\"}}",
                    "a".repeat(1800),
                    "b".repeat(1000)
                ),
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/notify")
                        .header("authorization", format!("Bearer {secret}"))
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status);
        }
        let oversized_json = format!("{{\"title\":\"{}\"}}", "x".repeat(MAX_JSON_BODY_BYTES));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/notify")
                    .header("authorization", format!("Bearer {secret}"))
                    .header("content-type", "application/json")
                    .body(Body::from(oversized_json))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn image_capability_has_fixed_headers_and_expired_urls_return_not_found() {
        let (app, _directory, _code, _secret, images, store) = app().await;
        let capability = images
            .capability("https://chime.example-tailnet.ts.net")
            .unwrap();
        images
            .save(
                &capability,
                crate::images::ImageType::Png,
                b"\x89PNG\r\n\x1a\nimage fixture".to_vec(),
            )
            .await
            .unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/images/{}", capability.token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "image/png");
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(
            response.headers()["cross-origin-resource-policy"],
            "cross-origin"
        );
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-origin")
        );

        let token = capability.token.clone();
        store
            .database()
            .run(move |connection| {
                connection.execute(
                    "UPDATE uploaded_images SET expires_at = unixepoch() - 1 WHERE capability = ?1",
                    [token],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/images/{}", capability.token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    fn multipart_body(
        boundary: &str,
        fields: &[(&str, &[u8])],
        image: Option<(&str, &str, &[u8])>,
    ) -> Vec<u8> {
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n")
                    .as_bytes(),
            );
            body.extend_from_slice(value);
            body.extend_from_slice(b"\r\n");
        }
        if let Some((filename, content_type, bytes)) = image {
            body.extend_from_slice(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n").as_bytes(),
            );
            body.extend_from_slice(bytes);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        body
    }
}
