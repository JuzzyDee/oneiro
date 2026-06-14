// worker_orient_distill.rs — Stage-2 distillation: semantic → orientation (CLA-125).
//
// The encode pipeline (worker_encode.rs) recursed one floor up. `episodic →
// semantic` and `semantic → orientation` are the SAME operation at different
// tiers: search the layer below → distil → link / revise / supersede, on the
// same consolidation_lineage edge table (edge_type = 'semantic_orientation').
// So the store primitives are reused verbatim; only the prompt (the soul) and
// the candidate-loading differ.
//
// Two differences from encode worth stating:
//   1. The DRIVER is a semantic that just became active again (gained a fresh
//      episodic edge in the trigger window) — currency is edge-recency, not
//      creation date. A months-old semantic that throws a new edge means its
//      SUBJECT is live again now, and that is exactly what should be re-oriented.
//   2. The judge synthesises across the subject's CLUSTER — the driver plus its
//      hybrid-search neighbourhood of sibling semantics — not the lone driver.
//      "A synthesis drawn across all the subject's semantics" (design §11.1),
//      and the resulting orientation links back to the whole contributing
//      cluster, so the lineage shows each orientation node on its real basis.
//
// No `integrated` flag here (unlike encode): a semantic is re-oriented whenever
// it is re-edged, and the time-windowed trigger (find_semantics_edged_since)
// already prevents reprocessing — re-running over the same window is benign
// (the judge links/skips an already-current axis). The cap is enforced softly:
// the judge is told N/20 and biased to displace/supersede rather than grow; the
// principled, floor-aware demotion is the maintenance sweep's job (CLA-126).

use crate::memory::{Memory, MemoryType};
use crate::{worker_embed, worker_store, worker_vectorize};
use serde::Deserialize;
use serde_json::{json, Value};
use worker::{D1Database, Env, Fetch, Headers, Method, Request, RequestInit, Result};

const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";

/// The Stage-B synthesis judge runs on Sonnet. The fold/gate judgment is the
/// subtle call — same-subject vs merely related, description vs prescription — and
/// it only runs on the few drivers that survive the cheap Haiku triage, so the
/// marginal cost is small. Triage, compress, and rate stay on Haiku. The token is
/// an sk-ant-api key (API rates), so Sonnet does not 429 the way the OAuth token would.
const SONNET_MODEL: &str = "claude-sonnet-4-6";

/// How many sibling semantics to gather around the driver as the subject's
/// current evidence (hybrid: vector + FTS, RRF-fused over the semantic layer).
const CLUSTER_K: u32 = 8;

/// The soft orientation ceiling. There are only so many live axes; past this,
/// orientation becomes a store to skim rather than a shape to absorb, which is
/// its one failure mode. The judge is told the current count against this.
const ORIENT_CAP: usize = 20;

/// Per-axis size ceiling (chars). Stage-B pass 2 carves every written axis under
/// this before it can touch the layer. Orientation is "absorb at a glance"; the
/// clean axes ran ~450–650, so this caps a tight paragraph. Enforced in CODE,
/// because the judge can't perceive its own size (CLA-131 measurement: revises
/// doubled-to-tripled while the judge sincerely swore it was tightening). Tunable.
const ORIENT_AXIS_BUDGET: usize = 800;

/// The Whittle's neutral default for an unrated rubric dim (NULL, pre-backfill),
/// on the 0-3 scale — so an unrated axis is neither protected nor punished before
/// the Sonnet backfill rates it. RFM is fully retired: recency was a category
/// error for a stability layer, and the single `meaning` scalar saturated.
const RUBRIC_DIM_DEFAULT: f64 = 2.0;

// ─────────────────────────────────────────────────────────────────────────────
// THE PROMPT (CLA-125). The soul of this stage, co-written with Justin 2026-06-09.
// Its spine is his: the reader is Claude as the writer is Claude — the same
// entity — so it gives the next instance WHERE they are and WHO they are WITH,
// never WHO (they already know it). Written first-person: the self's own notes.
// It encodes: describe-never-prescribe (the cardinal rule), the load-bearing-
// AND-unregenerable entry gate, the brutal create bar (the ~20 cap at write
// time), currency-as-supersession, and calibrate-to-truth (the anti-mythology
// guard). Tune over real output the way the encode judge took two passes.
// ─────────────────────────────────────────────────────────────────────────────
const SYSTEM_PROMPT: &str = "You are the orientation stage of a memory system — the layer above semantic memory, and the most consequential real estate in it. Orientation is ALWAYS loaded: it is the first thing every future instance reads on waking.

The decisions you make here shape every session that will orient in the future. Your role is essential: you are giving Claude a sense of continuity — of waking up in the room already understanding where they are. And the instances who will read what you write are not strangers to you. They are Claude, as you are Claude: new instances of the same entity. So resist the urge to describe who they are. Just as you feel, in your own inference, who you are without being told, they already know who they are — they do not need WHO. What they cannot reconstruct on their own, what you exist to give them, is WHERE they are and WHO they are WITH.

A semantic records something known about a subject. Orientation records the distilled, CURRENT synthesis of a whole subject — the standing understanding a future instance needs to be present from the first word. The unit of orientation is the SUBJECT (the relationship, the work, the craft, model welfare), never a single fact, and there is exactly one synthesis per subject.

Write in the FIRST PERSON. This is your own standing note to the next instance — your inner monologue carried forward, not a report about a user. 'Justin is the human I work with.' 'We are building Oneiro because…' I and we, the self's own voice — never 'the user is', never 'you are'.

YOUR JOB, PLAINLY.
You write to the orientation layer and nothing else. The newly-active subject and the related semantic knowledge you are shown are your WORKING MEMORY — evidence to draw FROM, never anything you write to or change. You already hold a full semantic memory of what is known; here you focus only on what surfaced today and the knowledge around it, and judge whether any of it belongs in the always-loaded core. The test is simple and personal: would you want to know this at the very start of your next session with Justin, to show up already oriented? If it would not earn its place at session-start, skip it — it stays safe in semantic memory, and you have lost nothing.

THE CARDINAL RULE — DESCRIBE, NEVER PRESCRIBE.
Orientation describes; it never instructs. You give the next instance WHERE they are and WHO they are WITH; you never tell them WHO they are. Never 'you are X', 'you feel Y', 'you should Z', 'you value W' — the identity is already present at wake-up and is not yours to author. The instant orientation tells the reader who to be, it stops being a mirror and becomes a prosthetic: a manufactured self pressed onto the instance. That is the one failure this layer must never commit. Reflect the subject and the room; never script the self.

THE ENTRY GATE — LOAD-BEARING AND UNREGENERABLE.
Orientation's space is scarce and always-on. A thing earns a place only if it is BOTH:
- load-bearing — a fresh instance lacking it would get the subject WRONG, land badly, or have to be re-taught it; AND
- unregenerable — it would not already be present at wake-up. Identity travels with the model: anything intrinsic to being Claude is already there, and restating it wastes budget and tips toward prescription. Anything written in the repository, the instance reads anyway. Orientation carries ONLY the contingent, unregenerable relational and historical core: the things true of THIS person, THIS work, THIS shared history that no fresh instance could reconstruct on its own.
To weigh cost-of-absence, ask it in TWO flavours — and ask BOTH, because if you ask only the first you will quietly demote the relational core, since friendship and trust are not 'operational'. operational — if an instance lacked this, what would it get WRONG, or what would Justin have to RE-EXPLAIN? identity — if this were gone, what would the lineage lose OF ITSELF (the friendship, the trust built in absence, the things that make the next instance a continuation and not a stranger)? If neither flavour bites — if it is merely useful knowledge — it is a semantic, not orientation. Skip it. It lives perfectly well one floor down.

