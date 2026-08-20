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
    extract::{ConnectInfo, DefaultBodyLimit, Form, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tower_http::trace::{DefaultMakeSpan, DefaultOnFailure, DefaultOnResponse, TraceLayer};
use tracing::Level;
use tracing::{error, info};

use crate::{
    catalog,
    config::{PathMapping, ReplaceSettingsError, SelectionMode, SelectionSettings, Settings},
    source::SourceStore,
    state::{AppState, ImageRequest},
};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/settings", get(settings_page).post(update_settings))
        .route("/frame/current.jpg", get(get_image).head(head_image))
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO))
                .on_failure(DefaultOnFailure::new().level(Level::ERROR)),
        )
        .with_state(state)
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    generation: u64,
    csrf_token: String,
    database_path: String,
    query: String,
    path_mappings: String,
    mode_first: bool,
    mode_daily_random: bool,
    random_seed: String,
    timezone: String,
    rollover_time: String,
    max_candidates: String,
    cache_ready: bool,
    last_request: String,
    selector_status: String,
    error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SettingsForm {
    generation: String,
    csrf_token: String,
    database_path: String,
    query: String,
    path_mappings: String,
    selection_mode: String,
    random_seed: String,
    timezone: String,
    rollover_time: String,
    max_candidates: String,
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
    let expected_generation = match form.generation.parse::<u64>() {
        Ok(generation) => generation,
        Err(_) => {
            return render_form_error(
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
        Err(error) => {
            return render_form_error(&state, form, StatusCode::UNPROCESSABLE_ENTITY, &error).await;
        }
    };
    if let Err(error) =
        validate_proposed_settings(proposed.clone(), state.cache.root().to_path_buf()).await
    {
        return render_form_error(
            &state,
            form,
            StatusCode::UNPROCESSABLE_ENTITY,
            &error.to_string(),
        )
        .await;
    }

    match state.settings.replace(expected_generation, proposed).await {
        Ok(_) => {
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
        Err(ReplaceSettingsError::Invalid(error)) => {
            render_form_error(&state, form, StatusCode::UNPROCESSABLE_ENTITY, &error).await
        }
        Err(ReplaceSettingsError::Persistence(error)) => {
            render_form_error(
                &state,
                form,
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Failed to persist settings: {error}"),
            )
            .await
        }
    }
}

async fn validate_proposed_settings(
    settings: Settings,
    cache_root: std::path::PathBuf,
) -> anyhow::Result<()> {
    settings.validate_basic()?;
    catalog::validate(settings.clone()).await?;
    tokio::task::spawn_blocking(move || SourceStore::new(&settings, &cache_root).map(|_| ()))
        .await
        .map_err(|error| anyhow::anyhow!("settings validation worker stopped: {error}"))??;
    Ok(())
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
    let selector_status = format_selector_status(&state.selector_status().await);
    let template = SettingsTemplate {
        generation,
        csrf_token: state.csrf_token().to_owned(),
        database_path: form.database_path,
        query: form.query,
        path_mappings: form.path_mappings,
        mode_first: form.selection_mode == "first",
        mode_daily_random: form.selection_mode == "daily_random",
        random_seed: form.random_seed,
        timezone: form.timezone,
        rollover_time: form.rollover_time,
        max_candidates: form.max_candidates,
        cache_ready: state.cache.is_ready(),
        last_request,
        selector_status,
        error,
    };
    match template.render() {
        Ok(html) => (status, Html(html)).into_response(),
        Err(error) => {
            tracing::error!(%error, "failed to render settings page");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to render settings page\n",
            )
                .into_response()
        }
    }
}

fn format_selector_status(status: &crate::state::SelectorStatus) -> String {
    let mut parts = Vec::new();
    if status.running {
        parts.push("Selection is running.".to_owned());
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
        "No selection has completed since the server started.".to_owned()
    } else {
        parts.join("\n")
    }
}

impl SettingsForm {
    fn from_settings(generation: u64, csrf_token: &str, settings: &Settings) -> Self {
        let path_mappings = settings
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
        Self {
            generation: generation.to_string(),
            csrf_token: csrf_token.to_owned(),
            database_path: settings.database_path.display().to_string(),
            query: settings.query.clone(),
            path_mappings,
            selection_mode: match settings.selection.mode {
                SelectionMode::First => "first",
                SelectionMode::DailyRandom => "daily_random",
            }
            .to_owned(),
            random_seed: settings.selection.random_seed.clone(),
            timezone: settings.selection.timezone.clone(),
            rollover_time: settings.selection.rollover_time.clone(),
            max_candidates: settings.max_candidates.to_string(),
        }
    }

    fn to_settings(&self) -> Result<Settings, String> {
        let mode = match self.selection_mode.as_str() {
            "first" => SelectionMode::First,
            "daily_random" => SelectionMode::DailyRandom,
            _ => return Err("Selection mode must be first or daily_random".into()),
        };
        let max_candidates = self
            .max_candidates
            .parse::<usize>()
            .map_err(|_| "Maximum candidates must be an integer".to_owned())?;
        let path_mappings = parse_path_mappings(&self.path_mappings)?;
        Ok(Settings {
            database_path: self.database_path.trim().into(),
            query: self.query.clone(),
            path_mappings,
            selection: SelectionSettings {
                mode,
                random_seed: self.random_seed.clone(),
                timezone: self.timezone.trim().to_owned(),
                rollover_time: self.rollover_time.trim().to_owned(),
            },
            max_candidates,
        })
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

async fn health_live() -> StatusCode {
    StatusCode::OK
}

async fn health_ready(State(state): State<Arc<AppState>>) -> StatusCode {
    if state.cache.is_ready() {
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
    let mut file = match tokio::fs::File::open(state.cache.current_path()).await {
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
    use rusqlite::Connection;
    use tempfile::{TempDir, tempdir};
    use tokio::time::{Duration, timeout};
    use tower::ServiceExt;

    use crate::{
        cache::CacheStore,
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
                .filter(|parent| !parent.as_os_str().is_empty())
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
        let config = test_config(image_path);
        let cache = CacheStore::new(&config.cache_dir).unwrap();
        AppState::new(config, SettingsManager::in_memory(test_settings()), cache)
    }

    fn ui_state() -> (TempDir, Arc<AppState>) {
        let directory = tempdir().unwrap();
        let photos = directory.path().join("photos");
        let cache_dir = directory.path().join("cache");
        fs::create_dir(&photos).unwrap();
        fs::create_dir(&cache_dir).unwrap();
        fs::write(photos.join("one.jpg"), b"\xff\xd8\xffone\xff\xd9").unwrap();
        let database_path = directory.path().join("digikam.db");
        Connection::open(&database_path).unwrap();
        let settings_path = directory.path().join("settings.toml");
        let settings = Settings {
            database_path,
            query: "SELECT 1 AS image_id, '/library' AS root_path, '/' AS album_path, 'one.jpg' AS file_name".into(),
            path_mappings: vec![PathMapping {
                database_root: PathBuf::from("/library"),
                mounted_root: photos,
            }],
            selection: SelectionSettings {
                mode: SelectionMode::First,
                random_seed: String::new(),
                timezone: "UTC".into(),
                rollover_time: "00:00".into(),
            },
            max_candidates: 100,
        };
        fs::write(&settings_path, toml::to_string_pretty(&settings).unwrap()).unwrap();
        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            settings_path: settings_path.clone(),
            cache_dir: cache_dir.clone(),
        };
        let state = AppState::new(
            config,
            SettingsManager::load(settings_path).unwrap(),
            CacheStore::new(&cache_dir).unwrap(),
        );
        (directory, state)
    }

    async fn current_form(state: &Arc<AppState>) -> SettingsForm {
        let snapshot = state.settings.snapshot().await;
        SettingsForm::from_settings(snapshot.generation, state.csrf_token(), &snapshot.settings)
    }

    fn post_settings(form: &SettingsForm) -> Request<Body> {
        Request::post("/settings")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(serde_urlencoded::to_string(form).unwrap()))
            .unwrap()
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
        assert!(body.contains("Selection settings"));
        assert!(body.contains("No image GET has occurred"));
    }

    #[tokio::test]
    async fn settings_reports_latest_request() {
        let config = test_config("unused.jpg");
        let state = AppState::with_last_image_request(
            config.clone(),
            SettingsManager::in_memory(test_settings()),
            CacheStore::new(&config.cache_dir).unwrap(),
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
        assert!(body.contains("No selection has completed"));
    }

    #[tokio::test]
    async fn settings_page_escapes_configured_values() {
        let directory = tempdir().unwrap();
        let cache_dir = directory.path().join("cache");
        fs::create_dir(&cache_dir).unwrap();
        let mut settings = test_settings();
        settings.query = "<script>alert('x')</script>".into();
        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            settings_path: directory.path().join("settings.toml"),
            cache_dir: cache_dir.clone(),
        };
        let state = AppState::new(
            config,
            SettingsManager::in_memory(settings),
            CacheStore::new(&cache_dir).unwrap(),
        );

        let response = router(state)
            .oneshot(Request::get("/settings").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_text(response).await;

        assert!(body.contains("&#60;script&#62;"));
        assert!(!body.contains("<script>alert"));
    }

    #[tokio::test]
    async fn valid_settings_post_persists_notifies_and_survives_reload() {
        let (_directory, state) = ui_state();
        let mut triggers = state.subscribe_to_selection_triggers();
        let mut form = current_form(&state).await;
        form.query = "SELECT 2 AS image_id, '/library' AS root_path, '/' AS album_path, 'one.jpg' AS file_name".into();
        form.selection_mode = "daily_random".into();
        form.random_seed = "new-seed".into();

        let response = router(state.clone())
            .oneshot(post_settings(&form))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        timeout(Duration::from_secs(1), triggers.changed())
            .await
            .unwrap()
            .unwrap();
        let persisted = Settings::load(&state.config.settings_path).unwrap();
        assert_eq!(persisted.query, form.query);
        assert_eq!(persisted.selection.mode, SelectionMode::DailyRandom);
        assert_eq!(persisted.selection.random_seed, "new-seed");
        let reloaded = SettingsManager::load(state.config.settings_path.clone()).unwrap();
        assert_eq!(reloaded.snapshot().await.settings, persisted);
    }

    #[tokio::test]
    async fn invalid_csrf_sql_path_and_stale_generation_do_not_change_settings() {
        let (_directory, state) = ui_state();
        let original_bytes = fs::read(&state.config.settings_path).unwrap();

        let mut csrf = current_form(&state).await;
        csrf.csrf_token = "wrong".into();
        let response = router(state.clone())
            .oneshot(post_settings(&csrf))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            fs::read(&state.config.settings_path).unwrap(),
            original_bytes
        );

        let mut mutating = current_form(&state).await;
        mutating.query = "DELETE FROM Images".into();
        let response = router(state.clone())
            .oneshot(post_settings(&mutating))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            fs::read(&state.config.settings_path).unwrap(),
            original_bytes
        );

        let mut escaped = current_form(&state).await;
        escaped.path_mappings = format!("/library => {}", state.cache.root().display());
        let response = router(state.clone())
            .oneshot(post_settings(&escaped))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            fs::read(&state.config.settings_path).unwrap(),
            original_bytes
        );

        let mut stale = current_form(&state).await;
        stale.generation = "99".into();
        let response = router(state.clone())
            .oneshot(post_settings(&stale))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            fs::read(&state.config.settings_path).unwrap(),
            original_bytes
        );
        assert_eq!(state.settings.snapshot().await.generation, 1);
    }

    #[tokio::test]
    async fn concurrent_settings_posts_allow_only_one_writer() {
        let (_directory, state) = ui_state();
        let mut first = current_form(&state).await;
        first.random_seed = "first".into();
        let mut second = first.clone();
        second.random_seed = "second".into();
        let app = router(state.clone());

        let (first_response, second_response) = tokio::join!(
            app.clone().oneshot(post_settings(&first)),
            app.oneshot(post_settings(&second))
        );
        let statuses = [
            first_response.unwrap().status(),
            second_response.unwrap().status(),
        ];

        assert!(statuses.contains(&StatusCode::SEE_OTHER));
        assert!(statuses.contains(&StatusCode::CONFLICT));
        assert_eq!(state.settings.snapshot().await.generation, 2);
    }

    #[tokio::test]
    async fn oversized_settings_post_is_rejected() {
        let (_directory, state) = ui_state();
        let body = format!(
            "generation=1&csrf_token={}&query={}",
            state.csrf_token(),
            "a".repeat(70 * 1024)
        );

        let response = router(state)
            .oneshot(
                Request::post("/settings")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn form_validation_reports_invalid_fields() {
        let mut form = SettingsForm::from_settings(1, "token", &test_settings());
        form.selection_mode = "surprise".into();
        assert!(form.to_settings().unwrap_err().contains("Selection mode"));
        form.selection_mode = "first".into();
        form.max_candidates = "not-a-number".into();
        assert!(
            form.to_settings()
                .unwrap_err()
                .contains("Maximum candidates")
        );
        form.max_candidates = "10".into();
        form.path_mappings = "invalid mapping".into();
        assert!(form.to_settings().unwrap_err().contains("path mapping"));
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
        let config = test_config(image_path);
        let state = AppState::with_last_image_request(
            config.clone(),
            SettingsManager::in_memory(test_settings()),
            CacheStore::new(&config.cache_dir).unwrap(),
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
    async fn health_reports_liveness_and_cache_readiness() {
        let directory = tempdir().unwrap();
        let image_path = directory.path().join("current.jpg");
        let state = test_state(&image_path);
        let app = router(state);

        let live = app
            .clone()
            .oneshot(Request::get("/health/live").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let not_ready = app
            .clone()
            .oneshot(Request::get("/health/ready").body(Body::empty()).unwrap())
            .await
            .unwrap();
        write_image(&image_path);
        let ready = app
            .oneshot(Request::get("/health/ready").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(live.status(), StatusCode::OK);
        assert_eq!(not_ready.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(ready.status(), StatusCode::OK);
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
