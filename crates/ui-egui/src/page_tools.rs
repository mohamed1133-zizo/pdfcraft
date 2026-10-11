//! Page tools beyond the Page submenu: Resize pages to a paper size, Pages per sheet (N-up),
//! Overlay pages from a file, and Repair PDF.
//!
//! Resize and Pages per sheet lay the pages out with the print engine and open the result as a
//! new document (the open one is not changed). Overlay is an undoable edit. Repair writes the
//! document out again from scratch and opens that copy.

use std::sync::Arc;

use egui::{Align, Layout};
use pdfcraft_engine::print::{self, PAPERS};
use pdfcraft_engine::{Edit, EditError};

use crate::theme::{self, Tokens};
use crate::{Dialog, PdfCraftApp, widgets};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Resize,
    PerSheet,
    Overlay,
}

/// Which pages a tool works on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pages {
    All,
    /// The selected pages, or the current page when none are selected.
    Selected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// Scale each page up or down to fill the paper.
    Fill,
    /// Only shrink pages that are too big.
    Shrink,
    /// Keep the page's size, centred.
    Actual,
}

/// The overlay file: chosen before the dialog opens.
#[derive(Clone, Debug, PartialEq)]
pub struct OverlayFile {
    pub name: String,
    pub bytes: Arc<Vec<u8>>,
    pub pages: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub paper: usize,
    pub fit: Fit,
    pub per_sheet: usize,
    pub border: bool,
    pub pages: Pages,
    pub under: bool,
    pub overlay: Option<OverlayFile>,
}

impl Default for Draft {
    fn default() -> Self {
        // A4 is the sixth paper in the list.
        Self { paper: 4, fit: Fit::Fill, per_sheet: 2, border: false, pages: Pages::All, under: false, overlay: None }
    }
}

/// Pages per sheet the dialog offers (Acrobat's choices).
pub const PER_SHEET: [usize; 7] = [2, 4, 6, 9, 16, 25, 36];

impl Draft {
    /// The print-engine settings for Resize or Pages per sheet on `pages` (0-based).
    pub fn settings(&self, tool: Tool, pages: Vec<usize>) -> print::Settings {
        let layout = match tool {
            Tool::PerSheet => match print::Layout::multiple(self.per_sheet) {
                print::Layout::Multiple { cols, rows, order, auto_rotate, .. } => {
                    print::Layout::Multiple { cols, rows, order, border: self.border, auto_rotate }
                }
                other => other,
            },
            _ => print::Layout::Size(match self.fit {
                Fit::Fill => print::SizeMode::Fit,
                Fit::Shrink => print::SizeMode::Shrink,
                Fit::Actual => print::SizeMode::Actual,
            }),
        };
        print::Settings {
            pages,
            paper: PAPERS.get(self.paper).map_or(PAPERS[0].1, |p| p.1),
            orientation: print::Orientation::Auto,
            layout,
            // A copy for reading: what is on the page, with its markups.
            content: print::Content::DocumentAndMarkups,
        }
    }
}

fn stem(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// Draw the dialog body; returns (apply, cancel).
pub(crate) fn body(ui: &mut egui::Ui, app: &mut PdfCraftApp, t: &Tokens, tool: Tool) -> (bool, bool) {
    let Some((i, _)) = app.active_ids() else { return (false, true) };
    let selected = app.views[i].selected.len();
    let title = match tool {
        Tool::Resize => "Resize pages to a paper size",
        Tool::PerSheet => "Pages per sheet",
        Tool::Overlay => "Overlay pages from a file",
    };
    ui.label(egui::RichText::new(title).font(theme::semibold(18.0)));
    ui.add_space(8.0);
    let d = &mut app.page_tools;
    let mut ok = true;
    egui::Grid::new("page-tools").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        if tool != Tool::Overlay {
            ui.label("Paper");
            let current = PAPERS.get(d.paper).map_or("", |p| p.0);
            egui::ComboBox::from_id_salt("page-tools-paper").selected_text(current).show_ui(ui, |ui| {
                for (k, (name, _)) in PAPERS.iter().enumerate() {
                    ui.selectable_value(&mut d.paper, k, *name);
                }
            });
            ui.end_row();
        }
        match tool {
            Tool::Resize => {
                ui.label("Scale");
                ui.vertical(|ui| {
                    ui.radio_value(&mut d.fit, Fit::Fill, "Fit each page to the paper");
                    ui.radio_value(&mut d.fit, Fit::Shrink, "Shrink only pages that are too big");
                    ui.radio_value(&mut d.fit, Fit::Actual, "Keep the page size (centred)");
                });
                ui.end_row();
            }
            Tool::PerSheet => {
                ui.label("Pages per sheet");
                egui::ComboBox::from_id_salt("page-tools-per-sheet").selected_text(d.per_sheet.to_string()).show_ui(ui, |ui| {
                    for n in PER_SHEET {
                        ui.selectable_value(&mut d.per_sheet, n, n.to_string());
                    }
                });
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut d.border, "Draw a border around each page");
                ui.end_row();
            }
            Tool::Overlay => {
                ui.label(tl!("File"));
                match &d.overlay {
                    Some(f) => {
                        ui.label(crate::i18n::fmt("{name} ({pages} pages)", &[("name", &f.name), ("pages", &f.pages.to_string())]));
                    }
                    None => {
                        ok = false;
                        ui.label(egui::RichText::new(tl!("No file chosen")).color(t.text_muted));
                    }
                }
                ui.end_row();
                ui.label("Put it");
                ui.vertical(|ui| {
                    ui.radio_value(&mut d.under, false, "On top of the page");
                    ui.radio_value(&mut d.under, true, "Behind the page");
                });
                ui.end_row();
            }
        }
        ui.label(tl!("Pages"));
        ui.vertical(|ui| {
            ui.radio_value(&mut d.pages, Pages::All, tl!("All pages"));
            let label = if selected > 0 { tl!("Selected pages") } else { tl!("Current page") };
            ui.radio_value(&mut d.pages, Pages::Selected, label);
        });
        ui.end_row();
    });
    ui.add_space(6.0);
    let note = match tool {
        Tool::Resize => "The result opens as a new document; this one is not changed.",
        Tool::PerSheet => "The sheets open as a new document; this one is not changed.",
        Tool::Overlay => "Page 1 of the file goes on the first page, page 2 on the next, and so on; a shorter file starts over.",
    };
    ui.label(egui::RichText::new(note).color(t.text_muted));
    ui.add_space(12.0);
    let (mut apply, mut cancel) = (false, false);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if ui.add_enabled_ui(ok, |ui| widgets::pill_button(ui, tl!("OK"), true)).inner.clicked() {
            apply = true;
        }
        if widgets::pill_button(ui, tl!("Cancel"), false).clicked() {
            cancel = true;
        }
    });
    (apply, cancel)
}

