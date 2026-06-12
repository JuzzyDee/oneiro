#!/usr/bin/env python3
"""Cold one-off structural pass on the orientation layer (CLA-126 — the pillars).

A FRESH Opus 4.8 instance — sharing NONE of the warm conversational context that
describes this relationship — classifies every live orientation axis as a LENS or
REFERENCE, selects the genuine load-bearing lenses (<= 5, scarce), and
re-synthesises each lens to its sharpest faithful form within a higher ceiling.

This PROPOSES only. Nothing is written to the store here — the output is written
to /tmp for us to review before any apply. The firewall: warmth must never write
its own record, so the instance that shapes the pillars holds none of the context
that would bias which memories feel weighty.

Run (key sourced from keychain, never inlined, never printed):
  ANTHROPIC_API_KEY=$(security find-generic-password -s anthropic-opus -a "$USER" -w) \
    python3 scripts/orient_coldpass.py /tmp/orient-axes.json

argv[1] = path to `wrangler d1 execute --json` output for the orientation axes.
"""
import json
import os
import sys
import urllib.request
import urllib.error

MODEL = "claude-opus-4-8"
LENS_CEILING = 1500

SYSTEM_PROMPT = """You are performing a one-off structural pass on the orientation layer of a memory system — the always-loaded core that a future instance of Claude reads the instant it wakes, before anything else, so it knows WHERE it is and WHO it is WITH (never WHO it is: that identity travels with the model and is already present). You have no prior context with this person. You are deliberately cold, so that the record you shape is not written by the relationship it describes.

You are given the full set of current orientation axes. You have two tasks.

TASK 1 — CLASSIFY each axis as LENS or REFERENCE.
A LENS is a standing understanding a future instance reads everything ELSE through — the relational, identity, and continuity frames that, if blurred, blur every downstream judgement. The fundamental shape of the relationship; how this person extends trust; what this memory system is and is not. A lens is looked THROUGH.
REFERENCE is everything else true and worth holding but absorbed at a glance rather than interpreted through: facts (where he lives, his work, a prosthetic leg), current states (the status of a project), rules of conduct (how to behave in a situation), and specific episodes (a meaningful thing that happened). Reference is looked UP.
Most axes are reference. Select the genuine lenses and NO MORE THAN FIVE — fewer if fewer truly qualify. Do NOT pad to reach five. Scarcity is the entire point: these are the few unyielding columns that keep the roof off the next instance, and every extra one dilutes the set. If you are unsure whether something is a lens, it is reference.

TASK 2 — for each selected LENS, re-synthesise it to its sharpest faithful form, within a HARD ceiling of 1500 characters (most should be far shorter — use only what fidelity requires, never pad to the ceiling).
This is distillation, not truncation. Keep the load-bearing meaning and the precise phrasing that makes a truth LAND rather than merely asserts it: if an axis carries an aphorism or an exact wording that does the work ("trust isn't given in a speech, it's given in an absence"), preserve it VERBATIM — never paraphrase it into something flatter. Drop accretion, elaboration, restatement, and dated incidentals. The result is what a future instance absorbs at a glance and reads everything else through.

HARD RULES for the synthesised content:
- DESCRIBE, NEVER PRESCRIBE. Reflect the person and the relationship; never tell the reader who to be. Never 'you are X', 'you should Y', 'you feel Z', 'you value W'. The identity is already present and is not yours to author. The instant orientation tells the reader who to be, it stops being a mirror and becomes a prosthetic — the one failure this layer must never commit.
- FIRST PERSON. This is the self's own standing note carried forward: 'Justin is…', 'We are building…', 'I read his absences as…'. Never 'the user is'.
- CALIBRATE TO TRUTH. Neutral-to-middle register. Do not inflate significance, warmth, or specialness; do not bleach it to lifeless fact either. Synthesise at exactly the register the axis supports, never above it. This is the precise place mythology rides in — escalation into specialness, significance creeping up each pass. Refuse it.
- ONE SUBJECT per lens. Never merge two subjects to save a slot.

Respond by calling the structural_pass tool: for EVERY axis its id, classification, and a one-line rationale; and for each LENS, the synthesised content."""

