//! The value a backend hands back, and how it lands on disk.
//!
//! This module deliberately offers more than any single backend uses: the
//! portal yields an encoded file while DRM and fbdev yield raw pixels, so some
//! of it is dead in any given feature combination. Allowing that here beats
//! annotating each item with a feature list that would need editing every time
//! a backend is added.
#![allow(dead_code)]

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::Error;

/// Open `dest` for writing, refusing to follow a symlink at the final
/// component. A regular file is still created or truncated as before, but a
/// planted symlink is rejected (`ELOOP`) rather than silently redirecting the
/// write to whatever it points at — which matters when rustshot runs more
/// privileged than whoever can create the output path.
fn create_no_follow(dest: &Path) -> Result<File, Error> {
    Ok(OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dest)?)
}

/// Byte layout of a source buffer, as it sits in memory (little-endian hosts).
/// DRM fourccs and fbdev bitfields both get normalised into one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    /// B, G, R, X — DRM_FORMAT_XRGB8888 / ARGB8888
    Bgrx8888,
    /// R, G, B, X — DRM_FORMAT_XBGR8888 / ABGR8888
    Rgbx8888,
    /// 16-bit 5:6:5, low bits blue
    Rgb565,
}

impl SourceFormat {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            SourceFormat::Bgrx8888 | SourceFormat::Rgbx8888 => 4,
            SourceFormat::Rgb565 => 2,
        }
    }
}

pub struct Image {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8888, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

impl Image {
    /// Convert a strided source buffer into packed RGBA.
    ///
    /// `stride` is the byte distance between rows, which is *not* generally
    /// `width * bpp` — scanout buffers are padded for alignment, and reading
    /// them as if they were tight produces the classic diagonal skew.
    pub fn from_raw(
        src: &[u8],
        width: u32,
        height: u32,
        stride: usize,
        format: SourceFormat,
    ) -> Result<Image, Error> {
        let bpp = format.bytes_per_pixel();
        let needed = stride
            .checked_mul(height as usize)
            .ok_or_else(|| Error::Other("buffer geometry overflows".into()))?;
        if src.len() < needed {
            return Err(Error::Other(format!(
                "buffer is {} bytes, need {needed} for {width}x{height} at stride {stride}",
                src.len()
            )));
        }
        if (width as usize) * bpp > stride {
            return Err(Error::Other(format!(
                "stride {stride} too small for {width}px at {bpp} bytes/px"
            )));
        }

        let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];
        for y in 0..height as usize {
            let row = &src[y * stride..y * stride + (width as usize) * bpp];
            let out = &mut rgba[y * (width as usize) * 4..(y + 1) * (width as usize) * 4];
            match format {
                SourceFormat::Bgrx8888 => {
                    for (px, o) in row.chunks_exact(4).zip(out.chunks_exact_mut(4)) {
                        o[0] = px[2];
                        o[1] = px[1];
                        o[2] = px[0];
                        o[3] = 0xff;
                    }
                }
                SourceFormat::Rgbx8888 => {
                    for (px, o) in row.chunks_exact(4).zip(out.chunks_exact_mut(4)) {
                        o[0] = px[0];
                        o[1] = px[1];
                        o[2] = px[2];
                        o[3] = 0xff;
                    }
                }
                SourceFormat::Rgb565 => {
                    for (px, o) in row.chunks_exact(2).zip(out.chunks_exact_mut(4)) {
                        let v = u16::from_le_bytes([px[0], px[1]]);
                        // Replicate high bits into the low ones so full-scale
                        // input maps to full-scale output.
                        let r = ((v >> 11) & 0x1f) as u8;
                        let g = ((v >> 5) & 0x3f) as u8;
                        let b = (v & 0x1f) as u8;
                        o[0] = (r << 3) | (r >> 2);
                        o[1] = (g << 2) | (g >> 4);
                        o[2] = (b << 3) | (b >> 2);
                        o[3] = 0xff;
                    }
                }
            }
        }
        Ok(Image { width, height, rgba })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scanout buffers are padded for alignment. Reading them as if rows were
    /// tightly packed produces a diagonal skew, so the stride must be honoured.
    #[test]
    fn honours_row_padding() {
        // 2x2 BGRX with 4 bytes of padding per row.
        let src: Vec<u8> = vec![
            3, 2, 1, 0, 30, 20, 10, 0, 0xde, 0xad, 0xbe, 0xef, // row 0 + pad
            6, 5, 4, 0, 60, 50, 40, 0, 0xde, 0xad, 0xbe, 0xef, // row 1 + pad
        ];
        let img = Image::from_raw(&src, 2, 2, 12, SourceFormat::Bgrx8888).unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(
            img.rgba,
            vec![
                1, 2, 3, 255, 10, 20, 30, 255, // row 0
                4, 5, 6, 255, 40, 50, 60, 255, // row 1
            ]
        );
    }

