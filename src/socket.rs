//! Authenticated account-state.v3 links. Playback writes are socket-scoped and never replayed.

#[path = "account_wire.rs"]
mod wire;

pub(crate) use wire::{
    Action, Advance, Artist, Claim, Command, CommandBasis, CommandResult, Context, ContextKind,
    Cursor, Device, Execute, FIRST_PREVIEW_KEY, Intent, MAX_SAFE_INTEGER, Observation,
    ObservationStatus, ObservationUpdate, Offer, Owner, Ownership, PlaybackContext, PlayerDelta,
    QueueEdit, QueueEditKind, ReattachBasis, Repeat, Report, ResultStatus, Selection, Settlement,
    SettlementStatus, Snapshot, State, StateUpdate, Track, TrackRef,
};

use std::{
    collections::VecDeque,
    future::{Future, pending, poll_fn},
    io::{self, Write},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::{Sink, SinkExt, StreamExt};
use serde::Serialize;
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, oneshot, watch},
    time,
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        protocol::{Role, WebSocketConfig},
    },
};

use crate::{
    api::{Api, Session},
    personal::{PersonalCommand, PersonalUpdate},
    personal_bridge::PersonalAccess,
    playlist_pin_store::PUBLICATION_GATE,
    quick_add::{BULK_WINDOW, Ticket},
};
use wire::{Incoming, Outgoing};

pub(crate) const SUBPROTOCOL: &str = "spofify.account-state.v3";
const MAX_INBOUND: usize = 5 << 20;
const MAX_OUTBOUND: usize = 64 << 10;
const MAX_STATE: usize = 2 << 20;
const MAX_HELLO: usize = MAX_STATE + MAX_OUTBOUND;
const OUTBOUND_CAPACITY: usize = 16;
const EVENT_CAPACITY: usize = 8;
const MAX_QUEUE_AGE: Duration = Duration::from_millis(500);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15);
const SOCKET_IDLE: Duration = Duration::from_secs(15);
const ONLINE_ATTEMPTS: u8 = 4;
const CLOCK_INTERVAL: Duration = Duration::from_secs(2);
const CLOCK_JUMP_MS: u128 = 100;
const CLOCK_SAMPLES: usize = 8;

/// Ticket remains opaque, memory-only, and is consumed only in hello.
pub(crate) struct AccountUpgrade {
    pub(crate) upgraded: reqwest::Upgraded,
    pub(crate) ticket: String,
}

#[derive(Clone)]
pub(crate) struct DeviceIdentity {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) can_play: bool,
}

/// A process-local socket incarnation; never a server generation or revision.
pub(crate) type Generation = u64;
static GENERATIONS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Link {
    Connecting,
    Connected(Generation),
    Disconnected,
    /// Four failed admissions. Wait for explicit network mode change, not a retry timer.
    Exhausted,
    /// A different binding with this installation's device ID replaced this socket.
    Superseded,
    Expired,
    Closed,
}

#[derive(Debug)]
pub(crate) struct Event {
    pub(crate) generation: Generation,
    pub(crate) kind: EventKind,
    pub(crate) personal_stamp: Option<u64>,
}

#[derive(Debug)]
pub(crate) enum EventKind {
    /// Before hello on each primary attempt; no transport generation has been assigned.
    ClaimRequested(oneshot::Sender<Claim>),
    Snapshot(Snapshot),
    State(StateUpdate),
    Observation(ObservationUpdate),
    Ownership(Ownership),
    Offer(Offer),
    Result(CommandResult),
    Execute(Execute),
    SampleClock(SampleClock),
    Personal(Value),
}

/// Server minus local wall time, scoped to this transport generation.
/// A fresh link starts without a sample; `None` clears an estimate after a clock jump.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SampleClock {
    pub(crate) offset_ms: Option<i64>,
    pub(crate) rtt_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SocketError {
    Invalid,
    NotConnected,
    Busy,
}

pub(crate) struct Socket {
    pub(crate) link: watch::Receiver<Link>,
    pub(crate) events: mpsc::Receiver<Event>,
    last_owner: watch::Receiver<Option<(Generation, bool)>>,
    outbound: mpsc::Sender<WriteFrame>,
    binding: Arc<Mutex<Option<Binding>>>,
    order_gate: Arc<Mutex<IngressSequence>>,
    personal_applied: watch::Sender<u64>,
    session: Session,
    task: tokio::task::JoinHandle<()>,
}

struct Binding {
    transport: Generation,
    connection_id: String,
    owner_generation: Option<u64>,
    // Only the initial owner snapshot can authorize one reconciliation attempt.
    reconcile_basis: Option<ReattachBasis>,
    command_counter: u64,
    report_sequence: u64,
}

impl Binding {
    fn new(transport: Generation, connection_id: String, owner_generation: Option<u64>) -> Self {
        Self {
            transport,
            connection_id,
            owner_generation,
            reconcile_basis: None,
            command_counter: 0,
            report_sequence: 0,
        }
    }

    fn opening(transport: Generation, snapshot: &Snapshot, own: bool) -> Self {
        let owner = own.then_some(snapshot.owner.generation);
        let mut binding = Self::new(transport, snapshot.connection_id.clone(), owner);
        binding.reconcile_basis = owner.map(|generation| ReattachBasis {
            session_id: snapshot.session_id.clone(),
            generation,
            revision: snapshot.revision,
            occurrence: snapshot.player.cursor.occurrence.clone(),
        });
        binding
    }

    fn next_command_id(&self) -> Result<(u64, String), SocketError> {
        let index = self.command_counter + 1;
        let id = format!("{}:{index}", self.connection_id);
        if !wire::command_id(&id) {
            return Err(SocketError::Invalid);
        }
        Ok((index, id))
    }
}

#[derive(Default)]
struct IngressSequence {
    personal: u64,
}

/// Primary personal ingress fence. Call while holding the Personal state lock,
/// as for order admission; the closure must not wait or perform async I/O.
#[derive(Clone)]
pub(crate) struct BulkOrder {
    order_gate: Arc<Mutex<IngressSequence>>,
    applied: watch::Receiver<u64>,
}

impl BulkOrder {
    pub(crate) fn when_applied<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        let published = self.order_gate.lock().expect("socket order admission gate");
        let applied = *self.applied.borrow();
        (applied == published.personal).then(f)
    }

    pub(crate) fn changes(&self) -> watch::Receiver<u64> {
        self.applied.clone()
    }
}

