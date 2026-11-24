//! Private local storage. Only the user-selected data base may resolve aliases;
//! app roots and entries are never canonicalized.

use std::{
    env,
    ffi::OsStr,
    fs as stdfs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};

use rustix::{
    fd::OwnedFd,
    fs::{self, Mode, OFlags},
    io::{Errno, Result},
    process::geteuid,
};

pub(crate) const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const PRIVATE_DIR: Mode = Mode::RWXU;

pub(crate) fn open_default(app_dir: &str) -> Result<OwnedFd> {
    open_data_dir(&default_base()?, app_dir, true)
}

pub(crate) fn open_default_existing(app_dir: &str) -> Result<OwnedFd> {
    open_data_dir(&default_base()?, app_dir, false)
}

fn default_base() -> Result<PathBuf> {
    let absolute = |name: &str| {
        env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    absolute("XDG_DATA_HOME")
        .or_else(|| absolute("HOME").map(|home| home.join(".local/share")))
        .ok_or(Errno::INVAL)
}

/// Only `base` is trusted for symlink resolution (e.g. /home -> var/home).
/// Resolve its nearest existing ancestor, then reopen that path without
/// following symlinks. Walk the missing suffix and app leaf by descriptor.
fn open_data_dir(base: &Path, app_dir: &str, create: bool) -> Result<OwnedFd> {
    let _ = absolute_parts(base)?;
    let mut parts = Path::new(app_dir).components();
    let (Some(Component::Normal(leaf)), None) = (parts.next(), parts.next()) else {
        return Err(Errno::INVAL);
    };
    if leaf != OsStr::new(app_dir) {
        return Err(Errno::INVAL);
    }
    let mut ancestor = base;
    let mut missing = Vec::new();
    let canonical = loop {
        match stdfs::canonicalize(ancestor) {
            Ok(path) => break path,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                if !create {
                    return Err(Errno::NOENT);
                }
                missing.push(ancestor.file_name().ok_or(Errno::INVAL)?);
                ancestor = ancestor.parent().ok_or(Errno::INVAL)?;
            }
            Err(_) => return Err(Errno::IO),
        }
    };
    if canonical == Path::new("/") {
        return Err(Errno::PERM);
    }
    let mut dir = walk(&canonical, false)?;
    for name in missing.into_iter().rev() {
        let child = open_child(&dir, name, true)?;
        // The child must survive a crash before an app can acknowledge a
        // durable write made under this newly created data base.
        fs::fsync(&dir)?;
        dir = child;
    }
    let root = open_child(&dir, leaf, create)?;
    private_dir(&root)?;
    if create {
        // A durable record inside the root is not sufficient if the newly
        // created root itself can disappear after a crash.
        fs::fsync(&dir)?;
    }
    Ok(root)
}

/// Explicit fixture path: no alias resolution, including at its ancestors.
#[cfg(test)]
pub(crate) fn open_private(path: &Path) -> Result<OwnedFd> {
    if path == Path::new("/") {
        return Err(Errno::INVAL);
    }
    let dir = walk(path, true)?;
    private_dir(&dir)?;
    Ok(dir)
}

fn absolute_parts(path: &Path) -> Result<std::path::Components<'_>> {
    let mut parts = path.components();
    if parts.next() != Some(Component::RootDir)
        || !parts.clone().all(|p| matches!(p, Component::Normal(_)))
    {
        return Err(Errno::INVAL);
    }
    Ok(parts)
}

fn walk(path: &Path, create: bool) -> Result<OwnedFd> {
    let parts = absolute_parts(path)?;
    let mut dir = fs::open("/", DIR_FLAGS, Mode::empty())?;
    for (i, part) in parts.enumerate() {
        let Component::Normal(name) = part else {
            return Err(Errno::INVAL);
        };
        dir = open_child(&dir, name, create && i != 0)?;
    }
    Ok(dir)
}

