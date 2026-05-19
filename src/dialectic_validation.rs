// dialectic_validation.rs — Stage 3 dispatch gate (CLA-100).
//
// Stage 2 writes whatever the Synthesizer produces to the audit row.
// The Synthesizer's tool schema permits any combination of {new_content,
// new_summary, note} keys, so Haiku can legally emit shape/action
// mismatches that are valid telemetry but unsafe to dispatch:
//
//   { "action": "reframe", "action_payload": {} }
//   { "action": "flag", "action_payload": { "new_content": "wrong" } }
//
// Stage 3 must catch these before touching the memory store. This
// module is the gate.
//
// Lives outside the worker_dialectic_dispatch.rs wasm cfg-gate so cargo
// test can cover the failure modes natively. Stage 3's actual dispatch
// machinery (Vectorize upserts, D1 mutations) needs wasm, but the
// shape-check that protects them does not.

use serde_json::Value;

/// Validate that an `action_payload` matches its `action`.
///
/// Returns `Ok(())` if the payload is the right shape for the action,
/// or `Err(reason)` if the row should be marked `validation_failed` and
/// skipped. **No partial dispatch under any circumstances** — a malformed
/// payload means we do not mutate the memory store, full stop.
pub fn validate_synthesis_payload(action: &str, payload: &Value) -> Result<(), String> {
    match action {
        "keep" => Ok(()),
        "reframe" => {
            let nc = payload
                .get("new_content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ns = payload
                .get("new_summary")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if nc.trim().is_empty() {
                return Err("reframe payload missing non-empty new_content".to_string());
            }
            if ns.trim().is_empty() {
                return Err("reframe payload missing non-empty new_summary".to_string());
            }
            Ok(())
        }
        "flag" => {
            let note = payload
                .get("note")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if note.trim().is_empty() {
                return Err("flag payload missing non-empty note".to_string());
            }
            Ok(())
        }
        other => Err(format!("unknown action: {}", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keep_accepts_empty_payload() {
        assert!(validate_synthesis_payload("keep", &json!({})).is_ok());
    }

    #[test]
    fn keep_accepts_extra_keys() {
        // keep ignores payload — extra junk is fine, not our concern.
        assert!(
            validate_synthesis_payload("keep", &json!({"new_content": "x"})).is_ok()
        );
    }

    #[test]
    fn reframe_accepts_valid_payload() {
        let payload = json!({"new_content": "rewritten", "new_summary": "shorter"});
        assert!(validate_synthesis_payload("reframe", &payload).is_ok());
    }

    #[test]
    fn reframe_rejects_missing_new_content() {
        let payload = json!({"new_summary": "shorter"});
        let err = validate_synthesis_payload("reframe", &payload).unwrap_err();
        assert!(err.contains("new_content"));
    }

    #[test]
    fn reframe_rejects_missing_new_summary() {
        let payload = json!({"new_content": "rewritten"});
        let err = validate_synthesis_payload("reframe", &payload).unwrap_err();
        assert!(err.contains("new_summary"));
    }

    #[test]
    fn reframe_rejects_empty_strings() {
        let payload = json!({"new_content": "  ", "new_summary": "  "});
        assert!(validate_synthesis_payload("reframe", &payload).is_err());
    }

    #[test]
    fn flag_accepts_valid_payload() {
        let payload = json!({"note": "look at this"});
        assert!(validate_synthesis_payload("flag", &payload).is_ok());
    }

    #[test]
    fn flag_rejects_missing_note() {
        let payload = json!({});
        let err = validate_synthesis_payload("flag", &payload).unwrap_err();
        assert!(err.contains("note"));
    }

    #[test]
    fn flag_rejects_empty_note() {
        let payload = json!({"note": "   "});
        let err = validate_synthesis_payload("flag", &payload).unwrap_err();
        assert!(err.contains("note"));
    }

    #[test]
    fn unknown_action_rejected() {
        let err = validate_synthesis_payload("forget", &json!({})).unwrap_err();
        assert!(err.contains("unknown action"));
        assert!(err.contains("forget"));
    }

    // worker_dialectic.rs now tolerant-deserialises a missing action_payload
    // to Value::Null. These tests lock in the gate behaviour for that case.

    #[test]
    fn keep_accepts_null_payload() {
        assert!(validate_synthesis_payload("keep", &Value::Null).is_ok());
    }

    #[test]
    fn reframe_rejects_null_payload() {
        let err = validate_synthesis_payload("reframe", &Value::Null).unwrap_err();
        assert!(err.contains("new_content"));
    }

    #[test]
    fn flag_rejects_null_payload() {
        let err = validate_synthesis_payload("flag", &Value::Null).unwrap_err();
        assert!(err.contains("note"));
    }
}
