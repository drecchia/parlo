//! External tools: their Settings list, their rows in the Forward dialog,
//! and the dialog's second step, where a picked tool gets an optional prompt.

use egui::{Sense, pos2, vec2};

use crate::app::App;
use crate::external_tools::{Tool, ToolKind};
use crate::model::Action;
use crate::theme::{self, Icon, Palette};

/// The Settings list: kind, name, target and whether the answer returns,
/// edited in place.
pub(super) fn settings(ui: &mut egui::Ui, app: &mut App) {
    let palette = app.palette;
    ui.add(
        egui::Label::new(
            egui::RichText::new(
                "Send selected messages to these tools from the Forward dialog. \
                 Their text and media leave ZapFast. With Answer on, the tool's \
                 answer comes back as a draft in the message box.",
            )
            .font(theme::regular(13.0))
            .color(palette.secondary),
        )
        .wrap(),
    );
    ui.add_space(6.0);
    let mut changed = false;
    let mut remove = None;
    for (index, tool) in app.settings.external_tools.iter_mut().enumerate() {
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt(("external-tool-kind", index))
                .selected_text(tool.kind.label())
                .width(110.0)
                .show_ui(ui, |ui| {
                    for kind in ToolKind::ALL {
                        if ui
                            .selectable_label(tool.kind == kind, kind.label())
                            .clicked()
                            && tool.kind != kind
                        {
                            tool.kind = kind;
                            changed = true;
                        }
                    }
                });
            changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut tool.name)
                        .hint_text("Name")
                        .font(theme::regular(13.0))
                        .text_color(palette.text)
                        .desired_width(120.0),
                )
                .changed();
            let hint = match tool.kind {
                ToolKind::Webhook => "https://…",
                ToolKind::Orca => "Bot name",
            };
            changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut tool.target)
                        .hint_text(hint)
                        .font(theme::regular(13.0))
                        .text_color(palette.text)
                        .desired_width(200.0),
                )
                .changed();
            changed |= ui
                .checkbox(&mut tool.answer, "Answer")
                .on_hover_text("Put the tool's answer in the message box as a draft")
                .changed();
            if theme::icon_button(
                ui,
                Icon::Trash,
                16.0,
                palette.secondary,
                palette.surface_hover,
                "Remove",
            )
            .clicked()
            {
                remove = Some(index);
            }
        });
    }
    if let Some(index) = remove {
        app.settings.external_tools.remove(index);
        changed = true;
    }
    if theme::soft_button(ui, &palette, Some(Icon::Plus), "Add tool", false).clicked() {
        app.settings.external_tools.push(Tool::default());
        changed = true;
    }
    if changed {
        app.actions.push(Action::SettingsChanged);
    }
}

/// The longest prompt the Forward dialog takes, in characters.
const PROMPT_LIMIT: usize = 4000;
/// A tool row, as tall as the dialog's chat rows.
const ROW_HEIGHT: f32 = 52.0;

/// The usable tools whose name holds the search, as `(index, name)`.
fn listed(app: &App, needle: &str) -> Vec<(usize, String)> {
    app.settings
        .external_tools
        .iter()
        .enumerate()
        .filter(|(_, tool)| tool.usable() && tool.name.to_lowercase().contains(needle))
        .map(|(index, tool)| (index, tool.name.trim().to_owned()))
        .collect()
}

/// A list section's label, as the chat list draws its search sections.
fn section_label(ui: &mut egui::Ui, palette: &Palette, label: &str) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        theme::text(ui, label, theme::semibold(12.5), palette.accent);
    });
    ui.add_space(4.0);
}

/// The round accent badge that stands for a tool where a chat has its photo.
fn paint_badge(ui: &egui::Ui, palette: &Palette, center: egui::Pos2) {
    let circle = egui::Rect::from_center_size(center, egui::Vec2::splat(38.0));
    ui.painter()
        .circle_filled(circle.center(), circle.width() / 2.0, palette.accent);
    theme::paint_icon(ui, Icon::ExternalLink, circle, 18.0, palette.on_accent);
}

