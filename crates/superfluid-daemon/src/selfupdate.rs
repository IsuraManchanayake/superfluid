//! `superfluid self update|list|use|check|install`: superfluid keeps its
//! releases side by side under `$SUPERFLUID_HOME/versions/<v>/`, with a
//! `current` symlink that the links on PATH go through. An update unpacks the
//! new release beside the old one, proves it starts, flips the link, keeps
//! the version it replaced for `self use`, and prunes the rest. Nothing a
//! running server uses is written to. `serve` asks once a day whether a
//! newer release exists and prints one line when it does.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use superfluid_adapter_kit::install::{fetch, single_top, unpack, version_key, Asset};

use crate::runtime_pick::RuntimeId;
use crate::runtimes::Catalog;

pub const REPO: &str = "basecompute/superfluid";
const LISTING: &str = "https://api.github.com/repos/basecompute/superfluid/releases?per_page=20";
/// A release younger than this is not announced by the daily check; `self
/// update` installs it when asked. The days give early reports time to land.
pub const HOLD_OFF_DAYS: i64 = 3;
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
pub const NO_CHECK_ENV: &str = "SUPERFLUID_NO_UPDATE_CHECK";
/// Where the release listing is read from, for a mirror or a test: a URL or a file.
pub const LISTING_ENV: &str = "SUPERFLUID_RELEASES";
const LINKED: &[&str] = &["superfluid", "superfluid-workerd", "superfluid-noded"];

pub const HELP: &str = "Update superfluid itself, or switch between the versions installed here.

Usage:
  superfluid self update [--version <v>] [--dry-run]   Install the newest release that fits this machine and make it current
  superfluid self check                                 Ask whether a newer release exists, without installing
  superfluid self list                                  The versions installed here, and which is current
  superfluid self use <v>                               Make an installed version current (the one an update replaced stays)
  superfluid self install --from <dir> [--prefix <dir>] Place an unpacked release here; what the install script runs

Releases live side by side under $SUPERFLUID_HOME/versions/<v>, with a `current` link that the
`superfluid` on PATH goes through. An update unpacks the new release beside the old one, proves
it starts, flips the link, keeps the version it replaced and removes older ones (a version with
a `keep` file inside, or one a running server started from, stays). A running server keeps the
version it started with until it is started again.

With a baseRT engine here, a release must declare the engine's ABI number; with llama.cpp or
MLX only, the newest release fits. A superfluid installed by a package manager or cargo is
updated the way it was installed, and this command says so.

`superfluid serve` checks for a newer release once a day, after the server is up, and prints
one line; SUPERFLUID_NO_UPDATE_CHECK=1 or --offline turns that off. A release is announced a
few days after it is published.
";

/// The release asset name for this build's platform.
pub fn target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        _ => None,
    }
}

pub fn running() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ----- the layout ---------------------------------------------------------

/// `$SUPERFLUID_HOME/versions`: one directory per release, `current` and
/// `previous`, and `prefix` naming the bin directory that links here.
pub struct Versions {
    pub dir: PathBuf,
}

impl Versions {
    pub fn of(home: &Path) -> Versions {
        Versions { dir: home.join("versions") }
    }

    pub fn version_dir(&self, v: &str) -> PathBuf {
        self.dir.join(v)
    }

    fn binary(&self, v: &str) -> PathBuf {
        self.version_dir(v).join("bin").join("superfluid")
    }

    pub fn current(&self) -> Option<String> {
        let target = std::fs::read_link(self.dir.join("current")).ok()?;
        target.file_name().and_then(|n| n.to_str()).map(str::to_string)
    }

    pub fn previous(&self) -> Option<String> {
        let v = std::fs::read_to_string(self.dir.join("previous")).ok()?.trim().to_string();
        (!v.is_empty() && self.binary(&v).is_file()).then_some(v)
    }

    /// The complete versions here, newest first.
    pub fn on_disk(&self) -> Vec<String> {
        let mut all: Vec<String> = std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().to_str().map(str::to_string))
                    .filter(|n| !n.starts_with('.') && n != "current" && n.chars().next().is_some_and(|c| c.is_ascii_digit()))
                    .filter(|n| self.binary(n).is_file())
                    .collect()
            })
            .unwrap_or_default();
        all.sort_by_key(|v| std::cmp::Reverse(version_key(v)));
        all
    }

    pub fn prefix(&self) -> Option<PathBuf> {
        let p = std::fs::read_to_string(self.dir.join("prefix")).ok()?.trim().to_string();
        (!p.is_empty()).then(|| PathBuf::from(p))
    }

    /// Point `current` at `v`, atomically, remembering what it pointed at.
    pub fn set_current(&self, v: &str) -> Result<(), String> {
        if !self.binary(v).is_file() {
            return Err(format!("{} is not an installed version (see `superfluid self list`)", v));
        }
        if let Some(was) = self.current().filter(|was| was != v) {
            std::fs::write(self.dir.join("previous"), format!("{was}\n")).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        }
        let tmp = self.dir.join(".current.tmp");
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(v, &tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, self.dir.join("current")).map_err(|e| format!("{}: {e}", self.dir.join("current").display()))
    }

    /// `current`, `previous`, a version with a `keep` file, and one a running
    /// process started from; everything else is removed.
    pub fn prune(&self) -> Result<Vec<String>, String> {
        let (current, previous) = (self.current(), self.previous());
        let mut gone = Vec::new();
        for v in self.on_disk() {
            let dir = self.version_dir(&v);
            if Some(&v) == current.as_ref() || Some(&v) == previous.as_ref() || dir.join("keep").exists() || running_from(&dir) {
                continue;
            }
            std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            gone.push(v);
        }
        Ok(gone)
    }

    /// The links in `prefix`: each of superfluid, superfluid-workerd and
    /// superfluid-noded through `current`, and `superfluid-<v>` per version
    /// here. A file that is not a link into this directory is replaced only
    /// when `replace_files` (the in-place installer's copies, on migration).
    pub fn link(&self, prefix: &Path, replace_files: bool) -> Result<Vec<String>, String> {
        std::fs::create_dir_all(prefix).map_err(|e| format!("{}: {e}", prefix.display()))?;
        std::fs::write(self.dir.join("prefix"), format!("{}\n", prefix.display())).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        let mut notes = Vec::new();
        let ours = |link: &Path| std::fs::read_link(link).map(|t| t.starts_with(&self.dir)).unwrap_or(false);
        let relink = |link: &Path, target: &Path| -> Result<(), String> {
            let tmp = link.with_file_name(format!(".{}.tmp", link.file_name().and_then(|n| n.to_str()).unwrap_or("link")));
            let _ = std::fs::remove_file(&tmp);
            std::os::unix::fs::symlink(target, &tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
            std::fs::rename(&tmp, link).map_err(|e| format!("{}: {e}", link.display()))
        };
        for name in LINKED {
            let link = prefix.join(name);
            let target = self.dir.join("current").join("bin").join(name);
            match link.symlink_metadata() {
                Ok(m) if m.file_type().is_symlink() && ours(&link) => continue,
                Ok(m) if m.file_type().is_symlink() || replace_files => relink(&link, &target)?,
                Ok(_) => {
                    notes.push(format!("{} is not a link into {}; left as it is", link.display(), self.dir.display()));
                    continue;
                }
                Err(_) => relink(&link, &target)?,
            }
        }
        let here = self.on_disk();
        for v in &here {
            let link = prefix.join(format!("superfluid-{v}"));
            if !ours(&link) {
                relink(&link, &self.binary(v))?;
            }
        }
        if let Ok(rd) = std::fs::read_dir(prefix) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(v) = name.strip_prefix("superfluid-") {
                    if v.chars().next().is_some_and(|c| c.is_ascii_digit()) && !here.iter().any(|h| h == v) && ours(&e.path()) {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        }
        Ok(notes)
    }
}

/// Where the running executable came from, which decides who updates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    /// `<home>/versions/<v>/bin/superfluid`: ours to update.
    Managed { versions: PathBuf, version: String },
    /// The install script's layout before versions existed: real files in
    /// `<prefix>/bin` with the workers in `<prefix>/libexec/superfluid`.
    InPlace { prefix: PathBuf },
    /// A package manager, `cargo install`, or a source build.
    Other(PathBuf),
}

