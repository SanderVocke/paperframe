use std::{
    io::ErrorKind,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    body::Body,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, Response},
    routing::get,
};
use chrono::{SecondsFormat, Utc};
use tokio::io::AsyncReadExt;
use tower_http::trace::{DefaultMakeSpan, DefaultOnFailure, DefaultOnResponse, TraceLayer};
use tracing::Level;
use tracing::{error, info};

use crate::state::{AppState, ImageRequest};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/settings", get(settings))
        .route("/frame/current.jpg", get(get_image).head(head_image))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO))
                .on_failure(DefaultOnFailure::new().level(Level::ERROR)),
        )
        .with_state(state)
}

async fn settings(State(state): State<Arc<AppState>>) -> Html<String> {
    let settings_generation = state.settings.snapshot().await.generation;
    let request = state.last_image_request().await;
    let status = match request {
        Some(request) => format!(
            "<dl><dt>Last image request</dt><dd>{}</dd><dt>Client IP</dt><dd>{}</dd><dt>Response</dt><dd>{} {}</dd></dl>",
            request
                .requested_at
                .to_rfc3339_opts(SecondsFormat::Secs, true),
            request.client_ip,
            request.response_status.as_u16(),
            request
                .response_status
                .canonical_reason()
                .unwrap_or("Unknown")
        ),
        None => "<p>No image request has occurred since the server started.</p>".to_owned(),
    };

    Html(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Paper frame settings</title></head><body><main><h1>Paper frame server</h1><p>Hello, world!</p><p>Settings generation: {settings_generation}</p><h2>Status</h2>{status}</main></body></html>"
    ))
}

async fn get_image(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let response = image_response(&state, &headers, true).await;
    let status = response.status();
    state
        .record_image_request(ImageRequest {
            requested_at: Utc::now(),
            client_ip: peer.ip(),
            response_status: status,
        })
        .await;
    info!(client_ip = %peer.ip(), %status, "image requested");
    response
}

async fn head_image(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    image_response(&state, &headers, false).await
}

async fn image_response(state: &AppState, headers: &HeaderMap, include_body: bool) -> Response {
    let mut file = match tokio::fs::File::open(state.config.current_image_path()).await {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return error_response(StatusCode::NOT_FOUND, include_body, "image not found\n");
        }
        Err(error) => {
            error!(error = %error, "failed to open configured image");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                include_body,
                "failed to read image\n",
            );
        }
    };

    let metadata = match file.metadata().await {
        Ok(metadata) => metadata,
        Err(error) => {
            error!(error = %error, "failed to read configured image metadata");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                include_body,
                "failed to read image\n",
            );
        }
    };
    let modified = match metadata.modified() {
        Ok(modified) => modified,
        Err(error) => {
            error!(error = %error, "configured image has no modification time");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                include_body,
                "failed to read image\n",
            );
        }
    };
    let last_modified = httpdate::fmt_http_date(modified);

    if is_not_modified(headers, modified) {
        return build_image_response(
            StatusCode::NOT_MODIFIED,
            Body::empty(),
            None,
            &last_modified,
        );
    }

    if !include_body {
        return build_image_response(
            StatusCode::OK,
            Body::empty(),
            Some(metadata.len()),
            &last_modified,
        );
    }

    let mut bytes = Vec::new();
    if let Err(error) = file.read_to_end(&mut bytes).await {
        error!(error = %error, "failed to read configured image");
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            include_body,
            "failed to read image\n",
        );
    }

    let content_length = bytes.len() as u64;
    build_image_response(
        StatusCode::OK,
        Body::from(bytes),
        Some(content_length),
        &last_modified,
    )
}

fn is_not_modified(headers: &HeaderMap, modified: SystemTime) -> bool {
    let Some(value) = headers.get(header::IF_MODIFIED_SINCE) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Ok(since) = httpdate::parse_http_date(value) else {
        return false;
    };
    let Ok(since_epoch) = modified.duration_since(UNIX_EPOCH) else {
        return false;
    };
    let modified_at_http_precision = UNIX_EPOCH + Duration::from_secs(since_epoch.as_secs());
    modified_at_http_precision <= since
}

fn build_image_response(
    status: StatusCode,
    body: Body,
    content_length: Option<u64>,
    last_modified: &str,
) -> Response {
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::LAST_MODIFIED, last_modified);
    if let Some(content_length) = content_length {
        builder = builder.header(header::CONTENT_LENGTH, content_length);
    }
    builder.body(body).unwrap_or_else(|error| {
        error!(error = %error, "failed to build image response");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            true,
            "failed to build response\n",
        )
    })
}

