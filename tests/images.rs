//! Images: every format read, `pixel`, their fields, and their errors. The
//! images are written by the tests themselves.

use std::path::{Path, PathBuf};

use rill::lang;
use rill::offline::{self, Blocks};
use rill::{Config, Engine};

const RATE: u32 = 48_000;

fn folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rill-images-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config(channels: usize) -> Config {
    Config {
        sample_rate: RATE,
        max_frames: 256,
        out_channels: channels,
    }
}

/// The first frame of `src`, written as `song.rill` in `dir`, one value per
/// channel.
fn first(dir: &Path, src: &str, channels: usize) -> Vec<f32> {
    let path = dir.join("song.rill");
    std::fs::write(&path, src).unwrap();
    let (map, result) = lang::load_file(
        &path,
        &lang::Disk,
        &config(channels),
        "main",
        &lang::build::Options::default(),
    );
    let (graph, _) = result.unwrap_or_else(|d| {
        panic!("{}", d.iter().map(|d| map.render(d)).collect::<String>())
    });
    let mut engine = Engine::new(graph, config(channels)).unwrap();
    offline::render(&mut engine, 1, &Blocks::Fixed(64))
}

fn errors(dir: &Path, src: &str) -> Vec<String> {
    let path = dir.join("song.rill");
    std::fs::write(&path, src).unwrap();
    let (_, result) = lang::load_file(
        &path,
        &lang::Disk,
        &config(1),
        "main",
        &lang::build::Options::default(),
    );
    match result {
        Ok(_) => Vec::new(),
        Err(d) => d.into_iter().filter(|d| d.is_error()).map(|d| d.message).collect(),
    }
}

/// 4 by 2: red, green, blue, white / black, grey, yellow, a half-clear red.
fn picture() -> image::RgbaImage {
    let px = [
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 255, 255],
        [0, 0, 0, 255],
        [128, 128, 128, 255],
        [255, 255, 0, 255],
        [255, 0, 0, 128],
    ];
    image::RgbaImage::from_fn(4, 2, |x, y| image::Rgba(px[(y * 4 + x) as usize]))
}

/// What `pixel` gives at each pixel's centre, as [r, g, b] per pixel.
fn read_back(dir: &Path, file: &str) -> Vec<[f32; 3]> {
    let mut got = Vec::new();
    for y in 0..2 {
        for x in 0..4 {
            let src = format!(
                "const PIC = ./{file}
                rill main() [Sample; 3] {{
                    let p = pixel(PIC, {x}.5, {y}.5)
                    return [p[0], p[1], p[2]]
                }}"
            );
            let v = first(dir, &src, 3);
            got.push([v[0], v[1], v[2]]);
        }
    }
    got
}

#[test]
fn every_format_reads_back() {
    let dir = folder("formats");
    let pic = picture();
    let rgb = image::DynamicImage::ImageRgba8(pic.clone()).to_rgb8();
    let float = image::DynamicImage::ImageRgba8(pic.clone()).to_rgb32f();
    // Lossless formats give the colours back exactly.
    let formats: [(&str, f32); 11] = [
        ("png", 1e-6),
        ("gif", 1e-6),
        ("webp", 1e-6),
        ("bmp", 1e-6),
        ("tga", 1e-6),
        ("ico", 1e-6),
        ("qoi", 1e-6),
        ("ppm", 1e-6),
        ("tiff", 1e-6),
        ("exr", 1e-3),
        ("hdr", 0.02),
    ];
    for (ext, tolerance) in formats {
        let file = format!("pic.{ext}");
        let path = dir.join(&file);
        match ext {
            "ppm" | "gif" => rgb.save(&path).unwrap(),
            "exr" | "hdr" => float.save(&path).unwrap(),
            _ => pic.save(&path).unwrap(),
        }
        let got = read_back(&dir, &file);
        for (i, g) in got.iter().enumerate() {
            let want = pic.as_raw()[i * 4..i * 4 + 3]
                .iter()
                .map(|v| *v as f32 / 255.0)
                .collect::<Vec<_>>();
            for c in 0..3 {
                assert!(
                    (g[c] - want[c]).abs() <= tolerance,
                    "{ext}: pixel {i} channel {c}: {} vs {}",
                    g[c],
                    want[c]
                );
            }
        }
    }
}

#[test]
fn jpeg_reads_back_nearly() {
    // JPEG blurs hard edges within its 8 by 8 blocks, so the test image is
    // two flat halves, each a whole block wide: orange, then sky blue.
    let dir = folder("jpeg");
    let pic = image::RgbImage::from_fn(16, 8, |x, _| {
        image::Rgb(if x < 8 { [230, 120, 20] } else { [90, 170, 230] })
    });
    pic.save(dir.join("pic.jpg")).unwrap();
    for (x, want) in [(3.5, [230, 120, 20]), (12.5, [90, 170, 230])] {
        let v = first(
            &dir,
            &format!("rill main() [Sample; 3] {{ let p = pixel(./pic.jpg, {x}, 4)\nreturn [p[0], p[1], p[2]] }}"),
            3,
        );
        for c in 0..3 {
            let w = want[c] as f32 / 255.0;
            assert!((v[c] - w).abs() < 0.03, "x {x} channel {c}: {} vs {w}", v[c]);
        }
    }
}

