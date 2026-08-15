//! `/admin/orgs/{org}/…` — the per-organization management console (Phase 2,
//! issue #30). This is the *foundation* the later console items (#31–#36) hang
//! off: it establishes how a member enters an org's workspace, the
//! role-aware navigation shell, the permission-matrix gating primitive
//! ([`ConsoleCtx`] + its capability helpers), and the principal carve-out.
//! The section routes here are gated **stubs** — later items replace their
//! bodies with real screens.
//!
//! **Org scoping is stateless / in the URL** (`/admin/orgs/{org}/…`), reusing
//! the same `:org_id` path param and `require_org_role`-style authorization
//! the phase-1 `/admin` and `/v1` trees already use — no session-stored
//! "active org".
//!
//! **Authorization** mirrors the CLAUDE.md permission matrix. Access to the
//! console at all requires either a staff role (`owner`/`archivist`/
//! `conductor`), `is_system_admin` (owner-equivalent, matching
//! [`crate::auth::authz::require_org_role`]), or the **principal carve-out**:
//! a `musician` with `is_principal = true` may enter, but only the read-only
//! section-coverage view. A plain `musician` and non-members are denied.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use maud::{html, Markup};
use uuid::Uuid;

use crate::auth::extractors::AuthSession;
use crate::domain::membership::{self, Role};
use crate::domain::organization::{self, Organization};
use crate::domain::user::User;
use crate::error::AppError;
use crate::routes::admin::layout;
use crate::state::AppState;

/// The permission facts a console request is authorized against, independent
/// of which user/org row they came from. Split out from [`ConsoleCtx`] so the
/// permission-matrix logic is a pure function of `(role, is_principal,
/// is_system_admin)` and can be unit-tested without a database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Access {
    /// The caller's role in the org, or `None` when access is granted purely
    /// by `is_system_admin` (a system admin with no membership in this org).
    role: Option<Role>,
    is_principal: bool,
    is_system_admin: bool,
}

impl Access {
    /// Whether this caller may enter the console for the org at all. Staff
    /// roles and system admins always may; a `musician` may only if they are
    /// a principal (the carve-out); everyone else may not.
    fn may_enter(role: Role, is_principal: bool) -> bool {
        match role {
            Role::Owner | Role::Archivist | Role::Conductor => true,
            Role::Musician => is_principal,
        }
    }

    /// Apply a role-capability predicate, with `is_system_admin` short-
    /// circuiting to owner-equivalent (matching `require_org_role`). The
    /// predicates themselves live on [`Role`] (the single source of truth for
    /// the permission matrix), so `/admin` and `/v1` can't drift.
    fn role_can(&self, cap: fn(Role) -> bool) -> bool {
        self.is_system_admin || self.role.is_some_and(cap)
    }

    fn is_staff(&self) -> bool {
        self.role_can(Role::is_staff)
    }

    fn can_edit_arrangements(&self) -> bool {
        self.role_can(Role::can_edit_arrangements)
    }

    fn can_build_collections(&self) -> bool {
        self.role_can(Role::can_build_collections)
    }

    fn can_manage_members(&self) -> bool {
        self.role_can(Role::can_manage_members)
    }

    fn can_author_global_annotations(&self) -> bool {
        self.role_can(Role::can_author_global_annotations)
    }

    /// Coverage is the one section a principal (non-staff) may see — the
    /// carve-out — in addition to all staff.
    fn can_view_coverage(&self) -> bool {
        self.is_staff() || self.is_principal
    }

    /// The nav sections this caller may see, in display order. `Home` (the
    /// workspace landing) is always present for anyone with console access.
    fn visible_sections(&self) -> Vec<Section> {
        Section::ALL
            .iter()
            .copied()
            .filter(|s| s.is_visible_to(self))
            .collect()
    }
}

/// A resolved console request: the authenticated user, the organization they
/// are operating in, and their [`Access`] within it. Handlers obtain one via
/// [`ConsoleCtx::load`], then gate on the capability helpers before rendering.
pub struct ConsoleCtx {
    user: User,
    org: Organization,
    access: Access,
}

