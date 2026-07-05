//! gw-bridge — bidirectional text bridge between OpenCode and the official `claude` CLI.
//!
//! Legit by design: it only shuttles TEXT and drives the OFFICIAL `claude` CLI in its
//! native stream-json mode (so claude keeps its own Max/OAuth auth). OpenCode never touches
//! Claude credentials. No token masking, no third-party routing of the subscription.
//!
//! Roles:
//!   gw-bridge serve   — own a persistent claude stream-json session + serve a unix socket
//!   gw-bridge send …  — send one user turn, stream the reply to stdout (for OpenCode/skills)
//!   gw-bridge tap     — subscribe and print every bridge event (debug / live view)
//!   gw-bridge interrupt — cancel the in-flight claude turn

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc};
use tokio::time::{interval, MissedTickBehavior};

use gw_bridge::config::{
    effective_routing, home, read_partial_routing, routing_global_file, routing_project_file,
    routing_project_file_legacy, state_dir, write_partial_routing, LaneRoute, PartialRouting,
};
use gw_bridge::protocol::{control_interrupt_line, lane_key, lane_of, parse_event};
use gw_bridge::sessions::{sessions_file, SessionStore};
use gw_bridge::statusline::format_statusline;
use gw_bridge::template::{
    escalation_block, escalation_template, prompt_file_global, prompt_file_project, upsert_block,
};

#[derive(Parser)]
#[command(name = "gw-bridge", version, about = "Bidirectional text bridge: OpenCode <-> official claude CLI (stream-json).")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the bridge daemon: own a claude stream-json session and serve the unix socket.
    Serve,
    /// Send one user turn to claude and stream the reply to stdout.
    Send {
        /// Ask Opus on a specific model for this turn (e.g. `opus`, `sonnet`). Sticky until changed.
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort for this turn (low, medium, high, xhigh, max). Sticky until changed.
        #[arg(long)]
        effort: Option<String>,
        /// Project the session belongs to (default: current dir). The daemon keeps one brain per project.
        #[arg(long)]
        project: Option<String>,
        /// Lane within the project (default: main). Different lanes = independent, concurrent
        /// sessions — e.g. `brain` (Opus) and `verify` (Sonnet) at the same time.
        #[arg(long)]
        lane: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        text: Vec<String>,
    },
    /// Subscribe and print every bridge event (NDJSON) — debug / live view.
    Tap {
        /// Lane to observe (default: main).
        #[arg(long)]
        lane: Option<String>,
    },
    /// Interrupt the current claude turn.
    Interrupt {
        /// Lane to interrupt (default: main).
        #[arg(long)]
        lane: Option<String>,
    },
    /// Print bridge health: alive, in-flight, session, model.
    Status {
        /// Lane to report (default: main).
        #[arg(long)]
        lane: Option<String>,
    },
    /// Print a one-line health indicator for Claude Code's `statusLine` (never fails).
    Statusline {
        /// Lane to report (default: main).
        #[arg(long)]
        lane: Option<String>,
    },
    /// First-run setup: install the Opus-escalation rule (asks global or project if no --scope).
    Init {
        /// `global` (all projects) or `project` (the --dir / cwd). Omit to be asked on a TTY.
        #[arg(long)]
        scope: Option<String>,
        /// Target project dir for `project` scope (default: current dir).
        #[arg(long)]
        dir: Option<String>,
        /// Exit 0 if already configured for this scope/dir, else exit 2 (no writes).
        #[arg(long)]
        check: bool,
    },
    /// Re-apply the escalation rule (e.g. after `gentle-ai sync` clobbers a global file).
    Doctor,
    /// Set the session-wide Opus model/effort default without sending a turn (for OpenCode's
    /// per-session control). Sticky: takes effect on the next ask; no respawn if unchanged.
    Config {
        /// Model alias for Opus (e.g. opus, sonnet). Omit to leave unchanged.
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort: low, medium, high, xhigh, max. Omit to leave unchanged.
        #[arg(long)]
        effort: Option<String>,
        /// Project to apply it to (default: current dir).
        #[arg(long)]
        project: Option<String>,
        /// Lane to apply it to (default: main).
        #[arg(long)]
        lane: Option<String>,
    },
    /// Run an MCP server (stdio) that proxies to the bridge — exposes `ask_opus` to any MCP client.
    Mcp,
    /// Show or change the per-lane model routing (built-in default < global file < project file).
    /// With no flags: print the effective routing for this project. With --lane: set values.
    Routing {
        /// Lane to configure: `brain` or `verify`.
        #[arg(long)]
        lane: Option<String>,
        /// Model alias for the lane (e.g. fable, opus, sonnet).
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort for the lane: low, medium, high, xhigh, max.
        #[arg(long)]
        effort: Option<String>,
        /// Where to save: `project` (<dir>/.gw-bridge.json, default) or `global`
        /// (~/.config/gw-bridge/routing.json).
        #[arg(long)]
        scope: Option<String>,
        /// Project dir for `project` scope (default: current dir).
        #[arg(long)]
        dir: Option<String>,
        /// Interactive questionnaire on a TTY: asks scope + both lanes, saves, re-applies the rule.
        #[arg(long)]
        wizard: bool,
        /// Print the effective routing as JSON (for tooling/OpenCode).
        #[arg(long)]
        json: bool,
    },
    /// Show or customize the escalation prompt template (project file < global file < built-in).
    /// With no flags: report which template is active for this project and its path.
    Prompts {
        /// Copy the ACTIVE template to the target scope so you can edit it.
        #[arg(long)]
        init: bool,
        /// Target scope for --init: `project` (<dir>/.gw-bridge/escalation.md, default) or
        /// `global` (~/.config/gw-bridge/escalation.md).
        #[arg(long)]
        scope: Option<String>,
        /// Project dir for `project` scope (default: current dir).
        #[arg(long)]
        dir: Option<String>,
        /// Overwrite an existing template file on --init.
        #[arg(long)]
        force: bool,
    },
}

/// Control messages from clients to the claude-session manager.
enum Ctl {
    Ask {
        text: String,
        /// Requested model for this turn (None = keep current).
        model: Option<String>,
        /// Requested reasoning effort for this turn (None = keep current).
        effort: Option<String>,
    },
    /// Set the session-wide model/effort default without sending a turn (takes effect next ask).
    Config {
        model: Option<String>,
        effort: Option<String>,
    },
    Interrupt,
}

fn sock_path() -> PathBuf {
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(d).join("gw-bridge.sock");
    }
    // No XDG_RUNTIME_DIR: fall back to the user's private state dir (created with mode
    // 0700 by `serve`), never a world-writable location like /tmp.
    state_dir().join("gw-bridge.sock")
}

