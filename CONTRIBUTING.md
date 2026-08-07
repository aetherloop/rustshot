# Contributing

## Before you open a PR

```
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Then check the build that embedded targets actually use, because most of the
lints that matter here live in the syscall backends and the portal one is the
only part that pulls a dependency tree:

```
cargo clippy --all-targets --no-default-features --features drm,fbdev -- -D warnings
```

## Formatting

Stock rustfmt. `rustfmt.toml` pins `style_edition = "2021"` and writes down the
defaults it relies on, so the output does not shift when a newer toolchain
changes them. There is nothing to argue about and no hand-formatting to
preserve — if `cargo fmt` wants to rewrite your hunk, let it.

Imports are grouped by hand, blank-line separated, in this order:

```rust
use std::path::PathBuf;          // std

use clap::Parser;                // external crates

use super::{Backend, Capture};   // this crate
```

rustfmt sorts within a group but will not create or merge the groups, so keep
the shape when you add an import.

## Lints

Levels live in `[lints.rust]` and `[lints.clippy]` in `Cargo.toml`, so plain
`cargo clippy` — and `cargo build` — give the same answer as CI without any
flags. Per-lint tuning lives in `clippy.toml`.

`clippy::all` and `clippy::pedantic` are both on. Four lints are allowed
crate-wide, each for a reason specific to talking to the kernel:

| Allowed | Why |
| --- | --- |
| `cast_possible_truncation`, `cast_possible_wrap`, `cast_sign_loss` | ioctl struct field widths *are* the ABI. Narrowing to the width the kernel declared is the correct move, and the cases that genuinely need checking already go through `try_from` or `checked_*`. |
| `struct_field_names` | FFI structs mirror kernel field names verbatim; renaming them breaks the correspondence a reader needs when checking against the headers. |

Those allows are a blunt instrument covering code that has no better option.
Do not extend the list to quiet a warning in ordinary code — fix the code, or
put a narrowly scoped `#[allow(..)]` with a comment on the specific item.

`unsafe_op_in_unsafe_fn` is on: an `unsafe fn` does not get an implicit unsafe
body. Every raw call needs its own `unsafe { }`, which is what makes the audit
of unsafe operations in `drm.rs` and `fbdev.rs` mean anything.

If clippy is wrong about a specific line, `#[allow(clippy::lint_name)]` on the
smallest enclosing item, with a comment saying why. An unexplained `allow` is a
review comment.

New kernel constants and spec names that appear in doc comments go in
`doc-valid-idents` in `clippy.toml` rather than getting backticked, when they
read better bare.

The tree is clean under these settings on both feature sets. Keep it that way:
a warning that survives a merge is one nobody reads afterwards.

## Unsafe code

`drm.rs` and `fbdev.rs` are the only modules with `unsafe`, and it is all
ioctl, `mmap`, and reading kernel-filled structs. When you add some:

- Every `unsafe` block gets a comment naming the invariant that makes it sound
  — which fd, whose lifetime, what the kernel guarantees about the buffer.
- Mirror the kernel struct field-for-field, including padding, and say which
  header it comes from.
- Anything the kernel hands back is untrusted input. Sizes and offsets get
  `checked_*` arithmetic before they reach a slice, not a cast and a hope.

## Backends

See "Adding a backend" in [README.md](README.md). The short version: `probe`
should be cheap and should decline with a reason a user can act on, because
those strings are what `--list-backends` prints. Prefer detecting a problem at
probe time, so the chain can fall through, over failing at capture time.
