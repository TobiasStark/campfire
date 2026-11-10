# Contributing to Campfire

Thanks for your interest in Campfire! Bug reports, fixes, documentation improvements and new ideas are all welcome.

Campfire is a Windows voice chat app built on peer-to-peer connections, with no central server. Keep that in mind when proposing changes: anything that would require a server, an account or automatic network access to third parties is unlikely to fit.

## Before you start

- **Bugs and small fixes:** open a pull request directly, or file an issue first if you are unsure.
- **New features or larger changes:** please open an issue to discuss the idea first. This avoids work on something that does not fit the project.
- **Questions:** use an issue as well.
- **Security problems:** do not open a public issue. See [Reporting security issues](#reporting-security-issues).

## Development setup

Prerequisites, ffmpeg setup and packaging are described in [docs/building.md](docs/building.md). In short:

```sh
cargo build --release -p app-desktop --bin campfire
```

The Rust toolchain is pinned in [rust-toolchain.toml](rust-toolchain.toml) and is picked up automatically by `rustup`. The repository layout is listed in the [README](README.md#technology).

## Checks before opening a PR

CI runs the following; please run them locally first:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

If you touch video code (`app-video`, or video features in `app-desktop`), also run the checks with the ffmpeg feature enabled:

```sh
cargo clippy --workspace --all-targets --features app-desktop/video,app-video/ffmpeg -- -D warnings
cargo test -p app-video -p app-desktop --features app-desktop/video,app-video/ffmpeg
```

If you add or update dependencies, run `cargo deny check` (configuration in [deny.toml](deny.toml)). New dependencies should be justified in the PR description; prefer fewer and well-maintained ones.

## Code guidelines

- Write code, comments, commit messages and PR descriptions in English.
- Follow `rustfmt` and fix all `clippy` warnings; CI treats warnings as errors.
- Add tests for new behavior and bug fixes. Helpers for network tests live in `crates/app-testkit`.
- Keep changes focused. Unrelated refactoring belongs in a separate PR.

### Protocol changes

The wire protocol lives in `crates/app-proto` and is documented in [docs/protocol.md](docs/protocol.md). A change to the protocol must update that document, and the PR must say whether it breaks compatibility with older versions.

### Security-sensitive changes

Campfire handles data from other people's machines, so please be conservative:

- Nothing from other users is downloaded, opened or executed automatically.
- Updates are only installed after explicit user confirmation.
- Respect the existing size and storage limits.
- Treat all data received from peers as untrusted and validate it.

If your change affects any of this, explain the reasoning in the PR.

## Commits and pull requests

- Branch from `main` and open the PR against `main`.
- One topic per PR; keep the diff small and reviewable.
- Write commit messages in the imperative mood with a short summary line, for example `fix chat not refreshing after delivery confirmation`. A prefix like `fix:`, `feat:`, `docs:` or `build(deps):` is welcome.
- In the PR description, explain **what** changed, **why**, and **how you tested it**. For UI changes, include a screenshot.
- Changes visible to users are noted in [CHANGELOG.md](CHANGELOG.md). Feel free to propose an entry in your PR; the maintainer adds it at release time otherwise.

## Reporting security issues

Please report vulnerabilities privately through GitHub's
[security advisories](https://github.com/TobiasStark/campfire/security/advisories/new) instead of a public issue. Include steps to reproduce and the affected version if you can.

## License

Campfire is dual-licensed under [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option. Unless you explicitly state otherwise, any contribution you submit for inclusion in this project is licensed under the same dual license, without additional terms or conditions.

## Conduct

Be respectful and constructive. Harassment and personal attacks are not tolerated, and the maintainer may moderate or close discussions that go that way.
