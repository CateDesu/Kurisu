use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, ElementId, ListOffset, ListState, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Role, Window, canvas, div, fill, point, prelude::*, px,
    rgb, size,
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

fn move_to_pointer(state: &ListState, bounds: Bounds<Pixels>, pointer_y: Pixels) {
    let height = f32::from(bounds.size.height);
    let maximum = f32::from(state.max_offset_for_scrollbar().y);
    if let Some(thumb) = thumb_geometry(height, maximum, 0.) {
        let y = f32::from(pointer_y - bounds.origin.y);
        let offset = offset_for_pointer(y, height, thumb.height, maximum);
        state.set_offset_from_scrollbar(point(px(0.), px(-offset)));
    }
}

fn notify_scroll(state: &ListState, changed: Rc<ScrollChanged>, window: &mut Window) {
    window.refresh();
    let state = state.clone();
    window.on_next_frame(move |window, _| {
        window.on_next_frame(move |window, cx| {
            changed(state.logical_scroll_top().item_ix, window, cx);
        });
    });
}

pub fn list_scrollbar(
    id: impl Into<ElementId>,
    label: &'static str,
    state: ListState,
    changed: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let changed: Rc<ScrollChanged> = Rc::new(changed);
    let maximum = f32::from(state.max_offset_for_scrollbar().y);
    let position = (-f32::from(state.scroll_px_offset_for_scrollbar().y))
        .max(0.)
        .min(maximum);
    let percent = if maximum > 0. {
        (position / maximum * 100.).round()
    } else {
        0.
    };
    let keyboard_state = state.clone();
    let keyboard_changed = changed.clone();
    let wheel_state = state.clone();
    let wheel_changed = changed.clone();
    let painted_state = state.clone();
    div()
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
            notify_scroll(&wheel_state, wheel_changed.clone(), window);
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
            notify_scroll(&keyboard_state, keyboard_changed.clone(), window);
        })
        .child(
            canvas(
                |_, _, _| (),
                move |bounds: Bounds<Pixels>, _, window, _| {
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

                    let down_state = painted_state.clone();
                    let down_changed = changed.clone();
                    window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                        if !phase.bubble()
                            || event.button != MouseButton::Left
                            || !bounds.contains(&event.position)
                            || down_state.max_offset_for_scrollbar().y <= px(0.)
                        {
                            return;
                        }
                        down_state.scrollbar_drag_started();
                        move_to_pointer(&down_state, bounds, event.position.y);
                        notify_scroll(&down_state, down_changed.clone(), window);
                        cx.stop_propagation();
                    });

                    let move_state = painted_state.clone();
                    let move_changed = changed.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                        if !phase.bubble()
                            || !event.dragging()
                            || !move_state.is_scrollbar_dragging()
                        {
                            return;
                        }
                        move_to_pointer(&move_state, bounds, event.position.y);
                        notify_scroll(&move_state, move_changed.clone(), window);
                        cx.stop_propagation();
                    });

                    let up_state = painted_state.clone();
                    let up_changed = changed.clone();
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                        if !phase.bubble()
                            || event.button != MouseButton::Left
                            || !up_state.is_scrollbar_dragging()
                        {
                            return;
                        }
                        let height = f32::from(bounds.size.height);
                        let maximum = f32::from(up_state.max_offset_for_scrollbar().y);
                        let at_end = thumb_geometry(height, maximum, 0.).is_some_and(|thumb| {
                            f32::from(event.position.y - bounds.origin.y)
                                >= height - thumb.height / 2.
                        });
                        move_to_pointer(&up_state, bounds, event.position.y);
                        up_state.scrollbar_drag_ended();
                        if at_end {
                            up_state.scroll_to_end();
                        }
                        notify_scroll(&up_state, up_changed.clone(), window);
                        cx.stop_propagation();
                    });
                },
            )
            .absolute()
            .inset_0(),
        )
        .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::ListAlignment;
    use std::cell::Cell;

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
            notify_scroll(&state, changed, window);
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
