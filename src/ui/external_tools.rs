//! External tools: their Settings list and their rows in the Forward dialog.

use egui::{Sense, pos2, vec2};

use crate::app::App;
use crate::external_tools::{Tool, ToolKind};
use crate::model::Action;
use crate::theme::{self, Icon};

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

/// One row per usable tool whose name holds the search, above the Forward
/// dialog's chats. A click sends at once. The rows take at most half the
/// room left above `bottom`, scrolling past that, so the chats stay in view.
pub(super) fn forward_rows(app: &mut App, ui: &mut egui::Ui, needle: &str, bottom: f32) {
    let palette = app.palette;
    let tools: Vec<(usize, String)> = app
        .settings
        .external_tools
        .iter()
        .enumerate()
        .filter(|(_, tool)| tool.usable() && tool.name.to_lowercase().contains(needle))
        .map(|(index, tool)| (index, tool.name.trim().to_owned()))
        .collect();
    if tools.is_empty() {
        return;
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
                    ui.allocate_exact_size(vec2(ui.available_width(), 52.0), Sense::click());
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
                    let circle = egui::Rect::from_center_size(
                        pos2(rect.left() + 23.0, rect.center().y),
                        egui::Vec2::splat(38.0),
                    );
                    ui.painter().circle_filled(
                        circle.center(),
                        circle.width() / 2.0,
                        palette.accent,
                    );
                    theme::paint_icon(ui, Icon::ExternalLink, circle, 18.0, palette.on_accent);
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
                }
                if response
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    app.actions.push(Action::SendToTool(index));
                }
            }
        });
}
