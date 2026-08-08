//! Direct KMS capture: read the framebuffer the display engine is currently
//! scanning out.
//!
//! This is the backend for systems with no compositor — embedded targets,
//! kiosk boot screens, bare consoles — where DRM is present but no portal or
//! Wayland/X server is. It is deliberately *last* among the real backends,
//! because it only works when nothing else owns the display:
//!
//! `GETFB2` fills in a buffer handle only for a caller that is the current DRM
//! master *and* has `CAP_SYS_ADMIN`. A running compositor holds master, so on
//! a desktop this backend declines during probe and the chain moves on. That
//! restriction is deliberate on the kernel's part — otherwise any process
//! could read any other client's buffers.
//!
//! Opening a primary node when no master exists makes the opener master
//! implicitly. That is the one side effect of probing, and it is the state we
//! want anyway on a machine with nothing else driving the display.

use std::fs::File;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;

use super::capture::SourceFormat;
use super::{Backend, Caps, Capture, Error, Image, Probe, Request};

mod ffi {
    #![allow(non_camel_case_types)]

    /// asm-generic ioctl encoding (x86, arm, arm64, riscv). mips/powerpc/sparc
    /// use a different layout; those would need a cfg here.
    const fn ioc(dir: u32, ty: u32, nr: u32, size: usize) -> libc::c_ulong {
        ((dir << 30) | ((size as u32) << 16) | (ty << 8) | nr) as libc::c_ulong
    }
    const WR: u32 = 3; // _IOC_READ | _IOC_WRITE
    const W: u32 = 1;
    const DRM: u32 = b'd' as u32;

    pub const SET_MASTER: libc::c_ulong = ioc(0, DRM, 0x1E, 0);
    pub const GETRESOURCES: libc::c_ulong = ioc(WR, DRM, 0xA0, size_of::<CardRes>());
    pub const GETCRTC: libc::c_ulong = ioc(WR, DRM, 0xA1, size_of::<Crtc>());
    pub const GETCONNECTOR: libc::c_ulong = ioc(WR, DRM, 0xA7, size_of::<GetConnector>());
    pub const GETFB: libc::c_ulong = ioc(WR, DRM, 0xAD, size_of::<FbCmd>());
    pub const GETFB2: libc::c_ulong = ioc(WR, DRM, 0xCE, size_of::<FbCmd2>());
    pub const MAP_DUMB: libc::c_ulong = ioc(WR, DRM, 0xB3, size_of::<MapDumb>());
    pub const PRIME_HANDLE_TO_FD: libc::c_ulong = ioc(WR, DRM, 0x2D, size_of::<PrimeHandle>());
    pub const GEM_CLOSE: libc::c_ulong = ioc(W, DRM, 0x09, size_of::<GemClose>());

