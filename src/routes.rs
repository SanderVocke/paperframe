use std::{
    io::ErrorKind,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use askama::Template;
use axum::{
    Router,
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, Form, Multipart, Path as AxumPath, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tower_http::trace::{DefaultMakeSpan, DefaultOnFailure, DefaultOnResponse, TraceLayer};
use tracing::{Level, error, info};

use crate::{
    catalog,
    config::{
        DigikamSettings, FolderSettings, ModeSettings, PathMapping, ReplaceSettingsError,
        SelectionMode, SelectionSettings, Settings,
    },
    state::{ActiveMode, AppState, ImageRequest},
};

const SETTINGS_BODY_LIMIT: usize = 64 * 1024;
const UPLOAD_BODY_LIMIT: usize = 101 * 1024 * 1024;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/settings", get(settings_page).post(update_settings))
        .route("/folder/refresh", post(refresh_folder))
        .route("/folder/reorder", post(reorder_folder))
        .route(
            "/folder/upload",
            post(upload_folder).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
        )
        .route("/folder/thumbnail/{id}", get(folder_thumbnail))
        .route("/frame/current.jpg", get(get_image).head(head_image))
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
        .layer(DefaultBodyLimit::max(SETTINGS_BODY_LIMIT))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO))
                .on_failure(DefaultOnFailure::new().level(Level::ERROR)),
        )
        .with_state(state)
}

#[derive(Clone)]
struct FolderTemplateEntry {
    id: String,
    filename: String,
    is_next: bool,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    generation: u64,
    csrf_token: String,
    active_digikam: bool,
    active_folder: bool,
    database_path: String,
    query: String,
    path_mappings: String,
    mode_first: bool,
    mode_daily_random: bool,
    random_seed: String,
    timezone: String,
    rollover_time: String,
    max_candidates: String,
    folder_directory: String,
    max_upload_bytes: String,
    folder_generation: u64,
    folder_entries: Vec<FolderTemplateEntry>,
    reorder_lines: String,
    cache_ready: bool,
    last_request: String,
    mode_status: String,
    error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SettingsForm {
    generation: String,
    csrf_token: String,
    active_mode: String,
    database_path: String,
    query: String,
    path_mappings: String,
    selection_mode: String,
    random_seed: String,
    timezone: String,
    rollover_time: String,
    max_candidates: String,
    folder_directory: String,
    max_upload_bytes: String,
}

#[derive(Debug, Deserialize)]
struct FolderActionForm {
    csrf_token: String,
    inventory_generation: String,
}

#[derive(Debug, Deserialize)]
struct FolderReorderForm {
    csrf_token: String,
    inventory_generation: String,
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
        return render_form_error(&state, form, StatusCode::FORBIDDEN, "Invalid CSRF token").await;
    }
    let expected_generation = match parse_u64(&form.generation, "Settings generation") {
        Ok(value) => value,
        Err(message) => {
            return render_form_error(&state, form, StatusCode::UNPROCESSABLE_ENTITY, &message)
                .await;
        }
    };
    let proposed = match form.to_settings() {
        Ok(settings) => settings,
        Err(message) => {
            return render_form_error(&state, form, StatusCode::UNPROCESSABLE_ENTITY, &message)
                .await;
        }
    };
    let prepared = match validate_and_prepare_mode(&state, &proposed).await {
        Ok(mode) => mode,
        Err(error) => {
            return render_form_error(
                &state,
                form,
                StatusCode::UNPROCESSABLE_ENTITY,
                &error.to_string(),
            )
            .await;
        }
    };

