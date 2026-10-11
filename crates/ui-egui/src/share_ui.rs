//! Share as link: copy the open document to the user's own cloud and make a link to it.
//!
//! Google Drive, OneDrive, Dropbox and Nextcloud are connected once in Preferences ▸ Cloud
//! sharing. The link lasts as long as the user chooses; when it runs out PdfCraft deletes the
//! file from the cloud, stops the link and keeps the file, or does nothing, as the user chose
//! there. The network work is done by the application ([`PdfCraftApp::cloud_source`]); this
//! module holds the settings, the dialog and the bookkeeping, and has no network code.
//! Everything starts from a button the user presses, except removing a link that has run out.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::theme::{self, Tokens};
use crate::{Dialog, PdfCraftApp, widgets};

const MAX_FIELD: usize = 2048;
const MAX_SHARED: usize = 200;
/// How long to wait before trying again after a removal failed.
const RETRY_SECS: u64 = 600;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudKind {
    GoogleDrive,
    OneDrive,
    Dropbox,
    Nextcloud,
}

impl CloudKind {
    pub const ALL: [CloudKind; 4] = [CloudKind::GoogleDrive, CloudKind::OneDrive, CloudKind::Dropbox, CloudKind::Nextcloud];

    pub fn id(self) -> &'static str {
        match self {
            CloudKind::GoogleDrive => "google-drive",
            CloudKind::OneDrive => "onedrive",
            CloudKind::Dropbox => "dropbox",
            CloudKind::Nextcloud => "nextcloud",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CloudKind::GoogleDrive => "Google Drive",
            CloudKind::OneDrive => "OneDrive",
            CloudKind::Dropbox => "Dropbox",
            CloudKind::Nextcloud => "Nextcloud",
        }
    }

    fn index(self) -> usize {
        CloudKind::ALL.iter().position(|k| *k == self).unwrap_or(0)
    }

    fn from_id(id: &str) -> Option<CloudKind> {
        CloudKind::ALL.into_iter().find(|k| k.id() == id)
    }

    /// Where to get the client ID, as a sentence.
    fn help(self) -> &'static str {
        match self {
            CloudKind::GoogleDrive => {
                "Google Cloud console ▸ APIs & Services ▸ enable Google Drive API ▸ Credentials ▸ OAuth client ID of type Desktop app. Paste its client ID and secret."
            }
            CloudKind::OneDrive => {
                "Azure portal ▸ App registrations ▸ New registration (personal accounts allowed) ▸ add the platform Mobile and desktop with redirect http://localhost:53682/ ▸ paste the Application (client) ID."
            }
            CloudKind::Dropbox => {
                "dropbox.com/developers/apps ▸ Create app (scoped access, App folder or full) ▸ Permissions: files.content.write and sharing.write ▸ add redirect http://localhost:53682/ ▸ paste the App key."
            }
            CloudKind::Nextcloud => "Your server's address, your user name, and an app password (Settings ▸ Security ▸ Devices & sessions).",
        }
    }
}

/// One cloud account: the app registration the user made, the sign-in, and for Nextcloud the
/// address and app password.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloudAccount {
    pub client_id: String,
    pub client_secret: String,
    pub server: String,
    pub user: String,
    pub password: String,
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: u64,
}

impl CloudAccount {
    pub fn connected(&self, kind: CloudKind) -> bool {
        match kind {
            CloudKind::Nextcloud => !self.server.is_empty() && !self.user.is_empty() && !self.password.is_empty(),
            _ => !self.access_token.is_empty() || !self.refresh_token.is_empty(),
        }
    }
}

/// A file PdfCraft put in the cloud, and its link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedFile {
    pub kind: CloudKind,
    pub name: String,
    pub link: String,
    pub remote_id: String,
    pub link_id: String,
    /// Unix seconds; `None` when the link doesn't end.
    pub expires_at: Option<u64>,
}

#[derive(Clone, Debug)]
pub enum CloudAction {
    Connect,
    Share { name: String, bytes: Arc<Vec<u8>>, days: Option<u32> },
    Remove { item: SharedFile, delete_file: bool },
}

#[derive(Clone, Debug)]
pub struct CloudJob {
    pub kind: CloudKind,
    pub account: CloudAccount,
    pub action: CloudAction,
}

#[derive(Clone, Debug)]
pub struct CloudOutput {
    /// The account, with the tokens the job refreshed or obtained.
    pub account: CloudAccount,
    pub shared: Option<SharedFile>,
}

