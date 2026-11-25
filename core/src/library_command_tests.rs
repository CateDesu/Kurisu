use super::*;
use library::LibraryBindingScope::{File, Folder};
use std::future::Future;
use std::pin::Pin;
use std::sync::{atomic::AtomicUsize, atomic::Ordering, mpsc};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

type CommandFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + 'a>>;

struct Fixture {
    state: AppState,
    root: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "kurisu-library-commands-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        let state = AppState {
            anilist: Mutex::new(AniList::new()),
            db: Arc::new(Db::open(std::path::Path::new(":memory:")).unwrap()),
            user: Mutex::new(None),
            auth_intent: tokio::sync::watch::channel(0).0,
            entry_lock: Default::default(),
            remote_lock: Default::default(),
            matchers: Mutex::new(Arc::new(vec![])),
            library_cache: Default::default(),
            events: tokio::sync::broadcast::channel(128).0,
            last_auth_expired: Mutex::new(None),
            last_now_playing: Mutex::new(None),
            last_tracking_prompt: Mutex::new(None),
        };
        Self { state, root }
    }

    fn path(&self, name: &str) -> String {
        self.root.join(name).to_str().unwrap().into()
    }

    fn account(&self, id: i64) {
        let user = User {
            id,
            name: format!("User {id}"),
            ..Default::default()
        };
        let token = format!("session-{id}");
        self.state.db.replace_account(&token, &user).unwrap();
        self.state.anilist.lock().set_token(Some(token));
        *self.state.user.lock() = Some(user);
    }

    fn entry(&self, id: i64) {
        self.state
            .db
            .upsert_media(&Media {
                id,
                title_english: Some(format!("Fixture Show {id}")),
                ..Default::default()
            })
            .unwrap();
        self.entry_without_media(id);
        self.state.refresh_matchers();
    }

    fn entry_without_media(&self, id: i64) {
        self.state
            .db
            .upsert_entry(&ListEntry {
                media_id: id,
                status: "CURRENT".into(),
                ..Default::default()
            })
            .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn poll_once<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[tokio::test]
async fn binding_rejects_a_configured_file_url_root() {
    let fixture = Fixture::new();
    fixture.account(7);
    fixture.entry(1);
    let state = &fixture.state;
    let root = fixture.path("library root");
    std::fs::create_dir(&root).unwrap();
    let file = std::path::Path::new(&root).join("Episode 01.mkv");
    std::fs::write(&file, []).unwrap();
    let url = reqwest::Url::from_directory_path(&root)
        .unwrap()
        .to_string();
    add_library_folder(url.clone(), state).await.unwrap();
    assert_eq!(
        scan_library(state).await.unwrap().files[0].path,
        file.to_str().unwrap()
    );
    for target in [root.clone(), url] {
        assert!(bind_library_path(target, 1, None, Folder, state)
            .await
            .is_err());
    }
    #[cfg(unix)]
    {
        let alias = fixture.path("root alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        assert!(bind_library_path(alias, 1, None, Folder, state)
            .await
            .is_err());
    }
    assert!(library::get_bindings(&state.db).unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn binding_rejects_an_alias_that_would_cover_a_library_root() {
    let fixture = Fixture::new();
    fixture.account(7);
    fixture.entry(1);
    let state = &fixture.state;
    let root = fixture.root.join("library root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("Episode 01.mkv"), []).unwrap();
    std::fs::write(fixture.root.join("Episode 02.mkv"), []).unwrap();
    let alias = root.join("shows");
    std::os::unix::fs::symlink(&fixture.root, &alias).unwrap();
    let roots = add_library_folder(
        reqwest::Url::from_directory_path(&root).unwrap().into(),
        state,
    )
    .await
    .unwrap();
    let scan = scan_library(state).await.unwrap();
    let aliased_file = alias.join("Episode 02.mkv");
    let aliased_path = aliased_file.to_str().unwrap();
    assert!(scan.files.iter().any(|file| file.path == aliased_path));
    assert_eq!(library::library_binding_folder(aliased_path, &roots), None);
    assert_eq!(
        bind_library_path(alias.to_str().unwrap().into(), 1, None, Folder, state)
            .await
            .unwrap_err(),
        "Choose a show folder inside the Library or link a single file."
    );
    assert!(library::get_bindings(&state.db).unwrap().is_empty());

    bind_library_path(aliased_path.into(), 1, None, File, state)
        .await
        .unwrap();
    for file in scan_library(state).await.unwrap().files {
        assert_eq!(file.bound, file.path == aliased_path);
    }
}

#[tokio::test]
async fn binding_scope_rejects_a_file_replaced_by_a_directory() {
    let fixture = Fixture::new();
    fixture.account(7);
    fixture.entry(1);
    let state = &fixture.state;
    let path = fixture.path("Episode 01.mkv");
    add_library_folder(fixture.root.to_str().unwrap().into(), state)
        .await
        .unwrap();
    std::fs::write(&path, []).unwrap();
    assert_eq!(scan_library(state).await.unwrap().files[0].path, path);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(bind_library_path(path.clone(), 1, None, File, state)
        .await
        .is_err());
    assert!(library::get_bindings(&state.db).unwrap().is_empty());

    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, []).unwrap();
    assert!(bind_library_path(path.clone(), 1, None, Folder, state)
        .await
        .is_err());
    let url = reqwest::Url::from_file_path(&path).unwrap().to_string();
    bind_library_path(url.clone(), 1, Some(2), File, state)
        .await
        .unwrap();
    assert!(scan_library(state).await.unwrap().files[0].bound);

    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert_eq!(
        library_binding_for(url.clone(), state).await.unwrap(),
        Some(1)
    );
    assert_eq!(
        library_binding_details(url.clone(), state).await.unwrap(),
        None
    );
    let descendant = std::path::Path::new(&path).join("Episode 02.mkv");
    std::fs::write(&descendant, []).unwrap();
    #[cfg(unix)]
    {
        let target = fixture.path("outside.txt");
        std::fs::write(&target, []).unwrap();
        let alias = descendant.with_file_name("Episode 03.mkv");
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        assert_eq!(
            library_binding_details(alias.to_str().unwrap().into(), state)
                .await
                .unwrap(),
            None
        );
    }
    assert!(scan_library(state)
        .await
        .unwrap()
        .files
        .iter()
        .all(|file| !file.bound));
    assert_eq!(
        library_binding_details(descendant.to_str().unwrap().into(), state)
            .await
            .unwrap(),
        None
    );

    std::fs::remove_dir_all(&path).unwrap();
    std::fs::write(&path, []).unwrap();
    assert_eq!(
        library_binding_details(url, state).await.unwrap(),
        Some(library::LibraryBinding {
            media_id: 1,
            episode_offset: 2,
            scope: Some(File),
        })
    );
}

#[tokio::test(flavor = "current_thread")]
async fn blocked_library_work_leaves_dispatch_responsive() {
    let fixture = Fixture::new();
    let dispatch_thread = std::thread::current().id();
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, blocked) = mpsc::channel();
    let work = library_db_work(fixture.state.db.clone(), move |db| {
        assert_ne!(std::thread::current().id(), dispatch_thread);
        db.set_setting("library_folders", r#"["synthetic"]"#)?;
        entered.send(()).unwrap();
        blocked.recv_timeout(Duration::from_secs(5))?;
        library::get_folders(db)
    });
    let heartbeat = async {
        started.await.unwrap();
        tokio::task::yield_now().await;
        release.send(()).unwrap();
    };
    let (result, ()) = tokio::join!(work, heartbeat);
    assert_eq!(result.unwrap(), ["synthetic"]);
}

#[tokio::test(flavor = "current_thread")]
async fn every_library_command_yields_before_waiting_for_sqlite() {
    let fixture = Fixture::new();
    fixture.account(7);
    fixture.entry(1);
    let state = &fixture.state;
    let path = fixture.path("season");
    std::fs::create_dir(&path).unwrap();
    let db = state.db.clone();
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, blocked) = mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _database = db.0.lock();
        entered.send(()).unwrap();
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    started.await.unwrap();
    let mut commands: Vec<CommandFuture<'_>> = vec![
        Box::pin(async { get_library_folders_inner(state).await.map(|_| ()) }),
        Box::pin(async { scan_library(state).await.map(|_| ()) }),
        Box::pin(async {
            add_library_folder_inner(path.clone(), state)
                .await
                .map(|_| ())
        }),
        Box::pin(async {
            remove_library_folder_inner(path.clone(), state)
                .await
                .map(|_| ())
        }),
        Box::pin(bind_library_path(path.clone(), 1, None, Folder, state)),
        Box::pin(async {
            library_binding_for_inner(path.clone(), state)
                .await
                .map(|_| ())
        }),
        Box::pin(async {
            library_binding_details_inner(path.clone(), state)
                .await
                .map(|_| ())
        }),
        Box::pin(async { get_watch_history_inner(None, None, state).await.map(|_| ()) }),
        Box::pin(unbind_library_media_inner(1, state)),
    ];
    for command in &mut commands {
        assert!(poll_once(command.as_mut()).is_pending());
    }
    tokio::task::yield_now().await;
    release.send(()).unwrap();
    for command in commands {
        command.await.unwrap();
    }
    holder.join().unwrap();
}

#[tokio::test]
async fn folder_and_binding_commands_preserve_validation_and_offsets() {
    let fixture = Fixture::new();
    let state = &fixture.state;
    let folder = fixture.path("season");
    let file = fixture.path("season/episode13.mkv");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(&file, []).unwrap();
    let root = fixture.root.to_str().unwrap().to_owned();
    assert!(get_library_folders_inner(state).await.unwrap().is_empty());
    assert_eq!(
        add_library_folder_inner(root.clone(), state).await.unwrap(),
        std::slice::from_ref(&root)
    );
    assert_eq!(
        add_library_folder_inner(root.clone(), state).await.unwrap(),
        std::slice::from_ref(&root)
    );
    assert_eq!(
        add_library_folder_inner(file.clone(), state)
            .await
            .unwrap_err(),
        format!("folder overlaps existing library folder: {root}")
    );
    assert_eq!(
        bind_library_path(folder.clone(), 1, None, Folder, state)
            .await
            .unwrap_err(),
        "only shows on your list can be linked"
    );
    fixture.entry(1);
    assert_eq!(
        bind_library_path(folder.clone(), 1, None, Folder, state)
            .await
            .unwrap_err(),
        "only shows on your list can be linked"
    );
    fixture.account(7);
    fixture.entry(1);
    assert_eq!(
        bind_library_path(folder.clone(), 1, Some(10000), Folder, state)
            .await
            .unwrap_err(),
        "episode offset must be between -9999 and 9999"
    );
    bind_library_path(folder.clone(), 1, Some(-12), Folder, state)
        .await
        .unwrap();
    assert_eq!(
        library_binding_for_inner(folder.clone(), state)
            .await
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        library_binding_for_inner(file.clone(), state)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        library_binding_details_inner(file.clone(), state)
            .await
            .unwrap(),
        Some(library::LibraryBinding {
            media_id: 1,
            episode_offset: -12,
            scope: Some(Folder),
        })
    );
    bind_library_path(file.clone(), 1, None, File, state)
        .await
        .unwrap();
    let file_url = reqwest::Url::from_file_path(&file).unwrap().to_string();
    assert_eq!(
        library_binding_for_inner(file_url.clone(), state)
            .await
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        library_binding_details_inner(file_url, state)
            .await
            .unwrap(),
        Some(library::LibraryBinding {
            media_id: 1,
            episode_offset: 0,
            scope: Some(File),
        })
    );
    state.db.delete_entry(1).unwrap();
    assert_eq!(
        library_binding_details_inner(file.clone(), state)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        library_binding_for_inner(file.clone(), state)
            .await
            .unwrap(),
        Some(1)
    );
    unbind_library_media_inner(1, state).await.unwrap();
    assert_eq!(library_binding_for_inner(file, state).await.unwrap(), None);
    assert!(remove_library_folder_inner(root, state)
        .await
        .unwrap()
        .is_empty());
    assert!(get_library_folders_inner(state).await.unwrap().is_empty());
}

#[tokio::test]
async fn link_requires_a_current_matcher_and_works_after_metadata_arrives() {
    let fixture = Fixture::new();
    fixture.account(7);
    fixture.entry_without_media(5);
    fixture.entry_without_media(6);
    let state = &fixture.state;
    let path = fixture.path("Some Show - 03.mkv");
    std::fs::write(&path, []).unwrap();
    state
        .db
        .upsert_media(&Media {
            id: 5,
            title_english: Some("Some Show".into()),
            ..Default::default()
        })
        .unwrap();
    state.refresh_matchers();
    add_library_folder(fixture.root.to_str().unwrap().into(), state)
        .await
        .unwrap();
    let recognized = |scan: LibraryScan| {
        assert_eq!(scan.files.len(), 1);
        let file = &scan.files[0];
        (file.media_id, file.episode, file.bound)
    };
    assert_eq!(
        recognized(scan_library(state).await.unwrap()),
        (Some(5), Some(3), false)
    );
    let missing = bind_library_path(path.clone(), 6, Some(-2), File, state).await;
    let saved = library_binding_for(path.clone(), state).await.unwrap();
    let after_attempt = recognized(scan_library(state).await.unwrap());
    assert_eq!(after_attempt, (Some(5), Some(3), false));
    assert_eq!(
        (missing, saved),
        (
            Err("Show details are unavailable. Refresh your list and try again.".into()),
            None
        )
    );

    state
        .db
        .upsert_media(&Media {
            id: 6,
            title_english: Some("Another Show".into()),
            ..Default::default()
        })
        .unwrap();
    state.refresh_media_matchers(&[6]);
    bind_library_path(path.clone(), 6, Some(-2), File, state)
        .await
        .unwrap();
    assert_eq!(library_binding_for(path, state).await.unwrap(), Some(6));
    assert_eq!(
        recognized(scan_library(state).await.unwrap()),
        (Some(6), Some(1), true)
    );
}

#[tokio::test]
async fn numbering_uses_the_same_live_binding_as_scan_and_preserves_inactive_links() {
    let fixture = Fixture::new();
    fixture.account(7);
    fixture.entry(5);
    fixture.entry_without_media(6);
    let state = &fixture.state;
    let folder = fixture.root.to_str().unwrap();
    let path = fixture.path("Episode 13.mkv");
    std::fs::write(&path, []).unwrap();
    add_library_folder(folder.into(), state).await.unwrap();
    library::bind_path(&state.db, folder, 5, -12).unwrap();
    library::bind_path(&state.db, &path, 6, -11).unwrap();

    let scan = scan_library(state).await.unwrap();
    let file = &scan.files[0];
    assert_eq!((file.media_id, file.episode), (Some(5), Some(1)));
    assert_eq!(
        library_binding_details(path.clone(), state).await.unwrap(),
        Some(library::LibraryBinding {
            media_id: 5,
            episode_offset: -12,
            scope: None,
        })
    );
    assert_eq!(
        library_binding_for(path.clone(), state).await.unwrap(),
        Some(6)
    );

    fixture.entry(6);
    let scan = scan_library(state).await.unwrap();
    let file = &scan.files[0];
    assert_eq!((file.media_id, file.episode), (Some(6), Some(2)));
    assert_eq!(
        library_binding_details(path, state).await.unwrap(),
        Some(library::LibraryBinding {
            media_id: 6,
            episode_offset: -11,
            scope: None,
        })
    );
}

#[tokio::test]
async fn queued_scan_does_not_use_matchers_from_a_replaced_account() {
    let fixture = Fixture::new();
    fixture.account(7);
    fixture.entry(1);
    std::fs::write(fixture.path("Fixture Show 1 - 03.mkv"), []).unwrap();
    let state = &fixture.state;
    add_library_folder(fixture.root.to_str().unwrap().into(), state)
        .await
        .unwrap();
    let transition = state.entry_lock.lock().await;
    let mut scan = Box::pin(scan_library(state));
    assert!(poll_once(scan.as_mut()).is_pending());
    fixture.account(9);
    fixture.entry(2);
    drop(transition);
    let result = scan.await;
    assert!(
        matches!(result, Err(ref error) if error == "Your AniList account changed. Try again."),
        "stale scan returned a result: {}",
        serde_json::to_string(&result).unwrap()
    );
    assert_eq!(scan_library(state).await.unwrap().files[0].media_id, None);
}

#[tokio::test(flavor = "current_thread")]
async fn in_flight_scan_rejects_a_different_account_with_the_same_token() {
    let fixture = Fixture::new();
    fixture.account(7);
    fixture.entry(1);
    std::fs::write(fixture.path("Fixture Show 1 - 03.mkv"), []).unwrap();
    let state = &fixture.state;
    add_library_folder(fixture.root.to_str().unwrap().into(), state)
        .await
        .unwrap();
    let cache = state.library_cache.clone();
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, blocked) = mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _cache = cache.lock();
        entered.send(()).unwrap();
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    started.await.unwrap();
    let mut scan = Box::pin(scan_library(state));
    tokio::time::timeout(
        Duration::from_secs(5),
        std::future::poll_fn(|cx| {
            assert!(scan.as_mut().poll(cx).is_pending());
            if state.entry_lock.try_lock().is_ok() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }),
    )
    .await
    .unwrap();
    state
        .db
        .replace_account(
            "session-7",
            &User {
                id: 9,
                name: "User 9".into(),
                ..Default::default()
            },
        )
        .unwrap();
    fixture.entry(2);
    release.send(()).unwrap();
    holder.join().unwrap();

    let result = scan.await;
    assert!(
        matches!(result, Err(ref error) if error == "Your AniList account changed. Try again."),
        "stale scan returned a result: {}",
        serde_json::to_string(&result).unwrap()
    );
    assert_eq!(scan_library(state).await.unwrap().files[0].media_id, None);
}

