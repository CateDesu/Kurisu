use super::my_list::MyListSlot;
use super::*;
use crate::components::covers::CoverSource;

pub(super) type CoverKey = (i64, u32, u32);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) enum CoverOwner {
    Route(Route),
    Playback,
    LibraryDialog,
    Editor,
    Avatar,
}

#[derive(Default, PartialEq, Eq)]
pub(super) struct CoverRegion {
    visible: HashSet<CoverKey>,
    nearby: HashSet<CoverKey>,
}

#[cfg(test)]
impl CoverRegion {
    pub(super) fn audit_visible(&self) -> &HashSet<CoverKey> {
        &self.visible
    }

    pub(super) fn audit_nearby(&self) -> &HashSet<CoverKey> {
        &self.nearby
    }
}

const AVATAR_KEY: CoverKey = (i64::MIN, 36, 36);

pub(super) fn visible_card_rows(rows: &gpui::ListState) -> std::ops::Range<usize> {
    let viewport = rows.viewport_bounds();
    let first = rows.logical_scroll_top().item_ix;
    if viewport.size.height <= px(0.) {
        return first..first;
    }
    let mut visible = (first..rows.item_count())
        .map_while(|index| rows.bounds_for_item(index).map(|bounds| (index, bounds)))
        .skip_while(|(_, bounds)| bounds.bottom() <= viewport.top())
        .take_while(|(_, bounds)| bounds.top() < viewport.bottom())
        .map(|(index, _)| index);
    let Some(start) = visible.next() else {
        return first..first;
    };
    start..visible.last().unwrap_or(start) + 1
}

impl Workspace {
    pub(super) fn reconcile_card_covers_on_layout(
        &self,
        page: gpui::Div,
        route: Route,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let weak = cx.weak_entity();
        let generation = self.generation;
        page.on_children_prepainted(move |_, window, app| {
            let weak = weak.clone();
            let route = route.clone();
            window.defer(app, move |_, app| {
                let _ = weak.update(app, |this, cx| {
                    if this.route != route || this.generation != generation {
                        return;
                    }
                    let owner = CoverOwner::Route(route.clone());
                    let before = this
                        .cover_regions
                        .get(&owner)
                        .map(|region| region.visible.clone());
                    match route {
                        Route::Search => this.refresh_search_covers(),
                        Route::Seasons => this.refresh_seasons_covers(),
                        _ => return,
                    }
                    if before.as_ref()
                        != this.cover_regions.get(&owner).map(|region| &region.visible)
                    {
                        cx.notify();
                    }
                });
            });
        })
    }

    pub(super) fn mark_list_cover_refresh(&mut self) {
        self.list_cover_revision = self.list_cover_revision.wrapping_add(1);
    }

    pub(super) fn schedule_list_cover_refresh(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.list_cover_refresh_scheduled
            || self.list_cover_revision == self.list_cover_reconciled
            || self.route != Route::MyList
            || self.profile.is_none()
            || self.visible_ids.is_empty()
        {
            return;
        }
        self.list_cover_refresh_scheduled = true;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        cx.on_next_frame(window, move |_, window, cx| {
            cx.on_next_frame(window, move |this, window, cx| {
                this.finish_list_cover_refresh(generation, account, true, window, cx);
            });
        });
    }

    fn finish_list_cover_refresh(
        &mut self,
        generation: u64,
        account: Option<i64>,
        retry: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.list_cover_refresh_scheduled = false;
        if self.route != Route::MyList
            || self.generation != generation
            || self.profile.as_ref().map(|user| user.id) != account
        {
            self.schedule_list_cover_refresh(window, cx);
            return;
        }
        let owner = CoverOwner::Route(Route::MyList);
        let before_visible = self
            .cover_regions
            .get(&owner)
            .map(|region| region.visible.clone())
            .unwrap_or_default();
        let before_images: HashMap<_, _> = before_visible
            .iter()
            .filter_map(|key| {
                self.cover_images
                    .get(key)
                    .map(|image| (*key, image.clone()))
            })
            .collect();
        if self.prefetch_visible_covers(window, cx) {
            self.list_cover_bounds = Some(self.list_widget.viewport_bounds());
            self.list_cover_reconciled = self.list_cover_revision;
            let after_visible = self.cover_regions.get(&owner).map(|region| &region.visible);
            let paint_changed = after_visible.is_some_and(|visible| {
                *visible != before_visible
                    || visible.iter().any(|key| {
                        match (before_images.get(key), self.cover_images.get(key)) {
                            (Some(before), Some(after)) => !Arc::ptr_eq(before, after),
                            (None, None) => false,
                            _ => true,
                        }
                    })
            });
            if paint_changed {
                cx.notify();
            }
        } else if retry {
            self.list_cover_refresh_scheduled = true;
            cx.on_next_frame(window, move |this, window, cx| {
                this.finish_list_cover_refresh(generation, account, false, window, cx);
            });
        }
    }

