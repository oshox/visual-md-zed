//! The properties panel: a block that stands in for a note's front matter while
//! no selection touches it, with a row for each property and a control to change
//! it.
//!
//! Every change is an ordinary edit of the buffer, made by [`crate::properties`]
//! as a replacement of the bytes of the thing that changed, so undo works and the
//! rest of the YAML stays as it was written. The one input that may be open at a
//! time lives in the addon, and the block is rebuilt when it opens or closes.
//!
//! A click inside the block must not reach the editor, which would put the
//! cursor in the front matter and so remove the block: the controls stop the
//! mouse-down event.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use editor::display_map::{
    BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId,
};
use editor::{Anchor, Editor, EditorEvent, MultiBufferOffset, MultiBufferSnapshot, ToOffset as _};
use gpui::AppContext as _;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, Context, ElementId, Entity, Focusable as _, InteractiveElement as _,
    IntoElement as _, MouseButton, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window, div, px, svg,
};
use theme::ActiveTheme as _;
use util::ResultExt as _;

use crate::VisualMdAddon;
use crate::plan::Plan;
use crate::properties::{self, EditError, Property, Value};

/// A guess at the panel's height in rows until the editor has drawn it.
const ESTIMATED_ROWS: u32 = 6;

static NEXT_INPUT: AtomicU64 = AtomicU64::new(1);

/// What a text input is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputTarget {
    /// The value of the property at this index.
    Value(usize),
    /// The name of the property at this index.
    Key(usize),
    /// A new item of the list at this index.
    ListItem(usize),
    /// The name of a property to add.
    NewProperty,
}

/// The one text input that is open.
pub(crate) struct PropertyInput {
    pub target: InputTarget,
    pub editor: Entity<Editor>,
    /// Why what was typed was not taken.
    pub error: Option<EditError>,
    serial: u64,
    _subscription: Subscription,
}

/// What a block depends on. Blocks are kept while their range and key are
/// unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PanelKey {
    body_hash: u64,
    input: Option<(u64, Option<EditError>)>,
}

/// The block standing in for the front matter.
pub(crate) struct PanelBlock {
    range: Range<usize>,
    key: PanelKey,
    id: CustomBlockId,
    /// Where the lines between the `---` are, kept across edits.
    body: Range<Anchor>,
}

/// What the block draws.
struct Panel {
    editor: WeakEntity<Editor>,
    properties: Vec<Property>,
    body: String,
    input: Option<(InputTarget, Entity<Editor>, Option<EditError>)>,
}

/// Diffs `computed.properties` against the block the previous refresh inserted.
pub(crate) fn apply_properties(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    text: &str,
    computed: &Plan,
    cx: &mut Context<Editor>,
) {
    let wanted = computed.properties.as_ref().and_then(|info| {
        let body = text.get(info.body_range.clone())?;
        Some((info, body, properties::parse(body)?))
    });

    let Some(addon) = editor.addon_mut::<VisualMdAddon>() else {
        return;
    };
    let previous = addon.properties_block.take();

    let Some((info, body, found)) = wanted else {
        addon.property_input = None;
        if let Some(block) = previous {
            editor.remove_blocks(std::iter::once(block.id).collect(), None, cx);
        }
        return;
    };

    // An input is only kept while it is for something that is still there.
    if addon.property_input.as_ref().is_some_and(|input| {
        let index = match input.target {
            InputTarget::Value(index) | InputTarget::Key(index) | InputTarget::ListItem(index) => {
                Some(index)
            }
            InputTarget::NewProperty => None,
        };
        index.is_some_and(|index| index >= found.len())
    }) {
        addon.property_input = None;
    }

    let key = PanelKey {
        body_hash: info.body_hash,
        input: addon
            .property_input
            .as_ref()
            .map(|input| (input.serial, input.error.clone())),
    };
    if let Some(block) = &previous
        && block.range == info.range
        && block.key == key
    {
        addon.properties_block = previous;
        return;
    }

    let input = addon
        .property_input
        .as_ref()
        .map(|input| (input.target, input.editor.clone(), input.error.clone()));
    if let Some(block) = previous {
        editor.remove_blocks(std::iter::once(block.id).collect(), None, cx);
    }

    let anchors = crate::to_anchor_range(snapshot, &info.range);
    let body_anchors = crate::to_anchor_range(snapshot, &info.body_range);
    let panel = Arc::new(Panel {
        editor: cx.weak_entity(),
        properties: found,
        body: body.to_string(),
        input,
    });
    let start = body_anchors.start;
    let ids = editor.insert_blocks(
        [BlockProperties {
            placement: BlockPlacement::Replace(anchors.start..=anchors.end),
            height: Some(ESTIMATED_ROWS),
            style: BlockStyle::Fixed,
            render: Arc::new(move |cx: &mut BlockContext| render_panel(&panel, start, cx)),
            priority: 0,
        }],
        None,
        cx,
    );
    if let (Some(id), Some(addon)) = (ids.into_iter().next(), editor.addon_mut::<VisualMdAddon>()) {
        addon.properties_block = Some(PanelBlock {
            range: info.range.clone(),
            key,
            id,
            body: body_anchors,
        });
    }
}