    match state.settings.replace(expected_generation, proposed).await {
        Ok(_) => {
            state.activate_mode(prepared).await;
            state.trigger_selection();
            Redirect::to("/settings").into_response()
        }
        Err(ReplaceSettingsError::Stale { current_generation }) => {
            render_form_error(
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
            render_form_error(&state, form, StatusCode::UNPROCESSABLE_ENTITY, &message).await
        }
        Err(ReplaceSettingsError::Persistence(message)) => {
            render_form_error(
                &state,
                form,
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Failed to persist settings: {message}"),
            )
            .await
        }
    }
}

async fn validate_and_prepare_mode(
    state: &Arc<AppState>,
    settings: &Settings,
) -> anyhow::Result<ActiveMode> {
    settings.validate_basic()?;
    if let ModeSettings::DigikamSqlite(digikam) = &settings.mode {
        catalog::validate(digikam.clone()).await?;
    }
    state.prepare_mode(&settings.mode).await
}

async fn refresh_folder(
    State(state): State<Arc<AppState>>,
    Form(form): Form<FolderActionForm>,
) -> Response {
    let generation =
        match validate_folder_form(&state, &form.csrf_token, &form.inventory_generation) {
            Ok(value) => value,
            Err((status, message)) => return plain_error(status, &message),
        };
    let ActiveMode::Folder(folder) = state.active_mode().await else {
        return plain_error(StatusCode::CONFLICT, "Folder mode is not active");
    };
    match folder.refresh(generation).await {
        Ok(_) => Redirect::to("/settings").into_response(),
        Err(error) => plain_error(StatusCode::CONFLICT, &error.to_string()),
    }
}

async fn reorder_folder(
    State(state): State<Arc<AppState>>,
    Form(form): Form<FolderReorderForm>,
) -> Response {
    let generation =
        match validate_folder_form(&state, &form.csrf_token, &form.inventory_generation) {
            Ok(value) => value,
            Err((status, message)) => return plain_error(status, &message),
        };
    let ActiveMode::Folder(folder) = state.active_mode().await else {
        return plain_error(StatusCode::CONFLICT, "Folder mode is not active");
    };
    let order = form
        .order
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    match folder.reorder(generation, order).await {
        Ok(_) => Redirect::to("/settings").into_response(),
        Err(error) => plain_error(StatusCode::CONFLICT, &error.to_string()),
    }
}

async fn upload_folder(State(state): State<Arc<AppState>>, mut multipart: Multipart) -> Response {
    let ActiveMode::Folder(folder) = state.active_mode().await else {
        return plain_error(StatusCode::CONFLICT, "Folder mode is not active");
    };
    let mut csrf_token = None;
    let mut generation = None;
    let mut uploads = Vec::new();
    let mut total = 0_u64;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                return plain_error(
                    StatusCode::BAD_REQUEST,
                    &format!("Invalid multipart upload: {error}"),
                );
            }
        };
        let name = field.name().unwrap_or_default().to_owned();
        if name == "csrf_token" {
            match field.text().await {
                Ok(value) => csrf_token = Some(value),
                Err(error) => {
                    return plain_error(
                        StatusCode::BAD_REQUEST,
                        &format!("Invalid CSRF field: {error}"),
                    );
                }
            }
        } else if name == "inventory_generation" {
            match field.text().await {
                Ok(value) => generation = Some(value),
                Err(error) => {
                    return plain_error(
                        StatusCode::BAD_REQUEST,
                        &format!("Invalid generation field: {error}"),
                    );
                }
            }
        } else if name == "images" {
            let Some(filename) = field.file_name().map(str::to_owned) else {
                continue;
            };
            let bytes = match field.bytes().await {
                Ok(bytes) => bytes,
                Err(error) => {
                    return plain_error(
                        StatusCode::BAD_REQUEST,
                        &format!("Failed to read upload: {error}"),
                    );
                }
            };
            total = match total.checked_add(bytes.len() as u64) {
                Some(value) => value,
                None => return plain_error(StatusCode::PAYLOAD_TOO_LARGE, "Upload is too large"),
            };
            if total > folder.max_upload_bytes() {
                return plain_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Upload exceeds the configured limit",
                );
            }
            uploads.push((filename, bytes.to_vec()));
        }
    }
    let Some(csrf_token) = csrf_token else {
        return plain_error(StatusCode::FORBIDDEN, "Missing CSRF token");
    };
    let Some(generation) = generation else {
        return plain_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Missing inventory generation",
        );
    };
    let generation = match validate_folder_form(&state, &csrf_token, &generation) {
        Ok(value) => value,
        Err((status, message)) => return plain_error(status, &message),
    };
    match folder.upload(generation, uploads).await {
        Ok(_) => Redirect::to("/settings").into_response(),
        Err(error) => plain_error(StatusCode::UNPROCESSABLE_ENTITY, &error.to_string()),
    }
}