pub(crate) fn spawn(
    api: Arc<Api>,
    session: Session,
    device: DeviceIdentity,
) -> Result<Socket, SocketError> {
    if !wire::text(&device.device_id, 128) || !wire::name(&device.name) {
        return Err(SocketError::Invalid);
    }
    let (outbound, input) = mpsc::channel(OUTBOUND_CAPACITY);
    let (event_out, events) = mpsc::channel(EVENT_CAPACITY);
    let (link_out, link) = watch::channel(Link::Connecting);
    let (owner_out, last_owner) = watch::channel(None);
    let binding = Arc::new(Mutex::new(None));
    let order_gate = Arc::new(Mutex::new(IngressSequence::default()));
    let personal_applied = watch::channel(0).0;
    let claims = ClaimRequests {
        events: event_out.clone(),
        order_gate: order_gate.clone(),
    };
    let signals = Signals {
        order_gate: order_gate.clone(),
        personal_applied: personal_applied.clone(),
        link: link_out,
        last_owner: owner_out,
        binding: binding.clone(),
        events: event_out,
    };
    let task = tokio::spawn(run_links(
        session.clone(),
        device.device_id.clone(),
        input,
        signals,
        {
            let session = session.clone();
            move |mode_stamp| {
                let (api, session, device, claims) =
                    (api.clone(), session.clone(), device.clone(), claims.clone());
                async move { admit(&api, &session, &device, Some(&claims), mode_stamp).await }
            }
        },
    ));
    Ok(Socket {
        link,
        events,
        last_owner,
        outbound,
        binding,
        order_gate,
        personal_applied,
        session,
        task,
    })
}

impl Socket {
    /// The last decoded authority for this transport, even after link retirement.
    /// This can know about a demotion that never reached the engine's event queue.
    pub(crate) fn last_owner(&self, transport: Generation) -> Option<bool> {
        (*self.last_owner.borrow())
            .filter(|(published, _)| *published == transport)
            .map(|(_, own)| own)
    }

    /// Returns the generated ID after local admission only, not a server receipt.
    pub(crate) fn command(
        &self,
        generation: Generation,
        basis: CommandBasis,
        action: Action,
    ) -> Result<String, SocketError> {
        let mut binding = self.binding.lock().expect("socket binding");
        let b = binding
            .as_mut()
            .filter(|b| b.transport == generation)
            .ok_or(SocketError::NotConnected)?;
        let (index, id) = b.next_command_id()?;
        let command = Command {
            id,
            issued_at_ms: unix_time_ms(),
            basis,
            action,
        };
        if !command.valid() {
            return Err(SocketError::Invalid);
        }
        // Keep allocation and the FIFO write reservation in one critical section.
        self.submit(generation, &command)?;
        b.command_counter = index;
        Ok(command.id)
    }

    /// Publish a cursor the native owner already applied during hello. This shares
    /// command IDs and ordering, but never sends an action or imports a queue.
    pub(crate) fn reconcile_claim(
        &self,
        transport: Generation,
        basis: ReattachBasis,
        cursor: Cursor,
    ) -> Result<String, SocketError> {
        if !basis.valid() || !cursor.valid() {
            return Err(SocketError::Invalid);
        }
        let mut binding = self.binding.lock().expect("socket binding");
        let b = binding
            .as_mut()
            .filter(|b| b.transport == transport && b.owner_generation == Some(basis.generation))
            .ok_or(SocketError::NotConnected)?;
        if b.reconcile_basis.as_ref() != Some(&basis) {
            return Err(SocketError::Invalid);
        }
        let (index, id) = b.next_command_id()?;
        self.submit_fenced(
            transport,
            &Outgoing::<()>::ReconcileClaim {
                id: &id,
                basis: &basis,
                cursor: &cursor,
            },
            None,
            Some(basis.generation),
            None,
        )?;
        b.command_counter = index;
        b.reconcile_basis = None;
        Ok(id)
    }

    pub(crate) fn report(&self, generation: Generation, report: Report) -> Result<(), SocketError> {
        if !report.valid() {
            return Err(SocketError::Invalid);
        }
        let mut binding = self.binding.lock().expect("socket binding");
        let b = binding
            .as_mut()
            .filter(|b| b.transport == generation && b.owner_generation == Some(report.generation))
            .ok_or(SocketError::NotConnected)?;
        let sequence = b.report_sequence + 1;
        if !wire::counter(sequence) {
            return Err(SocketError::Invalid);
        }
        self.submit_fenced(
            generation,
            &Outgoing::<()>::Report {
                sequence,
                report: &report,
            },
            None,
            Some(report.generation),
            None,
        )?;
        b.report_sequence = sequence;
        Ok(())
    }

    pub(crate) fn refresh(
        &self,
        generation: Generation,
        owner_generation: u64,
    ) -> Result<(), SocketError> {
        let binding = self.binding.lock().expect("socket binding");
        if !binding.as_ref().is_some_and(|b| {
            b.transport == generation && b.owner_generation == Some(owner_generation)
        }) {
            return Err(SocketError::NotConnected);
        }
        self.submit_fenced(
            generation,
            &Outgoing::<()>::Refresh {
                generation: owner_generation,
            },
            None,
            Some(owner_generation),
            None,
        )
    }

    pub(crate) fn accept(&self, generation: Generation, offer: Offer) -> Result<(), SocketError> {
        if !offer.valid() {
            return Err(SocketError::Invalid);
        }
        self.submit(
            generation,
            &Outgoing::<()>::Accept {
                id: &offer.id,
                generation: offer.generation,
            },
        )
    }

    pub(crate) fn rename(
        &self,
        generation: Generation,
        device_id: &str,
        name: &str,
    ) -> Result<(), SocketError> {
        if !wire::text(device_id, 128) || !wire::name(name) {
            return Err(SocketError::Invalid);
        }
        self.submit(generation, &Outgoing::<()>::Rename { device_id, name })
    }

    /// The personal replica owns the command/projection schema. The transport
    /// wraps it exactly once and still enforces message size and socket scope.
    pub(crate) fn personal(
        &self,
        generation: Generation,
        command: &(impl Serialize + ?Sized),
    ) -> Result<(), SocketError> {
        self.submit(generation, &Outgoing::Personal { command })
    }

    pub(crate) fn personal_playlist_sync(
        &self,
        epoch: u64,
        generation: Generation,
        command: &PersonalCommand,
        personal: PersonalAccess,
        current_epoch: Arc<AtomicU64>,
    ) -> Result<(), SocketError> {
        self.submit_fenced(
            generation,
            &Outgoing::Personal { command },
            None,
            None,
            Some(ScopedWrite::Playlist(PlaylistRead {
                epoch,
                personal,
                current_epoch,
                command: command.clone(),
            })),
        )
    }

    pub(crate) fn personal_order(
        &self,
        generation: Generation,
        mode_stamp: u64,
        command: &(impl Serialize + ?Sized),
    ) -> Result<(), SocketError> {
        let _gate = self.order_gate.lock().expect("socket order admission gate");
        if !self.events.is_empty() {
            return Err(SocketError::Busy);
        }
        self.submit_fenced(
            generation,
            &Outgoing::Personal { command },
            Some(mode_stamp),
            None,
            None,
        )
    }

