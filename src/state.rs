use std::{net::IpAddr, sync::Arc};

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use tokio::sync::RwLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRequest {
    pub requested_at: DateTime<Utc>,
    pub client_ip: IpAddr,
    pub response_status: StatusCode,
}

pub struct AppState {
    last_image_request: RwLock<Option<ImageRequest>>,
}

impl AppState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            last_image_request: RwLock::new(None),
        })
    }

    #[cfg(test)]
    pub fn with_last_image_request(request: ImageRequest) -> Arc<Self> {
        Arc::new(Self {
            last_image_request: RwLock::new(Some(request)),
        })
    }

    pub async fn last_image_request(&self) -> Option<ImageRequest> {
        self.last_image_request.read().await.clone()
    }
}
