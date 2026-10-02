//! Release checks and `dejavu self-update`. A port of `update.ts`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::VERSION;

pub const UPDATE_REPOSITORY: &str = "safzanpirani/dejavu";
pub const PACKAGE_NAME: &str = "@safzanpirani/dejavu";
pub const DISABLE_CHECK_ENV: &str = "DEJAVU_NO_UPDATE_CHECK";
const CHECK_INTERVAL_MS: f64 = 24.0 * 60.0 * 60.0 * 1000.0;
const CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// The two HTTP calls an update makes, replaceable in tests.
pub trait Fetch {
    /// A HEAD request that does not follow redirects: the status and `Location`.
    fn head(&self, url: &str, timeout: Duration) -> Result<(u16, Option<String>), String>;
    /// A GET that follows redirects: the final status and body.
    fn get(&self, url: &str, timeout: Duration) -> Result<(u16, Vec<u8>), String>;
}

/// The real network, through `ureq`.
pub struct Http;

fn agent(timeout: Duration, redirects: u32) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .max_redirects(redirects)
        .http_status_as_error(false)
        .build()
        .into()
}

impl Fetch for Http {
    fn head(&self, url: &str, timeout: Duration) -> Result<(u16, Option<String>), String> {
        let response = agent(timeout, 0)
            .head(url)
            .header("User-Agent", format!("dejavu/{VERSION}"))
            .call()
            .map_err(|error| error.to_string())?;
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        Ok((response.status().as_u16(), location))
    }

    fn get(&self, url: &str, timeout: Duration) -> Result<(u16, Vec<u8>), String> {
        let mut response = agent(timeout, 10)
            .get(url)
            .header("User-Agent", format!("dejavu/{VERSION}"))
            .call()
            .map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(512 * 1024 * 1024)
            .read_to_vec()
            .map_err(|error| error.to_string())?;
        Ok((status, body))
    }
}

/// Runs `npm` or `bun` with arguments.
pub type PackageManagerRunner = Box<dyn Fn(&str, &[String]) -> Result<(), String>>;

/// Everything `selfUpdate` and `availableUpdate` reach outside the process.
pub struct UpdateDeps {
    pub fetch: Box<dyn Fetch>,
    pub now: Box<dyn Fn() -> f64>,
    pub check_path: PathBuf,
    pub executable: Option<PathBuf>,
    /// `None` detects a source checkout from the executable's location.
    pub compiled: Option<bool>,
    pub platform: String,
    pub arch: String,
    pub log: Box<dyn Fn(&str)>,
    pub run_package_manager: PackageManagerRunner,
    /// `DEJAVU_NO_UPDATE_CHECK=1`.
    pub disabled: bool,
}

impl Default for UpdateDeps {
    fn default() -> Self {
        UpdateDeps {
            fetch: Box::new(Http),
            now: Box::new(now_ms),
            check_path: default_check_path(),
            executable: None,
            compiled: None,
            platform: node_platform().into(),
            arch: node_arch().into(),
            log: Box::new(|line| eprintln!("{line}")),
            run_package_manager: Box::new(run_package_manager),
            disabled: std::env::var(DISABLE_CHECK_ENV).is_ok_and(|value| value == "1"),
        }
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as f64)
        .unwrap_or(0.0)
}

/// `process.platform`.
pub fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// `process.arch`.
pub fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

pub fn parse_version(value: &str) -> Option<[f64; 3]> {
    let trimmed = js_trim(value);
    let trimmed = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let core = trimmed.split(['-', '+']).next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() > 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let mut numbers = [0.0; 3];
    for (slot, part) in numbers.iter_mut().zip(&parts) {
        *slot = part.parse().ok()?;
    }
    Some(numbers)
}

