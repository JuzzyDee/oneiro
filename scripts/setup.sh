#!/usr/bin/env bash
# scripts/setup.sh — one-command deploy for oneiro (CLA-96).
#
# Walks a new operator from `git clone` to a working Cloudflare Worker
# in one script. Creates D1 / Vectorize / KV / Queues / R2, generates
# OAuth credentials, prompts for an Anthropic API key, configures cron
# triggers for the user's timezone, pushes secrets, applies migrations,
# deploys.
#
# Designed for the audience already familiar with Claude but not
# necessarily with Cloudflare. Assumes wrangler is installed and the
# user has run `wrangler login` (script will prompt if not).
#
# Usage:
#   ./scripts/setup.sh                # interactive deploy
#   ./scripts/setup.sh --dry-run      # print actions without executing
#   NO_COLOR=1 ./scripts/setup.sh     # disable colored output

set -e
set -u
set -o pipefail

# ──── Setup ──────────────────────────────────────────────────────────

if [ -n "${NO_COLOR:-}" ] || [ ! -t 1 ]; then
    RED='' GREEN='' YELLOW='' BLUE='' BOLD='' DIM='' RESET=''
else
    RED=$'\033[31m'
    GREEN=$'\033[32m'
    YELLOW=$'\033[33m'
    BLUE=$'\033[34m'
    BOLD=$'\033[1m'
    DIM=$'\033[2m'
    RESET=$'\033[0m'
fi

DRY_RUN=false
for arg in "$@"; do
    case "$arg" in
        --dry-run|-n)
            DRY_RUN=true
            ;;
        --help|-h)
            sed -n '1,/^set -e/p' "$0" | sed 's/^# \?//;1d;$d'
            exit 0
            ;;
    esac
done

# ──── Helpers ────────────────────────────────────────────────────────

say()     { printf '%s\n' "$*"; }
header()  { printf '\n%s%s%s\n' "${BOLD}${BLUE}" "$*" "${RESET}"; }
ok()      { printf '  %s✓%s %s\n' "${GREEN}" "${RESET}" "$*"; }
warn()    { printf '  %s⚠%s  %s\n' "${YELLOW}" "${RESET}" "$*"; }
err()     { printf '  %s✗%s %s\n' "${RED}" "${RESET}" "$*" >&2; }
dim()     { printf '  %s%s%s\n' "${DIM}" "$*" "${RESET}"; }

# Run a command; in dry-run, print instead.
run() {
    if [ "$DRY_RUN" = true ]; then
        printf '  %s[dry-run]%s %s\n' "${YELLOW}" "${RESET}" "$*"
        return 0
    fi
    "$@"
}

# Prompt for input with optional default. Reads into the named variable.
prompt() {
    local var_name="$1"
    local prompt_text="$2"
    local default="${3:-}"
    local input
    if [ -n "$default" ]; then
        printf '  %s [%s]: ' "$prompt_text" "$default"
    else
        printf '  %s: ' "$prompt_text"
    fi
    read -r input
    if [ -z "$input" ] && [ -n "$default" ]; then
        input="$default"
    fi
    printf -v "$var_name" '%s' "$input"
}

# Prompt for secret input (no echo).
prompt_secret() {
    local var_name="$1"
    local prompt_text="$2"
    local input
    printf '  %s: ' "$prompt_text"
    read -rs input
    printf '\n'
    printf -v "$var_name" '%s' "$input"
}

# Detect whether `date` speaks GNU-style syntax (`-d <when>`) vs BSD/macOS
# (`-j -f`). Probe the CAPABILITY, not the version string: uutils coreutils
# (a Rust, GNU-compatible `date` now shipping on some Linux/WSL setups) parses
# `-d` fine but its `--version` says "uutils coreutils", so a "GNU coreutils"
# string-match wrongly routed it to the BSD branch and `date -j` blew up. The
# probe is true iff `-d @epoch` resolves (GNU and uutils); BSD date can't (its
# `-d` is the unrelated DST flag), so it errors/empties and we take the BSD path.
_is_gnu_date() {
    [ "$(date -u -d @0 +%Y 2>/dev/null)" = "1970" ]
}

# Cross-platform date arithmetic for "6 months from now in $TZ".
date_plus_six_months_offset() {
    local tz="$1"
    if _is_gnu_date; then
        TZ="$tz" date -d '+6 months' +%z
    else
        TZ="$tz" date -v+6m +%z
    fi
}