impl ConsoleCtx {
    /// Resolve the console context for `user` operating on org `org_id`:
    ///
    /// - `404` if the org does not exist.
    /// - `is_system_admin` enters as owner-equivalent (no membership needed).
    /// - otherwise the caller's membership decides: staff and principals may
    ///   enter; a plain `musician` and non-members get `403`.
    pub async fn load(state: &AppState, user: &User, org_id: Uuid) -> Result<ConsoleCtx, AppError> {
        // A system admin enters any org as owner-equivalent and needs no
        // membership row, so skip that query entirely for them.
        if user.is_system_admin {
            let org = organization::find_by_id(&state.db, org_id)
                .await?
                .ok_or(AppError::NotFound)?;
            return Ok(ConsoleCtx {
                user: user.clone(),
                org,
                access: Access {
                    role: None,
                    is_principal: false,
                    is_system_admin: true,
                },
            });
        }

        // Members: the org and the membership are independent lookups, so run
        // them concurrently rather than back-to-back.
        let (org, membership) = tokio::try_join!(
            organization::find_by_id(&state.db, org_id),
            membership::find_by_user_and_org(&state.db, user.id, org_id),
        )?;
        let org = org.ok_or(AppError::NotFound)?;
        let Some(m) = membership else {
            return Err(AppError::Forbidden);
        };
        if !Access::may_enter(m.role, m.is_principal) {
            return Err(AppError::Forbidden);
        }

        Ok(ConsoleCtx {
            user: user.clone(),
            org,
            access: Access {
                role: Some(m.role),
                is_principal: m.is_principal,
                is_system_admin: false,
            },
        })
    }

    pub fn is_staff(&self) -> bool {
        self.access.is_staff()
    }
    pub fn can_edit_arrangements(&self) -> bool {
        self.access.can_edit_arrangements()
    }
    pub fn can_build_collections(&self) -> bool {
        self.access.can_build_collections()
    }
    pub fn can_manage_members(&self) -> bool {
        self.access.can_manage_members()
    }
    pub fn can_author_global_annotations(&self) -> bool {
        self.access.can_author_global_annotations()
    }
    pub fn can_view_coverage(&self) -> bool {
        self.access.can_view_coverage()
    }

    fn role_label(&self) -> String {
        if self.access.is_system_admin {
            "system admin".to_string()
        } else {
            self.access
                .role
                .map(|r| r.as_str().to_string())
                .unwrap_or_default()
        }
    }

    /// The organization this console request is scoped to.
    pub fn org(&self) -> &Organization {
        &self.org
    }

    /// The authenticated user.
    pub fn user(&self) -> &User {
        &self.user
    }
}

/// Resolve a section handler's session and console context in one step:
/// returns the ready-to-send response (login redirect, 403, or 404 page) on
/// failure, so section modules (`arrangements`, `tags`, …) share the exact
/// same entry gate as the foundation's own handlers.
pub(crate) async fn enter(
    state: &AppState,
    auth: Option<AuthSession>,
    org_id: Uuid,
) -> Result<ConsoleCtx, Response> {
    let user = require_login(auth).map_err(IntoResponse::into_response)?;
    ConsoleCtx::load(state, &user, org_id)
        .await
        .map_err(load_error_response)
}

/// A console navigation section. Each maps to a route under the org prefix and
/// carries the capability that makes it visible in the nav.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Home,
    Arrangements,
    Collections,
    Coverage,
    Annotations,
    Search,
    Members,
}