fn validate_folder_form(
    state: &AppState,
    csrf: &str,
    generation: &str,
) -> Result<u64, (StatusCode, String)> {
    if csrf != state.csrf_token() {
        return Err((StatusCode::FORBIDDEN, "Invalid CSRF token".into()));
    }
    parse_u64(generation, "Inventory generation")
        .map_err(|message| (StatusCode::UNPROCESSABLE_ENTITY, message))
}

async fn folder_thumbnail(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let ActiveMode::Folder(folder) = state.active_mode().await else {
        return plain_error(StatusCode::NOT_FOUND, "Thumbnail not found");
    };
    match folder.thumbnail(&id).await {
        Ok(image) => rotating_image_response(StatusCode::OK, image.bytes, true),
        Err(_) => plain_error(StatusCode::NOT_FOUND, "Thumbnail not found"),
    }
}

async fn render_form_error(
    state: &Arc<AppState>,
    mut form: SettingsForm,
    status: StatusCode,
    error: &str,
) -> Response {
    let generation = state.settings.snapshot().await.generation;
    form.generation = generation.to_string();
    form.csrf_token = state.csrf_token().to_owned();
    render_settings(state, generation, form, Some(error.to_owned()), status).await
}

async fn render_settings(
    state: &Arc<AppState>,
    generation: u64,
    form: SettingsForm,
    error: Option<String>,
    status: StatusCode,
) -> Response {
    let last_request = match state.last_image_request().await {
        Some(request) => format!(
            "{} from {} — {} {}",
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
        None => "No image GET has occurred since the server started.".to_owned(),
    };
    let (folder_generation, folder_entries, reorder_lines, mode_status) =
        match state.active_mode().await {
            ActiveMode::Digikam => (
                0,
                Vec::new(),
                String::new(),
                format_selector_status(&state.selector_status().await),
            ),
            ActiveMode::Folder(folder) => {
                let snapshot = folder.snapshot().await;
                let serving_order = snapshot.serving_order();
                let lines = serving_order
                    .iter()
                    .map(|entry| entry.filename.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let entries = serving_order
                    .into_iter()
                    .enumerate()
                    .map(|(index, entry)| FolderTemplateEntry {
                        id: entry.id,
                        filename: entry.filename,
                        is_next: index == 0,
                    })
                    .collect();
                let status = if snapshot.entries.is_empty() {
                    "Folder queue is empty. Add JPEGs and refresh or upload.".to_owned()
                } else {
                    format!(
                        "Folder queue has {} image(s); the cursor is runtime-only.",
                        snapshot.entries.len()
                    )
                };
                (snapshot.generation, entries, lines, status)
            }
        };
    let template = SettingsTemplate {
        generation,
        csrf_token: state.csrf_token().to_owned(),
        active_digikam: form.active_mode == "digikam_sqlite",
        active_folder: form.active_mode == "folder_sequence",
        database_path: form.database_path,
        query: form.query,
        path_mappings: form.path_mappings,
        mode_first: form.selection_mode == "first",
        mode_daily_random: form.selection_mode == "daily_random",
        random_seed: form.random_seed,
        timezone: form.timezone,
        rollover_time: form.rollover_time,
        max_candidates: form.max_candidates,
        folder_directory: form.folder_directory,
        max_upload_bytes: form.max_upload_bytes,
        folder_generation,
        folder_entries,
        reorder_lines,
        cache_ready: state.is_ready().await,
        last_request,
        mode_status,
        error,
    };
    match template.render() {
        Ok(html) => (status, Html(html)).into_response(),
        Err(error) => {
            tracing::error!(%error, "failed to render settings page");
            plain_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to render settings page",
            )
        }
    }
}

fn format_selector_status(status: &crate::state::SelectorStatus) -> String {
    let mut parts = Vec::new();
    if status.running {
        parts.push("DigiKam selection is running.".to_owned());
    }
    if let Some(success) = &status.last_success {
        parts.push(format!(
            "Last success: {} — image ID {}, selection date {}, {} bytes, cache {}.",
            success
                .completed_at
                .to_rfc3339_opts(SecondsFormat::Secs, true),
            success.result.image_id,
            success.result.selection_date,
            success.result.content_length,
            if success.result.cache_changed {
                "updated"
            } else {
                "unchanged"
            }
        ));
    }
    if let Some(error) = &status.last_error {
        parts.push(format!("Last error: {error}"));
    }
    if let Some(next) = status.next_attempt {
        parts.push(format!(
            "Next attempt: {}.",
            next.to_rfc3339_opts(SecondsFormat::Secs, true)
        ));
    }
    if parts.is_empty() {
        "No DigiKam selection has completed since the server started.".to_owned()
    } else {
        parts.join("\n")
    }
}

impl SettingsForm {
    fn from_settings(generation: u64, csrf_token: &str, settings: &Settings) -> Self {
        let mut form = Self {
            generation: generation.to_string(),
            csrf_token: csrf_token.to_owned(),
            active_mode: String::new(),
            database_path: String::new(),
            query: String::new(),
            path_mappings: String::new(),
            selection_mode: "first".into(),
            random_seed: String::new(),
            timezone: "UTC".into(),
            rollover_time: "00:00".into(),
            max_candidates: "50000".into(),
            folder_directory: String::new(),
            max_upload_bytes: (25 * 1024 * 1024).to_string(),
        };
        match &settings.mode {
            ModeSettings::DigikamSqlite(settings) => {
                form.active_mode = "digikam_sqlite".into();
                form.database_path = settings.database_path.display().to_string();
                form.query = settings.query.clone();
                form.path_mappings = settings
                    .path_mappings
                    .iter()
                    .map(|mapping| {
                        format!(
                            "{} => {}",
                            mapping.database_root.display(),
                            mapping.mounted_root.display()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                form.selection_mode = match settings.selection.mode {
                    SelectionMode::First => "first",
                    SelectionMode::DailyRandom => "daily_random",
                }
                .into();
                form.random_seed = settings.selection.random_seed.clone();
                form.timezone = settings.selection.timezone.clone();
                form.rollover_time = settings.selection.rollover_time.clone();
                form.max_candidates = settings.max_candidates.to_string();
            }
            ModeSettings::FolderSequence(settings) => {
                form.active_mode = "folder_sequence".into();
                form.folder_directory = settings.directory.display().to_string();
                form.max_upload_bytes = settings.max_upload_bytes.to_string();
            }
        }
        form
    }

    fn to_settings(&self) -> Result<Settings, String> {
        let mode = match self.active_mode.as_str() {
            "digikam_sqlite" => {
                let selection_mode = match self.selection_mode.as_str() {
                    "first" => SelectionMode::First,
                    "daily_random" => SelectionMode::DailyRandom,
                    _ => return Err("Selection mode must be first or daily_random".into()),
                };
                ModeSettings::DigikamSqlite(DigikamSettings {
                    database_path: self.database_path.trim().into(),
                    query: self.query.clone(),
                    path_mappings: parse_path_mappings(&self.path_mappings)?,
                    selection: SelectionSettings {
                        mode: selection_mode,
                        random_seed: self.random_seed.clone(),
                        timezone: self.timezone.trim().to_owned(),
                        rollover_time: self.rollover_time.trim().to_owned(),
                    },
                    max_candidates: self
                        .max_candidates
                        .parse()
                        .map_err(|_| "Maximum candidates must be an integer".to_owned())?,
                })
            }
            "folder_sequence" => ModeSettings::FolderSequence(FolderSettings {
                directory: self.folder_directory.trim().into(),
                max_upload_bytes: self
                    .max_upload_bytes
                    .parse()
                    .map_err(|_| "Maximum upload bytes must be an integer".to_owned())?,
            }),
            _ => return Err("Active mode must be digikam_sqlite or folder_sequence".into()),
        };
        let settings = Settings { mode };
        settings
            .validate_basic()
            .map_err(|error| error.to_string())?;
        Ok(settings)
    }
}

fn parse_path_mappings(input: &str) -> Result<Vec<PathMapping>, String> {
    input
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (database_root, mounted_root) = line.split_once("=>").ok_or_else(|| {
                "Each path mapping must use: database root => mounted root".to_owned()
            })?;
            Ok(PathMapping {
                database_root: database_root.trim().into(),
                mounted_root: mounted_root.trim().into(),
            })
        })
        .collect()
}

fn parse_u64(value: &str, label: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("{label} must be an integer"))
}

async fn health_live() -> StatusCode {
    StatusCode::OK
}

async fn health_ready(State(state): State<Arc<AppState>>) -> StatusCode {
    if state.is_ready().await {
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
    let response = match state.active_mode().await {
        ActiveMode::Digikam => cached_image_response(&state, &headers, true).await,
        ActiveMode::Folder(folder) => match folder.next_image().await {
            Ok(image) => rotating_image_response(StatusCode::OK, image.bytes, true),
            Err(error) if !folder.is_ready().await => {
                plain_error(StatusCode::NOT_FOUND, &error.to_string())
            }
            Err(error) => {
                error!(%error, "failed to prepare next folder image");
                plain_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to read next image",
                )
            }
        },
    };
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
    match state.active_mode().await {
        ActiveMode::Digikam => cached_image_response(&state, &headers, false).await,
        ActiveMode::Folder(folder) => match folder.peek_image().await {
            Ok(image) => rotating_image_response(StatusCode::OK, image.bytes, false),
            Err(error) if !folder.is_ready().await => {
                plain_error(StatusCode::NOT_FOUND, &error.to_string())
            }
            Err(error) => {
                error!(%error, "failed to inspect next folder image");
                plain_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to read next image",
                )
            }
        },
    }
}

