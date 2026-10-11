//! Share as link: puts a copy of the document in the user's own cloud and makes a link to it.
//!
//! Google Drive, OneDrive and Dropbox are connected by signing in once in the browser (OAuth 2
//! with PKCE; a redirect to this computer only, nothing is stored but the tokens), Nextcloud with
//! an address and an app password. Every request goes over HTTPS with the user's own account,
//! only when they press Share, Connect or Stop sharing, or when a link they made has run out
//! (the app removes it then). The providers' answers are untrusted: their size is capped and only
//! the few fields we need are read.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pdfcraft_ui_egui::{CloudAccount, CloudAction, CloudJob, CloudKind, CloudOutput, SharedFile, ai_endpoint_allowed};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The address the browser comes back to. Register exactly this as the redirect in the
/// provider's developer console.
pub const REDIRECT: &str = "http://localhost:53682/";
const PORT: u16 = 53682;
const MAX_UPLOAD: usize = 100 << 20;
const MAX_ANSWER: u64 = 1 << 20;
const SIGN_IN_WAIT: Duration = Duration::from_secs(300);

/// One HTTP request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Req {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Req {
    fn new(method: &'static str, url: impl Into<String>) -> Self {
        Req { method, url: url.into(), headers: Vec::new(), body: Vec::new() }
    }
    fn header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.to_string(), v.into()));
        self
    }
    fn body(mut self, content_type: &str, body: Vec<u8>) -> Self {
        self.headers.push(("Content-Type".into(), content_type.into()));
        self.body = body;
        self
    }
    fn json(self, v: &Value) -> Self {
        self.body("application/json", v.to_string().into_bytes())
    }
    fn form(self, pairs: &[(&str, &str)]) -> Self {
        self.body("application/x-www-form-urlencoded", form(pairs).into_bytes())
    }
}

/// Sends a request; returns the status and the (capped) answer.
pub type Transport<'a> = &'a dyn Fn(&Req) -> Result<(u16, Vec<u8>), String>;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The real transport: HTTPS over rustls with the operating system's trusted roots.
fn network(r: &Req) -> Result<(u16, Vec<u8>), String> {
    let https = r.url.to_ascii_lowercase().starts_with("https://");
    let mut config = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(600))).http_status_as_error(false);
    if https {
        config = config.tls_config(ureq::tls::TlsConfig::builder().root_certs(crate::updates::os_roots()?).build());
    }
    let agent = config.build().new_agent();
    let mut b = ureq::http::Request::builder().method(r.method).uri(&r.url).header("User-Agent", concat!("PdfCraft/", env!("CARGO_PKG_VERSION")));
    for (k, v) in &r.headers {
        b = b.header(k.as_str(), v.as_str());
    }
    let request = b.body(r.body.clone()).map_err(|e| format!("bad request ({e})"))?;
    let mut response = agent.run(request).map_err(|e| format!("couldn't reach the service ({e})"))?;
    let status = response.status().as_u16();
    let body = response.body_mut().with_config().limit(MAX_ANSWER).read_to_vec().map_err(|e| format!("unreadable answer ({e})"))?;
    Ok((status, body))
}

// ---------------------------------------------------------------- small encoders

