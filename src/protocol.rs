//! Wire-level translation: claude stream-json (and headless opencode NDJSON) lines ->
//! compact bridge events, plus the (project, lane) keying scheme shared by the daemon
//! and the clients.

use serde_json::{json, Value};

/// Lane a client op targets: the `--lane` value if given, else `main`.
pub fn lane_of(over: Option<String>) -> String {
    over.filter(|s| !s.is_empty()).unwrap_or_else(|| "main".into())
}

/// Registry / sessions key for a (project, lane) pair. Lane `main` keeps the bare project key
/// for backward compatibility; other lanes get their own namespaced key (and thus their own
/// claude session, running concurrently and persisting independently).
pub fn lane_key(project: &str, lane: &str) -> String {
    if lane == "main" {
        project.to_string()
    } else {
        format!("{project}\u{1f}{lane}")
    }
}

/// Translate a raw claude stream-json line into the bridge's compact event(s).
/// Non-JSON or unrecognized lines yield no events — never a panic.
pub fn parse_event(line: &str) -> Vec<String> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    match v.get("type").and_then(|x| x.as_str()).unwrap_or("") {
        "system" if v.get("subtype").and_then(|x| x.as_str()) == Some("init") => {
            vec![json!({
                "ev": "init",
                "session_id": v.get("session_id").and_then(|x| x.as_str()).unwrap_or(""),
                "model": v.get("model").and_then(|x| x.as_str()).unwrap_or(""),
            })
            .to_string()]
        }
        "assistant" => {
            let text = extract_text(v.get("message"));
            if text.is_empty() {
                vec![]
            } else {
                vec![json!({"ev":"final","text":text}).to_string()]
            }
        }
        "result" => vec![json!({
            "ev": "result",
            "text": v.get("result").and_then(|x| x.as_str()).unwrap_or(""),
            "is_error": v.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false),
        })
        .to_string()],
        "content_block_delta" | "stream_event" => match find_delta_text(&v) {
            Some(t) => vec![json!({"ev":"delta","text":t}).to_string()],
            None => vec![],
        },
        // Assumes interrupt is the only control_request the bridge ever sends; if another
        // kind is added, correlate by request_id before mapping to `interrupted`.
        "control_response" => {
            let ok = v.get("response").and_then(|r| r.get("subtype")).and_then(|x| x.as_str())
                == Some("success");
            vec![json!({"ev":"interrupted","ok":ok}).to_string()]
        }
        _ => vec![],
    }
}

/// Stdin line asking claude to cancel the in-flight turn without ending the session.
/// Claude acks with a `control_response` and the aborted turn still emits its `result`.
pub fn control_interrupt_line(request_id: &str) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "interrupt" }
    })
    .to_string()
}

/// Concatenate every `text` content block of an assistant message.
pub fn extract_text(msg: Option<&Value>) -> String {
    let mut out = String::new();
    if let Some(arr) = msg.and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
        for b in arr {
            if b.get("type").and_then(|x| x.as_str()) == Some("text") {
                if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                    out.push_str(t);
                }
            }
        }
    }
    out
}

/// Find streaming delta text in either the flat (`delta.text`) or the wrapped
/// (`event.delta.text`) shape claude emits.
pub fn find_delta_text(v: &Value) -> Option<String> {
    if let Some(t) = v.get("delta").and_then(|d| d.get("text")).and_then(|x| x.as_str()) {
        return Some(t.to_string());
    }
    if let Some(t) = v
        .get("event")
        .and_then(|e| e.get("delta"))
        .and_then(|d| d.get("text"))
        .and_then(|x| x.as_str())
    {
        return Some(t.to_string());
    }
    None
}

/// Translate a raw `opencode run --format json` NDJSON line into bridge event(s).
/// Opencode emits whole completed text parts, and a turn may emit several — so each `text`
/// event maps to a `delta` (clients treat `final` as an emit-once whole answer and would
/// drop the later parts; deltas accumulate in order, and the manager's terminal `result`
/// carries the full concatenation). Everything else — step markers, tool events, empty
/// text, non-JSON — yields no events, never a panic.
pub fn parse_opencode_event(line: &str) -> Vec<String> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    if v.get("type").and_then(|x| x.as_str()) != Some("text") {
        return vec![];
    }
    match v.get("part").and_then(|p| p.get("text")).and_then(|x| x.as_str()) {
        Some(t) if !t.is_empty() => vec![json!({"ev":"delta","text":t}).to_string()],
        _ => vec![],
    }
}

