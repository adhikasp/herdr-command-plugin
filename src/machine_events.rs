//! Wake-ups from the saved SSH machines.
//!
//! Asking a machine for its snapshot costs a few SSH logins, so the machines
//! daemon should not do it on a timer. Instead each machine gets one
//! long-lived `ssh <target> herdr remote-api-bridge` carrying an
//! `events.subscribe`, and the daemon fetches a snapshot only when an event
//! arrives. As with the local `EventStream`, the events are never interpreted:
//! they only say "look again", so a lost one costs latency, not accuracy.
//!
//! A machine whose bridge cannot be opened (an older herdr, a Windows host
//! whose login shell is not POSIX) simply never reports itself live, and the
//! daemon keeps polling it.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::client::{dict, sleep};
use crate::pyjson::{self, Value};

const RETRY_MIN: f64 = 2.0;
const RETRY_MAX: f64 = 60.0;

/// Events worth a fresh snapshot. `pane.agent_status_changed` needs a pane id
/// and is added per agent pane by `subscriptions`.
const EVENTS: &[&str] = &[
    "workspace.created",
    "workspace.updated",
    "workspace.renamed",
    "workspace.moved",
    "workspace.reordered",
    "workspace.closed",
    "workspace.focused",
    "tab.created",
    "tab.closed",
    "tab.focused",
    "tab.renamed",
    "tab.moved",
    "pane.created",
    "pane.closed",
    "pane.updated",
    "pane.focused",
    "pane.moved",
    "pane.exited",
    "pane.agent_detected",
];

pub enum Signal {
    /// Something changed on this machine.
    Event(String),
    /// The machine's stream came up (true) or went down (false).
    Live(String, bool),
}

pub struct Watcher {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    /// Agent panes the next connection should follow.
    want: Arc<Mutex<Vec<String>>>,
    /// Agent panes the current connection follows.
    used: Arc<Mutex<Vec<String>>>,
}

impl Watcher {
    pub fn spawn(label: &str, target: &str, session: &str, signals: Sender<Signal>) -> Watcher {
        let watcher = Watcher {
            stop: Arc::new(AtomicBool::new(false)),
            child: Arc::new(Mutex::new(None)),
            want: Arc::new(Mutex::new(Vec::new())),
            used: Arc::new(Mutex::new(Vec::new())),
        };
        let link = Link {
            label: label.to_string(),
            target: target.to_string(),
            session: session.to_string(),
            stop: watcher.stop.clone(),
            child: watcher.child.clone(),
            want: watcher.want.clone(),
            used: watcher.used.clone(),
            signals,
        };
        std::thread::spawn(move || link.run());
        watcher
    }

    /// Follow these agent panes; reconnects if the current stream does not.
    pub fn follow_panes(&self, mut panes: Vec<String>) {
        panes.sort();
        panes.dedup();
        let changed = {
            let mut want = self.want.lock().unwrap();
            let changed = *want != panes;
            *want = panes;
            changed
        };
        if changed && *self.used.lock().unwrap() != *self.want.lock().unwrap() {
            self.hang_up();
        }
    }

    fn hang_up(&self) {
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.hang_up();
    }
}

struct Link {
    label: String,
    target: String,
    session: String,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    want: Arc<Mutex<Vec<String>>>,
    used: Arc<Mutex<Vec<String>>>,
    signals: Sender<Signal>,
}

impl Link {
    fn run(self) {
        let mut backoff = RETRY_MIN;
        while !self.stop.load(Ordering::SeqCst) {
            let wanted = self.want.lock().unwrap().clone();
            let held = self.connect(&wanted);
            let _ = self.signals.send(Signal::Live(self.label.clone(), false));
            let wait = if held {
                backoff = RETRY_MIN;
                // Dropped on purpose to follow other panes: straight back.
                if *self.want.lock().unwrap() != wanted { 0.0 } else { RETRY_MIN }
            } else {
                backoff = (backoff * 2.0).min(RETRY_MAX);
                backoff
            };
            let until = Instant::now() + Duration::from_secs_f64(wait);
            while Instant::now() < until && !self.stop.load(Ordering::SeqCst) {
                sleep(0.25);
            }
        }
    }

    /// One connection, until it drops. True if the subscription was accepted.
    fn connect(&self, panes: &[String]) -> bool {
        let remote = format!(
            "PATH=\"$HOME/.local/bin:$PATH\" exec herdr --session {} remote-api-bridge",
            shell_quote(&self.session)
        );
        let mut command = Command::new("ssh");
        command
            .args(["-o", "BatchMode=yes", "-o", "NumberOfPasswordPrompts=0", "-o", "ConnectTimeout=10"])
            .args(["-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3", "-T"])
            .arg(&self.target)
            .arg(remote)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let Ok(mut child) = command.spawn() else { return false };
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        };
        *self.child.lock().unwrap() = Some(child);
        let held = self.listen(&mut stdin, stdout, panes);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        held
    }

    fn listen(&self, stdin: &mut std::process::ChildStdin, stdout: std::process::ChildStdout, panes: &[String]) -> bool {
        let mut payload = pyjson::dumps(&subscribe_request(panes), false).into_bytes();
        payload.push(b'\n');
        if stdin.write_all(&payload).and_then(|_| stdin.flush()).is_err() {
            return false;
        }
        let mut accepted = false;
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            // Anything before the JSON (a banner, the readiness marker) is not ours.
            let line = line.trim();
            if !line.starts_with('{') {
                continue;
            }
            let Ok(message) = pyjson::loads(line) else { continue };
            if accepted {
                let _ = self.signals.send(Signal::Event(self.label.clone()));
            } else if message.get("id").as_str() == Some("bar-events") {
                if message.get("result").get("type").as_str() != Some("subscription_started") {
                    return false;
                }
                accepted = true;
                *self.used.lock().unwrap() = panes.to_vec();
                let _ = self.signals.send(Signal::Live(self.label.clone(), true));
            }
        }
        accepted
    }
}

fn subscribe_request(panes: &[String]) -> Value {
    let mut subscriptions: Vec<Value> = EVENTS.iter().map(|kind| dict(&[("type", Value::str(*kind))])).collect();
    for pane in panes {
        subscriptions.push(dict(&[
            ("type", Value::str("pane.agent_status_changed")),
            ("pane_id", Value::str(pane.clone())),
        ]));
    }
    dict(&[
        ("id", Value::str("bar-events")),
        ("method", Value::str("events.subscribe")),
        ("params", dict(&[("subscriptions", Value::List(subscriptions))])),
    ])
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_a_quote() {
        assert_eq!(shell_quote("default"), "'default'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn request_follows_each_agent_pane() {
        let text = pyjson::dumps(&subscribe_request(&["w1:p2".to_string()]), false);
        assert!(text.contains("\"workspace.created\""));
        assert!(text.contains("\"pane_id\": \"w1:p2\"") || text.contains("\"pane_id\":\"w1:p2\""));
        assert!(text.contains("\"id\": \"bar-events\"") || text.contains("\"id\":\"bar-events\""));
    }
}