fn render_panel(panel: &Panel, start: Anchor, cx: &mut BlockContext) -> AnyElement {
    let (border, muted, background) = {
        let colors = cx.app.theme().colors();
        (colors.border, colors.text_muted, colors.surface_background)
    };
    let block_id = ElementId::from(cx.block_id);

    let mut rows = div().flex().flex_col().gap_1();
    for (index, property) in panel.properties.iter().enumerate() {
        rows = rows.child(render_row(panel, index, property, &block_id, cx));
    }

    let adding = match &panel.input {
        Some((InputTarget::NewProperty, input, error)) => {
            Some(render_input(panel, input, error.is_some(), cx))
        }
        _ => None,
    };
    let footer = match adding {
        Some(input) => input,
        None => {
            let editor = panel.editor.clone();
            button(&block_id, "add", "+ Add property", cx)
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    with_editor(&editor, cx, |editor, cx| {
                        begin_input(editor, InputTarget::NewProperty, window, cx)
                    });
                })
                .into_any_element()
        }
    };

    let error = panel
        .input
        .as_ref()
        .and_then(|(_, _, error)| error.as_ref())
        .map(|error| {
            div()
                .text_color(cx.app.theme().status().error)
                .child(SharedString::from(error.to_string()))
        });

    let reveal = {
        let editor = panel.editor.clone();
        button(&block_id, "yaml", "Edit as YAML", cx).on_click(move |_, window, cx| {
            cx.stop_propagation();
            with_editor(&editor, cx, |editor, cx| {
                crate::fence_render::reveal_source(editor, start, window, cx)
            });
        })
    };

    div()
        .id(block_id.clone())
        .debug_selector(|| "prop-panel".to_string())
        .w(cx.max_width)
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .border_1()
        .border_color(border)
        .rounded_md()
        .bg(background)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            div()
                .flex()
                .justify_between()
                .text_color(muted)
                .child(SharedString::from("Properties"))
                .child(reveal),
        )
        .child(rows)
        .child(footer)
        .children(error)
        .into_any_element()
}

fn button(
    block_id: &ElementId,
    name: &'static str,
    label: &'static str,
    cx: &BlockContext,
) -> gpui::Stateful<gpui::Div> {
    let colors = cx.app.theme().colors();
    div()
        .id(ElementId::from((block_id.clone(), name)))
        .debug_selector(|| format!("prop-{name}"))
        .cursor_pointer()
        .text_color(colors.text_muted)
        .hover(|style| style.text_color(colors.text))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(SharedString::from(label))
}

fn render_row(
    panel: &Panel,
    index: usize,
    property: &Property,
    block_id: &ElementId,
    cx: &mut BlockContext,
) -> AnyElement {
    let muted = cx.app.theme().colors().text_muted;

    let name = match &panel.input {
        Some((InputTarget::Key(open), input, error)) if *open == index => {
            render_input(panel, input, error.is_some(), cx)
        }
        _ => {
            let editor = panel.editor.clone();
            div()
                .id(ElementId::from((block_id.clone(), format!("key-{index}"))))
                .debug_selector(|| format!("prop-key-{index}"))
                .cursor_pointer()
                .w(px(140.))
                .flex_none()
                .text_color(muted)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    with_editor(&editor, cx, |editor, cx| {
                        begin_input(editor, InputTarget::Key(index), window, cx)
                    });
                })
                .child(SharedString::from(property.key.clone()))
                .into_any_element()
        }
    };

    let value = render_value(panel, index, property, block_id, cx);

    let remove = {
        let editor = panel.editor.clone();
        div()
            .id(ElementId::from((
                block_id.clone(),
                format!("remove-{index}"),
            )))
            .debug_selector(|| format!("prop-remove-{index}"))
            .cursor_pointer()
            .flex_none()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                with_editor(&editor, cx, |editor, cx| {
                    change(editor, window, cx, |body, found| {
                        properties::remove_property(body, found, index)
                    })
                });
            })
            .child(
                svg()
                    .path(icons::IconName::Close.path())
                    .size(px(12.))
                    .text_color(muted),
            )
    };

    div()
        .flex()
        .items_start()
        .gap_2()
        .child(name)
        .child(div().flex_1().min_w_0().child(value))
        .child(remove)
        .into_any_element()
}

