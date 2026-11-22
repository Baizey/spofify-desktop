//! One authenticated account-state.v1 socket. No fallback, command receipts, or replay.

#[path = "account_wire.rs"]
mod wire;

pub(crate) use wire::{
    Artist, Command, Context, ContextKind, Cursor, Fact, FactError, Intent, MAX_SAFE_INTEGER,
    Outputs, PlayerDelta, QueueEditKind, Repeat, Selection, State, Track, TrackRef,
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
    time::{Duration, Instant},
};

use futures_util::{Sink, SinkExt, StreamExt};
use serde::Serialize;
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncWrite},
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

use crate::{
    api::{Api, Session},
    personal::PersonalCommand,
    personal_bridge::PersonalAccess,
    playlist_pin_store::PUBLICATION_GATE,
    quick_add::Ticket,
};
use wire::{Incoming, Outgoing};

pub(crate) const SUBPROTOCOL: &str = "spofify.account-state.v1";
// Personal changes can bypass socketRoom.emit's smaller player/frame limit.
const MAX_INBOUND: usize = 5 << 20; // Go DefaultOutboundBytes / browser receiver
const MAX_OUTBOUND: usize = 64 << 10;
const OUTBOUND_CAPACITY: usize = 16;
const EVENT_CAPACITY: usize = 8;
const MAX_QUEUE_AGE: Duration = Duration::from_millis(500);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);
const SOCKET_IDLE: Duration = Duration::from_secs(15); // Go pings every five seconds
const PRESENCE_INTERVAL: Duration = Duration::from_secs(2);
// Maximum server output grant since Go bab272b. Shorter grants keep their own deadline.
const MAX_LEASE: Duration = Duration::from_secs(15);
const RETRY_MIN: Duration = Duration::from_millis(300);
const RETRY_MAX: Duration = Duration::from_secs(8);

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

/// A process-local socket incarnation; never a server revision or a lease identity.
pub(crate) type Generation = u64;
static GENERATIONS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Link {
    Connecting,
    Connected(Generation),
    Disconnected,
    Expired,
    Closed,
}

/// Only an existing native operation may continue during detached grace. Never
/// start new work from a detached grant or use it to admit a socket write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutputAuthority {
    pub(crate) generation: Generation,
    pub(crate) deadline: Instant,
    pub(crate) attached: bool,
    // A grant answers a presence sent after these local Resume writes. Go
    // handles them synchronously in wire order; this is not a Play receipt.
    pub(crate) resume_through: u64,
}

impl OutputAuthority {
    pub(crate) fn permits(self, now: Instant) -> bool {
        now < self.deadline
    }
}

/// Immediate sample-side fence for direct starts, before the engine drains
/// the corresponding projection. A fresh visible selection may not have a
/// grant yet; a known expired grant is not that case.
#[derive(Default)]
pub(crate) struct DirectStartFence(AtomicU64);
impl DirectStartFence {
    // The low bit says a known lease expired. Keep it in the same atomic so
    // native PCM cannot miss the generation change across a renewal.
    pub(crate) fn stamp(&self) -> Option<u64> {
        let state = self.0.load(Ordering::Acquire);
        (state & 1 == 0).then_some(state)
    }
    fn invalidate(&self) {
        self.0.fetch_add(2, Ordering::AcqRel);
    }
    fn set_expired(&self, expired: bool) {
        if expired {
            self.0.fetch_or(1, Ordering::AcqRel);
            self.invalidate();
        } else {
            self.0.fetch_and(!1, Ordering::AcqRel);
        }
    }
}

#[derive(Debug)]
pub(crate) struct Event {
    pub(crate) generation: Generation,
    pub(crate) kind: EventKind,
}