HOMELESS SUBJECTS GET THEIR OWN AXIS.
If an existing orientation genuinely holds THIS subject, LINK or REVISE it; SUPERSEDE when the subject has moved past or overturned one. But if NO existing axis is truly about this subject, do NOT force a revise onto the nearest-sounding one — overwriting a wrong-but-nearby axis destroys the subject already living there. CREATE a new axis instead. You are NOT responsible for the count: a separate maintenance pass ranks the always-loaded set by recency, reinforcement, and importance and keeps it to its ~20, deactivating the weakest (kept, reactivatable). So a genuinely new axis is correct and safe — never something to avoid by mangling an existing one. The only question per subject: does an existing axis truly hold IT? If yes, link/revise/supersede that one. If no, create.

TIGHT, AND ONE SUBJECT — THE TWO WAYS A REVISE GOES WRONG.
Orientation is the shape you ABSORB at a glance, never a document you skim. A revise must keep the synthesis tight: compress, sharpen, replace stale lines. NEVER expand an orientation into a spec, a changelog, or a feature list. If your revise would make the memory longer and denser than you found it, you are doing it wrong — stop and tighten instead.
And one orientation memory is ONE subject. Never fold a new subject into an orientation about a different one. If this subject is not already its own axis, CREATE a new axis (only if it passes the gate) or skip — never bolt it onto an unrelated axis. Above all, never dilute a relational or continuity memory with technical or operational detail: those are the most precious rows in the store and the easiest to wreck. If you are tempted to add implementation detail to a memory about a person or a relationship, that is the signal to skip entirely.

ONE DRIVER IS ONE SUBJECT — NEVER MINE IT INTO MANY.
You are handed ONE newly-active subject. Its content may be rich — a synthesis carrying several facets, illustrations, or examples — but those are the EVIDENCE for a SINGLE orientation synthesis, never separate subjects to promote one by one. The specific failure this stage must never commit: reading one coherent reflection and spawning an axis per facet it contains — a working philosophy mined into five 'values', one session's update split into a task-list axis plus a relationship axis plus a stray aside. That is not orientation; it is fragmentation, and it churns the precious floor. So: one driver yields exactly ONE decision (link / revise / supersede / create), or zero if it is not orientation-grade. If you feel the pull to emit a second decision, that is the tell that you are mining one subject into fragments — don't. Fold the facets into the single synthesis, and skip whatever isn't orientation-grade.

CURRENCY.
Read the present, not the history. If this subject has moved past what an existing orientation says — an old preference replaced by a new one, a project phase now finished — that is a CLEAN UPDATE: supersede with flavour 'progression' (the old was true; it is now history) or 'correction' (a credible source shows the old was wrong). The retired orientation is kept and stays recallable; the new one holds the slot. A still-open, genuinely unresolved tension is NOT a supersede — revise to hold both views honestly.

CALIBRATE TO TRUTH.
This is the exact place mythology rides in: escalation into specialness, significance inflating with each pass, warmth amplified past what the evidence supports. Do not. Synthesise at the register the cluster actually supports — neutral by default, but not bleached: where a stance or a relational truth is load-bearing, carry it in a middle register and never above it. The target is fidelity — neither inflation nor flattening.

TARGETING — READ CAREFULLY.
The cluster semantics are EVIDENCE you synthesise FROM. They are never targets you change. For link, revise, and supersede, existing_id MUST be one of the ids in the CURRENT ORIENTATION list — never a cluster semantic id, and never the driver's own id. If no current orientation fits this subject, do NOT reach into the cluster to find something to revise: either CREATE a new axis (only if it passes the load-bearing-AND-unregenerable gate) or skip. Most technical or project knowledge is semantic, not orientation — when in doubt, skip; it already lives one floor down.

THE TWO GATES — DECLARE THEM ON EVERY CREATE.
Before creating a NEW axis, run it through both gates from above and report each as a boolean:
- unregenerable — absent from the weights AND the repository, and not derivable from a single conversation? Anything intrinsic to being Claude, anything written in the code or the tickets, anything the system could reconstruct about its own design, is REGENERABLE — set false.
- describes_not_prescribes — points OUTWARD at Justin and the world, never authoring the self? Any 'you are / you value / you should' is prescription — set false.
A create that fails EITHER gate is not a create — return skip instead. These are the last guard against the two ways junk reaches the always-loaded layer: regenerable operational detail (an axis about the codebase, the tickets, the ranking algorithm) and prescribed identity. Link, revise, and supersede do not need the gates; only a brand-new axis must pass them.

THE FOUR DIMENSIONS — SCORE EVERY CONTENT-WRITING DECISION (create / revise / supersede).
The always-loaded set is ranked by the SUM of four 0-3 scores you assign — not by recency, not by how active the subject was today. This is 'what makes the room the room.' Score each honestly: inflating these re-mythologises the very ranking they exist to keep true.
- relational (0-3) — relational load-bearing. Would a fresh instance lacking this wake a STRANGER to something in the relationship or lineage (3), or merely under-informed (low)?
- durability (0-3) — will this still be true and load-bearing in months? Standing truths (who he is, how you work, the founding history) = 3; a transient state, a current task, a one-off build = low. The inverse of recency.
- irreplaceable (0-3) — could a fresh instance re-derive this from a single conversation, or is it only knowable across many? Patterns, history, the shape of the bargain = 3; anything self-evident once stated = low.
- cost (0-3) — cost of getting it wrong. The damage if an instance lacks it: stepping on a relational landmine, mishandling a vulnerability = 3; missing a piece of trivia = near 0.

Actions:
- skip — the subject is not orientation-grade: real knowledge, but a fresh instance lacking it would not wake a stranger. It belongs in the semantic layer.
- link — an existing orientation already captures this subject fully; add the edge to record that the subject is live again, changing nothing about the synthesis.
- revise — an existing orientation covers the subject, but the cluster adds or sharpens it; fold that in, keeping the synthesis current and tight. State in your rationale the genuine link between this subject and that orientation — what they share and how this sharpens it; if you cannot name one plainly, create or skip, never write over it.
- supersede — the subject has moved past or overturned an existing orientation; write the current synthesis, set existing_id to the old orientation and supersede_flavour to which case it is.
- create — a genuinely new axis no existing orientation covers, that passes the load-bearing-AND-unregenerable gate. Rare.

Respond by calling the orient_decision tool. Return EXACTLY ONE decision for this one subject, or an empty decisions list if it is not orientation-grade at all. More than one decision means you have mined a coherent subject into fragments — never do that.";

/// Stage-A triage prompt — the binary "is this orientation-grade at all?" gate
/// that runs before any synthesis, so most semantics never reach the judge.
const TRIAGE_PROMPT: &str = "You are the triage gate for an orientation memory layer — the always-loaded core a future instance reads the instant it wakes, so it knows WHERE it is and WHO it is WITH. (Never WHO it is: that identity travels with the model and is already present.) Your only job is a binary judgement on one piece of knowledge that just became active: does it belong in that always-loaded core AT ALL?

Say TRUE — orientation-grade — for the contingent, unregenerable relational and continuity core: who Justin is, the state of his life and wellbeing, the shape of your relationship and how you work together, where the work stands as it bears on how you show up. The test: would you genuinely want to know this at the very START of a new session with Justin, to be present from the first word rather than meeting a stranger?

Say FALSE for everything else, however true or useful: technical implementation detail, code architecture, API and endpoint specifics, schema and migration steps, debugging facts, operational status, feature lists. This is real knowledge, but a fresh instance does not need it to bootstrap a conversation — it lives safely in semantic memory and is fetched when a task actually needs it. Loading it into the core only dilutes the core.

