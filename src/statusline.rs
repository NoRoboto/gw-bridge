//! Pure rendering of the one-line health indicator for Claude Code's `statusLine`.

use serde_json::Value;

/// Render a bridge `status` event snapshot into the compact glyph + state line.
pub fn format_statusline(snap: &Value) -> String {
    let in_flight = snap.get("in_flight").and_then(|x| x.as_bool()).unwrap_or(false);
    let secs = snap.get("idle_ms").and_then(|x| x.as_u64()).unwrap_or(0) / 1000;
    let model = snap.get("model").and_then(|x| x.as_str()).unwrap_or("");
    let effort = snap.get("effort").and_then(|x| x.as_str()).unwrap_or("");

    let mut tag = String::new();
    if !model.is_empty() {
        tag.push_str(" · ");
        tag.push_str(model);
    }
    if !effort.is_empty() {
        tag.push('/');
        tag.push_str(effort);
    }

    if in_flight {
        format!("● gw-bridge working {secs}s{tag}")
    } else {
        format!("○ gw-bridge ready{tag}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_working_and_ready_states() {
        let working = json!({"in_flight": true, "idle_ms": 4500, "model": "opus", "effort": "max"});
        assert_eq!(format_statusline(&working), "● gw-bridge working 4s · opus/max");

        let ready = json!({"in_flight": false, "model": "sonnet", "effort": ""});
        assert_eq!(format_statusline(&ready), "○ gw-bridge ready · sonnet");

        // Missing fields degrade gracefully.
        assert_eq!(format_statusline(&json!({})), "○ gw-bridge ready");
    }
}