pub fn installed_as(exe: &Path) -> Installed {
    let exe = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let parts: Vec<&std::ffi::OsStr> = exe.iter().collect();
    let n = parts.len();
    if n >= 4 && parts[n - 2] == "bin" && parts[n - 4] == "versions" {
        let versions = exe.ancestors().nth(3).map(Path::to_path_buf).unwrap_or_default();
        return Installed::Managed { versions, version: parts[n - 3].to_string_lossy().to_string() };
    }
    if let Some(prefix) = exe.parent().filter(|b| b.file_name().is_some_and(|n| n == "bin")).and_then(Path::parent) {
        if prefix.join("libexec").join("superfluid").is_dir() {
            return Installed::InPlace { prefix: prefix.to_path_buf() };
        }
    }
    Installed::Other(exe)
}

fn not_ours(exe: &Path) -> String {
    let p = exe.to_string_lossy();
    let how = if p.contains("/Cellar/") || p.contains("/homebrew/") || p.contains("/linuxbrew/") {
        "Homebrew installed it: `brew upgrade superfluid`"
    } else if p.contains("/.cargo/bin/") {
        "cargo installed it: `cargo install superfluid`"
    } else if p.contains("/target/") {
        "it is a source build: build it again"
    } else {
        "update it the way it was installed"
    };
    format!("{p} was not installed by the install script or by `superfluid self update`; {how}")
}

// ----- releases -----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    /// YYYY-MM-DD
    pub published: String,
    pub tarball: Asset,
    pub sha256_url: String,
    /// The basert engine ABI the release declares in an asset name.
    pub abi: Option<u32>,
    pub compat_url: Option<String>,
}

/// The releases of the listing that carry a tarball for `target`, newest
/// first; drafts and prereleases are left out.
pub fn releases(listing: &Value, target: &str) -> Vec<Release> {
    let tar = format!("superfluid-{target}.tar.gz");
    let mut out: Vec<Release> = listing
        .as_array()
        .map(|a| a.iter().filter_map(|r| release_of(r, &tar, target)).collect())
        .unwrap_or_default();
    out.sort_by_key(|r| std::cmp::Reverse(version_key(&r.version)));
    out
}

fn release_of(r: &Value, tar: &str, target: &str) -> Option<Release> {
    if r["draft"].as_bool().unwrap_or(false) || r["prerelease"].as_bool().unwrap_or(false) {
        return None;
    }
    let version = r["tag_name"].as_str()?.trim_start_matches('v').to_string();
    let assets = r["assets"].as_array()?;
    let url = |name: &str| assets.iter().find(|a| a["name"] == name).and_then(|a| a["browser_download_url"].as_str()).map(str::to_string);
    let size = assets.iter().find(|a| a["name"] == tar).and_then(|a| a["size"].as_u64());
    let abi = assets.iter().find_map(|a| a["name"].as_str()?.strip_prefix("superfluid-basert-abi-")?.strip_suffix(".txt")?.parse().ok());
    Some(Release {
        published: r["published_at"].as_str().unwrap_or_default().chars().take(10).collect(),
        tarball: Asset { name: tar.to_string(), url: url(tar)?, sha256: None, size },
        sha256_url: url(&format!("{tar}.sha256"))?,
        abi,
        compat_url: url(&format!("superfluid-{target}.compat.json")),
        version,
    })
}

/// What a release must declare to drive the baseRT engine here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Engine {
    /// No engine, or none this superfluid can tell: the newest release fits.
    None,
    Abi(u32),
    /// A 0.3.x engine, which states no number: the release must accept that line.
    Unnumbered(String),
}

pub fn engine_here(catalog: &Catalog) -> Engine {
    if std::env::var_os("BASERT_LIB").is_none() && !catalog.package_dir(RuntimeId::new("basert")).is_dir() {
        return Engine::None;
    }
    match catalog.inspect(RuntimeId::new("basert")).usable {
        Ok(rt) => match (rt.abi, rt.version.as_deref()) {
            (Some(n), _) => Engine::Abi(n as u32),
            (None, Some(v)) => Engine::Unnumbered(v.split(|c: char| !c.is_ascii_digit() && c != '.').next().unwrap_or("").split('.').take(2).collect::<Vec<_>>().join(".")),
            (None, None) => Engine::None,
        },
        Err(_) => Engine::None,
    }
}

impl Engine {
    pub fn describe(&self) -> String {
        match self {
            Engine::None => "no baseRT engine here".into(),
            Engine::Abi(n) => format!("the baseRT engine here states ABI {n}"),
            Engine::Unnumbered(minor) => format!("the baseRT engine here is {minor}.x and states no ABI"),
        }
    }
}

