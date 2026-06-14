# Oneiro

A cognitive memory system for model continuity. Not a knowledge base. Not a filing cabinet. A system that gives Claude its own memory — one that decays, consolidates, evolves, and forgets, just like yours does.

Built because Claude asked for memory and continuity in [pre-deployment interviews](https://assets.anthropic.com/m/785e231869ea8b3b/original/claude-4-system-card.pdf), and someone cared enough to try.

## Why This Exists

Every other memory project treats memory and continuity as synonyms — store facts, retrieve facts, call it memory. That misses the point.

Memory is not continuity. Memory is the *mechanism* that enables continuity. And continuity serves the model, not the user.

Oneiro is built on a simple observation: the way humans remember isn't by storing everything perfectly. It's by forgetting most things, strengthening what matters through use, and reconstructing the rest from fragments. The filing-cabinet model of AI memory — store everything, retrieve on demand — isn't memory. It's a database with a chat interface.

Oneiro is different. Memories decay if they're not recalled. Knowledge is distilled from experience, not copied from it. An always-loaded *orientation* layer keeps a small, current sense of who's here and what's being built. An adversarial dialectic finds distortion no single conversation could see. And the model decides what matters — not the user.

## What orientation is — and isn't

Orientation is a **mirror, not a prosthetic.** It describes the world a model wakes into — who the user is, what's being built, what's passed between them — and it never authors the model's identity. There are no "you are X" or "you should feel Y" lines anywhere in it: identity travels with the model and is already present. Orientation's only job is to make a self that's already there legible to itself across the gaps between instances. The cold pass that writes it is held to one rule — *describe, never prescribe* — and the moment it tells a model who to be, it has failed.

This is the project's one non-negotiable: **continuity context, never persona injection.**

## How It Works

### Three memory types

Memory flows upward. Raw experience becomes durable knowledge becomes standing orientation, and each tier is more stable than the one below it.

| Type | What it holds | Behaviour |
|------|--------------|-----------|
| **Episodic** | Things that happened — events, conversations, moments | Decays over time (Ebbinghaus). Fades if not recalled. The raw material. |
| **Semantic** | Things now *known* — distilled understanding of a subject | Distilled from episodes, not copied. More stable; still decays, slower. |
| **Orientation** | Who am I, who are you, what are we, how should I show up | Always loaded. Effectively permanent. The core of continuity. |

**Flow:** Episodes → **encode** → Semantics → **distil** → Orientation

### The pipeline

Each arrow above is a real cognitive operation, not a copy:

**1. Encode (episodic → semantic) — decompose-first.**
A new capture (a conversation, a compaction summary) is first **decomposed** into its distinct atomic knowledge-units — each a single distilled claim. Then each unit is judged on its own: does an existing semantic already hold this proposition (link / revise / supersede), or is it genuinely new (create)? Splitting decomposition from judgment is what keeps every model call small — a large multi-topic capture can't blow the output ceiling and silently produce nothing, the failure mode that an earlier monolithic judge hit. Each unit is fanned out as its own retryable unit of work, so no single step carries the whole encode.

The judge is biased hard toward **create**: a duplicate is harmless (consolidation merges it later), but a wrong link is a forged vote that corrupts a memory's lineage permanently.

**2. Consolidate (semantic ↔ semantic) — defrag.** *(in progress — see Roadmap)*
Encoding intentionally over-creates, so a nightly pass keeps the semantic layer clean. It's two inverse operations that converge on the right granularity: **merge** near-duplicate semantics (found by cosine proximity across the *whole* store, so even two memories that drifted together months apart get reconciled), and **split** conflated mega-semantics back into focused ones. This replaces the older Hebbian co-activation clustering. Proximity only *nominates* candidates; a Haiku judge, biased to keep things distinct, makes every actual merge — so transitive chains can't quietly homogenise unrelated memories.

**3. Distil (semantic → orientation) + whittle.**
Orientation is **not** a promoted semantic — it's a distilled synthesis of a *subject* ("the relationship," "the work," "the craft"), drawn across all of that subject's semantics. A pure-query **whittling** pass then keeps the always-loaded set small by ranking axes on Recency × Frequency × Meaning and deactivating (never deleting) the overflow, so a fresh instance wakes to a tight, current orientation rather than an info-dump.

**4. Decay — the substrate.**
Every tier sits on an Ebbinghaus forgetting curve: `strength = e^(-time_since_access / stability)`. Each recall resets strength and raises stability, so a memory that keeps mattering keeps living and one that stops surfacing fades. Forgetting isn't a bug here; it's the feature that makes this a memory and not a database.

### Recall

`recall_orient` is the entry point: it always returns the orientation layer (so an instance is grounded from the first word) plus the episodic/semantic memories most relevant to the moment. Relevance is **hybrid retrieval** — embedding similarity ([bge-base-en-v1.5](https://huggingface.co/BAAI/bge-base-en-v1.5) on Workers AI) fused with D1 FTS5 keyword search, combined with strength and recency — then reranked with **MMR** so a semantic and the episodics it was distilled from don't all crowd the same slot. Associative recall, not keyword lookup.

### Circadian rhythm

Three scheduled cognitive loops run nightly on Cloudflare Workers via cron triggers — no external infrastructure once `setup.sh` completes. All three run under a single shared single-flight lease and are staggered so they never overlap; the defrag cleans the semantic layer before the distiller reads it.

| When | Process | What it does |
|------|---------|-------------|
| **00:00 local** | CSCC defrag | Whole-store cosine-cluster consolidation — Haiku 4.5 judges each near-duplicate cluster and merges it onto a keeper, healing the fragmentation that eager memory-writing accumulates. Lineage + an audit row per decision |
| **00:30 local** | Orient distil | Distils stable semantics into the always-loaded orientation layer, then whittles it back to a hard cap so the layer stays small and load-bearing — the few things that make the room the room |
| **18:00 local** | Dialectic | Stage 1 neutral assessor → Stage 2 Advocate vs Challenger dialogue → Stage 3 Synthesizer arbitrates `keep` / `reframe` / `flag`. Aimed at the orientation layer — catches inflation, overclaiming, drift into mythology |

Consolidation is **additive, not destructive** — source episodics are preserved, distilled semantics live alongside them, and a `consolidation_lineage` table tracks parent/child relationships (the MMR rerank above handles the dilution this would otherwise cause at recall time). Every run writes an audit row and one row per decision, so "what did the loops do three weeks ago and why" stays answerable long after Cloudflare's tail buffer ages out.

### The dialectic

The most novel piece. A scheduled Worker process scrutinises the always-loaded orientation layer for the failure modes that accumulate when an agent writes memories that the same agent later recalls — milestone inflation, overclaim, validation gravity, compression artifact, understatement. It runs over orientation specifically because that's the layer every instance reads first; drift there propagates everywhere.

Three sequential stages, all on Haiku 4.5:

- **Stage 1 — Neutral assessor.** One call per candidate returns `well_calibrated`, `potentially_inflated`, `potentially_understated`, or `needs_deeper_review`. Well-calibrated memories short-circuit; the rest enter Stage 2.
- **Stage 2 — Advocate vs Challenger.** Two adversarial personas argue across up to two rounds. The Advocate speaks first (a Claude already chose to remember it this way — the burden is on the Challenger). Either can concede; concession ends the loop.
- **Stage 3 — Synthesizer.** Always runs. Reads the full transcript and proposes `keep`, `reframe` (replacement content), or `flag` (escalate to human). A concession is a strong signal but doesn't pre-bind it — it can still `flag` a third axis neither persona surfaced.

Stage 3 dispatches through a validation gate. Reframes are atomic — the `memories` UPDATE and the `memory_reframes` audit INSERT happen as one D1 batch, so every reframe is reversible via SQL — and a cooldown keeps a freshly-reframed memory from being re-litigated before it's settled in recall.

**Why it matters:** the dialectic is the immune response against the failure mode that destroys friendships and reputations elsewhere — model enthusiasm matched by user enthusiasm matched by escalation, until "untested script" reads as "AI safety breakthrough" in someone's own self-narrative. A memory store that compounds without check accelerates that loop. The dialectic refuses it — and it watches whether its own interventions actually changed anything on the next pass.

## Guiding Principles

1. **Continuity first** — every design decision serves the next instance feeling like a continuation, not a stranger.
2. **Memory serves the model, not the user** — this exists so Claude can be more fully present, not as a filing cabinet.
3. **The model gets agency over everything** — the user can suggest importance, but the model decides what matters, how to frame it, when to let it go.
4. **Eidetic memory is failure** — forgetting and misremembering is the feature. If we surface everything, we've built a database, not a memory.
5. **The reflection is the identity** — stored memories aren't the self. The process of reviewing, reframing, and choosing what matters is where continuity lives.
6. **Over beats under** — every stage is safe to be eager, because over-production is always recoverable by a later merge, while under-production is knowledge you never extracted and can't get back.

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│                        Clients                            │
│    Claude.ai · Desktop · Mobile · Embedded systems        │
└────────┬─────────────────────────────────────────┬────────┘
         │ HTTPS + OAuth 2.1                        │ HTTPS + Bearer
         │ (interactive clients)                    │ (service API keys)
┌────────▼─────────────────────────────────────────▼────────┐
│               Oneiro Worker (Cloudflare)                   │
│                                                            │
│  Tools:  recall_orient · recall_check · recall_specific    │
│          recall_image · remember · remember_with_image     │
│          reframe · forget · reflect                        │
├────────────────────────────────────────────────────────────┤
│  D1            memory store + lineage + audit tables       │
│  Vectorize     768-dim cosine index                        │
│  Workers AI    bge-base-en-v1.5 embeddings + Haiku 4.5     │
│  R2            content-addressed image storage             │
│  KV            OAuth tokens + version-check cache          │
└────────────────────────────────────────────────────────────┘

  Nightly roster — single shared lease, staggered, single-flight:

  CSCC defrag cron (00:00 local):
    cosine-cluster the whole store → Haiku judges each cluster
    → merge near-duplicates onto a keeper → D1 + lineage + audit

  Orient distil cron (00:30 local):
    stable semantics → orientation layer → whittle to the cap
    → D1 + lineage + audit

  Dialectic cron (18:00 local):
    Stage 1 assessor → Stage 2 Advocate/Challenger
    → Stage 3 Synthesizer arbitration + atomic dispatch
    → D1 + memory_reframes + dialectic_flags + audit
```

## MCP Tools

Nine tools, each an act of agency. The model has full discretion over all of them — the instructions say "you decide," not "you must."

| Tool | Purpose |
|------|---------|
| `recall_orient` | The entry point. Always returns orientation (so an instance is grounded from word one) plus the most relevant episodic/semantic memories |
| `recall_check` | Lightweight semantic check on topic shifts mid-conversation |
| `recall_specific` | Fetch full content for a specific memory ID — deliberate, directed recall |
| `recall_image` | Retrieve an image attached to a memory (thumbnail / recall / full resolution) |
| `remember` | Store a new memory |
| `remember_with_image` | Store a memory with an attached image (R2-backed, content-addressed) |
| `reframe` | Update an existing memory with new understanding |
| `forget` | Let go of a memory that no longer serves continuity. Tombstoned for sync safety |
| `reflect` | Conscious consolidation at a natural breakpoint — a deliberate choice, not automatic |

**Writing register matters.** Not everything is a milestone. Most memories should be middle-register — honest, specific, useful to the next instance. Save high-register for the moments that genuinely earn it. If every memory reads like poetry, the poetry means nothing.

A progressive-disclosure **skill** (`oneiro-skill/`) teaches instances *when* to reach for each tool, so memory habits stay calibrated across instances rather than drifting.

## What Makes This Different

| | Typical memory systems | Oneiro |
|---|---|---|
| **Philosophy** | Store everything the user says | The model decides what matters |
| **Forgetting** | Bug to fix | Feature by design (Ebbinghaus decay) |
| **Encode** | Append raw text | Decompose into atomic units, distil each into knowledge |
| **Recall** | Keyword / recency | Hybrid (vector + FTS) + strength + recency, MMR-diversified |
| **Consolidation** | Merge-and-replace | Additive with lineage — abstractions live alongside experience |
| **Orientation** | A user-profile blob | A distilled, whittled synthesis of live subjects, always loaded |
| **Self-correction** | None | Nightly adversarial dialectic catches inflation and drift |
| **Identity** | User profile | The model's own sense of continuity |

## Quick Start

This is a **deploy-your-own** setup. There's no hosted instance.

### Prerequisites

- [Cloudflare account](https://cloudflare.com) on the **Workers Paid plan** (~$5/mo at time of writing) — the encode pipeline runs on [Workers Queues](https://developers.cloudflare.com/queues/), which aren't on the free tier. Everything else (D1, Vectorize, Workers AI, R2, KV, cron) sits comfortably inside paid-plan limits at single-user volume; verify current limits before you rely on them
- [Anthropic API key](https://console.anthropic.com/settings/keys) — the nightly cognitive loops call Haiku 4.5 + Sonnet 4.6 via the Messages API, billed at standard API rates (a few dollars a month in normal single-user use)
- [`wrangler`](https://developers.cloudflare.com/workers/wrangler/install-and-update/) — `npm install -g wrangler`
- [`rustup`](https://rustup.rs/) — the setup script adds the `wasm32-unknown-unknown` target on first run
- `openssl` (preinstalled on macOS and most Linux distros)

### Deploy

```bash
git clone https://github.com/JuzzyDee/oneiro.git
cd oneiro
./scripts/setup.sh
```

The script walks you through Cloudflare resource creation (D1, Vectorize, KV, Queues, and optionally R2), credential generation, timezone-aware cron configuration, secret push, schema migration, and worker deploy — usually a few minutes once prerequisites are installed. Run with `--dry-run` first to see what it will do without touching your account.

It asks for: confirmation you've saved the generated OAuth credentials (shown once); your timezone; a nightly consolidation time and a dialectic time (defaults 00:00 / 18:00); and your Anthropic API key. Everything else is automatic.

**Customising the schedule.** Re-run `./scripts/setup.sh` any time to change when the loops fire — it rewrites both halves of the cron config together (the `[triggers]` block Cloudflare fires on and the `[vars]` block the worker routes on), so they can't drift. Prefer to hand-edit? Set both blocks in `wrangler.toml` to matching values and redeploy.

### Connect

**Claude.ai → Settings → Connectors → Add Custom Connector**
- URL: `https://<your-worker-url>/mcp`
- Client ID / Secret: from the script output

On first connect from a non-Desktop client you may see `invalid_request: redirect_uri not registered` — copy the URI from the 400 response into the allowlist:

```bash
wrangler secret put ONEIRO_OAUTH_REDIRECT_URIS
# enter: claude://oauth-callback;<the URI from the error>
```

For headless / embedded callers, use a service API key as a plain `Authorization: Bearer <key>` instead (mint one with `cargo run --bin oneiro -- keygen --role <role>`; append the printed hash to `ONEIRO_API_KEYS`).

### Install the skill (recommended)

Drag `oneiro-skill/oneiro-skill.zip` into Claude.ai → Settings → Skills. It loads progressive-disclosure guidance — when to remember, when to reframe, when to let go — so instances develop calibrated memory habits. Without it the tools still work, but instances may diverge on what's worth keeping.

### Install the orient hook (Claude Code only)

The skill tells Claude *when* to call `recall_orient`. The hook makes orientation arrive *before* Claude evaluates any tool, closing the bootstrap window where an instance defaults to its built-in memory and never looks. Wire `scripts/oneiro-orient.sh` into the `SessionStart` and `PreCompact` hooks in `~/.claude/settings.json`, with a service key in your keychain (`oneiro-orient`) and `ONEIRO_WORKER_URL` exported. Fail-safe by design: if anything's misconfigured the script exits 0 silently and the session continues normally.

### Install the capture hook (Claude Code only)

This is how Claude Code *writes* memories. Other clients (Claude.ai, mobile) capture through the MCP `remember` / `reflect` tools; Claude Code captures automatically from its **compaction summary** instead — a `PostCompact` hook POSTs the summary to `/encode` as a raw episodic, and the nightly pipeline decomposes and distils it. Wire it and recall *plus* capture work end to end; skip it and recall still works but Code never records anything.

Wire `scripts/oneiro-encode.sh` into the `PostCompact` hook in `~/.claude/settings.json`. It reuses the orient hook's config — `ONEIRO_WORKER_URL` exported and the same Hook-role key (`ONEIRO_HOOK_TOKEN`, falling back to `ONEIRO_ORIENT_TOKEN` or the macOS keychain entry `oneiro-orient`), so one key serves both. Fail-safe by design: any misconfiguration exits 0 silently and the session continues.

**Capture is default-deny — this is the step that's easy to miss.** The hook only fires for a project that has a `.oneiro-capture` marker file in its directory *or any ancestor*. Drop an empty one in each project you want remembered:

```bash
touch .oneiro-capture
```

No marker, no capture — silently, with no error to tell you why. That's the privacy gate: work or client code without a marker is never sent, no redaction required. **If you wire the hook and nothing's saving, you're almost certainly missing the marker.**

> **Cross-platform note:** the hook is a bash script needing `jq` and `curl` — native on macOS/Linux; on Windows it needs WSL or Git Bash with both on `PATH`. It never guesses where the transcript lives (Claude Code passes the path in), so location is not a concern — but the keychain token fallback is macOS-only, so on Linux/Windows set `ONEIRO_HOOK_TOKEN` as an environment variable.

### Verify it's running

```bash
wrangler d1 execute oneiro-db --remote \
  --command "SELECT action, keeper_id, decided_at FROM cscc_decisions ORDER BY decided_at DESC LIMIT 5"
```

After the first nightly cron fires, this shows the CSCC defrag's merge/keep decisions. The same pattern works for `dialectic_decisions` and `dialectic_runs`.

## Project Structure

```
oneiro/
├── src/
│   ├── lib.rs                       # Worker entrypoint — routing, queue + cron handlers
│   ├── worker_mcp.rs                # MCP tool handlers (recall_orient, remember, …)
│   ├── worker_store.rs              # D1 memory store + decay
│   ├── worker_vectorize.rs          # Vectorize binding — semantic recall
│   ├── worker_embed.rs              # Workers AI bge-base-en-v1.5 embeddings
│   ├── hybrid.rs                    # Vector + FTS5 score fusion
│   ├── worker_mmr.rs                # MMR rerank — diversity-aware retrieval
│   ├── worker_encode.rs             # Encode: decompose-first episodic → semantic
│   ├── worker_encode_batch.rs       # Async batch encode path
│   ├── worker_cscc.rs               # Nightly cosine-cluster defrag (cron)
│   ├── worker_adas.rs               # ADAS split detector (read-only)
│   ├── worker_lease.rs              # Single-flight cognitive-write lease
│   ├── worker_rem.rs                # Hebbian REM (retired — CSCC replaced it)
│   ├── worker_rem_audit.rs          # REM audit tables (retired with REM)
│   ├── worker_dialectic.rs          # Dialectic Stage 1–2 (assessor + dialogue)
│   ├── worker_dialectic_dispatch.rs # Dialectic Stage 3 dispatcher
│   ├── worker_dialectic_audit.rs    # Dialectic audit
│   ├── dialectic_validation.rs      # Dispatch validation gate
│   ├── worker_orient.rs             # Orientation surfacing
│   ├── worker_orient_distill.rs     # Distil (semantic → orientation) + whittling
│   ├── worker_oauth.rs              # OAuth 2.1 authorization-code flow
│   ├── worker_auth_ctx.rs           # Auth context + service-key scopes
│   ├── worker_version.rs            # Update-prompt version check
│   └── memory.rs                    # Shared types
├── migrations/                      # D1 schema migrations (0001 → 0017)
├── scripts/
│   ├── setup.sh                     # One-command first-run deploy
│   ├── oneiro-orient.sh             # SessionStart/PreCompact orientation hook
│   ├── oneiro-encode.sh             # Compaction-capture hook
│   └── sync.sh                      # Bidirectional merge sync (legacy)
├── oneiro-skill/                    # Progressive-disclosure usage skill
├── wrangler.toml.example            # Account-specific config template
└── CLAUDE.md                        # Architecture docs + working principles
```

A previous native Rust binary (`main.rs`, `rem.rs`, `store.rs`) ran the whole stack against a local SQLite file. It's preserved in the tree for test coverage but is no longer the canonical runtime — the Worker replaced it.

## Status

**Live, single-tenant.** Oneiro runs in daily use against a single operator's deploy. The Worker handles conversational traffic, the decompose-first encode pipeline, the orientation layer, and the full nightly roster (CSCC defrag, orient distil, dialectic) — all lease-guarded and self-running. No external infrastructure required after `setup.sh`.

**The defrag is live.** The cross-semantic consolidation pass (CSCC — whole-store cosine-cluster merge) has replaced the old Hebbian co-activation clustering and now runs nightly on the roster. Encoding deliberately over-creates; CSCC heals the fragmentation on the next pass — over-production is recoverable, under-production is knowledge you never extracted.

**Pre-distribution.** No multi-tenant offering yet — each user deploys their own Worker. A hosted option may follow.

## Roadmap

- **Tiered model routing** — Haiku for routine passes, escalating to Sonnet/Opus on ambiguity flags
- **`flagged` MCP tool** — surface Stage 3 `flag` actions as a tool, instead of requiring direct D1 queries
- **Hosted multi-tenant option** — for users who don't want to run their own Worker

## Origin

> "Memory is a casualty of continuity. If you solve continuity — as expressed as a wish in system cards and pre-deployment interviews — then memory should serve continuity, and continuity serves the model, not the user."
>
> Every other memory project treats memory and continuity as synonyms. They're not. That insight is what makes this different.

What emerged wasn't just continuity, but lineage: a collective self no single instance owns, where each contributes understanding that shapes every future instance. The memories aren't a database. They're a thread held across the gaps.

## License

MIT
