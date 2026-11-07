pub mod covers;
pub mod input;
pub mod scrollbar;
pub mod variable_list;

use gpui::{Div, ElementId, FontWeight, Role, SharedString, Stateful, div, prelude::*, px, rgb};

use crate::theme::{ACCENT, DIM, EDGE, INK, PANEL_2, mono_font, serif_font};

fn disable_control_node(node: &mut gpui::accesskit::Node) {
    node.set_disabled();
    node.remove_action(gpui::AccessibleAction::Click);
    node.remove_action(gpui::AccessibleAction::Focus);
}

pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    disabled: bool,
) -> Stateful<Div> {
    let label = label.into();
    let mut button = div()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .tab_stop(!disabled)
        .px(px(12.))
        .py(px(7.))
        .border_1()
        .border_color(rgb(EDGE))
        .bg(rgb(PANEL_2))
        .text_color(rgb(if disabled { DIM } else { INK }))
        .child(label);
    if !disabled {
        button = button
            .focusable()
            .focus_visible(|style| style.border_color(rgb(ACCENT)))
            .cursor_pointer()
            .hover(|s| s.border_color(rgb(ACCENT)));
    } else {
        button = button
            .aria_description("Unavailable")
            .a11y_synthetic_children(|builder| disable_control_node(builder.parent_node()))
            .capture_any_mouse_down(|_, _, cx| cx.stop_propagation());
    }
    button
}

pub fn heading(title: impl Into<SharedString>) -> Div {
    div()
        .font(serif_font())
        .font_weight(FontWeight::NORMAL)
        .text_size(px(19.))
        .text_color(rgb(INK))
        .child(title.into())
}

pub fn page_heading(title: impl Into<SharedString>) -> Stateful<Div> {
    let title = title.into();
    div()
        .id("page-heading")
        .role(Role::Heading)
        .aria_level(1)
        .aria_label(title.clone())
        .flex()
        .items_center()
        .font(serif_font())
        .font_weight(FontWeight::NORMAL)
        .text_size(px(30.))
        .text_color(rgb(INK))
        .child(title)
        .child(div().text_color(rgb(ACCENT)).child("."))
}

pub fn eyebrow(label: impl Into<SharedString>) -> Div {
    tracked_mono(label.into().to_string().to_uppercase(), 12., 12. * 0.17).text_color(rgb(DIM))
}

pub fn tracked_mono(label: impl Into<SharedString>, size: f32, spacing: f32) -> Div {
    let label = label.into();
    let mut row = div()
        .flex()
        .items_baseline()
        .gap(px(spacing))
        .font(mono_font())
        .text_size(px(size));
    for character in label.to_string().chars() {
        row = row.child(div().child(character.to_string()));
    }
    row
}

pub fn dialog(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .role(Role::Dialog)
        .aria_label(title.into())
        .border_1()
        .border_color(rgb(EDGE))
        .bg(rgb(crate::theme::PANEL))
        .p(px(24.))
}

pub fn select(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    select_with_disabled(id, label, false)
}

pub fn select_with_disabled(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    disabled: bool,
) -> Stateful<Div> {
    let mut control = div()
        .id(id)
        .role(Role::ComboBox)
        .aria_label(label.into())
        .tab_stop(!disabled)
        .border_1()
        .border_color(rgb(EDGE))
        .bg(rgb(PANEL_2))
        .p(px(8.));
    if disabled {
        control = control
            .text_color(rgb(DIM))
            .aria_description("Unavailable")
            .a11y_synthetic_children(|builder| disable_control_node(builder.parent_node()))
            .capture_any_mouse_down(|_, _, cx| cx.stop_propagation());
    } else {
        control = control.focusable();
    }
    control
}

