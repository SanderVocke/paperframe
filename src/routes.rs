use std::{
    future::Future,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use askama::Template;
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{ConnectInfo, DefaultBodyLimit, Form, Multipart, Path as AxumPath, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::Utc;
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::OwnedMutexGuard;
use tower_http::trace::{DefaultMakeSpan, DefaultOnFailure, DefaultOnResponse, TraceLayer};
use tracing::{Level, error, info};

use crate::{
    config::{
        AdvancementSettings, DigikamSettings, ManualFolderSettings, PathMapping, QueueSettings,
        ReplaceSettingsError, Settings, SupplierKind, SupplierSettings,
    },
    queue::{PreparedImage, QueueEntry, QueueEventReason},
    state::{AppState, ImageRequest},
};

const SETTINGS_LIMIT: usize = 64 * 1024;
const UPLOAD_LIMIT: usize = 101 * 1024 * 1024;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(|| async { Redirect::to("/settings") }))
        .route("/settings", get(settings_page).post(update_settings))
        .route("/queue/reorder", post(reorder_queue))
        .route("/queue/remove", post(remove_queue_entry))
        .route("/queue/advance", post(advance_queue))
        .route(
            "/queue/upload",
            post(upload_queue).layer(DefaultBodyLimit::max(UPLOAD_LIMIT)),
        )
        .route("/queue/thumbnail/{id}", get(queue_thumbnail))
        .route("/queue/preview/{id}", get(queue_preview))
        .route("/supplier/refresh", post(refresh_supplier))
        .route("/supplier/reset-seen", post(reset_supplier_seen))
        .route("/frame/current.jpg", get(get_image).head(head_image))
        .route("/health/live", get(|| async { StatusCode::OK }))
        .route("/health/ready", get(health_ready))
        .layer(DefaultBodyLimit::max(SETTINGS_LIMIT))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO))
                .on_failure(DefaultOnFailure::new().level(Level::ERROR)),
        )
        .with_state(state)
}

