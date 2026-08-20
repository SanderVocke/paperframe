use axum::Router;
use tower_http::trace::TraceLayer;

pub fn router() -> Router {
    Router::new().layer(TraceLayer::new_for_http())
}
