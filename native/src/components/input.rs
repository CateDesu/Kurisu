// Adapted from gpui-unofficial 1.27.0 examples/input.rs.
// Copyright 2022 - 2025 Zed Industries, Inc. Licensed under Apache-2.0.
// See the bundled dependency license for the full terms.
use std::{cell::Cell, ops::Range, rc::Rc};

use gpui::accesskit::{ActionData, Node, NodeId, TextPosition, TextSelection};
use gpui::{
    AccessibleAction, App, Bounds, ClipboardItem, Context, CursorStyle, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    GlobalElementId, KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PaintQuad, Pixels, Point, Role, SharedString, Style, TextRun, UTF16Selection,
    UnderlineStyle, Window, WrappedLine, actions, div, fill, hsla, point, prelude::*, px, relative,
    rgb, rgba, size,
};
use unicode_segmentation::*;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        ShowCharacterPalette,
        Paste,
        Cut,
        Copy,
        Submit,
        Enter,
    ]
);

pub struct TextInput {
    masked: bool,
    multiline: bool,
    disabled: bool,
    framed: bool,
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    accessible_label: Option<SharedString>,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_lines: Vec<WrappedLine>,
    last_display: String,
    last_visual_lines: usize,
    last_line_height: Pixels,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
}

#[derive(Clone)]
struct AccessibleText {
    text: String,
    anchor: usize,
    focus: usize,
    disabled: bool,
}

impl AccessibleText {
    fn run_node(&self) -> Node {
        let mut run = Node::new(Role::TextRun);
        run.set_value(self.text.clone());
        run.set_character_lengths(
            self.text
                .chars()
                .map(|ch| ch.len_utf8() as u8)
                .collect::<Vec<_>>(),
        );
        run
    }

    fn selection(&self, run_id: NodeId) -> TextSelection {
        TextSelection {
            anchor: TextPosition {
                node: run_id,
                character_index: self.anchor,
            },
            focus: TextPosition {
                node: run_id,
                character_index: self.focus,
            },
        }
    }

    fn write_parent(&self, parent: &mut Node, run_id: NodeId) {
        parent.set_text_selection(self.selection(run_id));
        if self.disabled {
            parent.set_disabled();
            parent.set_read_only();
        }
    }
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some("TextInput")),
        KeyBinding::new("delete", Delete, Some("TextInput")),
        KeyBinding::new("left", Left, Some("TextInput")),
        KeyBinding::new("right", Right, Some("TextInput")),
        KeyBinding::new("shift-left", SelectLeft, Some("TextInput")),
        KeyBinding::new("shift-right", SelectRight, Some("TextInput")),
        KeyBinding::new("ctrl-a", SelectAll, Some("TextInput")),
        KeyBinding::new("ctrl-v", Paste, Some("TextInput")),
        KeyBinding::new("ctrl-c", Copy, Some("TextInput")),
        KeyBinding::new("ctrl-x", Cut, Some("TextInput")),
        KeyBinding::new("home", Home, Some("TextInput")),
        KeyBinding::new("end", End, Some("TextInput")),
        KeyBinding::new("enter", Enter, Some("TextInput")),
        KeyBinding::new("ctrl-enter", Submit, Some("TextInput")),
    ]);
}

#[derive(Clone, Debug, PartialEq)]
pub enum TextInputEvent {
    Changed(String),
    Submitted,
}

impl EventEmitter<TextInputEvent> for TextInput {}

impl TextInput {
    pub fn new(
        value: impl Into<String>,
        placeholder: impl Into<String>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let content: String = value.into();
        let len = content.len();
        Self {
            focus_handle: cx.focus_handle().tab_stop(true),
            content: content.into(),
            placeholder: placeholder.into().into(),
            accessible_label: None,
            selected_range: len..len,
            selection_reversed: false,
            marked_range: None,
            last_lines: Vec::new(),
            last_display: String::new(),
            last_visual_lines: 1,
            last_line_height: px(22.),
            last_bounds: None,
            is_selecting: false,
            masked: false,
            multiline: false,
            disabled: false,
            framed: true,
        }
    }

    pub fn without_frame(mut self) -> Self {
        self.framed = false;
        self
    }