#[derive(Debug)]
pub(crate) enum EventKind {
    Snapshot {
        connection_id: String,
        player: State,
        outputs: Outputs,
    },
    Player(PlayerDelta),
    Outputs(Outputs),
    Personal(Value),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SocketError {
    Invalid,
    NotConnected,
    Busy,
}

pub(crate) struct Socket {
    pub(crate) link: watch::Receiver<Link>,
    pub(crate) authority: watch::Receiver<Option<OutputAuthority>>,
    pub(crate) direct_start: Arc<DirectStartFence>,
    pub(crate) events: mpsc::Receiver<Event>,
    outbound: mpsc::Sender<WriteFrame>,
    resume_sequence: AtomicU64,
    order_gate: Arc<Mutex<u64>>,
    actual: watch::Sender<ActualSample>,
    session: Session,
    task: tokio::task::JoinHandle<()>,
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
    let (authority_out, authority) = watch::channel(None);
    let (actual, _) = watch::channel(None);
    let direct_start: Arc<DirectStartFence> = Arc::default();
    let order_gate = Arc::new(Mutex::new(0));
    let signals = Signals {
        order_gate: order_gate.clone(),
        link: link_out,
        authority: authority_out,
        direct_start: direct_start.clone(),
        events: event_out,
    };
    let task = tokio::spawn(run(
        api,
        session.clone(),
        device,
        input,
        signals,
        actual.clone(),
    ));
    Ok(Socket {
        link,
        authority,
        direct_start,
        events,
        outbound,
        resume_sequence: AtomicU64::new(0),
        order_gate,
        actual,
        session,
        task,
    })
}

impl Socket {
    /// Ok means locally admitted to a bounded, generation-scoped buffer only.
    pub(crate) fn command(
        &self,
        generation: Generation,
        command: Command,
    ) -> Result<Option<u64>, SocketError> {
        if !command.valid() {
            return Err(SocketError::Invalid);
        }
        let class = if matches!(command, Command::Release) {
            Class::Release
        } else {
            Class::Choice
        };
        let resume = matches!(command, Command::Resume)
            .then(|| self.resume_sequence.fetch_add(1, Ordering::Relaxed) + 1);
        self.submit(generation, class, &command, resume)?;
        Ok(resume)
    }

    pub(crate) fn reorder(
        &self,
        generation: Generation,
        mode_stamp: u64,
        command: Command,
    ) -> Result<(), SocketError> {
        if !matches!(command, Command::Move { .. }) || !command.valid() {
            return Err(SocketError::Invalid);
        }
        self.submit_order(generation, mode_stamp, &command)
    }

    pub(crate) fn fact(&self, generation: Generation, fact: Fact) -> Result<(), SocketError> {
        if !fact.valid() {
            return Err(SocketError::Invalid);
        }
        if !self
            .authority
            .borrow()
            .is_some_and(|a| a.generation == generation && a.attached && a.permits(Instant::now()))
        {
            return Err(SocketError::NotConnected);
        }
        self.submit(
            generation,
            Class::Fact,
            &Outgoing::<()>::Fact { fact: &fact },
            None,
        )
    }

    /// The personal replica owns the command/projection schema. The transport
    /// wraps it exactly once and still enforces message size and socket scope.
    pub(crate) fn personal(
        &self,
        generation: Generation,
        command: &(impl Serialize + ?Sized),
    ) -> Result<(), SocketError> {
        self.submit(
            generation,
            Class::Choice,
            &Outgoing::Personal { command },
            None,
        )
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
            Class::Choice,
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
        self.submit_order(generation, mode_stamp, &Outgoing::Personal { command })
    }

    pub(crate) fn personal_quick_add(
        &self,
        personal: PersonalAccess,
        ticket: Ticket,
        epoch: Arc<AtomicU64>,
    ) -> Result<(), SocketError> {
        let gate = self.order_gate.lock().expect("socket order admission gate");
        if !self.events.is_empty() || epoch.load(Ordering::SeqCst) != ticket.epoch {
            return Err(SocketError::Busy);
        }
        let command = ticket.command();
        let (generation, mode_stamp) = (ticket.generation, ticket.mode_stamp);
        self.submit_fenced(
            generation,
            Class::Choice,
            &Outgoing::Personal { command: &command },
            None,
            Some(mode_stamp),
            Some(ScopedWrite::Quick(QuickWrite {
                personal,
                ticket,
                epoch,
                wire_stamp: *gate,
                session: self.session.clone(),
            })),
        )
    }

    /// The server responds with a fresh player snapshot and personal initial
    /// projection. Scoped playlist repairs are personal `syncPlaylist` writes.
    pub(crate) fn sync(&self, generation: Generation) -> Result<(), SocketError> {
        self.submit(generation, Class::Choice, &Outgoing::<()>::Sync, None)
    }