    fn cover_owner_active(&self, owner: &CoverOwner) -> bool {
        match owner {
            CoverOwner::Route(route) => *route == self.route,
            CoverOwner::Playback => self.route == Route::Now,
            CoverOwner::LibraryDialog => {
                self.route == Route::Library
                    && matches!(self.library_modal_layer(), Some(ModalLayer::LibraryBind))
            }
            CoverOwner::Editor => self.editor.is_some(),
            CoverOwner::Avatar => self.profile.is_some(),
        }
    }

    fn queue_cover(&mut self, key: CoverKey, url: &str, visible: bool) -> bool {
        let ticket = self.covers.request(
            CoverSource::Remote(url.into()),
            key.1,
            key.2,
            self.cover_scale,
            visible,
        );
        let changed = self.cover_tickets.get(&key) != Some(&ticket);
        if changed {
            self.cover_images.remove(&key);
            self.cover_tickets.insert(key, ticket);
        }
        changed
    }

    #[cfg(test)]
    fn request_cover(&mut self, id: i64, url: &str, width: u32, height: u32) {
        self.request_cover_for(
            CoverOwner::Route(self.route.clone()),
            (id, width, height),
            url,
        );
    }

    pub(super) fn request_cover_for(&mut self, owner: CoverOwner, key: CoverKey, url: &str) {
        if !self.cover_owner_active(&owner) {
            return;
        }
        let changed = self.queue_cover(key, url, true);
        let added = self
            .cover_regions
            .entry(owner)
            .or_default()
            .visible
            .insert(key);
        if changed || added {
            self.sync_cover_ownership();
        }
    }

    pub(super) fn replace_cover_region(
        &mut self,
        owner: CoverOwner,
        visible: Vec<(CoverKey, String)>,
        nearby: Vec<(CoverKey, String)>,
    ) {
        if !self.cover_owner_active(&owner) {
            return;
        }
        let mut region = CoverRegion::default();
        let mut changed = false;
        for (key, url) in visible {
            changed |= self.queue_cover(key, &url, true);
            region.visible.insert(key);
        }
        for (key, url) in nearby {
            if region.visible.contains(&key) {
                continue;
            }
            changed |= self.queue_cover(key, &url, false);
            region.nearby.insert(key);
        }
        if changed || self.cover_regions.get(&owner) != Some(&region) {
            self.cover_regions.insert(owner, region);
            self.sync_cover_ownership();
        }
    }

    pub(super) fn reset_detail_cover_region(&mut self) {
        if self
            .cover_regions
            .remove(&CoverOwner::Route(self.route.clone()))
            .is_some()
        {
            self.sync_cover_ownership();
        }
    }

    pub(super) fn retain_cover_route(&mut self) {
        let route = self.route.clone();
        let has_profile = self.profile.is_some();
        let has_editor = self.editor.is_some();
        let has_library_dialog =
            matches!(self.library_modal_layer(), Some(ModalLayer::LibraryBind));
        self.cover_regions.retain(|owner, _| match owner {
            CoverOwner::Route(owner_route) => *owner_route == route,
            CoverOwner::Playback => route == Route::Now,
            CoverOwner::LibraryDialog => route == Route::Library && has_library_dialog,
            CoverOwner::Editor => has_editor,
            CoverOwner::Avatar => has_profile,
        });
        self.sync_cover_ownership();
    }

    fn sync_cover_ownership(&mut self) {
        let mut visible = HashSet::new();
        let mut active = HashSet::new();
        for region in self.cover_regions.values() {
            visible.extend(region.visible.iter().copied());
            active.extend(region.visible.iter().copied());
            active.extend(region.nearby.iter().copied());
        }
        self.cover_tickets.retain(|key, _| active.contains(key));
        self.cover_images.retain(|key, _| visible.contains(key));
        let visible_tickets: Vec<_> = visible
            .iter()
            .filter_map(|key| self.cover_tickets.get(key).cloned())
            .collect();
        let nearby_tickets: Vec<_> = active
            .difference(&visible)
            .filter_map(|key| self.cover_tickets.get(key).cloned())
            .collect();
        self.covers.retain_active(&visible_tickets, &nearby_tickets);
        for key in visible {
            if let Some(ticket) = self.cover_tickets.get(&key)
                && let Some(image) = self.covers.image(ticket)
            {
                self.cover_images.insert(key, image);
            }
        }
    }