# Convert local HH:MM in given timezone to UTC HH:MM.
# Both platforms go via epoch — input is parsed in $tz, then re-emitted
# as UTC. BSD's `date -j -f ... -u +%H:%M` ignores the -u flag in
# practice, so we explicitly route through epoch instead.
local_to_utc() {
    local tz="$1"
    local hhmm="$2"
    local today epoch
    today=$(date +%Y-%m-%d)
    if _is_gnu_date; then
        epoch=$(TZ="$tz" date -d "$today $hhmm" +%s)
        date -u -d "@$epoch" +%H:%M
    else
        epoch=$(TZ="$tz" date -j -f "%Y-%m-%d %H:%M" "$today $hhmm" +%s)
        date -u -r "$epoch" +%H:%M
    fi
}

# Update a single value in wrangler.toml. Uses awk for context-aware edits.
# Args: <binding-marker> <key-name> <new-value>
# Finds lines like `<key-name> = "..."` AFTER seeing `<binding-marker>` and replaces.
toml_set_after_marker() {
    local marker="$1"
    local key="$2"
    local value="$3"
    if [ "$DRY_RUN" = true ]; then
        printf '  %s[dry-run]%s would set %s = "%s" after marker %s\n' \
            "${YELLOW}" "${RESET}" "$key" "$value" "$marker"
        return 0
    fi
    awk -v marker="$marker" -v key="$key" -v val="$value" '
        $0 ~ marker { found=1 }
        found && $0 ~ ("^" key " = ") {
            printf "%s = \"%s\"\n", key, val
            found=0
            next
        }
        { print }
    ' wrangler.toml > wrangler.toml.tmp && mv wrangler.toml.tmp wrangler.toml
}

# Replace the crons line with the 3-job nightly roster: CSCC, orient, dialectic.
toml_set_crons() {
    local cscc_cron="$1"
    local orient_cron="$2"
    local dialectic_cron="$3"
    if [ "$DRY_RUN" = true ]; then
        printf '  %s[dry-run]%s would set crons = ["%s", "%s", "%s"]\n' \
            "${YELLOW}" "${RESET}" "$cscc_cron" "$orient_cron" "$dialectic_cron"
        return 0
    fi
    awk -v cscc="$cscc_cron" -v orient="$orient_cron" -v dia="$dialectic_cron" '
        /^crons = / {
            printf "crons = [\"%s\", \"%s\", \"%s\"]\n", cscc, orient, dia
            next
        }
        { print }
    ' wrangler.toml > wrangler.toml.tmp && mv wrangler.toml.tmp wrangler.toml
}

# ──── Banner ─────────────────────────────────────────────────────────

cat <<EOF

${BOLD}============================================================
  Oneiro Setup
  A cognitive memory system for model continuity
============================================================${RESET}
EOF

if [ "$DRY_RUN" = true ]; then
    printf '\n%s%sDRY-RUN MODE%s — no Cloudflare resources will be created,\n' \
        "${BOLD}" "${YELLOW}" "${RESET}"
    printf '             no secrets will be pushed, no deploy will happen.\n'
fi

# ──── Step 1: Preflight ──────────────────────────────────────────────

header "[1/8] Preflight checks"

if [ ! -f Cargo.toml ]; then
    err "Run this from the repo root (where Cargo.toml lives)."
    exit 1
fi
ok "Repo root detected"

# Create wrangler.toml from the template on a fresh clone. The template
# is the canonical committed file; wrangler.toml itself is per-deploy
# and gitignored (CLA-97 PR 1). This means the script bootstraps cleanly
# from a fresh `git clone` with no prior state.
if [ ! -f wrangler.toml ]; then
    if [ -f wrangler.toml.example ]; then
        if [ "$DRY_RUN" = true ]; then
            dim "[dry-run] would: cp wrangler.toml.example wrangler.toml"
        else
            cp wrangler.toml.example wrangler.toml
        fi
        ok "Created wrangler.toml from wrangler.toml.example"
    else
        err "Missing both wrangler.toml and wrangler.toml.example."
        exit 1
    fi
else
    ok "wrangler.toml present (using existing)"
fi

if ! command -v wrangler >/dev/null 2>&1; then
    err "wrangler not installed."
    say "  Install: npm install -g wrangler"
    say "  Docs:    https://developers.cloudflare.com/workers/wrangler/install-and-update/"
    exit 1
fi
ok "wrangler installed ($(wrangler --version 2>&1 | head -1))"

if ! wrangler whoami >/dev/null 2>&1; then
    warn "Not logged into wrangler. Launching browser login..."
    run wrangler login
fi
ok "Logged into Cloudflare"

if ! command -v rustup >/dev/null 2>&1; then
    err "rustup not installed."
    say "  Install: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    exit 1