impl Section {
    /// Display order for the nav.
    const ALL: [Section; 7] = [
        Section::Home,
        Section::Arrangements,
        Section::Collections,
        Section::Coverage,
        Section::Annotations,
        Section::Search,
        Section::Members,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::Home => "Home",
            Section::Arrangements => "Arrangements",
            Section::Collections => "Collections",
            Section::Coverage => "Coverage",
            Section::Annotations => "Global annotations",
            Section::Search => "Search",
            Section::Members => "Members",
        }
    }

    /// Path segment under `/admin/orgs/{org}/`.
    fn segment(self) -> &'static str {
        match self {
            Section::Home => "console",
            Section::Arrangements => "arrangements",
            Section::Collections => "collections",
            Section::Coverage => "coverage",
            Section::Annotations => "annotations",
            Section::Search => "search",
            Section::Members => "members",
        }
    }

    fn path(self, org_id: Uuid) -> String {
        format!("/admin/orgs/{org_id}/{}", self.segment())
    }

    /// The issue that replaces this stub with a real screen (shown in the
    /// placeholder body so the foundation documents its own follow-ups).
    fn follow_up(self) -> Option<&'static str> {
        match self {
            Section::Home => None,
            Section::Arrangements => Some("#31 / #32"),
            Section::Collections => Some("#33"),
            Section::Coverage => Some("#35"),
            Section::Annotations => Some("#36"),
            Section::Search => Some("#34"),
            // Members is a real screen (admin::members), not a stub.
            Section::Members => None,
        }
    }

    /// Whether `access` may see this section in the nav — and, since the
    /// section handlers gate on this same predicate, whether they may open it
    /// at all. One source of truth for both the nav and the route gate.
    /// `Home` is always visible to anyone with console access.
    fn is_visible_to(self, access: &Access) -> bool {
        match self {
            Section::Home => true,
            Section::Arrangements | Section::Search => access.is_staff(),
            Section::Collections => access.can_build_collections(),
            Section::Coverage => access.can_view_coverage(),
            Section::Annotations => access.can_author_global_annotations(),
            Section::Members => access.can_manage_members(),
        }
    }

    /// Whether this section is read-only for `access`: the arrangements screen
    /// is read-only for a conductor (view-only per the matrix), and coverage
    /// is read-only for everyone. Drives the read-only note on the stub.
    fn is_read_only_for(self, access: &Access) -> bool {
        match self {
            Section::Arrangements => !access.can_edit_arrangements(),
            Section::Coverage => true,
            _ => false,
        }
    }
}

/// Whether a member with `role`/`is_principal` may enter the console at all —
/// exposed so the `/admin` landing lists only workspaces the caller can
/// actually open (it must agree with [`ConsoleCtx::load`], which uses the same
/// rule). System admins enter every org regardless and are handled separately.
pub fn can_enter(role: Role, is_principal: bool) -> bool {
    Access::may_enter(role, is_principal)
}

pub fn router() -> Router<AppState> {
    // Arrangements, Collections and Search are real screens merged separately
    // in `admin::router`; the rest are still gated stubs.
    Router::new()
        .route("/orgs/:org_id/console", get(home_page))
        .route("/orgs/:org_id/coverage", get(coverage_stub))
        .route("/orgs/:org_id/annotations", get(annotations_stub))
}

/// Render a full console page inside the org's role-aware nav shell, with
/// `active` highlighted and only the caller's permitted sections shown.
pub(crate) fn console_page(ctx: &ConsoleCtx, active: Section, body: Markup) -> Markup {
    let nav = html! {
        a href="/admin" { "← Workspaces" }
        span class="muted" { (ctx.org.name) " · " (ctx.role_label()) }
        " — "
        @for section in ctx.access.visible_sections() {
            @if section == active {
                span class="current" { (section.label()) }
            } @else {
                a href=(section.path(ctx.org.id)) { (section.label()) }
            }
        }
        " — "
        span class="muted" { (ctx.user.display_name) }
        " "
        form class="inline" method="post" action="/admin/logout" {
            button type="submit" { "Log out" }
        }
    };
    let title = format!("{} — {}", ctx.org.name, active.label());
    layout::document(&title, nav, body)
}

/// A stub body for a section whose real screen lands in a later issue.
fn stub_body(section: Section, read_only: bool) -> Markup {
    html! {
        p {
            "This section is part of the Phase 2 console foundation. Its screen "
            @if let Some(issue) = section.follow_up() {
                "arrives in issue " (issue) "."
            } @else {
                "is being built."
            }
        }
        @if read_only {
            p class="muted" {
                "You have read-only access to this section — you can view it "
                "but not make changes."
            }
        }
    }
}

/// Resolve the session for a console handler, redirecting to the login page
/// (rather than emitting a raw `401` Problem Details body into the browser)
/// when there is none — matching the `/admin` tree's HTML-response convention.
fn require_login(auth: Option<AuthSession>) -> Result<User, Redirect> {
    match auth {
        Some(AuthSession(user)) => Ok(user),
        None => Err(Redirect::to("/admin/login")),
    }
}

/// The shared error-message body for the console's full-page error responses.
fn error_body(message: &str) -> Markup {
    html! { p class="error" { (message) } }
}