/// A "Tools" section with one row per tool whose name holds the search, then
/// the "Chats" label for the list below it. A click on a row opens the
/// dialog's tool step. The rows take at most half the room left above
/// `bottom`, scrolling past that, so the chats stay in view. Without tools
/// the dialog is as it was.
pub(super) fn forward_rows(app: &mut App, ui: &mut egui::Ui, needle: &str, bottom: f32) {
    let palette = app.palette;
    let tools = listed(app, needle);
    if tools.is_empty() {
        return;
    }
    // The labels only where the lists keep room for a few rows under them.
    let labels = bottom - ui.cursor().top() > 4.0 * ROW_HEIGHT;
    if labels {
        section_label(ui, &palette, &crate::i18n::gettext(app.locale, "Tools"));
    }
    let room = ((bottom - ui.cursor().top() - 8.0) / 2.0).max(0.0);
    egui::ScrollArea::vertical()
        .id_salt("forward-tools")
        .max_height(room)
        .min_scrolled_height(0.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (index, name) in tools {
                let (rect, response) =
                    ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
                theme::reveal_focus(&response);
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &name)
                });
                #[cfg(any(test, feature = "demo"))]
                ui.ctx().data_mut(|data| {
                    data.insert_temp(egui::Id::new(("forward-tool", index)), rect)
                });
                if ui.is_rect_visible(rect) {
                    if response.hovered() || response.has_focus() {
                        super::widgets::dialog_row_highlight(ui, rect, palette.surface_hover);
                    }
                    paint_badge(ui, &palette, pos2(rect.left() + 23.0, rect.center().y));
                    let line = super::widgets::line(
                        ui,
                        &name,
                        theme::medium(14.5),
                        palette.text,
                        rect.width() - 90.0,
                        1,
                    );
                    line.paint(
                        ui,
                        pos2(rect.left() + 50.0, rect.center().y - line.size().y / 2.0),
                        palette.text,
                    );
                    let chevron = egui::Rect::from_center_size(
                        pos2(rect.right() - 18.0, rect.center().y),
                        egui::Vec2::splat(18.0),
                    );
                    theme::paint_icon(ui, Icon::ChevronRight, chevron, 18.0, palette.secondary);
                }
                if response
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    app.actions.push(Action::PickTool(index));
                }
            }
        });
    if labels {
        section_label(ui, &palette, &crate::i18n::gettext(app.locale, "Chats"));
    }
}

