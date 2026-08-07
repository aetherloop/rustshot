//! Legacy `/dev/fb0`.
//!
//! Last resort, kept only for kernels too old or too stripped to offer KMS.
//! On anything modern this node is emulation layered over DRM
//! (`drm_fbdev_generic`) and is frequently compiled out entirely, which is
//! exactly why the DRM backend sits above it.

use std::fs::File;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;

use super::capture::SourceFormat;
use super::{Backend, Capture, Caps, Error, Image, Probe, Request};

const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;
const FBIOGET_FSCREENINFO: libc::c_ulong = 0x4602;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Bitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

#[repr(C)]
#[derive(Default)]
struct VarScreenInfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: Bitfield,
    green: Bitfield,
    blue: Bitfield,
    transp: Bitfield,
    nonstd: u32,
    activate: u32,
    height: u32,
    width: u32,
    accel_flags: u32,
    pixclock: u32,
    left_margin: u32,
    right_margin: u32,
    upper_margin: u32,
    lower_margin: u32,
    hsync_len: u32,
    vsync_len: u32,
    sync: u32,
    vmode: u32,
    rotate: u32,
    colorspace: u32,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Default)]
struct FixScreenInfo {
    id: [u8; 16],
    smem_start: libc::c_ulong,
    smem_len: u32,
    type_: u32,
    type_aux: u32,
    visual: u32,
    xpanstep: u16,
    ypanstep: u16,
    ywrapstep: u16,
    line_length: u32,
    mmap_start: libc::c_ulong,
    mmap_len: u32,
    accel: u32,
    capabilities: u16,
    reserved: [u16; 2],
}

unsafe fn ioctl<T>(fd: RawFd, request: libc::c_ulong, arg: &mut T) -> std::io::Result<()> {
    if libc::ioctl(fd, request as _, arg as *mut T) == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub struct Fbdev {
    node: PathBuf,
    file: File,
    width: u32,
    height: u32,
    stride: usize,
    format: SourceFormat,
}

pub fn probe() -> Probe {
    let node = PathBuf::from("/dev/fb0");
    if !node.exists() {
        return Probe::unavailable("/dev/fb0 does not exist");
    }
    let file = match File::open(&node) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Probe::unavailable("permission denied (needs root or the 'video' group)")
        }
        Err(e) => return Probe::unavailable(e.to_string()),
    };
    let fd = file.as_raw_fd();

    let mut var = VarScreenInfo::default();
    let mut fix = FixScreenInfo::default();
    if let Err(e) = unsafe { ioctl(fd, FBIOGET_VSCREENINFO, &mut var) } {
        return Probe::unavailable(format!("FBIOGET_VSCREENINFO: {e}"));
    }
    if let Err(e) = unsafe { ioctl(fd, FBIOGET_FSCREENINFO, &mut fix) } {
        return Probe::unavailable(format!("FBIOGET_FSCREENINFO: {e}"));
    }

    let format = match (var.bits_per_pixel, var.red.offset) {
        (32, 16) | (24, 16) => SourceFormat::Bgrx8888,
        (32, 0) | (24, 0) => SourceFormat::Rgbx8888,
        (16, _) => SourceFormat::Rgb565,
        (bpp, _) => return Probe::unavailable(format!("unhandled {bpp} bits per pixel")),
    };
    if var.xres == 0 || var.yres == 0 {
        return Probe::unavailable("framebuffer reports a zero-sized mode");
    }

    Probe::Ready(Box::new(Fbdev {
        node,
        file,
        width: var.xres,
        height: var.yres,
        stride: fix.line_length as usize,
        format,
    }))
}

impl Backend for Fbdev {
    fn name(&self) -> &'static str {
        "fbdev"
    }

    fn caps(&self) -> Caps {
        Caps { interactive: false }
    }

    fn describe(&self) -> String {
        format!(
            "{} — {}x{}, {:?}",
            self.node.display(),
            self.width,
            self.height,
            self.format
        )
    }

    fn capture(&self, req: &Request) -> Result<Capture, Error> {
        if req.interactive {
            return Err(Error::Unsupported(
                "the fbdev backend has no picker; drop --interactive".into(),
            ));
        }
        let len = self.stride * self.height as usize;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                self.file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
        let result = Image::from_raw(bytes, self.width, self.height, self.stride, self.format);
        unsafe { libc::munmap(ptr, len) };
        result.map(Capture::Pixels)
    }
}