/// The project key a client tags its ops with: the `--project` override if given, else the
/// canonical current directory. The daemon keys a persistent Opus session per distinct value.
fn cwd_key(over: Option<String>) -> String {
    let raw = over
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    std::fs::canonicalize(&raw)
        .unwrap_or(raw)
        .to_string_lossy()
        .to_string()
}

/// Parse a duration from an env var (seconds). `0` disables the feature (returns None).
fn env_dur(key: &str, default_secs: u64) -> Option<Duration> {
    let secs = std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default_secs);
    (secs > 0).then(|| Duration::from_secs(secs))
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Serve => serve().await,
        Cmd::Send { model, effort, project, lane, text } => send_cmd(text.join(" "), model, effort, project, lane).await,
        Cmd::Tap { lane } => tap(lane).await,
        Cmd::Interrupt { lane } => one_op(json!({"op":"interrupt","project":cwd_key(None),"lane":lane_of(lane)})).await,
        Cmd::Status { lane } => status_cmd(lane).await,
        Cmd::Statusline { lane } => statusline_cmd(lane).await,
        Cmd::Init { scope, dir, check } => init_cmd(scope, dir, check),
        Cmd::Doctor => doctor_cmd(),
        Cmd::Config { model, effort, project, lane } => config_cmd(model, effort, project, lane).await,
        Cmd::Mcp => mcp_serve().await,
        Cmd::Routing { lane, model, effort, scope, dir, wizard, json } => {
            routing_cmd(lane, model, effort, scope, dir, wizard, json)
        }
        Cmd::Prompts { init, scope, dir, force } => prompts_cmd(init, scope, dir, force),
    }
}

// ---------------------------------------------------------------------------
// init / doctor: install the Opus-escalation rule (global or per-project)
// ---------------------------------------------------------------------------

fn init_state() -> PathBuf {
    state_dir().join("init.json")
}