fi
if ! rustup target list --installed 2>/dev/null | grep -q '^wasm32-unknown-unknown$'; then
    warn "wasm32-unknown-unknown target not installed. Adding..."
    run rustup target add wasm32-unknown-unknown
fi
ok "rustup + wasm32-unknown-unknown target available"

if ! command -v openssl >/dev/null 2>&1; then
    err "openssl not found. Install via your package manager (brew install openssl on macOS)."
    exit 1
fi
ok "openssl available"

# ──── Step 2: Create Cloudflare resources ────────────────────────────

header "[2/8] Creating Cloudflare resources"

# Image storage (R2) — optional. Oneiro already requires the Workers Paid
# plan (Cloudflare Queues, which the encode pipeline depends on, is
# Paid-only), so billing is enabled either way. R2 just layers image
# features on top — decline it and every non-image feature still works.
say ""
say "  ${BOLD}Image storage (R2) — optional${RESET}"
dim "    Image features (remember_with_image, recall_image, rover camera"
dim "    heartbeats) need a Cloudflare R2 bucket. Oneiro already needs the"
dim "    Workers Paid plan for Queues, so enabling R2 adds no new billing"
dim "    requirement. Skip it and every non-image feature still works."
say ""
prompt ENABLE_R2 "Enable image storage? [y/N]" "n"
case "$ENABLE_R2" in
    y|Y|yes|YES) ENABLE_R2="y" ;;
    *)           ENABLE_R2="n" ;;
esac

# D1
say "  Creating D1 database 'oneiro-db'..."
if [ "$DRY_RUN" = true ]; then
    D1_ID="dryrun-d1-0000-0000-0000-000000000000"
    dim "[dry-run] wrangler d1 create oneiro-db"
else
    if D1_OUTPUT=$(wrangler d1 create oneiro-db 2>&1); then
        D1_ID=$(printf '%s' "$D1_OUTPUT" | grep -oE 'database_id = "[a-f0-9-]+"' | head -1 | sed 's/.*"\(.*\)"/\1/')
    else
        # Likely already exists; pull current value from wrangler.toml
        # Split on quote chars — for `database_id = "abc"`, a[2] is `abc`.
        # The earlier `gsub(/.*"|".*/, "")` pattern was broken: `.*"` matches
        # greedily to the LAST quote in the line, so the entire line gets
        # replaced and we get an empty string back.
        D1_ID=$(awk '/database_name = "oneiro-db"/{found=1} found && /database_id = /{split($0, a, "\""); print a[2]; exit}' wrangler.toml)
        warn "D1 'oneiro-db' may already exist. Using existing id from wrangler.toml: ${D1_ID}"
    fi
    if [ -z "${D1_ID}" ]; then
        err "Couldn't determine D1 database_id."
        exit 1
    fi
fi
ok "D1 database: oneiro-db (id: ${D1_ID})"

# Vectorize
say "  Creating Vectorize index 'oneiro-vectors'..."
if [ "$DRY_RUN" = true ]; then
    dim "[dry-run] wrangler vectorize create oneiro-vectors --dimensions=768 --metric=cosine"
else
    # Capture stderr and stdout so we can decide whether the failure was
    # an "already exists" we tolerate, or something we should surface.
    if VECTORIZE_OUT=$(wrangler vectorize create oneiro-vectors --dimensions=768 --metric=cosine 2>&1); then
        printf '%s\n' "$VECTORIZE_OUT" | tail -3
    else
        warn "Vectorize create returned non-zero (likely already exists):"
        printf '%s\n' "$VECTORIZE_OUT" | tail -3 | sed 's/^/    /'
    fi
fi
ok "Vectorize index: oneiro-vectors"

# KV — OAuth tokens
say "  Creating KV namespace 'ONEIRO_TOKENS'..."
if [ "$DRY_RUN" = true ]; then
    KV_ID="dryrunkv00000000000000000000000000"
    dim "[dry-run] wrangler kv namespace create ONEIRO_TOKENS"
else
    if KV_OUTPUT=$(wrangler kv namespace create ONEIRO_TOKENS 2>&1); then
        KV_ID=$(printf '%s' "$KV_OUTPUT" | grep -oE 'id = "[a-f0-9]+"' | head -1 | sed 's/.*"\(.*\)"/\1/')
    else
        KV_ID=$(awk '/binding = "TOKENS"/{found=1} found && /^id = /{split($0, a, "\""); print a[2]; exit}' wrangler.toml)
        warn "KV 'ONEIRO_TOKENS' may already exist. Using existing id from wrangler.toml: ${KV_ID}"
    fi
    if [ -z "${KV_ID}" ]; then
        err "Couldn't determine KV id."
        exit 1
    fi
