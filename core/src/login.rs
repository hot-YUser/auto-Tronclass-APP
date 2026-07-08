//! Login: a single entry that routes on **detected login-page features** — never on school
//! identity (docs 30). A common username/password form logs in directly; a captcha page grabs the
//! image for the user to type (no OCR — docs 30); SSO / email-SPA pages defer to the browser-cookie
//! fallback (`ImportCookies`). `detect_login_kind` is pure and unit-tested.

use crate::providers::Endpoints;
use reqwest::Client;

/// A parsed username/password `<form>`: action + field names + **all hidden inputs verbatim** (CSRF
/// tokens must be echoed back on POST).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordForm {
    pub action: String,
    pub user_field: String,
    pub pass_field: String,
    pub hidden: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginKind {
    PasswordForm(PasswordForm),
    /// A password form guarded by an image captcha: carries the form, the captcha image URL, and the
    /// captcha input field name so the flow can grab the image and resubmit with the typed answer.
    Captcha { form: PasswordForm, image_url: String, captcha_field: String },
    SsoRedirect,
    EmailSpa,
    Unknown,
}

/// Outcome of an interactive login attempt. `NeedCaptcha` hands the caller the image bytes to show
/// the user, plus the pending state to resume via `complete_captcha` once they answer.
pub enum LoginOutcome {
    Ok,
    Failed(String),
    NeedCaptcha { image_bytes: Vec<u8>, pending: CaptchaPending },
}

/// Everything needed to finish a captcha login once the user supplies the text. Holds credentials
/// (in `base_form`) — this stays in the caller's task, never in an event.
pub struct CaptchaPending {
    pub action_url: String,
    pub base_form: Vec<(String, String)>,
    pub captcha_field: String,
}

/// Classify a login page by its features. Branch names describe the *page*, not any school.
pub fn detect_login_kind(html: &str, _page_url: &str) -> LoginKind {
    let lower = html.to_lowercase();
    let form = find_password_form(html);

    // Captcha gates even a present password form. We can only drive the human-in-loop flow if the
    // page also has a parseable password form; otherwise fall through to the other classifications.
    if lower.contains("captcha") {
        if let Some(form) = form {
            let image_url = find_captcha_image(html).unwrap_or_default();
            let captcha_field = find_captcha_field(html).unwrap_or_else(|| "captcha".to_string());
            return LoginKind::Captcha { form, image_url, captcha_field };
        }
    }
    // Enterprise SSO (NetIQ NAM `nidp`, SAML, generic "single sign-on").
    if lower.contains("nidp") || lower.contains("saml") || lower.contains("single sign-on") {
        return LoginKind::SsoRedirect;
    }
    if let Some(form) = form {
        return LoginKind::PasswordForm(form);
    }
    // Public-cloud email SPA: no server-rendered password form, a JS app root or email-first field.
    if lower.contains("type=\"email\"") || lower.contains("id=\"app\"") {
        return LoginKind::EmailSpa;
    }
    LoginKind::Unknown
}

/// Find the first `<form>` containing a password input; extract its action, the username/password
/// field names, and **all hidden inputs verbatim** (CSRF tokens must be echoed back on POST).
fn find_password_form(html: &str) -> Option<PasswordForm> {
    let dom = tl::parse(html, tl::ParserOptions::default()).ok()?;
    let parser = dom.parser();

    for form_handle in dom.query_selector("form")?.collect::<Vec<_>>() {
        let Some(form_tag) = form_handle.get(parser).and_then(|n| n.as_tag()) else { continue };

        let mut pass_field = None;
        let mut user_field: Option<String> = None;
        let mut hidden = Vec::new();

        // Walk every descendant input of this form (tl gives all descendants via children().all).
        for child in form_tag.children().all(parser) {
            let Some(input) = child.as_tag() else { continue };
            if input.name().as_utf8_str() != "input" {
                continue;
            }
            let attrs = input.attributes();
            let ty = attrs
                .get("type")
                .flatten()
                .map(|b| b.as_utf8_str().to_lowercase())
                .unwrap_or_else(|| "text".to_string());
            let Some(name) = attrs.get("name").flatten() else { continue };
            let name = name.as_utf8_str().to_string();
            match ty.as_str() {
                "password" => pass_field = Some(name),
                "hidden" => {
                    let val = attrs
                        .get("value")
                        .flatten()
                        .map(|b| b.as_utf8_str().to_string())
                        .unwrap_or_default();
                    hidden.push((name, val));
                }
                "text" | "email" | "tel" | "" if user_field.is_none() => user_field = Some(name),
                _ => {}
            }
        }

        if let Some(pass_field) = pass_field {
            let action = form_tag
                .attributes()
                .get("action")
                .flatten()
                .map(|b| b.as_utf8_str().to_string())
                .unwrap_or_default();
            return Some(PasswordForm {
                action,
                user_field: user_field.unwrap_or_else(|| "username".to_string()),
                pass_field,
                hidden,
            });
        }
    }
    None
}

/// The captcha image URL: the first `<img>` whose `src` looks like a captcha, else the first `<img>`.
fn find_captcha_image(html: &str) -> Option<String> {
    let dom = tl::parse(html, tl::ParserOptions::default()).ok()?;
    let parser = dom.parser();
    let mut first: Option<String> = None;
    for h in dom.query_selector("img")?.collect::<Vec<_>>() {
        let Some(tag) = h.get(parser).and_then(|n| n.as_tag()) else { continue };
        let Some(src) = tag.attributes().get("src").flatten() else { continue };
        let src = src.as_utf8_str().to_string();
        if src.to_lowercase().contains("captcha") || src.to_lowercase().contains("verif") {
            return Some(src);
        }
        first.get_or_insert(src);
    }
    first
}

