//! Host-side opening of explicitly selected local task content.
use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const BODY_LIMIT: usize = 32_000;
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

fn linked(meta: &Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Treat mount points, junctions and other reparse points as links too.
        meta.file_attributes() & 0x0400 != 0
    }
    #[cfg(not(windows))]
    false
}

struct PrivateDir(PathBuf);
impl PrivateDir {
    fn create() -> io::Result<Self> {
        let base = private_temp_parent()?.canonicalize()?;
        let salt = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..100 {
            let seq = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!(
                "lazygtd-edit-{}-{salt:x}-{seq:x}",
                std::process::id()
            ));
            match create_private(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not choose a unique editor directory",
        ))
    }
}
impl Drop for PrivateDir {
    fn drop(&mut self) {
        // An editor can create backups/swap beside the edited file. Its
        // directory is unique to this process; remove that and nothing else.
        if fs::symlink_metadata(&self.0).is_ok_and(|m| m.is_dir() && !linked(&m)) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(unix)]
fn private_temp_parent() -> io::Result<PathBuf> {
    Ok(env::temp_dir())
}
#[cfg(unix)]
pub(crate) fn create_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)
}
#[cfg(windows)]
fn private_temp_parent() -> io::Result<PathBuf> {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| invalid("LOCALAPPDATA must be an absolute path"))
}
#[cfg(windows)]
pub(crate) fn create_private(path: &Path) -> io::Result<()> {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr};
    #[repr(C)]
    struct SecurityAttributes {
        len: u32,
        descriptor: *mut c_void,
        inherit_handle: i32,
    }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text: *const u16,
            revision: u32,
            out: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateDirectoryW(path: *const u16, attributes: *const SecurityAttributes) -> i32;
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
    }
    // Protected DACL permits only the directory/file owner and Windows
    // SYSTEM. OI/CI propagate the same rule to editor-created contents.
    let acl: Vec<_> = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)"
        .encode_utf16()
        .chain([0])
        .collect();
    let path: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut desc = ptr::null_mut();
    // SAFETY: input buffers are NUL-terminated and remain live. A successful
    // conversion returns a LocalAlloc-owned descriptor which we release once.
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            acl.as_ptr(),
            1,
            &mut desc,
            ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let attrs = SecurityAttributes {
            len: std::mem::size_of::<SecurityAttributes>() as u32,
            descriptor: desc,
            inherit_handle: 0,
        };
        let result = if CreateDirectoryW(path.as_ptr(), &attrs) == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        };
        LocalFree(desc);
        result
    }
}

pub(crate) fn edit_body(body: &str) -> io::Result<String> {
    let program = selected_editor(env::var_os("LAZYGTD_EDITOR"), env::var_os("EDITOR"));
    #[cfg(windows)]
    if Path::new(&program)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| {
            ["cmd", "bat", "ps1"]
                .iter()
                .any(|candidate| ext.eq_ignore_ascii_case(candidate))
        })
    {
        return Err(invalid(
            "editor must be an executable, not a Windows shell script",
        ));
    }
    edit_with(&program, body)
}

fn selected_editor(override_: Option<OsString>, fallback: Option<OsString>) -> OsString {
    override_
        .filter(|s| !s.is_empty())
        .or_else(|| fallback.filter(|s| !s.is_empty()))
        .unwrap_or_else(|| OsString::from("nvim"))
}

