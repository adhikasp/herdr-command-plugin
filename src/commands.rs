//! The command palette: herdr's own actions and every plugin action, as rows.
//!
//! Rows are built from the same snapshot as the rest of the bar, with the
//! focused pane, tab and workspace filled into each command's arguments. A
//! chosen command runs through the Herdr CLI after the popup has closed.

use crate::items::{Item, ItemInit, KIND_COMMAND};
use crate::pyjson::{self, Value};
use std::sync::Mutex;

/// What a plugin's `action list` said about one action.
#[derive(Clone, Debug)]
pub struct PluginAction {
    pub plugin_id: String,
    pub action_id: String,
    pub title: String,
    pub description: String,
    pub contexts: Vec<String>,
}

static PLUGIN_ACTIONS: Mutex<Option<Vec<PluginAction>>> = Mutex::new(None);

/// Whether the plugin actions have been read yet (they load on a thread).
pub fn plugin_actions_ready() -> bool {
    PLUGIN_ACTIONS.lock().map(|guard| guard.is_some()).unwrap_or(false)
}

/// Read `herdr plugin action list` off the main thread: it is a CLI call,
/// and the bar should not wait on it to draw.
pub fn prefetch_plugin_actions() {
    std::thread::spawn(|| {
        let actions = load_plugin_actions().unwrap_or_default();
        if let Ok(mut guard) = PLUGIN_ACTIONS.lock() {
            *guard = Some(actions);
        }
    });
}

fn load_plugin_actions() -> Option<Vec<PluginAction>> {
    let output =
        std::process::Command::new(crate::opener::herdr_bin()).args(["plugin", "action", "list"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let parsed = pyjson::loads(&String::from_utf8_lossy(&output.stdout)).ok()?;
    let mut found = Vec::new();
    for action in parsed.get("result").get("actions").list_or_empty() {
        let text = |key: &str| action.get(key).as_str().unwrap_or("").to_string();
        let platforms: Vec<&str> = action.get("platforms").list_or_empty().iter().filter_map(|p| p.as_str()).collect();
        if !platforms.is_empty() && !platforms.contains(&std::env::consts::OS) {
            continue;
        }
        let contexts = action.get("contexts").list_or_empty().iter().filter_map(|c| c.as_str()).map(String::from).collect();
        found.push(PluginAction {
            plugin_id: text("plugin_id"),
            action_id: text("action_id"),
            title: text("title"),
            description: text("description"),
            contexts,
        });
    }
    Some(found)
}

/// The first binding of each `[keys]` entry in herdr's config, by name.
fn key_bindings() -> Vec<(String, String)> {
    let path = std::env::var_os("HERDR_CONFIG_PATH")
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config/herdr/config.toml")));
    let Some(text) = path.and_then(|p| std::fs::read_to_string(p).ok()) else { return Vec::new() };
    let mut in_keys = false;
    let mut bindings = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_keys = line == "[keys]";
            continue;
        }
        if !in_keys || line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else { continue };
        let Some(start) = value.find('"') else { continue };
        let rest = &value[start + 1..];
        let Some(end) = rest.find('"') else { continue };
        bindings.push((name.trim().to_string(), rest[..end].to_string()));
    }
    bindings
}

struct Spec {
    title: &'static str,
    category: &'static str,
    /// The `[keys]` name in herdr's config, to show the shortcut.
    key: &'static str,
    /// Arguments after `herdr`, with {pane}, {tab} and {ws} filled in.
    args: &'static [&'static str],
}

