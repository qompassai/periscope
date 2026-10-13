//! The joint-attention coordination board, vendored.
//!
//! Periscope is a standalone crate, so the protocol core from
//! `phlow-attention` is vendored here in miniature: the same `Focus`
//! wire shape, the same validation bounds, the same alignment metric
//! and coordination rule, and the same fail-closed behaviour. The two
//! implementations are kept honest by shared JSON test vectors (see
//! the tests at the bottom of this file): a focus or decision document
//! produced by either side parses and decides identically on the other.
//!
//! Semantics, in one paragraph: an agent advertises its focus — an
//! ordered list of salient workspace targets plus the hash of the
//! saliency snapshot it derived them from. Two foci are comparable
//! only inside one snapshot frame; across frames, alignment is refused
//! and the coordination rule rejects. When both agents' top target
//! coincides, the lexicographically smaller agent id holds it and the
//! other defers; otherwise the pair proceeds in parallel.
//!
//! The board lives in the server process (a static, like the broker
//! rendezvous it is): periscope is the one component that can see the
//! human's editor and every connected agent, which makes it the
//! natural meeting point. These tools never touch the editor or the
//! filesystem; write-class concerns stay with the brokered tools.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use crate::error::{Error, Result};

/// Most agents tracked on one board.
pub const BOARD_AGENTS_MAX: usize = 64;
/// Most targets in one focus.
pub const FOCUS_TARGETS_MAX: usize = 8;
/// Longest agent id, in bytes.
pub const AGENT_ID_MAX_BYTES: usize = 128;
/// Longest target id, in bytes.
pub const TARGET_ID_MAX_BYTES: usize = 512;
/// Longest snapshot hash, in bytes (a SHA-256 hex digest is 64).
pub const SNAPSHOT_HASH_MAX_BYTES: usize = 128;

/// One agent's advertised focus within one snapshot frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Focus {
    /// Who is attending; the board checks the transport identity
    /// against this claim.
    pub agent_id: String,
    /// Hash of the snapshot this focus was computed from.
    pub snapshot_hash: String,
    /// Per-agent monotone sequence; a publish that does not advance it
    /// is stale or replayed and is refused.
    pub seq: u64,
    /// Attended targets, most salient first.
    pub targets: Vec<String>,
}

impl Focus {
    /// Structural validation, mirroring `phlow-attention`'s
    /// `Focus::validate`: identifier rules re-run on every field,
    /// because deserialised values are only as trustworthy as the
    /// checks applied after parsing.
    pub fn validate(&self) -> Result<()> {
        validate_agent_id(&self.agent_id)?;
        if self.snapshot_hash.is_empty()
            || self.snapshot_hash.len() > SNAPSHOT_HASH_MAX_BYTES
            || self.snapshot_hash.chars().any(char::is_control)
        {
            return Err(Error::InvalidInput(
                "focus snapshot_hash breaks the identifier rules".to_string(),
            ));
        }
        if self.targets.len() > FOCUS_TARGETS_MAX {
            return Err(Error::InvalidInput(format!(
                "focus carries {} targets, over the maximum of {FOCUS_TARGETS_MAX}",
                self.targets.len()
            )));
        }
        let mut sorted = self.targets.clone();
        for target in &self.targets {
            validate_target_id(target)?;
        }
        sorted.sort();
        sorted.dedup();
        if sorted.len() != self.targets.len() {
            return Err(Error::InvalidInput(
                "focus carries duplicate targets".to_string(),
            ));
        }
        Ok(())
    }

    /// The primary target of attention, if any.
    pub fn top(&self) -> Option<&str> {
        self.targets.first().map(String::as_str)
    }
}

fn validate_agent_id(raw: &str) -> Result<()> {
    if raw.is_empty() || raw.len() > AGENT_ID_MAX_BYTES || raw.chars().any(char::is_control) {
        return Err(Error::InvalidInput(format!(
            "agent id {raw:?} breaks the identifier rules"
        )));
    }
    Ok(())
}

