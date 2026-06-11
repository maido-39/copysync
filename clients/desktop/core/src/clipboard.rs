//! OS clipboard access via `arboard` (text + images). Requires a display at
//! runtime (X11/Wayland), so it is never touched by the headless tests.

use anyhow::Result;

/// An RGBA image lifted from / pushed to the clipboard.
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

pub fn get_text() -> Result<String> {
    Ok(arboard::Clipboard::new()?.get_text()?)
}

pub fn set_text(s: &str) -> Result<()> {
    arboard::Clipboard::new()?.set_text(s.to_string())?;
    Ok(())
}

pub fn get_image() -> Result<Image> {
    let img = arboard::Clipboard::new()?.get_image()?;
    Ok(Image {
        width: img.width,
        height: img.height,
        rgba: img.bytes.into_owned(),
    })
}

/// File paths on the clipboard (Windows Explorer "copy file" = CF_HDROP). Returns
/// None on non-Windows, or when the clipboard holds no file list.
#[cfg(windows)]
pub fn get_files() -> Option<Vec<String>> {
    let files: Vec<String> = clipboard_win::get_clipboard(clipboard_win::formats::FileList).ok()?;
    if files.is_empty() {
        None
    } else {
        Some(files)
    }
}

#[cfg(not(windows))]
pub fn get_files() -> Option<Vec<String>> {
    None
}

pub fn set_image(img: &Image) -> Result<()> {
    arboard::Clipboard::new()?.set_image(arboard::ImageData {
        width: img.width,
        height: img.height,
        bytes: std::borrow::Cow::Borrowed(&img.rgba),
    })?;
    Ok(())
}

/// Read the clipboard's rich-text (HTML) representation, if any.
pub fn get_html() -> Result<String> {
    Ok(arboard::Clipboard::new()?.get().html()?)
}

/// Set both an HTML representation and a plain-text fallback (`alt`).
pub fn set_html(html: &str, alt: &str) -> Result<()> {
    arboard::Clipboard::new()?
        .set_html(html.to_string(), Some(alt.to_string()))?;
    Ok(())
}

/// Encode a raw RGBA clipboard image to PNG bytes (for the blob channel).
/// Pure CPU — no display needed, so it is unit-testable headlessly.
pub fn encode_png(img: &Image) -> Result<Vec<u8>> {
    let buf = image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.rgba.clone())
        .ok_or_else(|| anyhow::anyhow!("image dimensions do not match buffer"))?;
    let mut out = std::io::Cursor::new(Vec::new());
    buf.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

/// Decode PNG/JPEG/GIF/WebP/BMP bytes to a raw RGBA clipboard image.
pub fn decode_image(bytes: &[u8]) -> Result<Image> {
    let img = image::load_from_memory(bytes)?.to_rgba8();
    let (width, height) = (img.width() as usize, img.height() as usize);
    Ok(Image {
        width,
        height,
        rgba: img.into_raw(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_roundtrip() {
        let img = Image {
            width: 2,
            height: 2,
            rgba: vec![
                255, 0, 0, 255, 0, 255, 0, 255, // red, green
                0, 0, 255, 255, 255, 255, 0, 255, // blue, yellow
            ],
        };
        let png = encode_png(&img).unwrap();
        assert_eq!(&png[1..4], b"PNG");
        let back = decode_image(&png).unwrap();
        assert_eq!((back.width, back.height), (2, 2));
        assert_eq!(back.rgba, img.rgba);
    }
}