    pub(crate) fn personal_quick_add(
        &self,
        personal: PersonalAccess,
        ticket: Ticket,
        epoch: Arc<AtomicU64>,
    ) -> Result<(), SocketError> {
        if epoch.load(Ordering::SeqCst) != ticket.epoch {
            return Err(SocketError::Busy);
        }
        let command = ticket.command();
        let (generation, mode_stamp) = (ticket.generation, ticket.mode_stamp);
        self.submit_fenced(
            generation,
            &Outgoing::Personal { command: &command },
            Some(mode_stamp),
            None,
            Some(ScopedWrite::Quick(QuickWrite {
                personal,
                ticket,
                epoch,
                session: self.session.clone(),
            })),
        )
    }

    pub(crate) fn mark_personal_applied(&self, stamp: u64) {
        self.personal_applied.send_replace(stamp);
    }

    pub(crate) fn bulk_order(&self) -> BulkOrder {
        BulkOrder {
            order_gate: self.order_gate.clone(),
            applied: self.personal_applied.subscribe(),
        }
    }

    /// Repair a playback revision gap. The server also sends its private Personal Initial.
    pub(crate) fn sync(&self, generation: Generation) -> Result<(), SocketError> {
        self.submit(generation, &Outgoing::<()>::Sync)
    }

    fn submit(
        &self,
        generation: Generation,
        frame: &(impl Serialize + ?Sized),
    ) -> Result<(), SocketError> {
        self.submit_fenced(generation, frame, None, None, None)
    }

    fn submit_fenced(
        &self,
        generation: Generation,
        frame: &(impl Serialize + ?Sized),
        expected_mode: Option<u64>,
        owner_generation: Option<u64>,
        scoped: Option<ScopedWrite>,
    ) -> Result<(), SocketError> {
        let raw = encode(frame)?;
        // Hold the mode and link reads through bounded admission. Dispatch
        // also checks the stamped mode under the publication gate.
        let link = self.link.borrow();
        let mode = self.session.network_changes();
        let mode = mode.borrow();
        if *link != Link::Connected(generation)
            || (*mode & 1) != 0
            || expected_mode.is_some_and(|expected| expected != *mode)
            || self.session.is_expired()
        {
            return Err(SocketError::NotConnected);
        }
        let slot = self.outbound.try_reserve().map_err(|err| match err {
            mpsc::error::TrySendError::Full(_) => SocketError::Busy,
            mpsc::error::TrySendError::Closed(_) => SocketError::NotConnected,
        })?;
        slot.send(WriteFrame {
            generation,
            owner_generation,
            mode_stamp: *mode,
            at: Instant::now(),
            raw,
            scoped: scoped.map(Box::new),
        });
        Ok(())
    }
}

#[cfg(test)]
pub(crate) struct TestPeer {
    pub(crate) events: mpsc::Sender<Event>,
    pub(crate) link: watch::Sender<Link>,
    last_owner: watch::Sender<Option<(Generation, bool)>>,
    binding: Arc<Mutex<Option<Binding>>>,
    outbound: mpsc::Receiver<WriteFrame>,
}

#[cfg(test)]
impl TestPeer {
    pub(crate) fn read(&mut self) -> Option<Value> {
        self.outbound
            .try_recv()
            .ok()
            .map(|frame| serde_json::from_str(&frame.raw).unwrap())
    }

    /// Install the opening snapshot's writer scope before delivering the test event.
    pub(crate) fn set_opening_snapshot(&self, snapshot: &Snapshot, own: bool) {
        let mut binding = self.binding.lock().unwrap();
        let transport = binding.as_ref().unwrap().transport;
        *binding = Some(Binding::opening(transport, snapshot, own));
        self.last_owner.send_replace(Some((transport, own)));
    }

    pub(crate) fn set_owner_generation(&self, owner_generation: Option<u64>) {
        let mut binding = self.binding.lock().unwrap();
        let binding = binding.as_mut().unwrap();
        binding.owner_generation = owner_generation;
        self.last_owner
            .send_replace(Some((binding.transport, owner_generation.is_some())));
    }
}

