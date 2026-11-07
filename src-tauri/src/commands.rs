use kurisu_core::{models::*, Core};
use tauri::State;

#[tauri::command]
pub async fn acknowledge_tracking_prompt(
    event: crate::tracking::LegacyPrompt,
    core: State<'_, Core>,
) -> Result<bool, String> {
    crate::tracking::acknowledge(core.inner(), event).await
}

#[tauri::command]
pub fn get_tracking_config(core: State<'_, Core>) -> TrackingConfig {
    let core = core.inner().clone();
    tauri::async_runtime::block_on(core.get_tracking_config())
}

#[tauri::command]
pub async fn set_tracking_config(
    mode: String,
    prompt_seconds: u64,
    auto_percent: u64,
    auto_ask: bool,
    mpv_ipc_socket: String,
    discord_enabled: bool,
    core: State<'_, Core>,
) -> Result<TrackingConfig, String> {
    let core = core.inner().clone();
    core.set_tracking_config(
        mode,
        prompt_seconds,
        auto_percent,
        auto_ask,
        mpv_ipc_socket,
        discord_enabled,
    )
    .await
}

#[tauri::command]
pub fn is_logged_in(core: State<'_, Core>) -> bool {
    let core = core.inner().clone();
    tauri::async_runtime::block_on(core.is_logged_in())
}

#[tauri::command]
pub async fn get_app_setting(key: String, core: State<'_, Core>) -> Result<Option<String>, String> {
    let core = core.inner().clone();
    core.get_app_setting(key).await
}

#[tauri::command]
pub async fn set_app_setting(
    key: String,
    value: String,
    core: State<'_, Core>,
) -> Result<(), String> {
    let core = core.inner().clone();
    core.set_app_setting(key, value).await
}

#[tauri::command]
pub async fn login_with_token(token: String, core: State<'_, Core>) -> Result<User, String> {
    let core = core.inner().clone();
    core.login_with_token(token).await
}

#[tauri::command]
pub async fn login_oauth(core: State<'_, Core>) -> Result<User, String> {
    let core = core.inner().clone();
    core.login_oauth().await
}

#[tauri::command]
pub async fn logout(core: State<'_, Core>) -> Result<(), String> {
    let core = core.inner().clone();
    core.logout().await
}

#[tauri::command]
pub async fn current_user(core: State<'_, Core>) -> Result<Option<User>, String> {
    let core = core.inner().clone();
    core.current_user().await
}

#[tauri::command]
pub async fn search_anime_page(
    query: String,
    page: i64,
    core: State<'_, Core>,
) -> Result<SearchPage, String> {
    let core = core.inner().clone();
    core.search_anime_page(query, page).await
}

#[tauri::command]
pub async fn get_season(
    season: String,
    year: i64,
    core: State<'_, Core>,
) -> Result<Vec<Media>, String> {
    let core = core.inner().clone();
    core.get_season(season, year).await
}

#[tauri::command]
pub async fn get_recommendations(
    media_id: i64,
    core: State<'_, Core>,
) -> Result<Vec<Media>, String> {
    let core = core.inner().clone();
    core.get_recommendations(media_id).await
}

#[tauri::command]
pub async fn get_media(id: i64, core: State<'_, Core>) -> Result<Media, String> {
    let core = core.inner().clone();
    core.get_media(id).await
}

#[tauri::command]
pub async fn get_media_detail(id: i64, core: State<'_, Core>) -> Result<MediaDetail, String> {
    let core = core.inner().clone();
    core.get_media_detail(id).await
}

#[tauri::command]
pub async fn get_airing_schedule(
    start: i64,
    end: i64,
    core: State<'_, Core>,
) -> Result<Vec<AiringItem>, String> {
    let core = core.inner().clone();
    core.get_airing_schedule(start, end).await
}

#[tauri::command]
pub async fn sync_my_list(core: State<'_, Core>) -> Result<Vec<ListEntry>, String> {
    let core = core.inner().clone();
    core.sync_my_list().await
}

