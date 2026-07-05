//! Escalation prompt template: resolution (project file < global file < built-in),
//! placeholder rendering from the routing config, and guarded-block file editing.

use std::path::{Path, PathBuf};

use crate::config::{home, RoutingCfg};

pub const ESC_START: &str = "<!-- gw-bridge:escalation -->";
pub const ESC_END: &str = "<!-- /gw-bridge:escalation -->";

/// The built-in escalation prompt. A markdown TEMPLATE, not final text: `{brain_model}`,
/// `{brain_effort}`, `{verify_model}`, `{verify_effort}` are substituted from the routing
/// config at render time. Override per scope with `gw-bridge prompts --init`.
pub const DEFAULT_ESCALATION_TEMPLATE: &str = include_str!("../prompts/escalation.md");

pub fn prompt_file_global() -> PathBuf {
    home().join(".config/gw-bridge/escalation.md")
}

pub fn prompt_file_project(dir: &Path) -> PathBuf {
    dir.join(".gw-bridge").join("escalation.md")
}

/// The escalation template in effect: project file < global file < built-in default.
/// Returns (template, source label).
pub fn escalation_template(project_dir: Option<&Path>) -> (String, &'static str) {
    if let Some(d) = project_dir {
        if let Ok(s) = std::fs::read_to_string(prompt_file_project(d)) {
            return (s, "project");
        }
    }
    if let Ok(s) = std::fs::read_to_string(prompt_file_global()) {
        return (s, "global");
    }
    (DEFAULT_ESCALATION_TEMPLATE.to_string(), "built-in")
}

/// Pure placeholder substitution: fills `{brain_model}` `{brain_effort}` `{verify_model}`
/// `{verify_effort}` from the routing config. Unknown placeholders are left untouched.
pub fn render_escalation(tpl: &str, r: &RoutingCfg) -> String {
    tpl.replace("{brain_model}", &r.brain.model)
        .replace("{brain_effort}", &r.brain.effort)
        .replace("{verify_model}", &r.verify.model)
        .replace("{verify_effort}", &r.verify.effort)
}

/// Wrap a rendered body in the guard markers exactly once, so `upsert_block` can find and
/// replace the block even when a custom template already (or never) includes the markers.
pub fn wrap_guard(body: &str) -> String {
    let body = body.trim().trim_start_matches(ESC_START).trim_end_matches(ESC_END).trim();
    format!("{ESC_START}\n{body}\n{ESC_END}\n")
}

/// The final guarded block for a project: active template, rendered, wrapped.
pub fn escalation_block(r: &RoutingCfg, project_dir: Option<&Path>) -> String {
    let (tpl, _) = escalation_template(project_dir);
    wrap_guard(&render_escalation(&tpl, r))
}

/// Insert or replace the guarded escalation block in a markdown file (creating it if absent).
pub fn upsert_block(path: &Path, block: &str) -> std::io::Result<()> {
    let cur = std::fs::read_to_string(path).unwrap_or_default();
    let new = match (cur.find(ESC_START), cur.find(ESC_END)) {
        (Some(s), Some(e)) if e > s => {
            let end = e + ESC_END.len();
            format!("{}{}{}", &cur[..s], block.trim_end(), &cur[end..])
        }
        _ => {
            let mut out = cur;
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(block);
            out
        }
    };
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(path, new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LaneRoute;

    fn cfg() -> RoutingCfg {
        RoutingCfg {
            brain: LaneRoute { model: "opus".into(), effort: "max".into() },
            verify: LaneRoute { model: "sonnet".into(), effort: "low".into() },
            ..Default::default()
        }
    }

    #[test]
    fn render_substitutes_all_four_placeholders() {
        let tpl = "b={brain_model}@{brain_effort} v={verify_model}@{verify_effort}";
        assert_eq!(render_escalation(tpl, &cfg()), "b=opus@max v=sonnet@low");
    }

    #[test]
    fn render_leaves_unknown_placeholders_intact() {
        let tpl = "{brain_model} keeps {unknown_thing} and {verify_effort}";
        assert_eq!(render_escalation(tpl, &cfg()), "opus keeps {unknown_thing} and low");
    }

    #[test]
    fn default_template_renders_without_leftover_known_placeholders() {
        let out = render_escalation(DEFAULT_ESCALATION_TEMPLATE, &cfg());
        for ph in ["{brain_model}", "{brain_effort}", "{verify_model}", "{verify_effort}"] {
            assert!(!out.contains(ph), "placeholder {ph} was not substituted");
        }
        assert!(out.contains("opus") && out.contains("sonnet"));
    }

    #[test]
    fn wrap_guard_wraps_exactly_once() {
        let once = wrap_guard("body text");
        assert_eq!(once, format!("{ESC_START}\nbody text\n{ESC_END}\n"));
        let twice = wrap_guard(&once);
        assert_eq!(twice, once, "wrapping an already-wrapped body must not nest markers");
        assert_eq!(twice.matches(ESC_START).count(), 1);
        assert_eq!(twice.matches(ESC_END).count(), 1);
    }

    #[test]
    fn upsert_block_is_idempotent_and_preserves_surroundings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "# Existing rules\n\nkeep me\n").unwrap();

        let block = wrap_guard("the rule");
        upsert_block(&path, &block).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        assert!(first.contains("keep me"), "existing content must survive");
        assert!(first.contains("the rule"));

        upsert_block(&path, &block).unwrap();
        let second = std::fs::read_to_string(&path).unwrap();
        assert_eq!(second, first, "applying the same block twice must be byte-identical");
        assert_eq!(second.matches(ESC_START).count(), 1, "only one guarded block may exist");
    }

    #[test]
    fn upsert_block_replaces_existing_block_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        upsert_block(&path, &wrap_guard("old body")).unwrap();
        upsert_block(&path, &wrap_guard("new body")).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("new body"));
        assert!(!out.contains("old body"));
        assert_eq!(out.matches(ESC_START).count(), 1);
    }
}