#[cfg(test)]
impl Socket {
    pub(crate) fn test_peer(session: Session, generation: Generation) -> (Self, TestPeer) {
        let (link_out, link) = watch::channel(Link::Connected(generation));
        let (owner_out, last_owner) = watch::channel(None);
        let (event_out, events) = mpsc::channel(EVENT_CAPACITY);
        let (outbound, input) = mpsc::channel(OUTBOUND_CAPACITY);
        let binding = Arc::new(Mutex::new(Some(Binding::new(
            generation,
            "test-connection".into(),
            None,
        ))));
        let socket = Self {
            link,
            events,
            last_owner,
            outbound,
            binding: binding.clone(),
            order_gate: Arc::default(),
            personal_applied: watch::channel(0).0,
            session,
            task: tokio::spawn(pending()),
        };
        (
            socket,
            TestPeer {
                link: link_out,
                last_owner: owner_out,
                binding,
                events: event_out,
                outbound: input,
            },
        )
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct WriteFrame {
    generation: Generation,
    owner_generation: Option<u64>,
    mode_stamp: u64,
    at: Instant,
    raw: String,
    // Only writes with final admission checks allocate local metadata.
    scoped: Option<Box<ScopedWrite>>,
}

enum ScopedWrite {
    Quick(QuickWrite),
    Playlist(PlaylistRead),
}

struct PlaylistRead {
    epoch: u64,
    personal: PersonalAccess,
    current_epoch: Arc<AtomicU64>,
    command: PersonalCommand,
}

impl PlaylistRead {
    fn account_current(&self) -> bool {
        self.current_epoch.load(Ordering::SeqCst) == self.epoch
    }
}

struct QuickWrite {
    personal: PersonalAccess,
    ticket: Ticket,
    epoch: Arc<AtomicU64>,
    session: Session,
}

struct Bounded(Vec<u8>, usize);

impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.1 - self.0.len() {
            return Err(io::Error::other("account socket frame too large"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_limit(frame: &(impl Serialize + ?Sized), limit: usize) -> Result<String, SocketError> {
    let mut bytes = Bounded(Vec::new(), limit);
    serde_json::to_writer(&mut bytes, frame).map_err(|_| SocketError::Invalid)?;
    String::from_utf8(bytes.0).map_err(|_| SocketError::Invalid)
}

fn encode(frame: &(impl Serialize + ?Sized)) -> Result<String, SocketError> {
    encode_limit(frame, MAX_OUTBOUND)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Transport,
    Protocol,
    Backpressure,
    Closed,
    Expired,
    Superseded,
    NetworkChanged,
}

#[derive(Clone)]
struct ClaimRequests {
    events: mpsc::Sender<Event>,
    order_gate: Arc<Mutex<IngressSequence>>,
}

impl ClaimRequests {
    async fn latest(&self) -> Result<Claim, End> {
        let (reply, claim) = oneshot::channel();
        {
            let _gate = self.order_gate.lock().expect("socket order admission gate");
            self.events
                .try_send(Event {
                    generation: 0,
                    kind: EventKind::ClaimRequested(reply),
                    personal_stamp: None,
                })
                .map_err(event_error)?;
        }
        claim.await.map_err(|_| End::Closed)
    }
}

fn event_error<T>(e: mpsc::error::TrySendError<T>) -> End {
    match e {
        mpsc::error::TrySendError::Full(_) => End::Backpressure,
        mpsc::error::TrySendError::Closed(_) => End::Closed,
    }
}

struct Signals {
    order_gate: Arc<Mutex<IngressSequence>>,
    personal_applied: watch::Sender<u64>,
    link: watch::Sender<Link>,
    last_owner: watch::Sender<Option<(Generation, bool)>>,
    binding: Arc<Mutex<Option<Binding>>>,
    events: mpsc::Sender<Event>,
}

impl Signals {
    fn event(
        &self,
        generation: Generation,
        kind: EventKind,
        device: &str,
        first: bool,
    ) -> Result<(), End> {
        let mut gate = self.order_gate.lock().expect("socket order admission gate");
        let owner = match &kind {
            EventKind::Snapshot(s) => Some(&s.owner),
            EventKind::Ownership(o) => Some(&o.owner),
            _ => None,
        };
        if let Some(owner) = owner {
            let own_generation = owner_generation(owner, device);
            // Publish the fence before attempting to enqueue its display projection.
            // Retirement leaves this value intact. A fresh hello must obtain the
            // engine's claim before a new transport can replace it.
            self.last_owner
                .send_replace(Some((generation, own_generation.is_some())));
            if !first {
                let mut binding = self.binding.lock().expect("socket binding");
                if let Some(b) = binding.as_mut() {
                    if b.owner_generation != own_generation {
                        b.report_sequence = 0;
                        b.reconcile_basis = None;
                    }
                    b.owner_generation = own_generation;
                }
            }
        }
        let permit = self.events.try_reserve().map_err(event_error)?;
        if first {
            let EventKind::Snapshot(s) = &kind else {
                unreachable!("first frame is a snapshot")
            };
            let mut binding = self.binding.lock().expect("socket binding");
            *binding = Some(Binding::opening(
                generation,
                s,
                owner_generation(&s.owner, device).is_some(),
            ));
            self.link.send_replace(Link::Connected(generation));
        }
        let personal_stamp = matches!(kind, EventKind::Personal(_)).then(|| {
            gate.personal = gate.personal.wrapping_add(1);
            gate.personal
        });
        permit.send(Event {
            generation,
            kind,
            personal_stamp,
        });
        Ok(())
    }

    fn retire(&self, link: Link) {
        let mut binding = self.binding.lock().expect("socket binding");
        self.link.send_replace(link);
        *binding = None;
    }
}

fn owner_generation(owner: &Owner, device: &str) -> Option<u64> {
    (owner.device_id.as_deref() == Some(device)).then_some(owner.generation)
}

impl Drop for Signals {
    fn drop(&mut self) {
        let link = *self.link.borrow();
        if !matches!(link, Link::Expired | Link::Superseded | Link::Exhausted) {
            self.retire(Link::Closed);
        }
    }
}

type AccountWs = WebSocketStream<reqwest::Upgraded>;

// Go's personal FIFO allows up to 15 seconds per job. An eight-write
// window plus its Initial can therefore need 135 seconds without other
// clients. Allow bounded headroom, while still limiting ingress separately.
const BULK_CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(180);
const BULK_SCOPED_TIMEOUT: Duration = Duration::from_secs(45);
const BULK_REPLY_FRAMES: usize = 256;

/// A single-run, non-playing link. The primary socket alone feeds the shared
/// replica. This link sees private replies only as FIFO barriers, not receipts.
pub(crate) struct BulkLink<S = reqwest::Upgraded> {
    ws: Option<WebSocketStream<S>>,
    connection_id: String,
    device_id: String,
    session: Session,
    mode_stamp: u64,
    pending: usize,
    target: Option<String>,
}

impl BulkLink {
    pub(crate) async fn open(
        api: &Api,
        session: &Session,
        mode_stamp: u64,
        device_id: &str,
        device_name: &str,
    ) -> Result<Self, SocketError> {
        if !wire::text(device_id, 128) || !wire::name(device_name) {
            return Err(SocketError::Invalid);
        }
        // V3 atomically replaces *every* binding of a device ID. The private,
        // non-playing FIFO must never present the primary installation's ID.
        let device = DeviceIdentity {
            device_id: format!("bulk:{}", uuid::Uuid::new_v4()),
            name: device_name.into(),
            can_play: false,
        };
        bulk_scope(session, mode_stamp, async {
            let (ws, initial) = time::timeout(
                ATTEMPT_TIMEOUT,
                admit(api, session, &device, None, mode_stamp),
            )
            .await
            .map_err(|_| End::Transport)??;
            Self::finish_open(ws, initial, session, mode_stamp, &device.device_id).await
        })
        .await
        .map_err(bulk_error)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> BulkLink<S> {
    async fn finish_open(
        mut ws: WebSocketStream<S>,
        initial: Incoming,
        session: &Session,
        mode_stamp: u64,
        device_id: &str,
    ) -> Result<Self, End> {
        let Incoming::Snapshot(Snapshot { connection_id, .. }) = initial else {
            return Err(End::Protocol);
        };
        bulk_reply(
            &mut ws,
            device_id,
            &connection_id,
            "snapshot",
            SNAPSHOT_TIMEOUT,
            |update| matches!(update, PersonalUpdate::Snapshot(_)).then_some(()),
        )
        .await?;
        Ok(Self {
            ws: Some(ws),
            connection_id,
            device_id: device_id.into(),
            session: session.clone(),
            mode_stamp,
            pending: 0,
            target: None,
        })
    }

    /// `start_guard` runs under PUBLICATION_GATE after write readiness. Call
    /// `start` synchronously only while the exact scope and membership permit
    /// it. A true `start()` is an attempt even if this future then fails or is
    /// canceled; record it inside the guard, not after awaiting `add`.
    /// Ok(false) means the guard did not start a write. Ok(true) is only flush.
    pub(crate) async fn add(
        &mut self,
        command: &PersonalCommand,
        start_guard: impl FnOnce(&mut dyn FnMut() -> bool) -> bool,
    ) -> Result<bool, SocketError> {
        let PersonalCommand::AddEntry { playlist_id, .. } = command else {
            return Err(SocketError::Invalid);
        };
        if !command.valid()
            || self
                .target
                .as_ref()
                .is_some_and(|target| target != playlist_id)
        {
            return Err(SocketError::Invalid);
        }
        if self.pending >= BULK_WINDOW {
            return Err(SocketError::Busy);
        }
        let raw = encode(&Outgoing::Personal { command })?;
        // A dropped future (e.g. caller select!) or transport failure retires
        // this link, including any partial frame; nothing can resume or replay.
        let mut ws = self.ws.take().ok_or(SocketError::NotConnected)?;
        let started = bulk_scope(&self.session, self.mode_stamp, async {
            send_bulk(&mut ws, raw, &self.session, self.mode_stamp, start_guard).await
        })
        .await
        .map_err(bulk_error)?;
        self.ws = Some(ws);
        if started {
            self.pending += 1;
            self.target.get_or_insert_with(|| playlist_id.clone());
        }
        Ok(started)
    }

    /// Return the target revision after this link's FIFO barrier. In the
    /// bounded Initial list the target may be absent; a same-link, tokenless
    /// offset-zero read then supplies its revision, without fetching more rows.
    pub(crate) async fn checkpoint(&mut self, target: &str) -> Result<u64, SocketError> {
        let part = PersonalCommand::SyncPlaylist {
            playlist_id: target.into(),
            known: None,
            offset: 0,
            local_request: Arc::new(()),
        };
        if !part.valid() || self.target.as_deref().is_some_and(|id| id != target) {
            return Err(SocketError::Invalid);
        }
        let sync = encode(&Outgoing::<()>::Sync)?;
        let mut ws = self.ws.take().ok_or(SocketError::NotConnected)?;
        let revision = bulk_scope(&self.session, self.mode_stamp, async {
            send_bulk(&mut ws, sync, &self.session, self.mode_stamp, |start| {
                start()
            })
            .await?;
            // Go replies immediately with a playback Snapshot, then queues a
            // private personal Initial behind our writes. Only the latter can
            // confirm the FIFO ran; ordinary personal changes are broadcasts.
            let revision = bulk_reply(
                &mut ws,
                &self.device_id,
                &self.connection_id,
                "snapshot",
                BULK_CHECKPOINT_TIMEOUT,
                |update| {
                    let PersonalUpdate::Snapshot(snapshot) = update else {
                        return None;
                    };
                    Some(
                        snapshot
                            .playlists
                            .into_iter()
                            .find(|p| p.id == target)
                            .map(|p| p.revision),
                    )
                },
            )
            .await?;
            if let Some(revision) = revision {
                return Ok(revision);
            }
            let raw = encode(&Outgoing::Personal { command: &part }).map_err(|_| End::Protocol)?;
            send_bulk(&mut ws, raw, &self.session, self.mode_stamp, |start| {
                start()
            })
            .await?;
            bulk_reply(
                &mut ws,
                &self.device_id,
                &self.connection_id,
                "part",
                BULK_SCOPED_TIMEOUT,
                |update| match update {
                    PersonalUpdate::Part(part) if part.summary.id == target && part.offset == 0 => {
                        Some(part.summary.revision)
                    }
                    _ => None,
                },
            )
            .await
        })
        .await
        .map_err(bulk_error)?;
        self.ws = Some(ws);
        self.pending = 0;
        Ok(revision)
    }

    /// Service control frames while the owner awaits primary replica progress.
    /// This does not send application frames and is safe to drop and reuse.
    /// Tungstenite retains partial reads and queued Pongs in the socket; the
    /// next read or write flushes any Pong interrupted by the caller's select.
    pub(crate) async fn maintain(&mut self) -> Result<(), SocketError> {
        let result = bulk_scope(&self.session, self.mode_stamp, async {
            let ws = self.ws.as_mut().ok_or(End::Closed)?;
            for _ in 0..BULK_REPLY_FRAMES {
                match receive(ws, &self.device_id, true).await? {
                    Incoming::Snapshot(s) if s.connection_id != self.connection_id => {
                        return Err(End::Protocol);
                    }
                    Incoming::Personal { update }
                        if update.get("snapshot").is_some() || update.get("part").is_some() =>
                    {
                        // No request is outstanding. Never silently leave an
                        // unsolicited private reply to a future checkpoint.
                        return Err(End::Protocol);
                    }
                    _ => (),
                }
                tokio::task::consume_budget().await;
            }
            Err(End::Backpressure)
        })
        .await;
        if result.is_err() {
            self.ws = None;
        }
        result.map_err(bulk_error)
    }
}

fn bulk_current(session: &Session, mode_stamp: u64) -> Result<(), End> {
    if mode_stamp & 1 != 0 || *session.network_changes().borrow() != mode_stamp {
        return Err(End::NetworkChanged);
    }
    if session.is_expired() {
        return Err(End::Expired);
    }
    Ok(())
}

async fn bulk_scope<T>(
    session: &Session,
    mode_stamp: u64,
    work: impl Future<Output = Result<T, End>>,
) -> Result<T, End> {
    let mut mode = session.network_changes();
    bulk_current(session, mode_stamp)?;
    let result = tokio::select! {
        biased;
        _ = mode.changed() => return Err(End::NetworkChanged),
        _ = local_expiry(session) => return Err(End::Expired),
        result = work => result?,
    };
    bulk_current(session, mode_stamp)?;
    Ok(result)
}

async fn send_bulk<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    raw: String,
    session: &Session,
    mode_stamp: u64,
    start_guard: impl FnOnce(&mut dyn FnMut() -> bool) -> bool,
) -> Result<bool, End> {
    wait_write_ready(ws).await?;
    let mut raw = Some(raw);
    let mut result = Ok(());
    {
        let _publication = PUBLICATION_GATE.lock().expect("account publication gate");
        bulk_current(session, mode_stamp)?;
        start_guard(&mut || {
            // Mode and explicit revocation are serialized by the gate, but
            // the credential's natural deadline can elapse during the guard.
            if session.is_expired() {
                result = Err(End::Expired);
                return false;
            }
            let Some(raw) = raw.take() else {
                return false;
            };
            result = Pin::new(&mut *ws)
                .start_send(Message::Text(raw.into()))
                .map_err(|_| End::Transport);
            true
        });
    }
    result?;
    let started = raw.is_none();
    if started {
        flush_scoped(ws).await?;
    }
    Ok(started)
}

async fn bulk_reply<S: AsyncRead + AsyncWrite + Unpin, T>(
    ws: &mut WebSocketStream<S>,
    device_id: &str,
    connection_id: &str,
    kind: &str,
    timeout: Duration,
    pick: impl Fn(PersonalUpdate) -> Option<T>,
) -> Result<T, End> {
    time::timeout(timeout, async {
        for _ in 0..BULK_REPLY_FRAMES {
            match receive(ws, device_id, true).await? {
                Incoming::Snapshot(s) if s.connection_id != connection_id => {
                    return Err(End::Protocol);
                }
                Incoming::Personal { update } if update.get(kind).is_some() => {
                    let update = serde_json::from_value(update).map_err(|_| End::Protocol)?;
                    if let Some(reply) = pick(update) {
                        return Ok(reply);
                    }
                }
                _ => (),
            }
            tokio::task::consume_budget().await;
        }
        Err(End::Backpressure)
    })
    .await
    .map_err(|_| End::Transport)?
}

fn bulk_error(_: End) -> SocketError {
    SocketError::NotConnected
}

fn encode_hello(
    ticket: &str,
    device: &DeviceIdentity,
    claim: Option<&Claim>,
) -> Result<String, SocketError> {
    let claim = claim.filter(|_| device.can_play);
    if claim.is_some_and(|c| !c.valid()) {
        return Err(SocketError::Invalid);
    }
    let local = claim.and_then(Claim::local);
    if let Some(local) = local {
        // Enforce the separate state budget as well as the total hello budget.
        encode_limit(local, MAX_STATE)?;
    }
    encode_limit(
        &Outgoing::<()>::Hello {
            ticket,
            name: &device.name,
            device_id: &device.device_id,
            can_play: device.can_play,
            local,
            reattach_basis: claim.and_then(|c| c.reattach_basis.as_ref()),
        },
        MAX_HELLO,
    )
}

async fn admit(
    api: &Api,
    session: &Session,
    device: &DeviceIdentity,
    claims: Option<&ClaimRequests>,
    mode_stamp: u64,
) -> Result<(AccountWs, Incoming), End> {
    bulk_current(session, mode_stamp)?;
    let AccountUpgrade { upgraded, ticket } = api.account_upgrade(session).await.map_err(|e| {
        if e.expired {
            End::Expired
        } else {
            End::Transport
        }
    })?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_INBOUND))
        .max_frame_size(Some(MAX_INBOUND))
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_HELLO + 1024);
    let mut ws = WebSocketStream::from_raw_socket(upgraded, Role::Client, Some(config)).await;
    let first = hello_snapshot(&mut ws, &ticket, session, device, claims, mode_stamp).await?;
    Ok((ws, first))
}

async fn hello_snapshot<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    ticket: &str,
    session: &Session,
    device: &DeviceIdentity,
    claims: Option<&ClaimRequests>,
    mode_stamp: u64,
) -> Result<Incoming, End> {
    // Sample at the last possible moment, after upgrade. A reconnect must let
    // the actor process detachment and read the current native/local state, not
    // reuse a pre-loss export. The outer attempt bounds this rendezvous too.
    let claim = match claims {
        Some(requests) => Some(requests.latest().await?),
        None => None,
    };
    let raw = encode_hello(ticket, device, claim.as_ref()).map_err(|_| End::Protocol)?;
    send_bulk(ws, raw, session, mode_stamp, |start| start()).await?;
    let first = time::timeout(
        SNAPSHOT_TIMEOUT,
        receive(ws, &device.device_id, claims.is_none()),
    )
    .await
    .map_err(|_| End::Transport)??;
    matches!(first, Incoming::Snapshot(_))
        .then_some(first)
        .ok_or(End::Protocol)
}

async fn wait_write_ready<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
) -> Result<(), End> {
    time::timeout(
        WRITE_TIMEOUT,
        poll_fn(|cx| Pin::new(&mut *ws).poll_ready(cx)),
    )
    .await
    .map_err(|_| End::Transport)?
    .map_err(|_| End::Transport)
}

async fn flush_scoped<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
) -> Result<(), End> {
    time::timeout(WRITE_TIMEOUT, ws.flush())
        .await
        .map_err(|_| End::Transport)?
        .map_err(|_| End::Transport)
}

