use std::borrow::Cow;

use arboard::{Clipboard, Error as ArboardError, ImageData};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl ClipboardImage {
    pub fn png(&self) -> Result<Vec<u8>, ClipboardError> {
        encode_png(self.width, self.height, &self.rgba)
    }
}

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("system clipboard is unavailable: {0}")]
    Unavailable(String),
    #[error("clipboard backend error: {0}")]
    Backend(String),
}

/// Real OS clipboard. Text only for now; image and file URI support plug into
/// the same pipeline later.
pub struct SystemClipboard {
    inner: Clipboard,
}

impl SystemClipboard {
    pub fn new() -> Result<Self, ClipboardError> {
        Ok(Self {
            inner: Clipboard::new()
                .map_err(|error| ClipboardError::Unavailable(error.to_string()))?,
        })
    }

    pub fn read_text(&mut self) -> Result<Option<String>, ClipboardError> {
        match self.inner.get_text() {
            Ok(text) => Ok(Some(text)),
            Err(ArboardError::ContentNotAvailable) => Ok(None),
            Err(error) => Err(ClipboardError::Backend(error.to_string())),
        }
    }

    pub fn write_text(&mut self, text: &str) -> Result<(), ClipboardError> {
        self.inner
            .set_text(text)
            .map_err(|error| ClipboardError::Backend(error.to_string()))
    }

    pub fn read_image(&mut self) -> Result<Option<ClipboardImage>, ClipboardError> {
        match self.inner.get_image() {
            Ok(image) => {
                let width = u32::try_from(image.width)
                    .map_err(|_| ClipboardError::Backend("image width overflow".to_owned()))?;
                let height = u32::try_from(image.height)
                    .map_err(|_| ClipboardError::Backend("image height overflow".to_owned()))?;
                Ok(Some(ClipboardImage {
                    rgba: image.bytes.into_owned(),
                    width,
                    height,
                }))
            }
            Err(ArboardError::ContentNotAvailable) => Ok(None),
            Err(error) => Err(ClipboardError::Backend(error.to_string())),
        }
    }

    pub fn write_image(&mut self, png: &[u8]) -> Result<(), ClipboardError> {
        let (width, height, rgba) = decode_png(png)?;
        self.inner
            .set_image(ImageData {
                width,
                height,
                bytes: Cow::Owned(rgba),
            })
            .map_err(|error| ClipboardError::Backend(error.to_string()))
    }

    pub fn clear(&mut self) -> Result<(), ClipboardError> {
        self.inner
            .clear()
            .map_err(|error| ClipboardError::Backend(error.to_string()))
    }

    pub fn has_text(&mut self) -> Result<bool, ClipboardError> {
        Ok(self.read_text()?.is_some())
    }
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|error| ClipboardError::Backend(format!("PNG header: {error}")))?;
        writer
            .write_image_data(rgba)
            .map_err(|error| ClipboardError::Backend(format!("PNG encode: {error}")))?;
    }
    Ok(bytes)
}

fn decode_png(png: &[u8]) -> Result<(usize, usize, Vec<u8>), ClipboardError> {
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let mut reader = decoder
        .read_info()
        .map_err(|error| ClipboardError::Backend(format!("PNG header: {error}")))?;
    let mut buffer = vec![
        0_u8;
        reader.output_buffer_size().ok_or_else(|| {
            ClipboardError::Backend("PNG image is too large".to_owned())
        })?
    ];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|error| ClipboardError::Backend(format!("PNG decode: {error}")))?;
    let raw = &buffer[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => raw.to_vec(),
        png::ColorType::Rgb => raw
            .chunks_exact(3)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
            .collect(),
        png::ColorType::Grayscale => raw
            .iter()
            .flat_map(|value| [*value, *value, *value, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => raw
            .chunks_exact(2)
            .flat_map(|pixel| [pixel[0], pixel[0], pixel[0], pixel[1]])
            .collect(),
        png::ColorType::Indexed => {
            return Err(ClipboardError::Backend(
                "indexed PNG was not expanded".to_owned(),
            ))
        }
    };
    Ok((
        usize::try_from(info.width)
            .map_err(|_| ClipboardError::Backend("PNG width overflow".to_owned()))?,
        usize::try_from(info.height)
            .map_err(|_| ClipboardError::Backend("PNG height overflow".to_owned()))?,
        rgba,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_png_roundtrips() {
        let rgba = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 10, 20, 30, 40,
        ];
        let image = ClipboardImage {
            rgba: rgba.clone(),
            width: 2,
            height: 2,
        };
        let png = image.png().unwrap();
        let (width, height, decoded) = decode_png(&png).unwrap();
        assert_eq!((width, height), (2, 2));
        assert_eq!(decoded, rgba);
    }

    #[test]
    fn rgb_png_expands_to_rgba() {
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 1, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[10, 20, 30]).unwrap();
        }
        let (width, height, decoded) = decode_png(&png).unwrap();
        assert_eq!((width, height), (1, 1));
        assert_eq!(decoded, vec![10, 20, 30, 255]);
    }
}