fi
ok "KV namespace: ONEIRO_TOKENS (id: ${KV_ID})"

# KV — Version-check cache (CLA-102)
say "  Creating KV namespace 'ONEIRO_VERSION_CACHE'..."
if [ "$DRY_RUN" = true ]; then
    VERSION_KV_ID="dryrunvc00000000000000000000000000"
    dim "[dry-run] wrangler kv namespace create ONEIRO_VERSION_CACHE"
else
    if VKV_OUTPUT=$(wrangler kv namespace create ONEIRO_VERSION_CACHE 2>&1); then
        VERSION_KV_ID=$(printf '%s' "$VKV_OUTPUT" | grep -oE 'id = "[a-f0-9]+"' | head -1 | sed 's/.*"\(.*\)"/\1/')
    else
        VERSION_KV_ID=$(awk '/binding = "VERSION_CACHE"/{found=1} found && /^id = /{split($0, a, "\""); print a[2]; exit}' wrangler.toml)
        warn "KV 'ONEIRO_VERSION_CACHE' may already exist. Using existing id from wrangler.toml: ${VERSION_KV_ID}"
    fi
    if [ -z "${VERSION_KV_ID}" ]; then
        err "Couldn't determine VERSION_CACHE KV id."
        exit 1
    fi
fi
ok "KV namespace: ONEIRO_VERSION_CACHE (id: ${VERSION_KV_ID})"

# Queues — the capture pipeline. The encode hook (POST /encode) and the
# reflect tool enqueue captures here; the consumer writes the episodic
# and encodes it to semantics. wrangler.toml declares the producer +
# consumer bindings, but the queues themselves must exist before deploy
# or `wrangler deploy` fails on the missing queue. Create the DLQ first
# so the main queue's dead_letter_queue reference resolves.
#
# Cloudflare Queues is Workers-Paid-only — there is no free-tier path. If
# creation fails on a non-Paid account, stop with a clear message rather
# than letting `wrangler deploy` fail cryptically three steps later.
say "  Creating queues 'oneiro-capture-dlq' and 'oneiro-capture'..."
if [ "$DRY_RUN" = true ]; then
    dim "[dry-run] wrangler queues create oneiro-capture-dlq"
    dim "[dry-run] wrangler queues create oneiro-capture"
else
    for q in oneiro-capture-dlq oneiro-capture; do
        if Q_OUT=$(wrangler queues create "$q" 2>&1); then
            printf '%s\n' "$Q_OUT" | tail -2
        elif printf '%s' "$Q_OUT" | grep -qiE 'already exists|queue_already_exists'; then
            warn "Queue '$q' already exists; reusing it."
        else
            err "Queue create failed for '$q':"
            printf '%s\n' "$Q_OUT" | sed 's/^/    /'
            say ""
            err "Cloudflare Queues requires the Workers Paid plan (\$5/mo)."
            err "Enable it at dash.cloudflare.com → Workers & Pages → Plans,"
            err "then re-run ./scripts/setup.sh."
            exit 1
        fi
    done
fi
ok "Queues: oneiro-capture (+ DLQ)"

# R2 — only when the operator opted in above. Skipped builds get no
# IMAGES binding and the worker hides remember_with_image / recall_image
# from the MCP tools listing at runtime.
if [ "$ENABLE_R2" = "y" ]; then
    say "  Creating R2 bucket 'oneiro-images'..."
    if [ "$DRY_RUN" = true ]; then
        dim "[dry-run] wrangler r2 bucket create oneiro-images"
    else
        # Three outcomes worth distinguishing: success (proceed), bucket
        # already exists (proceed, harmless re-run), real failure (stop).
        # Treating every non-zero as "already exists" was the bug — billing
        # not enabled would silently uncomment the binding and surface as a
        # confusing wrangler deploy error two steps later.
        if R2_OUT=$(wrangler r2 bucket create oneiro-images 2>&1); then
            printf '%s\n' "$R2_OUT" | tail -3
        elif printf '%s' "$R2_OUT" | grep -qiE 'already exists|bucketalreadyexists'; then
            warn "R2 bucket 'oneiro-images' already exists; reusing it."
            printf '%s\n' "$R2_OUT" | tail -3 | sed 's/^/    /'
        else
            err "R2 bucket create failed:"
            printf '%s\n' "$R2_OUT" | sed 's/^/    /'
            say ""
            err "Common causes: billing not enabled on the Cloudflare account,"
            err "or insufficient R2 permissions on the API token. Re-run setup.sh"
            err "and decline R2 if you want a billing-free deploy."
            exit 1
        fi
        # Either created or already-exists: enable the binding. The example
        # ships [[r2_buckets]] commented out so wrangler deploy ignores it on
        # a no-R2 deploy. Idempotent: a second run with R2 already enabled
        # is a no-op (the `# ` prefix lines won't match).
        awk '
            /^# \[\[r2_buckets\]\]$/         { print "[[r2_buckets]]"; next }
            /^# binding = "IMAGES"$/         { print "binding = \"IMAGES\""; next }
            /^# bucket_name = "oneiro-images"$/ { print "bucket_name = \"oneiro-images\""; next }
            { print }
        ' wrangler.toml > wrangler.toml.r2tmp && mv wrangler.toml.r2tmp wrangler.toml
    fi
    ok "R2 bucket: oneiro-images (binding enabled in wrangler.toml)"
