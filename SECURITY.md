# Security Policy

Lied handles authentication, per-organization access control, and file storage,
so security reports are taken seriously — thank you for helping keep
self-hosters safe.

## Supported versions

Lied is pre-1.0 and under active development. Security fixes are applied to the
`main` branch; there are no long-term support branches yet. If you're running
Lied, track `main` (or tagged releases once they exist) for fixes.

## Reporting a vulnerability

**Please do not report security vulnerabilities through public GitHub issues,
pull requests, or discussions.**

Instead, report privately via GitHub's
[private vulnerability reporting](https://github.com/pekavau/Lied/security/advisories/new)
("Report a vulnerability" under the repository's **Security** tab). If that is
unavailable to you, contact the maintainer directly at **pekavau@gmail.com**.

Please include:

- a description of the issue and its impact,
- steps to reproduce (a proof of concept if you have one),
- affected component (WebDAV, REST `/v1`, admin UI, auth, storage, …) and any
  relevant configuration, and
- the commit or version you tested against.

You can expect an acknowledgement within a few days. We'll work with you to
understand and validate the issue, prepare a fix, and coordinate disclosure —
please give us a reasonable window to release a fix before any public
disclosure. We're happy to credit you in the advisory unless you'd prefer to
remain anonymous.

## Scope notes

A few things are known and by-design for the current phase, rather than
vulnerabilities:

- **Tenant isolation between *unrelated* organizations is not yet hardened.**
  Phase 1 assumes every organization sharing an instance trusts the others
  (single-org, allied-federation, and personal self-hosting). Strict
  multi-tenant SaaS isolation is explicitly out of scope for now — see the
  *Deployment topologies* section of [`CLAUDE.md`](CLAUDE.md).
- The bundled `.env.example` contains **development-only placeholder
  credentials**. Any real deployment must set its own secrets (ideally via the
  `_FILE` / external-command resolvers described in `CLAUDE.md`).
- `LIED_SECURE_COOKIES` defaults to `false` for plain-HTTP local use; set it to
  `true` behind HTTPS.

Reports that amount to "the documented placeholder secret is insecure" or "an
org admin can affect their own org" are not considered vulnerabilities, but
genuine cross-boundary access issues absolutely are — when in doubt, report it.