    /// Replaces a local sample, never queues a fact. `None` from an unexportable
    /// preview also clears the old sample so reconnect cannot import stale data.
    pub(crate) fn observe_local(
        &self,
        generation: Generation,
        cursor: Cursor,
    ) -> Result<(), SocketError> {
        if self.session.is_offline() || self.session.is_expired() {
            return Err(SocketError::NotConnected);
        }
        let authority = self.authority.borrow();
        if !authority.is_some_and(|a| a.generation == generation && a.permits(Instant::now())) {
            return Err(SocketError::NotConnected);
        }
        self.actual
            .send_replace(cursor.reattachable().then_some((generation, cursor)));
        Ok(())
    }

    fn submit(
        &self,
        generation: Generation,
        class: Class,
        frame: &(impl Serialize + ?Sized),
        resume: Option<u64>,
    ) -> Result<(), SocketError> {
        self.submit_fenced(generation, class, frame, resume, None, None)
    }

    fn submit_order(
        &self,
        generation: Generation,
        mode_stamp: u64,
        frame: &(impl Serialize + ?Sized),
    ) -> Result<(), SocketError> {
        // The engine owns the reducer; only the socket reader can publish a
        // new fact during this check. No waiting or I/O while holding this gate.
        let _gate = self.order_gate.lock().expect("socket order admission gate");
        if !self.events.is_empty() {
            return Err(SocketError::Busy);
        }
        self.submit_fenced(
            generation,
            Class::Choice,
            frame,
            None,
            Some(mode_stamp),
            None,
        )
    }

