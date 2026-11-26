use std::{
    env, fs,
    io::{self, Cursor, Read},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const REPO: &str = "https://github.com/Lettuce222/lazygtd";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_BINARY: u64 = 100 * 1024 * 1024;

#[derive(Deserialize)]
struct Feed {
    #[serde(rename = "entry", default)]
    entries: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "link", default)]
    links: Vec<Link>,
}

#[derive(Deserialize)]
struct Link {
    #[serde(rename = "@href", default)]
    href: String,
    #[serde(rename = "@rel", default)]
    rel: String,
}

fn release_version(tag: &str) -> Option<Version> {
    let ver = Version::parse(tag.strip_prefix('v')?).ok()?;
    let preview = ver
        .pre
        .as_str()
        .strip_prefix("preview.")
        .and_then(|n| n.parse::<u32>().ok());
    (ver.build.is_empty() && (ver.pre.is_empty() || preview.is_some())).then_some(ver)
}

fn newest_release(feed: &str) -> Result<Option<Version>> {
    if feed.contains("<!DOCTYPE") {
        return Err("invalid release feed".into());
    }
    let feed: Feed = quick_xml::de::from_str(feed)?;
    let prefix = format!("{REPO}/releases/tag/");
    Ok(feed
        .entries
        .into_iter()
        .flat_map(|entry| entry.links)
        .filter(|link| link.rel.is_empty() || link.rel == "alternate")
        .filter_map(|link| release_version(link.href.strip_prefix(&prefix)?))
        .max())
}

fn download(url: &str, seconds: u64, limit: u64) -> Result<Vec<u8>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(Duration::from_secs(seconds)))
        .build()
        .into();
    Ok(agent
        .get(url)
        .call()?
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()?)
}

fn platform() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("linux-x86_64"),
        ("linux", "aarch64") => Ok("linux-aarch64"),
        ("macos", "x86_64") => Ok("macos-x86_64"),
        ("macos", "aarch64") => Ok("macos-arm64"),
        ("windows", "x86_64") => Ok("windows-x86_64"),
        ("windows", "aarch64") => Ok("windows-arm64"),
        _ => Err("no release for this platform".into()),
    }
}

pub fn available() -> Result<Option<Version>> {
    platform()?;
    let feed = download(&format!("{REPO}/releases.atom"), 4, 10 * 1024 * 1024)?;
    let latest =
        newest_release(std::str::from_utf8(&feed)?)?.ok_or("no published lazygtd release")?;
    Ok((latest > Version::parse(VERSION)?).then_some(latest))
}

fn check_digest(sums: &str, asset: &str, archive: &[u8]) -> Result<()> {
    let mut entries = sums.lines().filter_map(|line| {
        let (digest, name) = line.split_once(char::is_whitespace)?;
        (name.trim().trim_start_matches('*') == asset).then_some(digest)
    });
    let expected = entries.next().ok_or("no checksum for release archive")?;
    if entries.next().is_some() {
        return Err("duplicate checksums for release archive".into());
    }
    if !expected.eq_ignore_ascii_case(&format!("{:x}", Sha256::digest(archive))) {
        return Err("checksum mismatch for release archive".into());
    }
    Ok(())
}

fn copy_binary(input: impl Read, output: &mut fs::File) -> Result<()> {
    let size = io::copy(&mut input.take(MAX_BINARY + 1), output)?;
    if size == 0 || size > MAX_BINARY {
        return Err("invalid release executable size".into());
    }
    Ok(())
}

fn unpack_binary(archive: &[u8], windows: bool, output: &mut fs::File) -> Result<()> {
    if windows {
        let mut zip = zip::ZipArchive::new(Cursor::new(archive))?;
        let mut files = (0..zip.len()).filter(|&i| zip.name_for_index(i) == Some("lazygtd.exe"));
        let index = files.next().ok_or("release executable missing from ZIP")?;
        let unique = files.next().is_none();
        let entry = zip.by_index(index)?;
        if !unique || !entry.is_file() || entry.is_symlink() {
            return Err("invalid release executable entry".into());
        }
        return copy_binary(entry, output);
    }

    let mut tar = tar::Archive::new(GzDecoder::new(archive));
    let mut found = false;
    for entry in tar.entries()? {
        let entry = entry?;
        if entry.path()?.as_ref() != Path::new("lazygtd") {
            continue;
        }
        if found || !entry.header().entry_type().is_file() {
            return Err("invalid release executable entry".into());
        }
        copy_binary(entry, output)?;
        found = true;
    }
    if !found {
        return Err("release executable missing from archive".into());
    }
    Ok(())
}