/// Percent-encode everything but the unreserved characters.
pub fn pct(s: &str) -> String {
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

fn form(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", pct(k), pct(v))).collect::<Vec<_>>().join("&")
}

fn base64(bytes: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..=chunk.len() {
            out.push(alphabet[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
        if pad {
            for _ in chunk.len()..3 {
                out.push('=');
            }
        }
    }
    out
}

const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// PKCE: the challenge for a verifier (S256).
pub fn challenge(verifier: &str) -> String {
    base64(&Sha256::digest(verifier.as_bytes()), URL, false)
}

fn random_token(bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| format!("no secure random numbers ({e})"))?;
    Ok(base64(&buf, URL, false))
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    if getrandom::fill(&mut buf).is_err() {
        buf = now().to_le_bytes()[..bytes.min(8)].to_vec();
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Year-month-day of a Unix time (UTC).
pub fn ymd(secs: u64) -> String {
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// A name that is safe in a path and unlikely to replace another file: letters, digits and a
/// few marks, then a random suffix and `.pdf`.
pub fn upload_name(name: &str) -> String {
    let stem = name.rsplit_once('.').filter(|(_, e)| e.eq_ignore_ascii_case("pdf")).map_or(name, |(s, _)| s);
    let mut clean: String = stem.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | ' ' | '.') { c } else { '_' }).collect();
    clean = clean.trim().trim_matches('.').chars().take(80).collect();
    if clean.is_empty() {
        clean = "document".into();
    }
    format!("{clean}-{}.pdf", random_hex(3))
}

// ---------------------------------------------------------------- reading answers

fn parse(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

fn failure(service: &str, status: u16, body: &[u8]) -> String {
    let v = parse(body);
    let msg = v["error"]["message"]
        .as_str()
        .or_else(|| v["error_description"].as_str())
        .or_else(|| v["error_summary"].as_str())
        .or_else(|| v["error"].as_str())
        .or_else(|| v["ocs"]["meta"]["message"].as_str())
        .unwrap_or("");
    let msg: String = msg.chars().take(200).collect();
    match (status, msg.is_empty()) {
        (401 | 403, true) => format!("{service} refused the request (HTTP {status}): sign in again in Preferences"),
        (_, true) => format!("{service} answered with an error (HTTP {status})"),
        (_, false) => format!("{service}: {msg} (HTTP {status})"),
    }
}

fn ok(service: &str, (status, body): (u16, Vec<u8>)) -> Result<Value, String> {
    if (200..300).contains(&status) { Ok(parse(&body)) } else { Err(failure(service, status, &body)) }
}

fn text<'a>(v: &'a Value, what: &str) -> Result<&'a str, String> {
    v.as_str().filter(|s| !s.is_empty() && s.len() <= 2048).ok_or_else(|| format!("the service's answer had no {what}"))
}

// ---------------------------------------------------------------- OAuth

struct Oauth {
    auth: &'static str,
    token: &'static str,
    scope: &'static str,
    extra: &'static [(&'static str, &'static str)],
}

fn oauth(kind: CloudKind) -> Option<Oauth> {
    Some(match kind {
        CloudKind::GoogleDrive => Oauth {
            auth: "https://accounts.google.com/o/oauth2/v2/auth",
            token: "https://oauth2.googleapis.com/token",
            scope: "https://www.googleapis.com/auth/drive.file",
            extra: &[("access_type", "offline"), ("prompt", "consent")],
        },
        CloudKind::OneDrive => Oauth {
            auth: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
            token: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
            scope: "Files.ReadWrite offline_access",
            extra: &[],
        },
        CloudKind::Dropbox => Oauth {
            auth: "https://www.dropbox.com/oauth2/authorize",
            token: "https://api.dropboxapi.com/oauth2/token",
            scope: "files.content.write sharing.write",
            extra: &[("token_access_type", "offline")],
        },
        CloudKind::Nextcloud => return None,
    })
}

/// The address the browser is sent to.
pub fn authorize_url(kind: CloudKind, client_id: &str, state: &str, challenge: &str) -> Option<String> {
    let o = oauth(kind)?;
    let mut pairs = vec![
        ("client_id", client_id),
        ("redirect_uri", REDIRECT),
        ("response_type", "code"),
        ("scope", o.scope),
        ("state", state),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
    ];
    pairs.extend_from_slice(o.extra);
    Some(format!("{}?{}", o.auth, form(&pairs)))
}

/// What the browser's request to the redirect address carries: the code, when the state is ours.
pub fn redirect_code(request_line: &str, state: &str) -> Result<Option<String>, String> {
    let mut parts = request_line.split_whitespace();
    if parts.next() != Some("GET") {
        return Ok(None);
    }
    let Some(target) = parts.next() else { return Ok(None) };
    let Some(query) = target.strip_prefix("/?") else { return Ok(None) };
    let (mut code, mut got_state, mut error) = (None, None, None);
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = unpct(v);
        match k {
            "code" => code = Some(v),
            "state" => got_state = Some(v),
            "error" => error = Some(v),
            _ => {}
        }
    }
    if let Some(e) = error {
        return Err(format!("sign-in was not completed ({})", e.chars().take(80).collect::<String>()));
    }
    if got_state.as_deref() != Some(state) {
        return Ok(None);
    }
    Ok(code.filter(|c| !c.is_empty() && c.len() <= 2048))
}

fn unpct(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        char::from(b).to_digit(16).map(|d| d as u8)
    }
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        match c {
            b'%' => match (b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn open_browser(url: &str) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("refusing to open a page that isn't https".into());
    }
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();
    result.map(|_| ()).map_err(|e| format!("couldn't open the browser ({e})"))
}

/// Wait for the browser to come back to the redirect address; returns the authorization code.
fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let started = Instant::now();
    loop {
        if started.elapsed() > SIGN_IN_WAIT {
            return Err("sign-in took too long; try Connect again".into());
        }
        let mut stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]);
        let line = head.lines().next().unwrap_or("");
        let result = redirect_code(line, state);
        let page = match &result {
            Ok(Some(_)) => "You are signed in. You can close this tab and go back to PdfCraft.",
            Ok(None) => "Nothing to do here.",
            Err(_) => "Sign-in was not completed. You can close this tab.",
        };
        let html = format!("<!doctype html><meta charset=utf-8><title>PdfCraft</title><p style=\"font:16px sans-serif;margin:3em\">{page}</p>");
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
            html.len()
        );
        match result {
            Ok(Some(code)) => return Ok(code),
            Ok(None) => continue,
            Err(e) => return Err(e),
        }
    }
}

