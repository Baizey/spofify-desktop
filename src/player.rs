//! Minimal guarded local-output protocol. No credentials, networking, or audio URLs live here.
//!
//! Drive one request at a time through `Api::player(session, request.body.clone())`.
//! Build requests immediately before sending (the lease starts at SEND, not receipt).
//! Call `consume` on success, `failed` on *every* error (including 409; never force/replay).
//! Poll every `POLL_INTERVAL`; run `tick` independently of HTTP awaits. The audio endpoint
//! MUST also retain Authority and check it on every start/continuation and in its output
//! watchdog: a stalled UI/network thread must not keep buffered audio playing past expiry.
//! Endpoint methods are synchronous admission/completion boundaries, not unguarded async
//! launches. If audio preparation is asynchronous, fence its continuation with Authority.

use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_SAFE: u64 = 9_007_199_254_740_991;

#[derive(Clone)]
pub struct Authority {
    epoch: u64,
    generation: u64,
    lease: Arc<Mutex<Lease>>,
}
impl Authority {
    pub fn is_valid(&self) -> bool {
        self.is_valid_at(Instant::now())
    }
    pub fn is_valid_at(&self, now: Instant) -> bool {
        self.lease.lock().is_ok_and(|l| {
            l.epoch == self.epoch
                && l.generation == self.generation
                && l.deadline.is_some_and(|d| now < d)
        })
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.lease.lock().ok().and_then(|l| {
            (l.epoch == self.epoch && l.generation == self.generation)
                .then_some(l.deadline)
                .flatten()
        })
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}
#[derive(Default)]
struct Lease {
    epoch: u64,
    generation: u64,
    deadline: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Blocked,
    Media,
    Rejected,
}
impl Status {
    fn wire(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Blocked => "blocked",
            Self::Media => "media",
            Self::Rejected => "rejected",
        }
    }
}
pub struct Outcome {
    pub status: Status,
    pub state: Value,
}