async fn send_playlist_read<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    raw: String,
    read: PlaylistRead,
    generation: Generation,
    queued: Instant,
    session: &Session,
    mode_stamp: u64,
) -> Result<(), End> {
    // An obsolete read must not trip the generic queue-age failure before
    // we notice that its exact local scope retired.
    let allowed =
        read.personal
            .inspect_playlist_sync(read.epoch, generation, &read.command, |optional| {
                if !read.account_current() {
                    Ok(false)
                } else if queued.elapsed() <= MAX_QUEUE_AGE {
                    Ok(true)
                } else if optional {
                    Ok(false) // Leave the optional Search read to its own bounded timeout.
                } else {
                    Err(End::Backpressure)
                }
            });
    if !allowed.unwrap_or(Ok(false))? {
        return Ok(());
    }
    if let Err(error) = wait_write_ready(ws).await {
        return if read
            .personal
            .inspect_playlist_sync(read.epoch, generation, &read.command, |_| {
                read.account_current()
            })
            == Some(true)
        {
            Err(error)
        } else {
            Ok(()) // Scope retired while waiting for write readiness.
        };
    }
    let mut result = Ok(());
    let started = {
        let _publication = PUBLICATION_GATE.lock().expect("account publication gate");
        bulk_current(session, mode_stamp)?;
        read.personal
            .start_playlist_sync(read.epoch, generation, &read.command, || {
                if !read.account_current() || session.is_expired() {
                    return false;
                }
                result = Pin::new(&mut *ws).start_send(Message::Text(raw.into()));
                true
            })
    };
    if started {
        result.map_err(|_| End::Transport)?;
        flush_scoped(ws).await?;
    }
    Ok(())
}