fn edit_with(program: &OsStr, body: &str) -> io::Result<String> {
    let directory = PrivateDir::create()?;
    let path = directory.0.join("body.md");
    let mut create = OpenOptions::new();
    create.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        create.mode(0o600);
    }
    {
        let mut file = create.open(&path)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
    }
    let status = Command::new(program).arg(command_path(&path)).status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "editor exited with {status}; existing body is unchanged"
        )));
    }
    let meta = fs::symlink_metadata(&path)?;
    if !meta.is_file() || linked(&meta) {
        return Err(invalid("editor output is not a regular file"));
    }
    if !path.canonicalize()?.starts_with(&directory.0) {
        return Err(invalid("editor output escaped its private directory"));
    }
    // An editor may emit a UTF-8 byte-order mark. Allow only that small
    // overhead and bound the read even if the file grows after stat.
    let mut data = Vec::new();
    File::open(&path)?
        .take((BODY_LIMIT + 4) as u64)
        .read_to_end(&mut data)?;
    let data = data.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&data);
    if data.len() > BODY_LIMIT {
        return Err(invalid("edited body exceeds 32000 bytes"));
    }
    let body = std::str::from_utf8(data).map_err(|_| invalid("edited body is not UTF-8"))?;
    if body
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(invalid(
            "edited body contains incompatible control characters",
        ));
    }
    Ok(body.replace("\r\n", "\n").replace('\r', "\n"))
}

fn contained_dir(parent: &Path, name: &OsStr) -> io::Result<PathBuf> {
    let path = parent.join(name);
    let meta = fs::symlink_metadata(&path)?;
    if !meta.is_dir() || linked(&meta) {
        return Err(invalid(
            "artifact directories must not be symlinks or reparse points",
        ));
    }
    let resolved = path.canonicalize()?;
    if resolved.parent() != Some(parent) {
        return Err(invalid("artifact directory is outside its expected parent"));
    }
    Ok(resolved)
}

/// Resolve only an existing task directory relative to the database's
/// canonical location. Does not create any directories or follow a link
/// inside the artifact tree.
pub(crate) fn artifact_dir(db_path: &Path, id: i64) -> io::Result<PathBuf> {
    if id <= 0 {
        return Err(invalid("task id must be a positive integer"));
    }
    let db = db_path.canonicalize()?;
    if !fs::metadata(&db)?.is_file() {
        return Err(invalid("artifact lookup requires a local database file"));
    }
    let parent = db
        .parent()
        .ok_or_else(|| invalid("database has no parent directory"))?;
    let mut root = db
        .file_name()
        .ok_or_else(|| invalid("database has no filename"))?
        .to_os_string();
    root.push(".assets");
    let root = contained_dir(parent, &root)?;
    contained_dir(&root, OsStr::new(&id.to_string()))
}

#[cfg(not(windows))]
fn command_path(path: &Path) -> PathBuf {
    path.to_owned()
}

#[cfg(windows)]
fn command_path(path: &Path) -> PathBuf {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    // Explorer carries its own path parser. Give it an ordinary absolute
    // Win32/UNC path rather than Rust canonicalize's extended-length spelling.
    let wide: Vec<_> = path.as_os_str().encode_wide().collect();
    let unc: Vec<_> = r"\\?\UNC\".encode_utf16().collect();
    let prefix: Vec<_> = r"\\?\".encode_utf16().collect();
    if let Some(tail) = wide.strip_prefix(&unc[..]) {
        let mut value = vec![b'\\' as u16; 2];
        value.extend_from_slice(tail);
        return PathBuf::from(OsString::from_wide(&value));
    }
    if let Some(tail) = wide.strip_prefix(&prefix[..])
        && tail.get(1) == Some(&(b':' as u16))
    {
        return PathBuf::from(OsString::from_wide(tail));
    }
    path.to_owned()
}

