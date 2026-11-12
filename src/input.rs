//! A single-line text input, adapted from GPUI's `examples/input.rs`: typing and IME composition
//! through `EntityInputHandler`, selection by keyboard and mouse, and the clipboard. The cursor
//! moves by `char`, not by grapheme cluster, to avoid a dependency for a filter field.
use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, GlobalElementId, KeyBinding, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window, actions, div,
    fill, point, prelude::*, px, relative, size,
};
use std::ops::Range;
use std::time::Duration;

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
        Paste,
        Cut,
        Copy,
        Clear,
        Submit
    ]
);

const CONTEXT: &str = "TextInput";
/// Each blink phase, on and off; close to AppKit's insertion point.
const BLINK: Duration = Duration::from_millis(500);

pub fn bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("delete", Delete, Some(CONTEXT)),
        KeyBinding::new("left", Left, Some(CONTEXT)),
        KeyBinding::new("right", Right, Some(CONTEXT)),
        KeyBinding::new("shift-left", SelectLeft, Some(CONTEXT)),
        KeyBinding::new("shift-right", SelectRight, Some(CONTEXT)),
        KeyBinding::new("cmd-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("home", Home, Some(CONTEXT)),
        KeyBinding::new("cmd-left", Home, Some(CONTEXT)),
        KeyBinding::new("end", End, Some(CONTEXT)),
        KeyBinding::new("cmd-right", End, Some(CONTEXT)),
        KeyBinding::new("cmd-v", Paste, Some(CONTEXT)),
        KeyBinding::new("cmd-x", Cut, Some(CONTEXT)),
        KeyBinding::new("cmd-c", Copy, Some(CONTEXT)),
        KeyBinding::new("escape", Clear, Some(CONTEXT)),
        KeyBinding::new("enter", Submit, Some(CONTEXT)),
    ]
}

/// Emitted whenever the text changes, not on cursor moves.
pub struct Changed;
/// Return was pressed.
pub struct Submitted;
/// Esc was pressed in a digits field, which keeps its text for the owner to restore.
pub struct Cancelled;

pub struct TextInput {
    pub focus: FocusHandle,
    content: String,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    cursor_visible: bool,
    /// Bumped to restart the blink cycle; a loop with an older epoch ends.
    blink_epoch: usize,
    blinking: bool,
    /// Painted focused since the last blink; otherwise the loop ends, so a hidden input costs nothing.
    painted_focused: bool,
    /// Accepts only ASCII digits, at most this many.
    max_digits: Option<usize>,
}

impl EventEmitter<Changed> for TextInput {}
impl EventEmitter<Submitted> for TextInput {}
impl EventEmitter<Cancelled> for TextInput {}

