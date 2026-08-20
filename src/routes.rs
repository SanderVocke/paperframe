use std::sync::Arc;

use axum::{Router, response::Html, routing::get};
use chrono::SecondsFormat;
use tower_http::trace::TraceLayer;

use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/settings", get(settings))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn settings(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Html<String> {
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
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Paper frame settings</title></head><body><main><h1>Paper frame server</h1><p>Hello, world!</p><h2>Status</h2>{status}</main></body></html>"
    ))
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use chrono::{TimeZone, Utc};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::state::{AppState, ImageRequest};

    use super::*;

    fn test_state() -> Arc<AppState> {
        AppState::new()
    }

    async fn body_text(response: axum::response::Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn settings_reports_no_request_initially() {
        let response = router(test_state())
            .oneshot(Request::get("/settings").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        let body = body_text(response).await;
        assert!(body.contains("Hello, world!"));
        assert!(body.contains("No image request has occurred"));
    }

    #[tokio::test]
    async fn settings_reports_latest_request() {
        let state = AppState::with_last_image_request(ImageRequest {
            requested_at: Utc.with_ymd_and_hms(2026, 8, 20, 22, 3, 14).unwrap(),
            client_ip: IpAddr::V4(Ipv4Addr::new(192, 168, 20, 47)),
            response_status: StatusCode::NOT_MODIFIED,
        });

        let response = router(state)
            .oneshot(Request::get("/settings").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_text(response).await;

        assert!(body.contains("2026-08-20T22:03:14Z"));
        assert!(body.contains("192.168.20.47"));
        assert!(body.contains("304 Not Modified"));
    }
}
