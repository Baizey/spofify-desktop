//! Stable identity for Go's same-device output reattach, never a credential.
use std::{
    fs::File,
    io::{Read, Write},
};

use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use rustix::{
    fd::OwnedFd,
    fs::{self, FileType, FlockOperation, Mode, OFlags},
    io::{Errno, Result},
    process::geteuid,
};

use crate::storage;

pub(crate) struct Installation {
    seed: [u8; 16],
    // A second process must not rebind an owner's lease as the same device.
    // Keep this open until the engine has stopped native output and its socket.
    _lock: File,
}

impl Installation {
    pub(crate) fn open() -> Result<Self> {
        Self::locked(&storage::open_default("spofify-output-v1")?)
    }

    fn locked(dir: &OwnedFd) -> Result<Self> {
        let fd = fs::openat(
            dir,
            "identity",
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let stat = fs::fstat(&fd)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
            || stat.st_uid != geteuid().as_raw()
            || stat.st_mode & 0o7777 != 0o600
            || stat.st_nlink != 1
        {
            return Err(Errno::PERM);
        }
        fs::flock(&fd, FlockOperation::NonBlockingLockExclusive)?;
        let mut file = File::from(fd);
        let mut seed = [0; 16];
        // Recheck size under the lock: another launch may have initialized it.
        match file.metadata().map_err(|_| Errno::IO)?.len() {
            0 => {
                SystemRandom::new().fill(&mut seed).map_err(|_| Errno::IO)?;
                file.write_all(&seed).map_err(|_| Errno::IO)?;
                file.sync_all().map_err(|_| Errno::IO)?;
                fs::fsync(dir)?;
            }
            16 => file.read_exact(&mut seed).map_err(|_| Errno::IO)?,
            _ => return Err(Errno::INVAL),
        }
        Ok(Self { seed, _lock: file })
    }

    pub(crate) fn device_id(&self, origin: &str, account: &str) -> String {
        let mut hash = digest::Context::new(&digest::SHA256);
        hash.update(b"spofify-desktop-output-v1");
        hash.update(&self.seed);
        for value in [origin, account] {
            hash.update(&(value.len() as u64).to_be_bytes());
            hash.update(value.as_bytes());
        }
        hash.finish()
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::DirBuilder, os::unix::fs::DirBuilderExt, path::PathBuf};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("spofify-output-test-{}", uuid::Uuid::new_v4()));
            DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn identity_survives_relaunch_but_is_exclusive_and_account_scoped() {
        let temp = Scratch::new();
        let dir = storage::open_private(&temp.0).unwrap();
        let first = Installation::locked(&dir).unwrap();
        let id = first.device_id("https://one.test", "account");
        assert!(matches!(Installation::locked(&dir), Err(Errno::WOULDBLOCK)));
        assert_ne!(id, first.device_id("https://two.test", "account"));
        assert_ne!(id, first.device_id("https://one.test", "other"));
        drop(first);
        let second = Installation::locked(&dir).unwrap();
        assert_eq!(id, second.device_id("https://one.test", "account"));
    }

    #[test]
    fn identity_rejects_symlinks() {
        let temp = Scratch::new();
        let dir = storage::open_private(&temp.0).unwrap();
        let target = temp.0.join("target");
        std::fs::write(&target, b"example").unwrap();
        std::os::unix::fs::symlink(&target, temp.0.join("identity")).unwrap();
        assert!(Installation::locked(&dir).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"example");
    }
}