async fn quick_ready(signals: &Signals, deadline: Option<Instant>) -> bool {
    let Some(deadline) = deadline else {
        return pending().await;
    };
    let published = signals
        .order_gate
        .lock()
        .expect("socket order admission gate")
        .personal;
    let mut applied = signals.personal_applied.subscribe();
    matches!(
        time::timeout_at(
            deadline.into(),
            applied.wait_for(|stamp| *stamp == published)
        )
        .await,
        Ok(Ok(_))
    )
}

async fn send_quick<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    raw: String,
    quick: QuickWrite,
    deadline: Instant,
) -> Result<(), End> {
    match time::timeout_at(deadline.into(), wait_write_ready(ws)).await {
        Ok(result) if Instant::now() <= deadline => result?,
        _ => {
            quick.personal.quick_cancel(&quick.ticket);
            return Ok(());
        }
    }
    let mut result = Ok(());
    let started = {
        // Account epoch changes, explicit mode changes and credential
        // revocation share this gate. Match publication → mode → lifetime.
        let _publication = PUBLICATION_GATE.lock().expect("account publication gate");
        quick.personal.start_quick_add(&quick.ticket, || {
            let mode = quick.session.network_changes();
            let mode = mode.borrow();
            if quick.epoch.load(Ordering::SeqCst) != quick.ticket.epoch
                || *mode != quick.ticket.mode_stamp
                || quick.session.is_expired()
            {
                return false;
            }
            result = Pin::new(&mut *ws).start_send(Message::Text(raw.into()));
            true // A transport error at/after start_send is not proof of no write.
        })
    };
    if started {
        result.map_err(|_| End::Transport)?;
        flush_scoped(ws).await?;
    }
    Ok(())
}