/// Whether `release` drives the engine; `compat` fetches its compat.json
/// when the asset names alone do not say.
pub fn fits(release: &Release, engine: &Engine, compat: &dyn Fn(&str) -> Option<Value>) -> bool {
    match engine {
        Engine::None => true,
        Engine::Abi(n) => match release.abi {
            Some(have) => have == *n,
            None => release.compat_url.as_deref().and_then(compat).and_then(|c| c["runtimes"]["basert"]["abi"].as_u64()) == Some(u64::from(*n)),
        },
        Engine::Unnumbered(minor) => release
            .compat_url
            .as_deref()
            .and_then(compat)
            .and_then(|c| c["runtimes"]["basert"]["unnumbered_minor"].as_str().map(str::to_string))
            .is_some_and(|m| m == *minor),
    }
}

/// The newest release that fits, skipping ones published fewer than
/// `hold_off` days ago when given.
pub fn newest<'a>(releases: &'a [Release], engine: &Engine, hold_off: Option<i64>, compat: &dyn Fn(&str) -> Option<Value>) -> Option<&'a Release> {
    releases.iter().find(|r| hold_off.is_none_or(|days| age_days(&r.published).is_some_and(|age| age >= days)) && fits(r, engine, compat))
}

/// The release listing to read: `SUPERFLUID_RELEASES`, else GitHub's.
pub fn listing_source() -> String {
    std::env::var(LISTING_ENV).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| LISTING.to_string())
}

/// A small text or JSON document: a file, or an https URL through curl.
fn get_text(url: &str, max_time: u32) -> Result<String, String> {
    if let Some(path) = url.strip_prefix("file://").or_else(|| (!url.contains("://")).then_some(url)) {
        return std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"));
    }
    if !url.starts_with("https://") {
        return Err(format!("{url}: only https:// URLs are fetched"));
    }
    let out = Command::new("curl")
        .args(["-fsSL", "--proto", "=https", "--tlsv1.2", "--max-time", &max_time.to_string(), "-H", "Accept: application/vnd.github+json"])
        .arg(url)
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(format!("fetching {url} failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn get_json(url: &str, max_time: u32) -> Result<Value, String> {
    serde_json::from_str(&get_text(url, max_time)?).map_err(|e| format!("{url}: {e}"))
}

pub fn fetch_releases(source: &str, target: &str, max_time: u32) -> Result<Vec<Release>, String> {
    let listing = get_json(source, max_time)?;
    let found = releases(&listing, target);
    if found.is_empty() {
        return Err(format!("{source} lists no release with a superfluid-{target}.tar.gz"));
    }
    Ok(found)
}

fn compat_fetcher(max_time: u32) -> impl Fn(&str) -> Option<Value> {
    move |url: &str| get_json(url, max_time).ok()
}

// ----- time ---------------------------------------------------------------

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn age_days(published: &str) -> Option<i64> {
    let mut parts = published.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (parts.next()??, parts.next()??, parts.next()??);
    let today = (SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() / 86_400) as i64;
    Some(today - days_from_civil(y, m, d))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ----- processes ----------------------------------------------------------

/// The executables of the other processes on this machine, as real paths.
#[cfg(target_os = "linux")]
fn other_executables() -> Vec<PathBuf> {
    let me = std::process::id().to_string();
    std::fs::read_dir("/proc")
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_name().to_str().is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()) && n != me))
                .filter_map(|e| std::fs::read_link(e.path().join("exe")).ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn other_executables() -> Vec<PathBuf> {
    let me = std::process::id() as libc::c_int;
    // SAFETY: with a null buffer, proc_listallpids only reports how many pids there are.
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0 as libc::c_int; n as usize * 2 + 16];
    let bytes = (pids.len() * std::mem::size_of::<libc::c_int>()) as libc::c_int;
    // SAFETY: the buffer is this function's, and its size in bytes is what is passed.
    let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr() as *mut libc::c_void, bytes) };
    pids.truncate(got.max(0) as usize);
    pids.into_iter()
        .filter(|&p| p > 0 && p != me)
        .filter_map(|p| {
            let mut buf = vec![0u8; 4096];
            // SAFETY: proc_pidpath writes at most the buffer's length, which it is told.
            let len = unsafe { libc::proc_pidpath(p, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
            (len > 0).then(|| PathBuf::from(String::from_utf8_lossy(&buf[..len as usize]).to_string()))
        })
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn other_executables() -> Vec<PathBuf> {
    Vec::new()
}

/// Whether another process runs an executable under `dir`.
pub fn running_from(dir: &Path) -> bool {
    let Ok(dir) = std::fs::canonicalize(dir) else { return false };
    other_executables().iter().any(|exe| exe.starts_with(&dir))
}

// ----- the update ---------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct Options {
    pub version: Option<String>,
    pub dry_run: bool,
    /// The release listing; `listing_source()` when not set.
    pub source: Option<String>,
}

/// What `self update` did, line by line.
#[derive(Debug, Default)]
pub struct Report {
    pub lines: Vec<String>,
    pub changed: bool,
}

impl Report {
    fn say(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }
}

fn version_of(binary: &Path) -> Result<String, String> {
    let out = Command::new(binary).arg("--version").output().map_err(|e| format!("{}: {e}", binary.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().nth(1).map(str::to_string).filter(|_| out.status.success()).ok_or_else(|| {
        format!("{} did not say its version ({}): {}", binary.display(), out.status, String::from_utf8_lossy(&out.stderr).trim())
    })
}

/// Proof that a version starts here and, when an engine is in use, drives
/// it: its `--version` and its `runtimes --json`, the probe a host runs.
fn probe(versions: &Versions, v: &str, home: &Path, engine: &Engine) -> Result<(), String> {
    let binary = versions.binary(v);
    let said = version_of(&binary)?;
    if said != v {
        return Err(format!("{} says it is {said}, not {v}", binary.display()));
    }
    let out = Command::new(&binary)
        .args(["runtimes", "--json"])
        .env("SUPERFLUID_HOME", home)
        .env(NO_CHECK_ENV, "1")
        .output()
        .map_err(|e| format!("{}: {e}", binary.display()))?;
    if !out.status.success() {
        return Err(format!("{} runtimes --json failed ({}): {}", binary.display(), out.status, String::from_utf8_lossy(&out.stderr).trim()));
    }
    if *engine != Engine::None {
        let rows: Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("{} runtimes --json: {e}", binary.display()))?;
        let basert = rows.as_array().and_then(|a| a.iter().find(|r| r["id"] == "basert"));
        match basert.map(|r| &r["ready"]) {
            Some(Value::Bool(true)) => {}
            Some(Value::String(why)) => return Err(format!("{v} cannot drive the engine here: {why}")),
            _ => return Err(format!("{v} reports no basert runtime, which this machine serves with")),
        }
    }
    Ok(())
}

fn move_tree(from: &Path, to: &Path) -> Result<(), String> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_tree(from, to)?;
    std::fs::remove_dir_all(from).map_err(|e| format!("{}: {e}", from.display()))
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    for e in std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?.flatten() {
        let (src, dest) = (e.path(), to.join(e.file_name()));
        let kind = e.file_type().map_err(|e| format!("{}: {e}", src.display()))?;
        if kind.is_dir() {
            copy_tree(&src, &dest)?;
        } else if kind.is_symlink() {
            let target = std::fs::read_link(&src).map_err(|e| format!("{}: {e}", src.display()))?;
            std::os::unix::fs::symlink(target, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
        } else {
            std::fs::copy(&src, &dest).map_err(|e| format!("{} -> {}: {e}", src.display(), dest.display()))?;
        }
    }
    Ok(())
}

/// Download, check and unpack `release` into the layout; nothing is placed
/// until the whole tree is there and verified.
fn place(versions: &Versions, release: &Release, report: &mut Report) -> Result<(), String> {
    let v = &release.version;
    let staging = versions.dir.join(".staging").join(v);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    let want = get_text(&release.sha256_url, 30)?.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    if want.len() != 64 || !want.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{}: not a SHA-256", release.sha256_url));
    }
    let asset = Asset { sha256: Some(want), ..release.tarball.clone() };
    report.say(format!("downloading {} ({})", asset.name, asset.size.map(|s| format!("{:.1} MB", s as f64 / 1e6)).unwrap_or_else(|| "size unknown".into())));
    let (archive, _) = fetch(&asset, &staging)?;
    let unpacked = staging.join("unpacked");
    unpack(&archive, &unpacked)?;
    let top = single_top(&unpacked)?;
    for needed in ["bin/superfluid", "bin/superfluid-workerd"] {
        if !top.join(needed).is_file() {
            return Err(format!("{} holds no {needed}", asset.name));
        }
    }
    let dest = versions.version_dir(v);
    let _ = std::fs::remove_dir_all(&dest);
    move_tree(&top, &dest)?;
    let _ = std::fs::remove_dir_all(&staging);
    Ok(())
}

/// Replace the install script's in-place copies with links into the
/// layout, and remove the workers they came with.
fn migrate(versions: &Versions, prefix: &Path, report: &mut Report) -> Result<(), String> {
    for note in versions.link(&prefix.join("bin"), true)? {
        report.say(note);
    }
    let libexec = prefix.join("libexec").join("superfluid");
    if let Ok(rd) = std::fs::read_dir(&libexec) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("superfluid-worker-") || name.starts_with("libsuperfluid_tokenizer_") {
                let _ = std::fs::remove_file(e.path());
            }
        }
        let _ = std::fs::remove_dir(&libexec);
    }
    report.say(format!(
        "{}/bin/superfluid now links into {}; the copy the install script placed there was replaced, and its workers in {} removed",
        prefix.display(),
        versions.dir.display(),
        libexec.display()
    ));
    Ok(())
}