#[tokio::test]
async fn library_commands_propagate_storage_errors_without_overwriting_data() {
    let fixture = Fixture::new();
    let state = &fixture.state;
    let path = fixture.path("season");
    fixture.account(7);
    fixture.entry(1);
    for key in ["library_folders", "library_bindings"] {
        state.db.set_setting(key, "{invalid").unwrap();
    }
    let folder_error = library::get_folders(&state.db).unwrap_err().to_string();
    assert_eq!(
        get_library_folders_inner(state).await.unwrap_err(),
        folder_error
    );
    assert_eq!(
        add_library_folder_inner(path.clone(), state)
            .await
            .unwrap_err(),
        folder_error
    );
    assert_eq!(
        remove_library_folder_inner(path.clone(), state)
            .await
            .unwrap_err(),
        folder_error
    );
    let binding_error = library::get_bindings(&state.db).unwrap_err().to_string();
    assert_eq!(
        bind_library_path(path.clone(), 1, None, Folder, state)
            .await
            .unwrap_err(),
        binding_error
    );
    assert_eq!(
        library_binding_for_inner(path.clone(), state)
            .await
            .unwrap_err(),
        binding_error
    );
    assert_eq!(
        library_binding_details_inner(path.clone(), state)
            .await
            .unwrap_err(),
        binding_error
    );
    assert_eq!(
        unbind_library_media_inner(1, state).await.unwrap_err(),
        binding_error
    );
    for key in ["library_folders", "library_bindings"] {
        assert_eq!(
            state.db.get_setting(key).unwrap().as_deref(),
            Some("{invalid")
        );
    }
    state
        .db
        .0
        .lock()
        .execute_batch("DROP TABLE list_entry")
        .unwrap();
    assert_eq!(
        bind_library_path(path, 1, None, File, state)
            .await
            .unwrap_err(),
        "no such table: list_entry"
    );
}