fn token_reply(account: &CloudAccount, v: &Value, now: u64) -> Result<CloudAccount, String> {
    let mut a = account.clone();
    a.access_token = text(&v["access_token"], "access token")?.to_string();
    // A refresh keeps the old refresh token when the answer has no new one.
    if let Some(r) = v["refresh_token"].as_str().filter(|r| !r.is_empty() && r.len() <= 4096) {
        a.refresh_token = r.to_string();
    }
    a.expires_at = now + v["expires_in"].as_u64().unwrap_or(3600).min(86_400);
    Ok(a)
}

fn token_request(kind: CloudKind, a: &CloudAccount, grant: &[(&str, &str)]) -> Option<Req> {
    let o = oauth(kind)?;
    let mut pairs: Vec<(&str, &str)> = grant.to_vec();
    pairs.push(("client_id", &a.client_id));
    if !a.client_secret.trim().is_empty() {
        pairs.push(("client_secret", a.client_secret.trim()));
    }
    Some(Req::new("POST", o.token).form(&pairs))
}

fn connect_oauth(kind: CloudKind, a: &CloudAccount, http: Transport) -> Result<CloudAccount, String> {
    if a.client_id.trim().is_empty() {
        return Err("enter the client ID from the provider's developer console first".into());
    }
    let verifier = random_token(48)?;
    let state = random_token(24)?;
    let url = authorize_url(kind, a.client_id.trim(), &state, &challenge(&verifier)).ok_or("this service doesn't sign in this way")?;
    let listener = TcpListener::bind(("127.0.0.1", PORT)).map_err(|e| format!("port {PORT} is busy ({e}); close the other program using it"))?;
    open_browser(&url)?;
    let code = wait_for_code(&listener, &state)?;
    drop(listener);
    let req =
        token_request(kind, a, &[("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", REDIRECT), ("code_verifier", &verifier)])
            .ok_or("this service doesn't sign in this way")?;
    let v = ok(kind.label(), http(&req)?)?;
    token_reply(a, &v, now())
}

/// A valid access token, refreshing when it has run out (or will in a minute).
fn fresh(kind: CloudKind, a: &CloudAccount, http: Transport, now: u64) -> Result<CloudAccount, String> {
    if kind == CloudKind::Nextcloud {
        return Ok(a.clone());
    }
    if a.access_token.is_empty() {
        return Err(format!("{} isn't connected: use Connect in Preferences", kind.label()));
    }
    if a.expires_at > now + 60 {
        return Ok(a.clone());
    }
    if a.refresh_token.is_empty() {
        return Err(format!("the sign-in to {} has run out: use Connect in Preferences", kind.label()));
    }
    let req = token_request(kind, a, &[("grant_type", "refresh_token"), ("refresh_token", &a.refresh_token)]).ok_or("no token address")?;
    let v = ok(kind.label(), http(&req)?)?;
    token_reply(a, &v, now)
}

// ---------------------------------------------------------------- the services

fn bearer(a: &CloudAccount) -> String {
    format!("Bearer {}", a.access_token)
}

fn basic(a: &CloudAccount) -> String {
    format!("Basic {}", base64(format!("{}:{}", a.user, a.password).as_bytes(), STD, true))
}

