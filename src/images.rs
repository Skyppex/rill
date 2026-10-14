//! Images named in a program, as in `const FACE = ./face.png`, and
//! spectrograms drawn as images.
//!
//! Like audio files (see [`crate::audio`]), an image is read in the build
//! stage through the program's [`Sources`], kept while its source gives it
//! the same stamp, and never touched while the program plays except
//! through `pixel`. Every format comes from the [`image`] crate, pure Rust:
//! PNG, JPEG, GIF (the first frame), WebP, BMP, TGA, ICO, QOI, PNM, TIFF,
//! HDR and OpenEXR.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::lang::Sources;

/// The extensions of the formats that can be read, in lower case.
pub const EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tga", "ico", "qoi", "pnm", "pbm", "pgm", "ppm",
    "pam", "tif", "tiff", "hdr", "exr",
];

/// Whether `path` names an image, by its extension.
pub fn is_image(path: &str) -> bool {
    crate::audio::extension(path).is_some_and(|e| EXTENSIONS.contains(&e.as_str()))
}

/// A decoded image: red, green, blue and alpha per pixel, each 0 to 1, row
/// by row from the top.
#[derive(Debug, PartialEq)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    /// Whether the file has an alpha channel; without one, alpha is 1.
    pub alpha: bool,
    pub pixels: Vec<[f32; 4]>,
}

impl Image {
    /// Channel `c` (red, green, blue, alpha) at `x`, `y` in pixels, between
    /// pixels read on a line from the four nearest (a pixel's value is at
    /// its centre, `x + 0.5`); 0 outside the image.
    pub fn at(&self, c: usize, x: f32, y: f32) -> f32 {
        let (w, h) = (self.width as f32, self.height as f32);
        if !(x >= 0.0 && x < w && y >= 0.0 && y < h) {
            return 0.0;
        }
        // From the centres of pixels, held at the edges.
        let fx = (x - 0.5).clamp(0.0, w - 1.0);
        let fy = (y - 0.5).clamp(0.0, h - 1.0);
        let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.width - 1), (y0 + 1).min(self.height - 1));
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let p = |x: usize, y: usize| self.pixels[y * self.width + x][c];
        let top = p(x0, y0) + (p(x1, y0) - p(x0, y0)) * tx;
        let bottom = p(x0, y1) + (p(x1, y1) - p(x0, y1)) * tx;
        top + (bottom - top) * ty
    }
}

/// An image a program names: where it is, and what it holds. Two are the
/// same image when their paths are.
#[derive(Clone)]
pub struct ImageRef {
    pub path: PathBuf,
    pub image: Arc<Image>,
}

impl PartialEq for ImageRef {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl std::fmt::Debug for ImageRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ImageRef({})", self.path.display())
    }
}

type Decoded = HashMap<PathBuf, (u64, Arc<Image>)>;

fn decoded() -> &'static Mutex<Decoded> {
    static CACHE: OnceLock<Mutex<Decoded>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The image at `path`, from `sources`, or why it cannot be read. Kept by
/// its canonical path while `sources` gives it the same stamp.
pub fn load(sources: &dyn Sources, path: &Path) -> Result<ImageRef, String> {
    let key = sources.canonical(path);
    let stamp = sources.stamp(path);
    if let Some(stamp) = stamp
        && let Some((s, image)) = decoded().lock().ok().and_then(|c| c.get(&key).cloned())
        && s == stamp
    {
        return Ok(ImageRef {
            path: path.to_owned(),
            image,
        });
    }
    let image = sources.image(path)?;
    if let Some(stamp) = stamp
        && let Ok(mut cache) = decoded().lock()
    {
        cache.insert(key, (stamp, Arc::clone(&image)));
    }
    Ok(ImageRef {
        path: path.to_owned(),
        image,
    })
}

/// An image from the bytes of a file; `path` says the format by its
/// extension.
#[cfg(feature = "images")]
pub fn decode_bytes(bytes: &[u8], path: &Path) -> Result<Image, String> {
    let format = image::ImageFormat::from_path(path)
        .or_else(|_| image::guess_format(bytes))
        .map_err(|_| "its format is not one Rill can read".to_owned())?;
    let decoded = image::load_from_memory_with_format(bytes, format)
        .map_err(|e| format!("it cannot be read as an image: {e}"))?;
    let alpha = decoded.color().has_alpha();
    let rgba = decoded.into_rgba32f();
    let (width, height) = (rgba.width() as usize, rgba.height() as usize);
    if width == 0 || height == 0 {
        return Err("it has no pixels".into());
    }
    let pixels = rgba
        .pixels()
        .map(|p| p.0.map(|v| if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 }))
        .collect();
    Ok(Image {
        width,
        height,
        alpha,
        pixels,
    })
}

#[cfg(not(feature = "images"))]
pub fn decode_bytes(_: &[u8], _: &Path) -> Result<Image, String> {
    Err("this build of Rill reads no images: it was built without the `images` feature".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> Image {
        // 2 by 2: black, white / red, blue.
        Image {
            width: 2,
            height: 2,
            alpha: false,
            pixels: vec![
                [0.0, 0.0, 0.0, 1.0],
                [1.0, 1.0, 1.0, 1.0],
                [1.0, 0.0, 0.0, 1.0],
                [0.0, 0.0, 1.0, 1.0],
            ],
        }
    }

    #[test]
    fn pixels_at_their_centres_and_between() {
        let img = image();
        assert_eq!(img.at(0, 0.5, 0.5), 0.0);
        assert_eq!(img.at(0, 1.5, 0.5), 1.0);
        assert_eq!(img.at(0, 1.0, 0.5), 0.5);
        assert_eq!(img.at(2, 1.5, 1.5), 1.0);
        // Held at the edges, and nothing outside.
        assert_eq!(img.at(0, 0.1, 0.1), 0.0);
        assert_eq!(img.at(0, 2.0, 0.5), 0.0);
        assert_eq!(img.at(0, -0.1, 0.5), 0.0);
    }

    #[test]
    fn extensions() {
        assert!(is_image("./face.png"));
        assert!(is_image("./a/B.JPEG"));
        assert!(!is_image("./kick.wav"));
    }
}
