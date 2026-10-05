//! Unit host: addressable agents that outlive a single client connection.
//!
//! Today the RPC loop (`pi_rpc::serve_session`) is bound to one reader and
//! one writer. The host keeps that loop but gives each unit channel-backed I/O:
//!
//! * commands are written into an in-memory channel (any number of clients);
//! * events are parsed off the unit's writer and fanned out to subscribers;
//! * a bounded replay buffer lets a late subscriber catch up;
//! * dropping the command channel ends the unit thread and releases its agent,
//!   so an idle unit costs nothing and wakes by replaying its session file.
//!
//! This is deliberately transport-free. The HTTP/SSE gateway (VED-341) and the
//! trigger engine (VED-343) build on this.

use pi_agent::Agent;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How many recent events a late subscriber can replay.
pub const REPLAY_LIMIT: usize = 1024;

/// How many events a live subscriber may lag behind before it is dropped.
pub const SUBSCRIBER_QUEUE: usize = 256;

#[derive(Debug)]
pub enum HostError {
    UnknownSession(String),
    Io(String),
    Json(String),
    Stopped,
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::UnknownSession(id) => write!(f, "unknown session: {id}"),
            HostError::Io(message) => write!(f, "io: {message}"),
            HostError::Json(message) => write!(f, "json: {message}"),
            HostError::Stopped => write!(f, "unit is suspended"),
        }
    }
}

impl std::error::Error for HostError {}

/// State shared between a unit's writer thread and its subscribers.
#[derive(Default)]
struct Shared {
    subscribers: Mutex<Vec<SyncSender<Value>>>,
    replay: Mutex<VecDeque<Value>>,
    /// Most recent event type and its unix-seconds timestamp, for the swarm view.
    last_event: Mutex<Option<String>>,
    last_event_at: Mutex<i64>,
    /// Session display name, cached against the session file's mtime.
    name_cache: Mutex<Option<(u64, Option<String>)>>,
    /// Unix-seconds of the last command or event, used by the idle reaper.
    last_activity: AtomicI64,
}

impl Shared {
    fn touch(&self) {
        self.last_activity.store(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs() as i64)
                .unwrap_or(0),
            Ordering::Relaxed,
        );
    }

    fn idle_for(&self, now: i64) -> i64 {
        now - self.last_activity.load(Ordering::Relaxed)
    }

    fn publish(&self, event: Value) {
        self.touch();
        if let Some(kind) = event.get("type").and_then(Value::as_str) {
            if let Ok(mut last) = self.last_event.lock() {
                *last = Some(kind.to_string());
            }
            if let Ok(mut at) = self.last_event_at.lock() {
                *at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_secs() as i64)
                    .unwrap_or(0);
            }
        }
        // Hold both locks so a subscribe either sees the event in its replay or
        // receives it live, never both and never neither.
        let mut replay = self
            .replay
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if replay.len() == REPLAY_LIMIT {
            replay.pop_front();
        }
        replay.push_back(event.clone());
        // Hold the replay lock while registering/sending so a concurrent
        // subscribe() cannot both snapshot this event and receive it live.
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // A full queue means the subscriber is not keeping up: drop it rather
        // than buffering without bound. It can re-subscribe and replay.
        subscribers.retain(|sender| sender.try_send(event.clone()).is_ok());
    }

    fn subscribe(&self) -> (Vec<Value>, Receiver<Value>) {
        let replay = self
            .replay
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (tx, rx) = mpsc::sync_channel(SUBSCRIBER_QUEUE);
        subscribers.push(tx);
        (replay.iter().cloned().collect(), rx)
    }

    fn last_event(&self) -> Option<String> {
        self.last_event.lock().ok().and_then(|last| last.clone())
    }

    fn last_event_at(&self) -> i64 {
        self.last_event_at.lock().map(|at| *at).unwrap_or(0)
    }

    fn subscriber_count(&self) -> usize {
        self.subscribers.lock().map(|subs| subs.len()).unwrap_or(0)
    }

    /// The session's display name, re-read only when the file changes. The
    /// swarm dashboard polls this, so parsing the whole JSONL every time is
    /// avoided for long sessions.
    fn session_name(&self, path: &std::path::Path) -> Option<String> {
        let mtime = std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0);
        if let Ok(cache) = self.name_cache.lock() {
            if let Some((cached_mtime, name)) = cache.as_ref() {
                if *cached_mtime == mtime {
                    return name.clone();
                }
            }
        }
        let name = pi_session::SessionFile::read(path)
            .ok()
            .and_then(|session| session.name().map(str::to_string));
        if let Ok(mut cache) = self.name_cache.lock() {
            *cache = Some((mtime, name.clone()));
        }
        name
    }
}

