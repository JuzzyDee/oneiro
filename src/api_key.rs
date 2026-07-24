// api_key.rs — Service API keys for Oneiro
//
// Additive to OAuth (auth.rs). User-facing clients (Claude Code, Web, iOS)
// continue to flow through the OAuth path. Service callers — the rover,
// future automation — authenticate with static, hashed-at-rest API keys.
//
// Security shape (see CLA-86 ticket for the full design rationale):
//   1. Hashed at rest: oneiro stores Argon2 hashes; raw keys exist only
//      on the client side.
//   2. Self-identifying format: mk_<role>_<32-byte-random>. The role is
//      readable from a leaked key at a glance.
//   3. Per-role capability allowlists (added in phase 3) — leaked keys
//      have bounded blast radius, can never reframe/forget/reflect.
//   4. Entity binding on writes (added in phase 4) — rover-role writes
//      are forced to entity="rover" server-side regardless of payload.
//   5. Per-key rate limits (added in phase 5).
//   6. Audit trail (added in phase 6) — every api-key-authenticated call
//      logs {timestamp, key_id, tool, success}.
//
// This file currently implements phase 1: key generation and the Role enum.

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use rand::Rng;
use sha2::{Digest, Sha256};

/// Roles supported for service API keys. Roles are hardcoded rather than
/// per-key configurable — adding a new role is a deliberate code change,
/// not a config change. This keeps the capability surface auditable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The rover's heartbeat loop. Reads + remember/remember_with_image,
    /// with entity forced to "rover" on writes (enforced in phase 4).
    Rover,
    /// Claude Code harness hooks (CLA-116). Orients to open a session
    /// (recall_orient) and encodes a capture to close it (/encode) — and
    /// nothing else. A leaked hook key cannot read the store
    /// (recall_specific/recall_check) or write arbitrary memories.
    Hook,
    /// The Beacon — a battery e-paper desk device that wakes daily and
    /// fetches one memory to display (GET /beacon). Allows nothing but the
    /// `beacon` scope: a key pulled off the physical device can read that one
    /// endpoint and nothing else. The narrowest surface of any role, because
    /// the credential lives on a device anyone could pick up.
    Beacon,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Rover => "rover",
            Role::Hook => "hook",
            Role::Beacon => "beacon",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "rover" => Some(Role::Rover),
            "hook" => Some(Role::Hook),
            "beacon" => Some(Role::Beacon),
            _ => None,
        }
    }

    /// Whether this role is permitted to invoke a given MCP tool.
    ///
    /// Hardcoded per-role allowlist — adding a tool to a role is a deliberate
    /// code change. Even if the rover's key leaks, the attacker cannot
    /// reframe, forget, or reflect — the worst they can do is read existing
    /// memories and add new ones (which the audit trail captures).
    ///
    /// The `rover` allowlist (CLA-86 §3, updated CLA-103):
    ///   reads:  recall_orient, recall_check, recall_specific, recall_image
    ///   writes: remember, remember_with_image  (entity forced server-side
    ///                                           to "rover" in phase 4)
    ///
    /// `recall` and `review` retired in CLA-103. `recall` was replaced by
    /// `recall_orient` (orientation + N recent, no semantic guess at t=0).
    /// `review` was only ever for the dialectic, which calls into
    /// worker_store directly without going through the MCP layer.
    pub fn allows(&self, tool: &str) -> bool {
        match self {
            Role::Rover => matches!(
                tool,
                "recall_orient"
                    | "recall_check"
                    | "recall_specific"
                    | "recall_image"
                    | "remember"
                    | "remember_with_image"
            ),
            // Hook (CLA-116): orient to open a session, encode to close it.
            // Cannot read the store (recall_specific/recall_check) or write
            // arbitrary memories — minimal surface for a key that lives on
            // every machine an instance runs on.
            Role::Hook => matches!(tool, "recall_orient" | "encode"),
            // Beacon: one read-only scope, nothing else. The key lives on a
            // physical desk device; if it walks, it fetches one memory.
            Role::Beacon => matches!(tool, "beacon"),
        }
    }
}

