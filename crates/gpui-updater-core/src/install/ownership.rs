//! Who keeps an installed app up to date: the app itself, a package manager,
//! or whoever packaged the build.
//!
//! An app that a package manager installed should not replace itself: the
//! manager then reports a version it did not install, and its next upgrade
//! can undo the update or fail on the changed files. Check [`ownership`] at
//! startup. When it is not [`Ownership::SelfUpdate`], tell the user how to
//! update instead of offering to.

use std::fmt;
use std::path::Path;

/// The package managers [`ownership`] recognizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PackageManager {
    /// Homebrew: a formula in `Cellar/` or a cask in `Caskroom/`.
    Homebrew,
    /// The Mac App Store: the bundle carries an App Store receipt.
    MacAppStore,
    /// dpkg and apt (Debian, Ubuntu).
    Apt,
    /// rpm (Fedora, RHEL, SUSE).
    Rpm,
    /// pacman (Arch Linux).
    Pacman,
    /// Nix: the app is in `/nix/store`.
    Nix,
    /// Flatpak.
    Flatpak,
    /// Snap.
    Snap,
    /// Scoop (Windows).
    Scoop,
    /// Chocolatey (Windows).
    Chocolatey,
    /// A winget portable package (Windows).
    Winget,
    /// The Microsoft Store: an MSIX package in `WindowsApps`.
    MicrosoftStore,
}

impl PackageManager {
    /// A display name, such as `Homebrew`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Homebrew => "Homebrew",
            Self::MacAppStore => "the Mac App Store",
            Self::Apt => "apt",
            Self::Rpm => "rpm",
            Self::Pacman => "pacman",
            Self::Nix => "Nix",
            Self::Flatpak => "Flatpak",
            Self::Snap => "Snap",
            Self::Scoop => "Scoop",
            Self::Chocolatey => "Chocolatey",
            Self::Winget => "winget",
            Self::MicrosoftStore => "the Microsoft Store",
        }
    }
}

impl fmt::Display for PackageManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Who keeps the installed app up to date.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Ownership {
    /// Nothing else claims the install: the app updates itself.
    SelfUpdate,
    /// A package manager installed the app and should update it.
    #[non_exhaustive]
    Managed {
        /// The package manager.
        manager: PackageManager,
        /// The package, cask, or app ID, when known.
        package: Option<String>,
    },
    /// The packager turned self-update off and says how this build is
    /// updated, through [`OwnershipHints::explanation`].
    Packager(String),
}

impl Ownership {
    /// An install that `manager` owns, as `package` when known.
    #[must_use]
    pub fn managed(manager: PackageManager, package: Option<String>) -> Self {
        Self::Managed { manager, package }
    }

    /// True when the app should update itself.
    #[must_use]
    pub fn is_self_update(&self) -> bool {
        matches!(self, Self::SelfUpdate)
    }

    /// A command that updates the app, for a message such as "Run
    /// `brew upgrade zed` to update". `None` when no single command fits.
    #[must_use]
    pub fn update_command(&self) -> Option<String> {
        let Self::Managed { manager, package } = self else {
            return None;
        };
        let package = package.as_deref();
        Some(match (manager, package) {
            (PackageManager::Pacman, _) => "sudo pacman -Syu".to_string(),
            (PackageManager::Homebrew, Some(p)) => format!("brew upgrade {p}"),
            (PackageManager::Apt, Some(p)) => format!("sudo apt install --only-upgrade {p}"),
            (PackageManager::Rpm, Some(p)) => format!("sudo dnf upgrade {p}"),
            (PackageManager::Flatpak, Some(p)) => format!("flatpak update {p}"),
            (PackageManager::Snap, Some(p)) => format!("sudo snap refresh {p}"),
            (PackageManager::Scoop, Some(p)) => format!("scoop update {p}"),
            (PackageManager::Chocolatey, Some(p)) => format!("choco upgrade {p}"),
            (PackageManager::Winget, Some(p)) => format!("winget upgrade --id {p}"),
            _ => return None,
        })
    }
}