async fn receive<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    device: &str,
    bulk: bool,
) -> Result<Incoming, End> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(raw))) => return decode_link(&raw, device, bulk),
            // Tungstenite queues the matching Pong, including before snapshot.
            Some(Ok(Message::Ping(_))) => flush_scoped(ws).await?,
            Some(Ok(Message::Pong(_))) => (),
            Some(Ok(Message::Close(frame))) => return Err(close_end(frame)),
            None => return Err(End::Transport),
            Some(Ok(_)) => return Err(End::Protocol),
            Some(Err(err)) => return Err(classify_wire_error(err)),
        }
        if bulk {
            tokio::task::consume_budget().await;
        }
    }
}

fn decode(raw: &str, device: &str) -> Result<Incoming, End> {
    decode_link(raw, device, false)
}

fn decode_link(raw: &str, device: &str, bulk: bool) -> Result<Incoming, End> {
    if raw.len() > MAX_INBOUND {
        return Err(End::Protocol);
    }
    let incoming: Incoming = serde_json::from_str(raw).map_err(|_| End::Protocol)?;
    incoming
        .valid(device, bulk)
        .then_some(incoming)
        .ok_or(End::Protocol)
}

pub(crate) fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(MAX_SAFE_INTEGER)) as u64
}

#[derive(Clone, Copy)]
struct ClockPoint {
    wall_ms: u64,
    monotonic: Instant,
}

impl ClockPoint {
    fn now() -> Self {
        Self {
            wall_ms: unix_time_ms(),
            monotonic: Instant::now(),
        }
    }
}

#[derive(Default)]
struct Clock {
    pending: Option<ClockPoint>,
    wall: Option<ClockPoint>,
    samples: VecDeque<(Duration, i64)>,
    best: SampleClock,
}

impl Clock {
    fn check_wall(&mut self, now: ClockPoint) -> Option<SampleClock> {
        if let Some(previous) = self.wall.replace(now) {
            let elapsed = now.monotonic.duration_since(previous.monotonic).as_millis() as i128;
            let wall = i128::from(now.wall_ms) - i128::from(previous.wall_ms);
            if (wall - elapsed).unsigned_abs() > CLOCK_JUMP_MS {
                self.pending = None;
                self.samples.clear();
                return self.publish(SampleClock::default());
            }
        }
        None
    }

    fn pong(
        &mut self,
        now: ClockPoint,
        client_time_ms: u64,
        server_time_ms: u64,
    ) -> Option<SampleClock> {
        let cleared = self.check_wall(now);
        let Some(sent) = self.pending.filter(|s| s.wall_ms == client_time_ms) else {
            return cleared;
        };
        self.pending = None;
        let elapsed = now.monotonic.duration_since(sent.monotonic);
        let offset = server_time_ms as i64 - sent.wall_ms as i64 - (elapsed.as_millis() / 2) as i64;
        if !wire::counter(offset.unsigned_abs()) {
            return None;
        }
        if self.samples.len() == CLOCK_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((elapsed, offset));
        let (rtt, offset) = self
            .samples
            .iter()
            .min_by_key(|(rtt, _)| *rtt)
            .copied()
            .expect("clock sample");
        self.publish(SampleClock {
            offset_ms: Some(offset),
            rtt_ms: Some(rtt.as_millis() as u64),
        })
    }

    fn publish(&mut self, sample: SampleClock) -> Option<SampleClock> {
        (std::mem::replace(&mut self.best, sample) != sample).then_some(sample)
    }
}

struct Connection {
    generation: Generation,
    id: String,
    owner_generation: Option<u64>,
    clock: Clock,
}