fn rotating_image_response(status: StatusCode, bytes: Vec<u8>, include_body: bool) -> Response {
    let length = bytes.len();
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CONTENT_LENGTH, length)
        .header(header::CACHE_CONTROL, "no-store")
        .body(if include_body {
            Body::from(bytes)
        } else {
            Body::empty()
        })
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

async fn cached_image_response(
    state: &AppState,
    headers: &HeaderMap,
    include_body: bool,
) -> Response {
    let mut file = match tokio::fs::File::open(state.cache.current_path()).await {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return plain_error(StatusCode::NOT_FOUND, "image not found");
        }
        Err(error) => {
            error!(%error, "failed to open cached image");
            return plain_error(StatusCode::INTERNAL_SERVER_ERROR, "failed to read image");
        }
    };
    let metadata = match file.metadata().await {
        Ok(metadata) => metadata,
        Err(error) => {
            error!(%error, "failed to inspect cached image");
            return plain_error(StatusCode::INTERNAL_SERVER_ERROR, "failed to read image");
        }
    };
    let modified = match metadata.modified() {
        Ok(modified) => modified,
        Err(error) => {
            error!(%error, "cached image has no modification time");
            return plain_error(StatusCode::INTERNAL_SERVER_ERROR, "failed to read image");
        }
    };
    let last_modified = httpdate::fmt_http_date(modified);
    if is_not_modified(headers, modified) {
        return build_cached_response(
            StatusCode::NOT_MODIFIED,
            Body::empty(),
            None,
            &last_modified,
        );
    }
    if !include_body {
        return build_cached_response(
            StatusCode::OK,
            Body::empty(),
            Some(metadata.len()),
            &last_modified,
        );
    }
    let mut bytes = Vec::new();
    if let Err(error) = file.read_to_end(&mut bytes).await {
        error!(%error, "failed to read cached image");
        return plain_error(StatusCode::INTERNAL_SERVER_ERROR, "failed to read image");
    }
    build_cached_response(
        StatusCode::OK,
        Body::from(bytes),
        Some(metadata.len()),
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
    UNIX_EPOCH + Duration::from_secs(since_epoch.as_secs()) <= since
}