/// A `Read` backed by a command channel. EOF (sender dropped) ends the unit.
struct ChannelReader {
    rx: Receiver<Vec<u8>>,
    buffer: Vec<u8>,
    position: usize,
}

impl ChannelReader {
    fn new(rx: Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            buffer: Vec::new(),
            position: 0,
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.position >= self.buffer.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.buffer = chunk;
                    self.position = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let available = self.buffer.len() - self.position;
        let n = available.min(out.len());
        out[..n].copy_from_slice(&self.buffer[self.position..self.position + n]);
        self.position += n;
        Ok(n)
    }
}

/// A `Write` that parses each JSON line and fans it out.
struct ChannelWriter {
    shared: Arc<Shared>,
    buffer: Vec<u8>,
}

impl ChannelWriter {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            buffer: Vec::new(),
        }
    }

    fn drain_lines(&mut self) {
        while let Some(position) = self.buffer.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=position).collect();
            let text = String::from_utf8_lossy(&line[..line.len().saturating_sub(1)]);
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(text) {
                self.shared.publish(value);
            }
        }
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        self.drain_lines();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.drain_lines();
        Ok(())
    }
}

/// A handle attached to one running unit.
pub struct Subscription {
    /// Events published before this subscription attached, oldest first.
    pub replay: Vec<Value>,
    rx: Receiver<Value>,
}

/// Outcome of [`Subscription::recv_timeout`].
#[derive(Debug)]
pub enum RecvError {
    /// No event arrived before the timeout elapsed; the unit is still alive.
    Timeout,
    /// The unit was suspended and dropped this subscriber (or the subscriber
    /// lagged and was dropped); it will never produce another event.
    Disconnected,
}

impl Subscription {
    /// Receive the next event, waiting up to `timeout`.
    ///
    /// A timeout and a disconnected unit are distinguished so callers (the SSE
    /// loop in particular) can close instead of heartbeating a dead unit.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Value, RecvError> {
        match self.rx.recv_timeout(timeout) {
            Ok(value) => Ok(value),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(RecvError::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(RecvError::Disconnected),
        }
    }

    /// Receive without blocking. `None` means no event is ready *right now*.
    pub fn try_recv(&self) -> Option<Value> {
        self.rx.try_recv().ok()
    }
}

type Factory = Arc<dyn Fn(&str) -> Agent + Send + Sync>;

/// One addressable agent.
struct Unit {
    session_path: PathBuf,
    cwd: String,
    factory: Factory,
    shared: Arc<Shared>,
    commands: Option<Sender<Vec<u8>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Unit {
    fn spawn(session_path: PathBuf, cwd: String, factory: Factory, shared: Arc<Shared>) -> Self {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let writer = ChannelWriter::new(shared.clone());
        let path = session_path.clone();
        let unit_cwd = cwd.clone();
        let agent_factory = factory.clone();
        let thread = std::thread::spawn(move || {
            let mut agent = agent_factory(&unit_cwd);
            let reader = BufReader::new(ChannelReader::new(rx));
            let _ =
                pi_rpc::serve_session(&mut agent, Some(path), &unit_cwd, reader, writer, |_| {});
        });
        shared.touch();
        Self {
            session_path,
            cwd,
            factory,
            shared,
            commands: Some(tx),
            thread: Some(thread),
        }
    }

    fn send(&mut self, command: &Value) -> Result<(), HostError> {
        self.shared.touch();
        if self.commands.is_none() {
            self.wake();
        }
        let mut line =
            serde_json::to_vec(command).map_err(|error| HostError::Json(error.to_string()))?;
        line.push(b'\n');
        self.commands
            .as_ref()
            .ok_or(HostError::Stopped)?
            .send(line)
            .map_err(|_| HostError::Stopped)
    }

    fn wake(&mut self) {
        if self.commands.is_some() {
            return;
        }
        self.shared.touch();
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let writer = ChannelWriter::new(self.shared.clone());
        let path = self.session_path.clone();
        let cwd = self.cwd.clone();
        let factory = self.factory.clone();
        self.thread = Some(std::thread::spawn(move || {
            let mut agent = factory(&cwd);
            let reader = BufReader::new(ChannelReader::new(rx));
            let _ = pi_rpc::serve_session(&mut agent, Some(path), &cwd, reader, writer, |_| {});
        }));
        self.commands = Some(tx);
    }

    fn suspend(&mut self) {
        // Dropping the sender gives the reader EOF, so the unit thread exits and
        // its agent is released. The session file is the durable state.
        self.commands = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    fn is_running(&self) -> bool {
        self.commands.is_some()
    }

    fn subscribe(&self) -> Subscription {
        let (replay, rx) = self.shared.subscribe();
        Subscription { replay, rx }
    }
}

/// A snapshot of one live unit, for the swarm view.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnitInfo {
    pub session_id: String,
    pub name: Option<String>,
    pub cwd: String,
    pub session_path: String,
    pub running: bool,
    pub last_event: Option<String>,
    pub last_event_at: i64,
    pub subscribers: usize,
}

/// Owns a set of units keyed by session id.
pub struct Host {
    cwd: String,
    factory: Factory,
    units: HashMap<String, Unit>,
}

impl Host {
    pub fn new(
        cwd: impl Into<String>,
        factory: impl Fn(&str) -> Agent + Send + Sync + 'static,
    ) -> Self {
        Self {
            cwd: cwd.into(),
            factory: Arc::new(factory),
            units: HashMap::new(),
        }
    }