/// Orders two dotted versions; unparsable values sort lowest.
pub fn compare_versions(a: &str, b: &str) -> i32 {
    match (parse_version(a), parse_version(b)) {
        (Some(left), Some(right)) => {
            for index in 0..3 {
                if left[index] != right[index] {
                    return if left[index] < right[index] { -1 } else { 1 };
                }
            }
            0
        }
        (Some(_), None) => 1,
        (None, Some(_)) => -1,
        (None, None) => 0,
    }
}

pub fn version_from_release_url(location: &str) -> Result<String, String> {
    let marker = "/releases/tag/";
    // GitHub redirects /releases/latest to /releases while a repository has no release.
    if location.ends_with("/releases") || location.ends_with("/releases/") {
        return Err(format!("{UPDATE_REPOSITORY} has no published release yet"));
    }
    let Some(index) = location.rfind(marker) else {
        return Err(format!("unexpected release location {location}"));
    };
    let decoded = percent_decode(&location[index + marker.len()..])?;
    let tag = decoded.strip_prefix('v').unwrap_or(&decoded).to_string();
    if parse_version(&tag).is_none() {
        return Err(format!("unexpected release tag {tag}"));
    }
    Ok(tag)
}

/// `decodeURIComponent`, which rejects malformed escapes.
fn percent_decode(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok());
            out.push(hex.ok_or("URI malformed")?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "URI malformed".into())
}

/// Asset names match the release workflow: `dejavu-<platform>-<arch>[.exe]`.
pub fn release_asset_name(platform: &str, arch: &str) -> String {
    let os = if platform == "win32" {
        "windows"
    } else {
        platform
    };
    format!(
        "dejavu-{os}-{arch}{}",
        if platform == "win32" { ".exe" } else { "" }
    )
}

pub fn checksum_for(checksums: &str, asset: &str) -> Option<String> {
    checksums.split('\n').find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        (fields.len() == 2 && fields[1].strip_prefix('*').unwrap_or(fields[1]) == asset)
            .then(|| fields[0].to_lowercase())
    })
}

fn release_download_url(tag: &str, asset: &str) -> String {
    format!("https://github.com/{UPDATE_REPOSITORY}/releases/download/{tag}/{asset}")
}

/// Reads the newest release tag from the redirect on /releases/latest, which is
/// not rate-limited the way the unauthenticated API is.
pub fn fetch_latest_version(fetch: &dyn Fetch, timeout: Duration) -> Result<String, String> {
    let url = format!("https://github.com/{UPDATE_REPOSITORY}/releases/latest");
    let (status, location) = fetch.head(&url, timeout)?;
    match location {
        Some(location) => version_from_release_url(&location),
        None => Err(format!(
            "{url} returned {status} without a release redirect"
        )),
    }
}

fn fetch_bytes(url: &str, fetch: &dyn Fetch) -> Result<Vec<u8>, String> {
    let (status, body) = fetch.get(url, DOWNLOAD_TIMEOUT)?;
    if !(200..300).contains(&status) {
        return Err(format!("{url} returned {status}"));
    }
    Ok(body)
}

pub fn default_check_path() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local").join("state"));
    state.join("dejavu").join("update-check.json")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheck {
    #[serde(serialize_with = "js_number")]
    pub checked_at: f64,
    pub latest: String,
    /// The installed version that wrote the check; missing or non-string reads as `None`.
    pub current: Option<String>,
}

fn js_number<S: serde::Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    crate::js::number(*value).serialize(serializer)
}

pub fn read_update_check(path: &Path) -> Option<UpdateCheck> {
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    Some(UpdateCheck {
        checked_at: value.get("checkedAt")?.as_f64()?,
        latest: value.get("latest")?.as_str()?.to_string(),
        current: value
            .get("current")
            .and_then(|current| current.as_str())
            .map(str::to_string),
    })
}

fn write_update_check(check: &UpdateCheck, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string(check).unwrap_or_default())
}

