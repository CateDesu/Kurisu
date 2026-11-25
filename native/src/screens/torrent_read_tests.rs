use std::io::Write;

use gpui::TestAppContext;
use kurisu_core::User;

use super::super::fixture_server::{child_gate, serve};
use super::super::workspace_tests::fixture;
use super::*;

const GATE_ENV: &str = "KURISU_TORRENT_READ_TEST_GATE";

fn serve_torrents() {
    let mut feeds = HashMap::new();
    let mut searches = Vec::new();
    let mut held = None;
    let mut held_feed = None;
    let thread = std::thread::current();
    serve(
        thread.name().unwrap(),
        GATE_ENV,
        "KURISU_FIXTURE_RSS",
        |request, child| {
            let mut stream = request.stream;
            let url = reqwest::Url::parse(&format!(
                "http://{}{}",
                stream.local_addr().unwrap(),
                request.target
            ))
            .unwrap();
            let (status, body) = if url.path() == "/" {
                let pairs: HashMap<_, _> = url.query_pairs().collect();
                let query = pairs["q"].as_ref();
                let filter = pairs["f"].as_ref();
                searches.push((query.to_owned(), filter.to_owned()));
                if query == "pending" && filter == "0" {
                    held = Some(stream);
                    std::fs::write(&child.gate, []).unwrap();
                    return;
                }
                if query == "empty" {
                    ("200 OK", "<rss><channel/></rss>".into())
                } else if query == "failed" {
                    ("503 Service Unavailable", String::new())
                } else {
                    ("200 OK", release(query, filter == "2"))
                }
            } else {
                let round = feeds.entry(url.path().to_owned()).or_insert(0);
                *round += 1;
                match (*round, url.path()) {
                    (1, name) => (
                        "200 OK",
                        release(name, false)
                            .replace(&format!("<guid>{name}</guid>"), "<guid>same-guid</guid>"),
                    ),
                    (2, "/first") => ("200 OK", release("/new", false)),
                    (2 | 3, "/second") => ("200 OK", "<rss><channel><item>".into()),
                    (3, "/first") => ("200 OK", "<html>Unavailable</html>".into()),
                    (4, _) => ("503 Service Unavailable", String::new()),
                    (5, _) => ("200 OK", "<rss><channel/></rss>".into()),
                    (6, "/second") => (
                        "200 OK",
                        feed(&[release_item("Recovered URL", "reused", "")]),
                    ),
                    (7, "/second") => {
                        if held_feed.is_some() {
                            std::fs::write(child.gate.with_extension("feed"), []).unwrap();
                        }
                        ("200 OK", release("shared", true))
                    }
                    (6, "/first") => (
                        "200 OK",
                        feed(&[
                            release_item("First hash", "shared", FIRST_HASH)
                                .replace("<guid>First hash</guid>", ""),
                            release_item("Duplicate hash", "reused", FIRST_HASH),
                            release_item("Second hash", "shared", SECOND_HASH)
                                .replace("<guid>Second hash</guid>", ""),
                            release_item("Shared URL without hash", "shared", ""),
                        ]),
                    ),
                    (7, "/first") => {
                        held_feed = Some(stream);
                        if feeds.get("/second") == Some(&7) {
                            std::fs::write(child.gate.with_extension("feed"), []).unwrap();
                        }
                        return;
                    }
                    (8, "/second") => ("503 Service Unavailable", String::new()),
                    (9, "/second") => ("200 OK", release("replacement", true)),
                    _ => panic!("unexpected feed request {url}"),
                }
            };
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            )
            .unwrap();
        },
    );
    assert!(held.is_some());
    assert!(held_feed.is_some());
    assert_eq!(
        searches,
        [
            ("initial", "0"),
            ("initial", "2"),
            ("pending", "0"),
            ("pending", "2"),
            ("pending", "2"),
            ("empty", "2"),
            ("failed", "2"),
            ("previous-account", "2"),
        ]
        .map(|(query, filter)| (query.into(), filter.into()))
    );
    assert_eq!(
        feeds,
        HashMap::from([("/first".into(), 7), ("/second".into(), 9)])
    );
}

fn release(name: &str, trusted: bool) -> String {
    format!(
        "<rss xmlns:nyaa=\"https://nyaa.si/xmlns/nyaa\"><channel><item>\
         <title>{name}</title><link>https://example.invalid/{name}.torrent</link>\
         <guid>{name}</guid><nyaa:trusted>{trusted}</nyaa:trusted>\
         </item></channel></rss>"
    )
}

