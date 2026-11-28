//! An image a session entry holds, as a viewer opens it.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// One image of an entry, borrowed from the entry's value. The bytes
/// stay encoded until a viewer asks for them, so that listing an
/// entry's images copies nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryImage<'a> {
    /// The media type the harness recorded, such as `image/png`
    pub media_type: Option<&'a str>,
    /// The image's bytes as base64 text
    pub base64: &'a str,
}

impl EntryImage<'_> {
    /// The number of bytes the base64 text decodes to.
    pub fn size(&self) -> usize {
        let padding = self.base64.bytes().rev().take_while(|b| *b == b'=').count();
        (self.base64.len() / 4 * 3).saturating_sub(padding)
    }

    /// The image's bytes.
    pub fn decode(&self) -> Result<Vec<u8>, base64::DecodeError> {
        STANDARD.decode(self.base64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(base64: &str) -> EntryImage<'_> {
        EntryImage {
            media_type: None,
            base64,
        }
    }

    #[test]
    fn size_is_the_decoded_length() {
        for encoded in ["", "YQ==", "YWI=", "YWJj", "YWJjZA=="] {
            let image = image(encoded);
            assert_eq!(image.size(), image.decode().unwrap().len(), "{encoded}");
        }
    }

    #[test]
    fn text_that_is_not_base64_does_not_decode() {
        assert!(image("not base64!").decode().is_err());
    }
}