fn validate_target_id(raw: &str) -> Result<()> {
    if raw.is_empty() || raw.len() > TARGET_ID_MAX_BYTES || raw.chars().any(char::is_control) {
        return Err(Error::InvalidInput(format!(
            "target id {raw:?} breaks the identifier rules"
        )));
    }
    Ok(())
}

/// How much two foci overlap, measured inside one snapshot frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Alignment {
    /// Jaccard overlap of the two target sets, `0.0..=1.0`.
    pub score: f64,
    /// Both agents rank the same target first.
    pub top_shared: bool,
    /// Targets both agents attend to, at any rank.
    pub shared_targets: usize,
}

/// Measure the alignment between two foci. Refuses across snapshot
/// frames: the comparison would be meaningless and must fail closed.
pub fn alignment(a: &Focus, b: &Focus) -> Result<Alignment> {
    a.validate()?;
    b.validate()?;
    if a.snapshot_hash != b.snapshot_hash {
        return Err(Error::InvalidInput(
            "snapshot mismatch: foci from different frames are not comparable".to_string(),
        ));
    }
    let shared = a
        .targets
        .iter()
        .filter(|target| b.targets.contains(target))
        .count();
    let union = a.targets.len() + b.targets.len() - shared;
    let score = if union == 0 {
        1.0
    } else {
        shared as f64 / union as f64
    };
    Ok(Alignment {
        score,
        top_shared: a.top().is_some() && a.top() == b.top(),
        shared_targets: shared,
    })
}

/// The outcome of the coordination rule for a pair of foci. The JSON
/// shape is the protocol's: identical to `phlow-attention`'s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    /// No contention: both agents proceed with their own focus.
    Parallel,
    /// Both agents' primary target is the same object; `holder`
    /// proceeds on it and the other agent defers.
    Serialize {
        /// The agent that proceeds on the contended target.
        holder: String,
        /// The contended target.
        target: String,
    },
    /// Coordination refused (stale frame or invalid focus). The caller
    /// must re-snapshot and retry, never proceed on a guess.
    Reject {
        /// Machine-stable reason: `snapshot_mismatch` or
        /// `invalid_focus`.
        reason: String,
    },
}

/// Apply the coordination rule. Never panics; untrustworthy input
/// becomes a `Reject` decision rather than an error, so callers can
/// record and surface it uniformly.
pub fn decide(a: &Focus, b: &Focus) -> Decision {
    if a.validate().is_err() || b.validate().is_err() {
        return Decision::Reject {
            reason: "invalid_focus".to_string(),
        };
    }
    if a.snapshot_hash != b.snapshot_hash {
        return Decision::Reject {
            reason: "snapshot_mismatch".to_string(),
        };
    }
    match (a.top(), b.top()) {
        (Some(top_a), Some(top_b)) if top_a == top_b => Decision::Serialize {
            holder: a.agent_id.clone().min(b.agent_id.clone()),
            target: top_a.to_string(),
        },
        _ => Decision::Parallel,
    }
}

/// The in-process coordination board.
#[derive(Debug, Default)]
pub struct FocusBoard {
    foci: HashMap<String, Focus>,
}

impl FocusBoard {
    /// Publish `focus` on behalf of `caller_id`. The payload's claimed
    /// agent id must equal the transport identity (anti-spoofing), the
    /// focus must validate (anti-flood/anti-garbage), and its sequence
    /// must advance past the stored one (anti-replay).
    pub fn publish(&mut self, caller_id: &str, focus: Focus) -> Result<()> {
        if focus.agent_id != caller_id {
            return Err(Error::InvalidInput(format!(
                "focus agent_id {:?} does not match the caller identity {caller_id:?}",
                focus.agent_id
            )));
        }
        focus.validate()?;
        if !self.foci.contains_key(caller_id) && self.foci.len() >= BOARD_AGENTS_MAX {
            return Err(Error::InvalidInput(format!(
                "attention board is full ({BOARD_AGENTS_MAX} agents)"
            )));
        }
        if let Some(previous) = self.foci.get(caller_id)
            && focus.seq <= previous.seq
        {
            return Err(Error::InvalidInput(format!(
                "stale focus sequence {} (current {})",
                focus.seq, previous.seq
            )));
        }
        self.foci.insert(caller_id.to_string(), focus);
        Ok(())
    }