#[derive(Clone)]
struct QueueView {
    id: String,
    display_name: String,
    provenance: String,
    is_current: bool,
    order_line: String,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    generation: u64,
    queue_generation: u64,
    csrf_token: String,
    entries: Vec<QueueView>,
    reorder_lines: String,
    ready: bool,
    last_request: String,
    refill_status: String,
    refill_detail: Option<String>,
    supplier_status: String,
    target_length: String,
    max_upload_bytes: String,
    policy_every_fetch: bool,
    policy_scheduled: bool,
    policy_manual: bool,
    frame_client_ips: String,
    schedule_timezone: String,
    schedule_times: String,
    supplier_digikam: bool,
    supplier_manual: bool,
    supplier_id: String,
    database_path: String,
    query: String,
    path_mappings: String,
    max_candidates: String,
    manual_directory: String,
    poll_interval_seconds: String,
    error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SettingsForm {
    generation: String,
    csrf_token: String,
    target_length: String,
    max_upload_bytes: String,
    advancement_type: String,
    frame_client_ips: String,
    schedule_timezone: String,
    schedule_times: String,
    supplier_type: String,
    supplier_id: String,
    database_path: String,
    query: String,
    path_mappings: String,
    max_candidates: String,
    manual_directory: String,
    poll_interval_seconds: String,
}

#[derive(Deserialize)]
struct QueueActionForm {
    csrf_token: String,
    queue_generation: String,
}
#[derive(Deserialize)]
struct RemoveForm {
    csrf_token: String,
    queue_generation: String,
    id: String,
}
#[derive(Deserialize)]
struct ReorderForm {
    csrf_token: String,
    queue_generation: String,
    order: String,
}

async fn settings_page(State(state): State<Arc<AppState>>) -> Response {
    let snapshot = state.settings.snapshot().await;
    render_settings(
        &state,
        snapshot.generation,
        SettingsForm::from_settings(snapshot.generation, state.csrf_token(), &snapshot.settings),
        None,
        StatusCode::OK,
    )
    .await
}

async fn update_settings(
    State(state): State<Arc<AppState>>,
    Form(form): Form<SettingsForm>,
) -> Response {
    if form.csrf_token != state.csrf_token() {
        return render_error(&state, form, StatusCode::FORBIDDEN, "Invalid CSRF token").await;
    }
    let expected = match form.generation.parse() {
        Ok(value) => value,
        Err(_) => {
            return render_error(
                &state,
                form,
                StatusCode::UNPROCESSABLE_ENTITY,
                "Settings generation must be an integer",
            )
            .await;
        }
    };
    let proposed = match form.to_settings() {
        Ok(settings) => settings,
        Err(message) => {
            return render_error(&state, form, StatusCode::UNPROCESSABLE_ENTITY, &message).await;
        }
    };
    let registry = match state.prepare_registry(&proposed).await {
        Ok(registry) => registry,
        Err(error) => {
            return render_error(
                &state,
                form,
                StatusCode::UNPROCESSABLE_ENTITY,
                &error.to_string(),
            )
            .await;
        }
    };
    match state.settings.replace(expected, proposed).await {
        Ok(_) => {
            state.activate_registry(registry).await;
            state.queue.demand(QueueEventReason::Settings);
            Redirect::to("/settings").into_response()
        }
        Err(ReplaceSettingsError::Stale { current_generation }) => {
            render_error(
                &state,
                form,
                StatusCode::CONFLICT,
                &format!(
                    "Settings changed concurrently; current generation is {current_generation}"
                ),
            )
            .await
        }
        Err(ReplaceSettingsError::Invalid(message)) => {
            render_error(&state, form, StatusCode::UNPROCESSABLE_ENTITY, &message).await
        }
        Err(ReplaceSettingsError::Persistence(message)) => {
            render_error(&state, form, StatusCode::INTERNAL_SERVER_ERROR, &message).await
        }
    }
}

async fn render_error(
    state: &Arc<AppState>,
    mut form: SettingsForm,
    status: StatusCode,
    message: &str,
) -> Response {
    let generation = state.settings.snapshot().await.generation;
    form.generation = generation.to_string();
    form.csrf_token = state.csrf_token().into();
    render_settings(state, generation, form, Some(message.into()), status).await
}

async fn render_settings(
    state: &Arc<AppState>,
    generation: u64,
    form: SettingsForm,
    error: Option<String>,
    status: StatusCode,
) -> Response {
    let queue = state.queue.snapshot().await;
    let entries = queue
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| queue_view(index, entry))
        .collect::<Vec<_>>();
    let reorder_lines = entries
        .iter()
        .map(|entry| entry.order_line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let last_request = state
        .last_image_request()
        .await
        .map(|request| {
            request
                .requested_at
                .format("%b %-d at %H:%M UTC")
                .to_string()
        })
        .unwrap_or_else(|| "No activity yet".into());
    let refill = state.refill_status().await;
    let (refill_status, refill_detail) = if refill.running {
        ("Finding new photos…".into(), None)
    } else if let Some(error) = refill.last_error {
        ("Needs attention".into(), Some(error))
    } else if refill.last_added == 0 {
        ("Up to date".into(), None)
    } else {
        (format!("Added {} new photo(s)", refill.last_added), None)
    };
    let live = state.settings.snapshot().await.settings;
    let supplier_detail = match &live.supplier().kind {
        SupplierKind::ManualFolder(_) => "Photo folder",
        SupplierKind::DigikamSqlite(_) => "DigiKam",
    };
    let advancement_detail = match &live.advancement {
        AdvancementSettings::EveryFetch { .. } => "Changes when requested".to_owned(),
        AdvancementSettings::Manual => "Manual controls".to_owned(),
        AdvancementSettings::Scheduled { timezone, times } => {
            match crate::scheduler::next_scheduled(timezone, times, Utc::now()) {
                Ok(next) => format!("Next at {}", next.format("%H:%M")),
                Err(_) => "Schedule needs attention".to_owned(),
            }
        }
    };
    let supplier_status = format!("{supplier_detail} · {advancement_detail}");
    let template = SettingsTemplate {
        generation,
        queue_generation: queue.generation,
        csrf_token: state.csrf_token().into(),
        entries,
        reorder_lines,
        ready: !queue.entries.is_empty(),
        last_request,
        refill_status,
        refill_detail,
        supplier_status,
        target_length: form.target_length,
        max_upload_bytes: form.max_upload_bytes,
        policy_every_fetch: form.advancement_type == "every_fetch",
        policy_scheduled: form.advancement_type == "scheduled",
        policy_manual: form.advancement_type == "manual",
        frame_client_ips: form.frame_client_ips,
        schedule_timezone: form.schedule_timezone,
        schedule_times: form.schedule_times,
        supplier_digikam: form.supplier_type == "digikam_sqlite",
        supplier_manual: form.supplier_type == "manual_folder",
        supplier_id: form.supplier_id,
        database_path: form.database_path,
        query: form.query,
        path_mappings: form.path_mappings,
        max_candidates: form.max_candidates,
        manual_directory: form.manual_directory,
        poll_interval_seconds: form.poll_interval_seconds,
        error,
    };
    match template.render() {
        Ok(html) => (status, Html(html)).into_response(),
        Err(error) => {
            error!(%error, "failed to render settings");
            plain(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to render settings",
            )
        }
    }
}

fn queue_view(index: usize, entry: &QueueEntry) -> QueueView {
    let provenance = if entry.supplier_id.is_some() {
        "photo library".into()
    } else {
        "your uploads".into()
    };
    QueueView {
        id: entry.id.clone(),
        display_name: entry.display_name.clone(),
        provenance,
        is_current: index == 0,
        order_line: format!("{}\t{}", entry.id, entry.display_name),
    }
}

async fn reorder_queue(
    State(state): State<Arc<AppState>>,
    Form(form): Form<ReorderForm>,
) -> Response {
    let generation = match validate_action(&state, &form.csrf_token, &form.queue_generation) {
        Ok(value) => value,
        Err((status, message)) => return plain(status, &message),
    };
    let ids = form
        .order
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect();
    mutation_response(state.queue.reorder(generation, ids).await)
}
async fn remove_queue_entry(
    State(state): State<Arc<AppState>>,
    Form(form): Form<RemoveForm>,
) -> Response {
    let generation = match validate_action(&state, &form.csrf_token, &form.queue_generation) {
        Ok(value) => value,
        Err((status, message)) => return plain(status, &message),
    };
    mutation_response(state.queue.remove(generation, &form.id).await)
}
async fn advance_queue(
    State(state): State<Arc<AppState>>,
    Form(form): Form<QueueActionForm>,
) -> Response {
    let generation = match validate_action(&state, &form.csrf_token, &form.queue_generation) {
        Ok(value) => value,
        Err((status, message)) => return plain(status, &message),
    };
    match state.queue.advance_at_generation(generation).await {
        Ok(_) => Redirect::to("/settings").into_response(),
        Err(error) => plain(StatusCode::CONFLICT, &error.to_string()),
    }
}
async fn refresh_supplier(
    State(state): State<Arc<AppState>>,
    Form(form): Form<QueueActionForm>,
) -> Response {
    let generation = match validate_action(&state, &form.csrf_token, &form.queue_generation) {
        Ok(value) => value,
        Err((status, message)) => return plain(status, &message),
    };
    if state.queue.snapshot().await.generation != generation {
        return plain(StatusCode::CONFLICT, "Queue changed concurrently");
    }
    state.queue.demand(QueueEventReason::SupplierRefresh);
    Redirect::to("/settings").into_response()
}
async fn reset_supplier_seen(
    State(state): State<Arc<AppState>>,
    Form(form): Form<QueueActionForm>,
) -> Response {
    let generation = match validate_action(&state, &form.csrf_token, &form.queue_generation) {
        Ok(value) => value,
        Err((status, message)) => return plain(status, &message),
    };
    let supplier_id = state
        .settings
        .snapshot()
        .await
        .settings
        .supplier()
        .id
        .clone();
    mutation_response(state.queue.reset_seen(generation, &supplier_id).await)
}
fn mutation_response(result: anyhow::Result<crate::queue::QueueSnapshot>) -> Response {
    match result {
        Ok(_) => Redirect::to("/settings").into_response(),
        Err(error) => plain(StatusCode::CONFLICT, &error.to_string()),
    }
}
fn validate_action(
    state: &AppState,
    csrf: &str,
    generation: &str,
) -> Result<u64, (StatusCode, String)> {
    if csrf != state.csrf_token() {
        return Err((StatusCode::FORBIDDEN, "Invalid CSRF token".into()));
    }
    generation.parse().map_err(|_| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "Queue generation must be an integer".into(),
        )
    })
}

