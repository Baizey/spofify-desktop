//! Server-authoritative Library playback v2. No dependency on Engine or native audio.

use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{mpsc, watch},
    time,
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        protocol::{Role, WebSocketConfig},
    },
};

use crate::api::{Api, ApiError, Session};

pub(crate) const SUBPROTOCOL: &str = "spofify.library-playback.v2";

/// Return value of `Api::playback_upgrade(&Session)`. Api owns the ticket POST,
/// credentialed same-origin reqwest upgrade and handshake validation; this module
/// owns WebSocket framing. Do not derive Debug or log the ticket.
pub(crate) struct PlaybackUpgrade {
    pub(crate) upgraded: reqwest::Upgraded,
    pub(crate) ticket: String,
}

/// Caller supplies an opaque logical id, stable across reconnects. This module
/// neither collects device identifiers nor persists account or playback state.
#[derive(Clone)]
pub(crate) struct DeviceIdentity {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) can_play: bool,
}

/// Account-scoped live socket incarnation, not the wire connection id or an output version.
pub(crate) type Generation = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Link {
    Connecting,
    Connected(Generation),
    Disconnected,
    Expired,
    Closed,
}

/// Observe `link` independently of the event queue to stop/cancel audio on loss.
/// `events` is bounded; slow consumers cause a disconnect rather than unbounded buffering.
pub(crate) struct Socket {
    pub(crate) link: watch::Receiver<Link>,
    pub(crate) events: mpsc::Receiver<Event>,
    /// Terminal command receipts, including for retired generations. Feedback only.
    pub(crate) settlements: mpsc::Receiver<Event>,
    settlement_credit: mpsc::Sender<Event>,
    outbound: mpsc::Sender<Outgoing>,
    task: tokio::task::JoinHandle<()>,
}

pub(crate) fn spawn(
    api: Arc<Api>,
    session: Session,
    device: DeviceIdentity,
) -> Result<Socket, SocketError> {
    if !valid_text(&device.device_id, 128, false) || !valid_name(&device.name) {
        return Err(SocketError::Invalid);
    }
    let (outbound, input) = mpsc::channel(OUTBOUND_CAPACITY);
    let (events_out, events) = mpsc::channel(8);
    let (settlement_credit, settlements) = mpsc::channel(SETTLEMENT_CAPACITY);
    let (link_out, link) = watch::channel(Link::Connecting);
    let task = tokio::spawn(run(api, session, device, input, events_out, link_out));
    Ok(Socket {
        link,
        events,
        settlements,
        settlement_credit,
        outbound,
        task,
    })
}

impl Socket {
    /// `request_id` is a caller correlation tag, not a wire command id.
    pub(crate) fn command(
        &self,
        generation: Generation,
        request_id: u64,
        command: Command,
        expected_state_version: Option<u64>,
    ) -> Result<(), SocketError> {
        if !command.valid() || !expected_state_version.is_none_or(safe_counter) {
            return Err(SocketError::Invalid);
        }
        self.submit(generation, || {
            let permit = self
                .settlement_credit
                .clone()
                .try_reserve_owned()
                .map_err(admission_error_for_queue)?;
            Ok(Output::Command {
                settlement: CommandSettlement::new(permit, generation, request_id),
                command,
                expected_state_version,
            })
        })
    }

    /// ACK `ok` for a release/stopped operation (and blocked/media for failure)
    /// only after cancelling all older pending native work and stopping it.
    pub(crate) fn ack(
        &self,
        generation: Generation,
        binding: Binding,
        status: AckStatus,
    ) -> Result<(), SocketError> {
        if !binding.valid() {
            return Err(SocketError::Invalid);
        }
        self.submit(generation, || Ok(Output::Ack { binding, status }))
    }

    pub(crate) fn report(
        &self,
        generation: Generation,
        binding: Binding,
        report: Report,
    ) -> Result<(), SocketError> {
        if !binding.valid() || !report.valid() {
            return Err(SocketError::Invalid);
        }
        self.submit(generation, || Ok(Output::Report { binding, report }))
    }

    fn submit(
        &self,
        generation: Generation,
        message: impl FnOnce() -> Result<Output, SocketError>,
    ) -> Result<(), SocketError> {
        // Keep admission under the watch read lock: once loss is published,
        // the actor can drain the old queue without racing another admission.
        let link = self.link.borrow();
        if *link != Link::Connected(generation) {
            return Err(SocketError::NotConnected);
        }
        let slot = self
            .outbound
            .try_reserve()
            .map_err(admission_error_for_queue)?;
        slot.send(Outgoing {
            generation,
            queued_at: Instant::now(),
            message: message()?,
        });
        Ok(())
    }
}