    /// The current focus of one agent, if it has published.
    pub fn get(&self, agent_id: &str) -> Option<&Focus> {
        self.foci.get(agent_id)
    }

    /// Alignment between two board members, when both have published.
    pub fn alignment_between(&self, a: &str, b: &str) -> Option<Result<Alignment>> {
        Some(alignment(self.foci.get(a)?, self.foci.get(b)?))
    }

    /// Coordination decision between two board members, when both
    /// have published.
    pub fn decide_between(&self, a: &str, b: &str) -> Option<Decision> {
        Some(decide(self.foci.get(a)?, self.foci.get(b)?))
    }
}

/// The server-process board: the one rendezvous every connected agent
/// (and the human's editor state, when it publishes) shares.
pub fn board() -> &'static Mutex<FocusBoard> {
    static BOARD: OnceLock<Mutex<FocusBoard>> = OnceLock::new();
    BOARD.get_or_init(|| Mutex::new(FocusBoard::default()))
}

/// Dispatch one `attention_*` tool against the process board.
///
/// `caller` is the transport identity established at `initialize`;
/// identity-bound tools fail closed when it is absent.
pub fn call(caller: Option<&str>, name: &str, args: &JsonValue) -> Result<JsonValue> {
    let caller = caller.ok_or_else(|| {
        Error::InvalidInput("attention tools require an identified client".to_string())
    })?;
    let mut board = board().lock().expect("attention board lock");
    match name {
        "attention_publish" => {
            let raw = args.get("focus").ok_or_else(|| {
                Error::InvalidInput("attention_publish requires a `focus` argument".to_string())
            })?;
            let focus: Focus = serde_json::from_value(raw.clone())
                .map_err(|error| Error::InvalidInput(format!("invalid focus document: {error}")))?;
            let seq = focus.seq;
            board.publish(caller, focus)?;
            Ok(json!({"status": "ok", "agent_id": caller, "seq": seq}))
        }
        "attention_focus" => {
            let agent_id = required_agent_id(args)?;
            let focus = board
                .get(agent_id)
                .map(|focus| serde_json::to_value(focus).expect("focus serialises"));
            Ok(json!({"status": "ok", "focus": focus}))
        }
        "attention_alignment" => {
            let partner = required_agent_id(args)?;
            let score = board.alignment_between(caller, partner).ok_or_else(|| {
                Error::InvalidInput("both agents must publish a focus before alignment".to_string())
            })??;
            Ok(json!({
                "status": "ok",
                "alignment": serde_json::to_value(score).expect("alignment serialises"),
            }))
        }
        "attention_decide" => {
            let partner = required_agent_id(args)?;
            let decision = board.decide_between(caller, partner).ok_or_else(|| {
                Error::InvalidInput(
                    "both agents must publish a focus before a decision".to_string(),
                )
            })?;
            Ok(json!({
                "status": "ok",
                "decision": serde_json::to_value(decision).expect("decision serialises"),
            }))
        }
        _ => Err(Error::InvalidInput(format!(
            "unknown attention tool: {name}"
        ))),
    }
}