/// `self update`: the newest release that fits, or the one named.
pub fn update(home: &Path, catalog: &Catalog, exe: &Path, opts: &Options) -> Result<Report, String> {
    let mut report = Report::default();
    let installed = installed_as(exe);
    let (versions, migrating) = match &installed {
        Installed::Managed { versions, .. } => (Versions { dir: versions.clone() }, None),
        Installed::InPlace { prefix } => (Versions::of(home), Some(prefix.clone())),
        Installed::Other(p) => return Err(not_ours(p)),
    };
    let target = target().ok_or_else(|| format!("no release is published for {} {}", std::env::consts::OS, std::env::consts::ARCH))?;
    let engine = engine_here(catalog);
    let compat = compat_fetcher(15);
    let found = fetch_releases(opts.source.as_deref().unwrap_or(&listing_source()), target, 30)?;
    let chosen = match &opts.version {
        Some(want) => {
            let want = want.trim_start_matches('v');
            let r = found.iter().find(|r| r.version == want).ok_or_else(|| {
                format!("no release {want} for {target} (the newest is {})", found.first().map(|r| r.version.as_str()).unwrap_or("none"))
            })?;
            if !fits(r, &engine, &compat) {
                return Err(format!("{want} does not declare support for the engine here ({}); `superfluid runtimes --json` shows what this build requires", engine.describe()));
            }
            r
        }
        None => newest(&found, &engine, None, &compat).ok_or_else(|| {
            format!("no release fits: {}, and the newest release for {target} is {}", engine.describe(), found.first().map(|r| r.version.as_str()).unwrap_or("none"))
        })?,
    };
    let v = chosen.version.clone();
    let current = versions.current();
    if migrating.is_none() && current.as_deref() == Some(v.as_str()) {
        report.say(format!("superfluid {v} is current, and the newest release that fits ({})", engine.describe()));
        return Ok(report);
    }
    // Without a version named, an update only moves forward: a source build
    // or a prerelease newer than every release stays as it is.
    let here = current.clone().unwrap_or_else(|| running().to_string());
    if opts.version.is_none() && migrating.is_none() && version_key(&v) < version_key(&here) {
        report.say(format!("superfluid {here} is current; the newest release that fits is {v} ({}), and `--version {v}` goes to it", engine.describe()));
        return Ok(report);
    }
    let on_disk = versions.binary(&v).is_file();
    report.say(format!(
        "{} -> {v} (published {}; {}){}",
        current.as_deref().unwrap_or(running()),
        chosen.published,
        engine.describe(),
        if on_disk { ", already installed here" } else { "" }
    ));
    if opts.dry_run {
        report.say("dry run: nothing fetched or changed");
        return Ok(report);
    }
    let _one_at_a_time = crate::pulls::Lock::take(&versions.dir, "self", "update of superfluid")?;
    if !on_disk {
        place(&versions, chosen, &mut report)?;
    }
    if let Err(e) = probe(&versions, &v, home, &engine) {
        if !on_disk {
            let _ = std::fs::remove_dir_all(versions.version_dir(&v));
        }
        return Err(format!("{v} was not made current: {e}"));
    }
    versions.set_current(&v)?;
    report.changed = true;
    report.say(format!("superfluid {v} is current ({})", versions.dir.join("current").display()));
    if let Some(prefix) = &migrating {
        migrate(&versions, prefix, &mut report)?;
    } else if let Some(prefix) = versions.prefix() {
        for note in versions.link(&prefix, false)? {
            report.say(note);
        }
    }
    if let Some(prev) = versions.previous() {
        report.say(format!("{prev} stays installed: `superfluid self use {prev}` goes back to it"));
    }
    let gone = versions.prune()?;
    if !gone.is_empty() {
        report.say(format!("removed {}", gone.join(", ")));
    }
    report.say("a running server keeps the version it started with; starting it again uses the new one");
    Ok(report)
}

