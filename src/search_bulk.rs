//! Read a canonical collection to completion before appending: each playlist
//! write changes the personal generation in Go's collection cursor. The
//! anonymous source spool is independent of playlist membership and receipts.
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::time;

use crate::{
    collections::spool_search_source,
    desk::Access,
    personal::PersonalCommand,
    personal_bridge::BulkDevice,
    quick_add::{BULK_WINDOW, BulkEnd, BulkTicket, BulkWrite},
    socket::{BulkLink, Link},
};

const SCOPED_WAIT: Duration = Duration::from_secs(20);

pub(crate) fn start(
    access: &Access,
    current_epoch: Arc<AtomicU64>,
    mode_stamp: u64,
    scope: &str,
    target: &str,
    kind: &str,
    source: &str,
) {
    if current_epoch.load(Ordering::SeqCst) != access.epoch
        || access.offline
        || access.session.is_expired()
        || access.session.is_offline()
    {
        return;
    }
    let Some(primary) = access.personal.quick_bulk_device() else {
        return;
    };
    let Some(ticket) =
        access
            .personal
            .quick_begin_bulk(access.epoch, mode_stamp, scope, target, kind, source)
    else {
        return;
    };
    let task = Operation {
        access: access.clone(),
        current_epoch,
        ticket,
        primary,
    };
    let (kind, source) = (kind.to_owned(), source.to_owned());
    tokio::spawn(async move {
        let result = tokio::select! {
            biased;
            _ = task.cancelled() => BulkEnd::Cancelled,
            result = task.execute(&kind, &source) => result.err().unwrap_or(BulkEnd::Done),
        };
        task.access.personal.quick_finish_bulk(&task.ticket, result);
    });
}

struct Operation {
    access: Access,
    current_epoch: Arc<AtomicU64>,
    ticket: BulkTicket,
    primary: BulkDevice,
}

impl Operation {
    fn live(&self) -> bool {
        let mode = *self.access.session.network_changes().borrow();
        self.current_epoch.load(Ordering::SeqCst) == self.ticket.epoch
            && self.primary.link.has_changed().is_ok()
            && *self.primary.link.borrow() == Link::Connected(self.ticket.generation)
            && mode == self.ticket.mode_stamp
            && !self.access.session.is_expired()
            && self.access.personal.quick_bulk_current(&self.ticket)
    }

    async fn cancelled(&self) {
        let mut changes = self.access.personal.changes();
        let mut mode = self.access.session.network_changes();
        let mut primary = self.primary.link.clone();
        while self.live() {
            tokio::select! {
                r = changes.changed() => if r.is_err() { break },
                r = mode.changed() => if r.is_err() { break },
                r = primary.changed() => if r.is_err() { break },
                // Account epoch and credential expiry are atomic, not watches.
                _ = time::sleep(Duration::from_millis(100)) => (),
            }
        }
    }

    async fn revision_at_least(&self, required: u64) -> Result<(), BulkEnd> {
        let mut changes = self.access.personal.changes();
        let mut ingress = self.primary.order.changes();
        time::timeout(SCOPED_WAIT, async {
            loop {
                if !self.live() {
                    return Err(BulkEnd::Cancelled);
                }
                if self
                    .access
                    .personal
                    .quick_bulk_revision(&self.ticket)
                    .is_some_and(|revision| revision >= required)
                    && self.primary.order.when_applied(|| ()).is_some()
                {
                    return Ok(());
                }
                tokio::select! {
                    r = changes.changed() => r.map_err(|_| BulkEnd::Connection)?,
                    r = ingress.changed() => r.map_err(|_| BulkEnd::Connection)?,
                }
            }
        })
        .await
        .map_err(|_| BulkEnd::Connection)?
    }