/// A binary under a Cargo `target/` directory of a dejavu checkout.
fn is_source_checkout(executable: &Path) -> bool {
    executable
        .ancestors()
        .skip(1)
        .take(5)
        .any(|dir| dir.join("Cargo.toml").is_file() && dir.join("crates").join("dejavu").is_dir())
}

/// The npm package keeps the binary in `native/` beside its package.json, so an
/// npm or Bun global install is updated through its package manager rather than
/// by replacing the binary in place.
pub fn package_manager_for(executable: &Path) -> Option<&'static str> {
    let native = executable.parent()?;
    let root = native.parent()?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("package.json")).ok()?).ok()?;
    if manifest.get("name").and_then(|name| name.as_str()) != Some(PACKAGE_NAME)
        || native.file_name().is_none_or(|name| name != "native")
    {
        return None;
    }
    Some(
        if root.to_string_lossy().replace('\\', "/").contains("/.bun/") {
            "bun"
        } else {
            "npm"
        },
    )
}

fn run_package_manager(command: &str, args: &[String]) -> Result<(), String> {
    // npm is a batch file on Windows, which Command runs only by its full name.
    let program = if cfg!(windows) && command == "npm" {
        "npm.cmd"
    } else {
        command
    };
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .map_err(|_| {
            format!(
                "{command} is not on PATH; run `{command} {}`",
                args.join(" ")
            )
        })?;
    if !status.success() {
        let code = status
            .code()
            .map_or_else(|| "null".into(), |code| code.to_string());
        return Err(format!("{command} {} exited with {code}", args.join(" ")));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SelfUpdateResult {
    pub current: String,
    pub latest: String,
    pub updated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// Installs the newest release over this binary after checking it against the
/// release checksums.
pub fn self_update(check_only: bool, deps: &UpdateDeps) -> Result<SelfUpdateResult, String> {
    let latest = fetch_latest_version(deps.fetch.as_ref(), DOWNLOAD_TIMEOUT)?;
    let check = UpdateCheck {
        checked_at: (deps.now)(),
        latest: latest.clone(),
        current: Some(VERSION.into()),
    };
    let _ = write_update_check(&check, &deps.check_path);
    let not_updated = || SelfUpdateResult {
        current: VERSION.into(),
        latest: latest.clone(),
        updated: false,
        path: None,
    };
    if compare_versions(&latest, VERSION) <= 0 || check_only {
        return Ok(not_updated());
    }
    let raw = match &deps.executable {
        Some(path) => path.clone(),
        None => std::env::current_exe().map_err(|error| error.to_string())?,
    };
    let executable =
        std::fs::canonicalize(&raw).map_err(|error| format!("{error}: {}", raw.display()))?;
    let compiled = deps
        .compiled
        .unwrap_or_else(|| !is_source_checkout(&raw) && !is_source_checkout(&executable));
    if !compiled {
        return Err(format!(
            "dejavu {latest} is available, but this dejavu runs from a source checkout; run `git pull` there instead"
        ));
    }
    let path = executable.to_string_lossy().into_owned();
    let updated = || SelfUpdateResult {
        current: VERSION.into(),
        latest: latest.clone(),
        updated: true,
        path: Some(path.clone()),
    };
    if let Some(manager) = package_manager_for(&executable) {
        let verb = if manager == "bun" { "add" } else { "install" };
        let args: Vec<String> = vec![verb.into(), "-g".into(), format!("{PACKAGE_NAME}@{latest}")];
        (deps.log)(&format!(
            "updating through {manager}: {manager} {}",
            args.join(" ")
        ));
        (deps.run_package_manager)(manager, &args)?;
        return Ok(updated());
    }
    let tag = format!("v{latest}");
    let asset = release_asset_name(&deps.platform, &deps.arch);
    let checksums = fetch_bytes(
        &release_download_url(&tag, "checksums.txt"),
        deps.fetch.as_ref(),
    )?;
    let Some(expected) = checksum_for(&String::from_utf8_lossy(&checksums), &asset) else {
        return Err(format!(
            "release {tag} has no prebuilt binary named {asset}"
        ));
    };
    (deps.log)(&format!("downloading {asset} {tag}"));
    let binary = fetch_bytes(&release_download_url(&tag, &asset), deps.fetch.as_ref())?;
    let actual = hex(&Sha256::digest(&binary));
    if actual != expected {
        return Err(format!(
            "{asset} checksum mismatch: expected {expected}, got {actual}"
        ));
    }
    swap_executable(&executable, &binary, &deps.platform).map_err(|error| error.to_string())?;
    Ok(updated())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Writes the new binary beside the old one and renames it into place, so a
/// running process keeps its original inode. Windows cannot overwrite a running
/// executable, but it can rename it aside first.
fn swap_executable(executable: &Path, binary: &[u8], platform: &str) -> std::io::Result<()> {
    let directory = executable.parent().unwrap_or(Path::new("."));
    let next = directory.join(format!(".dejavu-update-{}", std::process::id()));
    let result = (|| {
        std::fs::write(&next, binary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o755))?;
        }
        let mut aside = None;
        if platform == "win32" {
            let old = PathBuf::from(format!("{}.old", executable.display()));
            match std::fs::remove_file(&old) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
            std::fs::rename(executable, &old)?;
            aside = Some(old);
        }
        std::fs::rename(&next, executable).inspect_err(|_| {
            if let Some(old) = &aside {
                let _ = std::fs::rename(old, executable);
            }
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&next);
    }
    result
}

/// Refreshes the cached release lookup at most once a day, then returns a newer
/// release if the cache knows one. A failed lookup keeps the previous answer and
/// still counts toward the interval.
pub fn available_update(deps: &UpdateDeps) -> Option<String> {
    if deps.disabled {
        return None;
    }
    let now = (deps.now)();
    let mut check = read_update_check(&deps.check_path);
    let stale = match &check {
        None => true,
        Some(check) => {
            let age = now - check.checked_at;
            check.current.as_deref() != Some(VERSION) || !(0.0..=CHECK_INTERVAL_MS).contains(&age)
        }
    };
    if stale {
        let mut latest = check
            .as_ref()
            .map(|check| check.latest.clone())
            .unwrap_or_default();
        // Keep the last known release through an outage.
        if let Ok(found) = fetch_latest_version(deps.fetch.as_ref(), CHECK_TIMEOUT) {
            latest = found;
        }
        let fresh = UpdateCheck {
            checked_at: now,
            latest,
            current: Some(VERSION.into()),
        };
        let _ = write_update_check(&fresh, &deps.check_path);
        check = Some(fresh);
    }
    check
        .map(|check| check.latest)
        .filter(|latest| !latest.is_empty() && compare_versions(latest, VERSION) > 0)
}

/// `String.prototype.trim`: Unicode whitespace plus the byte-order mark.
fn js_trim(text: &str) -> &str {
    text.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    const RELEASE_URL: &str = "https://github.com/safzanpirani/dejavu/releases";

    struct FakeRelease {
        latest: Option<String>,
        files: HashMap<String, Vec<u8>>,
        calls: Rc<RefCell<Vec<String>>>,
    }

    impl Fetch for FakeRelease {
        fn head(&self, url: &str, _timeout: Duration) -> Result<(u16, Option<String>), String> {
            self.calls.borrow_mut().push(url.into());
            let latest = self.latest.as_ref().ok_or("offline")?;
            assert!(url.ends_with("/releases/latest"));
            Ok((302, Some(format!("{RELEASE_URL}/tag/v{latest}"))))
        }
        fn get(&self, url: &str, _timeout: Duration) -> Result<(u16, Vec<u8>), String> {
            self.calls.borrow_mut().push(url.into());
            let name = &url[url.rfind('/').unwrap() + 1..];
            Ok(self
                .files
                .get(name)
                .map_or_else(|| (404, b"missing".to_vec()), |body| (200, body.clone())))
        }
    }

    fn release(
        latest: &str,
        files: &[(&str, &[u8])],
        calls: &Rc<RefCell<Vec<String>>>,
    ) -> Box<dyn Fetch> {
        Box::new(FakeRelease {
            latest: Some(latest.into()),
            files: files
                .iter()
                .map(|(name, body)| (name.to_string(), body.to_vec()))
                .collect(),
            calls: calls.clone(),
        })
    }

    fn bumped(version: &str) -> String {
        let parts: Vec<u64> = version
            .split('.')
            .map(|part| part.parse().unwrap())
            .collect();
        format!("{}.{}.0", parts[0], parts[1] + 1)
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> TempDir {
            static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "dejavu-update-{name}-{}-{unique}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn deps(fetch: Box<dyn Fetch>, root: &Path) -> UpdateDeps {
        UpdateDeps {
            fetch,
            now: Box::new(|| 1_000.0),
            check_path: root.join("check.json"),
            executable: None,
            compiled: Some(true),
            platform: "darwin".into(),
            arch: "arm64".into(),
            log: Box::new(|_| {}),
            run_package_manager: Box::new(|_, _| panic!("unexpected package manager")),
            disabled: false,
        }
    }

    #[test]
    fn compares_dotted_versions_and_sorts_unparsable_values_lowest() {
        assert_eq!(compare_versions("0.10.0", "0.9.9"), 1);
        assert_eq!(compare_versions("v1.2", "1.2.0"), 0);
        assert_eq!(compare_versions("1.2.3-beta", "1.2.4"), -1);
        assert_eq!(compare_versions("junk", "0.0.1"), -1);
        assert_eq!(compare_versions("", ""), 0);
    }

    #[test]
    fn reads_the_tag_from_the_latest_release_redirect() {
        assert_eq!(
            version_from_release_url(&format!("{RELEASE_URL}/tag/v0.4.0")).unwrap(),
            "0.4.0"
        );
        assert!(
            version_from_release_url(RELEASE_URL)
                .unwrap_err()
                .contains("no published release")
        );
        assert!(
            version_from_release_url(&format!("{RELEASE_URL}/tag/nightly"))
                .unwrap_err()
                .contains("unexpected release tag")
        );
    }

    #[test]
    fn names_assets_per_platform_and_finds_their_checksums() {
        assert_eq!(release_asset_name("darwin", "arm64"), "dejavu-darwin-arm64");
        assert_eq!(release_asset_name("win32", "x64"), "dejavu-windows-x64.exe");
        assert_eq!(
            checksum_for(
                "ABC  dejavu-linux-x64\ndef *dejavu-darwin-arm64\n",
                "dejavu-darwin-arm64"
            )
            .as_deref(),
            Some("def")
        );
        assert_eq!(
            checksum_for("abc  dejavu-linux-x64\n", "dejavu-darwin-arm64"),
            None
        );
    }

    #[test]
    fn replaces_the_executable_with_the_checksum_verified_release_binary() {
        let root = TempDir::new("swap");
        let executable = root.0.join("dejavu");
        std::fs::write(&executable, "old").unwrap();
        let latest = bumped(VERSION);
        let binary = b"new binary";
        let digest = hex(&Sha256::digest(binary));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let checksums = format!("{digest}  dejavu-darwin-arm64\n");
        let fetch = release(
            &latest,
            &[
                ("checksums.txt", checksums.as_bytes()),
                ("dejavu-darwin-arm64", binary),
            ],
            &calls,
        );
        let mut deps = deps(fetch, &root.0);
        deps.executable = Some(executable.clone());
        let checked = self_update(true, &deps).unwrap();
        assert_eq!(
            checked,
            SelfUpdateResult {
                current: VERSION.into(),
                latest: latest.clone(),
                updated: false,
                path: None
            }
        );
        assert_eq!(
            serde_json::to_string(&checked).unwrap(),
            format!(r#"{{"current":"{VERSION}","latest":"{latest}","updated":false}}"#)
        );
        assert_eq!(std::fs::read_to_string(&executable).unwrap(), "old");
        let result = self_update(false, &deps).unwrap();
        assert!(result.updated);
        assert_eq!(result.latest, latest);
        assert_eq!(std::fs::read(&executable).unwrap(), binary);
        let names: Vec<String> = std::fs::read_dir(&root.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into())
            .collect();
        assert!(
            names
                .iter()
                .all(|name| !name.starts_with(".dejavu-update-")),
            "{names:?}"
        );
    }

    #[test]
    fn refuses_a_binary_whose_checksum_does_not_match_and_leaves_the_executable_alone() {
        let root = TempDir::new("mismatch");
        let executable = root.0.join("dejavu");
        std::fs::write(&executable, "old").unwrap();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let checksums = format!("{}  dejavu-linux-x64\n", "0".repeat(64));
        let fetch = release(
            &bumped(VERSION),
            &[
                ("checksums.txt", checksums.as_bytes()),
                ("dejavu-linux-x64", b"tampered"),
            ],
            &calls,
        );
        let mut deps = deps(fetch, &root.0);
        deps.executable = Some(executable.clone());
        deps.platform = "linux".into();
        deps.arch = "x64".into();
        assert!(
            self_update(false, &deps)
                .unwrap_err()
                .contains("checksum mismatch")
        );
        assert_eq!(std::fs::read_to_string(&executable).unwrap(), "old");
    }

    #[test]
    fn reports_a_release_without_this_platform() {
        let root = TempDir::new("noasset");
        let executable = root.0.join("dejavu");
        std::fs::write(&executable, "old").unwrap();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let fetch = release(
            &bumped(VERSION),
            &[("checksums.txt", b"abc  dejavu-linux-x64\n")],
            &calls,
        );
        let mut deps = deps(fetch, &root.0);
        deps.executable = Some(executable);
        let error = self_update(false, &deps).unwrap_err();
        assert_eq!(
            error,
            format!(
                "release v{} has no prebuilt binary named dejavu-darwin-arm64",
                bumped(VERSION)
            )
        );
    }

    #[test]
    fn updates_an_npm_or_bun_global_install_through_its_package_manager() {
        let root = TempDir::new("bun");
        let package_root = root
            .0
            .join(".bun/install/global/node_modules/@safzanpirani/dejavu");
        let executable = package_root.join("native").join("dejavu");
        std::fs::create_dir_all(package_root.join("native")).unwrap();
        std::fs::write(
            package_root.join("package.json"),
            r#"{"name":"@safzanpirani/dejavu"}"#,
        )
        .unwrap();
        std::fs::write(&executable, "old").unwrap();
        let latest = bumped(VERSION);
        let runs = Rc::new(RefCell::new(Vec::<Vec<String>>::new()));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut deps = deps(release(&latest, &[], &calls), &root.0);
        deps.executable = Some(executable.clone());
        let seen = runs.clone();
        deps.run_package_manager = Box::new(move |command, args| {
            seen.borrow_mut().push(
                std::iter::once(command.to_string())
                    .chain(args.iter().cloned())
                    .collect(),
            );
            Ok(())
        });
        let result = self_update(false, &deps).unwrap();
        assert!(result.updated);
        assert_eq!(
            *runs.borrow(),
            vec![vec![
                "bun".to_string(),
                "add".into(),
                "-g".into(),
                format!("@safzanpirani/dejavu@{latest}")
            ]]
        );
        assert_eq!(std::fs::read_to_string(&executable).unwrap(), "old");

        let npm_root = root.0.join("lib/node_modules/@safzanpirani/dejavu");
        std::fs::create_dir_all(npm_root.join("native")).unwrap();
        std::fs::write(
            npm_root.join("package.json"),
            r#"{"name":"@safzanpirani/dejavu"}"#,
        )
        .unwrap();
        assert_eq!(
            package_manager_for(&npm_root.join("native/dejavu")),
            Some("npm")
        );
        assert_eq!(package_manager_for(&npm_root.join("bin/dejavu")), None);
    }

    #[test]
    fn sends_a_source_checkout_to_git_pull() {
        let root = TempDir::new("source");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut deps = deps(release(&bumped(VERSION), &[], &calls), &root.0);
        deps.compiled = Some(false);
        assert!(self_update(false, &deps).unwrap_err().contains("git pull"));

        let checkout = root.0.join("checkout");
        std::fs::create_dir_all(checkout.join("crates/dejavu")).unwrap();
        std::fs::create_dir_all(checkout.join("target/release")).unwrap();
        std::fs::write(checkout.join("Cargo.toml"), "").unwrap();
        assert!(is_source_checkout(&checkout.join("target/release/dejavu")));
        assert!(!is_source_checkout(&root.0.join("bin/dejavu")));
    }

    #[test]
    fn looks_up_releases_at_most_once_a_day_and_keeps_the_last_answer_through_an_outage() {
        let root = TempDir::new("daily");
        let latest = bumped(VERSION);
        let calls = Rc::new(RefCell::new(Vec::new()));
        let day = 24.0 * 60.0 * 60.0 * 1000.0;
        let at = |fetch: Box<dyn Fetch>, now: f64| {
            let mut deps = deps(fetch, &root.0);
            deps.now = Box::new(move || now);
            available_update(&deps)
        };
        assert_eq!(
            at(release(&latest, &[], &calls), 1_000.0),
            Some(latest.clone())
        );
        assert_eq!(
            at(release(&latest, &[], &calls), 1_000.0 + day / 2.0),
            Some(latest.clone())
        );
        assert_eq!(calls.borrow().len(), 1);
        let offline = Box::new(FakeRelease {
            latest: None,
            files: HashMap::new(),
            calls: calls.clone(),
        });
        assert_eq!(at(offline, 2_000.0 + day), Some(latest.clone()));
        let stored = read_update_check(&root.0.join("check.json")).unwrap();
        assert_eq!(
            stored,
            UpdateCheck {
                checked_at: 2_000.0 + day,
                latest: latest.clone(),
                current: Some(VERSION.into())
            }
        );
        let text = std::fs::read_to_string(root.0.join("check.json")).unwrap();
        assert_eq!(
            text,
            format!(
                r#"{{"checkedAt":{},"latest":"{latest}","current":"{VERSION}"}}"#,
                2_000 + 86_400_000
            )
        );
        assert_eq!(at(release(VERSION, &[], &calls), 3_000.0 + 2.0 * day), None);
    }

    #[test]
    fn a_disabled_check_makes_no_request() {
        let root = TempDir::new("disabled");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut deps = deps(release(&bumped(VERSION), &[], &calls), &root.0);
        deps.disabled = true;
        assert_eq!(available_update(&deps), None);
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn the_real_client_reads_the_redirect_without_following_it() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let read = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
            let location =
                format!("http://127.0.0.1:{port}/safzanpirani/dejavu/releases/tag/v9.8.7");
            write!(stream, "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            request
        });
        let (status, location) = Http
            .head(
                &format!("http://127.0.0.1:{port}/releases/latest"),
                Duration::from_secs(5),
            )
            .unwrap();
        let request = server.join().unwrap();
        assert_eq!(status, 302);
        assert_eq!(
            version_from_release_url(&location.unwrap()).unwrap(),
            "9.8.7"
        );
        assert!(request.starts_with("HEAD /releases/latest"));
        assert!(
            request
                .to_lowercase()
                .contains(&format!("user-agent: dejavu/{VERSION}"))
        );
    }
}