#[tokio::test]
async fn history_commands_preserve_account_limits_cursor_and_errors() {
    let fixture = Fixture::new();
    let state = &fixture.state;
    let path = fixture.path("episode.mkv");
    state.db.record_watch(9, &path, 1, 999).unwrap();
    assert!(get_watch_history_inner(None, None, state)
        .await
        .unwrap()
        .is_empty());
    fixture.account(7);
    state
        .db
        .upsert_media(&Media {
            id: 1,
            title_english: Some("Synthetic show".into()),
            ..Default::default()
        })
        .unwrap();
    for episode in 1..=105 {
        state.db.record_watch(7, &path, 1, episode).unwrap();
    }
    let newest = get_watch_history_inner(Some(0), None, state).await.unwrap();
    assert_eq!(newest.len(), 1);
    assert_eq!(
        (newest[0].episode, newest[0].title.as_str()),
        (105, "Synthetic show")
    );
    assert_eq!(newest[0].path, path);
    assert_eq!(
        get_watch_history_inner(None, None, state)
            .await
            .unwrap()
            .len(),
        50
    );
    let older = get_watch_history_inner(Some(500), Some(newest[0].id), state)
        .await
        .unwrap();
    assert_eq!(older.len(), 100);
    assert_eq!(older[0].episode, 104);
    assert_eq!(older[99].episode, 5);
    fixture.account(9);
    let other = get_watch_history_inner(None, None, state).await.unwrap();
    assert_eq!(other.len(), 1);
    assert_eq!(other[0].episode, 999);
    state
        .db
        .0
        .lock()
        .execute_batch("DROP TABLE watch_history")
        .unwrap();
    assert_eq!(
        get_watch_history_inner(None, None, state)
            .await
            .unwrap_err(),
        "no such table: watch_history"
    );
    state.db.delete_setting(USER_ID_KEY).unwrap();
    assert!(get_watch_history_inner(None, None, state)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn queued_library_account_checks_reject_replaced_sessions() {
    let fixture = Fixture::new();
    fixture.account(7);
    fixture.entry(1);
    let state = &fixture.state;
    let path = fixture.path("season");
    state.db.record_watch(7, &path, 1, 7).unwrap();
    state.db.record_watch(9, &path, 1, 9).unwrap();
    let account_transition = state.entry_lock.lock().await;
    let mut bind = Box::pin(bind_library_path(path.clone(), 1, None, Folder, state));
    let mut details = Box::pin(library_binding_details_inner(path.clone(), state));
    let mut history = Box::pin(get_watch_history_inner(None, None, state));
    assert!(poll_once(bind.as_mut()).is_pending());
    assert!(poll_once(details.as_mut()).is_pending());
    assert!(poll_once(history.as_mut()).is_pending());
    fixture.account(9);
    fixture.entry(1);
    drop(account_transition);
    for error in [
        bind.await.unwrap_err(),
        details.await.unwrap_err(),
        history.await.unwrap_err(),
    ] {
        assert_eq!(error, "Your AniList account changed. Try again.");
    }
    assert!(library::get_bindings(&state.db).unwrap().is_empty());
    let current = get_watch_history_inner(None, None, state).await.unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].episode, 9);
}