    #[repr(C)]
    #[derive(Default)]
    pub struct CardRes {
        pub fb_id_ptr: u64,
        pub crtc_id_ptr: u64,
        pub connector_id_ptr: u64,
        pub encoder_id_ptr: u64,
        pub count_fbs: u32,
        pub count_crtcs: u32,
        pub count_connectors: u32,
        pub count_encoders: u32,
        pub min_width: u32,
        pub max_width: u32,
        pub min_height: u32,
        pub max_height: u32,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct ModeInfo {
        pub clock: u32,
        pub hdisplay: u16,
        pub hsync_start: u16,
        pub hsync_end: u16,
        pub htotal: u16,
        pub hskew: u16,
        pub vdisplay: u16,
        pub vsync_start: u16,
        pub vsync_end: u16,
        pub vtotal: u16,
        pub vscan: u16,
        pub vrefresh: u32,
        pub flags: u32,
        pub type_: u32,
        pub name: [u8; 32],
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct Crtc {
        pub set_connectors_ptr: u64,
        pub count_connectors: u32,
        pub crtc_id: u32,
        pub fb_id: u32,
        pub x: u32,
        pub y: u32,
        pub gamma_size: u32,
        pub mode_valid: u32,
        pub mode: ModeInfo,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct GetConnector {
        pub encoders_ptr: u64,
        pub modes_ptr: u64,
        pub props_ptr: u64,
        pub prop_values_ptr: u64,
        pub count_modes: u32,
        pub count_props: u32,
        pub count_encoders: u32,
        pub encoder_id: u32,
        pub connector_id: u32,
        pub connector_type: u32,
        pub connector_type_id: u32,
        pub connection: u32,
        pub mm_width: u32,
        pub mm_height: u32,
        pub subpixel: u32,
        pub pad: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct FbCmd2 {
        pub fb_id: u32,
        pub width: u32,
        pub height: u32,
        pub pixel_format: u32,
        pub flags: u32,
        pub handles: [u32; 4],
        pub pitches: [u32; 4],
        pub offsets: [u32; 4],
        pub modifier: [u64; 4],
    }

    /// Pre-atomic GETFB. No fourcc and no modifier — bpp/depth imply the
    /// layout — but implemented by drivers that reject GETFB2.
    #[repr(C)]
    #[derive(Default)]
    pub struct FbCmd {
        pub fb_id: u32,
        pub width: u32,
        pub height: u32,
        pub pitch: u32,
        pub bpp: u32,
        pub depth: u32,
        pub handle: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct MapDumb {
        pub handle: u32,
        pub pad: u32,
        pub offset: u64,
    }

    #[repr(C)]
    pub struct PrimeHandle {
        pub handle: u32,
        pub flags: u32,
        pub fd: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct GemClose {
        pub handle: u32,
        pub pad: u32,
    }

    pub const CONNECTOR_WRITEBACK: u32 = 18;
    pub const CONNECTED: u32 = 1;

    pub const MOD_LINEAR: u64 = 0;
    pub const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

    pub const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
        (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
    }
    pub const XRGB8888: u32 = fourcc(b'X', b'R', b'2', b'4');
    pub const ARGB8888: u32 = fourcc(b'A', b'R', b'2', b'4');
    pub const XBGR8888: u32 = fourcc(b'X', b'B', b'2', b'4');
    pub const ABGR8888: u32 = fourcc(b'A', b'B', b'2', b'4');
    pub const RGB565: u32 = fourcc(b'R', b'G', b'1', b'6');
}

/// # Safety
/// `arg` must point at the struct type the kernel associates with `request`.
unsafe fn ioctl<T>(fd: RawFd, request: libc::c_ulong, arg: &mut T) -> std::io::Result<()> {
    // DRM retries on signals; EINTR/EAGAIN here are transient, not failures.
    loop {
        // SAFETY: `arg` is a live `&mut T` for the whole call, and the caller
        // guarantees `T` is the struct `request` expects.
        let r = unsafe { libc::ioctl(fd, request as _, std::ptr::from_mut::<T>(arg)) };
        if r == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR | libc::EAGAIN) => {}
            _ => return Err(e),
        }
    }
}

/// For ioctls that carry no argument (`DRM_IO(...)`).
///
/// # Safety
/// `request` must be an ioctl that takes no argument.
unsafe fn ioctl_none(fd: RawFd, request: libc::c_ulong) -> std::io::Result<()> {
    // SAFETY: the caller guarantees `request` carries no argument, so the
    // kernel reads nothing from the variadic slot.
    if unsafe { libc::ioctl(fd, request as _) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Normalised framebuffer description, so callers do not care whether it came
/// from GETFB2 or the older GETFB.
struct FbInfo {
    handle: u32,
    width: u32,
    height: u32,
    stride: usize,
    offset: usize,
    format: SourceFormat,
    modifier: u64,
}

/// An active CRTC and the framebuffer it is scanning out.
struct Scanout {
    crtc_id: u32,
    fb_id: u32,
    width: u32,
    height: u32,
}

pub struct Drm {
    node: PathBuf,
    file: File,
    scanout: Scanout,
    /// Present when the hardware exposes a writeback connector — see
    /// `capture_writeback` for why we only report it today.
    writeback: Option<u32>,
}

pub fn probe() -> Probe {
    let nodes = card_nodes();
    if nodes.is_empty() {
        return Probe::unavailable("no /dev/dri/card* nodes (kernel has no KMS driver)");
    }

    let mut last = String::new();
    for node in nodes {
        match probe_node(&node) {
            Ok(b) => return Probe::Ready(Box::new(b)),
            Err(reason) => last = format!("{}: {reason}", node.display()),
        }
    }
    Probe::unavailable(last)
}

fn probe_node(node: &PathBuf) -> Result<Drm, String> {
    let file = File::open(node).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            "permission denied (needs root or the 'video' group)".to_string()
        }
        _ => e.to_string(),
    })?;
    let fd = file.as_raw_fd();

    // Establish up front whether the display is ours to read. Opening a
    // primary node with no master already made us master implicitly, so this
    // is a no-op in the case we care about; EBUSY is the compositor telling us
    // to go away, which is the single most common reason to decline and worth
    // reporting in those terms rather than as a downstream ioctl failure.
    if let Err(e) = unsafe { ioctl_none(fd, ffi::SET_MASTER) } {
        match e.raw_os_error() {
            Some(libc::EBUSY) => {
                return Err(
                    "another process holds DRM master (a compositor owns the display)".to_string(),
                )
            }
            Some(libc::EACCES | libc::EPERM) => {
                return Err("cannot become DRM master (needs root or CAP_SYS_ADMIN)".to_string())
            }
            _ => return Err(format!("SET_MASTER: {e}")),
        }
    }

    let scanout = active_scanout(fd)?.ok_or("no CRTC is scanning out a framebuffer")?;

    // Decisive and read-only: ask for the buffer behind the scanout. Anything
    // that would make capture fail should surface here, so the chain can move
    // on rather than failing at the moment of capture.
    let fb = framebuffer_info(fd, scanout.fb_id).map_err(|e| e.to_string())?;
    close_handle(fd, fb.handle);

    // Reject exotic layouts at probe time rather than emitting scrambled
    // pixels: tiled and compressed buffers need per-vendor detiling or a GPU
    // blit, neither of which this backend does.
    if fb.modifier != ffi::MOD_LINEAR && fb.modifier != ffi::MOD_INVALID {
        return Err(format!(
            "scanout buffer is not linear (modifier {:#x}); detiling is not implemented",
            fb.modifier
        ));
    }

    let writeback = find_writeback(fd);

    Ok(Drm {
        node: node.clone(),
        file,
        scanout,
        writeback,
    })
}

impl Backend for Drm {
    fn name(&self) -> &'static str {
        "drm"
    }

    fn caps(&self) -> Caps {
        Caps { interactive: false }
    }

    fn describe(&self) -> String {
        let wb = match self.writeback {
            Some(id) => format!(", writeback connector {id} present"),
            None => String::new(),
        };
        format!(
            "{} — CRTC {} scanning out {}x{}{}",
            self.node.display(),
            self.scanout.crtc_id,
            self.scanout.width,
            self.scanout.height,
            wb
        )
    }

    fn capture(&self, req: &Request) -> Result<Capture, Error> {
        if req.interactive {
            return Err(Error::Unsupported(
                "the drm backend has no picker; drop --interactive".into(),
            ));
        }
        self.capture_scanout().map(Capture::Pixels)
    }
}

impl Drm {
    fn capture_scanout(&self) -> Result<Image, Error> {
        let fd = self.file.as_raw_fd();
        // Re-query rather than caching: the compositor-free targets this
        // backend serves can still page-flip between probe and capture, which
        // changes fb_id and geometry.
        let scanout = active_scanout(fd)
            .map_err(Error::Other)?
            .ok_or_else(|| Error::Other("no CRTC is scanning out a framebuffer".into()))?;
        let fb = framebuffer_info(fd, scanout.fb_id)?;
        // Release the GEM reference however we exit from here.
        let _guard = HandleGuard {
            fd,
            handle: fb.handle,
        };

        let len = fb
            .offset
            .checked_add(fb.stride.saturating_mul(fb.height as usize))
            .ok_or_else(|| Error::Other("framebuffer geometry overflows".into()))?;

        let map = self.map_buffer(fb.handle, len)?;
        let bytes = &map.as_slice()[fb.offset..];
        Image::from_raw(bytes, fb.width, fb.height, fb.stride, fb.format)
    }

