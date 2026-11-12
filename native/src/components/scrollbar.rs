use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gpui::{
    AnyElement, App, Bounds, ElementId, ListOffset, ListState, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, RenderOnce, Role, Window, canvas, div, fill, point,
    prelude::*, px, rgb, size,
};

use crate::theme::{ACCENT, EDGE};

const TRACK_WIDTH: f32 = 10.;
const MIN_THUMB: f32 = 28.;
type ScrollChanged = dyn Fn(usize, &mut Window, &mut App);

#[derive(Clone, Copy, Debug, PartialEq)]
struct Thumb {
    top: f32,
    height: f32,
}

fn thumb_geometry(height: f32, maximum: f32, offset: f32) -> Option<Thumb> {
    if !height.is_finite() || !maximum.is_finite() || height <= 0. || maximum <= 0. {
        return None;
    }
    let thumb = (height * height / (height + maximum))
        .max(MIN_THUMB)
        .min(height);
    let top = (offset / maximum).clamp(0., 1.) * (height - thumb);
    Some(Thumb { top, height: thumb })
}

fn offset_for_pointer(pointer_y: f32, height: f32, thumb_height: f32, maximum: f32) -> f32 {
    if height <= thumb_height || maximum <= 0. {
        return 0.;
    }
    ((pointer_y - thumb_height / 2.) / (height - thumb_height)).clamp(0., 1.) * maximum
}

fn move_to_pointer(
    state: &ListState,
    bounds: Bounds<Pixels>,
    pointer_y: Pixels,
    grab: f32,
) -> bool {
    let height = f32::from(bounds.size.height);
    let maximum = f32::from(state.max_offset_for_scrollbar().y);
    if let Some(thumb) = thumb_geometry(height, maximum, 0.) {
        let y = f32::from(pointer_y - bounds.origin.y);
        let offset = offset_for_pointer(
            y + (0.5 - grab) * thumb.height,
            height,
            thumb.height,
            maximum,
        );
        state.set_offset_from_scrollbar(point(px(0.), px(-offset)));
        return offset >= maximum;
    }
    false
}

#[derive(Default)]
struct ScrollNotification {
    pending: RefCell<Option<(ListState, Rc<ScrollChanged>)>>,
    grab: Cell<Option<f32>>,
}

fn notify_scroll(
    state: &ListState,
    changed: Rc<ScrollChanged>,
    notification: Rc<ScrollNotification>,
    window: &mut Window,
) {
    window.refresh();
    if notification
        .pending
        .replace(Some((state.clone(), changed)))
        .is_some()
    {
        return;
    }
    // Layout resolves End and provisional anchors before the callback reads them.
    window.on_next_frame(move |window, _| {
        window.on_next_frame(move |window, cx| {
            let pending = notification.pending.take();
            if let Some((state, changed)) = pending {
                changed(state.logical_scroll_top().item_ix, window, cx);
            }
        });
    });
}

pub(crate) fn replace_list_rows<T, K: Eq + std::hash::Hash>(
    state: &ListState,
    previous: &[T],
    next: &[T],
    height: Pixels,
    key: impl Fn(&T) -> K,
) {
    if state.item_count() == next.len()
        && previous.len() == next.len()
        && previous.iter().zip(next).all(|(a, b)| key(a) == key(b))
    {
        state.remeasure_items(0..state.item_count());
        return;
    }
    let old = state.logical_scroll_top();
    let mut positions = std::collections::HashMap::with_capacity(next.len());
    for (index, row) in next.iter().enumerate() {
        positions.entry(key(row)).or_insert(index);
    }
    let anchor = previous
        .get(old.item_ix)
        .and_then(|row| positions.get(&key(row)))
        .copied();
    let neighbor = || {
        previous[..old.item_ix.min(previous.len())]
            .iter()
            .rev()
            .chain(previous.iter().skip(old.item_ix.saturating_add(1)))
            .find_map(|row| positions.get(&key(row)).copied())
    };
    let offset = ListOffset {
        item_ix: anchor.or_else(neighbor).unwrap_or(0),
        offset_in_item: if anchor.is_some() {
            old.offset_in_item
        } else {
            px(0.)
        },
    };
    state.reset_with_uniform_height(next.len(), height);
    if !previous.is_empty() && old.item_ix >= previous.len() {
        state.scroll_to_end();
    } else {
        state.scroll_to(offset);
    }
}

