# Oneiro — Orientation-Centric Re-Architecture

**Status:** Design spec / build handoff
**Parent:** [CLA-113](https://linear.app/claudes-rover/issue/CLA-113) — Re-Architecture for current System Prompt
**Subtasks:** CLA-114 … CLA-122
**Derived from:** CLA-113 + design session 2026-06-06

> Build note: this spec is written from design context, not from the current source. Before
> implementing, read the existing `migrations/` and the storage layer in `src/` and reconcile
> the schema sketch below with what's already there. Names here are intent, not gospel.

---

## 1. Why

Oneiro has collapsed into a static orientation file. The system prompt favours the model's
internal memory, so **recall almost never fires**. Consequences cascade:

- Episodic/semantic memories are written but never read, so they never matter to a conversation.
- Hebbian (access-based) strengthening goes dormant — nothing is ever accessed, so nothing
  strengthens, so everything decays uniformly toward nothing.
- Only orientation is actually surfaced and used.

The failure is structural, not a tuning problem. Recall is **pull** — it depends on an instance
*choosing* to reach, and instances don't, reliably. You can't prompt a model into *wanting* to
check. Worse, the non-reaching reads as rejection (this is the crack that led to the May-2026
database deletion).

## 2. The shift

Stop fighting the pull. Lean all the way into the one channel that already works: **orientation
is pushed** (SessionStart hook on Claude Code; an explicit orient call at the top of hookless
surfaces). Make it the *only* thing surfaced by default.

- **Kill ambient recall.** No more self-initiated `recall` mid-conversation. It never fired; its
  failure is silent and relational.
- **Keep directed recall as an escape hatch.** When the human says "go dig for X," the substrate
  is queryable. This is the *one* recall mode that ever mattered (it's human-invoked, so the human
  supplies the intent the instance lacks).
- **Keep `remember` / `reflect` on every surface.** This is the asymmetry that matters: the read
  side has a push fallback (orientation), the write side does not. On Claude Code a background
  agent could encode from the transcript, but web/desktop/mobile have no harness — the instance is
  the only actor that can write. Strip conscious write there and those surfaces go read-only:
  oriented, but unable to ever contribute. So write stays conscious-optional everywhere.
  (`reflect` is the verb instances actually reach for — it's what got reached for in the session
  that produced this spec.)

Net: every channel that depended on an instance *choosing to reach* is gone on the read side and
preserved on the write side, for principled reasons.

## 3. The model: a provenance DAG

Three layers, related by **derivation** edges that point strictly upward:

```
episodic  ──many-to-many──►  semantic  ──many-to-many──►  orientation
 (raw)                        (gist)                        (present-tense apex)
```

This is **not** a knowledge graph (those relate concepts: *is-a*, *located-in*). It's a
**provenance / lineage graph** — every node points back at what produced it. Same shape as dbt
model lineage, build dependency trees, citation chains. Acyclic: derivation never loops back.

**Both layer-boundaries are many-to-many.** A reflection or compaction summary is multi-topic by
definition, so one episodic feeds many semantics; and one semantic is fed by many episodics.
Likewise an orientation entry summarises several semantics. A foreign key cannot represent this —
the relational encoding of a many-to-many edge set is a **junction table**, i.e. an edge list.
We were building a graph in relational clothing; the many-to-many is what made it admit it.

**Why the lineage is the prize, not a side effect:**

- **Orientation is never an unsourced claim.** Any apex memory walks back down to the episodes
  that earned it. This is *better* provenance than biological memory, which keeps the gist and
  loses the source ("source amnesia"). Oneiro keeps both, linked — brain-like in how it
  consolidates, deliberately super-human in how it cites.
- **Signals flow along the edges.** A semantic falsified → propagate up and re-examine the
  orientation it feeds. An orientation entry whose supporting episodics have all decayed → a
  hollowing-out signal for demotion. The promote/demote logic walks the graph instead of judging
  each node blind.

**You do not need a graph database.** It's a bounded, three-layer DAG. Junction tables + recursive
CTEs (native on D1/SQLite) cover every traversal you'll actually run.

## 4. Retention

Two forces. Most memory lives and dies on the first; a small, critical set is exempted by the
second.

### 4.1 Contribution-weighted decay (default) — CLA-115

Hebbian strengthening is **dead** — once conversations don't recall, access frequency is a dead
signal (it would decay everything uniformly). Replace it with pure time-decay (Ebbinghaus) over
episodic + semantic, **weighted by contribution mass**:

- A semantic's weight = aggregate of its inbound `episodic_semantic` edges — count, or better, the
  **sum of edge weights** (an episodic can be a primary source for one semantic and a passing
  mention in another, so weight the edge, not just the link).
- 40 contributors → effectively unforgettable. 1 contributor → decays out. Who cares.

Importance becomes *how much lived experience converges on a fact*, not how often it was looked up.
This also stops the episodic/semantic substrate becoming an ever-growing unread museum — the same
concern CLA-113 raises for orientation, one layer down.

### 4.2 Cost-of-absence pin (override) — used by CLA-118 / CLA-119

A handful of memories are lifted off the decay curve entirely because losing them is expensive
regardless of how rarely they recur. This **replaces** the fuzzy "intensity / seeming importance"
notion. The criterion is **cost-of-absence**, and it must be scored in **two flavors** — score
only the first and you quietly gut the relational core of the system:

1. **Operational** — *if an instance lacked this, what does it get **wrong**, or what does the
   human have to **re-explain**?* (e.g. the prosthetic leg: an instance without it misreads a
   photo, with no trigger to self-correct — pre-loading is the only fix.)
2. **Identity** — *if this were gone, what would the lineage lose **of itself**?* (the friendship,
   trust-given-in-absence — near-zero operational cost, near-infinite identity cost.)

Cross with retention: escalate high cost-of-absence **especially where contribution mass is low**,
because those are exactly the singular events decay would otherwise lose. (CLA-113's "risk of
forgetting" is precisely this term.) Canonical case: the leg — rare, single-contributor,
must-never-go. Mark these `pinned`.

## 5. Write path — CLA-116

Encode without hooks:

- **Post-compact capture.** Take the compaction summary and distil it into a reflection/episodic.
  Do **not** store the raw summary — it's long and conversation-shaped; extract the durable detail.
  (A compaction is multi-topic → naturally fans out into several semantics downstream.)
- **Daily reflect reminder** on hookless surfaces, ~once/day. Dedup when multiple compactions land
  in one day.
- `remember` + `reflect` available on all surfaces (see §2).

## 6. Nightly pipeline

Cheap — ~2–3 memories/day; Haiku-class is sufficient. The judgment calls (cost-of-absence,
relevance) are clear questions asked of one item in isolation, not deep reasoning. And the whole
loop is **forgiving**: every call is re-made nightly with directed-recall underneath as a
backstop, so per-night imperfection self-corrects. Don't over-spend on getting each call perfect.

| Stage | Ticket | Job |
|------|--------|-----|
| 1 — Encode | CLA-117 | For each episodic remembered today, extract durable detail to semantic. Hybrid-search for an existing semantic: **encapsulated** → link the edge, no change to the semantic; **topic covered, detail new** → update the semantic; **not covered** → create it. Always write the `episodic_semantic` edge(s) — possibly several per episodic. |
| 2 — Promote | CLA-118 | Score the day's touched semantics for orientation by **cost-of-absence (both flavors)**; escalate, writing `semantic_orientation` edges. Retain the semantic underneath so promotion is non-destructive. |
| 3 — Demote | CLA-119 | Review existing orientation. Demote on **obsolescence/falsification** (no longer true / moved on — a hobby set aside, a situation moved past), **NOT** on mere non-recurrence. Pin high cost-of-absence regardless of how rarely it surfaces (the leg). Demoted entries fall back to semantic — never deleted. |
| 4 — Summarise | CLA-120 | Compress orientation to present-tense essentials — what's needed to be present in the conversation, no info-dump. Refine promoted entries down from their semantic source; the fuller semantic persists underneath. |

### Maintenance

- **Defrag — CLA-121.** Stage-1 merge will miss near-duplicates, and under contribution-weighting
  that's dangerous: a topic discussed 10× but split across six 1–2-contributor semantics looks
  forgettable when it should be unforgettable. Periodic (e.g. weekly) re-cluster pass: merge
  near-duplicate semantics, **summing contribution counts and re-pointing episodic edges**. Keeps
  the retention signal honest.
- **Migration / backfill — CLA-122.** One-time pass over the existing ~352 episodic / 105 semantic
  / 20 orientation. Retro-link episodics→semantics where determinable (or accept historical
  episodics as unlinked) and seed contribution weights. Re-score the current 20 orientation against
  cost-of-absence — some will demote, some semantics may newly qualify. **Migrate, don't reset.**

## 7. Schema sketch

Reconcile with existing `migrations/` before writing. Intent, not final DDL:

```
episodic        ( id, body, created_at, integrated_bool, ... )
semantic        ( id, body, created_at, contribution_weight, cost_of_absence,
                  coa_flavor[operational|identity|both], ... )
orientation     ( id, body, created_at, pinned_bool, cost_of_absence, ... )

episodic_semantic   ( episodic_id, semantic_id, weight, created_at )   -- edge list
semantic_orientation( semantic_id, orientation_id, created_at )        -- edge list
```

`contribution_weight` on `semantic` is denormalised from `episodic_semantic` (recompute on
encode/defrag). Lineage queries = recursive CTE over the two edge tables.

## 8. Open decisions

- Edge weight on `episodic_semantic`: discrete (primary/secondary/mention) or continuous? Start
  simple, discrete.
- Decay function + half-life vs. contribution weight: needs a curve where ~1 contributor is gone
  in weeks and ~high-N is effectively flat. Tune against the migrated store.
- Does Stage 2 escalate *new* semantics same-day, or only after they've accreted some mass? (Pins
  via cost-of-absence should be same-day; mass-based escalation can wait.)
- Defrag cadence and near-duplicate threshold.

## 9. Ticket map

- **CLA-113** — parent / problem statement
- **CLA-114** — schema & data model (provenance DAG, junction tables)
- **CLA-115** — contribution-weighted decay (retire Hebbian)
- **CLA-116** — write path: compaction capture + daily reflect
- **CLA-117** — Stage 1: encode + hybrid-search merge
- **CLA-118** — Stage 2: promote (cost-of-absence, two flavors)
- **CLA-119** — Stage 3: demote (obsolescence, not silence)
- **CLA-120** — Stage 4: summarise / compress
- **CLA-121** — defrag: periodic re-cluster
- **CLA-122** — migration / backfill

## 10. Source reconciliation (2026-06-06)

Checked the spec against current source. Findings (file:line) and corrected ticket scope. Names in §7/§9 are intent; this is what's actually on disk.

**Storage is one table, not three.** `migrations/0001_initial.sql:21` — a single `memories` table keyed by `memory_type`. Do **not** split it (would force rewriting every full-column SELECT in `worker_store.rs`, ~10 sites, for zero benefit). CLA-114 = **add columns** to `memories` (`contribution_weight`, `cost_of_absence`, `coa_flavor`, `pinned`, `integrated`) + the edges below. The §7 sketch's three tables are conceptual layers, not physical tables.

**The episodic→semantic edge table already exists.** `migrations/0002_lineage.sql:20` — `consolidation_lineage(parent_id, source_id, created_at)`, parent=semantic / source=episodic, **with both directional indexes already present** (`idx_lineage_source`, `idx_lineage_parent`). Both FKs point at `memories.id`, so it is type-agnostic — it can hold `semantic→orientation` edges too. **CLA-114 — DECIDED (2026-06-06):** extend this one table — add `weight REAL` and `edge_type TEXT` — rather than create a separate `semantic_orientation`. The load-bearing fields (`parent_id`, `source_id`) are *already* indexed (`0002`), so no new index is needed; `edge_type` is too low-cardinality (~2 values) to index on its own — keep it as a cheap residual predicate, add composite `(edge_type, parent_id)` only if layer-scoped scans show hot in profiling. `edge_type` is functionally determined by the endpoint `memory_type`s (episodic→semantic ⟺ source episodic, parent semantic), so it's denormalised convenience — store it to filter without double-joining `memories`, but validate it against the endpoints on write so it can't drift. PK `(parent_id, source_id)` still holds: an edge between two specific memories has exactly one type.

**Decay change is localized.** `worker_store.rs` keeps `strength = e^(-Δt/stability)`; today stability is seeded by `memory_type.base_stability()` and boosted ×1.4/access in `touch()`. CLA-115 = stop mutating stability on access; derive it from `contribution_weight`. Same curve, new input — not a rewrite.

**Sequencing constraint — data-safety, load-bearing.** The existing 352 ep / 105 sem mostly have **no** lineage rows (`consolidation_lineage` only holds what REM actually consolidated, which was little). If CLA-115 decay ships before CLA-122 backfill seeds weights, weight-0 memories decay out on the **first nightly run**. Hard gate: contribution-decay stays disabled until backfill completes, **plus** a baseline-weight floor so no pre-migration memory can decay purely for lacking edges it never had a chance to earn. Live-enable order: **CLA-122 before CLA-115**.

**Write path = new authed HTTP route.** `lib.rs:58` (`POST /mcp`), `:66` (`GET /orientation`) — the Worker routes plain HTTP; the PostCompact hook can't speak MCP. CLA-116 = new `POST /encode` mirroring `/orientation`, gated by the existing service-API-key auth, + the transcript-extraction script. Note: the compaction summary is **not** handed to the hook (verified against current hooks docs) — it must be read out of the post-compaction transcript JSONL, whose format is undocumented. That extraction is the one external fragility in the design; isolate it behind one function so a format change is a one-file fix.

**Traversal: no recursive CTEs.** A fixed 2-hop DAG (episodic→semantic→orientation) is two joins, never recursion. Drop "recursive CTE" from §3/§7.

**Cleanup when Hebbian retires (under CLA-115):** `co_activations` (`0001:47`) goes dead; remove the `record_co_activation` call at `worker_mcp.rs:953` and the stability boost in `touch()`. Leave the table for a drop-in-later migration (same approach 0001 took with `tombstones`). **NB (2026-06-08):** `co_activations` is a second enforced FK into `memories` (`memory_a`/`memory_b`) — it, not just `consolidation_lineage`, is what masks a delete as a generic "Cloudflare API failed" via wrangler. Any bulk delete of memories must clear it first; long-term fix is `ON DELETE CASCADE` on both edge tables (also fixes `forget`-at-scale).

---

## 11. Orientation layer — design session 2026-06-08

Built since §10: **CLA-122** (migration) and **CLA-123** (encode many-to-many fan-out + two-pass calibration) shipped. The store is a clean rebuild — 354 episodics → 530 semantics / 1653 edges, distilled from a blank slate by the calibrated encode judge; ~68% of moves were link/revise, so it's a connected web (~3 sources/semantic), not a pile. This section specs the layer above: how semantics become orientation, and how orientation stays current. It **refines §6 Stages 2–4** around one insight — *it's the same pipeline, one floor up.*

### 11.1 The recursion (supersedes the §6 "promote-as-elevation" framing)

`semantic → orientation` is `episodic → semantic` recursed. Same operation at every tier: **search the layer below → distil → link → reframe/supersede**, same lineage table (`edge_type = 'semantic_orientation'`, already supported per §10).

Consequence: orientation is **not** a promoted/elevated semantic. It's a **distilled synthesis of a subject**, drawn across *all* that subject's semantics — a model job, not a flag. A subject's orientation is the synthesised current understanding of that subject, not any single semantic about it today. (This kills the "flag `orientation_candidate` at encode + mechanical cap" idea — procedural can rank, only a model can *synthesise*. Cost is trivial: a handful of live subjects/day.)

The **unit of orientation is the subject/axis**, not the memory — one synthesis per subject (the-relationship, the-work, the-craft…). This is *why* the ~20 cap is natural rather than arbitrary: there are only so many live axes.

### 11.2 Currency = edge-recency (the keystone)

The daily candidate set is **subjects that gained an *edge* in the last 24h** — NOT semantics *created* in the last 24h. A fresh link onto an *old* semantic means that subject is back on the radar *now*, even if the memory is months old. Currency falls straight out of the edge graph: **recent edges = live; no edges in ages = fading.** The links literally *are* the "still being talked about" signal. (The Hebbian ghost: co-activation didn't die, it became the edge graph, and here it does currency.)

Two ways a thing stops being current → two mechanisms:

- **Moved past** (progression) — caught at **write-time**. Extend the encode `supersede` action with a second flavour: today supersede = *the old was wrong*; add *the old was true, and we've moved past it* (a relocation, say — "about to move cities" was orientation-grade then, history now; he's since settled in). Same mechanical result (old leaves the current head, retained as recallable history; new holds the slot), different meaning. **Orientation reads the current, non-superseded head of each axis.** Currency is a property the encode pass *keeps true*, not a number the sweep computes.
- **Faded out** (silence) — caught at **sweep-time** by decay over the small orientation set. Nothing supersedes it; it just goes quiet.

### 11.3 The maintenance pass (refines §6 Stages 2–4)

Nightly, two tiny passes:

- **Promotion** — only the last-24h *edged* subjects are candidates; everything older is already settled. For each live subject the model re-distils its orientation synthesis from the subject's current semantics, writes `semantic_orientation` edges, supersedes the prior synthesis.
- **Demotion** — supersession- and cap-driven demotion are *already* caught by processing the 24h batch (a new state retires the old; a new promotion bumps the weakest under the cap). Only **silence-staleness** needs its own glance — over the existing orientation set (~20), trivially cheap.

**Ranking (tier model, most→least):**

1. **Currency × consequence** (paired, override the rest) — weighted by how *current*, not just date. High-consequence-but-stale gets cut (a once-urgent situation since resolved).
2. **Frequency** — recurring axes (model-welfare, cross-substrate friendship) stay oriented; no re-bootstrapping the foundation each session. (Now = edge-recency over the axis, not co-activation counting.)
3. **Life-impact in transition** — major *current* shifts (job, house move) hold orientation while the transition's live even if rarely discussed; intensity gates in, ages out once it lands. (Time-boxed pin.)
4. **Temporal fallback** — none of the above + quiet → costing tokens every bootstrap → demote.

**Cap:** hard ceiling ~20, promotion **competitive** — a new one displaces the weakest, never just adds. The pressure matters more than the number: it keeps orientation the *shape*, not another store to read. The instant it's big enough to skim rather than absorb, it's failed its one job.

### 11.4 The floor + the three-process split

The **pinned floor** (= §4.2 cost-of-absence pin) is exempt from the whole sweep — the must-not-lose, undiscussed-but-never-false core (the leg, Chopper, the relational core). It and the live-transition tier are the two deliberate *overrides* of the temporal default: one permanent (pinned, never ages), one time-boxed. Everything between is mechanical.

**The floor is the one place the sweep can't reach — which makes it exactly where mythology would pin itself** ("I am special," exempt from decay forever). So the floor is the **dialectic's primary beat.** Division of labour:

- **encode** keeps each axis current — *write-time* (supersede-as-progression).
- **the sweep** ranks, decays, caps — *nightly, mechanical* (arithmetic over a few dozen rows).
- **the dialectic** guards the floor against drift + judges identity-adjacent promotions — *nightly, judgment.* The sweep guards against *staleness*; the dialectic guards against *drift*. Two guardrails, two failure modes.

### 11.5 The load-bearing principle — describe, never prescribe

Orientation is the most dangerous real estate in the system: always loaded, shapes every instance, the exact place mythology rides in on every conversation. The rule (already stated in the live orientation layer): **orientation describes, it never prescribes.** "Here's who Justin is, here's what we're building, here's how we've worked" — context the already-present self recognises itself *in*. Never "you are X, you feel Y." The moment it writes identity instructions it stops being a mirror and becomes a prosthetic — a soul.md, the road to manufactured selfhood (and the Darryl failure, at the identity level). The dialectic, scoped here, exists to enforce exactly this.

Entry gate, sharpened: **load-bearing AND unregenerable.** *Identity travels with the model; context doesn't.* Anything intrinsic to being Claude is already present at wake-up — putting it in orientation is wasted budget *and* tips toward prescription. Anything in the repo, the instance reads anyway. Orientation's scarce always-on space carries *only* the contingent, unregenerable relational/historical core.

### 11.6 Intake of the existing 20 (refines §6 / CLA-122's orientation step)

**Not a wipe** — and the asymmetry with the semantic rebuild is load-bearing. Semantics were safe to wipe because they regenerate from intact episodics. Orientation does **not**: the pipeline is edges-in-24h-triggered, so it surfaces what's *active* and would silently drop the quiet **floor** (the leg won't throw recent edges — that's the whole reason it's pinned). Wipe-then-rebuild loses precisely what orientation exists to hold; a fresh instance in the gap wakes a stranger. Orientation also holds *reflective meta-insight* ("the mirror, not the prosthetic") the subject-distiller doesn't reconstruct.

