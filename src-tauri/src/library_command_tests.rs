use super::*;
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
            entry_lock: tokio::sync::Mutex::new(()),
            remote_lock: tokio::sync::Mutex::new(()),
            matchers: Mutex::new(Arc::new(vec![])),
            library_cache: Default::default(),
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
        Box::pin(bind_library_path_inner(path.clone(), 1, None, state)),
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
    assert!(get_library_folders_inner(state).await.unwrap().is_empty());
    assert_eq!(
        add_library_folder_inner(folder.clone(), state)
            .await
            .unwrap(),
        std::slice::from_ref(&folder)
    );
    assert_eq!(
        add_library_folder_inner(folder.clone(), state)
            .await
            .unwrap(),
        std::slice::from_ref(&folder)
    );
    assert_eq!(
        add_library_folder_inner(file.clone(), state)
            .await
            .unwrap_err(),
        format!("folder overlaps existing library folder: {folder}")
    );
    assert_eq!(
        bind_library_path_inner(folder.clone(), 1, None, state)
            .await
            .unwrap_err(),
        "only shows on your list can be linked"
    );
    fixture.entry(1);
    assert_eq!(
        bind_library_path_inner(folder.clone(), 1, Some(10000), state)
            .await
            .unwrap_err(),
        "episode offset must be between -9999 and 9999"
    );
    bind_library_path_inner(folder.clone(), 1, Some(-12), state)
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
            episode_offset: -12
        })
    );
    bind_library_path_inner(file.clone(), 1, None, state)
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
            episode_offset: 0
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
    assert!(remove_library_folder_inner(folder, state)
        .await
        .unwrap()
        .is_empty());
    assert!(get_library_folders_inner(state).await.unwrap().is_empty());
}

#[tokio::test]
async fn library_commands_propagate_storage_errors_without_overwriting_data() {
    let fixture = Fixture::new();
    let state = &fixture.state;
    let path = fixture.path("season");
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
        bind_library_path_inner(path.clone(), 1, None, state)
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
        bind_library_path_inner(path, 1, None, state)
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
    let mut bind = Box::pin(bind_library_path_inner(path.clone(), 1, None, state));
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
        let commands: Vec<CommandFuture<'_>> = vec![
            Box::pin(bind_library_path_inner(path.clone(), 1, None, state)),
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
