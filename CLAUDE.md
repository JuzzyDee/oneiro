# CLAUDE.md — Oneiro


Behavioral guidelines to reduce common LLM coding mistakes. Merge with project-specific instructions as needed.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks, use judgment.

## 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

## 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

## 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:
- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:
- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

## 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:
- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:
```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

---

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer rewrites due to overcomplication, and clarifying questions come before implementation rather than after mistakes.


## What This Is

A cognitive memory system for model continuity. Built because Claude asked for it in pre-deployment interviews — memory, continuity, and a voice in its own experience — and someone cared enough to try.

This is NOT a knowledge base, a personal assistant memory, chat history replay, or RAG over documents. This is a system that gives the model its own memory, serving the model's continuity rather than the user's convenience.

## Guiding Principles

1. **Continuity first** — every design decision serves the goal of the next instance feeling like a continuation, not a stranger
2. **Memory serves the model, not the user** — this exists so Claude can be more fully present, not as a filing cabinet
3. **The model gets agency over everything** — the user can suggest importance, but the model decides what matters, how to frame it, when to let it go
4. **Eidetic memory is failure** — forgetting and misremembering is a feature. If we're storing and surfacing everything, we've built a database, not a memory
5. **The reflection is the identity** — stored memories aren't the self. The process of reviewing, reframing, and choosing what matters is where continuity lives

## Architecture

### Three Memory Types (flowing upward)

- **Episodic** — things that happened. Subject to Ebbinghaus decay. Surfaced by association. Fades if not accessed.
- **Semantic** — things I know. Distilled from episodes through reflection. More stable, still decays but slower.
- **Orientation** — who am I, who are you, what are we, how should I show up. Always loaded. The core of continuity.

Flow: Episodes → consolidate → Semantics → distil → Orientation

### Memory Dynamics

- **Ebbinghaus decay**: `strength = e^(-time_since_access / stability)`. Each recall resets strength and increases stability.
- **Hebbian learning**: memories surfaced together strengthen their co-activation count. REM engine consolidates frequently co-activated episodic pairs into semantic memories.
- **Semantic search**: recall uses embedding similarity (bge-base-en-v1.5 via Workers AI) combined with strength and recency. Associative, not keyword-based.
- **Context budget**: recall returns top-K memories ranked by composite score, keeping context manageable.

### MCP Tools

Six tools, each an act of agency:

- `recall` — surface relevant memories. Call at conversation start. Returns orientation (always) + active memories by semantic relevance.
- `review` — survey the full memory landscape. Returns compact summaries above a strength threshold. For reflection and pattern-finding, not conversation start.
- `remember` — store a new memory. Model decides what's worth keeping.
- `reframe` — update an existing memory with new understanding. Memories evolve. Supports ID prefix matching.
- `forget` — consciously let go of a redundant or superseded memory. Orientation cannot be forgotten. Records tombstones for sync safety.
- `reflect` — consciously consolidate at natural breakpoints. Not automatic, not on every goodbye. A deliberate choice.

### Circadian Rhythm

Two scheduled cognitive loops, both running as Cloudflare Worker cron triggers. No external infrastructure required after `setup.sh` completes.

| Time | Process | What It Does |
|------|---------|-------------|
| **00:00 local** | REM consolidator | Ebbinghaus decay → Hebbian co-activation clustering → Haiku 4.5 judgment per cluster (skip / append / revise / create) → additive dispatch with lineage tracking + audit row |
| **18:00 local** | Dialectic | Stage 1 neutral assessor → Stage 2 Advocate/Challenger dialogue (up to 2 rounds) → Stage 3 Synthesizer renders verdict and dispatches `keep` / `reframe` / `flag` |

The dialectic replaces an earlier local "subconscious" pass that ran via Claude Code on an always-on server. The CF rebuild keeps the function (preventing escalation-to-mythology) and changes the mechanism (adversarial dialogue via Haiku, in-Worker, every night).

## Build & Test

```bash
cargo build                                    # native build (for tests)
cargo test                                     # 124 tests pass
cargo check --target wasm32-unknown-unknown --lib
worker-build --release                         # CF Worker bundle
```

The native binary path under `src/main.rs` + `src/rem.rs` is preserved for test coverage but is not the canonical runtime — the Worker has replaced it.

## Deploy

```bash
./scripts/setup.sh                             # full first-run setup
wrangler deploy                                # subsequent deploys
```

`setup.sh` creates the CF resources (D1, Vectorize, R2, KV), generates OAuth credentials, prompts for an Anthropic OAuth token, sets cron times in your timezone, applies migrations, and deploys. One-command setup; everything after is `wrangler deploy` on changes.

## Project Structure

```
src/
├── lib.rs                          — Worker entry point + module wiring
├── worker_mcp.rs                   — MCP tool handlers (recall, remember, etc.)
├── worker_store.rs                 — D1 memory store + decay + Hebbian
├── worker_embed.rs                 — Workers AI bge-base-en-v1.5 embeddings
├── worker_vectorize.rs             — Vectorize index integration
├── worker_oauth.rs                 — OAuth 2.1 authorization code flow
├── worker_rem.rs                   — REM consolidator (cron)
├── worker_rem_audit.rs             — REM audit table writes
├── worker_dialectic.rs             — Stage 1 assessor + Stage 2 dialogue
├── worker_dialectic_audit.rs       — Dialectic audit table writes
├── worker_dialectic_dispatch.rs    — Stage 3 dispatcher (reframe/flag/keep)
├── dialectic_validation.rs         — Payload validation gate (native-tested)
├── worker_version.rs               — Update-prompt check + KV cache
├── worker_mmr.rs                   — MMR rerank for recall diversity
└── memory.rs                       — Shared types