    fn cover_is_visible(&self, key: &CoverKey) -> bool {
        self.cover_regions
            .values()
            .any(|region| region.visible.contains(key))
    }

    pub(crate) fn profile_avatar(&self) -> Option<Arc<gpui::RenderImage>> {
        self.cover_images.get(&AVATAR_KEY).cloned()
    }

    fn refresh_avatar_cover(&mut self) {
        let avatar = self
            .profile
            .as_ref()
            .and_then(|user| user.avatar.as_deref())
            .map(str::to_owned);
        if let Some(url) = avatar {
            self.replace_cover_region(CoverOwner::Avatar, vec![(AVATAR_KEY, url)], vec![]);
        } else if self.cover_regions.remove(&CoverOwner::Avatar).is_some() {
            self.sync_cover_ownership();
        }
    }

    pub(super) fn ensure_cover_scale(&mut self, scale: f32) -> bool {
        if self.cover_scale == scale {
            return false;
        }
        self.cover_scale = scale;
        self.covers.clear();
        self.cover_regions.clear();
        self.cover_tickets.clear();
        self.cover_images.clear();
        if self.editor.is_some() {
            self.requeue_editor_covers();
        }
        if self.route == Route::Now {
            self.refresh_playback_cover();
        }
        match self.route {
            Route::Anime(_) => self.requeue_detail_covers(),
            Route::MyList => self.mark_list_cover_refresh(),
            _ => self.refresh_route_covers(),
        }
        if self.cover_owner_active(&CoverOwner::LibraryDialog) {
            self.refresh_library_link_covers();
        }
        self.refresh_avatar_cover();
        true
    }

    pub(super) fn prefetch_covers(
        &mut self,
        range: std::ops::Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ensure_cover_scale(window.scale_factor());
        if !self.visible_ids.is_empty() && range.start >= self.visible_ids.len() {
            self.mark_list_cover_refresh();
            self.schedule_list_cover_refresh(window, cx);
            return;
        }
        let start = range.start.saturating_sub(8);
        let end = range.end.saturating_add(12).min(self.visible_ids.len());
        let mut visible = Vec::new();
        let mut nearby = Vec::new();
        for index in start.min(end)..end {
            let id = self.visible_ids[index];
            let Some(url) = self
                .list
                .entry(id)
                .and_then(|entry| entry.media.as_ref())
                .and_then(|media| media.cover_medium.as_ref())
            else {
                continue;
            };
            let item = ((id, 46, 63), url.clone());
            if range.contains(&index) {
                visible.push(item);
            } else {
                nearby.push(item);
            }
        }
        self.replace_cover_region(CoverOwner::Route(Route::MyList), visible, nearby);
    }

    pub(super) fn prefetch_visible_covers(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.route != Route::MyList {
            return false;
        }
        let count = self.visible_ids.len();
        if count == 0 {
            self.prefetch_covers(0..0, window, cx);
            return true;
        }
        let viewport = self.list_widget.viewport_bounds();
        if viewport.size.height <= px(0.) {
            return false;
        }
        let mut start = self.list_widget.logical_scroll_top().item_ix.max(1);
        if start > count {
            self.replace_cover_region(CoverOwner::Route(Route::MyList), vec![], vec![]);
            return true;
        }
        while start <= count {
            let Some(bounds) = self.list_widget.bounds_for_item(start) else {
                if start == 1
                    && self
                        .list_widget
                        .bounds_for_item(0)
                        .is_some_and(|header| header.bottom() >= viewport.bottom())
                {
                    self.replace_cover_region(CoverOwner::Route(Route::MyList), vec![], vec![]);
                    return true;
                }
                return false;
            };
            if bounds.bottom() > viewport.top() {
                break;
            }
            start += 1;
        }
        if start > count {
            self.replace_cover_region(CoverOwner::Route(Route::MyList), vec![], vec![]);
            return true;
        }
        let mut end = start;
        while end <= count {
            let Some(bounds) = self.list_widget.bounds_for_item(end) else {
                if end > start
                    && self
                        .list_widget
                        .bounds_for_item(end - 1)
                        .is_some_and(|prior| prior.bottom() >= viewport.bottom())
                {
                    break;
                }
                return false;
            };
            if bounds.top() >= viewport.bottom() {
                break;
            }
            end += 1;
        }
        if start < end {
            self.prefetch_covers(MyListSlot::media_range(start..end, count), window, cx);
        } else {
            self.replace_cover_region(CoverOwner::Route(Route::MyList), vec![], vec![]);
        }
        true
    }