fn admission_error_for_queue<T>(error: mpsc::error::TrySendError<T>) -> SocketError {
    match error {
        mpsc::error::TrySendError::Full(_) => SocketError::Busy,
        mpsc::error::TrySendError::Closed(_) => SocketError::NotConnected,
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SocketError {
    Invalid,
    NotConnected,
    Busy,
}

pub(crate) struct Event {
    pub(crate) generation: Generation,
    pub(crate) kind: EventKind,
}

pub(crate) enum EventKind {
    Snapshot {
        connection_id: String,
        device_id: String,
        snapshot: Arc<Snapshot>,
    },
    State(Arc<Snapshot>),
    Playback(Playback),
    /// `deadline` is present only for grants, and is based on an earlier local send.
    Instruction {
        instruction: Instruction,
        deadline: Option<Instant>,
    },
    Lease {
        binding: Binding,
        deadline: Instant,
    },
    /// No authority remains for this binding. Engine must cancel its native work.
    LeaseExpired(Binding),
    /// The actor sent the source ended report with this plan before emitting this
    /// event. Apply the reserved binding at elapsed=0, without extending deadline.
    /// A later matching execute is confirmation, not another native operation.
    NextAdmitted {
        source: Binding,
        plan_id: String,
        next: NextExecution,
        deadline: Instant,
    },
    Result {
        request_id: u64,
        result: CommandResult,
    },
    CommandNotSent {
        request_id: u64,
        error: SocketError,
    },
    /// Not a rejection: a sent command can have committed despite transport loss.
    CommandUncertain {
        request_id: u64,
    },
}

/// Exact executor/operation fence within a socket generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Binding {
    pub(crate) instruction_id: String,
    pub(crate) output_version: u64,
    pub(crate) event_id: String,
}

impl Binding {
    fn valid(&self) -> bool {
        positive(self.output_version)
            && valid_text(&self.instruction_id, 128, false)
            && valid_text(&self.event_id, 128, true)
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum AckStatus {
    Ok,
    Blocked,
    Media,
    Rejected,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Report {
    pub(crate) elapsed: f64,
    pub(crate) duration: f64,
    pub(crate) is_playing: bool,
    pub(crate) is_buffering: bool,
    pub(crate) is_loading: bool,
    pub(crate) error: MediaError,
    pub(crate) ended: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) plan_id: Option<String>,
}

pub(crate) const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_LEASE_MS: u64 = 8_000;
const MAX_INBOUND: usize = 5 * 1024 * 1024;
const MAX_OUTBOUND: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum MediaError {
    #[default]
    #[serde(rename = "")]
    None,
    Blocked,
    Media,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Repeat {
    Off,
    All,
    One,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Intent {
    Low,
    High,
    Next,
    Now,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ContextKind {
    Album,
    Artist,
    Playlist,
    Likes,
    Derived,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Context {
    pub(crate) kind: ContextKind,
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) revision: Option<u64>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) fence: Option<String>,
}

#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TrackRef {
    pub(crate) id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) entry_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) occurrence_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) selection_token: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum Selection {
    Track {
        track: TrackRef,
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<Context>,
    },
    Collection {
        context: Context,
    },
    Occurrence {
        key: u64,
    },
}

impl Selection {
    pub(crate) fn track(id: String) -> Self {
        Self::Track {
            track: TrackRef {
                id,
                ..TrackRef::default()
            },
            context: None,
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum Command {
    QueueIntent {
        intent: Intent,
        selection: Selection,
        #[serde(skip_serializing_if = "Option::is_none")]
        shuffle: Option<bool>,
    },
    SetShuffle {
        shuffle: bool,
    },
    SetRepeat {
        repeat: Repeat,
    },
    Seek {
        value: f64,
    },
    Jump {
        key: u64,
    },
    Remove {
        key: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        undo: Option<bool>,
    },
    UndoRemove {
        token: String,
    },
    Move {
        key: u64,
        after_key: Option<u64>,
    },
    Select {
        device_id: String,
    },
    Rename {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        device_id: Option<String>,
    },
    Pause,
    Resume,
    Next,
    Previous,
    Retry,
    Clear,
    Close,
    Deselect,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Artist {
    #[serde(default, deserialize_with = "present")]
    pub(crate) id: Option<String>,
    pub(crate) name: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Track {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) artists: Vec<Artist>,
    pub(crate) album_name: String,
    pub(crate) album_id: String,
    pub(crate) duration: f64,
    pub(crate) key: u64,
    pub(crate) order: u64,
    pub(crate) intent: Intent,
    #[serde(default, deserialize_with = "present")]
    pub(crate) entry_id: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) target_id: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) occurrence_id: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) playback_context: Option<Context>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Occurrence {
    pub(crate) key: u64,
    pub(crate) track_id: String,
    pub(crate) event_id: String,
    pub(crate) progress: f64,
    pub(crate) reported: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct State {
    pub(crate) version: u64,
    pub(crate) queue: Vec<Track>,
    pub(crate) position: i64,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) context: Option<Context>,
    pub(crate) shuffle: bool,
    pub(crate) repeat: Repeat,
    pub(crate) desired_playing: bool,
    pub(crate) high_keys: Vec<u64>,
    pub(crate) elapsed: f64,
    pub(crate) duration: f64,
    pub(crate) is_playing: bool,
    pub(crate) is_buffering: bool,
    pub(crate) is_loading: bool,
    pub(crate) error: MediaError,
    pub(crate) truncated: bool,
    pub(crate) notice: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) occurrence: Option<Occurrence>,
}

impl State {
    pub(crate) fn current(&self) -> Option<&Track> {
        self.queue.get(usize::try_from(self.position).ok()?)
    }

    pub(crate) fn event_id(&self) -> &str {
        self.occurrence
            .as_ref()
            .map_or("", |occurrence| &occurrence.event_id)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Device {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) available: bool,
    pub(crate) can_play: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TransferPhase {
    Preparing,
    Releasing,
    Draining,
    Activating,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Transfer {
    pub(crate) source_device_id: String,
    pub(crate) target_device_id: String,
    pub(crate) source_connection_id: String,
    pub(crate) target_connection_id: String,
    pub(crate) phase: TransferPhase,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Playback {
    pub(crate) output_version: u64,
    pub(crate) state_version: u64,
    pub(crate) playback_sequence: u64,
    pub(crate) event_id: String,
    pub(crate) has_reported_playback: bool,
    pub(crate) elapsed: f64,
    pub(crate) duration: f64,
    pub(crate) is_playing: bool,
    pub(crate) is_buffering: bool,
    pub(crate) is_loading: bool,
    pub(crate) error: MediaError,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Snapshot {
    pub(crate) session_id: String,
    pub(crate) state_version: u64,
    pub(crate) output_version: u64,
    pub(crate) state: State,
    pub(crate) playback: Playback,
    pub(crate) devices: Vec<Device>,
    pub(crate) selected_device_id: String,
    pub(crate) selected_connection_id: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) transfer: Option<Transfer>,
    pub(crate) drain_until_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum InstructionKind {
    Prepare,
    Release,
    Activate,
    Execute,
    StageNext,
    CancelNext,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Instruction {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) kind: InstructionKind,
    pub(crate) output_version: u64,
    pub(crate) event_id: String,
    pub(crate) elapsed: f64,
    pub(crate) desired_playing: bool,
    #[serde(default, deserialize_with = "present")]
    pub(crate) command_id: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) track: Option<Track>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) lease: Option<LeaseTiming>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) plan_id: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) current_instruction_id: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) next: Option<NextExecution>,
}

impl Instruction {
    pub(crate) fn binding(&self) -> Binding {
        Binding {
            output_version: self.output_version,
            event_id: self.event_id.clone(),
            instruction_id: self.id.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NextExecution {
    pub(crate) event_id: String,
    pub(crate) instruction_id: String,
    pub(crate) track: Track,
}

impl NextExecution {
    pub(crate) fn binding(&self, output_version: u64) -> Binding {
        Binding {
            output_version,
            event_id: self.event_id.clone(),
            instruction_id: self.instruction_id.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LeaseTiming {
    pub(crate) client_sequence: u64,
    pub(crate) duration_ms: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Renewal {
    output_version: u64,
    event_id: String,
    client_sequence: u64,
    duration_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Outcome {
    Resolving,
    Accepted,
    Rejected,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResultCode {
    #[serde(rename = "")]
    None,
    StaleStateVersion,
    StaleRevision,
    Conflict,
    Expired,
    Invalid,
    Unavailable,
    Capacity,
    Superseded,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Placement {
    None,
    Low,
    High,
    Next,
    Replace,
    Existing,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Execution {
    Unchanged,
    Pending,
    Confirmed,
    Blocked,
    Media,
    Uncertain,
    Superseded,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct UndoCapability {
    pub(crate) token: String,
    pub(crate) expires_at_ms: u64,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CommandResult {
    pub(crate) id: String,
    pub(crate) outcome: Outcome,
    pub(crate) code: ResultCode,
    pub(crate) state_version: u64,
    pub(crate) accepted_count: u64,
    pub(crate) placement: Placement,
    pub(crate) execution: Execution,
    #[serde(rename = "final")]
    pub(crate) is_final: bool,
    #[serde(default, deserialize_with = "present")]
    pub(crate) undo: Option<UndoCapability>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtocolError {
    code: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum Incoming {
    Snapshot {
        session_id: String,
        connection_id: String,
        device_id: String,
        snapshot: Snapshot,
    },
    State {
        session_id: String,
        snapshot: Snapshot,
    },
    Playback {
        session_id: String,
        playback: Playback,
    },
    Instruction {
        session_id: String,
        instruction: Box<Instruction>,
    },
    Lease {
        session_id: String,
        lease: Renewal,
    },
    Result {
        session_id: String,
        result: CommandResult,
    },
    Error {
        session_id: String,
        error: ProtocolError,
    },
}

impl Incoming {
    fn session_id(&self) -> &str {
        match self {
            Self::Snapshot { session_id, .. }
            | Self::State { session_id, .. }
            | Self::Playback { session_id, .. }
            | Self::Instruction { session_id, .. }
            | Self::Lease { session_id, .. }
            | Self::Result { session_id, .. }
            | Self::Error { session_id, .. } => session_id,
        }
    }
}

fn present<'de, D, T>(input: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(input).map(Some)
}

fn safe_counter(n: u64) -> bool {
    n <= MAX_SAFE_INTEGER
}
fn positive(n: u64) -> bool {
    n > 0 && safe_counter(n)
}
fn seconds(n: f64) -> bool {
    (0.0..=604_800.0).contains(&n)
}
fn valid_text(text: &str, max: usize, allow_empty: bool) -> bool {
    text.len() <= max
        && (allow_empty || !text.trim().is_empty())
        && !text.chars().any(char::is_control)
}
fn valid_name(name: &str) -> bool {
    let trimmed = name.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    valid_text(name, 256, false) && !trimmed.is_empty() && trimmed.chars().count() <= 64
}
fn optional_text(value: Option<&str>, max: usize, allow_empty: bool) -> bool {
    value.is_none_or(|s| valid_text(s, max, allow_empty))
}
fn decimal_id(id: &str) -> Option<u64> {
    if id.starts_with('0') || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    id.parse().ok().filter(|n| positive(*n))
}

impl Context {
    fn valid(&self) -> bool {
        valid_text(&self.id, 512, false)
            && valid_text(&self.name, 512, true)
            && self.revision.is_none_or(safe_counter)
            && optional_text(self.fence.as_deref(), 4096, true)
    }
}

impl Selection {
    fn valid(&self) -> bool {
        match self {
            Self::Occurrence { key } => positive(*key),
            Self::Collection { context } => context.valid(),
            Self::Track { track, context } => {
                if !valid_text(&track.id, 512, false)
                    || !context.as_ref().is_none_or(Context::valid)
                    || !optional_text(track.entry_id.as_deref(), 512, false)
                    || !optional_text(track.target_id.as_deref(), 512, false)
                    || !optional_text(track.occurrence_id.as_deref(), 4096, false)
                    || !optional_text(track.selection_token.as_deref(), 8192, false)
                {
                    return false;
                }
                match context.as_ref() {
                    Some(context) if context.kind == ContextKind::Derived => {
                        context.revision.is_some()
                            && optional_text(context.fence.as_deref(), 4096, false)
                            && context.fence.is_some()
                            && track.target_id.as_deref() == Some(context.id.as_str())
                            && track.occurrence_id.is_some()
                            && track.selection_token.is_some()
                            && track.entry_id.is_none()
                    }
                    _ => {
                        track.target_id.is_none()
                            && track.occurrence_id.is_none()
                            && track.selection_token.is_none()
                            && (track.entry_id.is_some()
                                == context
                                    .as_ref()
                                    .is_some_and(|c| c.kind == ContextKind::Playlist))
                    }
                }
            }
        }
    }
}

impl Command {
    fn valid(&self) -> bool {
        match self {
            Self::QueueIntent {
                intent,
                selection,
                shuffle,
            } => {
                selection.valid()
                    && (shuffle.is_none()
                        || *intent == Intent::Now
                            && matches!(selection, Selection::Collection { .. }))
            }
            Self::Seek { value } => seconds(*value),
            Self::Jump { key } | Self::Remove { key, .. } => positive(*key),
            Self::Move { key, after_key } => {
                positive(*key) && after_key.is_none_or(|anchor| positive(anchor) && anchor != *key)
            }
            Self::UndoRemove { token } => valid_text(token, 128, false),
            Self::Select { device_id } => valid_text(device_id, 128, false),
            Self::Rename { name, device_id } => {
                valid_name(name) && optional_text(device_id.as_deref(), 128, false)
            }
            _ => true,
        }
    }

    fn can_stop(&self) -> bool {
        matches!(self, Self::Pause | Self::Close | Self::Deselect)
    }
}

impl Report {
    fn valid(&self) -> bool {
        seconds(self.elapsed)
            && seconds(self.duration)
            && (!self.is_playing || !self.ended && self.error == MediaError::None)
            && self
                .plan_id
                .as_ref()
                .is_none_or(|id| self.ended && valid_text(id, 128, false))
    }
}

impl Track {
    fn valid(&self) -> bool {
        valid_text(&self.id, 512, false)
            && positive(self.key)
            && safe_counter(self.order)
            && seconds(self.duration)
            && valid_text(&self.name, 512, true)
            && valid_text(&self.album_name, 512, true)
            && valid_text(&self.album_id, 512, true)
            && self.artists.len() <= 64
            && self.artists.iter().all(|a| {
                valid_text(&a.name, 512, true) && optional_text(a.id.as_deref(), 512, true)
            })
            && optional_text(self.entry_id.as_deref(), 512, true)
            && optional_text(self.target_id.as_deref(), 512, true)
            && optional_text(self.occurrence_id.as_deref(), 4096, true)
            && self.playback_context.as_ref().is_none_or(Context::valid)
    }
}

impl Playback {
    fn valid(&self) -> bool {
        positive(self.output_version)
            && positive(self.state_version)
            && safe_counter(self.playback_sequence)
            && valid_text(&self.event_id, 128, true)
            && seconds(self.elapsed)
            && seconds(self.duration)
    }

    fn matches(&self, snapshot: &Snapshot) -> bool {
        self.output_version == snapshot.output_version
            && self.state_version == snapshot.state_version
            && self.event_id == snapshot.state.event_id()
    }
}

impl Snapshot {
    fn valid(&self, session_id: &str) -> bool {
        let state = &self.state;
        if self.session_id != session_id
            || !self.playback.valid()
            || !self.playback.matches(self)
            || !safe_counter(self.drain_until_ms)
            || state.version != 1
            || state.truncated
            || !seconds(state.elapsed)
            || !seconds(state.duration)
            || !valid_text(&state.notice, 512, true)
            || !state.context.as_ref().is_none_or(Context::valid)
            || self.devices.len() > 128
        {
            return false;
        }
        match (state.current(), &state.occurrence) {
            (None, None) if state.queue.is_empty() && state.position == -1 => {}
            (Some(track), Some(occurrence))
                if track.key == occurrence.key
                    && track.id == occurrence.track_id
                    && valid_text(&occurrence.event_id, 128, false)
                    && occurrence.progress.is_finite()
                    && occurrence.progress >= 0.0 => {}
            _ => return false,
        }
        let mut keys = HashSet::with_capacity(state.queue.len());
        if !state
            .queue
            .iter()
            .all(|track| track.valid() && keys.insert(track.key))
        {
            return false;
        }
        let upcoming = state
            .queue
            .iter()
            .skip((state.position + 1) as usize)
            .map(|track| track.key)
            .collect::<HashSet<_>>();
        keys.clear();
        if !state
            .high_keys
            .iter()
            .all(|key| upcoming.contains(key) && keys.insert(*key))
        {
            return false;
        }
        let mut device_ids = HashSet::with_capacity(self.devices.len());
        if !self.devices.iter().all(|device| {
            valid_text(&device.device_id, 128, false)
                && valid_name(&device.name)
                && device_ids.insert(device.device_id.as_str())
        }) {
            return false;
        }
        let endpoint = |device: &str, connection: &str| {
            (device.is_empty() && connection.is_empty())
                || (device_ids.contains(device) && valid_text(connection, 128, false))
        };
        endpoint(&self.selected_device_id, &self.selected_connection_id)
            && self.transfer.as_ref().is_none_or(|transfer| {
                endpoint(&transfer.source_device_id, &transfer.source_connection_id)
                    && endpoint(&transfer.target_device_id, &transfer.target_connection_id)
                    && !transfer.target_connection_id.is_empty()
            })
    }
}

impl LeaseTiming {
    fn valid(self) -> bool {
        positive(self.client_sequence) && (1..=MAX_LEASE_MS).contains(&self.duration_ms)
    }
}

impl Instruction {
    fn valid(&self) -> bool {
        use InstructionKind::*;
        if !self.binding().valid()
            || !seconds(self.elapsed)
            || self
                .command_id
                .as_ref()
                .is_some_and(|id| decimal_id(id).is_none())
            || !self
                .track
                .as_ref()
                .is_none_or(|track| !self.event_id.is_empty() && track.valid())
            || !optional_text(self.plan_id.as_deref(), 128, false)
            || !optional_text(self.current_instruction_id.as_deref(), 128, false)
        {
            return false;
        }
        let grant = self.lease.is_some_and(LeaseTiming::valid);
        match self.kind {
            Prepare | Release => {
                self.lease.is_none()
                    && self.plan_id.is_none()
                    && self.current_instruction_id.is_none()
                    && self.next.is_none()
                    && !self.desired_playing
                    && (self.kind != Release || self.track.is_none())
            }
            Activate | Execute => {
                grant
                    && self.current_instruction_id.is_none()
                    && self.next.is_none()
                    && (!self.desired_playing || self.track.is_some())
                    && (self.kind == Execute || self.plan_id.is_none())
            }
            StageNext | CancelNext => {
                self.lease.is_none()
                    && self.track.is_none()
                    && self.command_id.is_none()
                    && self.plan_id.is_some()
                    && self.current_instruction_id.is_some()
                    && !self.event_id.is_empty()
                    && (self.kind == StageNext) == self.next.is_some()
                    && self.next.as_ref().is_none_or(|next| {
                        valid_text(&next.event_id, 128, false)
                            && next.event_id != self.event_id
                            && valid_text(&next.instruction_id, 128, false)
                            && Some(&next.instruction_id) != self.current_instruction_id.as_ref()
                            && next.track.valid()
                    })
            }
        }
    }
}

impl CommandResult {
    fn valid(&self) -> bool {
        decimal_id(&self.id).is_some()
            && positive(self.state_version)
            && safe_counter(self.accepted_count)
            && self.undo.as_ref().is_none_or(|undo| {
                self.outcome == Outcome::Accepted
                    && valid_text(&undo.token, 128, false)
                    && positive(undo.expires_at_ms)
            })
    }
}

struct Outgoing {
    generation: Generation,
    queued_at: Instant,
    message: Output,
}

enum Output {
    Command {
        settlement: CommandSettlement,
        command: Command,
        expected_state_version: Option<u64>,
    },
    Ack {
        binding: Binding,
        status: AckStatus,
    },
    Report {
        binding: Binding,
        report: Report,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Hello<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    ticket: &'a str,
    name: &'a str,
    device_id: &'a str,
    can_play: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WireCommand<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_state_version: Option<u64>,
    command: &'a Command,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WireAck<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    instruction_id: &'a str,
    output_version: u64,
    event_id: &'a str,
    client_sequence: u64,
    status: AckStatus,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WireReport<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    instruction_id: &'a str,
    output_version: u64,
    event_id: &'a str,
    client_sequence: u64,
    #[serde(flatten)]
    report: &'a Report,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Transport,
    Protocol,
    Expired,
    ReceiverClosed,
}

impl End {
    fn link(self) -> Link {
        match self {
            Self::Expired => Link::Expired,
            Self::ReceiverClosed => Link::Closed,
            Self::Transport | Self::Protocol => Link::Disconnected,
        }
    }
}

const COMMAND_MAX_WAIT: Duration = Duration::from_millis(500);
const PENDING_COMMANDS: usize = 32;
const OUTBOUND_CAPACITY: usize = 16;
// Room for the whole pipeline (including the extra pending stop slot) when
// receipts are drained. Unread terminal events also count against this bound.
const SETTLEMENT_CAPACITY: usize = PENDING_COMMANDS + 1 + OUTBOUND_CAPACITY;
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);
// Go pings every five seconds. A silent half-open connection cannot remain ready.
const SOCKET_IDLE: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(18);
const RETRY_MIN: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(8);

struct CommandSettlement {
    permit: Option<mpsc::OwnedPermit<Event>>,
    generation: Generation,
    request_id: u64,
    write_started: bool,
}

impl CommandSettlement {
    fn new(permit: mpsc::OwnedPermit<Event>, generation: Generation, request_id: u64) -> Self {
        Self {
            permit: Some(permit),
            generation,
            request_id,
            write_started: false,
        }
    }

    fn not_sent(mut self, error: SocketError) {
        self.emit(EventKind::CommandNotSent {
            request_id: self.request_id,
            error,
        });
    }

    fn result(mut self, result: CommandResult) {
        self.emit(EventKind::Result {
            request_id: self.request_id,
            result,
        });
    }

    fn emit(&mut self, kind: EventKind) {
        if let Some(permit) = self.permit.take() {
            permit.send(Event {
                generation: self.generation,
                kind,
            });
        }
    }
}

impl Drop for CommandSettlement {
    fn drop(&mut self) {
        // Cancellation can drop either the outbound receive queue or a live
        // machine. A started write may have reached the server even on error.
        self.emit(if self.write_started {
            EventKind::CommandUncertain {
                request_id: self.request_id,
            }
        } else {
            EventKind::CommandNotSent {
                request_id: self.request_id,
                error: SocketError::NotConnected,
            }
        });
    }
}

struct PendingCommand {
    settlement: CommandSettlement,
    asked_undo: bool,
    blocks_plan: bool,
}

#[derive(Clone)]
struct Operation {
    binding: Binding,
    kind: InstructionKind,
    track: Option<(String, u64)>,
    desired_playing: bool,
}

impl From<&Instruction> for Operation {
    fn from(instruction: &Instruction) -> Self {
        Self {
            binding: instruction.binding(),
            kind: instruction.kind,
            desired_playing: instruction.desired_playing,
            track: instruction
                .track
                .as_ref()
                .map(|track| (track.id.clone(), track.key)),
        }
    }
}

struct Ready {
    operation: Operation,
    basis: u64,
}

struct ExecutionState {
    operation: Operation,
    deadline: Option<Instant>,
    through_sequence: u64,
    parked: bool,
    last_parked_report: Option<Report>,
    ended: bool,
    /// A planned-next operation starts only after the actor sends source ended.
    admission: Option<(Binding, String)>,
}

#[derive(Clone)]
struct Plan {
    source: Binding,
    id: String,
    next: NextExecution,
}

struct Machine {
    session_id: String,
    connection_id: String,
    snapshot: Arc<Snapshot>,
    playback_sequence: u64,
    future_playback: Option<Playback>,
    output_version: u64,
    command_id: u64,
    pending: BTreeMap<u64, PendingCommand>,
    client_sequence: u64,
    sent: VecDeque<(u64, Instant)>,
    // Entries remain until disconnect (bounded by the connection lifetime cap),
    // so an old delivery cannot resurrect execution after a burst of frames.
    seen: HashSet<String>,
    ack_due: Option<Operation>,
    release_ack: Option<Operation>,
    // At most one final source observation, before release ACK and before the
    // inherited deadline. It cannot renew authority or advance traversal.
    retiring: Option<(Binding, Instant)>,
    ready: Option<Ready>,
    execution: Option<ExecutionState>,
    plan: Option<Plan>,
    retired_plans: HashSet<String>,
    /// A cancelled speculative start cannot play/renew, but a *different*
    /// canonical execution can replace it under the source output fence.
    cancelled_next: Option<(Binding, Instant)>,
}

impl Machine {
    fn admit(message: Incoming, requested_device: &str) -> Result<(Self, EventKind), End> {
        let Incoming::Snapshot {
            session_id,
            connection_id,
            device_id,
            snapshot,
        } = message
        else {
            return Err(End::Protocol);
        };
        if !valid_text(&session_id, 128, false)
            || !valid_text(&connection_id, 128, false)
            || device_id != requested_device
            || !snapshot.valid(&session_id)
            || !snapshot
                .devices
                .iter()
                .any(|device| device.device_id == device_id)
        {
            return Err(End::Protocol);
        }
        let snapshot = Arc::new(snapshot);
        let machine = Self {
            session_id,
            connection_id: connection_id.clone(),
            output_version: snapshot.output_version,
            playback_sequence: snapshot.playback.playback_sequence,
            snapshot: snapshot.clone(),
            future_playback: None,
            command_id: 0,
            pending: BTreeMap::new(),
            client_sequence: 0,
            sent: VecDeque::new(),
            seen: HashSet::new(),
            ack_due: None,
            release_ack: None,
            retiring: None,
            ready: None,
            execution: None,
            plan: None,
            retired_plans: HashSet::new(),
            cancelled_next: None,
        };
        Ok((
            machine,
            EventKind::Snapshot {
                connection_id,
                device_id,
                snapshot,
            },
        ))
    }

    fn target(snapshot: &Snapshot, connection_id: &str) -> bool {
        snapshot.selected_connection_id == connection_id
            || snapshot
                .transfer
                .as_ref()
                .is_some_and(|transfer| transfer.target_connection_id == connection_id)
    }

    fn receive(
        &mut self,
        incoming: Incoming,
        now: Instant,
        events: &mut Vec<EventKind>,
    ) -> Result<(), End> {
        if incoming.session_id() != self.session_id {
            return Err(End::Protocol);
        }
        match incoming {
            Incoming::Snapshot { .. } => return Err(End::Protocol),
            Incoming::State { mut snapshot, .. } => {
                if !snapshot.valid(&self.session_id) {
                    return Err(End::Protocol);
                }
                if snapshot.output_version < self.output_version
                    || snapshot.state_version < self.snapshot.state_version
                {
                    return Ok(());
                }
                if snapshot.state_version == self.snapshot.state_version {
                    self.playback(snapshot.playback, events);
                    return Ok(());
                }
                if snapshot.output_version > self.output_version
                    || !Self::target(&snapshot, &self.connection_id)
                {
                    self.revoke(events);
                    self.ready = None;
                    self.ack_due = None;
                }
                self.output_version = snapshot.output_version;
                if self.future_playback.as_ref().is_some_and(|future| {
                    future.matches(&snapshot)
                        && future.playback_sequence > snapshot.playback.playback_sequence
                }) {
                    snapshot.playback = self.future_playback.take().expect("matching anchor");
                }
                self.future_playback = self.future_playback.take().filter(|future| {
                    future.state_version >= snapshot.state_version
                        && future.output_version >= snapshot.output_version
                        && (future.state_version > snapshot.state_version
                            || future.output_version > snapshot.output_version)
                });
                self.playback_sequence = snapshot.playback.playback_sequence;
                self.snapshot = Arc::new(snapshot);
                events.push(EventKind::State(self.snapshot.clone()));
            }
            Incoming::Playback { playback, .. } => {
                if !playback.valid() {
                    return Err(End::Protocol);
                }
                self.playback(playback, events);
            }
            Incoming::Instruction { instruction, .. } => {
                self.instruction(*instruction, now, events)?
            }
            Incoming::Lease { lease, .. } => self.renewal(lease, now, events)?,
            Incoming::Result { result, .. } => {
                if !result.valid() {
                    return Err(End::Protocol);
                }
                let id = decimal_id(&result.id).ok_or(End::Protocol)?;
                if let Some(pending) = self.pending.get(&id) {
                    if result.undo.is_some() && !pending.asked_undo {
                        return Err(End::Protocol);
                    }
                    // Provisional results are not terminal receipts. Consume the
                    // reserved credit as soon as a final is known, independently
                    // of the main event queue and any subsequent disconnect.
                    if result.is_final {
                        let pending = self.pending.remove(&id).expect("known command");
                        pending.settlement.result(result);
                    }
                }
            }
            Incoming::Error { error, .. } => {
                // Neither upstream text nor diagnostics escape the protocol boundary.
                let _ = (error.code, error.message);
                return Err(End::Protocol);
            }
        }
        Ok(())
    }

    fn playback(&mut self, playback: Playback, events: &mut Vec<EventKind>) {
        let snapshot = &self.snapshot;
        if playback.state_version < snapshot.state_version
            || playback.output_version < snapshot.output_version
            || playback.playback_sequence <= self.playback_sequence
        {
            return;
        }
        if playback.matches(snapshot) {
            self.playback_sequence = playback.playback_sequence;
            events.push(EventKind::Playback(playback));
        } else if (playback.state_version > snapshot.state_version
            || playback.output_version > snapshot.output_version)
            && self
                .future_playback
                .as_ref()
                .is_none_or(|future| playback.playback_sequence > future.playback_sequence)
        {
            self.future_playback = Some(playback);
        }
    }

    fn deadline(&self) -> Option<Instant> {
        self.execution.as_ref()?.deadline
    }

    fn basis(&self, timing: LeaseTiming, now: Instant) -> Option<Instant> {
        let sent_at = self.sent.iter().rev().find_map(|(sequence, sent_at)| {
            (*sequence == timing.client_sequence).then_some(*sent_at)
        })?;
        let deadline = sent_at.checked_add(Duration::from_millis(timing.duration_ms))?;
        (deadline > now).then_some(deadline)
    }

    fn sent(&mut self, sequence: u64, at: Instant) {
        self.client_sequence = sequence;
        self.sent.push_back((sequence, at));
        while self.sent.len() > 128
            || self.sent.front().is_some_and(|(_, timestamp)| {
                at.duration_since(*timestamp) >= Duration::from_millis(MAX_LEASE_MS)
            })
        {
            self.sent.pop_front();
        }
    }

    fn discard_plan(&mut self) {
        if let Some(plan) = self.plan.take() {
            self.retired_plans.insert(plan.id);
        }
    }

    fn revoke(&mut self, events: &mut Vec<EventKind>) {
        self.discard_plan();
        self.cancelled_next = None;
        self.retiring = None;
        if let Some(execution) = self.execution.take() {
            if self
                .ack_due
                .as_ref()
                .is_some_and(|ack| ack.binding == execution.operation.binding)
            {
                self.ack_due = None;
            }
            events.push(EventKind::LeaseExpired(execution.operation.binding));
        }
    }

    fn expire(&mut self, now: Instant, events: &mut Vec<EventKind>) {
        if self.deadline().is_some_and(|deadline| now >= deadline) {
            self.revoke(events);
        }
    }

    fn instruction(
        &mut self,
        instruction: Instruction,
        now: Instant,
        events: &mut Vec<EventKind>,
    ) -> Result<(), End> {
        use InstructionKind::*;
        if !instruction.valid() {
            return Err(End::Protocol);
        }
        self.expire(now, events);
        if instruction.output_version < self.output_version {
            return Ok(());
        }
        if self.seen.contains(&instruction.id) {
            return Ok(());
        }
        // Never evict and accidentally accept an old execution again. Reconnect
        // from a new snapshot if an unusually long incarnation hits this cap.
        if self.seen.len() >= 16_384 {
            return Err(End::Transport);
        }
        self.seen.insert(instruction.id.clone());
        let operation = Operation::from(&instruction);
        let mut deadline = None;
        match instruction.kind {
            StageNext | CancelNext => {
                let (Some(plan_id), Some(source_id)) =
                    (&instruction.plan_id, &instruction.current_instruction_id)
                else {
                    return Err(End::Protocol);
                };
                let source = Binding {
                    instruction_id: source_id.clone(),
                    event_id: instruction.event_id.clone(),
                    output_version: instruction.output_version,
                };
                if instruction.kind == CancelNext {
                    self.retired_plans.insert(plan_id.clone());
                    if self
                        .plan
                        .as_ref()
                        .is_some_and(|plan| plan.id == *plan_id && plan.source == source)
                    {
                        self.discard_plan();
                    }
                    let cancelled = self.execution.as_ref().and_then(|execution| {
                        let (admitted, id) = execution.admission.as_ref()?;
                        if *admitted == source && id == plan_id {
                            Some((execution.operation.binding.clone(), execution.deadline?))
                        } else {
                            None
                        }
                    });
                    if let Some(cancelled) = cancelled {
                        self.revoke(events);
                        self.cancelled_next = Some(cancelled);
                    }
                } else if self.execution.as_ref().is_some_and(|execution| {
                    execution.operation.binding == source
                        && execution.operation.desired_playing
                        && execution.admission.is_none()
                        && !execution.ended
                        && execution.deadline.is_some()
                }) && self.snapshot.selected_connection_id == self.connection_id
                    && !self.retired_plans.contains(plan_id)
                    && !self.pending.values().any(|command| command.blocks_plan)
                {
                    if self.plan.as_ref().is_some_and(|plan| plan.id == *plan_id) {
                        return Ok(());
                    }
                    self.discard_plan();
                    self.plan = instruction.next.clone().map(|next| Plan {
                        source,
                        id: plan_id.clone(),
                        next,
                    });
                } else {
                    return Ok(());
                }
            }
            Prepare => {
                self.revoke(events);
                self.ready = None;
                self.release_ack = None;
                self.ack_due = Some(operation);
                self.output_version = instruction.output_version;
            }
            Release => {
                self.discard_plan();
                self.cancelled_next = None;
                // Engine stops on the targeted Release itself. Retain only a
                // final observation slot, not an executable/renewable binding.
                self.retiring = self.execution.take().and_then(|execution| {
                    let binding = execution.operation.binding;
                    (binding.output_version == instruction.output_version
                        && !execution.ended
                        && execution.admission.is_none())
                    .then_some((binding, execution.deadline?))
                });
                self.ack_due = None;
                self.ready = self.ready.take().filter(|ready| {
                    ready.operation.binding.output_version == instruction.output_version
                        && ready.operation.binding.event_id == instruction.event_id
                });
                self.release_ack = Some(operation);
                self.output_version = instruction.output_version;
            }
            Activate | Execute => {
                let timing = instruction.lease.ok_or(End::Protocol)?;
                let fresh = self.basis(timing, now);
                let next_confirmation = self.execution.as_ref().is_some_and(|current| {
                    current.operation.binding == operation.binding
                        && current.operation.track == operation.track
                        && current
                            .admission
                            .as_ref()
                            .is_some_and(|(_, plan)| Some(plan) == instruction.plan_id.as_ref())
                });
                let can_grant = Self::target(&self.snapshot, &self.connection_id)
                    && match instruction.kind {
                        Activate => self.ready.as_ref().is_some_and(|ready| {
                            ready.basis == timing.client_sequence
                                && ready.operation.binding.output_version
                                    < instruction.output_version
                                && ready.operation.binding.event_id == instruction.event_id
                                && ready.operation.track == operation.track
                        }),
                        Execute => {
                            self.execution.as_ref().is_some_and(|current| {
                                // A stopped ACK retires the local lease, not the selected
                                // binding. Go may have issued this absolute execute before
                                // processing that ACK (e.g. another controller resumed).
                                // Validate its own fresh grant below; renewals still cannot
                                // revive a parked binding.
                                current.operation.binding.output_version
                                    == instruction.output_version
                                    && (current.admission.is_none()
                                        || current.deadline.is_some()
                                            && (next_confirmation
                                                || current.operation.binding != operation.binding))
                            }) || self.cancelled_next.as_ref().is_some_and(
                                |(cancelled, deadline)| {
                                    *deadline > now
                                        && cancelled.output_version
                                            == operation.binding.output_version
                                        && *cancelled != operation.binding
                                },
                            )
                        }
                        _ => false,
                    };
                // Missing or expired basis is never reinterpreted as a fresh lease.
                if !can_grant || fresh.is_none() {
                    return Err(End::Transport);
                }
                deadline = fresh;
                self.ready = None;
                self.cancelled_next = None;
                self.retiring = None;
                self.discard_plan();
                let ended = next_confirmation
                    && self.execution.as_ref().is_some_and(|current| current.ended);
                self.execution = Some(ExecutionState {
                    operation: operation.clone(),
                    deadline,
                    through_sequence: timing.client_sequence,
                    parked: false,
                    last_parked_report: None,
                    ended,
                    admission: None,
                });
                self.ack_due = Some(operation);
                self.output_version = instruction.output_version;
            }
        }
        events.push(EventKind::Instruction {
            instruction,
            deadline,
        });
        Ok(())
    }

    fn renewal(
        &mut self,
        renewal: Renewal,
        now: Instant,
        events: &mut Vec<EventKind>,
    ) -> Result<(), End> {
        let timing = LeaseTiming {
            client_sequence: renewal.client_sequence,
            duration_ms: renewal.duration_ms,
        };
        if !timing.valid()
            || !positive(renewal.output_version)
            || !valid_text(&renewal.event_id, 128, true)
        {
            return Err(End::Protocol);
        }
        self.expire(now, events);
        let Some(current) = &self.execution else {
            return Ok(());
        };
        let binding = &current.operation.binding;
        if current.parked
            || current.admission.is_some()
            || renewal.output_version != binding.output_version
            || renewal.event_id != binding.event_id
            || renewal.client_sequence <= current.through_sequence
        {
            return Ok(());
        }
        if let Some(deadline) = self.basis(timing, now) {
            let binding = binding.clone();
            let active = self.execution.as_mut().expect("current execution");
            active.deadline = Some(deadline);
            active.through_sequence = renewal.client_sequence;
            events.push(EventKind::Lease { binding, deadline });
        }
        Ok(())
    }

    async fn send<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        &mut self,
        outgoing: Outgoing,
        generation: Generation,
        ws: &mut WebSocketStream<S>,
        session: &Session,
        events: &mut Vec<EventKind>,
    ) -> Result<(), End> {
        if outgoing.generation != generation {
            return Ok(());
        }
        let now = Instant::now();
        self.expire(now, events);
        match outgoing.message {
            Output::Command {
                mut settlement,
                command,
                expected_state_version,
            } => {
                let limit = PENDING_COMMANDS + if command.can_stop() { 1 } else { 0 };
                let time_left =
                    COMMAND_MAX_WAIT.saturating_sub(now.duration_since(outgoing.queued_at));
                if time_left.is_zero() || self.pending.len() >= limit {
                    settlement.not_sent(SocketError::Busy);
                    return Ok(());
                }
                let id = self.command_id + 1;
                if !positive(id) {
                    return Err(End::Transport);
                }
                let message = WireCommand {
                    kind: "command",
                    id: id.to_string(),
                    expected_state_version,
                    command: &command,
                };
                let frame = prepare_frame(session, &message, time_left)?;
                self.command_id = id;
                let blocks_plan = !matches!(command, Command::Rename { .. });
                if blocks_plan {
                    self.discard_plan();
                }
                settlement.write_started = true;
                self.pending.insert(
                    id,
                    PendingCommand {
                        settlement,
                        asked_undo: matches!(
                            command,
                            Command::Remove {
                                undo: Some(true),
                                ..
                            }
                        ),
                        blocks_plan,
                    },
                );
                write_frame(ws, frame, time_left).await?;
            }
            Output::Ack { binding, status } => {
                let operation = if self
                    .ack_due
                    .as_ref()
                    .is_some_and(|operation| operation.binding == binding)
                {
                    self.ack_due.take()
                } else if self
                    .release_ack
                    .as_ref()
                    .is_some_and(|operation| operation.binding == binding)
                {
                    self.retiring = None;
                    self.release_ack.take()
                } else {
                    None
                };
                let Some(operation) = operation else {
                    return Ok(());
                };
                let status = if operation.kind == InstructionKind::Release
                    && !matches!(status, AckStatus::Ok | AckStatus::Rejected)
                {
                    AckStatus::Rejected
                } else {
                    status
                };
                let sequence = self.client_sequence + 1;
                if !positive(sequence) {
                    return Err(End::Transport);
                }
                let message = WireAck {
                    kind: "ack",
                    instruction_id: &binding.instruction_id,
                    output_version: binding.output_version,
                    event_id: &binding.event_id,
                    client_sequence: sequence,
                    status,
                };
                let budget = if matches!(
                    operation.kind,
                    InstructionKind::Activate | InstructionKind::Execute
                ) {
                    let Some(deadline) = self.deadline() else {
                        return Ok(());
                    };
                    WRITE_TIMEOUT.min(deadline.saturating_duration_since(now))
                } else {
                    WRITE_TIMEOUT
                };
                let at = transmit(ws, session, &message, budget).await?;
                self.sent(sequence, at);
                match operation.kind {
                    InstructionKind::Prepare => {
                        if status == AckStatus::Ok {
                            self.ready = Some(Ready {
                                operation,
                                basis: sequence,
                            });
                        }
                    }
                    InstructionKind::Release => {
                        if let Some(ready) = self.ready.as_mut().filter(|ready| {
                            ready.operation.binding.output_version == binding.output_version
                                && ready.operation.binding.event_id == binding.event_id
                        }) {
                            if status == AckStatus::Ok {
                                ready.basis = sequence;
                            } else {
                                self.ready = None;
                            }
                        }
                    }
                    InstructionKind::Activate | InstructionKind::Execute => {
                        if status == AckStatus::Rejected {
                            self.revoke(events);
                        } else if status != AckStatus::Ok || !operation.desired_playing {
                            self.discard_plan();
                            if let Some(current) = self.execution.as_mut() {
                                current.parked = true;
                                current.deadline = None;
                            }
                        }
                    }
                    _ => return Err(End::Protocol),
                }
            }
            Output::Report {
                binding,
                mut report,
            } => {
                let (parked, deadline, retiring) = if let Some(current) = self.execution.as_ref() {
                    if current.operation.binding != binding
                        || current.ended
                        || current.parked && (report.is_playing || report.ended)
                        || current.parked && current.last_parked_report.as_ref() == Some(&report)
                    {
                        return Ok(());
                    }
                    (current.parked, current.deadline, false)
                } else if let Some((source, deadline)) = self.retiring.as_ref() {
                    if *source != binding || *deadline <= now || report.ended {
                        return Ok(());
                    }
                    (false, Some(*deadline), true)
                } else {
                    return Ok(());
                };
                if binding.event_id.is_empty() && (report.is_playing || report.ended) {
                    return Ok(());
                }
                let plan = self
                    .plan
                    .as_ref()
                    .filter(|plan| {
                        Some(&plan.id) == report.plan_id.as_ref()
                            && plan.source == binding
                            && report.ended
                            && self.snapshot.selected_connection_id == self.connection_id
                            && deadline.is_some()
                    })
                    .cloned();
                // A queued token can cross a server cancellation. The ended fact
                // remains valid for the source; send it without a revoked hint.
                if plan.is_none() {
                    report.plan_id = None;
                }
                let sequence = self.client_sequence + 1;
                if !positive(sequence) {
                    return Err(End::Transport);
                }
                let message = WireReport {
                    kind: "report",
                    instruction_id: &binding.instruction_id,
                    output_version: binding.output_version,
                    event_id: &binding.event_id,
                    client_sequence: sequence,
                    report: &report,
                };
                let budget = deadline.map_or(WRITE_TIMEOUT, |deadline| {
                    WRITE_TIMEOUT.min(deadline.saturating_duration_since(now))
                });
                let at = transmit(ws, session, &message, budget).await?;
                self.sent(sequence, at);
                if retiring {
                    self.retiring = None;
                }
                if parked {
                    if let Some(current) = self.execution.as_mut() {
                        current.last_parked_report = Some(report.clone());
                    }
                }
                if report.ended {
                    self.discard_plan();
                    let current = self
                        .execution
                        .as_mut()
                        .expect("report has current execution");
                    current.ended = true;
                    if let Some((plan, deadline)) = plan
                        .zip(current.deadline)
                        .filter(|(_, deadline)| *deadline > Instant::now())
                    {
                        let operation = Operation {
                            binding: plan.next.binding(binding.output_version),
                            kind: InstructionKind::Execute,
                            track: Some((plan.next.track.id.clone(), plan.next.track.key)),
                            desired_playing: true,
                        };
                        self.ack_due = None;
                        self.execution = Some(ExecutionState {
                            operation,
                            deadline: Some(deadline),
                            through_sequence: sequence,
                            parked: false,
                            last_parked_report: None,
                            ended: false,
                            admission: Some((binding.clone(), plan.id.clone())),
                        });
                        events.push(EventKind::NextAdmitted {
                            source: binding,
                            plan_id: plan.id,
                            next: plan.next,
                            deadline,
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

type Transport = WebSocketStream<reqwest::Upgraded>;

async fn transmit<
    T: Serialize + ?Sized,
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
>(
    ws: &mut WebSocketStream<S>,
    session: &Session,
    value: &T,
    budget: Duration,
) -> Result<Instant, End> {
    let frame = prepare_frame(session, value, budget)?;
    write_frame(ws, frame, budget).await
}

fn prepare_frame<T: Serialize + ?Sized>(
    session: &Session,
    value: &T,
    budget: Duration,
) -> Result<Message, End> {
    if session.is_expired() {
        return Err(End::Expired);
    }
    if session.is_offline() || budget.is_zero() {
        return Err(End::Transport);
    }
    let text = serde_json::to_string(value).map_err(|_| End::Protocol)?;
    if text.len() > MAX_OUTBOUND {
        return Err(End::Protocol);
    }
    Ok(Message::Text(text.into()))
}

async fn write_frame<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    frame: Message,
    budget: Duration,
) -> Result<Instant, End> {
    // Capture *before* entering the websocket write, never on grant receipt.
    let sent_at = Instant::now();
    time::timeout(budget, ws.send(frame))
        .await
        .map_err(|_| End::Transport)?
        .map_err(|_| End::Transport)?;
    Ok(sent_at)
}

fn emit(sender: &mpsc::Sender<Event>, generation: Generation, kind: EventKind) -> Result<(), End> {
    sender
        .try_send(Event { generation, kind })
        .map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => End::Transport,
            mpsc::error::TrySendError::Closed(_) => End::ReceiverClosed,
        })
}

fn decode(text: &str) -> Result<Incoming, End> {
    if text.len() > MAX_INBOUND {
        return Err(End::Protocol);
    }
    serde_json::from_str(text).map_err(|_| End::Protocol)
}

async fn read_frame(ws: &mut Transport) -> Result<Option<Incoming>, End> {
    match ws.next().await {
        Some(Ok(Message::Text(text))) => decode(text.as_str()).map(Some),
        Some(Ok(Message::Ping(_))) => {
            // tungstenite queues the corresponding pong automatically.
            time::timeout(WRITE_TIMEOUT, ws.flush())
                .await
                .map_err(|_| End::Transport)?
                .map_err(|_| End::Transport)?;
            Ok(None)
        }
        Some(Ok(Message::Pong(_))) => Ok(None),
        None | Some(Ok(Message::Close(_))) | Some(Err(_)) => Err(End::Transport),
        Some(Ok(_)) => Err(End::Protocol),
    }
}

fn admission_error(error: ApiError) -> End {
    if error.expired || error.status == 401 {
        End::Expired
    } else {
        End::Transport
    }
}

async fn connect(
    api: &Api,
    session: &Session,
    device: &DeviceIdentity,
) -> Result<(Transport, Machine, EventKind), End> {
    let PlaybackUpgrade { upgraded, ticket } =
        time::timeout(CONNECT_TIMEOUT, api.playback_upgrade(session))
            .await
            .map_err(|_| End::Transport)?
            .map_err(admission_error)?;
    let config = WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(0)
        .max_write_buffer_size(2 * MAX_OUTBOUND)
        .max_message_size(Some(MAX_INBOUND))
        .max_frame_size(Some(MAX_INBOUND));
    let mut ws = WebSocketStream::from_raw_socket(upgraded, Role::Client, Some(config)).await;
    transmit(
        &mut ws,
        session,
        &Hello {
            kind: "hello",
            ticket: &ticket,
            name: &device.name,
            device_id: &device.device_id,
            can_play: device.can_play,
        },
        WRITE_TIMEOUT,
    )
    .await?;
    drop(ticket);
    let incoming = time::timeout(SNAPSHOT_TIMEOUT, async {
        loop {
            if let Some(frame) = read_frame(&mut ws).await? {
                return Ok::<_, End>(frame);
            }
        }
    })
    .await
    .map_err(|_| End::Transport)??;
    let (machine, event) = Machine::admit(incoming, &device.device_id)?;
    Ok((ws, machine, event))
}

struct Online<'a> {
    generation: Generation,
    session: &'a Session,
    outbound: &'a mut mpsc::Receiver<Outgoing>,
    events_out: &'a mpsc::Sender<Event>,
    links: &'a watch::Sender<Link>,
}

impl Online<'_> {
    async fn drive<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        &mut self,
        mut ws: WebSocketStream<S>,
        mut machine: Machine,
        snapshot: EventKind,
        mode: &mut watch::Receiver<u64>,
    ) -> (End, bool) {
        // Engine can issue a command immediately when it consumes the snapshot.
        self.links.send_replace(Link::Connected(self.generation));
        let (end, ready) = match emit(self.events_out, self.generation, snapshot) {
            Ok(()) => {
                let (session, events) = (self.session, self.events_out);
                let end = tokio::select! {
                    biased;
                    _ = mode.changed() => End::Transport,
                    _ = local_expiry(session) => End::Expired,
                    _ = events.closed() => End::ReceiverClosed,
                    end = self.exchange(&mut ws, &mut machine) => end,
                };
                (end, true)
            }
            Err(end) => (end, false),
        };
        // Closing the TCP stream is not stop proof. Signal loss first, then
        // settle all in-flight writes and discard the unsent queue without replay.
        self.links.send_replace(end.link());
        drop(ws);
        drop(machine);
        while let Ok(request) = self.outbound.try_recv() {
            drop(request);
        }
        (end, ready)
    }

    async fn exchange<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
        &mut self,
        ws: &mut WebSocketStream<S>,
        machine: &mut Machine,
    ) -> End {
        let mut events = Vec::with_capacity(4);
        let mut idle_at = Instant::now() + SOCKET_IDLE;
        loop {
            if self.session.is_expired() {
                return End::Expired;
            }
            if self.session.is_offline() {
                return End::Transport;
            }
            let now = Instant::now();
            machine.expire(now, &mut events);
            for event in events.drain(..) {
                if let Err(end) = emit(self.events_out, self.generation, event) {
                    return end;
                }
            }
            if now >= idle_at {
                return End::Transport;
            }
            let wake = machine
                .deadline()
                .map_or(idle_at, |deadline| deadline.min(idle_at));
            // read_frame must not be cancelled after consuming a ping and before
            // writing its pong. Only poll the raw stream inside select.
            let result = tokio::select! {
                frame = ws.next() => {
                    idle_at = Instant::now() + SOCKET_IDLE;
                    match frame {
                        Some(Ok(Message::Text(text))) => decode(text.as_str()).and_then(|incoming| machine.receive(incoming, Instant::now(), &mut events)),
                        Some(Ok(Message::Ping(_))) => match time::timeout(WRITE_TIMEOUT, ws.flush()).await { Ok(Ok(())) => Ok(()), _ => Err(End::Transport) },
                        Some(Ok(Message::Pong(_))) => Ok(()),
                        None | Some(Ok(Message::Close(_))) | Some(Err(_)) => Err(End::Transport),
                        Some(Ok(_)) => Err(End::Protocol),
                    }
                }
                request = self.outbound.recv() => match request {
                    Some(request) => machine.send(request, self.generation, ws, self.session, &mut events).await,
                    None => Err(End::ReceiverClosed),
                },
                _ = time::sleep_until(wake.into()) => Ok(()),
            };
            if let Err(end) = result {
                return end;
            }
        }
    }
}

async fn run(
    api: Arc<Api>,
    session: Session,
    device: DeviceIdentity,
    mut outbound: mpsc::Receiver<Outgoing>,
    events: mpsc::Sender<Event>,
    links: watch::Sender<Link>,
) {
    let mut mode = session.network_changes();
    let mut generation: Generation = 0;
    let mut backoff = RETRY_MIN;
    loop {
        let prior_mode = *mode.borrow_and_update();
        if session.is_expired() {
            links.send_replace(Link::Expired);
            return;
        }
        if prior_mode & 1 != 0 {
            links.send_replace(Link::Disconnected);
            tokio::select! { _ = mode.changed() => {}, _ = local_expiry(&session) => {}, _ = events.closed() => return }
            continue;
        }
        generation = generation.saturating_add(1);
        let connection = tokio::select! {
            biased;
            _ = mode.changed() => Err(End::Transport),
            _ = local_expiry(&session) => Err(End::Expired),
            _ = events.closed() => Err(End::ReceiverClosed),
            result = connect(&api, &session, &device) => result,
        };
        let (end, ready) = match connection {
            Ok((ws, machine, snapshot)) => {
                Online {
                    generation,
                    session: &session,
                    outbound: &mut outbound,
                    events_out: &events,
                    links: &links,
                }
                .drive(ws, machine, snapshot, &mut mode)
                .await
            }
            Err(end) => {
                links.send_replace(end.link());
                (end, false)
            }
        };
        if matches!(end, End::Expired | End::ReceiverClosed) {
            return;
        }
        if *mode.borrow() != prior_mode {
            backoff = RETRY_MIN;
            continue;
        }
        if ready {
            backoff = RETRY_MIN;
        }
        tokio::select! {
            _ = time::sleep(backoff) => { backoff = (backoff * 2).min(RETRY_MAX); }
            _ = mode.changed() => { backoff = RETRY_MIN; }
            _ = local_expiry(&session) => { links.send_replace(Link::Expired); return; }
            _ = events.closed() => { links.send_replace(Link::Closed); return; }
        }
    }
}

async fn local_expiry(session: &Session) {
    // Local in-memory fence only. Do not renew sessions via periodic REST reads.
    while !session.is_expired() {
        time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn playback(output_version: u64, state_version: u64, sequence: u64, event: &str) -> Value {
        json!({ "outputVersion":output_version, "stateVersion":state_version, "playbackSequence":sequence,
            "eventId":event, "hasReportedPlayback":false, "elapsed":0, "duration":0,
            "isPlaying":false, "isBuffering":false, "isLoading":false, "error":"" })
    }

    fn track(id: &str, key: u64) -> Value {
        json!({ "id":id, "name":"Synthetic track", "artists":[], "albumName":"", "albumId":"", "key":key,
            "order":key, "duration":120, "intent":"now" })
    }

    fn snapshot(
        output_version: u64,
        state_version: u64,
        selected: bool,
        transfer: Value,
        event: &str,
    ) -> Value {
        let empty = event.is_empty();
        json!({ "sessionId":"synthetic-room", "stateVersion":state_version, "outputVersion":output_version,
            "state": {"version":1, "queue":if empty {json!([])} else {json!([track("track-a",1)])},
                "position":if empty {-1} else {0}, "context":null, "shuffle":false, "repeat":"off",
                "desiredPlaying":!empty, "highKeys":[], "elapsed":0, "duration":0, "isPlaying":false,
                "isBuffering":false, "isLoading":false, "error":"", "truncated":false, "notice":"",
                "occurrence":if empty {Value::Null} else {json!({"key":1,"trackId":"track-a","eventId":event,"progress":0,"reported":false})} },
            "playback":playback(output_version,state_version,0,event),
            "devices":[{"deviceId":"synthetic-device", "name":"Synthetic output", "available":true, "canPlay":true}],
            "selectedDeviceId":if selected {"synthetic-device"} else {""},
            "selectedConnectionId":if selected {"synthetic-connection"} else {""},
            "transfer":transfer, "drainUntilMs":0 })
    }

    fn opening(snapshot: Value) -> Value {
        json!({"type":"snapshot", "sessionId":"synthetic-room", "connectionId":"synthetic-connection", "deviceId":"synthetic-device", "snapshot":snapshot})
    }

    fn machine(snapshot: Value) -> Machine {
        Machine::admit(
            decode(&opening(snapshot).to_string()).unwrap(),
            "synthetic-device",
        )
        .unwrap()
        .0
    }

    fn instruction(kind: &str, id: &str, output_version: u64, event_id: &str) -> Value {
        json!({"type":kind, "id":id, "outputVersion":output_version, "eventId":event_id, "elapsed":0, "desiredPlaying":false})
    }

    fn receive(
        machine: &mut Machine,
        kind: &str,
        payload: Value,
        now: Instant,
        events: &mut Vec<EventKind>,
    ) -> Result<(), End> {
        let field = if kind == "state" { "snapshot" } else { kind };
        machine.receive(
            decode(&json!({"type":kind,"sessionId":"synthetic-room",(field):payload}).to_string())?,
            now,
            events,
        )
    }

    #[test]
    fn command_variants_serialize_exact_wire_fields() {
        let move_first = Command::Move {
            key: 7,
            after_key: None,
        };
        assert!(move_first.valid());
        assert_eq!(
            serde_json::to_value(move_first).unwrap(),
            json!({"type":"move","key":7,"afterKey":null})
        );
        let rename = WireCommand {
            kind: "command",
            id: "1".into(),
            expected_state_version: Some(17),
            command: &Command::Rename {
                name: "Desk".into(),
                device_id: Some("synthetic-device".into()),
            },
        };
        assert_eq!(
            serde_json::to_value(rename).unwrap(),
            json!({"type":"command","id":"1","expectedStateVersion":17,
            "command":{"type":"rename","name":"Desk","deviceId":"synthetic-device"}})
        );
        let start = Command::QueueIntent {
            intent: Intent::Now,
            selection: Selection::track("é".repeat(256)),
            shuffle: None,
        };
        assert!(start.valid());
        assert_eq!(
            serde_json::to_value(start).unwrap()["selection"]["track"]["id"],
            "é".repeat(256)
        );
        assert!(
            !Command::QueueIntent {
                intent: Intent::Now,
                selection: Selection::track("track-a".into()),
                shuffle: Some(false)
            }
            .valid()
        );
        assert!(!Command::Seek { value: f64::NAN }.valid());
        assert!(
            !Command::Remove {
                key: MAX_SAFE_INTEGER + 1,
                undo: None
            }
            .valid()
        );
        assert!(
            !Command::Rename {
                name: "desk\n".into(),
                device_id: None
            }
            .valid()
        );
        let context = Context {
            kind: ContextKind::Album,
            id: "album-a".into(),
            name: "".into(),
            revision: None,
            fence: None,
        };
        assert!(
            Command::QueueIntent {
                intent: Intent::Now,
                selection: Selection::Collection { context },
                shuffle: Some(true)
            }
            .valid()
        );
    }

    #[test]
    fn decoder_requires_strict_shape_and_admission_identity() {
        let snapshot = snapshot(1, 1, false, Value::Null, "");
        let text = opening(snapshot.clone()).to_string();
        assert!(decode(&text).is_ok());
        assert!(
            decode(&text.replacen(
                "\"type\":\"snapshot\"",
                "\"type\":\"snapshot\",\"type\":\"snapshot\"",
                1
            ))
            .is_err()
        );
        assert!(decode(&text.replacen("\"version\":1", "\"version\":1,\"version\":1", 1)).is_err());
        assert!(decode(&text.replacen("\"deviceId\":", "\"DeviceId\":", 1)).is_err());
        assert!(decode(&text.replacen("\"context\":null,", "", 1)).is_err());
        assert!(
            decode(&text.replacen(
                "\"type\":\"snapshot\"",
                "\"type\":\"snapshot\",\"result\":null",
                1
            ))
            .is_err()
        );
        assert!(Machine::admit(decode(&text).unwrap(), "another-device").is_err());
        assert!(
            Machine::admit(
                decode(
                    &json!({"type":"state","sessionId":"synthetic-room","snapshot":snapshot})
                        .to_string()
                )
                .unwrap(),
                "synthetic-device"
            )
            .is_err()
        );
    }

    #[test]
    fn scalar_only_applies_to_matching_output_state_and_event() {
        let mut machine = machine(snapshot(1, 1, true, Value::Null, "event-a"));
        let now = Instant::now();
        let mut events = Vec::new();
        receive(
            &mut machine,
            "playback",
            playback(1, 2, 4, "event-b"),
            now,
            &mut events,
        )
        .unwrap();
        receive(
            &mut machine,
            "playback",
            playback(1, 1, 3, "event-a"),
            now,
            &mut events,
        )
        .unwrap();
        receive(
            &mut machine,
            "playback",
            playback(1, 1, 2, "event-a"),
            now,
            &mut events,
        )
        .unwrap();
        receive(
            &mut machine,
            "playback",
            playback(1, 1, 5, "event-b"),
            now,
            &mut events,
        )
        .unwrap();
        assert!(
            matches!(events.as_slice(), [EventKind::Playback(playback)] if playback.playback_sequence == 3)
        );
        events.clear();
        receive(
            &mut machine,
            "state",
            snapshot(1, 2, true, Value::Null, "event-b"),
            now,
            &mut events,
        )
        .unwrap();
        assert!(
            matches!(events.as_slice(), [EventKind::State(snapshot)] if snapshot.playback.playback_sequence == 4)
        );
        events.clear();
        receive(
            &mut machine,
            "playback",
            playback(1, 1, 7, "event-a"),
            now,
            &mut events,
        )
        .unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn activation_is_anchored_to_exact_prepare_ack_and_lease_cannot_restart() {
        let transfer = json!({"sourceDeviceId":"","sourceConnectionId":"","targetDeviceId":"synthetic-device",
            "targetConnectionId":"synthetic-connection","phase":"preparing"});
        let mut machine = machine(snapshot(1, 1, false, transfer, "event-a"));
        let at = Instant::now();
        let mut events = Vec::new();
        let mut prepare = instruction("prepare", "prepare-a", 1, "event-a");
        prepare["track"] = track("track-a", 1);
        receive(&mut machine, "instruction", prepare, at, &mut events).unwrap();
        machine.ready = Some(Ready {
            operation: machine.ack_due.take().unwrap(),
            basis: 1,
        });
        machine.sent(1, at);
        let mut activate = instruction("activate", "activate-a", 2, "event-a");
        activate["track"] = track("track-a", 1);
        activate["desiredPlaying"] = json!(true);
        activate["lease"] = json!({"clientSequence":1,"durationMs":8000});
        let received_at = at + Duration::from_secs(2);
        receive(
            &mut machine,
            "instruction",
            activate.clone(),
            received_at,
            &mut events,
        )
        .unwrap();
        assert_eq!(machine.deadline(), Some(at + Duration::from_secs(8)));
        assert!(
            matches!(events.last(), Some(EventKind::Instruction { deadline:Some(deadline), .. }) if *deadline == at + Duration::from_secs(8))
        );
        receive(
            &mut machine,
            "instruction",
            activate,
            received_at,
            &mut events,
        )
        .unwrap();
        assert_eq!(events.len(), 2);
        let later = at + Duration::from_secs(8);
        machine.expire(later, &mut events);
        assert!(
            matches!(events.last(), Some(EventKind::LeaseExpired(binding)) if binding.instruction_id == "activate-a")
        );
        machine.sent(2, later);
        receive(
            &mut machine,
            "lease",
            json!({"outputVersion":2,"eventId":"event-a","clientSequence":2,"durationMs":8000}),
            later,
            &mut events,
        )
        .unwrap();
        assert_eq!(machine.deadline(), None);
    }

    fn active(machine: &mut Machine, at: Instant) -> Binding {
        let binding = Binding {
            instruction_id: "source".into(),
            output_version: 1,
            event_id: "event-a".into(),
        };
        machine.execution = Some(ExecutionState {
            operation: Operation {
                binding: binding.clone(),
                kind: InstructionKind::Activate,
                track: Some(("track-a".into(), 1)),
                desired_playing: true,
            },
            deadline: Some(at + Duration::from_secs(7)),
            through_sequence: 1,
            parked: false,
            last_parked_report: None,
            ended: false,
            admission: None,
        });
        machine.sent(1, at);
        binding
    }

    #[tokio::test]
    async fn planned_next_is_admitted_after_ended_send_then_confirmed_not_replayed() {
        use crate::api::Origin;
        let mut machine = machine(snapshot(1, 1, true, Value::Null, "event-a"));
        let at = Instant::now();
        let binding = active(&mut machine, at);
        let mut events = Vec::new();
        let mut stage = instruction("stageNext", "stage", 1, "event-a");
        stage["currentInstructionId"] = json!("source");
        stage["planId"] = json!("plan-a");
        stage["next"] =
            json!({"eventId":"event-b","instructionId":"reserved","track":track("track-b",2)});
        receive(&mut machine, "instruction", stage, at, &mut events).unwrap();
        assert!(matches!(
            events.as_slice(),
            [EventKind::Instruction { deadline: None, .. }]
        ));
        events.clear();
        let api = Api::new(Origin::parse("https://example.invalid").unwrap()).unwrap();
        let session = api.test_session();
        let (client, server) = tokio::io::duplex(8192);
        let mut client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let report = Report {
            elapsed: 120.0,
            duration: 120.0,
            is_playing: false,
            is_buffering: false,
            is_loading: false,
            error: MediaError::None,
            ended: true,
            plan_id: Some("plan-a".into()),
        };
        machine
            .send(
                Outgoing {
                    generation: 3,
                    queued_at: at,
                    message: Output::Report { binding, report },
                },
                3,
                &mut client,
                &session,
                &mut events,
            )
            .await
            .unwrap();
        let Message::Text(sent) = server.next().await.unwrap().unwrap() else {
            panic!("expected text report");
        };
        let sent: Value = serde_json::from_str(sent.as_str()).unwrap();
        assert_eq!(sent["type"], "report");
        assert_eq!(sent["instructionId"], "source");
        assert_eq!(sent["planId"], "plan-a");
        assert_eq!(sent["clientSequence"], 2);
        assert!(
            matches!(events.as_slice(), [EventKind::NextAdmitted { next, deadline, .. }]
            if next.instruction_id == "reserved" && *deadline == at + Duration::from_secs(7))
        );
        events.clear();
        receive(
            &mut machine,
            "lease",
            json!({"outputVersion":1,"eventId":"event-a","clientSequence":2,"durationMs":8000}),
            Instant::now(),
            &mut events,
        )
        .unwrap();
        assert!(events.is_empty());
        let mut execute = instruction("execute", "reserved", 1, "event-b");
        execute["track"] = track("track-b", 2);
        execute["planId"] = json!("plan-a");
        execute["desiredPlaying"] = json!(true);
        execute["lease"] = json!({"clientSequence":2,"durationMs":8000});
        receive(
            &mut machine,
            "instruction",
            execute.clone(),
            Instant::now(),
            &mut events,
        )
        .unwrap();
        receive(
            &mut machine,
            "instruction",
            execute,
            Instant::now(),
            &mut events,
        )
        .unwrap();
        assert!(
            matches!(events.as_slice(), [EventKind::Instruction { instruction, deadline:Some(_), .. }]
            if instruction.id == "reserved" && instruction.plan_id.as_deref() == Some("plan-a"))
        );
        assert!(machine.execution.as_ref().unwrap().admission.is_none());
    }

    #[tokio::test]
    async fn remote_resume_execute_crosses_the_local_pause_ack() {
        use crate::api::Origin;
        let mut machine = machine(snapshot(1, 1, true, Value::Null, "event-a"));
        let sent_at = Instant::now() - Duration::from_secs(1);
        active(&mut machine, sent_at);
        let mut events = Vec::new();
        let mut pause = instruction("execute", "pause-a", 1, "event-a");
        pause["track"] = track("track-a", 1);
        pause["lease"] = json!({"clientSequence":1,"durationMs":8000});
        receive(
            &mut machine,
            "instruction",
            pause,
            Instant::now(),
            &mut events,
        )
        .unwrap();
        let binding = machine
            .execution
            .as_ref()
            .unwrap()
            .operation
            .binding
            .clone();
        events.clear();

        let api = Api::new(Origin::parse("https://example.invalid").unwrap()).unwrap();
        let session = api.test_session();
        let (client, server) = tokio::io::duplex(8192);
        let mut client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        machine
            .send(
                Outgoing {
                    generation: 1,
                    queued_at: Instant::now(),
                    message: Output::Ack {
                        binding,
                        status: AckStatus::Ok,
                    },
                },
                1,
                &mut client,
                &session,
                &mut events,
            )
            .await
            .unwrap();
        let Message::Text(ack) = server.next().await.unwrap().unwrap() else {
            panic!("expected Pause ACK");
        };
        assert_eq!(
            serde_json::from_str::<Value>(ack.as_str()).unwrap(),
            json!({"type":"ack","instructionId":"pause-a",
            "outputVersion":1,"eventId":"event-a","clientSequence":2,"status":"ok"})
        );
        assert!(machine.execution.as_ref().unwrap().parked);
        assert_eq!(machine.deadline(), None);

        receive(
            &mut machine,
            "lease",
            json!({"outputVersion":1,"eventId":"event-a","clientSequence":2,"durationMs":8000}),
            Instant::now(),
            &mut events,
        )
        .unwrap();
        assert!(events.is_empty());
        assert_eq!(machine.deadline(), None);

        // Controller B's Resume reached Go before A's Pause ACK. The distinct
        // execute keeps the output version and echoes A's earlier send, not ACK 2.
        let mut resume = instruction("execute", "resume-b", 1, "event-a");
        resume["commandId"] = json!("1");
        resume["track"] = track("track-a", 1);
        resume["desiredPlaying"] = json!(true);
        resume["lease"] = json!({"clientSequence":1,"durationMs":8000});
        receive(
            &mut machine,
            "instruction",
            resume,
            Instant::now(),
            &mut events,
        )
        .unwrap();
        let expected = sent_at + Duration::from_secs(8);
        assert!(
            matches!(events.as_slice(), [EventKind::Instruction { instruction, deadline:Some(deadline) }]
            if instruction.id == "resume-b" && instruction.desired_playing && *deadline == expected)
        );
        assert_eq!(machine.deadline(), Some(expected));
        assert!(!machine.execution.as_ref().unwrap().parked);
        assert!(machine.pending.is_empty());
    }

    #[tokio::test]
    async fn stale_commands_are_not_sent_or_carried_into_new_generation() {
        use crate::api::Origin;
        let mut machine = machine(snapshot(1, 1, false, Value::Null, ""));
        let api = Api::new(Origin::parse("https://example.invalid").unwrap()).unwrap();
        let (client, _server) = tokio::io::duplex(8192);
        let mut client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
        let session = api.test_session();
        let mut events = Vec::new();
        let (credit, mut settlements) = mpsc::channel(2);
        let command = |generation, queued_at, request_id| Outgoing {
            generation,
            queued_at,
            message: Output::Command {
                settlement: CommandSettlement::new(
                    credit.clone().try_reserve_owned().unwrap(),
                    generation,
                    request_id,
                ),
                command: Command::Resume,
                expected_state_version: None,
            },
        };
        machine
            .send(
                command(1, Instant::now(), 12),
                2,
                &mut client,
                &session,
                &mut events,
            )
            .await
            .unwrap();
        machine
            .send(
                command(2, Instant::now() - Duration::from_secs(1), 13),
                2,
                &mut client,
                &session,
                &mut events,
            )
            .await
            .unwrap();
        assert!(events.is_empty());
        assert_not_sent(&mut settlements, 1, 12, SocketError::NotConnected);
        assert_not_sent(&mut settlements, 2, 13, SocketError::Busy);
        assert!(settlements.is_empty());
        assert_eq!(machine.command_id, 0);
        assert!(machine.pending.is_empty());
    }

    fn assert_not_sent(
        settlements: &mut mpsc::Receiver<Event>,
        generation: Generation,
        expected_id: u64,
        expected_error: SocketError,
    ) {
        let event = settlements.try_recv().unwrap();
        assert_eq!(event.generation, generation);
        assert!(
            matches!(event.kind, EventKind::CommandNotSent { request_id, error }
            if request_id == expected_id && error == expected_error)
        );
    }

    #[tokio::test]
    async fn admission_reserves_receipt_capacity_before_accepting_commands() {
        let (_links, link) = watch::channel(Link::Connected(3));
        let (_events_out, events) = mpsc::channel(1);
        let (settlement_credit, settlements) = mpsc::channel(1);
        let (outbound, mut queued) = mpsc::channel(1);
        let mut socket = Socket {
            link,
            events,
            settlements,
            settlement_credit,
            outbound,
            task: tokio::spawn(std::future::pending()),
        };
        socket.command(3, 10, Command::Resume, None).unwrap();
        assert_eq!(
            socket.command(3, 11, Command::Resume, None),
            Err(SocketError::Busy)
        );
        let first = queued.try_recv().unwrap();
        assert_eq!(
            socket.command(3, 12, Command::Resume, None),
            Err(SocketError::Busy)
        );
        assert!(queued.is_empty());
        assert!(socket.settlements.is_empty());
        drop(first);
        assert_eq!(
            socket.command(3, 13, Command::Resume, None),
            Err(SocketError::Busy)
        );
        socket
            .ack(
                3,
                Binding {
                    instruction_id: "pause-a".into(),
                    output_version: 1,
                    event_id: "event-a".into(),
                },
                AckStatus::Ok,
            )
            .unwrap();
        drop(queued.try_recv().unwrap());
        assert_not_sent(&mut socket.settlements, 3, 10, SocketError::NotConnected);
        assert!(socket.settlements.is_empty());
        socket.command(3, 14, Command::Resume, None).unwrap();
    }

    type TestTransport = WebSocketStream<tokio::io::DuplexStream>;

    async fn online_socket() -> (Socket, TestTransport, Session) {
        use crate::api::Origin;
        let (links, link) = watch::channel(Link::Connecting);
        let (events_out, events) = mpsc::channel(1);
        let (settlement_credit, settlements) = mpsc::channel(2);
        let (outbound, mut queued) = mpsc::channel(2);
        let api = Api::new(Origin::parse("https://example.invalid").unwrap()).unwrap();
        let session = api.test_session();
        let actor_session = session.clone();
        let (client, server) = tokio::io::duplex(8192);
        let client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
        let server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let mut mode = session.network_changes();
        let task = tokio::spawn(async move {
            let incoming =
                decode(&opening(snapshot(1, 1, false, Value::Null, "")).to_string()).unwrap();
            let (machine, snapshot) = Machine::admit(incoming, "synthetic-device").unwrap();
            let end = Online {
                generation: 3,
                session: &actor_session,
                outbound: &mut queued,
                events_out: &events_out,
                links: &links,
            }
            .drive(client, machine, snapshot, &mut mode)
            .await;
            assert_eq!(end, (End::Transport, true));
        });
        let mut socket = Socket {
            link,
            events,
            settlements,
            settlement_credit,
            outbound,
            task,
        };
        time::timeout(Duration::from_secs(2), socket.link.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*socket.link.borrow(), Link::Connected(3));
        (socket, server, session)
    }

    async fn submit_and_read(
        socket: &Socket,
        server: &mut TestTransport,
        request_id: u64,
        wire_id: &str,
    ) {
        socket
            .command(3, request_id, Command::Resume, None)
            .unwrap();
        let frame = time::timeout(Duration::from_secs(2), server.next())
            .await
            .unwrap();
        let Some(Ok(Message::Text(text))) = frame else {
            panic!("expected command frame");
        };
        assert_eq!(
            serde_json::from_str::<Value>(text.as_str()).unwrap()["id"],
            wire_id
        );
    }

    async fn assert_disconnected(socket: &mut Socket) {
        time::timeout(Duration::from_secs(2), &mut socket.task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*socket.link.borrow(), Link::Disconnected);
        assert!(matches!(
            socket.events.try_recv().unwrap().kind,
            EventKind::Snapshot { .. }
        ));
        assert!(socket.events.is_empty());
    }

    #[tokio::test]
    async fn cancellation_with_full_events_distinguishes_sent_from_unsent() {
        let (mut socket, mut server, session) = online_socket().await;
        submit_and_read(&socket, &mut server, 10, "1").await;
        // The snapshot still fills events. On this current-thread test runtime,
        // cancel the actor before it can poll the newly queued command.
        assert_eq!(socket.events.len(), 1);
        socket.command(3, 11, Command::Resume, None).unwrap();
        session.set_offline(true);
        assert_disconnected(&mut socket).await;
        let event = socket.settlements.try_recv().unwrap();
        assert_eq!(event.generation, 3);
        assert!(matches!(
            event.kind,
            EventKind::CommandUncertain { request_id: 10 }
        ));
        assert_not_sent(&mut socket.settlements, 3, 11, SocketError::NotConnected);
        assert!(socket.settlements.is_empty());
    }

    #[tokio::test]
    async fn final_receipt_survives_peer_loss_or_event_overflow() {
        for overflow in [false, true] {
            let (mut socket, mut server, _session) = online_socket().await;
            submit_and_read(&socket, &mut server, 10, "1").await;
            submit_and_read(&socket, &mut server, 11, "2").await;
            assert_eq!(socket.events.len(), 1);
            for (id, is_final, execution) in [("2", false, "pending"), ("1", true, "confirmed")] {
                let result = json!({"type":"result", "sessionId":"synthetic-room", "result": {
                    "id": id, "outcome":"accepted", "code":"", "stateVersion":1,
                    "acceptedCount":0, "placement":"none", "execution":execution, "final":is_final
                }});
                server
                    .send(Message::Text(result.to_string().into()))
                    .await
                    .unwrap();
            }
            if overflow {
                let update = json!({"type":"playback", "sessionId":"synthetic-room",
                    "playback":playback(1, 1, 1, "")});
                server
                    .send(Message::Text(update.to_string().into()))
                    .await
                    .unwrap();
            } else {
                drop(server);
            }
            assert_disconnected(&mut socket).await;
            let final_event = socket.settlements.try_recv().unwrap();
            assert_eq!(final_event.generation, 3);
            assert!(
                matches!(final_event.kind, EventKind::Result { request_id: 10, result }
                if result.is_final && result.execution == Execution::Confirmed)
            );
            let unresolved_event = socket.settlements.try_recv().unwrap();
            assert_eq!(unresolved_event.generation, 3);
            assert!(matches!(
                unresolved_event.kind,
                EventKind::CommandUncertain { request_id: 11 }
            ));
            assert!(socket.settlements.is_empty());
        }
    }
}
