# nixpkgs

`package.nix` is the package for a nixpkgs submission, at
`pkgs/by-name/ha/hatchzip/package.nix`. It builds the command line tool from
the release tag without the `rar` feature, so the build is MIT/Apache-2.0
throughout, adds the `hz` short name, completions and the man page, and runs
the test suite as a debug build, since the tests lower limits through
environment variables that only debug builds read (`test_knob` in
`crates/core/src/engine.rs`).

Built and tested on x86_64-linux at v0.1.8 (2026-10-30): every test binary
passed, and `hatchzip --version`, `hz --version` and a real extraction ran
from the output.

Before submitting:

1. nixpkgs needs a maintainer for a new package: add yourself to
   `maintainers/maintainer-list.nix` in a first commit (`maintainers: add
   xevrion`), then set `maintainers = with lib.maintainers; [ xevrion ];`.
2. Read nixpkgs' CONTRIBUTING, including its policy on disclosing automated
   or AI assistance in commits.
3. Commit as `hatchzip: init at <version>` and open the PR with their
   template.

For a new version, update `version`, then the `hash` and `cargoHash` from
the mismatch nix reports on the first build (or run `nix-update hatchzip`).
