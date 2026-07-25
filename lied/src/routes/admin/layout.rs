//! Shared HTML shell for `/admin` pages, built with `maud` (CLAUDE.md Admin
//! UI: "`maud` for templates"). This item (issue #5) is the first to render a
//! real page (vs. the hand-rolled login-page string in `routes::admin`), so
//! the page shell and the CSRF field helper live here once, for every later
//! admin screen to reuse.
//!
//! **CSRF**: every state-changing admin `<form>` embeds a hidden `csrf_token`
//! field ([`csrf_field`]); the [`crate::auth::csrf`] middleware validates it
//! server-side as the double-submit half. This is deliberately JS-free — it
//! works whether or not htmx loads in the browser, which an `HX-CSRF` header
//! set by client-side script does not (issue #15: the script-based hook never
//! fired in practice, so every write 403'd).

use maud::{html, Markup, PreEscaped, DOCTYPE};

/// A hidden `csrf_token` field for a server-rendered admin `<form>`. `token`
/// is the session's CSRF token (see [`crate::auth::csrf::ensure_token`]); the
/// CSRF middleware compares this field against the session value on every
/// state-changing submit. maud auto-escapes the value.
pub fn csrf_field(token: &str) -> Markup {
    html! { input type="hidden" name="csrf_token" value=(token); }
}

/// Shared page styles, inlined into every admin document's `<head>`.
const STYLES: &str = r#"
    body { font-family: sans-serif; margin: 2rem; color: #222; }
    nav a { margin-right: 1rem; }
    nav .current { margin-right: 1rem; font-weight: bold; }
    table { border-collapse: collapse; width: 100%; margin-top: 1rem; }
    th, td { border: 1px solid #ccc; padding: 0.4rem 0.6rem; text-align: left; }
    form.inline { display: inline; }
    .error { color: #b00020; }
    .muted { color: #666; }
    fieldset { margin-bottom: 1rem; }
"#;

/// Render a full `/admin` document: doctype, head (with the shared styles),
/// the caller-supplied `nav` bar, and the body content under an `h1(title)`.
/// Every admin surface — the system-admin screens ([`page`]) and the per-org
/// console (`routes::admin::console`) — shares this skeleton so styling and
/// structure stay in one place. `title` is auto-escaped by `maud`.
///
/// No JS framework: admin screens are plain server-rendered forms with a
/// hidden CSRF field (see [`csrf_field`]). htmx can return as a progressive
/// enhancement later, but the UI must not depend on it loading — CSRF
/// protection stays server-side.
pub fn document(title: &str, nav: Markup, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html {
            head {
                meta charset="utf-8";
                title { "Lied admin — " (title) }
                style { (PreEscaped(STYLES)) }
            }
            body {
                nav { (nav) }
                hr;
                h1 { (title) }
                (body)
            }
        }
    }
}

/// Render a full `/admin` page inside the instance-admin nav shell (Home /
/// Organizations / Users). Used by the system-admin provisioning screens; the
/// per-org console builds its own role-aware nav (see `routes::admin::console`).
pub fn page(title: &str, display_name: &str, body: Markup) -> Markup {
    let nav = html! {
        // No trailing slash — see the login redirect note; `/admin/`
        // is unrouted (issue #15, bug 2).
        a href="/admin" { "Home" }
        a href="/admin/orgs" { "Organizations" }
        a href="/admin/users" { "Users" }
        span { " — logged in as " (display_name) " — " }
        form class="inline" method="post" action="/admin/logout" {
            button type="submit" { "Log out" }
        }
    };
    document(title, nav, body)
}
