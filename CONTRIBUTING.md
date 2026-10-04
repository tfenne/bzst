# Contributing to bzst

Thanks for your interest in bzst. The repository holds two things that evolve together: the format specification (`spec/`) and the Rust reference implementation (`rust/`). This guide covers how changes to each are made and checked.

## Discuss format changes first

Anything that changes the wire format (a field, a frame layout, a checksum, a validation rule) should start as an issue or a draft PR, so the design can be discussed before it is implemented. Implementation-only changes such as bug fixes, performance work or API improvements can go straight to a PR.

## The spec and the implementation change together

The spec and the reference implementation must agree, so a PR that changes the format updates both:

1. Edit `spec/bzst.typ`, including its open-issues and resolved-decisions sections where relevant.
2. Regenerate the PDF with `typst compile spec/bzst.typ` from the repository root, and commit `spec/bzst.pdf` alongside the source. CI checks that the spec compiles but doesn't diff the PDF, which embeds its build date.
3. Update the Rust implementation and its tests to match.

An implementation-only PR shouldn't need spec changes. If it turns out to, the implementation and the spec disagreed, and the PR should say which one was right.

## Getting started

Prerequisites:

- Rust via [rustup](https://rustup.rs). `rust/rust-toolchain.toml` pins the toolchain, including rustfmt and clippy, and rustup installs it the first time you run `cargo` in `rust/`.
- The `zstd` command-line tool, for the interoperability tests. They skip themselves when it's missing; CI installs it so they always run.
- [Typst](https://typst.app) 0.15, to render the spec.
- [cargo-deny](https://embarkstudios.github.io/cargo-deny/), optional locally; CI runs it on every PR.

```sh
cd rust
cargo build --release   # target/release/bzst
```

## Checks

CI runs the same commands, defined as aliases in `rust/.cargo/config.toml`. Run them from `rust/` before pushing:

```sh
cargo ci-fmt       # formatting (rustfmt --check)
cargo ci-clippy    # clippy, with every warning an error
cargo ci-test      # all tests, against the committed Cargo.lock
cargo deny check   # licences, advisories and dependency sources
```

If you changed the spec, also run this from the repository root:

```sh
typst compile spec/bzst.typ
```

CI runs on every PR whatever its base branch, so stacked PRs are checked too. Tests and clippy run on both Linux and macOS, because the library has OS-specific code.

## Code style

- `rust/rustfmt.toml` sets the formatting; run `cargo fmt --all` before committing.
- Public items get doc comments: what the item does, its inputs and outputs, and any constraints.
- Comments explain *why*, not *what*, and describe the code as it is now; history belongs in git.
- Keep `unsafe` to a minimum and comment why each use is sound.
- Prefer clear, simple code over clever code: correctness first, then readability, then performance.

## Tests

- **Generate test data in code; never commit data files.** Build inputs inline (see `rust/bzst/tests/common/`) so reviewers can see exactly what is being tested.
- Test behaviour, not implementation details, so tests survive refactoring.
- Prefer many small tests to table-driven ones, and name each after the behaviour it asserts, e.g. `truncation_at_a_block_boundary_is_detected_not_silently_partial`.
- Every check a reader makes on untrusted input deserves a test that feeds it crafted input.

## Dependencies

Add a dependency only for a clear reason, prefer crates already in the tree, and make sure `cargo deny check` still passes. Shared versions live in `[workspace.dependencies]` in `rust/Cargo.toml`.

## Pull requests

- Keep each PR focused on one change. Stacking PRs on top of each other is fine.
- Write commit messages with a short imperative subject and a body explaining what changed and why.
- Explain the change and its motivation in the PR description, and link related issues with `Closes #N`.
- Don't hard-wrap Markdown prose (READMEs, PR and issue text): one line per paragraph or bullet.

## Licensing

By contributing, you agree that your contributions to the specification are licensed under CC BY 4.0 ([`spec/LICENSE`](spec/LICENSE)) and your contributions to the implementation under the MIT License ([`rust/LICENSE`](rust/LICENSE)), the same terms as the rest of the project.
