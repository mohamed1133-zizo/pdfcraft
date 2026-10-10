//! Asks the AI provider (the AI assistant in All tools). One HTTPS request with the user's own
//! key, over rustls with the OS's trusted roots, only when the user presses Send in the
//! assistant's dialog. Three request shapes cover the providers: Anthropic's Messages API,
//! Google's `generateContent` (Gemini), and the OpenAI Chat Completions shape that OpenAI, xAI,
//! DeepSeek, Mistral, Groq, OpenRouter, Ollama and many others speak. The answer is untrusted:
//! its size is capped, and it is shown as text, never run.

use std::time::Duration;

use pdfcraft_ui_egui::{AiKind, AiRequest, ai_endpoint_allowed};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_TOKENS: u32 = 4096;
const MAX_ANSWER_BYTES: u64 = 4 << 20;

/// A request ready to send.
#[derive(Debug, PartialEq, Eq)]
struct Prepared {
    url: String,
    headers: Vec<(&'static str, String)>,
    body: String,
}

/// The key to use: the one typed in, otherwise the provider's environment variable. `None` when
/// there is neither (a local server needs none).
fn key(request: &AiRequest) -> Option<String> {
    let typed = request.api_key.trim();
    if !typed.is_empty() {
        return Some(typed.to_string());
    }
    if request.key_env.is_empty() {
        return None;
    }
    std::env::var(&request.key_env).ok().map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
}

/// A model name is one path segment of Gemini's address: only letters, digits and `._-:` pass.
fn model_segment(model: &str) -> Result<&str, String> {
    let m = model.trim().trim_start_matches("models/");
    if !m.is_empty() && m.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':')) {
        Ok(m)
    } else {
        Err("the model name has characters a model name can't have".into())
    }
}

/// The URL, headers and body for `request` with `key`.
fn prepare(request: &AiRequest, key: Option<&str>) -> Result<Prepared, String> {
    if !ai_endpoint_allowed(&request.base_url) {
        return Err("the address must be https:// (http:// only for this computer)".into());
    }
    let base = request.base_url.trim().trim_end_matches('/');
    let name = request.provider.as_str();
    let need = |what: &str| format!("no API key for {name}: enter one in Provider settings{what}");
    match request.kind {
        AiKind::Anthropic => {
            let key = key.ok_or_else(|| need(""))?;
            let mut headers = vec![("x-api-key", key.to_string()), ("anthropic-version", ANTHROPIC_VERSION.to_string())];
            // A key that isn't scoped to one workspace must say which one the request is for.
            if !request.workspace_id.trim().is_empty() {
                headers.push(("anthropic-workspace-id", request.workspace_id.trim().to_string()));
            }
            let body = serde_json::json!({
                "model": request.model,
                "max_tokens": MAX_TOKENS,
                "system": request.system,
                "messages": [{ "role": "user", "content": request.prompt }],
            });
            Ok(Prepared { url: base.to_string(), headers, body: body.to_string() })
        }
        AiKind::Gemini => {
            let key = key.ok_or_else(|| need(""))?;
            let model = model_segment(&request.model)?;
            let body = serde_json::json!({
                "systemInstruction": { "parts": [{ "text": request.system }] },
                "contents": [{ "role": "user", "parts": [{ "text": request.prompt }] }],
                "generationConfig": { "maxOutputTokens": 8192 },
            });
            Ok(Prepared {
                url: format!("{base}/models/{model}:generateContent"),
                headers: vec![("x-goog-api-key", key.to_string())],
                body: body.to_string(),
            })
        }
        AiKind::OpenAiCompatible => {
            let headers = key.map(|k| vec![("Authorization", format!("Bearer {k}"))]).unwrap_or_default();
            let body = serde_json::json!({
                "model": request.model,
                "messages": [
                    { "role": "system", "content": request.system },
                    { "role": "user", "content": request.prompt },
                ],
            });
            Ok(Prepared { url: format!("{base}/chat/completions"), headers, body: body.to_string() })
        }
    }
}

/// An error's message from any provider's reply: `error.message`, `error` as a string, or
/// `message`.
fn error_message(v: &serde_json::Value) -> Option<String> {
    let e = &v["error"];
    e["message"].as_str().or_else(|| e.as_str()).or_else(|| v["message"].as_str()).map(str::to_string)
}