pub type CloudSource = Arc<dyn Fn(&CloudJob) -> Result<CloudOutput, String> + Send + Sync>;

/// What happens to a file when its link has run out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterExpiry {
    /// Delete the file from the cloud.
    Delete,
    /// Stop the link and keep the file there.
    StopLink,
    /// Do nothing; PdfCraft forgets the link.
    Leave,
}

impl AfterExpiry {
    fn id(self) -> &'static str {
        match self {
            AfterExpiry::Delete => "delete",
            AfterExpiry::StopLink => "stop-link",
            AfterExpiry::Leave => "leave",
        }
    }
}

/// Where the finished link goes straight away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Via {
    Email,
    WhatsApp,
    Telegram,
}

impl Via {
    fn label(self) -> &'static str {
        match self {
            Via::Email => "your email program",
            Via::WhatsApp => "WhatsApp",
            Via::Telegram => "Telegram",
        }
    }
}

fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The address that opens `via` with a message carrying `link` for the file `name`.
pub(crate) fn via_url(via: Via, name: &str, link: &str) -> String {
    match via {
        Via::Email => format!("mailto:?subject={}&body={}", pct(name), pct(&format!("{name}\n{link}"))),
        Via::WhatsApp => format!("https://wa.me/?text={}", pct(&format!("{name} {link}"))),
        Via::Telegram => format!("https://t.me/share/url?url={}&text={}", pct(link), pct(name)),
    }
}

enum Purpose {
    Connect(CloudKind),
    Share,
    /// The link of the record being removed.
    Remove(String),
}

pub(crate) struct Cloud {
    pub(crate) accounts: [CloudAccount; 4],
    pub(crate) after: AfterExpiry,
    /// Days a new link lasts; 0 for no end.
    pub(crate) default_days: u32,
    pub(crate) shared: Vec<SharedFile>,
    busy: Option<(Purpose, Receiver<Result<CloudOutput, String>>)>,
    retry_at: u64,
    /// The provider chosen in the dialog.
    pub(crate) choice: usize,
    pub(crate) days: u32,
    pub(crate) message: Option<String>,
    /// Open the finished link in this program.
    pub(crate) via: Option<Via>,
}

impl Default for Cloud {
    fn default() -> Self {
        Cloud {
            accounts: Default::default(),
            after: AfterExpiry::Delete,
            default_days: 7,
            shared: Vec::new(),
            busy: None,
            retry_at: 0,
            choice: 0,
            days: 7,
            message: None,
            via: None,
        }
    }
}

fn cut(v: &serde_json::Value, max: usize) -> String {
    v.as_str().unwrap_or_default().chars().take(max).collect()
}

impl Cloud {
    pub(crate) fn persist(&self) -> serde_json::Value {
        let accounts: serde_json::Map<String, serde_json::Value> = CloudKind::ALL
            .iter()
            .map(|k| {
                let a = &self.accounts[k.index()];
                (
                    k.id().to_string(),
                    serde_json::json!({
                        "client_id": a.client_id, "client_secret": a.client_secret, "server": a.server, "user": a.user,
                        "password": a.password, "access_token": a.access_token, "refresh_token": a.refresh_token, "expires_at": a.expires_at,
                    }),
                )
            })
            .collect();
        let shared: Vec<_> = self
            .shared
            .iter()
            .map(|s| {
                serde_json::json!({
                    "kind": s.kind.id(), "name": s.name, "link": s.link, "remote_id": s.remote_id, "link_id": s.link_id, "expires_at": s.expires_at,
                })
            })
            .collect();
        serde_json::json!({ "accounts": accounts, "after": self.after.id(), "default_days": self.default_days, "shared": shared })
    }

