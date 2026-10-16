# Contributing to Kelpie

Kelpie is a small fork of [herdr](https://github.com/herdrdev/herdr), run by one maintainer. Pull requests are welcome.

## Pull requests

Anyone can open a pull request. You do not need permission first.

I review every pull request myself. If it is good, I validate it and merge it. If it needs work, I say what. If it does not fit Kelpie, I close it and say why.

For a new feature or a change to how Kelpie behaves or looks, open a Discussion first. A short "would you take this?" saves you building something that will not be merged.

### What makes a pull request easy to merge

- **One problem per pull request.** No unrelated cleanup or refactors mixed in.
- **You understand the change.** Writing it with an AI agent is fine. You should still be able to explain what every line does and what the tests prove.
- **Tests.** A bug fix comes with a test that fails without the fix. Run `just ci` before opening the pull request; it must pass.
- **Windows and Mac matter.** Kelpie is built and tested on Windows 11 and macOS. Say which one you tested on.
- **A clear description.** What was wrong, what you changed, how you tested it. Screenshots for anything visible.
- **A conventional, lowercase title**, such as `fix: keep pane focus after detach`. Reference issues with `refs #123` in the body.

### Approved contributors

People whose pull requests have been merged and who know the codebase are listed in [`.github/APPROVED_CONTRIBUTORS`](.github/APPROVED_CONTRIBUTORS). Their pull requests are reviewed first, and they may be asked to review others. Being listed is a thank-you, not a requirement and not maintainer access. You do not need to apply; I add people as their work comes in.

Maintainers are listed in [`.github/MAINTAINERS`](.github/MAINTAINERS).

## Bugs

Open an issue with the bug template. Include what happened, what you expected, the shortest way to reproduce it, your Kelpie version (`kelpie --version`), operating system and terminal. A short log excerpt helps. Search existing issues first.

## Ideas and questions

Use GitHub Discussions.

## Bugs that also affect herdr

Most of Kelpie's code comes from herdr. If a bug also exists in herdr, say so in the issue or pull request.

## Development setup

```bash
just install-hooks   # once
just ci              # before every pull request
```

Requirements: Rust (the version in `rust-toolchain.toml`) and Zig 0.16.0.

For changes that affect users, update the draft under `docs/next/` rather than the root `README.md` or `CHANGELOG.md`; those are updated at release time.

## License

By contributing, you agree that your contribution is licensed under the Apache License 2.0, the same license as Kelpie and herdr.
