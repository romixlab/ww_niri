//! Images of [FillOwned::Image](ww_niri_api::FillOwned::Image): PNG files by name in ww_niri's
//! backgrounds folder, read when an item is put and kept while an item uses them.
//!
//! Only names reach here, never bytes over the API; [ww_niri_api::image_name_ok] has made sure a
//! name is no path. A file read again only when its size or modification time changed.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Cursor, Read as _};
use std::path::Path;
use std::rc::Rc;
use std::time::SystemTime;

use anyhow::Context as _;
use smithay::backend::allocator::Fourcc;
use smithay::utils::Transform;
use ww_niri_api::OverlayError;

use crate::render_helpers::memory::MemoryBuffer;

/// Largest image file read, in bytes.
const FILE_BYTES_MAX: u64 = 32 << 20;
/// Most pixels of an image: two 4K screens.
const PIXELS_MAX: u64 = 2 * 3840 * 2160;
/// Most bytes the decoder may take.
const DECODE_BYTES_MAX: usize = 160 << 20;

/// An image, as premultiplied ARGB pixels.
pub struct Image {
    pub name: String,
    pub pixels: MemoryBuffer,
    /// The file's size and modification time when it was read.
    stamp: (u64, Option<SystemTime>),
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let size = self.pixels.size();
        write!(f, "Image({:?}, {}x{})", self.name, size.w, size.h)
    }
}

impl PartialEq for Image {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.stamp == other.stamp
    }
}

impl Eq for Image {}

/// The images items use, by name.
#[derive(Debug, Default)]
pub struct Images {
    by_name: HashMap<String, Rc<Image>>,
}

impl Images {
    /// The image `name` of `dir`, read again when its file changed.
    ///
    /// # Errors
    ///
    /// `NoSuchImage` when the file is missing, too large or not a PNG image.
    pub fn load(&mut self, dir: &Path, name: &str) -> Result<Rc<Image>, OverlayError> {
        let path = dir.join(name);
        let stamp = stamp(&path).ok_or(OverlayError::NoSuchImage)?;
        if let Some(image) = self.by_name.get(name).filter(|i| i.stamp == stamp) {
            return Ok(image.clone());
        }
        let pixels = read(&path).map_err(|err| {
            warn!("ww: error reading the overlay image {path:?}: {err:?}");
            OverlayError::NoSuchImage
        })?;
        let image = Rc::new(Image {
            name: name.to_owned(),
            pixels,
            stamp,
        });
        self.by_name.insert(name.to_owned(), image.clone());
        Ok(image)
    }

    /// Keeps only the images of `used`.
    pub fn keep<'a>(&mut self, used: impl Iterator<Item = &'a Rc<Image>>) {
        let used: Vec<&str> = used.map(|i| i.name.as_str()).collect();
        self.by_name.retain(|name, _| used.contains(&name.as_str()));
    }
}

fn stamp(path: &Path) -> Option<(u64, Option<SystemTime>)> {
    let meta = path.metadata().ok()?;
    meta.is_file().then(|| (meta.len(), meta.modified().ok()))
}

/// Reads a PNG file into premultiplied ARGB pixels.
fn read(path: &Path) -> anyhow::Result<MemoryBuffer> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(FILE_BYTES_MAX + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= FILE_BYTES_MAX,
        "larger than {FILE_BYTES_MAX} bytes"
    );
    let limits = png::Limits {
        bytes: DECODE_BYTES_MAX,
    };
    let mut decoder = png::Decoder::new_with_limits(Cursor::new(bytes.as_slice()), limits);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let (w, h) = (reader.info().width, reader.info().height);
    anyhow::ensure!(
        u64::from(w) * u64::from(h) <= PIXELS_MAX,
        "{w}x{h}, more than {PIXELS_MAX} pixels"
    );
    let size = reader.output_buffer_size().context("too large")?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf)?;
    let rows = buf.chunks(info.line_size).take(h as usize);
    let mut pixels = Vec::with_capacity(w as usize * h as usize * 4);
    for row in rows {
        argb_row(info.color_type, row, &mut pixels)?;
    }
    let (w, h) = (i32::try_from(w)?, i32::try_from(h)?);
    Ok(MemoryBuffer::new(
        pixels,
        Fourcc::Argb8888,
        (w, h),
        1.,
        Transform::Normal,
    ))
}

/// Appends a row of 8-bit `color` pixels as premultiplied ARGB8888, little-endian (B, G, R, A).
fn argb_row(color: png::ColorType, row: &[u8], out: &mut Vec<u8>) -> anyhow::Result<()> {
    use png::ColorType::{Grayscale, GrayscaleAlpha, Rgb, Rgba};

    let premultiplied = |v: u8, a: u8| ((u16::from(v) * u16::from(a) + 127) / 255) as u8;
    let mut push = |r: u8, g: u8, b: u8, a: u8| {
        out.extend([
            premultiplied(b, a),
            premultiplied(g, a),
            premultiplied(r, a),
            a,
        ]);
    };
    match color {
        Grayscale => row.iter().for_each(|&v| push(v, v, v, 255)),
        GrayscaleAlpha => (row.chunks_exact(2)).for_each(|p| push(p[0], p[0], p[0], p[1])),
        Rgb => (row.chunks_exact(3)).for_each(|p| push(p[0], p[1], p[2], 255)),
        Rgba => (row.chunks_exact(4)).for_each(|p| push(p[0], p[1], p[2], p[3])),
        png::ColorType::Indexed => anyhow::bail!("indexed colours left after expanding"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w` by `h` image of one half-clear colour.
    fn write_png(path: &Path, w: u32, h: u32) {
        let mut encoder = png::Encoder::new(File::create(path).unwrap(), w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let pixels = [200u8, 100, 50, 128].repeat((w * h) as usize);
        writer.write_image_data(&pixels).unwrap();
    }

    /// An empty folder of the test's own.
    fn folder(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ww-images-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_a_png_and_keeps_it_while_used() {
        let dir = folder("read");
        write_png(&dir.as_path().join("sky.png"), 6, 4);
        let mut images = Images::default();
        let sky = images.load(dir.as_path(), "sky.png").unwrap();
        assert_eq!(sky.pixels.size(), (6, 4).into());
        assert_eq!(sky.pixels.data().len(), 6 * 4 * 4);
        // Premultiplied B, G, R, A.
        assert_eq!(&sky.pixels.data()[..4], &[25, 50, 100, 128]);
        // Unchanged: the same image, not read again.
        assert!(Rc::ptr_eq(
            &sky,
            &images.load(dir.as_path(), "sky.png").unwrap()
        ));

        images.keep(std::iter::empty());
        assert!(!Rc::ptr_eq(
            &sky,
            &images.load(dir.as_path(), "sky.png").unwrap()
        ));
    }

    #[test]
    fn refuses_missing_and_non_png_files() {
        let dir = folder("refuse");
        std::fs::write(dir.as_path().join("note.png"), "not a png").unwrap();
        std::fs::create_dir(dir.as_path().join("folder")).unwrap();
        let mut images = Images::default();
        for name in ["none.png", "note.png", "folder"] {
            assert_eq!(
                images.load(dir.as_path(), name).unwrap_err(),
                OverlayError::NoSuchImage,
                "{name}"
            );
        }
    }
}