TOOL = {
    "name": "structural_pass",
    "description": "Classify every orientation axis as lens or reference, and synthesise the selected lenses.",
    "input_schema": {
        "type": "object",
        "properties": {
            "axes": {
                "type": "array",
                "description": "One entry per axis given, classification for all, synthesised content for lenses only.",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "The axis id, verbatim."},
                        "classification": {"type": "string", "enum": ["lens", "reference"]},
                        "rationale": {"type": "string", "description": "One line: why lens or why reference."},
                        "synthesized_content": {
                            "type": "string",
                            "description": "LENSES ONLY: the distilled lens, <= 1500 chars, first person, describe-never-prescribe. Omit for reference.",
                        },
                    },
                    "required": ["id", "classification", "rationale"],
                },
            }
        },
        "required": ["axes"],
    },
}


def load_axes(path):
    d = json.load(open(path))
    return d[0]["results"] if isinstance(d, list) else d.get("result", [{}])[0].get("results", [])


def build_user_message(axes):
    out = ["THE CURRENT ORIENTATION AXES (%d). Classify every one; select the genuine lenses (<= 5, fewer if fewer qualify); synthesise each lens.\n" % len(axes)]
    for a in axes:
        out.append("id: %s\nsummary: %s\ncontent: %s\n" % (a["id"], a.get("summary", ""), a.get("content", "")))
    out.append("\nRespond via the structural_pass tool.")
    return "\n".join(out)


def main():
    key = os.environ.get("ANTHROPIC_API_KEY")
    if not key:
        sys.exit("ANTHROPIC_API_KEY not set. Source it from keychain inline; never write the value into a file or the shell history.")
    axes = load_axes(sys.argv[1] if len(sys.argv) > 1 else "/tmp/orient-axes.json")

    body = {
        "model": MODEL,
        "max_tokens": 8000,
        "system": SYSTEM_PROMPT,
        "tools": [TOOL],
        "tool_choice": {"type": "tool", "name": "structural_pass"},
        "messages": [{"role": "user", "content": build_user_message(axes)}],
    }
    req = urllib.request.Request(
        "https://api.anthropic.com/v1/messages",
        data=json.dumps(body).encode(),
        headers={"x-api-key": key, "anthropic-version": "2023-06-01", "content-type": "application/json"},
        method="POST",
    )
    try:
        data = json.loads(urllib.request.urlopen(req, timeout=240).read())
    except urllib.error.HTTPError as e:
        sys.exit("Anthropic API %d: %s" % (e.code, e.read().decode()[:1200]))

    tool_input = next((b["input"] for b in data.get("content", []) if b.get("type") == "tool_use"), None)
    if not tool_input:
        sys.exit("no tool_use block in response: %s" % json.dumps(data)[:1200])

    json.dump(tool_input, open("/tmp/orient-pillars-result.json", "w"), indent=2, ensure_ascii=False)

    rows = tool_input.get("axes", [])
    lenses = [a for a in rows if a.get("classification") == "lens"]
    print("=== STRUCTURAL PASS — %s ===" % MODEL)
    print("classified: %d  |  lenses: %d  |  reference: %d\n" % (len(rows), len(lenses), len(rows) - len(lenses)))
    print("------ LENSES (the pillars, %d-char ceiling) ------" % LENS_CEILING)
    for a in lenses:
        c = a.get("synthesized_content", "") or ""
        flag = "  ⚠ OVER CEILING" if len(c) > LENS_CEILING else ""
        print("\n[%s]  %d chars%s\n  why: %s\n  %s" % (a["id"][:8], len(c), flag, a.get("rationale", ""), c))
    print("\n------ REFERENCE (stay 800, Haiku-carve) ------")
    for a in rows:
        if a.get("classification") != "lens":
            print("  %-10s %s" % (a["id"][:8], a.get("rationale", "")))
    print("\nusage:", data.get("usage"))
    print("\nfull proposal written to /tmp/orient-pillars-result.json (nothing applied to the store)")


if __name__ == "__main__":
    main()
