use super::covers::CoverOwner;
use super::*;
use gpui::{Entity, TestAppContext, VisualTestContext, WindowVisibility, px, size};
use kurisu_core::models::ListEntry;
use kurisu_core::{Platform, UpdateChannel};
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
    let path = std::env::temp_dir().join(format!(
        "kurisu-cover-layout-{}-{nonce}",
        std::process::id()
    ));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let core = Core::open(
        path.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    (core, runtime, FixtureDir(path))
}

fn settle(cx: &mut TestAppContext, runtime: &tokio::runtime::Runtime) {
    for _ in 0..16 {
        cx.run_until_parked();
        runtime.block_on(async { tokio::task::yield_now().await });
    }
}

fn stage05_entries(count: i64) -> Vec<ListEntry> {
    (1..=count)
        .map(|id| ListEntry {
            media_id: id,
            status: "CURRENT".into(),
            progress: 3,
            media: Some(Media {
                id,
                title_english: Some(format!(
                    "Series {id:05} {}",
                    "夜空 Long Title ".repeat(if id % 7 == 0 { 100 } else { 0 })
                )),
                episodes: Some(24),
                cover_medium: (id % 13 != 0).then(|| format!("invalid-{id}")),
                ..Default::default()
            }),
            ..Default::default()
        })
        .collect()
}

fn stage05_visible(w: &Workspace) -> HashSet<(i64, u32, u32)> {
    let viewport = w.list_widget.viewport_bounds();
    w.list
        .visible()
        .iter()
        .enumerate()
        .filter_map(|(i, id)| {
            w.list
                .entry(*id)
                .and_then(|entry| entry.media.as_ref())
                .and_then(|media| media.cover_medium.as_ref())?;
            w.list_widget.bounds_for_item(i + 1).and_then(|bounds| {
                (bounds.top() < viewport.bottom() && bounds.bottom() > viewport.top())
                    .then_some((*id, 46, 63))
            })
        })
        .collect()
}

fn frames(cx: &mut VisualTestContext, count: usize) -> usize {
    let mut callbacks = 0;
    for _ in 0..count {
        callbacks += cx.update(|window, app| {
            window.draw(app).clear(app);
            window.simulate_next_frame(app)
        });
        cx.run_until_parked();
    }
    callbacks
}

fn seed(w: &Entity<Workspace>, cx: &mut VisualTestContext, count: i64) {
    w.update_in(cx, |w, _, cx| {
        w.auth_intent += 1;
        w.profile = Some(User {
            id: 47,
            ..Default::default()
        });
        w.list.replace(stage05_entries(count), 0);
        w.reset_list_widget();
        cx.notify();
    });
    cx.run_until_parked();
    frames(cx, 4);
}

fn exact(w: &Entity<Workspace>, cx: &mut VisualTestContext, label: &str) {
    w.read_with(cx, |w, _| {
        let expected = stage05_visible(w);
        let actual = w.cover_regions.get(&CoverOwner::Route(Route::MyList))
            .map(|r| r.audit_visible().clone()).unwrap_or_default();
        eprintln!("{label}: top={:?} expected={expected:?} actual={actual:?} revision={} reconciled={} pending={}", w.list_widget.logical_scroll_top(), w.list_cover_revision, w.list_cover_reconciled, w.list_cover_refresh_scheduled);
        assert_eq!(actual, expected, "Measured visible owners: {label}");
        if let Some(region) = w.cover_regions.get(&CoverOwner::Route(Route::MyList)) {
            assert!(region.audit_nearby().len() <= 20);
            assert!(region.audit_nearby().is_disjoint(region.audit_visible()));
        }
        assert!(!w.list_cover_refresh_scheduled, "Reconciliation did not settle");
    });
}