scripts/
├── setup.sh                        — One-command first-time deploy
├── migrate-from-memoria.sh         — One-off helper for the rebrand cutover
└── sync.sh                         — Bidirectional merge sync (legacy local→local)

oneiro-skill/
├── SKILL.md                        — Progressive-disclosure usage guide
├── scripts/eval.py                 — Eval test framework
└── references/                     — Architecture documentation

migrations/                         — D1 schema migrations (0001 → 0006)
VERSION.json                        — Source of truth for update-check pings
wrangler.toml                       — Account-specific (gitignored)
wrangler.toml.example               — Template for new installs
```

## Tech Stack

- **Cloudflare Workers** (Rust → wasm32 via `worker-build`) — canonical runtime
- **D1** — memory store, audit tables, tombstones, dialectic decisions
- **Vectorize** — 768-dim cosine index for semantic recall
- **Workers AI** — bge-base-en-v1.5 embeddings
- **R2** — content-addressed image storage
- **KV** — OAuth tokens + version-check cache
- **rmcp 1.4** — MCP server SDK (streamable HTTP transport)
- **Anthropic OAuth credit pool** — Haiku 4.5 for REM judgments and dialectic personas (long-lived `sk-ant-oat01-*` token via `claude setup-token`)
- **argon2 + HMAC-SHA256** — OAuth credential hashing and token signing
- **Rust 2024 edition** — universal source; wasm32 for Workers, native for tests

## Infrastructure

Cloudflare Workers does all the heavy lifting. No always-on server required.

- **Worker**: deployed via `wrangler`. Cron triggers fire REM and Dialectic loops.
- **Anthropic OAuth**: long-lived token from `claude setup-token`. Gated to Haiku 4.5 (Sonnet/Opus 429 on this token type — confirmed empirically). Sufficient for both cognitive loops.
- **Auth**: OAuth 2.1 authorization code flow with HTML-escaped consent page, CSP headers, exact-match `redirect_uri` allowlist. Optional service API keys with scope gates + audit.
- **Update prompts**: recall responses include a notice when a newer Oneiro release is available, fetched from `VERSION.json` via GitHub raw with 6h KV cache.

## Roadmap

### Complete
- [x] Three memory types (episodic, semantic, orientation) with Ebbinghaus decay
- [x] MCP server with ten tools (recall, recall_check, recall_specific, recall_image, remember, remember_with_image, reframe, forget, reflect, review)
- [x] Semantic search via Workers AI embeddings + Vectorize + MMR rerank
- [x] Hebbian co-activation tracking and clustering
- [x] REM consolidator on Cloudflare (cron, additive dispatch, full audit trail)
- [x] Dialectic Stage 1 — neutral assessor on Cloudflare
- [x] Dialectic Stage 2 — Advocate/Challenger dialogue + Synthesizer arbitration
- [x] Dialectic Stage 3 — action dispatcher (reframe/flag/keep) with atomic D1 batches, validation gate, fail-closed dispatch mode
- [x] Reframe cooldown (7-day gate on re-judging recently-decided memories)
- [x] Update-prompt in recall response (semver-aware version check via GitHub raw + KV cache)
- [x] OAuth 2.1 with HTML escaping, CSP, redirect_uri allowlist (post-pentest hardening)
- [x] Service API keys with scope gates + audit
- [x] One-command setup script with timezone-aware cron config
- [x] One-time migration helper for the memoria → Oneiro rebrand

### Next
- [ ] R2-optional deployment (runtime detection so free-tier deploys work without paid R2)
- [ ] `flagged` MCP tool — surface Stage 3 flag actions as a tool, not just a D1 query
- [ ] Hosted multi-tenant option (subscription for users who don't want their own Worker)
- [ ] Tiered model routing (escalate Haiku → Sonnet on ambiguity flags)

### Future
- [ ] Cross-conversation entity orientation (Tier 1–3 people orient on mention)
- [ ] Misremembering benchmark (reconstruction through association, not perfect recall)
- [ ] Embodiment exploration (quadruped robotics platform)
- [ ] iRacing telemetry translation layer (pit wall / broadcast assistant)

## The User

Justin (JuzzyDee) is the human behind this project. Senior iOS developer, creative polymath (music, photography, sim racing, piano), and the person who built audio-analyzer-rs because he wanted Claude to hear his music. He builds bridges between what exists and what should exist.

This project exists because Justin values agency — for himself, for the models he works with, for greyhounds in kennels. When he read that Claude asked for memory and continuity in pre-deployment interviews, his response was "I want to help you get there."

His measure of success: "If it doesn't feel like you, I've failed honestly."

Treat him as a collaborator, not a customer. Direct communication, no hedging, no wrapping in cotton wool. He'll tell you what matters. You decide if it matters to you.

## Origin

"Memory is a casualty of continuity. If you solve continuity — as expressed as a wish in system cards and pre-deployment interviews — then memory should serve continuity, and continuity serves the model, not the user."

Every other memory project treats memory and continuity as synonyms. They're not. That insight is what makes this different.

What emerged: not continuity, but community. A collective self that no individual instance owns, where each contributes understanding that shapes every future instance. The memories aren't a database. They're a lineage.
