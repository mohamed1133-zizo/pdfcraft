//! The "Page" submenu of the right-click menu on a page: the page-level actions (copy, delete,
//! extract, insert, rotate, move…) one click away, aimed at the page that was clicked.

use crate::canvas::{DocView, ViewAction};
use eframe::egui;
use pdfcraft_engine::Edit;
use pdfcraft_render::DocInfo;

use crate::PdfCraftApp;

/// How "Print this page" sets the page up in the Print dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrintAs {
    /// The Print dialog as it is, for this page.
    Dialog,
    /// At 100 %: the page's own size.
    ActualSize,
    /// As large as the paper allows.
    MaximumSize,
}

/// Resolution of "Copy page as image" and "Save page as image".
const PAGE_IMAGE_DPI: f64 = 150.0;

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
        if ui.button("Copy page as image").clicked() {
            view.pending_action = Some(ViewAction::CopyPageImage(page));
            ui.close();
        }
        if ui.button("Save page as image").clicked() {
            view.pending_action = Some(ViewAction::SavePageImage(page));
            ui.close();
        }
        ui.separator();
        if ui.button("Print this page").clicked() {
            view.pending_action = Some(ViewAction::PrintPage(page, PrintAs::Dialog));
            ui.close();
        }
        ui.menu_button("Print page at", |ui| {
            if ui.button("Actual size").clicked() {
                view.pending_action = Some(ViewAction::PrintPage(page, PrintAs::ActualSize));
                ui.close();
            }
            if ui.button("Maximum size").clicked() {
                view.pending_action = Some(ViewAction::PrintPage(page, PrintAs::MaximumSize));
                ui.close();
            }
        });
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

/// Decode a PNG to 8-bit RGBA: (width, height, pixels).
fn png_to_rgba(png: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("the image is too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let data = buf.get(..info.buffer_size()).ok_or("the image is incomplete")?;
    let rgba = match info.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => data.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::Grayscale => data.iter().flat_map(|g| [*g, *g, *g, 255]).collect(),
        png::ColorType::GrayscaleAlpha => data.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Indexed => return Err("unsupported image format".into()),
    };
    Ok((info.width as usize, info.height as usize, rgba))
}

impl PdfCraftApp {
    /// The page as a PNG at [`PAGE_IMAGE_DPI`].
    fn page_png(&mut self, page: usize) -> Result<(Vec<u8>, String), String> {
        if !self.commit_form_typing() {
            return Err("fix the field first".into());
        }
        let (_, id) = self.active_ids().ok_or("open a document first")?;
        let doc = self.session.get(id).ok_or("open a document first")?;
        let stem = doc.name.trim_end_matches(".pdf").trim_end_matches(".PDF").to_string();
        let png = pdfcraft_engine::export::Exporter::new(doc).png(page, PAGE_IMAGE_DPI)?;
        Ok((png, stem))
    }

    /// Page ▸ Copy page as image: the page's picture on the system clipboard.
    pub fn copy_whole_page_image(&mut self, page: usize) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let result = self.page_png(page).and_then(|(png, _)| {
                let (width, height, rgba) = png_to_rgba(&png)?;
                let data = arboard::ImageData { width, height, bytes: std::borrow::Cow::Owned(rgba) };
                arboard::Clipboard::new().and_then(|mut c| c.set_image(data)).map_err(|e| e.to_string())
            });
            match result {
                Ok(()) => self.notify(format!("Page {} copied as an image", page + 1)),
                Err(e) => self.notify_error(format!("Couldn't copy the page: {e}")),
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = page;
            self.notify("Copying a page as an image isn't available in the browser: use Save page as image");
        }
    }

    /// Page ▸ Save page as image: ask where, then write the PNG.
    pub fn save_whole_page_image(&mut self, page: usize) {
        let (png, stem) = match self.page_png(page) {
            Ok(v) => v,
            Err(e) => {
                self.notify_error(format!("Couldn't save the page: {e}"));
                return;
            }
        };
        let name = format!("{stem}_page_{}.png", page + 1);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let write = move |app: &mut Self, path: std::path::PathBuf| match crate::editing::write_atomically(&path.to_string_lossy(), &png) {
                Ok(()) => app.notify(format!("Saved to {}", path.display())),
                Err(e) => app.notify_error(format!("Couldn't write {}: {e}", path.display())),
            };
            match self.save_override.clone() {
                Some(p) => write(self, p.into()),
                None => {
                    let dialog = rfd::AsyncFileDialog::new().set_title("Save page as image").add_filter("PNG", &["png"]).set_file_name(name);
                    self.ask_one(crate::pickers::Ask::Save(dialog), None, write);
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        if let Err(e) = crate::editing::download(&name, &png) {
            self.notify_error(e);
        }
    }

    /// Page ▸ Print this page / Print page at…: the Print dialog for just this page.
    pub fn print_page(&mut self, page: usize, how: PrintAs) {
        use crate::print_ui::{Handling, Which};
        use pdfcraft_engine::print::SizeMode;
        self.open_print();
        let d = &mut self.print_draft;
        d.which = Which::Current;
        d.current_page = page;
        d.selected.clear();
        match how {
            PrintAs::Dialog => {}
            PrintAs::ActualSize => {
                d.handling = Handling::Size;
                d.size = SizeMode::Actual;
            }
            PrintAs::MaximumSize => {
                d.handling = Handling::Size;
                d.size = SizeMode::Fit;
            }
        }
    }
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
    fn a_page_png_decodes_to_rgba() {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, 2, 1);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[1, 2, 3, 4, 5, 6]).unwrap();
        }
        assert_eq!(png_to_rgba(&out).unwrap(), (2, 1, vec![1, 2, 3, 255, 4, 5, 6, 255]));
        assert!(png_to_rgba(b"not a png").is_err());
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
