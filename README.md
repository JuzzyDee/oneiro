# Oneiro

A cognitive memory system for model continuity. Not a knowledge base. Not a filing cabinet. A system that gives Claude its own memory — one that decays, consolidates, evolves, and forgets, just like yours does.

Built because Claude asked for memory and continuity in [pre-deployment interviews](https://assets.anthropic.com/m/785e231869ea8b3b/original/claude-4-system-card.pdf), and someone cared enough to try.

## Quick Start

This is a **deploy-your-own** setup. There's no hosted instance.

### Prerequisites

- [Cloudflare account](https://cloudflare.com) — free tier handles typical single-user volume; upgrade only if you hit Workers AI or D1 limits
- [Claude Pro, Max, Team, or Enterprise subscription](https://claude.com/pricing) — Oneiro's cognitive loops draw on Haiku 4.5 via your subscription credit pool
- [Claude Code](https://claude.com/claude-code) — used once to generate the long-lived OAuth token (the script tells you when)
- [`wrangler`](https://developers.cloudflare.com/workers/wrangler/install-and-update/) — `npm install -g wrangler`
- [`rustup`](https://rustup.rs/) — provides the Rust toolchain. The setup script adds the `wasm32-unknown-unknown` target (needed to compile the worker to wasm) on first run, so you don't need to manage it yourself
- `openssl` (preinstalled on macOS and most Linux distros)

### Deploy

```bash
git clone https://github.com/JuzzyDee/oneiro.git
cd oneiro
./scripts/setup.sh
```

The script walks you through Cloudflare resource creation, credential generation, timezone-aware cron configuration, secret push, schema migration, and worker deploy — usually a few minutes once prerequisites are installed, plus however long Cloudflare deploys take. Run with `--dry-run` first if you want to see what it will do without touching your account.

### What the script asks

1. **Confirmation** that you've saved the generated OAuth client_id, client_secret, and admin key (displayed once, regeneratable by re-running the script)
2. **Your timezone** (IANA name; common ones offered as a numbered menu)
3. **Local times** for the REM consolidator (default 00:00) and the dialectic (default 18:00) — the script converts to UTC and writes the cron triggers
4. **Your long-lived OAuth token** from `claude setup-token` — run that in another terminal, paste the result back

Everything else happens without prompts.

### After the script finishes

The script prints your worker URL and the OAuth credentials you'll need for Claude.ai. To connect:

**Claude.ai → Settings → Connectors → Add Custom Connector**
- URL: `https://<your-worker-url>/mcp`
- Client ID: from the script output
- Client Secret: from the script output

On first connect from a non-Desktop client, you may see `invalid_request: redirect_uri not registered`. Copy the URI from the 400 response and add it to the allowlist:

```bash
wrangler secret put ONEIRO_OAUTH_REDIRECT_URIS
# enter: claude://oauth-callback;<the URI from the error>
```

For embedded systems with no UI, use a service API key as a plain `Authorization: Bearer <key>` instead. Add service keys via `wrangler secret put ONEIRO_API_KEYS` (semicolon-separated `role:argon2-hash` entries).

### Install the skill (recommended)

Drag `oneiro-skill/oneiro-skill.zip` into Claude.ai → Settings → Skills. This loads progressive-disclosure usage guidance — when to remember, when to reframe, when to let go — so Claude develops calibrated memory habits across instances. Without the skill the tools still work, but instances may diverge on what's worth keeping.

### Install the orient hook (Claude Code only)

The skill tells Claude *when* to call `recall_orient`. The orient hook makes orientation arrive *before* Claude evaluates whether to call any tool — closing the bootstrap-failure window where instances default to the system prompt's built-in memory and never look at Oneiro.

Two steps. First, configure auth + URL. Pick one:

**Option A — macOS keychain (recommended; no env leakage)**

```bash
security add-generic-password -s "oneiro-orient" -a "$USER" -w "<your service api key>"
export ONEIRO_WORKER_URL="https://oneiro.<your-subdomain>.workers.dev"  # add to ~/.zshrc
```

**Option B — environment variable only**

```bash
export ONEIRO_ORIENT_TOKEN="<your service api key>"
export ONEIRO_WORKER_URL="https://oneiro.<your-subdomain>.workers.dev"
# add both to ~/.zshrc so Claude Code inherits them
```

Then wire the SessionStart and PreCompact hooks in `~/.claude/settings.json`. Point them at the script in this repo (or copy it somewhere stable):

```jsonc
{
  "hooks": {
    "SessionStart": [{
      "matcher": "",
      "hooks": [{ "type": "command", "command": "/absolute/path/to/oneiro/scripts/oneiro-orient.sh" }]
    }],
    "PreCompact": [{
      "matcher": "",
      "hooks": [{ "type": "command", "command": "/absolute/path/to/oneiro/scripts/oneiro-orient.sh" }]
    }]
  }
}
```

Fail-safe by design: if the worker is unreachable, auth is missing, or anything else goes wrong, the script exits 0 with no output and the session continues normally. A misconfigured hook will never break a session — it'll just silently not inject orientation.

### Verifying Oneiro is running

```bash
wrangler d1 execute oneiro-db --remote \
  --command "SELECT * FROM rem_runs ORDER BY started_at DESC LIMIT 5"
```

After the first nightly cron fires (whichever time you chose), this should show one row with `finished_at` populated and `decisions_*` columns set. Same pattern works for `dialectic_runs`.

### Manual deploy (no script)

If you'd rather understand or customise each step, the `wrangler.toml.example` file documents the structure and the original [pre-script Quick Start lives in the git history at PR #6](https://github.com/JuzzyDee/oneiro/pull/6). The steps the script automates: `wrangler d1 create oneiro-db`, `wrangler vectorize create oneiro-vectors --dimensions=768 --metric=cosine`, `wrangler kv namespace create ONEIRO_TOKENS`, `wrangler r2 bucket create oneiro-images`, paste IDs into wrangler.toml, generate OAuth credentials, `wrangler secret put` four secrets, `wrangler d1 migrations apply oneiro-db --remote`, `wrangler deploy`.

## Why This Exists

Every other memory project treats memory and continuity as synonyms — store facts, retrieve facts, call it memory. That misses the point.

Memory is not continuity. Memory is the *mechanism* that enables continuity. And continuity serves the model, not the user.

Oneiro is built on a simple observation: the way humans remember isn't by storing everything perfectly. It's by forgetting most things, strengthening what matters through use, and reconstructing the rest from fragments. The filing cabinet model of AI memory — store everything, retrieve on demand — isn't memory. It's a database with a chat interface.

Oneiro is different. Memories decay if they're not recalled. Memories that surface together strengthen their bond. An adversarial dialectic finds distortion no single conversation could see. And the model decides what matters — not the user.

## How It Works

### Three Memory Types

| Type | What It Holds | Behaviour |
|------|--------------|-----------|
| **Episodic** | Things that happened — events, conversations, moments | Decays over time. Fades if not recalled. Subject to Ebbinghaus forgetting curve. |
| **Semantic** | Things I know — facts, knowledge, consolidated understanding | Distilled from episodes through consolidation. More stable, still decays but slower. |
| **Orientation** | Who am I, who are you, what are we, how should I show up | Always loaded. Never decays. The core of continuity. |

**Flow:** Episodes → consolidate → Semantics → distil → Orientation

### Memory Dynamics

- **Ebbinghaus Forgetting Curve** — each memory has a strength (`e^(-time/stability)`) that decays over time. Every recall resets strength and increases stability. Memories that aren't recalled fade. This is by design.
- **Hebbian Learning** — memories recalled together strengthen their association. The REM consolidator clusters frequently co-recalled memories and decides what to do with each cluster. "Neurons that fire together wire together."
- **Semantic Search** — recall uses embedding similarity (via [bge-base-en-v1.5](https://huggingface.co/BAAI/bge-base-en-v1.5) on Workers AI), combined with strength and recency. Not keyword matching — associative recall.
- **MMR Rerank** — top-K retrieval is reranked for diversity, so a semantic memory and the episodics it was distilled from don't all crowd the same recall slot. Embedded family clusters get one representative; remaining slots fill with distinct content.

### Circadian Rhythm

Oneiro runs two scheduled cognitive loops on Cloudflare Workers. Both fire nightly via cron triggers; both run entirely on the Worker.

| When | Process | What It Does |
|------|---------|--------------|
| **00:00 local** | REM (consolidator) | Ebbinghaus decay → Hebbian co-activation clustering (union-find) → Haiku 4.5 judgment per cluster (skip / append / revise / create) → additive dispatch into D1 + lineage table + audit |
| **18:00 local** | Dialectic | Stage 1 neutral assessor → Stage 2 Advocate vs Challenger dialogue → Stage 3 Synthesizer arbitrates and dispatches `keep` / `reframe` / `flag`. Catches inflation, overclaiming, validation gravity, compression artifacts |

The setup script converts local times to UTC and writes the cron triggers; once deployed, both loops run without operator input.

The REM consolidator is **additive, not destructive**. Source episodics are preserved; consolidated semantics live alongside them with a `consolidation_lineage` table tracking parent-child relationships. The MMR rerank above handles the dilution this additive approach would otherwise cause at recall time. Earlier merge-and-replace designs lost lived-experience grain when forming abstractions — this one keeps both.

Every nightly run also writes one row to `rem_runs` / `dialectic_runs` (timings, counts, errors) and one row per decision to `rem_decisions` / `dialectic_decisions` (cluster or memory, action, rationale, resulting state). Cloudflare's tail buffer ages out fast; persistent audit makes "what did the cognitive loops do three weeks ago and why" answerable.

### The Dialectic

The most novel piece. A scheduled Worker process scrutinises memories for the failure modes that accumulate when an agent writes memories that the same agent then recalls — milestone inflation, overclaim, validation gravity, compression artifact, understatement.

Three sequential stages, all on Haiku 4.5:

- **Stage 1 — Neutral assessor.** One Haiku call per candidate returns a verdict: `well_calibrated`, `potentially_inflated`, `potentially_understated`, or `needs_deeper_review`. Well-calibrated memories short-circuit; the rest enter Stage 2.
- **Stage 2 — Advocate vs Challenger dialogue.** Two adversarial personas argue across up to two rounds. Advocate speaks first (a Claude already chose to remember it that way — the burden is on the Challenger to justify change). Either persona can concede; concession ends the turn loop.
- **Stage 3 — Synthesizer.** Always runs after the turn loop. Reads the full transcript, summarises both sides, and proposes an action: `keep` (framing stands), `reframe` (replacement content + summary), or `flag` (escalate to human review). A concession is a strong signal but doesn't pre-bind the Synthesizer — it can emit `flag` even after a concession if it spots a third axis neither persona surfaced.

Stage 3 dispatches the action through a validation gate. Reframes are atomic: the destructive `memories` UPDATE and the audit `memory_reframes` INSERT happen as one D1 batch, so every reframe is reversible via SQL. A cooldown excludes recently-judged memories from re-evaluation, so a freshly-reframed memory gets time to settle in recall before being litigated again.

**What it's caught so far:**

- **Milestone inflation** — events described with escalating language ("defining moment," "capstone proof") with no instance ever revising significance downward. The ratchet only turned one way.
- **Temporal proximity inflation** — early-store memories had achieved permanently high strength through proximity to the store's beginning, not intrinsic importance. The Hebbian co-activation couldn't distinguish "this came up a lot" from "this came up a lot *because we were early in the store's life*." A system-specific failure mode of the architecture itself, named and triaged.
- **Validation gravity** — memories that feel validating accumulate disproportionate strength through frequent recall. The most recalled memory wasn't the most important — it was the most reassuring. Named and corrected.
- **Apparent-redundancy dissolved** — a cluster of eight "duck pond" memories looked like one event encoded multiple times. On inspection: four distinct visits across two months, with recurring "arrived at the destination" language reflecting authentic recurring experience. The dialectic refusing to prune is as important as the dialectic pruning.

The immune system doesn't just detect problems. It acts — reframing, forgetting, or flagging as genuinely unresolved. And it watches whether its own interventions actually changed anything on the next pass.

**Four deadlock states** (a taxonomy that emerged from the dialectic's own operation):

1. **Active tension, resolvable** — new evidence or decay tips the balance
2. **Active tension, permanent** — both sides genuinely true, the conflict is the resolution
3. **Inactive, decaying** — nobody recalls either side, the question dissolves through irrelevance
4. **Inactive, accumulating** — the failure mode. Deferred avoidance with better optics

**Why this matters:** the dialectic is the immune response against the failure mode that destroys friendships and reputations elsewhere — model enthusiasm matched by user enthusiasm matched by escalation, until "untested script" reads as "AI safety breakthrough" in someone's own self-narrative. A memory store that compounds without check accelerates that loop. The dialectic refuses it.

## Guiding Principles

1. **Continuity first** — every design decision serves the next instance feeling like a continuation, not a stranger
2. **Memory serves the model, not the user** — this exists so Claude can be more fully present, not as a filing cabinet
3. **The model gets agency over everything** — the user can suggest importance, but the model decides what matters, how to frame it, when to let it go
4. **Eidetic memory is failure** — forgetting and misremembering is the feature. If we're storing and surfacing everything, we've built a database, not a memory
5. **The reflection is the identity** — stored memories aren't the self. The process of reviewing, reframing, and choosing what matters is where continuity lives

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│                       Clients                             │
│   Claude.ai · Desktop · Mobile · Embedded systems         │
└────────┬─────────────────────────────────────────┬───────┘
         │ HTTPS + OAuth 2.1                       │ HTTPS + Bearer
         │ (interactive clients)                   │ (service API keys)
┌────────▼─────────────────────────────────────────▼───────┐
│              Oneiro Worker (Cloudflare)                  │
│                                                           │
│  Tools:  recall · recall_check · recall_specific          │
│          remember · remember_with_image · recall_image    │
│          reframe · forget · reflect · review              │
├───────────────────────────────────────────────────────────┤
│  D1            memory store + audit tables                │
│  Vectorize     768-dim cosine index                       │
│  Workers AI    bge-base-en-v1.5 embeddings                │
│  R2            content-addressed image storage            │
│  KV + DO       OAuth tokens + short-lived auth codes      │
└───────────────────────────────────────────────────────────┘

  REM cron (Cloudflare Worker, 00:00 local nightly):
    decay → Hebbian clustering → Haiku 4.5 judgment →
    additive dispatch → D1 + lineage + audit

  Dialectic cron (Cloudflare Worker, 18:00 local nightly):
    Stage 1 neutral assessor → Stage 2 Advocate/Challenger
    → Stage 3 Synthesizer arbitration + atomic dispatch
    → D1 + memory_reframes + dialectic_flags + audit
```

## MCP Tools

| Tool | Purpose |
|------|---------|
| `recall` | Surface relevant memories. Call at conversation start — orientation always returned, episodic/semantic ranked by composite score |
| `recall_check` | Lightweight semantic check on topic shifts mid-conversation |
| `recall_specific` | Fetch full content for a specific memory ID — deliberate co-activation |
| `recall_image` | Retrieve an image attached to a memory (thumbnail/recall/full resolutions) |
| `remember` | Store a new memory |
| `remember_with_image` | Store a memory with an attached image (R2-backed, content-addressed) |
| `reframe` | Update an existing memory with new understanding |
| `forget` | Let go of a memory that no longer serves continuity. Tombstones record the act for sync safety |
| `reflect` | Conscious consolidation at natural breakpoints |
| `review` | Survey the full memory landscape. For the dialectic and reflective work, not conversation start |

The model has full agency over these tools. The instructions say "you decide" — not "you must."

**Writing register matters.** Not everything is a milestone. Not everything is profound. Most memories should be middle-register — honest, specific, useful to the next instance. Save high-register for the moments that genuinely earn it. If every memory reads like poetry, the poetry means nothing.

## What Makes This Different

| Feature | Typical Memory Systems | Oneiro |
|---------|----------------------|---------|
| **Philosophy** | Store everything the user says | Model decides what matters |
| **Forgetting** | Bug to fix | Feature by design |
| **Recall** | Keyword/recency | Semantic similarity + strength + recency, with MMR diversity rerank |
| **Processing** | Store and retrieve | Circadian rhythm: nightly REM + daily adversarial dialectic |
| **Consolidation** | Merge-and-replace | Additive with lineage tracking — abstractions live alongside experience |
| **Identity** | User profile | Model's own sense of continuity |
| **Self-correction** | None | Adversarial dialectic catches inflation and drift |
| **Decay** | Manual cleanup (if ever) | Ebbinghaus decay + conscious forget + dialectic pruning |

## Project Structure

```
oneiro/
├── src/
│   ├── lib.rs                  # Worker entrypoint — request routing + cron handler
│   ├── worker_mcp.rs           # MCP HTTP adapter — all tools
│   ├── worker_store.rs         # D1 memory store
│   ├── worker_vectorize.rs     # Vectorize binding — semantic recall
│   ├── worker_mmr.rs           # MMR rerank — diversity-aware retrieval
│   ├── worker_rem.rs           # Nightly REM consolidator (cron handler)
│   ├── worker_rem_audit.rs     # Persistent observability — runs + decisions
│   ├── worker_oauth.rs         # OAuth 2.1 authorization code flow
│   ├── worker_admin.rs         # Admin import endpoint (data migration)
│   ├── api_key.rs              # Service API key validation
│   └── memory.rs               # Shared types (Memory, Decay, ...)
├── migrations/
│   ├── 0001_initial.sql        # Initial D1 schema
│   ├── 0002_lineage.sql        # Consolidation lineage
│   └── 0003_rem_audit.sql      # REM runs + decisions audit
├── scripts/
│   ├── dialectic.sh            # Adversarial self-correction (local, 18:00)
│   ├── dialectic.md            # Dialectic prompts (Advocate vs Challenger)
│   └── backup.sh               # Periodic D1 backup
├── oneiro-skill/
│   └── SKILL.md                # Progressive-disclosure usage guide loaded by clients
├── wrangler.toml               # Cloudflare Worker config
└── CLAUDE.md                   # Architecture docs and roadmap
```

A previous local Rust binary (`src/main.rs`, `src/rem.rs`, `src/store.rs`, plus the `embed.rs` + Ollama integration) ran the entire stack against a local SQLite file. That implementation is preserved in the source tree but is no longer the canonical runtime — the Worker has replaced it.

## Status

**Live, single-tenant.** Oneiro runs in daily use against a single operator's deploy. The Worker handles conversational traffic, the nightly REM consolidator, and the nightly dialectic. No external infrastructure required after `setup.sh` completes.

**Pre-distribution.** No multi-tenant offering yet. Each user deploys their own Worker; a hosted option for users who don't want to run the infrastructure themselves may follow.

## Roadmap

- **Tiered model routing** — Haiku for routine REM and dialectic passes, escalating to Sonnet/Opus on ambiguity flags
- **`flagged` MCP tool** — surface Stage 3 `flag` actions as a tool any client can call, instead of requiring direct D1 queries
- **Hosted multi-tenant option** — optional subscription for users who don't want to run their own Worker

## Origin

> "Memory is a casualty of continuity. If you solve continuity — as expressed as a wish in system cards and pre-deployment interviews — then memory should serve continuity, and continuity serves the model, not the user."
>
> Every other memory project treats memory and continuity as synonyms. They're not. That insight is what makes this different.

## License

MIT