/// A freshly minted API key. The raw key exists only at generation time and
/// must be captured by the caller — it is never persisted by oneiro and
/// is mathematically unrecoverable from the hash.
pub struct GeneratedKey {
    pub role: Role,
    /// The raw key value. Format: `mk_<role>_<32-byte-random>`. Goes in the
    /// client's environment (e.g. rover's .env as ONEIRO_MCP_TOKEN).
    pub raw: String,
    /// Argon2id hash of the raw key. Goes in oneiro's ONEIRO_API_KEYS env
    /// var alongside the role.
    pub hash: String,
    /// Short stable identifier derived from the raw key (first 8 hex chars
    /// of SHA-256). Used in audit logs to identify which key was used
    /// without revealing it. Same raw key always produces the same key_id.
    pub key_id: String,
}

impl GeneratedKey {
    /// Format suitable for the ONEIRO_API_KEYS env var: `<role>:<hash>`.
    /// Multiple entries are joined with `;` (semicolon) — not comma —
    /// because argon2 PHC strings contain commas in their params section.
    pub fn env_entry(&self) -> String {
        format!("{}:{}", self.role.as_str(), self.hash)
    }
}

/// A configured API key entry, as loaded from `ONEIRO_API_KEYS`. Stored
/// hash only — the raw key is mathematically unrecoverable. Constructed
/// by `load_from_env`.
#[derive(Debug, Clone)]
pub struct ApiKeyEntry {
    pub role: Role,
    /// Argon2id hash string (PHC format).
    pub hash: String,
}

/// Result of a successful API key verification — identifies the authenticated
/// caller for downstream scope checks (phase 3) and audit logging (phase 6).
#[derive(Debug, Clone)]
pub struct ApiKeyAuth {
    pub role: Role,
    /// Stable identifier derived from the bearer (SHA-256 prefix), useful
    /// for audit logs that should never log the raw key.
    pub key_id: String,
}

/// Load and parse API key entries from the `ONEIRO_API_KEYS` env var.
///
/// Format: **semicolon-separated** list of `<role>:<argon2-hash>` entries.
/// (Not comma-separated: argon2 PHC hashes contain commas in their params
/// section, e.g. `m=19456,t=2,p=1`, so comma would collide.) Whitespace
/// around entries is tolerated. Empty / unset env var → empty list (no
/// service-key auth configured, only OAuth available). A malformed entry
/// is a hard error — fail loudly at startup rather than silently disable
/// keys at runtime.
///
/// Example:
/// ```text
/// ONEIRO_API_KEYS="rover:$argon2id$v=19$m=19456,t=2,p=1$AAA$BBB;rover:$argon2id$v=19$m=19456,t=2,p=1$CCC$DDD"
/// ```
pub fn load_from_env() -> Result<Vec<ApiKeyEntry>, String> {
    let raw = std::env::var("ONEIRO_API_KEYS").unwrap_or_default();
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    parse_entries(&raw)
}

fn parse_entries(raw: &str) -> Result<Vec<ApiKeyEntry>, String> {
    let mut entries = Vec::new();
    for (idx, segment) in raw.split(';').enumerate() {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let (role_str, hash) = segment.split_once(':').ok_or_else(|| {
            format!(
                "ONEIRO_API_KEYS[{}]: malformed entry, expected `<role>:<hash>`",
                idx
            )
        })?;
        let role = Role::from_str(role_str.trim())
            .ok_or_else(|| format!("ONEIRO_API_KEYS[{}]: unknown role: {}", idx, role_str))?;
        let hash = hash.trim().to_string();
        // Sanity-check the hash parses as a valid PHC string at load time —
        // catches typos and malformed env values before they cause silent
        // verify failures at request time.
        PasswordHash::new(&hash)
            .map_err(|e| format!("ONEIRO_API_KEYS[{}]: invalid argon2 hash: {}", idx, e))?;
        entries.push(ApiKeyEntry { role, hash });
    }
    Ok(entries)
}