    pub(super) fn poll_covers(&mut self, window: &Window, cx: &mut Context<Self>) {
        let completed = self.covers.completion_signal();
        cx.spawn_in(window, async move |this, cx| {
            // The deterministic scheduler cannot accept wakes from worker threads.
            let deterministic = cx.background_executor().is_test();
            let mut maintenance = cx.background_executor().timer(Duration::from_millis(120));
            loop {
                tokio::select! {
                    _ = completed.notified(), if !deterministic => {},
                    _ = &mut maintenance => {
                        maintenance = cx.background_executor().timer(Duration::from_millis(120));
                    },
                }
                match this.update_in(cx, |this, window, cx| {
                    if this.stopping {
                        for image in this.covers.take_disposed() {
                            cx.drop_image(image, Some(window));
                        }
                        return true;
                    }
                    let mut changed = this.ensure_cover_scale(window.scale_factor());
                    if this.route == Route::MyList
                        && (changed || this.list_cover_revision != this.list_cover_reconciled)
                    {
                        let start = this.list_widget.logical_scroll_top().item_ix;
                        let range = MyListSlot::media_range(
                            start..start.saturating_add(32),
                            this.visible_ids.len(),
                        );
                        this.prefetch_covers(range, window, cx);
                        if changed {
                            this.mark_list_cover_refresh();
                        }
                        this.schedule_list_cover_refresh(window, cx);
                    }
                    if this
                        .cover_regions
                        .keys()
                        .any(|owner| !this.cover_owner_active(owner))
                    {
                        this.retain_cover_route();
                    }
                    this.refresh_avatar_cover();
                    let ready = this.covers.poll();
                    for item in ready {
                        let keys: Vec<_> = this
                            .cover_tickets
                            .iter()
                            .filter_map(|(key, ticket)| {
                                (*ticket == item.ticket && this.cover_is_visible(key))
                                    .then_some(*key)
                            })
                            .collect();
                        for key in keys {
                            if let Some(image) = this.covers.image(&item.ticket) {
                                this.cover_images.insert(key, image);
                                changed = true;
                            }
                        }
                    }
                    if changed {
                        cx.notify();
                    }
                    for image in this.covers.take_disposed() {
                        cx.drop_image(image, Some(window));
                    }
                    false
                }) {
                    Ok(true) | Err(_) => break,
                    Ok(false) => {}
                }
            }
        })
        .detach();
    }

    pub(super) fn refresh_detail_covers(&mut self, scale: f32) {
        if !self.ensure_cover_scale(scale) {
            self.requeue_detail_covers();
        }
    }