/// The captcha input field name: the first `<input>` whose name mentions captcha/verify/code.
fn find_captcha_field(html: &str) -> Option<String> {
    let dom = tl::parse(html, tl::ParserOptions::default()).ok()?;
    let parser = dom.parser();
    for h in dom.query_selector("input")?.collect::<Vec<_>>() {
        let Some(tag) = h.get(parser).and_then(|n| n.as_tag()) else { continue };
        let Some(name) = tag.attributes().get("name").flatten() else { continue };
        let name = name.as_utf8_str().to_string();
        let low = name.to_lowercase();
        if low.contains("captcha") || low.contains("verif") || low.contains("code") {
            return Some(name);
        }
    }
    None
}

/// Perform login over `client` (whose cookie jar the caller owns for caching). GET the login page
/// so its Set-Cookies (session/CSRF) enter the jar, detect the kind, and for a password form POST
/// the credentials + hidden fields, then confirm the session is genuinely authenticated. A captcha
/// page returns `NeedCaptcha` with the image bytes; SSO / email-SPA / unknown pages return `Failed`
/// (the caller then offers the browser-cookie fallback, `ImportCookies`).
pub async fn login(
    client: &Client,
    endpoints: &Endpoints,
    username: &str,
    password: &str,
) -> LoginOutcome {
    let html = match client.get(endpoints.login_page()).send().await {
        Ok(page) => match page.text().await {
            Ok(h) => h,
            Err(e) => return LoginOutcome::Failed(e.to_string()),
        },
        Err(e) => return LoginOutcome::Failed(format!("connect: {e}")),
    };

    match detect_login_kind(&html, &endpoints.login_page()) {
        LoginKind::PasswordForm(form) => {
            let action_url = resolve_action(endpoints, &form.action);
            let mut fields: Vec<(String, String)> = form.hidden;
            fields.push((form.user_field, username.to_string()));
            fields.push((form.pass_field, password.to_string()));

            // Same client/jar carries the CSRF+session cookies from the GET into this POST.
            if let Err(e) = client.post(&action_url).form(&fields).send().await {
                return LoginOutcome::Failed(format!("post: {e}"));
            }
            if verify_session(client, endpoints).await {
                LoginOutcome::Ok
            } else {
                LoginOutcome::Failed("login failed: credentials rejected or session not established".into())
            }
        }
        LoginKind::Captcha { form, image_url, captcha_field } => {
            let action_url = resolve_action(endpoints, &form.action);
            let mut base_form: Vec<(String, String)> = form.hidden;
            base_form.push((form.user_field, username.to_string()));
            base_form.push((form.pass_field, password.to_string()));

            let img_url = resolve_action(endpoints, &image_url);
            let image_bytes = match client.get(&img_url).send().await {
                Ok(r) => match r.bytes().await {
                    Ok(b) => b.to_vec(),
                    Err(e) => return LoginOutcome::Failed(format!("captcha image: {e}")),
                },
                Err(e) => return LoginOutcome::Failed(format!("captcha image: {e}")),
            };
            LoginOutcome::NeedCaptcha {
                image_bytes,
                pending: CaptchaPending { action_url, base_form, captcha_field },
            }
        }
        LoginKind::SsoRedirect => LoginOutcome::Failed("此校為企業 SSO 登入，請改用瀏覽器 cookie 匯入登入".into()),
        LoginKind::EmailSpa => LoginOutcome::Failed("此校為公有雲 email 登入頁，請改用瀏覽器 cookie 匯入登入".into()),
        LoginKind::Unknown => LoginOutcome::Failed("無法辨識的登入頁型態，請改用瀏覽器 cookie 匯入登入".into()),
    }
}

/// Finish a captcha login: append the user-typed captcha to the pending form, POST it, and confirm.
pub async fn complete_captcha(
    client: &Client,
    endpoints: &Endpoints,
    pending: CaptchaPending,
    captcha_text: &str,
) -> Result<(), String> {
    let mut form = pending.base_form;
    form.push((pending.captcha_field, captcha_text.to_string()));
    client
        .post(&pending.action_url)
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("post: {e}"))?;
    if verify_session(client, endpoints).await {
        Ok(())
    } else {
        Err("login failed: captcha rejected or session not established".into())
    }
}

/// Standard base64 (encode-only, with padding). Hand-rolled to avoid a dependency for the single use
/// of shipping captcha image bytes across the UTF-8-JSON event seam (same ethos as the hand-rolled
/// account-id hex / no futures-stream dep).
pub fn encode_base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// Content-based session check — NEVER status-code alone. A failed/expired login commonly returns
/// HTTP 200 with the login page or redirects back to it; only a genuine authenticated JSON body
/// counts. Also restores a cached session: call it before re-login to skip an unnecessary one.
pub async fn verify_session(client: &Client, endpoints: &Endpoints) -> bool {
    let resp = match client.get(endpoints.current_semester()).send().await {
        Ok(r) => r,
        Err(_) => return false,
    };
    if !resp.status().is_success() {
        return false;
    }
    if resp.url().path().contains("login") {
        return false; // redirected back to a login page
    }
    let body = resp.text().await.unwrap_or_default();
    serde_json::from_str::<serde_json::Value>(body.trim())
        .map(|v| v.is_object())
        .unwrap_or(false)
}

fn resolve_action(endpoints: &Endpoints, action: &str) -> String {
    if action.is_empty() {
        return endpoints.login_page();
    }
    if action.starts_with("http://") || action.starts_with("https://") {
        return action.to_string();
    }
    if let Some(rest) = action.strip_prefix('/') {
        return format!("{}/{}", endpoints.base(), rest);
    }
    format!("{}/{}", endpoints.base(), action)
}