fn render_value(
    panel: &Panel,
    index: usize,
    property: &Property,
    block_id: &ElementId,
    cx: &mut BlockContext,
) -> AnyElement {
    let (muted, accent, border, chip) = {
        let colors = cx.app.theme().colors();
        (
            colors.text_muted,
            colors.text_accent,
            colors.border,
            colors.element_background,
        )
    };

    if let Some((InputTarget::Value(open), input, error)) = &panel.input
        && *open == index
    {
        return render_input(panel, input, error.is_some(), cx);
    }

    let editor = panel.editor.clone();
    let line_height = cx.line_height;
    let text_button = |label: SharedString, shown_muted: bool| {
        let editor = editor.clone();
        div()
            .id(ElementId::from((
                block_id.clone(),
                format!("value-{index}"),
            )))
            .debug_selector(|| format!("prop-value-{index}"))
            .cursor_pointer()
            .min_h(line_height)
            .when(shown_muted, |this| this.text_color(muted))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                with_editor(&editor, cx, |editor, cx| {
                    begin_input(editor, InputTarget::Value(index), window, cx)
                });
            })
            .child(label)
            .into_any_element()
    };

    match &property.value {
        Value::Bool(checked) => {
            let checked = *checked;
            let editor = editor.clone();
            div()
                .id(ElementId::from((
                    block_id.clone(),
                    format!("value-{index}"),
                )))
                .debug_selector(|| format!("prop-value-{index}"))
                .cursor_pointer()
                .flex()
                .items_center()
                .justify_center()
                .size(px(14.))
                .rounded(px(3.))
                .border_1()
                .border_color(muted)
                .when(checked, |this| this.bg(accent).border_color(accent))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    with_editor(&editor, cx, |editor, cx| {
                        change(editor, window, cx, |body, found| {
                            properties::toggle_bool(body, found, index)
                        })
                    });
                })
                .children(checked.then(|| {
                    svg()
                        .path("icons/check.svg")
                        .size(px(10.))
                        .text_color(cx.app.theme().colors().editor_background)
                }))
                .into_any_element()
        }
        Value::Text(text) if text.is_empty() => text_button("Empty".into(), true),
        Value::Text(text) | Value::Number(text) | Value::Date(text) | Value::DateTime(text) => {
            text_button(text.clone().into(), false)
        }
        Value::List { items, .. } => {
            let mut chips = div().flex().flex_wrap().items_center().gap_1();
            for (item_index, item) in items.iter().enumerate() {
                let editor = editor.clone();
                chips = chips.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .rounded_md()
                        .bg(chip)
                        .child(SharedString::from(item.text.clone()))
                        .child(
                            div()
                                .id(ElementId::from((
                                    block_id.clone(),
                                    format!("item-{index}-{item_index}"),
                                )))
                                .debug_selector(|| format!("prop-item-{index}-{item_index}"))
                                .cursor_pointer()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(move |_, window, cx| {
                                    cx.stop_propagation();
                                    with_editor(&editor, cx, |editor, cx| {
                                        change(editor, window, cx, |body, found| {
                                            properties::list_remove(body, found, index, item_index)
                                        })
                                    });
                                })
                                .child(
                                    svg()
                                        .path(icons::IconName::Close.path())
                                        .size(px(10.))
                                        .text_color(muted),
                                ),
                        ),
                );
            }
            let adding = match &panel.input {
                Some((InputTarget::ListItem(open), input, error)) if *open == index => {
                    render_input(panel, input, error.is_some(), cx)
                }
                _ => {
                    let editor = editor.clone();
                    div()
                        .id(ElementId::from((block_id.clone(), format!("add-{index}"))))
                        .debug_selector(|| format!("prop-add-{index}"))
                        .cursor_pointer()
                        .px_1()
                        .rounded_md()
                        .border_1()
                        .border_color(border)
                        .text_color(muted)
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            with_editor(&editor, cx, |editor, cx| {
                                begin_input(editor, InputTarget::ListItem(index), window, cx)
                            });
                        })
                        .child(SharedString::from("+"))
                        .into_any_element()
                }
            };
            chips.child(adding).into_any_element()
        }
        Value::Complex => {
            let summary = property
                .value_range
                .as_ref()
                .and_then(|range| panel.body.get(range.clone()))
                .map(|written| {
                    let first = written.lines().next().unwrap_or_default().trim();
                    if written.trim().lines().count() > 1 {
                        format!("{first} …")
                    } else {
                        first.to_string()
                    }
                })
                .unwrap_or_default();
            div()
                .text_color(muted)
                .child(SharedString::from(summary))
                .into_any_element()
        }
    }
}