    /// Settings are untrusted: every value is cut to a sane length.
    pub(crate) fn restore(&mut self, v: &serde_json::Value) {
        for k in CloudKind::ALL {
            let e = &v["accounts"][k.id()];
            if e.is_object() {
                self.accounts[k.index()] = CloudAccount {
                    client_id: cut(&e["client_id"], MAX_FIELD),
                    client_secret: cut(&e["client_secret"], MAX_FIELD),
                    server: cut(&e["server"], MAX_FIELD),
                    user: cut(&e["user"], MAX_FIELD),
                    password: cut(&e["password"], MAX_FIELD),
                    access_token: cut(&e["access_token"], 4 * MAX_FIELD),
                    refresh_token: cut(&e["refresh_token"], 4 * MAX_FIELD),
                    expires_at: e["expires_at"].as_u64().unwrap_or(0),
                };
            }
        }
        self.after = match v["after"].as_str() {
            Some("stop-link") => AfterExpiry::StopLink,
            Some("leave") => AfterExpiry::Leave,
            _ => AfterExpiry::Delete,
        };
        if let Some(d) = v["default_days"].as_u64() {
            self.default_days = d.min(3650) as u32;
            self.days = self.default_days;
        }
        self.shared = v["shared"]
            .as_array()
            .map(|list| {
                list.iter()
                    .take(MAX_SHARED)
                    .filter_map(|s| {
                        Some(SharedFile {
                            kind: CloudKind::from_id(s["kind"].as_str()?)?,
                            name: cut(&s["name"], 200),
                            link: cut(&s["link"], MAX_FIELD),
                            remote_id: cut(&s["remote_id"], MAX_FIELD),
                            link_id: cut(&s["link_id"], MAX_FIELD),
                            expires_at: s["expires_at"].as_u64(),
                        })
                    })
                    .filter(|s| !s.link.is_empty())
                    .collect()
            })
            .unwrap_or_default();
    }

    /// The first link that has run out by `now`.
    pub(crate) fn due(&self, now: u64) -> Option<&SharedFile> {
        self.shared.iter().find(|s| s.expires_at.is_some_and(|e| e <= now))
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// "in 3 days", "in 5 hours", "ended".
pub(crate) fn remaining(expires_at: Option<u64>, now: u64) -> String {
    let Some(e) = expires_at else { return "no end".into() };
    let left = e.saturating_sub(now);
    match left {
        0 => "ended".into(),
        1..=3599 => format!("in {} min", left.div_ceil(60)),
        3600..=86_399 => format!("in {} h", left / 3600),
        _ => format!("in {} days", left / 86_400),
    }
}

impl PdfCraftApp {
    fn start_cloud(&mut self, ctx: &egui::Context, purpose: Purpose, job: CloudJob) {
        let Some(source) = self.cloud_source.clone() else {
            self.notify("Sharing isn't available in this build.");
            return;
        };
        if self.cloud.busy.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source(&job)))
                .unwrap_or_else(|_| Err("the request stopped unexpectedly".into()));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.cloud.message = None;
        self.cloud.busy = Some((purpose, rx));
    }