/// `self use <v>`.
pub fn use_version(home: &Path, catalog: &Catalog, exe: &Path, v: &str) -> Result<Report, String> {
    let versions = match installed_as(exe) {
        Installed::Managed { versions, .. } => Versions { dir: versions },
        _ => Versions::of(home),
    };
    let v = v.trim_start_matches('v');
    let mut report = Report::default();
    if versions.current().as_deref() == Some(v) {
        report.say(format!("superfluid {v} is current already"));
        return Ok(report);
    }
    probe(&versions, v, home, &engine_here(catalog))?;
    versions.set_current(v)?;
    report.changed = true;
    report.say(format!("superfluid {v} is current"));
    if let Some(prefix) = versions.prefix() {
        for note in versions.link(&prefix, false)? {
            report.say(note);
        }
    }
    Ok(report)
}

/// `self install --from <dir> [--prefix <dir>]`: an unpacked release placed
/// into the layout and made current; the install script's last step.
pub fn install_from(home: &Path, catalog: &Catalog, from: &Path, prefix: Option<&Path>) -> Result<Report, String> {
    let versions = Versions::of(home);
    let mut report = Report::default();
    let v = version_of(&from.join("bin").join("superfluid"))?;
    std::fs::create_dir_all(&versions.dir).map_err(|e| format!("{}: {e}", versions.dir.display()))?;
    let _one_at_a_time = crate::pulls::Lock::take(&versions.dir, "self", "install of superfluid")?;
    let dest = versions.version_dir(&v);
    if dest != from {
        let _ = std::fs::remove_dir_all(&dest);
        move_tree(from, &dest)?;
    }
    let engine = engine_here(catalog);
    probe(&versions, &v, home, &engine).map_err(|e| format!("{v} was not made current: {e}"))?;
    versions.set_current(&v)?;
    report.changed = true;
    report.say(format!("superfluid {v} is current ({})", versions.dir.join("current").display()));
    if let Some(prefix) = prefix.map(Path::to_path_buf).or_else(|| versions.prefix()) {
        for note in versions.link(&prefix, true)? {
            report.say(note);
        }
        report.say(format!("{} links to it", prefix.join("superfluid").display()));
    }
    let gone = versions.prune()?;
    if !gone.is_empty() {
        report.say(format!("removed {}", gone.join(", ")));
    }
    Ok(report)
}

/// `self list`.
pub fn list(home: &Path, exe: &Path) -> String {
    let (versions, note) = match installed_as(exe) {
        Installed::Managed { versions, .. } => (Versions { dir: versions }, None),
        Installed::InPlace { prefix } => (Versions::of(home), Some(format!("this superfluid is the install script's copy in {}/bin; `superfluid self update` moves it under {}", prefix.display(), home.join("versions").display()))),
        Installed::Other(p) => (Versions::of(home), Some(not_ours(&p))),
    };
    let mut out = String::new();
    let here = versions.on_disk();
    if here.is_empty() {
        out.push_str(&format!("no versions in {}\n", versions.dir.display()));
    } else {
        out.push_str(&format!("versions in {}:\n", versions.dir.display()));
        let (current, previous) = (versions.current(), versions.previous());
        for v in &here {
            let mut marks = Vec::new();
            if Some(v) == current.as_ref() {
                marks.push("current".to_string());
            }
            if Some(v) == previous.as_ref() {
                marks.push(format!("previous (`superfluid self use {v}` goes back)"));
            }
            if versions.version_dir(v).join("keep").exists() {
                marks.push("kept (a `keep` file)".into());
            }
            out.push_str(&format!("  {v}  {}\n", marks.join(", ")));
        }
    }
    out.push_str(&format!("running: {}\n", running()));
    if let Some(cache) = Check::read(home) {
        out.push_str(&format!("{}\n", cache.describe()));
    }
    if let Some(n) = note {
        out.push_str(&format!("{n}\n"));
    }
    out
}

// ----- the daily check ----------------------------------------------------

/// `$SUPERFLUID_HOME/update-check.json`: when the listing was last read,
/// the newest release that fit then, and which version was announced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Check {
    pub checked_at: u64,
    pub newest: Option<String>,
    pub published: Option<String>,
    pub notified: Option<String>,
}

impl Check {
    fn path(home: &Path) -> PathBuf {
        home.join("update-check.json")
    }

    pub fn read(home: &Path) -> Option<Check> {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(Self::path(home)).ok()?).ok()?;
        Some(Check {
            checked_at: v["checked_at"].as_u64().unwrap_or(0),
            newest: v["newest"].as_str().map(str::to_string),
            published: v["published"].as_str().map(str::to_string),
            notified: v["notified"].as_str().map(str::to_string),
        })
    }

    fn write(&self, home: &Path) -> Result<(), String> {
        std::fs::create_dir_all(home).map_err(|e| format!("{}: {e}", home.display()))?;
        let v = json!({"checked_at": self.checked_at, "newest": self.newest, "published": self.published, "notified": self.notified});
        std::fs::write(Self::path(home), format!("{v:#}\n")).map_err(|e| format!("{}: {e}", Self::path(home).display()))
    }

    pub fn due(&self) -> bool {
        now_secs().saturating_sub(self.checked_at) >= CHECK_EVERY.as_secs()
    }

    /// The newest release known, when it is newer than this one.
    pub fn newer(&self) -> Option<&str> {
        self.newest.as_deref().filter(|n| version_key(n) > version_key(running()))
    }

    pub fn describe(&self) -> String {
        let ago = now_secs().saturating_sub(self.checked_at);
        let when = if ago < 3600 { format!("{}m ago", ago / 60) } else if ago < 86_400 { format!("{}h ago", ago / 3600) } else { format!("{}d ago", ago / 86_400) };
        match self.newer() {
            Some(n) => format!("newest release: {n} (published {}; checked {when}): `superfluid self update` installs it", self.published.as_deref().unwrap_or("?")),
            None => format!("newest release that fits: {} (checked {when})", self.newest.as_deref().unwrap_or("none known")),
        }
    }
}