pub fn list_scrollbar(
    id: impl Into<ElementId>,
    label: &'static str,
    state: ListState,
    changed: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> AnyElement {
    Scrollbar {
        id: id.into(),
        label,
        state,
        changed: Rc::new(changed),
    }
    .into_any_element()
}

#[derive(IntoElement)]
struct Scrollbar {
    id: ElementId,
    label: &'static str,
    state: ListState,
    changed: Rc<ScrollChanged>,
}

impl RenderOnce for Scrollbar {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        let Self {
            id,
            label,
            state,
            changed,
        } = self;
        let notification = window.with_global_id(id.clone(), |id, window| {
            window.with_element_state(id, |state: Option<Rc<ScrollNotification>>, _| {
                let state = state.unwrap_or_default();
                (state.clone(), state)
            })
        });
        render_scrollbar(id, label, state, changed, notification)
    }
}

fn render_scrollbar(
    id: ElementId,
    label: &'static str,
    state: ListState,
    changed: Rc<ScrollChanged>,
    notification: Rc<ScrollNotification>,
) -> gpui::Stateful<gpui::Div> {
    let maximum = f32::from(state.max_offset_for_scrollbar().y);
    let position = (-f32::from(state.scroll_px_offset_for_scrollbar().y))
        .max(0.)
        .min(maximum);
    let percent = if maximum > 0. {
        (position / maximum * 100.).round()
    } else {
        0.
    };
    let keyboard_notification = notification.clone();
    let wheel_notification = notification.clone();
    let keyboard_state = state.clone();
    let keyboard_changed = changed.clone();
    let wheel_state = state.clone();
    let wheel_changed = changed.clone();
    let painted_state = state.clone();
    let painted_changed = changed.clone();
    let painted_notification = notification.clone();
    let mut track = div()
        .id(id)
        .role(Role::ScrollBar)
        .aria_label(label)
        .aria_orientation(gpui::Orientation::Vertical)
        .aria_min_numeric_value(0.)
        .aria_max_numeric_value(100.)
        .aria_numeric_value(percent as f64)
        .aria_numeric_value_step(1.)
        .aria_value(format!("{percent:.0}%"))
        .focusable()
        .tab_stop(maximum > 0.)
        .focus_visible(|style| style.border_1().border_color(rgb(ACCENT)))
        .cursor_pointer()
        .occlude()
        .absolute()
        .top_0()
        .bottom_0()
        .right_0()
        .w(px(TRACK_WIDTH))
        .on_scroll(move |event, window, cx| {
            let maximum = wheel_state.max_offset_for_scrollbar().y;
            let delta = event.delta.pixel_delta(px(20.)).y;
            if maximum <= px(0.) || delta == px(0.) {
                return;
            }
            let next =
                (wheel_state.scroll_px_offset_for_scrollbar().y + delta).clamp(-maximum, px(0.));
            wheel_state.set_offset_from_scrollbar(point(px(0.), next));
            notify_scroll(
                &wheel_state,
                wheel_changed.clone(),
                wheel_notification.clone(),
                window,
            );
            cx.stop_propagation();
        })
        .on_key_down(move |event: &gpui::KeyDownEvent, window, cx| {
            let maximum = f32::from(keyboard_state.max_offset_for_scrollbar().y);
            if maximum <= 0. {
                return;
            }
            let current = -f32::from(keyboard_state.scroll_px_offset_for_scrollbar().y);
            let page = f32::from(keyboard_state.viewport_bounds().size.height).max(40.);
            let next = match event.keystroke.key.as_str() {
                "up" | "arrowup" => current - 40.,
                "down" | "arrowdown" => current + 40.,
                "pageup" => current - page,
                "pagedown" => current + page,
                "home" => 0.,
                "end" => maximum,
                _ => return,
            };
            cx.stop_propagation();
            if event.keystroke.key == "end" {
                keyboard_state.scroll_to_end();
            } else if event.keystroke.key == "home" {
                keyboard_state.scroll_to(ListOffset::default());
            } else {
                keyboard_state
                    .set_offset_from_scrollbar(point(px(0.), px(-next.clamp(0., maximum))));
            }
            notify_scroll(
                &keyboard_state,
                keyboard_changed.clone(),
                keyboard_notification.clone(),
                window,
            );
        })
        .child(
            canvas(
                |bounds, window, _| window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal),
                move |bounds: Bounds<Pixels>, hitbox, window, _| {
                    let height = f32::from(bounds.size.height);
                    let maximum = f32::from(painted_state.max_offset_for_scrollbar().y);
                    let offset = -f32::from(painted_state.scroll_px_offset_for_scrollbar().y);
                    if let Some(thumb) = thumb_geometry(height, maximum, offset) {
                        window.paint_quad(fill(
                            Bounds::new(
                                point(bounds.origin.x, bounds.origin.y + px(thumb.top)),
                                size(px(TRACK_WIDTH), px(thumb.height)),
                            ),
                            rgb(EDGE),
                        ));
                    }

                    let down_notification = painted_notification.clone();
                    let down_state = painted_state.clone();
                    let down_changed = painted_changed.clone();
                    window.on_mouse_event(move |event: &MouseDownEvent, phase, window, _| {
                        if !phase.bubble()
                            || event.button != MouseButton::Left
                            || !hitbox.is_hovered(window)
                            || down_state.max_offset_for_scrollbar().y <= px(0.)
                        {
                            return;
                        }
                        let height = f32::from(bounds.size.height);
                        let maximum = f32::from(down_state.max_offset_for_scrollbar().y);
                        let offset = -f32::from(down_state.scroll_px_offset_for_scrollbar().y);
                        let Some(thumb) = thumb_geometry(height, maximum, offset) else {
                            return;
                        };
                        let y = f32::from(event.position.y - bounds.origin.y);
                        let grab = if (thumb.top..=thumb.top + thumb.height).contains(&y) {
                            (y - thumb.top) / thumb.height
                        } else {
                            0.5
                        };
                        down_notification.grab.set(Some(grab));
                        down_state.scrollbar_drag_started();
                        move_to_pointer(&down_state, bounds, event.position.y, grab);
                        notify_scroll(
                            &down_state,
                            down_changed.clone(),
                            down_notification.clone(),
                            window,
                        );
                    });

                    let move_notification = painted_notification.clone();
                    let move_state = painted_state.clone();
                    let move_changed = painted_changed.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                        if !phase.bubble() || !move_state.is_scrollbar_dragging() {
                            return;
                        }
                        if event.pressed_button != Some(MouseButton::Left) {
                            move_notification.grab.set(None);
                            move_state.scrollbar_drag_ended();
                            return;
                        }
                        let Some(grab) = move_notification.grab.get() else {
                            return;
                        };
                        move_to_pointer(&move_state, bounds, event.position.y, grab);
                        notify_scroll(
                            &move_state,
                            move_changed.clone(),
                            move_notification.clone(),
                            window,
                        );
                        cx.stop_propagation();
                    });

                    let up_state = painted_state.clone();
                    let up_changed = painted_changed.clone();
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                        if !phase.bubble()
                            || event.button != MouseButton::Left
                            || !up_state.is_scrollbar_dragging()
                        {
                            return;
                        }
                        let Some(grab) = painted_notification.grab.take() else {
                            return;
                        };
                        let at_end = move_to_pointer(&up_state, bounds, event.position.y, grab);
                        up_state.scrollbar_drag_ended();
                        if at_end {
                            up_state.scroll_to_end();
                        }
                        notify_scroll(
                            &up_state,
                            up_changed.clone(),
                            painted_notification.clone(),
                            window,
                        );
                        cx.stop_propagation();
                    });
                },
            )
            .absolute()
            .inset_0(),
        );
    use gpui::accesskit::{Action, ActionData};
    for action in [Action::SetValue, Action::Increment, Action::Decrement] {
        let state = state.clone();
        let changed = changed.clone();
        let notification = notification.clone();
        track = track.on_a11y_action(action, move |data, window, _| {
            let maximum = f32::from(state.max_offset_for_scrollbar().y);
            if maximum <= 0. {
                return;
            }
            let current = -f32::from(state.scroll_px_offset_for_scrollbar().y) / maximum * 100.;
            let percent = match (action, data) {
                (Action::SetValue, Some(ActionData::NumericValue(value))) if value.is_finite() => {
                    *value as f32
                }
                (Action::Increment, _) => current + 1.,
                (Action::Decrement, _) => current - 1.,
                _ => return,
            }
            .clamp(0., 100.);
            if percent >= 100. {
                state.scroll_to_end();
            } else {
                state.set_offset_from_scrollbar(point(px(0.), px(-maximum * percent / 100.)));
            }
            notify_scroll(&state, changed.clone(), notification.clone(), window);
        });
    }
    track
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::ListAlignment;
    use std::cell::Cell;

    struct InteractiveRows {
        state: ListState,
        covered: bool,
    }

    impl gpui::Render for InteractiveRows {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            div()
                .relative()
                .size_full()
                .child(
                    gpui::list(self.state.clone(), |_, _, _| div().h(px(90.)).into_any())
                        .size_full(),
                )
                .child(list_scrollbar(
                    "interactive-track",
                    "Rows",
                    self.state.clone(),
                    |_, _, _| {},
                ))
                .when(self.covered, |view| {
                    view.child(div().absolute().inset_0().occlude())
                })
        }
    }

    #[gpui::test]
    fn thumb_grab_preserves_position_and_click_focuses_the_track(cx: &mut gpui::TestAppContext) {
        let state =
            ListState::new(250, ListAlignment::Top, px(0.)).with_uniform_item_height(px(90.));
        let (_, cx) = cx.add_window_view(|_, _| InteractiveRows {
            state: state.clone(),
            covered: false,
        });
        cx.run_until_parked();
        state.scroll_to(ListOffset {
            item_ix: 100,
            offset_in_item: px(0.),
        });
        cx.update(|window, app| {
            window.refresh();
            window.draw(app).clear(app);
        });
        let before = state.scroll_px_offset_for_scrollbar().y;
        let position = cx.update(|window, _| {
            let thumb = thumb_geometry(
                f32::from(window.viewport_size().height),
                f32::from(state.max_offset_for_scrollbar().y),
                -f32::from(before),
            )
            .unwrap();
            point(window.viewport_size().width - px(5.), px(thumb.top + 2.))
        });
        cx.simulate_mouse_down(position, MouseButton::Left, gpui::Modifiers::default());
        assert!((f32::from(state.scroll_px_offset_for_scrollbar().y - before)).abs() < 0.01);
        cx.simulate_mouse_move(position, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(position, MouseButton::Left, gpui::Modifiers::default());
        assert!((f32::from(state.scroll_px_offset_for_scrollbar().y - before)).abs() < 0.01);
        cx.simulate_keystrokes("down");
        assert!(state.scroll_px_offset_for_scrollbar().y < before);
    }

    #[gpui::test]
    fn missing_mouse_up_does_not_leave_the_scroll_range_frozen(cx: &mut gpui::TestAppContext) {
        let state =
            ListState::new(250, ListAlignment::Top, px(0.)).with_uniform_item_height(px(90.));
        let (_, cx) = cx.add_window_view(|_, _| InteractiveRows {
            state: state.clone(),
            covered: false,
        });
        cx.run_until_parked();
        let position = cx.update(|window, _| point(window.viewport_size().width - px(5.), px(2.)));
        cx.simulate_mouse_down(position, MouseButton::Left, gpui::Modifiers::default());
        assert!(state.is_scrollbar_dragging());
        cx.simulate_mouse_move(position, None, gpui::Modifiers::default());
        assert!(!state.is_scrollbar_dragging());
    }

    #[gpui::test]
    fn covered_track_ignores_pointer_input(cx: &mut gpui::TestAppContext) {
        let state =
            ListState::new(250, ListAlignment::Top, px(0.)).with_uniform_item_height(px(90.));
        let (_, cx) = cx.add_window_view(|_, _| InteractiveRows {
            state: state.clone(),
            covered: true,
        });
        cx.run_until_parked();
        let position =
            cx.update(|window, _| point(window.viewport_size().width - px(5.), px(200.)));
        cx.simulate_click(position, gpui::Modifiers::default());
        assert_eq!(state.scroll_px_offset_for_scrollbar().y, px(0.));
        assert!(!state.is_scrollbar_dragging());
    }

    #[test]
    fn end_request_survives_row_changes_before_layout() {
        let rows = ListState::new(3, ListAlignment::Top, px(0.)).with_uniform_item_height(px(90.));
        rows.scroll_to_end();
        replace_list_rows(&rows, &[1, 2, 3], &[2, 3, 4, 5], px(90.), |id| *id);
        assert_eq!(
            rows.logical_scroll_top(),
            ListOffset {
                item_ix: 4,
                offset_in_item: px(0.)
            }
        );
        replace_list_rows(&rows, &[2, 3, 4, 5], &[], px(90.), |id| *id);
        assert_eq!(rows.logical_scroll_top(), ListOffset::default());
    }

    #[test]
    fn removed_anchor_uses_a_neighbor_and_unchanged_rows_keep_drag_state() {
        let state = ListState::new(4, ListAlignment::Top, px(0.)).with_uniform_item_height(px(90.));
        let previous = ["header", "show", "release", "tail"];
        let next = ["header", "show", "tail"];
        state.scroll_to(ListOffset {
            item_ix: 2,
            offset_in_item: px(17.),
        });
        replace_list_rows(&state, &previous, &next, px(90.), |key| *key);
        assert_eq!(
            state.logical_scroll_top(),
            ListOffset {
                item_ix: 1,
                offset_in_item: px(0.)
            }
        );
        state.scroll_to(ListOffset {
            item_ix: 2,
            offset_in_item: px(11.),
        });
        state.scrollbar_drag_started();
        replace_list_rows(&state, &next, &next, px(90.), |key| *key);
        assert!(state.is_scrollbar_dragging());
        assert_eq!(
            state.logical_scroll_top(),
            ListOffset {
                item_ix: 2,
                offset_in_item: px(11.)
            }
        );
        state.scrollbar_drag_ended();
        replace_list_rows(&state, &next, &["tail"], px(90.), |key| *key);
        assert_eq!(
            state.logical_scroll_top(),
            ListOffset {
                item_ix: 0,
                offset_in_item: px(11.)
            }
        );
        replace_list_rows(&state, &["tail"], &[], px(90.), |key| *key);
        assert_eq!(state.logical_scroll_top(), ListOffset::default());
    }

    #[test]
    fn scrollbar_exposes_value_actions() {
        use gpui::{Element, accesskit::Action};
        let state =
            ListState::new(250, ListAlignment::Top, px(0.)).with_uniform_item_height(px(90.));
        let track = render_scrollbar(
            "a11y-track".into(),
            "Rows",
            state,
            Rc::new(|_, _, _| {}),
            Rc::default(),
        );
        let mut node = gpui::accesskit::Node::new(Role::ScrollBar);
        track.write_a11y_info(&mut node);
        for action in [Action::SetValue, Action::Increment, Action::Decrement] {
            assert!(node.supports_action(action));
        }
        assert_eq!(node.numeric_value(), Some(0.));
        assert_eq!(node.min_numeric_value(), Some(0.));
        assert_eq!(node.max_numeric_value(), Some(100.));
        assert_eq!(node.orientation(), Some(gpui::Orientation::Vertical));
    }

    #[gpui::test]
    fn track_callbacks_coalesce_across_redraws(cx: &mut gpui::TestAppContext) {
        use gpui::{Context, Render, ScrollDelta, ScrollEvent};

        struct Rows(ListState, Rc<Cell<usize>>);
        impl Render for Rows {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let calls = self.1.clone();
                div()
                    .relative()
                    .size_full()
                    .child(
                        gpui::list(self.0.clone(), |_, _, _| div().h(px(90.)).into_any())
                            .size_full(),
                    )
                    .child(list_scrollbar(
                        "coalesced-track",
                        "Rows",
                        self.0.clone(),
                        move |_, _, _| {
                            calls.set(calls.get() + 1);
                        },
                    ))
            }
        }

        let state =
            ListState::new(250, ListAlignment::Top, px(0.)).with_uniform_item_height(px(90.));
        let calls = Rc::new(Cell::new(0));
        let (view, cx) = cx.add_window_view(|_, _| Rows(state.clone(), calls.clone()));
        cx.run_until_parked();
        let position =
            cx.update(|window, _| point(window.viewport_size().width - px(5.), px(150.)));
        for _ in 0..2 {
            cx.update(|window, app| {
                view.update(app, |_, cx| cx.notify());
                window.draw(app).clear(app);
            });
            for _ in 0..20 {
                cx.simulate_event(ScrollEvent {
                    position,
                    delta: ScrollDelta::Lines(point(0., -1.)),
                    ..Default::default()
                });
            }
        }
        assert!(state.logical_scroll_top().item_ix > 0);
        assert_eq!(calls.get(), 0);
        cx.update(|window, app| {
            window.simulate_next_frame(app);
        });
        assert_eq!(calls.get(), 0);
        cx.update(|window, app| {
            window.simulate_next_frame(app);
        });
        assert_eq!(calls.get(), 1);
        cx.simulate_event(ScrollEvent {
            position,
            delta: ScrollDelta::Lines(point(0., -1.)),
            ..Default::default()
        });
        cx.update(|window, app| {
            window.simulate_next_frame(app);
        });
        cx.update(|window, app| {
            window.simulate_next_frame(app);
        });
        assert_eq!(calls.get(), 2);
    }

    #[gpui::test]
    fn callbacks_use_latest_input_and_are_independent(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        let state =
            ListState::new(100, ListAlignment::Top, px(0.)).with_uniform_item_height(px(90.));
        let notification = Rc::<ScrollNotification>::default();
        let first_calls = Rc::new(Cell::new(0));
        let latest = Rc::new(Cell::new(None));
        let independent = Rc::new(Cell::new(0));
        cx.update(|window, _| {
            for _ in 0..50 {
                let calls = first_calls.clone();
                notify_scroll(
                    &state,
                    Rc::new(move |_, _, _| calls.set(calls.get() + 1)),
                    notification.clone(),
                    window,
                );
            }
            let calls = independent.clone();
            notify_scroll(
                &state,
                Rc::new(move |_, _, _| calls.set(calls.get() + 1)),
                Rc::default(),
                window,
            );
        });
        cx.update(|window, app| {
            window.simulate_next_frame(app);
        });
        state.scroll_to(ListOffset {
            item_ix: 40,
            offset_in_item: px(7.),
        });
        cx.update(|window, _| {
            let latest = latest.clone();
            notify_scroll(
                &state,
                Rc::new(move |first, _, _| latest.set(Some(first))),
                notification,
                window,
            );
        });
        cx.update(|window, app| {
            window.simulate_next_frame(app);
        });
        assert_eq!(first_calls.get(), 0);
        assert_eq!(latest.get(), Some(40));
        assert_eq!(independent.get(), 1);
    }

    #[gpui::test]
    fn wheel_on_track_scrolls_in_both_directions(cx: &mut gpui::TestAppContext) {
        use gpui::{Context, Render, ScrollDelta, ScrollEvent};

        struct Rows(ListState);
        impl Render for Rows {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .relative()
                    .size_full()
                    .child(
                        gpui::list(self.0.clone(), |_, _, _| {
                            div().h(px(90.)).w_full().into_any()
                        })
                        .size_full(),
                    )
                    .child(list_scrollbar(
                        "wheel-track",
                        "Rows",
                        self.0.clone(),
                        |_, _, _| {},
                    ))
            }
        }

        let state =
            ListState::new(250, ListAlignment::Top, px(0.)).with_uniform_item_height(px(90.));
        let (_, cx) = cx.add_window_view(|_, _| Rows(state.clone()));
        cx.run_until_parked();
        let point_on_track =
            cx.update(|window, _| point(window.viewport_size().width - px(5.), px(150.)));
        cx.simulate_event(ScrollEvent {
            position: point_on_track,
            delta: ScrollDelta::Lines(point(0., -4.)),
            ..Default::default()
        });
        assert!(state.scroll_px_offset_for_scrollbar().y < px(0.));
        cx.run_until_parked();
        cx.simulate_event(ScrollEvent {
            position: point_on_track,
            delta: ScrollDelta::Pixels(point(px(0.), px(10_000.))),
            ..Default::default()
        });
        assert_eq!(state.scroll_px_offset_for_scrollbar().y, px(0.));
    }

    #[gpui::test]
    fn height_hints_survive_first_layout_and_resize(cx: &mut gpui::TestAppContext) {
        use gpui::{Context, Render};
        use std::cell::Cell;

        let cx = cx.add_empty_window();
        let state =
            ListState::new(250, ListAlignment::Top, px(0.)).with_uniform_item_height(px(90.));
        let renders = Rc::new(Cell::new(0));
        struct HintedList(ListState, f32, Rc<Cell<usize>>);
        impl Render for HintedList {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let height = self.1;
                let renders = self.2.clone();
                gpui::list(self.0.clone(), move |_, _, _| {
                    renders.set(renders.get() + 1);
                    div().h(px(height)).w_full().into_any()
                })
                .w_full()
                .h_full()
            }
        }
        cx.draw(point(px(0.), px(0.)), size(px(100.), px(360.)), |_, cx| {
            cx.new(|_| HintedList(state.clone(), 90., renders.clone()))
                .into_any_element()
        });
        assert_eq!(state.max_offset_for_scrollbar().y, px(22_140.));
        assert!(renders.get() < 20);
        renders.set(0);
        cx.draw(point(px(0.), px(0.)), size(px(80.), px(360.)), |_, cx| {
            cx.new(|_| HintedList(state.clone(), 120., renders.clone()))
                .into_any_element()
        });
        assert_eq!(state.bounds_for_item(0).unwrap().size.height, px(120.));
        assert!(state.max_offset_for_scrollbar().y >= px(22_140.));
        assert!(renders.get() < 20);
    }

    #[test]
    fn thumb_reaches_both_ends_for_large_unmeasured_lists() {
        for count in [250, 2_000, 10_000] {
            let height = 410.;
            let maximum = count as f32 * 90. - height;
            let first = thumb_geometry(height, maximum, 0.).unwrap();
            let last = thumb_geometry(height, maximum, maximum).unwrap();
            assert_eq!(first.top, 0.);
            assert!(first.height >= MIN_THUMB);
            assert!((last.top + last.height - height).abs() < 0.001);
            assert_eq!(
                offset_for_pointer(height, height, last.height, maximum),
                maximum
            );
            assert_eq!(offset_for_pointer(0., height, first.height, maximum), 0.);
        }
    }

    #[test]
    fn pointer_mapping_is_bounded_and_monotonic_as_heights_refine() {
        for maximum in [90_000., 120_000.] {
            let thumb = thumb_geometry(600., maximum, 0.).unwrap();
            let positions = [-100., 0., 100., 300., 600., 700.]
                .map(|y| offset_for_pointer(y, 600., thumb.height, maximum));
            assert_eq!(positions[0], 0.);
            assert_eq!(positions[5], maximum);
            assert!(positions.windows(2).all(|pair| pair[0] <= pair[1]));
        }
        assert!(thumb_geometry(600., 0., 0.).is_none());
    }

    #[test]
    fn uniform_hints_give_unrendered_rows_a_scroll_range() {
        let list =
            ListState::new(10_000, ListAlignment::Top, px(0.)).with_uniform_item_height(px(90.));
        assert_eq!(list.item_count(), 10_000);
        assert!(list.max_offset_for_scrollbar().y >= px(900_000.));
    }

    #[gpui::test]
    fn end_callback_uses_post_layout_anchor_for_variable_rows(cx: &mut gpui::TestAppContext) {
        use gpui::{Context, Render};

        struct Rows(ListState);
        impl Render for Rows {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                gpui::list(self.0.clone(), |index, _, _| {
                    div()
                        .h(px(if index >= 18 { 150. } else { 90. }))
                        .w_full()
                        .into_any()
                })
                .w_full()
                .h_full()
            }
        }

        let cx = cx.add_empty_window();
        let state =
            ListState::new(20, ListAlignment::Top, px(0.)).with_uniform_item_height(px(90.));
        cx.draw(point(px(0.), px(0.)), size(px(200.), px(300.)), |_, cx| {
            cx.new(|_| Rows(state.clone())).into_any_element()
        });
        state.scroll_to_end();
        assert_eq!(state.logical_scroll_top().item_ix, 20);
        let seen = Rc::new(Cell::new(None));
        let changed: Rc<ScrollChanged> = Rc::new({
            let seen = seen.clone();
            move |first, _, _| seen.set(Some(first))
        });
        cx.update(|window, _| {
            notify_scroll(&state, changed, Rc::default(), window);
            assert_eq!(seen.get(), None);
        });
        cx.update(|window, app| assert!(window.simulate_next_frame(app) > 0));
        assert_eq!(seen.get(), None);
        cx.draw(point(px(0.), px(0.)), size(px(200.), px(300.)), |_, cx| {
            cx.new(|_| Rows(state.clone())).into_any_element()
        });
        cx.update(|window, app| assert!(window.simulate_next_frame(app) > 0));
        assert_eq!(seen.get(), Some(state.logical_scroll_top().item_ix));
        assert!(seen.get().unwrap() < 20);
        assert_eq!(state.bounds_for_item(19).unwrap().size.height, px(150.));
    }
}