impl PdfCraftApp {
    fn tool_pages(&self, i: usize, count: usize) -> Vec<usize> {
        match self.page_tools.pages {
            Pages::All => (0..count).collect(),
            Pages::Selected => self.views[i].target_pages().into_iter().filter(|p| *p < count).collect(),
        }
    }

    /// Open the Resize or Pages per sheet dialog.
    pub(crate) fn open_page_tool(&mut self, tool: Tool) {
        if self.active_ids().is_none() {
            self.notify_tr("Open a document first");
            return;
        }
        self.dialog = Some(Dialog::PageTools(tool));
    }

    /// Overlay pages from a file: choose the file, then the dialog.
    pub(crate) fn overlay_dialog(&mut self) {
        if self.active_ids().is_none() {
            self.notify_tr("Open a document first");
            return;
        }
        self.pick_overlay_file();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn pick_overlay_file(&mut self) {
        let dialog = rfd::AsyncFileDialog::new().add_filter("PDF", &["pdf"]);
        self.ask_one(crate::pickers::Ask::File(dialog), None, |app, path| {
            let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            match std::fs::read(&path) {
                Ok(bytes) => app.set_overlay_file(name, bytes),
                Err(e) => app.notify_fmt("Could not open {name}: {e}", &[("name", &name), ("e", &e.to_string())]),
            }
        });
    }

    #[cfg(target_arch = "wasm32")]
    fn pick_overlay_file(&mut self) {
        self.notify_tr("Overlay needs a file from disk, which a browser can't offer here");
    }

    /// The overlay file is in hand: check it, then show the dialog.
    pub(crate) fn set_overlay_file(&mut self, name: String, bytes: Vec<u8>) {
        let bytes = Arc::new(bytes);
        match self.session.page_count_of(&name, &bytes) {
            Ok(pages) if pages > 0 => {
                self.page_tools.overlay = Some(OverlayFile { name, bytes, pages });
                self.dialog = Some(Dialog::PageTools(Tool::Overlay));
            }
            Ok(_) => self.notify_fmt("{name} has no pages", &[("name", &name)]),
            Err(e) => self.notify_error(e),
        }
    }

    /// OK in the dialog.
    pub(crate) fn run_page_tool(&mut self, tool: Tool) {
        let Some((i, id)) = self.active_ids() else { return };
        let Some(doc) = self.session.get(id) else { return };
        let (count, name) = (doc.info.pages.len(), doc.name.clone());
        let pages = self.tool_pages(i, count);
        if pages.is_empty() {
            return;
        }
        if tool == Tool::Overlay {
            let Some(f) = self.page_tools.overlay.clone() else { return };
            let under = self.page_tools.under;
            self.apply_edit(Edit::OverlayPages { pages, name: f.name, bytes: f.bytes, under });
            return;
        }
        let settings = self.page_tools.settings(tool, pages);
        match self.session.print_pdf(id, &settings) {
            Ok(bytes) => {
                let suffix = if tool == Tool::Resize { "resized" } else { "pages per sheet" };
                self.open_copy(&format!("{} ({suffix}).pdf", stem(&name)), bytes);
            }
            Err(e) => self.notify_error(e),
        }
    }

    /// Repair PDF: write the document out again and open the copy.
    pub(crate) fn repair_pdf(&mut self) {
        let Some((_, id)) = self.active_ids() else {
            self.notify_tr("Open a document first");
            return;
        };
        let Some(name) = self.session.get(id).map(|d| d.name.clone()) else { return };
        match self.session.repair(id) {
            Ok((bytes, log)) => {
                self.open_copy(&format!("{} (repaired).pdf", stem(&name)), bytes.as_ref().clone());
                if log.is_empty() {
                    self.notify_tr("Nothing needed repairing; this is a clean rewritten copy");
                } else {
                    self.notify_fmt("Repaired {n} problems; the repaired copy is open", &[("n", &log.len().to_string())]);
                }
            }
            Err(e) => self.notify_error(e),
        }
    }

    fn open_copy(&mut self, name: &str, bytes: Vec<u8>) {
        if let Err(e) = self.open_bytes(name, None, bytes) {
            self.notify_error(EditError::Write(e));
        }
    }
}
