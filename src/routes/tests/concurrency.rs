use super::*;

async fn action(
    state: &Arc<AppState>,
    path: &str,
    generation: u64,
    fields: &[(&str, &str)],
) -> Response {
    let mut form = vec![
        ("csrf_token", state.csrf_token().to_owned()),
        ("queue_generation", generation.to_string()),
    ];
    form.extend(
        fields
            .iter()
            .map(|(key, value)| (*key, (*value).to_owned())),
    );
    router(state.clone())
        .oneshot(
            Request::post(path)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(serde_urlencoded::to_string(form).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

fn ids(snapshot: &crate::queue::QueueSnapshot) -> Vec<String> {
    snapshot
        .entries
        .iter()
        .map(|entry| entry.id.clone())
        .collect()
}

async fn displayed(state: &Arc<AppState>) -> crate::queue::QueueSnapshot {
    let response = router(state.clone())
        .oneshot(Request::get("/settings").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let html = String::from_utf8(bytes(response).await).unwrap();
    assert!(html.contains("name=\"original_order\""));
    assert!(html.contains("data-entry-version="));
    state.queue.snapshot().await
}

#[tokio::test]
async fn delete_refill_delete_reorder_delete_and_repeat_without_reload() {
    let (directory, state) = manual_state(AdvancementSettings::Manual).await;
    let first = displayed(&state).await;
    assert_eq!(
        action(
            &state,
            "/queue/remove",
            first.generation,
            &[("id", &first.entries[0].id)]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    // Redirected page renders BEFORE the supplier finishes, just as in the bug.
    let page = displayed(&state).await;
    fs::write(directory.path().join("source/d.jpg"), A).unwrap();
    refill::refill_once(&state).await.unwrap();
    assert!(state.queue.snapshot().await.generation > page.generation);
    assert_eq!(
        action(
            &state,
            "/queue/remove",
            page.generation,
            &[("id", &page.entries[0].id)]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let page = displayed(&state).await;
    fs::write(directory.path().join("source/e.jpg"), B).unwrap();
    refill::refill_once(&state).await.unwrap();
    let appended = state
        .queue
        .snapshot()
        .await
        .entries
        .last()
        .unwrap()
        .id
        .clone();
    let baseline = ids(&page);
    let reversed = baseline.iter().rev().cloned().collect::<Vec<_>>();
    assert_eq!(
        action(
            &state,
            "/queue/reorder",
            page.generation,
            &[
                ("original_order", &baseline.join("\n")),
                ("order", &reversed.join("\n")),
            ]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        ids(&state.queue.snapshot().await),
        [reversed, vec![appended]].concat()
    );
    // Even the pre-reorder page's deletion is still unambiguous, and repeatable.
    for _ in 0..2 {
        assert_eq!(
            action(
                &state,
                "/queue/remove",
                page.generation,
                &[("id", &baseline[0])]
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
    }
    let page = displayed(&state).await;
    let order = ids(&page).into_iter().rev().collect::<Vec<_>>().join("\n");
    assert_eq!(
        action(
            &state,
            "/queue/reorder",
            page.generation,
            &[
                ("original_order", &ids(&page).join("\n")),
                ("order", &order)
            ]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn competing_reorder_advance_and_removal_require_review_without_mutation() {
    for change in ["reorder", "advance", "remove", "front"] {
        let (_directory, state) = manual_state(AdvancementSettings::Manual).await;
        let page = state.queue.snapshot().await;
        let baseline = ids(&page);
        match change {
            "reorder" => {
                state
                    .queue
                    .reorder(
                        page.generation,
                        vec![
                            baseline[0].clone(),
                            baseline[2].clone(),
                            baseline[1].clone(),
                        ],
                    )
                    .await
                    .unwrap();
            }
            "advance" => {
                state
                    .queue
                    .advance(QueueAdvanceBehavior::Rotate)
                    .await
                    .unwrap();
            }
            "remove" => {
                state.queue.remove_by_id(&baseline[1]).await.unwrap();
            }
            "front" => {
                let request = imports::prepare_photo(
                    "new.jpg".into(),
                    A.to_vec(),
                    RenderingSettings::default(),
                )
                .unwrap();
                state
                    .queue
                    .enqueue_rendered_batch_at(
                        None,
                        None,
                        QueueInsertPosition::Front,
                        vec![request],
                    )
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let before = state.queue.snapshot().await;
        for path in ["/queue/reorder", "/queue/advance"] {
            let response = action(
                &state,
                path,
                page.generation,
                &[
                    ("original_order", &baseline.join("\n")),
                    (
                        "order",
                        &baseline
                            .iter()
                            .rev()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                ],
            )
            .await;
            assert_eq!(response.status(), StatusCode::CONFLICT, "{change} {path}");
            let html = String::from_utf8(bytes(response).await).unwrap();
            assert!(html.contains("Action not applied:"));
            assert!(html.contains("id=\"queue-panel\""));
            assert_eq!(state.queue.snapshot().await.entries, before.entries);
            assert_eq!(state.queue.snapshot().await.generation, before.generation);
        }
    }
}

#[tokio::test]
async fn edits_are_entry_scoped_and_competing_edit_is_not_overwritten() {
    let (directory, state) = manual_state(AdvancementSettings::Manual).await;
    let page = state.queue.snapshot().await;
    let photo = &page.entries[1];
    let version = photo.version();
    state.queue.remove_by_id(&page.entries[0].id).await.unwrap();
    fs::write(directory.path().join("source/d.jpg"), A).unwrap();
    refill::refill_once(&state).await.unwrap();
    let fields = [
        ("id", photo.id.as_str()),
        ("entry_version", version.as_str()),
        ("orientation", "portrait"),
        ("body", ""),
        ("horizontal", "middle"),
        ("vertical", "bottom"),
        ("background", "black"),
        ("size", "large"),
    ];
    assert_eq!(
        action(&state, "/queue/edit", page.generation, &fields)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    let edited = state.queue.snapshot().await;
    assert_eq!(
        edited.entries[0].orientation,
        crate::renderer::ImageOrientation::Portrait
    );
    // A reorder based on the original IDs survives an unrelated entry edit.
    let original = ids(&edited).join("\n");
    let reversed = ids(&edited)
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        action(
            &state,
            "/queue/reorder",
            page.generation,
            &[("original_order", &original), ("order", &reversed)]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let before = state.queue.snapshot().await;
    assert_eq!(
        action(&state, "/queue/edit", page.generation, &fields)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(state.queue.snapshot().await.entries, before.entries);
    // Fresh entry version remains valid across reorder and library-only mutation.
    let current = edited
        .entries
        .iter()
        .find(|entry| entry.id == photo.id)
        .unwrap();
    state
        .queue
        .favorite_queue_entry(EntryExpectation::Version(current.version()), &current.id)
        .await
        .unwrap();
    assert_eq!(
        action(
            &state,
            "/queue/orientation",
            page.generation,
            &[
                ("id", &current.id),
                ("entry_version", &current.version()),
                ("orientation", "landscape"),
            ]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        action(
            &state,
            "/queue/remove",
            page.generation,
            &[("id", &photo.id)]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn advance_tolerates_appends_but_never_replays_after_current_changes() {
    let (_directory, state) = manual_state(AdvancementSettings::Manual).await;
    let page = state.queue.snapshot().await;
    let request =
        imports::prepare_photo("new.jpg".into(), A.to_vec(), RenderingSettings::default()).unwrap();
    state
        .queue
        .enqueue_rendered_batch(None, None, vec![request])
        .await
        .unwrap();
    let original = ids(&page).join("\n");
    assert_eq!(
        action(
            &state,
            "/queue/advance",
            page.generation,
            &[("original_order", &original)]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let advanced = state.queue.snapshot().await;
    assert_eq!(advanced.entries[0].id, page.entries[1].id);
    assert_eq!(
        action(
            &state,
            "/queue/advance",
            page.generation,
            &[("original_order", &original)]
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(state.queue.snapshot().await.entries, advanced.entries);
    assert_eq!(
        action(
            &state,
            "/queue/remove",
            page.generation,
            &[("id", &page.entries[2].id)]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn stale_additions_preserve_position_and_fresh_actions_work() {
    let (_directory, state) = manual_state(AdvancementSettings::Manual).await;
    let page = state.queue.snapshot().await;
    let saved = state
        .queue
        .favorite_queue_entry(page.generation, &page.entries[0].id)
        .await
        .unwrap();
    state.queue.remove_by_id(&page.entries[1].id).await.unwrap();
    for position in ["front", "back"] {
        assert_eq!(
            action(
                &state,
                "/queue/from-saved",
                page.generation,
                &[
                    ("source", "favorites"),
                    ("id", &saved),
                    ("position", position),
                ]
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
        let snapshot = state.queue.snapshot().await;
        let added = if position == "front" {
            &snapshot.entries[0]
        } else {
            snapshot.entries.last().unwrap()
        };
        assert_eq!(added.display_name, page.entries[0].display_name);
        assert_ne!(added.id, page.entries[0].id);
        assert_eq!(
            action(
                &state,
                "/queue/remove",
                page.generation,
                &[("id", &added.id)]
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
    }
    assert_eq!(
        action(
            &state,
            "/queue/message",
            page.generation,
            &[
                ("body", "Message after queue changes"),
                ("orientation", "landscape"),
                ("horizontal", "middle"),
                ("vertical", "bottom"),
                ("background", "black"),
                ("size", "large"),
                ("position", "back"),
            ]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    let message = state.queue.snapshot().await.entries.last().unwrap().clone();
    assert!(message.dedicated_text);
    assert_eq!(
        action(
            &state,
            "/queue/remove",
            page.generation,
            &[("id", &message.id)]
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn uploads_accept_stale_pages_and_preserve_front_back_position() {
    let (_directory, state) = manual_state(AdvancementSettings::Manual).await;
    let page = state.queue.snapshot().await;
    state.queue.remove_by_id(&page.entries[1].id).await.unwrap();
    for position in ["front", "back"] {
        let boundary = "stale-queue-upload";
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"csrf_token\"\r\n\r\n{}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"queue_generation\"\r\n\r\n{}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"position\"\r\n\r\n{position}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"images\"; filename=\"upload.png\"\r\nContent-Type: image/png\r\n\r\n",
            state.csrf_token(), page.generation,
        ).into_bytes();
        body.extend_from_slice(&tiny_png());
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let response = router(state.clone())
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
        let snapshot = state.queue.snapshot().await;
        let added = if position == "front" {
            &snapshot.entries[0]
        } else {
            snapshot.entries.last().unwrap()
        };
        assert_eq!(added.display_name, "upload.png");
        assert_eq!(snapshot.entries.len(), 3);
        // Upload -> reorder on a fresh view, followed by a stale-page deletion.
        let original = ids(&snapshot);
        let reversed = original.iter().rev().cloned().collect::<Vec<_>>();
        assert_eq!(
            action(
                &state,
                "/queue/reorder",
                snapshot.generation,
                &[
                    ("original_order", &original.join("\n")),
                    ("order", &reversed.join("\n")),
                ]
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
        assert_eq!(
            action(
                &state,
                "/queue/remove",
                page.generation,
                &[("id", &added.id)]
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
    }
}

#[tokio::test]
async fn malformed_reorders_and_bad_csrf_do_not_mutate_queue() {
    let (_directory, state) = manual_state(AdvancementSettings::Manual).await;
    let page = state.queue.snapshot().await;
    for order in [
        page.entries[0].id.clone(),
        "unknown".into(),
        vec![page.entries[0].id.clone(); 3].join("\n"),
    ] {
        assert_eq!(
            action(
                &state,
                "/queue/reorder",
                page.generation,
                &[
                    ("original_order", &ids(&page).join("\n")),
                    ("order", &order),
                ]
            )
            .await
            .status(),
            StatusCode::CONFLICT
        );
    }
    for path in ["/queue/remove", "/queue/reorder", "/queue/advance"] {
        let form = serde_urlencoded::to_string([
            ("csrf_token", "invalid"),
            ("queue_generation", "0"),
            ("id", &page.entries[0].id),
            ("original_order", ""),
            ("order", ""),
        ])
        .unwrap();
        let response = router(state.clone())
            .oneshot(
                Request::post(path)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(form))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    assert_eq!(state.queue.snapshot().await.entries, page.entries);
    assert_eq!(state.queue.snapshot().await.generation, page.generation);
}
