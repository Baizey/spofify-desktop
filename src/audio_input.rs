//! Seekable encoded audio capabilities. Only loaders/decoder workers touch files.
//! Online readers share one anonymous file and only see its written prefix.

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    os::unix::fs::FileExt,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};

use rustix::fs::{self, AtFlags, Mode, OFlags};
use tokio::{sync::Semaphore, task::AbortHandle, time::timeout};

use crate::api::{ApiError, AudioBody};

#[cfg(test)]
#[path = "audio_input_tests.rs"]
mod tests;

const SPOOL_DIR: &str = "spofify-playback";
const DISK_STALL: Duration = Duration::from_secs(10);
const FETCHING: u8 = 0;
const COMPLETE: u8 = 1;
const FAILED: u8 = 2;
// Include retired writes that may still be stuck in the kernel.
static SPOOL_SLOTS: Semaphore = Semaphore::const_new(4);

const STORAGE_ERROR: ApiError = ApiError {
    message: "Unable to use local storage to load audio.",
    expired: false,
    status: 0,
};

#[derive(Clone, Debug)]
pub struct AudioInput {
    backing: Backing,
    // Only actor-held source capabilities keep an online transfer alive.
    // A retired worker stuck in read_at must not keep sending credentials/data.
    _transfer: Option<Arc<Transfer>>,
    len: u64,
}

#[derive(Clone, Debug)]
enum Backing {
    File {
        file: Arc<File>,
        offset: u64,
        saved: Option<Arc<crate::offline::SavedProof>>,
    },
    Online(Arc<Progress>),
    #[cfg(test)]
    Memory(Arc<[u8]>),
}

#[derive(Debug)]
struct Transfer(AbortHandle);

impl Drop for Transfer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Debug)]
struct Progress {
    file: File,
    written: AtomicU64,
    state: AtomicU8,
}

struct FailUnlessComplete(Arc<Progress>);
impl Drop for FailUnlessComplete {
    fn drop(&mut self) {
        let _ =
            self.0
                .state
                .compare_exchange(FETCHING, FAILED, Ordering::AcqRel, Ordering::Relaxed);
    }
}

impl AudioInput {
    pub fn file(file: File, offset: u64, len: u64) -> Self {
        Self::file_input(file, offset, len, None)
    }

    /// Only for the exact Saved descriptor after its full payload is verified.
    pub(crate) fn verified_saved(file: File, offset: u64, len: u64, proof: crate::offline::SavedProof) -> Self {
        Self::file_input(file, offset, len, Some(Arc::new(proof)))
    }

    fn file_input(file: File, offset: u64, len: u64, saved: Option<Arc<crate::offline::SavedProof>>) -> Self {
        Self {
            backing: Backing::File {
                file: Arc::new(file),
                offset,
                saved,
            },
            _transfer: None,
            len,
        }
    }

    pub(crate) async fn online(body: AudioBody) -> Result<Self, ApiError> {
        Self::spool(body, private_spool).await
    }