#[tokio::test]
async fn queued_folder_and_unlink_writes_do_not_cross_accounts() {
    let fixture = Fixture::new();
    fixture.account(7);
    fixture.entry(1);
    let state = &fixture.state;
    let existing = fixture.path("existing");
    let new_folder = fixture.path("new");
    let linked = fixture.path("existing/show");
    std::fs::create_dir_all(&linked).unwrap();
    add_library_folder_inner(existing.clone(), state)
        .await
        .unwrap();
    bind_library_path(linked.clone(), 1, None, Folder, state)
        .await
        .unwrap();
    let transition = state.entry_lock.lock().await;
    let mut add = Box::pin(add_library_folder_inner(new_folder.clone(), state));
    let mut remove = Box::pin(remove_library_folder_inner(existing.clone(), state));
    let mut unlink = Box::pin(unbind_library_media_inner(1, state));
    assert!(poll_once(add.as_mut()).is_pending());
    assert!(poll_once(remove.as_mut()).is_pending());
    assert!(poll_once(unlink.as_mut()).is_pending());
    fixture.account(9);
    drop(transition);
    for error in [
        add.await.unwrap_err(),
        remove.await.unwrap_err(),
        unlink.await.unwrap_err(),
    ] {
        assert_eq!(error, "Your AniList account changed. Try again.");
    }
    assert_eq!(
        get_library_folders_inner(state).await.unwrap(),
        std::slice::from_ref(&existing)
    );
    assert_eq!(
        library_binding_for_inner(linked, state).await.unwrap(),
        Some(1)
    );
}