impl fmt::Display for Ownership {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SelfUpdate => f.write_str("updated by the app itself"),
            Self::Managed {
                manager,
                package: Some(package),
            } => write!(f, "installed by {manager} ({package})"),
            Self::Managed { manager, .. } => write!(f, "installed by {manager}"),
            Self::Packager(text) => f.write_str(text),
        }
    }
}

/// What [`ownership`] may use beyond the install location.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OwnershipHints {
    /// How the packager says this build is updated. A distribution that
    /// builds the app sets it, and the app passes it on, for example from
    /// `option_env!("MYAPP_UPDATE_EXPLANATION")`, as Zed does with
    /// `ZED_UPDATE_EXPLANATION`. When set, [`ownership`] returns
    /// [`Ownership::Packager`] without looking further.
    pub explanation: Option<String>,
    /// Ask the system package database (dpkg, rpm, pacman) who owns the app
    /// on Linux. Each query is limited to two seconds. Defaults to `true`.
    pub query_package_database: bool,
}

impl Default for OwnershipHints {
    fn default() -> Self {
        Self::new()
    }
}

impl OwnershipHints {
    /// No packager explanation, and package database queries on.
    #[must_use]
    pub fn new() -> Self {
        Self {
            explanation: None,
            query_package_database: true,
        }
    }

    /// Set the packager's explanation. `None` or a blank text sets none, so
    /// `option_env!(…)` can be passed as it is.
    #[must_use]
    pub fn explanation<S: Into<String>>(mut self, explanation: Option<S>) -> Self {
        self.explanation = explanation
            .map(Into::into)
            .filter(|text| !text.trim().is_empty());
        self
    }

    /// Turn the Linux package database queries on or off.
    #[must_use]
    pub fn query_package_database(mut self, query: bool) -> Self {
        self.query_package_database = query;
        self
    }
}

/// Find who keeps the app at `install_root` up to date (see
/// [`current_install_root`](super::current_install_root)).
///
/// In order: the packager's explanation in `hints`; the location (Nix,
/// Snap, Flatpak, Homebrew's `Cellar`, Scoop, Chocolatey, winget, the
/// Microsoft Store); a Mac App Store receipt; a Homebrew cask that links
/// back to the app; and on Linux the dpkg, rpm, and pacman databases.
/// Anything else is [`Ownership::SelfUpdate`]. Errors are not reported:
/// what cannot be checked does not claim the install.
///
/// ```
/// use gpui_updater_core::{Ownership, OwnershipHints, current_install_root, install};
///
/// let hints = OwnershipHints::new().explanation(option_env!("MYAPP_UPDATE_EXPLANATION"));
/// let owner = current_install_root()
///     .map_or(Ownership::SelfUpdate, |root| install::ownership(&root, &hints));
/// if !owner.is_self_update() {
///     println!("{owner}: {}", owner.update_command().unwrap_or_default());
/// }
/// ```
#[must_use]
pub fn ownership(install_root: &Path, hints: &OwnershipHints) -> Ownership {
    if let Some(text) = hints
        .explanation
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        return Ownership::Packager(text.to_string());
    }
    let canonical = std::fs::canonicalize(install_root).ok();
    let paths: Vec<&Path> = std::iter::once(install_root)
        .chain(canonical.as_deref().filter(|c| *c != install_root))
        .collect();
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    for path in &paths {
        if let Some(found) = by_path(&path.to_string_lossy(), &env) {
            return found;
        }
    }
    let root = canonical.as_deref().unwrap_or(install_root);
    if root.join("Contents/_MASReceipt/receipt").is_file() {
        return Ownership::managed(PackageManager::MacAppStore, None);
    }
    #[cfg(unix)]
    if let Some(found) = homebrew_cask(root, &homebrew_prefixes(&env)) {
        return found;
    }
    #[cfg(target_os = "linux")]
    if hints.query_package_database {
        if let Some(found) = linux::package_database(&paths) {
            return found;
        }
    }
    Ownership::SelfUpdate
}

/// The places Homebrew is installed: `HOMEBREW_PREFIX`, then the defaults.
fn homebrew_prefixes(env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    env("HOMEBREW_PREFIX")
        .into_iter()
        .chain(
            ["/opt/homebrew", "/usr/local", "/home/linuxbrew/.linuxbrew"]
                .into_iter()
                .map(str::to_string),
        )
        .collect()
}