else
    dim "  Skipping R2 — image features will be disabled in this deploy."
    dim "  To enable later: uncomment [[r2_buckets]] in wrangler.toml,"
    dim "  run 'wrangler r2 bucket create oneiro-images', then redeploy."
fi

# Patch wrangler.toml with the new IDs.
if [ "$DRY_RUN" = true ]; then
    dim "[dry-run] would back up wrangler.toml and patch D1 + KV ids"
else
    cp wrangler.toml wrangler.toml.bak
fi
toml_set_after_marker 'database_name = "oneiro-db"' 'database_id' "$D1_ID"
toml_set_after_marker 'binding = "TOKENS"' 'id' "$KV_ID"
toml_set_after_marker 'binding = "VERSION_CACHE"' 'id' "$VERSION_KV_ID"
[ "$DRY_RUN" != true ] && ok "wrangler.toml patched (backup at wrangler.toml.bak)"

# ──── Step 3: Timezone + cron ────────────────────────────────────────

header "[3/8] Configuring schedule"

say "  Oneiro runs three nightly cognitive loops on cron triggers:"
dim "    CSCC defrag   — merges near-duplicate semantic memories"
dim "    Orient distil — distils stable semantics into the orientation layer"
dim "    Dialectic     — adversarially scrutinises orientation for drift"
say ""
say "  CSCC and orient run as a pair at your consolidation time (orient 30"
say "  minutes after CSCC, so the semantic layer is clean before it distils)."
say ""
say "  Common timezones:"
say "    1) Australia/Brisbane     (AEST, no DST)"
say "    2) Australia/Sydney       (AEST/AEDT)"
say "    3) America/Los_Angeles    (PST/PDT)"
say "    4) America/New_York       (EST/EDT)"
say "    5) Europe/London          (GMT/BST)"
say "    6) Europe/Berlin          (CET/CEST)"
say "    7) Asia/Tokyo             (JST, no DST)"
say "    8) Other (enter IANA name)"
prompt TZ_CHOICE "Choose timezone (number or IANA name)" "1"

case "$TZ_CHOICE" in
    1) TZ_NAME="Australia/Brisbane" ;;
    2) TZ_NAME="Australia/Sydney" ;;
    3) TZ_NAME="America/Los_Angeles" ;;
    4) TZ_NAME="America/New_York" ;;
    5) TZ_NAME="Europe/London" ;;
    6) TZ_NAME="Europe/Berlin" ;;
    7) TZ_NAME="Asia/Tokyo" ;;
    8) prompt TZ_NAME "Enter IANA timezone name (e.g., Pacific/Auckland)" ;;
    *) TZ_NAME="$TZ_CHOICE" ;;
esac

if ! TZ="$TZ_NAME" date >/dev/null 2>&1; then
    err "Unknown timezone: ${TZ_NAME}"
    exit 1
fi
ok "Timezone: ${TZ_NAME}"

validate_hhmm() {
    local val="$1"
    case "$val" in
        [01][0-9]:[0-5][0-9]|2[0-3]:[0-5][0-9]) return 0 ;;
        *) return 1 ;;
    esac
}

while true; do
    prompt CSCC_LOCAL "Consolidation time (HH:MM local, default 00:00)" "00:00"
    if validate_hhmm "$CSCC_LOCAL"; then
        break
    fi
    warn "Invalid time. Use HH:MM in 24-hour form, e.g. 00:00 or 18:30."
done