/// Render a full-page error inside a given section's shell, at `status`. The
/// one error-page primitive every section module (`arrangements`, `tags`,
/// `members`, …) shares — the `section` keeps the nav highlight correct.
pub(crate) fn error_page(
    ctx: &ConsoleCtx,
    section: Section,
    status: StatusCode,
    message: &str,
) -> Response {
    (
        status,
        Html(console_page(ctx, section, error_body(message)).into_string()),
    )
        .into_response()
}

/// The stale-write retry page (`412`): the row changed since the form loaded.
/// `entity` names the thing ("arrangement", "voice", "member", "tag") and
/// `back_url` links back to *that* entity — shared so every editable screen
/// reports the right thing (not always "arrangement").
pub(crate) fn precondition_page(
    ctx: &ConsoleCtx,
    section: Section,
    entity: &str,
    back_url: &str,
) -> Response {
    let body = html! {
        p class="error" {
            "This " (entity) " was changed by someone else since you opened the "
            "form. Your edit was not saved — reload and try again."
        }
        p { a href=(back_url) { "Reload the " (entity) } }
    };
    (
        StatusCode::PRECONDITION_FAILED,
        Html(console_page(ctx, section, body).into_string()),
    )
        .into_response()
}

/// Render a full-page access-denied / not-found response in the admin styling,
/// with a link back to the workspace list. Used when [`ConsoleCtx::load`]
/// rejects the caller (before a console shell is available).
fn deny_response(status: StatusCode, heading: &str, message: &str) -> Response {
    let nav = html! { a href="/admin" { "← Workspaces" } };
    let markup = layout::document(heading, nav, error_body(message));
    (status, Html(markup.into_string())).into_response()
}

/// Map a [`ConsoleCtx::load`] error to a full-page response. `403` for no
/// console access, `404` for a missing org, `500` otherwise.
fn load_error_response(err: AppError) -> Response {
    match err {
        AppError::NotFound => deny_response(
            StatusCode::NOT_FOUND,
            "Not found",
            "That organization does not exist.",
        ),
        AppError::Forbidden => deny_response(
            StatusCode::FORBIDDEN,
            "No access",
            "You do not have access to this organization's console.",
        ),
        other => {
            tracing::error!(error = %other, "console context load failed");
            deny_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Error",
                "Something went wrong loading this workspace.",
            )
        }
    }
}

/// A 403 within a workspace the caller *can* enter, but a section they may
/// not (e.g. an archivist reaching global annotations, or a principal
/// reaching arrangements). Rendered inside the console shell so the nav stays
/// available.
pub(crate) fn section_forbidden(ctx: &ConsoleCtx) -> Response {
    let body = error_body("You do not have permission to view this section.");
    (
        StatusCode::FORBIDDEN,
        Html(console_page(ctx, Section::Home, body).into_string()),
    )
        .into_response()
}

async fn home_page(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path(org_id): Path<Uuid>,
) -> Response {
    let user = match require_login(auth) {
        Ok(user) => user,
        Err(redirect) => return redirect.into_response(),
    };
    let ctx = match ConsoleCtx::load(&state, &user, org_id).await {
        Ok(ctx) => ctx,
        Err(err) => return load_error_response(err),
    };

    let sections: Vec<Section> = ctx
        .access
        .visible_sections()
        .into_iter()
        .filter(|s| *s != Section::Home)
        .collect();

    let body = html! {
        p { "Welcome to the " (ctx.org.name) " workspace. Choose a section:" }
        ul {
            @for section in &sections {
                li { a href=(section.path(ctx.org.id)) { (section.label()) } }
            }
        }
        @if sections.is_empty() {
            p class="muted" { "No sections are available to your role yet." }
        }
    };
    Html(console_page(&ctx, Section::Home, body).into_string()).into_response()
}