#[tokio::test]
async fn queued_library_writes_reject_a_new_intent_for_the_same_account() {
    let fixture = Fixture::new();
    fixture.account(7);
    fixture.entry(1);
    let state = &fixture.state;
    let path = fixture.path("same-account");
    let transition = state.entry_lock.lock().await;
    let mut add = Box::pin(add_library_folder_inner(path.clone(), state));
    let mut bind = Box::pin(bind_library_path(path.clone(), 1, None, Folder, state));
    assert!(poll_once(add.as_mut()).is_pending());
    assert!(poll_once(bind.as_mut()).is_pending());
    state.begin_auth_intent();
    drop(transition);
    assert_eq!(add.await.unwrap_err(), AUTH_REPLACED);
    assert_eq!(bind.await.unwrap_err(), AUTH_REPLACED);
    assert!(get_library_folders_inner(state).await.unwrap().is_empty());
    assert!(library::get_bindings(&state.db).unwrap().is_empty());
}

#[test]
fn library_write_is_cancellable_until_admitted_and_keeps_its_guard_afterwards() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let state = super::tests::test_state();
        let wait = Duration::from_secs(5);
        let guard = state.entry_lock.lock().await;
        let mut unadmitted = Box::pin(add_library_folder("fixture-cancelled".into(), &state));
        assert!(poll_once(unadmitted.as_mut()).is_pending());
        drop(unadmitted);
        drop(guard);

        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, blocked) = mpsc::channel();
        let occupied = tokio::task::spawn_blocking(move || {
            entered.send(()).unwrap();
            blocked.recv_timeout(wait).unwrap();
        });
        tokio::time::timeout(wait, started).await.unwrap().unwrap();
        let mut admitted = Box::pin(add_library_folder("fixture-admitted".into(), &state));
        assert!(poll_once(admitted.as_mut()).is_pending());
        drop(admitted);
        assert!(state.entry_lock.try_lock().is_err());
        release.send(()).unwrap();
        occupied.await.unwrap();

        let _guard = tokio::time::timeout(wait, state.entry_lock.lock())
            .await
            .unwrap();
        assert_eq!(
            library::get_folders(&state.db).unwrap(),
            ["fixture-admitted"]
        );
    });
}

