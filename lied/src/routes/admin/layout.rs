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

/// Render a full `/admin` page: doctype, head, and the given body content
/// inside a minimal nav shell. `title` is plain
/// text and is auto-escaped by `maud`'s `html!` macro like everything else
/// here — no `PreEscaped` needed for any caller-supplied string.
pub fn page(title: &str, display_name: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html {
            head {
                meta charset="utf-8";
                title { "Lied admin — " (title) }
                // No JS framework: admin screens are plain server-rendered
                // forms with a hidden CSRF field (see `csrf_field`). htmx can
                // return as a progressive enhancement later, but the UI must
                // not depend on it loading — CSRF protection stays server-side.
                style {
                    (PreEscaped(r#"
                        body { font-family: sans-serif; margin: 2rem; color: #222; }
                        nav a { margin-right: 1rem; }
                        table { border-collapse: collapse; width: 100%; margin-top: 1rem; }
                        th, td { border: 1px solid #ccc; padding: 0.4rem 0.6rem; text-align: left; }
                        form.inline { display: inline; }
                        .error { color: #b00020; }
                        fieldset { margin-bottom: 1rem; }
                    "#))
                }
            }
            body {
                nav {
                    // No trailing slash — see the login redirect note; `/admin/`
                    // is unrouted (issue #15, bug 2).
                    a href="/admin" { "Home" }
                    a href="/admin/orgs" { "Organizations" }
                    a href="/admin/users" { "Users" }
                    span { " — logged in as " (display_name) " — " }
                    form class="inline" method="post" action="/admin/logout" {
                        button type="submit" { "Log out" }
                    }
                }
                hr;
                h1 { (title) }
                (body)
            }
        }
    }
}