fn render_input(
    panel: &Panel,
    input: &Entity<Editor>,
    invalid: bool,
    cx: &mut BlockContext,
) -> AnyElement {
    let (focused_border, error) = {
        let theme = cx.app.theme();
        (theme.colors().border_focused, theme.status().error)
    };
    let confirm = panel.editor.clone();
    let cancel = panel.editor.clone();
    let cancel_editor_action = panel.editor.clone();
    div()
        .debug_selector(|| "prop-input".to_string())
        .min_w(px(120.))
        .flex_1()
        .px_1()
        .border_1()
        .border_color(if invalid { error } else { focused_border })
        .rounded_sm()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_action(move |_: &menu::Confirm, window, cx| {
            with_editor(&confirm, cx, |editor, cx| {
                commit_input(editor, true, window, cx)
            });
        })
        .on_action(move |_: &menu::Cancel, window, cx| {
            with_editor(&cancel, cx, |editor, cx| cancel_input(editor, window, cx));
        })
        .on_action(move |_: &editor::actions::Cancel, window, cx| {
            with_editor(&cancel_editor_action, cx, |editor, cx| {
                cancel_input(editor, window, cx)
            });
        })
        .child(input.clone())
        .into_any_element()
}

/// Runs `update` on the editor a click came from, if it is still there.
fn with_editor(
    editor: &WeakEntity<Editor>,
    cx: &mut App,
    update: impl FnOnce(&mut Editor, &mut Context<Editor>),
) {
    editor.update(cx, update).log_err();
}

/// The body of the front matter as it is now, with its properties and where it
/// starts in the buffer.
fn live_body(editor: &Editor, cx: &App) -> Option<(String, Vec<Property>, usize)> {
    let addon = editor.addon::<VisualMdAddon>()?;
    let block = addon.properties_block.as_ref()?;
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let start = block.body.start.to_offset(&snapshot);
    let end = block.body.end.to_offset(&snapshot);
    let body: String = snapshot.text_for_range(start..end).collect();
    let found = properties::parse(&body)?;
    Some((body, found, start.0))
}

/// Makes the edits `make_edits` returns for the front matter as it is now, as
/// one change of the buffer.
fn try_change(
    editor: &mut Editor,
    cx: &mut Context<Editor>,
    make_edits: impl FnOnce(&str, &[Property]) -> Result<Vec<properties::Edit>, EditError>,
) -> Result<(), EditError> {
    let (body, found, start) = live_body(editor, cx).ok_or(EditError::NotEditable)?;
    let edits = make_edits(&body, &found)?;
    editor.edit(
        edits.into_iter().map(|(range, replacement)| {
            (
                MultiBufferOffset(start + range.start)..MultiBufferOffset(start + range.end),
                replacement,
            )
        }),
        cx,
    );
    Ok(())
}

/// [`try_change`] for a control that has nowhere to show why it did not work.
fn change(
    editor: &mut Editor,
    _window: &mut Window,
    cx: &mut Context<Editor>,
    make_edits: impl FnOnce(&str, &[Property]) -> Result<Vec<properties::Edit>, EditError>,
) {
    if let Err(error) = try_change(editor, cx, make_edits) {
        log::warn!("could not change the properties: {error}");
    }
}

/// Opens a text input in place of what `target` shows.
pub(crate) fn begin_input(
    editor: &mut Editor,
    target: InputTarget,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let Some((_, found, _)) = live_body(editor, cx) else {
        return;
    };
    let (text, placeholder) = match target {
        InputTarget::Value(index) => match found.get(index).map(|property| &property.value) {
            Some(
                Value::Text(text) | Value::Number(text) | Value::Date(text) | Value::DateTime(text),
            ) => (text.clone(), "Value"),
            _ => return,
        },
        InputTarget::Key(index) => match found.get(index) {
            Some(property) => (property.key.clone(), "Name"),
            None => return,
        },
        InputTarget::ListItem(_) => (String::new(), "New item"),
        InputTarget::NewProperty => (String::new(), "Property name"),
    };

    let input = cx.new(|cx| {
        let mut input = Editor::single_line(window, cx);
        input.set_placeholder_text(placeholder, window, cx);
        input.set_text(text, window, cx);
        input
    });
    input.update(cx, |input, cx| {
        input.select_all(&editor::actions::SelectAll, window, cx)
    });
    let subscription = cx.subscribe_in(
        &input,
        window,
        |editor, _input, event: &EditorEvent, window, cx| {
            if matches!(event, EditorEvent::Blurred) {
                commit_input(editor, false, window, cx);
            }
        },
    );
    let Some(addon) = editor.addon_mut::<VisualMdAddon>() else {
        return;
    };
    addon.property_input = Some(PropertyInput {
        target,
        editor: input.clone(),
        error: None,
        serial: NEXT_INPUT.fetch_add(1, Ordering::Relaxed),
        _subscription: subscription,
    });
    crate::force_refresh(editor, window, cx);
    window.focus(&input.focus_handle(cx), cx);
}