    pub(super) fn requeue_detail_covers(&mut self) {
        let Route::Anime(id) = self.route else {
            return;
        };
        if self.detail.media_id != Some(id) {
            return;
        }
        let mut visible = Vec::new();
        if let Some(detail) = &self.detail.detail {
            if let Some(url) = detail
                .media
                .cover_large
                .as_deref()
                .or(detail.media.cover_medium.as_deref())
            {
                visible.push(((id, 128, 184), url.to_owned()));
            }
            if let Some(url) = detail.media.banner_image.as_deref() {
                visible.push(((id, 800, 192), url.to_owned()));
            }
            for (index, character) in detail
                .characters
                .iter()
                .enumerate()
                .take(super::detail::DETAIL_PERSON_LIMIT)
            {
                if let Some(url) = character.image.as_deref() {
                    visible.push(((-1 - index as i64, 112, 160), url.to_owned()));
                }
            }
            for (index, person) in detail
                .staff
                .iter()
                .enumerate()
                .take(super::detail::DETAIL_PERSON_LIMIT)
            {
                if let Some(url) = person.image.as_deref() {
                    visible.push(((-200_001 - index as i64, 112, 160), url.to_owned()));
                }
            }
        }
        let related = self
            .detail
            .detail
            .iter()
            .flat_map(|detail| detail.relations.iter().map(|relation| &relation.media));
        let mut thumbnails = HashSet::new();
        for media in self.detail.recommendations.iter().chain(related) {
            if let Some(url) = media.cover_medium.as_ref().or(media.cover_large.as_ref())
                && thumbnails.insert(media.id)
            {
                visible.push(((media.id, 112, 160), url.clone()));
            }
        }
        self.replace_cover_region(CoverOwner::Route(Route::Anime(id)), visible, vec![]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use kurisu_core::Platform;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestPlatform;

    impl Platform for TestPlatform {
        fn open_url(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn open_path(&self, _: &Path) -> Result<(), String> {
            Ok(())
        }
        fn reveal_path(&self, _: &Path) -> Result<(), String> {
            Ok(())
        }
    }

    struct FixtureDir(PathBuf);

    impl Drop for FixtureDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> (Core, tokio::runtime::Runtime, FixtureDir) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("kurisu-covers-{}-{nonce}", std::process::id()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let core = Core::open(
            path.clone(),
            runtime.handle().clone(),
            Arc::new(TestPlatform),
        )
        .unwrap();
        (core, runtime, FixtureDir(path))
    }

    #[gpui::test]
    fn real_cover_workers_finish_without_waking_the_test_scheduler(cx: &mut TestAppContext) {
        let (core, runtime, dir) = fixture();
        let path = dir.0.join("cover.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
            .save(&path)
            .unwrap();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |w, _, _| {
            w.auth_intent += 1;
            w.profile = Some(User {
                id: 47,
                ..Default::default()
            });
        });
        cx.run_until_parked();
        for (index, route) in [Route::Now, Route::Library].into_iter().enumerate() {
            let key = (index as i64, index as u32 + 1, 2);
            let completed = workspace.update_in(cx, |w, _, _| {
                w.route = route.clone();
                w.retain_cover_route();
                let ticket =
                    w.covers
                        .request(CoverSource::Local(path.clone()), key.1, key.2, 1., true);
                w.cover_tickets.insert(key, ticket);
                w.cover_regions.insert(
                    CoverOwner::Route(route),
                    CoverRegion {
                        visible: HashSet::from([key]),
                        nearby: HashSet::new(),
                    },
                );
                w.sync_cover_ownership();
                w.covers.completion_signal()
            });
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), completed.notified())
                    .await
                    .unwrap();
            });
            cx.executor().advance_clock(Duration::from_millis(120));
            cx.run_until_parked();
            workspace.read_with(cx, |w, _| {
                assert!(w.cover_images.contains_key(&key));
            });
        }
    }

    #[gpui::test]
    fn unchanged_ownership_skips_reconciliation_but_source_and_visibility_changes_do_not(
        cx: &mut TestAppContext,
    ) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        let baseline = workspace.update_in(cx, |w, _, _| {
            w.auth_intent += 1;
            w.profile = Some(User {
                id: 47,
                avatar: Some("invalid-avatar".into()),
                ..Default::default()
            });
            w.route = Route::Search;
            w.refresh_avatar_cover();
            let owner = CoverOwner::Route(Route::Search);
            let visible = vec![((1, 46, 63), "invalid-first".into())];
            let nearby = vec![((2, 46, 63), "invalid-second".into())];
            w.replace_cover_region(owner.clone(), visible.clone(), nearby.clone());
            let baseline = w.covers.audit_ownership_updates();
            for _ in 0..32 {
                w.replace_cover_region(owner.clone(), visible.clone(), nearby.clone());
                w.request_cover_for(owner.clone(), (1, 46, 63), "invalid-first");
                w.refresh_avatar_cover();
            }
            assert_eq!(w.covers.audit_ownership_updates(), baseline);
            baseline
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(240));
        cx.run_until_parked();
        workspace.update_in(cx, |w, _, _| {
            assert_eq!(w.covers.audit_ownership_updates(), baseline);
            let owner = CoverOwner::Route(Route::Search);
            let old = w.cover_tickets[&(1, 46, 63)].clone();
            w.replace_cover_region(
                owner.clone(),
                vec![((1, 46, 63), "changed-source".into())],
                vec![((2, 46, 63), "invalid-second".into())],
            );
            assert_ne!(w.cover_tickets[&(1, 46, 63)], old);
            assert_eq!(w.covers.audit_ownership_updates(), baseline + 1);
            w.replace_cover_region(
                owner.clone(),
                vec![((2, 46, 63), "invalid-second".into())],
                vec![((1, 46, 63), "changed-source".into())],
            );
            assert_eq!(w.covers.audit_ownership_updates(), baseline + 2);
            assert_eq!(
                w.cover_regions[&owner].visible,
                HashSet::from([(2, 46, 63)])
            );
            w.replace_cover_region(owner, vec![], vec![]);
            assert_eq!(w.covers.audit_ownership_updates(), baseline + 3);
            assert!(!w.cover_tickets.contains_key(&(1, 46, 63)));
            assert!(!w.cover_tickets.contains_key(&(2, 46, 63)));
            assert!(w.cover_tickets.contains_key(&AVATAR_KEY));
        });
    }

    #[gpui::test]
    fn recommendation_cover_preference_survives_scale_refresh(cx: &mut TestAppContext) {
        use kurisu_core::models::{MediaDetail, MediaRelation};

        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        workspace.update_in(cx, |w, _, _| {
            w.route = Route::Anime(9);
            w.ensure_cover_scale(1.);
            let request = w.detail.start(9);
            let items = vec![
                Media {
                    id: 10,
                    cover_medium: Some("preferred-medium".into()),
                    cover_large: Some("unused-large".into()),
                    ..Default::default()
                },
                Media {
                    id: 11,
                    cover_large: Some("only-large".into()),
                    ..Default::default()
                },
                Media {
                    id: 12,
                    ..Default::default()
                },
            ];
            w.detail.finish_recommendations(9, request, Ok(items));
            w.refresh_detail_covers(1.);

            let first = w.cover_tickets.clone();
            w.detail.finish_detail(
                9,
                request,
                Ok(MediaDetail {
                    media: Media {
                        id: 9,
                        ..Default::default()
                    },
                    relations: vec![MediaRelation {
                        media: Media {
                            id: 11,
                            cover_medium: Some("related-medium".into()),
                            ..Default::default()
                        },
                        relation: "SEQUEL".into(),
                    }],
                    characters: vec![],
                    staff: vec![],
                    cached_at: None,
                    warning: None,
                    unavailable_sections: vec![],
                }),
            );
            w.requeue_detail_covers();
            assert_eq!(w.cover_tickets, first);
            assert!(w.ensure_cover_scale(2.));
            for (id, url) in [(10, "preferred-medium"), (11, "only-large")] {
                let key = (id, 112, 160);
                let refreshed = w.cover_tickets[&key].clone();
                assert_ne!(refreshed, first[&key]);
                w.request_cover(id, url, 112, 160);
                assert_eq!(w.cover_tickets[&key], refreshed);
            }
            assert!(!w.cover_tickets.contains_key(&(12, 112, 160)));
        });
    }

    #[gpui::test]
    fn detail_navigation_retains_cache_identity_and_retires_synthetic_portrait_owners(
        cx: &mut TestAppContext,
    ) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |w, window, cx| {
            w.auth_intent += 1;
            w.profile = Some(User {
                id: 47,
                avatar: Some("invalid-avatar".into()),
                ..Default::default()
            });
            w.refresh_avatar_cover();
            w.request_cover(7, "invalid-list", 46, 63);
            let list = w.cover_tickets[&(7, 46, 63)].clone();
            let avatar = w.cover_tickets[&AVATAR_KEY].clone();
            w.switch_route(Route::Anime(7), window, cx);
            assert_eq!(w.cover_tickets[&AVATAR_KEY], avatar);
            assert!(!w.cover_tickets.contains_key(&(7, 46, 63)));
            w.request_cover(-1, "first-character", 112, 160);
            let portrait = w.cover_tickets[&(-1, 112, 160)].clone();
            w.load_detail(7, window, cx);
            assert!(!w.cover_tickets.contains_key(&(-1, 112, 160)));
            assert_eq!(w.cover_tickets[&AVATAR_KEY], avatar);
            w.request_cover(-1, "second-character", 112, 160);
            assert_ne!(w.cover_tickets[&(-1, 112, 160)], portrait);
            w.switch_route(Route::MyList, window, cx);
            assert!(!w.cover_tickets.contains_key(&(-1, 112, 160)));
            w.request_cover(7, "invalid-list", 46, 63);
            assert_eq!(w.cover_tickets[&(7, 46, 63)], list);
            w.ensure_cover_scale(w.cover_scale * 2.);
            w.request_cover(7, "invalid-list", 46, 63);
            assert_ne!(w.cover_tickets[&(7, 46, 63)], list);
            let scaled = w.cover_tickets[&(7, 46, 63)].clone();
            w.set_profile(
                Some(User {
                    id: 48,
                    ..Default::default()
                }),
                window,
                cx,
            );
            w.request_cover(7, "invalid-list", 46, 63);
            assert_ne!(w.cover_tickets[&(7, 46, 63)], scaled);
        });
    }

    #[gpui::test]
    fn ensure_cover_scale_requeues_active_route_library_dialog_and_avatar(cx: &mut TestAppContext) {
        use kurisu_core::models::ListEntry;

        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |w, window, cx| {
            w.auth_intent += 1;
            w.profile = Some(User {
                id: 51,
                avatar: Some("https://example.invalid/avatar.png".into()),
                ..Default::default()
            });
            w.switch_route(Route::Search, window, cx);
            w.search.results = vec![Media {
                id: 301,
                cover_medium: Some("https://example.invalid/search-301.png".into()),
                ..Default::default()
            }];
            w.search.rows = Some(gpui::ListState::new(
                1,
                gpui::ListAlignment::Top,
                gpui::px(0.),
            ));
            w.refresh_search_covers();
            w.refresh_avatar_cover();
            let old_search = w.cover_tickets[&(301, 174, 176)].clone();
            let old_avatar = w.cover_tickets[&AVATAR_KEY].clone();

            let next_scale = w.cover_scale + 0.5;
            assert!(w.ensure_cover_scale(next_scale));
            assert_ne!(w.cover_tickets[&(301, 174, 176)], old_search);
            assert_ne!(w.cover_tickets[&AVATAR_KEY], old_avatar);

            w.route = Route::MyList;
            w.list_cover_reconciled = w.list_cover_revision;
            assert!(w.ensure_cover_scale(next_scale + 0.5));
            assert_ne!(
                w.list_cover_revision, w.list_cover_reconciled,
                "MyList must mark a cover refresh when scale changes"
            );

            w.route = Route::Library;
            w.open_library_link_dialog_for_test(
                vec![ListEntry {
                    media_id: 401,
                    media: Some(Media {
                        id: 401,
                        title_english: Some("Linked Show".into()),
                        cover_medium: Some("https://example.invalid/link-401.png".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                "/anime/ep01.mkv".into(),
                window,
                cx,
            );
            let old_dialog = w.cover_tickets[&(401, 32, 44)].clone();
            assert!(w.ensure_cover_scale(next_scale + 1.0));
            assert_ne!(w.cover_tickets[&(401, 32, 44)], old_dialog);
            assert!(w.cover_tickets.contains_key(&AVATAR_KEY));
        });
    }

    #[gpui::test]
    fn detail_covers_limit_character_and_staff_portraits_to_rendered_cards(
        cx: &mut TestAppContext,
    ) {
        use kurisu_core::models::{MediaCharacter, MediaDetail, MediaStaff};

        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        workspace.update_in(cx, |w, _, _| {
            w.profile = Some(User {
                id: 52,
                ..Default::default()
            });
            w.route = Route::Anime(88);
            let request = w.detail.start(88);
            let characters = (0..50)
                .map(|index| MediaCharacter {
                    name: format!("Char {index}"),
                    image: Some(format!("https://example.invalid/char-{index}.png")),
                    ..Default::default()
                })
                .collect();
            let staff = (0..50)
                .map(|index| MediaStaff {
                    name: format!("Staff {index}"),
                    image: Some(format!("https://example.invalid/staff-{index}.png")),
                    ..Default::default()
                })
                .collect();
            w.detail.finish_detail(
                88,
                request,
                Ok(MediaDetail {
                    media: Media {
                        id: 88,
                        ..Default::default()
                    },
                    relations: vec![],
                    characters,
                    staff,
                    cached_at: None,
                    warning: None,
                    unavailable_sections: vec![],
                }),
            );
            w.requeue_detail_covers();
            assert!(w.cover_tickets.contains_key(&(-40, 112, 160)));
            assert!(!w.cover_tickets.contains_key(&(-41, 112, 160)));
            assert!(w.cover_tickets.contains_key(&(-200_040, 112, 160)));
            assert!(!w.cover_tickets.contains_key(&(-200_041, 112, 160)));
        });
    }
}