/// Build a gated section-stub handler. Visibility *is* the gate — the handler
/// admits exactly the callers the nav would show a link to
/// ([`Section::is_visible_to`]), so nav and gate can't disagree — and the
/// read-only note comes from the same [`Section::is_read_only_for`] the stub
/// body uses. Each section's policy therefore lives only on `Section`.
macro_rules! section_handler {
    ($name:ident, $section:expr) => {
        async fn $name(
            State(state): State<AppState>,
            auth: Option<AuthSession>,
            Path(org_id): Path<Uuid>,
        ) -> Response {
            let user = match require_login(auth) {
                Ok(user) => user,
                Err(redirect) => return redirect.into_response(),
            };
            let ctx = match ConsoleCtx::load(&state, &user, org_id).await {
                Ok(ctx) => ctx,
                Err(err) => return load_error_response(err),
            };
            if !$section.is_visible_to(&ctx.access) {
                return section_forbidden(&ctx);
            }
            let body = stub_body($section, $section.is_read_only_for(&ctx.access));
            Html(console_page(&ctx, $section, body).into_string()).into_response()
        }
    };
}

section_handler!(coverage_stub, Section::Coverage);
section_handler!(annotations_stub, Section::Annotations);

#[cfg(test)]
mod tests {
    use super::*;

    fn access(role: Option<Role>, is_principal: bool, is_system_admin: bool) -> Access {
        Access {
            role,
            is_principal,
            is_system_admin,
        }
    }

    fn member(role: Role) -> Access {
        access(Some(role), false, false)
    }

    #[test]
    fn may_enter_matches_the_carve_out() {
        assert!(Access::may_enter(Role::Owner, false));
        assert!(Access::may_enter(Role::Archivist, false));
        assert!(Access::may_enter(Role::Conductor, false));
        // The carve-out: a plain musician cannot enter, a principal can.
        assert!(!Access::may_enter(Role::Musician, false));
        assert!(Access::may_enter(Role::Musician, true));
    }

    #[test]
    fn owner_has_every_capability() {
        let a = member(Role::Owner);
        assert!(a.is_staff());
        assert!(a.can_edit_arrangements());
        assert!(a.can_build_collections());
        assert!(a.can_manage_members());
        assert!(a.can_author_global_annotations());
        assert!(a.can_view_coverage());
    }

    #[test]
    fn archivist_edits_content_but_not_members_or_annotations() {
        let a = member(Role::Archivist);
        assert!(a.is_staff());
        assert!(a.can_edit_arrangements());
        assert!(a.can_build_collections());
        assert!(!a.can_manage_members());
        assert!(!a.can_author_global_annotations());
        assert!(a.can_view_coverage());
    }

    #[test]
    fn conductor_plans_and_annotates_but_cannot_edit_arrangements_or_members() {
        let a = member(Role::Conductor);
        assert!(a.is_staff());
        assert!(!a.can_edit_arrangements());
        assert!(a.can_build_collections());
        assert!(!a.can_manage_members());
        assert!(a.can_author_global_annotations());
        assert!(a.can_view_coverage());
    }

    #[test]
    fn principal_sees_only_coverage() {
        let a = access(Some(Role::Musician), true, false);
        assert!(!a.is_staff());
        assert!(!a.can_edit_arrangements());
        assert!(!a.can_build_collections());
        assert!(!a.can_manage_members());
        assert!(!a.can_author_global_annotations());
        assert!(a.can_view_coverage());
        assert_eq!(a.visible_sections(), vec![Section::Home, Section::Coverage]);
    }

    #[test]
    fn system_admin_is_owner_equivalent() {
        let a = access(None, false, true);
        assert!(a.is_staff());
        assert!(a.can_edit_arrangements());
        assert!(a.can_manage_members());
        assert!(a.can_author_global_annotations());
        assert!(a.can_view_coverage());
    }

    #[test]
    fn visible_sections_reflect_the_matrix() {
        assert_eq!(
            member(Role::Owner).visible_sections(),
            vec![
                Section::Home,
                Section::Arrangements,
                Section::Collections,
                Section::Coverage,
                Section::Annotations,
                Section::Search,
                Section::Members,
            ]
        );
        // Archivist: everything staff except global annotations and members.
        assert_eq!(
            member(Role::Archivist).visible_sections(),
            vec![
                Section::Home,
                Section::Arrangements,
                Section::Collections,
                Section::Coverage,
                Section::Search,
            ]
        );
        // Conductor: staff sections including annotations.
        assert_eq!(
            member(Role::Conductor).visible_sections(),
            vec![
                Section::Home,
                Section::Arrangements,
                Section::Collections,
                Section::Coverage,
                Section::Annotations,
                Section::Search,
            ]
        );
    }
}
