# Contributing to Gage

Thank you for your interest in contributing. This document describes how
contributions are accepted and what is in scope.

## Scope

Contributions are accepted to the free Gage platform: the engine, the core
scanners, and the scanner SDK and format specification, all licensed under
Apache-2.0.

Proprietary commercial scanners and extensions are owned first-party and are
not accepted as open contributions.

## Reporting bugs

Open an issue describing the problem, what you expected, and what happened.
Include the Gage version (`gage --version`), the platform, and a minimal
reproduction when possible.

For security issues, do not open a public issue. Email garrett@gageml.com
instead.

## Pull requests

- Branch from `main` and open the pull request against `main`.
- Keep each commit a single logical change.
- Ensure `cargo clippy` passes with no warnings and `cargo fmt` has been run.
  All tests must pass. The Rust style rules in `CLAUDE.md` apply.

A maintainer will review the pull request. Nothing is set in stone: review may
request changes, and a contribution does not need to be perfect on first
submission.

## AI-assisted contributions

AI tools are used extensively in this project and contributions produced with
their assistance are welcome. The requirement is that a human contributor has
reviewed the contribution and takes responsibility for it under their own
GitHub identity.

The contributor is responsible for understanding the change well enough to
defend it in review and to be confident the work does not contain code copied
from sources the contributor does not have the right to relicense.
Contributions from accounts that identify as bots, agents, or autonomous
systems will not be accepted.

There is no requirement to disclose AI assistance and no separate process for
AI-assisted contributions.

## License

By contributing, you agree that your contributions are licensed under the
Apache License 2.0 (see [LICENSE.txt](LICENSE.txt)).
