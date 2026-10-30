//! Rows from the saved SSH machines.
//!
//! Herdr only answers for its own server, so each enabled machine is asked
//! through `herdr --machine <label> api snapshot`, which takes a couple of
//! seconds. A background thread keeps the latest rows; the bar picks them up
//! on its next refresh instead of waiting for them to open. The daemon asks a
//! machine when its event stream (see `machine_events`) says something changed,
//! and polls only the machines that have no stream.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::client::HerdrClient;
use crate::items::{build_items, Item};
use crate::machine_events::{Signal, Watcher};
use crate::opener::herdr_bin;
use crate::pyjson::{self, Value};

/// How often a machine with no event stream is asked.
const POLL_SECONDS: f64 = 4.0;
/// How often a machine with a live event stream is asked anyway, in case an
/// event was lost.
const HEARTBEAT_SECONDS: f64 = 60.0;
/// How long to let a burst of events settle before asking.
const DEBOUNCE_SECONDS: f64 = 0.25;
/// How often the saved machines are listed again.
const LIST_SECONDS: f64 = 10.0;
const MACHINE_TIMEOUT: f64 = 20.0;

#[derive(Default)]
struct Latest {
    version: u64,
    signature: String,
    items: Vec<Item>,
}

#[derive(Clone, Default)]
pub struct Remote {
    latest: Arc<Mutex<Latest>>,
}

/// Each machine's last snapshot, in the order the machines were listed.
type Snapshots = Vec<(String, Value)>;

fn cache_path() -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("HERDR_PLUGIN_STATE_DIR").filter(|v| !v.is_empty())?;
    Some(std::path::Path::new(&dir).join("remote.json"))
}

fn load_cache() -> Snapshots {
    let Some(path) = cache_path() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let Ok(Value::List(entries)) = pyjson::loads(&text) else { return Vec::new() };
    entries
        .iter()
        .filter_map(|e| Some((e.get("machine").as_str()?.to_string(), e.get("snapshot").clone())))
        .filter(|(_, snapshot)| snapshot.as_dict().is_some())
        .collect()
}

fn save_cache(snapshots: &Snapshots) {
    let Some(path) = cache_path() else { return };
    let entries = snapshots
        .iter()
        .map(|(machine, snapshot)| {
            crate::client::dict(&[("machine", Value::str(machine.clone())), ("snapshot", snapshot.clone())])
        })
        .collect();
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, pyjson::dumps(&Value::List(entries), false)).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn items_of(snapshots: &Snapshots) -> Vec<Item> {
    let mut all = Vec::new();
    for (machine, snapshot) in snapshots {
        let mut items = build_items(snapshot);
        for item in items.iter_mut() {
            item.on_machine(machine);
            // Only the machine you are on can be "here".
            item.focused = false;
        }
        all.extend(items);
    }
    all
}

fn publish(latest: &Mutex<Latest>, snapshots: &Snapshots) {
    let items = items_of(snapshots);
    let signature = items.iter().map(|i| format!("{}\t{}\t{}", i.key, i.title, i.status)).collect::<Vec<_>>().join("\n");
    if let Ok(mut latest) = latest.lock() {
        if latest.signature != signature || latest.version == 0 {
            latest.signature = signature;
            latest.items = items;
            latest.version += 1;
        }
    }
}

impl Remote {
    /// Show what the daemon last saved at once, and follow the file as the
    /// daemon rewrites it. Starts the daemon if it isn't running.
    pub fn start() -> Remote {
        ensure_daemon();
        let remote = Remote::default();
        publish(&remote.latest, &load_cache());
        let latest = remote.latest.clone();
        std::thread::spawn(move || {
            let mut seen = cache_stamp();
            loop {
                crate::client::sleep(1.0);
                let stamp = cache_stamp();
                if stamp != seen {
                    seen = stamp;
                    publish(&latest, &load_cache());
                }
            }
        });
        remote
    }

    /// The rows and their version, which changes whenever the rows do.
    pub fn rows(&self) -> (u64, Vec<Item>) {
        match self.latest.lock() {
            Ok(latest) => (latest.version, latest.items.clone()),
            Err(_) => (0, Vec::new()),
        }
    }

    pub fn version(&self) -> u64 {
        self.latest.lock().map(|l| l.version).unwrap_or(0)
    }
}

fn cache_stamp() -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(cache_path()?).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

fn lock_path() -> Option<std::path::PathBuf> {
    Some(cache_path()?.with_file_name("machines.lock"))
}

/// Start the daemon unless one already holds the lock.
pub fn ensure_daemon() {
    let Some(path) = lock_path() else { return };
    let Some(file) = path.parent().map(|dir| std::fs::create_dir_all(dir)).and_then(|_| crate::host::open_append(&path).ok()) else {
        return;
    };
    if crate::host::lock(&file, false).is_err() {
        return;
    }
    crate::host::unlock(&file);
    drop(file);
    let Ok(exe) = std::env::current_exe() else { return };
    let mut command = std::process::Command::new(exe);
    command.arg("--watch-machines").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = command.spawn();
}