/// The answer's text from a reply, or the provider's own error message.
fn parse(kind: AiKind, status: u16, text: &str) -> Result<String, String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|_| format!("unreadable answer (HTTP {status})"))?;
    if status >= 400 || v["type"] == "error" {
        let why = error_message(&v).unwrap_or_else(|| "the request was refused".into());
        return Err(format!("{why} (HTTP {status})"));
    }
    let answer: String = match kind {
        AiKind::Anthropic => v["content"]
            .as_array()
            .map(|blocks| blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default(),
        AiKind::Gemini => v["candidates"][0]["content"]["parts"]
            .as_array()
            .map(|parts| parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(""))
            .unwrap_or_default(),
        AiKind::OpenAiCompatible => match &v["choices"][0]["message"]["content"] {
            serde_json::Value::String(s) => s.clone(),
            // Some servers answer with parts: [{ "type": "text", "text": "…" }].
            serde_json::Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(""),
            _ => String::new(),
        },
    };
    if !answer.trim().is_empty() {
        return Ok(answer);
    }
    // Gemini says why it answered nothing: the prompt was blocked.
    match v["promptFeedback"]["blockReason"].as_str() {
        Some(reason) => Err(format!("the provider blocked the request ({reason})")),
        None => Err("the answer was empty".into()),
    }
}

