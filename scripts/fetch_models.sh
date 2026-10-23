#!/bin/sh
# Fetches the pinned model assets a release bundles, verified
# against the same SHA-256 digests as docmonk-ml's and docmonk-convert's
# manifests, into the layout `<home>/models` uses:
#
#   <dir>/all-minilm-l6-v2/      embedding model
#   <dir>/ms-marco-minilm-l6-v2/ cross-encoder reranker
#   <dir>/ocrs/                  OCR detection + recognition models
#
# With --multilingual, also the optional multilingual embedding model
# (`search.embedding_model: multilingual-e5-small`, ADR 0040):
#
#   <dir>/multilingual-e5-small/
#
# Usage: fetch_models.sh <dir> [--multilingual]. Files already present
# are re-verified, not re-downloaded.
set -eu
ROOT="${1:?usage: fetch_models.sh <dir> [--multilingual]}"
MULTILINGUAL="${2:-}"

# Hashes stdin: Git Bash's sha256sum prefixes the digest with `\` when the
# file name contains a backslash (Windows temp paths).
sha() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum < "$1" | cut -d' ' -f1
    else
        shasum -a 256 < "$1" | cut -d' ' -f1
    fi
}

fetch() { # <dir> <base-url> <file> <sha256>
    mkdir -p "$1"
    [ -f "$1/$3" ] || curl -sSfL --retry 4 -o "$1/$3" "$2/$3"
    got="$(sha "$1/$3")"
    if [ "$got" != "$4" ]; then
        echo "error: $1/$3: expected sha256 $4, got $got" >&2
        rm -f "$1/$3"
        exit 1
    fi
}

D="$ROOT/all-minilm-l6-v2"
B=https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/1110a243fdf4706b3f48f1d95db1a4f5529b4d41
fetch "$D" "$B" config.json 953f9c0d463486b10a6871cc2fd59f223b2c70184f49815e7efbcab5d8908b41
fetch "$D" "$B" model.safetensors 53aa51172d142c89d9012cce15ae4d6cc0ca6895895114379cacb4fab128d9db
fetch "$D" "$B" tokenizer.json be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037

D="$ROOT/ms-marco-minilm-l6-v2"
B=https://huggingface.co/cross-encoder/ms-marco-MiniLM-L-6-v2/resolve/233902d25c440f23af6f7d6e94d2946bac0bee0a
fetch "$D" "$B" config.json 380e02c93f431831be65d99a4e7e5f67c133985bf2e77d9d4eba46847190bacc
fetch "$D" "$B" model.safetensors 821d1aa69520101d6e0737f78a042ae25b19e5cb9160701909d10434f4aeb0ae
fetch "$D" "$B" tokenizer.json d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66

D="$ROOT/ocrs"
B=https://ocrs-models.s3-accelerate.amazonaws.com
fetch "$D" "$B" text-detection.rten f15cfb56bd02c4bf478a20343986504a1f01e1665c2b3a0ad66340f054b1b5ca
fetch "$D" "$B" text-recognition.rten e484866d4cce403175bd8d00b128feb08ab42e208de30e42cd9889d8f1735a6e
if [ "$MULTILINGUAL" = "--multilingual" ]; then
    D="$ROOT/multilingual-e5-small"
    B=https://huggingface.co/intfloat/multilingual-e5-small/resolve/614241f622f53c4eeff9890bdc4f31cfecc418b3
    fetch "$D" "$B" config.json 69137736cab8b8903a07fe8afaafdda25aac55415a12a55d1bffa9f581abf959
    fetch "$D" "$B" model.safetensors 1a55775f53449dac10a2bcbc312469fac40b96d53198c407081a831f81c98477
    fetch "$D" "$B" tokenizer.json 0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39
fi
echo "models ready in $ROOT"
