//! Routing config: which model/effort each lane uses. Layered, not hardcoded:
//!   built-in default  <  ~/.config/gw-bridge/routing.json  <  <project>/.gw-bridge/routing.json
//! The escalation rule is RENDERED from this, and the daemon applies it as the
//! per-lane spawn default — change the config, not the source.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LaneRoute {
    pub model: String,
    pub effort: String,
}

/// The `worker` lane runs headless `opencode run`, so it routes by opencode's own axes:
/// model (`provider/model`) and agent. An empty string means "don't pass the flag" — the
/// worker then uses opencode's own defaults. Both empty is the built-in default.
#[derive(Default, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkerRoute {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub agent: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RoutingCfg {
    pub brain: LaneRoute,
    pub verify: LaneRoute,
    #[serde(default)]
    pub worker: WorkerRoute,
}

impl Default for RoutingCfg {
    fn default() -> Self {
        Self {
            brain: LaneRoute { model: "fable".into(), effort: "high".into() },
            verify: LaneRoute { model: "sonnet".into(), effort: "high".into() },
            worker: WorkerRoute::default(),
        }
    }
}

/// A routing file may set either lane or both; unset lanes fall through to the layer below.
#[derive(Default, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PartialRouting {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brain: Option<LaneRoute>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify: Option<LaneRoute>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<WorkerRoute>,
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

/// The daemon's private state directory: `$XDG_STATE_HOME/gw-bridge` or
/// `~/.local/state/gw-bridge`. Also hosts the fallback unix socket, so it must
/// never live somewhere world-writable.
pub fn state_dir() -> PathBuf {
    let base = if let Ok(d) = std::env::var("XDG_STATE_HOME") {
        PathBuf::from(d)
    } else {
        home().join(".local/state")
    };
    base.join("gw-bridge")
}

pub fn routing_global_file() -> PathBuf {
    home().join(".config/gw-bridge/routing.json")
}

pub fn routing_project_file(dir: &Path) -> PathBuf {
    dir.join(".gw-bridge").join("routing.json")
}

/// Legacy location (pre-directory layout); still read, never written.
pub fn routing_project_file_legacy(dir: &Path) -> PathBuf {
    dir.join(".gw-bridge.json")
}

pub fn read_partial_routing(p: &Path) -> PartialRouting {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn write_partial_routing(p: &Path, r: &PartialRouting) -> std::io::Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(p, serde_json::to_string_pretty(r).unwrap_or_default())
}

/// Pure merge of the routing layers: `defaults < global < project`, per lane.
/// Each lane falls through independently — a project file that only sets `verify`
/// still inherits `brain` from the global layer (or the default).
pub fn merge_routing(defaults: RoutingCfg, global: PartialRouting, project: PartialRouting) -> RoutingCfg {
    let mut r = defaults;
    for layer in [global, project] {
        if let Some(b) = layer.brain {
            r.brain = b;
        }
        if let Some(v) = layer.verify {
            r.verify = v;
        }
        if let Some(w) = layer.worker {
            r.worker = w;
        }
    }
    r
}

/// The routing in effect for a project (or globally when `project_dir` is None).
pub fn effective_routing(project_dir: Option<&Path>) -> RoutingCfg {
    let global = read_partial_routing(&routing_global_file());
    let project = match project_dir {
        Some(d) => {
            let f = routing_project_file(d);
            if f.exists() {
                read_partial_routing(&f)
            } else {
                read_partial_routing(&routing_project_file_legacy(d))
            }
        }
        None => PartialRouting::default(),
    };
    merge_routing(RoutingCfg::default(), global, project)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(model: &str, effort: &str) -> LaneRoute {
        LaneRoute { model: model.into(), effort: effort.into() }
    }

    #[test]
    fn merge_empty_layers_keep_defaults() {
        let r = merge_routing(RoutingCfg::default(), PartialRouting::default(), PartialRouting::default());
        assert_eq!(r, RoutingCfg::default());
    }

    #[test]
    fn merge_precedence_default_lt_global_lt_project() {
        let global = PartialRouting {
            brain: Some(route("g-brain", "low")),
            verify: Some(route("g-verify", "low")),
            worker: None,
        };
        let project = PartialRouting {
            brain: Some(route("p-brain", "max")),
            verify: Some(route("p-verify", "max")),
            worker: None,
        };
        let r = merge_routing(RoutingCfg::default(), global, project);
        assert_eq!(r.brain, route("p-brain", "max"));
        assert_eq!(r.verify, route("p-verify", "max"));
    }

    #[test]
    fn merge_lanes_fall_through_independently() {
        // Global sets only brain; project sets only verify. Each lane resolves on its own.
        let global = PartialRouting { brain: Some(route("g-brain", "low")), verify: None, worker: None };
        let project = PartialRouting { brain: None, verify: Some(route("p-verify", "xhigh")), worker: None };
        let r = merge_routing(RoutingCfg::default(), global, project);
        assert_eq!(r.brain, route("g-brain", "low"), "brain comes from the global layer");
        assert_eq!(r.verify, route("p-verify", "xhigh"), "verify comes from the project layer");

        // And the reverse: project sets only brain — verify falls through to the default.
        let project_only_brain = PartialRouting { brain: Some(route("p-brain", "high")), verify: None, worker: None };
        let r = merge_routing(RoutingCfg::default(), PartialRouting::default(), project_only_brain);
        assert_eq!(r.brain, route("p-brain", "high"));
        assert_eq!(r.verify, RoutingCfg::default().verify, "verify falls through to the default");
    }

    #[test]
    fn merge_worker_falls_through_like_the_other_lanes() {
        let w = |model: &str, agent: &str| WorkerRoute { model: model.into(), agent: agent.into() };
        // Built-in default: both empty = opencode's own defaults.
        let r = merge_routing(RoutingCfg::default(), PartialRouting::default(), PartialRouting::default());
        assert_eq!(r.worker, WorkerRoute::default());
        // Global sets worker; a project file that leaves it unset inherits it.
        let global = PartialRouting { worker: Some(w("zai/glm-4.7", "build")), ..Default::default() };
        let r = merge_routing(RoutingCfg::default(), global.clone(), PartialRouting::default());
        assert_eq!(r.worker, w("zai/glm-4.7", "build"));
        // A project worker entry wins over the global one, without touching the claude lanes.
        let project = PartialRouting { worker: Some(w("minimax/m3", "")), ..Default::default() };
        let r = merge_routing(RoutingCfg::default(), global, project);
        assert_eq!(r.worker, w("minimax/m3", ""));
        assert_eq!(r.brain, RoutingCfg::default().brain);
        assert_eq!(r.verify, RoutingCfg::default().verify);
    }
}
