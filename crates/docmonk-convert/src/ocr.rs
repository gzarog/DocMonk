//! OCR with the pure-Rust `ocrs` engine.
//!
//! Models are not bundled. They are loaded from a directory
//! (`RAGMONK_OCR_MODELS_DIR`, else `<home>/models/ocrs`) and verified
//! against pinned SHA-256 digests before use. Missing or modified models
//! make OCR unavailable: callers keep the plain conversion: OCR is
//! best effort.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

pub const OCR_MODEL_MANIFEST: &[(&str, &str)] = &[
    (
        "text-detection.rten",
        "f15cfb56bd02c4bf478a20343986504a1f01e1665c2b3a0ad66340f054b1b5ca",
    ),
    (
        "text-recognition.rten",
        "e484866d4cce403175bd8d00b128feb08ab42e208de30e42cd9889d8f1735a6e",
    ),
];

/// Where the pinned models can be downloaded from (documentation/tests).
pub const OCR_MODEL_BASE_URL: &str = "https://ocrs-models.s3-accelerate.amazonaws.com";

/// OCR engine identity, part of the conversion cache key.
pub const OCR_ENGINE_VERSION: &str = "ocrs-0.12.0";

pub struct Ocr {
    engine: ocrs::OcrEngine,
}

impl std::fmt::Debug for Ocr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ocr")
    }
}

fn load_verified(dir: &Path, name: &str, digest: &str) -> Result<rten::Model, String> {
    let path = dir.join(name);
    let bytes = std::fs::read(&path)
        .map_err(|e| format!("OCR model {} unavailable: {e}", path.display()))?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != digest {
        return Err(format!(
            "OCR model {} failed integrity verification (expected sha256 {digest}, got {actual})",
            path.display()
        ));
    }
    rten::Model::load(bytes)
        .map_err(|e| format!("OCR model {} failed to load: {e}", path.display()))
}

impl Ocr {
    pub fn load(dir: &Path) -> Result<Self, String> {
        let det = load_verified(dir, OCR_MODEL_MANIFEST[0].0, OCR_MODEL_MANIFEST[0].1)?;
        let rec = load_verified(dir, OCR_MODEL_MANIFEST[1].0, OCR_MODEL_MANIFEST[1].1)?;
        let engine = ocrs::OcrEngine::new(ocrs::OcrEngineParams {
            detection_model: Some(det),
            recognition_model: Some(rec),
            ..Default::default()
        })
        .map_err(|e| format!("OCR engine failed to start: {e}"))?;
        Ok(Self { engine })
    }

    /// Recognized text of one image (lines separated by `\n`).
    pub fn image_text(&self, img: image::DynamicImage) -> Result<String, String> {
        let rgb = img.into_rgb8();
        let (w, h) = rgb.dimensions();
        if w == 0 || h == 0 {
            return Ok(String::new());
        }
        let src = ocrs::ImageSource::from_bytes(rgb.as_raw(), (w, h)).map_err(|e| e.to_string())?;
        let input = self.engine.prepare_input(src).map_err(|e| e.to_string())?;
        self.engine.get_text(&input).map_err(|e| e.to_string())
    }
}

/// The OCR models directory for this process.
pub fn models_dir(home_models: Option<&Path>) -> PathBuf {
    std::env::var_os("RAGMONK_OCR_MODELS_DIR")
        .map(PathBuf::from)
        .or_else(|| home_models.map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("models/ocrs"))
}

/// Lazily loaded shared engine; `Err` (logged once) when unavailable.
pub struct LazyOcr {
    dir: PathBuf,
    cell: OnceLock<Result<Ocr, String>>,
}

impl LazyOcr {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            cell: OnceLock::new(),
        }
    }

    pub fn get(&self) -> Result<&Ocr, &str> {
        self.cell
            .get_or_init(|| {
                let r = Ocr::load(&self.dir);
                if let Err(e) = &r {
                    tracing::warn!(component = "ocr", event = "ocr_unavailable", error = %e);
                }
                r
            })
            .as_ref()
            .map_err(String::as_str)
    }
}
