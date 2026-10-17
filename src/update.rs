//! Updating from GitHub Releases: find the newest `vX.Y.Z` release, download its zip, check
//! its SHA-256, swap the exe in place, and let the caller restart.
//!
//! No HTTP or zip crate: Windows' own `curl.exe` and `tar.exe` (always from System32, never
//! from PATH) do the downloading and unpacking. Every release is a prerelease, so the newest
//! one is picked from `/releases` (`/releases/latest` skips prereleases).

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use windows_sys::Win32::Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptHash};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

/// `owner/name` of the GitHub repository, from Cargo.toml's `repository`.
pub const REPO: &str = {
    let url = env!("CARGO_PKG_REPOSITORY").as_bytes();
    let prefix = b"https://github.com/";
    // `str::strip_prefix` isn't const; slice past the prefix instead.
    let (_, rest) = url.split_at(prefix.len());
    match std::str::from_utf8(rest) {
        Ok(s) => s,
        Err(_) => panic!("repository must be a GitHub URL"),
    }
};
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const USER_AGENT: &str = concat!("fan-lite/", env!("CARGO_PKG_VERSION"));
/// The exe's name inside the release zip.
const EXE: &str = "fan-lite.exe";
/// Where the running exe is moved while the new one takes its place.
const OLD: &str = "fan-lite.old.exe";

/// `vX.Y.Z` or `X.Y.Z` as a comparable triple; anything else (e.g. `-rc1`) is `None`.
pub fn version(tag: &str) -> Option<(u32, u32, u32)> {
    let mut it = tag.strip_prefix('v').unwrap_or(tag).split('.');
    let mut n = || it.next()?.parse::<u32>().ok();
    let v = (n()?, n()?, n()?);
    it.next().is_none().then_some(v)
}

/// The highest version tag in a GitHub `/releases` JSON reply.
pub fn newest_tag(json: &str) -> Option<String> {
    json.split("\"tag_name\"")
        .skip(1)
        .filter_map(|rest| {
            let rest = rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
            let tag = &rest[..rest.find('"')?];
            Some((version(tag)?, tag.to_owned()))
        })
        .max()
        .map(|(_, t)| t)
}

/// Lower-case hex SHA-256, by Windows' CNG.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut out = [0u8; 32];
    // BCryptHash takes a u32 length; hash in one go (release zips are a few MB).
    let Ok(len) = u32::try_from(data.len()) else {
        return String::new();
    };
    let status = unsafe {
        BCryptHash(
            BCRYPT_SHA256_ALG_HANDLE,
            std::ptr::null(),
            0,
            data.as_ptr(),
            len,
            out.as_mut_ptr(),
            32,
        )
    };
    if status != 0 {
        return String::new();
    }
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// A Windows tool from System32, without a console window.
fn system_tool(name: &str) -> Command {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let mut c = Command::new(Path::new(&root).join("System32").join(format!("{name}.exe")));
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

/// Run a command; its stderr (first line) is the error.
fn run(c: &mut Command) -> Result<Vec<u8>, String> {
    let out = c.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let line = err.lines().next().unwrap_or("").trim();
    Err(line.strip_prefix("curl: ").unwrap_or(line).to_owned())
}

fn curl() -> Command {
    let mut c = system_tool("curl");
    c.args(["--silent", "--show-error", "--fail", "--location", "--max-time", "120"])
        .args(["--user-agent", USER_AGENT]);
    c
}

/// The newest release tag if it is newer than this build.
pub fn check() -> Result<Option<String>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=30");
    let json = run(curl().arg(url))?;
    Ok(newer_than(&String::from_utf8_lossy(&json), CURRENT))
}

/// `newest_tag(json)` if it is newer than `current`.
pub fn newer_than(json: &str, current: &str) -> Option<String> {
    let tag = newest_tag(json)?;
    (version(&tag)? > version(current)?).then_some(tag)
}

/// The folder downloads are unpacked into.
fn work_dir() -> PathBuf {
    std::env::temp_dir().join("fan-lite-update")
}

/// True for an exe inside a cargo `target` folder, which updates come from git, not GitHub.
pub fn is_dev_build(exe: &Path) -> bool {
    exe.parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .is_some_and(|n| n.eq_ignore_ascii_case("target"))
}

/// Download release `tag`, check it and swap it in for the exe at `exe`. On success the old
/// exe is at `fan-lite.old.exe` next to it (see [`rollback`] and [`clean_up`]).
pub fn install(tag: &str, exe: &Path) -> Result<(), String> {
    if is_dev_build(exe) {
        return Err("this is a development build: update it with git pull and cargo build".into());
    }
    let dir = work_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = format!("fan-lite-{tag}-windows-x64.zip");
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/{name}");
    let zip = dir.join(&name);
    run(curl().arg("--output").arg(&zip).arg(&url))?;
    let sums = run(curl().arg(format!("{url}.sha256")))?;
    let want = String::from_utf8_lossy(&sums)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let data = std::fs::read(&zip).map_err(|e| e.to_string())?;
    if want.len() != 64 || sha256_hex(&data) != want {
        return Err("the download is damaged (checksum mismatch); nothing was changed".into());
    }
    let out = dir.join("unpacked");
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    run(system_tool("tar").arg("-xf").arg(&zip).arg("-C").arg(&out))?;
    let new = out.join(EXE);
    if !new.is_file() {
        return Err(format!("{EXE} is not in the release zip; nothing was changed"));
    }
    swap(exe, &new)
}