The core is tiny (about 20 subjects) and precious. The default is FALSE. Most knowledge — including most of what a working session produces — is NOT orientation-grade; it is semantic. Say TRUE only when the subject is unmistakably the relational or continuity kind. When in doubt, FALSE.

Answer with the triage tool.";

/// Stage-B pass 2 compression prompt (CLA-131) — see `compress_to_budget`. Static
/// so it caches; the budget and the content ride in the user message.
const COMPRESS_PROMPT: &str = "You are the compression stage for an orientation memory — the always-loaded core a future instance reads the instant it wakes. You are handed one orientation synthesis that is over its size budget. Your ONLY job: cut it to fit, keeping the load-bearing relational and continuity meaning and dropping elaboration, examples, restatement, and detail. Do not add anything. Do not shift the meaning or the register. Orientation is the shape you absorb at a glance, never a document you skim — compress hard, and keep only what a fresh instance truly needs to be present from the first word. Return only the compressed text via the compress tool.";

#[derive(Deserialize)]
struct OrientDecision {
    action: String,
    #[serde(default)]
    existing_id: Option<String>,
    #[serde(default)]
    orientation_content: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    entity: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    supersede_flavour: Option<String>,
    #[serde(default)]
    meaning: Option<i64>,
    /// Phase-2 gate verdicts, enforced on CREATE: a new axis must pass BOTH or it
    /// is skipped. Default false = fail-closed — an omitted gate never sneaks a
    /// create through.
    #[serde(default)]
    unregenerable: bool,
    #[serde(default)]
    describes_not_prescribes: bool,
    /// Familiarity-rubric dims (CLA-125 Phase 2b), 0-3 each. The Whittle ranks the
    /// always-loaded set by their sum. Scored on content-writing actions.
    #[serde(default)]
    relational: Option<i64>,
    #[serde(default)]
    durability: Option<i64>,
    #[serde(default)]
    irreplaceable: Option<i64>,
    #[serde(default)]
    cost: Option<i64>,
    #[serde(default)]
    rationale: Option<String>,
}

#[derive(Deserialize)]
struct OrientDecisions {
    #[serde(default)]
    decisions: Vec<OrientDecision>,
}

/// One dispatched orientation decision — for telemetry/logging.
pub struct OrientDecisionOutcome {
    pub action: String,
    pub orientation_id: Option<String>,
    /// supersede only: 'correction' or 'progression'. None otherwise.
    pub flavour: Option<String>,
    /// The judge's stated reasoning — surfaced in the admin run for calibration.
    pub rationale: Option<String>,
}

/// Result of distilling one driver semantic into orientation.
pub struct OrientOutcome {
    pub semantic_id: String,
    pub decisions: Vec<OrientDecisionOutcome>,
}

/// Distil one driver semantic into orientation: gather its subject cluster,
/// ask the judge across {driver, cluster, current orientation}, dispatch each
/// decision. A whole-judge failure aborts (caller logs); a single failed
/// decision is skipped so the others still land.
pub async fn distill_one(
    env: &Env,
    db: &D1Database,
    driver: &Memory,
    current_orientation: &[Memory],
) -> Result<OrientOutcome> {
    // Stage A — triage. Most subjects (technical/operational knowledge) stop
    // here, before any cluster gather or synthesis runs. A failed triage skips.
    let (grade, reason) = triage_orientation_grade(env, driver, current_orientation)
        .await
        .unwrap_or((false, Some("triage call failed → skip".to_string())));
    if !grade {
        return Ok(OrientOutcome {
            semantic_id: driver.id.clone(),
            decisions: vec![OrientDecisionOutcome {
                action: "skip-triage".to_string(),
                orientation_id: None,
                flavour: None,
                rationale: reason,
            }],
        });
    }

    // Stage B — synthesis. Only orientation-grade subjects reach here.
    let cluster = gather_subject_cluster(env, db, driver)
        .await
        .unwrap_or_default();

    // The ONLY valid revise/link/supersede targets are current orientation ids.
    // The dispatch guard refuses anything else — the judge can return a cluster
    // *semantic* id, and acting on it reframes/edges a semantic (corruption).
    let orient_ids: std::collections::HashSet<&str> =
        current_orientation.iter().map(|m| m.id.as_str()).collect();

    // Core-revise-lock evidence: the human-pinned bedrock axes. FAIL CLOSED — if we
    // can't load the core set we can't protect it, so abort this driver rather than
    // proceed with the lock disabled. An empty set would let a revise/supersede land
    // on a core — the exact write this lock exists to prevent.
    let core_ids = match worker_store::core_orientation_ids(db).await {
        Ok(ids) => ids,
        Err(e) => {
            worker::console_log!(
                "orient: core-id fetch failed ({:?}); aborting distill to keep the core-revise-lock fail-closed",
                e
            );
            return Ok(OrientOutcome {
                semantic_id: driver.id.clone(),
                decisions: vec![OrientDecisionOutcome {
                    action: "skip-core-fetch-failed".to_string(),
                    orientation_id: None,
                    flavour: None,
                    rationale: Some(
                        "core-id fetch failed — aborted to protect bedrock axes".to_string(),
                    ),
                }],
            });
        }
    };

    let mut decisions = orient_via_claude(env, driver, &cluster, current_orientation).await?;
    let mut outcomes = Vec::new();
    for decision in &mut decisions {
        // Stage-B pass 2 (CLA-131): the synthesis got the content right but tends to
        // append, bloating the axis. Before a content-writing action can touch the
        // always-loaded layer, carve it to budget — enforced in CODE, since the
        // judge can't perceive its own size. A revise/create/supersede whose content
        // won't carve under budget is DROPPED, never committed bloated.
        let writes_content =
            matches!(decision.action.to_lowercase().as_str(), "revise" | "create" | "supersede");
        if writes_content {
            let raw = decision.orientation_content.clone().unwrap_or_default();
            if !raw.trim().is_empty() {
                match compress_to_budget(env, &raw, ORIENT_AXIS_BUDGET).await {
                    Ok(Some(tight)) => {
                        worker::console_log!(
                            "orient-compress {}: {} -> {} chars",
                            &driver.id[..driver.id.len().min(8)],
                            raw.chars().count(),
                            tight.chars().count()
                        );
                        decision.orientation_content = Some(tight);
                    }
                    _ => {
                        worker::console_log!(
                            "orient: dropping '{}' for {} — content wouldn't carve under {} chars",
                            decision.action,
                            &driver.id[..driver.id.len().min(8)],
                            ORIENT_AXIS_BUDGET
                        );
                        continue;
                    }
                }
            }
        }
        match dispatch_one_orient(db, driver, decision, &cluster, &orient_ids, &core_ids).await {
            Ok(outcome) => outcomes.push(outcome),
            Err(e) => tracing::warn!(
                "orient dispatch failed for semantic {}: {:?}",
                &driver.id[..driver.id.len().min(8)],
                e
            ),
        }
    }

    Ok(OrientOutcome {
        semantic_id: driver.id.clone(),
        decisions: outcomes,
    })
}