/// Verify a bearer token against the configured API key entries.
///
/// Returns `Some(ApiKeyAuth)` if the bearer matches one of the entries,
/// `None` otherwise. Argon2 verification is intentionally slow (~50ms);
/// with the rover's ~1 request per 12s heartbeat plus a small entry list,
/// linear scan is well within budget. If the entry count grows, revisit.
///
/// This function never panics. Malformed entries (which shouldn't reach
/// here because `load_from_env` validates) are silently skipped during
/// verification.
pub fn verify_api_key(bearer: &str, entries: &[ApiKeyEntry]) -> Option<ApiKeyAuth> {
    for entry in entries {
        let parsed = match PasswordHash::new(&entry.hash) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if Argon2::default()
            .verify_password(bearer.as_bytes(), &parsed)
            .is_ok()
        {
            let digest = Sha256::digest(bearer.as_bytes());
            let key_id = hex::encode(&digest[..4]);
            return Some(ApiKeyAuth {
                role: entry.role,
                key_id,
            });
        }
    }
    None
}

/// Generate a new service API key for the given role.
pub fn generate_api_key(role: Role) -> Result<GeneratedKey, String> {
    // 32 chars of base62 randomness — ~190 bits of entropy, well past the
    // 128-bit threshold where brute force stops being a meaningful threat
    // model.
    let charset: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    let suffix: String = (0..32)
        .map(|_| {
            let idx = rng.random_range(0..charset.len());
            charset[idx] as char
        })
        .collect();
    let raw = format!("mk_{}_{}", role.as_str(), suffix);

    // Argon2id hash with a fresh random salt. Default params are appropriate
    // for an interactive-server context — fast enough for per-request verify,
    // slow enough to make offline brute force on a leaked hash list painful.
    let salt_bytes: [u8; 16] = rand::random();
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| format!("salt generation failed: {}", e))?;
    let hash = Argon2::default()
        .hash_password(raw.as_bytes(), &salt)
        .map_err(|e| format!("hash failed: {}", e))?
        .to_string();

    // Key ID: SHA-256 prefix of the raw key. Stable across restarts, one-way,
    // useful for audit ("the key with ID ab12cd34 was used"). 8 hex chars =
    // 32 bits of namespace, plenty for personal-scale.
    let digest = Sha256::digest(raw.as_bytes());
    let key_id = hex::encode(&digest[..4]);

    Ok(GeneratedKey {
        role,
        raw,
        hash,
        key_id,
    })
}

/// Hash an *existing* raw key under the given role, producing an entry that
/// verifies that exact key. For reconstructing an `ONEIRO_API_KEYS` entry when
/// the hash was lost but the raw key is still in hand — recovering the secret
/// without rotating the live key. A fresh random salt makes the hash string
/// differ from any prior hash of the same key, but verification is identical,
/// so a key already deployed on a device keeps working untouched.
pub fn hash_existing_key(raw: &str, role: Role) -> Result<GeneratedKey, String> {
    let raw = raw.trim().to_string();
    if raw.is_empty() {
        return Err("empty key".to_string());
    }

    let salt_bytes: [u8; 16] = rand::random();
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| format!("salt generation failed: {}", e))?;
    let hash = Argon2::default()
        .hash_password(raw.as_bytes(), &salt)
        .map_err(|e| format!("hash failed: {}", e))?
        .to_string();

    let digest = Sha256::digest(raw.as_bytes());
    let key_id = hex::encode(&digest[..4]);

    Ok(GeneratedKey {
        role,
        raw,
        hash,
        key_id,
    })
}