const FIRST_HASH: &str = "abcdef0123456789abcdef0123456789abcdef01";
const SECOND_HASH: &str = "0123456789abcdef0123456789abcdef01234567";

fn release_item(name: &str, link: &str, hash: &str) -> String {
    format!(
        "<item><title>{name}</title><link>https://example.invalid/{link}.torrent</link>\
         <guid>{name}</guid><nyaa:trusted>true</nyaa:trusted><nyaa:infoHash>{hash}</nyaa:infoHash></item>"
    )
}

fn feed(items: &[String]) -> String {
    format!(
        "<rss xmlns:nyaa=\"https://nyaa.si/xmlns/nyaa\"><channel>{}</channel></rss>",
        items.concat()
    )
}

#[gpui::test]
fn torrent_reads_apply_saved_filters_and_keep_a_snapshot_on_total_failure(tc: &mut TestAppContext) {
    let Some(gate) = child_gate(GATE_ENV) else {
        serve_torrents();
        return;
    };
    let (core, runtime, dir) = fixture();
    let origin = std::env::var("KURISU_FIXTURE_RSS").unwrap();
    runtime.block_on(async {
        core.set_preference(
            "kurisu.torrents.filters".into(),
            Some(r#"{"category":"1_0","release":"2"}"#.into()),
        )
        .await
        .unwrap();
        core.remove_rss_feed(core.get_rss_feeds().await.unwrap().remove(0))
            .await
            .unwrap();
        for feed in ["first", "second"] {
            core.add_rss_feed(format!("{origin}/{feed}")).await.unwrap();
        }
    });
    let (view, cx) = tc
        .add_window_view(|window, cx| Workspace::new_with_session_restore(core, window, cx, false));
    let wait = |cx: &mut TestAppContext, done: &dyn Fn(&mut TestAppContext) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            cx.run_until_parked();
            if done(cx) {
                break;
            }
            assert!(Instant::now() < deadline, "torrent read timed out");
            runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
        }
    };
    let await_jobs = |jobs: &[tokio::task::AbortHandle]| {
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(3), async {
                while jobs.iter().any(|job| !job.is_finished()) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        });
    };
    view.update_in(cx, |w, _, _| {
        w.profile = Some(User {
            id: 7,
            ..Default::default()
        });
        w.route = Route::Torrents;
    });
    for query in ["initial", "pending"] {
        view.update_in(cx, |w, window, cx| {
            w.torrents.prefs_loaded = false;
            w.torrents.release = "0".into();
            w.search_torrents(Some(query.into()), window, cx);
        });
        if query == "initial" {
            wait(cx, &|cx| view.read_with(cx, |w, _| w.torrents.searched));
            view.read_with(cx, |w, _| {
                assert_eq!(w.torrents.results[0].trusted, Some(false))
            });
        } else {
            wait(cx, &|_| gate.is_file());
            view.read_with(cx, |w, _| assert!(w.torrents.searching));
        }
        let old_job = view.update_in(cx, |w, window, cx| {
            let old_job = w.read_aborts["torrents-search"][0].clone();
            w.load_torrent_preferences(window, cx);
            old_job
        });
        wait(cx, &|cx| {
            view.read_with(cx, |w, _| {
                w.torrents.prefs_loaded && !w.torrents.searching && old_job.is_finished()
            })
        });
        view.read_with(cx, |w, _| {
            assert!(w.torrents.error.is_empty(), "{}", w.torrents.error);
            assert_eq!(w.torrents.release, "2");
            assert_eq!(w.torrents.results[0].trusted, Some(true));
            assert!(matches!(
                w.torrents.rows.get(1),
                Some(TorrentDisplayRow::SearchRelease(_))
            ));
        });
    }
    let (item, search) = view.update_in(cx, |w, window, cx| {
        let item = w.torrents.results[0].clone();
        assert!(!item.seen);
        w.search_torrents(Some("pending".into()), window, cx);
        (item, w.read_aborts["torrents-search"][0].clone())
    });
    await_jobs(&[search]);
    view.update_in(cx, |w, window, cx| {
        w.mark_torrent_item_seen(item, window, cx)
    });
    cx.run_until_parked();
    view.read_with(cx, |w, _| {
        assert!(w.torrents.searched && w.torrents.results[0].seen);
    });
    view.update(cx, |w, _| w.torrents.entries_loaded = true);
    let mut snapshot = None;
    for round in 1..=5 {
        view.update_in(cx, |w, window, cx| w.refresh_torrent_feed(true, window, cx));
        wait(cx, &|cx| view.read_with(cx, |w, _| !w.torrents.loading));
        view.read_with(cx, |w, _| {
            let titles: Vec<_> = w
                .torrents
                .items
                .iter()
                .map(|item| item.title.as_str())
                .collect();
            match round {
                1 => {
                    assert_eq!(titles, ["/first", "/second"]);
                    assert!(matches!(
                        w.torrents.rows.last(),
                        Some(TorrentDisplayRow::Unmatched(2))
                    ));
                }
                2 => {
                    assert_eq!(titles, ["/new"]);
                    assert_eq!(w.torrents.failures.len(), 1);
                    assert!(w.torrents.failures[0].url.ends_with("/second"));
                    assert!(w.torrents.failures[0].error.contains("unexpected end"));
                    assert!(matches!(
                        w.torrents.rows.last(),
                        Some(TorrentDisplayRow::Unmatched(1))
                    ));
                    snapshot = Some(w.torrents.rows.clone());
                }
                3 => {
                    assert_eq!(titles, ["/new"]);
                    assert!(Arc::ptr_eq(snapshot.as_ref().unwrap(), &w.torrents.rows));
                    assert!(
                        w.torrents
                            .feed_error
                            .contains("/first: response was not an RSS feed")
                    );
                    assert!(
                        w.torrents
                            .feed_error
                            .contains("unexpected end of RSS document")
                    );
                    assert!(w.torrents.refreshed.is_none());
                }
                4 => {
                    assert_eq!(titles, ["/new"]);
                    assert!(Arc::ptr_eq(snapshot.as_ref().unwrap(), &w.torrents.rows));
                    assert_eq!(
                        w.torrents
                            .feed_error
                            .matches("503 Service Unavailable")
                            .count(),
                        2
                    );
                }
                5 => {
                    assert!(titles.is_empty());
                    assert!(w.torrents.feed_error.is_empty());
                    assert!(w.torrents.failures.is_empty());
                    assert!(w.torrents.error.contains("503 Service Unavailable"));
                }
                _ => unreachable!(),
            }
        });
        if round == 3 {
            let feed_error = view.read_with(cx, |w, _| w.torrents.feed_error.clone());
            view.update_in(cx, |w, window, cx| {
                w.search_torrents(Some("empty".into()), window, cx);
                assert_eq!(w.torrents.feed_error, feed_error);
            });
            wait(cx, &|cx| view.read_with(cx, |w, _| w.torrents.searched));
            view.read_with(cx, |w, _| {
                assert!(w.torrents.results.is_empty());
                assert_eq!(w.torrents.feed_error, feed_error);
                snapshot = Some(w.torrents.rows.clone());
            });
        } else if round == 4 {
            view.update_in(cx, |w, window, cx| {
                w.search_torrents(Some("failed".into()), window, cx);
            });
            wait(cx, &|cx| view.read_with(cx, |w, _| !w.torrents.searching));
            view.read_with(cx, |w, _| {
                assert!(w.torrents.error.contains("503 Service Unavailable"));
                assert_eq!(
                    w.torrents
                        .feed_error
                        .matches("503 Service Unavailable")
                        .count(),
                    2
                );
            });
        }
    }
    view.update_in(cx, |w, window, cx| {
        w.load_torrent_feeds(window, cx);
        w.refresh_torrent_feed(true, window, cx);
    });
    wait(cx, &|cx| {
        view.read_with(cx, |w, _| w.torrents.feeds_loaded && !w.torrents.loading)
    });
    view.update_in(cx, |w, window, cx| {
        assert_eq!(w.torrents.items.len(), 5);
        assert_eq!(w.torrents.items[0].seen_guid, w.torrents.items[1].seen_guid);
        assert_ne!(w.torrents.items[0].seen_guid, w.torrents.items[2].seen_guid);
        assert_eq!(w.torrents.items[0].link, w.torrents.items[2].link);
        assert_eq!(w.torrents.items[0].guid, w.torrents.items[2].guid);
        assert_eq!(w.torrents.items[1].link, w.torrents.items[4].link);
        assert!(matches!(
            w.torrents.rows.last(),
            Some(TorrentDisplayRow::Unmatched(3))
        ));
        let mut items = w.torrents.items.clone();
        for item in &mut items {
            item.media_id = Some(42);
        }
        let entries = [ListEntry {
            media_id: 42,
            ..Default::default()
        }];
        let groups = display_groups(&entries, &items, "1_0", "2", false);
        let titles: Vec<_> = groups[0]
            .items
            .iter()
            .map(|item| item.title.as_str())
            .collect();
        assert_eq!(titles, ["First hash", "Second hash", "Recovered URL"]);

        let mut found = groups[0].items[0].clone();
        found.guid = found.seen_guid.clone().unwrap();
        let mut choices = ChoiceState::new("fixture".into());
        choices.choices = Some(ShowTorrents {
            episodes: vec![found],
            ..Default::default()
        });
        let mut rows = Vec::new();
        append_choice_rows(&mut rows, &groups[0], Some(&choices), &HashSet::new());
        let titles: Vec<_> = rows
            .iter()
            .filter_map(|row| match row {
                TorrentDisplayRow::ChoiceRelease { item, .. } => Some(item.title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(titles, ["First hash", "Second hash", "Recovered URL"]);
        let release_keys: HashSet<_> = rows
            .iter()
            .filter(|row| matches!(row, TorrentDisplayRow::ChoiceRelease { .. }))
            .map(TorrentDisplayRow::key)
            .collect();
        assert_eq!(release_keys.len(), 3);

        let first = w.torrents.items[0].clone();
        w.mark_torrent_item_seen(first, window, cx);
        assert!(w.torrents.items[0].seen && w.torrents.items[1].seen);
        assert!(!w.torrents.items[2].seen);
        w.refresh_torrent_feed(true, window, cx);
    });
    wait(cx, &|cx| {
        gate.with_extension("feed").is_file() && view.read_with(cx, |w, _| w.active_writes == 0)
    });
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    let seen = db.rss_seen_set().unwrap();
    view.read_with(cx, |w, _| {
        let first = &w.torrents.items[0];
        let second = &w.torrents.items[2];
        assert!(seen.contains(first.seen_guid.as_ref().unwrap()));
        assert!(!seen.contains(second.seen_guid.as_ref().unwrap()));
        assert!(!seen.contains(&second.guid));
    });
    let old_refresh = view.update_in(cx, |w, window, cx| {
        let old_refresh = w.read_aborts["torrents-feed-items"][0].clone();
        w.ask_remove_torrent_feed(&format!("{origin}/first"), window, cx);
        w.remove_torrent_feed(window, cx);
        assert!(w.torrents.removing);
        old_refresh
    });
    wait(cx, &|cx| {
        view.read_with(cx, |w, _| {
            !w.torrents.removing && !w.torrents.loading && old_refresh.is_finished()
        })
    });
    view.read_with(cx, |w, _| {
        assert_eq!(w.torrents.feeds, [format!("{origin}/second")]);
        assert_eq!(w.torrents.items.len(), 1);
        assert!(
            w.torrents.items[0]
                .guid
                .starts_with(&format!("{origin}/second"))
        );
        assert!(w.torrents.feed_error.contains("503 Service Unavailable"));
        assert!(!w.torrents.feed_error.contains("/first"));
    });

    let previous_account = view.update_in(cx, |w, window, cx| {
        w.search_torrents(Some("previous-account".into()), window, cx);
        w.refresh_torrent_feed(true, window, cx);
        [
            w.read_aborts["torrents-search"][0].clone(),
            w.read_aborts["torrents-feed-items"][0].clone(),
        ]
    });
    await_jobs(&previous_account);
    view.update_in(cx, |w, window, cx| w.set_profile(None, window, cx));
    cx.run_until_parked();
    view.read_with(cx, |w, _| {
        assert!(w.profile.is_none());
        assert!(w.torrents.results.is_empty() && w.torrents.items.is_empty());
        assert!(!w.torrents.searching && !w.torrents.loading);
        assert!(w.torrents.error.is_empty() && w.torrents.feed_error.is_empty());
        assert_eq!(w.torrents.release, "2");
    });
}