    pub fn value(&self) -> &str {
        &self.content
    }
    pub fn set_accessible_label(&mut self, label: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.accessible_label = Some(label.into());
        cx.notify();
    }
    pub fn set_value(&mut self, value: impl Into<String>, cx: &mut Context<Self>) {
        let content = value.into();
        if self.content.as_ref() == content.as_str() {
            return;
        }
        let len = content.len();
        self.content = content.into();
        self.selected_range = len..len;
        self.marked_range = None;
        self.last_lines.clear();
        self.last_display.clear();
        cx.notify();
    }
    pub fn set_masked(&mut self, masked: bool, cx: &mut Context<Self>) {
        self.masked = masked;
        cx.notify();
    }
    pub fn set_multiline(&mut self, multiline: bool, cx: &mut Context<Self>) {
        self.multiline = multiline;
        cx.notify();
    }
    pub fn set_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        self.disabled = disabled;
        self.focus_handle = self.focus_handle.clone().tab_stop(!disabled);
        cx.notify();
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        if !self.disabled {
            cx.emit(TextInputEvent::Submitted);
        }
    }

    fn enter(&mut self, _: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        if self.multiline {
            self.replace_text_in_range(None, "\n", window, cx);
        } else {
            cx.emit(TextInputEvent::Submitted);
        }
    }

    fn left(&mut self, _: &Left, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), window, cx);
        } else {
            self.move_to(self.selected_range.start, window, cx)
        }
    }

    fn right(&mut self, _: &Right, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), window, cx);
        } else {
            self.move_to(self.selected_range.end, window, cx)
        }
    }

    fn select_left(&mut self, _: &SelectLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), window, cx);
    }

    fn select_right(&mut self, _: &SelectRight, window: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), window, cx);
    }

    fn select_all(&mut self, _: &SelectAll, window: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, window, cx);
        self.select_to(self.content.len(), window, cx)
    }

    fn home(&mut self, _: &Home, window: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, window, cx);
    }

    fn end(&mut self, _: &End, window: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), window, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            if self.cursor_offset() == prev {
                window.play_system_bell();
                return;
            }
            self.select_to(prev, window, cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            if self.cursor_offset() == next {
                window.play_system_bell();
                return;
            }
            self.select_to(next, window, cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }
        self.is_selecting = true;
        window.request_virtual_keyboard();

        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), window, cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), window, cx)
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), window, cx);
        }
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = if self.multiline {
                text
            } else {
                text.replace(['\r', '\n'], " ")
            };
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.masked && !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }
    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.disabled && !self.masked && !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn move_to(&mut self, offset: usize, window: &Window, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        window.notify_text_selection_changed(&self.focus_handle);
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn display_text(&self) -> String {
        if self.content.is_empty() {
            self.placeholder.to_string()
        } else if self.masked {
            "•".repeat(self.content.chars().count())
        } else {
            self.content.to_string()
        }
    }

    fn accessible_text(&self) -> AccessibleText {
        let text = if self.masked {
            "•".repeat(self.content.chars().count())
        } else {
            self.content.to_string()
        };
        let (anchor, focus) = if self.selection_reversed {
            (self.selected_range.end, self.selected_range.start)
        } else {
            (self.selected_range.start, self.selected_range.end)
        };
        AccessibleText {
            text,
            anchor: utf8_to_codepoint(&self.content, anchor),
            focus: utf8_to_codepoint(&self.content, focus),
            disabled: self.disabled,
        }
    }

    fn accessible_role(&self) -> Role {
        if self.masked {
            Role::PasswordInput
        } else if self.multiline {
            Role::MultilineTextInput
        } else {
            Role::TextInput
        }
    }

    fn accessible_name(&self) -> SharedString {
        self.accessible_label
            .clone()
            .unwrap_or_else(|| self.placeholder.clone())
    }

    fn accessible_value(&self) -> SharedString {
        if self.masked {
            "".into()
        } else {
            self.content.clone()
        }
    }

    fn set_accessible_selection(
        &mut self,
        selection: &TextSelection,
        run_id: NodeId,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled || selection.anchor.node != run_id || selection.focus.node != run_id {
            return;
        }
        let anchor = codepoint_to_utf8(&self.content, selection.anchor.character_index);
        let focus = codepoint_to_utf8(&self.content, selection.focus.character_index);
        self.selected_range = anchor.min(focus)..anchor.max(focus);
        self.selection_reversed = focus < anchor;
        window.notify_text_selection_changed(&self.focus_handle);
        cx.notify();
    }

    fn set_accessible_value(&mut self, value: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        let value = if self.multiline {
            value.to_string()
        } else {
            value.replace(['\r', '\n'], " ")
        };
        let length = self.content.encode_utf16().count();
        self.replace_text_in_range(Some(0..length), &value, window, cx);
    }

    fn display_offset(&self, source_offset: usize) -> usize {
        if self.masked {
            self.content[..source_offset.min(self.content.len())]
                .chars()
                .count()
                * "•".len()
        } else {
            source_offset.min(self.content.len())
        }
    }

    fn source_offset(&self, display_offset: usize) -> usize {
        if self.masked {
            self.content
                .char_indices()
                .nth(display_offset / "•".len())
                .map(|(index, _)| index)
                .unwrap_or(self.content.len())
        } else {
            display_offset.min(self.content.len())
        }
    }

    fn line_and_offset(display: &str, offset: usize) -> (usize, usize) {
        let mut start = 0;
        for (line, segment) in display.split('\n').enumerate() {
            let end = start + segment.len();
            if offset <= end {
                return (line, offset.saturating_sub(start));
            }
            start = end + 1;
        }
        (0, 0)
    }

    fn display_index_for_point(&self, position: Point<Pixels>) -> Option<usize> {
        let bounds = self.last_bounds?;
        if self.last_lines.is_empty() || self.last_display != self.display_text() {
            return None;
        }
        let mut top = bounds.top();
        let mut start = 0;
        for (index, line) in self.last_lines.iter().enumerate() {
            let height = line.size(self.last_line_height).height;
            if position.y < top + height || index + 1 == self.last_lines.len() {
                let local = point(position.x - bounds.left(), position.y - top);
                let offset = line
                    .closest_index_for_position(local, self.last_line_height)
                    .unwrap_or_else(|offset| offset);
                return Some(start + offset);
            }
            top += height;
            start += line.len() + 1;
        }
        None
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        self.display_index_for_point(position)
            .map(|offset| self.source_offset(offset))
            .unwrap_or(0)
    }

    fn select_to(&mut self, offset: usize, window: &Window, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        window.notify_text_selection_changed(&self.focus_handle);
        cx.notify()
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        utf16_to_utf8(&self.content, offset)
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        utf8_to_utf16(&self.content, offset)
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        previous_grapheme(&self.content, offset)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        next_grapheme(&self.content, offset)
    }
}

