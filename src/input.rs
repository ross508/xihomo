//! A single-line text input, adapted from GPUI's `examples/input.rs`: typing and IME composition
//! through `EntityInputHandler`, selection by keyboard and mouse, and the clipboard. The cursor
//! moves by `char`, not by grapheme cluster, to avoid a dependency for a filter field.
use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, GlobalElementId, KeyBinding, LayoutId,
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
/// Esc was pressed in a settings field, which keeps its text for the owner to restore.
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
    /// Esc reports `Cancelled` instead of clearing.
    cancels: bool,
    /// How far the line is shifted left, so the cursor stays in view.
    scroll: Pixels,
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
            ..Self::setting(placeholder, cx)
        }
    }

    /// A field that edits a saved value: Esc reports `Cancelled` instead of clearing.
    pub fn setting(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            cancels: true,
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
            cancels: false,
            scroll: px(0.),
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
        if self.cancels {
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
        line.closest_index_for_x(position.x - bounds.left() + self.scroll)
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
        utf8_offset(&self.content, offset)
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

/// The UTF-8 offset of a UTF-16 offset in `text`, clamped to its end.
fn utf8_offset(text: &str, utf16_offset: usize) -> usize {
    let mut utf16 = 0;
    for (utf8, ch) in text.char_indices() {
        if utf16 >= utf16_offset {
            return utf8;
        }
        utf16 += ch.len_utf16();
    }
    text.len()
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
        let sent = new_text;
        let new_text = &*self.accept(&range, sent);
        let marked = (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        self.replace(range.clone(), new_text, marked, cx);
        // The new selection is relative to the text the input method sent, so it no longer
        // applies once a digits field has dropped some of it.
        let end = range.start + new_text.len();
        self.selected_range = new_selected_range_utf16
            .filter(|_| new_text == sent)
            .map(|r| {
                range.start + utf8_offset(new_text, r.start)
                    ..range.start + utf8_offset(new_text, r.end)
            })
            .unwrap_or(end..end);
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
                bounds.left() - self.scroll + layout.x_for_index(range.start),
                bounds.top(),
            ),
            point(
                bounds.left() - self.scroll + layout.x_for_index(range.end),
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
        let index = self
            .last_layout
            .as_ref()?
            .index_for_x(local.x + self.scroll)?;
        Some(self.offset_to_utf16(index))
    }
}

/// Shapes and paints the line, its selection and cursor, and registers the input handler.
struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    scroll: Pixels,
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

        let scroll = if input.content.is_empty() {
            px(0.)
        } else {
            scroll_for(
                input.scroll,
                line.x_for_index(input.cursor_offset()),
                line.width,
                bounds.size.width,
            )
        };
        let x = |index| bounds.left() - scroll + line.x_for_index(index);
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
            scroll,
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
        let Some(line) = prepaint.line.take() else {
            return;
        };
        // Like AppKit, the cursor shows only in the key window.
        let focused = focus.is_focused(window) && window.is_window_active();
        let scroll = prepaint.scroll;
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            if let Some(selection) = prepaint.selection.take() {
                window.paint_quad(selection)
            }
            let origin = point(bounds.left() - scroll, bounds.top());
            let _ = line.paint(origin, window.line_height(), window, cx);
            if focused
                && self.input.read(cx).cursor_visible
                && let Some(cursor) = prepaint.cursor.take()
            {
                window.paint_quad(cursor);
            }
        });
        self.input.update(cx, |input, cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
            input.scroll = scroll;
            input.painted_focused = focused;
            if focused && !input.blinking {
                input.start_blinking(cx);
            }
        });
    }
}