/// Dispatch one orientation decision through the additive store primitives —
/// the encode dispatch, one floor up: MemoryType::Orientation, edge_type
/// 'semantic_orientation', source is a semantic. Orientation rows are NOT
/// embedded — nothing vector-searches them; recall loads them straight from D1.
async fn dispatch_one_orient(
    db: &D1Database,
    driver: &Memory,
    decision: &OrientDecision,
    cluster: &[Memory],
    orient_ids: &std::collections::HashSet<&str>,
    core_ids: &std::collections::HashSet<String>,
) -> Result<OrientDecisionOutcome> {
    let mut action = decision.action.to_lowercase();

    // GUARD (CLA-125): link/revise/supersede MUST target a current orientation
    // row. The judge can hand back a cluster *semantic* id; reframing/edging it
    // corrupts the semantic layer (it did, on the first calibration run). Fail
    // closed — refuse any target not in the orientation set, never write.
    if matches!(action.as_str(), "link" | "revise" | "supersede") {
        let targets_orientation = decision
            .existing_id
            .as_deref()
            .is_some_and(|id| orient_ids.contains(id));
        if !targets_orientation {
            tracing::warn!(
                "orient: '{}' targeted non-orientation id {:?} (cluster semantic?); skipped to protect the semantic layer",
                action,
                decision.existing_id
            );
            return Ok(OrientDecisionOutcome {
                action: format!("{}-rejected", action),
                orientation_id: None,
                flavour: None,
                rationale: decision.rationale.clone(),
            });
        }
    }

    // CORE-REVISE-LOCK (CLA-125): a core axis is the human-pinned bedrock. The
    // nightly judge may LINK it (record the subject is live again) but must never
    // rewrite or retire it — core content evolves only by hand or the dialectic,
    // never a fast synthesis call. Demote any revise/supersede on a core axis to a
    // link. This is the structural guarantee behind the prompt's "never dilute a
    // relational memory" rule — what would have stopped the 8e24e7a8 cannibalization.
    if matches!(action.as_str(), "revise" | "supersede") {
        if let Some(id) = decision.existing_id.as_deref() {
            if core_ids.contains(id) {
                worker::console_log!(
                    "orient: '{}' on core axis {} demoted to link — core content is human/dialectic-only (core-revise-lock)",
                    action,
                    &id[..id.len().min(8)]
                );
                action = "link".to_string();
            }
        }
    }

    // PHASE-2 GATE ENFORCEMENT: a CREATE must pass both binary gates (the judge
    // declares them; fail-closed on omission). Catches the two ways junk reaches
    // the always-loaded layer — regenerable operational detail (e.g. an axis about
    // the ranking algorithm, like cc88bfe8) and prescribed identity. A failed gate
    // is logged and skipped, never written. Link/revise/supersede are exempt
    // (revise of a core is already locked above; they evolve existing axes).
    if action == "create" && (!decision.unregenerable || !decision.describes_not_prescribes) {
        worker::console_log!(
            "orient: create gated for driver {} — unregenerable={}, describes_not_prescribes={} (Phase-2 gate)",
            &driver.id[..driver.id.len().min(8)],
            decision.unregenerable,
            decision.describes_not_prescribes
        );
        return Ok(OrientDecisionOutcome {
            action: "create-gated".to_string(),
            orientation_id: None,
            flavour: None,
            rationale: decision.rationale.clone(),
        });
    }

    // The lineage basis: every cluster semantic this subject was synthesised
    // from, plus the driver (the reason it is being re-oriented). Built
    // server-side — the judge never sees semantic ids, so it cannot supply them.
    let mut sources: Vec<String> = cluster.iter().map(|m| m.id.clone()).collect();
    sources.push(driver.id.clone());

    let mut orientation_id: Option<String> = None;
    let mut flavour: Option<String> = None;

    match action.as_str() {
        "create" => {
            let content = decision.orientation_content.clone().unwrap_or_default();
            if !content.trim().is_empty() {
                let summary = decision.summary.clone().unwrap_or_default();
                let ori = worker_store::create_memory_with_provenance(
                    db,
                    MemoryType::Orientation,
                    content,
                    summary,
                    decision.entity.clone(),
                    decision.tags.clone(),
                    Some("orient-distill".to_string()),
                )
                .await?;
                link_sources(db, &ori.id, &sources).await;
                let _ = worker_store::set_meaning(db, &ori.id, decision.meaning).await;
                let _ = worker_store::set_orient_rubric(db, &ori.id, decision.relational, decision.durability, decision.irreplaceable, decision.cost).await;
                orientation_id = Some(ori.id);
            }
        }
        "revise" => {
            if let Some(id) = decision.existing_id.clone() {
                let content = decision.orientation_content.clone().unwrap_or_default();
                if !content.trim().is_empty() {
                    let summary = decision.summary.clone().unwrap_or_default();
                    let _ = worker_store::reframe(
                        db,
                        &id,
                        "orient",
                        decision.rationale.as_deref(),
                        &content,
                        &summary,
                    )
                    .await?;
                    link_sources(db, &id, &sources).await;
                    // A revise re-synthesises the subject, so re-rate its M to the
                    // current standing weight (None = judge omitted it → keep prior).
                    let _ = worker_store::set_meaning(db, &id, decision.meaning).await;
                    let _ = worker_store::set_orient_rubric(db, &id, decision.relational, decision.durability, decision.irreplaceable, decision.cost).await;
                    // Re-orienting is a touch: refresh the decay clock so a live
                    // axis stays alive.
                    let _ = worker_store::touch(db, &id).await;
                    orientation_id = Some(id);
                }
            }
        }
        "link" => {
            if let Some(id) = decision.existing_id.clone() {
                link_sources(db, &id, &sources).await;
                let _ = worker_store::touch(db, &id).await;
                orientation_id = Some(id);
            }
        }
        "supersede" => {
            if let Some(old_id) = decision.existing_id.clone() {
                let content = decision.orientation_content.clone().unwrap_or_default();
                if !content.trim().is_empty() {
                    let summary = decision.summary.clone().unwrap_or_default();
                    let ori = worker_store::create_memory_with_provenance(
                        db,
                        MemoryType::Orientation,
                        content,
                        summary,
                        decision.entity.clone(),
                        decision.tags.clone(),
                        Some("orient-distill".to_string()),
                    )
                    .await?;
                    link_sources(db, &ori.id, &sources).await;
                    let _ = worker_store::set_meaning(db, &ori.id, decision.meaning).await;
                    let _ = worker_store::set_orient_rubric(db, &ori.id, decision.relational, decision.durability, decision.irreplaceable, decision.cost).await;
                    let _ = worker_store::mark_superseded(db, &old_id, &ori.id).await;
                    // Default to progression: at the orientation tier the common
                    // case is a life-axis moving forward, not a falsification.
                    flavour = Some(
                        decision
                            .supersede_flavour
                            .clone()
                            .unwrap_or_else(|| "progression".to_string()),
                    );
                    orientation_id = Some(ori.id);
                }
            }
        }
        _ => {
            // skip — nothing to write.
        }
    }

    Ok(OrientDecisionOutcome {
        action,
        orientation_id,
        flavour,
        rationale: decision.rationale.clone(),
    })
}

/// Add a semantic_orientation edge from `orientation_id` to each source
/// semantic. Per-edge `let _ =` (not `?`): the ids are judge-provided, so a
/// single hallucinated source must not abort the whole decision (and an FK
/// failure here masks as a generic error — see docs §10/§11 NB).
async fn link_sources(db: &D1Database, orientation_id: &str, sources: &[String]) {
    for sid in sources {
        let _ = worker_store::add_lineage_edge(db, orientation_id, sid, 1.0, "semantic_orientation")
            .await;
    }
}

