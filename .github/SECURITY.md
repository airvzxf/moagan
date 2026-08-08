# Security policy — moagan

> Note: when this file lives at `.github/SECURITY.md` GitHub exposes a
> "Report a vulnerability" button on the repo's Security tab. The same
> content is reaching the public docs site (`docs/SECURITY.md`) so
> security researchers don't need a GitHub account to find the policy.

## Supported versions

| Version | Supported |
|---|---|
| `main` (unreleased) | ✅ Patches shipped as they land. |
| Latest tagged release (≥ `v0.5.0`) | ✅ Patches and security backports. |
| `v0.4.x` and earlier | ❌ EOL. Please upgrade. |

The AGPL-3.0 license means consumers can always fork the latest commit
and self-patch; the table above is about *our* commitment to ship fixes.

## Reporting a vulnerability

**Email (preferred):** `israel.alberto.rv@gmail.com`
— please prefix the subject with `[moagan security]`.

**GitHub Security Advisories:** use the
"Report a vulnerability" button on the
[Security tab](https://github.com/airvzxf/moagan/security/advisories/new).
This is the private channel; no public issue is created.

**What to include:**

- A description of the vulnerability and its impact.
- A reproducer (commands, prompt, configuration) — or a failing test.
- The affected commit SHA or tag.
- Whether you intend to publish a coordinated disclosure.

## Triage SLA

| Stage | Target |
|---|---|
| First acknowledgement | 72 h |
| Triage (confirmed / won't fix / out of scope) | 7 d |
| Fix for critical / high severity | ≤ 30 d |
| Public disclosure *after* a fix is shipped | coordinated |

We credit the reporter in the release notes unless they ask to stay
anonymous.

## Out of scope

- Vulnerabilities in *upstream* crates we don't own (`tokio`, `reqwest`,
  `rusqlite`, etc.). Report those to the upstream maintainer. We add
  the affected transitives to our `cargo-audit` workflow as soon as we
  learn about them.
- Vulnerabilities in the LLM provider (MiniMax). Contact the provider
  directly. The `MOAGAN_PROVIDER` abstraction should never blind-trust
  upstream responses; if you find a path where it does, **that** is a
  moagan bug and is in scope.
- Misconfiguration that ships secrets to git history or logs (always
  rotate the secret first, then report).
- Denial-of-service against the developer's own local SQLite store.

## What we do *not* do

- We do not request **demo credentials** or connections to any
  production LLM tenant.
- We do not publish a CVE before a fix is available unless disclosure
  is forced (e.g. the report is already public).
- We do not retroactively re-sign tags. Releases shipped before this
  policy was published are signed with the same key that signs new
  commits (414687A3CD7E65B9) but were not created with this triage
  SLA in mind.
