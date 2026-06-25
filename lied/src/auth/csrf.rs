//! CSRF double-submit protection for the `/admin` HTMX tree (CLAUDE.md
//! "HTTP & REST API conventions": CSRF).
//!
//! Shape: a random token is generated per session and stored both (a) in
//! the session itself (the "session-bound" half) and (b) in a non-`HttpOnly`
//! cookie so client-side JS can read it. The admin UI's `htmx:configRequest`
//! handler (a later, UI-focused item) is expected to copy the cookie value
//! into an `HX-CSRF` request header on every state-changing request; this
//! middleware validates that the header matches the session-stored value.
//! `SameSite=Lax` on the session cookie is a backstop, not the primary
//! defense, per CLAUDE.md.
//!
//! Bearer-token `/v1` REST is exempt (not cookie-auth, so not CSRF-able) —
//! this middleware is only ever mounted on the `/admin` tree.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rand::RngCore;
use tower_sessions::Session;

use crate::state::AppState;

pub const CSRF_SESSION_KEY: &str = "csrf_token";
pub const CSRF_COOKIE_NAME: &str = "lied_csrf";
pub const CSRF_HEADER_NAME: &str = "hx-csrf";
/// Hidden form-field name carrying the token on server-rendered admin `<form>`
/// submits (the JS-free double-submit half).
pub const CSRF_FORM_FIELD: &str = "csrf_token";

/// Generate a fresh CSRF token (32 random bytes, base64url, no padding).
fn generate_token() -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Fetch the session's CSRF token, generating and persisting one if absent.
/// Called both by the middleware (to know what to compare against) and by
/// any handler that needs to embed the token in rendered HTML (e.g. a login
/// form) or set the readable cookie.
pub async fn ensure_token(session: &Session) -> Result<String, tower_sessions::session::Error> {
    if let Some(existing) = session.get::<String>(CSRF_SESSION_KEY).await? {
        return Ok(existing);
    }
    let token = generate_token();
    session.insert(CSRF_SESSION_KEY, token.clone()).await?;
    Ok(token)
}

/// Paths exempt from the double-submit header check even though they're
/// state-changing POSTs: `/login` happens *before* there's an authenticated
/// session to protect (the attack CSRF defends against — a forged
/// state-changing request riding the victim's cookies — presupposes the
/// victim is already logged in), and a plain server-rendered `<form>` POST
/// has no opportunity to run the `htmx:configRequest` JS hook that would
/// populate `HX-CSRF`. `/logout` is exempted for the same plain-form-post
/// reason; a forced logout is a low-value CSRF target (CLAUDE.md's "client
/// JS handler is later UI work" note covers the gap until the admin UI
/// switches these to HTMX-driven requests).
const CSRF_EXEMPT_PATHS: &[&str] = &["/login", "/logout"];