#[tauri::command]
pub async fn local_entries(core: State<'_, Core>) -> Result<Vec<ListEntry>, String> {
    let core = core.inner().clone();
    core.local_entries().await
}

#[tauri::command]
pub async fn local_entry_updates(
    media_ids: Vec<i64>,
    account_id: i64,
    core: State<'_, Core>,
) -> Result<Vec<Option<ListEntry>>, String> {
    let core = core.inner().clone();
    core.local_entry_updates(media_ids, account_id).await
}

#[tauri::command]
pub async fn get_entry(media_id: i64, core: State<'_, Core>) -> Result<Option<ListEntry>, String> {
    let core = core.inner().clone();
    core.get_entry(media_id).await
}

#[tauri::command]
pub async fn update_entry(
    media_id: i64,
    status: Option<String>,
    progress: Option<i64>,
    score: Option<f64>,
    repeat: Option<i64>,
    core: State<'_, Core>,
) -> Result<ListEntry, String> {
    let core = core.inner().clone();
    core.update_entry(media_id, status, progress, score, repeat)
        .await
}

#[tauri::command]
pub async fn set_progress(
    media_id: i64,
    progress: i64,
    expected: Option<i64>,
    core: State<'_, Core>,
) -> Result<ListEntry, String> {
    let core = core.inner().clone();
    core.set_progress(media_id, progress, expected).await
}

#[tauri::command]
pub async fn delete_entry_cmd(media_id: i64, core: State<'_, Core>) -> Result<(), String> {
    let core = core.inner().clone();
    core.delete_entry_cmd(media_id).await
}

#[tauri::command]
pub async fn get_library_folders(core: State<'_, Core>) -> Result<Vec<String>, String> {
    let core = core.inner().clone();
    core.get_library_folders().await
}

#[tauri::command]
pub async fn add_library_folder(
    path: String,
    core: State<'_, Core>,
) -> Result<Vec<String>, String> {
    let core = core.inner().clone();
    core.add_library_folder(path).await
}

#[tauri::command]
pub async fn remove_library_folder(
    path: String,
    core: State<'_, Core>,
) -> Result<Vec<String>, String> {
    let core = core.inner().clone();
    core.remove_library_folder(path).await
}

#[tauri::command]
pub async fn scan_library(core: State<'_, Core>) -> Result<LibraryScan, String> {
    let core = core.inner().clone();
    core.scan_library().await
}

#[tauri::command]
pub async fn bind_library_path(
    path: String,
    media_id: i64,
    episode_offset: Option<i64>,
    core: State<'_, Core>,
) -> Result<(), String> {
    let core = core.inner().clone();
    core.bind_library_path(path, media_id, episode_offset).await
}

#[tauri::command]
pub async fn library_binding_for(
    path: String,
    core: State<'_, Core>,
) -> Result<Option<i64>, String> {
    let core = core.inner().clone();
    core.library_binding_for(path).await
}

#[tauri::command]
pub async fn library_binding_details(
    path: String,
    core: State<'_, Core>,
) -> Result<Option<LibraryBinding>, String> {
    let core = core.inner().clone();
    core.library_binding_details(path).await
}

#[tauri::command]
pub async fn get_watch_history(
    limit: Option<i64>,
    before: Option<i64>,
    core: State<'_, Core>,
) -> Result<Vec<WatchHistoryItem>, String> {
    let core = core.inner().clone();
    core.get_watch_history(limit, before).await
}

#[tauri::command]
pub async fn unbind_library_media(media_id: i64, core: State<'_, Core>) -> Result<(), String> {
    let core = core.inner().clone();
    core.unbind_library_media(media_id).await
}

#[tauri::command]
pub async fn get_rss_feeds(core: State<'_, Core>) -> Result<Vec<String>, String> {
    let core = core.inner().clone();
    core.get_rss_feeds().await
}

#[tauri::command]
pub async fn add_rss_feed(url: String, core: State<'_, Core>) -> Result<Vec<String>, String> {
    let core = core.inner().clone();
    core.add_rss_feed(url).await
}