fn load_state() -> Value {
    std::fs::read_to_string(init_state())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn save_state(v: &Value) -> std::io::Result<()> {
    let p = init_state();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(p, serde_json::to_string_pretty(v).unwrap_or_default())
}

/// Ensure `dir/CLAUDE.md` imports AGENTS.md so Claude Code also picks up the rule.
fn ensure_import(dir: &Path) -> std::io::Result<()> {
    let p = dir.join("CLAUDE.md");
    let cur = std::fs::read_to_string(&p).unwrap_or_default();
    if cur.contains("@AGENTS.md") {
        return Ok(());
    }
    let mut out = cur;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("@AGENTS.md\n");
    std::fs::write(&p, out)
}

fn abs(dir: &Path) -> String {
    std::fs::canonicalize(dir)
        .unwrap_or_else(|_| dir.to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn is_configured(dir: &Path) -> bool {
    let st = load_state();
    if st.get("scope").and_then(|x| x.as_str()) == Some("global") {
        return true;
    }
    let a = abs(dir);
    st.get("projects")
        .and_then(|x| x.as_array())
        .map(|arr| arr.iter().any(|v| v.as_str() == Some(a.as_str())))
        .unwrap_or(false)
}

fn apply_global() -> Result<(PathBuf, PathBuf)> {
    let block = escalation_block(&effective_routing(None), None);
    let oc = home().join(".config/opencode/AGENTS.md");
    let cl = home().join(".claude/CLAUDE.md");
    upsert_block(&oc, &block)?;
    upsert_block(&cl, &block)?;
    Ok((oc, cl))
}

fn apply_project(dir: &Path) -> Result<()> {
    let block = escalation_block(&effective_routing(Some(dir)), Some(dir));
    upsert_block(&dir.join("AGENTS.md"), &block)?;
    ensure_import(dir)?;
    Ok(())
}

fn init_cmd(scope: Option<String>, dir: Option<String>, check: bool) -> Result<()> {
    let dir = dir.map(PathBuf::from).unwrap_or(std::env::current_dir()?);

    if check {
        if is_configured(&dir) {
            return Ok(());
        }
        std::process::exit(2);
    }

    let scope = match scope {
        Some(s) => s,
        None => {
            use std::io::IsTerminal;
            if std::io::stdin().is_terminal() {
                print!("Install the Opus-escalation rule [g]lobal or [p]roject? ");
                std::io::stdout().flush().ok();
                let mut s = String::new();
                std::io::stdin().read_line(&mut s)?;
                match s.trim().to_lowercase().chars().next() {
                    Some('g') => "global".into(),
                    _ => "project".into(),
                }
            } else {
                anyhow::bail!("pass --scope global|project (no TTY to prompt on)");
            }
        }
    };

    match scope.as_str() {
        "global" => {
            let (oc, cl) = apply_global()?;
            save_state(&json!({"scope":"global"}))?;
            println!("gw-bridge: escalation rule installed GLOBALLY:\n  - {}\n  - {}", oc.display(), cl.display());
            eprintln!("note: `gentle-ai sync` may overwrite the global AGENTS.md; run `gw-bridge doctor` to re-apply.");
        }
        "project" => {
            apply_project(&dir)?;
            let mut projects: Vec<String> = load_state()
                .get("projects")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let a = abs(&dir);
            if !projects.contains(&a) {
                projects.push(a);
            }
            save_state(&json!({"scope":"project","projects":projects}))?;
            println!("gw-bridge: escalation rule installed in project {}", dir.display());
        }
        other => anyhow::bail!("invalid scope: {other} (use global|project)"),
    }
    Ok(())
}

fn doctor_cmd() -> Result<()> {
    let st = load_state();
    match st.get("scope").and_then(|x| x.as_str()) {
        Some("global") => {
            let (oc, cl) = apply_global()?;
            println!("gw-bridge doctor: global rule re-applied:\n  - {}\n  - {}", oc.display(), cl.display());
        }
        Some("project") => {
            let mut n = 0;
            if let Some(arr) = st.get("projects").and_then(|x| x.as_array()) {
                for p in arr.iter().filter_map(|v| v.as_str()) {
                    let d = PathBuf::from(p);
                    if d.is_dir() {
                        apply_project(&d)?;
                        n += 1;
                    }
                }
            }
            println!("gw-bridge doctor: rule re-applied in {n} project(s).");
        }
        _ => println!("gw-bridge doctor: no init recorded yet. Run `gw-bridge init`."),
    }
    Ok(())
}

/// Prompt with a default; empty input keeps the default.
fn prompt_default(label: &str, default: &str) -> Result<String> {
    print!("{label} [{default}]: ");
    std::io::stdout().flush().ok();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    let t = s.trim();
    Ok(if t.is_empty() { default.to_string() } else { t.to_string() })
}

/// After a routing change, re-render the escalation block wherever it is installed so the rule
/// text the workers read always matches the config the daemon applies.
fn reapply_rule(scope: &str, dir: &Path) -> Result<()> {
    match scope {
        "global" => {
            let (oc, cl) = apply_global()?;
            println!("rule re-rendered:\n  - {}\n  - {}", oc.display(), cl.display());
        }
        _ => {
            apply_project(dir)?;
            println!("rule re-rendered in {}", dir.join("AGENTS.md").display());
        }
    }
    Ok(())
}

fn routing_cmd(
    lane: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    scope: Option<String>,
    dir: Option<String>,
    wizard: bool,
    json_out: bool,
) -> Result<()> {
    let dir = dir.map(PathBuf::from).unwrap_or(std::env::current_dir()?);
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);

    if wizard {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("--wizard needs a TTY; use --lane/--model/--effort flags instead");
        }
        let eff = effective_routing(Some(&dir));
        let scope = match prompt_default("Save routing for [p]roject or [g]lobal?", "p")?
            .to_lowercase()
            .chars()
            .next()
        {
            Some('g') => "global".to_string(),
            _ => "project".to_string(),
        };
        let brain = LaneRoute {
            model: prompt_default("brain lane model (thinking/architecture)", &eff.brain.model)?,
            effort: prompt_default("brain lane effort (low|medium|high|xhigh|max)", &eff.brain.effort)?,
        };
        let verify = LaneRoute {
            model: prompt_default("verify lane model (verification/tests)", &eff.verify.model)?,
            effort: prompt_default("verify lane effort", &eff.verify.effort)?,
        };
        let file = if scope == "global" { routing_global_file() } else { routing_project_file(&dir) };
        write_partial_routing(&file, &PartialRouting { brain: Some(brain), verify: Some(verify) })?;
        println!("routing saved to {}", file.display());
        reapply_rule(&scope, &dir)?;
        return Ok(());
    }

    // Set mode: any of --lane/--model/--effort present.
    if lane.is_some() || model.is_some() || effort.is_some() {
        let lane = lane.context("--lane brain|verify is required to set routing")?;
        if lane != "brain" && lane != "verify" {
            anyhow::bail!("invalid --lane: {lane} (use brain|verify)");
        }
        let scope = scope.unwrap_or_else(|| "project".into());
        if scope != "project" && scope != "global" {
            anyhow::bail!("invalid --scope: {scope} (use project|global)");
        }
        let file = if scope == "global" { routing_global_file() } else { routing_project_file(&dir) };
        // Fill whatever the caller omitted from the currently effective values.
        let eff = effective_routing(Some(&dir));
        let cur = if lane == "brain" { eff.brain } else { eff.verify };
        let route = LaneRoute {
            model: model.unwrap_or(cur.model),
            effort: effort.unwrap_or(cur.effort),
        };
        let mut part = read_partial_routing(&file);
        if lane == "brain" {
            part.brain = Some(route.clone());
        } else {
            part.verify = Some(route.clone());
        }
        write_partial_routing(&file, &part)?;
        println!("routing[{lane}] = {} @ {} ({scope}: {})", route.model, route.effort, file.display());
        reapply_rule(&scope, &dir)?;
        return Ok(());
    }

    // Show mode.
    let eff = effective_routing(Some(&dir));
    if json_out {
        println!("{}", serde_json::to_string_pretty(&eff)?);
        return Ok(());
    }
    let gf = routing_global_file();
    let mut pf = routing_project_file(&dir);
    if !pf.exists() && routing_project_file_legacy(&dir).exists() {
        pf = routing_project_file_legacy(&dir);
    }
    let src = |lane: &str| -> &str {
        let p = read_partial_routing(&pf);
        let g = read_partial_routing(&gf);
        let (pp, gg) = if lane == "brain" { (p.brain.is_some(), g.brain.is_some()) } else { (p.verify.is_some(), g.verify.is_some()) };
        if pp {
            "project"
        } else if gg {
            "global"
        } else {
            "default"
        }
    };
    println!("effective routing for {}:", dir.display());
    println!("  brain  → {} @ {}  ({})", eff.brain.model, eff.brain.effort, src("brain"));
    println!("  verify → {} @ {}  ({})", eff.verify.model, eff.verify.effort, src("verify"));
    println!("layers: default < {} < {}", gf.display(), pf.display());
    println!("change: `gw-bridge routing --wizard` or `gw-bridge routing --lane brain --model X --effort Y [--scope global]`");
    Ok(())
}

fn prompts_cmd(init: bool, scope: Option<String>, dir: Option<String>, force: bool) -> Result<()> {
    let dir = dir.map(PathBuf::from).unwrap_or(std::env::current_dir()?);
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);

    if init {
        let scope = scope.unwrap_or_else(|| "project".into());
        let target = match scope.as_str() {
            "project" => prompt_file_project(&dir),
            "global" => prompt_file_global(),
            other => anyhow::bail!("invalid --scope: {other} (use project|global)"),
        };
        if target.exists() && !force {
            anyhow::bail!("{} already exists (use --force to overwrite)", target.display());
        }
        // Seed from the currently active template so a project starts from whatever the
        // global layer already customized, not necessarily the built-in.
        let (tpl, src) = escalation_template(Some(&dir));
        if let Some(p) = target.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&target, &tpl)?;
        println!("escalation template ({src}) copied to {}", target.display());
        println!("placeholders: {{brain_model}} {{brain_effort}} {{verify_model}} {{verify_effort}}");
        println!("edit it, then run `gw-bridge doctor` (or any `routing` change) to re-render the rule.");
        return Ok(());
    }

    let (_, src) = escalation_template(Some(&dir));
    println!("active escalation template for {}: {src}", dir.display());
    println!("layers: built-in < {} < {}", prompt_file_global().display(), prompt_file_project(&dir).display());
    println!("customize: `gw-bridge prompts --init [--scope global]`, edit, then `gw-bridge doctor`.");
    Ok(())
}

// ---------------------------------------------------------------------------
// serve: unix socket broker + claude session manager
// ---------------------------------------------------------------------------

/// Shared bridge health snapshot — written by the manager, read by `status` clients.
#[derive(Default)]
struct StatusState {
    session_id: String,
    model: String,
    effort: String,
    in_flight: bool,
    last_activity: Option<Instant>,
}

/// A single project's "brain": its own claude session manager + event stream + health.
struct Project {
    asks: mpsc::Sender<Ctl>,
    events: broadcast::Sender<String>,
    status: Arc<Mutex<StatusState>>,
}

/// All live projects, keyed by canonical project directory. The daemon multiplexes the one
/// socket across N persistent Opus sessions — one per project, resumed across restarts.
type Registry = Arc<Mutex<HashMap<String, Arc<Project>>>>;

/// The daemon-owned session map: loaded ONCE at `serve` start, mutated only under this lock,
/// and flushed atomically through `SessionStore::save`. Managers never touch the file directly,
/// so two lanes starting concurrently can no longer lose each other's writes.
type SharedSessions = Arc<Mutex<SessionStore>>;

