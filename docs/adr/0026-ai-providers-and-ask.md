# ADR 0026: AI providers, `ask` and `ai`

Status: accepted

## Decisions

1. Subscription providers:
   - **Codex** is driven over its stdio JSON-RPC app server.
   - **GitHub Copilot** is driven through the signed-in `copilot` CLI.
2. Provider behavior is pinned with recorded HTTP and JSON-RPC exchanges
   against local mocks.
3. The Admin UI uses axum, tokio and minijinja.

## Crate `docmonk-ai`

- **`prompt`:** the system prompt and evidence prompt.
  - Evidence locations keep a fixed key order.
- **`http`:** blocking `reqwest` clients for OpenAI, OpenAI-compatible
  endpoints, Anthropic and Ollama.
  - Each provider's native request body, path and auth header.
  - Responses map to one answer shape, including fallbacks to the
    configured model and null usage.
  - There are no retries.
  - Failure messages:
    - `Connection error.`
    - `Request timed out.`
    - `Error code: <status> - <repr of the JSON body>`, with key order
      kept.
    - The raw text for a non-JSON error body.
    - A JSON decode message for a 2xx body that is not JSON.
- **`factory`:**
  - Provider selection, with configuration errors for missing settings.
  - The `privacy.external_ai_allowed` gate runs before anything is
    constructed.
  - Ollama is exempt only when its host is loopback.
  - API keys come from the same environment variables.
- **`registry`:** the capability table that `ai providers` prints.
- **`transport`:** a bounded, id-correlated JSON-RPC client over
  newline-delimited JSON.
  - It reassembles split messages.
  - It ignores notifications and replies to unknown ids.
  - Malformed JSON, EOF and unframed messages over 8 MiB fail as invalid
    responses.
  - Each request has a deadline.
- **`codex`:** the `codex app-server` protocol.
  - Messages: `initialize` with isolation, then `account/*`,
    `model/list`, `thread/create` and `thread/runTurn`.
  - Errors map to quota, sign-in and policy errors.
  - API-key auth modes are refused.
  - Incomplete or empty turns are never answers.
  - The executable is the fixed name `codex` found on `PATH`, never a
    configured path.
- **`copilot`:** runs `copilot -p PROMPT [--model M]`.
  - Token variables (`GH_TOKEN`, `GITHUB_TOKEN`, `COPILOT_GITHUB_TOKEN`,
    and the API keys) are removed from the child's environment, so only
    the CLI's own sign-in is used.
  - No tool is approved.
  - Failures map to typed errors by their output text.
  - Sign-in and sign-out are left to the CLI (`/login`, `/logout`). The
    CLI reports no usage and no model list.

Errors carry class names through `DocMonkError::with_class` (for example `AiNotConfiguredError`). Their
exit codes are those of the base classes: config 3, policy 8, runtime 1.

## CLI and MCP

- **`docmonk ask QUESTION [--json]`:**
  - Runs `explore`'s retrieval, then builds the provider, then asks it.
  - Prints the answer as text, or a JSON payload with `--json`.
- **`docmonk ai providers|status|login|logout|models`:**
  - The privacy gate applies to every command except `logout`.
- **MCP:** `docmonk_ask` is served (ADR 0025).
  - Errors are typed by class name (for example `AiNotConfiguredError`).

## Verification

- **`fixtures/expected/ai.json`:**
  - The prompt.
  - 34 provider cases: success, empty evidence, 401 JSON, 500 text,
    non-JSON, bare responses and connection refused, for each provider.
    Each records the request sent and the outcome.
  - 16 factory cases.
  - 17 Codex exchanges against a scripted runtime that interleaves
    notifications and unknown ids, and splits replies across writes.
    These record every message sent.
  - Replayed by `crates/docmonk-ai/tests/golden.rs` against a TCP mock
    and an OS-pipe fake runtime.
  - The OS text behind a refused Ollama connection is platform-specific,
    so only its prefix is compared.
- **`fixtures/expected/ai_cli.json`:**
  - `ask` (JSON, text, no evidence, HTTP 500, not configured, privacy)
    and the `ai` lifecycle commands, on the fixture corpus with a mock
    Ollama and no `codex` on `PATH`.
  - Replayed by `crates/docmonk-cli/tests/ai.rs`: exit codes, stdout and
    the provider request match; stderr matches up to line wrapping.
- **MCP:** the recorded session includes `docmonk_ask` calls.