/// The daemon: keep the cache fresh until the herdr server goes away.
pub fn run_daemon() -> i32 {
    let Some(path) = lock_path() else { return 1 };
    let Ok(lock) = crate::host::open_append(&path) else { return 1 };
    if crate::host::lock(&lock, false).is_err() {
        return 0;
    }
    let socket = std::env::var_os("HERDR_SOCKET_PATH").map(std::path::PathBuf::from).filter(|p| p.is_absolute());
    let mut snapshots = load_cache();
    let (signals, inbox) = std::sync::mpsc::channel();
    let mut states: HashMap<String, State> = HashMap::new();
    loop {
        if cfg!(unix) && socket.as_ref().is_some_and(|socket| !socket.exists()) {
            return 0;
        }
        let machines = enabled_machines();
        // One event stream per machine; a machine no longer enabled drops its.
        states.retain(|label, state| machines.iter().any(|m| m.label == *label && m.key() == state.key));
        for machine in &machines {
            states.entry(machine.label.clone()).or_insert_with(|| State {
                key: machine.key(),
                watcher: Watcher::spawn(&machine.label, &machine.target, &machine.session, signals.clone()),
                live: false,
                dirty_since: None,
                fetched: None,
            });
        }
        let now = Instant::now();
        let due: Vec<&Machine> = machines.iter().filter(|m| states.get(&m.label).is_some_and(|s| s.due(now))).collect();
        let fresh = fetch(&due);
        for (label, snapshot) in &fresh {
            if let Some(state) = states.get_mut(label) {
                state.fetched = Some(now);
                state.dirty_since = None;
                if let Some(snapshot) = snapshot {
                    state.watcher.follow_panes(agent_panes(snapshot));
                }
            }
        }
        // A machine that didn't answer keeps its last rows rather than
        // vanishing for a moment; one no longer enabled drops out.
        if !machines.is_empty() {
            let mut next: Snapshots = Vec::new();
            for machine in &machines {
                match fresh.iter().find(|(label, _)| *label == machine.label) {
                    Some((_, Some(snapshot))) => next.push((machine.label.clone(), snapshot.clone())),
                    _ => next.extend(snapshots.iter().find(|(m, _)| *m == machine.label).cloned()),
                }
            }
            if next != snapshots {
                save_cache(&next);
            }
            snapshots = next;
        }
        // Sleep until the next machine is due or a stream speaks up.
        let now = Instant::now();
        let wait = states.values().map(|s| s.wait(now)).fold(LIST_SECONDS, f64::min).max(0.05);
        if let Ok(first) = inbox.recv_timeout(Duration::from_secs_f64(wait)) {
            apply(&mut states, first);
            // Events arrive in bursts; let the burst finish, then fetch once.
            crate::client::sleep(DEBOUNCE_SECONDS);
            while let Ok(next) = inbox.try_recv() {
                apply(&mut states, next);
            }
        }
    }
}

struct Machine {
    label: String,
    target: String,
    session: String,
}

impl Machine {
    fn key(&self) -> String {
        format!("{}\t{}", self.target, self.session)
    }
}

struct State {
    key: String,
    watcher: Watcher,
    live: bool,
    dirty_since: Option<Instant>,
    fetched: Option<Instant>,
}

impl State {
    /// Seconds until this machine should be asked again.
    fn wait(&self, now: Instant) -> f64 {
        let Some(fetched) = self.fetched else { return 0.0 };
        if self.dirty_since.is_some() {
            return 0.0;
        }
        let every = if self.live { HEARTBEAT_SECONDS } else { POLL_SECONDS };
        (every - now.saturating_duration_since(fetched).as_secs_f64()).max(0.0)
    }

    fn due(&self, now: Instant) -> bool {
        self.wait(now) <= 0.0
    }
}

fn apply(states: &mut HashMap<String, State>, signal: Signal) {
    match signal {
        Signal::Event(label) => {
            if let Some(state) = states.get_mut(&label) {
                state.dirty_since.get_or_insert_with(Instant::now);
            }
        }
        Signal::Live(label, live) => {
            if let Some(state) = states.get_mut(&label) {
                // Whatever happened while the stream was down is unknown. A
                // stream that never came up has nothing to catch up on.
                if live || state.live {
                    state.dirty_since.get_or_insert_with(Instant::now);
                }
                state.live = live;
            }
        }
    }
}

/// Panes running an agent: the ones whose status changes are worth a wake-up.
fn agent_panes(snapshot: &Value) -> Vec<String> {
    snapshot
        .get("agents")
        .list_or_empty()
        .iter()
        .filter_map(|agent| agent.get("pane_id").as_str().map(str::to_string))
        .collect()
}

fn enabled_machines() -> Vec<Machine> {
    let argv = vec![herdr_bin(), "machine".to_string(), "list".to_string(), "--json".to_string()];
    let Some(output) = crate::proc::run_capture(&argv, MACHINE_TIMEOUT) else { return Vec::new() };
    let Ok(Value::List(machines)) = pyjson::loads(&String::from_utf8_lossy(&output.stdout)) else { return Vec::new() };
    machines
        .iter()
        .filter(|m| m.get("enabled").is_true())
        .filter_map(|m| {
            let label = m.get("label").as_str()?.to_string();
            let target = m.get("target").as_str().unwrap_or(&label).to_string();
            let session = m.get("session").as_str().unwrap_or("default").to_string();
            Some(Machine { label, target, session })
        })
        .collect()
}