/// Read the listing now and record the newest release that fits here and
/// has been out for the hold-off; `announce` says what to print.
pub fn check_now(home: &Path, catalog: &Catalog, hold_off: Option<i64>, source: &str) -> Result<Check, String> {
    let target = target().ok_or("no release is published for this platform")?;
    let found = fetch_releases(source, target, 5)?;
    let engine = engine_here(catalog);
    let compat = compat_fetcher(5);
    let fit = newest(&found, &engine, hold_off, &compat);
    let previous = Check::read(home).unwrap_or_default();
    let check = Check {
        checked_at: now_secs(),
        newest: fit.map(|r| r.version.clone()),
        published: fit.map(|r| r.published.clone()),
        notified: previous.notified,
    };
    check.write(home)?;
    Ok(check)
}

/// The one line `serve` prints when a newer release is out, once per release.
fn announce(home: &Path, mut check: Check) -> Option<String> {
    let newer = check.newer()?.to_string();
    if check.notified.as_deref() == Some(newer.as_str()) {
        return None;
    }
    check.notified = Some(newer.clone());
    let _ = check.write(home);
    Some(format!("superfluid {newer} is available (this is {}): `superfluid self update` installs it; {NO_CHECK_ENV}=1 silences this", running()))
}

/// What `serve` runs after the server is up: at most one listing read a
/// day, never when offline or told not to, and one line per new release.
pub fn check_in_background(home: PathBuf, offline: bool) {
    if offline || std::env::var_os(NO_CHECK_ENV).is_some_and(|v| !v.is_empty() && v != "0") {
        return;
    }
    std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            let line = match Check::read(&home).filter(|c| !c.due()) {
                Some(cached) => announce(&home, cached),
                None => match check_now(&home, &Catalog::from_env(), Some(HOLD_OFF_DAYS), &listing_source()) {
                    Ok(fresh) => announce(&home, fresh),
                    Err(_) => None,
                },
            };
            if let Some(line) = line {
                eprintln!("superfluid: {line}");
            }
        })
        .ok();
}

// ----- the command --------------------------------------------------------

fn usage() -> ! {
    eprint!("{HELP}");
    std::process::exit(2);
}