fn utf16_to_utf8(text: &str, offset: usize) -> usize {
    let mut utf8 = 0;
    let mut utf16 = 0;
    for ch in text.chars() {
        if utf16 >= offset {
            break;
        }
        utf16 += ch.len_utf16();
        utf8 += ch.len_utf8();
    }
    utf8
}

fn utf8_to_utf16(text: &str, offset: usize) -> usize {
    let mut utf8 = 0;
    let mut utf16 = 0;
    for ch in text.chars() {
        if utf8 >= offset {
            break;
        }
        utf8 += ch.len_utf8();
        utf16 += ch.len_utf16();
    }
    utf16
}

fn utf8_to_codepoint(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    text[..offset].chars().count()
}

fn codepoint_to_utf8(text: &str, offset: usize) -> usize {
    text.char_indices()
        .nth(offset)
        .map_or(text.len(), |(index, _)| index)
}

fn previous_grapheme(text: &str, offset: usize) -> usize {
    text.grapheme_indices(true)
        .rev()
        .find_map(|(index, _)| (index < offset).then_some(index))
        .unwrap_or(0)
}

fn next_grapheme(text: &str, offset: usize) -> usize {
    text.grapheme_indices(true)
        .find_map(|(index, _)| (index > offset).then_some(index))
        .unwrap_or(text.len())
}

fn caret_bounds(
    lines: &[WrappedLine],
    display: &str,
    offset: usize,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
) -> Option<Bounds<Pixels>> {
    let (line_index, local_offset) = TextInput::line_and_offset(display, offset);
    let line = lines.get(line_index)?;
    let preceding_height = lines[..line_index]
        .iter()
        .map(|line| line.size(line_height).height)
        .fold(Pixels::ZERO, |sum, height| sum + height);
    let position = line.position_for_index(local_offset, line_height)?;
    Some(Bounds::new(
        point(
            bounds.left() + position.x,
            bounds.top() + preceding_height + position.y,
        ),
        size(px(2.), line_height),
    ))
}