/// A path as `/`-separated text, with a lowercase copy for matching. ASCII
/// lowercasing keeps byte offsets, so a match in `lower` slices `original`.
struct PathText {
    original: String,
    lower: String,
}

impl PathText {
    fn new(path: &str) -> Self {
        let original = path.replace('\\', "/");
        let lower = original.to_ascii_lowercase();
        Self { original, lower }
    }

    /// The path component right after the first `marker` (lowercase, with
    /// `/` on both ends).
    fn after(&self, marker: &str) -> Option<String> {
        let start = self.lower.find(marker)? + marker.len();
        self.component_at(start)
    }

    /// The path component right after `dir` when the path starts in `dir`.
    fn after_prefix(&self, dir: &str) -> Option<String> {
        let mut dir = dir.replace('\\', "/").to_ascii_lowercase();
        if !dir.ends_with('/') {
            dir.push('/');
        }
        self.lower
            .starts_with(&dir)
            .then(|| self.component_at(dir.len()))
            .flatten()
    }

    fn component_at(&self, start: usize) -> Option<String> {
        let name = self.original.get(start..)?.split('/').next()?;
        (!name.is_empty()).then(|| name.to_string())
    }
}

/// Ownership that the location alone shows.
fn by_path(path: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<Ownership> {
    use PackageManager as M;
    let text = PathText::new(path);
    let managed = |manager, package| Some(Ownership::managed(manager, package));
    if text.lower.starts_with("/nix/store/") {
        return managed(M::Nix, None);
    }
    if let Some(name) = text.after_prefix("/snap") {
        return managed(M::Snap, Some(name));
    }
    if text.lower.starts_with("/app/") {
        if let Some(id) = env("FLATPAK_ID") {
            return managed(M::Flatpak, Some(id));
        }
    }
    if let Some(id) = text.after("/flatpak/app/") {
        return managed(M::Flatpak, Some(id));
    }
    for prefix in homebrew_prefixes(env) {
        if let Some(name) = text.after_prefix(&format!("{prefix}/Cellar")) {
            return managed(M::Homebrew, Some(name));
        }
    }
    let scoop = [env("SCOOP"), env("SCOOP_GLOBAL")];
    if let Some(name) = scoop
        .iter()
        .flatten()
        .find_map(|dir| text.after_prefix(&format!("{dir}/apps")))
        .or_else(|| text.after("/scoop/apps/"))
    {
        return managed(M::Scoop, Some(name));
    }
    let chocolatey = env("ChocolateyInstall").unwrap_or_else(|| "C:/ProgramData/chocolatey".into());
    if let Some(name) = text.after_prefix(&format!("{chocolatey}/lib")) {
        return managed(M::Chocolatey, Some(name));
    }
    // A portable package unpacks into `<PackageId>_<SourceId>`.
    if let Some(folder) = text.after("/microsoft/winget/packages/") {
        let id = folder.split_once('_').map_or(folder.as_str(), |(id, _)| id);
        return managed(M::Winget, Some(id.to_string()));
    }
    // An MSIX package folder is `<Name>_<Version>_<Arch>_<Resource>_<Publisher>`.
    if let Some(folder) = text.after("/program files/windowsapps/") {
        let name = folder
            .split_once('_')
            .map_or(folder.as_str(), |(name, _)| name);
        return managed(M::MicrosoftStore, Some(name.to_string()));
    }
    None
}

/// A Homebrew cask that installed `root`: Homebrew moves the app into place
/// and leaves a symlink to it at `Caskroom/<token>/<version>/<App>`.
#[cfg(unix)]
fn homebrew_cask(root: &Path, prefixes: &[String]) -> Option<Ownership> {
    let name = root.file_name()?;
    for prefix in prefixes {
        let Ok(tokens) = std::fs::read_dir(Path::new(prefix).join("Caskroom")) else {
            continue;
        };
        for token in tokens.flatten() {
            let Ok(versions) = std::fs::read_dir(token.path()) else {
                continue;
            };
            for version in versions.flatten() {
                let link = version.path().join(name);
                let is_link = link
                    .symlink_metadata()
                    .is_ok_and(|m| m.file_type().is_symlink());
                if is_link && std::fs::canonicalize(&link).is_ok_and(|target| target == root) {
                    let token = token.file_name().to_string_lossy().into_owned();
                    return Some(Ownership::managed(PackageManager::Homebrew, Some(token)));
                }
            }
        }
    }
    None
}

/// Queries of the Linux package databases.
#[cfg(any(target_os = "linux", test))]
mod linux {
    /// The package that owns one of `paths`, by dpkg, rpm, or pacman.
    #[cfg(target_os = "linux")]
    pub(super) fn package_database(paths: &[&std::path::Path]) -> Option<super::Ownership> {
        use super::{Ownership, PackageManager};

        for path in paths {
            let path = path.as_os_str();
            if let Some(out) = run("dpkg-query", &["--search".as_ref(), "--".as_ref(), path]) {
                if let Some(package) = dpkg_owner(&out, &path.to_string_lossy()) {
                    let format = "--showformat=${db:Status-Status}";
                    let args = ["--show", format, "--", package.as_str()];
                    let status = run("dpkg-query", &args.map(std::ffi::OsStr::new));
                    if status.as_deref().map(str::trim) == Some("installed") {
                        return Some(Ownership::managed(PackageManager::Apt, Some(package)));
                    }
                }
            }
            let rpm = ["--query", "--file", "--queryformat", "%{NAME}\\n", "--"];
            let args: Vec<&std::ffi::OsStr> =
                rpm.iter().map(std::ffi::OsStr::new).chain([path]).collect();
            if let Some(name) = run("rpm", &args).as_deref().and_then(single_name) {
                return Some(Ownership::managed(PackageManager::Rpm, Some(name)));
            }
            if let Some(name) = run("pacman", &["-Qqo".as_ref(), "--".as_ref(), path])
                .as_deref()
                .and_then(single_name)
            {
                return Some(Ownership::managed(PackageManager::Pacman, Some(name)));
            }
        }
        None
    }

    /// Run `program` with `LC_ALL=C` for at most two seconds, and return its
    /// output when it succeeds. A missing program is `None`.
    #[cfg(target_os = "linux")]
    fn run(program: &str, args: &[&std::ffi::OsStr]) -> Option<String> {
        use std::io::Read;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let mut child = Command::new(program)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let mut out = String::new();
                    child.stdout.take()?.read_to_string(&mut out).ok()?;
                    return status.success().then_some(out);
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::debug!(program, "package database query timed out");
                    return None;
                }
            }
        }
    }

    /// The single package that `dpkg-query --search` names for exactly
    /// `path`. Lines read `package: path`; a shared path lists several
    /// packages, and diversions are not ownership.
    pub(super) fn dpkg_owner(out: &str, path: &str) -> Option<String> {
        out.lines()
            .filter(|line| !line.starts_with("diversion "))
            .filter_map(|line| line.split_once(": "))
            .find(|(_, found)| found.trim_end() == path)
            .map(|(owner, _)| owner.trim())
            .filter(|owner| !owner.is_empty() && !owner.contains([',', ' ']))
            .map(str::to_string)
    }

    /// The package name an rpm or pacman query prints: one word on the
    /// first line.
    pub(super) fn single_name(out: &str) -> Option<String> {
        let name = out.lines().next()?.trim();
        (!name.is_empty() && !name.contains(char::is_whitespace)).then(|| name.to_string())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn owner(path: &str) -> Option<(PackageManager, Option<String>)> {
        owner_with(path, &no_env)
    }

    fn owner_with(
        path: &str,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Option<(PackageManager, Option<String>)> {
        match by_path(path, env)? {
            Ownership::Managed { manager, package } => Some((manager, package)),
            other => panic!("{other:?}"),
        }
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "compared with what owner() returns"
    )]
    fn some(manager: PackageManager, package: &str) -> Option<(PackageManager, Option<String>)> {
        Some((manager, Some(package.to_string())))
    }

    #[test]
    fn recognizes_unix_package_locations() {
        use PackageManager as M;
        assert_eq!(
            owner("/nix/store/abc-zed-1.0/bin/zed"),
            Some((M::Nix, None))
        );
        assert_eq!(owner("/snap/zed/42/usr/bin/zed"), some(M::Snap, "zed"));
        assert_eq!(
            owner("/var/lib/flatpak/app/dev.zed.Zed/x86_64/stable/active/files/bin/zed"),
            some(M::Flatpak, "dev.zed.Zed")
        );
        assert_eq!(
            owner("/opt/homebrew/Cellar/zed/1.0/bin/zed"),
            some(M::Homebrew, "zed")
        );
        assert_eq!(
            owner("/home/linuxbrew/.linuxbrew/Cellar/zed/1.0/bin/zed"),
            some(M::Homebrew, "zed")
        );
        // Inside the Flatpak sandbox the app is in /app.
        let flatpak = |name: &str| (name == "FLATPAK_ID").then(|| "dev.zed.Zed".to_string());
        assert_eq!(
            owner_with("/app/bin/zed", &flatpak),
            some(M::Flatpak, "dev.zed.Zed")
        );
        assert_eq!(owner("/app/bin/zed"), None);
        let brew = |name: &str| (name == "HOMEBREW_PREFIX").then(|| "/custom/brew".to_string());
        assert_eq!(
            owner_with("/custom/brew/Cellar/zed/1.0/bin/zed", &brew),
            some(M::Homebrew, "zed")
        );
        for unowned in [
            "/usr/bin/zed",
            "/opt/zed/zed",
            "/home/me/.local/zed.app/bin/zed",
            "/Applications/Zed.app",
            "/snap",
        ] {
            assert_eq!(owner(unowned), None, "{unowned}");
        }
    }

    #[test]
    fn recognizes_windows_package_locations() {
        use PackageManager as M;
        assert_eq!(
            owner(r"C:\Users\me\scoop\apps\zed\current\zed.exe"),
            some(M::Scoop, "zed")
        );
        assert_eq!(
            owner(r"C:\ProgramData\scoop\apps\zed\1.0\zed.exe"),
            some(M::Scoop, "zed")
        );
        let scoop = |name: &str| (name == "SCOOP").then(|| r"D:\Tools".to_string());
        assert_eq!(
            owner_with(r"D:\Tools\apps\zed\current\zed.exe", &scoop),
            some(M::Scoop, "zed")
        );
        assert_eq!(
            owner(r"C:\ProgramData\chocolatey\lib\zed\tools\zed.exe"),
            some(M::Chocolatey, "zed")
        );
        assert_eq!(
            owner(
                r"C:\Users\me\AppData\Local\Microsoft\WinGet\Packages\ZedIndustries.Zed_Microsoft.Winget.Source_8wekyb3d8bbwe\zed.exe"
            ),
            some(M::Winget, "ZedIndustries.Zed")
        );
        assert_eq!(
            owner(
                r"C:\Program Files\WindowsApps\ZedIndustries.Zed_1.0.0.0_x64__abcdefghjkmnp\zed.exe"
            ),
            some(M::MicrosoftStore, "ZedIndustries.Zed")
        );
        for unowned in [
            r"C:\Users\me\AppData\Local\Programs\Zed\zed.exe",
            r"C:\Program Files\Zed\zed.exe",
        ] {
            assert_eq!(owner(unowned), None, "{unowned}");
        }
    }

    #[test]
    fn a_packager_explanation_wins() {
        let dir = tempfile::tempdir().unwrap();
        let hints = OwnershipHints::new().explanation(Some("Update with your distribution."));
        assert_eq!(
            ownership(dir.path(), &hints),
            Ownership::Packager("Update with your distribution.".into())
        );
        assert_eq!(
            OwnershipHints::new().explanation(Some("  ")).explanation,
            None
        );
        assert_eq!(
            OwnershipHints::new().explanation(None::<&str>).explanation,
            None
        );
    }

    #[test]
    fn an_app_nothing_claims_updates_itself() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        std::fs::write(&app, b"app").unwrap();
        assert_eq!(
            ownership(&app, &OwnershipHints::new()),
            Ownership::SelfUpdate
        );
    }

    #[test]
    fn a_mac_app_store_receipt_claims_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("App.app");
        std::fs::create_dir_all(app.join("Contents/_MASReceipt")).unwrap();
        std::fs::write(app.join("Contents/_MASReceipt/receipt"), b"receipt").unwrap();
        let found = ownership(&app, &OwnershipHints::new().query_package_database(false));
        assert_eq!(found, Ownership::managed(PackageManager::MacAppStore, None));
        assert_eq!(found.update_command(), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_homebrew_cask_links_back_to_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let apps = dir.path().join("Applications");
        let app = apps.join("Zed.app");
        std::fs::create_dir_all(app.join("Contents")).unwrap();
        let prefix = dir.path().join("brew");
        let version = prefix.join("Caskroom/zed/1.0");
        std::fs::create_dir_all(&version).unwrap();
        std::fs::create_dir_all(prefix.join("Caskroom/zed/.metadata")).unwrap();
        let app = std::fs::canonicalize(&app).unwrap();
        let prefixes = [prefix.to_string_lossy().into_owned()];
        assert_eq!(homebrew_cask(&app, &prefixes), None);
        std::os::unix::fs::symlink(&app, version.join("Zed.app")).unwrap();
        assert_eq!(
            homebrew_cask(&app, &prefixes),
            Some(Ownership::managed(
                PackageManager::Homebrew,
                Some("zed".into())
            ))
        );
        // Another app of the same name elsewhere is not the cask's.
        let other = dir.path().join("Other/Zed.app");
        std::fs::create_dir_all(&other).unwrap();
        assert_eq!(
            homebrew_cask(&std::fs::canonicalize(&other).unwrap(), &prefixes),
            None
        );
    }

    #[test]
    fn parses_package_database_answers() {
        let out =
            "diversion by foo from: /usr/bin/zed\nzed: /usr/bin/zed\nzed-data: /usr/share/zed\n";
        assert_eq!(
            linux::dpkg_owner(out, "/usr/bin/zed").as_deref(),
            Some("zed")
        );
        assert_eq!(
            linux::dpkg_owner("libc6:amd64: /lib/x.so\n", "/lib/x.so").as_deref(),
            Some("libc6:amd64")
        );
        assert_eq!(
            linux::dpkg_owner("a, b: /usr/share/doc\n", "/usr/share/doc"),
            None
        );
        assert_eq!(
            linux::dpkg_owner("zed: /usr/bin/zed-cli\n", "/usr/bin/zed"),
            None
        );
        assert_eq!(linux::single_name("zed\n").as_deref(), Some("zed"));
        assert_eq!(
            linux::single_name("file /x is not owned by any package\n"),
            None
        );
        assert_eq!(linux::single_name(""), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dpkg_claims_a_system_program() {
        if !Path::new("/usr/bin/dpkg-query").exists() {
            eprintln!("skipped: no dpkg-query");
            return;
        }
        let found = ownership(Path::new("/usr/bin/ls"), &OwnershipHints::new());
        assert_eq!(
            found,
            Ownership::managed(PackageManager::Apt, Some("coreutils".into()))
        );
        assert_eq!(
            found.update_command().as_deref(),
            Some("sudo apt install --only-upgrade coreutils")
        );
        let off = OwnershipHints::new().query_package_database(false);
        assert_eq!(
            ownership(Path::new("/usr/bin/ls"), &off),
            Ownership::SelfUpdate
        );
    }

    #[test]
    fn describes_ownership() {
        let brew = Ownership::managed(PackageManager::Homebrew, Some("zed".into()));
        assert_eq!(brew.to_string(), "installed by Homebrew (zed)");
        assert_eq!(brew.update_command().as_deref(), Some("brew upgrade zed"));
        assert!(!brew.is_self_update());
        assert_eq!(
            Ownership::managed(PackageManager::Nix, None).to_string(),
            "installed by Nix"
        );
        assert_eq!(
            Ownership::managed(PackageManager::Nix, None).update_command(),
            None
        );
        assert_eq!(
            Ownership::managed(PackageManager::Pacman, None)
                .update_command()
                .as_deref(),
            Some("sudo pacman -Syu")
        );
        assert!(Ownership::SelfUpdate.is_self_update());
        assert_eq!(
            Ownership::Packager("Use apt.".into()).to_string(),
            "Use apt."
        );
    }
}