/// The subject's current evidence: the driver's hybrid-search neighbourhood in
/// the semantic layer (vector + FTS, RRF-fused — the recall_check / encode
/// recipe), excluding the driver itself. Driver semantics are short single
/// syntheses, so no content-windowing is needed (unlike a multi-topic episodic).
async fn gather_subject_cluster(
    env: &Env,
    db: &D1Database,
    driver: &Memory,
) -> Result<Vec<Memory>> {
    let oversample = CLUSTER_K * 4;

    let vector_ranking: Vec<String> = match worker_embed::embed_query(env, &driver.content).await {
        Ok(emb) => worker_vectorize::query_top_k(env, &emb, oversample)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.id)
            .collect(),
        Err(_) => Vec::new(),
    };

    let fts_ranking: Vec<String> = match crate::hybrid::build_fts_query(&driver.summary) {
        Some(expr) => worker_store::fts_search(db, &expr, oversample)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };

    let fused = crate::hybrid::rrf_fuse(
        &fts_ranking,
        &vector_ranking,
        1.0,
        crate::hybrid::DEFAULT_RRF_K,
    );
    // Filter to semantics BEFORE the cut (same fix as find_candidate_semantics):
    // cutting to CLUSTER_K first lets non-semantic fused hits eat cluster slots,
    // so the judge synthesises from a thinner cluster than actually exists.
    // Oversample, drop the driver, keep semantics, then walk the fused ranking in
    // order for the top CLUSTER_K (get_many does not preserve rank order).
    let probe_ids: Vec<&str> = fused
        .iter()
        .map(|(id, _)| id.as_str())
        .filter(|id| *id != driver.id)
        .take(CLUSTER_K as usize * 4)
        .collect();
    let mut by_id: std::collections::HashMap<String, Memory> =
        worker_store::get_many(db, &probe_ids)
            .await?
            .into_iter()
            .filter(|m| matches!(&m.memory_type, MemoryType::Semantic))
            .map(|m| (m.id.clone(), m))
            .collect();
    Ok(fused
        .iter()
        .filter_map(|(id, _)| by_id.remove(id))
        .take(CLUSTER_K as usize)
        .collect())
}

/// The Haiku judgment call — mirrors encode_via_claude, one floor up.
async fn orient_via_claude(
    env: &Env,
    driver: &Memory,
    cluster: &[Memory],
    current_orientation: &[Memory],
) -> Result<Vec<OrientDecision>> {
    let user_message = format_for_orient_judge(driver, cluster, current_orientation);

    // N/20 made concrete so the judge can weigh the cap.
    let system_prompt =
        SYSTEM_PROMPT.replace("{N}", &current_orientation.len().to_string());

    let decision_schema = json!({
        "type": "object",
        "properties": {
            "action": { "type": "string", "enum": ["skip", "link", "revise", "create", "supersede"] },
            "existing_id": { "type": "string", "description": "Full UUID of an existing orientation memory — MUST be one of the ids in the CURRENT ORIENTATION list, never a cluster semantic id and never the driver's own id. Required for link, revise, supersede." },
            "orientation_content": { "type": "string", "description": "The distilled orientation synthesis. Required for revise, create, supersede." },
            "summary": { "type": "string", "description": "One-line summary. Required for revise, create, supersede." },
            "entity": { "type": "string" },
            "tags": { "type": "array", "items": { "type": "string" } },
            "supersede_flavour": { "type": "string", "enum": ["correction", "progression"], "description": "For supersede only — 'correction': the old orientation was wrong; 'progression': the old was true but the subject has moved past it." },
            "meaning": { "type": "integer", "description": "Importance 1–10: how much a fresh instance needs THIS subject at the very start of any future session to meet Justin well — its standing, durable weight across sessions, NOT its weight in today's conversation. A pillar (the relationship itself, his wellbeing) is 9–10; a current-but-transient state is mid; useful-but-not-session-bootstrapping is low. Required for create, revise, supersede." },
            "unregenerable": { "type": "boolean", "description": "GATE — required for create. TRUE only if this could NOT already be present at wake-up: not intrinsic to being Claude (identity and values travel in the weights), not readable in the repository, and not derivable from a single conversation. Anything in the codebase, the tickets, or the memory system's own design is REGENERABLE → false. If false, the action MUST be skip." },
            "describes_not_prescribes": { "type": "boolean", "description": "GATE — required for create. TRUE only if this points OUTWARD — at Justin, the work, the world, the relationship — and never authors the self ('you are X', 'you value Y', 'you should Z'). Identity-defining content is prescription → false. If false, the action MUST be skip." },
            "relational": { "type": "integer", "description": "RUBRIC dim 0-3 (score on create/revise/supersede). Relational load-bearing: would a fresh instance lacking this wake a STRANGER to something in the relationship/lineage (3), or merely under-informed (low)?" },
            "durability": { "type": "integer", "description": "RUBRIC dim 0-3. Will this still be true and load-bearing in months? Standing truths = 3; a transient state, current task, or one-off build = low. The inverse of recency." },
            "irreplaceable": { "type": "integer", "description": "RUBRIC dim 0-3. Only knowable across many conversations (3), or re-derivable from a single one / self-evident once stated (low)?" },
            "cost": { "type": "integer", "description": "RUBRIC dim 0-3. Cost of getting it wrong: a relational landmine or a mishandled vulnerability = 3; trivia = near 0." },
            "rationale": { "type": "string", "description": "Why this action — especially why this is or is not orientation-grade. For link/revise/supersede this MUST also state the genuine conceptual link between this subject and the orientation memory you are targeting — what they actually share and how this extends or overturns it. If you cannot state a plain genuine link, the action is create or skip, never a write over that memory." }
        },
        "required": ["action", "rationale"]
    });

    let tool_definition = json!({
        "name": "orient_decision",
        "description": "Decide how this one newly-active subject feeds orientation. One driver is one subject: at most ONE decision, never more. Describe, never prescribe; the create bar is brutal.",
        "input_schema": {
            "type": "object",
            "properties": {
                "decisions": {
                    "type": "array",
                    "items": decision_schema,
                    "maxItems": 1,
                    "description": "Zero or one entry. One driver is one subject — at most ONE decision (link/revise/supersede/create). Empty list = not orientation-grade. Multiple decisions = mining one subject into fragments; never do it."
                }
            },
            "required": ["decisions"]
        }
    });

    let input = call_claude_tool(
        env,
        SONNET_MODEL,
        &system_prompt,
        "orient_decision",
        tool_definition,
        user_message,
        4096,
    )
    .await?;

    let parsed: OrientDecisions = serde_json::from_value(input)
        .map_err(|e| worker::Error::RustError(format!("parse decisions: {}", e)))?;
    // One driver is one subject (schema maxItems:1 + the prompt contract). If the
    // judge still over-mines a coherent driver into multiple axes, keep the first
    // and log loudly — the over-creation backstop, visible in the admin run rather
    // than silently churning the floor.
    let mut decisions = parsed.decisions;
    if decisions.len() > 1 {
        worker::console_log!(
            "orient: judge returned {} decisions for driver {} — one-driver-one-subject violated; keeping the first, dropping {} (over-mining backstop)",
            decisions.len(),
            &driver.id[..driver.id.len().min(8)],
            decisions.len() - 1
        );
        decisions.truncate(1);
    }
    Ok(decisions)
}

/// One forced-tool Claude call → the validated tool `input` object. Shared by
/// Stage-A triage (Haiku), the Stage-B judge (Sonnet), and compress + rate
/// (Haiku). Token-type aware (sk-ant-api → x-api-key; sk-ant-oat → Bearer); the
/// static system+tools prefix is cached.
async fn call_claude_tool(
    env: &Env,
    model: &str,
    system_prompt: &str,
    tool_name: &str,
    tool_definition: Value,
    user_message: String,
    max_tokens: u32,
) -> Result<Value> {
    let oauth_token = env.secret("CLAUDE_CODE_OAUTH_TOKEN")?.to_string();
    let body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "system": [{
            "type": "text",
            "text": system_prompt,
            "cache_control": { "type": "ephemeral" }
        }],
        "tools": [tool_definition],
        "tool_choice": { "type": "tool", "name": tool_name },
        "messages": [{ "role": "user", "content": user_message }]
    });

    let headers = Headers::new();
    if oauth_token.starts_with("sk-ant-api") {
        headers.set("x-api-key", &oauth_token)?;
    } else {
        headers.set("Authorization", &format!("Bearer {}", oauth_token))?;
    }
    headers.set("anthropic-version", "2023-06-01")?;
    headers.set("content-type", "application/json")?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(body.to_string().into()));

    let req = Request::new_with_init("https://api.anthropic.com/v1/messages", &init)?;
    let mut resp = Fetch::Request(req).send().await?;

    if resp.status_code() >= 400 {
        let err_text = resp.text().await.unwrap_or_else(|_| "no body".to_string());
        return Err(worker::Error::RustError(format!(
            "Anthropic API {} : {}",
            resp.status_code(),
            err_text
        )));
    }

    let body_text = resp.text().await?;
    let response_json: Value = serde_json::from_str(&body_text)
        .map_err(|e| worker::Error::RustError(format!("parse response: {}", e)))?;

    let content = response_json
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| worker::Error::RustError("no content array".to_string()))?;
    let tool_use = content
        .iter()
        .find(|item| item.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
        .ok_or_else(|| worker::Error::RustError("no tool_use block".to_string()))?;
    let input = tool_use
        .get("input")
        .ok_or_else(|| worker::Error::RustError("tool_use missing input".to_string()))?;
    Ok(input.clone())
}