#[test]
fn library_account_checks_hold_the_guard_until_blocking_work_finishes() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let fixture = Fixture::new();
        fixture.account(7);
        fixture.entry(1);
        let state = &fixture.state;
        let path = fixture.path("season");
        std::fs::create_dir(&path).unwrap();
        let commands: Vec<CommandFuture<'_>> = vec![
            Box::pin(bind_library_path(path.clone(), 1, None, Folder, state)),
            Box::pin(async {
                library_binding_details_inner(path.clone(), state)
                    .await
                    .map(|_| ())
            }),
            Box::pin(async { get_watch_history_inner(None, None, state).await.map(|_| ()) }),
        ];
        for mut command in commands {
            let (entered, started) = tokio::sync::oneshot::channel();
            let (release, blocked) = mpsc::channel();
            let occupied = tokio::task::spawn_blocking(move || {
                entered.send(()).unwrap();
                blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            });
            started.await.unwrap();
            assert!(poll_once(command.as_mut()).is_pending());
            assert!(state.entry_lock.try_lock().is_err());
            release.send(()).unwrap();
            command.await.unwrap();
            occupied.await.unwrap();
            assert!(state.entry_lock.try_lock().is_ok());
        }
    });
}

#[tokio::test]
async fn library_worker_panics_return_command_errors() {
    let fixture = Fixture::new();
    let error = library_db_work::<()>(fixture.state.db.clone(), |_| {
        panic!("synthetic library worker failure")
    })
    .await
    .unwrap_err();
    assert!(error.contains("synthetic library worker failure"));
}