impl Connection {
    fn begin(initial: Incoming, device: &str, signals: &Signals) -> Result<Self, End> {
        let Incoming::Snapshot(snapshot) = initial else {
            return Err(End::Protocol);
        };
        let previous = GENERATIONS
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| End::Closed)?;
        let generation = previous + 1;
        let result = Self {
            generation,
            id: snapshot.connection_id.clone(),
            owner_generation: owner_generation(&snapshot.owner, device),
            clock: Clock::default(),
        };
        signals.event(generation, EventKind::Snapshot(snapshot), device, true)?;
        Ok(result)
    }

    fn receive(&mut self, msg: Incoming, device: &str, signals: &Signals) -> Result<(), End> {
        let event = match msg {
            Incoming::Snapshot(s) => {
                if s.connection_id != self.id {
                    return Err(End::Protocol);
                }
                self.owner_generation = owner_generation(&s.owner, device);
                EventKind::Snapshot(s)
            }
            Incoming::Ownership(o) => {
                self.owner_generation = owner_generation(&o.owner, device);
                EventKind::Ownership(o)
            }
            Incoming::State(s) => EventKind::State(s),
            Incoming::Execute(e) => EventKind::Execute(e),
            Incoming::Observation(o) => EventKind::Observation(o),
            Incoming::Offer(o) => EventKind::Offer(o),
            Incoming::Result(r) => EventKind::Result(r),
            Incoming::Pong {
                client_time_ms,
                server_time_ms,
            } => {
                let Some(sample) =
                    self.clock
                        .pong(ClockPoint::now(), client_time_ms, server_time_ms)
                else {
                    return Ok(());
                };
                EventKind::SampleClock(sample)
            }
            Incoming::Personal { update } => EventKind::Personal(update),
        };
        signals.event(self.generation, event, device, false)
    }

    async fn drive<S: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        ws: &mut WebSocketStream<S>,
        device: &str,
        input: &mut mpsc::Receiver<WriteFrame>,
        signals: &Signals,
        session: &Session,
        mode_stamp: u64,
    ) -> Result<(), End> {
        // Take the first clock sample immediately so followers can interpret
        // fresh observations without waiting for the periodic clock tick.
        let mut ping = Instant::now();
        let mut idle = ping + SOCKET_IDLE;
        // Park only Quick Add frames until the engine has applied everything
        // this socket read. Ordinary controls and ingress keep flowing.
        let mut quick_writes: VecDeque<(String, QuickWrite, Instant)> = VecDeque::new();
        loop {
            let quick_deadline = quick_writes.front().map(|(_, _, at)| *at);
            // Keep each websocket read cancel-safe: Pong flushing happens
            // after the select, not inside a future that another branch drops.
            tokio::select! {
                biased;
                _ = signals.events.closed() => return Err(End::Closed),
                _ = time::sleep_until(idle.into()) => return Err(End::Transport),
                _ = time::sleep_until(ping.into()) => {
                    let now = ClockPoint::now();
                    if let Some(reset) = self.clock.check_wall(now) {
                        signals.event(self.generation, EventKind::SampleClock(reset), device, false)?;
                    }
                    let raw = encode(&Outgoing::<()>::Ping { client_time_ms: now.wall_ms }).map_err(|_| End::Protocol)?;
                    send_bulk(ws, raw, session, mode_stamp, |start| start()).await?;
                    self.clock.pending = Some(now);
                    ping = Instant::now() + CLOCK_INTERVAL;
                }
                // Unbiased across application reads/writes so neither starves the other.
                selected = async {
                    tokio::select! {
                        m = ws.next() => Drive::Read(m),
                        msg = input.recv() => Drive::Write(msg),
                        ready = quick_ready(signals, quick_deadline) => Drive::Quick(ready),
                    }
                } => match selected {
                    Drive::Quick(ready) => {
                        let (raw, quick, deadline) = quick_writes.pop_front().expect("scoped quick write");
                        // No other actor publishes a wire frame between this
                        // select branch and start_send. Local guards still apply.
                        if ready && Instant::now() <= deadline {
                            send_quick(ws, raw, quick, deadline).await?;
                        } else {
                            quick.personal.quick_cancel(&quick.ticket);
                        }
                    }
                    Drive::Write(Some(frame)) => {
                        if frame.generation != self.generation || frame.mode_stamp != mode_stamp { continue; }
                        if let Some(scoped) = frame.scoped {
                            match *scoped {
                                ScopedWrite::Quick(quick) => {
                                    if frame.at.elapsed() > MAX_QUEUE_AGE || quick_writes.len() >= OUTBOUND_CAPACITY {
                                        quick.personal.quick_cancel(&quick.ticket);
                                    } else {
                                        quick_writes.push_back((frame.raw, quick, frame.at + MAX_QUEUE_AGE));
                                    }
                                }
                                ScopedWrite::Playlist(read) => {
                                    send_playlist_read(ws, frame.raw, read, self.generation, frame.at, session, mode_stamp).await?;
                                }
                            }
                            continue;
                        }
                        if frame.owner_generation.is_some() && frame.owner_generation != self.owner_generation { continue; }
                        if frame.at.elapsed() > MAX_QUEUE_AGE { return Err(End::Backpressure); }
                        send_bulk(ws, frame.raw, session, mode_stamp, |start| start()).await?;
                    }
                    Drive::Write(None) => return Err(End::Closed),
                    Drive::Read(message) => {
                        idle = Instant::now() + SOCKET_IDLE;
                        match message {
                            Some(Ok(Message::Text(raw))) => self.receive(decode(&raw, device)?, device, signals)?,
                            Some(Ok(Message::Ping(_))) => flush_scoped(ws).await?,
                            Some(Ok(Message::Pong(_))) => (),
                            Some(Ok(Message::Close(frame))) => return Err(close_end(frame)),
                            None => return Err(End::Transport),
                            Some(Ok(_)) => return Err(End::Protocol),
                            Some(Err(err)) => return Err(classify_wire_error(err)),
                        }
                    }
                }
            }
        }
    }
}

enum Drive {
    Read(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
    Write(Option<WriteFrame>),
    Quick(bool),
}

fn close_end(frame: Option<tokio_tungstenite::tungstenite::protocol::CloseFrame>) -> End {
    if frame.is_some_and(|frame| u16::from(frame.code) == 4001) {
        End::Superseded
    } else {
        End::Transport
    }
}

fn classify_wire_error(err: tokio_tungstenite::tungstenite::Error) -> End {
    use tokio_tungstenite::tungstenite::{Error, error::ProtocolError};
    match err {
        Error::Protocol(ProtocolError::ResetWithoutClosingHandshake) => End::Transport,
        Error::Capacity(_) | Error::Utf8(_) | Error::Protocol(_) => End::Protocol,
        _ => End::Transport,
    }
}

async fn local_expiry(session: &Session) {
    // Session revocation is an atomic shared with existing HTTP methods, not
    // a watch. No additional HTTP polling, credential refresh, or fallback.
    while !session.is_expired() {
        time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
#[path = "account_socket_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "bulk_socket_tests.rs"]
mod bulk_tests;

async fn run_links<S, F>(
    session: Session,
    device: String,
    mut input: mpsc::Receiver<WriteFrame>,
    signals: Signals,
    mut admit: impl FnMut(u64) -> F,
) where
    S: AsyncRead + AsyncWrite + Unpin,
    F: Future<Output = Result<(WebSocketStream<S>, Incoming), End>>,
{
    let mut mode = session.network_changes();
    let mut previous_stamp = None;
    let mut failures = 0;
    let expiry = local_expiry(&session);
    tokio::pin!(expiry);
    loop {
        let stamp = *mode.borrow_and_update();
        if previous_stamp != Some(stamp) {
            failures = 0;
            previous_stamp = Some(stamp);
        }
        if session.is_expired() {
            signals.retire(Link::Expired);
            return;
        }
        if stamp & 1 != 0 || failures == ONLINE_ATTEMPTS {
            signals.retire(if stamp & 1 != 0 {
                Link::Disconnected
            } else {
                Link::Exhausted
            });
            tokio::select! {
                biased;
                _ = &mut expiry => { signals.retire(Link::Expired); return; }
                _ = signals.events.closed() => return,
                _ = mode.changed() => (),
            }
            continue;
        }
        let admitted = tokio::select! {
            biased;
            _ = &mut expiry => Err(End::Expired),
            _ = mode.changed() => Err(End::NetworkChanged),
            _ = signals.events.closed() => Err(End::Closed),
            result = time::timeout(ATTEMPT_TIMEOUT, admit(stamp)) => result.unwrap_or(Err(End::Transport)),
        };
        let mut established = false;
        let ended = match admitted {
            Ok((mut ws, initial)) => match bulk_current(&session, stamp)
                .and_then(|()| Connection::begin(initial, &device, &signals))
            {
                Ok(mut connection) => {
                    established = true;
                    tokio::select! {
                        biased;
                        _ = &mut expiry => End::Expired,
                        _ = mode.changed() => End::NetworkChanged,
                        result = connection.drive(&mut ws, &device, &mut input, &signals, &session, stamp) => result.err().unwrap_or(End::Closed),
                    }
                }
                Err(ended) => ended,
            },
            Err(ended) => ended,
        };
        signals.retire(match ended {
            End::Expired => Link::Expired,
            End::Superseded => Link::Superseded,
            End::Closed => Link::Closed,
            _ => Link::Disconnected,
        });
        while input.try_recv().is_ok() {}
        if matches!(ended, End::Expired | End::Closed | End::Superseded) {
            return;
        }
        if established {
            failures = 0;
        } else if ended != End::NetworkChanged {
            failures += 1;
        }
    }
}