/// Launch the existing host directory without a shell or an externally
/// supplied URI. Only pass the path already resolved by `artifact_dir`.
pub(crate) fn open_artifact_dir(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !path.is_absolute() || !meta.is_dir() || linked(&meta) || path.canonicalize()? != path {
        return Err(invalid("artifact directory must be existing and resolved"));
    }
    #[cfg(windows)]
    let mut opener = {
        let mut cmd = Command::new("explorer.exe");
        cmd.arg(command_path(path));
        cmd
    };
    #[cfg(target_os = "macos")]
    let mut opener = {
        let mut cmd = Command::new("open");
        cmd.arg(path);
        cmd
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut opener = {
        if env::var_os("WSL_INTEROP").is_some() {
            let out = Command::new("wslpath").arg("-w").arg(path).output()?;
            if !out.status.success() {
                return Err(invalid(
                    "wslpath could not translate the artifact directory",
                ));
            }
            let out = std::str::from_utf8(&out.stdout)
                .map_err(|_| invalid("wslpath did not return UTF-8"))?;
            let win = out.trim_end_matches(['\n', '\r']);
            let absolute = win.starts_with(r"\\") || win.as_bytes().get(1..3) == Some(b":\\");
            if !absolute || win.chars().any(char::is_control) {
                return Err(invalid(
                    "could not resolve a Windows directory for the artifact",
                ));
            }
            let mut cmd = Command::new("explorer.exe");
            cmd.arg(win);
            cmd
        } else {
            let mut cmd = Command::new("xdg-open");
            cmd.arg(path);
            cmd
        }
    };
    let mut child = opener
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Reap the launcher without holding the terminal until the application
    // closes. Success here is launch accepted, not proof of a GUI window.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_is_database_specific_and_never_creates_artifacts() {
        let temp = PrivateDir::create().unwrap();
        let db = temp.0.join("work.sqlite3");
        fs::write(&db, b"fixture").unwrap();
        assert_eq!(
            artifact_dir(&db, 7).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let base = temp.0.join("work.sqlite3.assets");
        assert!(!base.exists());
        let expected = base.join("7");
        fs::create_dir(&base).unwrap();
        fs::create_dir(&expected).unwrap();
        assert_eq!(artifact_dir(&db, 7).unwrap(), expected);
        assert_eq!(
            artifact_dir(&db, 0).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            artifact_dir(&db, 8).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            open_artifact_dir(Path::new("relative-path"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_even_when_they_point_inside_the_artifact_tree() {
        use std::os::unix::fs::symlink;
        let temp = PrivateDir::create().unwrap();
        let db = temp.0.join("test.db");
        fs::write(&db, b"fixture").unwrap();
        let outside = temp.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::create_dir(outside.join("7")).unwrap();
        let base = temp.0.join("test.db.assets");
        symlink(&outside, &base).unwrap();
        assert_eq!(
            artifact_dir(&db, 7).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        fs::remove_file(&base).unwrap();
        fs::create_dir(&base).unwrap();
        fs::create_dir(base.join("8")).unwrap();
        symlink(base.join("8"), base.join("7")).unwrap();
        assert_eq!(
            artifact_dir(&db, 7).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[cfg(unix)]
    #[test]
    fn editor_receives_one_path_can_replace_atomically_and_cannot_return_link() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = PrivateDir::create().unwrap();
        let executable = fixture.0.join("fake editor");
        let write_script = |body: &str| {
            fs::write(&executable, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        };
        write_script(
            r#"test "$#" -eq 1
mode() { stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1"; }
test "$(mode "$(dirname "$1")")" = 700
test "$(mode "$1")" = 600
test "$(cat "$1")" = initial
printf '新しい本文\r\n2' > "$1.new"
mv -- "$1.new" "$1""#,
        );
        assert_eq!(
            edit_with(executable.as_os_str(), "initial").unwrap(),
            "新しい本文\n2"
        );
        write_script(
            r#"rm -- "$1"
ln -s /dev/null "$1""#,
        );
        assert_eq!(
            edit_with(executable.as_os_str(), "initial")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        write_script(r#"yes x | head -c 32001 > "$1""#);
        assert_eq!(
            edit_with(executable.as_os_str(), "initial")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn selected_editor_does_not_parse_arguments() {
        let _func: fn(&str) -> io::Result<String> = edit_body;
        assert_eq!(selected_editor(None, None), "nvim");
        assert_eq!(selected_editor(Some("".into()), Some("vim".into())), "vim");
        assert_eq!(
            selected_editor(Some("Name With Spaces".into()), Some("vim".into())),
            "Name With Spaces"
        );
    }
}