fn error_response(status: StatusCode, include_body: bool, message: &'static str) -> Response {
    let body = if include_body {
        Body::from(message)
    } else {
        Body::empty()
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(body)
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        net::{IpAddr, Ipv4Addr},
        path::{Path, PathBuf},
    };

    use axum::{
        body::Body,
        http::{Request, StatusCode, header},
    };
    use chrono::{TimeZone, Utc};
    use http_body_util::BodyExt;
    use tempfile::tempdir;
    use tower::ServiceExt;

    use crate::{
        config::{
            Config, PathMapping, SelectionMode, SelectionSettings, Settings, SettingsManager,
        },
        state::{AppState, ImageRequest},
    };

    use super::*;

    const JPEG_BYTES: &[u8] = b"\xff\xd8test-jpeg\xff\xd9";

    fn test_config(image_path: impl Into<PathBuf>) -> Config {
        let image_path = image_path.into();
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            settings_path: PathBuf::from("unused-settings.toml"),
            cache_dir: image_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
        }
    }

    fn test_settings() -> Settings {
        Settings {
            database_path: PathBuf::from("unused.db"),
            query: "SELECT 1 AS image_id, '/photos' AS root_path, '/' AS album_path, 'one.jpg' AS file_name".into(),
            path_mappings: vec![PathMapping {
                database_root: PathBuf::from("/photos"),
                mounted_root: PathBuf::from("/photos"),
            }],
            selection: SelectionSettings {
                mode: SelectionMode::First,
                random_seed: String::new(),
                timezone: "UTC".into(),
                rollover_time: "00:00".into(),
            },
            max_candidates: 100,
        }
    }

    fn test_state(image_path: impl Into<PathBuf>) -> Arc<AppState> {
        AppState::new(
            test_config(image_path),
            SettingsManager::in_memory(test_settings()),
        )
    }

    fn image_request(method: &str, peer: [u8; 4]) -> axum::http::request::Builder {
        Request::builder()
            .method(method)
            .uri("/frame/current.jpg")
            .extension(ConnectInfo(SocketAddr::from((peer, 43210))))
    }

    async fn body_bytes(response: Response) -> Vec<u8> {
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec()
    }

    async fn body_text(response: Response) -> String {
        String::from_utf8(body_bytes(response).await).unwrap()
    }

    fn write_image(path: &Path) {
        fs::write(path, JPEG_BYTES).unwrap();
    }

    #[tokio::test]
    async fn settings_reports_no_request_initially() {
        let response = router(test_state("unused.jpg"))
            .oneshot(Request::get("/settings").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        let body = body_text(response).await;
        assert!(body.contains("Hello, world!"));
        assert!(body.contains("No image request has occurred"));
    }

    #[tokio::test]
    async fn settings_reports_latest_request() {
        let state = AppState::with_last_image_request(
            test_config("unused.jpg"),
            SettingsManager::in_memory(test_settings()),
            ImageRequest {
                requested_at: Utc.with_ymd_and_hms(2026, 8, 20, 22, 3, 14).unwrap(),
                client_ip: IpAddr::V4(Ipv4Addr::new(192, 168, 20, 47)),
                response_status: StatusCode::NOT_MODIFIED,
            },
        );

        let response = router(state)
            .oneshot(Request::get("/settings").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_text(response).await;

        assert!(body.contains("2026-08-20T22:03:14Z"));
        assert!(body.contains("192.168.20.47"));
        assert!(body.contains("304 Not Modified"));
    }

    #[tokio::test]
    async fn get_returns_unchanged_image_and_records_direct_peer() {
        let directory = tempdir().unwrap();
        let image_path = directory.path().join("current.jpg");
        write_image(&image_path);
        let state = test_state(image_path);

        let response = router(state.clone())
            .oneshot(
                image_request("GET", [192, 168, 1, 42])
                    .header("x-forwarded-for", "203.0.113.9")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            JPEG_BYTES.len().to_string()
        );
        assert!(response.headers().contains_key(header::LAST_MODIFIED));
        assert_eq!(body_bytes(response).await, JPEG_BYTES);
        let recorded = state.last_image_request().await.unwrap();
        assert_eq!(
            recorded.client_ip,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42))
        );
        assert_eq!(recorded.response_status, StatusCode::OK);
    }

    #[tokio::test]
    async fn conditional_get_returns_304_and_updates_state() {
        let directory = tempdir().unwrap();
        let image_path = directory.path().join("current.jpg");
        write_image(&image_path);
        let state = test_state(image_path);
        let app = router(state.clone());

        let first = app
            .clone()
            .oneshot(
                image_request("GET", [192, 168, 1, 10])
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let last_modified = first.headers()[header::LAST_MODIFIED].clone();

        let response = app
            .oneshot(
                image_request("GET", [192, 168, 1, 11])
                    .header(header::IF_MODIFIED_SINCE, last_modified)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert!(body_bytes(response).await.is_empty());
        let recorded = state.last_image_request().await.unwrap();
        assert_eq!(
            recorded.client_ip,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 11))
        );
        assert_eq!(recorded.response_status, StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn head_returns_headers_without_body_or_state_change() {
        let directory = tempdir().unwrap();
        let image_path = directory.path().join("current.jpg");
        write_image(&image_path);
        let original_request = ImageRequest {
            requested_at: Utc.with_ymd_and_hms(2026, 8, 20, 22, 3, 14).unwrap(),
            client_ip: "192.168.1.8".parse().unwrap(),
            response_status: StatusCode::NOT_MODIFIED,
        };
        let state = AppState::with_last_image_request(
            test_config(image_path),
            SettingsManager::in_memory(test_settings()),
            original_request.clone(),
        );

        let response = router(state.clone())
            .oneshot(
                image_request("HEAD", [192, 168, 1, 99])
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            JPEG_BYTES.len().to_string()
        );
        assert!(response.headers().contains_key(header::LAST_MODIFIED));
        assert!(body_bytes(response).await.is_empty());
        assert_eq!(state.last_image_request().await, Some(original_request));
    }

    #[tokio::test]
    async fn missing_image_returns_404_and_records_request() {
        let directory = tempdir().unwrap();
        let state = test_state(directory.path().join("missing.jpg"));

        let response = router(state.clone())
            .oneshot(
                image_request("GET", [10, 0, 0, 4])
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            state.last_image_request().await.unwrap().response_status,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn unreadable_image_returns_500_and_records_request() {
        let directory = tempdir().unwrap();
        let image_path = directory.path().join("current.jpg");
        fs::create_dir(&image_path).unwrap();
        let state = test_state(image_path);

        let response = router(state.clone())
            .oneshot(
                image_request("GET", [10, 0, 0, 5])
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            state.last_image_request().await.unwrap().response_status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn unrelated_route_is_not_found() {
        let response = router(test_state("unused.jpg"))
            .oneshot(Request::get("/other").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
