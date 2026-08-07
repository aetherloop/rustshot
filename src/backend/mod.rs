//! Capture backends and the detection chain that picks one.
//!
//! # Adding a backend
//!
//! 1. Write `src/backend/<name>.rs` exposing `pub fn probe() -> Probe`.
//! 2. Add one line to [`REGISTRY`], in priority order.
//! 3. Add a Cargo feature so embedded builds can drop it.
//!
//! Nothing else in the crate needs to change. `REGISTRY` order *is* the
//! priority order, and it matters: on desktop compositors the portal is the
//! only sanctioned path, so it must be tried before the direct ones, which
//! exist for environments that have no portal at all.

use std::fmt;
use std::path::PathBuf;

pub mod capture;

#[cfg(feature = "drm")]
pub mod drm;
#[cfg(feature = "fbdev")]
pub mod fbdev;
#[cfg(feature = "portal")]
pub mod portal;

// `Image` is used only by pixel-producing backends; the re-export stays
// unconditional so adding one never means editing this line.
#[allow(unused_imports)]
pub use capture::{Capture, Image};

/// What the caller wants captured.
///
/// New selectors (region, output name, cursor) belong here; backends that
/// cannot honour one should say so in [`Caps`] rather than silently ignoring it.
#[derive(Debug, Default, Clone)]
pub struct Request {
    /// Let the desktop present its own picker, if it has one.
    pub interactive: bool,
}

/// What a backend can do, so the CLI can reject impossible requests up front
/// instead of producing a surprising image.
#[derive(Debug, Default, Clone, Copy)]
pub struct Caps {
    pub interactive: bool,
}

/// Outcome of probing a backend. Declining is the *normal* case — most
/// backends are inapplicable on any given system — so it is a value, not an
/// error, and it carries a reason for `--list-backends`.
pub enum Probe {
    Ready(Box<dyn Backend>),
    Unavailable(String),
}

impl Probe {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Probe::Unavailable(reason.into())
    }
}

pub trait Backend {
    fn name(&self) -> &'static str;

    fn caps(&self) -> Caps {
        Caps::default()
    }

    /// One line of detail for `--list-backends` (device node, resolution…).
    fn describe(&self) -> String {
        self.name().to_string()
    }

    fn capture(&self, req: &Request) -> Result<Capture, Error>;
}

#[derive(Debug)]
pub enum Error {
    /// The user dismissed an interactive picker. Not a failure.
    /// Unused in builds containing no interactive backend.
    #[allow(dead_code)]
    Cancelled,
    /// Reached the backend, but this request is beyond what it can do.
    Unsupported(String),
    Io(std::io::Error),
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Cancelled => write!(f, "capture cancelled"),
            Error::Unsupported(m) => write!(f, "{m}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub struct Entry {
    pub name: &'static str,
    pub probe: fn() -> Probe,
}

/// Priority order — see module docs before reordering.
pub const REGISTRY: &[Entry] = &[
    #[cfg(feature = "portal")]
    Entry { name: "portal", probe: portal::probe },
    // Slots for wlr-screencopy and X11 GetImage go here: after the portal,
    // before DRM. Both are compositor-cooperative and work unprivileged,
    // so they should win over reading scanout memory directly.
    #[cfg(feature = "drm")]
    Entry { name: "drm", probe: drm::probe },
    #[cfg(feature = "fbdev")]
    Entry { name: "fbdev", probe: fbdev::probe },
];

/// Walk the registry in order and return the first backend that accepts.
/// Collects every rejection so a total failure can explain itself.
pub fn detect() -> Result<Box<dyn Backend>, Vec<(&'static str, String)>> {
    let mut declined = Vec::new();
    for entry in REGISTRY {
        match (entry.probe)() {
            Probe::Ready(b) => return Ok(b),
            Probe::Unavailable(reason) => declined.push((entry.name, reason)),
        }
    }
    Err(declined)
}

pub fn detect_named(name: &str) -> Result<Box<dyn Backend>, String> {
    let entry = REGISTRY
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| {
            let known: Vec<_> = REGISTRY.iter().map(|e| e.name).collect();
            format!("unknown backend \"{name}\" (built with: {})", known.join(", "))
        })?;
    match (entry.probe)() {
        Probe::Ready(b) => Ok(b),
        Probe::Unavailable(reason) => Err(format!("backend \"{name}\" unavailable: {reason}")),
    }
}

/// Probe everything and report, for `--list-backends`.
pub fn survey() -> Vec<(&'static str, Result<String, String>)> {
    REGISTRY
        .iter()
        .map(|e| {
            let status = match (e.probe)() {
                Probe::Ready(b) => Ok(b.describe()),
                Probe::Unavailable(r) => Err(r),
            };
            (e.name, status)
        })
        .collect()
}

/// Default output path when the user gives none.
pub fn default_output() -> PathBuf {
    PathBuf::from(format!(
        "screenshot_{}.png",
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    ))
}
