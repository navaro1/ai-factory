//! Read one image from the platform clipboard and save it under the AIF
//! state root.
//!
//! A terminal cannot hand image bytes to a program, so image paste binds
//! `ctrl-v` to this reader: it asks `wl-paste` on a Wayland session or
//! `xclip` on an X11 session for the `image/png` target first and the
//! `image/jpeg` target second, names the media type from the magic bytes
//! alone, and saves one image as `<state root>/images/<uuid>.<ext>`. A
//! clipboard without an image, a missing platform tool, and a missing
//! display each fail with their own named error, and no error path writes
//! a file. No surface calls the module yet.
//!
//! The reader spawns the platform tool itself instead of going through
//! [`crate::exec::Exec`], because clipboard bytes are binary and `CmdOut`
//! decodes stdout as UTF-8 with replacements. Tests drive
//! [`PlatformClipboard::paste_with`] with a scripted byte source, so no
//! test runs a real tool.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::config;

/// The clipboard targets the reader tries, in order.
const TARGETS: [&str; 2] = ["image/png", "image/jpeg"];

/// The largest image the reader accepts: 10 MiB, the same order as the
/// opencode attach cap.
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// The media type of a pasted image, sniffed from its magic bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// A PNG image, sniffed from the `\x89PNG\r\n\x1a\n` signature.
    Png,
    /// A JPEG image, sniffed from the `ff d8 ff` signature.
    Jpeg,
}

impl ImageFormat {
    /// The file extension of the sniffed type.
    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
        }
    }
}

impl std::fmt::Display for ImageFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
        };
        f.write_str(name)
    }
}

/// Why the reader saved no image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardError {
    /// Neither `WAYLAND_DISPLAY` nor `DISPLAY` has a value, so the reader
    /// knows no platform tool to ask.
    NoDisplayServer,
    /// The platform reader program of the session is not installed.
    ReaderMissing {
        /// The program the session asked for.
        program: &'static str,
    },
    /// Both clipboard targets are unavailable or hold no image bytes.
    NoImage,
    /// The image bytes exceed [`MAX_IMAGE_BYTES`].
    TooLarge {
        /// The size of the rejected image in bytes.
        bytes: usize,
    },
}

impl std::fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClipboardError::NoDisplayServer => {
                write!(
                    f,
                    "no display server: WAYLAND_DISPLAY and DISPLAY are both unset"
                )
            }
            ClipboardError::ReaderMissing { program } => {
                write!(f, "the clipboard reader {program} is not installed")
            }
            ClipboardError::NoImage => {
                write!(f, "the clipboard holds no image/png or image/jpeg")
            }
            ClipboardError::TooLarge { bytes } => {
                write!(
                    f,
                    "the clipboard image holds {bytes} bytes, above the {}-byte cap",
                    MAX_IMAGE_BYTES
                )
            }
        }
    }
}

impl std::error::Error for ClipboardError {}

/// One image the reader saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastedImage {
    /// The absolute path of the saved image.
    pub path: PathBuf,
    /// The media type the magic bytes named.
    pub media: ImageFormat,
}

/// The paste capability: read one image from the clipboard and save it.
pub trait ClipboardReader {
    /// Read the clipboard image and save it under the state root.
    fn paste_image(&self) -> Result<PastedImage>;
}

/// The platform clipboard reader for a Wayland or an X11 session.
#[derive(Debug, Clone)]
pub struct PlatformClipboard {
    state_root: PathBuf,
}

impl PlatformClipboard {
    /// A reader that saves under the AIF state root.
    pub fn new() -> Self {
        Self {
            state_root: config::state_dir(),
        }
    }

    /// A reader that saves under an explicit root, for tests.
    pub fn with_state_root(state_root: PathBuf) -> Self {
        Self { state_root }
    }

    /// Paste through an injected byte source.
    ///
    /// The source answers one target name at a time. The loop tries
    /// `image/png` first and `image/jpeg` second, accepts the first target
    /// whose bytes carry an image signature, and saves exactly those bytes.
    fn paste_with(
        &self,
        fetch: &mut dyn FnMut(&str) -> Result<Vec<u8>, FetchError>,
    ) -> Result<PastedImage> {
        for target in TARGETS {
            let bytes = match fetch(target) {
                Ok(bytes) => bytes,
                Err(FetchError::Unavailable) => continue,
                Err(FetchError::Missing(program)) => {
                    return Err(ClipboardError::ReaderMissing { program }.into());
                }
            };
            if bytes.is_empty() {
                continue;
            }
            // The magic bytes decide the truth, never the target name the
            // tool answered.
            let Some(media) = sniff_media_type(&bytes) else {
                continue;
            };
            if bytes.len() > MAX_IMAGE_BYTES {
                return Err(ClipboardError::TooLarge { bytes: bytes.len() }.into());
            }
            let path = save_image(&self.state_root, &bytes, media)?;
            return Ok(PastedImage { path, media });
        }
        Err(ClipboardError::NoImage.into())
    }
}