const fn spec(title: &'static str, category: &'static str, key: &'static str, args: &'static [&'static str]) -> Spec {
    Spec { title, category, key, args }
}

const SPECS: &[Spec] = &[
    spec("Split pane right", "Pane", "split_horizontal", &["pane", "split", "--pane", "{pane}", "--direction", "right", "--focus"]),
    spec("Split pane down", "Pane", "split_vertical", &["pane", "split", "--pane", "{pane}", "--direction", "down", "--focus"]),
    spec("Close pane", "Pane", "close_pane", &["pane", "close", "{pane}"]),
    spec("Toggle zoom", "Pane", "zoom", &["pane", "zoom", "--pane", "{pane}", "--toggle"]),
    spec("Focus pane left", "Pane", "focus_pane_left", &["pane", "focus", "--pane", "{pane}", "--direction", "left"]),
    spec("Focus pane down", "Pane", "focus_pane_down", &["pane", "focus", "--pane", "{pane}", "--direction", "down"]),
    spec("Focus pane up", "Pane", "focus_pane_up", &["pane", "focus", "--pane", "{pane}", "--direction", "up"]),
    spec("Focus pane right", "Pane", "focus_pane_right", &["pane", "focus", "--pane", "{pane}", "--direction", "right"]),
    spec("Swap pane left", "Pane", "swap_pane_left", &["pane", "swap", "--pane", "{pane}", "--direction", "left"]),
    spec("Swap pane down", "Pane", "swap_pane_down", &["pane", "swap", "--pane", "{pane}", "--direction", "down"]),
    spec("Swap pane up", "Pane", "swap_pane_up", &["pane", "swap", "--pane", "{pane}", "--direction", "up"]),
    spec("Swap pane right", "Pane", "swap_pane_right", &["pane", "swap", "--pane", "{pane}", "--direction", "right"]),
    spec("New tab", "Tab", "new_tab", &["tab", "create", "--workspace", "{ws}", "--focus"]),
    spec("Close tab", "Tab", "close_tab", &["tab", "close", "{tab}"]),
    spec("New workspace", "Workspace", "new_workspace", &["workspace", "create", "--focus"]),
    spec("Close workspace", "Workspace", "close_workspace", &["workspace", "close", "{ws}"]),
    spec("Reload herdr config", "Herdr", "reload_config", &["server", "reload-config"]),
];

/// Ids of the focused pane, tab and workspace, plus where next/previous go.
struct Focus {
    pane: String,
    tab: String,
    ws: String,
    next_tab: Option<String>,
    previous_tab: Option<String>,
    next_ws: Option<String>,
    previous_ws: Option<String>,
}

fn neighbours(ids: &[String], current: &str) -> (Option<String>, Option<String>) {
    let Some(at) = ids.iter().position(|id| id == current) else { return (None, None) };
    if ids.len() < 2 {
        return (None, None);
    }
    (Some(ids[(at + 1) % ids.len()].clone()), Some(ids[(at + ids.len() - 1) % ids.len()].clone()))
}

fn focus_of(snapshot: &Value) -> Focus {
    let text = |key: &str| snapshot.get(key).as_str().unwrap_or("").to_string();
    let (pane, tab, ws) = (text("focused_pane_id"), text("focused_tab_id"), text("focused_workspace_id"));
    let tabs: Vec<String> = snapshot
        .get("tabs")
        .list_or_empty()
        .iter()
        .filter(|t| t.get("workspace_id").as_str() == Some(ws.as_str()))
        .filter_map(|t| t.get("tab_id").as_str().map(String::from))
        .collect();
    let spaces: Vec<String> =
        snapshot.get("workspaces").list_or_empty().iter().filter_map(|w| w.get("workspace_id").as_str().map(String::from)).collect();
    let (next_tab, previous_tab) = neighbours(&tabs, &tab);
    let (next_ws, previous_ws) = neighbours(&spaces, &ws);
    Focus { pane, tab, ws, next_tab, previous_tab, next_ws, previous_ws }
}

fn command_item(id: &str, title: &str, category: &str, hint: &str, search: &[String], argv: Vec<String>) -> Item {
    let mut item = Item::new(ItemInit {
        kind: KIND_COMMAND,
        key: format!("cmd:{}", id),
        title: title.to_string(),
        detail: hint.to_string(),
        subtitle: category.to_string(),
        workspace_id: String::new(),
        workspace_label: String::new(),
        tab_id: None,
        pane_id: None,
        agent: None,
        agent_name: None,
        status: "none".to_string(),
        tab_number: None,
        focused: false,
        extra_terms: search.to_vec(),
    });
    item.command = Some(argv);
    item
}

/// A herdr command row with its key binding: shown at the row's right, but not
/// searched, since "prefix+v" would make every "p" or "pi" hit half the palette.
fn bound_command_item(id: &str, title: &str, category: &str, binding: &str, argv: Vec<String>) -> Item {
    let mut item = command_item(id, title, category, "", &[], argv);
    item.binding = binding.to_string();
    item
}

pub fn build_items(snapshot: &Value) -> Vec<Item> {
    let focus = focus_of(snapshot);
    let bindings = key_bindings();
    let hint_for = |name: &str| bindings.iter().find(|(key, _)| key == name).map(|(_, v)| v.clone()).unwrap_or_default();
    let fill = |arg: &str| -> String {
        arg.replace("{pane}", &focus.pane).replace("{tab}", &focus.tab).replace("{ws}", &focus.ws)
    };
    let mut items = Vec::new();
    for spec in SPECS {
        if spec.args.iter().any(|a| a.contains("{pane}")) && focus.pane.is_empty() {
            continue;
        }
        if spec.args.iter().any(|a| a.contains("{tab}")) && focus.tab.is_empty() {
            continue;
        }
        if spec.args.iter().any(|a| a.contains("{ws}")) && focus.ws.is_empty() {
            continue;
        }
        let argv = spec.args.iter().map(|a| fill(a)).collect();
        items.push(bound_command_item(spec.key, spec.title, spec.category, &hint_for(spec.key), argv));
    }
    let jumps = [
        ("Next tab", "Tab", "next_tab", "tab", &focus.next_tab),
        ("Previous tab", "Tab", "previous_tab", "tab", &focus.previous_tab),
        ("Next workspace", "Workspace", "next_workspace", "workspace", &focus.next_ws),
        ("Previous workspace", "Workspace", "previous_workspace", "workspace", &focus.previous_ws),
    ];
    for (title, category, key, noun, target) in jumps {
        if let Some(target) = target {
            let argv = vec![noun.to_string(), "focus".to_string(), target.clone()];
            items.push(bound_command_item(key, title, category, &hint_for(key), argv));
        }
    }
    let plugin = PLUGIN_ACTIONS.lock().ok().and_then(|guard| guard.clone()).unwrap_or_default();
    for action in plugin {
        // The bar's own entry would just reopen the bar inside itself.
        if action.plugin_id == "herdr-command-plugin" && action.action_id == "open" {
            continue;
        }
        let id = format!("{}.{}", action.plugin_id, action.action_id);
        let title = if action.title.is_empty() { action.action_id.clone() } else { action.title.clone() };
        let argv = vec!["plugin".into(), "action".into(), "invoke".into(), id.clone()];
        let search = vec![action.plugin_id.clone(), action.description.clone()];
        let context = action.contexts.first().cloned().unwrap_or_default();
        let _ = context;
        items.push(command_item(&id, &title, &action.plugin_id, &action.description, &search, argv));
    }
    items
}

/// Run a chosen command with the Herdr CLI and report how it went.
pub fn run(item: &Item) -> Result<(), String> {
    let Some(argv) = item.command.as_ref() else { return Ok(()) };
    let output = std::process::Command::new(crate::opener::herdr_bin())
        .args(argv)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("could not run herdr: {}", error))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let message = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
    Err(format!("{}: {}", item.title, message.lines().next().unwrap_or("failed")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv() -> Vec<String> {
        vec!["pane".into(), "zoom".into()]
    }

    #[test]
    fn a_key_binding_is_shown_but_not_searched() {
        let item = bound_command_item("zoom", "Toggle zoom", "Pane", "prefix+z", argv());
        assert_eq!(item.binding, "prefix+z");
        assert!(item.detail.is_empty());
        assert!(!item.fields.iter().any(|f| f == "prefix+z"));
        assert_eq!(item.detail_field, -1);
        assert_eq!(item.fields.len(), item.prepared.len());
    }

    #[test]
    fn a_plugin_description_stays_searchable() {
        let search = vec!["herdr-command-plugin".to_string(), "Start the tab watcher".to_string()];
        let item = command_item("herdr-command-plugin.start", "Start titles", "herdr-command-plugin", "Start the tab watcher", &search, argv());
        assert!(item.fields.iter().any(|f| f == "Start the tab watcher"));
        assert!(item.detail_field >= 0);
    }

    #[test]
    fn commands_fill_in_the_focused_pane() {
        let snapshot = pyjson::loads(
            r#"{"focused_pane_id": "w1:p2", "focused_tab_id": "w1:t1", "focused_workspace_id": "w1",
                "workspaces": [{"workspace_id": "w1"}], "tabs": [{"tab_id": "w1:t1", "workspace_id": "w1"}]}"#,
        )
        .unwrap();
        let items = build_items(&snapshot);
        let zoom = items.iter().find(|i| i.title == "Toggle zoom").unwrap();
        assert_eq!(zoom.command.as_ref().unwrap(), &["pane", "zoom", "--pane", "w1:p2", "--toggle"]);
        // One tab and one workspace: there is nowhere for next or previous to go.
        assert!(!items.iter().any(|i| i.title == "Next tab" || i.title == "Next workspace"));
    }
}