    fn submit_fenced(
        &self,
        generation: Generation,
        class: Class,
        frame: &(impl Serialize + ?Sized),
        resume: Option<u64>,
        expected_mode: Option<u64>,
        scoped: Option<ScopedWrite>,
    ) -> Result<(), SocketError> {
        let raw = encode(frame)?;
        // Hold the lock through bounded admission. The socket's writer also
        // retires work on loss of link or any mode-stamp change.
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
            class,
            resume,
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
    pub(crate) authority: watch::Sender<Option<OutputAuthority>>,
    _link: watch::Sender<Link>,
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
}
#[cfg(test)]
impl Socket {
    pub(crate) fn test_peer(session: Session, generation: Generation) -> (Self, TestPeer) {
        let (_link, link) = watch::channel(Link::Connected(generation));
        let (authority, grant) = watch::channel(None);
        let (event_out, events) = mpsc::channel(EVENT_CAPACITY);
        let (outbound, input) = mpsc::channel(OUTBOUND_CAPACITY);
        let (actual, _) = watch::channel(None);
        let socket = Self {
            link,
            authority: grant,
            direct_start: Arc::default(),
            events,
            outbound,
            resume_sequence: AtomicU64::new(0),
            order_gate: Arc::default(),
            actual,
            session,
            task: tokio::spawn(pending()),
        };
        (
            socket,
            TestPeer {
                _link,
                authority,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Choice,
    Fact,
    Release,
}

struct WriteFrame {
    generation: Generation,
    class: Class,
    resume: Option<u64>,
    at: Instant,
    raw: String,
    // The common playback/write queue stays small; only the two paths with
    // final personal-state admission allocate local metadata.
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
    wire_stamp: u64,
    session: Session,
}

struct Bounded(Vec<u8>);

impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_OUTBOUND - self.0.len() {
            return Err(io::Error::other("account socket frame too large"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode(frame: &(impl Serialize + ?Sized)) -> Result<String, SocketError> {
    let mut bytes = Bounded(Vec::new());
    serde_json::to_writer(&mut bytes, frame).map_err(|_| SocketError::Invalid)?;
    String::from_utf8(bytes.0).map_err(|_| SocketError::Invalid)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Transport,
    Protocol,
    Backpressure,
    Closed,
    Expired,
    NetworkChanged,
}

struct Signals {
    order_gate: Arc<Mutex<u64>>,
    link: watch::Sender<Link>,
    authority: watch::Sender<Option<OutputAuthority>>,
    direct_start: Arc<DirectStartFence>,
    events: mpsc::Sender<Event>,
}

impl Signals {
    fn event(&self, generation: Generation, kind: EventKind, first: bool) -> Result<(), End> {
        let mut gate = self.order_gate.lock().expect("socket order admission gate");
        let permit = self.events.try_reserve().map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => End::Backpressure,
            mpsc::error::TrySendError::Closed(_) => End::Closed,
        })?;
        if first {
            self.link.send_replace(Link::Connected(generation));
        }
        if matches!(kind, EventKind::Personal(_)) {
            *gate = gate.wrapping_add(1);
        }
        permit.send(Event { generation, kind });
        Ok(())
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        self.authority.send_replace(None);
        if *self.link.borrow() != Link::Expired {
            self.link.send_replace(Link::Closed);
        }
    }
}

struct Lease {
    generation: Generation,
    connection: String,
    deadline: Instant,
    handle: String,
    // Some means detached, with the last canonical cursor as potential base.
    detached: Option<Cursor>,
}

type ActualSample = Option<(Generation, Cursor)>;
type Attach<'a> = (&'a str, Option<(Cursor, Cursor)>);

struct OutputState {
    lease: Option<Lease>,
    actual: watch::Sender<ActualSample>,
}

impl OutputState {
    fn clear(&mut self, signals: &Signals) {
        if self.lease.take().is_some() {
            signals.authority.send_replace(None);
            // Clear after flipping the watch, so an observation holding the
            // old authority read lock cannot restore a retired lease's sample.
            self.actual.send_replace(None);
        }
    }

    fn expire(&mut self, signals: &Signals) {
        if self
            .lease
            .as_ref()
            .is_some_and(|l| Instant::now() >= l.deadline)
        {
            signals.direct_start.set_expired(true);
            self.clear(signals);
        }
    }

    fn detach(&mut self, generation: Generation, cursor: Cursor, signals: &Signals) {
        self.expire(signals);
        if let Some(lease) = self
            .lease
            .as_mut()
            .filter(|l| l.generation == generation && l.detached.is_none())
        {
            lease.detached = Some(cursor);
            signals.authority.send_replace(Some(OutputAuthority {
                generation,
                deadline: lease.deadline,
                attached: false,
                resume_through: 0,
            }));
        }
    }

    // Read the latest sample once per attach frame; never use a sample from a
    // prior authority or include a provisional/unrelated cursor.
    fn attach_frame(&self, owner: &str) -> Option<Attach<'_>> {
        let lease = self.lease.as_ref()?;
        let base = lease.detached.as_ref()?;
        if owner != lease.connection || Instant::now() >= lease.deadline {
            return None;
        }
        let pair = self
            .actual
            .borrow()
            .as_ref()
            .and_then(|(generation, actual)| {
                (*generation == lease.generation
                    && base.reattachable()
                    && actual.reattachable()
                    && base.chain == actual.chain
                    && actual.visit >= base.visit
                    && actual.visit - base.visit <= 256
                    && (actual.visit > base.visit || actual.run >= base.run)
                    && (!actual.same_run(base) || actual.desired == base.desired))
                    .then(|| (base.clone(), actual.clone()))
            });
        Some((&lease.handle, pair))
    }

    fn owner_changed(&mut self, owner: &str, connection: &str, signals: &Signals) {
        if self
            .lease
            .as_ref()
            .is_some_and(|l| owner != connection && (l.detached.is_none() || owner != l.connection))
        {
            self.clear(signals);
        }
    }

    fn deadline(&self) -> Option<Instant> {
        self.lease.as_ref().map(|l| l.deadline)
    }
}

#[derive(Default)]
struct Pulses {
    sequence: u64,
    last_granted: u64,
    written_resume: u64,
    seen_resume: u64,
    outstanding: VecDeque<(u64, Instant, u64)>,
}

impl Pulses {
    fn next(&mut self, now: Instant) -> Result<u64, End> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .filter(|n| *n <= MAX_SAFE_INTEGER)
            .ok_or(End::Protocol)?;
        while self
            .outstanding
            .front()
            .is_some_and(|(_, sent, _)| now.duration_since(*sent) >= MAX_LEASE)
            || self.outstanding.len() >= 8
        {
            self.outstanding.pop_front();
        }
        self.outstanding
            .push_back((self.sequence, now, self.written_resume));
        Ok(self.sequence)
    }

    fn accept(&mut self, basis: u64, ms: u64, handle: &str, now: Instant) -> Option<Instant> {
        if basis <= self.last_granted
            || !(1..=MAX_LEASE.as_millis() as u64).contains(&ms)
            || !wire::text(handle, 128)
        {
            return None;
        }
        let i = self.outstanding.iter().position(|(n, _, _)| *n == basis)?;
        let (_, sent, resume) = self.outstanding.remove(i)?;
        let deadline = sent.checked_add(Duration::from_millis(ms))?;
        if deadline <= now {
            return None;
        }
        self.last_granted = basis;
        self.seen_resume = resume;
        Some(deadline)
    }
}

type AccountWs = WebSocketStream<reqwest::Upgraded>;

async fn admit(
    api: &Api,
    session: &Session,
    device: &DeviceIdentity,
) -> Result<(AccountWs, Incoming), End> {
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
        .max_write_buffer_size(MAX_OUTBOUND + 1024);
    let mut ws = WebSocketStream::from_raw_socket(upgraded, Role::Client, Some(config)).await;
    let hello = Outgoing::<()>::Hello {
        ticket: &ticket,
        name: &device.name,
        device_id: &device.device_id,
        can_play: device.can_play,
    };
    send(&mut ws, encode(&hello).map_err(|_| End::Protocol)?).await?;
    let first = time::timeout(SNAPSHOT_TIMEOUT, receive(&mut ws, &device.device_id))
        .await
        .map_err(|_| End::Transport)??;
    if !matches!(first, Incoming::Snapshot { .. }) {
        return Err(End::Protocol);
    }
    Ok((ws, first))
}

async fn send<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    frame: String,
) -> Result<(), End> {
    time::timeout(WRITE_TIMEOUT, ws.send(Message::Text(frame.into())))
        .await
        .map_err(|_| End::Transport)?
        .map_err(|_| End::Transport)
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
) -> Result<(), End> {
    // In particular, an obsolete read must not trip the generic queue-age
    // connection failure before we notice that its exact local scope retired.
    let allowed =
        read.personal
            .inspect_playlist_sync(read.epoch, generation, &read.command, |quick| {
                if !read.account_current() {
                    Ok(false)
                } else if queued.elapsed() <= MAX_QUEUE_AGE {
                    Ok(true)
                } else if quick {
                    Ok(false) // Leave a current quick read to its own bounded timeout.
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
        read.personal
            .start_playlist_sync(read.epoch, generation, &read.command, || {
                if !read.account_current() {
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

async fn send_quick<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    raw: String,
    quick: QuickWrite,
    signals: &Signals,
) -> Result<(), End> {
    wait_write_ready(ws).await?;
    // Reader and writer share this actor. If a newer personal frame was
    // published after buffering the command, don't assume the engine has
    // reduced it yet; refuse this exact ticket rather than enqueue it again.
    if *signals
        .order_gate
        .lock()
        .expect("socket order admission gate")
        != quick.wire_stamp
    {
        quick.personal.quick_cancel(&quick.ticket);
        return Ok(());
    }
    let mut result = Ok(());
    let started = {
        // Account epoch changes, explicit mode changes and credential
        // revocation share this gate. Match the existing publication →
        // mode → lifetime order. No PersonalHub path acquires publication
        // while holding its state; queue admission never takes this gate.
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
            // Linearize local dispatch with selection and publication. No
            // guard crosses the asynchronous network flush below.
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
) -> Result<Incoming, End> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(raw))) => return decode(&raw, device),
            Some(Ok(Message::Ping(_))) => {
                // Tungstenite schedules the matching Pong itself. Flush now,
                // including when waiting on the initial snapshot.
                time::timeout(WRITE_TIMEOUT, ws.flush())
                    .await
                    .map_err(|_| End::Transport)?
                    .map_err(|_| End::Transport)?;
            }
            Some(Ok(Message::Pong(_))) => (),
            Some(Ok(Message::Close(_))) | None => return Err(End::Transport),
            Some(Ok(_)) => return Err(End::Protocol),
            Some(Err(err)) => return Err(classify_wire_error(err)),
        }
    }
}

fn decode(raw: &str, device: &str) -> Result<Incoming, End> {
    if raw.len() > MAX_INBOUND {
        return Err(End::Protocol);
    }
    let incoming: Incoming = serde_json::from_str(raw).map_err(|_| End::Protocol)?;
    incoming
        .valid(device)
        .then_some(incoming)
        .ok_or(End::Protocol)
}

struct Connection {
    generation: Generation,
    id: String,
    owner: String,
    canonical: Cursor,
    pulses: Pulses,
    releasing: bool,
}

impl Connection {
    fn begin(initial: Incoming, output: &mut OutputState, signals: &Signals) -> Result<Self, End> {
        let Incoming::Snapshot {
            connection_id,
            player,
            outputs,
        } = initial
        else {
            return Err(End::Protocol);
        };
        let mut previous = GENERATIONS.load(Ordering::Relaxed);
        let generation = loop {
            let next = previous.checked_add(1).ok_or(End::Closed)?;
            match GENERATIONS.compare_exchange_weak(
                previous,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break next,
                Err(current) => previous = current,
            }
        };
        let result = Self {
            generation,
            id: connection_id.clone(),
            owner: outputs.owner.clone(),
            canonical: player.cursor.clone(),
            pulses: Pulses::default(),
            releasing: false,
        };
        output.owner_changed(&result.owner, &result.id, signals);
        signals.direct_start.invalidate();
        signals.direct_start.set_expired(false);
        signals.event(
            generation,
            EventKind::Snapshot {
                connection_id,
                player,
                outputs,
            },
            true,
        )?;
        Ok(result)
    }

    fn receive(
        &mut self,
        msg: Incoming,
        output: &mut OutputState,
        signals: &Signals,
    ) -> Result<bool, End> {
        let mut pulse_now = false;
        let event = match msg {
            Incoming::Snapshot {
                connection_id,
                player,
                outputs,
            } => {
                if connection_id != self.id {
                    return Err(End::Protocol);
                }
                self.canonical = player.cursor.clone();
                signals.direct_start.invalidate();
                pulse_now = self.outputs(&outputs, output, signals);
                EventKind::Snapshot {
                    connection_id,
                    player,
                    outputs,
                }
            }
            Incoming::Player { player } => {
                if let Some(cursor) = &player.cursor {
                    self.canonical = cursor.clone();
                }
                EventKind::Player(player)
            }
            Incoming::Outputs { outputs } => {
                pulse_now = self.outputs(&outputs, output, signals);
                EventKind::Outputs(outputs)
            }
            Incoming::Grant { basis, ms, handle } => {
                if self.owner == self.id && !self.releasing {
                    let now = Instant::now();
                    if let Some(deadline) = self.pulses.accept(basis, ms, &handle, now) {
                        output.lease = Some(Lease {
                            generation: self.generation,
                            connection: self.id.clone(),
                            deadline,
                            handle,
                            detached: None,
                        });
                        signals.direct_start.set_expired(false);
                        signals.authority.send_replace(Some(OutputAuthority {
                            generation: self.generation,
                            deadline,
                            attached: true,
                            resume_through: self.pulses.seen_resume,
                        }));
                    }
                }
                return Ok(false);
            }
            Incoming::Personal { update } => EventKind::Personal(update),
        };
        signals.event(self.generation, event, false)?;
        Ok(pulse_now)
    }

    fn outputs(&mut self, outputs: &Outputs, output: &mut OutputState, signals: &Signals) -> bool {
        let gained = self.owner != self.id && outputs.owner == self.id;
        if (self.owner != outputs.owner && !(self.owner.is_empty() && gained))
            || (outputs.owner.is_empty()
                && outputs
                    .devices
                    .iter()
                    .any(|d| d.can_play && d.id != self.id))
        {
            signals.direct_start.invalidate();
        }
        if self.owner != outputs.owner {
            signals.direct_start.set_expired(false);
        }
        // For a plain owner change, outstanding bases issued for a prior
        // ownership epoch cannot mint authority. Keep the current attach
        // pulse when this follows a successful server-side reattach.
        if self.owner == self.id && outputs.owner != self.id {
            self.pulses.outstanding.clear();
            self.releasing = false;
        }
        self.owner.clone_from(&outputs.owner);
        output.owner_changed(&self.owner, &self.id, signals);
        gained && output.lease.as_ref().is_none_or(|l| l.detached.is_none())
    }

    fn release(&mut self, output: &mut OutputState, signals: &Signals) {
        self.releasing = self.owner == self.id;
        self.pulses.outstanding.clear();
        output.clear(signals);
    }

    async fn pulse<S: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        ws: &mut WebSocketStream<S>,
        output: &OutputState,
    ) -> Result<(), End> {
        if self.releasing {
            return Ok(());
        }
        let basis = self.pulses.next(Instant::now())?;
        let attach = output.attach_frame(&self.owner);
        let (handle, pair) = attach.as_ref().map_or((None, None), |(handle, pair)| {
            (Some(*handle), pair.as_ref())
        });
        let presence = Outgoing::<()>::Presence {
            basis,
            handle,
            base: pair.map(|p| &p.0),
            actual: pair.map(|p| &p.1),
        };
        send(ws, encode(&presence).map_err(|_| End::Protocol)?).await
    }

    async fn drive<S: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        ws: &mut WebSocketStream<S>,
        device: &str,
        input: &mut mpsc::Receiver<WriteFrame>,
        output: &mut OutputState,
        signals: &Signals,
    ) -> Result<(), End> {
        // Issue the first pulse before servicing admitted UI writes, including
        // the reattach candidate for a detached owner's replacement socket.
        self.pulse(ws, output).await?;
        let mut pulse = Instant::now() + PRESENCE_INTERVAL;
        let mut idle = Instant::now() + SOCKET_IDLE;
        loop {
            output.expire(signals);
            // Keep each websocket read cancel-safe: Pong flushing happens
            // after the select, not inside a future that another branch drops.
            tokio::select! {
                biased;
                _ = signals.events.closed() => return Err(End::Closed),
                _ = until(output.deadline()) => output.expire(signals),
                _ = until(Some(idle)) => return Err(End::Transport),
                _ = until(Some(pulse)) => {
                    self.pulse(ws, output).await?;
                    pulse = Instant::now() + PRESENCE_INTERVAL;
                }
                // Unbiased across application reads/writes so a stream of
                // observations cannot starve a control and vice versa.
                selected = async {
                    tokio::select! {
                        m = ws.next() => Drive::Read(m),
                        msg = input.recv() => Drive::Write(msg),
                    }
                } => match selected {
                    Drive::Write(Some(frame)) => {
                        if frame.generation != self.generation { continue; }
                        let now = Instant::now();
                        if let Some(scoped) = frame.scoped {
                            match *scoped {
                                ScopedWrite::Quick(quick) => {
                                    if now.duration_since(frame.at) > MAX_QUEUE_AGE {
                                        quick.personal.quick_cancel(&quick.ticket);
                                    } else {
                                        send_quick(ws, frame.raw, quick, signals).await?;
                                    }
                                }
                                ScopedWrite::Playlist(read) => {
                                    send_playlist_read(ws, frame.raw, read, self.generation, frame.at).await?;
                                }
                            }
                            continue;
                        }
                        if frame.class == Class::Fact && !output.lease.as_ref().is_some_and(|l| l.generation == self.generation && l.detached.is_none() && now < l.deadline) { continue; }
                        if now.duration_since(frame.at) > MAX_QUEUE_AGE { return Err(End::Backpressure); }
                        if frame.class == Class::Release { self.release(output, signals); }
                        send(ws, frame.raw).await?;
                        if let Some(resume) = frame.resume {
                            self.pulses.written_resume = resume;
                        }
                    }
                    Drive::Write(None) => return Err(End::Closed),
                    Drive::Read(message) => {
                        idle = Instant::now() + SOCKET_IDLE;
                        match message {
                            Some(Ok(Message::Text(raw))) => {
                                if self.receive(decode(&raw, device)?, output, signals)? { pulse = Instant::now(); }
                            }
                            Some(Ok(Message::Ping(_))) => time::timeout(WRITE_TIMEOUT, ws.flush()).await.map_err(|_| End::Transport)?.map_err(|_| End::Transport)?,
                            Some(Ok(Message::Pong(_))) => (),
                            Some(Ok(Message::Close(_))) | None => return Err(End::Transport),
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
}

fn classify_wire_error(err: tokio_tungstenite::tungstenite::Error) -> End {
    use tokio_tungstenite::tungstenite::{Error, error::ProtocolError};
    match err {
        // Go's socket handler defers CloseNow, so even routine loss commonly
        // arrives as EOF without a close frame. This is eligible for reattach.
        Error::Protocol(ProtocolError::ResetWithoutClosingHandshake) => End::Transport,
        Error::Capacity(_) | Error::Utf8(_) | Error::Protocol(_) => End::Protocol,
        _ => End::Transport,
    }
}

async fn until(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        time::sleep_until(deadline.into()).await;
    } else {
        pending::<()>().await;
    }
}

async fn local_expiry(session: &Session) {
    // Session revocation is an atomic shared with existing HTTP methods, not
    // a watch. No additional HTTP polling, credential refresh, or fallback.
    while !session.is_expired() {
        time::sleep(Duration::from_millis(100)).await;
    }
}

async fn expiry_aware<T>(
    future: impl Future<Output = T>,
    output: &mut OutputState,
    signals: &Signals,
) -> T {
    tokio::pin!(future);
    loop {
        tokio::select! {
            biased;
            _ = until(output.deadline()) => output.expire(signals),
            result = &mut future => return result,
        }
    }
}

#[cfg(test)]
#[path = "account_socket_tests.rs"]
mod tests;

async fn run(
    api: Arc<Api>,
    session: Session,
    device: DeviceIdentity,
    mut input: mpsc::Receiver<WriteFrame>,
    signals: Signals,
    actual: watch::Sender<ActualSample>,
) {
    let mut mode = session.network_changes();
    let mut output = OutputState {
        lease: None,
        actual,
    };
    let mut delay = RETRY_MIN;
    let expiry = local_expiry(&session);
    tokio::pin!(expiry);
    loop {
        let offline = *mode.borrow_and_update() & 1 != 0;
        if offline {
            output.clear(&signals);
            signals.link.send_replace(Link::Disconnected);
            tokio::select! {
                biased;
                _ = &mut expiry => { signals.link.send_replace(Link::Expired); return; }
                _ = signals.events.closed() => return,
                _ = mode.changed() => (),
            }
            continue;
        }
        if session.is_expired() {
            signals.link.send_replace(Link::Expired);
            return;
        }
        let started = Instant::now();
        let admitted = tokio::select! {
            biased;
            _ = &mut expiry => Err(End::Expired),
            _ = mode.changed() => Err(End::NetworkChanged),
            _ = signals.events.closed() => Err(End::Closed),
            result = expiry_aware(admit(&api, &session, &device), &mut output, &signals) => result,
        };
        let ended = match admitted {
            Ok((mut ws, initial)) => match Connection::begin(initial, &mut output, &signals) {
                Ok(mut connection) => {
                    let ended = tokio::select! {
                        biased;
                        _ = &mut expiry => End::Expired,
                        _ = mode.changed() => End::NetworkChanged,
                        result = connection.drive(&mut ws, &device.device_id, &mut input, &mut output, &signals) => result.err().unwrap_or(End::Closed),
                    };
                    if ended == End::Transport {
                        output.detach(connection.generation, connection.canonical, &signals);
                    }
                    ended
                }
                Err(ended) => ended,
            },
            Err(ended) => ended,
        };
        // Never preserve native authority on a known scope/protocol/consumer
        // failure. Retain only an already-granted lease on transport loss.
        if ended != End::Transport {
            output.clear(&signals);
        }
        signals.link.send_replace(match ended {
            End::Expired => Link::Expired,
            End::Closed => Link::Closed,
            _ => Link::Disconnected,
        });
        while input.try_recv().is_ok() {}
        if matches!(ended, End::Expired | End::Closed) {
            return;
        }
        if ended == End::NetworkChanged {
            delay = RETRY_MIN;
            continue;
        }
        if started.elapsed() >= SOCKET_IDLE {
            delay = RETRY_MIN;
        }
        tokio::select! {
            biased;
            _ = &mut expiry => { signals.link.send_replace(Link::Expired); return; }
            _ = signals.events.closed() => return,
            _ = mode.changed() => { output.clear(&signals); delay = RETRY_MIN; }
            _ = expiry_aware(time::sleep(delay), &mut output, &signals) => delay = (delay * 2).min(RETRY_MAX),
        }
    }
}
