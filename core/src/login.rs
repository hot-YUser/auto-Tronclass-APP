//! Login: a single entry that routes on **detected login-page features** — never on school
//! identity (docs 30). A common username/password form logs in directly; a captcha page grabs the
//! image for the user to type (no OCR — docs 30); SSO / email-SPA pages defer to the browser-cookie
//! fallback (`ImportCookies`). `detect_login_kind` is pure and unit-tested.

use crate::providers::Endpoints;
use reqwest::Client;

/// A parsed username/password `<form>`: action + field names + **every other named input verbatim**
/// (CSRF/theme tokens must be echoed back on POST — not only `type=hidden`, some CAS/Keycloak themes
/// render the token as a visible input). `captcha_field` is set when a form input matches the captcha
/// allowlist (a form-scoped decision, never a whole-page substring).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordForm {
    pub action: String,
    pub user_field: String,
    pub pass_field: String,
    pub fields: Vec<(String, String)>,
    pub captcha_field: Option<String>,
}

/// Captcha input-field allowlist (v1 `_classify_captcha`) — common static-`<img>` captcha field names.
/// Deliberately NO bare `"code"` (it matches `session_code`/`authorization_code`). NOTE: Keycloak
/// (`captchakey`/`captchacode`) is intentionally NOT here: an enforcing Keycloak realm serves the captcha
/// image via AJAX JSON, not an `<img>`, so we can't auto-drive it — such tenants must use the
/// browser-cookie login (Phase C). Don't imply support that isn't there (the QR "尚未發現" honesty rule).
const CAPTCHA_FIELDS: &[&str] = &[
    "captcha", "authcode", "auth_code", "verify_code", "verifycode",
    "checkcode", "check_code", "vcode", "yzm", "imgcode", "seccode", "kaptcha", "validatecode", "validate_code",
];

fn is_captcha_field(name: &str) -> bool {
    let low = name.to_lowercase();
    CAPTCHA_FIELDS.iter().any(|f| low.contains(f))
}