/// Ask the provider. Blocking; the assistant runs it on its own thread.
pub fn ask(request: &AiRequest) -> Result<String, String> {
    let key = key(request);
    let prepared = prepare(request, key.as_deref())?;
    let https = prepared.url.to_ascii_lowercase().starts_with("https://");
    let mut config = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(180))).http_status_as_error(false);
    if https {
        config = config.tls_config(ureq::tls::TlsConfig::builder().root_certs(crate::updates::os_roots()?).build());
    }
    let agent = config.build().new_agent();
    let mut call = agent.post(&prepared.url).header("User-Agent", concat!("PdfCraft/", env!("CARGO_PKG_VERSION"))).content_type("application/json");
    for (name, value) in &prepared.headers {
        call = call.header(*name, value.as_str());
    }
    let mut response = call.send(prepared.body).map_err(|e| format!("couldn't reach {} ({e})", request.provider))?;
    let status = response.status().as_u16();
    let text = response.body_mut().with_config().limit(MAX_ANSWER_BYTES).read_to_string().map_err(|e| format!("unreadable answer ({e})"))?;
    parse(request.kind, status, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(kind: AiKind, base_url: &str, model: &str) -> AiRequest {
        AiRequest {
            kind,
            provider: "Test".into(),
            base_url: base_url.into(),
            api_key: String::new(),
            key_env: String::new(),
            workspace_id: String::new(),
            model: model.into(),
            system: "sys".into(),
            prompt: "hi \"there\"".into(),
        }
    }

    #[test]
    fn anthropic_requests_carry_the_key_in_a_header_never_the_body() {
        let mut r = request(AiKind::Anthropic, "https://api.anthropic.com/v1/messages", "claude-sonnet-5-5");
        r.workspace_id = "wrkspc_1".into();
        let p = prepare(&r, Some("sk-secret")).unwrap();
        assert_eq!(p.url, "https://api.anthropic.com/v1/messages");
        assert!(p.headers.contains(&("x-api-key", "sk-secret".into())) && p.headers.contains(&("anthropic-workspace-id", "wrkspc_1".into())));
        assert!(!p.body.contains("sk-secret"));
        let v: serde_json::Value = serde_json::from_str(&p.body).unwrap();
        assert_eq!(
            (v["model"].as_str(), v["system"].as_str(), v["messages"][0]["content"].as_str()),
            (Some("claude-sonnet-5-5"), Some("sys"), Some("hi \"there\""))
        );
        assert!(prepare(&r, None).is_err(), "Claude needs a key");
    }

    #[test]
    fn gemini_requests_name_the_model_in_the_address() {
        let r = request(AiKind::Gemini, "https://generativelanguage.googleapis.com/v1beta/", "models/gemini-2.5-flash");
        let p = prepare(&r, Some("k")).unwrap();
        assert_eq!(p.url, "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent");
        assert_eq!(p.headers, vec![("x-goog-api-key", "k".to_string())]);
        let v: serde_json::Value = serde_json::from_str(&p.body).unwrap();
        assert_eq!(v["contents"][0]["parts"][0]["text"], "hi \"there\"");
        assert_eq!(v["systemInstruction"]["parts"][0]["text"], "sys");
        // A model name can't escape its path segment.
        assert!(prepare(&request(AiKind::Gemini, "https://x.test/v1beta", "a/../b"), Some("k")).is_err());
        assert!(prepare(&request(AiKind::Gemini, "https://x.test/v1beta", "a?key=1"), Some("k")).is_err());
    }

    #[test]
    fn openai_style_requests_use_a_bearer_key_when_there_is_one() {
        let r = request(AiKind::OpenAiCompatible, "https://api.openai.com/v1", "gpt-4o-mini");
        let p = prepare(&r, Some("sk-x")).unwrap();
        assert_eq!(p.url, "https://api.openai.com/v1/chat/completions");
        assert_eq!(p.headers, vec![("Authorization", "Bearer sk-x".to_string())]);
        let v: serde_json::Value = serde_json::from_str(&p.body).unwrap();
        assert_eq!((v["messages"][0]["role"].as_str(), v["messages"][1]["content"].as_str()), (Some("system"), Some("hi \"there\"")));
        // A local server needs no key at all.
        let local = prepare(&request(AiKind::OpenAiCompatible, "http://localhost:11434/v1", "llama3.2"), None).unwrap();
        assert!(local.headers.is_empty());
        assert_eq!(local.url, "http://localhost:11434/v1/chat/completions");
    }

    #[test]
    fn a_key_is_never_sent_over_plain_http_to_another_computer() {
        for url in ["http://api.example.com/v1", "ftp://x.test/v1", "https://", "https://user@evil.test/v1", "", "localhost:11434/v1"] {
            assert!(prepare(&request(AiKind::OpenAiCompatible, url, "m"), Some("k")).is_err(), "{url}");
        }
        for url in ["http://127.0.0.1:1234/v1", "http://[::1]:8080/v1", "http://localhost/v1"] {
            assert!(prepare(&request(AiKind::OpenAiCompatible, url, "m"), None).is_ok(), "{url}");
        }
    }

    #[test]
    fn answers_and_errors_are_read_for_every_provider() {
        let claude = r#"{"type":"message","content":[{"type":"text","text":"Line one"},{"type":"text","text":"Line two"}]}"#;
        assert_eq!(parse(AiKind::Anthropic, 200, claude).unwrap(), "Line one\nLine two");
        let bad = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        assert_eq!(parse(AiKind::Anthropic, 401, bad).unwrap_err(), "invalid x-api-key (HTTP 401)");

        let gemini = r#"{"candidates":[{"content":{"parts":[{"text":"Hello "},{"text":"world"}]}}]}"#;
        assert_eq!(parse(AiKind::Gemini, 200, gemini).unwrap(), "Hello world");
        let blocked = r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#;
        assert!(parse(AiKind::Gemini, 200, blocked).unwrap_err().contains("SAFETY"));
        let gerr = r#"{"error":{"code":400,"message":"API key not valid.","status":"INVALID_ARGUMENT"}}"#;
        assert_eq!(parse(AiKind::Gemini, 400, gerr).unwrap_err(), "API key not valid. (HTTP 400)");

        let openai = r#"{"choices":[{"message":{"role":"assistant","content":"Answer"}}]}"#;
        assert_eq!(parse(AiKind::OpenAiCompatible, 200, openai).unwrap(), "Answer");
        let parts = r#"{"choices":[{"message":{"content":[{"type":"text","text":"A"},{"type":"text","text":"B"}]}}]}"#;
        assert_eq!(parse(AiKind::OpenAiCompatible, 200, parts).unwrap(), "AB");
        assert_eq!(parse(AiKind::OpenAiCompatible, 429, r#"{"error":{"message":"Rate limit"}}"#).unwrap_err(), "Rate limit (HTTP 429)");
        assert_eq!(parse(AiKind::OpenAiCompatible, 401, r#"{"error":"bad key"}"#).unwrap_err(), "bad key (HTTP 401)");
        assert!(parse(AiKind::OpenAiCompatible, 200, r#"{"choices":[]}"#).is_err());
        assert!(parse(AiKind::Anthropic, 502, "<html>").unwrap_err().contains("502"));
    }
}