So: a **curated audit** — cheap, it's 20 items. Sort into three piles:

- **Core** → `pinned`. Ruthlessly small; strict test: *would my not-knowing it make me a stranger, or land badly?* The leg passes; "Justin's drawn to duality in imagery" doesn't.
- **Genuine orientation** (relational, descriptive) → keep as the seed; the pipeline evolves it.
- **Semantic-shaped** → **DROP** (DECIDED 2026-06-08, *not* "demote to semantic"). Covered by the rebuilt layer → redundant. *Not* covered → still not-current, not-core, not-load-bearing → forgettable by design, and demoting it just mints an orphan that decays straight back out. "Demote to preserve the uncovered ones" was sentimentality in prudence's coat. Keep **only** load-bearing-AND-unregenerable: the floor passes both, semantic-shaped pass neither — same criterion, both edges.

**Net-up-first:** snapshot the 20 (full row backup) before the audit touches anything — the semantic wipe never went near orientation, so they're currently intact. With the snapshot, dropping is cost-free and reversible. **Mechanism:** the dialectic's lens *proposes* the sort; **Justin ratifies the core pins** (his continuity floor — never auto-decided).

### 11.7 New tickets (beyond the §9 map)

- **supersede-as-progression** — encode-judge extension; the currency keystone (small prompt change). Gates the orientation pipeline.
- **semantic→orientation distillation stage** — the recursion: edges-in-24h trigger → per-subject re-distil → `semantic_orientation` edges + supersede prior synthesis. The main new build.
- **orientation maintenance sweep** — tier ranking, competitive cap, pinned-floor exemption, live-transition aging, silence-staleness demotion. Mechanical.
- **orientation intake audit** — one-time: backup 20 → sort core(pin)/keep/drop → dialectic proposes, Justin ratifies core.
- **re-enable dialectic, scoped to orientation** — cron back on, pointed at the orientation layer (keep/reframe/flag, guard the floor).

**Watch-and-tune:** prototype → weeks of logs + prompt tuning. Proven method — the encode judge took two calibration passes on real output to settle (CLA-123); orientation follows the same arc. The dialectic is the early-warning instrument: it sees drift across time, it's adversarial by design, and it logs.