/// Dropbox takes JSON in a header: anything outside ASCII must be escaped.
pub fn dropbox_arg(v: &Value) -> String {
    let mut out = String::new();
    for c in v.to_string().chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

fn nextcloud_base(a: &CloudAccount) -> Result<String, String> {
    let base = a.server.trim().trim_end_matches('/');
    if !ai_endpoint_allowed(base) {
        return Err("the Nextcloud address must start with https:// (http:// only for this computer)".into());
    }
    if a.user.trim().is_empty() || a.password.is_empty() {
        return Err("enter the Nextcloud user name and an app password".into());
    }
    Ok(base.to_string())
}

/// The id Nextcloud files this account's data under. It can differ from the name typed at sign-in
/// (an e-mail address, say), so ask the server rather than assume.
fn nextcloud_uid(a: &CloudAccount, base: &str, http: Transport) -> Result<String, String> {
    let req = Req::new("GET", format!("{base}/ocs/v2.php/cloud/user"))
        .header("Authorization", basic(a))
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json");
    let v = ok("Nextcloud (sign-in check)", http(&req)?)?;
    Ok(v["ocs"]["data"]["id"].as_str().filter(|s| !s.is_empty()).unwrap_or(a.user.trim()).to_string())
}

fn dav_path(uid: &str, base: &str, name: &str) -> String {
    format!("{base}/remote.php/dav/files/{}/PdfCraft{}", pct(uid), if name.is_empty() { String::new() } else { format!("/{}", pct(name)) })
}

/// Check a Nextcloud account by asking who it is.
fn nextcloud_check(a: &CloudAccount, http: Transport) -> Result<(), String> {
    let base = nextcloud_base(a)?;
    let req = Req::new("GET", format!("{base}/ocs/v2.php/cloud/user"))
        .header("Authorization", basic(a))
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json");
    ok("Nextcloud", http(&req)?).map(|_| ())
}

/// Upload `bytes` and make a public link. `days` sets the link's own end date where the service
/// supports one (Nextcloud); the app also removes the link itself when it has run out.
pub fn share(
    kind: CloudKind,
    a: &CloudAccount,
    name: &str,
    bytes: &[u8],
    days: Option<u32>,
    now: u64,
    http: Transport,
) -> Result<(CloudAccount, SharedFile), String> {
    if bytes.len() > MAX_UPLOAD {
        return Err(format!("the document is larger than {} MB", MAX_UPLOAD >> 20));
    }
    let a = fresh(kind, a, http, now)?;
    let file = upload_name(name);
    let (remote_id, link_id, link) = match kind {
        CloudKind::GoogleDrive => {
            let boundary = format!("pdfcraft{}", random_hex(8));
            let meta = json!({ "name": file, "mimeType": "application/pdf" }).to_string();
            let mut body = format!(
                "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n--{boundary}\r\nContent-Type: application/pdf\r\n\r\n"
            )
            .into_bytes();
            body.extend_from_slice(bytes);
            body.extend_from_slice(format!("\r\n--{boundary}--").as_bytes());
            let up = Req::new("POST", "https://www.googleapis.com/upload/drive/v3/files?uploadType=multipart&fields=id")
                .header("Authorization", bearer(&a))
                .body(&format!("multipart/related; boundary={boundary}"), body);
            let id = text(&ok("Google Drive", http(&up)?)?["id"], "file id")?.to_string();
            let perm = Req::new("POST", format!("https://www.googleapis.com/drive/v3/files/{}/permissions", pct(&id)))
                .header("Authorization", bearer(&a))
                .json(&json!({ "role": "reader", "type": "anyone" }));
            let perm_id = ok("Google Drive", http(&perm)?)?["id"].as_str().unwrap_or("anyoneWithLink").to_string();
            let get = Req::new("GET", format!("https://www.googleapis.com/drive/v3/files/{}?fields=webViewLink", pct(&id)))
                .header("Authorization", bearer(&a));
            let link = text(&ok("Google Drive", http(&get)?)?["webViewLink"], "link")?.to_string();
            (id, perm_id, link)
        }
        CloudKind::OneDrive => {
            let up = Req::new("PUT", format!("https://graph.microsoft.com/v1.0/me/drive/root:/PdfCraft/{}:/content", pct(&file)))
                .header("Authorization", bearer(&a))
                .body("application/pdf", bytes.to_vec());
            let id = text(&ok("OneDrive", http(&up)?)?["id"], "file id")?.to_string();
            let create = Req::new("POST", format!("https://graph.microsoft.com/v1.0/me/drive/items/{}/createLink", pct(&id)))
                .header("Authorization", bearer(&a))
                .json(&json!({ "type": "view", "scope": "anonymous" }));
            let v = ok("OneDrive", http(&create)?)?;
            let link = text(&v["link"]["webUrl"], "link")?.to_string();
            let perm_id = v["id"].as_str().unwrap_or_default().to_string();
            (id, perm_id, link)
        }
        CloudKind::Dropbox => {
            let up = Req::new("POST", "https://content.dropboxapi.com/2/files/upload")
                .header("Authorization", bearer(&a))
                .header(
                    "Dropbox-API-Arg",
                    dropbox_arg(&json!({ "path": format!("/PdfCraft/{file}"), "mode": "add", "autorename": true, "mute": true })),
                )
                .body("application/octet-stream", bytes.to_vec());
            let path = text(&ok("Dropbox", http(&up)?)?["path_display"], "file path")?.to_string();
            let create = Req::new("POST", "https://api.dropboxapi.com/2/sharing/create_shared_link_with_settings")
                .header("Authorization", bearer(&a))
                .json(&json!({ "path": path, "settings": { "requested_visibility": "public" } }));
            let link = text(&ok("Dropbox", http(&create)?)?["url"], "link")?.to_string();
            (path, link.clone(), link)
        }
        CloudKind::Nextcloud => {
            let base = nextcloud_base(&a)?;
            // The folder may exist already (405); anything else surfaces when the upload fails.
            let uid = nextcloud_uid(&a, &base, http)?;
            // 405 means the folder exists already.
            let (mk, mk_body) = http(&Req::new("MKCOL", dav_path(&uid, &base, "")).header("Authorization", basic(&a)))?;
            if !(200..300).contains(&mk) && mk != 405 {
                return Err(failure("Nextcloud (creating the PdfCraft folder)", mk, &mk_body));
            }
            let up = Req::new("PUT", dav_path(&uid, &base, &file)).header("Authorization", basic(&a)).body("application/pdf", bytes.to_vec());
            ok("Nextcloud (upload)", http(&up)?)?;
            let path = format!("/PdfCraft/{file}");
            let expire = days.map(|d| ymd(now + u64::from(d) * 86_400));
            let mut pairs = vec![("path", path.as_str()), ("shareType", "3"), ("permissions", "1")];
            if let Some(e) = &expire {
                pairs.push(("expireDate", e));
            }
            let create = Req::new("POST", format!("{base}/ocs/v2.php/apps/files_sharing/api/v1/shares"))
                .header("Authorization", basic(&a))
                .header("OCS-APIRequest", "true")
                .header("Accept", "application/json")
                .form(&pairs);
            let v = ok("Nextcloud (making the link)", http(&create)?)?;
            let link = text(&v["ocs"]["data"]["url"], "link")?.to_string();
            let id = v["ocs"]["data"]["id"]
                .as_str()
                .map(str::to_string)
                .or_else(|| v["ocs"]["data"]["id"].as_u64().map(|n| n.to_string()))
                .unwrap_or_default();
            (file.clone(), id, link)
        }
    };
    let expires_at = days.map(|d| now + u64::from(d) * 86_400);
    Ok((a, SharedFile { kind, name: file, link, remote_id, link_id, expires_at }))
}

/// Stop a link (`delete_file` false) or delete the file (true) from the cloud. A file that is
/// already gone counts as done.
pub fn remove(kind: CloudKind, a: &CloudAccount, item: &SharedFile, delete_file: bool, now: u64, http: Transport) -> Result<CloudAccount, String> {
    let a = fresh(kind, a, http, now)?;
    let auth = ("Authorization", bearer(&a));
    let req = match (kind, delete_file) {
        (CloudKind::GoogleDrive, true) => {
            Req::new("DELETE", format!("https://www.googleapis.com/drive/v3/files/{}", pct(&item.remote_id))).header(auth.0, auth.1)
        }
        (CloudKind::GoogleDrive, false) => Req::new(
            "DELETE",
            format!(
                "https://www.googleapis.com/drive/v3/files/{}/permissions/{}",
                pct(&item.remote_id),
                pct(if item.link_id.is_empty() { "anyoneWithLink" } else { &item.link_id })
            ),
        )
        .header(auth.0, auth.1),
        (CloudKind::OneDrive, true) => {
            Req::new("DELETE", format!("https://graph.microsoft.com/v1.0/me/drive/items/{}", pct(&item.remote_id))).header(auth.0, auth.1)
        }
        (CloudKind::OneDrive, false) => {
            if item.link_id.is_empty() {
                return Err("OneDrive didn't name this link, so it can't be stopped from here; delete the file instead".into());
            }
            Req::new("DELETE", format!("https://graph.microsoft.com/v1.0/me/drive/items/{}/permissions/{}", pct(&item.remote_id), pct(&item.link_id)))
                .header(auth.0, auth.1)
        }
        (CloudKind::Dropbox, true) => {
            Req::new("POST", "https://api.dropboxapi.com/2/files/delete_v2").header(auth.0, auth.1).json(&json!({ "path": item.remote_id }))
        }
        (CloudKind::Dropbox, false) => {
            Req::new("POST", "https://api.dropboxapi.com/2/sharing/revoke_shared_link").header(auth.0, auth.1).json(&json!({ "url": item.link_id }))
        }
        (CloudKind::Nextcloud, true) => {
            let base = nextcloud_base(&a)?;
            let uid = nextcloud_uid(&a, &base, http)?;
            Req::new("DELETE", dav_path(&uid, &base, &item.remote_id)).header("Authorization", basic(&a))
        }
        (CloudKind::Nextcloud, false) => {
            let base = nextcloud_base(&a)?;
            Req::new("DELETE", format!("{base}/ocs/v2.php/apps/files_sharing/api/v1/shares/{}", pct(&item.link_id)))
                .header("Authorization", basic(&a))
                .header("OCS-APIRequest", "true")
                .header("Accept", "application/json")
        }
    };
    let (status, body) = http(&req)?;
    if (200..300).contains(&status) || status == 404 || (kind == CloudKind::Dropbox && status == 409) {
        Ok(a)
    } else {
        Err(failure(kind.label(), status, &body))
    }
}

/// Run a job with the real network.
pub fn run(job: &CloudJob) -> Result<CloudOutput, String> {
    run_with(job, &network, now())
}

fn run_with(job: &CloudJob, http: Transport, now: u64) -> Result<CloudOutput, String> {
    let kind = job.kind;
    match &job.action {
        CloudAction::Connect => {
            let account = if kind == CloudKind::Nextcloud {
                nextcloud_check(&job.account, http)?;
                job.account.clone()
            } else {
                connect_oauth(kind, &job.account, http)?
            };
            Ok(CloudOutput { account, shared: None })
        }
        CloudAction::Share { name, bytes, days } => {
            let (account, shared) = share(kind, &job.account, name, bytes, *days, now, http)?;
            Ok(CloudOutput { account, shared: Some(shared) })
        }
        CloudAction::Remove { item, delete_file } => {
            let account = remove(kind, &job.account, item, *delete_file, now, http)?;
            Ok(CloudOutput { account, shared: None })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Replies in order and records what was sent.
    struct Script {
        replies: RefCell<Vec<(u16, String)>>,
        sent: RefCell<Vec<Req>>,
    }

    impl Script {
        fn new(replies: &[(u16, &str)]) -> Self {
            Script { replies: RefCell::new(replies.iter().rev().map(|(s, b)| (*s, b.to_string())).collect()), sent: RefCell::new(Vec::new()) }
        }
        fn transport(&self) -> impl Fn(&Req) -> Result<(u16, Vec<u8>), String> + '_ {
            move |r| {
                self.sent.borrow_mut().push(r.clone());
                let (s, b) = self.replies.borrow_mut().pop().ok_or("unexpected request")?;
                Ok((s, b.into_bytes()))
            }
        }
    }

    fn account() -> CloudAccount {
        CloudAccount { access_token: "tok".into(), refresh_token: "ref".into(), expires_at: 10_000, client_id: "cid".into(), ..Default::default() }
    }

    #[test]
    fn pkce_matches_the_published_example() {
        assert_eq!(challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        assert_eq!(base64(b"user:pass", STD, true), "dXNlcjpwYXNz");
        assert_eq!(base64(b"ab", STD, true), "YWI=");
    }

    #[test]
    fn dates_and_names() {
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(1_760_000_000), "2025-10-09");
        assert_eq!(ymd(951_782_400), "2000-02-29");
        let n = upload_name("../My report?.pdf");
        assert!(n.starts_with("_") || n.starts_with("My"), "{n}");
        assert!(n.ends_with(".pdf") && !n.contains('/') && !n.contains('?'), "{n}");
        assert_ne!(upload_name("a.pdf"), upload_name("a.pdf"));
        assert!(upload_name("").starts_with("document-"));
    }

    #[test]
    fn the_sign_in_address_asks_for_the_least_it_needs() {
        let u = authorize_url(CloudKind::GoogleDrive, "my id", "st", "ch").unwrap();
        assert!(u.starts_with("https://accounts.google.com/o/oauth2/v2/auth?client_id=my%20id&redirect_uri=http%3A%2F%2Flocalhost%3A53682%2F&response_type=code&scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdrive.file"), "{u}");
        assert!(u.contains("code_challenge=ch&code_challenge_method=S256") && u.contains("access_type=offline"));
        assert!(authorize_url(CloudKind::Nextcloud, "x", "s", "c").is_none());
    }

    #[test]
    fn only_our_own_redirect_is_accepted() {
        assert_eq!(redirect_code("GET /?code=abc%2F1&state=S HTTP/1.1", "S"), Ok(Some("abc/1".into())));
        assert_eq!(redirect_code("GET /?code=abc&state=OTHER HTTP/1.1", "S"), Ok(None), "another page's state is ignored");
        assert_eq!(redirect_code("GET /favicon.ico HTTP/1.1", "S"), Ok(None));
        assert_eq!(redirect_code("POST /?code=a&state=S HTTP/1.1", "S"), Ok(None));
        assert!(redirect_code("GET /?error=access_denied&state=S HTTP/1.1", "S").is_err());
        assert_eq!(unpct("a%2"), "a%2");
        assert_eq!(unpct("%E2%9C%93+x"), "✓ x");
    }

    #[test]
    fn google_drive_uploads_then_opens_the_link_to_anyone() {
        let s = Script::new(&[
            (200, r#"{"id":"F1"}"#),
            (200, r#"{"id":"anyoneWithLink"}"#),
            (200, r#"{"webViewLink":"https://drive.google.com/file/d/F1/view"}"#),
        ]);
        let (_, item) = share(CloudKind::GoogleDrive, &account(), "a.pdf", b"%PDF", Some(7), 1000, &s.transport()).unwrap();
        let sent = s.sent.borrow();
        assert!(sent[0].url.contains("uploadType=multipart") && sent[0].headers.contains(&("Authorization".into(), "Bearer tok".into())));
        assert!(String::from_utf8_lossy(&sent[0].body).contains("%PDF"));
        assert_eq!(sent[1].url, "https://www.googleapis.com/drive/v3/files/F1/permissions");
        assert_eq!(String::from_utf8_lossy(&sent[1].body), r#"{"role":"reader","type":"anyone"}"#);
        assert_eq!(
            (item.link.as_str(), item.remote_id.as_str(), item.expires_at),
            ("https://drive.google.com/file/d/F1/view", "F1", Some(1000 + 7 * 86_400))
        );
    }

    #[test]
    fn one_drive_and_dropbox_make_anonymous_view_links() {
        let s = Script::new(&[(201, r#"{"id":"I1"}"#), (201, r#"{"id":"P1","link":{"webUrl":"https://1drv.ms/b/x"}}"#)]);
        let (_, od) = share(CloudKind::OneDrive, &account(), "a.pdf", b"x", None, 1000, &s.transport()).unwrap();
        assert!(s.sent.borrow()[0].url.starts_with("https://graph.microsoft.com/v1.0/me/drive/root:/PdfCraft/"));
        assert!(String::from_utf8_lossy(&s.sent.borrow()[1].body).contains("\"scope\":\"anonymous\""));
        assert_eq!((od.link.as_str(), od.link_id.as_str(), od.expires_at), ("https://1drv.ms/b/x", "P1", None));

        let s = Script::new(&[(200, r#"{"path_display":"/PdfCraft/é.pdf"}"#), (200, r#"{"url":"https://www.dropbox.com/s/abc/é.pdf?dl=0"}"#)]);
        let (_, db) = share(CloudKind::Dropbox, &account(), "é.pdf", b"x", None, 1000, &s.transport()).unwrap();
        let arg = &s.sent.borrow()[0].headers.iter().find(|(k, _)| k == "Dropbox-API-Arg").unwrap().1.clone();
        assert!(arg.is_ascii() && arg.contains("\\u00e9"), "{arg}");
        assert_eq!((db.remote_id.as_str(), db.link_id.as_str()), ("/PdfCraft/é.pdf", "https://www.dropbox.com/s/abc/é.pdf?dl=0"));
    }

    #[test]
    fn nextcloud_sets_the_links_own_end_date() {
        let mut a = CloudAccount { server: "https://cloud.test/".into(), user: "me".into(), password: "pw".into(), ..Default::default() };
        let s = Script::new(&[
            (200, r#"{"ocs":{"data":{"id":"me"}}}"#),
            (201, ""),
            (201, ""),
            (200, r#"{"ocs":{"data":{"id":"77","url":"https://cloud.test/s/tok"}}}"#),
        ]);
        let (_, item) = share(CloudKind::Nextcloud, &a, "a.pdf", b"x", Some(3), 1_760_000_000, &s.transport()).unwrap();
        let sent = s.sent.borrow();
        assert_eq!((sent[1].method, sent[2].method), ("MKCOL", "PUT"));
        assert!(sent[2].url.starts_with("https://cloud.test/remote.php/dav/files/me/PdfCraft/"));
        assert!(
            String::from_utf8_lossy(&sent[3].body).contains("expireDate=2025-10-12")
                && String::from_utf8_lossy(&sent[3].body).contains("shareType=3")
        );
        assert_eq!((item.link.as_str(), item.link_id.as_str()), ("https://cloud.test/s/tok", "77"));
        // Plain http to another computer is refused before anything is sent.
        a.server = "http://cloud.test".into();
        assert!(share(CloudKind::Nextcloud, &a, "a.pdf", b"x", None, 0, &Script::new(&[]).transport()).is_err());
    }

    #[test]
    fn an_old_token_is_refreshed_and_the_new_one_kept() {
        let s = Script::new(&[
            (200, r#"{"access_token":"new","expires_in":3600}"#),
            (200, r#"{"id":"F1"}"#),
            (200, "{}"),
            (200, r#"{"webViewLink":"https://x.test/v"}"#),
        ]);
        let mut a = account();
        a.expires_at = 500;
        let (a2, _) = share(CloudKind::GoogleDrive, &a, "a.pdf", b"x", None, 1000, &s.transport()).unwrap();
        assert_eq!((a2.access_token.as_str(), a2.refresh_token.as_str(), a2.expires_at), ("new", "ref", 4600));
        let sent = s.sent.borrow();
        assert_eq!(sent[0].url, "https://oauth2.googleapis.com/token");
        assert!(
            String::from_utf8_lossy(&sent[0].body).contains("grant_type=refresh_token")
                && sent[1].headers.contains(&("Authorization".into(), "Bearer new".into()))
        );
        // Without a refresh token the user is told to connect again.
        a.refresh_token.clear();
        assert!(share(CloudKind::GoogleDrive, &a, "a.pdf", b"x", None, 1000, &Script::new(&[]).transport()).unwrap_err().contains("Connect"));
    }

    #[test]
    fn removing_stops_the_link_or_deletes_the_file_and_forgives_a_missing_one() {
        let item = SharedFile {
            kind: CloudKind::GoogleDrive,
            name: "n".into(),
            link: "l".into(),
            remote_id: "F1".into(),
            link_id: "anyoneWithLink".into(),
            expires_at: None,
        };
        let s = Script::new(&[(204, "")]);
        remove(CloudKind::GoogleDrive, &account(), &item, false, 1000, &s.transport()).unwrap();
        assert_eq!(
            (s.sent.borrow()[0].method, s.sent.borrow()[0].url.as_str()),
            ("DELETE", "https://www.googleapis.com/drive/v3/files/F1/permissions/anyoneWithLink")
        );
        let s = Script::new(&[(204, "")]);
        remove(CloudKind::GoogleDrive, &account(), &item, true, 1000, &s.transport()).unwrap();
        assert_eq!(s.sent.borrow()[0].url, "https://www.googleapis.com/drive/v3/files/F1");
        assert!(remove(CloudKind::GoogleDrive, &account(), &item, true, 1000, &Script::new(&[(404, "{}")]).transport()).is_ok());
        let err =
            remove(CloudKind::GoogleDrive, &account(), &item, true, 1000, &Script::new(&[(403, r#"{"error":{"message":"No access"}}"#)]).transport())
                .unwrap_err();
        assert_eq!(err, "Google Drive: No access (HTTP 403)");
    }

    #[test]
    fn the_browser_coming_back_hands_over_the_code() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = std::thread::spawn(move || {
            for target in ["/favicon.ico", "/?code=wrong&state=NOPE", "/?code=the%2Dcode&state=S1"] {
                let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
                write!(s, "GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
                let mut reply = String::new();
                let _ = s.read_to_string(&mut reply);
                assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
            }
        });
        assert_eq!(wait_for_code(&listener, "S1").unwrap(), "the-code");
        client.join().unwrap();
    }

    #[test]
    fn provider_errors_are_explained() {
        let s = Script::new(&[(401, "")]);
        let err = share(CloudKind::OneDrive, &account(), "a.pdf", b"x", None, 1000, &s.transport()).unwrap_err();
        assert!(err.contains("sign in again"), "{err}");
        assert!(
            share(CloudKind::OneDrive, &account(), "a.pdf", &vec![0; MAX_UPLOAD + 1], None, 1000, &Script::new(&[]).transport())
                .unwrap_err()
                .contains("larger")
        );
    }
}
