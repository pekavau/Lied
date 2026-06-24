//! Shared HTML shell for `/admin` pages, built with `maud` (CLAUDE.md Admin
//! UI: "HTMX for interactivity; server returns HTML fragments" + "`maud` for
//! templates"). This item (issue #5) is the first to render a real page
//! (vs. the hand-rolled login-page string in `routes::admin`), so the htmx
//! script tag and the CSRF wiring live here once, for every later admin
//! screen to reuse.
//!
//! **CSRF wiring**: [`crate::auth::csrf`]'s double-submit defense expects an
//! `HX-CSRF` header on every state-changing HTMX request, sourced from the
//! readable `lied_csrf` cookie. The inline script below registers exactly
//! that `htmx:configRequest` listener once per page load — this is the
//! "later, UI-focused item" the csrf module's doc comment refers to.

use maud::{html, Markup, PreEscaped, DOCTYPE};

/// The `htmx:configRequest` hook that copies the `lied_csrf` cookie into the
/// `HX-CSRF` header on every htmx-issued request. `PreEscaped`: this is a
/// fixed, hand-written script literal with no user input ever interpolated
/// into it, so auto-escaping would only mangle valid JS for no safety
/// benefit (CLAUDE.md: "any `PreEscaped` use needs a justification
/// comment").
fn csrf_script() -> Markup {
    PreEscaped(
        r#"
        document.addEventListener('htmx:configRequest', function (event) {
            var match = document.cookie.match(/(?:^|; )lied_csrf=([^;]*)/);
            if (match) {
                event.detail.headers['HX-CSRF'] = decodeURIComponent(match[1]);
            }
        });
        "#
        .to_string(),
    )
}

/// Render a full `/admin` page: doctype, head (htmx script + CSRF hook),
/// and the given body content inside a minimal nav shell. `title` is plain
/// text and is auto-escaped by `maud`'s `html!` macro like everything else
/// here — no `PreEscaped` needed for any caller-supplied string.
pub fn page(title: &str, display_name: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html {
            head {
                meta charset="utf-8";
                title { "Lied admin — " (title) }
                // Pinned version + SRI-less CDN load: acceptable for a
                // self-hosted admin tool with no public anonymous traffic;
                // a vendored copy is a reasonable phase-2 hardening step if
                // this ever needs to run offline.
                script src="https://unpkg.com/htmx.org@1.9.12" {}
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
                (csrf_script())
                nav {
                    a href="/admin/" { "Home" }
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
