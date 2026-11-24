//! Keep task bodies and agent prompts private at the filesystem boundary.
use std::{
    fs::{self, Metadata, OpenOptions},
    io,
    path::Path,
};

pub fn create_profile_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    #[cfg(windows)]
    {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        match crate::local_files::create_private(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    guard(&fs::symlink_metadata(path)?, true)
}

fn deny(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, msg)
}
fn guard(meta: &Metadata, dir: bool) -> io::Result<()> {
    if meta.file_type().is_symlink() || (dir && !meta.is_dir()) || (!dir && !meta.is_file()) {
        return Err(deny(
            "profile paths must be regular files/directories, not symlinks",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if dir && meta.mode() & 0o022 != 0 {
            return Err(deny(
                "database directory is writable by other users; use a user-owned local directory",
            ));
        }
        if !dir && (meta.nlink() != 1 || meta.mode() & 0o077 != 0) {
            return Err(deny(
                "database or journal is not private; close lazygtd, back it up, then remove group/other permissions (chmod go-rwx) or use a private copy",
            ));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(deny(
                "reparse points are not allowed for database files or their immediate directory",
            ));
        }
    }
    Ok(())
}

pub fn prepare(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    guard(&fs::symlink_metadata(parent)?, true)?;
    match fs::symlink_metadata(path) {
        Ok(m) => guard(&m, false)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                match OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)
                {
                    Ok(file) => guard(&file.metadata()?, false)?,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        guard(&fs::symlink_metadata(path)?, false)?
                    }
                    Err(e) => return Err(e),
                }
            }
            #[cfg(windows)]
            {
                // Native Windows explicitly uses a protected profile directory. A custom
                // directory is treated as trusted by the user; see the threat model.
                match OpenOptions::new().write(true).create_new(true).open(path) {
                    Ok(file) => guard(&file.metadata()?, false)?,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        guard(&fs::symlink_metadata(path)?, false)?
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        Err(e) => return Err(e),
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut file = path.as_os_str().to_os_string();
        file.push(suffix);
        match fs::symlink_metadata(&file) {
            Ok(meta) => guard(&meta, false)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
