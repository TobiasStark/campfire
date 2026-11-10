# Security policy

## Reporting a vulnerability

Please don't open a public issue for security problems. Use GitHub's private reporting instead:
[Report a vulnerability](https://github.com/TobiasStark/campfire/security/advisories/new).

Helpful to include: the Campfire version, what you did, what happened, and what you expected. A proof of concept
is welcome but not required.

I'll answer as soon as I can, usually within a few days, and will tell you when a fix is released. If you want to be
credited in the changelog, say so.

## Supported versions

Only the latest release gets fixes. Campfire is pre-1.0 and the protocol can change between versions (0.6.4 broke
compatibility with 0.6.3 and earlier), so please update before reporting.

## Scope

In scope: the Campfire app, its protocol ([docs/protocol.md](docs/protocol.md)) and the release/update process.

Out of scope: vulnerabilities in dependencies that don't affect Campfire, and attacks that require control of the
victim's PC.
