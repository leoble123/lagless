//! Auto-update from GitHub Releases.
//!
//! Every few hours: ask the GitHub API for the latest release. If it is newer, download the asset
//! for this platform, check it against the release's published SHA-256, and stage it. Then
//! "Restart now" (or simply quitting the app) swaps it in, so the next launch is the new version.
//!
//! The repository is baked in at build time (`LAGLESS_UPDATE_REPO=owner/repo`, which the release
//! workflow sets). Builds without it never phone home.

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const REPO: Option<&str> = option_env!("LAGLESS_UPDATE_REPO");
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);

#[cfg(windows)]
const ASSET: &str = "lagless-windows-x64.exe";
#[cfg(target_os = "macos")]
const ASSET: &str = "Lagless-macos.zip";
#[cfg(not(any(windows, target_os = "macos")))]
const ASSET: &str = "";

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// No update source compiled in, or not a platform we package.
    Disabled,
    Idle,
    Checking,
    UpToDate,
    Downloading(String),
    /// Downloaded, verified and staged; applies on restart or quit.
    Ready(String),
    Failed(String),
}

struct Staged {
    version: String,
    /// Windows: the new .exe. macOS: the new Lagless.app (next to the current one).
    path: PathBuf,
}

#[derive(Clone)]
pub struct Updater {
    state: Arc<Mutex<State>>,
    staged: Arc<Mutex<Option<Staged>>>,
    wake: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

impl Updater {
    pub fn start(enabled: bool) -> Self {
        let u = Updater {
            state: Arc::new(Mutex::new(State::Idle)),
            staged: Default::default(),
            wake: Default::default(),
        };
        cleanup_previous();
        if REPO.is_none() || ASSET.is_empty() {
            *u.state.lock().unwrap() = State::Disabled;
            return u;
        }
        let me = u.clone();
        std::thread::Builder::new()
            .name("updater".into())
            .spawn(move || {
                let mut auto = enabled;
                loop {
                    if auto {
                        me.check_once();
                    }
                    if matches!(*me.state.lock().unwrap(), State::Ready(_)) {
                        return;
                    }
                    // Sleep until the next scheduled check, or until "Check now".
                    let (lock, cv) = &*me.wake;
                    let woke = cv.wait_timeout_while(lock.lock().unwrap(), CHECK_EVERY, |w| !*w).unwrap();
                    let mut w = woke.0;
                    auto = enabled || *w;
                    *w = false;
                }
            })
            .ok();
        u
    }

    /// Check, download and install right now (command-line `lagless update`).
    pub fn update_blocking() -> Result<Option<String>> {
        if REPO.is_none() || ASSET.is_empty() {
            bail!("this build has no update source (official releases update from GitHub)");
        }
        let u = Updater { state: Arc::new(Mutex::new(State::Idle)), staged: Default::default(), wake: Default::default() };
        let v = u.try_update()?;
        if v.is_some() {
            u.apply(false)?;
        }
        Ok(v)
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    pub fn check_now(&self) {
        let (lock, cv) = &*self.wake;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }

    fn set(&self, s: State) {
        *self.state.lock().unwrap() = s;
    }

    fn check_once(&self) {
        self.set(State::Checking);
        match self.try_update() {
            Ok(Some(v)) => self.set(State::Ready(v)),
            Ok(None) => self.set(State::UpToDate),
            Err(e) => self.set(State::Failed(format!("{e:#}"))),
        }
    }

    fn try_update(&self) -> Result<Option<String>> {
        let repo = REPO.unwrap();
        // Override only for testing against a local mock of the GitHub API.
        let api = std::env::var("LAGLESS_UPDATE_API").unwrap_or_else(|_| "https://api.github.com".into());
        let rel: serde_json::Value = agent()
            .get(&format!("{api}/repos/{repo}/releases/latest"))
            .header("Accept", "application/vnd.github+json")
            .call()?
            .body_mut()
            .read_json()?;
        let tag = rel["tag_name"].as_str().ok_or_else(|| anyhow!("release has no tag"))?;
        if !is_newer(tag, VERSION) {
            return Ok(None);
        }
        let version = tag.trim_start_matches('v').to_string();
        let url_of = |name: &str| {
            rel["assets"].as_array().into_iter().flatten().find(|a| a["name"] == name).and_then(|a| a["browser_download_url"].as_str()).map(str::to_string)
        };
        let url = url_of(ASSET).ok_or_else(|| anyhow!("release {tag} has no {ASSET}"))?;
        let sum_url = url_of(&format!("{ASSET}.sha256")).ok_or_else(|| anyhow!("release {tag} has no checksum"))?;

        self.set(State::Downloading(version.clone()));
        let want = String::from_utf8(download(&sum_url)?)?.split_whitespace().next().unwrap_or("").to_lowercase();
        let bytes = download(&url)?;
        let got: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        if want.len() != 64 || got != want {
            bail!("downloaded update failed its checksum; ignoring it");
        }
        let path = stage(&bytes, &version)?;
        *self.staged.lock().unwrap() = Some(Staged { version: version.clone(), path });
        Ok(Some(version))
    }

    /// Swap the staged update in. With `relaunch`, start the new version (caller should exit).
    pub fn apply(&self, relaunch: bool) -> Result<()> {
        let staged = self.staged.lock().unwrap().take().ok_or_else(|| anyhow!("no update staged"))?;
        install(&staged.path, relaunch).with_context(|| format!("installing {}", staged.version))
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(120)))
        .user_agent(format!("lagless/{VERSION}"))
        .build()
        .into()
}

fn download(url: &str) -> Result<Vec<u8>> {
    Ok(agent().get(url).call()?.body_mut().with_config().limit(200 << 20).read_to_vec()?)
}

fn parse_version(v: &str) -> Vec<u64> {
    v.trim_start_matches('v').split(['.', '-']).map_while(|p| p.parse().ok()).collect()
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    parse_version(candidate) > parse_version(current)
}

// ---------------------------------------------------------------- Windows

#[cfg(windows)]
fn stage(bytes: &[u8], version: &str) -> Result<PathBuf> {
    let p = std::env::temp_dir().join(format!("lagless-{version}.exe"));
    std::fs::write(&p, bytes)?;
    Ok(p)
}

#[cfg(windows)]
fn install(new_exe: &Path, relaunch: bool) -> Result<()> {
    self_replace::self_replace(new_exe)?;
    let _ = std::fs::remove_file(new_exe);
    if relaunch {
        std::process::Command::new(std::env::current_exe()?).spawn()?;
    }
    Ok(())
}

#[cfg(windows)]
fn cleanup_previous() {}

// ---------------------------------------------------------------- macOS

/// …/Lagless.app when running from an app bundle.
#[cfg(target_os = "macos")]
fn bundle_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let app = exe.ancestors().nth(3)?;
    (app.extension()? == "app").then(|| app.to_path_buf())
}