    /// Each frame: pick up a finished job, and remove links that have run out.
    pub(crate) fn poll_cloud(&mut self, ctx: &egui::Context) {
        if self.cloud_source.is_none() {
            return;
        }
        if let Some((_, rx)) = &self.cloud.busy {
            let result = match rx.try_recv() {
                Ok(r) => r,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => Err("the request stopped unexpectedly".into()),
            };
            let Some((purpose, _)) = self.cloud.busy.take() else { return };
            self.finish_cloud(purpose, result);
        }
        let now = unix_now();
        if !self.cloud.shared.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_secs(60));
        }
        if now < self.cloud.retry_at {
            return;
        }
        let Some(item) = self.cloud.due(now).cloned() else { return };
        if self.cloud.after == AfterExpiry::Leave {
            self.cloud.shared.retain(|s| s.link != item.link);
            return;
        }
        let account = self.cloud.accounts[item.kind.index()].clone();
        let delete_file = self.cloud.after == AfterExpiry::Delete;
        let link = item.link.clone();
        self.start_cloud(ctx, Purpose::Remove(link), CloudJob { kind: item.kind, account, action: CloudAction::Remove { item, delete_file } });
    }

    fn finish_cloud(&mut self, purpose: Purpose, result: Result<CloudOutput, String>) {
        match (purpose, result) {
            (Purpose::Connect(kind), Ok(out)) => {
                self.cloud.accounts[kind.index()] = out.account;
                self.notify(format!("Connected to {}", kind.label()));
            }
            (Purpose::Share, Ok(out)) => {
                if let Some(mut s) = out.shared {
                    self.cloud.accounts[s.kind.index()] = out.account;
                    // The end date counts from now, as the user saw it, not from the server's clock.
                    if s.expires_at.is_some() && self.cloud.days > 0 {
                        s.expires_at = Some(unix_now() + u64::from(self.cloud.days) * 86_400);
                    }
                    self.cloud.shared.insert(0, s.clone());
                    self.cloud.shared.truncate(MAX_SHARED);
                    self.copy_link(&s.link);
                    if let Some(via) = self.cloud.via.take() {
                        let url = via_url(via, &s.name, &s.link);
                        self.open_url(&url);
                    }
                }
            }
            (Purpose::Remove(link), Ok(out)) => {
                self.cloud.accounts.iter_mut().zip(CloudKind::ALL).for_each(|(a, k)| {
                    if self.cloud.shared.iter().any(|s| s.link == link && s.kind == k) {
                        *a = out.account.clone();
                    }
                });
                self.cloud.shared.retain(|s| s.link != link);
                self.cloud.message = Some("The link was removed.".into());
            }
            (Purpose::Remove(_), Err(e)) => {
                self.cloud.retry_at = unix_now() + RETRY_SECS;
                self.cloud.message = Some(format!("Couldn't remove a link yet: {e}"));
            }
            (_, Err(e)) => self.cloud.message = Some(e),
        }
    }

    fn copy_link(&mut self, link: &str) {
        if let Some(ctx) = self.ctx.clone() {
            ctx.copy_text(link.to_string());
        }
        self.cloud.message = Some("The link is ready, and copied to the clipboard.".into());
    }

    /// Share ▸ Send by email / WhatsApp / Telegram: make the link, then open it there.
    pub(crate) fn open_share_via(&mut self, via: Via) {
        self.open_share();
        if self.dialog == Some(Dialog::ShareLink) {
            self.cloud.via = Some(via);
        }
    }

    /// File ▸ Share as link…
    pub fn open_share(&mut self) {
        self.cloud.via = None;
        if self.cloud_source.is_none() {
            self.notify("Sharing isn't available in this build.");
            return;
        }
        if self.active_ids().is_none() {
            self.notify("Open a document first");
            return;
        }
        // Start on a connected provider.
        if let Some(k) = CloudKind::ALL.into_iter().find(|k| self.cloud.accounts[k.index()].connected(*k)) {
            self.cloud.choice = k.index();
        }
        self.cloud.days = self.cloud.default_days;
        self.cloud.message = None;
        self.dialog = Some(Dialog::ShareLink);
    }

    fn share_now(&mut self, ctx: &egui::Context) {
        let Some((_, id)) = self.active_ids() else { return };
        let Some(name) = self.session.get(id).map(|d| d.name.clone()) else { return };
        let bytes = match self.session.save_bytes(id) {
            Ok(b) => b,
            Err(e) => {
                self.cloud.message = Some(e.to_string());
                return;
            }
        };
        let kind = CloudKind::ALL[self.cloud.choice.min(3)];
        let days = (self.cloud.days > 0).then_some(self.cloud.days);
        let account = self.cloud.accounts[kind.index()].clone();
        self.start_cloud(ctx, Purpose::Share, CloudJob { kind, account, action: CloudAction::Share { name, bytes, days } });
    }
}

