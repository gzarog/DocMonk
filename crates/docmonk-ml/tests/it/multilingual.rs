//! Compatibility spike for the multilingual embedding candidates
//! (ADR 0040): each pinned model loads into the in-process BERT, its
//! SentencePiece tokenizer runs, vectors have the declared size, and
//! translations land closer together than unrelated text. Needs the
//! candidates under `DOCMONK_MODELS_DIR`; skipped otherwise.

use std::path::PathBuf;

use docmonk_ml::embedder::{cosine, Embedder};
use docmonk_ml::manifest::{
    EmbeddingModelSpec, ALL_MINILM_L6_V2, MULTILINGUAL_E5_SMALL,
    PARAPHRASE_MULTILINGUAL_MINILM_L12_V2,
};

fn load(spec: EmbeddingModelSpec) -> Option<Embedder> {
    let dir = std::env::var_os("DOCMONK_MODELS_DIR")
        .map(PathBuf::from)?
        .join(spec.slug);
    if !dir.join("model.safetensors").exists() {
        eprintln!("skipping {}: not installed", spec.slug);
        return None;
    }
    Some(Embedder::load(&dir, spec).expect("a pinned candidate loads"))
}

/// (English query, its translation, an unrelated text in the same
/// language as the translation).
const PAIRS: &[(&str, &str, &str)] = &[
    (
        "how long is parental leave",
        "Η γονική άδεια διαρκεί δεκατέσσερις εβδομάδες για κάθε γονέα.",
        "Οι κωδικοί πρόσβασης πρέπει να έχουν τουλάχιστον δεκαέξι χαρακτήρες.",
    ),
    (
        "how long are database backups kept",
        "生产数据库每天凌晨两点进行全量备份，备份文件保留三十天。",
        "每季度至少进行一次恢复演练，演练结果需提交给运维负责人审核。",
    ),
    (
        "how long is the security training for new hires",
        "研修の所要時間は九十分です。",
        "社員証は人事部で受け取ってください。",
    ),
    (
        "two-factor authentication for email accounts",
        "يجب على جميع الموظفين تفعيل المصادقة الثنائية على حسابات البريد الإلكتروني.",
        "يتم قفل الجهاز تلقائيا بعد عشر دقائق من عدم الاستخدام.",
    ),
    (
        "when are application logs deleted",
        "Anwendungsprotokolle werden nach dreißig Tagen gelöscht.",
        "Kundendaten dürfen nur in Rechenzentren der EU gespeichert werden.",
    ),
];

/// Pairs where the translation scores above the unrelated text.
fn cross_lingual_wins(e: &Embedder) -> usize {
    PAIRS
        .iter()
        .filter(|(q, t, other)| {
            let qv = &e.embed_queries(&[q]).unwrap()[0];
            let v = e.embed(&[t, other]).unwrap();
            cosine(qv, &v[0]) > cosine(qv, &v[1])
        })
        .count()
}

#[test]
fn multilingual_candidates_load_and_align_translations() {
    for spec in [MULTILINGUAL_E5_SMALL, PARAPHRASE_MULTILINGUAL_MINILM_L12_V2] {
        let Some(e) = load(spec) else { continue };
        let v = e
            .embed(&["Ελληνικά", "日本語のテキスト", "plain English"])
            .unwrap();
        assert!(v.iter().all(|x| x.len() == spec.dims), "{}", spec.slug);
        // Deterministic: the same text, the same vector.
        assert_eq!(e.embed(&["Ελληνικά"]).unwrap()[0], v[0]);
        let wins = cross_lingual_wins(&e);
        eprintln!("{}: {wins}/{} translations closer", spec.slug, PAIRS.len());
        assert!(
            wins * 5 >= PAIRS.len() * 4,
            "{}: {wins}/{}",
            spec.slug,
            PAIRS.len()
        );
    }
    if let Some(e) = load(ALL_MINILM_L6_V2) {
        // The English-only default, for comparison (not asserted).
        eprintln!(
            "{}: {}/{} translations closer",
            ALL_MINILM_L6_V2.slug,
            cross_lingual_wins(&e),
            PAIRS.len()
        );
    }
}