while true; do
    prompt DIALECTIC_LOCAL "Dialectic run time (HH:MM local, default 18:00)" "18:00"
    if validate_hhmm "$DIALECTIC_LOCAL"; then
        break
    fi
    warn "Invalid time. Use HH:MM in 24-hour form, e.g. 00:00 or 18:30."
done

CSCC_UTC=$(local_to_utc "$TZ_NAME" "$CSCC_LOCAL")
DIALECTIC_UTC=$(local_to_utc "$TZ_NAME" "$DIALECTIC_LOCAL")

# Strip leading zeros so cron sees "8" not "08" (cron expressions
# don't allow zero-padded numbers in some implementations).
# 10# prefix forces base-10 in bash arithmetic, which is the only
# place that syntax is understood — printf '%d' doesn't honor it.
CSCC_H=$((10#${CSCC_UTC%:*}))
CSCC_M=$((10#${CSCC_UTC#*:}))
DIA_H=$((10#${DIALECTIC_UTC%:*}))
DIA_M=$((10#${DIALECTIC_UTC#*:}))

# Orient runs 30 min after CSCC. Compute in minutes-since-midnight and
# wrap with mod 1440 so a late consolidation time (e.g. 23:45 → 00:15)
# rolls cleanly into the next UTC day instead of producing hour 24.
ORIENT_TOTAL=$(( (CSCC_H * 60 + CSCC_M + 30) % 1440 ))
ORIENT_H=$(( ORIENT_TOTAL / 60 ))
ORIENT_M=$(( ORIENT_TOTAL % 60 ))
printf -v ORIENT_UTC '%02d:%02d' "$ORIENT_H" "$ORIENT_M"

CSCC_CRON="${CSCC_M} ${CSCC_H} * * *"
ORIENT_CRON="${ORIENT_M} ${ORIENT_H} * * *"
DIALECTIC_CRON="${DIA_M} ${DIA_H} * * *"

toml_set_crons "$CSCC_CRON" "$ORIENT_CRON" "$DIALECTIC_CRON"
ok "CSCC:      ${CSCC_LOCAL} ${TZ_NAME} = ${CSCC_UTC} UTC (cron: ${CSCC_CRON})"
ok "Orient:    ${ORIENT_UTC} UTC (cron: ${ORIENT_CRON}) — 30m after CSCC"
ok "Dialectic: ${DIALECTIC_LOCAL} ${TZ_NAME} = ${DIALECTIC_UTC} UTC (cron: ${DIALECTIC_CRON})"

NOW_OFFSET=$(TZ="$TZ_NAME" date +%z)
if SIXMO_OFFSET=$(date_plus_six_months_offset "$TZ_NAME" 2>/dev/null) \
    && [ -n "$SIXMO_OFFSET" ] && [ "$NOW_OFFSET" != "$SIXMO_OFFSET" ]; then
    warn "Timezone ${TZ_NAME} observes DST. Your schedule will shift by an hour seasonally."
    dim "Re-run setup.sh at the DST boundary to fix, or accept the drift."
fi

# ──── Step 4: Generate credentials ───────────────────────────────────

header "[4/8] Generating credentials"

CLIENT_ID="oneiro-$(openssl rand -hex 4)"
CLIENT_SECRET=$(openssl rand -hex 32)

cat <<EOF

  ${BOLD}${YELLOW}⚠  SAVE THESE NOW — only displayed once.${RESET}
  ${YELLOW}If lost, re-run ./scripts/setup.sh to regenerate.${RESET}

  ${BOLD}ONEIRO_OAUTH_CLIENT_ID:${RESET}     ${CLIENT_ID}
  ${BOLD}ONEIRO_OAUTH_CLIENT_SECRET:${RESET} ${CLIENT_SECRET}

EOF
prompt SAVED "Saved these? Type 'yes' to continue"
if [ "$SAVED" != "yes" ] && [ "$SAVED" != "YES" ] && [ "$SAVED" != "y" ]; then
    err "Setup aborted. Re-run when ready to save the credentials."
    exit 1
fi

# Optional starter service API key. Service keys let non-interactive
# callers (orient hook, rover, custom scripts) authenticate without an
# OAuth dance. The keygen subcommand is always available later via
# `cargo run --bin oneiro -- keygen --role <role>`; this just folds the
# first-key path into setup so the orient hook works out of the box.
say ""
say "  ${BOLD}Starter service API key — optional${RESET}"
dim "    A service key lets the orient hook, the rover, or any other"
dim "    non-interactive client authenticate against Oneiro without"
dim "    OAuth. You can mint one now or skip and generate later via"
dim "    'cargo run --bin oneiro -- keygen --role rover'."
say ""
prompt MAKE_API_KEY "Generate a starter rover service key now? [y/N]" "n"
case "$MAKE_API_KEY" in
    y|Y|yes|YES) MAKE_API_KEY="y" ;;
    *)           MAKE_API_KEY="n" ;;
esac

RAW_API_KEY=""
API_KEY_ENTRY=""
if [ "$MAKE_API_KEY" = "y" ]; then
    if [ "$DRY_RUN" = true ]; then
        dim "[dry-run] cargo run --bin oneiro -- keygen --role rover --quiet"
        RAW_API_KEY="mk_rover_dryrunplaceholderrawkeyvalue00"
        API_KEY_ENTRY="rover:\$argon2id\$v=19\$m=19456,t=2,p=1\$dryrunsaltvalue\$dryrunhashvalue"
        ok "Service key (dry-run synthetic)"
    else
        say "  Building keygen binary (cargo run, first invocation may compile)..."
        if KEYGEN_OUT=$(cargo run --quiet --bin oneiro -- keygen --role rover --quiet 2>/dev/null); then
            RAW_API_KEY=$(printf '%s\n' "$KEYGEN_OUT" | sed -n '1p')
            API_KEY_ENTRY=$(printf '%s\n' "$KEYGEN_OUT" | sed -n '2p')
        fi
        if [ -z "$RAW_API_KEY" ] || [ -z "$API_KEY_ENTRY" ]; then
            err "keygen produced no output — check 'cargo run --bin oneiro -- keygen --role rover'"
            exit 1
        fi
        ok "Service key generated (role: rover) — raw key shown in final summary"
    fi
fi

# ──── Step 5: Anthropic API key ──────────────────────────────────────

header "[5/8] Anthropic API key"

if [ "$DRY_RUN" = true ]; then
    dim "[dry-run] would prompt for Anthropic API key (skipping interactive read)"
    ANTHROPIC_API_KEY="sk-ant-api-dryrun-placeholder-key-not-real"
    ok "Key captured (dry-run synthetic)"
else
    say "  Oneiro's nightly loops (CSCC defrag, orient distil, dialectic)"
    say "  call Haiku 4.5 and Sonnet 4.6 via the Anthropic Messages API."
    say "  They bill to a standard Anthropic API key at API rates — a few"
    say "  dollars a month in normal use."
    say ""
    say "  Create a key at:"
    say "    ${BOLD}https://console.anthropic.com/settings/keys${RESET}"
    say ""
    say "  Copy the key (starts with ${BOLD}sk-ant-api${RESET}) and paste here."

    prompt_secret ANTHROPIC_API_KEY "Paste Anthropic API key"
    if [ -z "$ANTHROPIC_API_KEY" ] || ! [[ "$ANTHROPIC_API_KEY" == sk-ant-api* ]]; then
        err "That doesn't look like an Anthropic API key (expected prefix sk-ant-api)."
        exit 1
    fi
    ok "Key captured"
fi

# ──── Step 6: Push secrets ───────────────────────────────────────────

header "[6/8] Pushing secrets to Cloudflare"

push_secret() {
    local name="$1"
    local value="$2"
    if [ "$DRY_RUN" = true ]; then
        dim "[dry-run] wrangler secret put $name"
        return 0
    fi
    printf '%s' "$value" | wrangler secret put "$name" >/dev/null 2>&1
}

push_secret "ONEIRO_OAUTH_CLIENT_ID" "$CLIENT_ID"
ok "ONEIRO_OAUTH_CLIENT_ID"
push_secret "ONEIRO_OAUTH_CLIENT_SECRET" "$CLIENT_SECRET"
ok "ONEIRO_OAUTH_CLIENT_SECRET"
# Secret name is historical (CLA-117 made the auth token-type-aware): the
# worker reads CLAUDE_CODE_OAUTH_TOKEN and routes sk-ant-api* values to the
# x-api-key header. We store the Anthropic API key under that name so the
# runtime contract is unchanged — renaming would touch 8 modules.
push_secret "CLAUDE_CODE_OAUTH_TOKEN" "$ANTHROPIC_API_KEY"
ok "CLAUDE_CODE_OAUTH_TOKEN (holds the Anthropic API key)"

if [ -n "$API_KEY_ENTRY" ]; then
    push_secret "ONEIRO_API_KEYS" "$API_KEY_ENTRY"
    ok "ONEIRO_API_KEYS (rover service key hash)"
fi

# Stage 3 dispatcher mode. The worker defaults to dry_run when this is
# missing — that's a fail-safe for in-place operator deploys (burn-in
# observation period before flipping live). Fresh consumer deployments
# via setup.sh don't want that — they want a working dialectic out of
# the box, not silent audit rows that never act on anything.
push_secret "ONEIRO_DIALECTIC_DISPATCH" "on"
ok "ONEIRO_DIALECTIC_DISPATCH (on — dialectic dispatches reframes/flags live)"

# ──── Step 7: Apply migrations ───────────────────────────────────────

header "[7/8] Applying database migrations"
if [ "$DRY_RUN" = true ]; then
    dim "[dry-run] wrangler d1 migrations apply oneiro-db --remote"
else
    # Pipe a 'y' to auto-confirm the migration prompt. Newer wrangler
    # versions dropped the --yes flag in favour of TTY detection — and
    # rather than depending on wrangler's auto-detect (which got it wrong
    # for an interactive setup.sh run on 2026-05-18), we answer the prompt
    # deterministically. Capture stderr for failure handling.
    if MIGRATE_OUT=$(printf 'y\n' | wrangler d1 migrations apply oneiro-db --remote 2>&1); then
        printf '%s\n' "$MIGRATE_OUT" | tail -10
    else
        err "Migrations failed. Output:"
        printf '%s\n' "$MIGRATE_OUT" | sed 's/^/    /'
        exit 1
    fi
fi
ok "Migrations applied"

# ──── Step 8: Deploy ─────────────────────────────────────────────────

header "[8/8] Deploying worker"
if [ "$DRY_RUN" = true ]; then
    dim "[dry-run] wrangler deploy"
    WORKER_URL="https://oneiro-dryrun.workers.dev"
else
    DEPLOY_OUTPUT=$(wrangler deploy 2>&1)
    printf '%s\n' "$DEPLOY_OUTPUT" | tail -8
    WORKER_URL=$(printf '%s' "$DEPLOY_OUTPUT" \
        | grep -oE 'https://[a-zA-Z0-9.-]+\.workers\.dev' | head -1)
    if [ -z "$WORKER_URL" ]; then
        WORKER_URL="(check wrangler deploy output above)"
    fi
fi
ok "Deployed: ${WORKER_URL}"

# ──── Final summary ──────────────────────────────────────────────────

# Service key — surfaced AFTER deploy succeeds so the hash on the worker
# matches the raw value we're about to print. Shown first because it's
# unrecoverable from the stored hash; the operator must capture it now.
if [ -n "$RAW_API_KEY" ]; then
    cat <<EOF

${BOLD}${YELLOW}============================================================
  ⚠  STARTER SERVICE KEY — STORE THIS NOW
============================================================${RESET}

  ${BOLD}Role:${RESET}    rover
  ${BOLD}Raw key:${RESET}

    ${BOLD}${RAW_API_KEY}${RESET}

  ${YELLOW}The hash is on the worker; the raw key cannot be recovered.${RESET}
  ${YELLOW}Save it to a password manager / .env now.${RESET}

  ${BOLD}Common uses${RESET}
    Orient hook (macOS keychain):
      ${DIM}security add-generic-password -s "oneiro-orient" -a "\$USER" -w '<paste raw key>'${RESET}
    Rover .env / any non-interactive client:
      ${DIM}ONEIRO_MCP_TOKEN=<paste raw key>${RESET}

EOF
fi

cat <<EOF

${BOLD}${GREEN}============================================================
  Setup complete!
============================================================${RESET}

  ${BOLD}Worker URL${RESET}     ${WORKER_URL}

  ${BOLD}Connect Claude.ai${RESET}
    Settings → Connectors → Add Custom Connector
    URL:           ${WORKER_URL}/mcp
    Client ID:     ${CLIENT_ID}
    Client Secret: (the one you saved above)

  ${BOLD}If you see "invalid_request: redirect_uri not registered"${RESET}
    Copy the URI from the 400 response body, then:
      ${DIM}wrangler secret put ONEIRO_OAUTH_REDIRECT_URIS${RESET}
      ${DIM}# enter: claude://oauth-callback;<URI from error>${RESET}

  ${BOLD}Verify Oneiro is running${RESET}
    Open a Claude.ai conversation. Oneiro should appear as an MCP
    tool. Try asking Claude to remember something, then start a
    new conversation and ask it to recall.

  ${BOLD}Inspect cognitive activity${RESET}
    ${DIM}wrangler d1 execute oneiro-db --remote --command \\${RESET}
    ${DIM}  "SELECT action, keeper_id, decided_at FROM cscc_decisions ORDER BY decided_at DESC LIMIT 5"${RESET}

${BOLD}${GREEN}============================================================${RESET}

EOF
