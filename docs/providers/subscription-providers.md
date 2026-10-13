# Subscription AI providers (beta)

DocMonk can answer `docmonk ask` questions using an eligible **existing AI
subscription** instead of an API key, through each provider's own official
runtime. This is a convenience over key management — it is still cloud
inference that consumes your account's allowance, and it is neither offline
nor unlimited. Indexing and retrieval stay fully local; enabling a
subscription provider requires no reindexing.

Two subscription providers ship, both **beta**:

| Provider id | Product | Runtime | Optional install |
|---|---|---|---|
| `codex` | ChatGPT (Codex access) | official Codex runtime (`codex`) | install the Codex runtime and put it on `PATH` |
| `github_copilot` | GitHub Copilot | official `copilot` CLI (`copilot -p`) | install the GitHub Copilot CLI and sign in with it |

## Two ways to use a subscription

### A. From your existing AI client, over MCP (no DocMonk provider)

Register DocMonk as an MCP server in a client you already sign into, then
ask that client to retrieve with `docmonk_explore` and answer from the
returned evidence:

```sh
docmonk serve --mcp   # the command to register as a stdio MCP server
```

DocMonk needs no AI provider configured for this flow. Note the privacy
boundary: DocMonk's retrieval reads locally, but the cloud client sends the
tool results to its own provider — DocMonk's `privacy.external_ai_allowed`
flag does not govern that external client's inference.

### B. From `docmonk ask`, using a subscription provider

```sh
docmonk ai providers                 # list providers and capabilities
docmonk ai login codex               # official sign-in (browser/device)
docmonk ai status codex              # connection state, no secrets
docmonk ai models codex              # models the account offers
docmonk config set privacy.external_ai_allowed true
docmonk config set ai.provider codex
docmonk ask "Explain the settlement flow and cite the source files"
docmonk ai logout codex
```

Model selection: leave `ai.model` empty to let the runtime choose and
report the model; set it (`docmonk config set ai.model <name>`) to pin one
from `docmonk ai models`. The OpenAI API adapter's default model is never
used for a subscription provider.

Switching providers is just `docmonk config set ai.provider <id>`; the API
(`openai`/`anthropic`/`openai_compatible`) and local (`ollama`) providers
remain available and are never silently substituted.

## Configuration

```yaml
ai:
  provider: codex        # or github_copilot, or an existing provider
  model: ""              # empty = let the runtime choose and report it
  timeout_seconds: 120
privacy:
  external_ai_allowed: true
```

No secrets belong in this file. Sign-in is delegated to the provider's
runtime; DocMonk stores no token and reads none from config. The runtime
executable is a fixed, validated name — a project-level `.docmonk.yaml`
cannot point DocMonk at an arbitrary binary to run.

## What DocMonk does and does not do

- **Never** asks for a password, copies browser cookies, or extracts a
  token.
- **Never** silently uses an API key or a billable fallback for a
  subscription request; it fails clearly when sign-in is required.
- **Never** lets `GH_TOKEN`/`GITHUB_TOKEN`/`OPENAI_API_KEY` quietly become
  the effective sign-in; the effective auth mode is verified and an
  unexpected token-based mode is rejected. `docmonk doctor` flags such env
  vars.
- Runs every answer in a **fresh, isolated session** with tools, file
  access, web, plugins, hooks, and inherited MCP servers disabled in the
  runtime configuration — so prompt injection inside retrieved evidence
  cannot make the runtime act, and no earlier question's state leaks in.
- Blocks a cloud provider **before** starting any child process when
  `privacy.external_ai_allowed` is false.
- Reports usage only when the runtime provides it; it never invents a
  dollar cost from token counts.

## Diagnostics

`docmonk doctor` gains an **AI** section: it reports the configured
provider and privacy posture and, for a subscription provider, whether its
runtime/SDK is even detectable and whether any shadowing credential env var
is present — all offline and without printing secrets.

## Limitations and status

- Both adapters are **beta** and gated behind version/isolation checks.
  If answer-only isolation cannot be enforced for your pinned runtime
  version, use the MCP client flow (A) instead.
- Account allowances, model access, and provider/organization policies
  still apply.
- Subscription mode is not an unlimited background service: daemon indexing
  never uses it and never prompts for sign-in.
- Tested runtime/SDK versions are recorded in the integration note as real
  verification is performed; a successful login alone is not a
  compatibility claim.

## Claude and Gemini

For Claude, use the MCP client flow (A) with Claude Code; DocMonk ships no
Claude subscription-token adapter. Gemini is a later feasibility item. See
the integration note for details.