/// Look up (or lazily spawn) the manager for a (project, lane). Each lane is an independent
/// claude session — Opus on lane `brain` and Sonnet on lane `verify` run at the same time,
/// on their own session ids, without colliding. Returns the lane's handle.
fn get_or_create_project(reg: &Registry, sessions: &SharedSessions, project: &str, lane: &str) -> Arc<Project> {
    let key = lane_key(project, lane);
    let mut map = reg.lock().unwrap();
    if let Some(p) = map.get(&key) {
        return p.clone();
    }
    let (asks_tx, asks_rx) = mpsc::channel::<Ctl>(256);
    let (ev_tx, _rx) = broadcast::channel::<String>(2048);
    let status = Arc::new(Mutex::new(StatusState::default()));
    let proj = Arc::new(Project {
        asks: asks_tx,
        events: ev_tx.clone(),
        status: status.clone(),
    });
    map.insert(key, proj.clone());
    let project = project.to_string();
    let lane = lane.to_string();
    let sessions = sessions.clone();
    tokio::spawn(async move {
        if let Err(e) = manager(project, lane, asks_rx, ev_tx, status, sessions).await {
            eprintln!("gw-bridge: manager exited: {e:#}");
        }
    });
    proj
}

/// True when a live daemon is already listening on `sp`. A successful connect means
/// SOMETHING accepts on the socket — never steal it in that case; a status ping is sent
/// as confirmation, best-effort. A refused connect means the file is a stale leftover.
async fn daemon_alive(sp: &Path) -> bool {
    let Ok(stream) = UnixStream::connect(sp).await else {
        return false;
    };
    let (rd, mut wr) = stream.into_split();
    let ping = json!({"op":"status","project":cwd_key(None),"lane":"main"}).to_string();
    let _ = wr.write_all(format!("{ping}\n").as_bytes()).await;
    let _ = wr.flush().await;
    let mut lines = BufReader::new(rd).lines();
    let _ = tokio::time::timeout(Duration::from_secs(1), lines.next_line()).await;
    true
}

async fn serve() -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let sp = sock_path();
    if let Some(p) = sp.parent() {
        if !p.exists() {
            // Private socket/state dir: only this user may traverse it.
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(p)
                .with_context(|| format!("create {}", p.display()))?;
        }
    }
    if sp.exists() {
        // Single-instance guard: only reclaim the path if nothing answers on it.
        if daemon_alive(&sp).await {
            eprintln!("gw-bridge: daemon already running at {}", sp.display());
            std::process::exit(1);
        }
        let _ = std::fs::remove_file(&sp); // stale socket from a dead daemon
    }
    let listener = UnixListener::bind(&sp).with_context(|| format!("bind {}", sp.display()))?;
    // Belt and suspenders on top of the 0700 dir: the socket itself is owner-only.
    std::fs::set_permissions(&sp, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 {}", sp.display()))?;

    let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
    // Single owner of sessions.json for the daemon's whole life (see `SharedSessions`).
    let sessions: SharedSessions = Arc::new(Mutex::new(SessionStore::load(sessions_file())));

    eprintln!("gw-bridge: listening on {} (multi-project)", sp.display());
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(handle_client(stream, registry.clone(), sessions.clone()));
    }
}