impl TextInput {
    /// A port or similar number: digits only, Esc reports `Cancelled` instead of clearing.
    pub fn digits(
        placeholder: impl Into<SharedString>,
        max: usize,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            max_digits: Some(max),
            ..Self::new(placeholder, cx)
        }
    }

    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            content: String::new(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            cursor_visible: true,
            blink_epoch: 0,
            blinking: false,
            painted_focused: false,
            max_digits: None,
        }
    }

    /// Set by the owner, not typed: no `Changed`.
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.content = text.to_owned();
        self.marked_range = None;
        self.selected_range = text.len()..text.len();
        self.selection_reversed = false;
        cx.notify();
    }

    /// What of `text` may replace `range`: everything, or in a digits field only digits that fit.
    fn accept<'a>(&self, range: &Range<usize>, text: &'a str) -> std::borrow::Cow<'a, str> {
        let Some(max) = self.max_digits else {
            return text.into();
        };
        let room = max.saturating_sub(self.content.len() - range.len());
        text.chars()
            .filter(char::is_ascii_digit)
            .take(room)
            .collect::<String>()
            .into()
    }

    /// Solid while typing or moving; blinking resumes at the next paint.
    fn show_cursor(&mut self) {
        self.cursor_visible = true;
        self.blink_epoch += 1;
        self.blinking = false;
    }

    fn start_blinking(&mut self, cx: &mut Context<Self>) {
        self.blinking = true;
        let epoch = self.blink_epoch;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(BLINK).await;
                let go_on = this.update(cx, |this, cx| {
                    if this.blink_epoch != epoch {
                        return false;
                    }
                    if !std::mem::take(&mut this.painted_focused) {
                        this.show_cursor();
                        return false;
                    }
                    this.cursor_visible = !this.cursor_visible;
                    cx.notify();
                    true
                });
                if !go_on.unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
    }

    /// The committed text: IME composition in progress is shown but not part of it.
    pub fn text(&self) -> String {
        match &self.marked_range {
            Some(marked) => [&self.content[..marked.start], &self.content[marked.end..]].concat(),
            None => self.content.clone(),
        }
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(Submitted);
    }

    fn clear(&mut self, _: &Clear, window: &mut Window, cx: &mut Context<Self>) {
        if self.max_digits.is_some() {
            cx.emit(Cancelled);
            return;
        }
        self.selected_range = 0..self.content.len();
        self.replace_text_in_range(None, "", window, cx)
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.is_selecting = true;
        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx)
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text.replace('\n', " "), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            self.copy(&Copy, window, cx);
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.show_cursor();
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        // One line: only x matters, and points left or right of the text clamp to its ends.
        if self.content.is_empty() {
            return 0;
        }
        line.closest_index_for_x(position.x - bounds.left())
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.show_cursor();
        cx.notify()
    }

    /// The platform speaks UTF-16 offsets; `content` is indexed in UTF-8 bytes.
    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf16 = 0;
        for (utf8, ch) in self.content.char_indices() {
            if utf16 >= offset {
                return utf8;
            }
            utf16 += ch.len_utf16();
        }
        self.content.len()
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        self.content[..offset].chars().map(char::len_utf16).sum()
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content[..offset]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content[offset..]
            .chars()
            .next()
            .map_or(offset, |ch| offset + ch.len_utf8())
    }

    /// `Changed` fires only when the committed text changes, so composing does not filter.
    fn replace(
        &mut self,
        range: Range<usize>,
        new_text: &str,
        marked: Option<Range<usize>>,
        cx: &mut Context<Self>,
    ) {
        let before = self.text();
        self.content.replace_range(range, new_text);
        self.marked_range = marked;
        if self.text() != before {
            cx.emit(Changed);
        }
        self.show_cursor();
        cx.notify();
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    /// Some input methods commit by unmarking, leaving the composed text in place.
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.marked_range.take().is_some_and(|r| !r.is_empty()) {
            cx.emit(Changed);
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let new_text = &*self.accept(&range, new_text);
        let cursor = range.start + new_text.len();
        self.selected_range = cursor..cursor;
        self.selection_reversed = false;
        self.replace(range, new_text, None, cx);
    }

    /// IME composition: the marked text is shown underlined until it is committed.
    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let new_text = &*self.accept(&range, new_text);
        let marked = (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        self.replace(range.clone(), new_text, marked, cx);
        // The new selection is relative to the inserted text.
        self.selected_range = new_selected_range_utf16
            .map(|r| {
                let r = self.range_from_utf16(&r);
                range.start + r.start..range.start + r.end
            })
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        Some(Bounds::from_corners(
            point(
                bounds.left() + layout.x_for_index(range.start),
                bounds.top(),
            ),
            point(
                bounds.left() + layout.x_for_index(range.end),
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let local = self.last_bounds?.localize(&point)?;
        let index = self.last_layout.as_ref()?.index_for_x(local.x)?;
        Some(self.offset_to_utf16(index))
    }
}

/// Shapes and paints the line, its selection and cursor, and registers the input handler.
struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
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
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        // Colors come from the surrounding text style, so the caller's theme applies.
        let style = window.text_style();
        let (text, color): (SharedString, _) = if input.content.is_empty() {
            (input.placeholder.clone(), style.color.opacity(0.5))
        } else {
            (input.content.clone().into(), style.color)
        };
        let run = TextRun {
            len: text.len(),
            font: style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = match input.marked_range.as_ref() {
            Some(marked) => [
                TextRun {
                    len: marked.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked.end - marked.start,
                    underline: Some(UnderlineStyle {
                        color: Some(color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: text.len() - marked.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect(),
            None => vec![run],
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(text, font_size, &runs, None);

        let x = |index| bounds.left() + line.x_for_index(index);
        let range = input.selected_range.clone();
        let (selection, cursor) = if range.is_empty() {
            // A little shorter than the line, like AppKit's insertion point.
            let inset = bounds.size.height * 0.12;
            let cursor = Bounds::new(
                point(x(input.cursor_offset()), bounds.top() + inset),
                size(px(1.5), bounds.size.height - inset * 2.),
            );
            (None, Some(fill(cursor, style.color)))
        } else {
            let selection = Bounds::from_corners(
                point(x(range.start), bounds.top()),
                point(x(range.end), bounds.bottom()),
            );
            (Some(fill(selection, style.color.opacity(0.2))), None)
        };
        PrepaintState {
            line: Some(line),
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection)
        }
        let Some(line) = prepaint.line.take() else {
            return;
        };
        let _ = line.paint(bounds.origin, window.line_height(), window, cx);
        // Like AppKit, the cursor shows only in the key window.
        let focused = focus.is_focused(window) && window.is_window_active();
        if focused
            && self.input.read(cx).cursor_visible
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }
        self.input.update(cx, |input, cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
            input.painted_focused = focused;
            if focused && !input.blinking {
                input.start_blinking(cx);
            }
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The padding belongs to the input, so a click anywhere inside its frame lands here.
        div()
            .flex()
            .w_full()
            .px_2()
            .py(px(3.))
            .key_context(CONTEXT)
            .track_focus(&self.focus)
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
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::clear))
            .on_action(cx.listener(Self::submit))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(TextElement { input: cx.entity() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    #[gpui::test]
    fn cursor_blinks_while_focused_and_stays_solid_while_typing(cx: &mut TestAppContext) {
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("x", cx));
        let draw = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| window.draw(cx).clear());
            cx.run_until_parked();
        };
        let tick = |cx: &mut VisualTestContext| {
            cx.executor()
                .advance_clock(BLINK + Duration::from_millis(10));
            cx.run_until_parked();
        };
        let visible = |cx: &mut VisualTestContext| input.read_with(cx, |i, _| i.cursor_visible);
        let focus = input.read_with(cx, |i, _| i.focus.clone());
        cx.update(|window, _| {
            window.activate_window();
            window.focus(&focus);
        });
        draw(cx);
        assert!(visible(cx));
        tick(cx);
        assert!(!visible(cx), "off after one phase");
        draw(cx);
        tick(cx);
        assert!(visible(cx), "and on again");
        draw(cx);
        tick(cx);
        assert!(!visible(cx));

        // Typing shows it at once and restarts the cycle.
        input.update_in(cx, |i, window, cx| {
            i.replace_text_in_range(None, "a", window, cx)
        });
        assert!(visible(cx));
        draw(cx);
        tick(cx);
        assert!(!visible(cx));

        // An inactive window draws no cursor, so the blinking stops and leaves it shown.
        cx.deactivate_window();
        draw(cx);
        tick(cx);
        assert!(visible(cx));
        assert!(!input.read_with(cx, |i, _| i.blinking));
        draw(cx);
        tick(cx);
        assert!(visible(cx), "no blinking while inactive");
    }

    #[gpui::test]
    fn edits_by_char_with_selection_and_utf16_offsets(cx: &mut TestAppContext) {
        let input = cx.new(|cx| TextInput::new("x", cx));
        let cx = cx.add_empty_window();
        input.update_in(cx, |input, window, cx| {
            input.replace_text_in_range(None, "a😀b", window, cx);
            assert_eq!(input.selected_range, 6..6);
            input.backspace(&Backspace, window, cx);
            input.backspace(&Backspace, window, cx);
            assert_eq!(input.text(), "a", "the emoji goes as one char");
            input.replace_text_in_range(None, "😀c", window, cx);
            // UTF-16: a=1, 😀=2, c=1.
            assert_eq!(input.range_to_utf16(&(0..input.text().len())), 0..4);
            assert_eq!(input.offset_from_utf16(3), 5);

            input.home(&Home, window, cx);
            input.select_right(&SelectRight, window, cx);
            input.select_right(&SelectRight, window, cx);
            assert_eq!(&input.text()[input.selected_range.clone()], "a😀");
            input.replace_text_in_range(None, "x", window, cx);
            assert_eq!(input.text(), "xc");

            // IME: marked text replaces itself until committed.
            input.replace_and_mark_text_in_range(None, "n", None, window, cx);
            input.replace_and_mark_text_in_range(None, "ni", None, window, cx);
            assert_eq!(input.content, "xnic", "shown while composing");
            input.replace_text_in_range(None, "你", window, cx);
            assert_eq!(input.text(), "x你c");
            assert_eq!(input.marked_range, None);

            input.clear(&Clear, window, cx);
            assert_eq!(input.text(), "");
        });
    }

    #[gpui::test]
    fn a_digits_field_takes_only_digits_that_fit(cx: &mut TestAppContext) {
        let input = cx.new(|cx| TextInput::digits("10999", 5, cx));
        let cx = cx.add_empty_window();
        input.update_in(cx, |input, window, cx| {
            input.replace_text_in_range(None, "1a2-3", window, cx);
            assert_eq!(input.text(), "123");
            input.replace_text_in_range(None, "456789", window, cx);
            assert_eq!(input.text(), "12345", "at most five");
            // Over a selection, the selected digits make room.
            input.selected_range = 0..2;
            input.replace_text_in_range(None, "99x9", window, cx);
            assert_eq!(input.text(), "99345");
            input.replace_and_mark_text_in_range(None, "ni", None, window, cx);
            assert_eq!(input.content, "99345", "no composing letters");
            input.set_text("7", cx);
            assert_eq!(
                (input.text(), input.selected_range.clone()),
                ("7".into(), 1..1)
            );
        });
    }

    #[gpui::test]
    fn composition_reports_a_change_only_when_committed(cx: &mut TestAppContext) {
        let input = cx.new(|cx| TextInput::new("x", cx));
        let changes = std::rc::Rc::new(std::cell::Cell::new(0));
        let counter = changes.clone();
        cx.update(|cx| {
            cx.subscribe(&input, move |_, _: &Changed, _| {
                counter.set(counter.get() + 1)
            })
            .detach()
        });
        let cx = cx.add_empty_window();
        let mut step = |f: &dyn Fn(&mut TextInput, &mut Window, &mut Context<TextInput>)| {
            input.update_in(cx, |input, window, cx| f(input, window, cx));
            let n = changes.replace(0);
            (input.read_with(cx, |input, _| input.text()), n)
        };

        // Typed letters change the text at once.
        assert_eq!(
            step(&|i, w, cx| i.replace_text_in_range(None, "ab", w, cx)),
            ("ab".into(), 1)
        );
        // Composing: the pinyin is shown but neither committed nor reported.
        for pinyin in ["n", "ni", "nih"] {
            assert_eq!(
                step(&|i, w, cx| i.replace_and_mark_text_in_range(None, pinyin, None, w, cx)),
                ("ab".into(), 0)
            );
        }
        // Picking a candidate commits it.
        assert_eq!(
            step(&|i, w, cx| i.replace_text_in_range(None, "你好", w, cx)),
            ("ab你好".into(), 1)
        );
        // Committing by unmarking keeps the composed text.
        step(&|i, w, cx| i.replace_and_mark_text_in_range(None, "ok", None, w, cx));
        assert_eq!(
            step(&|i, w, cx| i.unmark_text(w, cx)),
            ("ab你好ok".into(), 1)
        );
        // Cancelling a composition commits nothing.
        step(&|i, w, cx| i.replace_and_mark_text_in_range(None, "x", None, w, cx));
        step(&|i, w, cx| i.replace_and_mark_text_in_range(None, "", None, w, cx));
        assert_eq!(
            step(&|i, w, cx| i.unmark_text(w, cx)),
            ("ab你好ok".into(), 0)
        );
        // Composing over a selection removes the selected text right away.
        step(&|i, _, _| {
            i.selected_range = 0..2;
        });
        assert_eq!(
            step(&|i, w, cx| i.replace_and_mark_text_in_range(None, "z", None, w, cx)),
            ("你好ok".into(), 1)
        );
    }
}