/// The dialog's tool step: the picked tool, and the prompt it may get with
/// the messages. Fills the room above `bottom`, where the footer is.
pub(super) fn forward_step(app: &mut App, ui: &mut egui::Ui, bottom: f32) {
    let palette = app.palette;
    let Some(tool) = app
        .tool_pick
        .and_then(|index| app.settings.external_tools.get(index))
        .cloned()
    else {
        return;
    };
    // The picked tool, as its row in the list showed it.
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::hover());
    paint_badge(ui, &palette, pos2(rect.left() + 23.0, rect.center().y));
    let name = super::widgets::line(
        ui,
        tool.name.trim(),
        theme::medium(14.5),
        palette.text,
        rect.width() - 60.0,
        1,
    );
    let kind = super::widgets::line(
        ui,
        tool.kind.label(),
        theme::regular(13.0),
        palette.secondary,
        rect.width() - 60.0,
        1,
    );
    let top = rect.center().y - (name.size().y + kind.size().y) / 2.0;
    name.paint(ui, pos2(rect.left() + 50.0, top), palette.text);
    kind.paint(
        ui,
        pos2(rect.left() + 50.0, top + name.size().y),
        palette.secondary,
    );

    // In a short window the label, then the note, give way to the field.
    let room = bottom - ui.cursor().top();
    if room > 90.0 {
        section_label(ui, &palette, &crate::i18n::gettext(app.locale, "Prompt"));
    }
    if room > 200.0 {
        // Indented as the label above it.
        egui::Frame::new()
            .inner_margin(egui::Margin {
                left: 4,
                ..egui::Margin::ZERO
            })
            .show(ui, |ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(
                            "Optional. Tells the tool what to do with these messages; \
                             it gets them either way.",
                        )
                        .font(theme::regular(13.0))
                        .color(palette.secondary),
                    )
                    .wrap(),
                );
            });
        ui.add_space(4.0);
    }
    ui.add_space(4.0);

    // A rounded field like the search field, filling the room to the footer.
    let id = egui::Id::new("forward-tool-prompt");
    let focused = ui.memory(|memory| memory.has_focus(id));
    let height = (bottom - ui.cursor().top() - 8.0).max(20.0);
    let (field, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let radius = 12.0;
    ui.painter().rect_filled(
        field,
        radius,
        if focused {
            palette.surface_hover
        } else {
            palette.surface
        },
    );
    // Registered before the editor, so the editor wins its own clicks and a
    // click elsewhere in the field still types in it.
    let surface = ui
        .interact(field, id.with("surface"), Sense::CLICK)
        .on_hover_cursor(egui::CursorIcon::Text);
    // The padding shrinks with the field, so a short one keeps a line to type in.
    let padding = ((field.height() - 18.0) / 2.0).clamp(2.0, 10.0);
    let inner = field.shrink2(vec2(14.0, padding));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner));
    let rows = ((inner.height() / 20.0).floor() as usize).max(1);
    let output = egui::ScrollArea::vertical()
        .id_salt("forward-tool-prompt-scroll")
        .max_height(inner.height())
        .auto_shrink([false, false])
        .show(&mut child, |ui| {
            egui::TextEdit::multiline(&mut app.tool_prompt)
                .id(id)
                .hint_text(
                    egui::RichText::new("e.g. Summarize this and open a ticket")
                        .color(palette.dim)
                        .font(theme::regular(14.0)),
                )
                .char_limit(PROMPT_LIMIT)
                .desired_rows(rows)
                .desired_width(f32::INFINITY)
                .font(theme::regular(14.0))
                .text_color(palette.text)
                .frame(egui::Frame::NONE)
                .margin(egui::Margin::ZERO)
                .show(ui)
        })
        .inner;
    let response = output.response.response;
    ui.ctx()
        .accesskit_node_builder(response.id, |node| node.set_label("Prompt"));
    theme::focus_outline(ui, response.id, field, radius);
    // A click in the field, or the step's first frame, types in it.
    if surface.clicked() || ui.memory(|memory| memory.focused().is_none()) {
        ui.memory_mut(|memory| memory.request_focus(id));
    }
    // Ctrl+Enter (Cmd+Enter on macOS) sends, as Enter adds a line.
    if response.has_focus()
        && ui.input(|input| input.modifiers.command && input.key_pressed(egui::Key::Enter))
        && let Some(index) = app.tool_pick
    {
        app.actions.push(Action::SendToTool(index));
    }
    #[cfg(any(test, feature = "demo"))]
    ui.ctx().data_mut(|data| {
        data.insert_temp(egui::Id::new(("forward-layout", "list")), field);
        data.insert_temp(egui::Id::new(("forward-layout", "prompt")), field);
    });
}

/// The tool step's footer: Back to the list, and Send.
pub(super) fn forward_step_footer(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let Some(index) = app.tool_pick else {
        return;
    };
    ui.horizontal(|ui| {
        if theme::pill_button(
            ui,
            &palette,
            &crate::i18n::gettext(app.locale, "Back"),
            false,
        )
        .clicked()
        {
            app.actions.push(Action::BackToForwardSelection);
        }
        if theme::pill_button(
            ui,
            &palette,
            &crate::i18n::gettext(app.locale, "Send"),
            true,
        )
        .clicked()
        {
            app.actions.push(Action::SendToTool(index));
        }
    });
}