/// Takes what was typed into the open input. When `refocus` is set the editor
/// gets the keyboard back, which a confirming Enter means and a click elsewhere
/// does not.
pub(crate) fn commit_input(
    editor: &mut Editor,
    refocus: bool,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let Some(mut input) = editor
        .addon_mut::<VisualMdAddon>()
        .and_then(|addon| addon.property_input.take())
    else {
        return;
    };
    let typed = input.editor.read(cx).text(cx);

    let target = input.target;
    let mut added = None;
    let result = try_change(editor, cx, |body, found| match target {
        InputTarget::Value(index) => properties::set_value(body, found, index, &typed),
        InputTarget::Key(index) => properties::rename_property(body, found, index, &typed),
        InputTarget::ListItem(index) => properties::list_add(body, found, index, &typed),
        InputTarget::NewProperty => {
            let edits = properties::add_property(body, found, &typed)?;
            added = Some(found.len());
            Ok(edits)
        }
    });

    match result {
        Ok(()) => {
            if let (Some(index), true) = (added, refocus) {
                // The new property has no value yet, and is what is typed next.
                crate::refresh(editor, window, cx);
                begin_input(editor, InputTarget::Value(index), window, cx);
                return;
            }
            if refocus {
                window.focus(&editor.focus_handle(cx), cx);
            }
            crate::force_refresh(editor, window, cx);
        }
        Err(error) => {
            input.error = Some(error);
            if let Some(addon) = editor.addon_mut::<VisualMdAddon>() {
                addon.property_input = Some(input);
            }
            crate::force_refresh(editor, window, cx);
        }
    }
}