fn selection_rects(
    lines: &[WrappedLine],
    range: Range<usize>,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
) -> Vec<Bounds<Pixels>> {
    let mut rectangles = Vec::new();
    let mut text_start = 0;
    let mut top = bounds.top();
    for line in lines {
        let text_end = text_start + line.len();
        let selection_start = range.start.max(text_start).min(text_end) - text_start;
        let selection_end = range.end.max(text_start).min(text_end) - text_start;
        if selection_start < selection_end {
            let mut boundaries = vec![0];
            for boundary in line.wrap_boundaries() {
                if let Some(glyph) = line
                    .runs()
                    .get(boundary.run_ix)
                    .and_then(|run| run.glyphs.get(boundary.glyph_ix))
                {
                    boundaries.push(glyph.index);
                }
            }
            boundaries.push(line.len());
            for (visual_row, pair) in boundaries.windows(2).enumerate() {
                let from = selection_start.max(pair[0]);
                let to = selection_end.min(pair[1]);
                if from >= to {
                    continue;
                }
                let x_start = if from == pair[0] {
                    Pixels::ZERO
                } else {
                    line.position_for_index(from, line_height)
                        .map_or(Pixels::ZERO, |position| position.x)
                };
                let x_end = line
                    .position_for_index(to, line_height)
                    .map_or(Pixels::ZERO, |position| position.x);
                let row_top = top + line_height * visual_row as f32;
                rectangles.push(Bounds::from_corners(
                    point(bounds.left() + x_start, row_top),
                    point(bounds.left() + x_end.max(x_start), row_top + line_height),
                ));
            }
        }
        text_start = text_end + 1;
        top += line.size(line_height).height;
    }
    rectangles
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_range = self.range_from_utf16(&range_utf16);
        self.selection_reversed = false;
        window.notify_text_selection_changed(&self.focus_handle);
        cx.notify();
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.content.encode_utf16().count())
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.marked_range = None;
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.selection_reversed = false;
        self.marked_range.take();
        window.notify_text_input_changed(&self.focus_handle);
        cx.emit(TextInputEvent::Changed(self.content.to_string()));
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        let insertion_start_utf16 = self.offset_to_utf16(range.start);
        let inserted_length_utf16 = new_text.encode_utf16().count();
        self.selected_range = new_selected_range_utf16
            .map(|selected| {
                self.range_from_utf16(
                    &(insertion_start_utf16 + selected.start.min(inserted_length_utf16)
                        ..insertion_start_utf16 + selected.end.min(inserted_length_utf16)),
                )
            })
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        self.selection_reversed = false;

        window.notify_text_input_changed(&self.focus_handle);
        cx.emit(TextInputEvent::Changed(self.content.to_string()));
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        if self.last_display != self.display_text() {
            return None;
        }
        let range = self.range_from_utf16(&range_utf16);
        let start = self.display_offset(range.start);
        let end = self.display_offset(range.end);
        if start == end {
            return caret_bounds(
                &self.last_lines,
                &self.last_display,
                start,
                bounds,
                self.last_line_height,
            );
        }
        let rectangles =
            selection_rects(&self.last_lines, start..end, bounds, self.last_line_height);
        let first = rectangles.first()?;
        let mut left = first.left();
        let mut top = first.top();
        let mut right = first.right();
        let mut bottom = first.bottom();
        for rect in rectangles.iter().skip(1) {
            left = left.min(rect.left());
            top = top.min(rect.top());
            right = right.max(rect.right());
            bottom = bottom.max(rect.bottom());
        }
        Some(Bounds::from_corners(point(left, top), point(right, bottom)))
    }

    fn selection_bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        let display_range = self.display_offset(range.start)..self.display_offset(range.end);
        if display_range.is_empty() {
            return self
                .bounds_for_range(range_utf16, bounds, window, cx)
                .into_iter()
                .collect();
        }
        selection_rects(
            &self.last_lines,
            display_range,
            bounds,
            self.last_line_height,
        )
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        self.display_index_for_point(point)
            .map(|offset| self.offset_to_utf16(self.source_offset(offset)))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    lines: Vec<WrappedLine>,
    display: String,
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let input = self.input.read(cx);
        let line_count = if input.last_display == input.display_text() {
            input.last_visual_lines
        } else if input.multiline {
            input.display_text().split('\n').count().max(1)
        } else {
            1
        };
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (window.line_height() * line_count as f32).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let display = input.display_text();
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let color = if input.content.is_empty() {
            hsla(0., 0., 0.7, 0.7)
        } else {
            style.color
        };
        let run = TextRun {
            len: display.len(),
            font: style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked) = input.marked_range.as_ref() {
            let start = input.display_offset(marked.start);
            let end = input.display_offset(marked.end);
            vec![
                TextRun {
                    len: start,
                    ..run.clone()
                },
                TextRun {
                    len: end - start,
                    underline: Some(UnderlineStyle {
                        color: Some(color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display.len() - end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect::<Vec<_>>()
        } else {
            vec![run]
        };
        let wrap_width =
            (input.multiline && bounds.size.width > px(1.)).then_some(bounds.size.width);
        let lines: Vec<_> = window
            .text_system()
            .shape_text(display.clone().into(), font_size, &runs, wrap_width, None)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let offset = input.display_offset(input.cursor_offset());
        let cursor = caret_bounds(&lines, &display, offset, bounds, line_height)
            .map(|bounds| fill(bounds, rgb(crate::theme::ACCENT)));
        let mut selection = Vec::new();
        if !input.selected_range.is_empty() {
            let first = input.display_offset(input.selected_range.start);
            let last = input.display_offset(input.selected_range.end);
            selection.extend(
                selection_rects(&lines, first..last, bounds, line_height)
                    .into_iter()
                    .map(|bounds| fill(bounds, rgba(0x613f3080))),
            );
        }
        PrepaintState {
            lines,
            display,
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        for selection in prepaint.selection.drain(..) {
            window.paint_quad(selection);
        }
        let line_height = window.line_height();
        let mut top = bounds.top();
        for line in &prepaint.lines {
            line.paint(
                point(bounds.left(), top),
                line_height,
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            )
            .unwrap();
            top += line.size(line_height).height;
        }
        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }
        self.input.update(cx, |input, _cx| {
            let visual_lines = prepaint
                .lines
                .iter()
                .map(|line| line.wrap_boundaries().len() + 1)
                .sum();
            if input.last_visual_lines != visual_lines {
                input.last_visual_lines = visual_lines;
                _cx.notify();
            }
            input.last_lines = std::mem::take(&mut prepaint.lines);
            input.last_display = prepaint.display.clone();
            input.last_line_height = line_height;
            input.last_bounds = Some(bounds);
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let accessible = self.accessible_text();
        let run_id = Rc::new(Cell::new(None));
        let selection_run_id = run_id.clone();
        let selection_input = cx.entity().downgrade();
        let value_input = cx.entity().downgrade();
        let disabled = self.disabled;
        div()
            .flex()
            .id("text-input")
            .role(self.accessible_role())
            .aria_label(self.accessible_name())
            .aria_value(self.accessible_value())
            .aria_placeholder(self.placeholder.clone())
            .a11y_synthetic_children(move |builder| {
                let id = builder.synthetic_node_id("content");
                run_id.set(Some(id));
                builder.push_child(id, accessible.run_node());
                accessible.write_parent(builder.parent_node(), id);
            })
            .when(!disabled, |field| {
                field
                    .on_a11y_action(
                        AccessibleAction::SetTextSelection,
                        move |data, window, cx| {
                            let (Some(ActionData::SetTextSelection(selection)), Some(id)) =
                                (data, selection_run_id.get())
                            else {
                                return;
                            };
                            let _ = selection_input.update(cx, |input, cx| {
                                input.set_accessible_selection(selection, id, window, cx)
                            });
                        },
                    )
                    .on_a11y_action(AccessibleAction::SetValue, move |data, window, cx| {
                        if let Some(ActionData::Value(value)) = data {
                            let _ = value_input.update(cx, |input, cx| {
                                input.set_accessible_value(value, window, cx)
                            });
                        }
                    })
            })
            .tab_stop(!disabled)
            .key_context("TextInput")
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::enter))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .when(self.framed, |field| {
                field
                    .bg(rgb(crate::theme::PANEL_2))
                    .border_1()
                    .border_color(rgb(crate::theme::EDGE))
                    .focus_visible(|style| style.border_color(rgb(crate::theme::ACCENT)))
            })
            .text_color(rgb(crate::theme::INK))
            .line_height(px(if self.framed { 22. } else { 22.5 }))
            .text_size(px(if self.framed { 14. } else { 15. }))
            .h(px(if self.multiline {
                120.
            } else if self.framed {
                38.
            } else {
                22.5
            }))
            .overflow_y_scroll()
            .child(
                div()
                    .min_h(px(if self.framed { 38. } else { 22.5 }))
                    .w_full()
                    .when(self.framed, |content| {
                        content.p(px(4.)).bg(rgb(crate::theme::PANEL_2))
                    })
                    .child(TextElement { input: cx.entity() }),
            )
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{EntityInputHandler, TestAppContext};

    #[gpui::test]
    fn explicit_accessible_label_preserves_placeholder_value_and_selection(
        cx: &mut TestAppContext,
    ) {
        let (input, cx) =
            cx.add_window_view(|window, cx| TextInput::new("2024", "Year", window, cx));
        input.update_in(cx, |input, _, cx| {
            assert_eq!(input.accessible_name().as_ref(), "Year");
            let selection = input.selected_range.clone();
            input.set_accessible_label("Started year", cx);
            assert_eq!(input.accessible_name().as_ref(), "Started year");
            assert_eq!(input.placeholder.as_ref(), "Year");
            assert_eq!(input.value(), "2024");
            assert_eq!(input.selected_range, selection);
        });
    }

    #[gpui::test]
    fn control_shortcuts_select_copy_cut_and_paste(cx: &mut gpui::TestAppContext) {
        cx.update(bind_keys);
        let (input, cx) =
            cx.add_window_view(|window, cx| TextInput::new("Original 日本語", "Notes", window, cx));
        input.update_in(cx, |input, window, cx| {
            window.focus(&input.focus_handle, cx);
        });
        cx.simulate_keystrokes("ctrl-a ctrl-c");
        cx.simulate_input("Replacement");
        assert_eq!(
            input.read_with(cx, |input, _| input.value().to_owned()),
            "Replacement"
        );
        cx.simulate_keystrokes("ctrl-a ctrl-v");
        assert_eq!(
            input.read_with(cx, |input, _| input.value().to_owned()),
            "Original 日本語"
        );
        cx.simulate_keystrokes("ctrl-a ctrl-x");
        assert_eq!(input.read_with(cx, |input, _| input.value().to_owned()), "");
        cx.simulate_keystrokes("ctrl-v");
        assert_eq!(
            input.read_with(cx, |input, _| input.value().to_owned()),
            "Original 日本語"
        );
    }

    #[gpui::test]
    fn clipboard_line_endings_respect_single_and_multiline_fields(cx: &mut TestAppContext) {
        let (input, cx) = cx.add_window_view(|window, cx| TextInput::new("", "Notes", window, cx));
        input.update_in(cx, |input, window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string("日本\r\n語\rtext".into()));
            input.paste(&Paste, window, cx);
            assert_eq!(input.value(), "日本  語 text");
            input.set_value("", cx);
            input.set_multiline(true, cx);
            input.paste(&Paste, window, cx);
            assert_eq!(input.value(), "日本\r\n語\rtext");
        });
    }

    #[test]
    fn grapheme_movement_keeps_combining_and_joined_emoji_intact() {
        let text = "e\u{301}👩‍🚀漢";
        assert_eq!(next_grapheme(text, 0), "e\u{301}".len());
        assert_eq!(next_grapheme(text, "e\u{301}".len()), "e\u{301}👩‍🚀".len());
        assert_eq!(previous_grapheme(text, text.len()), "e\u{301}👩‍🚀".len());
    }

    #[test]
    fn ime_offsets_map_utf16_surrogates_to_utf8_boundaries() {
        let text = "A🙂漢";
        assert_eq!(utf16_to_utf8(text, 1), 1);
        assert_eq!(utf16_to_utf8(text, 2), 5);
        assert_eq!(utf16_to_utf8(text, 3), 5);
        assert_eq!(utf8_to_utf16(text, 5), 3);
        assert_eq!(utf8_to_utf16(text, text.len()), 4);
    }

    #[gpui::test]
    fn disabled_input_rejects_text_and_leaves_tab_order(cx: &mut TestAppContext) {
        let (input, cx) =
            cx.add_window_view(|window, cx| TextInput::new("80", "Score", window, cx));
        cx.run_until_parked();
        input.update_in(cx, |input, window, cx| {
            window.focus_next(cx);
            assert!(input.focus_handle.is_focused(window));
            input.set_disabled(true, cx);
            input.replace_text_in_range(Some(0..2), "90", window, cx);
            input.set_accessible_value("90", window, cx);
            assert_eq!(input.value(), "80");
            let selection = TextSelection {
                anchor: TextPosition {
                    node: NodeId(1),
                    character_index: 0,
                },
                focus: TextPosition {
                    node: NodeId(1),
                    character_index: 1,
                },
            };
            let previous = input.selected_range.clone();
            input.set_accessible_selection(&selection, NodeId(1), window, cx);
            assert_eq!(input.selected_range, previous);
            let mut node = Node::new(input.accessible_role());
            input.accessible_text().write_parent(&mut node, NodeId(1));
            assert!(node.is_disabled());
            assert!(node.is_read_only());
        });
        cx.run_until_parked();
        input.update_in(cx, |input, window, cx| {
            window.blur(cx);
            window.focus_next(cx);
            assert!(window.focused(cx).is_none());
            input.set_disabled(false, cx);
            input.replace_text_in_range(Some(0..2), "90", window, cx);
            assert_eq!(input.value(), "90");
            let mut node = Node::new(input.accessible_role());
            input.accessible_text().write_parent(&mut node, NodeId(1));
            assert!(!node.is_disabled());
            assert!(!node.is_read_only());
        });
        cx.run_until_parked();
        input.update_in(cx, |input, window, cx| {
            window.focus_next(cx);
            assert!(input.focus_handle.is_focused(window));
        });
    }

    #[gpui::test]
    fn accessible_text_tracks_unicode_selection_and_edit_actions(cx: &mut TestAppContext) {
        let (input, cx) =
            cx.add_window_view(|window, cx| TextInput::new("A🙂漢", "Notes", window, cx));
        cx.run_until_parked();
        input.update_in(cx, |input, window, cx| {
            input.set_selected_text_range(1..4, window, cx);
            let snapshot = input.accessible_text();
            let run = snapshot.run_node();
            assert_eq!(run.value(), Some("A🙂漢"));
            assert_eq!(run.character_lengths(), &[1, 4, 3]);
            let selection = snapshot.selection(NodeId(7));
            assert_eq!(selection.anchor.character_index, 1);
            assert_eq!(selection.focus.character_index, 3);
            let mut parent = Node::new(input.accessible_role());
            snapshot.write_parent(&mut parent, NodeId(7));
            assert_eq!(parent.text_selection(), Some(&selection));

            let reversed = TextSelection {
                anchor: TextPosition {
                    node: NodeId(7),
                    character_index: 3,
                },
                focus: TextPosition {
                    node: NodeId(7),
                    character_index: 1,
                },
            };
            input.set_accessible_selection(&reversed, NodeId(7), window, cx);
            assert_eq!(input.selected_range, 1..8);
            assert!(input.selection_reversed);
            let snapshot = input.accessible_text();
            assert_eq!(snapshot.selection(NodeId(7)), reversed);
            input.set_accessible_value("B🙂\n漢", window, cx);
            assert_eq!(input.value(), "B🙂 漢");
            input.set_multiline(true, cx);
            input.set_accessible_value("B🙂\n漢", window, cx);
            assert_eq!(input.value(), "B🙂\n漢");
            assert_eq!(input.accessible_role(), Role::MultilineTextInput);
            assert_eq!(
                input.accessible_text().run_node().character_lengths(),
                &[1, 4, 1, 3]
            );
        });
    }

    #[gpui::test]
    fn masked_accessibility_never_exposes_source_text(cx: &mut TestAppContext) {
        let (input, cx) =
            cx.add_window_view(|window, cx| TextInput::new("secret🙂", "Token", window, cx));
        input.update_in(cx, |input, _, cx| input.set_masked(true, cx));
        cx.run_until_parked();
        input.read_with(cx, |input, _| {
            assert_eq!(input.accessible_role(), Role::PasswordInput);
            assert!(input.accessible_value().is_empty());
            let snapshot = input.accessible_text();
            let run = snapshot.run_node();
            assert_eq!(run.value(), Some("•••••••"));
            assert!(!run.value().unwrap().contains("secret"));
            assert_eq!(run.character_lengths(), &[3, 3, 3, 3, 3, 3, 3]);
            let mut parent = Node::new(input.accessible_role());
            parent.set_value(input.accessible_value().to_string());
            snapshot.write_parent(&mut parent, NodeId(9));
            assert_eq!(parent.value(), Some(""));
        });
    }
}