/// Per-client connection. Each op carries a `project` key (canonical dir); the connection is
/// bound to the first project it names: its writer streams that project's events, and its ops
/// are routed to that project's manager (spawned lazily on demand).
async fn handle_client(stream: UnixStream, registry: Registry, sessions: SharedSessions) {
    let (rd, wr) = stream.into_split();
    let mut wr_opt = Some(wr);
    let mut writer: Option<tokio::task::JoinHandle<()>> = None;
    let mut lines = BufReader::new(rd).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let project = v.get("project").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let lane = v.get("lane").and_then(|x| x.as_str()).unwrap_or("main").to_string();
        let proj = get_or_create_project(&registry, &sessions, &project, &lane);

        // Bind this connection's writer to the project's event stream (once, before the first op
        // is forwarded, so no early events are missed).
        if writer.is_none() {
            let mut sub = proj.events.subscribe();
            let mut wr = wr_opt.take().unwrap();
            writer = Some(tokio::spawn(async move {
                loop {
                    match sub.recv().await {
                        Ok(msg) => {
                            if wr.write_all(msg.as_bytes()).await.is_err()
                                || wr.write_all(b"\n").await.is_err()
                            {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            }));
        }

        match v.get("op").and_then(|x| x.as_str()) {
            Some("ask") => {
                let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let model = v.get("model").and_then(|x| x.as_str()).map(String::from);
                let effort = v.get("effort").and_then(|x| x.as_str()).map(String::from);
                let _ = proj.asks.send(Ctl::Ask { text, model, effort }).await;
            }
            Some("config") => {
                let model = v.get("model").and_then(|x| x.as_str()).map(String::from);
                let effort = v.get("effort").and_then(|x| x.as_str()).map(String::from);
                let _ = proj.asks.send(Ctl::Config { model, effort }).await;
            }
            Some("interrupt") => {
                let _ = proj.asks.send(Ctl::Interrupt).await;
            }
            Some("status") => {
                let ev = {
                    let s = proj.status.lock().unwrap();
                    let idle_ms = s.last_activity.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
                    json!({
                        "ev": "status",
                        "alive": true,
                        "project": project,
                        "lane": lane,
                        "in_flight": s.in_flight,
                        "session_id": s.session_id,
                        "model": s.model,
                        "effort": s.effort,
                        "idle_ms": idle_ms,
                    })
                    .to_string()
                };
                let _ = proj.events.send(ev);
            }
            _ => {} // "subscribe" or unknown: the writer is already bound above
        }
    }
    if let Some(w) = writer {
        w.abort();
    }
}

/// Owns the claude stream-json child. Lazily (re)spawns on the next ask; keeps the process
/// alive across turns for low latency. Resumes the same session id after a respawn.
async fn manager(
    project: String,
    lane: String,
    mut asks: mpsc::Receiver<Ctl>,
    tx: broadcast::Sender<String>,
    status: Arc<Mutex<StatusState>>,
    sessions: SharedSessions,
) -> Result<()> {
    // Per (project, lane) session key — each lane persists and resumes independently.
    let session_key = lane_key(&project, &lane);
    // Resume this lane's prior conversation across restarts: reuse its saved session id if
    // present (then `--resume`), else mint a fresh one and persist it.
    let (mut sid, mut resume) = match sessions.lock().unwrap().get(&session_key) {
        Some(id) => (id.clone(), true),
        None => (uuid::Uuid::new_v4().to_string(), false),
    };
    // Persist the id only once we actually spawn claude for a real ask — so a bare `status`/
    // `statusline` ping never reserves a session that was never created.
    let mut persisted = resume;
    // claude scopes sessions by working dir, so run the child IN the project dir (also picks up
    // that project's CLAUDE.md). Empty project = inherit the daemon's cwd. Lanes share the dir.
    let cwd: Option<String> = (!project.is_empty()).then(|| project.clone());

    // Knobs (seconds; 0 disables). Defaults: kill a hung turn at 5m, ping every 15s, reclaim idle at 10m.
    let turn_timeout = env_dur("GW_TURN_TIMEOUT", 300);
    let heartbeat = env_dur("GW_HEARTBEAT", 15);
    let idle_timeout = env_dur("GW_IDLE_TIMEOUT", 600);

    // Model/effort are spawn flags, not per-message: switching them respawns claude with --resume.
    // Named lanes (brain/verify) start from the project's routing config — that's what makes the
    // routing parameterizable without touching the rule text. `main` keeps the env defaults.
    // A per-ask override or a `config` op still wins afterwards (sticky).
    let project_dir = (!project.is_empty()).then(|| PathBuf::from(&project));
    let (mut cur_model, mut cur_effort): (Option<String>, Option<String>) = match lane.as_str() {
        "brain" => {
            let r = effective_routing(project_dir.as_deref()).brain;
            (Some(r.model), Some(r.effort))
        }
        "verify" => {
            let r = effective_routing(project_dir.as_deref()).verify;
            (Some(r.model), Some(r.effort))
        }
        _ => (
            std::env::var("GW_MODEL").ok().filter(|s| !s.is_empty()),
            std::env::var("GW_EFFORT").ok().filter(|s| !s.is_empty()),
        ),
    };
    // Ask text to deliver right after a deliberate respawn (None = wait for the next client ask).
    let mut pending: Option<String> = None;

    loop {
        // Wait for the first ask before spawning (avoids respawn spin / wasted sessions),
        // unless we already hold a pending ask from a model/effort switch.
        let first = match pending.take() {
            Some(t) => t,
            None => match next_ask(&mut asks, &mut cur_model, &mut cur_effort, &status, &tx).await {
                Some((text, model, effort)) => {
                    if model.is_some() {
                        cur_model = model;
                    }
                    if effort.is_some() {
                        cur_effort = effort;
                    }
                    text
                }
                None => return Ok(()), // all clients gone
            },
        };

        let used_resume = resume;
        let mut child = match spawn_claude(&sid, used_resume, cwd.as_deref(), cur_model.as_deref(), cur_effort.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                // Don't let a spawn failure kill the bridge — report it and wait for the next ask.
                let _ = tx.send(json!({"ev":"error","text":format!("{e:#}")}).to_string());
                let _ = tx.send(json!({"ev":"session_ended"}).to_string());
                continue;
            }
        };
        resume = true;
        if !persisted {
            sessions.lock().unwrap().insert(session_key.clone(), sid.clone());
            persisted = true;
        }
        // Did this spawn ever reach an `init`? If a `--resume` spawn never does, the saved
        // session id is stale and we regenerate it after the loop.
        let mut ever_init = false;
        // What the LIVE child was spawned with — `cur_*` is the DESIRED value. When they drift
        // apart (a per-ask override or a `config` op), the next ask respawns to reconcile.
        let spawned_model = cur_model.clone();
        let spawned_effort = cur_effort.clone();
        {
            let mut st = status.lock().unwrap();
            if let Some(m) = &cur_model {
                st.model = m.clone();
            }
            st.effort = cur_effort.clone().unwrap_or_default();
        }
        let mut stdin = child.stdin.take().context("claude sin stdin")?;
        let stdout = child.stdout.take().context("claude sin stdout")?;

        let (eof_tx, mut eof_rx) = tokio::sync::oneshot::channel::<()>();
        let txr = tx.clone();
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                for ev in parse_event(&line) {
                    let _ = txr.send(ev);
                }
            }
            let _ = eof_tx.send(());
        });

        // The manager watches its own event stream to know when a turn completes.
        let mut sub = tx.subscribe();
        let mut ticker = interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let _ = write_user(&mut stdin, &first).await;
        let mut in_flight = true;
        let mut turn_start = Instant::now();
        let mut last_activity = Instant::now();
        let mut last_hb = Instant::now();
        // Set when a clean interrupt was sent; if claude doesn't end the turn by then, we
        // fall back to the old kill+resume path.
        let mut interrupt_deadline: Option<Instant> = None;

        loop {
            tokio::select! {
                _ = &mut eof_rx => break, // claude exited
                ev = sub.recv() => {
                    if let Ok(s) = ev {
                        if s.contains("\"ev\":\"init\"") {
                            ever_init = true;
                            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                                let mut st = status.lock().unwrap();
                                st.session_id = v.get("session_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                                st.model = v.get("model").and_then(|x| x.as_str()).unwrap_or("").to_string();
                            }
                        } else if s.contains("\"ev\":\"result\"") {
                            in_flight = false;
                            interrupt_deadline = None;
                            last_activity = Instant::now();
                            let mut st = status.lock().unwrap();
                            st.in_flight = false;
                            st.last_activity = Some(Instant::now());
                        }
                    }
                }
                a = asks.recv() => match a {
                    Some(Ctl::Ask { text, model, effort }) => {
                        // Sticky overrides update the desired value; respawn only if it now
                        // differs from what the LIVE child was spawned with.
                        if model.is_some() { cur_model = model; }
                        if effort.is_some() { cur_effort = effort; }
                        if cur_model != spawned_model || cur_effort != spawned_effort {
                            let _ = tx.send(json!({
                                "ev":"model_switch",
                                "model": cur_model.clone().unwrap_or_default(),
                                "effort": cur_effort.clone().unwrap_or_default(),
                            }).to_string());
                            pending = Some(text); // delivered after the respawn (--resume keeps history)
                            let _ = child.start_kill();
                            break;
                        }
                        if write_user(&mut stdin, &text).await.is_err() { break; }
                        in_flight = true;
                        interrupt_deadline = None;
                        turn_start = Instant::now();
                        last_activity = Instant::now();
                        last_hb = Instant::now();
                        let mut st = status.lock().unwrap();
                        st.in_flight = true;
                        st.last_activity = Some(Instant::now());
                    }
                    Some(Ctl::Config { model, effort }) => {
                        // Set the session default; takes effect on the next ask (sticky drift).
                        if model.is_some() { cur_model = model; }
                        if effort.is_some() { cur_effort = effort; }
                        {
                            let mut st = status.lock().unwrap();
                            if let Some(m) = &cur_model { st.model = m.clone(); }
                            st.effort = cur_effort.clone().unwrap_or_default();
                        }
                        let _ = tx.send(json!({
                            "ev":"config",
                            "model": cur_model.clone().unwrap_or_default(),
                            "effort": cur_effort.clone().unwrap_or_default(),
                        }).to_string());
                    }
                    Some(Ctl::Interrupt) => {
                        // Clean path: ask claude to cancel the turn in place; the aborted turn
                        // still emits its `result`, which unblocks clients and clears in_flight.
                        // Idle sessions have nothing to cancel — keep them alive. Repeated
                        // interrupts don't re-arm the deadline, so a hung child still dies on time.
                        if in_flight && interrupt_deadline.is_none() {
                            if write_interrupt(&mut stdin).await.is_ok() {
                                interrupt_deadline = Some(Instant::now() + Duration::from_secs(10));
                            } else {
                                let _ = child.start_kill();
                                break;
                            }
                        }
                    }
                    None => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        reader.abort();
                        return Ok(());
                    }
                },
                _ = ticker.tick() => {
                    let now = Instant::now();
                    if in_flight {
                        if let Some(dl) = interrupt_deadline {
                            if now >= dl {
                                let _ = tx.send(json!({"ev":"error","text":"clean interrupt not honored in time; killing turn"}).to_string());
                                let _ = child.start_kill();
                                break;
                            }
                        }
                        if let Some(to) = turn_timeout {
                            if now.duration_since(turn_start) >= to {
                                let _ = tx.send(json!({"ev":"timeout","after_ms":to.as_millis() as u64}).to_string());
                                let _ = child.start_kill();
                                break;
                            }
                        }
                        if let Some(hb) = heartbeat {
                            if now.duration_since(last_hb) >= hb {
                                let _ = tx.send(json!({"ev":"heartbeat","elapsed_ms":now.duration_since(turn_start).as_millis() as u64}).to_string());
                                last_hb = now;
                            }
                        }
                    } else if let Some(idle) = idle_timeout {
                        if now.duration_since(last_activity) >= idle {
                            let _ = tx.send(json!({"ev":"idle_reclaim"}).to_string());
                            let _ = child.start_kill();
                            break;
                        }
                    }
                }
            }
        }

        reader.abort();
        let _ = child.start_kill();
        let _ = child.wait().await;
        if used_resume && !ever_init {
            // The saved session id couldn't be resumed (expired/cleaned). Regenerate + persist so
            // the next ask starts fresh, and tell the client to re-send.
            let new = uuid::Uuid::new_v4().to_string();
            sessions.lock().unwrap().insert(session_key.clone(), new.clone());
            sid = new;
            resume = false;
            persisted = true;
            let _ = tx.send(json!({"ev":"error","text":"previous session could not be resumed; starting fresh — re-send your message"}).to_string());
        } else if pending.is_none() {
            // A model/effort switch leaves a pending ask: respawn silently, don't signal "ended".
            let _ = tx.send(json!({"ev":"session_ended"}).to_string());
        }
        {
            let mut st = status.lock().unwrap();
            st.in_flight = false;
        }
    }
}