impl Default for PlatformClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardReader for PlatformClipboard {
    fn paste_image(&self) -> Result<PastedImage> {
        let tool = select_tool(
            env_value("WAYLAND_DISPLAY").as_deref(),
            env_value("DISPLAY").as_deref(),
        )?;
        self.paste_with(&mut |target| spawn_target(tool, target))
    }
}

/// Name the media type of image bytes from the magic bytes alone.
pub fn sniff_media_type(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some(ImageFormat::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(ImageFormat::Jpeg)
    } else {
        None
    }
}

/// The platform clipboard reader of one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardTool {
    /// `wl-paste` serves a Wayland session.
    Wayland,
    /// `xclip` serves an X11 session.
    X11,
}

impl ClipboardTool {
    fn program(self) -> &'static str {
        match self {
            ClipboardTool::Wayland => "wl-paste",
            ClipboardTool::X11 => "xclip",
        }
    }
}

/// Pick the reader the session needs, from the display environment.
fn select_tool(
    wayland_display: Option<&str>,
    display: Option<&str>,
) -> std::result::Result<ClipboardTool, ClipboardError> {
    if wayland_display.is_some_and(|value| !value.is_empty()) {
        Ok(ClipboardTool::Wayland)
    } else if display.is_some_and(|value| !value.is_empty()) {
        Ok(ClipboardTool::X11)
    } else {
        Err(ClipboardError::NoDisplayServer)
    }
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// One probe of one clipboard target.
enum FetchError {
    /// The target is unavailable or the read failed; the loop tries the
    /// next target.
    Unavailable,
    /// The reader program of the session is not installed.
    Missing(&'static str),
}

/// Read one target from the platform tool and return the raw bytes.
fn spawn_target(tool: ClipboardTool, target: &str) -> Result<Vec<u8>, FetchError> {
    let mut command = Command::new(tool.program());
    match tool {
        ClipboardTool::Wayland => {
            command.args(["-t", target]);
        }
        ClipboardTool::X11 => {
            command.args(["-selection", "clipboard", "-t", target, "-o"]);
        }
    }
    let output = match command.output() {
        Ok(output) => output,
        // A missing reader is a named error; any other start failure only
        // loses this target.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(FetchError::Missing(tool.program()));
        }
        Err(_) => return Err(FetchError::Unavailable),
    };
    // A reader without the target, such as `xclip` with no image in the
    // clipboard, exits non-zero and names the target on stderr.
    if !output.status.success() {
        return Err(FetchError::Unavailable);
    }
    Ok(output.stdout)
}

/// Save the image as `<state root>/images/<uuid>.<ext>` and return the path.
fn save_image(state_root: &Path, bytes: &[u8], media: ImageFormat) -> Result<PathBuf> {
    let dir = state_root.join("images");
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let path = dir.join(format!("{}.{}", Uuid::new_v4(), media.extension()));
    fs::write(&path, bytes).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// One valid-looking PNG: the signature plus a payload.
    fn png_bytes() -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(b"png payload");
        bytes
    }

    /// One valid-looking JPEG: the signature plus a payload.
    fn jpeg_bytes() -> Vec<u8> {
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10];
        bytes.extend_from_slice(b"jpeg payload");
        bytes
    }

    /// A unique temporary state root for one test.
    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aif-paste-image-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("the temp dir must be creatable");
        dir
    }

    #[test]
    fn sniff_media_type_names_the_type_from_the_image_magic_bytes() {
        assert_eq!(sniff_media_type(&png_bytes()), Some(ImageFormat::Png));
        assert_eq!(sniff_media_type(&jpeg_bytes()), Some(ImageFormat::Jpeg));
        assert_eq!(sniff_media_type(b"a plain text snippet"), None);
        assert_eq!(sniff_media_type(&[]), None);
    }

    #[test]
    fn select_tool_prefers_wayland_then_x11_then_names_no_display() {
        assert_eq!(
            select_tool(Some("wayland-1"), Some(":1")),
            Ok(ClipboardTool::Wayland)
        );
        assert_eq!(select_tool(None, Some(":1")), Ok(ClipboardTool::X11));
        assert_eq!(
            select_tool(None, None),
            Err(ClipboardError::NoDisplayServer)
        );
        // An empty variable value counts as unset.
        assert_eq!(
            select_tool(Some(""), Some("")),
            Err(ClipboardError::NoDisplayServer)
        );
    }

    #[test]
    fn the_clipboard_image_errors_are_distinct_and_named() {
        let errors = [
            ClipboardError::NoDisplayServer,
            ClipboardError::ReaderMissing { program: "xclip" },
            ClipboardError::NoImage,
            ClipboardError::TooLarge {
                bytes: MAX_IMAGE_BYTES + 1,
            },
        ];
        let texts: Vec<String> = errors.iter().map(ToString::to_string).collect();
        let unique: std::collections::HashSet<&String> = texts.iter().collect();
        assert_eq!(unique.len(), texts.len(), "texts: {texts:?}");
        assert!(texts[0].contains("WAYLAND_DISPLAY"));
        assert!(texts[1].contains("xclip"));
        assert!(texts[2].contains("no image"));
        assert!(texts[3].contains("cap"));
    }

    #[test]
    fn paste_image_tries_png_then_jpeg_and_names_the_sniffed_type() {
        let dir = temp_dir("png-first");
        let reader = PlatformClipboard::with_state_root(dir.clone());
        let requested: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let png = png_bytes();
        let mut fetch = |target: &str| {
            requested.borrow_mut().push(target.to_string());
            if target == "image/png" {
                Ok(png.clone())
            } else {
                Err(FetchError::Unavailable)
            }
        };

        let pasted = reader.paste_with(&mut fetch).expect("the paste must save");

        assert_eq!(requested.into_inner(), ["image/png"]);
        assert_eq!(pasted.media, ImageFormat::Png);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn paste_image_falls_back_to_jpeg_when_png_holds_no_image() {
        let dir = temp_dir("jpeg-fallback");
        let reader = PlatformClipboard::with_state_root(dir.clone());
        let requested: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let jpeg = jpeg_bytes();
        let mut fetch = |target: &str| {
            requested.borrow_mut().push(target.to_string());
            if target == "image/jpeg" {
                Ok(jpeg.clone())
            } else {
                Err(FetchError::Unavailable)
            }
        };

        let pasted = reader.paste_with(&mut fetch).expect("the paste must save");

        assert_eq!(requested.into_inner(), ["image/png", "image/jpeg"]);
        assert_eq!(pasted.media, ImageFormat::Jpeg);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn paste_image_trusts_the_magic_bytes_over_the_target_name() {
        let dir = temp_dir("magic");
        let reader = PlatformClipboard::with_state_root(dir.clone());
        // The `image/png` target answers with JPEG bytes; the signature wins.
        let jpeg = jpeg_bytes();
        let mut fetch = move |_target: &str| Ok(jpeg.clone());

        let pasted = reader.paste_with(&mut fetch).expect("the paste must save");

        assert_eq!(pasted.media, ImageFormat::Jpeg);
        assert!(pasted.path.to_string_lossy().ends_with(".jpg"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn paste_image_saves_one_uuid_named_file_with_the_sniffed_extension() {
        let dir = temp_dir("save-name");
        let reader = PlatformClipboard::with_state_root(dir.clone());
        let png = png_bytes();
        let closure_bytes = png.clone();
        let mut fetch = move |_target: &str| Ok(closure_bytes.clone());

        let first = reader.paste_with(&mut fetch).expect("the paste must save");
        let second = reader
            .paste_with(&mut fetch)
            .expect("the second paste must save");

        for pasted in [&first, &second] {
            assert!(pasted.path.is_absolute(), "path: {}", pasted.path.display());
            let images_dir: &Path = &dir.join("images");
            assert_eq!(pasted.path.parent(), Some(images_dir));
            assert_eq!(pasted.media, ImageFormat::Png);
            let name = pasted
                .path
                .file_name()
                .expect("the save path must name a file")
                .to_string_lossy()
                .into_owned();
            assert!(name.ends_with(".png"), "name: {name}");
            assert_eq!(fs::read(&pasted.path).expect("the save must exist"), png);
        }
        assert_ne!(first.path, second.path, "each paste needs a fresh uuid");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn paste_image_without_an_image_fails_named_and_writes_no_file() {
        let dir = temp_dir("no-image");
        let reader = PlatformClipboard::with_state_root(dir.clone());
        let mut garbage = |_target: &str| Ok(b"a plain text snippet".to_vec());

        let error = reader.paste_with(&mut garbage).unwrap_err();
        assert!(error.to_string().contains("no image"), "error: {error}");
        assert!(
            !dir.join("images").exists(),
            "the reader must write no file"
        );

        let mut empty_targets = |_target: &str| Err(FetchError::Unavailable);
        let error = reader.paste_with(&mut empty_targets).unwrap_err();
        assert!(error.to_string().contains("no image"), "error: {error}");
        assert!(
            !dir.join("images").exists(),
            "the reader must write no file"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn paste_image_names_a_missing_platform_tool_and_writes_no_file() {
        let dir = temp_dir("missing-tool");
        let reader = PlatformClipboard::with_state_root(dir.clone());
        let mut fetch = |_target: &str| Err(FetchError::Missing("wl-paste"));

        let error = reader.paste_with(&mut fetch).unwrap_err();

        assert!(
            error.to_string().contains("wl-paste is not installed"),
            "error: {error}"
        );
        assert!(
            !dir.join("images").exists(),
            "the reader must write no file"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn paste_image_above_the_size_cap_fails_named_and_writes_no_file() {
        let dir = temp_dir("too-large");
        let reader = PlatformClipboard::with_state_root(dir.clone());
        let mut bytes = png_bytes();
        bytes.resize(MAX_IMAGE_BYTES + 1, 0);
        let mut fetch = move |_target: &str| Ok(bytes.clone());

        let error = reader.paste_with(&mut fetch).unwrap_err();

        assert!(error.to_string().contains("cap"), "error: {error}");
        assert!(
            !dir.join("images").exists(),
            "the reader must write no file"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