fn build_cached_response(
    status: StatusCode,
    body: Body,
    content_length: Option<u64>,
    last_modified: &str,
) -> Response {
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::LAST_MODIFIED, last_modified);
    if let Some(length) = content_length {
        builder = builder.header(header::CONTENT_LENGTH, length);
    }
    builder
        .body(body)
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn plain_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("{message}\n"),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use rusqlite::Connection;
    use tempfile::tempdir;
    use tokio::{
        sync::watch,
        time::{Duration as TokioDuration, timeout},
    };
    use tower::ServiceExt;

    use crate::{cache::CacheStore, config::SettingsManager};

    use super::*;

    const JPEG_A: &[u8] = b"\xff\xd8\xffA\xff\xd9";
    const JPEG_B: &[u8] = b"\xff\xd8\xffB\xff\xd9";

    async fn folder_state() -> (tempfile::TempDir, Arc<AppState>) {
        let directory = tempdir().unwrap();
        let images = directory.path().join("images");
        let cache = directory.path().join("cache");
        fs::create_dir(&images).unwrap();
        fs::create_dir(&cache).unwrap();
        fs::write(images.join("a.jpg"), JPEG_A).unwrap();
        fs::write(images.join("b.jpg"), JPEG_B).unwrap();
        let settings_path = directory.path().join("settings.toml");
        let settings = Settings {
            mode: ModeSettings::FolderSequence(FolderSettings {
                directory: images,
                max_upload_bytes: 1024,
            }),
        };
        fs::write(&settings_path, toml::to_string_pretty(&settings).unwrap()).unwrap();
        let config = crate::config::Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            settings_path: settings_path.clone(),
            cache_dir: cache.clone(),
        };
        let state = AppState::initialize(
            config,
            SettingsManager::load(settings_path).unwrap(),
            CacheStore::new(&cache).unwrap(),
        )
        .await
        .unwrap();
        (directory, state)
    }

    fn frame_request(method: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri("/frame/current.jpg")
            .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1234))))
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

    #[tokio::test]
    async fn folder_get_advances_head_does_not_and_wraps() {
        let (_directory, state) = folder_state().await;
        let app = router(state);
        let head = app.clone().oneshot(frame_request("HEAD")).await.unwrap();
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers()[header::CACHE_CONTROL], "no-store");
        assert!(bytes(head).await.is_empty());
        assert_eq!(
            bytes(app.clone().oneshot(frame_request("GET")).await.unwrap()).await,
            JPEG_A
        );
        let conditional = Request::builder()
            .method("GET")
            .uri("/frame/current.jpg")
            .header(header::IF_MODIFIED_SINCE, "Wed, 31 Dec 2099 23:59:59 GMT")
            .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1234))))
            .body(Body::empty())
            .unwrap();
        let conditional = app.clone().oneshot(conditional).await.unwrap();
        assert_eq!(conditional.status(), StatusCode::OK);
        assert_eq!(bytes(conditional).await, JPEG_B);
        assert_eq!(
            bytes(app.oneshot(frame_request("GET")).await.unwrap()).await,
            JPEG_A
        );
    }

    #[tokio::test]
    async fn thumbnails_and_settings_do_not_advance() {
        let (_directory, state) = folder_state().await;
        let snapshot = match state.active_mode().await {
            ActiveMode::Folder(folder) => folder.snapshot().await,
            ActiveMode::Digikam => unreachable!(),
        };
        let app = router(state);
        let settings = app
            .clone()
            .oneshot(Request::get("/settings").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let html = String::from_utf8(bytes(settings).await).unwrap();
        assert!(html.contains("a.jpg"));
        let thumbnail = app
            .clone()
            .oneshot(
                Request::get(format!("/folder/thumbnail/{}", snapshot.entries[0].id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bytes(thumbnail).await, JPEG_A);
        assert_eq!(
            bytes(app.oneshot(frame_request("GET")).await.unwrap()).await,
            JPEG_A
        );
    }

    #[tokio::test]
    async fn settings_escapes_folder_filenames() {
        let (directory, state) = folder_state().await;
        fs::write(directory.path().join("images/<script>.jpg"), JPEG_A).unwrap();
        if let ActiveMode::Folder(folder) = state.active_mode().await {
            let generation = folder.snapshot().await.generation;
            folder.refresh(generation).await.unwrap();
        }
        let response = router(state)
            .oneshot(Request::get("/settings").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let html = String::from_utf8(bytes(response).await).unwrap();
        assert!(html.contains("&#60;script&#62;.jpg"));
        assert!(!html.contains("<script>.jpg"));
    }

    #[tokio::test]
    async fn readiness_tracks_empty_folder() {
        let (_directory, state) = folder_state().await;
        let app = router(state.clone());
        assert_eq!(
            app.clone()
                .oneshot(Request::get("/health/ready").body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        if let ActiveMode::Folder(folder) = state.active_mode().await {
            for entry in folder.snapshot().await.entries {
                fs::remove_file(folder.root().join(entry.filename)).unwrap();
            }
            let generation = folder.snapshot().await.generation;
            folder.refresh(generation).await.unwrap();
        }
        assert_eq!(
            app.oneshot(Request::get("/health/ready").body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn folder_http_upload_reorder_refresh_and_csrf_protection_work() {
        let (directory, state) = folder_state().await;
        let (generation, token) = match state.active_mode().await {
            ActiveMode::Folder(folder) => (
                folder.snapshot().await.generation,
                state.csrf_token().to_owned(),
            ),
            ActiveMode::Digikam => unreachable!(),
        };
        let boundary = "paperframe-boundary";
        let mut body = Vec::new();
        for (name, value) in [
            ("csrf_token", token.as_str()),
            ("inventory_generation", &generation.to_string()),
        ] {
            body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
        }
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"images\"; filename=\"c.jpg\"\r\nContent-Type: image/jpeg\r\n\r\n").as_bytes());
        body.extend_from_slice(JPEG_A);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let app = router(state.clone());
        let upload = app
            .clone()
            .oneshot(
                Request::post("/folder/upload")
                    .header(
                        header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(upload.status(), StatusCode::SEE_OTHER);

        let folder = match state.active_mode().await {
            ActiveMode::Folder(folder) => folder,
            ActiveMode::Digikam => unreachable!(),
        };
        let snapshot = folder.snapshot().await;
        assert_eq!(snapshot.entries.len(), 3);
        let reversed = snapshot
            .entries
            .iter()
            .rev()
            .map(|entry| entry.filename.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let reorder_body = serde_urlencoded::to_string([
            ("csrf_token", token.as_str()),
            ("inventory_generation", &snapshot.generation.to_string()),
            ("order", reversed.as_str()),
        ])
        .unwrap();
        let reorder = app
            .clone()
            .oneshot(
                Request::post("/folder/reorder")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(reorder_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(reorder.status(), StatusCode::SEE_OTHER);
        assert!(
            fs::read_dir(directory.path().join("images"))
                .unwrap()
                .all(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with('0')
                })
        );

        let current_generation = folder.snapshot().await.generation;
        let bad_refresh = serde_urlencoded::to_string([
            ("csrf_token", "wrong"),
            ("inventory_generation", &current_generation.to_string()),
        ])
        .unwrap();
        let response = app
            .oneshot(
                Request::post("/folder/refresh")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(bad_refresh))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(folder.snapshot().await.generation, current_generation);
    }

    #[tokio::test]
    async fn settings_http_switches_modes_and_triggers_immediate_digikam_selection() {
        let (directory, state) = folder_state().await;
        let photos = directory.path().join("photos");
        fs::create_dir(&photos).unwrap();
        fs::write(photos.join("selected.jpg"), JPEG_B).unwrap();
        let database = directory.path().join("digikam.db");
        Connection::open(&database).unwrap();

        let snapshot = state.settings.snapshot().await;
        let mut form = SettingsForm::from_settings(
            snapshot.generation,
            state.csrf_token(),
            &snapshot.settings,
        );
        form.active_mode = "digikam_sqlite".into();
        form.database_path = database.display().to_string();
        form.query = "SELECT 7 AS image_id, '/library' AS root_path, '/' AS album_path, 'selected.jpg' AS file_name".into();
        form.path_mappings = format!("/library => {}", photos.display());
        form.selection_mode = "first".into();
        form.timezone = "UTC".into();
        form.rollover_time = "00:00".into();
        form.max_candidates = "10".into();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let scheduler = tokio::spawn(crate::scheduler::run(state.clone(), shutdown_rx));
        let response = router(state.clone())
            .oneshot(
                Request::post("/settings")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(serde_urlencoded::to_string(&form).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        timeout(TokioDuration::from_secs(1), async {
            while !state.cache.is_ready() {
                tokio::time::sleep(TokioDuration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(fs::read(state.cache.current_path()).unwrap(), JPEG_B);
        assert!(matches!(state.active_mode().await, ActiveMode::Digikam));

        let snapshot = state.settings.snapshot().await;
        let mut back = SettingsForm::from_settings(
            snapshot.generation,
            state.csrf_token(),
            &snapshot.settings,
        );
        back.active_mode = "folder_sequence".into();
        back.folder_directory = directory.path().join("images").display().to_string();
        back.max_upload_bytes = "1024".into();
        let response = router(state.clone())
            .oneshot(
                Request::post("/settings")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(serde_urlencoded::to_string(&back).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(matches!(state.active_mode().await, ActiveMode::Folder(_)));
        shutdown_tx.send(true).unwrap();
        scheduler.await.unwrap();
    }

    #[tokio::test]
    async fn digikam_cached_endpoint_retains_conditional_behavior_and_peer_tracking() {
        let directory = tempdir().unwrap();
        let cache_dir = directory.path().join("cache");
        fs::create_dir(&cache_dir).unwrap();
        fs::write(cache_dir.join("current.jpg"), JPEG_A).unwrap();
        let settings = Settings {
            mode: ModeSettings::DigikamSqlite(DigikamSettings {
                database_path: "/unused.db".into(),
                query: "SELECT 1".into(),
                path_mappings: vec![PathMapping {
                    database_root: "/library".into(),
                    mounted_root: "/photos".into(),
                }],
                selection: SelectionSettings {
                    mode: SelectionMode::First,
                    random_seed: String::new(),
                    timezone: "UTC".into(),
                    rollover_time: "00:00".into(),
                },
                max_candidates: 1,
            }),
        };
        let config = crate::config::Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            settings_path: directory.path().join("settings.toml"),
            cache_dir: cache_dir.clone(),
        };
        let state = AppState::new(
            config,
            SettingsManager::in_memory(settings),
            CacheStore::new(&cache_dir).unwrap(),
        );
        let app = router(state.clone());
        let first = app.clone().oneshot(frame_request("GET")).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let modified = first.headers()[header::LAST_MODIFIED].clone();
        assert_eq!(bytes(first).await, JPEG_A);
        let conditional = Request::builder()
            .method("GET")
            .uri("/frame/current.jpg")
            .header(header::IF_MODIFIED_SINCE, modified)
            .extension(ConnectInfo(SocketAddr::from(([192, 168, 1, 9], 1234))))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(conditional).await.unwrap().status(),
            StatusCode::NOT_MODIFIED
        );
        assert_eq!(
            state.last_image_request().await.unwrap().client_ip,
            "192.168.1.9".parse::<std::net::IpAddr>().unwrap()
        );
    }

    #[test]
    fn form_builds_both_modes_and_rejects_invalid_mode() {
        let folder = Settings {
            mode: ModeSettings::FolderSequence(FolderSettings {
                directory: PathBuf::from("/images"),
                max_upload_bytes: 10,
            }),
        };
        let mut form = SettingsForm::from_settings(1, "token", &folder);
        assert!(matches!(
            form.to_settings().unwrap().mode,
            ModeSettings::FolderSequence(_)
        ));
        form.active_mode = "invalid".into();
        assert!(form.to_settings().is_err());
    }
}