type AskParts = (String, Option<String>, Option<String>);

/// Wait for the next ask (the trigger to spawn). While waiting, a `config` op updates the
/// session default in place so the upcoming spawn picks it up; `interrupt` is a no-op here.
async fn next_ask(
    asks: &mut mpsc::Receiver<Ctl>,
    cur_model: &mut Option<String>,
    cur_effort: &mut Option<String>,
    status: &Arc<Mutex<StatusState>>,
    tx: &broadcast::Sender<String>,
) -> Option<AskParts> {
    loop {
        match asks.recv().await {
            Some(Ctl::Ask { text, model, effort }) => return Some((text, model, effort)),
            Some(Ctl::Config { model, effort }) => {
                if model.is_some() { *cur_model = model; }
                if effort.is_some() { *cur_effort = effort; }
                {
                    let mut st = status.lock().unwrap();
                    if let Some(m) = cur_model.as_ref() { st.model = m.clone(); }
                    st.effort = cur_effort.clone().unwrap_or_default();
                }
                let _ = tx.send(json!({
                    "ev": "config",
                    "model": cur_model.clone().unwrap_or_default(),
                    "effort": cur_effort.clone().unwrap_or_default(),
                }).to_string());
            }
            Some(Ctl::Interrupt) => continue,
            None => return None,
        }
    }
}

fn spawn_claude(
    sid: &str,
    resume: bool,
    cwd: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<tokio::process::Child> {
    let mut c = Command::new("claude");
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    c.arg("-p")
        .arg("--verbose")
        .arg("--input-format").arg("stream-json")
        .arg("--output-format").arg("stream-json")
        .arg("--include-partial-messages");
    if let Some(m) = model {
        c.arg("--model").arg(m);
    }
    if let Some(e) = effort {
        c.arg("--effort").arg(e);
    }
    if resume {
        c.arg("--resume").arg(sid);
    } else {
        c.arg("--session-id").arg(sid);
    }
    c.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    c.spawn().context("could not spawn `claude` (is it on PATH?)")
}

/// Ask claude to cancel the in-flight turn in place (no kill, no resume).
async fn write_interrupt(stdin: &mut tokio::process::ChildStdin) -> std::io::Result<()> {
    let line = control_interrupt_line(&format!("gw-{}", uuid::Uuid::new_v4()));
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

async fn write_user(stdin: &mut tokio::process::ChildStdin, text: &str) -> std::io::Result<()> {
    let line = json!({
        "type": "user",
        "message": { "role": "user", "content": [{ "type": "text", "text": text }] }
    })
    .to_string();
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

// ---------------------------------------------------------------------------
// clients: send / tap / interrupt
// ---------------------------------------------------------------------------

async fn connect() -> Result<UnixStream> {
    UnixStream::connect(sock_path())
        .await
        .context("could not connect — is `gw-bridge serve` running?")
}

async fn send_cmd(text: String, model: Option<String>, effort: Option<String>, project: Option<String>, lane: Option<String>) -> Result<()> {
    let stream = connect().await?;
    let (rd, mut wr) = stream.into_split();
    let mut op = json!({"op":"ask","text":text,"project":cwd_key(project),"lane":lane_of(lane)});
    if let Some(m) = model {
        op["model"] = json!(m);
    }
    if let Some(e) = effort {
        op["effort"] = json!(e);
    }
    let ask = op.to_string();
    wr.write_all(ask.as_bytes()).await?;
    wr.write_all(b"\n").await?;
    wr.flush().await?;

    let mut lines = BufReader::new(rd).lines();
    let mut printed = false;
    while let Some(line) = lines.next_line().await? {
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v.get("ev").and_then(|x| x.as_str()) {
            Some("delta") => {
                if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
                    print!("{t}");
                    let _ = std::io::stdout().flush();
                    printed = true;
                }
            }
            Some("final") => {
                if !printed {
                    if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
                        print!("{t}");
                        printed = true;
                    }
                }
            }
            Some("result") => {
                if !printed {
                    if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
                        print!("{t}");
                    }
                }
                println!();
                break;
            }
            Some("timeout") => {
                eprintln!("\n[gw-bridge: turn exceeded the timeout and was cancelled]");
                break;
            }
            Some("session_ended") => {
                eprintln!("\n[gw-bridge: claude session ended before the turn completed]");
                break;
            }
            Some("error") => {
                let msg = v.get("text").and_then(|x| x.as_str()).unwrap_or("unknown error");
                eprintln!("\n[gw-bridge: {msg}]");
                break;
            }
            _ => {} // heartbeat / init / idle_reclaim / model_switch: ignored by send
        }
    }
    Ok(())
}