async fn upload_queue(State(state): State<Arc<AppState>>, mut multipart: Multipart) -> Response {
    let settings = state.settings.snapshot().await.settings;
    let mut csrf = None;
    let mut generation = None;
    let mut uploads = Vec::new();
    let mut total = 0_u64;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => return plain(StatusCode::BAD_REQUEST, &error.to_string()),
        };
        match field.name().unwrap_or_default() {
            "csrf_token" => match field.text().await {
                Ok(value) => csrf = Some(value),
                Err(error) => return plain(StatusCode::BAD_REQUEST, &error.to_string()),
            },
            "queue_generation" => match field.text().await {
                Ok(value) => generation = Some(value),
                Err(error) => return plain(StatusCode::BAD_REQUEST, &error.to_string()),
            },
            "images" => {
                let Some(filename) = field.file_name().map(str::to_owned) else {
                    continue;
                };
                let bytes = match field.bytes().await {
                    Ok(bytes) => bytes,
                    Err(error) => return plain(StatusCode::BAD_REQUEST, &error.to_string()),
                };
                total = total.saturating_add(bytes.len() as u64);
                if total > settings.queue.max_upload_bytes {
                    return plain(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "Upload exceeds configured limit",
                    );
                }
                uploads.push((safe_display_name(&filename), bytes.to_vec()));
            }
            _ => {}
        }
    }
    let csrf = csrf.unwrap_or_default();
    let generation = generation.unwrap_or_default();
    let generation = match validate_action(&state, &csrf, &generation) {
        Ok(value) => value,
        Err((status, message)) => return plain(status, &message),
    };
    if uploads.is_empty() {
        return plain(StatusCode::UNPROCESSABLE_ENTITY, "Select at least one JPEG");
    }
    let requests = uploads
        .into_iter()
        .map(|(display_name, bytes)| crate::queue::EnqueueRequest {
            display_name,
            bytes,
            supplier_id: None,
            source_key: None,
        })
        .collect();
    match state
        .queue
        .enqueue_batch(Some(generation), None, requests)
        .await
    {
        Ok(_) => Redirect::to("/settings").into_response(),
        Err(error) => plain(StatusCode::UNPROCESSABLE_ENTITY, &error.to_string()),
    }
}
fn safe_display_name(name: &str) -> String {
    std::path::Path::new(name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("upload.jpg")
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect()
}

async fn queue_thumbnail(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    preview_response(state.queue.prepare(&id).await)
}
async fn queue_preview(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    preview_response(state.queue.prepare(&id).await)
}
fn preview_response(result: anyhow::Result<PreparedImage>) -> Response {
    match result {
        Ok(image) => jpeg_response(
            StatusCode::OK,
            Body::from(image.bytes.clone()),
            image.bytes.len(),
            None,
            Some("no-store"),
        ),
        Err(_) => plain(StatusCode::NOT_FOUND, "Queue image not found"),
    }
}

async fn health_ready(State(state): State<Arc<AppState>>) -> StatusCode {
    if state.queue.current().await.ok().flatten().is_some() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn get_image(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let settings = state.settings.snapshot().await.settings;
    let every_fetch = matches!(settings.advancement, AdvancementSettings::EveryFetch { .. });
    let qualifies = match &settings.advancement {
        AdvancementSettings::EveryFetch { frame_client_ips } => {
            frame_client_ips.is_empty() || frame_client_ips.contains(&peer.ip())
        }
        _ => false,
    };
    let response = if every_fetch && qualifies {
        let permit = state.frame_serialization.clone().lock_owned().await;
        match state.queue.current().await {
            Ok(Some(image)) => completion_response(state.queue.clone(), image, permit),
            Ok(None) => plain(StatusCode::NOT_FOUND, "Queue is empty"),
            Err(error) => plain(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
        }
    } else {
        stable_current_response(&state, &headers, true, every_fetch).await
    };
    let status = response.status();
    state
        .record_image_request(ImageRequest {
            requested_at: Utc::now(),
            client_ip: peer.ip(),
            response_status: status,
        })
        .await;
    info!(client_ip = %peer.ip(), qualifies, %status, "frame image requested");
    response
}

async fn head_image(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let every_fetch = matches!(
        state.settings.snapshot().await.settings.advancement,
        AdvancementSettings::EveryFetch { .. }
    );
    stable_current_response(&state, &headers, false, every_fetch).await
}

async fn stable_current_response(
    state: &AppState,
    headers: &HeaderMap,
    include_body: bool,
    no_store: bool,
) -> Response {
    let image = match state.queue.current().await {
        Ok(Some(image)) => image,
        Ok(None) => return plain(StatusCode::NOT_FOUND, "Queue is empty"),
        Err(error) => return plain(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let modified = httpdate::fmt_http_date(image.modified);
    if !no_store && is_not_modified(headers, image.modified) {
        return jpeg_response(
            StatusCode::NOT_MODIFIED,
            Body::empty(),
            0,
            Some(&modified),
            None,
        );
    }
    let length = image.bytes.len();
    jpeg_response(
        StatusCode::OK,
        if include_body {
            Body::from(image.bytes)
        } else {
            Body::empty()
        },
        length,
        if no_store { None } else { Some(&modified) },
        if no_store { Some("no-store") } else { None },
    )
}

fn completion_response(
    queue: Arc<crate::queue::QueueStore>,
    image: PreparedImage,
    permit: OwnedMutexGuard<()>,
) -> Response {
    let id = image.entry.id;
    let stream = CompletionStream {
        bytes: Bytes::from(image.bytes),
        completion: Some((queue, id, permit)),
        delay: None,
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

struct CompletionStream {
    bytes: Bytes,
    completion: Option<(Arc<crate::queue::QueueStore>, String, OwnedMutexGuard<()>)>,
    delay: Option<Pin<Box<tokio::time::Sleep>>>,
}
impl Stream for CompletionStream {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        if let Some(delay) = self.delay.as_mut() {
            if delay.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            self.delay = None;
        }
        if !self.bytes.is_empty() {
            let count = self.bytes.len().min(64 * 1024);
            let chunk = self.bytes.split_to(count);
            self.delay = Some(Box::pin(tokio::time::sleep(Duration::from_millis(1))));
            return Poll::Ready(Some(Ok(chunk)));
        }
        if let Some((queue, id, permit)) = self.completion.take() {
            tokio::spawn(async move {
                if let Err(error) = queue.advance_if_current(&id).await {
                    error!(%error, "completed-response advancement failed");
                }
                drop(permit);
            });
        }
        Poll::Ready(None)
    }
}

fn jpeg_response(
    status: StatusCode,
    body: Body,
    length: usize,
    modified: Option<&str>,
    cache: Option<&str>,
) -> Response {
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "image/jpeg");
    if status != StatusCode::NOT_MODIFIED {
        builder = builder.header(header::CONTENT_LENGTH, length);
    }
    if let Some(value) = modified {
        builder = builder.header(header::LAST_MODIFIED, value);
    }
    if let Some(value) = cache {
        builder = builder.header(header::CACHE_CONTROL, value);
    }
    builder
        .body(body)
        .unwrap_or_else(|_| Response::new(Body::empty()))
}
fn is_not_modified(headers: &HeaderMap, modified: SystemTime) -> bool {
    let Some(value) = headers
        .get(header::IF_MODIFIED_SINCE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| httpdate::parse_http_date(value).ok())
    else {
        return false;
    };
    let Ok(duration) = modified.duration_since(UNIX_EPOCH) else {
        return false;
    };
    UNIX_EPOCH + Duration::from_secs(duration.as_secs()) <= value
}
fn plain(status: StatusCode, message: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("{message}\n"),
    )
        .into_response()
}

impl SettingsForm {
    fn from_settings(generation: u64, csrf: &str, settings: &Settings) -> Self {
        let mut form = Self {
            generation: generation.to_string(),
            csrf_token: csrf.into(),
            target_length: settings.queue.target_length.to_string(),
            max_upload_bytes: settings.queue.max_upload_bytes.to_string(),
            advancement_type: "manual".into(),
            frame_client_ips: String::new(),
            schedule_timezone: "UTC".into(),
            schedule_times: "03:00".into(),
            supplier_type: String::new(),
            supplier_id: settings.supplier().id.clone(),
            database_path: String::new(),
            query: String::new(),
            path_mappings: String::new(),
            max_candidates: "50000".into(),
            manual_directory: String::new(),
            poll_interval_seconds: "60".into(),
        };
        match &settings.advancement {
            AdvancementSettings::EveryFetch { frame_client_ips } => {
                form.advancement_type = "every_fetch".into();
                form.frame_client_ips = frame_client_ips
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            AdvancementSettings::Scheduled { timezone, times } => {
                form.advancement_type = "scheduled".into();
                form.schedule_timezone = timezone.clone();
                form.schedule_times = times.join("\n");
            }
            AdvancementSettings::Manual => form.advancement_type = "manual".into(),
        }
        match &settings.supplier().kind {
            SupplierKind::DigikamSqlite(digi) => {
                form.supplier_type = "digikam_sqlite".into();
                form.database_path = digi.database_path.display().to_string();
                form.query = digi.query.clone();
                form.path_mappings = digi
                    .path_mappings
                    .iter()
                    .map(|m| {
                        format!(
                            "{} => {}",
                            m.database_root.display(),
                            m.mounted_root.display()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                form.max_candidates = digi.max_candidates.to_string();
            }
            SupplierKind::ManualFolder(manual) => {
                form.supplier_type = "manual_folder".into();
                form.manual_directory = manual.directory.display().to_string();
                form.poll_interval_seconds = manual.poll_interval_seconds.to_string();
            }
        }
        form
    }

    fn to_settings(&self) -> Result<Settings, String> {
        let advancement = match self.advancement_type.as_str() {
            "every_fetch" => AdvancementSettings::EveryFetch {
                frame_client_ips: self
                    .frame_client_ips
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| {
                        line.trim()
                            .parse::<IpAddr>()
                            .map_err(|_| format!("Invalid frame client IP: {line}"))
                    })
                    .collect::<Result<_, _>>()?,
            },
            "scheduled" => AdvancementSettings::Scheduled {
                timezone: self.schedule_timezone.trim().into(),
                times: self
                    .schedule_times
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| line.trim().into())
                    .collect(),
            },
            "manual" => AdvancementSettings::Manual,
            _ => return Err("Unknown advancement policy".into()),
        };
        let kind = match self.supplier_type.as_str() {
            "digikam_sqlite" => SupplierKind::DigikamSqlite(DigikamSettings {
                database_path: self.database_path.trim().into(),
                query: self.query.clone(),
                path_mappings: parse_mappings(&self.path_mappings)?,
                max_candidates: self
                    .max_candidates
                    .parse()
                    .map_err(|_| "Maximum candidates must be an integer")?,
            }),
            "manual_folder" => SupplierKind::ManualFolder(ManualFolderSettings {
                directory: self.manual_directory.trim().into(),
                poll_interval_seconds: self
                    .poll_interval_seconds
                    .parse()
                    .map_err(|_| "Poll interval must be an integer")?,
            }),
            _ => return Err("Unknown supplier type".into()),
        };
        let settings = Settings {
            queue: QueueSettings {
                target_length: self
                    .target_length
                    .parse()
                    .map_err(|_| "Target length must be an integer")?,
                max_upload_bytes: self
                    .max_upload_bytes
                    .parse()
                    .map_err(|_| "Maximum upload bytes must be an integer")?,
            },
            advancement,
            suppliers: vec![SupplierSettings {
                id: self.supplier_id.trim().into(),
                kind,
            }],
        };
        settings.validate().map_err(|error| error.to_string())?;
        Ok(settings)
    }
}
fn parse_mappings(input: &str) -> Result<Vec<PathMapping>, String> {
    input
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (a, b) = line
                .split_once("=>")
                .ok_or("Path mappings must use source => mounted")?;
            Ok(PathMapping {
                database_root: a.trim().into(),
                mounted_root: b.trim().into(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, net::IpAddr, path::Path};

    use axum::{body::Body, extract::ConnectInfo, http::Request};
    use http_body_util::BodyExt;
    use tempfile::tempdir;
    use tokio::time::{Duration as TokioDuration, timeout};
    use tower::ServiceExt;

    use crate::{config::SettingsManager, refill};

    use super::*;

    const A: &[u8] = b"\xff\xd8\xffA\xff\xd9";
    const B: &[u8] = b"\xff\xd8\xffB\xff\xd9";
    const C: &[u8] = b"\xff\xd8\xffC\xff\xd9";

    async fn manual_state(policy: AdvancementSettings) -> (tempfile::TempDir, Arc<AppState>) {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let queue = directory.path().join("queue");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&queue).unwrap();
        fs::write(source.join("b.jpg"), B).unwrap();
        fs::write(source.join("a.jpg"), A).unwrap();
        fs::write(source.join("c.jpg"), C).unwrap();
        let settings = Settings {
            queue: QueueSettings {
                target_length: 3,
                max_upload_bytes: 1024,
            },
            advancement: policy,
            suppliers: vec![SupplierSettings {
                id: "manual".into(),
                kind: SupplierKind::ManualFolder(ManualFolderSettings {
                    directory: source,
                    poll_interval_seconds: 60,
                }),
            }],
        };
        let settings_path = directory.path().join("settings.toml");
        fs::write(&settings_path, toml::to_string_pretty(&settings).unwrap()).unwrap();
        let state = AppState::initialize(
            crate::config::Config {
                bind: "127.0.0.1:0".parse().unwrap(),
                settings_path: settings_path.clone(),
                queue_dir: queue,
            },
            SettingsManager::load(settings_path).unwrap(),
        )
        .await
        .unwrap();
        refill::refill_once(&state).await.unwrap();
        (directory, state)
    }

    fn frame_request(method: &str, ip: [u8; 4]) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri("/frame/current.jpg")
            .extension(ConnectInfo(SocketAddr::from((ip, 1234))))
            .body(Body::empty())
            .unwrap()
    }

    async fn bytes(response: Response) -> Vec<u8> {
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec()
    }

    async fn wait_for_current(state: &AppState, expected: &[u8]) {
        timeout(TokioDuration::from_secs(1), async {
            loop {
                if state.queue.current().await.unwrap().unwrap().bytes == expected {
                    break;
                }
                tokio::time::sleep(TokioDuration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn manual_supplier_fills_alphabetically_and_common_ui_is_source_independent() {
        let (directory, state) = manual_state(AdvancementSettings::Manual).await;
        let snapshot = state.queue.snapshot().await;
        assert_eq!(
            snapshot
                .entries
                .iter()
                .map(|entry| entry.display_name.as_str())
                .collect::<Vec<_>>(),
            vec!["a.jpg", "b.jpg", "c.jpg"]
        );
        let source_hash = fs::read(directory.path().join("source/a.jpg")).unwrap();
        let response = router(state)
            .oneshot(Request::get("/settings").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let html = String::from_utf8(bytes(response).await).unwrap();
        assert!(html.contains("Your photo queue"));
        assert!(html.contains("a.jpg"));
        assert_eq!(
            fs::read(directory.path().join("source/a.jpg")).unwrap(),
            source_hash
        );
    }

    #[tokio::test]
    async fn every_fetch_advances_only_after_complete_body_and_serializes() {
        let (_directory, state) = manual_state(AdvancementSettings::EveryFetch {
            frame_client_ips: Vec::new(),
        })
        .await;
        let app = router(state.clone());
        let response = app
            .clone()
            .oneshot(frame_request("GET", [127, 0, 0, 1]))
            .await
            .unwrap();
        assert_eq!(state.queue.current().await.unwrap().unwrap().bytes, A);
        assert_eq!(bytes(response).await, A);
        wait_for_current(&state, B).await;

        let dropped = app
            .clone()
            .oneshot(frame_request("GET", [127, 0, 0, 1]))
            .await
            .unwrap();
        drop(dropped);
        tokio::time::sleep(TokioDuration::from_millis(10)).await;
        assert_eq!(state.queue.current().await.unwrap().unwrap().bytes, B);

        let first = app
            .clone()
            .oneshot(frame_request("GET", [127, 0, 0, 1]))
            .await
            .unwrap();
        let first_body = tokio::spawn(bytes(first));
        let second = app
            .oneshot(frame_request("GET", [127, 0, 0, 1]))
            .await
            .unwrap();
        assert_eq!(first_body.await.unwrap(), B);
        assert_eq!(bytes(second).await, C);
        wait_for_current(&state, C).await; // C is retained as the sole last image.
    }

    #[tokio::test]
    async fn in_flight_completion_cannot_advance_a_new_current_after_reorder() {
        let (_directory, state) = manual_state(AdvancementSettings::EveryFetch {
            frame_client_ips: Vec::new(),
        })
        .await;
        let response = router(state.clone())
            .oneshot(frame_request("GET", [127, 0, 0, 1]))
            .await
            .unwrap();
        let snapshot = state.queue.snapshot().await;
        let reversed = snapshot
            .entries
            .iter()
            .rev()
            .map(|entry| entry.id.clone())
            .collect();
        state
            .queue
            .reorder(snapshot.generation, reversed)
            .await
            .unwrap();
        assert_eq!(state.queue.current().await.unwrap().unwrap().bytes, C);
        assert_eq!(bytes(response).await, A);
        tokio::time::sleep(TokioDuration::from_millis(10)).await;
        assert_eq!(state.queue.current().await.unwrap().unwrap().bytes, C);
    }

    #[tokio::test]
    async fn client_allowlist_head_and_manual_policy_do_not_advance_on_fetch() {
        let (_directory, state) = manual_state(AdvancementSettings::EveryFetch {
            frame_client_ips: vec![IpAddr::from([10, 0, 0, 2])],
        })
        .await;
        let app = router(state.clone());
        let unmatched = Request::builder()
            .uri("/frame/current.jpg")
            .header("x-forwarded-for", "10.0.0.2")
            .header(header::USER_AGENT, "PaperlessPaper-like-browser")
            .extension(ConnectInfo(SocketAddr::from(([10, 0, 0, 3], 1))))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            bytes(app.clone().oneshot(unmatched).await.unwrap()).await,
            A
        );
        assert_eq!(
            bytes(
                app.clone()
                    .oneshot(frame_request("HEAD", [10, 0, 0, 2]))
                    .await
                    .unwrap()
            )
            .await,
            b""
        );
        assert_eq!(state.queue.current().await.unwrap().unwrap().bytes, A);

        let (_directory, manual) = manual_state(AdvancementSettings::Manual).await;
        let app = router(manual.clone());
        let first = app
            .clone()
            .oneshot(frame_request("GET", [127, 0, 0, 1]))
            .await
            .unwrap();
        let modified = first.headers()[header::LAST_MODIFIED].clone();
        assert_eq!(bytes(first).await, A);
        let conditional = Request::builder()
            .uri("/frame/current.jpg")
            .header(header::IF_MODIFIED_SINCE, modified)
            .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1))))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(conditional).await.unwrap().status(),
            StatusCode::NOT_MODIFIED
        );
        assert_eq!(manual.queue.current().await.unwrap().unwrap().bytes, A);
    }

    #[tokio::test]
    async fn universal_http_controls_are_protected_and_do_not_touch_source() {
        let (directory, state) = manual_state(AdvancementSettings::Manual).await;
        let app = router(state.clone());
        let before = state.queue.snapshot().await;
        let thumbnail = app
            .clone()
            .oneshot(
                Request::get(format!("/queue/thumbnail/{}", before.entries[0].id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bytes(thumbnail).await, A);
        assert_eq!(state.queue.snapshot().await.generation, before.generation);

        let bad = serde_urlencoded::to_string([
            ("csrf_token", "wrong"),
            ("queue_generation", &before.generation.to_string()),
            ("id", before.entries[0].id.as_str()),
        ])
        .unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::post("/queue/remove")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(bad))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let good = serde_urlencoded::to_string([
            ("csrf_token", state.csrf_token()),
            ("queue_generation", &before.generation.to_string()),
            ("id", before.entries[0].id.as_str()),
        ])
        .unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::post("/queue/remove")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(good))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(state.queue.current().await.unwrap().unwrap().bytes, B);

        let generation = state.queue.snapshot().await.generation;
        let boundary = "queue-upload-boundary";
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"csrf_token\"\r\n\r\n{}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"queue_generation\"\r\n\r\n{generation}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"images\"; filename=\"direct.jpg\"\r\nContent-Type: image/jpeg\r\n\r\n",
            state.csrf_token()
        ).into_bytes();
        body.extend_from_slice(A);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let response = app
            .oneshot(
                Request::post("/queue/upload")
                    .header(
                        header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(
            state
                .queue
                .snapshot()
                .await
                .entries
                .iter()
                .any(|entry| entry.display_name == "direct.jpg" && entry.supplier_id.is_none())
        );
        assert!(!directory.path().join("source/direct.jpg").exists());
    }

    #[tokio::test]
    async fn existing_queue_starts_and_remains_ready_when_supplier_is_unavailable() {
        let (directory, state) = manual_state(AdvancementSettings::Manual).await;
        drop(state);
        fs::remove_dir_all(directory.path().join("source")).unwrap();
        let settings_path = directory.path().join("settings.toml");
        let restarted = AppState::initialize(
            crate::config::Config {
                bind: "127.0.0.1:0".parse().unwrap(),
                settings_path: settings_path.clone(),
                queue_dir: directory.path().join("queue"),
            },
            SettingsManager::load(settings_path).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(restarted.queue.current().await.unwrap().unwrap().bytes, A);
        let mut settings = restarted.settings.snapshot().await.settings;
        settings.queue.target_length = 4;
        restarted.settings.replace(1, settings).await.unwrap();
        assert!(refill::refill_once(&restarted).await.is_err());
        assert_eq!(restarted.queue.current().await.unwrap().unwrap().bytes, A);
        let ready = router(restarted)
            .oneshot(Request::get("/health/ready").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(ready.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn background_refill_reacts_to_queue_events_and_shuts_down() {
        let (directory, state) = manual_state(AdvancementSettings::Manual).await;
        fs::write(
            directory.path().join("source/d.jpg"),
            b"\xff\xd8\xffD\xff\xd9",
        )
        .unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(refill::run(state.clone(), shutdown_rx));
        tokio::time::sleep(TokioDuration::from_millis(5)).await;
        let snapshot = state.queue.snapshot().await;
        state
            .queue
            .remove(snapshot.generation, &snapshot.entries[0].id)
            .await
            .unwrap();
        timeout(TokioDuration::from_secs(1), async {
            while state.queue.snapshot().await.entries.len() != 3 {
                tokio::time::sleep(TokioDuration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        let names = state
            .queue
            .snapshot()
            .await
            .entries
            .into_iter()
            .map(|entry| entry.display_name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["b.jpg", "c.jpg", "d.jpg"]);
        shutdown_tx.send(true).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn queue_change_refills_with_next_unseen_and_source_changes_do_not_touch_copy() {
        let (directory, state) = manual_state(AdvancementSettings::Manual).await;
        let original_b = state
            .queue
            .prepare(&state.queue.snapshot().await.entries[1].id)
            .await
            .unwrap()
            .bytes;
        fs::write(
            directory.path().join("source/b.jpg"),
            b"\xff\xd8\xffCHANGED\xff\xd9",
        )
        .unwrap();
        fs::remove_file(directory.path().join("source/a.jpg")).unwrap();
        assert_eq!(
            state
                .queue
                .prepare(&state.queue.snapshot().await.entries[1].id)
                .await
                .unwrap()
                .bytes,
            original_b
        );

        let snapshot = state.queue.snapshot().await;
        state
            .queue
            .remove(snapshot.generation, &snapshot.entries[0].id)
            .await
            .unwrap();
        // c was already queued, and removed a remains seen; add d as the next alphabetical unseen candidate.
        fs::write(
            directory.path().join("source/d.jpg"),
            b"\xff\xd8\xffD\xff\xd9",
        )
        .unwrap();
        refill::refill_once(&state).await.unwrap();
        let names = state
            .queue
            .snapshot()
            .await
            .entries
            .into_iter()
            .map(|entry| entry.display_name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["b.jpg", "c.jpg", "b.jpg"]); // changed b content is a new identity and sorts before d.
        assert_eq!(
            state
                .queue
                .seen("manual")
                .await
                .iter()
                .filter(|key| key.contains("a.jpg"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn queue_reorder_remove_and_manual_advance_include_current() {
        let (_directory, state) = manual_state(AdvancementSettings::Manual).await;
        let snapshot = state.queue.snapshot().await;
        let reversed = snapshot
            .entries
            .iter()
            .rev()
            .map(|entry| entry.id.clone())
            .collect();
        state
            .queue
            .reorder(snapshot.generation, reversed)
            .await
            .unwrap();
        assert_eq!(state.queue.current().await.unwrap().unwrap().bytes, C);
        state.queue.advance().await.unwrap();
        assert_eq!(state.queue.current().await.unwrap().unwrap().bytes, B);
        let snapshot = state.queue.snapshot().await;
        let current = snapshot.entries[0].id.clone();
        state
            .queue
            .remove(snapshot.generation, &current)
            .await
            .unwrap();
        assert_eq!(state.queue.current().await.unwrap().unwrap().bytes, A);
        assert_eq!(
            fs::read(Path::new(state.config.settings_path.parent().unwrap()).join("source/b.jpg"))
                .unwrap(),
            B
        );
    }
}