fn open_child(dir: &OwnedFd, name: &OsStr, create: bool) -> Result<OwnedFd> {
    match fs::openat(dir, name, DIR_FLAGS, Mode::empty()) {
        Err(Errno::NOENT) if create => {
            let s = fs::fstat(dir)?;
            let uid = geteuid().as_raw();
            // Never mkdir under a shared directory. When run as root, mere
            // ownership would also match system directories such as /var.
            if s.st_uid != uid
                || s.st_mode & 0o022 != 0
                || (uid == 0 && s.st_mode & 0o7777 != 0o700)
            {
                return Err(Errno::PERM);
            }
            match fs::mkdirat(dir, name, PRIVATE_DIR) {
                Ok(()) | Err(Errno::EXIST) => fs::openat(dir, name, DIR_FLAGS, Mode::empty()),
                Err(e) => Err(e),
            }
        }
        result => result,
    }
}

pub(crate) fn private_dir(dir: &OwnedFd) -> Result<()> {
    let s = fs::fstat(dir)?;
    if s.st_uid != geteuid().as_raw() || s.st_mode & 0o7777 != 0o700 {
        return Err(Errno::PERM);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = stdfs::canonicalize(env::temp_dir())
                .unwrap()
                .join(format!("spofify-data-base-test-{}", uuid::Uuid::new_v4()));
            stdfs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            stdfs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn distro_home_alias_works_but_app_leaf_symlinks_do_not() {
        let fixture = Fixture::new();
        let real_home = fixture.0.join("var/home/listener");
        stdfs::create_dir_all(&real_home).unwrap();
        stdfs::set_permissions(&real_home, stdfs::Permissions::from_mode(0o700)).unwrap();
        symlink("var/home", fixture.0.join("home")).unwrap();
        let base = fixture.0.join("home/listener/.local/share");
        let real_base = real_home.join(".local/share");
        // Playback lookups don't create a data tree. Explicit saves may create
        // missing .local/share, but only after resolving the fixture alias.
        assert!(matches!(
            open_data_dir(&base, "spofify-downloads-v1", false),
            Err(Errno::NOENT)
        ));
        assert!(!real_home.join(".local").exists());
        let opened = open_data_dir(&base, "spofify-downloads-v1", true).unwrap();
        assert!(open_data_dir(&base, "spofify-downloads-v1", false).is_ok());
        let app = real_base.join("spofify-downloads-v1");
        let reopened = open_private(&app).unwrap();
        assert_eq!(
            fs::fstat(&opened).unwrap().st_ino,
            fs::fstat(&reopened).unwrap().st_ino
        );
        assert!(open_private(&base.join("spofify-downloads-v1")).is_err());

        let target = fixture.0.join("private-target");
        stdfs::DirBuilder::new()
            .mode(0o700)
            .create(&target)
            .unwrap();
        stdfs::write(target.join("sentinel"), b"fixture").unwrap();
        stdfs::remove_dir(&app).unwrap();
        symlink(&target, &app).unwrap();
        symlink(&target, real_base.join("spofify-collections-v1")).unwrap();
        for name in ["spofify-downloads-v1", "spofify-collections-v1"] {
            assert!(open_data_dir(&base, name, true).is_err());
            assert!(open_data_dir(&base, name, false).is_err());
        }
        assert_eq!(stdfs::read(target.join("sentinel")).unwrap(), b"fixture");
    }

    #[test]
    fn shared_ancestor_and_dangling_base_are_not_created_through() {
        let fixture = Fixture::new();
        let shared = fixture.0.join("shared");
        stdfs::create_dir(&shared).unwrap();
        stdfs::set_permissions(&shared, stdfs::Permissions::from_mode(0o777)).unwrap();
        assert!(open_data_dir(&shared.join("missing/data"), "app", true).is_err());
        assert!(!shared.join("missing").exists());

        let alias = fixture.0.join("alias");
        symlink("not-there", &alias).unwrap();
        assert!(open_data_dir(&alias.join("data"), "app", true).is_err());
        assert!(!fixture.0.join("not-there").exists());
    }
}