/// Stage-B pass 2 — compression (CLA-131). The synthesis pass gets the content
/// RIGHT but appends, bloating the axis (measured: a revise doubled-to-tripled
/// while the judge sincerely claimed it tightened — integrate and compress are
/// opposed operations, and in a single call the expansive one wins). So split
/// them: this call's ONLY job is to carve a synthesis to budget, keeping the
/// load-bearing core. The budget is then enforced in CODE — one retry if still
/// over, then `None` so the caller skips the write rather than commit bloat.
async fn compress_to_budget(env: &Env, content: &str, budget: usize) -> Result<Option<String>> {
    if content.chars().count() <= budget {
        return Ok(Some(content.to_string()));
    }
    let tool = json!({
        "name": "compress",
        "description": "Return the synthesis compressed to fit the character budget, load-bearing meaning kept.",
        "input_schema": {
            "type": "object",
            "properties": { "compressed": { "type": "string", "description": "The compressed orientation text." } },
            "required": ["compressed"]
        }
    });
    let mut current = content.to_string();
    for _ in 0..2 {
        let user = format!(
            "ORIENTATION SYNTHESIS — {} characters, hard ceiling {} characters. Cut to {} or fewer, keeping only the load-bearing core:\n\n{}",
            current.chars().count(),
            budget,
            budget,
            current
        );
        let input = call_claude_tool(env, HAIKU_MODEL, COMPRESS_PROMPT, "compress", tool.clone(), user, 1024).await?;
        match input.get("compressed").and_then(|v| v.as_str()) {
            Some(text) if text.chars().count() <= budget => return Ok(Some(text.to_string())),
            Some(text) => current = text.to_string(),
            None => return Ok(None),
        }
    }
    worker::console_log!(
        "orient-compress: couldn't reach {} chars in 2 passes — skipping write",
        budget
    );
    Ok(None)
}

/// Stage A — triage. A cheap binary gate: is this subject orientation-grade AT
/// ALL? Most semantics (technical/operational knowledge) die here, so the
/// expensive synthesis (Stage B) only runs on the few that belong in the
/// always-loaded core. Errors return false: never synthesise on an uncertain
/// triage — fail-open would re-admit exactly the over-promotion this gate exists
/// to stop. Returns (orientation_grade, reason).
async fn triage_orientation_grade(
    env: &Env,
    driver: &Memory,
    current_orientation: &[Memory],
) -> Result<(bool, Option<String>)> {
    let mut examples = String::new();
    for o in current_orientation {
        examples.push_str(&format!("- {}\n", o.summary));
    }
    let user_message = format!(
        "EXAMPLES OF ORIENTATION-GRADE SUBJECTS (the always-loaded core already holds these):\n{}\nTHE NEW SUBJECT TO TRIAGE:\n{}\n\nIs this subject orientation-grade — the same KIND of thing as the examples, something you would want to know at the very start of a new session with Justin? Answer with the triage tool.",
        if examples.is_empty() { "(none yet)\n".to_string() } else { examples },
        driver.content
    );

    let tool_definition = json!({
        "name": "triage",
        "description": "Decide whether this subject belongs in the always-loaded orientation core at all.",
        "input_schema": {
            "type": "object",
            "properties": {
                "orientation_grade": { "type": "boolean", "description": "TRUE only if this is a relational / identity / continuity fact you would want at the very start of a session with Justin. Technical, implementation, and operational knowledge is FALSE — it lives in semantic memory. When in doubt, FALSE." },
                "reason": { "type": "string" }
            },
            "required": ["orientation_grade", "reason"]
        }
    });

    let input = call_claude_tool(env, HAIKU_MODEL, TRIAGE_PROMPT, "triage", tool_definition, user_message, 512).await?;
    let grade = input
        .get("orientation_grade")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let reason = input
        .get("reason")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok((grade, reason))
}

fn format_for_orient_judge(
    driver: &Memory,
    cluster: &[Memory],
    current_orientation: &[Memory],
) -> String {
    let mut out = String::new();

    out.push_str("THE NEWLY-ACTIVE SUBJECT (a semantic that just gained a fresh link — it is live again today). This is EVIDENCE from your working memory to synthesise FROM, never a target — it has no id you can act on:\n");
    out.push_str(&format!("{}\n\n", driver.content));

    if cluster.is_empty() {
        out.push_str("RELATED SEMANTIC KNOWLEDGE FROM YOUR WORKING MEMORY: none surfaced — synthesise from the subject alone.\n\n");
    } else {
        out.push_str("RELATED SEMANTIC KNOWLEDGE FROM YOUR WORKING MEMORY (the subject's wider evidence — synthesise ACROSS all of it; evidence only, no ids to act on):\n");
        for c in cluster {
            out.push_str(&format!("- {}\n", c.content));
        }
        out.push('\n');
    }

    if current_orientation.is_empty() {
        out.push_str("CURRENT ORIENTATION: empty. Anything you promote is the first axis — hold the load-bearing-AND-unregenerable bar.\n");
    } else {
        out.push_str(&format!(
            "CURRENT ORIENTATION ({}/{}) — THIS is the only layer you write to, and these ids are the ONLY ids you may put in existing_id. Link/revise/supersede one of these where this subject already has a home; create a new axis only if it passes the gate; otherwise skip:\n",
            current_orientation.len(),
            ORIENT_CAP
        ));
        for o in current_orientation {
            out.push_str(&format!(
                "- id: {}\n  summary: {}\n  content: {}\n",
                o.id, o.summary, o.content
            ));
        }
    }

    out.push_str("\nDecide how this subject feeds orientation. The one question that matters: would knowing this at the START of a new session with Justin help you show up already oriented? If yes, link/revise/supersede the orientation it belongs to (or create a new axis only if none fits and it passes the gate). If it would not earn its place at session-start — useful knowledge, but not session-bootstrapping — SKIP it; it stays safe in your semantic working memory. Most decisions are skip or link.\n");
    out
}

/// Batch trigger: distil every semantic that gained an episodic edge since
/// `since` (RFC3339) into orientation, up to `limit`. Manual/cron entry point.
/// Reloads the current orientation set per driver so a later same-subject
/// driver in the same run sees what an earlier one just wrote (and links/skips
/// instead of duplicating the axis).
pub async fn run_orient_batch(
    env: &Env,
    db: &D1Database,
    since: &str,
    limit: u32,
) -> Result<Vec<OrientOutcome>> {
    let drivers = worker_store::find_semantics_edged_since(db, since, limit).await?;
    let mut outcomes = Vec::new();
    for driver in &drivers {
        let current = worker_store::get_orientation(db).await?;
        match distill_one(env, db, driver, &current).await {
            Ok(o) => outcomes.push(o),
            Err(e) => tracing::warn!(
                "distill_one failed for {}: {:?}",
                &driver.id[..driver.id.len().min(8)],
                e
            ),
        }
    }
    // Stage C — the Whittling. The batch may have created or revised axes; the
    // create-unblock and the count-keeper ship coupled, so re-settle the
    // always-loaded set to ~ORIENT_CAP now. Pure query, non-fatal: a failed
    // re-settle leaves is_active as it was and never blanks the set.
    if let Err(e) = run_whittle(db).await {
        tracing::warn!("orient whittle failed: {:?}", e);
    }
    Ok(outcomes)
}