    async fn spool(
        body: AudioBody,
        open: impl FnOnce() -> io::Result<File> + Send + 'static,
    ) -> Result<Self, ApiError> {
        let slot = timeout(DISK_STALL, SPOOL_SLOTS.acquire())
            .await
            .map_err(|_| STORAGE_ERROR)?
            .map_err(|_| STORAGE_ERROR)?;
        let (file, slot) = timeout(
            DISK_STALL,
            tokio::task::spawn_blocking(move || (open(), slot)),
        )
        .await
        .map_err(|_| STORAGE_ERROR)?
        .map_err(|_| STORAGE_ERROR)?;
        let progress = Arc::new(Progress {
            file: file.map_err(|_| STORAGE_ERROR)?,
            written: AtomicU64::new(0),
            state: AtomicU8::new(FETCHING),
        });
        let len = body.len();
        let guard = FailUnlessComplete(progress.clone());
        let transfer = tokio::spawn(async move {
            let mut body = body;
            let mut slot = slot;
            let mut offset = 0;
            let progress = &guard.0;
            loop {
                let chunk = match body.chunk().await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break,
                    Err(_) => return,
                };
                let end = offset + chunk.len() as u64; // AudioBody checked the representation length.
                let file = progress.clone();
                // One chunk in flight, and its worker keeps admission even if
                // cancellation or a disk stall detaches the blocking call.
                let write = tokio::task::spawn_blocking(move || {
                    (file.file.write_all_at(&chunk, offset), slot)
                });
                match timeout(DISK_STALL, write).await {
                    Ok(Ok((Ok(()), returned))) => slot = returned,
                    _ => return,
                }
                offset = end;
                progress.written.store(offset, Ordering::Release);
            }
            // Require successful HTTP completion and check delayed allocation
            // errors (not just synchronous write errors) before natural EOF.
            let file = progress.clone();
            let sync = tokio::task::spawn_blocking(move || {
                let _slot = slot;
                file.file.sync_data()
            });
            if matches!(timeout(DISK_STALL, sync).await, Ok(Ok(Ok(())))) && body.live().is_ok() {
                progress.state.store(COMPLETE, Ordering::Release);
            }
        });
        Ok(Self {
            backing: Backing::Online(progress),
            _transfer: Some(Arc::new(Transfer(transfer.abort_handle()))),
            len,
        })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    /// Safe to share with another run of this exact authenticated audio GET.
    /// Saved records must be reopened and verified at every new admission.
    pub(crate) fn reusable_online(&self) -> bool {
        match &self.backing {
            Backing::Online(_) => !self.is_failed(),
            Backing::File { .. } => false,
            #[cfg(test)]
            Backing::Memory(_) => true,
        }
    }

    pub(crate) fn cacheable(&self) -> bool {
        match &self.backing {
            Backing::Online(progress) => progress.state.load(Ordering::Acquire) == COMPLETE,
            Backing::File { .. } => false,
            #[cfg(test)]
            Backing::Memory(_) => true,
        }
    }

    /// Pin bytes already usable without HTTP. Does not wait for a partial
    /// transfer, retain the transfer task, or authorize playback. Keep only
    /// the current/next inputs, fenced by account and queue identity.
    pub(crate) fn isolated(&self) -> Option<Self> {
        (self.cacheable()
            || matches!(
                &self.backing,
                Backing::File {
                    saved: Some(_),
                    ..
                }
            ))
        .then(|| self.decoder_input())
    }

    #[cfg(test)]
    pub(crate) fn failed_stream(file: File, len: u64, written: u64) -> Self {
        Self {
            backing: Backing::Online(Arc::new(Progress {
                file, written: AtomicU64::new(written), state: AtomicU8::new(FAILED),
            })),
            _transfer: None, len,
        }
    }

    pub(crate) fn may_start(&self) -> bool {
        match &self.backing {
            Backing::File { saved: Some(proof), .. } => proof.current(),
            _ => true,
        }
    }

    pub(crate) fn is_saved(&self) -> bool {
        matches!(&self.backing, Backing::File { saved: Some(_), .. })
    }

    pub(crate) fn is_stream(&self) -> bool {
        matches!(&self.backing, Backing::Online(_))
    }

    pub(crate) fn stop_transfer(&self) {
        if let Some(transfer) = &self._transfer {
            transfer.0.abort();
        }
    }

    pub(crate) fn is_failed(&self) -> bool {
        matches!(&self.backing, Backing::Online(progress) if progress.state.load(Ordering::Acquire) == FAILED)
    }