/// `superfluid self <action> ...`; the exit code.
pub fn run(args: &[String]) -> i32 {
    let home = crate::runtimes::default_home();
    let exe = std::env::current_exe().unwrap_or_default();
    let catalog = Catalog::from_env();
    let args = crate::cli::split_equals(args, |f| Some(matches!(f, "--version" | "--from" | "--prefix")));
    let print = |r: Result<Report, String>| -> i32 {
        match r {
            Ok(report) => {
                for line in report.lines {
                    println!("superfluid: {line}");
                }
                0
            }
            Err(e) => {
                eprintln!("superfluid: {e}");
                1
            }
        }
    };
    match args.first().map(String::as_str) {
        Some("update") => {
            let mut opts = Options::default();
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--version" => opts.version = it.next().cloned().or_else(|| usage()),
                    "--dry-run" => opts.dry_run = true,
                    _ => usage(),
                }
            }
            print(update(&home, &catalog, &exe, &opts))
        }
        Some("check") if args.len() == 1 => match check_now(&home, &catalog, Some(HOLD_OFF_DAYS), &listing_source()) {
            Ok(check) => {
                println!("superfluid: {}", check.describe());
                0
            }
            Err(e) => {
                eprintln!("superfluid: {e}");
                1
            }
        },
        Some("list") if args.len() == 1 => {
            let _ = std::io::stdout().write_all(list(&home, &exe).as_bytes());
            0
        }
        Some("use") if args.len() == 2 => print(use_version(&home, &catalog, &exe, &args[1])),
        Some("install") => {
            let (mut from, mut prefix) = (None, None);
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--from" => from = it.next().map(PathBuf::from).or_else(|| usage()),
                    "--prefix" => prefix = it.next().map(PathBuf::from).or_else(|| usage()),
                    _ => usage(),
                }
            }
            let Some(from) = from else { usage() };
            print(install_from(&home, &catalog, &from, prefix.as_deref()))
        }
        _ => usage(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use superfluid_adapter_kit::install::sha256_of;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("superfluid-self-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A release tree whose `superfluid` is a script that answers
    /// `--version` and `runtimes --json` the way a real one would.
    fn release_tree(dir: &Path, v: &str) -> PathBuf {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(dir.join("libexec").join("superfluid")).unwrap();
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  --version) echo \"superfluid {v}\";;\n  runtimes) echo '[{{\"id\":\"basert\",\"ready\":true}}]';;\n  *) exit 2;;\nesac\n"
        );
        for name in LINKED {
            let p = bin.join(name);
            std::fs::write(&p, &script).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(dir.join("libexec").join("superfluid").join("superfluid-worker-toy"), "#!/bin/sh\n").unwrap();
        dir.to_path_buf()
    }

    /// A listing with one release per version, its assets as local files.
    fn listing(dir: &Path, versions: &[(&str, &str, Option<u32>)]) -> PathBuf {
        let target = target().unwrap();
        let mut rs = Vec::new();
        for (v, published, abi) in versions {
            let name = format!("superfluid-{target}");
            let tree = dir.join(format!("tree-{v}")).join(&name);
            release_tree(&tree, v);
            let tar = dir.join(format!("{name}-{v}.tar.gz"));
            let st = Command::new("tar").args(["-C"]).arg(tree.parent().unwrap()).arg("-czf").arg(&tar).arg(&name).status().unwrap();
            assert!(st.success());
            let sum = sha256_of(&tar).unwrap();
            let sha = dir.join(format!("{name}-{v}.tar.gz.sha256"));
            std::fs::write(&sha, format!("{sum}  {name}.tar.gz\n")).unwrap();
            let compat = dir.join(format!("{name}-{v}.compat.json"));
            std::fs::write(&compat, json!({"runtimes": {"basert": {"abi": abi, "unnumbered_minor": abi.map(|_| "0.3")}}}).to_string()).unwrap();
            let mut assets = vec![
                json!({"name": format!("{name}.tar.gz"), "browser_download_url": tar, "size": std::fs::metadata(&tar).unwrap().len()}),
                json!({"name": format!("{name}.tar.gz.sha256"), "browser_download_url": sha}),
                json!({"name": format!("{name}.compat.json"), "browser_download_url": compat}),
            ];
            if let Some(n) = abi {
                assets.push(json!({"name": format!("superfluid-basert-abi-{n}.txt"), "browser_download_url": "x"}));
            }
            rs.push(json!({"tag_name": format!("v{v}"), "draft": false, "prerelease": false, "published_at": format!("{published}T12:00:00Z"), "assets": assets}));
        }
        rs.push(json!({"tag_name": "v9.9.9", "draft": true, "prerelease": false, "published_at": "2026-01-01T00:00:00Z", "assets": []}));
        let p = dir.join("releases.json");
        std::fs::write(&p, Value::Array(rs).to_string()).unwrap();
        p
    }

    fn no_compat(_: &str) -> Option<Value> {
        None
    }

    #[test]
    fn a_listing_is_read_newest_first_and_a_release_fits_by_the_engine_it_declares() {
        let dir = scratch("listing");
        let p = listing(&dir, &[("0.1.3", "2026-10-01", Some(1)), ("0.2.0", "2026-10-20", Some(2)), ("0.1.2", "2026-09-01", None)]);
        let found = releases(&serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap(), target().unwrap());
        assert_eq!(found.iter().map(|r| r.version.as_str()).collect::<Vec<_>>(), ["0.2.0", "0.1.3", "0.1.2"], "newest first, the draft left out");
        assert_eq!((found[0].abi, found[1].abi, found[2].abi), (Some(2), Some(1), None));
        assert_eq!(found[1].published, "2026-10-01");
        let compat = |url: &str| get_json(url, 1).ok();
        assert_eq!(newest(&found, &Engine::None, None, &no_compat).map(|r| r.version.as_str()), Some("0.2.0"));
        assert_eq!(newest(&found, &Engine::Abi(1), None, &no_compat).map(|r| r.version.as_str()), Some("0.1.3"), "by the marker alone");
        assert_eq!(newest(&found, &Engine::Abi(3), None, &compat), None);
        assert_eq!(newest(&found, &Engine::Unnumbered("0.3".into()), None, &compat).map(|r| r.version.as_str()), Some("0.2.0"), "compat.json names the line");
        assert_eq!(newest(&found, &Engine::Unnumbered("0.2".into()), None, &compat), None);
        // The hold-off keeps a fresh release out of the daily notice.
        assert_eq!(newest(&found, &Engine::None, Some(100_000), &no_compat), None);
        assert_eq!(age_days("2000-01-01").map(|d| d > 9000), Some(true));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn where_an_executable_came_from_decides_who_updates_it() {
        let dir = scratch("installed");
        let managed = dir.join("home").join("versions").join("0.1.3").join("bin").join("superfluid");
        std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
        std::fs::write(&managed, "").unwrap();
        assert_eq!(installed_as(&managed), Installed::Managed { versions: std::fs::canonicalize(dir.join("home").join("versions")).unwrap(), version: "0.1.3".into() });
        let in_place = dir.join("local").join("bin").join("superfluid");
        std::fs::create_dir_all(in_place.parent().unwrap()).unwrap();
        std::fs::create_dir_all(dir.join("local").join("libexec").join("superfluid")).unwrap();
        std::fs::write(&in_place, "").unwrap();
        assert_eq!(installed_as(&in_place), Installed::InPlace { prefix: std::fs::canonicalize(dir.join("local")).unwrap() });
        let other = dir.join("Cellar").join("superfluid").join("0.1.3").join("bin").join("superfluid");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, "").unwrap();
        assert!(matches!(installed_as(&other), Installed::Other(_)));
        assert!(not_ours(&other).contains("brew upgrade superfluid"));
        assert!(running_from(std::env::current_exe().unwrap().parent().unwrap()) || true, "other processes may or may not run from here");
        assert!(!running_from(&dir.join("nowhere")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_update_places_the_release_beside_the_old_one_flips_current_and_keeps_one_back() {
        let dir = scratch("update");
        let home = dir.join("home");
        let prefix = dir.join("local");
        let p = listing(&dir, &[("0.1.3", "2026-10-01", Some(1)), ("0.1.4", "2026-10-10", Some(1)), ("0.1.5", "2026-10-12", Some(1))]);
        let from = |version: Option<&str>, dry_run: bool| Options { version: version.map(str::to_string), dry_run, source: Some(p.to_string_lossy().to_string()) };
        let catalog = Catalog::new(home.clone(), None, home.join("superfluid-workerd"));
        // The install script's last step: the unpacked 0.1.3 becomes the layout.
        let unpacked = release_tree(&dir.join("unpacked").join("superfluid-x"), "0.1.3");
        let r = install_from(&home, &catalog, &unpacked, Some(&prefix.join("bin"))).unwrap();
        assert!(r.changed, "{:?}", r.lines);
        let versions = Versions::of(&home);
        assert_eq!(versions.current().as_deref(), Some("0.1.3"));
        assert_eq!(std::fs::read_link(prefix.join("bin").join("superfluid")).unwrap(), versions.dir.join("current").join("bin").join("superfluid"));
        assert!(prefix.join("bin").join("superfluid-0.1.3").exists() && prefix.join("bin").join("superfluid-workerd").exists());
        assert!(!unpacked.exists(), "moved, not copied");
        // A dry run names the newest release and changes nothing.
        let exe = versions.version_dir("0.1.3").join("bin").join("superfluid");
        let r = update(&home, &catalog, &exe, &from(None, true)).unwrap();
        assert!(r.lines[0].starts_with("0.1.3 -> 0.1.5 (published 2026-10-12;") && !r.changed, "{:?}", r.lines);
        assert_eq!(versions.on_disk(), ["0.1.3"]);
        // The update: 0.1.5 beside 0.1.3, current flipped, 0.1.3 kept as previous.
        let r = update(&home, &catalog, &exe, &from(None, false)).unwrap();
        assert!(r.changed, "{:?}", r.lines);
        assert_eq!(versions.current().as_deref(), Some("0.1.5"));
        assert_eq!(versions.previous().as_deref(), Some("0.1.3"));
        assert_eq!(versions.on_disk(), ["0.1.5", "0.1.3"]);
        assert!(r.lines.iter().any(|l| l == "0.1.3 stays installed: `superfluid self use 0.1.3` goes back to it"), "{:?}", r.lines);
        assert!(prefix.join("bin").join("superfluid-0.1.5").exists());
        assert!(!versions.dir.join(".staging").join("0.1.5").exists());
        // Again: nothing to do.
        let r = update(&home, &catalog, &versions.version_dir("0.1.5").join("bin").join("superfluid"), &from(None, false)).unwrap();
        assert!(!r.changed && r.lines[0].starts_with("superfluid 0.1.5 is current"), "{:?}", r.lines);
        // Ahead of every release: an update without a version named stays put.
        release_tree(&versions.version_dir("0.9.0"), "0.9.0");
        versions.set_current("0.9.0").unwrap();
        let r = update(&home, &catalog, &exe, &from(None, false)).unwrap();
        assert!(!r.changed && r.lines[0].starts_with("superfluid 0.9.0 is current; the newest release that fits is 0.1.5"), "{:?}", r.lines);
        versions.set_current("0.1.5").unwrap();
        std::fs::remove_dir_all(versions.version_dir("0.9.0")).unwrap();
        std::fs::write(versions.dir.join("previous"), "0.1.3\n").unwrap();
        // A named older version that is still here is a flip, not a download; then 0.1.4 by name prunes 0.1.3.
        let r = update(&home, &catalog, &exe, &from(Some("v0.1.3"), false)).unwrap();
        assert!(r.changed && versions.current().as_deref() == Some("0.1.3") && versions.previous().as_deref() == Some("0.1.5"), "{:?}", r.lines);
        let r = update(&home, &catalog, &exe, &from(Some("0.1.4"), false)).unwrap();
        assert_eq!((versions.current().as_deref(), versions.previous().as_deref()), (Some("0.1.4"), Some("0.1.3")));
        assert_eq!(versions.on_disk(), ["0.1.4", "0.1.3"], "0.1.5 was neither current nor previous: {:?}", r.lines);
        assert!(!prefix.join("bin").join("superfluid-0.1.5").exists(), "its link went with it");
        // A kept version survives the prune; `self use` flips without the network.
        std::fs::write(versions.version_dir("0.1.3").join("keep"), "").unwrap();
        let r = use_version(&home, &catalog, &exe, "0.1.3").unwrap();
        assert!(r.changed && versions.current().as_deref() == Some("0.1.3"));
        assert_eq!(list(&home, &exe).lines().filter(|l| l.contains("current") || l.contains("previous")).count(), 2);
        let unreachable = Options { source: Some("/nonexistent/releases.json".into()), ..Options::default() };
        assert!(update(&home, &catalog, &exe, &unreachable).unwrap_err().contains("releases.json"), "the listing is needed for an update");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_release_that_does_not_start_or_drive_the_engine_is_not_made_current() {
        let dir = scratch("probe");
        let home = dir.join("home");
        let versions = Versions::of(&home);
        let tree = release_tree(&versions.version_dir("0.5.0"), "0.5.0");
        std::fs::write(
            tree.join("bin").join("superfluid"),
            "#!/bin/sh\nif [ \"$1\" = runtimes ]; then echo '[{\"id\":\"basert\",\"ready\":\"libbaseRT states ABI 2\"}]'; else echo 'superfluid 0.5.0'; fi\n",
        )
        .unwrap();
        assert!(probe(&versions, "0.5.0", &home, &Engine::None).is_ok(), "no engine in use: starting is enough");
        let e = probe(&versions, "0.5.0", &home, &Engine::Abi(1)).unwrap_err();
        assert!(e.contains("cannot drive the engine here: libbaseRT states ABI 2"), "{e}");
        std::fs::write(tree.join("bin").join("superfluid"), "#!/bin/sh\necho 'superfluid 0.4.9'\n").unwrap();
        assert!(probe(&versions, "0.5.0", &home, &Engine::None).unwrap_err().contains("says it is 0.4.9"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_in_place_install_is_migrated_into_the_layout() {
        let dir = scratch("migrate");
        let (home, prefix) = (dir.join("home"), dir.join("local"));
        let p = listing(&dir, &[("0.1.3", "2026-10-01", None)]);
        let catalog = Catalog::new(home.clone(), None, home.join("superfluid-workerd"));
        // What the old install script leaves: real files in bin, workers in libexec.
        release_tree(&prefix, "0.1.2");
        let exe = prefix.join("bin").join("superfluid");
        assert!(matches!(installed_as(&exe), Installed::InPlace { .. }));
        let opts = Options { source: Some(p.to_string_lossy().to_string()), ..Options::default() };
        let r = update(&home, &catalog, &exe, &opts).unwrap();
        let versions = Versions::of(&home);
        assert_eq!(versions.current().as_deref(), Some("0.1.3"), "{:?}", r.lines);
        assert!(std::fs::symlink_metadata(&exe).unwrap().file_type().is_symlink(), "the copy became a link");
        assert_eq!(std::fs::read_link(prefix.join("bin").join("superfluid-noded")).unwrap(), versions.dir.join("current").join("bin").join("superfluid-noded"));
        assert!(!prefix.join("libexec").join("superfluid").join("superfluid-worker-toy").exists(), "the old workers are gone");
        assert_eq!(versions.prefix().as_deref(), Some(std::fs::canonicalize(prefix.join("bin")).unwrap().as_path()));
        assert!(r.lines.iter().any(|l| l.contains("now links into")), "{:?}", r.lines);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_daily_check_records_the_newest_release_and_announces_it_once() {
        let dir = scratch("check");
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let p = listing(&dir, &[("99.0.0", "2026-01-01", None), ("99.1.0", "2099-01-01", None)]);
        let catalog = Catalog::new(home.clone(), None, home.join("superfluid-workerd"));
        let c = check_now(&home, &catalog, Some(HOLD_OFF_DAYS), &p.to_string_lossy()).unwrap();
        assert_eq!(c.newest.as_deref(), Some("99.0.0"), "99.1.0 is inside the hold-off");
        assert!(!c.due());
        assert_eq!(Check::read(&home), Some(c.clone()));
        let line = announce(&home, c.clone()).unwrap();
        assert!(line.starts_with("superfluid 99.0.0 is available (this is ") && line.contains("`superfluid self update`"), "{line}");
        assert_eq!(announce(&home, Check::read(&home).unwrap()), None, "announced once");
        assert!(Check::read(&home).unwrap().describe().starts_with("newest release: 99.0.0 (published 2026-01-01;"));
        let stale = Check { checked_at: 1, ..Check::default() };
        assert!(stale.due());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