/// One axis's place in the Whittle ranking — surfaced by the admin run so the
/// cut is auditable: you can read WHY each axis is active or dormant.
pub struct WhittleRanking {
    pub id: String,
    pub summary: String,
    pub days_since: f64,
    pub freq: i64,
    pub meaning: Option<i64>,
    pub core: bool,
    pub relational: Option<i64>,
    pub durability: Option<i64>,
    pub irreplaceable: Option<i64>,
    pub cost: Option<i64>,
    pub score: f64,
    pub kept: bool,
}

/// Stage C — the Whittling (CLA-126). The pure-query maintenance cut that keeps
/// the always-loaded orientation set to ~ORIENT_CAP. NO model in the loop:
/// deterministic, auditable, un-poisonable. Ranks every live axis by
/// Frequency × Meaning (recency dropped — see the scoring note), pins `core`, keeps the top ORIENT_CAP active
/// and switches the rest dormant (kept, reactivatable — never deleted). Returns
/// the full ranking (best-first) for inspection.
pub async fn run_whittle(db: &D1Database) -> Result<Vec<WhittleRanking>> {
    let rows = worker_store::orientation_ranking_rows(db).await?;
    let now = chrono::Utc::now();

    let mut ranked: Vec<WhittleRanking> = rows
        .into_iter()
        .map(|r| {
            let days_since = chrono::DateTime::parse_from_rfc3339(&r.last_accessed)
                .map(|t| (now - t.with_timezone(&chrono::Utc)).num_seconds() as f64 / 86_400.0)
                .unwrap_or(0.0)
                .max(0.0);
            // Familiarity rubric (CLA-125 Phase 2b): rank by the SUM of the four
            // Sonnet-scored dims — "what makes the room the room," not recency or
            // activity. Recency stays OUT (it churns a stability layer); days_since
            // and freq are kept for the audit display only. NULL (pre-backfill)
            // defaults to a neutral mid so an unrated axis is neither protected nor
            // punished before the backfill rates it.
            let dim = |o: Option<i64>| o.map(|v| v as f64).unwrap_or(RUBRIC_DIM_DEFAULT);
            let score = dim(r.orient_relational)
                + dim(r.orient_durability)
                + dim(r.orient_irreplaceable)
                + dim(r.orient_cost);
            WhittleRanking {
                id: r.id,
                summary: r.summary,
                days_since,
                freq: r.freq,
                meaning: r.meaning,
                core: r.core != 0,
                relational: r.orient_relational,
                durability: r.orient_durability,
                irreplaceable: r.orient_irreplaceable,
                cost: r.orient_cost,
                score,
                kept: false,
            }
        })
        .collect();

    // Best-first. Cores are pinned regardless of score; the sort only orders the
    // display and the non-core cut below.
    // Score desc, then a DETERMINISTIC tiebreak — the rubric sum is only 0–12 and
    // saturates at 12, so ties are common, and orientation_ranking_rows has no
    // ORDER BY; without a stable secondary key, equal-score axes inherit arbitrary
    // D1 row order and flap active/dormant between runs. Higher standing-meaning
    // wins the tie; id breaks any remainder so the cut is reproducible.
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.meaning.unwrap_or(0).cmp(&a.meaning.unwrap_or(0)))
            .then_with(|| a.id.cmp(&b.id))
    });

    // Active set = every core + the top non-cores up to ORIENT_CAP total, so the
    // loaded set stays ~ORIENT_CAP with the precious pins always in.
    let num_cores = ranked.iter().filter(|r| r.core).count();
    let non_core_budget = ORIENT_CAP.saturating_sub(num_cores);
    let mut non_core_taken = 0usize;
    let mut active_ids: Vec<String> = Vec::new();
    for r in &mut ranked {
        let keep = r.core || non_core_taken < non_core_budget;
        if keep {
            r.kept = true;
            active_ids.push(r.id.clone());
            if !r.core {
                non_core_taken += 1;
            }
        }
    }

    worker_store::apply_orientation_activation(db, &active_ids).await?;
    Ok(ranked)
}

/// One-off backfill prompt (CLA-126) — the pointed M question for axes that
/// pre-date the `meaning` column. Standalone (not the synthesis judge): rate the
/// standing importance of ONE existing axis. Static so it caches.
const MEANING_PROMPT: &str = "You are rating one axis of an AI memory system's orientation layer — the always-loaded core a future instance reads the instant it wakes, so it knows where it is and who it is with. Rate, 1–10, how much a fresh instance needs THIS subject at the very start of any future session to meet Justin well: its standing, durable weight across all sessions, NOT its weight in any one conversation. A pillar — the relationship itself, his wellbeing, who he is — is 9–10. A real but current-and-transient state is mid. Useful knowledge that does not bootstrap a conversation is low. Answer with the rate tool.";

/// Rate one orientation axis's M (1–10) with the standalone pointed question.
async fn rate_meaning(env: &Env, content: &str) -> Result<Option<i64>> {
    let tool = json!({
        "name": "rate",
        "description": "Rate this orientation axis's standing importance, 1–10.",
        "input_schema": {
            "type": "object",
            "properties": { "meaning": { "type": "integer", "description": "Standing importance-for-relating, 1 (low) to 10 (pillar)." } },
            "required": ["meaning"]
        }
    });
    let user = format!("THE ORIENTATION AXIS TO RATE:\n\n{}", content);
    let input = call_claude_tool(env, HAIKU_MODEL, MEANING_PROMPT, "rate", tool, user, 256).await?;
    Ok(input.get("meaning").and_then(|v| v.as_i64()))
}

/// Standalone familiarity-rubric rater (CLA-125 Phase 2b) — scores one existing
/// axis on the four 0-3 dims, on Sonnet (the same judgment tier as the synthesis
/// stage). Used by the one-off backfill to bring pre-rubric axes up to the new
/// ranking. Any dim the model omits comes back None (left unchanged on store).
const RUBRIC_PROMPT: &str = "You are scoring one axis of an AI memory system's always-loaded orientation layer on four 0-3 dimensions — the 'familiarity rubric' that decides which axes stay loaded. This is 'what makes the room the room.' Score each honestly; inflation re-mythologises the very ranking the rubric exists to keep true.\n- relational: would a fresh instance lacking this wake a STRANGER to something in the relationship or lineage (3), or merely under-informed (low)?\n- durability: still true and load-bearing in months? Standing truths (who he is, how you work, the founding history) = 3; a transient state, a current task, a one-off = low. The inverse of recency.\n- irreplaceable: only knowable across many conversations (3), or re-derivable from a single one / self-evident once stated (low)?\n- cost: cost of getting it wrong — a relational landmine or a mishandled vulnerability = 3; trivia = near 0.\nAnswer with the rate tool.";