/// Closes the open input without taking what was typed.
pub(crate) fn cancel_input(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let had_input = editor
        .addon_mut::<VisualMdAddon>()
        .is_some_and(|addon| addon.property_input.take().is_some());
    if had_input {
        window.focus(&editor.focus_handle(cx), cx);
        crate::force_refresh(editor, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Modifiers, TestAppContext, VisualTestContext, WindowHandle};
    use serde_json::json;

    use crate::integration_tests::{editor_in_project, init_test};

    use super::*;

    const NOTE: &str = "---\ntitle: Hello\ndone: false\ncount: 3\ntags: [a, b]\naliases:\n  - one\nnested:\n  a: 1\n---\n\nbody text\n";

    async fn open(cx: &mut TestAppContext, text: &str) -> WindowHandle<Editor> {
        init_test(cx);
        let window = editor_in_project(cx, json!({ "a.md": text }), "/dir/a.md").await;
        // Focus only moves between elements of a window that is the active one.
        window
            .update(cx, |_, window, _| window.activate_window())
            .expect("the window is open");
        cx.run_until_parked();
        window
    }

    fn buffer_text(window: &WindowHandle<Editor>, cx: &mut TestAppContext) -> String {
        window
            .read_with(cx, |editor, cx| {
                editor.buffer().read(cx).snapshot(cx).text()
            })
            .expect("the window is open")
    }

    fn block_id(window: &WindowHandle<Editor>, cx: &mut TestAppContext) -> Option<CustomBlockId> {
        window
            .read_with(cx, |editor, _| {
                editor
                    .addon::<VisualMdAddon>()
                    .and_then(|addon| addon.properties_block.as_ref().map(|block| block.id))
            })
            .expect("the window is open")
    }

    fn input_target(
        window: &WindowHandle<Editor>,
        cx: &mut TestAppContext,
    ) -> Option<(InputTarget, Option<EditError>)> {
        window
            .read_with(cx, |editor, _| {
                editor.addon::<VisualMdAddon>().and_then(|addon| {
                    addon
                        .property_input
                        .as_ref()
                        .map(|input| (input.target, input.error.clone()))
                })
            })
            .expect("the window is open")
    }

    fn editor_has_the_keyboard(window: &WindowHandle<Editor>, cx: &mut TestAppContext) -> bool {
        window
            .update(cx, |editor, window, cx| {
                editor.focus_handle(cx).is_focused(window)
            })
            .expect("the window is open")
    }

    fn click(window: &WindowHandle<Editor>, cx: &mut TestAppContext, selector: &'static str) {
        let mut visual = VisualTestContext::from_window((*window).into(), cx);
        let bounds = visual
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("nothing is drawn as {selector}"));
        visual.simulate_click(bounds.center(), Modifiers::none());
        visual.run_until_parked();
    }

    fn is_drawn(
        window: &WindowHandle<Editor>,
        cx: &mut TestAppContext,
        selector: &'static str,
    ) -> bool {
        VisualTestContext::from_window((*window).into(), cx)
            .debug_bounds(selector)
            .is_some()
    }

    fn type_text(window: &WindowHandle<Editor>, cx: &mut TestAppContext, text: &str) {
        let mut visual = VisualTestContext::from_window((*window).into(), cx);
        visual.dispatch_action(editor::actions::SelectAll);
        visual.simulate_input(text);
        visual.run_until_parked();
    }

    fn confirm(window: &WindowHandle<Editor>, cx: &mut TestAppContext) {
        let mut visual = VisualTestContext::from_window((*window).into(), cx);
        visual.dispatch_action(menu::Confirm);
        visual.run_until_parked();
    }

    fn put_cursor_at(window: &WindowHandle<Editor>, cx: &mut TestAppContext, offset: usize) {
        window
            .update(cx, |editor, window, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections
                        .select_ranges([MultiBufferOffset(offset)..MultiBufferOffset(offset)]);
                });
                crate::refresh(editor, window, cx);
            })
            .expect("the window is open");
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_front_matter_is_a_panel_until_the_cursor_touches_it(cx: &mut TestAppContext) {
        let window = open(cx, NOTE).await;
        assert!(block_id(&window, cx).is_some());
        assert!(is_drawn(&window, cx, "prop-value-0"));

        put_cursor_at(&window, cx, 10);
        assert!(block_id(&window, cx).is_none(), "the cursor is in the YAML");

        put_cursor_at(&window, cx, NOTE.len());
        assert!(block_id(&window, cx).is_some());
        assert_eq!(
            buffer_text(&window, cx),
            NOTE,
            "none of this edits the note"
        );
    }

    #[gpui::test]
    async fn test_a_click_on_the_panel_does_not_reach_the_note(cx: &mut TestAppContext) {
        let window = open(cx, NOTE).await;
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        let bounds = visual
            .debug_bounds("prop-panel")
            .expect("the panel is drawn");

        visual.simulate_click(
            bounds.origin + gpui::point(gpui::px(2.), gpui::px(2.)),
            Modifiers::none(),
        );
        visual.run_until_parked();

        assert!(
            block_id(&window, cx).is_some(),
            "the cursor stayed out of the YAML"
        );
        let head = window
            .update(cx, |editor, _, cx| {
                let display_snapshot = editor.display_snapshot(cx);
                editor
                    .selections
                    .newest::<MultiBufferOffset>(&display_snapshot)
                    .head()
                    .0
            })
            .expect("the window is open");
        assert_eq!(head, NOTE.len());
    }

    #[gpui::test]
    async fn test_front_matter_that_is_not_properties_stays_source(cx: &mut TestAppContext) {
        let window = open(cx, "---\n- not\n- a mapping\n---\n\nbody\n").await;

        assert!(block_id(&window, cx).is_none());
    }

    #[gpui::test]
    async fn test_a_checkbox_flips_a_value_in_one_undoable_step(cx: &mut TestAppContext) {
        let window = open(cx, NOTE).await;

        click(&window, cx, "prop-value-1");
        assert_eq!(
            buffer_text(&window, cx),
            NOTE.replace("done: false", "done: true")
        );
        assert!(block_id(&window, cx).is_some(), "the panel is still there");

        window
            .update(cx, |editor, window, cx| {
                editor.undo(&editor::actions::Undo, window, cx)
            })
            .expect("the window is open");
        cx.run_until_parked();
        assert_eq!(buffer_text(&window, cx), NOTE);
    }

    #[gpui::test]
    async fn test_a_text_value_is_typed_over_and_taken_with_enter(cx: &mut TestAppContext) {
        let window = open(cx, NOTE).await;

        click(&window, cx, "prop-value-0");
        assert_eq!(
            input_target(&window, cx).map(|(target, _)| target),
            Some(InputTarget::Value(0))
        );
        assert!(is_drawn(&window, cx, "prop-input"));
        type_text(&window, cx, "A new: title");
        assert_eq!(
            buffer_text(&window, cx),
            NOTE,
            "nothing is written while typing"
        );
        confirm(&window, cx);

        assert_eq!(
            buffer_text(&window, cx),
            NOTE.replace("title: Hello", "title: \"A new: title\"")
        );
        assert_eq!(input_target(&window, cx), None);
        assert!(
            is_drawn(&window, cx, "prop-value-0"),
            "the value is shown again"
        );
        let focused = window
            .update(cx, |editor, window, cx| {
                editor.focus_handle(cx).is_focused(window)
            })
            .expect("the window is open");
        assert!(focused, "the keyboard is back in the note");
    }

    #[gpui::test]
    async fn test_escape_closes_the_input_and_changes_nothing(cx: &mut TestAppContext) {
        let window = open(cx, NOTE).await;
        click(&window, cx, "prop-value-0");
        type_text(&window, cx, "discarded");

        let mut visual = VisualTestContext::from_window(window.into(), cx);
        visual.dispatch_action(menu::Cancel);
        visual.run_until_parked();

        assert_eq!(buffer_text(&window, cx), NOTE);
        assert_eq!(input_target(&window, cx), None);
        assert!(editor_has_the_keyboard(&window, cx));
    }

    #[gpui::test]
    async fn test_escape_in_the_editor_closes_the_input_too(cx: &mut TestAppContext) {
        let window = open(cx, NOTE).await;
        click(&window, cx, "prop-value-0");
        type_text(&window, cx, "discarded");

        let mut visual = VisualTestContext::from_window(window.into(), cx);
        visual.dispatch_action(editor::actions::Cancel);
        visual.run_until_parked();

        assert_eq!(buffer_text(&window, cx), NOTE);
        assert_eq!(input_target(&window, cx), None);
    }

    #[gpui::test]
    async fn test_a_number_that_is_not_one_keeps_the_input_open_and_says_why(
        cx: &mut TestAppContext,
    ) {
        let window = open(cx, NOTE).await;
        click(&window, cx, "prop-value-2");
        type_text(&window, cx, "many");
        confirm(&window, cx);

        assert_eq!(buffer_text(&window, cx), NOTE);
        assert_eq!(
            input_target(&window, cx),
            Some((InputTarget::Value(2), Some(EditError::NotANumber)))
        );
        assert!(is_drawn(&window, cx, "prop-input"));

        type_text(&window, cx, "4.5");
        confirm(&window, cx);
        assert_eq!(
            buffer_text(&window, cx),
            NOTE.replace("count: 3", "count: 4.5")
        );
        assert_eq!(input_target(&window, cx), None);
    }

    #[gpui::test]
    async fn test_clicking_into_the_note_takes_what_was_typed(cx: &mut TestAppContext) {
        let window = open(cx, NOTE).await;
        click(&window, cx, "prop-value-0");
        type_text(&window, cx, "Typed");

        window
            .update(cx, |editor, window, cx| {
                window.focus(&editor.focus_handle(cx), cx)
            })
            .expect("the window is open");
        // The editor that lost the keyboard hears of it as a frame is drawn,
        // not while the one that took it is being updated.
        cx.update_window(window.into(), |_, window, cx| {
            window.simulate_next_frame(cx)
        })
        .expect("the window is open");
        cx.run_until_parked();

        assert_eq!(
            buffer_text(&window, cx),
            NOTE.replace("title: Hello", "title: Typed")
        );
        assert_eq!(input_target(&window, cx), None);
    }

    #[gpui::test]
    async fn test_list_items_are_removed_by_their_button_and_added_with_an_input(
        cx: &mut TestAppContext,
    ) {
        let window = open(cx, NOTE).await;

        click(&window, cx, "prop-item-3-0");
        assert_eq!(buffer_text(&window, cx), NOTE.replace("[a, b]", "[b]"));

        click(&window, cx, "prop-add-3");
        assert_eq!(
            input_target(&window, cx).map(|(target, _)| target),
            Some(InputTarget::ListItem(3))
        );
        type_text(&window, cx, "c d");
        confirm(&window, cx);
        assert_eq!(buffer_text(&window, cx), NOTE.replace("[a, b]", "[b, c d]"));

        click(&window, cx, "prop-item-4-0");
        assert_eq!(
            buffer_text(&window, cx),
            NOTE.replace("[a, b]", "[b, c d]")
                .replace("aliases:\n  - one\n", "aliases:\n")
        );
    }

    #[gpui::test]
    async fn test_a_property_is_removed_by_its_button(cx: &mut TestAppContext) {
        let window = open(cx, NOTE).await;

        click(&window, cx, "prop-remove-2");

        assert_eq!(buffer_text(&window, cx), NOTE.replace("count: 3\n", ""));
    }

    #[gpui::test]
    async fn test_a_property_is_added_by_name_and_then_given_a_value(cx: &mut TestAppContext) {
        let window = open(cx, NOTE).await;

        click(&window, cx, "prop-add");
        assert_eq!(
            input_target(&window, cx).map(|(target, _)| target),
            Some(InputTarget::NewProperty)
        );
        type_text(&window, cx, "status");
        confirm(&window, cx);
        assert_eq!(
            buffer_text(&window, cx),
            NOTE.replace("  a: 1\n---", "  a: 1\nstatus:\n---")
        );
        assert_eq!(
            input_target(&window, cx).map(|(target, _)| target),
            Some(InputTarget::Value(6)),
            "the value of the new property is asked for next"
        );

        type_text(&window, cx, "draft");
        confirm(&window, cx);
        assert_eq!(
            buffer_text(&window, cx),
            NOTE.replace("  a: 1\n---", "  a: 1\nstatus: draft\n---")
        );
    }

    #[gpui::test]
    async fn test_a_name_that_is_taken_is_refused(cx: &mut TestAppContext) {
        let window = open(cx, NOTE).await;

        click(&window, cx, "prop-add");
        type_text(&window, cx, "title");
        confirm(&window, cx);

        assert_eq!(buffer_text(&window, cx), NOTE);
        assert_eq!(
            input_target(&window, cx),
            Some((
                InputTarget::NewProperty,
                Some(EditError::DuplicateName("title".to_string()))
            ))
        );
    }

    #[gpui::test]
    async fn test_a_property_is_renamed_by_clicking_its_name(cx: &mut TestAppContext) {
        let window = open(cx, NOTE).await;

        click(&window, cx, "prop-key-0");
        type_text(&window, cx, "heading");
        confirm(&window, cx);

        assert_eq!(buffer_text(&window, cx), NOTE.replace("title:", "heading:"));
    }

    #[gpui::test]
    async fn test_edit_as_yaml_shows_the_source_with_the_cursor_in_it(cx: &mut TestAppContext) {
        let window = open(cx, NOTE).await;

        click(&window, cx, "prop-yaml");

        assert!(block_id(&window, cx).is_none());
        assert_eq!(buffer_text(&window, cx), NOTE);
        let head = window
            .update(cx, |editor, _, cx| {
                let display_snapshot = editor.display_snapshot(cx);
                editor
                    .selections
                    .newest::<MultiBufferOffset>(&display_snapshot)
                    .head()
                    .0
            })
            .expect("the window is open");
        assert!(
            head > 0 && head < NOTE.find("---\n\nbody").unwrap_or(0) + 3,
            "{head}"
        );
    }

    #[gpui::test]
    async fn test_the_panel_is_kept_while_the_front_matter_is_not_edited(cx: &mut TestAppContext) {
        let window = open(cx, NOTE).await;
        let first = block_id(&window, cx);

        window
            .update(cx, |editor, window, cx| {
                editor.insert("typed ", window, cx);
            })
            .expect("the window is open");
        cx.run_until_parked();
        assert_eq!(block_id(&window, cx), first, "typing in the body keeps it");

        click(&window, cx, "prop-value-1");
        assert_ne!(
            block_id(&window, cx),
            first,
            "an edit of the YAML rebuilds it"
        );
    }

    #[gpui::test]
    async fn test_an_input_goes_when_its_property_does(cx: &mut TestAppContext) {
        let window = open(cx, NOTE).await;
        click(&window, cx, "prop-add-4");
        assert_eq!(
            input_target(&window, cx).map(|(target, _)| target),
            Some(InputTarget::ListItem(4))
        );

        // Takes the text from `from` up to `to` out of the note as it is now.
        let delete =
            |window: &WindowHandle<Editor>, cx: &mut TestAppContext, from: &str, to: &str| {
                let current = buffer_text(window, cx);
                let start = current.find(from).expect("the text has it");
                let end = start + current[start..].find(to).expect("the text has it");
                window
                    .update(cx, |editor, _, cx| {
                        editor.edit([(MultiBufferOffset(start)..MultiBufferOffset(end), "")], cx);
                    })
                    .expect("the window is open");
                cx.run_until_parked();
            };

        delete(&window, cx, "title: Hello\n", "done: false");
        assert_eq!(
            input_target(&window, cx).map(|(target, _)| target),
            Some(InputTarget::ListItem(4)),
            "five properties are left, and the fifth is still there"
        );
        delete(&window, cx, "tags: [a, b]\n", "aliases:");
        assert_eq!(
            input_target(&window, cx),
            None,
            "four are left, so there is no fifth for the input to be for"
        );
    }

    #[gpui::test]
    async fn test_the_panel_follows_an_edit_that_leaves_the_ranges_alone(cx: &mut TestAppContext) {
        let window = open(cx, NOTE).await;
        let first = block_id(&window, cx);
        let start = NOTE.find("count: 3").expect("the text has it") + "count: ".len();

        window
            .update(cx, |editor, _, cx| {
                editor.edit(
                    [(MultiBufferOffset(start)..MultiBufferOffset(start + 1), "4")],
                    cx,
                );
            })
            .expect("the window is open");
        cx.run_until_parked();

        assert_ne!(
            block_id(&window, cx),
            first,
            "the block shows a 3 otherwise"
        );
    }

    /// The height in rows of every block, in document order.
    fn block_heights(window: &WindowHandle<Editor>, cx: &mut TestAppContext) -> Vec<u32> {
        window
            .update(cx, |editor, window, cx| {
                let snapshot = editor.snapshot(window, cx);
                snapshot
                    .blocks_in_range(
                        editor::display_map::DisplayRow(0)..editor::display_map::DisplayRow(500),
                    )
                    .map(|(_, block)| block.height())
                    .collect()
            })
            .expect("the window is open")
    }

    #[gpui::test]
    async fn test_the_panel_is_as_tall_as_its_rows(cx: &mut TestAppContext) {
        let window = open(cx, NOTE).await;
        let before = block_heights(&window, cx);
        assert_eq!(before.len(), 1, "{before:?}");
        assert_ne!(before[0], ESTIMATED_ROWS, "the estimate was never replaced");

        click(&window, cx, "prop-remove-2");
        click(&window, cx, "prop-remove-1");
        let after = block_heights(&window, cx);
        assert!(after[0] < before[0], "{before:?} then {after:?}");
    }
}
