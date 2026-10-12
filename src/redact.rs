//! Credential redaction for the network log.
//!
//! Takeover exists so the agent never sees credentials, but a login the human
//! completes still goes through the session's page, and the listener logs its
//! requests. Every network entry passes through [`network_entry`] before it is
//! written, and again when it is read back (so older logs on disk are served
//! redacted too):
//!
//! - Login/auth URLs (`/authn`, `/oauth`, `/token`, …): request bodies become
//!   [`REDACTED`] and every query value is masked.
//! - Anywhere else, JSON and form-encoded bodies and query strings have the
//!   values of credential-like keys masked (and the username next to a
//!   password); an unparsable body mentioning a credential key is dropped
//!   whole.
//! - While a human takeover is active for the session, no body is recorded at
//!   all, and query values on auth URLs are masked as above.
//!
//! The log records no headers or response bodies, so there is nothing else to
//! redact.
use crate::logs::NetworkEntry;
use serde_json::Value;
use url::form_urlencoded;

pub const REDACTED: &str = "[redacted]";

/// Path fragments that mark a login/auth endpoint.
const AUTH_PATH: &[&str] = &[
    "authn", "login", "logon", "signin", "sign-in", "auth/", "oauth", "token", "session",
    "password", "otp", "mfa", "verify", "sso", "saml",
];

/// Keys whose value is a secret wherever they appear (substring match on the
/// lowercased key with `_`/`-` removed).
const SECRET_SUBSTR: &[&str] = &[
    "password",
    "passwd",
    "passcode",
    "secret",
    "token",
    "credential",
    "apikey",
    "assertion",
    "samlresponse",
    "cookie",
    "authorization",
];

/// Keys that are secrets only as an exact (normalized) name; as substrings they
/// would hit innocent keys ("shipping", "zipcode").
const SECRET_EXACT: &[&str] = &[
    "pass", "pwd", "pin", "otp", "totp", "code", "mfa", "cvv", "cvc", "ssn", "answer",
];

/// Identity keys masked when they sit next to a password.
const IDENTITY: &[&str] = &[
    "username",
    "user",
    "email",
    "login",
    "loginid",
    "userid",
    "identifier",
];

pub fn network_entry(entry: &mut NetworkEntry, takeover_active: bool) {
    let auth = is_auth_url(&entry.url);
    entry.url = url(&entry.url, auth);
    if let Some(to) = entry.redirected_to.as_mut() {
        *to = url(to, auth || is_auth_url(to));
    }
    if takeover_active {
        entry.post_data = None;
    } else if let Some(body) = entry.post_data.as_mut() {
        *body = if auth {
            REDACTED.to_string()
        } else {
            body_text(body)
        };
    }
}

fn is_auth_url(url: &str) -> bool {
    let path = url::Url::parse(url)
        .map(|u| u.path().to_ascii_lowercase())
        .unwrap_or_default();
    AUTH_PATH.iter().any(|p| path.contains(p))
}

fn norm(key: &str) -> String {
    key.chars()
        .filter(|c| *c != '_' && *c != '-')
        .collect::<String>()
        .to_ascii_lowercase()
}

fn is_secret_key(key: &str) -> bool {
    let k = norm(key);
    SECRET_EXACT.contains(&k.as_str()) || SECRET_SUBSTR.iter().any(|s| k.contains(s))
}

fn is_password_key(key: &str) -> bool {
    let k = norm(key);
    k.contains("pass") || k == "pwd"
}

/// Mask query values: all of them on an auth URL, else just secret keys.
fn url(raw: &str, auth: bool) -> String {
    let Ok(mut u) = url::Url::parse(raw) else {
        return raw.to_string();
    };
    let Some(query) = u.query() else {
        return raw.to_string();
    };
    match form(query, auth) {
        Some(masked) => {
            u.set_query(Some(&masked));
            u.to_string()
        }
        None => raw.to_string(),
    }
}

fn body_text(body: &str) -> String {
    if let Ok(mut v) = serde_json::from_str::<Value>(body) {
        if v.is_object() || v.is_array() {
            json(&mut v);
            return v.to_string();
        }
    }
    let looks_form = body.contains('=') && !body.contains(char::is_whitespace);
    if looks_form {
        return form(body, false).unwrap_or_else(|| body.to_string());
    }
    let lower = norm(body);
    if SECRET_SUBSTR.iter().any(|s| lower.contains(s)) {
        REDACTED.to_string()
    } else {
        body.to_string()
    }
}

