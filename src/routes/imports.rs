use super::*;

pub(super) fn prepare_photo(
    display_name: String,
    bytes: Vec<u8>,
    rendering: RenderingSettings,
) -> anyhow::Result<crate::queue::RenderedEnqueueRequest> {
    let decoded = crate::renderer::decode_source_named(&bytes, Some(&display_name))?;
    let thumbnail = crate::renderer::thumbnail_jpeg(&decoded)?;
    let rendered = crate::renderer::render_with_original(
        &decoded,
        &bytes,
        rendering.target,
        rendering.output_mode,
        decoded.orientation,
    )?;
    Ok(crate::queue::RenderedEnqueueRequest {
        display_name,
        source_bytes: bytes,
        derivative_bytes: rendered.bytes,
        thumbnail_bytes: thumbnail,
        media_type: rendered.media_type.into(),
        source_width: decoded.pixels.width(),
        source_height: decoded.pixels.height(),
        orientation: decoded.orientation,
        frame_target: rendering.target,
        text_overlay: None,
        dedicated_text: false,
        supplier_id: None,
        source_key: None,
    })
}

#[derive(Deserialize)]
pub(super) struct PathImportForm {
    csrf_token: String,
    queue_generation: String,
    settings_generation: u64,
    root: usize,
    path: String,
}
pub(super) async fn import_path(
    State(state): State<Arc<AppState>>,
    Form(form): Form<PathImportForm>,
) -> Response {
    let generation = match validate_action(&state, &form.csrf_token, &form.queue_generation) {
        Ok(generation) => generation,
        Err((status, message)) => return plain(status, &message),
    };
    let _transition = state.rendering_transition.read().await;
    let settings = state.settings.snapshot().await;
    if settings.generation != form.settings_generation {
        return plain(
            StatusCode::CONFLICT,
            "Source settings changed; reload and retry",
        );
    }
    let roots = super::library::source_roots(&settings.settings);
    let Some((_, root)) = roots.get(form.root).cloned() else {
        return plain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Choose a configured source root",
        );
    };
    let _permit = match state.render_serialization.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(error) => return plain(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
    };
    let limit = settings.settings.queue.max_upload_bytes;
    let rendering = settings.settings.rendering;
    let request = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let path = std::path::Path::new(&form.path);
        let bytes = crate::source::read_relative_picture(&root, path, limit)?;
        let display_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(safe_display_name)
            .ok_or_else(|| anyhow::anyhow!("Invalid picture filename"))?;
        prepare_photo(display_name, bytes, rendering)
    })
    .await;
    publish_import(&state, generation, request).await
}

pub(super) async fn import_upload(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Response {
    let _transition = state.rendering_transition.read().await;
    let settings = state.settings.snapshot().await;
    let mut csrf = None;
    let mut generation = None;
    let mut settings_generation = None;
    let mut upload = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => return plain(StatusCode::BAD_REQUEST, &error.to_string()),
        };
        match field.name().unwrap_or("") {
            "csrf_token" => csrf = field.text().await.ok(),
            "queue_generation" => generation = field.text().await.ok(),
            "settings_generation" => settings_generation = field.text().await.ok(),
            "image" => {
                if upload.is_some() {
                    return plain(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "Upload one picture at a time",
                    );
                }
                let filename = field
                    .file_name()
                    .map(safe_display_name)
                    .unwrap_or_else(|| "upload".into());
                let bytes = match field.bytes().await {
                    Ok(bytes) => bytes,
                    Err(error) => return plain(StatusCode::BAD_REQUEST, &error.to_string()),
                };
                if bytes.len() as u64
                    > settings
                        .settings
                        .queue
                        .max_upload_bytes
                        .min(100 * 1024 * 1024)
                {
                    return plain(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "Picture exceeds configured upload limit",
                    );
                }
                upload = Some((filename, bytes.to_vec()));
            }
            _ => {}
        }
    }
    let generation = match validate_action(
        &state,
        &csrf.unwrap_or_default(),
        &generation.unwrap_or_default(),
    ) {
        Ok(generation) => generation,
        Err((status, message)) => return plain(status, &message),
    };
    if settings_generation.and_then(|value| value.parse::<u64>().ok()) != Some(settings.generation)
    {
        return plain(StatusCode::CONFLICT, "Settings changed; reload and retry");
    }
    let Some((filename, bytes)) = upload else {
        return plain(StatusCode::UNPROCESSABLE_ENTITY, "Choose one picture");
    };
    let _permit = match state.render_serialization.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(error) => return plain(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
    };
    let rendering = settings.settings.rendering;
    let request =
        tokio::task::spawn_blocking(move || prepare_photo(filename, bytes, rendering)).await;
    publish_import(&state, generation, request).await
}
async fn publish_import(
    state: &Arc<AppState>,
    generation: u64,
    request: Result<anyhow::Result<crate::queue::RenderedEnqueueRequest>, tokio::task::JoinError>,
) -> Response {
    let request = match request {
        Ok(Ok(request)) => request,
        Ok(Err(error)) => return plain(StatusCode::UNPROCESSABLE_ENTITY, &error.to_string()),
        Err(error) => return plain(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    match state.queue.import_favorite(generation, request).await {
        Ok(_) => Redirect::to("/favorites").into_response(),
        Err(error) => plain(
            StatusCode::CONFLICT,
            &format!("Could not import favorite; reload and retry: {error}"),
        ),
    }
}
