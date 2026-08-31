# Contributing to hop

## Building

hop is a normal Cargo workspace with four crates: `hop-proto`,
`hop-core`, `hop-platform`, and `hop`. From the repository root:

```
cargo build --workspace
```

`rust-toolchain.toml` pins the stable channel with the `rustfmt` and
`clippy` components; `rustup` picks it up automatically.

macOS input capture (`hop-platform::macos`) needs Accessibility
permission granted to whatever process actually runs it. If you are
running `hop` directly from a terminal, grant that permission to the
terminal application in System Settings -> Privacy & Security ->
Accessibility. If you are running it from an IDE or a debugger, that
process needs the permission instead. Without it, `MacCapturer::start`
fails to create its `CGEventTap` rather than silently capturing
nothing.

The Windows code (`hop-platform::windows`) compiles on macOS but cannot
run there: `SendInput` and the rest of `windows-sys` only do anything
on an actual Windows machine. CI, which runs the full suite on both
`macos-latest` and `windows-latest`, is the real check for that half of
the codebase.

## Before committing

Four gates must pass. All four run in CI; run them locally first so you
are not waiting on a CI failure to find out:

```
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace --all-targets --target x86_64-pc-windows-msvc
```

The `x86_64-pc-windows-msvc` target needs to be installed once with
`rustup target add x86_64-pc-windows-msvc`.

The `--all-targets` flag on that last command is not optional. Without
it, `cargo check` only checks library and binary targets for the
Windows target, and silently skips every `#[cfg(test)]` module in
`hop-platform::windows` (and anywhere else Windows-only test code
lives). This project has actually shipped a change that broke Windows
test compilation without anyone noticing locally, because the check was
run without `--all-targets`; it was only caught when CI failed on the
`windows-latest` job. Always pass `--all-targets` for this check.

## What CI actually verifies

`.github/workflows/ci.yml` runs `cargo fmt --all -- --check`, `cargo
clippy --workspace --all-targets -- -D warnings`, and `cargo test
--workspace` on both `macos-latest` and `windows-latest`, then builds
release binaries on each and uploads them as artifacts (`hop.exe` on
Windows, `hop` on macOS). Since the Windows test suite cannot run on a
Mac, the `windows-latest` job in CI is the only place `hop-platform`'s
Windows code is ever actually exercised as tests, not just type-checked
locally.