/// The shift that keeps the cursor at `cursor` in a `width` wide view, moving no more than it must,
/// and leaving no empty room after the end of a `line` wide text.
fn scroll_for(scroll: Pixels, cursor: Pixels, line: Pixels, width: Pixels) -> Pixels {
    // Room for the cursor itself at the right edge.
    let width = width - px(2.);
    let scroll = scroll.min(cursor).max(cursor - width);
    scroll.min((line - width).max(px(0.))).max(px(0.))
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
            input.replace_and_mark_text_in_range(None, "n", Some(1..1), window, cx);
            input.replace_and_mark_text_in_range(None, "ni", Some(2..2), window, cx);
            assert_eq!(input.content, "xnic", "shown while composing");
            assert_eq!(
                input.selected_range,
                3..3,
                "the cursor follows the composition"
            );
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
            // An input method places the cursor inside the letters it sent; with the letters
            // dropped, the cursor stays inside the text it asks about next.
            input.selected_range = 5..5;
            input.replace_and_mark_text_in_range(None, "a", Some(1..1), window, cx);
            assert_eq!(input.selected_range, 5..5);
            input.selected_text_range(false, window, cx);
            input.replace_and_mark_text_in_range(None, "ab", Some(2..2), window, cx);
            assert_eq!(
                input.selected_text_range(false, window, cx).unwrap().range,
                5..5
            );
            input.set_text("7", cx);
            assert_eq!(
                (input.text(), input.selected_range.clone()),
                ("7".into(), 1..1)
            );
        });
    }

    #[test]
    fn the_view_moves_only_as_far_as_the_cursor_needs() {
        let s = |scroll: f32, cursor: f32, line: f32| {
            f32::from(scroll_for(px(scroll), px(cursor), px(line), px(102.)))
        };
        assert_eq!(s(0., 50., 300.), 0., "in view");
        assert_eq!(s(0., 250., 300.), 150., "the cursor at the right edge");
        assert_eq!(s(150., 200., 300.), 150., "still in view: no move");
        assert_eq!(s(150., 120., 300.), 120., "the cursor at the left edge");
        assert_eq!(s(150., 300., 300.), 200., "the end of the text");
        assert_eq!(s(200., 80., 80.), 0., "a shorter text leaves no gap");
    }

    /// An input narrower than its text.
    struct Narrow(Entity<TextInput>);
    impl Render for Narrow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(80.)).child(self.0.clone())
        }
    }

    #[gpui::test]
    fn a_long_line_scrolls_to_keep_the_cursor_in_view(cx: &mut TestAppContext) {
        let input = cx.new(|cx| TextInput::setting("x", cx));
        let cancelled = std::rc::Rc::new(std::cell::Cell::new(false));
        let seen = cancelled.clone();
        cx.update(|cx| {
            cx.subscribe(&input, move |_, _: &Cancelled, _| seen.set(true))
                .detach()
        });
        let shown = input.clone();
        let (_, cx) = cx.add_window_view(|_, _| Narrow(shown));
        let draw = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| window.draw(cx).clear());
            cx.run_until_parked();
        };
        let state = |cx: &mut VisualTestContext| {
            input.read_with(cx, |i, _| {
                let bounds = i.last_bounds.unwrap();
                let x = bounds.left() - i.scroll
                    + i.last_layout
                        .as_ref()
                        .unwrap()
                        .x_for_index(i.cursor_offset());
                (i.scroll, bounds.left() <= x && x <= bounds.right())
            })
        };
        input.update_in(cx, |i, window, cx| {
            i.replace_text_in_range(None, "https://cp.cloudflare.com/generate_204", window, cx)
        });
        draw(cx);
        let (scroll, visible) = state(cx);
        assert!(scroll > px(0.) && visible, "{scroll:?}");
        input.update_in(cx, |i, window, cx| i.home(&Home, window, cx));
        draw(cx);
        assert_eq!(state(cx), (px(0.), true));
        // A click lands on the character under the pointer, wherever the line is shifted.
        input.update_in(cx, |i, window, cx| i.end(&End, window, cx));
        draw(cx);
        let (end, at) = input.read_with(cx, |i, _| {
            (i.content.len(), i.last_bounds.unwrap().right() - px(1.))
        });
        assert_eq!(
            input.read_with(cx, |i, _| i.index_for_mouse_position(point(at, px(0.)))),
            end
        );

        input.update_in(cx, |i, window, cx| i.clear(&Clear, window, cx));
        assert!(
            cancelled.get(),
            "Esc in a setting reports instead of clearing"
        );
        assert_eq!(input.read_with(cx, |i, _| i.text().len()), end);
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
