## Summary

<!-- What changed and why. Roadmap PRs put the work package id in the title. -->

## Checks run

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace` (with pinned models: `DOCMONK_REQUIRE_MODELS=1`, `DOCMONK_REQUIRE_OCR=1`)
- [ ] `cargo xtask clean-slate-audit && cargo xtask branding-audit`
- [ ] `cargo deny check` (when dependencies changed)

## Quality evidence

- [ ] Retrieval, chunking, embedding or ranking changed: before/after held-out reports attached (`DOCMONK_EVAL_OUT`)
- [ ] Performance claims carry measured numbers and the command that produced them

## Judging files (need human approval)

- [ ] This PR changes held-out labels (`fixtures/eval/`), quality floors (`*gates*.json`), the remediation ledger or audit rules
  - Floors only move up; labels are approved by a person, never by the model that wrote them.

## Rollback

<!-- How to undo this change. -->