#[cfg(target_os = "macos")]
fn work_dir() -> Option<PathBuf> {
    // Same volume as the app, so swapping is an atomic rename.
    Some(bundle_path()?.parent()?.join(".lagless-update"))
}

#[cfg(target_os = "macos")]
fn stage(bytes: &[u8], version: &str) -> Result<PathBuf> {
    let work = work_dir().ok_or_else(|| anyhow!("not running from Lagless.app; download the update manually"))?;
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).context("the folder holding Lagless.app isn't writable — move the app to Applications")?;
    let dest = work.join(version);
    extract_zip(bytes, &dest)?;
    let app = std::fs::read_dir(&dest)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .ok_or_else(|| anyhow!("update archive has no .app"))?;
    Ok(app)
}

#[cfg(target_os = "macos")]
fn install(new_app: &Path, relaunch: bool) -> Result<()> {
    let app = bundle_path().ok_or_else(|| anyhow!("not running from Lagless.app"))?;
    let old = work_dir().unwrap().join("previous.app");
    std::fs::rename(&app, &old)?;
    if let Err(e) = std::fs::rename(new_app, &app) {
        let _ = std::fs::rename(&old, &app); // put the working version back
        return Err(e.into());
    }
    if relaunch {
        std::process::Command::new("open").arg("-n").arg(&app).spawn()?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn cleanup_previous() {
    if let Some(w) = work_dir() {
        let _ = std::fs::remove_dir_all(w);
    }
}

#[cfg(target_os = "macos")]
fn extract_zip(bytes: &[u8], dest: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        let Some(rel) = f.enclosed_name() else { continue };
        if rel.starts_with("__MACOSX") {
            continue;
        }
        let out = dest.join(rel);
        if f.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if f.is_symlink() {
            let mut target = String::new();
            std::io::Read::read_to_string(&mut f, &mut target)?;
            std::os::unix::fs::symlink(target, &out)?;
            continue;
        }
        std::io::copy(&mut f, &mut std::fs::File::create(&out)?)?;
        if let Some(mode) = f.unix_mode() {
            std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode & 0o777))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- other (dev builds)

#[cfg(not(any(windows, target_os = "macos")))]
fn stage(_: &[u8], _: &str) -> Result<PathBuf> {
    bail!("self-update is only packaged for Windows and macOS")
}

#[cfg(not(any(windows, target_os = "macos")))]
fn install(_: &Path, _: bool) -> Result<()> {
    bail!("self-update is only packaged for Windows and macOS")
}

#[cfg(not(any(windows, target_os = "macos")))]
fn cleanup_previous() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("v1.0.0", "0.9.12"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.0.9", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }
}
