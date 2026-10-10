//! The "Page" submenu of the right-click menu on a page: the page-level actions (copy, delete,
//! extract, insert, rotate, move…) one click away, aimed at the page that was clicked.

use crate::canvas::{DocView, ViewAction};
use eframe::egui;
use pdfcraft_engine::Edit;
use pdfcraft_render::DocInfo;

/// Where `page` goes when moved one step up (earlier), one step down (later), or to an end.
/// `None` when it is already there. `count` is the number of pages.
pub(crate) fn move_target(page: usize, count: usize, how: Move) -> Option<usize> {
    let to = match how {
        Move::First => 0,
        Move::Up => page.checked_sub(1)?,
        Move::Down => page.checked_add(1).filter(|p| *p < count)?,
        Move::Last => count.checked_sub(1)?,
    };
    (to != page).then_some(to)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Move {
    First,
    Up,
    Down,
    Last,
}

/// Make `page` the only selected and current page, so the app's page actions (copy, extract…)
/// aim at it.
fn aim(view: &mut DocView, page: usize) {
    view.selected = [page].into();
    view.current = page;
}

/// The submenu. `page` is the page under the pointer when the menu opened.
pub(crate) fn page_menu(ui: &mut egui::Ui, view: &mut DocView, info: &DocInfo, page: usize, can_modify: bool) {
    let count = info.pages.len();
    if page >= count {
        return;
    }
    ui.menu_button("Page", |ui| {
        if ui.button("Select all text on this page").clicked() {
            view.current = page;
            view.select_all();
            ui.close();
        }
        ui.separator();
        if ui.button("Copy").clicked() {
            aim(view, page);
            view.pending_action = Some(ViewAction::CopyPages { cut: false });
            ui.close();
        }
        if ui.add_enabled(can_modify && count > 1, egui::Button::new("Cut")).clicked() {
            aim(view, page);
            view.pending_action = Some(ViewAction::CopyPages { cut: true });
            ui.close();
        }
        ui.add_enabled_ui(can_modify, |ui| {
            ui.menu_button("Insert copied pages", |ui| {
                if ui.button("Before the page").clicked() {
                    view.pending_action = Some(ViewAction::PasteAt(page));
                    ui.close();
                }
                if ui.button("After the page").clicked() {
                    view.pending_action = Some(ViewAction::PasteAt(page + 1));
                    ui.close();
                }
            });
            if ui.button("Replace page with copied pages").clicked() {
                view.pending_action = Some(ViewAction::ReplaceWithCopied(page));
                ui.close();
            }
        });
        if ui.add_enabled(can_modify && count > 1, egui::Button::new("Delete")).clicked() {
            view.pending_edit = Some(Edit::DeletePages { pages: vec![page] });
            ui.close();
        }
        if ui.add_enabled(can_modify, egui::Button::new("Extract")).clicked() {
            aim(view, page);
            view.pending_action = Some(ViewAction::Extract);
            ui.close();
        }
        ui.separator();
        ui.add_enabled_ui(can_modify, |ui| {
            ui.menu_button("Insert empty page", |ui| {
                let (w, h) = info
                    .pages
                    .get(page)
                    .map_or((612.0, 792.0), |p| ((p.crop[2] - p.crop[0]).abs().max(1.0) as f64, (p.crop[3] - p.crop[1]).abs().max(1.0) as f64));
                if ui.button("Before the page").clicked() {
                    view.pending_edit = Some(Edit::InsertBlankPage { at: page, width: w, height: h });
                    ui.close();
                }
                if ui.button("After the page").clicked() {
                    view.pending_edit = Some(Edit::InsertBlankPage { at: page + 1, width: w, height: h });
                    ui.close();
                }
            });
            ui.menu_button("Insert file", |ui| {
                if ui.button("Before the page").clicked() {
                    view.pending_action = Some(ViewAction::InsertFromFileAt(page));
                    ui.close();
                }
                if ui.button("After the page").clicked() {
                    view.pending_action = Some(ViewAction::InsertFromFileAt(page + 1));
                    ui.close();
                }
            });
        });
        ui.separator();
        if ui.add_enabled(can_modify, egui::Button::new("Rotate left")).clicked() {
            view.pending_edit = Some(Edit::RotatePages { pages: vec![page], degrees: -90 });
            ui.close();
        }
        if ui.add_enabled(can_modify, egui::Button::new("Rotate right")).clicked() {
            view.pending_edit = Some(Edit::RotatePages { pages: vec![page], degrees: 90 });
            ui.close();
        }
        ui.separator();
        for (label, how) in [("Move up", Move::Up), ("Move down", Move::Down), ("Move to first", Move::First), ("Move to last", Move::Last)] {
            let to = move_target(page, count, how);
            if ui.add_enabled(can_modify && to.is_some(), egui::Button::new(label)).clicked()
                && let Some(to) = to
            {
                view.pending_edit = Some(Edit::MovePages { pages: vec![page], to });
                // Keep following the page that was moved.
                view.current = to;
                ui.close();
            }
        }
        ui.separator();
        if ui.button("Show page information").clicked() {
            view.pending_action = Some(ViewAction::PageInfo(page));
            ui.close();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_move_one_step_or_to_an_end() {
        assert_eq!(move_target(2, 5, Move::Up), Some(1));
        assert_eq!(move_target(2, 5, Move::Down), Some(3));
        assert_eq!(move_target(2, 5, Move::First), Some(0));
        assert_eq!(move_target(2, 5, Move::Last), Some(4));
    }

    #[test]
    fn pages_at_an_end_cannot_move_further() {
        assert_eq!(move_target(0, 5, Move::Up), None);
        assert_eq!(move_target(0, 5, Move::First), None);
        assert_eq!(move_target(4, 5, Move::Down), None);
        assert_eq!(move_target(4, 5, Move::Last), None);
        assert_eq!(move_target(0, 1, Move::Down), None);
        assert_eq!(move_target(0, 0, Move::Last), None);
    }
}
