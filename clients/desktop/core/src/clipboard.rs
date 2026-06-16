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

/// Empty the OS clipboard (used by the auto-clear timer after receiving a clip).
pub fn clear() -> Result<()> {
    arboard::Clipboard::new()?.clear()?;
    Ok(())
}

/// Set clipboard text and, on Windows, tag it so Clipboard History (Win+V) and
/// the cloud clipboard skip it — for received clips the user chose to treat as
/// sensitive. Off Windows this is just a plain text set.
#[cfg(windows)]
pub fn set_text_sensitive(s: &str) -> Result<()> {
    use clipboard_win::raw;
    let _clip = clipboard_win::Clipboard::new_attempts(10)
        .map_err(|e| anyhow::anyhow!("open clipboard: {e}"))?;
    raw::empty().map_err(|e| anyhow::anyhow!("empty clipboard: {e}"))?;
    raw::set_string(s).map_err(|e| anyhow::anyhow!("set text: {e}"))?;
    // The mere presence of this format tells clipboard managers to skip the clip.
    if let Some(f) = raw::register_format("ExcludeClipboardContentFromMonitorProcessing") {
        let _ = raw::set_without_clear(f.get(), &[0u8; 4]);
    }
    // DWORD 0 = opt out of the local history and the cloud clipboard.
    for name in ["CanIncludeInClipboardHistory", "CanUploadToCloudClipboard"] {
        if let Some(f) = raw::register_format(name) {
            let _ = raw::set_without_clear(f.get(), &0u32.to_ne_bytes());
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn set_text_sensitive(s: &str) -> Result<()> {
    set_text(s)
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

/// Clipboard change counter (Windows `GetClipboardSequenceNumber`); None elsewhere.
/// The watcher uses it to avoid re-opening an unchanged clipboard — on RDP/remote
/// sessions, repeatedly opening the clipboard contends with the redirector
/// (`rdpclip`) and drops copies.
#[cfg(windows)]
pub fn seq_num() -> Option<u32> {
    clipboard_win::raw::seq_num().map(|n| n.get())
}
#[cfg(not(windows))]
pub fn seq_num() -> Option<u32> {
    None
}

/// Names of the clipboard formats currently present (diagnostics) — e.g. spotting
/// that an RDP file copy offers `FileGroupDescriptorW`/`FileContents` instead of
/// `CF_HDROP`. Empty off Windows.
#[cfg(windows)]
pub fn list_formats() -> Vec<String> {
    let _clip = match clipboard_win::Clipboard::new_attempts(5) {
        Ok(c) => c,
        Err(_) => return vec!["<clipboard busy>".to_string()],
    };
    clipboard_win::raw::EnumFormats::new()
        .map(|f| clipboard_win::raw::format_name_big(f).unwrap_or_else(|| format!("#{f}")))
        .collect()
}
#[cfg(not(windows))]
pub fn list_formats() -> Vec<String> {
    Vec::new()
}

/// File names offered as virtual/streamed clipboard files (RDP, Outlook, archives)
/// via `CFSTR_FILEDESCRIPTORW` — which the `CF_HDROP`-based [`get_files`] cannot
/// see. Returns just the names; the bytes are streamed separately (via
/// `CFSTR_FILECONTENTS`) and are not pulled yet. None off Windows / when absent.
#[cfg(windows)]
pub fn get_virtual_file_names() -> Option<Vec<String>> {
    use clipboard_win::raw;
    let fmt = raw::register_format("FileGroupDescriptorW")?.get();
    let _clip = clipboard_win::Clipboard::new_attempts(5).ok()?;
    if !raw::is_format_avail(fmt) {
        return None;
    }
    let mut buf = Vec::new();
    raw::get_vec(fmt, &mut buf).ok()?;
    if buf.len() < 4 {
        return None;
    }
    // FILEGROUPDESCRIPTORW = u32 count, then count * FILEDESCRIPTORW (592 bytes
    // each; cFileName is 260 WCHARs at offset 72).
    let count = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    const DESC: usize = 592;
    const NAME_OFF: usize = 72;
    let mut names = Vec::new();
    for i in 0..count.min(64) {
        let base = 4 + i * DESC + NAME_OFF;
        let mut wide = Vec::new();
        let mut j = base;
        while j + 1 < buf.len() && (j - base) < 260 * 2 {
            let c = u16::from_le_bytes([buf[j], buf[j + 1]]);
            if c == 0 {
                break;
            }
            wide.push(c);
            j += 2;
        }
        if !wide.is_empty() {
            names.push(String::from_utf16_lossy(&wide));
        }
    }
    if names.is_empty() {
        None
    } else {
        Some(names)
    }
}
#[cfg(not(windows))]
pub fn get_virtual_file_names() -> Option<Vec<String>> {
    None
}

/// Put file paths on the clipboard (Windows Explorer paste = CF_HDROP), so a
/// received file is immediately pasteable. No-op off Windows.
#[cfg(windows)]
pub fn set_files(paths: &[String]) -> Result<()> {
    let _clip = clipboard_win::Clipboard::new_attempts(10)
        .map_err(|e| anyhow::anyhow!("open clipboard: {e}"))?;
    clipboard_win::raw::empty().map_err(|e| anyhow::anyhow!("empty clipboard: {e}"))?;
    clipboard_win::raw::set_file_list(paths).map_err(|e| anyhow::anyhow!("set CF_HDROP: {e}"))?;
    Ok(())
}

#[cfg(not(windows))]
pub fn set_files(_paths: &[String]) -> Result<()> {
    Ok(())
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
