# rustshot

A screenshot tool for Linux that adapts to whatever display stack it finds,
from a GNOME desktop down to an embedded board with no compositor at all.

It has no C library dependencies. Every backend talks to a socket or a device
node directly, so `--target x86_64-unknown-linux-musl` produces a single static
binary that needs nothing but the kernel.

## Usage

```
rustshot                      # capture to screenshot_YYYYMMDD_HHMMSS.png
rustshot -o shot.png          # choose the output path
rustshot -i                   # use the desktop's own area/window picker
rustshot --list-backends      # what this build has, and what applies here
rustshot --backend drm        # force one instead of autodetecting
```

`--list-backends` is the first thing to run when a capture fails; it prints the
specific reason each backend declined.

## Backends

The chain is tried in order and the first backend that accepts wins.

| Backend  | Mechanism                       | Applies when |
| -------- | ------------------------------- | ------------ |
| `portal` | `xdg-desktop-portal` over D-Bus | A desktop session is running (GNOME, KDE, …) |
| `drm`    | Read the KMS scanout buffer     | No compositor owns the display |
| `fbdev`  | `/dev/fb0`                      | Kernels too old or stripped for KMS |

Order matters. Under Wayland, GNOME and KDE refuse the direct capture
protocols outright, so the portal is not merely preferred — it is the only
door. The direct backends exist for systems that have no portal at all.

The portal is also the only backend with a picker, so `-i` is rejected up front
by the others rather than silently capturing something else.

### Why `drm` usually declines on a desktop

`GETFB2` returns a framebuffer handle only to a caller that is the current DRM
master *and* holds `CAP_SYS_ADMIN`. A running compositor holds master, so the
backend declines during probe and the chain moves on. That restriction is
deliberate: without it, any process could read any other client's buffers.

On a machine with no compositor, opening the primary node makes the opener
master implicitly, and the capture path works.

Two further limits, both detected at probe time so they surface as a clear
reason rather than a corrupt image:

- **Tiled or compressed buffers.** Detiling Intel X/Y-tiling, AMD DCC or
  NVIDIA block-linear needs per-vendor work or a GPU blit. Non-linear
  modifiers are rejected.
- **Drivers that export no handles.** TTM-based drivers such as `vmwgfx` have
  framebuffers with no GEM object and no `create_handle`, so `GETFB2` answers
  `ENODEV` no matter who is asking.

### Writeback connectors

`DRM_MODE_CONNECTOR_WRITEBACK` is the sanctioned KMS capture path and yields
linear pixels even on tiled hardware. `probe` already detects the connector and
reports it in `--list-backends`; performing the atomic commit is **not
implemented yet**. See `capture_writeback` in `src/backend/drm.rs` — it belongs
behind the same `Backend` interface and would be tried ahead of the scanout
read.

## Building

```
cargo build --release                      # everything (default)

# Embedded: no D-Bus, fully static, ~1.5 MB
cargo build --release \
  --target x86_64-unknown-linux-musl \
  --no-default-features --features drm,fbdev
```

Features are `portal`, `drm`, `fbdev`; all are on by default and each can be
dropped independently.

## Adding a backend

1. Write `src/backend/<name>.rs` exposing `pub fn probe() -> Probe`.
2. Add one line to `REGISTRY` in `src/backend/mod.rs`, in priority order.
3. Add a Cargo feature so embedded builds can drop it.

Nothing else in the crate changes. A backend returns either
`Capture::Pixels` (raw RGBA, converted via `Image::from_raw`, which handles
row padding and pixel formats) or `Capture::EncodedFile` (a file it already
wrote, moved rather than re-encoded).

`probe` should be cheap and should decline with a reason a user can act on —
those strings are what `--list-backends` prints. Prefer to detect a problem
during probe, so the chain can fall through, over failing at capture time.

The obvious next additions are **wlr-screencopy** (Sway, Hyprland, kiosk
compositors) and **X11 `GetImage`**. Both are compositor-cooperative and work
unprivileged, so they belong after `portal` and before `drm`; `REGISTRY` has
the slot marked.

## Layout

```
src/backend/mod.rs      Backend trait, error taxonomy, registry, detection
src/backend/capture.rs  Image representation, pixel conversion, saving
src/backend/portal.rs   xdg-desktop-portal over D-Bus
src/backend/drm.rs      KMS scanout read (raw ioctls)
src/backend/fbdev.rs    /dev/fb0
```