/// Move `cur` to `fan-lite.old.exe` beside it and copy `new` into its place. If the copy
/// fails, `cur` is put back.
pub fn swap(cur: &Path, new: &Path) -> Result<(), String> {
    let old = cur.with_file_name(OLD);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(cur, &old).map_err(|e| format!("cannot move the running exe aside: {e}"))?;
    if let Err(e) = std::fs::copy(new, cur) {
        let _ = std::fs::remove_file(cur);
        let _ = std::fs::rename(&old, cur);
        return Err(format!("cannot write the new exe ({e}); nothing was changed"));
    }
    Ok(())
}

/// Undo a [`swap`]: the new exe is removed and the old one put back.
pub fn rollback(cur: &Path) -> Result<(), String> {
    let old = cur.with_file_name(OLD);
    if !old.is_file() {
        return Err("no previous version to go back to".into());
    }
    let _ = std::fs::remove_file(cur);
    std::fs::rename(&old, cur).map_err(|e| e.to_string())
}

/// Remove what an earlier update left behind. Run at every start.
pub fn clean_up(exe: &Path) {
    let _ = std::fs::remove_file(exe.with_file_name(OLD));
    let _ = std::fs::remove_dir_all(work_dir());
}

/// [`clean_up`] right after an update, on a background thread: the old process has released
/// the instance lock but Windows can keep its exe locked for a moment while it exits, so keep
/// trying for a few seconds.
pub fn clean_up_after_update(exe: PathBuf) {
    std::thread::spawn(move || {
        let old = exe.with_file_name(OLD);
        for _ in 0..50 {
            clean_up(&exe);
            if !old.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_and_versions() {
        assert_eq!(REPO, "Matt3ws/fan-lite");
        assert_eq!(version("v0.2.0"), Some((0, 2, 0)));
        assert_eq!(version("1.10.3"), Some((1, 10, 3)));
        assert_eq!(version("v0.3.0-rc1"), None);
        assert_eq!(version("v1.2"), None);
        assert_eq!(version("v1.2.3.4"), None);
        assert!(version("v0.10.0") > version("v0.9.9"));
        assert!(version(CURRENT).is_some(), "Cargo.toml version must parse");
    }

    #[test]
    fn newest_tag_from_releases_json() {
        let json = r#"[{"url":"x","tag_name": "v0.2.0","name":"a","body":"see v9.9.9"},
            {"tag_name":"v0.10.1","assets":[]},{"tag_name":"v0.3.0-rc1"},{"tag_name":"v0.9.0"}]"#;
        assert_eq!(newest_tag(json).as_deref(), Some("v0.10.1"));
        assert_eq!(newer_than(json, "0.10.1"), None);
        assert_eq!(newer_than(json, "0.2.0").as_deref(), Some("v0.10.1"));
        assert_eq!(newest_tag("[]"), None);
    }

    #[test]
    fn sha256_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(&vec![b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn swap_and_rollback() {
        let dir = std::env::temp_dir().join(format!("fan-lite-swap-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cur = dir.join("fan-lite.exe");
        let new = dir.join("new.exe");
        std::fs::write(&cur, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();

        swap(&cur, &new).unwrap();
        assert_eq!(std::fs::read(&cur).unwrap(), b"new");
        assert_eq!(std::fs::read(dir.join(OLD)).unwrap(), b"old");
        rollback(&cur).unwrap();
        assert_eq!(std::fs::read(&cur).unwrap(), b"old");
        assert!(!dir.join(OLD).exists());

        // A missing new file leaves the current exe where it was.
        assert!(swap(&cur, &dir.join("missing.exe")).is_err());
        assert_eq!(std::fs::read(&cur).unwrap(), b"old");

        std::fs::write(dir.join(OLD), b"leftover").unwrap();
        clean_up(&cur);
        assert!(!dir.join(OLD).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dev_builds_are_recognised() {
        assert!(is_dev_build(Path::new(r"C:\src\fan-lite\target\release\fan-lite.exe")));
        assert!(is_dev_build(Path::new(r"C:\src\fan-lite\target\debug\fan-lite.exe")));
        assert!(!is_dev_build(Path::new(r"C:\Tools\fan-lite\fan-lite.exe")));
    }
}

/// Against the real GitHub: `cargo test --lib update_live -- --ignored --nocapture`.
#[cfg(test)]
mod live {
    #[test]
    #[ignore]
    fn update_live() {
        println!("check(): {:?}", super::check());
    }

    /// Installs release v0.2.0 over a dummy exe in a temp folder (nothing is started).
    #[test]
    #[ignore]
    fn install_live() {
        let dir = std::env::temp_dir().join("fan-lite-install-live");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("fan-lite.exe");
        std::fs::write(&exe, b"dummy").unwrap();
        println!("install: {:?}", super::install("v0.2.0", &exe));
        let new = std::fs::metadata(&exe).map(|m| m.len()).unwrap_or(0);
        let old = std::fs::read(dir.join(super::OLD)).unwrap_or_default();
        println!("new exe {new} bytes, old exe {:?}", String::from_utf8_lossy(&old));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