#[gpui::test]
fn ownership_membership_bursts_and_idle(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (w, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    cx.simulate_resize(size(px(800.), px(600.)));
    for count in [2, 250, 2000, 10000] {
        seed(&w, cx, count);
        exact(&w, cx, "initial");
        w.update_in(cx, |w, _, cx| {
            for index in 0..64 {
                w.list.set_query(
                    if index % 2 == 0 {
                        "missing".into()
                    } else {
                        "Series".into()
                    },
                    0,
                );
                w.reset_list_widget();
            }
            cx.notify();
        });
        cx.run_until_parked();
        let callbacks = frames(cx, 5);
        exact(&w, cx, "64 membership revisions");
        assert!(callbacks <= 3, "Burst callback count {callbacks}");
        assert_eq!(frames(cx, 20), 0, "Idle list schedules more callbacks");
        for status in ["COMPLETED", "CURRENT"] {
            w.update_in(cx, |w, _, cx| {
                w.list.set_filter(status, 0);
                w.reset_list_widget();
                cx.notify();
            });
            cx.run_until_parked();
            frames(cx, 4);
            exact(&w, cx, status);
        }
        for sort in SortKey::ALL {
            w.update_in(cx, |w, _, cx| {
                w.list.set_sort(sort, 0);
                w.reset_list_widget();
                cx.notify();
            });
            cx.run_until_parked();
            frames(cx, 4);
            exact(&w, cx, "sort");
        }
    }
}

#[gpui::test]
fn ownership_after_resize(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (w, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    cx.simulate_resize(size(px(800.), px(600.)));
    seed(&w, cx, 2000);
    exact(&w, cx, "before resize");
    cx.simulate_resize(size(px(1100.), px(1400.)));
    cx.run_until_parked();
    frames(cx, 5);
    exact(&w, cx, "after resize without repair input");
}

#[gpui::test]
fn ownership_after_scale(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (w, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    cx.simulate_resize(size(px(800.), px(600.)));
    seed(&w, cx, 2000);
    exact(&w, cx, "before scale");
    cx.simulate_scale_factor_change(1.25);
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(240));
    cx.run_until_parked();
    frames(cx, 5);
    exact(&w, cx, "after scale poll without repair input");
}

#[gpui::test]
fn ownership_after_scoped_height_change(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (w, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    cx.simulate_resize(size(px(800.), px(600.)));
    seed(&w, cx, 2000);
    w.update_in(cx, |w, window, cx| {
        w.list_widget.scroll_to(gpui::ListOffset {
            item_ix: 101,
            offset_in_item: px(13.),
        });
        w.mark_list_cover_refresh();
        w.schedule_list_cover_refresh(window, cx);
        cx.notify();
    });
    cx.run_until_parked();
    frames(cx, 4);
    exact(&w, cx, "before scoped height");
    w.update_in(cx, |w, _, cx| {
        let ids: Vec<_> = (101..=110).collect();
        let entries = ids
            .iter()
            .map(|id| {
                let mut e = w.list.entry(*id).unwrap().clone();
                e.media.as_mut().unwrap().title_english =
                    Some(format!("Series {id:05} {}", "夜空 Long title ".repeat(100)));
                Some(e)
            })
            .collect();
        let change = w.list.apply_scoped(&ids, entries, 0);
        assert!(!change.order_changed);
        w.apply_list_change(change);
        cx.notify();
    });
    cx.run_until_parked();
    frames(cx, 5);
    w.read_with(cx, |w, _| {
        assert_eq!(w.list_widget.logical_scroll_top().item_ix, 101);
        assert_eq!(w.list_widget.logical_scroll_top().offset_in_item, px(13.));
    });
    exact(&w, cx, "after scoped height without repair input");
}

#[gpui::test]
fn ownership_route_account_footer(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (w, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    seed(&w, cx, 250);
    w.update_in(cx, |w, window, cx| {
        w.list.set_query("Series 00249".into(), 0);
        w.reset_list_widget();
        w.schedule_list_cover_refresh(window, cx);
        w.switch_route(Route::Settings, window, cx);
    });
    frames(cx, 5);
    w.read_with(cx, |w, _| {
        assert!(
            !w.cover_regions
                .contains_key(&CoverOwner::Route(Route::MyList))
        );
        assert!(!w.list_cover_refresh_scheduled);
    });
    w.update_in(cx, |w, window, cx| {
        w.switch_route(Route::MyList, window, cx)
    });
    cx.run_until_parked();
    frames(cx, 5);
    exact(&w, cx, "route return");
    w.update_in(cx, |w, window, cx| {
        w.list.set_query("Series".into(), 0);
        w.reset_list_widget();
        w.schedule_list_cover_refresh(window, cx);
        w.set_profile(
            Some(User {
                id: 48,
                offline: true,
                ..Default::default()
            }),
            window,
            cx,
        );
    });
    frames(cx, 5);
    w.read_with(cx, |w, _| {
        assert!(w.cover_tickets.is_empty());
        assert!(w.cover_images.is_empty());
        assert!(!w.list_cover_refresh_scheduled);
    });
    seed(&w, cx, 250);
    cx.simulate_resize(size(px(800.), px(40.)));
    w.update_in(cx, |w, window, cx| {
        w.list_widget.scroll_to(gpui::ListOffset {
            item_ix: 251,
            offset_in_item: px(5.),
        });
        w.mark_list_cover_refresh();
        w.schedule_list_cover_refresh(window, cx);
        cx.notify();
    });
    cx.run_until_parked();
    frames(cx, 5);
    exact(&w, cx, "footer only");
    w.read_with(cx, |w, _| assert!(w.cover_tickets.is_empty()));
}

#[gpui::test]
fn footer_timer_account_hidden_overlap(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    let (w, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    settle(cx, &runtime);
    w.update_in(cx, |w, window, cx| {
        w.profile = Some(User {
            id: 47,
            ..Default::default()
        });
        w.sync_request = 10;
        w.mark_list_synced(window, cx);
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    w.update_in(cx, |w, window, cx| {
        w.set_profile(
            Some(User {
                id: 48,
                offline: true,
                ..Default::default()
            }),
            window,
            cx,
        );
        assert!(w.list_sync_flash.is_none());
        w.mark_list_synced(window, cx);
    });
    cx.simulate_visibility_change(WindowVisibility::Hidden);
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    w.read_with(cx, |w, _| {
        assert_eq!(w.list_sync_flash, Some(w.sync_request))
    });
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    w.read_with(cx, |w, _| assert!(w.list_sync_flash.is_none()));
    cx.executor().advance_clock(Duration::from_secs(30));
    cx.run_until_parked();
    w.read_with(cx, |w, _| assert!(w.list_sync_flash.is_none()));
}

struct NestedHost {
    workspace: Entity<Workspace>,
    _observe: Subscription,
}

impl Render for NestedHost {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let sidebar = if f32::from(window.viewport_size().width) <= 960. {
            216.
        } else {
            244.
        };
        gpui::div()
            .size_full()
            .flex()
            .child(gpui::div().w(px(sidebar)).h_full())
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.workspace.clone()),
            )
    }
}

#[gpui::test]
fn nested_resize_repaints_promoted_covers(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let notifications = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let observed = notifications.clone();
    let (host, cx) = cx.add_window_view(|window, cx| {
        let workspace = cx.new(|cx| Workspace::new(core, window, cx));
        let observer = observed.clone();
        let subscription = cx.observe_in(&workspace, window, move |_, _, _, _| {
            observer.set(observer.get() + 1);
        });
        NestedHost {
            workspace,
            _observe: subscription,
        }
    });
    let workspace = host.read_with(cx, |host, _| host.workspace.clone());
    cx.simulate_resize(size(px(800.), px(600.)));
    seed(&workspace, cx, 250);
    cx.simulate_resize(size(px(1100.), px(1400.)));
    cx.run_until_parked();
    let before_reconciliation = notifications.get();
    let callbacks = frames(cx, 5);
    workspace.read_with(cx, |workspace, _| {
        let visible = workspace.cover_regions[&CoverOwner::Route(Route::MyList)].audit_visible();
        assert_eq!(visible, &stage05_visible(workspace));
        assert_eq!(workspace.list_cover_window_size, size(px(1100.), px(1400.)));
        assert_eq!(
            workspace.list_cover_bounds,
            Some(workspace.list_widget.viewport_bounds())
        );
        assert!(!workspace.list_cover_refresh_scheduled);
    });
    assert!(
        notifications.get() > before_reconciliation,
        "Measured cover promotion must invalidate the nested Workspace"
    );
    assert!(callbacks <= 3);
    assert_eq!(frames(cx, 20), 0);
}