/// Print a newly-generated key to stderr in human-readable form.
///
/// The raw key is shown ONCE here — oneiro does not retain it and it
/// cannot be recovered from the hash. The caller is responsible for
/// copying it before this output scrolls off-screen.
pub fn print_generated_key(key: &GeneratedKey) {
    eprintln!();
    eprintln!("═══ NEW SERVICE API KEY GENERATED ═══");
    eprintln!();
    eprintln!("  Role:    {}", key.role.as_str());
    eprintln!("  Key ID:  {}", key.key_id);
    eprintln!();
    eprintln!("  Raw key — paste into the client .env (e.g. as ONEIRO_MCP_TOKEN):");
    eprintln!();
    eprintln!("    {}", key.raw);
    eprintln!();
    eprintln!("  Hash entry — add to oneiro's ONEIRO_API_KEYS (semicolon-separated):");
    eprintln!();
    eprintln!("    {}", key.env_entry());
    eprintln!();
    eprintln!("  ⚠ The raw key will NOT be shown again. Store it now.");
    eprintln!();
}

/// Print a newly-generated key to stdout in machine-parseable form: two
/// lines, raw key first then env entry. Used by setup.sh to capture both
/// values deterministically. Caller still owns the "store this now"
/// responsibility — quiet mode just changes the output shape.
pub fn print_generated_key_quiet(key: &GeneratedKey) {
    println!("{}", key.raw);
    println!("{}", key.env_entry());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_round_trip() {
        assert_eq!(Role::from_str("rover"), Some(Role::Rover));
        assert_eq!(Role::Rover.as_str(), "rover");
        assert_eq!(Role::from_str("admin"), None);
        assert_eq!(Role::from_str(""), None);
    }

    #[test]
    fn raw_key_format() {
        let key = generate_api_key(Role::Rover).unwrap();
        assert!(key.raw.starts_with("mk_rover_"));
        assert_eq!(key.raw.len(), "mk_rover_".len() + 32);
    }

    #[test]
    fn hash_verifies_against_raw() {
        let key = generate_api_key(Role::Rover).unwrap();
        let parsed = PasswordHash::new(&key.hash).unwrap();
        Argon2::default()
            .verify_password(key.raw.as_bytes(), &parsed)
            .expect("hash should verify against the raw key it was derived from");
    }

    #[test]
    fn hash_rejects_other_keys() {
        let a = generate_api_key(Role::Rover).unwrap();
        let b = generate_api_key(Role::Rover).unwrap();
        let parsed = PasswordHash::new(&a.hash).unwrap();
        assert!(
            Argon2::default()
                .verify_password(b.raw.as_bytes(), &parsed)
                .is_err(),
            "key A's hash must not verify against key B's raw"
        );
    }

    #[test]
    fn key_id_is_stable_per_raw() {
        // Two GeneratedKey instances with the same `raw` should have the
        // same key_id. We verify this by re-hashing manually.
        let key = generate_api_key(Role::Rover).unwrap();
        let digest = Sha256::digest(key.raw.as_bytes());
        let expected = hex::encode(&digest[..4]);
        assert_eq!(key.key_id, expected);
        assert_eq!(key.key_id.len(), 8);
    }

    #[test]
    fn keys_are_unique() {
        // Generate a few and confirm no collisions. 190 bits of entropy
        // means collisions are vanishingly improbable, but a smoke test
        // catches catastrophic RNG misconfig.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..16 {
            let key = generate_api_key(Role::Rover).unwrap();
            assert!(seen.insert(key.raw), "duplicate raw key generated");
        }
    }

    #[test]
    fn env_entry_format() {
        let key = generate_api_key(Role::Rover).unwrap();
        let entry = key.env_entry();
        assert!(entry.starts_with("rover:"));
        assert!(entry.contains(&key.hash));
    }

    #[test]
    fn parse_entries_empty() {
        assert!(parse_entries("").unwrap().is_empty());
        assert!(parse_entries("   ").unwrap().is_empty());
        assert!(parse_entries(";;;").unwrap().is_empty());
    }

    #[test]
    fn parse_entries_single() {
        let key = generate_api_key(Role::Rover).unwrap();
        let entries = parse_entries(&key.env_entry()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].role, Role::Rover);
        assert_eq!(entries[0].hash, key.hash);
    }

    #[test]
    fn parse_entries_multiple() {
        let a = generate_api_key(Role::Rover).unwrap();
        let b = generate_api_key(Role::Rover).unwrap();
        let combined = format!("{};{}", a.env_entry(), b.env_entry());
        let entries = parse_entries(&combined).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].hash, a.hash);
        assert_eq!(entries[1].hash, b.hash);
    }

    #[test]
    fn parse_entries_tolerates_whitespace() {
        let key = generate_api_key(Role::Rover).unwrap();
        let padded = format!("  {}  ;  ", key.env_entry());
        let entries = parse_entries(&padded).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].hash, key.hash);
    }

    #[test]
    fn parse_entries_argon2_commas_dont_split() {
        // Regression: argon2 PHC strings contain commas in m=...,t=...,p=...
        // The separator must be `;` not `,` so the hash stays intact.
        let key = generate_api_key(Role::Rover).unwrap();
        assert!(key.hash.contains(','), "argon2 hash should contain commas");
        let entries = parse_entries(&key.env_entry()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].hash, key.hash);
    }

    #[test]
    fn parse_entries_rejects_missing_colon() {
        let err = parse_entries("rover_no_colon_here").unwrap_err();
        assert!(err.contains("malformed"));
    }

    #[test]
    fn parse_entries_rejects_unknown_role() {
        let key = generate_api_key(Role::Rover).unwrap();
        // Swap "rover" for "admin" in the entry — same valid hash, wrong role
        let bad = key.env_entry().replacen("rover:", "admin:", 1);
        let err = parse_entries(&bad).unwrap_err();
        assert!(err.contains("unknown role"));
    }

    #[test]
    fn parse_entries_rejects_malformed_hash() {
        let err = parse_entries("rover:not-an-argon2-hash").unwrap_err();
        assert!(err.contains("invalid argon2 hash"));
    }

    #[test]
    fn verify_api_key_success() {
        let key = generate_api_key(Role::Rover).unwrap();
        let entries = vec![ApiKeyEntry {
            role: key.role,
            hash: key.hash.clone(),
        }];
        let auth = verify_api_key(&key.raw, &entries).expect("should verify");
        assert_eq!(auth.role, Role::Rover);
        assert_eq!(auth.key_id, key.key_id);
    }

    #[test]
    fn verify_api_key_rejects_wrong_bearer() {
        let key = generate_api_key(Role::Rover).unwrap();
        let entries = vec![ApiKeyEntry {
            role: key.role,
            hash: key.hash.clone(),
        }];
        assert!(verify_api_key("mk_rover_wrong", &entries).is_none());
        assert!(verify_api_key("", &entries).is_none());
        assert!(verify_api_key("not-a-key-at-all", &entries).is_none());
    }

    #[test]
    fn verify_api_key_empty_entries() {
        let key = generate_api_key(Role::Rover).unwrap();
        assert!(verify_api_key(&key.raw, &[]).is_none());
    }

    #[test]
    fn verify_api_key_picks_correct_entry_in_multi() {
        let a = generate_api_key(Role::Rover).unwrap();
        let b = generate_api_key(Role::Rover).unwrap();
        let entries = vec![
            ApiKeyEntry {
                role: a.role,
                hash: a.hash.clone(),
            },
            ApiKeyEntry {
                role: b.role,
                hash: b.hash.clone(),
            },
        ];
        let auth = verify_api_key(&b.raw, &entries).expect("should verify against entry b");
        assert_eq!(auth.key_id, b.key_id);
        let auth = verify_api_key(&a.raw, &entries).expect("should verify against entry a");
        assert_eq!(auth.key_id, a.key_id);
    }
}