#[tauri::command]
pub async fn remove_rss_feed(url: String, core: State<'_, Core>) -> Result<Vec<String>, String> {
    let core = core.inner().clone();
    core.remove_rss_feed(url).await
}

#[tauri::command]
pub async fn fetch_torrents(core: State<'_, Core>) -> Result<TorrentFetch, String> {
    let core = core.inner().clone();
    core.fetch_torrents().await
}

#[tauri::command]
pub async fn mark_torrents_seen(guids: Vec<String>, core: State<'_, Core>) -> Result<(), String> {
    let core = core.inner().clone();
    core.mark_torrents_seen(guids).await
}

#[tauri::command]
pub async fn search_torrents(
    query: String,
    category: Option<String>,
    filter: Option<String>,
    core: State<'_, Core>,
) -> Result<Vec<TorrentItem>, String> {
    let core = core.inner().clone();
    core.search_torrents(query, category, filter).await
}

#[tauri::command]
pub async fn find_show_torrents(
    media_id: i64,
    category: Option<String>,
    filter: Option<String>,
    core: State<'_, Core>,
) -> Result<ShowTorrents, String> {
    let core = core.inner().clone();
    core.find_show_torrents(media_id, category, filter).await
}

#[tauri::command]
pub async fn get_user_stats(core: State<'_, Core>) -> Result<UserStats, String> {
    let core = core.inner().clone();
    core.get_user_stats().await
}

#[tauri::command]
pub async fn get_notifications_page(
    page: i64,
    core: State<'_, Core>,
) -> Result<NotificationPage, String> {
    let core = core.inner().clone();
    core.get_notifications_page(page).await
}

#[tauri::command]
pub async fn mark_notifications_read(core: State<'_, Core>) -> Result<(), String> {
    let core = core.inner().clone();
    core.mark_notifications_read().await
}

#[tauri::command]
pub async fn get_entry_details(
    media_id: i64,
    core: State<'_, Core>,
) -> Result<EntryDetails, String> {
    let core = core.inner().clone();
    core.get_entry_details(media_id).await
}

#[tauri::command]
pub async fn update_entry_details(
    media_id: i64,
    notes: Option<String>,
    started_at: Option<FuzzyDate>,
    completed_at: Option<FuzzyDate>,
    custom_lists: Option<Vec<String>>,
    core: State<'_, Core>,
) -> Result<EntryDetails, String> {
    let core = core.inner().clone();
    core.update_entry_details(media_id, notes, started_at, completed_at, custom_lists)
        .await
}

#[tauri::command]
pub async fn check_update(core: State<'_, Core>) -> Result<UpdateInfo, String> {
    let _ = core;
    kurisu_core::check_bridge_update().await
}

#[tauri::command]
pub async fn install_update(
    app: tauri::AppHandle,
    core: State<'_, Core>,
) -> Result<String, String> {
    crate::profile_transfer::handoff(app, core.inner().clone()).await
}

#[tauri::command]
pub async fn get_pending_changes(core: State<'_, Core>) -> Result<Vec<PendingChange>, String> {
    let core = core.inner().clone();
    core.get_pending_changes().await
}

#[tauri::command]
pub async fn sync_pending_changes(core: State<'_, Core>) -> Result<Vec<PendingChange>, String> {
    let core = core.inner().clone();
    core.sync_pending_changes().await
}

#[tauri::command]
pub async fn resolve_pending_change(
    media_id: i64,
    keep_local: bool,
    core: State<'_, Core>,
) -> Result<(), String> {
    let core = core.inner().clone();
    core.resolve_pending_change(media_id, keep_local).await
}

#[tauri::command]
pub fn take_update_failed(core: State<'_, Core>) -> Option<String> {
    let core = core.inner().clone();
    tauri::async_runtime::block_on(core.take_update_failed())
}

#[tauri::command]
pub fn take_pending_update(core: State<'_, Core>) -> Option<UpdateInfo> {
    let core = core.inner().clone();
    tauri::async_runtime::block_on(core.take_pending_update())
}