fn required_agent_id(args: &JsonValue) -> Result<&str> {
    args.get("agent_id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| {
            Error::InvalidInput("tool requires a string `agent_id` argument".to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn focus(agent: &str, hash: &str, seq: u64, targets: &[&str]) -> Focus {
        Focus {
            agent_id: agent.to_string(),
            snapshot_hash: hash.to_string(),
            seq,
            targets: targets.iter().map(|t| t.to_string()).collect(),
        }
    }

    /// Shared wire vector: this exact focus document is produced by
    /// `phlow-attention`'s serializer (crate test
    /// `focus_json_shape_is_stable`); both implementations must parse
    /// it to the same value.
    const SHARED_FOCUS_JSON: &str = r#"{"agent_id":"alice","snapshot_hash":"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08","seq":3,"targets":["src/a.rs","src/b.rs"]}"#;

    #[test]
    fn shared_vector_parses_and_validates() {
        let parsed: Focus = serde_json::from_str(SHARED_FOCUS_JSON).expect("vector parses");
        assert_eq!(parsed.agent_id, "alice");
        assert_eq!(parsed.seq, 3);
        assert_eq!(parsed.targets, ["src/a.rs", "src/b.rs"]);
        parsed.validate().expect("vector validates");
        let echoed = serde_json::to_value(&parsed).expect("serialises");
        let expected: JsonValue = serde_json::from_str(SHARED_FOCUS_JSON).unwrap();
        assert_eq!(echoed, expected, "wire shape round-trips byte-equal");
    }

    #[test]
    fn decision_wire_shape_matches_the_protocol() {
        let a = focus("alice", "h", 1, &["src/a.rs"]);
        let b = focus("bob", "h", 1, &["src/a.rs", "src/b.rs"]);
        let value = serde_json::to_value(decide(&a, &b)).unwrap();
        assert_eq!(value["decision"], "serialize");
        assert_eq!(value["holder"], "alice");
        assert_eq!(value["target"], "src/a.rs");
        let stale = focus("bob", "other-frame", 1, &["src/a.rs"]);
        let value = serde_json::to_value(decide(&a, &stale)).unwrap();
        assert_eq!(value["decision"], "reject");
        assert_eq!(value["reason"], "snapshot_mismatch");
    }

    #[test]
    fn alignment_refuses_across_frames() {
        let a = focus("alice", "h1", 1, &["x"]);
        let b = focus("bob", "h2", 1, &["x"]);
        assert!(alignment(&a, &b).is_err());
        let same = focus("bob", "h1", 1, &["x", "y"]);
        let score = alignment(&a, &same).expect("same frame aligns");
        assert_eq!(score.shared_targets, 1);
        assert!((score.score - 0.5).abs() < f64::EPSILON);
        assert!(score.top_shared);
    }

    #[test]
    fn board_rejects_spoofing_replay_and_floods() {
        let mut board = FocusBoard::default();
        let alice = focus("alice", "h", 1, &["x"]);
        // Spoofing: payload names someone else.
        assert!(board.publish("mallory", alice.clone()).is_err());
        board.publish("alice", alice).expect("honest publish");
        // Replay: same or lower sequence.
        assert!(
            board
                .publish("alice", focus("alice", "h", 1, &["y"]))
                .is_err()
        );
        assert!(
            board
                .publish("alice", focus("alice", "h", 0, &["y"]))
                .is_err()
        );
        // Flood: over the target bound.
        let wide: Vec<String> = (0..=FOCUS_TARGETS_MAX).map(|i| format!("t{i}")).collect();
        let flood = Focus {
            agent_id: "alice".to_string(),
            snapshot_hash: "h".to_string(),
            seq: 2,
            targets: wide,
        };
        assert!(board.publish("alice", flood).is_err());
        // Board capacity: 64 distinct agents, the 65th is refused but
        // tracked agents keep updating.
        let mut full = FocusBoard::default();
        for i in 0..BOARD_AGENTS_MAX {
            let id = format!("agent-{i}");
            full.publish(&id, focus(&id, "h", 1, &["x"]))
                .expect("within capacity");
        }
        assert!(
            full.publish("agent-new", focus("agent-new", "h", 1, &["x"]))
                .is_err()
        );
        full.publish("agent-0", focus("agent-0", "h", 2, &["z"]))
            .expect("tracked agents still update a full board");
    }
}