    /// Two routes to the pixels. dma-buf export is the portable one and keeps
    /// the mapping independent of the DRM fd; MAP_DUMB is the fallback for
    /// drivers whose exporter has no mmap, which covers the simple display
    /// controllers this backend mostly runs on.
    fn map_buffer(&self, handle: u32, len: usize) -> Result<Mapping, Error> {
        let fd = self.file.as_raw_fd();

        let mut prime = ffi::PrimeHandle {
            handle,
            flags: libc::O_RDONLY as u32,
            fd: -1,
        };
        if unsafe { ioctl(fd, ffi::PRIME_HANDLE_TO_FD, &mut prime) }.is_ok() && prime.fd >= 0 {
            let dmabuf = OwnedFd(prime.fd);
            if let Ok(m) = Mapping::new(dmabuf.0, 0, len) {
                return Ok(m);
            }
        }

        let mut map = ffi::MapDumb {
            handle,
            pad: 0,
            offset: 0,
        };
        unsafe { ioctl(fd, ffi::MAP_DUMB, &mut map) }.map_err(|e| {
            Error::Other(format!(
                "cannot map the scanout buffer: dma-buf mmap failed and MAP_DUMB returned {e} \
                 (the buffer is likely GPU-allocated rather than a dumb buffer)"
            ))
        })?;
        Mapping::new(fd, map.offset as i64, len)
    }
}

// Reading composited output through a writeback connector is the sanctioned
// KMS capture path and yields linear pixels even on tiled hardware. It needs
// an atomic commit (mode blob, a destination framebuffer, an out-fence to wait
// on) and DRM master, and only some display controllers expose one. `probe`
// already detects the connector; wiring the commit belongs here, behind the
// same `Backend` interface, and would be tried ahead of `capture_scanout`.
#[allow(dead_code)]
fn capture_writeback() {}

fn card_nodes() -> Vec<PathBuf> {
    let mut nodes: Vec<PathBuf> = match std::fs::read_dir("/dev/dri") {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("card"))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    nodes.sort();
    nodes
}

fn active_scanout(fd: RawFd) -> Result<Option<Scanout>, String> {
    let crtc_ids = crtc_ids(fd)?;
    for id in crtc_ids {
        let mut crtc = ffi::Crtc {
            crtc_id: id,
            ..Default::default()
        };
        if unsafe { ioctl(fd, ffi::GETCRTC, &mut crtc) }.is_err() {
            continue;
        }
        if crtc.mode_valid != 0 && crtc.fb_id != 0 {
            return Ok(Some(Scanout {
                crtc_id: id,
                fb_id: crtc.fb_id,
                width: u32::from(crtc.mode.hdisplay),
                height: u32::from(crtc.mode.vdisplay),
            }));
        }
    }
    Ok(None)
}

/// Two-pass: the first call reports counts, the second fills our buffers.
fn crtc_ids(fd: RawFd) -> Result<Vec<u32>, String> {
    let mut res = ffi::CardRes::default();
    unsafe { ioctl(fd, ffi::GETRESOURCES, &mut res) }.map_err(|e| format!("GETRESOURCES: {e}"))?;
    if res.count_crtcs == 0 {
        return Ok(Vec::new());
    }
    let mut ids = vec![0u32; res.count_crtcs as usize];
    let mut res2 = ffi::CardRes {
        crtc_id_ptr: ids.as_mut_ptr() as u64,
        count_crtcs: res.count_crtcs,
        ..Default::default()
    };
    unsafe { ioctl(fd, ffi::GETRESOURCES, &mut res2) }.map_err(|e| format!("GETRESOURCES: {e}"))?;
    ids.truncate(res2.count_crtcs as usize);
    Ok(ids)
}

fn find_writeback(fd: RawFd) -> Option<u32> {
    let mut res = ffi::CardRes::default();
    unsafe { ioctl(fd, ffi::GETRESOURCES, &mut res) }.ok()?;
    if res.count_connectors == 0 {
        return None;
    }
    let mut ids = vec![0u32; res.count_connectors as usize];
    let mut res2 = ffi::CardRes {
        connector_id_ptr: ids.as_mut_ptr() as u64,
        count_connectors: res.count_connectors,
        ..Default::default()
    };
    unsafe { ioctl(fd, ffi::GETRESOURCES, &mut res2) }.ok()?;

    ids.into_iter().find(|&id| {
        // All counts left at zero: the kernel reports metadata without
        // needing us to allocate for modes, props or encoders.
        let mut conn = ffi::GetConnector {
            connector_id: id,
            ..Default::default()
        };
        unsafe { ioctl(fd, ffi::GETCONNECTOR, &mut conn) }.is_ok()
            && conn.connector_type == ffi::CONNECTOR_WRITEBACK
            && conn.connection == ffi::CONNECTED
    })
}

/// Describe the framebuffer behind `fb_id`, preferring GETFB2 (which reports
/// the fourcc and modifier) and falling back to GETFB for drivers that reject
/// it. On success the caller owns the returned GEM handle.
fn framebuffer_info(fd: RawFd, fb_id: u32) -> Result<FbInfo, Error> {
    let mut fb2 = ffi::FbCmd2 {
        fb_id,
        ..Default::default()
    };
    match unsafe { ioctl(fd, ffi::GETFB2, &mut fb2) } {
        Ok(()) if fb2.handles[0] != 0 => {
            return Ok(FbInfo {
                handle: fb2.handles[0],
                width: fb2.width,
                height: fb2.height,
                stride: fb2.pitches[0] as usize,
                offset: fb2.offsets[0] as usize,
                format: source_format(fb2.pixel_format)?,
                modifier: fb2.modifier[0],
            });
        }
        // Geometry came back but the handle was withheld: we are not master
        // with CAP_SYS_ADMIN. The kernel does this deliberately — otherwise
        // any process could read any other client's buffers.
        Ok(()) => {
            return Err(Error::Unsupported(
                "kernel withheld the framebuffer handle (not DRM master, or no CAP_SYS_ADMIN)"
                    .into(),
            ))
        }
        Err(e) => {
            if e.raw_os_error() == Some(libc::ENODEV) {
                // GETFB2 answers ENODEV when the framebuffer has no GEM object
                // and the driver exposes no create_handle — TTM-based drivers
                // such as vmwgfx. GETFB will not do better, but say so plainly.
                return Err(Error::Unsupported(
                    "this DRM driver does not export scanout buffer handles \
                     (no GEM object and no create_handle)"
                        .into(),
                ));
            }
            // Otherwise fall through and try the older call.
        }
    }

    let mut fb = ffi::FbCmd {
        fb_id,
        ..Default::default()
    };
    unsafe { ioctl(fd, ffi::GETFB, &mut fb) }.map_err(|e| Error::Other(format!("GETFB: {e}")))?;
    if fb.handle == 0 {
        return Err(Error::Unsupported(
            "kernel withheld the framebuffer handle (not DRM master, or no CAP_SYS_ADMIN)".into(),
        ));
    }
    Ok(FbInfo {
        handle: fb.handle,
        width: fb.width,
        height: fb.height,
        stride: fb.pitch as usize,
        offset: 0,
        // GETFB predates fourccs; bpp/depth carry the conventional meaning.
        format: match (fb.bpp, fb.depth) {
            (32, 24 | 32) | (24, 24) => SourceFormat::Bgrx8888,
            (16, 16) => SourceFormat::Rgb565,
            (bpp, depth) => {
                return Err(Error::Unsupported(format!(
                    "unhandled legacy framebuffer geometry ({bpp} bpp, depth {depth})"
                )))
            }
        },
        // v1 cannot report tiling; assume linear, as it predates modifiers.
        modifier: ffi::MOD_INVALID,
    })
}

fn source_format(fourcc: u32) -> Result<SourceFormat, Error> {
    match fourcc {
        ffi::XRGB8888 | ffi::ARGB8888 => Ok(SourceFormat::Bgrx8888),
        ffi::XBGR8888 | ffi::ABGR8888 => Ok(SourceFormat::Rgbx8888),
        ffi::RGB565 => Ok(SourceFormat::Rgb565),
        other => {
            let b = other.to_le_bytes().map(|c| c as char);
            Err(Error::Unsupported(format!(
                "unhandled scanout pixel format '{}{}{}{}'",
                b[0], b[1], b[2], b[3]
            )))
        }
    }
}

fn close_handle(fd: RawFd, handle: u32) {
    let mut c = ffi::GemClose { handle, pad: 0 };
    let _ = unsafe { ioctl(fd, ffi::GEM_CLOSE, &mut c) };
}

struct HandleGuard {
    fd: RawFd,
    handle: u32,
}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        close_handle(self.fd, self.handle);
    }
}

struct OwnedFd(RawFd);

impl Drop for OwnedFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

struct Mapping {
    ptr: *mut libc::c_void,
    len: usize,
}

impl Mapping {
    fn new(fd: RawFd, offset: i64, len: usize) -> Result<Mapping, Error> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                offset as libc::off_t,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        Ok(Mapping { ptr, len })
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}