#[test]
fn pixels_blend_between_centres_and_fields_tell_the_size() {
    let dir = folder("fields");
    picture().save(dir.join("pic.png")).unwrap();
    image::DynamicImage::ImageRgba8(picture())
        .to_rgb8()
        .save(dir.join("flat.png"))
        .unwrap();
    let v = first(
        &dir,
        "const PIC = ./pic.png
        const FLAT = ./flat.png
        rill main() [Sample; 6] {
            let rows: [Float; PIC.height] = [1, 2]
            let half = pixel(PIC, 1, .5)
            return [
                PIC.width as Float,
                rows[1],
                half[0],
                pixel(PIC, 3.5, 1.5)[3],
                if PIC.alpha && !FLAT.alpha { 1 } else { 0 },
                pixel(PIC, 9, 9)[0] + pixel(PIC, -1, 0)[1],
            ]
        }",
        6,
    );
    // Halfway from red to green: red half, and the alpha of the clear red.
    let want = [4.0, 2.0, 0.5, 128.0 / 255.0, 1.0, 0.0];
    for (g, w) in v.iter().zip(want) {
        assert!((g - w).abs() < 1e-6, "{v:?}");
    }
}

#[test]
fn images_have_their_own_errors() {
    let dir = folder("errors");
    picture().save(dir.join("pic.png")).unwrap();
    let e = errors(&dir, "rill main() Sample { return pixel(./gone.png, 0, 0)[0] }");
    assert!(e.iter().any(|m| m.starts_with("cannot read `./gone.png`")), "{e:?}");
    let e = errors(&dir, "rill main() Sample { return pixel(./notes.txt, 0, 0)[0] }");
    assert_eq!(e, ["`./notes.txt` is not an audio file or an image"]);
    let e = errors(&dir, "rill main() Sample { state p: Image = ./pic.png\nreturn 0 }");
    assert!(e.contains(&"an image cannot be `state`".to_owned()), "{e:?}");
    let e = errors(&dir, "rill main(p: Image = ./pic.png) Sample { return 0 }");
    assert!(e.contains(&"`p` cannot be an image".to_owned()), "{e:?}");
    let e = errors(&dir, "rill main() Sample { return (./pic.png).depth }");
    assert_eq!(e, ["an image has no field `depth`"]);
    std::fs::write(dir.join("bad.png"), b"not a png").unwrap();
    let e = errors(&dir, "rill main() Sample { return pixel(./bad.png, 0, 0)[0] }");
    assert!(e.iter().any(|m| m.starts_with("cannot read `./bad.png`")), "{e:?}");
}

/// Render `frames` of `src` in `dir`, both channels.
fn render_stereo(dir: &Path, src: &str, frames: usize) -> [Vec<f32>; 2] {
    let path = dir.join("song.rill");
    std::fs::write(&path, src).unwrap();
    let (map, result) = lang::load_file(
        &path,
        &lang::Disk,
        &config(2),
        "main",
        &lang::build::Options::default(),
    );
    let (graph, _) = result.unwrap_or_else(|d| {
        panic!("{}", d.iter().map(|d| map.render(d)).collect::<String>())
    });
    let mut engine = Engine::new(graph, config(2)).unwrap();
    let out = offline::render(&mut engine, frames, &Blocks::Fixed(64));
    [
        out.iter().step_by(2).copied().collect(),
        out.iter().skip(1).step_by(2).copied().collect(),
    ]
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

fn crossings(x: &[f32]) -> f32 {
    let n = x.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    n as f32 * RATE as f32 / x.len() as f32
}

/// 32 by 8, black but for row `row` in `colour`.
fn line(path: &Path, row: u32, colour: [u8; 3]) {
    image::RgbImage::from_fn(32, 8, |_, y| image::Rgb(if y == row { colour } else { [0; 3] }))
        .save(path)
        .unwrap();
}

#[test]
fn a_painted_row_plays_at_its_pitch_on_its_side() {
    let dir = folder("painted");
    let play = |file: &str, extra: &str| {
        render_stereo(
            &dir,
            &format!(
                "import @gen/spectral
                rill main() [Sample; 2] {{ return painted<8>(./{file}, 1s{extra}) }}"
            ),
            24_000,
        )
    };
    // Row 2 of 8, from 15kHz down to 50Hz evenly in pitch.
    let want = 15_000.0 * (50.0f32 / 15_000.0).powf(2.0 / 7.0);
    line(&dir.join("white.png"), 2, [255, 255, 255]);
    let [l, r] = play("white.png", "");
    assert!((crossings(&l) - want).abs() < 3.0, "{} vs {want}", crossings(&l));
    assert!((rms(&l) - rms(&r)).abs() < 1e-6 && rms(&l) > 0.2);
    // Red is the left channel, blue the right.
    line(&dir.join("red.png"), 2, [255, 0, 0]);
    let [l, r] = play("red.png", "");
    assert!(rms(&l) > 0.2 && rms(&r) == 0.0);
    line(&dir.join("blue.png"), 2, [0, 0, 255]);
    let [l, r] = play("blue.png", "");
    assert!(rms(&l) == 0.0 && rms(&r) > 0.2);
    // Mono plays the brightness in both.
    let [l, r] = play("red.png", ", mono: true");
    assert!(rms(&l) > 0.0 && rms(&l) == rms(&r));
}

#[test]
fn a_size_used_only_in_the_body_is_given_at_the_call() {
    let dir = folder("sizes");
    line(&dir.join("x.png"), 0, [255; 3]);
    let e = errors(
        &dir,
        "import @gen/spectral\nrill main() Sample { return painted(./x.png, 1s)[0] }",
    );
    assert!(
        e.iter().any(|m| m.starts_with("`painted` needs its size `R` written here")),
        "{e:?}"
    );
}