#[tokio::test]
async fn signed_out_binding_details_and_rejected_session_binds_do_not_expose_or_link_shows() {
    let fixture = Fixture::new();
    let state = &fixture.state;
    let folder = fixture.path("season");
    let file = fixture.path("season/episode01.mkv");
    fixture.entry(1);
    library::bind_path(&state.db, &folder, 1, -12).unwrap();
    assert!(!state.anilist.lock().has_token());
    assert_eq!(
        library_binding_for_inner(folder.clone(), state)
            .await
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        library_binding_details_inner(file.clone(), state)
            .await
            .unwrap(),
        None
    );

    fixture.account(7);
    fixture.entry(1);
    assert_eq!(
        library_binding_details_inner(file.clone(), state)
            .await
            .unwrap(),
        Some(library::LibraryBinding {
            media_id: 1,
            episode_offset: -12,
            scope: None,
        })
    );
    state
        .db
        .0
        .lock()
        .execute_batch(
            "CREATE TEMP TRIGGER keep_list_on_expiry BEFORE DELETE ON list_entry
             BEGIN SELECT RAISE(ABORT, 'fixture list clear blocked'); END;",
        )
        .unwrap();
    let other_folder = fixture.path("season2");
    let guard = state.entry_lock.lock().await;
    let mut bind = Box::pin(bind_library_path(
        other_folder.clone(),
        1,
        None,
        Folder,
        state,
    ));
    assert!(poll_once(bind.as_mut()).is_pending());
    clear_rejected_session(state);
    drop(guard);
    assert_eq!(
        bind.await.unwrap_err(),
        "Your AniList account changed. Try again."
    );
    assert_eq!(
        bind_library_path(other_folder.clone(), 1, None, Folder, state)
            .await
            .unwrap_err(),
        "only shows on your list can be linked"
    );
    assert_eq!(
        library_binding_for_inner(other_folder, state)
            .await
            .unwrap(),
        None
    );
}