/// Set the session-wide Opus model/effort default on the daemon (no turn sent).
async fn config_cmd(model: Option<String>, effort: Option<String>, project: Option<String>, lane: Option<String>) -> Result<()> {
    if model.is_none() && effort.is_none() {
        anyhow::bail!("nothing to set — pass --model and/or --effort");
    }
    let stream = connect().await?;
    let (rd, mut wr) = stream.into_split();
    let mut op = json!({"op": "config", "project": cwd_key(project), "lane": lane_of(lane)});
    if let Some(m) = model {
        op["model"] = json!(m);
    }
    if let Some(e) = effort {
        op["effort"] = json!(e);
    }
    wr.write_all(format!("{op}\n").as_bytes()).await?;
    wr.flush().await?;

    // Confirm by waiting for the daemon's `config` echo (short timeout).
    let mut lines = BufReader::new(rd).lines();
    let read = async {
        while let Some(line) = lines.next_line().await? {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if v.get("ev").and_then(|x| x.as_str()) == Some("config") {
                    let m = v.get("model").and_then(|x| x.as_str()).unwrap_or("");
                    let e = v.get("effort").and_then(|x| x.as_str()).unwrap_or("");
                    let shown = |s: &str| if s.is_empty() { "(claude default)".to_string() } else { s.to_string() };
                    println!("gw-bridge: Opus default set — model={} effort={}", shown(m), shown(e));
                    return Ok::<bool, anyhow::Error>(true);
                }
            }
        }
        Ok(false)
    };
    if (tokio::time::timeout(Duration::from_secs(3), read).await).is_err() {
        println!("gw-bridge: config sent (no echo within timeout)");
    }
    Ok(())
}

async fn tap(lane: Option<String>) -> Result<()> {
    let stream = connect().await?;
    let (rd, mut wr) = stream.into_split();
    let op = json!({"op":"subscribe","project":cwd_key(None),"lane":lane_of(lane)}).to_string();
    let _ = wr.write_all(format!("{op}\n").as_bytes()).await;
    let mut lines = BufReader::new(rd).lines();
    while let Some(line) = lines.next_line().await? {
        println!("{line}");
    }
    Ok(())
}

async fn one_op(op: Value) -> Result<()> {
    let mut stream = connect().await?;
    let line = format!("{op}\n");
    stream.write_all(line.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

async fn status_cmd(lane: Option<String>) -> Result<()> {
    let stream = match UnixStream::connect(sock_path()).await {
        Ok(s) => s,
        Err(_) => {
            println!("{}", json!({"alive": false}));
            return Ok(());
        }
    };
    let (rd, mut wr) = stream.into_split();
    let op = json!({"op":"status","project":cwd_key(None),"lane":lane_of(lane)}).to_string();
    wr.write_all(format!("{op}\n").as_bytes()).await?;
    wr.flush().await?;

    let mut lines = BufReader::new(rd).lines();
    let read = async {
        while let Some(line) = lines.next_line().await? {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if v.get("ev").and_then(|x| x.as_str()) == Some("status") {
                    println!("{line}");
                    return Ok::<bool, anyhow::Error>(true);
                }
            }
        }
        Ok(false)
    };
    if (tokio::time::timeout(Duration::from_secs(3), read).await).is_err() {
        println!("{}", json!({"alive": true, "note": "no status event (timeout)"}));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// mcp: an MCP stdio server that proxies tool calls to the bridge socket.
// Exposes `ask_opus` / `bridge_status` to any MCP client (Claude Code, OpenCode, …).
// The unix socket stays the internal transport, so the claude "brain" is shared.
// ---------------------------------------------------------------------------

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ProgressNotificationParam, ProgressToken, ServerCapabilities, ServerInfo};
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, RoleServer, ServerHandler, ServiceExt};

#[derive(Clone)]
struct McpBridge;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AskOpusArgs {
    /// A clear, self-contained question for Opus. Include the minimal context (paths, the
    /// decision, the options) — Opus has its own session but cannot see your screen.
    question: String,
    /// Optional model alias for this turn (e.g. "opus", "sonnet"). Sticky until changed.
    #[serde(default)]
    model: Option<String>,
    /// Optional reasoning effort: low, medium, high, xhigh, max. Sticky until changed.
    #[serde(default)]
    effort: Option<String>,
    /// Optional lane — an independent, concurrent claude session within the project.
    /// Use "brain" for thinking/architecture/analysis and "verify" for verification/test
    /// design; each lane's model/effort comes from the project's routing config (see
    /// `gw-bridge routing`), so leave model/effort unset unless explicitly overriding.
    /// Lanes run in parallel without colliding. Default: "main".
    #[serde(default)]
    lane: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BridgeStatusArgs {
    /// Optional lane to inspect (e.g. "brain", "verify"). Default: "main".
    #[serde(default)]
    lane: Option<String>,
}

#[tool_router]
impl McpBridge {
    #[tool(
        description = "Ask a Claude brain one question via gw-bridge and get the answer. \
        Requires `gw-bridge serve` running. Route by the kind of help: thinking/architecture/\
        analysis/tradeoffs → lane \"brain\"; verification/test design/\"what breaks this\" → \
        lane \"verify\". Each lane already runs the project's configured model/effort — omit \
        model/effort unless explicitly overriding. Lanes run concurrently. Not for routine \
        implementation."
    )]
    async fn ask_opus(
        &self,
        params: Parameters<AskOpusArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let AskOpusArgs { question, model, effort, lane } = params.0;
        mcp_ask(question, model, effort, lane, ctx).await
    }

    #[tool(
        description = "Report gw-bridge health for one lane: whether the daemon is up, whether a \
        turn is in flight, the active model/effort, and idle time. Returns JSON."
    )]
    async fn bridge_status(&self, params: Parameters<BridgeStatusArgs>) -> Result<String, String> {
        mcp_status(params.0.lane).await
    }
}

