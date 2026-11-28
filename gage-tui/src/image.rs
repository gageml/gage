//! Opening an entry's images from the session view: one image goes to
//! the desktop's viewer, and several are listed for the user to pick
//! from.

use gage_core::human::format_size;
use gage_session::EntryImage;
use ratatui::text::Span;

use crate::desktop::Desktop;
use crate::picker::{PickColumn, PickItem, Picker};
use crate::styles;

/// Shown in place of a viewer when the process has no display on the
/// machine it runs on.
pub(crate) const NO_DISPLAY: &str = "Images open in an image viewer on the machine Gage runs on. \
This terminal has no local display, so the image cannot be shown.";

/// A picker of `images`, one row each, whose row id is the image's
/// position among them.
pub(crate) fn picker(images: &[EntryImage]) -> Picker {
    let columns = vec![
        PickColumn::right("#", 3),
        PickColumn::fit("Type", 24),
        PickColumn::right("Size", 9),
    ];
    let items = images
        .iter()
        .enumerate()
        .map(|(position, image)| PickItem {
            id: position.to_string(),
            cells: vec![
                Span::styled((position + 1).to_string(), styles::Text::dim()),
                Span::raw(image.media_type.unwrap_or("image").to_string()),
                Span::raw(format_size(i64::try_from(image.size()).unwrap_or(i64::MAX))),
            ],
        })
        .collect();
    Picker::new("Images", columns, items, None)
}

/// Show `image` in the desktop's viewer. The error is a message for
/// the user.
pub(crate) fn open(desktop: &dyn Desktop, image: &EntryImage) -> Result<(), String> {
    let bytes = image
        .decode()
        .map_err(|e| format!("The image cannot be decoded: {e}."))?;
    let extension = extension(&bytes).ok_or_else(|| {
        let recorded = image.media_type.unwrap_or("no type");
        format!("The image is in a format Gage does not open ({recorded}).")
    })?;
    desktop
        .open_file(&bytes, extension)
        .map_err(|e| format!("The image viewer cannot be started: {e}."))
}

/// The file extension of the image format `bytes` begin as. The
/// session's own media type is a label the session's author wrote, so
/// the name the desktop picks a program by comes from the bytes.
fn extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("webp")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::RecordingDesktop;

    // The eight bytes every PNG file begins with
    const PNG: &str = "iVBORw0KGgo=";

    fn image(base64: &str) -> EntryImage<'_> {
        EntryImage {
            media_type: Some("text/html"),
            base64,
        }
    }

    #[test]
    fn the_extension_follows_the_bytes() {
        assert_eq!(extension(b"\x89PNG\r\n\x1a\n...."), Some("png"));
        assert_eq!(extension(b"\xff\xd8\xff\xe0"), Some("jpg"));
        assert_eq!(extension(b"GIF89a.."), Some("gif"));
        assert_eq!(extension(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("webp"));
        assert_eq!(extension(b"<html>"), None);
        assert_eq!(extension(b""), None);
    }

    #[test]
    fn open_passes_the_decoded_bytes_and_their_extension_to_the_desktop() {
        let desktop = RecordingDesktop::default();
        open(&desktop, &image(PNG)).unwrap();
        let opened = desktop.opened.lock().unwrap();
        assert_eq!(
            *opened,
            [(b"\x89PNG\r\n\x1a\n".to_vec(), "png".to_string())]
        );
    }

    #[test]
    fn open_reports_bytes_that_are_not_an_image_and_opens_nothing() {
        let desktop = RecordingDesktop::default();
        // "<html>" in base64
        let error = open(&desktop, &image("PGh0bWw+")).unwrap_err();
        assert!(error.contains("text/html"), "{error}");
        let error = open(&desktop, &image("not base64!")).unwrap_err();
        assert!(error.contains("decoded"), "{error}");
        assert!(desktop.opened.lock().unwrap().is_empty());
    }

    #[test]
    fn open_reports_a_viewer_that_does_not_start() {
        let desktop = RecordingDesktop {
            fail: true,
            ..RecordingDesktop::default()
        };
        let error = open(&desktop, &image(PNG)).unwrap_err();
        assert!(error.contains("no opener"), "{error}");
    }
}
