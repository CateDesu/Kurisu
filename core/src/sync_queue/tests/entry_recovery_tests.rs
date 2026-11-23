use super::*;

fn mutation(entry: serde_json::Value) -> (u16, serde_json::Value) {
    (200, json!({"data":{"SaveMediaListEntry":entry}}))
}

fn lookup(total: Option<i64>, entry: serde_json::Value) -> (u16, serde_json::Value) {
    (
        200,
        json!({"data":{"Media":{"id":1,"episodes":total,"mediaListEntry":entry}}}),
    )
}

fn entry(status: &str, progress: i64) -> serde_json::Value {
    let mut entry = remote(progress);
    entry["status"] = json!(status);
    entry
}

#[tokio::test]
async fn queued_rewatch_treats_explicit_null_differently_from_an_omitted_total() {
    for total_cleared in [false, true] {
        let mut base = entry("REPEATING", 11);
        base["repeat"] = json!(2);
        let mut reply = lookup(None, base.clone());
        if !total_cleared {
            reply.1["data"]["Media"]
                .as_object_mut()
                .unwrap()
                .remove("episodes");
        }
        let mut saved = if total_cleared {
            entry("REPEATING", 12)
        } else {
            entry("COMPLETED", 12)
        };
        let repeat = if total_cleared { 2 } else { 3 };
        saved["repeat"] = json!(repeat);
        let (api, mut requests) = anilist::mock_api(vec![
            (503, json!({"errors":[{"message":"Offline"}]})),
            reply,
            mutation(saved),
        ])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let mut cached = remote_entry(1, serde_json::from_value(base).unwrap());
        cached.media = state.db.get_media(1).unwrap();
        state.db.upsert_entry(&cached).unwrap();
        let local = crate::commands::watcher_set_progress(&state, 1, 12, Some("test-session"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!((local.status.as_str(), local.repeat), ("COMPLETED", 3));

        assert!(sync_pending_changes(&state).await.unwrap().is_empty());
        assert_eq!(
            state.db.episode_total(1).unwrap(),
            (!total_cleared).then_some(12)
        );
        let saved = state.db.get_entry(1).unwrap().unwrap();
        let status = if total_cleared {
            "REPEATING"
        } else {
            "COMPLETED"
        };
        assert_eq!(
            (saved.status.as_str(), saved.progress, saved.repeat),
            (status, 12, repeat)
        );
        requests.try_recv().unwrap();
        requests.try_recv().unwrap();
        let vars = &requests.try_recv().unwrap()["variables"];
        assert_eq!(vars["progress"], 12);
        assert_eq!(
            vars.get("status"),
            (!total_cleared).then_some(&json!("COMPLETED"))
        );
        assert_eq!(vars.get("repeat"), (!total_cleared).then_some(&json!(3)));
        assert!(requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn merged_playback_and_offline_confirmation_only_replay_the_watched_episode() {
    for (confirm, episode) in [(false, 12), (true, 12), (true, 13)] {
        let mut base = entry("REPEATING", 10);
        base["repeat"] = json!(2);
        let mut saved = base.clone();
        saved["progress"] = json!(12);
        let (api, mut requests) = anilist::mock_api(vec![
            (503, json!({"errors":[{"message":"Offline"}]})),
            lookup(Some(13), base.clone()),
            mutation(saved),
        ])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        state
            .db
            .upsert_entry(&remote_entry(1, serde_json::from_value(base).unwrap()))
            .unwrap();

        if confirm {
            state.emit(crate::EventKind::TrackingPrompt(crate::TrackingPrompt {
                media_id: 1,
                episode,
                progress: 10,
                title: "Test show".into(),
                raw_title: format!("Test show {episode}"),
            }));
            let event = state.retained_tracking_prompt().unwrap();
            crate::commands::confirm_tracking_prompt(event, 12, &state)
                .await
                .unwrap();
            assert!(state.retained_tracking_prompt().is_none());
        } else {
            crate::commands::set_progress_inner(&state, 1, 11, Some(10))
                .await
                .unwrap();
            crate::commands::watcher_set_progress(&state, 1, 12, Some("test-session"))
                .await
                .unwrap()
                .unwrap();
        }
        let local = state.db.get_entry(1).unwrap().unwrap();
        let before_sync = if episode == 12 {
            ("COMPLETED", 12, 3)
        } else {
            ("REPEATING", 12, 2)
        };
        assert_eq!(
            (local.status.as_str(), local.progress, local.repeat),
            before_sync
        );

        assert!(sync_pending_changes(&state).await.unwrap().is_empty());
        assert_eq!(
            requests.try_recv().unwrap()["variables"]["progress"],
            if confirm { 12 } else { 11 }
        );
        assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
        assert_eq!(
            requests.try_recv().unwrap()["variables"],
            json!({"id":22,"progress":12,"scoreFormat":"POINT_100"})
        );
        let local = state.db.get_entry(1).unwrap().unwrap();
        assert_eq!(
            (local.status.as_str(), local.progress, local.repeat),
            ("REPEATING", 12, 2)
        );
        assert!(requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn clamped_prompt_preserves_a_previous_offline_start() {
    for legacy in [false, true] {
        let base = entry("PLANNING", 0);
        let (api, mut requests) = anilist::mock_api(vec![
            (503, json!({"errors":[{"message":"Offline"}]})),
            lookup(Some(12), base.clone()),
            mutation(entry("CURRENT", 12)),
        ])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        state
            .db
            .upsert_entry(&remote_entry(1, serde_json::from_value(base).unwrap()))
            .unwrap();
        let first = crate::commands::watcher_set_progress(&state, 1, 11, Some("test-session"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.status, "CURRENT");
        if legacy {
            let mut pending = state.db.pending_change(7, 1).unwrap().unwrap();
            pending.patch.playback = None;
            pending.playback = true;
            state.db.put_pending(7, &pending).unwrap();
        }
        state.emit(crate::EventKind::TrackingPrompt(crate::TrackingPrompt {
            media_id: 1,
            episode: 13,
            progress: 11,
            title: "Test show".into(),
            raw_title: "Test show 13".into(),
        }));
        crate::commands::confirm_tracking_prompt(
            state.retained_tracking_prompt().unwrap(),
            12,
            &state,
        )
        .await
        .unwrap();
        assert!(sync_pending_changes(&state).await.unwrap().is_empty());
        requests.try_recv().unwrap();
        requests.try_recv().unwrap();
        assert_eq!(
            requests.try_recv().unwrap()["variables"],
            json!({"id":22,"progress":12,"status":"CURRENT","scoreFormat":"POINT_100"})
        );
    }
}

#[tokio::test]
async fn later_manual_edits_supersede_only_the_playback_fields_they_set() {
    for (status, progress, repeat, expected_status, target, expected_repeat) in [
        (Some("PAUSED"), None, None, "PAUSED", 12, 4),
        (None, None, Some(9), "REPEATING", 12, 9),
        (Some("CURRENT"), Some(10), None, "CURRENT", 10, 4),
        (Some("COMPLETED"), None, Some(4), "COMPLETED", 13, 4),
    ] {
        let mut saved = entry(expected_status, target);
        saved["repeat"] = json!(expected_repeat);
        let (api, mut requests) = anilist::mock_api(vec![
            (503, json!({"errors":[{"message":"Offline"}]})),
            lookup(Some(13), remote(3)),
            mutation(saved),
        ])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        crate::commands::save_entry_inner(
            &state,
            1,
            Some("REPEATING".into()),
            Some(11),
            None,
            Some(4),
        )
        .await
        .unwrap();
        let watched = crate::commands::watcher_set_progress(&state, 1, 12, Some("test-session"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (watched.status.as_str(), watched.progress, watched.repeat),
            ("COMPLETED", 12, 5)
        );
        if progress.is_some() {
            state.db.cache_episode_total(1, Some(13)).unwrap();
        }
        crate::commands::save_entry_inner(
            &state,
            1,
            status.map(str::to_owned),
            progress,
            None,
            repeat,
        )
        .await
        .unwrap();
        assert!(sync_pending_changes(&state).await.unwrap().is_empty());
        requests.try_recv().unwrap();
        requests.try_recv().unwrap();
        assert_eq!(
            requests.try_recv().unwrap()["variables"],
            json!({"id":22,"progress":target,"status":expected_status,"repeat":expected_repeat,
                   "scoreFormat":"POINT_100"})
        );
        assert!(requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn legacy_queue_distinguishes_playback_from_manual_completion() {
    for playback in [false, true] {
        let target = if playback { 12 } else { 13 };
        let status = if playback { "CURRENT" } else { "COMPLETED" };
        let (api, mut requests) = anilist::mock_api(vec![
            lookup(Some(13), remote(3)),
            mutation(entry(status, target)),
        ])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let (_, mut legacy, _) = stage(
            &state,
            1,
            Patch {
                status: Some("COMPLETED".into()),
                progress: Some(12),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        legacy.playback = playback;
        state.db.put_pending(7, &legacy).unwrap();

        assert!(sync_pending_changes(&state).await.unwrap().is_empty());
        requests.try_recv().unwrap();
        let vars = requests.try_recv().unwrap()["variables"].clone();
        assert_eq!(vars["progress"], target);
        assert_eq!(
            vars.get("status"),
            (!playback).then_some(&json!("COMPLETED"))
        );
        assert!(requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn pending_playback_and_its_projection_roll_back_together() {
    let (api, _) = anilist::mock_api(vec![(503, json!({"errors":[{"message":"Offline"}]}))]).await;
    let state = state(api, Db::open(Path::new(":memory:")).unwrap());
    seed(&state);
    let mut base = state.db.get_entry(1).unwrap().unwrap();
    base.status = "REPEATING".into();
    base.progress = 11;
    base.repeat = 2;
    state.db.upsert_entry(&base).unwrap();
    crate::commands::watcher_set_progress(&state, 1, 12, Some("test-session"))
        .await
        .unwrap();
    let prior = serde_json::to_value(state.db.pending(7).unwrap()).unwrap();
    state.db.cache_episode_total(1, None).unwrap();
    state
        .db
        .0
        .lock()
        .execute_batch(
            "CREATE TEMP TRIGGER reject_projection BEFORE INSERT ON list_entry
         BEGIN SELECT RAISE(ABORT, 'projection failed'); END;",
        )
        .unwrap();
    assert!(crate::commands::set_progress_inner(&state, 1, 10, Some(12))
        .await
        .unwrap_err()
        .contains("projection failed"));
    assert_eq!(
        serde_json::to_value(state.db.pending(7).unwrap()).unwrap(),
        prior
    );
    assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 12);

    state
        .db
        .0
        .lock()
        .execute_batch("DROP TRIGGER reject_projection")
        .unwrap();
    let saved = crate::commands::set_progress_inner(&state, 1, 10, Some(12))
        .await
        .unwrap();
    assert_eq!(
        (saved.status.as_str(), saved.progress, saved.repeat),
        ("REPEATING", 10, 2)
    );
}

#[tokio::test]
async fn mismatched_media_write_or_confirmation_cannot_clear_the_queue() {
    for identity in [
        "create mismatch",
        "create missing",
        "lookup mismatch",
        "lookup missing",
        "nested mismatch",
        "nested missing",
    ] {
        let creating = identity.starts_with("create");
        let wrong_id = identity.ends_with("mismatch");
        let mut response = entry("COMPLETED", if creating { 12 } else { 13 });
        let mut replies = Vec::new();
        if creating {
            replies.push(lookup(Some(12), json!(null)));
            if wrong_id {
                response["mediaId"] = json!(2);
            } else {
                response.as_object_mut().unwrap().remove("mediaId");
            }
            replies.push(mutation(response));
        } else {
            replies.push(mutation(response.clone()));
            let mut confirmation = lookup(Some(13), response);
            let media = &mut confirmation.1["data"]["Media"];
            let (object, field) = if identity.starts_with("nested") {
                (&mut media["mediaListEntry"], "mediaId")
            } else {
                (media, "id")
            };
            if wrong_id {
                object[field] = json!(2);
            } else {
                object.as_object_mut().unwrap().remove(field);
            }
            replies.push(confirmation);
        }
        let (api, mut requests) = anilist::mock_api(replies).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        if creating {
            state.db.delete_entry(1).unwrap();
        }
        let saved = crate::commands::save_entry_inner(
            &state,
            1,
            Some("COMPLETED".into()),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(saved.progress, 12, "{identity}");
        assert_eq!(state.db.episode_total(1).unwrap(), Some(12));
        let pending = state.db.pending_change(7, 1).unwrap().unwrap();
        assert_eq!(pending.patch.progress, Some(12));
        assert!(!pending.conflict && pending.error.is_some());
        assert_eq!(saved.id, (!creating).then_some(22));
        let queries = [requests.try_recv().unwrap(), requests.try_recv().unwrap()];
        let write = &queries[usize::from(creating)]["query"].as_str().unwrap();
        assert!(write.contains("{ id mediaId status"));
        let lookup = &queries[usize::from(!creating)]["query"].as_str().unwrap();
        assert!(lookup.contains("id episodes"));
        assert!(requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn saved_changes_require_a_matching_viewer_before_replay_or_resolution() {
    for action in [
        "list",
        "manual",
        "background",
        "send",
        "discard",
        "remove",
        "remove fresh",
    ] {
        let mut other = viewer("POINT_100");
        other["data"]["Viewer"]["id"] = json!(8);
        let (api, mut requests) = anilist::mock_api(vec![(200, other)]).await;
        let state = Arc::new(state(api, Db::open(Path::new(":memory:")).unwrap()));
        seed(&state);
        state.anilist.lock().set_token(Some("test-session".into()));
        state.db.delete_entry(1).unwrap();
        let pending = (action != "remove fresh").then(|| {
            let (_, mut pending, _) = stage(
                &state,
                1,
                Patch {
                    progress: Some(3),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            if action == "remove" {
                pending.deleting = true;
                state.db.put_pending(7, &pending).unwrap();
            }
            pending
        });
        let result = match action {
            "list" => crate::commands::sync_my_list(&state).await.map(drop),
            "manual" => sync_pending_changes(&state).await.map(drop),
            "background" => flush_background(&state).await,
            "send" => resolve_pending_inner(1, true, &state).await,
            "discard" => resolve_pending_inner(1, false, &state).await,
            "remove" | "remove fresh" => crate::commands::delete_entry_cmd(1, &state).await,
            _ => unreachable!(),
        };
        assert_eq!(result.unwrap_err(), "Account changed", "{action}");
        assert!(requests.try_recv().unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("Viewer"));
        assert!(requests.try_recv().is_err(), "{action}");
        assert_eq!(account_id(&state.db), Some(7));
        assert!(state.db.get_entry(1).unwrap().is_none());
        assert_eq!(
            serde_json::to_value(state.db.pending_change(7, 1).unwrap()).unwrap(),
            serde_json::to_value(pending).unwrap(),
        );
    }
}

#[tokio::test]
async fn a_cold_edit_keeps_unverified_remote_fields_out_of_the_queue_owners_list() {
    let mut foreign = entry("REPEATING", 3);
    foreign["id"] = json!(99);
    foreign["score"] = json!(80);
    let (api, mut requests) = anilist::mock_api(vec![lookup(Some(24), foreign)]).await;
    let state = state(api, Db::open(Path::new(":memory:")).unwrap());
    seed(&state);
    stage(
        &state,
        1,
        Patch {
            progress: Some(4),
            ..Default::default()
        },
    )
    .unwrap();
    state.db.clear_entries().unwrap();
    state.anilist.lock().set_token(Some("unverified".into()));

    let saved = crate::commands::save_entry_inner(&state, 1, None, Some(5), None, None)
        .await
        .unwrap();
    assert_eq!(saved.id, Some(22));
    assert_eq!(saved.status, "CURRENT");
    assert_eq!(saved.score, None);
    assert_eq!(saved.progress, 5);
    assert_eq!(state.db.episode_total(1).unwrap(), Some(12));
    let pending = state.db.pending_change(7, 1).unwrap().unwrap();
    assert_eq!(pending.patch.progress, Some(5));
    assert_eq!(pending.base.unwrap().id, Some(22));
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn viewer_failure_preserves_the_queue_and_only_auth_rejection_expires_the_session() {
    for (status, message) in [(503, "Unavailable"), (401, "Invalid Token")] {
        let (api, mut requests) = anilist::mock_api(vec![
            (
                status,
                json!({"errors":[{"message":message,"status":status}]}),
            ),
            (200, viewer("POINT_100")),
            lookup(Some(12), remote(3)),
            mutation(remote(4)),
        ])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        state.anilist.lock().set_token(Some("test-session".into()));
        let (_, pending, _) = stage(
            &state,
            1,
            Patch {
                progress: Some(4),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        assert!(flush_background(&state).await.is_err());
        assert!(requests.try_recv().unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("Viewer"));
        assert!(requests.try_recv().is_err());
        assert_eq!(
            serde_json::to_value(state.db.pending_change(7, 1).unwrap().unwrap()).unwrap(),
            serde_json::to_value(pending).unwrap(),
        );
        assert_eq!(state.anilist.lock().has_token(), status == 503);
        if status == 503 {
            flush_background(&state).await.unwrap();
            assert!(requests.try_recv().unwrap()["query"]
                .as_str()
                .unwrap()
                .contains("Viewer"));
            assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
            assert_eq!(requests.try_recv().unwrap()["variables"]["progress"], 4);
            assert!(requests.try_recv().is_err());
            assert!(state.db.pending(7).unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn unchanged_score_or_repeat_keeps_the_requested_edit_for_review() {
    for (score, repeat) in [(Some(80.0), None), (None, Some(3)), (Some(80.0), Some(3))] {
        for replay in [false, true] {
            let mut unchanged = remote(3);
            unchanged["score"] = json!(37);
            let mut replies = Vec::new();
            if score.is_some() {
                replies.push((200, viewer("POINT_100")));
            }
            if replay {
                replies.push(lookup(Some(12), unchanged.clone()));
                if score.is_some() {
                    replies.push((200, viewer("POINT_100")));
                }
            }
            replies.push(mutation(unchanged));
            if score.is_some() {
                replies.push((200, viewer("POINT_100")));
            }
            let (api, mut requests) = anilist::mock_api(replies).await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            let mut base = state.db.get_entry(1).unwrap().unwrap();
            base.score = Some(37.0);
            state.db.upsert_entry(&base).unwrap();
            if replay {
                stage(
                    &state,
                    1,
                    Patch {
                        score,
                        repeat,
                        ..Default::default()
                    },
                )
                .unwrap();
                flush_background(&state).await.unwrap();
            } else {
                let saved = crate::commands::save_entry_inner(&state, 1, None, None, score, repeat)
                    .await
                    .unwrap();
                assert_eq!(saved.score, score.or(base.score));
                assert_eq!(saved.repeat, repeat.unwrap_or(base.repeat));
            }
            let pending = state.db.pending_change(7, 1).unwrap().unwrap();
            assert!(pending.conflict);
            assert_eq!(pending.patch.score, score);
            assert_eq!(pending.patch.repeat, repeat);
            assert!(pending.rejected_progress.is_none());
            let writes = std::iter::from_fn(|| requests.try_recv().ok())
                .filter(|request| request["query"].as_str().unwrap().contains("mutation"))
                .collect::<Vec<_>>();
            assert_eq!(writes.len(), 1);
            assert_eq!(writes[0]["variables"]["score"], json!(score));
            assert_eq!(writes[0]["variables"]["repeat"], json!(repeat));
        }
    }
}

#[tokio::test]
async fn adjusted_completion_uses_the_confirmed_remote_entry() {
    let mut saved = entry("COMPLETED", 13);
    saved["repeat"] = json!(1);
    saved["updatedAt"] = json!(100);
    let mut current = saved.clone();
    current["score"] = json!(45);
    current["repeat"] = json!(2);
    current["updatedAt"] = json!(101);
    let (api, mut requests) =
        anilist::mock_api(vec![mutation(saved), lookup(Some(13), current)]).await;
    let state = state(api, Db::open(Path::new(":memory:")).unwrap());
    seed(&state);
    let saved =
        crate::commands::save_entry_inner(&state, 1, Some("COMPLETED".into()), None, None, None)
            .await
            .unwrap();
    for row in [saved, state.db.get_entry(1).unwrap().unwrap()] {
        assert_eq!(
            (row.progress, row.score, row.repeat, row.updated_at),
            (13, Some(45.0), 2, Some(101))
        );
    }
    assert_eq!(state.db.episode_total(1).unwrap(), Some(13));
    assert!(state.db.pending(7).unwrap().is_empty());
    assert_eq!(requests.try_recv().unwrap()["variables"]["progress"], 12);
    assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn adjusted_completion_readback_must_confirm_a_score_clear_without_a_cached_format() {
    let saved = entry("COMPLETED", 13);
    let mut current = saved.clone();
    current["score"] = json!(45);
    let (api, mut requests) =
        anilist::mock_api(vec![mutation(saved), lookup(Some(13), current)]).await;
    let state = state(api, Db::open(Path::new(":memory:")).unwrap());
    seed(&state);
    let mut user = state.db.cached_user().unwrap().unwrap();
    user.score_format = None;
    state.db.replace_account("test-session", &user).unwrap();
    crate::commands::save_entry_inner(&state, 1, Some("COMPLETED".into()), None, Some(0.0), None)
        .await
        .unwrap();
    let pending = state.db.pending_change(7, 1).unwrap().unwrap();
    assert!(pending.conflict);
    assert_eq!(pending.patch.score, Some(0.0));
    assert_eq!(state.db.episode_total(1).unwrap(), Some(13));
    assert_eq!(requests.try_recv().unwrap()["variables"]["score"], 0.0);
    assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn refused_score_keeps_the_confirmed_completion_total_when_the_format_changes() {
    for replay in [false, true] {
        let completed = entry("COMPLETED", 13);
        let mut replies = vec![(200, viewer("POINT_100"))];
        if replay {
            replies.extend([lookup(Some(12), remote(3)), (200, viewer("POINT_100"))]);
        }
        replies.extend([
            mutation(completed.clone()),
            lookup(Some(13), completed),
            (200, viewer("POINT_5")),
        ]);
        let (api, _) = anilist::mock_api(replies).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let patch = Patch {
            status: Some("COMPLETED".into()),
            score: Some(80.0),
            ..Default::default()
        };
        if replay {
            stage(&state, 1, patch).unwrap();
            assert_eq!(
                flush_background(&state).await.unwrap_err(),
                SCORE_FORMAT_ERROR
            );
        } else {
            crate::commands::save_entry_inner(&state, 1, patch.status, None, patch.score, None)
                .await
                .unwrap();
        }
        let pending = state.db.pending_change(7, 1).unwrap().unwrap();
        assert_eq!(pending.patch.score, Some(80.0));
        assert!(pending.conflict);
        assert_eq!(pending.error.as_deref(), Some(SCORE_FORMAT_ERROR));
        assert_eq!(state.db.episode_total(1).unwrap(), Some(13));
    }
}

#[tokio::test]
async fn superseded_queue_mutation_keeps_a_definitive_refusal_for_review() {
    for reply in [
        (400, json!({"errors":[{"message":"Invalid progress"}]})),
        mutation(entry("COMPLETED", 4)),
        mutation(entry("CURRENT", 4)),
    ] {
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        let (api, mut requests) =
            anilist::mock_api_gated(vec![lookup(Some(12), remote(3)), reply], Some(gate.clone()))
                .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        stage(
            &state,
            1,
            Patch {
                status: Some("CURRENT".into()),
                progress: Some(4),
                repeat: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        let observe = async {
            assert_eq!(requests.recv().await.unwrap()["variables"]["mediaId"], 1);
            assert_eq!(requests.recv().await.unwrap()["variables"]["progress"], 4);
            state.cancel_login();
            gate.add_permits(1);
        };
        let (result, ()) = tokio::join!(flush_background(&state), observe);
        assert!(result.is_err());
        let retained = state.db.pending_change(7, 1).unwrap().unwrap();
        assert!(retained.conflict);
        assert_eq!(retained.patch.progress, Some(4));
        assert_eq!(retained.patch.repeat, Some(2));
        flush_background(&state).await.unwrap();
        assert!(requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn retrying_the_same_overshoot_does_not_rewrite_the_queue() {
    for playback in [false, true] {
        let replies = vec![lookup(Some(6), remote(3)); 2];
        let (api, mut requests) = anilist::mock_api(replies).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let (_, mut pending, _) = stage(
            &state,
            1,
            Patch {
                progress: Some(8),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        pending.playback = playback;
        state.db.put_pending(7, &pending).unwrap();
        let expected = if playback {
            crate::commands::EPISODE_EXCEEDS_TOTAL
        } else {
            PROGRESS_EXCEEDS_TOTAL
        };
        assert_eq!(
            resolve_pending_inner(1, true, &state).await.unwrap_err(),
            expected
        );
        let commits = crate::db::durable_commit_count();
        let events = ENTRY_NOTIFICATIONS.get();
        assert_eq!(
            resolve_pending_inner(1, true, &state).await.unwrap_err(),
            expected
        );
        assert_eq!(crate::db::durable_commit_count(), commits);
        assert_eq!(ENTRY_NOTIFICATIONS.get(), events);
        for _ in 0..2 {
            assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
        }
        assert!(requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn corrected_progress_does_not_acknowledge_a_drifted_clamp() {
    for (keep_local, status) in [(false, "PAUSED"), (false, "COMPLETED"), (true, "COMPLETED")] {
        let mut replies = vec![
            mutation(entry("COMPLETED", 12)),
            lookup(Some(12), entry(status, 10)),
        ];
        if keep_local {
            replies.push(mutation(entry("CURRENT", 10)));
        }
        let (api, mut requests) = anilist::mock_api(replies).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        state
            .db
            .0
            .lock()
            .execute("UPDATE media SET episodes=NULL WHERE id=1", [])
            .unwrap();
        let mut base = state.db.get_entry(1).unwrap().unwrap();
        base.progress = 11;
        state.db.upsert_entry(&base).unwrap();
        let edit = |progress| {
            crate::commands::save_entry_inner(&state, 1, None, Some(progress), None, None)
        };
        edit(13).await.unwrap();
        edit(10).await.unwrap();
        assert_eq!(requests.try_recv().unwrap()["variables"]["progress"], 13);
        if keep_local {
            resolve_pending_inner(1, true, &state).await.unwrap();
        } else {
            flush_background(&state).await.unwrap();
        }
        assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
        if keep_local {
            assert!(state.db.pending(7).unwrap().is_empty());
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().status, "CURRENT");
            assert_eq!(
                requests.try_recv().unwrap()["variables"]["status"],
                "CURRENT"
            );
        } else {
            assert!(state.db.pending(7).unwrap()[0].conflict, "{status}");
        }
        assert!(requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn stale_missing_media_origin_cannot_change_saved_entries() {
    let (api, _) = anilist::mock_api(vec![]).await;
    let state = state(api, Db::open(Path::new(":memory:")).unwrap());
    seed(&state);
    stage(
        &state,
        1,
        Patch {
            progress: Some(4),
            ..Default::default()
        },
    )
    .unwrap();
    let origin = state.origin();
    let _remote = state.remote_lock.lock().await;
    let _entry = state.entry_lock.lock().await;
    state.cancel_login();
    assert!(
        crate::commands::write_origin_guard(&state, origin, &Some("test-session".into())).is_err()
    );
    assert!(state.db.get_entry(1).unwrap().is_some());
    let saved = state.db.pending_change(7, 1).unwrap().unwrap();
    assert!(!saved.conflict && !saved.missing_media);
}

#[tokio::test]
async fn a_corrected_statusless_clamp_reopens_only_the_matching_completion() {
    for change in [
        "none",
        "null total",
        "rewatch",
        "new total",
        "id",
        "status",
        "progress",
        "repeat",
        "score",
        "untouched score",
        "deleted",
        "paused reply",
    ] {
        let mut reply = entry(
            if change == "paused reply" {
                "PAUSED"
            } else {
                "COMPLETED"
            },
            12,
        );
        if matches!(change, "score" | "untouched score") {
            reply["score"] = json!(70);
        }
        let mut fresh = reply.clone();
        match change {
            "id" => fresh["id"] = json!(23),
            "status" => fresh["status"] = json!("PAUSED"),
            "progress" => fresh["progress"] = json!(8),
            "repeat" => fresh["repeat"] = json!(1),
            "score" | "untouched score" => fresh["score"] = json!(20),
            "deleted" => fresh = serde_json::Value::Null,
            _ => {}
        }
        let total = match change {
            "null total" => None,
            "new total" => Some(24),
            _ => Some(12),
        };
        let can_reopen = matches!(
            change,
            "none" | "null total" | "rewatch" | "untouched score"
        );
        let original = if change == "rewatch" {
            "REPEATING"
        } else {
            "CURRENT"
        };
        let mut replies = vec![mutation(reply), lookup(total, fresh)];
        if can_reopen {
            let mut saved = entry(original, 10);
            if change == "untouched score" {
                saved["score"] = json!(20);
            }
            replies.push(mutation(saved));
        }
        let (api, mut requests) = anilist::mock_api(replies).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        state
            .db
            .0
            .lock()
            .execute("UPDATE media SET episodes = NULL WHERE id=1", [])
            .unwrap();
        let mut base = state.db.get_entry(1).unwrap().unwrap();
        base.progress = 11;
        base.status = original.into();
        if matches!(change, "score" | "untouched score") {
            base.score = Some(70.0);
        }
        state.db.upsert_entry(&base).unwrap();

        let edit = |progress, score| {
            crate::commands::save_entry_inner(&state, 1, None, Some(progress), score, None)
        };
        assert_eq!(edit(13, None).await.unwrap().progress, 13);
        assert!(state.db.pending(7).unwrap()[0].conflict);
        assert_eq!(
            requests.try_recv().unwrap()["variables"],
            json!({"id":22,"progress":13,"scoreFormat":"POINT_100"})
        );
        assert_eq!(
            edit(10, (change == "score").then_some(0.0))
                .await
                .unwrap()
                .progress,
            10
        );
        let pending = sync_pending_changes(&state).await.unwrap();
        assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
        if can_reopen {
            assert!(pending.is_empty(), "{change}");
            if change == "untouched score" {
                assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(20.0));
            }
            assert_eq!(
                requests.try_recv().unwrap()["variables"],
                json!({"id":22,"status":original,"progress":10,"scoreFormat":"POINT_100"})
            );
        } else {
            assert!(pending[0].conflict, "{change}");
            assert!(
                pending[0]
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("AniList changed while"),
                "{change}"
            );
            assert_eq!(edit(9, None).await.unwrap().progress, 9);
        }
        sync_pending_changes(&state).await.unwrap();
        assert!(requests.try_recv().is_err(), "{change}");
    }
}

#[tokio::test]
async fn editing_a_forced_completion_checks_the_fresh_tuple() {
    for (status, progress, cached, fresh, should_write, should_conflict) in [
        ("COMPLETED", None, 12, Some(12), false, false),
        ("PAUSED", None, 12, Some(12), true, false),
        ("DROPPED", None, 12, Some(12), true, false),
        ("PAUSED", Some(12), 12, Some(12), true, false),
        ("CURRENT", Some(12), 12, Some(12), false, true),
        ("CURRENT", Some(12), 24, Some(12), false, true),
        ("CURRENT", Some(12), 24, None, false, true),
        ("CURRENT", Some(13), 24, None, false, true),
        ("CURRENT", Some(13), 24, Some(12), false, true),
        ("CURRENT", Some(13), 24, Some(13), false, true),
        ("CURRENT", Some(12), 12, Some(24), true, false),
        ("CURRENT", Some(12), 24, Some(24), true, false),
        ("CURRENT", Some(13), 12, Some(24), true, false),
        ("CURRENT", Some(13), 24, Some(24), true, false),
    ] {
        let desired = progress.unwrap_or(12);
        let mut replies = vec![
            mutation(entry("COMPLETED", 12)),
            lookup(fresh, entry("COMPLETED", 12)),
        ];
        if should_write {
            replies.push(mutation(entry(status, desired)));
        }
        let (api, mut requests) = anilist::mock_api(replies).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let edit = |status: &str, progress| {
            crate::commands::save_entry_inner(&state, 1, Some(status.into()), progress, None, None)
        };
        edit("CURRENT", Some(12)).await.unwrap();
        assert_eq!(
            requests.try_recv().unwrap()["variables"]["status"],
            "CURRENT"
        );
        assert!(state.db.pending(7).unwrap()[0].conflict);
        state.db.cache_episode_total(1, Some(cached)).unwrap();

        let saved = edit(status, progress).await.unwrap();
        assert_eq!(saved.status, status);
        assert_eq!(
            state.db.pending(7).unwrap()[0].patch.progress,
            Some(desired)
        );
        let label = (status, progress, cached, fresh);
        let pending = sync_pending_changes(&state).await.unwrap();
        assert_eq!(
            requests.try_recv().unwrap()["variables"]["mediaId"],
            1,
            "{label:?}"
        );
        if should_conflict {
            assert!(pending[0].conflict, "{label:?}");
        } else {
            assert!(pending.is_empty(), "{label:?}");
            let saved = state.db.get_entry(1).unwrap().unwrap();
            assert_eq!((saved.status.as_str(), saved.progress), (status, desired));
        }
        if should_write {
            assert_eq!(
                requests.try_recv().unwrap()["variables"],
                json!({"id":22,"status":status,"progress":desired,"scoreFormat":"POINT_100"}),
                "{label:?}"
            );
        }
        sync_pending_changes(&state).await.unwrap();
        assert!(requests.try_recv().is_err(), "{label:?}");
    }
}

#[tokio::test]
async fn keeping_anilist_acknowledges_the_forced_completion_without_mutating() {
    let completed = entry("COMPLETED", 12);
    let (api, mut requests) = anilist::mock_api(vec![
        mutation(completed.clone()),
        lookup(Some(12), completed),
    ])
    .await;
    let state = state(api, Db::open(Path::new(":memory:")).unwrap());
    seed(&state);
    crate::commands::save_entry_inner(&state, 1, Some("CURRENT".into()), Some(12), None, None)
        .await
        .unwrap();
    assert_eq!(
        requests.try_recv().unwrap()["variables"]["status"],
        "CURRENT"
    );
    resolve_pending_change(1, false, &state).await.unwrap();
    assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
    assert_eq!(state.db.get_entry(1).unwrap().unwrap().status, "COMPLETED");
    assert!(state.db.pending(7).unwrap().is_empty());
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn viewer_outage_leaves_a_saved_progress_conflict_correctable() {
    let mut saved = remote(10);
    saved["score"] = json!(80);
    let (api, mut requests) = anilist::mock_api(vec![
        (503, json!({"errors":[{"message":"Unavailable"}]})),
        (
            400,
            json!({"errors":[{"message":"Viewer unavailable","status":400}]}),
        ),
        (200, viewer("POINT_100")),
        lookup(Some(12), remote(3)),
        (200, viewer("POINT_100")),
        mutation(saved),
        (200, viewer("POINT_100")),
    ])
    .await;
    let state = state(api, Db::open(Path::new(":memory:")).unwrap());
    seed(&state);
    let (_, mut pending, _) = stage(
        &state,
        1,
        Patch {
            progress: Some(13),
            score: Some(80.0),
            ..Default::default()
        },
    )
    .unwrap()
    .unwrap();
    pending.conflict = true;
    pending.error = Some(PROGRESS_EXCEEDS_TOTAL.into());
    state.db.put_pending(7, &pending).unwrap();

    for message in ["503", "Viewer unavailable"] {
        let outage = resolve_pending_change(1, true, &state).await.unwrap_err();
        assert!(outage.contains(message), "{outage}");
        assert!(requests.try_recv().unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("Viewer"));
        let changes = get_pending_changes(&state).await.unwrap();
        assert_eq!(changes[0].error.as_deref(), Some(PROGRESS_EXCEEDS_TOTAL));
        assert!(changes[0].conflict);
        let retained = state.db.pending_change(7, 1).unwrap().unwrap();
        assert_eq!(retained.retry_error.as_deref(), Some(outage.as_str()));
    }

    crate::commands::save_entry_inner(&state, 1, None, Some(10), None, None)
        .await
        .unwrap();
    let corrected = state.db.pending_change(7, 1).unwrap().unwrap();
    assert!(!corrected.conflict);
    assert_eq!(corrected.patch.progress, Some(10));
    assert_eq!(corrected.patch.score, Some(80.0));
    assert!(corrected.retry_error.is_none());
    assert!(get_pending_changes(&state).await.unwrap()[0]
        .error
        .is_none());
    assert!(requests.try_recv().is_err());

    assert!(sync_pending_changes(&state).await.unwrap().is_empty());
    let saved = state.db.get_entry(1).unwrap().unwrap();
    assert_eq!((saved.progress, saved.score), (10, Some(80.0)));
    assert_eq!(requests.len(), 5);
}

#[tokio::test]
async fn keeping_anilist_publishes_only_after_the_saved_edit_is_removed() {
    let response = lookup(Some(13), entry("PAUSED", 3));
    let (api, requests) = anilist::mock_api(vec![response.clone(), response]).await;
    let state = state(api, Db::open(Path::new(":memory:")).unwrap());
    seed_competing_title(&state);
    stage(
        &state,
        1,
        Patch {
            progress: Some(4),
            ..Default::default()
        },
    )
    .unwrap();
    crate::commands::local_entries(&state).await.unwrap();
    assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 4);
    let mut events = state.events.subscribe();
    fail_pending_acknowledgement(&state);

    let error = resolve_pending_change(1, false, &state).await.unwrap_err();
    assert!(error.contains("fixture acknowledgement failure"), "{error}");
    let cached = state.db.get_entry(1).unwrap().unwrap();
    assert_eq!((cached.status.as_str(), cached.progress), ("CURRENT", 4));
    assert_eq!(cached.media.unwrap().episodes, Some(12));
    assert_title_winner(&state, 1);
    assert!(state.db.pending_change(7, 1).unwrap().is_some());
    assert!(events.try_recv().is_err());

    state
        .db
        .0
        .lock()
        .execute_batch("DROP TRIGGER fail_pending_ack")
        .unwrap();
    resolve_pending_change(1, false, &state).await.unwrap();
    assert!(state.db.pending(7).unwrap().is_empty());
    assert_title_winner(&state, 2);
    let crate::EventKind::EpisodeUpdated(saved) = events.try_recv().unwrap().kind else {
        panic!("expected the retained AniList entry");
    };
    assert_eq!((saved.status.as_str(), saved.progress), ("PAUSED", 3));
    assert_eq!(saved.media.unwrap().episodes, Some(13));
    assert!(matches!(
        events.try_recv().unwrap().kind,
        crate::EventKind::PendingChanged(_)
    ));
    let cached = state.db.get_entry(1).unwrap().unwrap();
    assert_eq!((cached.status.as_str(), cached.progress), ("PAUSED", 3));
    assert_eq!(requests.len(), 2);
}

#[tokio::test]
async fn reselecting_current_after_an_overshoot_rechecks_the_saved_progress() {
    let completed = entry("COMPLETED", 12);
    let (api, mut requests) = anilist::mock_api(vec![
        mutation(completed.clone()),
        lookup(Some(12), completed.clone()),
        lookup(Some(24), completed),
        mutation(remote(13)),
    ])
    .await;
    let state = state(api, Db::open(Path::new(":memory:")).unwrap());
    seed(&state);
    let edit = |progress| {
        crate::commands::save_entry_inner(&state, 1, Some("CURRENT".into()), progress, None, None)
    };
    edit(Some(12)).await.unwrap();
    assert_eq!(requests.try_recv().unwrap()["variables"]["progress"], 12);
    edit(Some(13)).await.unwrap();
    let pending = sync_pending_changes(&state).await.unwrap();
    assert_eq!(pending[0].error.as_deref(), Some(PROGRESS_EXCEEDS_TOTAL));
    assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
    sync_pending_changes(&state).await.unwrap();
    assert!(requests.try_recv().is_err());

    edit(None).await.unwrap();
    assert!(sync_pending_changes(&state).await.unwrap().is_empty());
    assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
    assert_eq!(
        requests.try_recv().unwrap()["variables"],
        json!({"id":22,"status":"CURRENT","progress":13,"scoreFormat":"POINT_100"})
    );
    sync_pending_changes(&state).await.unwrap();
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn an_unknown_total_cannot_extend_a_statusless_clamp() {
    for progress in [12, 14] {
        let completed = entry("COMPLETED", 12);
        let (api, mut requests) =
            anilist::mock_api(vec![mutation(completed.clone()), lookup(None, completed)]).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        state.db.cache_episode_total(1, Some(24)).unwrap();
        let edit = |progress| {
            crate::commands::save_entry_inner(&state, 1, None, Some(progress), None, None)
        };
        edit(13).await.unwrap();
        assert_eq!(requests.try_recv().unwrap()["variables"]["progress"], 13);
        edit(progress).await.unwrap();
        let pending = sync_pending_changes(&state).await.unwrap();
        if progress == 12 {
            assert!(pending.is_empty());
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().status, "COMPLETED");
        } else {
            assert!(pending[0].conflict);
            assert_eq!(
                pending[0].error.as_deref(),
                Some(anilist::IncompatibleEntryWrite::MESSAGE)
            );
        }
        assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
        sync_pending_changes(&state).await.unwrap();
        assert!(requests.try_recv().is_err());
    }
}

async fn wait_for_read_completion(state: &AppState, waiting: usize, context: &str) {
    assert!(waiting >= 2, "{context} expected active intent receiver");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while state.auth_intent.receiver_count() >= waiting {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{context} timed out"));
}

#[tokio::test]
async fn superseded_confirmed_queue_save_acknowledges_entry_and_total() {
    for mode in [
        "plain",
        "total",
        "logout_total",
        "switch_account",
        "score_after_viewer",
        "readonly_applied",
    ] {
        let mut saved = match mode {
            "total" | "logout_total" => entry("COMPLETED", 13),
            "score_after_viewer" => {
                let mut row = remote(3);
                row["score"] = json!(80);
                row
            }
            _ => remote(4),
        };
        if matches!(mode, "plain" | "switch_account") {
            saved["status"] = json!("PAUSED");
        }
        let mut replies = Vec::new();
        let initial_permits = match mode {
            "plain" | "switch_account" => {
                replies.push(lookup(Some(12), remote(3)));
                replies.push(mutation(saved.clone()));
                1
            }
            "total" | "logout_total" => {
                replies.push(lookup(Some(12), remote(3)));
                replies.push(mutation(saved.clone()));
                replies.push(lookup(Some(13), saved.clone()));
                2
            }
            "score_after_viewer" => {
                replies.push((200, viewer("POINT_100")));
                replies.push(lookup(Some(12), remote(3)));
                replies.push((200, viewer("POINT_100")));
                replies.push(mutation(saved.clone()));
                replies.push((200, viewer("POINT_100")));
                4
            }
            _ => {
                replies.push(lookup(Some(12), saved.clone()));
                0
            }
        };
        let gate = Arc::new(tokio::sync::Semaphore::new(initial_permits));
        let (api, mut requests) = anilist::mock_api_gated(replies, Some(gate.clone())).await;
        let state = Arc::new(state(api, Db::open(Path::new(":memory:")).unwrap()));
        seed(&state);
        let patch = match mode {
            "total" | "logout_total" => Patch {
                status: Some("COMPLETED".into()),
                ..Default::default()
            },
            "score_after_viewer" => Patch {
                score: Some(80.0),
                ..Default::default()
            },
            "plain" | "switch_account" => Patch {
                status: Some("PAUSED".into()),
                progress: Some(4),
                ..Default::default()
            },
            _ => Patch {
                progress: Some(4),
                ..Default::default()
            },
        };
        stage(&state, 1, patch).unwrap().unwrap();
        let mut events = state.events.subscribe();
        let mut signing_out = None;

        if mode == "score_after_viewer" {
            let mut action = Box::pin(flush_background(&state));
            let observe = async {
                for _ in 0..4 {
                    requests.recv().await.unwrap();
                }
                let postflight = requests.recv().await.unwrap();
                assert!(postflight["query"].as_str().unwrap().contains("Viewer"));
                let guard = state.entry_lock.lock().await;
                let waiting = state.auth_intent.receiver_count();
                gate.add_permits(1);
                wait_for_read_completion(&state, waiting, "score_after_viewer postflight").await;
                let mut reader = Box::pin(state.entry_lock.lock());
                enqueue_waiter(reader.as_mut()).await;
                drop(guard);
                let reader = reader.await;
                state.cancel_login();
                drop(reader);
            };
            let (result, ()) = tokio::join!(action.as_mut(), observe);
            assert_eq!(
                result.unwrap_err(),
                "A newer sign-in or sign-out replaced this attempt."
            );
        } else {
            let observe = async {
                for _ in 0..=initial_permits {
                    requests.recv().await.unwrap();
                }
                if matches!(mode, "readonly_applied" | "total" | "logout_total") {
                    let guard = state.entry_lock.lock().await;
                    let waiting = state.auth_intent.receiver_count();
                    gate.add_permits(1);
                    wait_for_read_completion(&state, waiting, mode).await;
                    if mode == "logout_total" {
                        let mut job = Box::pin(crate::commands::logout(&state));
                        tokio::select! {
                            biased;
                            result = &mut job => panic!("logout bypassed remote_lock: {result:?}"),
                            _ = std::future::ready(()) => {}
                        }
                        signing_out = Some(job);
                    } else {
                        state.cancel_login();
                    }
                    drop(guard);
                } else if mode == "switch_account" {
                    state.db.set_setting("anilist_user_id", "8").unwrap();
                    state
                        .db
                        .upsert_entry(&ListEntry {
                            id: Some(88),
                            media_id: 1,
                            status: "CURRENT".into(),
                            progress: 9,
                            ..Default::default()
                        })
                        .unwrap();
                    gate.add_permits(1);
                } else {
                    state.cancel_login();
                    gate.add_permits(1);
                }
            };
            let (result, ()) = tokio::join!(flush_background(&state), observe);
            assert_eq!(
                result.unwrap_err(),
                "A newer sign-in or sign-out replaced this attempt.",
                "{mode}"
            );
        }

        assert!(events.try_recv().is_err(), "{mode}");
        if mode == "readonly_applied" {
            assert!(state.db.pending_change(7, 1).unwrap().is_some());
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 3);
        } else if let Some(job) = signing_out {
            job.await.unwrap();
            assert!(!state.anilist.lock().has_token());
            assert_eq!(state.db.episode_total(1).unwrap(), Some(13));
            assert!(state.db.pending(7).unwrap().is_empty());
        } else if mode == "switch_account" {
            assert!(state.db.pending(7).unwrap().is_empty());
            let replacement = state.db.get_entry(1).unwrap().unwrap();
            assert_eq!((replacement.id, replacement.progress), (Some(88), 9));
        } else {
            assert!(state.db.pending(7).unwrap().is_empty(), "{mode}");
            let cached = state.db.get_entry(1).unwrap().unwrap();
            match mode {
                "plain" => assert_eq!((cached.status.as_str(), cached.progress), ("PAUSED", 4)),
                "total" => {
                    assert_eq!((cached.status.as_str(), cached.progress), ("COMPLETED", 13));
                    assert_eq!(state.db.episode_total(1).unwrap(), Some(13));
                    assert_eq!(cached.media.unwrap().episodes, Some(13));
                }
                "score_after_viewer" => assert_eq!(cached.score, Some(80.0)),
                _ => unreachable!(),
            }
            flush_background(&state).await.unwrap();
            assert!(requests.try_recv().is_err(), "{mode}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_incompatible_score_write_stays_conflicted_after_postflight_failure() {
    let expected_refusal =
        "AniList did not confirm the requested score or rewatch count. Review this saved change.";
    let session_expired = "Your AniList session expired or was revoked. Please sign in again.";
    let auth_replaced = "A newer sign-in or sign-out replaced this attempt.";
    for mode in [
        "viewer_replaced",
        "total_replaced",
        "viewer_401",
        "viewer_503",
        "viewer_400",
        "viewer_other_account",
    ] {
        let mut completed = entry("COMPLETED", 13);
        completed["score"] = json!(70);
        let mut replies = vec![
            (200, viewer("POINT_100")),
            lookup(Some(12), remote(3)),
            (200, viewer("POINT_100")),
        ];
        match mode {
            "total_replaced" => {
                replies.push(mutation(completed.clone()));
                replies.push(lookup(Some(13), completed));
            }
            "viewer_401" => {
                replies.push(mutation(remote(3)));
                replies.push((
                    401,
                    json!({"errors":[{"message":"Invalid token","status":401}]}),
                ));
                replies.push((200, viewer("POINT_100")));
            }
            "viewer_503" => {
                replies.push(mutation(remote(3)));
                replies.push((503, json!({"errors":[{"message":"Unavailable"}]})));
            }
            "viewer_400" => {
                replies.push(mutation(remote(3)));
                replies.push((
                    400,
                    json!({"errors":[{"message":"Invalid Viewer request","status":400}]}),
                ));
            }
            "viewer_other_account" => {
                let mut other = viewer("POINT_100");
                other["data"]["Viewer"]["id"] = json!(8);
                replies.push(mutation(remote(3)));
                replies.push((200, other));
            }
            _ => {
                replies.push(mutation(remote(3)));
                replies.push((200, viewer("POINT_5")));
            }
        }
        let gated = matches!(mode, "viewer_replaced" | "total_replaced");
        let gate = Arc::new(tokio::sync::Semaphore::new(if gated { 4 } else { 10 }));
        let (api, mut requests) = anilist::mock_api_gated(replies, Some(gate.clone())).await;
        let state = Arc::new(state(api, Db::open(Path::new(":memory:")).unwrap()));
        seed(&state);
        stage(
            &state,
            1,
            Patch {
                status: (mode == "total_replaced").then(|| "COMPLETED".into()),
                score: Some(80.0),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        let mut events = state.events.subscribe();

        if gated {
            let observe = async {
                for _ in 0..5 {
                    requests.recv().await.unwrap();
                }
                if mode == "total_replaced" {
                    let guard = state.entry_lock.lock().await;
                    let waiting = state.auth_intent.receiver_count();
                    gate.add_permits(1);
                    wait_for_read_completion(&state, waiting, "total_replaced lookup").await;
                    state.cancel_login();
                    drop(guard);
                } else {
                    state.cancel_login();
                    gate.add_permits(1);
                }
            };
            let (result, ()) = tokio::join!(flush_background(&state), observe);
            assert_eq!(result.unwrap_err(), auth_replaced, "{mode}");
            assert!(events.try_recv().is_err(), "{mode}");
        } else if mode == "viewer_401" {
            let auth_expired = async {
                loop {
                    if let crate::EventKind::AuthExpired = events.recv().await.unwrap().kind {
                        let pending = state.db.pending_change(7, 1).unwrap().unwrap();
                        assert!(pending.conflict);
                        assert_eq!(pending.error.as_deref(), Some(expected_refusal));
                        assert_eq!(pending.retry_error.as_deref(), Some(session_expired));
                        break;
                    }
                }
            };
            let (result, ()) = tokio::join!(flush_background(&state), auth_expired);
            assert_eq!(result.unwrap_err(), session_expired);
            assert!(!state.anilist.lock().has_token());
            crate::commands::login_with_token(
                "renewed-session".into(),
                &state,
                crate::db::ListPreferences::Preserve,
            )
            .await
            .unwrap();
        } else if mode == "viewer_other_account" {
            assert_eq!(
                flush_background(&state).await.unwrap_err(),
                "Account changed"
            );
            let notified = std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
                matches!(
                    event.kind,
                    crate::EventKind::PendingChanged(Some(ref ids)) if ids == &[1]
                )
            });
            assert!(notified, "{mode}");
        } else {
            assert!(flush_background(&state).await.is_err(), "{mode}");
        }

        let pending = state.db.pending_change(7, 1).unwrap().unwrap();
        assert!(pending.conflict, "{mode}");
        assert_eq!(pending.error.as_deref(), Some(expected_refusal), "{mode}");
        assert_eq!(
            pending.retry_error.is_some(),
            matches!(mode, "viewer_401" | "viewer_503" | "viewer_400"),
            "{mode}"
        );
        assert_eq!(pending.patch.score, Some(80.0), "{mode}");
        while requests.try_recv().is_ok() {}
        flush_background(&state).await.unwrap();
        assert!(requests.try_recv().is_err(), "{mode}");
    }
}
