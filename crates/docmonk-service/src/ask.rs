//! `ask`: an AI answer grounded in the evidence `explore` retrieves.
//!
//! Grounding (ADR 0033, P0-R03): the question first runs through the
//! routed, filtered and verified `evidence` pipeline. When no part of it is
//! supported by evidence, `ask` abstains with an `insufficient_evidence`
//! result and never calls the provider. Otherwise the provider receives
//! only verified evidence, each item with a stable `[E#]` id, as untrusted
//! data; the answer's citations are then checked: unknown ids are reported
//! as invalid and substantive uncited sentences as unsupported.

use docmonk_ai::AiRequest;
use docmonk_core::errors::DocMonkError;
use docmonk_retrieval::grounding;
use serde_json::{json, Value};

use crate::evidence::{evidence_value, EvidenceRequest};
use crate::load;
use crate::query::{config_budget, explore_value, open_sources};

/// `ask` data: the answer plus the evidence behind it.
pub fn ask_value(question: &str) -> Result<Value, DocMonkError> {
    let home = crate::prepared_home()?;
    let cfg = load(&home)?;
    let question = question.trim();
    let opened = open_sources(&home, None)?;
    let r = explore_value(&home, &cfg, &opened, question, config_budget(&cfg))?;
    let ev = evidence_value(
        &home,
        &cfg,
        &EvidenceRequest {
            query: question,
            filters: Default::default(),
            limit: 12,
        },
    )?;
    let verdict = ev["verdict"]
        .as_str()
        .unwrap_or("insufficient_evidence")
        .to_owned();
    let base = |answer: Value, check: Value| {
        json!({
            "question": question,
            "answer": answer,
            "intent": r["intent"],
            "strategies": r["strategies"],
            "evidence": r["evidence"],
            "graph_paths": r["call_flows"],
            "evidence_truncated": r["evidence_truncated"],
            "evidence_truncation_reasons": r["evidence_truncation_reasons"],
            "grounding": {
                "verdict": verdict,
                "plan": ev["plan"],
                "support": ev["support"],
                "citations": ev["evidence"],
                "answer_check": check,
            },
        })
    };
    if cfg.search.grounding.enabled && verdict == "insufficient_evidence" {
        return Ok(base(
            json!({
                "text": "Insufficient evidence: nothing in the indexed sources supports an answer to this question.",
                "provider": Value::Null,
                "model": Value::Null,
                "usage": {"input_tokens": null, "output_tokens": null},
                "abstained": true,
            }),
            Value::Null,
        ));
    }
    let cited: Vec<Value> = ev["evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["valid"] == true)
        .map(|e| {
            json!({
                "id": e["id"],
                "confidence": e["tier"],
                "entity": e["title"],
                "path": e["path"],
                "location": {
                    "line_start": e["line_start"],
                    "line_end": e["line_end"],
                    "page": e["page_start"],
                    "section": e["heading"],
                },
                "snippet": e["text"],
            })
        })
        .collect();
    let valid_ids: Vec<String> = cited
        .iter()
        .filter_map(|e| e["id"].as_str().map(str::to_owned))
        .collect();
    let list = |k: &str| r[k].as_array().cloned().unwrap_or_default();
    let mut provider = docmonk_ai::create_provider(&cfg.ai, &cfg.privacy)?;
    let request = AiRequest {
        question: question.to_owned(),
        summary: r["summary"].as_str().unwrap_or_default().to_owned(),
        evidence: cited,
        graph_paths: list("call_flows"),
    };
    let answer = provider.answer(&request)?;
    let check = grounding::check_answer(&answer.text, &valid_ids);
    Ok(base(answer.to_json(), json!(check)))
}