/// The dialog body; returns `true` to close.
pub(crate) fn dialog_body(ui: &mut egui::Ui, app: &mut PdfCraftApp, t: &Tokens) -> bool {
    let ctx = ui.ctx().clone();
    ui.set_width(540.0);
    ui.label(egui::RichText::new("Share as link").font(theme::semibold(18.0)));
    ui.add_space(8.0);
    let busy = app.cloud.busy.is_some();
    let connected: Vec<CloudKind> = CloudKind::ALL.into_iter().filter(|k| app.cloud.accounts[k.index()].connected(*k)).collect();
    let mut close = false;
    let mut open_prefs = false;
    let mut share = false;
    if connected.is_empty() {
        ui.label("No cloud is connected yet. Connect Google Drive, OneDrive, Dropbox or Nextcloud once in Preferences.");
        ui.add_space(8.0);
        if widgets::pill_button(ui, "Open Preferences", true).clicked() {
            open_prefs = true;
        }
    } else {
        if !connected.iter().any(|k| k.index() == app.cloud.choice) {
            app.cloud.choice = connected[0].index();
        }
        egui::Grid::new("share-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Cloud");
            let current = CloudKind::ALL[app.cloud.choice.min(3)].label();
            ui.add_enabled_ui(!busy, |ui| {
                egui::ComboBox::from_id_salt("share-cloud").selected_text(current).show_ui(ui, |ui| {
                    for k in &connected {
                        ui.selectable_value(&mut app.cloud.choice, k.index(), k.label());
                    }
                });
            });
            ui.end_row();
            ui.label("The link works for");
            ui.add_enabled_ui(!busy, |ui| {
                egui::ComboBox::from_id_salt("share-days")
                    .selected_text(match app.cloud.days {
                        0 => "No end".to_string(),
                        1 => "1 day".to_string(),
                        d => format!("{d} days"),
                    })
                    .show_ui(ui, |ui| {
                        for (d, label) in [(1, "1 day"), (3, "3 days"), (7, "7 days"), (30, "30 days"), (90, "90 days"), (0, "No end")] {
                            ui.selectable_value(&mut app.cloud.days, d, label);
                        }
                    });
            });
            ui.end_row();
        });
        ui.add_space(4.0);
        let after = match app.cloud.after {
            AfterExpiry::Delete => "then PdfCraft deletes the file from the cloud",
            AfterExpiry::StopLink => "then PdfCraft stops the link and keeps the file in the cloud",
            AfterExpiry::Leave => "then PdfCraft leaves the file and the link as they are",
        };
        let end = if app.cloud.days == 0 {
            "The link has no end.".to_string()
        } else {
            format!(
                "When the time is up, {after} (change this in Preferences). PdfCraft must be open to do it; if it was closed, it does it when it next starts."
            )
        };
        ui.label(egui::RichText::new(end).small().color(t.text_muted));
        ui.label(
            egui::RichText::new("A copy of the document as it is now (with unsaved changes) is uploaded to your account.")
                .small()
                .color(t.text_muted),
        );
        if let Some(via) = app.cloud.via {
            ui.label(egui::RichText::new(format!("When the link is ready it opens in {}.", via.label())).small().color(t.text_muted));
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let go = if app.cloud.via.is_some() { "Upload and send" } else { "Upload and make link" };
            if ui.add_enabled_ui(!busy, |ui| widgets::pill_button(ui, go, true)).inner.clicked() {
                share = true;
            }
            if busy {
                ui.spinner();
                ui.label("Working…");
            }
        });
    }
    if let Some(m) = &app.cloud.message {
        ui.add_space(6.0);
        ui.label(egui::RichText::new(m.as_str()).color(t.text_muted));
    }
    if !app.cloud.shared.is_empty() {
        ui.add_space(10.0);
        ui.label(egui::RichText::new("Links PdfCraft made").font(theme::semibold(13.0)));
        let now = unix_now();
        let mut stop: Option<(SharedFile, bool)> = None;
        egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
            for s in &app.cloud.shared {
                ui.horizontal(|ui| {
                    ui.label(format!("{} · {} · {}", s.kind.label(), s.name, remaining(s.expires_at, now)));
                    if ui.small_button("Copy link").clicked() {
                        ui.ctx().copy_text(s.link.clone());
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::new("Stop sharing").small())
                        .on_hover_text("Stops the link and keeps the file in your cloud")
                        .clicked()
                    {
                        stop = Some((s.clone(), false));
                    }
                    if ui.add_enabled(!busy, egui::Button::new("Delete file").small()).on_hover_text("Deletes the file from your cloud").clicked() {
                        stop = Some((s.clone(), true));
                    }
                });
            }
        });
        if let Some((item, delete_file)) = stop {
            let account = app.cloud.accounts[item.kind.index()].clone();
            let link = item.link.clone();
            app.start_cloud(&ctx, Purpose::Remove(link), CloudJob { kind: item.kind, account, action: CloudAction::Remove { item, delete_file } });
        }
    }
    ui.add_space(10.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if widgets::pill_button(ui, "Close", false).clicked() {
            close = true;
        }
    });
    if share {
        app.share_now(&ctx);
    }
    if open_prefs {
        app.dialog = Some(Dialog::Preferences);
    }
    close
}