#[tool_handler]
impl ServerHandler for McpBridge {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo is #[non_exhaustive] — start from default and set what we care about.
        let mut info = ServerInfo::default();
        // Render the routing that is actually in effect for this project (the MCP server runs
        // in the client's cwd), so the client always reads current config, never stale text.
        let r = effective_routing(std::env::current_dir().ok().as_deref());
        info.instructions = Some(format!(
            "gw-bridge proxies to the official `claude` CLI. Call `ask_opus` for decisions above \
             implementation, routing by lane: lane=brain for thinking/architecture/analysis \
             (currently {} @ {}); lane=verify for verification (currently {} @ {}). The daemon \
             applies those models per lane — omit model/effort. Lanes run concurrently. Call \
             `bridge_status` to check a lane's health. It moves text only and never sees \
             credentials.",
            r.brain.model, r.brain.effort, r.verify.model, r.verify.effort
        ));
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

async fn mcp_serve() -> Result<()> {
    // stdio IS the MCP channel — keep stdout clean for protocol frames; logs go to stderr.
    let service = McpBridge.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Fire a progress notification toward the MCP client, if it asked for one (provided a token).
async fn mcp_progress(
    ctx: &RequestContext<RoleServer>,
    token: &Option<ProgressToken>,
    progress: &mut f64,
    message: String,
) {
    if let Some(tok) = token {
        *progress += 1.0; // MUST be monotonically increasing per the MCP spec
        let _ = ctx
            .peer
            .notify_progress(ProgressNotificationParam {
                progress_token: tok.clone(),
                progress: *progress,
                total: None,
                message: Some(message),
            })
            .await;
    }
}

/// Run one `ask` turn over the bridge socket, streaming deltas/heartbeats as MCP progress and
/// returning the final answer. MCP cancellation (`ctx.ct`) is forwarded as a bridge `interrupt`.
async fn mcp_ask(
    question: String,
    model: Option<String>,
    effort: Option<String>,
    lane: Option<String>,
    ctx: RequestContext<RoleServer>,
) -> Result<String, String> {
    let stream = UnixStream::connect(sock_path())
        .await
        .map_err(|_| "gw-bridge daemon is not running (start it with `gw-bridge serve`)".to_string())?;
    let (rd, mut wr) = stream.into_split();

    let project = cwd_key(None);
    let lane = lane_of(lane);
    let mut op = json!({"op": "ask", "text": question, "project": project.clone(), "lane": lane.clone()});
    if let Some(m) = model {
        op["model"] = json!(m);
    }
    if let Some(e) = effort {
        op["effort"] = json!(e);
    }
    wr.write_all(format!("{op}\n").as_bytes()).await.map_err(|e| e.to_string())?;
    wr.flush().await.map_err(|e| e.to_string())?;

    let token = ctx.meta.get_progress_token();
    let mut progress = 0.0f64;
    let mut lines = BufReader::new(rd).lines();
    let mut acc = String::new();

    loop {
        tokio::select! {
            _ = ctx.ct.cancelled() => {
                let stop = json!({"op":"interrupt","project":project,"lane":lane});
                let _ = wr.write_all(format!("{stop}\n").as_bytes()).await;
                let _ = wr.flush().await;
                return Err("cancelled by client".into());
            }
            next = lines.next_line() => {
                let line = match next {
                    Ok(Some(l)) => l,
                    Ok(None) => break,
                    Err(e) => return Err(e.to_string()),
                };
                if line.trim().is_empty() {
                    continue;
                }
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                match v.get("ev").and_then(|x| x.as_str()) {
                    Some("delta") => {
                        if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
                            acc.push_str(t);
                            mcp_progress(&ctx, &token, &mut progress, t.to_string()).await;
                        }
                    }
                    Some("final") => {
                        if acc.is_empty() {
                            if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
                                acc.push_str(t);
                            }
                        }
                    }
                    Some("result") => {
                        if acc.is_empty() {
                            if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
                                acc.push_str(t);
                            }
                        }
                        return Ok(acc);
                    }
                    Some("heartbeat") => {
                        mcp_progress(&ctx, &token, &mut progress, "Opus is thinking…".into()).await;
                    }
                    Some("timeout") => return Err("the turn exceeded the bridge timeout".into()),
                    Some("session_ended") => {
                        return Err("the claude session ended before completing the turn".into())
                    }
                    Some("error") => {
                        return Err(v
                            .get("text")
                            .and_then(|x| x.as_str())
                            .unwrap_or("bridge error")
                            .to_string())
                    }
                    _ => {} // init / idle_reclaim / model_switch / status: ignored
                }
            }
        }
    }
    Ok(acc)
}

/// Read the bridge health snapshot and return it as a JSON string (for the `bridge_status` tool).
async fn mcp_status(lane: Option<String>) -> Result<String, String> {
    let stream = match UnixStream::connect(sock_path()).await {
        Ok(s) => s,
        Err(_) => return Ok(json!({"alive": false}).to_string()),
    };
    let (rd, mut wr) = stream.into_split();
    let op = json!({"op":"status","project":cwd_key(None),"lane":lane_of(lane)}).to_string();
    wr.write_all(format!("{op}\n").as_bytes()).await.map_err(|e| e.to_string())?;
    wr.flush().await.map_err(|e| e.to_string())?;

    let mut lines = BufReader::new(rd).lines();
    let read = async {
        while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if v.get("ev").and_then(|x| x.as_str()) == Some("status") {
                    return Ok::<String, String>(line);
                }
            }
        }
        Err("no status event".to_string())
    };
    match tokio::time::timeout(Duration::from_secs(3), read).await {
        Ok(r) => r,
        Err(_) => Ok(json!({"alive": true, "note": "no status event (timeout)"}).to_string()),
    }
}

/// One-line, human-readable health for Claude Code's `statusLine`. Never errors, never blocks
/// long: a compact glyph + state so every Claude session can see the bridge at a glance.
async fn statusline_cmd(lane: Option<String>) -> Result<()> {
    // Ignore any context JSON Claude Code pipes in on stdin; we only report the bridge.
    let stream = match UnixStream::connect(sock_path()).await {
        Ok(s) => s,
        Err(_) => {
            println!("○ gw-bridge off");
            return Ok(());
        }
    };
    let (rd, mut wr) = stream.into_split();
    let op = json!({"op":"status","project":cwd_key(None),"lane":lane_of(lane)}).to_string();
    if wr.write_all(format!("{op}\n").as_bytes()).await.is_err() || wr.flush().await.is_err() {
        println!("○ gw-bridge off");
        return Ok(());
    }

    let mut lines = BufReader::new(rd).lines();
    let read = async {
        while let Some(line) = lines.next_line().await? {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if v.get("ev").and_then(|x| x.as_str()) == Some("status") {
                    return Ok::<Option<Value>, anyhow::Error>(Some(v));
                }
            }
        }
        Ok(None)
    };

    let snap = match tokio::time::timeout(Duration::from_secs(2), read).await {
        Ok(Ok(Some(v))) => v,
        _ => {
            println!("◐ gw-bridge up");
            return Ok(());
        }
    };

    println!("{}", format_statusline(&snap));
    Ok(())
}