/// Re-encode with masked values, or `None` when nothing needed masking (so
/// untouched URLs and bodies keep their original encoding).
fn form(encoded: &str, mask_all: bool) -> Option<String> {
    let pairs: Vec<(String, String)> = form_urlencoded::parse(encoded.as_bytes())
        .into_owned()
        .collect();
    let has_password = pairs.iter().any(|(k, _)| is_password_key(k));
    let mut out = form_urlencoded::Serializer::new(String::new());
    let mut masked = false;
    for (k, v) in &pairs {
        let mask =
            mask_all || is_secret_key(k) || (has_password && IDENTITY.contains(&norm(k).as_str()));
        masked |= mask;
        out.append_pair(k, if mask { REDACTED } else { v });
    }
    masked.then(|| out.finish())
}

fn json(v: &mut Value) {
    match v {
        Value::Object(map) => {
            let has_password = map.keys().any(|k| is_password_key(k));
            for (k, val) in map.iter_mut() {
                // A secret key masks its whole value, nested objects included
                // (e.g. `"credentials": {...}`).
                if is_secret_key(k) || (has_password && IDENTITY.contains(&norm(k).as_str())) {
                    *val = Value::String(REDACTED.into());
                } else {
                    json(val);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(json),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(method: &str, url: &str, body: Option<&str>) -> NetworkEntry {
        NetworkEntry {
            t: String::new(),
            method: method.into(),
            url: url.into(),
            status: Some(200),
            failure: None,
            post_data: body.map(Into::into),
            redirected_to: None,
        }
    }

    #[test]
    fn okta_authn_body_is_redacted() {
        let mut e = entry(
            "POST",
            "https://eversource-external.okta.com/api/v1/authn",
            Some(
                r#"{"username":"me@example.com","password":"hunter2","options":{"warnBeforePasswordExpired":true}}"#,
            ),
        );
        network_entry(&mut e, false);
        assert_eq!(e.post_data.as_deref(), Some(REDACTED));
    }

    #[test]
    fn json_password_masked_on_any_url() {
        let mut e = entry(
            "POST",
            "https://example.com/api/account",
            Some(
                r#"{"email":"me@example.com","password":"hunter2","plan":"basic","nested":[{"apiToken":"t0k"}],"headers":{"Authorization":"Bearer abc.def","Cookie":"sid=s3"}}"#,
            ),
        );
        network_entry(&mut e, false);
        let body = e.post_data.unwrap();
        for leaked in ["hunter2", "me@example.com", "t0k", "abc.def", "sid=s3"] {
            assert!(!body.contains(leaked), "{leaked} in {body}");
        }
        assert!(body.contains(r#""plan":"basic""#), "{body}");
    }

    #[test]
    fn form_login_is_masked() {
        let mut e = entry(
            "POST",
            "https://example.com/account/update",
            Some("user=me%40example.com&pass=hunter2&remember=1"),
        );
        network_entry(&mut e, false);
        let body = e.post_data.unwrap();
        assert!(
            !body.contains("hunter2") && !body.contains("example.com"),
            "{body}"
        );
        assert!(body.contains("remember=1"), "{body}");

        let mut e = entry("POST", "https://example.com/login", Some("user=me&pw=x"));
        network_entry(&mut e, false);
        assert_eq!(e.post_data.as_deref(), Some(REDACTED));
    }

    #[test]
    fn auth_query_values_and_redirect_codes_are_masked() {
        let mut e = entry(
            "GET",
            "https://idp.example.com/oauth2/v1/authorize?client_id=abc&state=s1",
            None,
        );
        e.redirected_to =
            Some("https://app.example.com/callback?code=SECRETCODE&next=/home".into());
        network_entry(&mut e, false);
        assert!(!e.url.contains("abc") && !e.url.contains("s1"), "{}", e.url);
        let to = e.redirected_to.unwrap();
        assert!(!to.contains("SECRETCODE"), "{to}");
    }

    #[test]
    fn ordinary_requests_are_untouched() {
        let url = "https://example.com/collect?v=2&tid=G-1&zipcode=02108";
        let body = r#"{"event":"page_view","shipping":"ground"}"#;
        let mut e = entry("POST", url, Some(body));
        network_entry(&mut e, false);
        assert_eq!(e.url, url);
        assert_eq!(e.post_data.as_deref(), Some(body));
    }

    #[test]
    fn takeover_records_no_bodies() {
        let mut e = entry(
            "POST",
            "https://example.com/api/thing?x=1",
            Some(r#"{"anything":"typed by the human"}"#),
        );
        network_entry(&mut e, true);
        assert_eq!(e.post_data, None);
        assert_eq!(e.url, "https://example.com/api/thing?x=1");
        assert_eq!(e.method, "POST");
        assert_eq!(e.status, Some(200));
    }
}
