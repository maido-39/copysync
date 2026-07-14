//! OS clipboard access via `arboard` (text + images). Requires a display at
//! runtime (X11/Wayland), so it is never touched by the headless tests.

use anyhow::Result;

/// An RGBA image lifted from / pushed to the clipboard.
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// Bounded open-retry for arboard. `arboard::Clipboard::new()` can transiently fail
/// when another app (or the RDP redirector) is mid-write and still holds the
/// clipboard — the *set* path already uses `clipboard_win::new_attempts(10)`, but
/// the read path had no retry and would silently skip a just-copied generation.
/// Retries ~5×40ms (~200ms) before giving up; logs the final failure to the
/// engine debug log so a contended/locked clipboard is observable, not lost.
const OPEN_ATTEMPTS: u32 = 5;
const OPEN_BACKOFF: std::time::Duration = std::time::Duration::from_millis(40);

// Standard Win32 clipboard format ids (stable ABI). Used with the no-open
// `IsClipboardFormatAvailable` probe so we never open the clipboard just to learn
// a format is absent — opening contends with the RDP redirector (`rdpclip`) and
// with the app that owns the clipboard, which is the #1 cause of copies being
// dropped across an RDP boundary or in clipboard-sensitive apps.
#[cfg(windows)]
mod cf {
    pub const CF_TEXT: u32 = 1;
    pub const CF_DIB: u32 = 8;
    pub const CF_UNICODETEXT: u32 = 13;
    pub const CF_DIBV5: u32 = 17;
}

/// Is a text format advertised on the clipboard? A pure format probe — does NOT
/// open the clipboard and does NOT force a delayed-render. Always true off Windows
/// (there the read path polls content directly every tick).
#[cfg(windows)]
fn text_available() -> bool {
    use clipboard_win::raw::is_format_avail;
    is_format_avail(cf::CF_UNICODETEXT) || is_format_avail(cf::CF_TEXT)
}

/// Is a bitmap/image format advertised? Same no-open, no-render semantics.
#[cfg(windows)]
fn image_available() -> bool {
    use clipboard_win::raw::is_format_avail;
    is_format_avail(cf::CF_DIBV5) || is_format_avail(cf::CF_DIB)
}

fn open_clipboard(what: &str) -> Result<arboard::Clipboard> {
    let mut last_err = None;
    for attempt in 1..=OPEN_ATTEMPTS {
        match arboard::Clipboard::new() {
            Ok(c) => {
                if attempt > 1 {
                    crate::engine::debug_log(
                        "clipboard",
                        &format!("open for {what}: succeeded on attempt {attempt}/{OPEN_ATTEMPTS}"),
                    );
                }
                return Ok(c);
            }
            Err(e) => {
                last_err = Some(e);
                if attempt < OPEN_ATTEMPTS {
                    std::thread::sleep(OPEN_BACKOFF);
                }
            }
        }
    }
    let e = last_err.expect("at least one open attempt failed");
    crate::engine::debug_log(
        "clipboard",
        &format!("open for {what}: FAILED after {OPEN_ATTEMPTS} attempts: {e}"),
    );
    Err(anyhow::anyhow!("open clipboard for {what}: {e}"))
}

pub fn get_text() -> Result<String> {
    // Windows: if no text format is advertised, report "not available" WITHOUT
    // opening the clipboard. arboard would also return an error here — but only
    // AFTER opening, which needlessly contends with rdpclip / the owning app on
    // every non-text copy. The caller (clipboard_loop) treats this Err exactly as
    // it treats arboard's ContentNotAvailable, so behavior is unchanged.
    #[cfg(windows)]
    if !text_available() {
        anyhow::bail!("no text format on clipboard");
    }
    Ok(open_clipboard("get_text")?.get_text()?)
}

pub fn set_text(s: &str) -> Result<()> {
    open_clipboard("set_text")?.set_text(s.to_string())?;
    Ok(())
}

/// Empty the OS clipboard (used by the auto-clear timer after receiving a clip).
pub fn clear() -> Result<()> {
    open_clipboard("clear")?.clear()?;
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
    // Windows: skip the clipboard open entirely when no bitmap format is present
    // (the common case on every text/file copy). Besides cutting rdpclip/app
    // contention, this avoids calling GetClipboardData(CF_DIB) when there is no
    // image — which for a delayed-render owner would force an unnecessary (and on
    // RDP, slow) render. Same Err the loop already handles, minus the open.
    #[cfg(windows)]
    if !image_available() {
        anyhow::bail!("no image format on clipboard");
    }
    let img = open_clipboard("get_image")?.get_image()?;
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
    // Fast path: if there is no CF_HDROP file list on the clipboard at all (the
    // overwhelmingly common case — every plain-text or image copy), bail out
    // immediately. `IsClipboardFormatAvailable` does not open the clipboard, so
    // it returns instantly and never contends. Without this guard the retry loop
    // below could not tell "no file list present" (a permanent miss) from
    // "clipboard transiently locked", and would burn all OPEN_ATTEMPTS × backoff
    // (~160ms) on every text/image copy before returning None.
    if !clipboard_win::raw::is_format_avail(clipboard_win::formats::CF_HDROP) {
        return None;
    }
    // A file list IS present — only now do the bounded open-retry, since
    // clipboard_win::get_clipboard opens the clipboard, which can be transiently
    // locked by another app / the RDP redirector.
    let mut files: Option<Vec<String>> = None;
    for attempt in 1..=OPEN_ATTEMPTS {
        match clipboard_win::get_clipboard::<Vec<String>, _>(clipboard_win::formats::FileList) {
            Ok(f) => { files = Some(f); break; }
            Err(e) => {
                if attempt < OPEN_ATTEMPTS {
                    std::thread::sleep(OPEN_BACKOFF);
                } else {
                    crate::engine::debug_log(
                        "clipboard",
                        &format!("get_files: clipboard read FAILED after {OPEN_ATTEMPTS} attempts: {e}"),
                    );
                }
            }
        }
    }
    let files = files?;
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
    open_clipboard("set_image")?.set_image(arboard::ImageData {
        width: img.width,
        height: img.height,
        bytes: std::borrow::Cow::Borrowed(&img.rgba),
    })?;
    Ok(())
}

/// Read the clipboard's rich-text (HTML) representation, if any.
pub fn get_html() -> Result<String> {
    // Windows: most text copies (Notepad, terminals, code editors) carry NO "HTML
    // Format", so probe for it before opening. Saves the second clipboard open on
    // every plain-text copy — again reducing rdpclip/app contention.
    #[cfg(windows)]
    {
        let avail = clipboard_win::raw::register_format("HTML Format")
            .map(|f| clipboard_win::raw::is_format_avail(f.get()))
            .unwrap_or(false);
        if !avail {
            anyhow::bail!("no HTML format on clipboard");
        }
    }
    Ok(open_clipboard("get_html")?.get().html()?)
}

/// Set both an HTML representation and a plain-text fallback (`alt`).
pub fn set_html(html: &str, alt: &str) -> Result<()> {
    open_clipboard("set_html")?
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