    /// Open (creating if necessary) the session at `session_path` and start its
    /// unit if it is not already running. Returns the session id.
    ///
    /// The unit runs in the working directory recorded in the session header, so
    /// one host can serve sessions from different projects.
    pub fn open(&mut self, session_path: PathBuf) -> Result<String, HostError> {
        let (journal, _) = pi_agent::SessionJournal::open(session_path.clone(), &self.cwd)
            .map_err(|error| HostError::Io(error.to_string()))?;
        let id = journal.session_id().to_string();
        let cwd = if journal.cwd().is_empty() {
            self.cwd.clone()
        } else {
            journal.cwd().to_string()
        };
        drop(journal);
        if !self.units.contains_key(&id) {
            let unit = Unit::spawn(
                session_path,
                cwd,
                self.factory.clone(),
                Arc::new(Shared::default()),
            );
            self.units.insert(id.clone(), unit);
        }
        Ok(id)
    }

    pub fn send(&mut self, session_id: &str, command: Value) -> Result<(), HostError> {
        let unit = self
            .units
            .get_mut(session_id)
            .ok_or_else(|| HostError::UnknownSession(session_id.to_string()))?;
        unit.send(&command)
    }

    pub fn subscribe(&self, session_id: &str) -> Result<Subscription, HostError> {
        let unit = self
            .units
            .get(session_id)
            .ok_or_else(|| HostError::UnknownSession(session_id.to_string()))?;
        Ok(unit.subscribe())
    }

    /// Release a unit's in-memory agent; the next `send` wakes it.
    pub fn suspend(&mut self, session_id: &str) {
        if let Some(unit) = self.units.get_mut(session_id) {
            unit.suspend();
        }
    }

    pub fn is_running(&self, session_id: &str) -> bool {
        self.units
            .get(session_id)
            .map(Unit::is_running)
            .unwrap_or(false)
    }

    /// Suspend every running unit that has been idle for at least `idle`.
    ///
    /// A unit is idle when it has no live subscribers and no command or event
    /// since `now - idle`. Suspending drops the agent (the session file is the
    /// durable state); a later `send` wakes it. Returns the suspended ids.
    pub fn reap_idle(&mut self, idle: Duration, now: i64) -> Vec<String> {
        let idle_secs = idle.as_secs() as i64;
        let victims: Vec<String> = self
            .units
            .iter()
            .filter(|(_, unit)| unit.is_running())
            .filter(|(_, unit)| unit.shared.subscriber_count() == 0)
            .filter(|(_, unit)| unit.shared.idle_for(now) >= idle_secs)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &victims {
            if let Some(unit) = self.units.get_mut(id) {
                unit.suspend();
            }
        }
        victims
    }

    pub fn session_ids(&self) -> Vec<String> {
        self.units.keys().cloned().collect()
    }

    /// A status snapshot of every unit this host knows about.
    pub fn swarm(&self) -> Vec<UnitInfo> {
        let mut units: Vec<UnitInfo> = self
            .units
            .iter()
            .map(|(session_id, unit)| UnitInfo {
                session_id: session_id.clone(),
                name: unit.shared.session_name(&unit.session_path),
                cwd: unit.cwd.clone(),
                session_path: unit.session_path.to_string_lossy().into_owned(),
                running: unit.is_running(),
                last_event: unit.shared.last_event(),
                last_event_at: unit.shared.last_event_at(),
                subscribers: unit.shared.subscriber_count(),
            })
            .collect();
        units.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        units
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }
}

#[cfg(test)]
mod tests;
