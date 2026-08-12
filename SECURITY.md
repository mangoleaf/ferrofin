# Security Policy

## Reporting a vulnerability

**Do not open a public issue for security vulnerabilities.**

Email **ken@mangoleafstudios.com** with:

- a description of the issue and its impact,
- steps to reproduce (a proof of concept if you have one),
- affected version(s) / commit.

You'll get an acknowledgement within a few days. Once a fix is available and
released, we'll credit you in the release notes unless you prefer to stay
anonymous.

Hermit speaks the Jellyfin HTTP API and handles authentication, media, and
transcoding — please treat auth bypass, path traversal, SSRF, and unauthenticated
data exposure as high priority.

## Supported versions

Hermit is pre-1.0 and moves fast. Only the **latest released version** receives
security fixes. Once maintenance branches (`release-x.y`) exist, this section will
list the supported window.