/// A public-cloud TronClass email login, extracted from the `<login-view>` web component. Its
/// credentials POST plain to a discoverable endpoint (`{origin}/login?login=email`), so it logs in
/// headlessly — no browser needed. Ground truth: v1 `tron_http.extract_public_cloud_email_login_form`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicCloudForm {
    pub action: String,
    pub fields: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginKind {
    PasswordForm(PasswordForm),
    /// Public-cloud email SPA whose credentials POST to a discoverable endpoint — logs in headlessly.
    PublicCloudEmail(PublicCloudForm),
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
pub fn detect_login_kind(html: &str, page_url: &str) -> LoginKind {
    let lower = html.to_lowercase();
    let form = find_password_form(html);

    // Public-cloud email SPA: a `<login-view>` web component. Its credentials POST to a discoverable
    // endpoint, so extract the form and log in headlessly (ground truth: v1
    // extract_public_cloud_email_login_form). Checked first — this page has no server-rendered form.
    if lower.contains("<login-view") {
        if let Some(form) = extract_public_cloud_form(html, page_url) {
            return LoginKind::PublicCloudEmail(form);
        }
    }

    // Captcha ONLY when the password form itself carries a captcha field AND a real captcha <img> is
    // present — not merely because page copy/JS mentions "captcha" (that false-blocks the many schools
    // whose form is plain username+password but whose page text says "captcha"). Verified live 2026-06.
    if let Some(f) = &form {
        if let Some(cf) = &f.captcha_field {
            if let Some(image_url) = find_captcha_image(html) {
                return LoginKind::Captcha { form: f.clone(), image_url, captcha_field: cf.clone() };
            }
        }
    }
    // Enterprise SSO: NetIQ NAM (`nidp`), NEAI (Tamkang's NAM front-end — confirmed live on iclass.tku),
    // SAML, or a generic "single sign-on". Headless auto-login isn't supported → the caller offers the
    // browser-cookie fallback; classifying it as SSO (not Unknown) gives that honest message.
    if lower.contains("nidp") || lower.contains("neai") || lower.contains("saml") || lower.contains("single sign-on") {
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

        // Collect (name, type, value) for every named <input> in this form, then classify.
        let mut inputs: Vec<(String, String, String)> = Vec::new();
        for child in form_tag.children().all(parser) {
            let Some(input) = child.as_tag() else { continue };
            if input.name().as_utf8_str() != "input" {
                continue;
            }
            let attrs = input.attributes();
            let Some(name) = attrs.get("name").flatten() else { continue };
            let name = name.as_utf8_str().to_string();
            let ty = attrs.get("type").flatten().map(|b| b.as_utf8_str().to_lowercase()).unwrap_or_else(|| "text".to_string());
            let val = attrs.get("value").flatten().map(|b| b.as_utf8_str().to_string()).unwrap_or_default();
            inputs.push((name, ty, val));
        }

        let Some(pass_field) = inputs.iter().find(|(_, ty, _)| ty == "password").map(|(n, ..)| n.clone()) else {
            continue; // not a password form → try the next <form>
        };
        let captcha_field = inputs.iter().find(|(n, ..)| is_captcha_field(n)).map(|(n, ..)| n.clone());
        // Username = first text-like input that isn't the password or the captcha field.
        let user_field = inputs
            .iter()
            .find(|(n, ty, _)| matches!(ty.as_str(), "text" | "email" | "tel" | "") && *n != pass_field && captcha_field.as_deref() != Some(n))
            .map(|(n, ..)| n.clone())
            .unwrap_or_else(|| "username".to_string());
        // Echo EVERY other named input verbatim (hidden CSRF, visible theme tokens) — but not the three
        // fields we fill ourselves (user, pass, captcha). HTML-unescape values (a token may carry `&amp;`).
        let fields: Vec<(String, String)> = inputs
            .iter()
            .filter(|(n, ..)| *n != pass_field && *n != user_field && captcha_field.as_deref() != Some(n))
            .map(|(n, _, v)| (n.clone(), html_unescape(v)))
            .collect();

        // HTML-unescape the action: a Keycloak/CAS action joins its session_code/execution/client_id
        // query params with `&amp;` in the markup. Posting the raw `&amp;` mangles every param after the
        // first (`execution` becomes `amp;execution`), so the IdP rejects the login — this is why every
        // Keycloak school (Tunghai/THU included) failed with "credentials rejected".
        let action = form_tag
            .attributes()
            .get("action")
            .flatten()
            .map(|b| html_unescape(&b.as_utf8_str()))
            .unwrap_or_default();
        return Some(PasswordForm { action, user_field, pass_field, fields, captcha_field });
    }
    None
}

/// Extract the public-cloud email login form from a `<login-view>` page (ground truth: v1
/// `extract_public_cloud_email_login_form`). Body = the component's hidden inputs + `next`/`org_id`/
/// `submit=login` (+`remember_me` when set); the caller appends `email`/`password`. `None` if the
/// component isn't the expected email login.
fn extract_public_cloud_form(html: &str, page_url: &str) -> Option<PublicCloudForm> {
    // Hidden inputs carried in the `email-login-hidden-tag` attribute (an HTML fragment).
    let mut fields: Vec<(String, String)> = Vec::new();
    if let Some(hidden) = attr_value(html, "email-login-hidden-tag") {
        if let Ok(dom) = tl::parse(&hidden, tl::ParserOptions::default()) {
            let parser = dom.parser();
            if let Some(q) = dom.query_selector("input") {
                for h in q.collect::<Vec<_>>() {
                    let Some(tag) = h.get(parser).and_then(|n| n.as_tag()) else { continue };
                    let Some(name) = tag.attributes().get("name").flatten() else { continue };
                    let val = tag.attributes().get("value").flatten().map(|b| b.as_utf8_str().to_string()).unwrap_or_default();
                    fields.push((name.as_utf8_str().to_string(), val));
                }
            }
        }
    }

    // `:email-login-form` is a JSON blob carrying next / org_id / remember defaults.
    let form_json: serde_json::Value = attr_value(html, ":email-login-form")
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    let json_str = |k: &str| form_json.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();

    // next: the hidden input, else the JSON, else the page's `?next=`.
    let mut next = fields.iter().find(|(n, _)| n == "next").map(|(_, v)| v.clone()).unwrap_or_default();
    if next.is_empty() {
        next = json_str("next");
    }
    if next.is_empty() {
        next = query_param(page_url, "next").unwrap_or_default();
    }
    // org_id: the JSON, else the `:org-id` attribute (a literal "0" means "none").
    let mut org_id = json_str("org_id");
    if org_id.is_empty() {
        let a = attr_value(html, ":org-id").unwrap_or_default();
        org_id = if a.trim() == "0" { String::new() } else { a.trim().to_string() };
    }

    // setdefault (keep an existing hidden input; add ours only if absent) — matches v1.
    if !fields.iter().any(|(n, _)| n == "next") {
        fields.push(("next".into(), next.clone()));
    }
    if !fields.iter().any(|(n, _)| n == "org_id") {
        fields.push(("org_id".into(), org_id));
    }
    fields.push(("submit".into(), "login".into()));
    let remember = form_json.get("remember").and_then(|v| v.as_bool()).unwrap_or(false)
        || form_json.get("remember_me").and_then(|v| v.as_bool()).unwrap_or(false);
    if remember && !fields.iter().any(|(n, _)| n == "remember_me") {
        fields.push(("remember_me".into(), "true".into()));
    }

    Some(PublicCloudForm { action: public_cloud_email_url(page_url, &next), fields })
}

/// The public-cloud email login POST URL: `{origin}/login?login=email` (+ `next` when set). Built on
/// `reqwest::Url` so the query is percent-encoded for us (v1 order: `next` before `login`).
fn public_cloud_email_url(page_url: &str, next: &str) -> String {
    let Ok(mut u) = reqwest::Url::parse(page_url).and_then(|b| b.join("/login")) else {
        return "/login?login=email".to_string();
    };
    {
        let mut q = u.query_pairs_mut();
        if !next.is_empty() {
            q.append_pair("next", next);
        }
        q.append_pair("login", "email");
    }
    u.into()
}

/// One `?key=` value from a URL. ponytail: a tiny scan — enough for the single `next` param.
fn query_param(url: &str, key: &str) -> Option<String> {
    reqwest::Url::parse(url).ok()?.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
}

/// Read a (possibly `:`-prefixed) HTML attribute's value by name, HTML-unescaping entities.
/// ponytail: a light forward scanner, not a full attribute parser — sized for the `<login-view>` tag.
fn attr_value(html: &str, name: &str) -> Option<String> {
    let mut from = 0;
    while let Some(p) = html[from..].find(name) {
        let idx = from + p;
        from = idx + name.len();
        let rest = html[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let rest = rest.trim_start();
        let Some(quote) = rest.chars().next().filter(|c| *c == '\'' || *c == '"') else { continue };
        if let Some(end) = rest[1..].find(quote) {
            return Some(html_unescape(&rest[1..1 + end]));
        }
    }
    None
}

/// Unescape the entities that can appear in an HTML **attribute** value (`&amp;` + the quote forms);
/// `<`/`>` don't need escaping in attributes, so they're intentionally omitted. `&amp;` goes last so a
/// literal `&amp;quot;` isn't double-decoded.
fn html_unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The captcha image URL: an `<img>` whose `src` looks like a captcha, else the first `<img>` that
/// isn't obviously page chrome (logo/banner/icon/favicon). `None` ⇒ no captcha image on the page.
fn find_captcha_image(html: &str) -> Option<String> {
    let dom = tl::parse(html, tl::ParserOptions::default()).ok()?;
    let parser = dom.parser();
    let mut fallback: Option<String> = None;
    for h in dom.query_selector("img")?.collect::<Vec<_>>() {
        let Some(tag) = h.get(parser).and_then(|n| n.as_tag()) else { continue };
        let Some(src) = tag.attributes().get("src").flatten() else { continue };
        let src = src.as_utf8_str().to_string();
        let low = src.to_lowercase();
        // strong-match toward v1's find_captcha_source regexes.
        if ["captcha", "verif", "authimage", "getcode", "get_code", "kaptcha", "yzm", "valid"].iter().any(|w| low.contains(w)) {
            return Some(src);
        }
        // fallback: the first <img> that isn't obviously page chrome.
        if fallback.is_none()
            && !["logo", "banner", "icon", "favicon", "header", "download_file", "loading", "btn", "button", "sprite", "avatar", "qrcode"]
                .iter()
                .any(|w| low.contains(w))
        {
            fallback = Some(src);
        }
    }
    fallback
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
    // Capture the FINAL url after redirects (an SSO/Keycloak host) BEFORE consuming the body — a
    // relative form action / captcha src must resolve against where the page actually landed, not base.
    let (final_url, html) = match client.get(endpoints.login_page()).send().await {
        Ok(page) => {
            let url = page.url().to_string();
            match page.text().await {
                Ok(h) => (url, h),
                Err(e) => return LoginOutcome::Failed(e.to_string()),
            }
        }
        Err(e) => return LoginOutcome::Failed(format!("connect: {e}")),
    };

    match detect_login_kind(&html, &final_url) {
        LoginKind::PasswordForm(form) => {
            let action_url = resolve_action(&final_url, &form.action);
            let mut fields: Vec<(String, String)> = form.fields;
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
        LoginKind::PublicCloudEmail(form) => {
            // action is already absolute (built from the page origin); fields carry the hidden inputs.
            let mut fields = form.fields;
            fields.push(("email".to_string(), username.to_string()));
            fields.push(("password".to_string(), password.to_string()));
            if let Err(e) = client.post(&form.action).form(&fields).send().await {
                return LoginOutcome::Failed(format!("post: {e}"));
            }
            if verify_session(client, endpoints).await {
                LoginOutcome::Ok
            } else {
                LoginOutcome::Failed("login failed: credentials rejected or session not established".into())
            }
        }
        LoginKind::Captcha { form, image_url, captcha_field } => {
            let action_url = resolve_action(&final_url, &form.action);
            let mut base_form: Vec<(String, String)> = form.fields;
            base_form.push((form.user_field, username.to_string()));
            base_form.push((form.pass_field, password.to_string()));

            let img_url = resolve_action(&final_url, &image_url);
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

/// The account's own identity for per-account recheck / my_present — the **login username**, lowercased.
/// Confirmed against a real tenant (2026-07): a rollcall roster's `student_rollcalls[].user_no` is the
/// login id itself (e.g. the account email), NOT any numeric id — and `/api/user` returns `{message}`
/// here, so it is never a reliable source. Matches v1 `_current_user_no` (the active profile's `user`).
pub fn user_no_from_username(username: &str) -> String {
    username.trim().to_lowercase()
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

/// Resolve a form action / captcha `src` against the ACTUAL fetched page URL (post-redirect), so a
/// relative action on an SSO/Keycloak login page posts to that host — not the configured base.
fn resolve_action(page_url: &str, action: &str) -> String {
    if action.is_empty() {
        return page_url.to_string();
    }
    match reqwest::Url::parse(page_url).and_then(|base| base.join(action)) {
        Ok(u) => u.to_string(),
        Err(_) => action.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed `<login-view>` mirroring the real www.tronclass.com.tw markup (single-quoted attrs).
    const LOGIN_VIEW: &str = r#"<html><body><div id="app"><login-view token="abc123" next=""
        email-login-hidden-tag='<input id="next" name="next" type="hidden" value="">'
        :email-login-form='{"captcha_code": "", "email": "", "next": null, "org_id": "", "password": "", "remember": false, "submit": false}'
        :org-id='0'></login-view></div></body></html>"#;

    #[test]
    fn public_cloud_email_detected_and_posts_plain() {
        let kind = detect_login_kind(LOGIN_VIEW, "https://www.tronclass.com.tw/login");
        let LoginKind::PublicCloudEmail(form) = kind else { panic!("expected PublicCloudEmail, got {kind:?}") };
        assert_eq!(form.action, "https://www.tronclass.com.tw/login?login=email");
        // body carries the hidden `next`, an `org_id` (empty, "0" dropped), and submit=login.
        assert!(form.fields.contains(&("next".into(), "".into())));
        assert!(form.fields.contains(&("org_id".into(), "".into())));
        assert!(form.fields.contains(&("submit".into(), "login".into())));
        assert!(!form.fields.iter().any(|(n, _)| n == "remember_me"), "remember=false → no remember_me");
    }

    #[test]
    fn login_view_less_email_spa_still_defers() {
        // No <login-view> → the old browser-fallback path is unchanged.
        assert!(matches!(detect_login_kind(r#"<div id="app"></div>"#, "u"), LoginKind::EmailSpa));
    }

    #[test]
    fn keycloak_form_action_ampersands_are_unescaped() {
        // A Keycloak/CAS login form (Tunghai/THU): the action joins its query params with `&amp;`.
        // The extracted action must decode them to `&`, or every param after session_code is mangled.
        let html = r#"<html><body><form id="kc-form-login" method="post"
            action="https://idp.example/auth/realms/x/login-actions/authenticate?session_code=SC&amp;execution=EX&amp;client_id=tronclass&amp;tab_id=TB">
            <input name="username" type="text"><input name="password" type="password">
            </form></body></html>"#;
        let form = find_password_form(html).expect("password form");
        assert_eq!(form.user_field, "username");
        assert_eq!(form.pass_field, "password");
        assert!(!form.action.contains("&amp;"), "action must be HTML-unescaped, got {}", form.action);
        assert!(
            form.action.contains("session_code=SC&execution=EX&client_id=tronclass&tab_id=TB"),
            "all query params intact, got {}",
            form.action
        );
    }
}