    async fn execute(&self, kind: &str, source: &str) -> Result<(), BulkEnd> {
        let mut spool = spool_search_source(&self.access, kind, source, || self.live())
            .await
            .map_err(|_| BulkEnd::Source)?;
        if spool.total() == 0 {
            return Ok(());
        }
        let mut link = None;
        let mut started = 0;
        loop {
            let batch = spool
                .next_batch(BULK_WINDOW)
                .await
                .map_err(|_| BulkEnd::Connection)?;
            if batch.is_empty() {
                break;
            }
            for track in batch {
                if !self.live() {
                    return Err(BulkEnd::Cancelled);
                }
                if self.append(&mut link, &track).await? {
                    started += 1;
                }
                if started == BULK_WINDOW {
                    self.checkpoint(link.as_mut().expect("started window"))
                        .await?;
                    started = 0;
                }
            }
        }
        if let Some(link) = &mut link
            && started != 0
        {
            self.checkpoint(link).await?;
        }
        Ok(())
    }

    async fn append(&self, link: &mut Option<BulkLink>, track: &str) -> Result<bool, BulkEnd> {
        let personal = &self.access.personal;
        let ticket = &self.ticket;
        let command = PersonalCommand::AddEntry {
            playlist_id: ticket.target.clone(),
            track_id: track.into(),
        };
        if !command.valid() {
            return Err(BulkEnd::Connection);
        }
        loop {
            let outcome = if let Some(link) = link {
                let mut outcome = BulkWrite::Stop;
                link.add(&command, |start| {
                    outcome = personal.quick_bulk_send_ordered(
                        ticket,
                        track,
                        Some(&self.primary),
                        &self.current_epoch,
                        start,
                    );
                    outcome == BulkWrite::Started
                })
                .await
                .map_err(|_| BulkEnd::Connection)?;
                outcome
            } else {
                // Already present sources need no secondary connection at all.
                personal.quick_bulk_send_ordered(
                    ticket,
                    track,
                    Some(&self.primary),
                    &self.current_epoch,
                    || false,
                )
            };
            match outcome {
                BulkWrite::Wait => self.wait_for_primary(link.as_mut(), 0).await?,
                BulkWrite::Stop => return Err(BulkEnd::Connection),
                BulkWrite::Skipped => return Ok(false),
                BulkWrite::Started => return Ok(true),
                BulkWrite::Ready if link.is_none() => {
                    if !self.live() {
                        return Err(BulkEnd::Cancelled);
                    }
                    // Read the latest primary name, which can be changed externally
                    // while the source spool is being prepared. Using the same
                    // device ID keeps the auxiliary link out of the device rows.
                    let device = personal.quick_bulk_device().ok_or(BulkEnd::Connection)?;
                    *link = Some(
                        BulkLink::open(
                            &self.access.api,
                            &self.access.session,
                            ticket.mode_stamp,
                            &device.id,
                            &device.name,
                        )
                        .await
                        .map_err(|_| BulkEnd::Connection)?,
                    );
                }
                BulkWrite::Ready => return Err(BulkEnd::Connection),
            }
        }
    }

    async fn wait_for_primary(
        &self,
        link: Option<&mut BulkLink>,
        revision: u64,
    ) -> Result<(), BulkEnd> {
        if let Some(link) = link {
            tokio::select! {
                r = self.revision_at_least(revision) => r,
                _ = link.maintain() => Err(BulkEnd::Connection),
            }
        } else {
            self.revision_at_least(revision).await
        }
    }

    async fn checkpoint(&self, link: &mut BulkLink) -> Result<(), BulkEnd> {
        // The secondary connection's private account Initial crosses Go's
        // FIFO. It is not an edit receipt or a membership projection.
        let revision = link
            .checkpoint(&self.ticket.target)
            .await
            .map_err(|_| BulkEnd::Connection)?;
        if !self.access.personal.quick_bulk_ack(&self.ticket, revision) {
            return Err(BulkEnd::Connection);
        }
        self.wait_for_primary(Some(link), revision).await?;
        self.access.personal.quick_bulk_checkpoint(&self.ticket);
        Ok(())
    }
}