/// Axum middleware: for state-changing methods (`POST`/`PUT`/`PATCH`/
/// `DELETE`), require the `HX-CSRF` request header to match the session's
/// CSRF token. Mismatched or missing → 403. For safe methods
/// (`GET`/`HEAD`/`OPTIONS`) the request passes through, and — unless the
/// client is already presenting one — the response carries the readable
/// `lied_csrf` cookie (the second half of the double-submit pair), so a
/// later HTMX request can echo it back in `HX-CSRF`. Without this the
/// session would hold a token the client can never read.
pub async fn csrf_middleware(
    State(state): State<AppState>,
    session: Session,
    request: Request,
    next: Next,
) -> Response {
    let is_state_changing = matches!(
        request.method().as_str(),
        "POST" | "PUT" | "PATCH" | "DELETE"
    ) && !CSRF_EXEMPT_PATHS.contains(&request.uri().path());

    if is_state_changing {
        let expected = match session.get::<String>(CSRF_SESSION_KEY).await {
            Ok(Some(token)) => token,
            Ok(None) => {
                return csrf_rejection("no CSRF token established for this session");
            }
            Err(error) => {
                tracing::error!(%error, "failed to read CSRF token from session");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };

        // Accept either double-submit half:
        //  (a) the `HX-CSRF` request header (programmatic / future htmx use), or
        //  (b) a `csrf_token` field in an `application/x-www-form-urlencoded`
        //      body — the server-rendered admin `<form>`s. (b) is JS-free, so
        //      it works whether or not any client-side script runs (issue #15:
        //      the script-based header was never sent in practice).
        let header_ok = request
            .headers()
            .get(CSRF_HEADER_NAME)
            .and_then(|v| v.to_str().ok())
            == Some(expected.as_str());
        if header_ok {
            return next.run(request).await;
        }

        if is_form_urlencoded(&request) {
            // Buffer the (size-bounded) form body to read the token, then
            // rebuild the request so the handler's `Form` extractor still sees
            // the body. Only urlencoded bodies are buffered; multipart uploads
            // never arrive as one of these form posts, so streaming is
            // unaffected.
            let limit = state.config.max_request_bytes as usize;
            let (parts, body) = request.into_parts();
            let bytes = match axum::body::to_bytes(body, limit).await {
                Ok(bytes) => bytes,
                Err(_) => {
                    return csrf_rejection("request body unreadable or too large for CSRF check");
                }
            };
            let field_ok =
                form_field(&bytes, CSRF_FORM_FIELD).as_deref() == Some(expected.as_str());
            let request = Request::from_parts(parts, Body::from(bytes));
            if field_ok {
                return next.run(request).await;
            }
            return csrf_rejection("CSRF token mismatch or missing csrf_token field");
        }

        return csrf_rejection("CSRF token mismatch or missing HX-CSRF header / csrf_token field");
    }

    // Safe method: make sure the session carries a token, then hand the
    // client the readable cookie half (only when it isn't already presenting
    // one, to avoid a redundant `Set-Cookie` on every request).
    let already_presented = request_carries_csrf_cookie(&request);
    let token = match ensure_token(&session).await {
        Ok(token) => token,
        Err(error) => {
            tracing::error!(%error, "failed to ensure CSRF token");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let response = next.run(request).await;
    if already_presented {
        response
    } else {
        attach_csrf_cookie(response, &token, state.config.secure_cookies)
    }
}

/// Whether the incoming request already carries the readable `lied_csrf`
/// cookie, so the middleware can skip re-issuing it on every safe request.
fn request_carries_csrf_cookie(request: &Request) -> bool {
    let needle = format!("{CSRF_COOKIE_NAME}=");
    request
        .headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(';'))
        .any(|pair| pair.trim().starts_with(&needle))
}

/// Whether the request body is an `application/x-www-form-urlencoded` form
/// (the only body shape the CSRF middleware buffers to read its token field).
fn is_form_urlencoded(request: &Request) -> bool {
    request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| {
            ct.trim_start()
                .starts_with("application/x-www-form-urlencoded")
        })
        .unwrap_or(false)
}

/// Extract a single field value from an `application/x-www-form-urlencoded`
/// body. Returns the first match, percent/`+`-decoded.
fn form_field(body: &[u8], name: &str) -> Option<String> {
    let body = std::str::from_utf8(body).ok()?;
    body.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (urldecode(key) == name).then(|| urldecode(value))
    })
}

/// Minimal `application/x-www-form-urlencoded` value decoder: `+` → space and
/// `%XX` escapes. Sufficient for reading the CSRF field; avoids a new
/// dependency for one field.
fn urldecode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(
                    std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                    16,
                ) {
                    Ok(decoded) => {
                        out.push(decoded);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn csrf_rejection(detail: &str) -> Response {
    tracing::warn!(detail, "CSRF check failed");
    (StatusCode::FORBIDDEN, "CSRF token missing or invalid").into_response()
}

/// Build the readable (non-`HttpOnly`) CSRF cookie header value for a given
/// token, so a handler (e.g. the login page render, or login response) can
/// attach it alongside the session cookie. `secure` mirrors
/// `config.secure_cookies`.
pub fn csrf_cookie_header(token: &str, secure: bool) -> HeaderValue {
    let secure_attr = if secure { "; Secure" } else { "" };
    let value = format!("{CSRF_COOKIE_NAME}={token}; Path=/; SameSite=Lax{secure_attr}");
    HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// Helper used by handlers that issue a fresh CSRF token to attach it to a
/// response as both a readable cookie and an explicit body/header value the
/// caller can also choose to surface. Returns the unmodified response with
/// the cookie appended.
pub fn attach_csrf_cookie(mut response: Response<Body>, token: &str, secure: bool) -> Response {
    response.headers_mut().append(
        axum::http::header::SET_COOKIE,
        csrf_cookie_header(token, secure),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_token_produces_distinct_values() {
        assert_ne!(generate_token(), generate_token());
    }

    #[test]
    fn csrf_cookie_header_includes_secure_when_requested() {
        let header = csrf_cookie_header("abc", true);
        let value = header.to_str().unwrap();
        assert!(value.contains("Secure"));
        assert!(value.contains("SameSite=Lax"));
    }

    #[test]
    fn csrf_cookie_header_omits_secure_when_not_requested() {
        let header = csrf_cookie_header("abc", false);
        let value = header.to_str().unwrap();
        assert!(!value.contains("Secure"));
    }
}
