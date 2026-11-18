//! PDF pages as PNG images, for AI providers that read images but take no
//! PDF input (the ChatGPT subscription through codex, ADR 0045).
//!
//! Rendering uses hayro, a pure-Rust rasterizer (no system libraries, no
//! network, no unsafe code). Each page is scaled so its longer side is at
//! most [`MAX_SIDE_PX`] pixels, enough for a vision model to read body text,
//! tables and scans. A renderer panic on a hostile file is contained and
//! reported as an error for that file only.

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{render, PixmapSettings, RenderCache, RenderSettings};

/// The longer side of a rendered page, in pixels.
pub const MAX_SIDE_PX: f32 = 1600.0;
/// Never upscale a small page beyond this factor (points to pixels).
const MAX_SCALE: f32 = 3.0;

/// The renderer identity, part of the analysis cache key: a change of
/// renderer or resolution re-analyses PDFs sent as images.
pub const PAGE_RENDER_VERSION: &str = "hayro-0.9+1600px";

/// Renders the first `max_pages` pages of `bytes` to PNG, in page order.
pub fn pdf_pages_png(bytes: &[u8], max_pages: usize) -> Result<Vec<Vec<u8>>, String> {
    let data = bytes.to_vec();
    std::panic::catch_unwind(move || render_pages(data, max_pages))
        .map_err(|_| "the PDF renderer failed on this file".to_owned())?
}

fn render_pages(data: Vec<u8>, max_pages: usize) -> Result<Vec<Vec<u8>>, String> {
    let pdf = Pdf::new(data).map_err(|e| format!("cannot open the PDF for rendering: {e:?}"))?;
    let cache = RenderCache::new();
    let interpreter = InterpreterSettings::default();
    let settings = RenderSettings::default();
    let mut out = Vec::new();
    for page in pdf.pages().iter().take(max_pages) {
        let (w, h) = page.render_dimensions();
        let longer = w.max(h).max(1.0);
        let scale = (MAX_SIDE_PX / longer).min(MAX_SCALE);
        let pixmap = render(
            page,
            &cache,
            &interpreter,
            &settings,
            &PixmapSettings {
                x_scale: scale,
                y_scale: scale,
                bg_color: WHITE,
            },
        );
        out.push(
            pixmap
                .into_png()
                .map_err(|e| format!("cannot encode a rendered page: {e}"))?,
        );
    }
    if out.is_empty() {
        return Err("the PDF has no pages to render".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_pdf_pages_render_to_bounded_pngs() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/documents/five_pages.pdf");
        let pages = pdf_pages_png(&std::fs::read(p).unwrap(), 3).unwrap();
        assert_eq!(pages.len(), 3, "capped at max_pages");
        for png in &pages {
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
            let img = image::load_from_memory(png).unwrap();
            assert!(img.width().max(img.height()) as f32 <= MAX_SIDE_PX + 1.0);
            assert!(img.width().max(img.height()) > 500, "readable resolution");
        }
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(pdf_pages_png(b"%PDF-1.4 not really", 5).is_err());
        assert!(pdf_pages_png(b"", 5).is_err());
    }
}
