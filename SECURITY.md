# Security Policy

## Supported versions

| Version | Supported |
|---------|-----------|
| 0.1.x   | ✅        |

Older versions do not receive security fixes.

## Reporting a vulnerability

Please do not open a public issue for security reports. Report privately by either:

- emailing **security@arcodash.com**, or
- using GitHub private vulnerability reporting: open the repository's **Security** tab and choose
  **Report a vulnerability**.

Please include:

- A description of the vulnerability and its potential impact.
- Steps to reproduce, or a proof of concept.
- The affected version or commit, and the component or connector involved.
- Any suggested fix or mitigation.

## What to expect

- Acknowledgement within 3 business days.
- Triage and an initial severity assessment within 7 days.
- Fix timelines depend on severity and complexity; we will keep you updated.

We ask for a reasonable window to fix an issue before public disclosure. Reporters are credited in
the release notes unless they prefer to stay anonymous.

## Notes for self-hosters

- Keep secrets out of version control. `.env` files hold real credentials; only the committed
  `*.example` templates belong in the repository.
- Replace every `CHANGE-ME` placeholder. Generate a strong `COOKIE_SECRET` and a 64-character hex
  `ENCRYPTION_KEY` (for example `openssl rand -hex 32`); the latter encrypts data source
  credentials and destination secrets at rest, so losing or rotating it makes stored credentials
  unreadable.
- Do not set `ARCODASH_ALLOW_PRIVATE_CONNECTORS=1` on a deployment reachable from untrusted
  networks.
- Only enable `REMOTE_USER_HEADER` behind a proxy that strips the header from client requests.
- Terminate TLS in front of the frontend and backend. See [docs/self-hosting.md](docs/self-hosting.md).
