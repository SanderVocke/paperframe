use super::*;

#[derive(Default, Deserialize)]
pub(super) struct PageQuery {
    #[serde(default = "first_page")]
    page: usize,
}
fn first_page() -> usize {
    1
}

struct SavedCard {
    id: String,
    display_name: String,
    timestamp: String,
    favorite: bool,
    dithered: bool,
    dedicated: bool,
}

#[derive(Template)]
#[template(path = "library.html")]
struct LibraryTemplate {
    title: &'static str,
    description: &'static str,
    path: &'static str,
    favorites: bool,
    csrf_token: String,
    generation: u64,
    items: Vec<SavedCard>,
    page: usize,
    previous: usize,
    next: usize,
    pages: usize,
    roots: Vec<RootChoice>,
    settings_generation: u64,
}

struct RootChoice {
    index: usize,
    label: String,
}
pub(super) fn source_roots(settings: &Settings) -> Vec<(String, std::path::PathBuf)> {
    settings
        .suppliers
        .iter()
        .flat_map(|supplier| match &supplier.kind {
            SupplierKind::ManualFolder(folder) => vec![(
                format!("{} · source folder", supplier.id),
                folder.directory.clone(),
            )],
            SupplierKind::DigikamSqlite(digikam) => digikam
                .path_mappings
                .iter()
                .map(|mapping| {
                    (
                        format!("{} · {}", supplier.id, mapping.database_root.display()),
                        mapping.mounted_root.clone(),
                    )
                })
                .collect(),
        })
        .collect()
}

pub(super) async fn history_page(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PageQuery>,
) -> Response {
    render_library(&state, query.page, false).await
}
pub(super) async fn favorites_page(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PageQuery>,
) -> Response {
    render_library(&state, query.page, true).await
}
async fn render_library(state: &Arc<AppState>, page: usize, favorites: bool) -> Response {
    let snapshot = match state.queue.library_page(favorites, page, 24).await {
        Ok(snapshot) => snapshot,
        Err(error) => return plain(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let items = snapshot
        .items
        .into_iter()
        .map(|item| SavedCard {
            id: item.picture_id,
            display_name: item.entry.display_name,
            timestamp: chrono::DateTime::from_timestamp(item.timestamp, 0)
                .map(|date| date.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_default(),
            favorite: item.favorite,
            dithered: item.entry.media_type == "image/bmp",
            dedicated: item.entry.dedicated_text,
        })
        .collect();
    let settings = state.settings.snapshot().await;
    let roots = source_roots(&settings.settings)
        .into_iter()
        .enumerate()
        .map(|(index, (label, _))| RootChoice { index, label })
        .collect();
    let template = LibraryTemplate {
        title: if favorites {
            "Favorites"
        } else {
            "Recent history"
        },
        description: if favorites {
            "Saved until you remove them, independently of the queue and source files."
        } else {
            "Pictures recently in the current queue slot—not confirmed physical frame deliveries."
        },
        path: if favorites { "/favorites" } else { "/history" },
        favorites,
        csrf_token: state.csrf_token().into(),
        generation: snapshot.generation,
        items,
        page,
        previous: page.saturating_sub(1),
        next: page.saturating_add(1),
        pages: snapshot.pages,
        roots,
        settings_generation: settings.generation,
    };
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(error) => plain(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

#[derive(Deserialize)]
pub(super) struct FavoriteForm {
    csrf_token: String,
    queue_generation: String,
    source: String,
    id: String,
}
pub(super) async fn add_favorite(
    State(state): State<Arc<AppState>>,
    Form(form): Form<FavoriteForm>,
) -> Response {
    let generation = match validate_action(&state, &form.csrf_token, &form.queue_generation) {
        Ok(generation) => generation,
        Err((status, message)) => return plain(status, &message),
    };
    let (result, redirect) = match form.source.as_str() {
        "queue" => (
            state
                .queue
                .favorite_queue_entry(generation, &form.id)
                .await
                .map(|_| ()),
            "/settings",
        ),
        "history" => (
            state
                .queue
                .favorite_history_entry(generation, &form.id)
                .await,
            "/history",
        ),
        _ => return plain(StatusCode::UNPROCESSABLE_ENTITY, "Unknown favorite source"),
    };
    match result {
        Ok(()) => Redirect::to(redirect).into_response(),
        Err(error) => plain(
            StatusCode::CONFLICT,
            &format!("Could not save favorite; reload and retry: {error}"),
        ),
    }
}
pub(super) async fn remove_favorite(
    State(state): State<Arc<AppState>>,
    Form(form): Form<RemoveForm>,
) -> Response {
    let generation = match validate_action(&state, &form.csrf_token, &form.queue_generation) {
        Ok(generation) => generation,
        Err((status, message)) => return plain(status, &message),
    };
    match state.queue.remove_favorite(generation, &form.id).await {
        Ok(()) => Redirect::to("/favorites").into_response(),
        Err(error) => plain(
            StatusCode::CONFLICT,
            &format!("Could not remove favorite; reload and retry: {error}"),
        ),
    }
}
#[derive(Deserialize)]
pub(super) struct SavedQueueForm {
    csrf_token: String,
    queue_generation: String,
    source: String,
    id: String,
    position: String,
}
pub(super) async fn enqueue_saved(
    State(state): State<Arc<AppState>>,
    Form(form): Form<SavedQueueForm>,
) -> Response {
    let generation = match validate_action(&state, &form.csrf_token, &form.queue_generation) {
        Ok(generation) => generation,
        Err((status, message)) => return plain(status, &message),
    };
    let favorites = match form.source.as_str() {
        "favorites" => true,
        "history" => false,
        _ => {
            return plain(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Unknown saved picture source",
            );
        }
    };
    let position = match form.position.as_str() {
        "front" => QueueInsertPosition::Front,
        "back" => QueueInsertPosition::Back,
        _ => return plain(StatusCode::UNPROCESSABLE_ENTITY, "Choose front or back"),
    };
    if !state.queue.saved_in_collection(&form.id, favorites).await {
        return plain(
            StatusCode::NOT_FOUND,
            "Saved picture not found in this collection",
        );
    }
    let _transition = state.rendering_transition.read().await;
    let rendering = state.settings.snapshot().await.settings.rendering;
    let _permit = match state.render_serialization.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(error) => return plain(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
    };
    match state
        .queue
        .enqueue_saved(
            generation,
            &form.id,
            position,
            rendering.target,
            rendering.output_mode,
        )
        .await
    {
        Ok(_) => Redirect::to("/settings").into_response(),
        Err(error) => plain(
            StatusCode::CONFLICT,
            &format!("Could not enqueue saved picture; reload and retry: {error}"),
        ),
    }
}