/// Extract the opencode session id (`sessionID`) from any NDJSON event line, if present.
/// The daemon captures it from a run's first events to continue the session via `--session`.
pub fn opencode_session_id(line: &str) -> Option<String> {
    serde_json::from_str::<Value>(line)
        .ok()?
        .get("sessionID")
        .and_then(|x| x.as_str())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single(line: &str) -> Value {
        let evs = parse_event(line);
        assert_eq!(evs.len(), 1, "expected exactly one event for {line}");
        serde_json::from_str(&evs[0]).unwrap()
    }

    #[test]
    fn parse_event_init() {
        let ev = single(r#"{"type":"system","subtype":"init","session_id":"abc-123","model":"claude-opus"}"#);
        assert_eq!(ev["ev"], "init");
        assert_eq!(ev["session_id"], "abc-123");
        assert_eq!(ev["model"], "claude-opus");
    }

    #[test]
    fn parse_event_result_carries_error_flag() {
        let ev = single(r#"{"type":"result","result":"done","is_error":true}"#);
        assert_eq!(ev["ev"], "result");
        assert_eq!(ev["text"], "done");
        assert_eq!(ev["is_error"], true);
    }

    #[test]
    fn parse_event_assistant_final_and_empty() {
        let ev = single(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#);
        assert_eq!(ev["ev"], "final");
        assert_eq!(ev["text"], "hi");
        // Assistant message with no text blocks emits nothing.
        assert!(parse_event(r#"{"type":"assistant","message":{"content":[{"type":"tool_use"}]}}"#).is_empty());
    }

    #[test]
    fn parse_event_delta_both_shapes() {
        let flat = single(r#"{"type":"content_block_delta","delta":{"text":"a"}}"#);
        assert_eq!(flat["ev"], "delta");
        assert_eq!(flat["text"], "a");
        let wrapped = single(r#"{"type":"stream_event","event":{"delta":{"text":"b"}}}"#);
        assert_eq!(wrapped["ev"], "delta");
        assert_eq!(wrapped["text"], "b");
    }

    #[test]
    fn parse_event_control_response_maps_to_interrupted() {
        let ok = single(r#"{"type":"control_response","response":{"subtype":"success","request_id":"i1"}}"#);
        assert_eq!(ok["ev"], "interrupted");
        assert_eq!(ok["ok"], true);
        let err = single(r#"{"type":"control_response","response":{"subtype":"error","request_id":"i1"}}"#);
        assert_eq!(err["ok"], false);
    }

    #[test]
    fn control_interrupt_line_round_trips() {
        let v: Value = serde_json::from_str(&control_interrupt_line("gw-1")).unwrap();
        assert_eq!(v["type"], "control_request");
        assert_eq!(v["request_id"], "gw-1");
        assert_eq!(v["request"]["subtype"], "interrupt");
    }

    #[test]
    fn parse_event_garbage_never_panics() {
        for line in ["", "not json", "{", "42", r#""just a string""#, r#"{"type":"unknown"}"#, "\u{0}\u{1}"] {
            assert!(parse_event(line).is_empty(), "garbage line {line:?} must yield no events");
        }
    }

    #[test]
    fn extract_text_concatenates_text_blocks_only() {
        let msg: Value = serde_json::from_str(
            r#"{"content":[{"type":"text","text":"one "},{"type":"tool_use","name":"x"},{"type":"text","text":"two"}]}"#,
        )
        .unwrap();
        assert_eq!(extract_text(Some(&msg)), "one two");
        assert_eq!(extract_text(None), "");
    }

    #[test]
    fn lane_key_main_is_bare_and_lanes_do_not_collide() {
        // Backward compat: lane `main` keeps the pre-lane bare project key.
        assert_eq!(lane_key("/proj", "main"), "/proj");
        // Other lanes are namespaced and mutually distinct.
        let brain = lane_key("/proj", "brain");
        let verify = lane_key("/proj", "verify");
        assert_ne!(brain, lane_key("/proj", "main"));
        assert_ne!(brain, verify);
        // The unit-separator namespacing cannot collide with another project's main key.
        assert_ne!(brain, lane_key("/proj-brain", "main"));
    }

    #[test]
    fn parse_opencode_event_text_part_becomes_delta() {
        // Deltas, not finals: clients treat `final` as an emit-once whole answer, so a turn
        // with several text parts would reach them truncated. Deltas accumulate in order.
        let evs = parse_opencode_event(
            r#"{"type":"text","timestamp":1751234567,"sessionID":"ses_abc","part":{"type":"text","text":"PONG","time":{"start":1,"end":2}}}"#,
        );
        assert_eq!(evs.len(), 1, "one text part maps to one delta event");
        let ev: Value = serde_json::from_str(&evs[0]).unwrap();
        assert_eq!(ev["ev"], "delta");
        assert_eq!(ev["text"], "PONG");
    }

    #[test]
    fn parse_opencode_event_ignores_everything_else() {
        for line in [
            r#"{"type":"step_start","timestamp":1751234567,"sessionID":"ses_abc","part":{}}"#,
            r#"{"type":"step_finish","timestamp":1751234567,"sessionID":"ses_abc","part":{"type":"step-finish","reason":"stop","tokens":{},"cost":0}}"#,
            r#"{"type":"text","part":{"type":"text"}}"#, // text event without a text payload
            r#"{"type":"text","part":{"type":"text","text":""}}"#, // empty text part
            "",
            "not json",
            "{",
        ] {
            assert!(parse_opencode_event(line).is_empty(), "line {line:?} must yield no events");
        }
    }

    #[test]
    fn opencode_session_id_from_any_event_shape() {
        let step = r#"{"type":"step_start","timestamp":1751234567,"sessionID":"ses_123","part":{}}"#;
        let text = r#"{"type":"text","sessionID":"ses_123","part":{"type":"text","text":"hi"}}"#;
        assert_eq!(opencode_session_id(step).as_deref(), Some("ses_123"));
        assert_eq!(opencode_session_id(text).as_deref(), Some("ses_123"));
        assert!(opencode_session_id(r#"{"type":"text","part":{}}"#).is_none());
        assert!(opencode_session_id("garbage").is_none());
    }

    #[test]
    fn lane_of_defaults_and_filters_empty() {
        assert_eq!(lane_of(None), "main");
        assert_eq!(lane_of(Some(String::new())), "main");
        assert_eq!(lane_of(Some("verify".into())), "verify");
    }
}
