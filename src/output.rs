//! Where `rill render` writes, and how: `--out song.wav:i16`.
//!
//! Options for the file follow the last `:`, which Windows does not allow
//! in a file name. They count as options only when the part before them is
//! a WAV file and the whole is not; otherwise it is all a path, so
//! `notes:take.wav` and `C:\out\song.wav` are files.

use std::path::PathBuf;

use crate::wav::WavFormat;

/// What to write, and where.
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    pub path: PathBuf,
    pub format: WavFormat,
}

/// Whether `path` names a WAV file.
fn is_wav(path: &str) -> bool {
    crate::audio::extension(path).is_some_and(|e| e == "wav" || e == "wave")
}

/// `--out`, as written.
pub fn parse(arg: &str) -> Result<Output, String> {
    let (path, options) = match arg.rsplit_once(':') {
        Some((path, options)) if is_wav(path) && !is_wav(arg) => (path, options),
        _ => (arg, ""),
    };
    if !is_wav(path) {
        return Err(format!(
            "`{path}` is not a file `render` writes: give it `.wav`"
        ));
    }
    let mut format = WavFormat::Float32;
    for o in options.split(',').filter(|o| !o.is_empty()) {
        format = match o {
            "f32" => WavFormat::Float32,
            "i16" => WavFormat::Pcm16,
            "i24" => WavFormat::Pcm24,
            other => {
                return Err(format!(
                    "a WAV file takes `f32`, `i16` or `i24` after `:`, not `{other}`"
                ));
            }
        };
    }
    Ok(Output {
        path: PathBuf::from(path),
        format,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_options() {
        assert_eq!(
            parse("song.wav").unwrap(),
            Output {
                path: "song.wav".into(),
                format: WavFormat::Float32
            }
        );
        assert_eq!(
            parse("out/song.wav:i16").unwrap(),
            Output {
                path: "out/song.wav".into(),
                format: WavFormat::Pcm16
            }
        );
        assert_eq!(parse("a.wav:i24").unwrap().format, WavFormat::Pcm24);
        // A `:` in a file name, and a Windows drive.
        assert_eq!(
            parse("notes:take.wav").unwrap().path,
            PathBuf::from("notes:take.wav")
        );
        assert_eq!(
            parse("C:\\out\\song.wav").unwrap().path,
            PathBuf::from("C:\\out\\song.wav")
        );
    }

    #[test]
    fn mistakes() {
        let e = |arg: &str| parse(arg).unwrap_err();
        assert!(e("song.mp3").starts_with("`song.mp3` is not a file `render` writes"));
        assert!(e("song.png").starts_with("`song.png` is not a file `render` writes"));
        assert!(e("song.wav:1000x500").starts_with("a WAV file takes"));
    }
}