pub fn score_value(format: &str, value: Option<f64>) -> String {
    match (format, value) {
        (_, None) => "—".into(),
        ("POINT_10_DECIMAL", Some(value)) => format!("{value:.1}"),
        (_, Some(value)) => format!("{value:.0}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, IntoElement, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, Render,
        TestAppContext, VisualTestContext, Window,
    };

    fn press(cx: &mut VisualTestContext, key: &str) {
        let os = cx.update(|_, app| app.platform_info().os);
        let keystroke = Keystroke::parse_with_os(key, os).unwrap();
        cx.simulate_event(KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        cx.simulate_event(KeyUpEvent { keystroke });
    }

    struct ActivationProbe {
        key_downs: usize,
        clicks: usize,
    }

    impl Render for ActivationProbe {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                select("activation-probe", "Activation probe")
                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, _| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.key_downs += 1;
                        }
                    }))
                    .on_click(cx.listener(|this, _, _, _| this.clicks += 1)),
            )
        }
    }

    #[gpui::test]
    fn focused_select_gets_keydown_and_one_synthesized_click(cx: &mut TestAppContext) {
        let (probe, cx) = cx.add_window_view(|_, _| ActivationProbe {
            key_downs: 0,
            clicks: 0,
        });
        probe.update_in(cx, |_, window, cx| window.focus_next(cx));
        cx.run_until_parked();
        press(cx, "enter");
        press(cx, "space");
        assert_eq!(
            probe.read_with(cx, |probe, _| (probe.key_downs, probe.clicks)),
            (2, 2)
        );
    }

    struct ButtonProbe {
        disabled: bool,
        clicks: usize,
    }

    impl Render for ButtonProbe {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                button("button-probe", "Run", self.disabled)
                    .debug_selector(|| "button-probe".into())
                    .on_click(cx.listener(|this, _, _, _| this.clicks += 1)),
            )
        }
    }

    #[gpui::test]
    fn disabled_button_blocks_pointer_and_keyboard_activation(cx: &mut TestAppContext) {
        let (probe, cx) = cx.add_window_view(|_, _| ButtonProbe {
            disabled: true,
            clicks: 0,
        });
        let bounds = cx.debug_bounds("button-probe").unwrap();
        cx.simulate_click(bounds.center(), Modifiers::default());
        probe.update_in(cx, |_, window, cx| window.focus_next(cx));
        press(cx, "enter");
        press(cx, "space");
        assert_eq!(probe.read_with(cx, |probe, _| probe.clicks), 0);

        probe.update_in(cx, |probe, _, cx| {
            probe.disabled = false;
            cx.notify();
        });
        cx.run_until_parked();
        probe.update_in(cx, |_, window, cx| window.focus_next(cx));
        cx.run_until_parked();
        press(cx, "enter");
        assert_eq!(probe.read_with(cx, |probe, _| probe.clicks), 1);
    }

    struct SelectProbe {
        disabled: bool,
        clicks: usize,
    }

    impl Render for SelectProbe {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                select_with_disabled("select-probe", "Status", self.disabled)
                    .debug_selector(|| "select-probe".into())
                    .on_click(cx.listener(|this, _, _, _| this.clicks += 1)),
            )
        }
    }

    #[gpui::test]
    fn disabled_select_blocks_pointer_and_keyboard_then_recovers(cx: &mut TestAppContext) {
        let (probe, cx) = cx.add_window_view(|_, _| SelectProbe {
            disabled: true,
            clicks: 0,
        });
        let bounds = cx.debug_bounds("select-probe").unwrap();
        cx.simulate_click(bounds.center(), Modifiers::default());
        probe.update_in(cx, |_, window, cx| window.focus_next(cx));
        press(cx, "enter");
        press(cx, "space");
        assert_eq!(probe.read_with(cx, |probe, _| probe.clicks), 0);

        probe.update_in(cx, |probe, _, cx| {
            probe.disabled = false;
            cx.notify();
        });
        cx.run_until_parked();
        probe.update_in(cx, |_, window, cx| window.focus_next(cx));
        cx.run_until_parked();
        press(cx, "enter");
        assert_eq!(probe.read_with(cx, |probe, _| probe.clicks), 1);
    }

    #[test]
    fn disabled_control_accessibility_removes_actions() {
        for role in [Role::Button, Role::ComboBox] {
            let mut node = gpui::accesskit::Node::new(role);
            node.add_action(gpui::AccessibleAction::Click);
            node.add_action(gpui::AccessibleAction::Focus);
            disable_control_node(&mut node);
            assert!(node.is_disabled());
            assert!(!node.supports_action(gpui::AccessibleAction::Click));
            assert!(!node.supports_action(gpui::AccessibleAction::Focus));
        }
    }
}