fn executable_version(path: &Path) -> Result<Version> {
    let output = Command::new(path).arg("--version").output()?;
    let version = std::str::from_utf8(&output.stdout)?.trim();
    if !output.status.success() {
        return Err("could not run the release executable".into());
    }
    Ok(Version::parse(
        version
            .strip_prefix("lazygtd ")
            .ok_or("invalid lazygtd version")?,
    )?)
}

pub fn install(version: &Version) -> Result<PathBuf> {
    let executable = env::current_exe()?;
    let directory = executable.parent().ok_or("no executable directory")?;
    let windows = cfg!(windows);
    let extension = if windows { "zip" } else { "tar.gz" };
    let platform = platform()?;
    let asset = format!("lazygtd-v{version}-{platform}.{extension}");
    let base = format!("{REPO}/releases/download/v{version}");
    let sums = download(&format!("{base}/SHA256SUMS"), 15, 64 * 1024)?;
    let archive = download(&format!("{base}/{asset}"), 90, MAX_BINARY)?;
    check_digest(std::str::from_utf8(&sums)?, &asset, &archive)?;

    let mut staged = tempfile::Builder::new()
        .prefix(".lazygtd-update-")
        .suffix(env::consts::EXE_SUFFIX)
        .tempfile_in(directory)?;
    unpack_binary(&archive, windows, staged.as_file_mut())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = executable.metadata()?.permissions().mode() & 0o777;
        staged
            .as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
    }
    staged.as_file().sync_all()?;
    let staged = staged.into_temp_path();
    if executable_version(&staged)? != *version {
        return Err("release executable does not match its tag".into());
    }

    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(".lazygtd-update.lock"))?;
    lock.lock()?;
    if executable_version(&executable)? < *version {
        #[cfg(unix)]
        staged.persist(&executable)?;
        #[cfg(windows)]
        self_replace::self_replace(&staged)?;
    }
    Ok(executable)
}

pub fn run() -> Result<()> {
    match available()? {
        Some(version) => {
            let executable = install(&version)?;
            println!("lazygtd {} installed.", executable_version(&executable)?);
        }
        None => println!("lazygtd {VERSION} is up to date."),
    }
    Ok(())
}

pub fn restart(executable: &Path, args: &[std::ffi::OsString]) -> Result<()> {
    let mut command = Command::new(executable);
    command.args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec().into())
    }
    #[cfg(windows)]
    {
        if !command.status()?.success() {
            return Err("updated lazygtd exited unsuccessfully".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_the_highest_supported_published_release_including_previews() {
        let feed = format!(
            r#"<feed xmlns="http://www.w3.org/2005/Atom">
              <link href="{REPO}/releases/tag/v9.0.0"/>
              <entry><link rel="alternate" href="{REPO}/releases/tag/v0.1.0-preview.9"/></entry>
              <entry><link href="{REPO}/releases/tag/v0.1.0-preview.10"/></entry>
              <entry><link href="{REPO}/releases/tag/v1.0.0-rc.1"/></entry>
              <entry><link href="https://example.org/releases/tag/v5.0.0"/></entry>
              <entry><link rel="self" href="{REPO}/releases/tag/v3.0.0"/></entry>
            </feed>"#
        );
        assert_eq!(
            newest_release(&feed).unwrap().unwrap().to_string(),
            "0.1.0-preview.10"
        );
        assert!(release_version("v0.1.0").unwrap() > release_version("v0.1.0-preview.10").unwrap());
    }

    #[test]
    fn rejects_tampered_or_ambiguous_downloads() {
        let asset = "lazygtd-v0.1.0-preview.9-linux-x86_64.tar.gz";
        let archive = b"release archive";
        let checksums = format!("{:x}  {asset}\n", Sha256::digest(archive));
        assert!(check_digest(&checksums, asset, archive).is_ok());
        assert!(check_digest(&checksums, asset, b"tampered").is_err());
        assert!(check_digest(&checksums.repeat(2), asset, archive).is_err());
    }
}