    #[test]
    fn rgbx_keeps_channel_order() {
        let src: Vec<u8> = vec![1, 2, 3, 0];
        let img = Image::from_raw(&src, 1, 1, 4, SourceFormat::Rgbx8888).unwrap();
        assert_eq!(img.rgba, vec![1, 2, 3, 255]);
    }

    /// 5/6-bit channels must reach full scale, not 248/252.
    #[test]
    fn rgb565_reaches_full_scale() {
        let white = 0xffffu16.to_le_bytes();
        let img = Image::from_raw(&white, 1, 1, 2, SourceFormat::Rgb565).unwrap();
        assert_eq!(img.rgba, vec![255, 255, 255, 255]);

        let red = 0xf800u16.to_le_bytes();
        let img = Image::from_raw(&red, 1, 1, 2, SourceFormat::Rgb565).unwrap();
        assert_eq!(img.rgba, vec![255, 0, 0, 255]);

        let black = 0x0000u16.to_le_bytes();
        let img = Image::from_raw(&black, 1, 1, 2, SourceFormat::Rgb565).unwrap();
        assert_eq!(img.rgba, vec![0, 0, 0, 255]);
    }

    /// A short mapping must be refused rather than read out of bounds.
    #[test]
    fn rejects_undersized_buffer() {
        let src = vec![0u8; 8];
        assert!(Image::from_raw(&src, 2, 2, 8, SourceFormat::Bgrx8888).is_err());
    }

    /// A stride that cannot hold one row means we misread the geometry.
    #[test]
    fn rejects_stride_narrower_than_row() {
        let src = vec![0u8; 64];
        assert!(Image::from_raw(&src, 4, 2, 8, SourceFormat::Bgrx8888).is_err());
    }

    /// A symlink planted at the destination must not be followed: the write is
    /// refused rather than redirected through the link to the victim file.
    #[test]
    fn refuses_to_write_through_symlink() {
        let dir = std::env::temp_dir().join(format!(
            "rustshot-symtest-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let victim = dir.join("victim.txt");
        std::fs::write(&victim, b"do not touch").unwrap();
        let dest = dir.join("shot.png");
        std::os::unix::fs::symlink(&victim, &dest).unwrap();

        let img = Image::from_raw(&[1, 2, 3, 0], 1, 1, 4, SourceFormat::Rgbx8888).unwrap();
        let err = Capture::Pixels(img).save(&dest);

        assert!(err.is_err(), "writing through a symlink should be refused");
        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"do not touch",
            "victim must be untouched"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

pub enum Capture {
    /// The backend already produced an encoded file (the portal writes its
    /// own PNG). Kept distinct so we move it instead of decoding and
    /// re-encoding it for nothing.
    EncodedFile(PathBuf),
    Pixels(Image),
}

impl Capture {
    pub fn save(self, dest: &Path) -> Result<(), Error> {
        match self {
            Capture::EncodedFile(src) => {
                // rename() fails across filesystems; the portal's directory is
                // often on a different mount than the target. rename() replaces
                // a symlink at `dest` rather than following it, so it is safe;
                // the copy fallback is not, hence the O_NOFOLLOW open below.
                if std::fs::rename(&src, dest).is_err() {
                    let mut from = File::open(&src)?;
                    let mut to = create_no_follow(dest)?;
                    std::io::copy(&mut from, &mut to)?;
                    let _ = std::fs::remove_file(&src);
                }
                Ok(())
            }
            Capture::Pixels(img) => {
                // image::save_buffer opens the path itself with O_CREAT|O_TRUNC
                // and would follow a symlinked dest, so encode into a handle we
                // opened with O_NOFOLLOW instead.
                let file = create_no_follow(dest)?;
                let encoder = image::codecs::png::PngEncoder::new(file);
                image::ImageEncoder::write_image(
                    encoder,
                    &img.rgba,
                    img.width,
                    img.height,
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|e| Error::Other(format!("encoding PNG: {e}")))
            }
        }
    }
}