    pub(crate) fn await_complete(&self, live: impl Fn() -> bool) -> io::Result<()> {
        if let Backing::Online(progress) = &self.backing {
            wait_for(progress, &live, |state| (state == COMPLETE).then_some(()))?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn reader(&self) -> AudioReader {
        self.reader_with_cancel(|| true)
    }

    pub(crate) fn reader_with_cancel(
        &self,
        live: impl Fn() -> bool + Send + Sync + 'static,
    ) -> AudioReader {
        AudioReader {
            input: self.decoder_input(),
            pos: 0,
            live: Box::new(live),
        }
    }

    pub(crate) fn decoder_input(&self) -> Self {
        Self {
            backing: self.backing.clone(),
            _transfer: None,
            len: self.len,
        }
    }
}

fn private_spool() -> io::Result<File> {
    let dir = crate::storage::open_default(SPOOL_DIR)?;
    let mode = Mode::RUSR | Mode::WUSR;
    let flags = OFlags::RDWR | OFlags::CLOEXEC;
    match fs::openat(&dir, ".", flags | OFlags::TMPFILE | OFlags::EXCL, mode) {
        Ok(file) => Ok(File::from(file)),
        Err(rustix::io::Errno::OPNOTSUPP | rustix::io::Errno::INVAL) => {
            // Some local filesystems don't support O_TMPFILE. Unlink before
            // accepting any data, so even a crash can leave at most an empty file.
            let name = uuid::Uuid::new_v4().to_string();
            let file = fs::openat(
                &dir,
                name.as_str(),
                flags | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
                mode,
            )?;
            fs::unlinkat(&dir, name.as_str(), AtFlags::empty())?;
            Ok(File::from(file))
        }
        Err(e) => Err(e.into()),
    }
}

pub struct AudioReader {
    input: AudioInput,
    pos: u64,
    live: Box<dyn Fn() -> bool + Send + Sync>,
}

fn check_live(live: &dyn Fn() -> bool) -> io::Result<()> {
    if !live() {
        // Interrupted is retried by Read::read_exact, even on cancellation.
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "audio read cancelled",
        ));
    }
    Ok(())
}

fn wait_for<T>(
    progress: &Progress,
    live: &dyn Fn() -> bool,
    value: impl Fn(u8) -> Option<T>,
) -> io::Result<T> {
    loop {
        check_live(live)?;
        let state = progress.state.load(Ordering::Acquire);
        // A failed transfer can still satisfy reads into its written prefix.
        // The incomplete tail must remain an error, never natural EOF.
        if let Some(value) = value(state) {
            return Ok(value);
        }
        if state == FAILED {
            return Err(io::Error::other("audio transfer did not complete"));
        }
        // This wait is worker-only. No lock can be held by a disk writer or
        // awaited by the mixer. Poll at a fixed interval when missing bytes.
        std::thread::sleep(Duration::from_millis(5));
    }
}

impl Read for AudioReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        check_live(&*self.live)?;
        let count = self
            .input
            .len
            .saturating_sub(self.pos)
            .min(buf.len() as u64) as usize;
        if count == 0 {
            return Ok(0);
        }
        let n = match &self.input.backing {
            Backing::File { file, offset, .. } => {
                let absolute = offset
                    .checked_add(self.pos)
                    .ok_or(io::ErrorKind::InvalidInput)?;
                file.read_at(&mut buf[..count], absolute)?
            }
            Backing::Online(progress) => {
                let available = wait_for(progress, &*self.live, |_| {
                    let written = progress.written.load(Ordering::Acquire);
                    (written > self.pos).then_some(written.saturating_sub(self.pos))
                })?;
                let count = available.min(count as u64) as usize;
                progress.file.read_at(&mut buf[..count], self.pos)?
            }
            #[cfg(test)]
            Backing::Memory(bytes) => {
                let pos = self.pos as usize;
                buf[..count].copy_from_slice(&bytes[pos..pos + count]);
                count
            }
        };
        check_live(&*self.live)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for AudioReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        check_live(&*self.live)?;
        self.pos = match from {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(n) => self.pos.checked_add_signed(n),
            SeekFrom::End(n) => self.input.len.checked_add_signed(n),
        }
        .ok_or(io::ErrorKind::InvalidInput)?;
        Ok(self.pos)
    }
}

#[cfg(test)]
impl From<Arc<[u8]>> for AudioInput {
    fn from(bytes: Arc<[u8]>) -> Self {
        Self {
            len: bytes.len() as u64,
            backing: Backing::Memory(bytes),
            _transfer: None,
        }
    }
}