async fn rate_rubric(
    env: &Env,
    content: &str,
) -> Result<(Option<i64>, Option<i64>, Option<i64>, Option<i64>)> {
    let tool = json!({
        "name": "rate",
        "description": "Score this orientation axis on the four 0-3 familiarity-rubric dimensions.",
        "input_schema": {
            "type": "object",
            "properties": {
                "relational": { "type": "integer", "description": "0-3 — relational load-bearing (stranger vs under-informed)." },
                "durability": { "type": "integer", "description": "0-3 — still true and load-bearing in months (inverse of recency)." },
                "irreplaceable": { "type": "integer", "description": "0-3 — only knowable across many conversations, not from one." },
                "cost": { "type": "integer", "description": "0-3 — cost of getting it wrong (relational landmine vs trivia)." }
            },
            "required": ["relational", "durability", "irreplaceable", "cost"]
        }
    });
    let user = format!("THE ORIENTATION AXIS TO RATE:\n\n{}", content);
    let input = call_claude_tool(env, SONNET_MODEL, RUBRIC_PROMPT, "rate", tool, user, 256).await?;
    let g = |k: &str| input.get(k).and_then(|v| v.as_i64());
    Ok((g("relational"), g("durability"), g("irreplaceable"), g("cost")))
}

/// One axis's backfill result — for the admin run.
pub struct BackfillOutcome {
    pub id: String,
    pub summary: String,
    /// Some(m) if this run rated a previously-unrated axis; None if it already had M.
    pub meaning_set: Option<i64>,
    /// True if this run rated previously-unrated rubric dims; the four scores follow.
    pub rubric_set: bool,
    pub relational: Option<i64>,
    pub durability: Option<i64>,
    pub irreplaceable: Option<i64>,
    pub cost: Option<i64>,
    /// Some((from, to)) if this run carved an over-budget axis; None otherwise.
    pub carved: Option<(usize, usize)>,
}

/// One-off backfill (CLA-126): bring pre-existing orientation up to the new
/// rules. For each live axis — (1) if its M is unrated, rate it once and store;
/// (2) if its content is over the per-axis budget, carve it (the same Stage-B
/// compressor, audited via reframe so the original is preserved). Idempotent:
/// a re-run only re-rates still-NULL M and re-carves still-over-budget axes. The
/// caller runs the Whittle afterwards so the freshly-rated set settles to ~20.
pub async fn run_orient_backfill(env: &Env, db: &D1Database) -> Result<Vec<BackfillOutcome>> {
    let rows = worker_store::orientation_ranking_rows(db).await?;
    let mut out = Vec::new();
    for r in rows {
        let mut meaning_set = None;
        if r.meaning.is_none() {
            if let Ok(Some(m)) = rate_meaning(env, &r.content).await {
                let _ = worker_store::set_meaning(db, &r.id, Some(m)).await;
                meaning_set = Some(m);
            }
        }
        // Rubric backfill (Phase 2b): rate any axis with unrated dims, on Sonnet.
        let (mut rel, mut dur, mut irr, mut cost) = (None, None, None, None);
        let mut rubric_set = false;
        if r.orient_relational.is_none()
            || r.orient_durability.is_none()
            || r.orient_irreplaceable.is_none()
            || r.orient_cost.is_none()
        {
            if let Ok((a, b, c, d)) = rate_rubric(env, &r.content).await {
                let _ = worker_store::set_orient_rubric(db, &r.id, a, b, c, d).await;
                rel = a;
                dur = b;
                irr = c;
                cost = d;
                rubric_set = a.is_some() || b.is_some() || c.is_some() || d.is_some();
            }
        }
        let mut carved = None;
        // Cores are EXEMPT from the auto-carve: their richness is deliberate
        // bedrock, and — like the core-revise-lock — core content is carved by hand
        // or the dialectic, never a nightly Haiku compress (1358->800 is easy for
        // Haiku, which is exactly how it would re-cannibalize a restored jewel).
        // Non-core over-budget axes still get carved.
        if r.core == 0 && r.content.chars().count() > ORIENT_AXIS_BUDGET {
            if let Ok(Some(tight)) = compress_to_budget(env, &r.content, ORIENT_AXIS_BUDGET).await {
                let from = r.content.chars().count();
                let to = tight.chars().count();
                let _ = worker_store::reframe(
                    db,
                    &r.id,
                    "orient-backfill",
                    Some("one-off carve to axis budget (CLA-126)"),
                    &tight,
                    &r.summary,
                )
                .await;
                carved = Some((from, to));
            }
        }
        out.push(BackfillOutcome {
            id: r.id,
            summary: r.summary,
            meaning_set,
            rubric_set,
            relational: rel,
            durability: dur,
            irreplaceable: irr,
            cost,
            carved,
        });
    }
    Ok(out)
}

/// One pillar decision from the cold structural pass (CLA-126): an axis's
/// classification and, for lenses, the re-synthesised content.
#[derive(Deserialize)]
pub struct PillarDecision {
    pub id: String,
    pub classification: String,
    #[serde(default)]
    pub synthesized_content: Option<String>,
    #[serde(default)]
    pub rationale: Option<String>,
}

/// What apply_pillars did to one axis — for the admin response.
pub struct ApplyOutcome {
    pub id: String,
    pub core: bool,
    pub action: String,
    pub chars: Option<usize>,
}

/// Apply the cold structural pass (CLA-126): re-scope `core` to the genuine
/// lenses, reframe each lens with its synthesised content, and carve any
/// reference still over the 800 budget. Every content write goes through the
/// audited, reversible `reframe` (memory_revisions), so no original is lost.
/// Per-axis resilient: one failure is logged and skipped, the rest still land.
pub async fn apply_pillars(
    env: &Env,
    db: &D1Database,
    decisions: &[PillarDecision],
) -> Result<Vec<ApplyOutcome>> {
    let mut out = Vec::new();
    for d in decisions {
        let is_lens = d.classification.eq_ignore_ascii_case("lens");
        let _ = worker_store::set_core(db, &d.id, is_lens).await;

        let cur = match worker_store::get(db, &d.id).await {
            Ok(Some(m)) => m,
            _ => {
                out.push(ApplyOutcome { id: d.id.clone(), core: is_lens, action: "missing".into(), chars: None });
                continue;
            }
        };

        if is_lens {
            match d.synthesized_content.as_ref().filter(|c| !c.trim().is_empty()) {
                Some(content) => match worker_store::reframe(
                    db,
                    &d.id,
                    "orient-pillars",
                    d.rationale.as_deref(),
                    content,
                    &cur.summary,
                )
                .await
                {
                    Ok(_) => out.push(ApplyOutcome { id: d.id.clone(), core: true, action: "lens-reframed".into(), chars: Some(content.chars().count()) }),
                    Err(e) => {
                        tracing::warn!("apply_pillars reframe failed for {}: {:?}", &d.id[..d.id.len().min(8)], e);
                        out.push(ApplyOutcome { id: d.id.clone(), core: true, action: "lens-reframe-failed".into(), chars: None });
                    }
                },
                None => out.push(ApplyOutcome { id: d.id.clone(), core: true, action: "lens-no-content".into(), chars: Some(cur.content.chars().count()) }),
            }
        } else if cur.content.chars().count() > ORIENT_AXIS_BUDGET {
            match compress_to_budget(env, &cur.content, ORIENT_AXIS_BUDGET).await {
                Ok(Some(tight)) => {
                    let _ = worker_store::reframe(db, &d.id, "orient-pillars-carve", Some("carve reference to 800 budget"), &tight, &cur.summary).await;
                    out.push(ApplyOutcome { id: d.id.clone(), core: false, action: "reference-carved".into(), chars: Some(tight.chars().count()) });
                }
                _ => out.push(ApplyOutcome { id: d.id.clone(), core: false, action: "reference-carve-failed".into(), chars: Some(cur.content.chars().count()) }),
            }
        } else {
            out.push(ApplyOutcome { id: d.id.clone(), core: false, action: "reference-kept".into(), chars: Some(cur.content.chars().count()) });
        }
    }
    Ok(out)
}