/// Preferences ▸ Cloud sharing.
pub(crate) fn settings_ui(ui: &mut egui::Ui, app: &mut PdfCraftApp, t: &Tokens) {
    ui.label(egui::RichText::new("Cloud sharing").font(theme::semibold(13.0)));
    let ctx = ui.ctx().clone();
    let busy = app.cloud.busy.is_some();
    let mut start: Option<CloudKind> = None;
    let mut disconnect: Option<CloudKind> = None;
    for kind in CloudKind::ALL {
        let a = &mut app.cloud.accounts[kind.index()];
        let connected = a.connected(kind);
        egui::CollapsingHeader::new(format!("{}{}", kind.label(), if connected { " — connected" } else { "" })).id_salt(kind.id()).show(ui, |ui| {
            let field = |ui: &mut egui::Ui, label: &str, value: &mut String, password: bool| {
                ui.horizontal(|ui| {
                    ui.label(label);
                    ui.add(egui::TextEdit::singleline(value).password(password).desired_width(320.0).char_limit(MAX_FIELD));
                });
            };
            if kind == CloudKind::Nextcloud {
                field(ui, "Server address", &mut a.server, false);
                field(ui, "User name", &mut a.user, false);
                field(ui, "App password", &mut a.password, true);
            } else {
                field(ui, "Client ID", &mut a.client_id, false);
                if kind == CloudKind::GoogleDrive {
                    field(ui, "Client secret", &mut a.client_secret, true);
                }
            }
            let needs_id = kind != CloudKind::Nextcloud && a.client_id.trim().is_empty();
            if needs_id {
                ui.label(
                    egui::RichText::new("Paste the Client ID first (steps below). Without it the button has nothing to sign in with.")
                        .color(t.text_muted),
                );
            }
            ui.horizontal(|ui| {
                let label = match (connected, kind) {
                    (true, _) => "Sign in again",
                    (false, CloudKind::Nextcloud) => "Connect",
                    _ => "Connect (opens your browser)",
                };
                if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                    start = Some(kind);
                }
                if connected && ui.add_enabled(!busy, egui::Button::new("Disconnect")).clicked() {
                    disconnect = Some(kind);
                }
                if busy {
                    ui.spinner();
                }
            });
            ui.label(egui::RichText::new(kind.help()).small().color(t.text_muted));
        });
    }
    ui.horizontal(|ui| {
        ui.label("A new link lasts");
        egui::ComboBox::from_id_salt("pref-days")
            .selected_text(match app.cloud.default_days {
                0 => "No end".to_string(),
                d => format!("{d} days"),
            })
            .show_ui(ui, |ui| {
                for (d, label) in [(1, "1 day"), (3, "3 days"), (7, "7 days"), (30, "30 days"), (90, "90 days"), (0, "No end")] {
                    ui.selectable_value(&mut app.cloud.default_days, d, label);
                }
            });
    });
    ui.label("When a link has run out:");
    ui.radio_value(&mut app.cloud.after, AfterExpiry::Delete, "Delete the file from the cloud");
    ui.radio_value(&mut app.cloud.after, AfterExpiry::StopLink, "Stop the link and keep the file in the cloud");
    ui.radio_value(&mut app.cloud.after, AfterExpiry::Leave, "Do nothing");
    ui.label(
        egui::RichText::new("Sign-ins are kept on this computer, unencrypted, in PdfCraft's settings. Disconnect to forget one. PdfCraft only contacts a cloud when you press a button here or in Share as link, and to remove a link that has run out.")
            .small()
            .color(t.text_muted),
    );
    if let Some(m) = &app.cloud.message {
        ui.label(egui::RichText::new(m.as_str()).color(t.text_muted));
    }
    if let Some(kind) = disconnect {
        app.cloud.accounts[kind.index()] = CloudAccount::default();
    }
    if let Some(kind) = start {
        let account = app.cloud.accounts[kind.index()].clone();
        app.start_cloud(&ctx, Purpose::Connect(kind), CloudJob { kind, account, action: CloudAction::Connect });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_hostile_values_are_cut() {
        let mut c = Cloud::default();
        c.accounts[0].client_id = "cid".into();
        c.accounts[0].refresh_token = "r".into();
        c.accounts[3].server = "https://cloud.test".into();
        c.after = AfterExpiry::StopLink;
        c.default_days = 30;
        c.shared.push(SharedFile {
            kind: CloudKind::Dropbox,
            name: "a.pdf".into(),
            link: "https://x.test/s".into(),
            remote_id: "/a".into(),
            link_id: "l".into(),
            expires_at: Some(5),
        });
        let mut back = Cloud::default();
        back.restore(&c.persist());
        assert_eq!(
            (back.accounts[0].client_id.as_str(), back.accounts[0].refresh_token.as_str(), back.accounts[3].server.as_str()),
            ("cid", "r", "https://cloud.test")
        );
        assert_eq!((back.after, back.default_days, back.shared.len()), (AfterExpiry::StopLink, 30, 1));
        assert_eq!(back.shared[0].expires_at, Some(5));

        let mut hostile = Cloud::default();
        hostile.restore(&serde_json::json!({
            "accounts": { "dropbox": { "client_id": "x".repeat(100_000) }, "nope": 3 },
            "after": 7, "default_days": 999999999,
            "shared": [ {"kind": "ftp", "link": "l"}, {"kind": "dropbox", "link": ""}, 5, {"kind": "onedrive", "link": "https://ok"} ],
        }));
        assert_eq!(hostile.accounts[2].client_id.len(), MAX_FIELD);
        assert_eq!((hostile.after, hostile.default_days, hostile.shared.len()), (AfterExpiry::Delete, 3650, 1));
    }

    #[test]
    fn links_come_due_when_their_time_is_up() {
        let mut c = Cloud::default();
        let item = |link: &str, e: Option<u64>| SharedFile {
            kind: CloudKind::Dropbox,
            name: "n".into(),
            link: link.into(),
            remote_id: String::new(),
            link_id: String::new(),
            expires_at: e,
        };
        c.shared = vec![item("a", None), item("b", Some(200)), item("c", Some(100))];
        assert_eq!(c.due(99).map(|s| s.link.as_str()), None);
        assert_eq!(c.due(150).map(|s| s.link.as_str()), Some("c"));
        assert_eq!(c.due(300).map(|s| s.link.as_str()), Some("b"));
    }

    #[test]
    fn time_left_reads_naturally() {
        assert_eq!(remaining(None, 0), "no end");
        assert_eq!(remaining(Some(10), 10), "ended");
        assert_eq!(remaining(Some(100), 10), "in 2 min");
        assert_eq!(remaining(Some(7300), 0), "in 2 h");
        assert_eq!(remaining(Some(3 * 86_400 + 5), 0), "in 3 days");
    }

    /// A fake cloud: shares succeed with a fixed link, and removals are recorded.
    fn fake_cloud(removed: Arc<std::sync::Mutex<Vec<(String, bool)>>>) -> CloudSource {
        Arc::new(move |job: &CloudJob| match &job.action {
            CloudAction::Share { name, days, .. } => Ok(CloudOutput {
                account: CloudAccount { access_token: "newer".into(), ..job.account.clone() },
                shared: Some(SharedFile {
                    kind: job.kind,
                    name: name.clone(),
                    link: "https://cloud.test/s/1".into(),
                    remote_id: "R1".into(),
                    link_id: "L1".into(),
                    expires_at: days.map(|d| 1_000 + u64::from(d)),
                }),
            }),
            CloudAction::Remove { item, delete_file } => {
                removed.lock().unwrap().push((item.remote_id.clone(), *delete_file));
                Ok(CloudOutput { account: job.account.clone(), shared: None })
            }
            CloudAction::Connect => Ok(CloudOutput { account: CloudAccount { access_token: "t".into(), ..job.account.clone() }, shared: None }),
        })
    }

    fn wait(app: &mut PdfCraftApp, ctx: &egui::Context) {
        for _ in 0..200 {
            app.poll_cloud(ctx);
            if app.cloud.busy.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the cloud job never finished");
    }

    #[test]
    fn a_shared_file_is_linked_then_removed_when_its_time_is_up() {
        let ctx = egui::Context::default();
        let removed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut app = PdfCraftApp::new();
        app.open_bytes("form.pdf", None, include_bytes!("../tests/data/form.pdf").to_vec()).unwrap();
        app.cloud_source = Some(fake_cloud(removed.clone()));
        // Connect Dropbox, then share a copy for 7 days.
        let dropbox = CloudKind::Dropbox.index();
        app.cloud.accounts[dropbox].client_id = "cid".into();
        app.start_cloud(
            &ctx,
            Purpose::Connect(CloudKind::Dropbox),
            CloudJob { kind: CloudKind::Dropbox, account: app.cloud.accounts[dropbox].clone(), action: CloudAction::Connect },
        );
        wait(&mut app, &ctx);
        assert!(app.cloud.accounts[dropbox].connected(CloudKind::Dropbox));
        app.open_share();
        assert_eq!(app.dialog, Some(Dialog::ShareLink));
        assert_eq!(app.cloud.choice, dropbox, "starts on a connected cloud");
        app.cloud.days = 7;
        app.share_now(&ctx);
        wait(&mut app, &ctx);
        assert_eq!(app.cloud.shared.len(), 1);
        assert_eq!(app.cloud.shared[0].link, "https://cloud.test/s/1");
        assert_eq!(app.cloud.accounts[dropbox].access_token, "newer", "refreshed tokens are kept");
        let end = app.cloud.shared[0].expires_at.unwrap();
        assert!(end > unix_now() + 6 * 86_400 && end < unix_now() + 8 * 86_400, "seven days from now");

        // Sharing "via" a program hands the link over once, then forgets it.
        app.open_share_via(Via::WhatsApp);
        assert_eq!(app.cloud.via, Some(Via::WhatsApp));
        app.share_now(&ctx);
        wait(&mut app, &ctx);
        assert_eq!(app.cloud.via, None);
        app.cloud.shared.remove(0);

        // Not due yet: nothing is removed.
        app.poll_cloud(&ctx);
        assert!(removed.lock().unwrap().is_empty() && app.cloud.shared.len() == 1);
        // Time is up: the file is deleted (the default), and the record goes.
        app.cloud.shared[0].expires_at = Some(1);
        app.poll_cloud(&ctx);
        wait(&mut app, &ctx);
        assert_eq!(*removed.lock().unwrap(), vec![("R1".to_string(), true)]);
        assert!(app.cloud.shared.is_empty());

        // With "stop the link, keep the file", the file stays; with "do nothing" nothing is sent.
        app.share_now(&ctx);
        wait(&mut app, &ctx);
        app.cloud.after = AfterExpiry::StopLink;
        app.cloud.shared[0].expires_at = Some(1);
        app.poll_cloud(&ctx);
        wait(&mut app, &ctx);
        assert_eq!(removed.lock().unwrap().last(), Some(&("R1".to_string(), false)));
        app.share_now(&ctx);
        wait(&mut app, &ctx);
        app.cloud.after = AfterExpiry::Leave;
        app.cloud.shared[0].expires_at = Some(1);
        let before = removed.lock().unwrap().len();
        app.poll_cloud(&ctx);
        assert!(app.cloud.shared.is_empty() && removed.lock().unwrap().len() == before);
    }

    #[test]
    fn a_failed_removal_is_kept_and_tried_again_later() {
        let ctx = egui::Context::default();
        let mut app = PdfCraftApp::new();
        app.cloud_source = Some(Arc::new(|_: &CloudJob| Err("offline".to_string())));
        app.cloud.shared.push(SharedFile {
            kind: CloudKind::OneDrive,
            name: "a".into(),
            link: "l".into(),
            remote_id: "r".into(),
            link_id: "i".into(),
            expires_at: Some(1),
        });
        app.poll_cloud(&ctx);
        wait(&mut app, &ctx);
        assert_eq!(app.cloud.shared.len(), 1, "the record stays so the file isn't forgotten");
        assert!(app.cloud.retry_at > unix_now());
        assert!(app.cloud.message.as_deref().unwrap_or("").contains("offline"));
    }

    #[test]
    fn the_link_opens_in_the_chosen_program() {
        let (name, link) = ("Q3 report.pdf", "https://cloud.test/s/a?x=1&y=2");
        assert_eq!(via_url(Via::WhatsApp, name, link), "https://wa.me/?text=Q3%20report.pdf%20https%3A%2F%2Fcloud.test%2Fs%2Fa%3Fx%3D1%26y%3D2");
        assert_eq!(
            via_url(Via::Telegram, name, link),
            "https://t.me/share/url?url=https%3A%2F%2Fcloud.test%2Fs%2Fa%3Fx%3D1%26y%3D2&text=Q3%20report.pdf"
        );
        assert_eq!(
            via_url(Via::Email, name, link),
            "mailto:?subject=Q3%20report.pdf&body=Q3%20report.pdf%0Ahttps%3A%2F%2Fcloud.test%2Fs%2Fa%3Fx%3D1%26y%3D2"
        );
    }

    #[test]
    fn connected_means_something_to_sign_in_with() {
        let mut a = CloudAccount::default();
        assert!(!a.connected(CloudKind::OneDrive) && !a.connected(CloudKind::Nextcloud));
        a.refresh_token = "r".into();
        assert!(a.connected(CloudKind::OneDrive));
        a.server = "https://x".into();
        a.user = "u".into();
        a.password = "p".into();
        assert!(a.connected(CloudKind::Nextcloud));
    }
}