fn fetch_machine(label: &str) -> Option<Value> {
    let client = HerdrClient::new(None, MACHINE_TIMEOUT, false);
    let args: Vec<String> = ["--machine", label, "api", "snapshot"].iter().map(|s| s.to_string()).collect();
    client.cli(&args).and_then(HerdrClient::unwrap).and_then(HerdrClient::snapshot_of).ok()
}

fn fetch(machines: &[&Machine]) -> Vec<(String, Option<Value>)> {
    let handles: Vec<_> = machines
        .iter()
        .map(|m| m.label.clone())
        .map(|label| std::thread::spawn(move || (label.clone(), fetch_machine(&label))))
        .collect();
    handles.into_iter().filter_map(|h| h.join().ok()).collect()
}

/// Each machine's workspace ids, in the order the sidebar lists them.
fn workspace_order() -> Vec<(String, Vec<String>)> {
    load_cache()
        .into_iter()
        .map(|(machine, snapshot)| {
            let ids = snapshot
                .get("workspaces")
                .list_or_empty()
                .iter()
                .filter_map(|w| w.get("workspace_id").as_str().map(str::to_string))
                .collect();
            (machine, ids)
        })
        .collect()
}

/// Herdr has no call that switches the window to another machine, so press
/// the keys that do (`prefix+w`, move to the workspace, Enter) once the popup
/// has closed. Assumes the sidebar lists Local first and that Local is what
/// you are viewing now. Best effort: needs `wtype`, on a Wayland desktop.
pub fn show_machine(machine: &str, workspace_id: &str, local_workspaces: &[String], local_focused: &str, prefix: &str) {
    let here = local_workspaces.iter().position(|id| id == local_focused).unwrap_or(0) as i64;
    let mut before = local_workspaces.len() as i64;
    let mut target = None;
    for (name, ids) in workspace_order() {
        if name == machine {
            target = ids.iter().position(|id| id == workspace_id).map(|at| before + at as i64);
            break;
        }
        before += ids.len() as i64;
    }
    let Some(target) = target else { return };
    let steps = target - here;
    let mut keys: Vec<String> = vec!["-s".into(), "500".into()];
    // "ctrl+space" -> -M ctrl -k space -m ctrl
    let mut parts: Vec<&str> = prefix.split('+').collect();
    let key = parts.pop().unwrap_or("space");
    for modifier in &parts {
        keys.extend(["-M".to_string(), modifier.to_string()]);
    }
    keys.extend(["-k".to_string(), key.to_string()]);
    for modifier in parts.iter().rev() {
        keys.extend(["-m".to_string(), modifier.to_string()]);
    }
    keys.extend(["-s".into(), "150".into(), "w".into()]);
    let direction = if steps > 0 { "Down" } else { "Up" };
    for _ in 0..steps.abs() {
        keys.extend(["-s".into(), "60".into(), "-k".into(), direction.into()]);
    }
    keys.extend(["-s".into(), "120".into(), "-k".into(), "Return".into()]);
    // Its own session, so closing the popup doesn't take it down mid-wait.
    let mut command = std::process::Command::new("wtype");
    command
        .args(&keys)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = command.spawn();
}

/// Focus things on a machine after this process is gone: one detached shell
/// runs the `herdr --machine` commands in order.
pub fn focus_in_background(herdr: &str, machine: &str, steps: &[(&str, &str)]) {
    let mut command = std::process::Command::new("sh");
    let mut body = String::new();
    let mut args: Vec<String> = Vec::new();
    for (index, (kind, id)) in steps.iter().enumerate() {
        // $1 is herdr, $2 the machine; each step brings its own kind and id.
        body.push_str(&format!("\"$1\" --machine \"$2\" \"${}\" focus \"${}\"; ", 3 + index * 2, 4 + index * 2));
        args.push(kind.to_string());
        args.push(id.to_string());
    }
    command
        .arg("-c")
        .arg(body)
        .arg("herdr-focus")
        .arg(herdr)
        .arg(machine)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = command.spawn();
}

/// `keys.prefix` from herdr's config.toml, or herdr's default.
pub fn prefix_key() -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    let base = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from).unwrap_or_else(|| home.join(".config"));
    let text = std::fs::read_to_string(base.join("herdr").join("config.toml")).unwrap_or_default();
    let mut in_keys = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_keys = line == "[keys]";
        } else if in_keys {
            if let Some(value) = line.strip_prefix("prefix").map(str::trim_start).and_then(|r| r.strip_prefix('=')) {
                let value = value.trim().trim_matches('"');
                if !value.is_empty() && !value.starts_with('[') {
                    return value.to_string();
                }
            }
        }
    }
    "ctrl+b".to_string()
}