/// Audio must synchronously silence buffered output in stop(), including pending starts.
/// prepare() validates only: no replacing/loading/playing the current queue.
/// Return Rejected for unsupported remote commands, never a successful no-op.
/// activate/execute return the actual state and exact outcome (blocked/media must be paused).
pub trait Endpoint {
    fn stop(&mut self);
    fn snapshot(&self) -> Value;
    fn prepare(&mut self, state: &Value) -> bool;
    fn activate(&mut self, state: &Value, authority: Authority) -> Outcome;
    fn execute(&mut self, command: &Value, authority: Authority) -> Outcome;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Busy,
    NotRegistered,
    OtherOutputSelected,
    NoAuthority,
    Stale,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "Invalid or unsupported playback state",
            Self::Busy => "Playback is awaiting confirmation",
            Self::NotRegistered => "Playback device is not registered",
            Self::OtherOutputSelected => {
                "Another output is selected; desktop will not take it over"
            }
            Self::NoAuthority => "Playback permission expired; press Play again after reconnecting",
            Self::Stale => "Stale playback response ignored",
        })
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestKind {
    Register,
    Poll,
    Claim,
    Command,
}
/// Opaque correlation metadata prevents delayed responses from reviving disposed authority.
pub struct Request {
    pub body: Value,
    pub sent_at: Instant,
    serial: u64,
    kind: RequestKind,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct View {
    #[serde(default)]
    device_id: String,
    devices: Vec<Device>,
    active_device_id: String,
    epoch: u64,
    lease_ms: u64,
    #[serde(default)]
    quarantine_ms: u64,
    state: Option<Value>,
    state_seq: u64,
    #[serde(default)]
    state_age_ms: u64,
    transfer: Option<Value>,
    directives: Vec<Directive>,
    #[serde(default)]
    commands: Vec<CommandOutcome>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Device {
    device_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    available: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommandOutcome {
    command_id: String,
    status: String,
}
struct Operation {
    id: String,
    kind: String,
    deadline: Instant,
}
struct QueuedCommand {
    command: Value,
    output: String,
    epoch: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Directive {
    delivery_id: String,
    epoch: u64,
    #[serde(rename = "type")]
    kind: String,
    state: Option<Value>,
    command: Option<Value>,
}

pub struct Player {
    device_id: String,
    active: String,
    selected: String,
    epoch: u64,
    sequence: u64,
    selection_sequence: u64,
    serial: u64,
    in_flight: Option<u64>,
    transfer: bool,
    quarantine: bool,
    connected: bool,
    released: bool,
    initialized: bool,
    lease: Arc<Mutex<Lease>>,
    delivered: HashSet<String>,
    acks: Vec<Value>,
    ack_state: Option<Value>,
    remote_state: Value,
    used_claims: HashSet<String>,
    devices: Vec<Device>,
    observed_at: Instant,
    state_age_ms: u64,
    queued: Option<QueuedCommand>,
    operation: Option<Operation>,
    interrupted: Option<Operation>,
    interrupted_result: Option<String>,
    feedback_status: String,
    feedback_until: Option<Instant>,
    claim_operation: Option<Operation>,
    claim_accepted: bool,
    feedback: String,
}
impl Default for Player {
    fn default() -> Self {
        Self {
            device_id: String::new(),
            active: String::new(),
            selected: String::new(),
            epoch: 0,
            sequence: 0,
            selection_sequence: 0,
            serial: 0,
            in_flight: None,
            transfer: false,
            quarantine: false,
            connected: false,
            released: false,
            initialized: false,
            lease: Arc::new(Mutex::new(Lease::default())),
            delivered: HashSet::new(),
            acks: Vec::new(),
            ack_state: None,
            remote_state: empty_snapshot(),
            used_claims: HashSet::new(),
            devices: Vec::new(),
            observed_at: Instant::now(),
            state_age_ms: 0,
            queued: None,
            operation: None,
            interrupted: None,
            interrupted_result: None,
            feedback_status: String::new(),
            feedback_until: None,
            claim_operation: None,
            claim_accepted: false,
            feedback: String::new(),
        }
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        // The endpoint's watchdog observes revocation even when its owner is dropped.
        if let Ok(mut l) = self.lease.lock() {
            l.deadline = None;
            l.generation += 1;
        }
    }
}
impl Player {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn device_id(&self) -> &str {
        &self.device_id
    }
    pub fn active_device_id(&self) -> &str {
        &self.active
    }
    pub fn is_remote(&self) -> bool {
        !self.active.is_empty() && self.active != self.device_id
    }
    pub fn remote_available(&self, now: Instant) -> bool {
        self.is_remote()
            && self.connected
            && !self.transfer
            && !self.quarantine
            && now.saturating_duration_since(self.observed_at) < Duration::from_secs(5)
            && self
                .devices
                .iter()
                .any(|d| d.device_id == self.active && d.available)
    }
    pub fn output_name(&self) -> &str {
        self.devices
            .iter()
            .find(|d| d.device_id == self.selected)
            .map(|d| d.name.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(if self.active.is_empty() {
                "No output selected"
            } else {
                "Selected output"
            })
    }
    pub fn observed_state(&self, now: Instant) -> Value {
        let mut state = self.remote_state.clone();
        if state["playing"] == true
            && state["waiting"] == false
            && state["loading"] != true
            && self.remote_available(now)
        {
            let age = (self.state_age_ms as f64 / 1000.0
                + now
                    .saturating_duration_since(self.observed_at)
                    .as_secs_f64())
            .min(5.0);
            let duration = state["duration"].as_f64().unwrap_or(0.0);
            state["elapsed"] =
                json!((state["elapsed"].as_f64().unwrap_or(0.0) + age).min(duration));
        }
        state
    }
    pub fn feedback(&self) -> &str {
        &self.feedback
    }
    pub fn command_pending(&self) -> bool {
        self.operation.is_some() || self.queued.is_some()
    }
    pub fn can_interrupt(&self) -> bool {
        self.interrupted.is_none()
            && (self.operation.as_ref().is_some_and(|op| op.kind != "pause")
                || self
                    .queued
                    .as_ref()
                    .is_some_and(|q| q.command["type"] != "pause"))
            && self
                .queued
                .as_ref()
                .is_none_or(|q| q.command["type"] != "pause")
    }
    pub fn selection_busy(&self) -> bool {
        self.transfer || self.quarantine
    }
    pub fn claim_in_progress(&self) -> bool {
        self.claim_operation.is_some()
    }
    pub fn claim_ready(&self) -> bool {
        self.claim_accepted
            && self.claim_operation.is_none()
            && self.initialized
            && !self.pending_acks()
            && !self.transfer
    }
    pub fn queue_remote(&mut self, command: Value, now: Instant) -> Result<(), Error> {
        if !self.remote_available(now) {
            return Err(Error::OtherOutputSelected);
        }
        let seek = command["type"] == "seek";
        let interrupt = command["type"] == "pause" && self.can_interrupt();
        let valid = match command["type"].as_str() {
            Some("pause" | "resume" | "next" | "previous") => {
                command.as_object().is_some_and(|o| o.len() == 1)
            }
            Some("seek") => {
                command.as_object().is_some_and(|o| o.len() == 2) && number(&command["value"])
            }
            Some("play") => {
                command.as_object().is_some_and(|o| o.len() == 2)
                    && crate::observation::valid_track(&command["track"], false)
            }
            _ => false,
        };
        if !valid {
            return Err(Error::Invalid);
        }
        if (self.operation.is_some() || self.queued.is_some()) && !seek && !interrupt {
            return Err(Error::Busy);
        }
        if self
            .queued
            .as_ref()
            .is_some_and(|q| q.command["type"] != "seek" && !interrupt)
        {
            return Err(Error::Busy);
        }
        self.queued = Some(QueuedCommand {
            command,
            output: self.active.clone(),
            epoch: self.epoch,
        });
        self.feedback = "Waiting for output confirmation…".into();
        self.feedback_status.clear();
        self.feedback_until = None;
        Ok(())
    }
    pub fn remote_request(&mut self, now: Instant) -> Result<Option<Request>, Error> {
        if self.in_flight.is_some()
            || (self.operation.is_some()
                && !self
                    .queued
                    .as_ref()
                    .is_some_and(|q| q.command["type"] == "pause" && self.interrupted.is_none()))
        {
            return Ok(None);
        }
        let Some(q) = self.queued.take() else {
            return Ok(None);
        };
        if q.output != self.active || q.epoch != self.epoch || !self.remote_available(now) {
            self.feedback =
                "Output changed. Command cancelled; act again on the selected output.".into();
            return Err(Error::Stale);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let kind = q.command["type"].as_str().unwrap_or("").to_owned();
        let r = self.request(json!({"action":"command","deviceId":self.device_id,"epoch":self.epoch,"commandId":id,"command":q.command}), RequestKind::Command, now)?;
        if self.operation.is_some() {
            self.interrupted = self.operation.take();
            self.interrupted_result = None;
        }
        self.operation = Some(Operation {
            id,
            kind,
            deadline: now + Duration::from_secs(10),
        });
        Ok(Some(r))
    }
    pub fn connection_status(&self, now: Instant) -> &'static str {
        if self.quarantine {
            "Output lease draining; playback is paused"
        } else if self.transfer {
            "Switching output…"
        } else if !self.connected {
            "Playback connection unavailable. Retry connection."
        } else if self.is_remote() && !self.remote_available(now) {
            "Selected output unavailable; no local fallback"
        } else {
            ""
        }
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn is_owner(&self, now: Instant) -> bool {
        self.authority(now).is_some()
    }
    pub fn pending_acks(&self) -> bool {
        !self.acks.is_empty()
    }
    pub fn authority(&self, now: Instant) -> Option<Authority> {
        if !self.connected
            || self.released
            || self.device_id.is_empty()
            || self.active != self.device_id
        {
            return None;
        }
        let generation = self.lease.lock().ok()?.generation;
        let a = Authority {
            epoch: self.epoch,
            generation,
            lease: self.lease.clone(),
        };
        a.is_valid_at(now).then_some(a)
    }
    fn request(&mut self, body: Value, kind: RequestKind, now: Instant) -> Result<Request, Error> {
        if self.in_flight.is_some() {
            return Err(Error::Busy);
        }
        self.serial += 1;
        self.in_flight = Some(self.serial);
        Ok(Request {
            body,
            kind,
            serial: self.serial,
            sent_at: now,
        })
    }
    pub fn register(&mut self, name: &str, now: Instant) -> Result<Request, Error> {
        if !self.device_id.is_empty() {
            return Err(Error::Busy);
        }
        if name.trim().is_empty() || name.chars().count() > 64 || !text(name, 256) {
            return Err(Error::Invalid);
        }
        self.request(
            json!({"action":"register", "name":name}),
            RequestKind::Register,
            now,
        )
    }
    /// Fresh command_id from caller (UUID/random protocol ID); never reuse or auto-retry.
    pub fn claim(&mut self, command_id: &str, now: Instant) -> Result<Request, Error> {
        if self.device_id.is_empty() {
            return Err(Error::NotRegistered);
        }
        if self.in_flight.is_some() {
            return Err(Error::Busy);
        }
        if !self.connected {
            return Err(Error::NoAuthority);
        }
        if !self.active.is_empty() || self.transfer || self.quarantine {
            return Err(Error::OtherOutputSelected);
        }
        if !protocol_id(command_id)
            || self.used_claims.contains(command_id)
            || self.used_claims.len() >= 1024
        {
            return Err(Error::Invalid);
        }
        self.selection_sequence += 1;
        self.used_claims.insert(command_id.to_owned());
        self.claim_accepted = false;
        self.claim_operation = Some(Operation {
            id: command_id.to_owned(),
            kind: "claim".into(),
            deadline: now + Duration::from_secs(10),
        });
        self.request(
            json!({"action":"transfer", "deviceId":self.device_id,
            "targetDeviceId":self.device_id, "epoch":self.epoch, "commandId":command_id,
            "selectionSeq":self.selection_sequence, "requireUnselected":true}),
            RequestKind::Claim,
            now,
        )
    }
    pub fn poll(&mut self, endpoint: &mut impl Endpoint, now: Instant) -> Result<Request, Error> {
        self.tick(endpoint, now);
        if self.device_id.is_empty() {
            return Err(Error::NotRegistered);
        }
        if self.in_flight.is_some() {
            return Err(Error::Busy);
        }
        let state = if self.active == self.device_id {
            self.ack_state
                .clone()
                .or_else(|| (self.initialized && self.is_owner(now)).then(|| endpoint.snapshot()))
        } else {
            None
        };
        if state.as_ref().is_some_and(|s| !valid_snapshot(s)) {
            self.failed(endpoint);
            return Err(Error::Invalid);
        }
        let mut body = json!({"action":"poll", "deviceId":self.device_id, "epoch":self.epoch, "acks":self.acks});
        if let Some(state) = state {
            self.sequence = self
                .sequence
                .checked_add(1)
                .filter(|n| *n <= MAX_SAFE)
                .ok_or(Error::Invalid)?;
            body["stateSeq"] = json!(self.sequence);
            body["state"] = state;
        }
        let request = self.request(body, RequestKind::Poll, now)?;
        self.acks.clear();
        self.ack_state = None;
        Ok(request)
    }
    /// No retries. A 409 is not permission to force a selection. Poll later to reconcile.
    pub fn failed(&mut self, endpoint: &mut impl Endpoint) {
        self.in_flight = None;
        self.connected = false;
        if self.operation.take().is_some() || self.interrupted.take().is_some() {
            self.interrupted = None;
            self.feedback =
                "Command outcome unknown. Check the actual output, then act again.".into();
            self.feedback_status = "unknown".into();
            self.feedback_until = Some(Instant::now() + Duration::from_secs(8));
        }
        self.queued = None;
        self.claim_operation = None;
        self.claim_accepted = false;
        self.fence(endpoint);
        self.acks.clear();
        self.ack_state = None;
    }
    pub fn failed_status(&mut self, endpoint: &mut impl Endpoint, request: &Request, status: u16) {
        let submission = request.kind == RequestKind::Command
            && self
                .operation
                .as_ref()
                .is_some_and(|op| request.body["commandId"] == op.id);
        let previous_unknown = self.interrupted.is_some();
        self.failed(endpoint);
        if submission && matches!(status, 400 | 403 | 404 | 409 | 413 | 429) {
            self.feedback = if previous_unknown {
                "Pause refused; prior command outcome unknown. Check output."
            } else {
                "Command refused by server. Check output and act again."
            }
            .into();
            self.feedback_status = "rejected".into();
            self.feedback_until = Some(Instant::now() + Duration::from_secs(8));
        }
    }
    /// Observe a bounded 409 response body (or GET view) after failure, without granting
    /// authority, replaying the failed action, or executing directives. A subsequent normal
    /// poll is required. Without this view a stale-epoch poll cannot recover.
    pub fn reconcile(
        &mut self,
        mut value: Value,
        endpoint: &mut impl Endpoint,
        now: Instant,
    ) -> Result<(), Error> {
        self.failed(endpoint);
        if !value.get("directives").is_some_and(Value::is_array) {
            return Err(Error::Invalid);
        }
        value["directives"] = json!([]);
        let observation = Request {
            body: Value::Null,
            sent_at: now,
            serial: 0,
            kind: RequestKind::Claim,
        };
        let result = self.accept(&observation, value, endpoint, now);
        if result.is_err() {
            self.failed(endpoint);
        }
        result
    }
    /// On logout/auth loss discard this Player after calling shutdown. Never reuse across sessions.
    pub fn shutdown(&mut self, endpoint: &mut impl Endpoint) {
        self.failed(endpoint);
    }
    fn fence(&mut self, endpoint: &mut impl Endpoint) {
        if let Ok(mut l) = self.lease.lock() {
            l.deadline = None;
            l.generation += 1;
        }
        self.initialized = false;
        endpoint.stop();
    }
    /// Call independently of the network task; audio must additionally enforce Authority itself.
    pub fn tick(&mut self, endpoint: &mut impl Endpoint, now: Instant) {
        if self.feedback_until.is_some_and(|until| now >= until) {
            self.feedback.clear();
            self.feedback_status.clear();
            self.feedback_until = None;
        }
        if self
            .interrupted
            .as_ref()
            .is_some_and(|op| now >= op.deadline)
        {
            self.interrupted = None;
            self.interrupted_result = Some("unknown".into());
        }
        if self.operation.as_ref().is_some_and(|op| now >= op.deadline) {
            self.operation = None;
            self.queued = None;
            self.feedback =
                "Command outcome unknown. Check the actual output, then act again.".into();
            self.feedback_status = "unknown".into();
            self.feedback_until = Some(now + Duration::from_secs(8));
        }
        if self
            .claim_operation
            .as_ref()
            .is_some_and(|op| now >= op.deadline)
        {
            self.claim_operation = None;
            self.claim_accepted = false;
            self.feedback = "Playback selection outcome unknown. Select the track again.".into();
        }
        let expired = self
            .lease
            .lock()
            .map_or(true, |l| l.deadline.is_some_and(|d| now >= d));
        if expired {
            self.connected = false;
            self.fence(endpoint);
            self.acks.clear();
            self.ack_state = None;
        }
    }
    pub fn consume(
        &mut self,
        request: &Request,
        value: Value,
        endpoint: &mut impl Endpoint,
        now: Instant,
    ) -> Result<(), Error> {
        if self.in_flight != Some(request.serial) {
            return Err(Error::Stale);
        }
        self.in_flight = None;
        let result = self.accept(request, value, endpoint, now);
        if result.is_err() {
            self.failed(endpoint);
        }
        result
    }
    fn accept(
        &mut self,
        request: &Request,
        value: Value,
        endpoint: &mut impl Endpoint,
        now: Instant,
    ) -> Result<(), Error> {
        self.tick(endpoint, now);
        let view: View = serde_json::from_value(value).map_err(|_| Error::Invalid)?;
        if view.epoch == 0
            || view.epoch > MAX_SAFE
            || view.epoch < self.epoch
            || view.state_seq > MAX_SAFE
            || view.lease_ms > 8000
            || view.quarantine_ms > 8000
            || view.devices.len() > 16
            || view.directives.len() > 16
            || view.state_age_ms > MAX_SAFE
            || view.commands.len() > 128
            || view.commands.iter().any(|c| {
                !protocol_id(&c.command_id)
                    || !matches!(
                        c.status.as_str(),
                        "pending"
                            | "ok"
                            | "blocked"
                            | "media"
                            | "rejected"
                            | "timeout"
                            | "cancelled"
                    )
            })
            || view
                .commands
                .iter()
                .map(|c| &c.command_id)
                .collect::<HashSet<_>>()
                .len()
                != view.commands.len()
            || view
                .state
                .as_ref()
                .is_some_and(|s| !crate::observation::valid_state(s))
            || view.transfer.as_ref().is_some_and(|t| !valid_transfer(t))
            || view.devices.iter().any(|d| {
                !protocol_id(&d.device_id) || !text(&d.name, 256) || d.name.chars().count() > 64
            })
            || view
                .devices
                .iter()
                .map(|d| &d.device_id)
                .collect::<HashSet<_>>()
                .len()
                != view.devices.len()
            || (!view.active_device_id.is_empty()
                && !view
                    .devices
                    .iter()
                    .any(|d| d.device_id == view.active_device_id))
            || view.directives.iter().any(|d| {
                !protocol_id(&d.delivery_id)
                    || d.epoch != view.epoch
                    || !matches!(
                        d.kind.as_str(),
                        "prepare" | "release" | "activate" | "command"
                    )
            })
        {
            return Err(Error::Invalid);
        }
        if request.kind == RequestKind::Register {
            if !protocol_id(&view.device_id) {
                return Err(Error::Invalid);
            }
            self.device_id = view.device_id.clone();
        }
        if !view.devices.iter().any(|d| d.device_id == self.device_id)
            || (!view.device_id.is_empty() && view.device_id != self.device_id)
        {
            return Err(Error::Invalid);
        }
        // Only an acknowledged, live-source handoff may carry playing intent. Initial
        // selection and recovery always import paused; reconnect must not restart audio.
        let resume_handoff = self.connected
            && view
                .transfer
                .as_ref()
                .and_then(|t| t["sourceDeviceId"].as_str())
                .is_some_and(protocol_id);
        let changed = view.epoch != self.epoch || view.active_device_id != self.active;
        if changed {
            self.fence(endpoint);
            self.acks.clear();
            self.ack_state = None;
            self.delivered.clear();
            self.sequence = 0;
            self.released = false;
        }
        self.epoch = view.epoch;
        self.active = view.active_device_id;
        self.selected = view
            .transfer
            .as_ref()
            .and_then(|t| t["targetDeviceId"].as_str())
            .unwrap_or(&self.active)
            .to_owned();
        self.sequence = self.sequence.max(view.state_seq);
        self.transfer = view.transfer.is_some();
        self.quarantine = view.quarantine_ms > 0;
        self.remote_state = view.state.unwrap_or_else(empty_snapshot);
        self.devices = view.devices;
        self.state_age_ms = view.state_age_ms.min(5000);
        self.observed_at = now;
        self.connected = true;
        if matches!(self.feedback_status.as_str(), "blocked" | "media")
            && self.remote_state["error"] != self.feedback_status
        {
            self.feedback.clear();
            self.feedback_status.clear();
            self.feedback_until = None;
        }
        for c in view.commands {
            if self
                .interrupted
                .as_ref()
                .is_some_and(|op| op.id == c.command_id)
                && c.status != "pending"
            {
                self.interrupted = None;
                self.interrupted_result = Some(c.status.clone());
            }
            if self
                .operation
                .as_ref()
                .is_some_and(|op| op.id == c.command_id)
                && c.status != "pending"
            {
                self.operation = None;
                self.feedback = match c.status.as_str() {
                    "ok" => "",
                    "blocked" => "Output needs a tap. Continue playback on the actual output.",
                    "media" => "Media failed on the output. Retry there or choose another track.",
                    "rejected" => "Output rejected the command. Check the output and act again.",
                    "timeout" => "Output timed out. Check the output before acting again.",
                    _ => "Command cancelled by output change. Act again if needed.",
                }
                .into();
                self.feedback_status = c.status.clone();
                self.feedback_until = Some(now + Duration::from_secs(8));
                if c.status != "ok"
                    && self
                        .queued
                        .as_ref()
                        .is_none_or(|q| q.command["type"] != "pause")
                {
                    // A deliberate Pause is cancellation intent, not a replay of
                    // the failed operation. Keep it bound to its output/epoch.
                    self.queued = None;
                }
            }
            if self
                .claim_operation
                .as_ref()
                .is_some_and(|op| op.id == c.command_id)
                && c.status != "pending"
            {
                self.claim_operation = None;
                self.claim_accepted = c.status == "ok" && self.active == self.device_id;
                if !self.claim_accepted {
                    self.feedback = "Local selection failed. Select the track again.".into();
                }
            }
        }
        // Command/transfer replies and GET reconciliation never grant/renew output.
        if matches!(request.kind, RequestKind::Register | RequestKind::Poll) {
            let deadline =
                request.sent_at + Duration::from_millis(view.lease_ms.saturating_sub(2000));
            if self.active == self.device_id && !self.released && now < deadline {
                if let Ok(mut l) = self.lease.lock() {
                    l.epoch = self.epoch;
                    l.deadline = Some(deadline);
                }
            } else {
                self.fence(endpoint);
            }
        }
        for d in view.directives {
            if self.delivered.contains(&d.delivery_id) {
                continue;
            }
            // Never evict a dedup entry then execute the same delivery twice in one epoch.
            if self.delivered.len() >= 4096 {
                return Err(Error::Invalid);
            }
            self.delivered.insert(d.delivery_id.clone());
            if d.kind == "prepare" {
                let state = d.state.unwrap_or_else(empty_snapshot);
                let ok = valid_snapshot(&state) && endpoint.prepare(&state);
                self.ack(
                    &d.delivery_id,
                    if ok { Status::Ok } else { Status::Rejected },
                    None,
                );
                continue;
            }
            let Some(authority) = self.authority(now) else {
                continue;
            }; // no ACK for unexecuted work
            let result = match d.kind.as_str() {
                "release" => {
                    let state = if self.initialized {
                        endpoint.snapshot()
                    } else {
                        paused(self.remote_state.clone())
                    };
                    self.fence(endpoint);
                    self.released = true;
                    Outcome {
                        status: Status::Ok,
                        state,
                    }
                }
                "activate" => {
                    let state = d.state.unwrap_or_else(empty_snapshot);
                    if !valid_snapshot(&state) {
                        endpoint.stop();
                        Outcome {
                            status: Status::Rejected,
                            state: empty_snapshot(),
                        }
                    } else {
                        let state = if resume_handoff { state } else { paused(state) };
                        endpoint.activate(&state, authority.clone())
                    }
                }
                "command" => {
                    let mut command = d.command.ok_or(Error::Invalid)?;
                    if !self.initialized {
                        let state = paused(self.remote_state.clone());
                        if !valid_snapshot(&state) {
                            return Err(Error::Invalid);
                        }
                        let initial = endpoint.activate(&state, authority.clone());
                        if !valid_outcome(&initial)
                            || initial.status != Status::Ok
                            || initial.state["error"] != ""
                        {
                            return Err(Error::Invalid);
                        }
                        self.initialized = true;
                    }
                    if command["type"] == "toggle" {
                        command = json!({"type":if endpoint.snapshot()["playing"] == true { "pause" } else { "resume" }});
                    }
                    if matches!(command["type"].as_str(), Some("pause" | "close")) {
                        endpoint.stop();
                    }
                    endpoint.execute(&command, authority.clone())
                }
                _ => unreachable!(),
            };
            if !valid_outcome(&result)
                || (d.kind == "activate"
                    && result.status == Status::Ok
                    && result.state["error"] != "")
            {
                return Err(Error::Invalid);
            }
            if d.kind != "release" && !authority.is_valid() {
                self.fence(endpoint);
                continue;
            }
            if d.kind == "activate" {
                self.initialized = result.status != Status::Rejected;
            }
            self.ack(&d.delivery_id, result.status, Some(result.state));
        }
        Ok(())
    }
    fn ack(&mut self, id: &str, status: Status, state: Option<Value>) {
        self.acks
            .push(json!({"deliveryId":id,"epoch":self.epoch,"status":status.wire()}));
        if state.is_some() {
            self.ack_state = state;
        }
    }
    /// Explicit single-track local play. Caller resolves authenticated audio by track ID only.
    pub fn play(
        &mut self,
        state: Value,
        endpoint: &mut impl Endpoint,
        now: Instant,
    ) -> Result<Status, Error> {
        if !valid_snapshot(&state) || state["queue"].as_array().is_none_or(|q| q.len() != 1) {
            return Err(Error::Invalid);
        }
        self.local_admission(endpoint, now)?;
        let authority = self.authority(now).ok_or(Error::NoAuthority)?;
        let result = endpoint.activate(&state, authority.clone());
        self.local_result(result, authority, endpoint, true)
    }
    /// Owner-local transport: execute first, publish actual snapshot in the next poll.
    pub fn local(
        &mut self,
        command: Value,
        endpoint: &mut impl Endpoint,
        now: Instant,
    ) -> Result<Status, Error> {
        self.local_admission(endpoint, now)?;
        let authority = self.authority(now).ok_or(Error::NoAuthority)?;
        if !self.initialized {
            return Err(Error::NoAuthority);
        }
        if matches!(command["type"].as_str(), Some("pause" | "close")) {
            endpoint.stop();
        }
        let result = endpoint.execute(&command, authority.clone());
        self.local_result(result, authority, endpoint, false)
    }
    fn local_admission(&mut self, endpoint: &mut impl Endpoint, now: Instant) -> Result<(), Error> {
        self.tick(endpoint, now);
        if self.transfer || !self.acks.is_empty() || self.in_flight.is_some() {
            return Err(Error::Busy);
        }
        if !self.active.is_empty() && self.active != self.device_id {
            return Err(Error::OtherOutputSelected);
        }
        if !self.is_owner(now) {
            return Err(Error::NoAuthority);
        }
        Ok(())
    }
    fn local_result(
        &mut self,
        result: Outcome,
        authority: Authority,
        endpoint: &mut impl Endpoint,
        activation: bool,
    ) -> Result<Status, Error> {
        if !valid_outcome(&result)
            || (activation && result.status == Status::Ok && result.state["error"] != "")
            || !authority.is_valid()
        {
            self.failed(endpoint);
            return Err(Error::NoAuthority);
        }
        self.initialized = true;
        self.ack_state = Some(result.state);
        Ok(result.status)
    }
}

fn valid_transfer(t: &Value) -> bool {
    t.as_object().is_some_and(|o| o.len() == 4)
        && t["commandId"].as_str().is_some_and(protocol_id)
        && t["sourceDeviceId"]
            .as_str()
            .is_some_and(|s| s.is_empty() || protocol_id(s))
        && t["targetDeviceId"].as_str().is_some_and(protocol_id)
        && matches!(
            t["phase"].as_str(),
            Some("waiting" | "preparing" | "releasing" | "activating")
        )
}
pub fn empty_snapshot() -> Value {
    json!({"version":1,"queue":[],"position":-1,"context":null,"shuffle":false,"repeat":"off",
        "elapsed":0.0,"duration":0.0,"playing":false,"waiting":false,"loading":false,
        "error":"","truncated":false,"notice":"","occurrence":null})
}
/// `track` is positively projected; URLs, filesystem paths and tokens can never be published.
/// Catalog durationMs must be converted to seconds by the caller before passing `duration`.
pub fn single_track(track: &Value, key: u64) -> Result<Value, Error> {
    let artists = track["artists"]
        .as_array()
        .ok_or(Error::Invalid)?
        .iter()
        .map(|a| json!({"id":a["id"].as_str().unwrap_or(""), "name":a["name"]}))
        .collect::<Vec<_>>();
    let mut s = empty_snapshot();
    s["queue"] = json!([{"id":track["id"],"name":track["name"],"artists":artists,
        "albumName":track["albumName"].as_str().unwrap_or(""),"albumId":track["albumId"].as_str().unwrap_or(""),
        "duration":track["duration"],"key":key,"order":0}]);
    s["position"] = json!(0);
    s["duration"] = track["duration"].clone();
    s["playing"] = json!(true);
    if valid_snapshot(&s) {
        Ok(s)
    } else {
        Err(Error::Invalid)
    }
}
fn paused(mut state: Value) -> Value {
    if let Some(s) = state.as_object_mut() {
        for name in ["playing", "waiting", "loading"] {
            s.insert(name.into(), json!(false));
        }
    }
    state
}
fn protocol_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn text(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(char::is_control)
}
fn number(v: &Value) -> bool {
    v.as_f64()
        .is_some_and(|n| n.is_finite() && (0.0..=604800.0).contains(&n))
}
fn id(v: &Value, optional: bool) -> bool {
    v.as_str()
        .is_some_and(|s| (optional || !s.is_empty()) && s.trim() == s && text(s, 512))
}
fn string(v: &Value, max: usize) -> bool {
    v.as_str().is_some_and(|s| text(s, max))
}
/// Deliberately only the supported single-track, context-free subset. Never silently truncate a handoff.
pub fn valid_snapshot(s: &Value) -> bool {
    let Some(obj) = s.as_object() else {
        return false;
    };
    let fields = [
        "version",
        "queue",
        "position",
        "context",
        "shuffle",
        "repeat",
        "elapsed",
        "duration",
        "playing",
        "waiting",
        "loading",
        "error",
        "truncated",
        "notice",
        "occurrence",
    ];
    if obj.keys().any(|k| !fields.contains(&k.as_str()))
        || fields
            .iter()
            .any(|k| *k != "loading" && !obj.contains_key(*k))
        || s["version"].as_u64() != Some(1)
        || !s["context"].is_null()
        || !s["occurrence"].is_null()
        || s["shuffle"] != false
        || s["repeat"] != "off"
        || s["truncated"] != false
        || s["notice"] != ""
        || !matches!(
            s["error"].as_str(),
            Some("" | "blocked" | "media" | "queue")
        )
        || !number(&s["elapsed"])
        || !number(&s["duration"])
        || ["playing", "waiting"].iter().any(|k| !s[*k].is_boolean())
        || obj.get("loading").is_some_and(|v| !v.is_boolean())
    {
        return false;
    }
    let Some(q) = s["queue"].as_array() else {
        return false;
    };
    if q.is_empty() {
        return s["position"].as_i64() == Some(-1) && s["playing"] == false;
    }
    if q.len() != 1 || s["position"].as_i64() != Some(0) {
        return false;
    }
    let t = &q[0];
    let Some(tobj) = t.as_object() else {
        return false;
    };
    let tf = [
        "id",
        "name",
        "artists",
        "albumName",
        "albumId",
        "duration",
        "key",
        "order",
        "entryId",
        "targetId",
        "occurrenceId",
        "playbackContext",
    ];
    if tobj.keys().any(|k| !tf.contains(&k.as_str()))
        || !id(&t["id"], false)
        || !string(&t["name"], 512)
        || !string(&t["albumName"], 512)
        || !id(&t["albumId"], true)
        || !number(&t["duration"])
        || !t["key"]
            .as_u64()
            .is_some_and(|n| (1..=MAX_SAFE).contains(&n))
        || !t["order"].as_u64().is_some_and(|n| n <= MAX_SAFE)
        || ["entryId", "targetId", "occurrenceId"]
            .iter()
            .any(|k| tobj.get(*k).is_some_and(|v| v != ""))
        || tobj.get("playbackContext").is_some_and(|v| !v.is_null())
    {
        return false;
    }
    t["artists"].as_array().is_some_and(|artists| {
        artists.len() <= 64
            && artists.iter().all(|a| {
                a.as_object()
                    .is_some_and(|o| o.keys().all(|k| k == "id" || k == "name"))
                    && a.get("id").is_none_or(|value| id(value, true))
                    && string(&a["name"], 512)
            })
    })
}
fn valid_outcome(result: &Outcome) -> bool {
    valid_snapshot(&result.state)
        && match result.status {
            Status::Blocked | Status::Media => {
                result.state["playing"] == false && result.state["error"] == result.status.wire()
            }
            Status::Ok => true,
            Status::Rejected => true,
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Audio {
        state: Value,
        stops: usize,
        starts: usize,
        permit: Option<Authority>,
    }
    impl Default for Audio {
        fn default() -> Self {
            Self {
                state: empty_snapshot(),
                stops: 0,
                starts: 0,
                permit: None,
            }
        }
    }
    impl Endpoint for Audio {
        fn stop(&mut self) {
            self.stops += 1;
            self.state = paused(self.state.clone());
        }
        fn snapshot(&self) -> Value {
            self.state.clone()
        }
        fn prepare(&mut self, _: &Value) -> bool {
            true
        }
        fn activate(&mut self, s: &Value, a: Authority) -> Outcome {
            self.starts += usize::from(s["playing"] == true);
            self.state = s.clone();
            self.permit = Some(a);
            Outcome {
                status: Status::Ok,
                state: self.state.clone(),
            }
        }
        fn execute(&mut self, c: &Value, a: Authority) -> Outcome {
            self.permit = Some(a);
            match c["type"].as_str() {
                Some("pause") => self.state = paused(self.state.clone()),
                Some("resume") => {
                    self.state["playing"] = json!(true);
                    self.starts += 1;
                }
                Some("close") => self.state = empty_snapshot(),
                _ => {
                    return Outcome {
                        status: Status::Rejected,
                        state: self.state.clone(),
                    };
                }
            }
            Outcome {
                status: Status::Ok,
                state: self.state.clone(),
            }
        }
    }
    fn view(epoch: u64, active: &str, directives: Value) -> Value {
        json!({"deviceId":"desktop","devices":[{"deviceId":"desktop","name":"Desktop","available":true},{"deviceId":"phone","name":"Browser output","available":true}],
            "activeDeviceId":active,"epoch":epoch,"leaseMs":8000,"quarantineMs":0,"state":null,
            "stateSeq":0,"transfer":null,"directives":directives})
    }
    fn registered(active: &str) -> (Player, Audio, Instant) {
        let mut p = Player::new();
        let mut a = Audio::default();
        let now = Instant::now();
        let r = p.register("Desktop", now).unwrap();
        p.consume(&r, view(1, active, json!([])), &mut a, now)
            .unwrap();
        (p, a, now)
    }
    fn track() -> Value {
        single_track(&json!({"id":"song","name":"Song","artists":[],"albumName":"Album","albumId":"a","duration":123.0,
            "url":"https://must-not-publish.invalid","selectionToken":"secret","libraryPath":"private"}),1).unwrap()
    }
    #[test]
    fn remote_commands_wait_for_terminal_receipt_and_never_grant_output() {
        let (mut p, mut a, now) = registered("phone");
        p.queue_remote(json!({"type":"pause"}), now).unwrap();
        let r = p.remote_request(now).unwrap().unwrap();
        assert_eq!(r.body["deviceId"], "desktop");
        assert_eq!(r.body["action"], "command");
        let command_id = r.body["commandId"].clone();
        p.consume(&r, view(1, "phone", json!([])), &mut a, now)
            .unwrap();
        assert!(p.command_pending());
        assert!(!p.is_owner(now));
        let r = p.poll(&mut a, now).unwrap();
        let mut v = view(1, "phone", json!([]));
        v["commands"] = json!([{"commandId":command_id,"status":"blocked"}]);
        p.consume(&r, v, &mut a, now).unwrap();
        assert!(!p.command_pending());
        assert!(p.feedback().contains("tap"));
        assert_eq!(a.starts, 0);
    }
    #[test]
    fn seek_coalesces_only_unsent_and_failure_does_not_replay() {
        let (mut p, mut a, now) = registered("phone");
        p.queue_remote(json!({"type":"seek","value":10}), now)
            .unwrap();
        p.queue_remote(json!({"type":"seek","value":20}), now)
            .unwrap();
        let r = p.remote_request(now).unwrap().unwrap();
        assert_eq!(r.body["command"]["value"], 20);
        p.queue_remote(json!({"type":"seek","value":30}), now)
            .unwrap();
        assert!(p.remote_request(now).unwrap().is_none());
        p.failed(&mut a);
        assert!(!p.command_pending());
        assert!(p.feedback().contains("unknown"));
        assert!(p.remote_request(now).unwrap().is_none());
        assert!(matches!(
            p.consume(&r, view(1, "phone", json!([])), &mut a, now),
            Err(Error::Stale)
        ));
    }
    #[test]
    fn remote_terminal_outcomes_and_stalled_poll_are_bounded_without_replay() {
        for status in ["ok", "blocked", "media", "rejected", "timeout", "cancelled"] {
            let (mut p, mut a, now) = registered("phone");
            p.queue_remote(json!({"type":"next"}), now).unwrap();
            let r = p.remote_request(now).unwrap().unwrap();
            let id = r.body["commandId"].clone();
            let mut v = view(1, "phone", json!([]));
            v["commands"] = json!([{"commandId":id,"status":status}]);
            p.consume(&r, v, &mut a, now).unwrap();
            assert!(!p.command_pending());
            assert_eq!(p.feedback().is_empty(), status == "ok");
            assert!(p.remote_request(now).unwrap().is_none());
            assert_eq!(a.starts, 0);
        }
        let (mut p, mut a, now) = registered("phone");
        p.queue_remote(json!({"type":"pause"}), now).unwrap();
        let _r = p.remote_request(now).unwrap().unwrap();
        p.tick(&mut a, now + Duration::from_secs(10));
        assert!(!p.command_pending());
        assert!(p.feedback().contains("unknown"));
        assert!(!p.remote_available(now + Duration::from_secs(5)));
    }
    #[test]
    fn pause_preempts_one_pending_play_and_accounts_for_both_receipts() {
        let (mut p, mut a, now) = registered("phone");
        let selected = track()["queue"][0].clone();
        p.queue_remote(json!({"type":"play","track":selected}), now)
            .unwrap();
        let play = p.remote_request(now).unwrap().unwrap();
        let play_id = play.body["commandId"].clone();
        // Pause is deliberately retained while the serialized Play HTTP is in flight.
        assert!(p.can_interrupt());
        p.queue_remote(json!({"type":"pause"}), now).unwrap();
        assert!(p.remote_request(now).unwrap().is_none());
        p.consume(&play, view(1, "phone", json!([])), &mut a, now)
            .unwrap();
        let pause = p.remote_request(now).unwrap().unwrap();
        assert_eq!(pause.body["command"]["type"], "pause");
        assert!(!p.can_interrupt());
        assert!(p.queue_remote(json!({"type":"pause"}), now).is_err());
        let pause_id = pause.body["commandId"].clone();
        let mut v = view(1, "phone", json!([]));
        v["commands"] = json!([{"commandId":play_id,"status":"cancelled"},{"commandId":pause_id,"status":"pending"}]);
        p.consume(&pause, v, &mut a, now).unwrap();
        assert_eq!(p.interrupted_result.as_deref(), Some("cancelled"));
        let poll = p.poll(&mut a, now).unwrap();
        let mut v = view(1, "phone", json!([]));
        v["commands"] = json!([{"commandId":pause_id,"status":"ok"}]);
        p.consume(&poll, v, &mut a, now).unwrap();
        assert!(!p.command_pending());
        assert_eq!(a.starts, 0);
    }
    #[test]
    fn definitive_refusal_differs_from_lost_reply_and_stale_feedback_recovers() {
        for status in [0, 403, 409] {
            let (mut p, mut a, now) = registered("phone");
            p.queue_remote(json!({"type":"pause"}), now).unwrap();
            let r = p.remote_request(now).unwrap().unwrap();
            p.failed_status(&mut a, &r, status);
            assert_eq!(p.feedback().contains("unknown"), status == 0);
            assert_eq!(p.feedback().contains("refused"), status != 0);
            assert!(p.remote_request(now).unwrap().is_none());
        }
        let (mut p, mut a, now) = registered("phone");
        p.queue_remote(json!({"type":"resume"}), now).unwrap();
        let r = p.remote_request(now).unwrap().unwrap();
        let mut v = view(1, "phone", json!([]));
        v["state"] = paused(track());
        v["state"]["error"] = json!("blocked");
        v["commands"] = json!([{"commandId":r.body["commandId"],"status":"blocked"}]);
        p.consume(&r, v, &mut a, now).unwrap();
        assert!(p.feedback().contains("tap"));
        let r = p.poll(&mut a, now).unwrap();
        let mut v = view(1, "phone", json!([]));
        v["state"] = track();
        p.consume(&r, v, &mut a, now).unwrap();
        assert!(p.feedback().is_empty());
    }
    #[test]
    fn accepted_command_poll_refusal_is_unknown_and_queued_pause_survives_terminal_failure() {
        for status in [403, 409] {
            let (mut p, mut a, now) = registered("phone");
            p.queue_remote(json!({"type":"resume"}), now).unwrap();
            let r = p.remote_request(now).unwrap().unwrap();
            p.consume(&r, view(1, "phone", json!([])), &mut a, now)
                .unwrap();
            let poll = p.poll(&mut a, now).unwrap();
            p.failed_status(&mut a, &poll, status);
            assert!(p.feedback().contains("unknown"));
            assert!(!p.feedback().contains("refused"));
        }
        let (mut p, mut a, now) = registered("phone");
        p.queue_remote(json!({"type":"seek","value":10}), now)
            .unwrap();
        let r = p.remote_request(now).unwrap().unwrap();
        let id = r.body["commandId"].clone();
        p.consume(&r, view(1, "phone", json!([])), &mut a, now)
            .unwrap();
        let poll = p.poll(&mut a, now).unwrap();
        p.queue_remote(json!({"type":"pause"}), now).unwrap();
        let mut v = view(1, "phone", json!([]));
        v["commands"] = json!([{"commandId":id,"status":"rejected"}]);
        p.consume(&poll, v, &mut a, now).unwrap();
        let pause = p.remote_request(now).unwrap().unwrap();
        assert_eq!(pause.body["command"]["type"], "pause");
    }
    #[test]
    fn refusing_interrupting_pause_does_not_refuse_previously_accepted_play() {
        let (mut p, mut a, now) = registered("phone");
        p.queue_remote(json!({"type":"resume"}), now).unwrap();
        let r = p.remote_request(now).unwrap().unwrap();
        p.consume(&r, view(1, "phone", json!([])), &mut a, now)
            .unwrap();
        p.queue_remote(json!({"type":"pause"}), now).unwrap();
        let pause = p.remote_request(now).unwrap().unwrap();
        p.failed_status(&mut a, &pause, 403);
        assert!(p.feedback().contains("Pause refused"));
        assert!(p.feedback().contains("prior command outcome unknown"));
        assert!(!p.command_pending());
    }
    #[test]
    fn unavailable_selection_never_falls_back_and_progress_is_bounded() {
        let (mut p, mut a, now) = registered("phone");
        let r = p.poll(&mut a, now).unwrap();
        let mut v = view(1, "phone", json!([]));
        v["state"] = track();
        v["state"]["elapsed"] = json!(20);
        v["stateAgeMs"] = json!(90000);
        p.consume(&r, v, &mut a, now).unwrap();
        assert_eq!(
            p.observed_state(now + Duration::from_secs(1))["elapsed"],
            25.0
        );
        let r = p.poll(&mut a, now).unwrap();
        let mut v = view(1, "phone", json!([]));
        v["devices"][1]["available"] = json!(false);
        p.consume(&r, v, &mut a, now).unwrap();
        assert!(p.is_remote());
        assert_eq!(p.active_device_id(), "phone");
        assert!(p.queue_remote(json!({"type":"resume"}), now).is_err());
        assert!(p.claim("never-take-over", now).is_err());
        assert_eq!(a.starts, 0);
    }
    #[test]
    fn self_claim_requires_matching_terminal_acceptance_after_activation_ack() {
        let (mut p, mut a, now) = registered("");
        let claim = p.claim("claim", now).unwrap();
        p.consume(&claim, view(1, "", json!([])), &mut a, now)
            .unwrap();
        let poll = p.poll(&mut a, now).unwrap();
        p.consume(
            &poll,
            view(
                2,
                "desktop",
                json!([{"deliveryId":"activate","epoch":2,"type":"activate"}]),
            ),
            &mut a,
            now,
        )
        .unwrap();
        assert!(!p.claim_ready());
        let ack = p.poll(&mut a, now).unwrap();
        assert!(!p.claim_ready());
        assert_eq!(ack.body["acks"][0]["status"], "ok");
        let mut v = view(2, "desktop", json!([]));
        v["commands"] = json!([{"commandId":"other","status":"ok"}]);
        p.consume(&ack, v, &mut a, now).unwrap();
        assert!(!p.claim_ready());
        let poll = p.poll(&mut a, now).unwrap();
        let mut v = view(2, "desktop", json!([]));
        v["commands"] = json!([{"commandId":"claim","status":"ok"}]);
        p.consume(&poll, v, &mut a, now).unwrap();
        assert!(p.claim_ready());
        assert_eq!(a.starts, 0);
    }
    #[test]
    fn lease_uses_send_time_and_expires_at_boundary() {
        let (mut p, mut a, now) = registered("desktop");
        p.play(track(), &mut a, now).unwrap();
        let permit = a.permit.clone().unwrap();
        assert!(permit.is_valid_at(now + Duration::from_millis(5999)));
        assert!(!permit.is_valid_at(now + Duration::from_secs(6)));
        p.tick(&mut a, now + Duration::from_secs(6));
        assert_eq!(a.state["playing"], false);
        assert!(!p.is_owner(now));
    }
    #[test]
    fn delayed_response_does_not_mint_a_fresh_lease() {
        let mut p = Player::new();
        let mut a = Audio::default();
        let now = Instant::now();
        let r = p.register("Desktop", now).unwrap();
        p.consume(
            &r,
            view(1, "desktop", json!([])),
            &mut a,
            now + Duration::from_secs(7),
        )
        .unwrap();
        assert!(!p.is_owner(now + Duration::from_secs(7)));
        assert_eq!(a.starts, 0);
    }
    #[test]
    fn claim_is_only_self_unselected_and_never_forced() {
        let (mut p, mut a, now) = registered("phone");
        assert!(matches!(
            p.claim("fresh", now),
            Err(Error::OtherOutputSelected)
        ));
        assert!(matches!(
            p.play(track(), &mut a, now),
            Err(Error::OtherOutputSelected)
        ));
        let (mut p, mut a, now) = registered("");
        let r = p.claim("fresh", now).unwrap();
        assert_eq!(
            r.body,
            json!({"action":"transfer","deviceId":"desktop","targetDeviceId":"desktop",
            "epoch":1,"commandId":"fresh","selectionSeq":1,"requireUnselected":true})
        );
        p.failed(&mut a); // Includes HTTP 409. A new claim is not automatically submitted.
        assert!(matches!(p.claim("next", now), Err(Error::NoAuthority)));
        assert!(matches!(
            p.consume(&r, view(2, "desktop", json!([])), &mut a, now),
            Err(Error::Stale)
        ));
    }
    #[test]
    fn self_claim_prepare_activate_and_atomic_ack_state() {
        let (mut p, mut a, now) = registered("");
        let r = p.claim("fresh", now).unwrap();
        p.consume(&r, view(1, "", json!([])), &mut a, now).unwrap();
        let r = p.poll(&mut a, now).unwrap();
        p.consume(
            &r,
            view(
                1,
                "",
                json!([{"deliveryId":"prep","epoch":1,"type":"prepare"}]),
            ),
            &mut a,
            now,
        )
        .unwrap();
        let r = p.poll(&mut a, now).unwrap();
        assert_eq!(
            r.body["acks"],
            json!([{"deliveryId":"prep","epoch":1,"status":"ok"}])
        );
        assert!(r.body.get("state").is_none());
        p.consume(
            &r,
            view(
                2,
                "desktop",
                json!([{"deliveryId":"activate","epoch":2,"type":"activate","state":track()}]),
            ),
            &mut a,
            now,
        )
        .unwrap();
        assert_eq!(a.starts, 0); // recovery is paused, explicit play only
        let r = p.poll(&mut a, now).unwrap();
        assert_eq!(r.body["acks"][0]["status"], "ok");
        assert_eq!(r.body["state"]["playing"], false);
        assert_eq!(r.body["stateSeq"], 1);
    }
    #[test]
    fn release_stops_but_ack_preserves_final_playing_intent() {
        let (mut p, mut a, now) = registered("desktop");
        p.play(track(), &mut a, now).unwrap();
        let old = a.permit.clone().unwrap();
        let r = p.poll(&mut a, now).unwrap();
        p.consume(
            &r,
            view(
                1,
                "desktop",
                json!([{"deliveryId":"release","epoch":1,"type":"release"}]),
            ),
            &mut a,
            now,
        )
        .unwrap();
        assert!(!old.is_valid());
        assert_eq!(a.state["playing"], false);
        let r = p.poll(&mut a, now).unwrap();
        assert_eq!(r.body["state"]["playing"], true);
        assert_eq!(r.body["acks"][0]["status"], "ok");
        p.consume(&r, view(1, "desktop", json!([])), &mut a, now)
            .unwrap();
        assert!(!p.is_owner(now));
    }
    #[test]
    fn commands_deduplicate_and_local_actions_publish_without_command_request() {
        let (mut p, mut a, now) = registered("desktop");
        p.play(track(), &mut a, now).unwrap();
        let r = p.poll(&mut a, now).unwrap();
        assert_eq!(r.body["action"], "poll");
        let directive =
            json!([{"deliveryId":"pause","epoch":1,"type":"command","command":{"type":"pause"}}]);
        p.consume(&r, view(1, "desktop", directive.clone()), &mut a, now)
            .unwrap();
        assert_eq!(a.state["playing"], false);
        let r = p.poll(&mut a, now).unwrap();
        assert_eq!(r.body["acks"][0]["status"], "ok");
        p.consume(&r, view(1, "desktop", directive), &mut a, now)
            .unwrap();
        assert!(!p.pending_acks());
    }
    #[test]
    fn wrong_epoch_fences_and_old_authority_never_revives() {
        let (mut p, mut a, now) = registered("desktop");
        p.play(track(), &mut a, now).unwrap();
        let old = a.permit.clone().unwrap();
        let r = p.poll(&mut a, now).unwrap();
        assert!(
            p.consume(
                &r,
                view(
                    2,
                    "desktop",
                    json!([{"deliveryId":"bad","epoch":1,"type":"activate"}])
                ),
                &mut a,
                now
            )
            .is_err()
        );
        assert!(!old.is_valid());
        assert_eq!(a.state["playing"], false);
        let r = p.poll(&mut a, now).unwrap();
        p.consume(&r, view(2, "desktop", json!([])), &mut a, now)
            .unwrap();
        assert!(!old.is_valid());
        assert_eq!(a.starts, 1);
    }
    #[test]
    fn conflict_view_updates_epoch_but_never_grants_or_executes() {
        let (mut p, mut a, now) = registered("desktop");
        p.play(track(), &mut a, now).unwrap();
        let conflict = view(
            3,
            "desktop",
            json!([{"deliveryId":"never","epoch":3,"type":"activate","state":track()}]),
        );
        p.reconcile(conflict, &mut a, now).unwrap();
        assert_eq!(p.epoch(), 3);
        assert!(!p.is_owner(now));
        assert_eq!(a.starts, 1);
        assert!(!p.pending_acks());
        let r = p.poll(&mut a, now).unwrap();
        assert_eq!(r.body["epoch"], 3);
        assert!(r.body.get("state").is_none());
    }
    #[test]
    fn unsupported_prepare_is_rejected_without_starting_audio() {
        let (mut p, mut a, now) = registered("");
        let mut state = track();
        state["context"] = json!({"kind":"album","id":"a","name":"Album"});
        let r = p.poll(&mut a, now).unwrap();
        p.consume(
            &r,
            view(
                1,
                "",
                json!([{"deliveryId":"prepare","epoch":1,"type":"prepare","state":state}]),
            ),
            &mut a,
            now,
        )
        .unwrap();
        let r = p.poll(&mut a, now).unwrap();
        assert_eq!(r.body["acks"][0]["status"], "rejected");
        assert!(r.body.get("state").is_none());
        assert_eq!(a.starts, 0);
    }
    #[test]
    fn heartbeat_renews_live_permit_but_failure_revokes_it() {
        let (mut p, mut a, now) = registered("desktop");
        p.play(track(), &mut a, now).unwrap();
        let permit = a.permit.clone().unwrap();
        let later = now + Duration::from_secs(1);
        let r = p.poll(&mut a, later).unwrap();
        p.consume(&r, view(1, "desktop", json!([])), &mut a, later)
            .unwrap();
        assert!(permit.is_valid_at(now + Duration::from_millis(6500)));
        assert!(!permit.is_valid_at(now + Duration::from_secs(7)));
        p.failed(&mut a);
        assert!(!permit.is_valid());
        assert_eq!(a.state["playing"], false);
    }
    #[test]
    fn failure_outcomes_must_match_the_actual_snapshot() {
        let mut state = paused(track());
        state["error"] = json!("blocked");
        assert!(valid_outcome(&Outcome {
            status: Status::Blocked,
            state: state.clone()
        }));
        assert!(!valid_outcome(&Outcome {
            status: Status::Media,
            state: state.clone()
        }));
        state["playing"] = json!(true);
        assert!(!valid_outcome(&Outcome {
            status: Status::Blocked,
            state
        }));
    }
    #[test]
    fn positive_projection_seconds_and_reject_unsupported_queues() {
        let s = track();
        assert!(valid_snapshot(&s));
        assert_eq!(s["position"], 0);
        assert_eq!(s["duration"], 123.0);
        let encoded = s.to_string();
        for secret in ["url", "selectionToken", "libraryPath", "secret"] {
            assert!(!encoded.contains(secret));
        }
        let mut bad = s.clone();
        bad["queue"]
            .as_array_mut()
            .unwrap()
            .push(s["queue"][0].clone());
        assert!(!valid_snapshot(&bad));
        let mut bad = s;
        bad["queue"][0]["key"] = json!(0);
        assert!(!valid_snapshot(&bad));
    }
}
