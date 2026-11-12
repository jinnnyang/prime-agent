//! The installer-takeover update funnel: `prime-agent update` and the TUI's
//! `/update` download the update channel's installer and run it — the
//! official domain endpoint or the download base's rendered installers,
//! with a recorded GitHub releases base riding the release-asset routes
//! (`latest/download/install.sh`, `download/nightly/install-beta.sh`);
//! never a GitHub raw or workflow URL. The
//! script is the single source of truth for the whole move — it resolves
//! and downloads the latest build, uninstalls the TypeScript version,
//! publishes the payload, and never touches `~/.prime/agent` (the sessions
//! and configuration the products share). The command's contract is "fetch
//! from the official source, run it". This module only fetches and execs
//! the script, then reports what landed; every install/uninstall decision
//! stays in the script the installer-takeover lane owns, so the two
//! surfaces can never drift from it.

use std::io::{Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use super::release::update_user_agent;

/// `PRIME_AGENT_RUST_PREFIX`: the installer's own prefix knob (the launcher
/// probe reads the same value the script installs under).
pub const ENV_PREFIX: &str = "PRIME_AGENT_RUST_PREFIX";
/// `PRIME_AGENT_RUST_INSTALLER_URL`: the installer script URL override —
/// tests serve their own script, a pinned install can point elsewhere.
pub const ENV_INSTALLER_URL: &str = "PRIME_AGENT_RUST_INSTALLER_URL";
/// The installer's release-channel knob (`stable` | `beta`): the funnel
/// passes the requested channel, else the installed one.
pub const ENV_RELEASE_CHANNEL: &str = "PRIME_AGENT_RELEASE_CHANNEL";
/// The installer's download-base knob (the bucket holding the channel
/// manifests and release archives).
pub const ENV_DOWNLOAD_BASE_URL: &str = "PRIME_AGENT_DOWNLOAD_BASE_URL";
/// `install-rust.sh`'s `DOWNLOAD_BASE_URL_DEFAULT`: where the channel
/// manifests (`latest.json`, `beta.json`) are published.
pub const DEFAULT_DOWNLOAD_BASE_URL: &str = "https://pub-728493de92a943e2a9b2d17b4719f318.r2.dev";

/// The official domain's install endpoint — the stable channel's installer
/// source; never a GitHub raw or workflow URL (the override env var stays
/// for tests and pinned installs).
pub const OFFICIAL_INSTALLER_URL: &str = "https://app.primeintellect.ai/prime-agent/install.sh";
/// The stable installer's file under the download base (the R2 layout's
/// served name; a GitHub releases base serves it at the `latest/download/`
/// redirect).
pub const STABLE_INSTALLER_FILE: &str = "install.sh";
/// The nightly installer's file under the download base (the domain only
/// forwards `install.sh`, so nightly updates fetch it from the base).
pub const BETA_INSTALLER_FILE: &str = "install-beta.sh";

/// The official domain's PowerShell install endpoint — the ps1 route's
/// stable installer source, rendered to the same places the sh pair is
/// (the release pipeline renders `install.ps1` and `install-beta.ps1`
/// beside the sh pair). Windows only: the ps1 leg exists only there.
#[cfg(windows)]
pub(crate) const OFFICIAL_PS1_INSTALLER_URL: &str =
    "https://app.primeintellect.ai/prime-agent/install.ps1";
/// The stable installer's ps1 file under the download base (the same
/// layout the sh pair rides).
#[cfg(windows)]
pub(crate) const STABLE_PS1_INSTALLER_FILE: &str = "install.ps1";
/// The nightly installer's ps1 file under the download base.
#[cfg(windows)]
pub(crate) const BETA_PS1_INSTALLER_FILE: &str = "install-beta.ps1";

/// `install-rust.sh` as this build shipped it. The local operations
/// (`prime-agent update --rollback` and `--archive`) run this copy: they
/// need no network, and the script matches the layout this build was
/// installed with.
const BUNDLED_INSTALLER: &str = include_str!("../../../../install-rust.sh");

/// The payload binary's file name under `<prefix>/share/prime-agent`.
const PAYLOAD_BINARY: &str = if cfg!(windows) {
    "prime-agent.exe"
} else {
    "prime-agent"
};

/// The small-file budget for the script download (the script is a few KB;
/// a hung fetch must not hang the update).
const SCRIPT_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// The launcher `--version` probe budget (the same bound the release
/// probe uses; a hung launcher must not hang the report).
const LAUNCHER_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The installer script URL for an installer channel (`stable` | `beta`):
/// `<download base>/install-beta.sh` for beta, the official domain's
/// install endpoint otherwise. `PRIME_AGENT_RUST_INSTALLER_URL` overrides
/// both — tests serve their own script, a pinned install can point
/// elsewhere.
#[must_use]
pub fn installer_script_url(channel: Option<&str>) -> String {
    if let Ok(url) = std::env::var(ENV_INSTALLER_URL) {
        if !url.trim().is_empty() {
            return url;
        }
    }
    match installer_script_route(channel, STABLE_INSTALLER_FILE, BETA_INSTALLER_FILE) {
        Some(url) => url,
        None if channel == Some("beta") => format!(
            "{}/{BETA_INSTALLER_FILE}",
            download_base_url().trim_end_matches('/')
        ),
        None => OFFICIAL_INSTALLER_URL.to_string(),
    }
}

/// The recorded download base's script route, when the base carries one:
/// a GitHub releases base serves the installers as release assets (the
/// `latest/download/` redirect for stable, the rolling `nightly` release
/// for beta — the same file names the R2 layout publishes), so an update
/// installs from its recorded source instead of silently switching back
/// to the official domain. Every other base keeps the official logic: the
/// official stable script (the base answers it through the child's env
/// steer) and the nightly file under the base. `stable_file`/`beta_file`
/// pick the pair the route spells: the sh pair for the bash legs, the ps1
/// pair for the Windows PowerShell leg (the release pipeline renders both
/// pairs to the same places).
#[must_use]
fn installer_script_route(
    channel: Option<&str>,
    stable_file: &str,
    beta_file: &str,
) -> Option<String> {
    let base = download_base_url();
    if !super::release::is_github_releases_base(&base) {
        return None;
    }
    let base = base.trim_end_matches('/');
    Some(match channel {
        Some("beta") => format!("{base}/download/nightly/{beta_file}"),
        _ => format!("{base}/latest/download/{stable_file}"),
    })
}

/// The PowerShell leg's installer URL: the sh leg's route table spelled
/// with the ps1 file names, the same fallback shape (the official domain's
/// ps1 endpoint for stable, the nightly ps1 file under the base). The
/// `PRIME_AGENT_RUST_INSTALLER_URL` override never reaches here: the
/// interpreter decision refuses the ps1 leg while an override pins the
/// script file (a pinned sh script under PowerShell would be misparsed,
/// not refused).
#[cfg(windows)]
#[must_use]
fn installer_ps1_script_url(channel: Option<&str>) -> String {
    match installer_script_route(channel, STABLE_PS1_INSTALLER_FILE, BETA_PS1_INSTALLER_FILE) {
        Some(url) => url,
        None if channel == Some("beta") => format!(
            "{}/{BETA_PS1_INSTALLER_FILE}",
            download_base_url().trim_end_matches('/')
        ),
        None => OFFICIAL_PS1_INSTALLER_URL.to_string(),
    }
}

/// The download base `--check` reads the channel manifest from: the
/// installer's own knob, else the install marker's recorded `base` line
/// (an update follows the source the install came from), else the default.
/// An explicit env override always wins — the escape hatch when the
/// recorded base is gone.
#[must_use]
pub fn download_base_url() -> String {
    if let Some(base) = std::env::var(ENV_DOWNLOAD_BASE_URL)
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return base;
    }
    installed_base(&install_prefix()).unwrap_or_else(|| DEFAULT_DOWNLOAD_BASE_URL.to_string())
}

/// Whether this update run carries an explicit download-base override.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BaseOverride {
    /// A set, non-empty `PRIME_AGENT_DOWNLOAD_BASE_URL` — the escape hatch
    /// when the recorded source is gone; the user's own choice.
    Explicit,
    /// No override: the recorded install source (or the default) rides.
    Inherited,
}

impl BaseOverride {
    /// The caller's override state, read from the environment.
    fn from_env() -> Self {
        if std::env::var(ENV_DOWNLOAD_BASE_URL).is_ok_and(|value| !value.trim().is_empty()) {
            Self::Explicit
        } else {
            Self::Inherited
        }
    }
}

/// The effective base's provenance, surfaced for the update report and the
/// adoption event.
#[derive(Clone, Copy)]
pub enum DownloadBaseSource {
    /// The explicit env override.
    Env,
    /// The install marker's recorded `base` line.
    Marker,
    /// The official bucket (no override, no recorded base).
    Default,
}

impl DownloadBaseSource {
    /// The fixed telemetry vocabulary for this provenance.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Marker => "marker",
            Self::Default => "default",
        }
    }
}

/// The effective base's provenance: the explicit override, else the
/// recorded install source, else the default.
#[must_use]
pub fn download_base_source() -> DownloadBaseSource {
    if BaseOverride::from_env() == BaseOverride::Explicit {
        DownloadBaseSource::Env
    } else if installed_base(&install_prefix()).is_some() {
        DownloadBaseSource::Marker
    } else {
        DownloadBaseSource::Default
    }
}

/// The effective base's transport kind for reports and telemetry: never
/// the URL itself (a base can be a private mirror), only its shape.
#[derive(Clone, Copy)]
pub enum DownloadBaseKind {
    /// A GitHub releases base (the release-asset routes).
    GithubReleases,
    /// Plain http — only ever an explicitly consented source.
    Http,
    /// Any other https base.
    Https,
}

impl DownloadBaseKind {
    /// The fixed telemetry vocabulary for this base kind.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GithubReleases => "github-releases",
            Self::Http => "http",
            Self::Https => "https",
        }
    }

    /// The kind of one base URL.
    #[must_use]
    pub fn of(base_url: &str) -> Self {
        if super::release::is_github_releases_base(base_url) {
            Self::GithubReleases
        } else if base_url.starts_with("http://") {
            Self::Http
        } else {
            Self::Https
        }
    }
}

/// The install prefix the launcher probe reads (`PRIME_AGENT_RUST_PREFIX`,
/// the installer's own `~/.local` default).
#[must_use]
pub fn install_prefix() -> PathBuf {
    if let Some(prefix) = std::env::var_os(ENV_PREFIX) {
        if !prefix.is_empty() {
            return PathBuf::from(prefix);
        }
    }
    if let Some(prefix) = running_installer_prefix() {
        return prefix;
    }
    pa_types::platform::home_dir()
        .unwrap_or_default()
        .join(".local")
}

/// The target triple for one platform pair (`std::env::consts`' vocabulary:
/// the installer's own `uname -s`/`-m` mapping over the same four targets).
#[must_use]
pub fn target_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

/// The target triple this machine's update would install.
///
/// # Errors
/// Returns an error naming the machine when no continuous build is published for its platform.
pub fn current_target() -> Result<&'static str> {
    target_for(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
        anyhow!(
            "{}",
            no_build_message(std::env::consts::OS, std::env::consts::ARCH)
        )
    })
}

/// The refusal `--check` prints for one platform pair: the machine plus the
/// full published matrix, so an unsupported machine sees exactly what the
/// channel builds (the same message install-rust.sh's uname arm dies with).
#[must_use]
pub fn no_build_message(os: &str, arch: &str) -> String {
    format!(
        "no rust build is published for {os} {arch} (the release channel \
         builds aarch64-apple-darwin, x86_64-apple-darwin, \
         aarch64-unknown-linux-gnu, x86_64-unknown-linux-gnu, and \
         x86_64-pc-windows-msvc)"
    )
}

/// The commit a running version's `-continuous.<sha>` stamp names — the
/// identity `--check` compares against the latest run's `head_sha`.
#[must_use]
pub fn running_commit(version: &str) -> Option<&str> {
    let (_, commit) = version.trim().rsplit_once("-continuous.")?;
    let hex =
        (7..=40).contains(&commit.len()) && commit.bytes().all(|byte| byte.is_ascii_hexdigit());
    hex.then_some(commit)
}

/// Where the installer's output goes while it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallerOutput {
    /// The CLI's run: the installer's own progress streams to the user's terminal.
    Inherit,
    /// The TUI's run: the output is captured (the live frame stays intact)
    /// and the failure tail becomes the message; the run is detached from
    /// the terminal (it can never prompt).
    Capture,
}

/// What a completed installer run landed: the new build's version line, when the probe found one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub version: Option<String>,
}

/// Why an update run failed: the actionable message the surfaces print
/// (the CLI as its `Error:` line, the TUI as the error row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateFailure {
    pub message: String,
}

/// Run the takeover update with the environment's knobs (the install
/// prefix and the channel's script URL): the composition root's entry.
/// `channel` is the requested release channel (`stable` | `beta`); `None`
/// keeps the installed one.
///
/// # Errors
/// Returns the failure message for every non-installing outcome (see
/// [`run_installer_from`]).
pub async fn run_installer(
    channel: Option<&'static str>,
    output: InstallerOutput,
) -> std::result::Result<Installed, UpdateFailure> {
    let prefix = install_prefix();
    let channel = channel.or_else(|| installed_channel(&prefix));
    // The base this run installs from, remembered across the script's
    // publish: the official stable script's v1 marker would otherwise
    // erase it (see [`re_record_base`]).
    let installed_from = std::env::var(ENV_DOWNLOAD_BASE_URL)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| installed_base(&prefix));
    // The interpreter is resolved BEFORE the fetch: the script file the
    // URL serves must match the interpreter's invocation form (the sh
    // scripts ride stdin, the ps1 script rides -File). Git Bash keeps the
    // sh route (an sh-installed machine runs the same script it was
    // installed with); no trusted Git Bash takes the system PowerShell
    // and the ps1 route (#41).
    let interpreter = InstallerInterpreter::trusted()?;
    let url = match &interpreter {
        InstallerInterpreter::PowerShell(_) => installer_ps1_script_url(channel),
        InstallerInterpreter::Bash(_) => installer_script_url(channel),
    };
    let result = run_installer_from(&url, &interpreter, &prefix, channel, output).await;
    if result.is_ok() {
        if let Some(base) = &installed_from {
            re_record_base(&prefix, base);
        }
    }
    result
}

/// Re-record the download base this funnel just installed from, into the
/// marker the installer script wrote: the official stable script still
/// writes a v1 marker (no `base` line), so without this a mirror-base
/// install loses its recorded source to the FIRST successful update —
/// the exact silent reversion issue #17 opens with. A script that already
/// writes the base itself (this fork's scripts) passes through untouched,
/// and an encoded (ps1-written) marker is left alone — appending ASCII to
/// UTF-16 would corrupt it, and this funnel's scripts never write one.
fn re_record_base(prefix: &Path, base: &str) {
    use std::fmt::Write as _;
    let path = prefix.join("share/prime-agent/.prime-agent-install");
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    if bytes.contains(&0) || bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return;
    }
    let Ok(mut marker) = String::from_utf8(bytes) else {
        return;
    };
    if marker.lines().any(|line| line.trim().starts_with("base ")) {
        return;
    }
    if !marker.ends_with('\n') {
        marker.push('\n');
    }
    let _ = writeln!(marker, "base {base}");
    if base.starts_with("http://") {
        let _ = writeln!(marker, "allow-http 1");
    }
    // The install already succeeded; a failed provenance patch must never
    // turn it into a reported failure, so the write error is dropped by
    // design (the next run re-records).
    let _ = std::fs::write(&path, marker);
}

/// Run the takeover update from one explicit script URL and install
/// prefix: platform preflight, fetch the branch's installer script, and
/// exec it with the environment passed through (the installer's own
/// `PRIME_AGENT_RUST_*` knobs ride the process environment; the script
/// owns download, the TypeScript uninstall, the publish, and the
/// `~/.prime/agent` preserve). On success the
/// launcher's `--version` answers the new version; on failure the
/// previous install is kept (the script's own rollback covers a
/// mid-publish crash).
///
/// # Errors
/// Returns the failure message for every non-installing outcome.
pub(crate) async fn run_installer_from(
    url: &str,
    interpreter: &InstallerInterpreter,
    prefix: &Path,
    channel: Option<&'static str>,
    output: InstallerOutput,
) -> std::result::Result<Installed, UpdateFailure> {
    current_target().map_err(|error| UpdateFailure {
        message: format!("{error:#}"),
    })?;
    let (script, handle) = fetch_script(url, interpreter.script_extension())
        .await
        .map_err(|error| {
        let mut failure = UpdateFailure {
            message: format!("could not download the installer from {url}: {error:#}"),
        };
        // A release-asset route serves the installer the release pipeline
        // renders: stable rides the `latest/download/` redirect, beta the
        // rolling `nightly` release — route misses carry the rerun source
        // and the override knob the old refusal gate named (the beta route
        // 404s until the first nightly release exists).
        if url.contains("/latest/download/") || url.contains("/download/nightly/") {
            failure.message.push_str(
                "; the recorded release base serves the rendered installer pair itself (stable at the release's `latest/download/` assets, beta at the rolling `nightly` release) — rerun the installer from that source, or set PRIME_AGENT_DOWNLOAD_BASE_URL to override",
            );
        }
        failure
    })?;
    let result = execute_script(&script, &handle, &[], prefix, channel, output, interpreter).await;
    let _ = std::fs::remove_file(&script);
    result?;
    let version = launcher_version(prefix).await;
    Ok(Installed { version })
}

/// Run the bundled `install-rust.sh` with `args` (`--rollback`, or `--archive
/// <path>`) against the install under `prefix`. On success the version is
/// the one the published payload's install marker records — except on the
/// Windows payload handoff, where the caller IS the locked payload: the
/// child is spawned detached and no version is read (the installer's own
/// streamed output carries the outcome).
///
/// # Errors
/// Returns the failure message when the script cannot be written or run,
/// or exits nonzero (its own message already streamed to the terminal).
pub async fn run_bundled_installer(
    prefix: &Path,
    args: &[&std::ffi::OsStr],
) -> std::result::Result<Installed, UpdateFailure> {
    // The local modes run under the sh interpreter or not at all: the
    // bundled installer is the sh script, resolved before the write (the
    // script file's name rides the interpreter's form). On unix this is
    // /bin/sh — the interpreter [`InstallerInterpreter::trusted`]
    // resolves there too.
    let interpreter = InstallerInterpreter::trusted_sh()?;
    // `mut` serves the Windows handoff's post-pre-flight rewind; the
    // unix path only ever borrows the handle.
    #[cfg_attr(not(windows), allow(unused_mut))]
    let (script, mut handle) =
        write_script(BUNDLED_INSTALLER.as_bytes(), interpreter.script_extension()).map_err(
            |error| UpdateFailure {
                message: format!("could not write the bundled installer: {error:#}"),
            },
        )?;
    #[cfg(windows)]
    if caller_owns_payload(prefix) {
        // THE HANDOFF (Windows rename rule): this process's image sits inside
        // <prefix>/share/prime-agent and Windows refuses to rename a directory
        // that contains a running image, so the script's publish step can only
        // succeed after this process exits. The child is spawned without
        // waiting and inherits the terminal, so the installer's own steps and
        // error messages still print there. The script rides the child's stdin
        // (`bash -s --`, the curl|sh one-liner's own invocation form), so the
        // temp file's name is removed the moment the child holds its handle —
        // nothing of this run outlives the process, not even on a failed
        // spawn (std opens with FILE_SHARE_DELETE; a failure leaves nobody
        // reading and the removal is the same). The caller returns
        // immediately; the outcome rides the installer's own output.
        // The local-mode pre-flight (the exact-equivalence source): the
        // rollback's generations record rides the script's OWN prefix
        // spelling (cygpath + physical_path), and the archive's checks
        // (name, tar, payload, the version the packaged package.json
        // reports) are the script's staged run's own — no native mirror
        // can answer either faithfully, so the script itself answers, in
        // its PRIME_AGENT_ROLLBACK_CHECK / PRIME_AGENT_ARCHIVE_CHECK
        // modes, synchronously. Neither mode claims the lock or renames
        // the live tree (the rollback check only reads; the archive
        // check's extraction rides a temporary stage swept on its own
        // exit), so waiting never fights this process's locked image,
        // and the refusal they print is the one the handed-off run
        // would die with — the caller exits with the real status, and
        // only the publication itself stays async (the platform-
        // inherent residue).
        if let Err(error) = local_mode_preflight(&handle, prefix, args).await {
            let _ = std::fs::remove_file(&script);
            return Err(error);
        }
        // The pre-flight child read the script to EOF through its own
        // descriptor — a clone shares this handle's file position, so the
        // real child must start from the first byte again.
        handle.seek(SeekFrom::Start(0)).map_err(|error| {
            let _ = std::fs::remove_file(&script);
            UpdateFailure {
                message: format!("could not run the installer: {error}"),
            }
        })?;
        let mut command = installer_child(&interpreter, prefix, None);
        command
            .arg("-s")
            .arg("--")
            .args(args)
            .stdin(std::process::Stdio::from(handle));
        // THE PROCESS-CONTROL WALL (the in-house detached-spawn wrapper:
        // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW,
        // the product's own detached-survives-parent mapping): a console-
        // attached child dies with the caller's terminal close (the
        // CTRL_CLOSE broadcast) — mid-publish, exactly when the handoff
        // exists to let it finish — and Ctrl+C at the caller's terminal
        // must not reach the installer either. The inherited stdio
        // handles keep the installer's own steps printing to the
        // caller's window for as long as that window lives.
        crate::platform::process::set_new_process_group(command.as_std_mut());
        // THE PARENT EXIT: the payload unlock this whole handoff exists
        // for happens only when THIS process dies — the child gets the
        // pid and the script's head waits for it (bounded) before any
        // step touches the payload, so the release-before-publish
        // ordering is explicit instead of a spawn-timing coincidence.
        command.env(
            "PRIME_AGENT_INSTALLER_PARENT_PID",
            std::process::id().to_string(),
        );
        let spawned = command.spawn();
        let _ = std::fs::remove_file(&script);
        spawned.map_err(|error| UpdateFailure {
            message: format!("could not run the installer: {error}"),
        })?;
        return Ok(Installed { version: None });
    }
    let result = execute_script(
        &script,
        &handle,
        args,
        prefix,
        None,
        InstallerOutput::Inherit,
        &interpreter,
    )
    .await;
    let _ = std::fs::remove_file(&script);
    result?;
    Ok(Installed {
        version: installed_version(prefix),
    })
}

/// The install prefix of an installer-owned binary: `exe` is
/// `<prefix>/share/prime-agent/prime-agent` (`prime-agent.exe` on Windows)
/// and that payload carries the installer's marker. `None` for any other
/// binary (a managed release, a development build).
#[must_use]
fn installer_prefix_of(exe: &Path) -> Option<PathBuf> {
    let payload = exe.parent()?;
    let share = payload.parent()?;
    let owned = exe.file_name()? == PAYLOAD_BINARY
        && payload.file_name()? == "prime-agent"
        && share.file_name()? == "share";
    let prefix = share.parent()?;
    (owned && installed_channel(prefix).is_some()).then(|| prefix.to_path_buf())
}

/// The install prefix of the running binary when the installer owns it
/// (`<prefix>/share/prime-agent/prime-agent` in a marked payload).
#[must_use]
pub fn running_installer_prefix() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // Windows answers the module path already absolute; its canonical form
    // is the \\?\ verbatim spelling, which the installer cannot take as a
    // prefix.
    #[cfg(not(windows))]
    let exe = exe.canonicalize().ok()?;
    installer_prefix_of(&exe)
}

/// True when this process's own image is the payload binary of `prefix`
/// (`<prefix>/share/prime-agent/prime-agent.exe` with the install marker):
/// the Windows rename rule — a running image locks its directory, so an
/// installer child can only publish this payload after this process exits.
/// On unix a rename never fights a running image, so the answer is false.
#[must_use]
pub fn caller_owns_payload(prefix: &Path) -> bool {
    #[cfg(not(windows))]
    {
        let _ = prefix;
        false
    }
    #[cfg(windows)]
    {
        // The same helper the CLI read its prefix from: both sides then
        // spell the path identically (canonicalizing here would answer
        // the \\?\ verbatim form the prefix never equals).
        running_installer_prefix().is_some_and(|self_prefix| self_prefix == prefix)
    }
}

/// The synchronous local-mode pre-flight for the Windows handoff: run the
/// bundled installer ITSELF in its check mode — `PRIME_AGENT_ROLLBACK_CHECK`
/// or `PRIME_AGENT_ARCHIVE_CHECK` by the requested mode — and fail with the
/// script's own refusal when the operation cannot run. Neither mode ever
/// claims the publication lock or renames the live tree — the rollback
/// check only reads, and the archive check's extraction lives in a
/// temporary stage swept on its own exit — so waiting never fights the
/// locked image; and the answer is the script's OWN scan, the exact
/// equivalence the caller's exit code needs (once handed off, the
/// outcome can no longer be this process's exit status).
///
/// # Errors
/// Returns the pre-flight's captured refusal tail or the spawn failure.
#[cfg(windows)]
async fn local_mode_preflight(
    script: &std::fs::File,
    prefix: &Path,
    args: &[&std::ffi::OsStr],
) -> std::result::Result<(), UpdateFailure> {
    let interpreter = InstallerInterpreter::trusted_bash()?;
    let mut command = installer_child(&interpreter, prefix, None);
    command
        .arg("-s")
        .arg("--")
        .args(args)
        .stdin(std::process::Stdio::from(script.try_clone().map_err(
            |error| UpdateFailure {
                message: format!("could not run the installer: {error}"),
            },
        )?));
    if args.first().copied() == Some(std::ffi::OsStr::new("--rollback")) {
        command.env("PRIME_AGENT_ROLLBACK_CHECK", "1");
    } else {
        command.env("PRIME_AGENT_ARCHIVE_CHECK", "1");
    }
    let output = command.output().await.map_err(|error| UpdateFailure {
        message: format!("could not run the installer: {error}"),
    })?;
    if output.status.success() {
        return Ok(());
    }
    let refusal =
        output_tail(&output).unwrap_or_else(|| "the installer refused the request".to_string());
    Err(UpdateFailure { message: refusal })
}

/// Flatten an install marker's ps1-era encodings to plain bytes: strip a
/// leading UTF-8 or UTF-16 BOM, then drop the NULs (UTF-16's interleave;
/// this script's own ASCII markers contain none). UTF-16BE text loses its
/// byte pairing, but the marker content is ASCII, so both UTF-16 orders
/// read back as the same ASCII bytes.
fn normalize_marker_bytes(bytes: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let bytes = match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] | [0xFF, 0xFE, rest @ ..] | [0xFE, 0xFF, rest @ ..] => rest,
        _ => bytes,
    };
    if bytes.contains(&0) {
        std::borrow::Cow::Owned(
            bytes
                .iter()
                .copied()
                .filter(|byte| *byte != 0)
                .collect::<Vec<u8>>(),
        )
    } else {
        std::borrow::Cow::Borrowed(bytes)
    }
}

/// The installed payload's version, read from the install marker's
/// "version <v>" line — through the same encoding normalization as
/// [`installed_channel`]: a ps1-written marker can be UTF-16 or
/// BOM-prefixed, and the version line of an encoded marker must read too.
fn installed_version(prefix: &Path) -> Option<String> {
    let bytes = std::fs::read(prefix.join("share/prime-agent/.prime-agent-install")).ok()?;
    let normalized = normalize_marker_bytes(&bytes).into_owned();
    let marker = String::from_utf8_lossy(&normalized);
    let version = marker.lines().nth(1)?.strip_prefix("version ")?.trim();
    (!version.is_empty()).then(|| version.to_string())
}

/// The installed payload's download base, read from the install marker's
/// "base <url>" line (the installer records the base it resolved — the env
/// override or its own default — so updates can follow the install source
/// instead of silently switching to the official bucket). `None` when the
/// marker is absent, predates the line, or fails the same ownership gate
/// as [`installed_channel`]: a foreign marker must never steer the update
/// onto an attacker's download base. The value must carry an http(s)
/// scheme — anything else is a corrupt marker, not a base.
#[must_use]
pub fn installed_base(prefix: &Path) -> Option<String> {
    let bytes = std::fs::read(prefix.join("share/prime-agent/.prime-agent-install")).ok()?;
    let bytes = normalize_marker_bytes(&bytes);
    let marker = String::from_utf8_lossy(&bytes);
    // The same ownership proof as the channel read: a foreign marker never
    // steers the update onto an attacker's download base.
    installed_channel(prefix)?;
    let base = marker
        .lines()
        .find_map(|line| line.trim().strip_prefix("base "))?
        .trim();
    (base.starts_with("https://") || base.starts_with("http://")).then(|| base.to_string())
}

/// The installed payload's channel, read from the install marker (the
/// installer's own `.prime-agent-install` under the prefix's share dir;
/// its first line is "channel <name>"). `None` when the marker is absent
/// (a pre-marker install or a foreign tree) or carries no known channel —
/// the update then rides the fetched script's own default.
#[must_use]
pub fn installed_channel(prefix: &Path) -> Option<&'static str> {
    // The marker read mirrors install-rust.sh's marker_text: install.ps1's
    // Set-Content follows $PSDefaultParameterValues['*:Encoding'], so a
    // ps1-published live marker can be UTF-16 (NUL-interleaved, and its
    // BOM is not valid UTF-8, failing the whole read) or BOM-prefixed.
    // Flattening the NULs and stripping a leading BOM keeps the exact
    // prefix check working for this script's own ASCII writes and for the
    // ps1-written ones alike (a plain marker passes through untouched).
    let bytes = std::fs::read(prefix.join("share/prime-agent/.prime-agent-install")).ok()?;
    let bytes = normalize_marker_bytes(&bytes);
    let marker = String::from_utf8_lossy(&bytes);
    // The marker's first line must be the installer's OWN write shape —
    // "install-rust.sh channel <name>" — the exact prefix is the ownership
    // proof, so a foreign marker never steers the update onto a channel.
    let channel = marker
        .lines()
        .next()?
        .trim()
        .strip_prefix("install-rust.sh channel ")?
        .trim();
    match channel {
        "stable" => Some("stable"),
        "beta" => Some("beta"),
        _ => None,
    }
}

/// Fetch the installer script into a per-run temp file (the small-file
/// budget; the file is the exact bytes the branch serves) — the path with
/// the held handle [`write_script`] created it through.
///
/// # Errors
/// Returns an error when the request fails, answers a non-success status,
/// or the body cannot be read or written.
async fn fetch_script(url: &str, extension: &str) -> Result<(PathBuf, std::fs::File)> {
    let response = pa_ai::utils::tls::http_client_builder()
        .build()
        .expect("installer client")
        .get(url)
        .header("User-Agent", update_user_agent(env!("CARGO_PKG_VERSION")))
        .timeout(SCRIPT_FETCH_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("request {url}"))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .with_context(|| "read the installer script")?;
    if !status.is_success() {
        anyhow::bail!("download {url} returned {status}");
    }
    if bytes.is_empty() {
        anyhow::bail!("the installer script at {url} was empty");
    }
    write_script(&bytes, extension)
}
/// Write installer script bytes to a per-run temp file, created fresh
/// under an unguessable name and returned together with the OPEN HANDLE
/// the bytes were written through. The temp directory is writable by
/// every process of this user (and, under a permissive umask, by every
/// local user), so a script the child later re-opens BY NAME could be
/// swapped between this write and the run — executing the swap with the
/// credentials this funnel is hardened to guard. The stdin legs are
/// therefore given the handle only (as their stdin, the `curl|sh`
/// one-liner's own invocation form): the bytes that run are the bytes
/// written here, and the name only serves the post-run cleanup. The
/// PowerShell leg re-opens the file BY NAME (`-File`, the invocation
/// whose trust-domain difference [`InstallerInterpreter::invocation`]
/// argues): on Windows the temp directory is user-scoped — the same
/// trust domain as this process — and the unix shared-temp swap hazard
/// does not exist there.
fn write_script(bytes: &[u8], extension: &str) -> Result<(PathBuf, std::fs::File)> {
    let script = std::env::temp_dir().join(format!(
        "prime-agent-update-{}.{}",
        uuid::Uuid::now_v7().simple(),
        extension
    ));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // Owner-only: the default umask would leave the shared temp
        // directory's other users a write window into the file the
        // child is about to run.
        options.mode(0o600);
    }
    let mut script_file = options
        .open(&script)
        .with_context(|| format!("create {}", script.display()))?;
    script_file
        .write_all(bytes)
        .with_context(|| format!("write {}", script.display()))?;
    // Every consumer hands a clone of this handle to the child as its
    // stdin, and a clone shares the file position: start them all from
    // the first byte.
    script_file
        .seek(SeekFrom::Start(0))
        .with_context(|| format!("rewind {}", script.display()))?;
    Ok((script, script_file))
}

/// The trusted interpreter the funnel's child runs under, resolved before
/// the fetch: the script file the URL serves must match the interpreter's
/// invocation form (the sh scripts ride the child's stdin, the ps1 script
/// rides `-File`).
#[derive(Debug)]
pub(crate) enum InstallerInterpreter {
    /// The interpreter the sh legs run under: the absolute /bin/sh on
    /// unix (the `curl|sh` one-liner's own), Git Bash at a trusted root
    /// on Windows (hardcoded install-dir literals, never PATH).
    Bash(std::path::PathBuf),
    /// The system Windows PowerShell at its fixed install path: the ps1
    /// leg's only permitted interpreter — never `pwsh` and never the
    /// PowerShell 7 install dir (a PATH lookup); System32 is a fixed
    /// path. Constructed on Windows only; the unix build keeps the
    /// variant so the child-form match in [`Self::invocation`] stays
    /// shared.
    #[cfg_attr(not(windows), allow(dead_code))]
    PowerShell(std::path::PathBuf),
}

impl InstallerInterpreter {
    /// The interpreter's program path: the sh legs' `/bin/sh` on unix,
    /// the resolved executable on Windows.
    fn program(&self) -> &std::path::Path {
        match self {
            Self::Bash(shell) | Self::PowerShell(shell) => shell.as_path(),
        }
    }

    /// The script file extension the interpreter's invocation form
    /// serves: the stdin legs run the sh pair, the PowerShell leg the
    /// ps1 pair.
    fn script_extension(&self) -> &'static str {
        match self {
            Self::PowerShell(_) => "ps1",
            Self::Bash(_) => "sh",
        }
    }

    /// Complete `command` for this interpreter's invocation form: the
    /// stdin legs run `-s --` with the script riding the child's stdin
    /// from the handle [`write_script`] returned; the PowerShell leg
    /// runs `-File <script>` — the ps1 script's own header pins the file
    /// invocation as the exit-code-faithful form (`-Command -` reads
    /// stdin but does not carry `exit N` to the process exit code), and
    /// the by-name open rides the user-scoped Windows temp directory
    /// (the same trust domain as this process; the unix shared-temp
    /// swap hazard does not exist) with an unguessable name.
    fn invocation(
        &self,
        command: &mut tokio::process::Command,
        script_path: &Path,
        script: &std::fs::File,
        args: &[&std::ffi::OsStr],
    ) -> std::result::Result<(), UpdateFailure> {
        match self {
            Self::PowerShell(_) => {
                command
                    .arg("-NoProfile")
                    .arg("-NonInteractive")
                    .arg("-ExecutionPolicy")
                    .arg("Bypass")
                    .arg("-File")
                    .arg(script_path)
                    .args(args);
                Ok(())
            }
            Self::Bash(_) => {
                let stdin = std::process::Stdio::from(script.try_clone().map_err(|error| {
                    UpdateFailure {
                        message: format!("could not run the installer: {error}"),
                    }
                })?);
                command.arg("-s").arg("--").args(args).stdin(stdin);
                Ok(())
            }
        }
    }

    /// The system Windows PowerShell at its fixed install path:
    /// `%WINDIR%\System32\WindowsPowerShell\v1.0\powershell.exe` — a
    /// hardcoded system location satisfying the funnel's never-PATH trust
    /// rule (the same standard the bash roots and /bin/sh follow).
    #[cfg(windows)]
    fn system_power_shell(windir: Option<&std::ffi::OsStr>) -> Option<Self> {
        let windir = windir?;
        let powershell = std::path::PathBuf::from(windir)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        powershell.is_file().then_some(Self::PowerShell(powershell))
    }

    /// The trusted Git Bash root, when the machine has one (the ps1
    /// leg's predecessor in the network decision and the local modes'
    /// only interpreter — the bundled installer is the sh script, and
    /// the ps1 script has no local-mode equivalent).
    ///
    /// # Errors
    /// Returns the failure when no trusted Git Bash root exists.
    #[cfg(windows)]
    fn trusted_bash() -> std::result::Result<Self, UpdateFailure> {
        crate::platform::shell::default_kernel_bash_shell()
            .map(|shell| Self::Bash(std::path::PathBuf::from(shell)))
            .ok_or_else(|| UpdateFailure {
                // No shellPath guidance here: the funnel, like its unix
                // side (the hardcoded /bin/sh), resolves only the trusted
                // roots - the settings key serves the kernel shell, not
                // this privileged execution (the promise would lie).
                message: NO_TRUSTED_BASH_MESSAGE.to_string(),
            })
    }

    /// The trusted interpreter for the LOCAL modes (`--rollback`,
    /// `--archive`): /bin/sh on unix, Git Bash at a trusted root on
    /// Windows — the bundled installer is the sh script, and the ps1
    /// script has no local-mode equivalent (no rollback/archive
    /// switches): these modes run under sh or not at all.
    ///
    /// # Errors
    /// Returns the failure when no trusted sh interpreter exists.
    fn trusted_sh() -> std::result::Result<Self, UpdateFailure> {
        #[cfg(not(windows))]
        {
            return Ok(Self::Bash(PathBuf::from("/bin/sh")));
        }
        #[cfg(windows)]
        {
            Self::trusted_bash()
        }
    }

    /// The trusted interpreter for the NETWORK update: /bin/sh on unix;
    /// on Windows Git Bash at a trusted root first (an sh-installed
    /// machine keeps running the sh script), then the system Windows
    /// PowerShell and the ps1 route — hardcoded install-dir literals,
    /// never PATH and never `where bash.exe`/`pwsh` (the
    /// `get_shell_config` fallback that serves the kernel shell would let
    /// a repo-controlled `PATH` place the interpreter that receives the
    /// inherited `GITHUB_TOKEN`; the funnel must not use it).
    ///
    /// # Errors
    /// Returns the failure when neither leg exists.
    fn trusted() -> std::result::Result<Self, UpdateFailure> {
        #[cfg(not(windows))]
        {
            // The absolute /bin/sh — never PATH-resolved, so a poisoned
            // PATH cannot substitute the interpreter that runs the
            // installer with the inherited credentials.
            return Ok(Self::Bash(PathBuf::from("/bin/sh")));
        }
        #[cfg(windows)]
        {
            InterpreterCandidates {
                bash: crate::platform::shell::default_kernel_bash_shell()
                    .map(std::path::PathBuf::from)
                    .as_deref(),
                installer_url_override: std::env::var(ENV_INSTALLER_URL)
                    .is_ok_and(|url| !url.trim().is_empty()),
                windir: std::env::var_os("WINDIR")
                    .filter(|value| !value.is_empty())
                    .as_deref(),
            }
            .trusted()
        }
    }
}

/// The network update's interpreter decision over its raw candidate
/// inputs (the machine's bash probe, the override knob, the system
/// drive root): a named-field seam so the decision matrix is testable
/// without a machine's actual install layout.
#[cfg(windows)]
struct InterpreterCandidates<'a> {
    /// The trusted Git Bash root, when the machine has one.
    bash: Option<&'a std::path::Path>,
    /// `PRIME_AGENT_RUST_INSTALLER_URL` is set: it pins the script file
    /// itself, which runs under the bash leg or not at all (see
    /// [`installer_ps1_script_url`]).
    installer_url_override: bool,
    /// The `%WINDIR%` value, when set and non-empty (the PowerShell
    /// leg's fixed path rides it).
    windir: Option<&'a std::ffi::OsStr>,
}

#[cfg(windows)]
impl InterpreterCandidates<'_> {
    fn trusted(&self) -> std::result::Result<InstallerInterpreter, UpdateFailure> {
        if let Some(bash) = self.bash {
            return Ok(InstallerInterpreter::Bash(bash.to_path_buf()));
        }
        if self.installer_url_override {
            return Err(UpdateFailure {
                message: NO_TRUSTED_BASH_MESSAGE.to_string(),
            });
        }
        InstallerInterpreter::system_power_shell(self.windir)
            .ok_or_else(no_trusted_interpreter_failure)
    }
}

/// The local modes' refusal (the bundled sh installer has no ps1
/// equivalent): no trusted Git Bash root exists.
#[cfg(windows)]
const NO_TRUSTED_BASH_MESSAGE: &str = "could not run the installer: no Git Bash found at \
      the trusted install roots \
      (C:\\Program Files\\Git\\bin\\bash.exe); install \
      Git for Windows (https://git-scm.com/download/win) \
      to update from this machine";

/// The network update's refusal: neither the bash leg nor the PowerShell
/// fallback exists on this machine.
#[cfg(windows)]
fn no_trusted_interpreter_failure() -> UpdateFailure {
    UpdateFailure {
        message: "could not run the installer: no trusted interpreter found \
              (Git Bash at C:\\Program Files\\Git\\bin\\bash.exe, Windows \
              PowerShell at %WINDIR%\\System32\\WindowsPowerShell\\v1.0\\ \
              powershell.exe); install Git for Windows \
              (https://git-scm.com/download/win) to update from this machine"
            .to_string(),
    }
}

/// Build the installer's child command: the resolved interpreter
/// with the installer's env knobs (the caller completes the invocation
/// form through [`InstallerInterpreter::invocation`] — the stdin legs'
/// `-s --` form where the script rides the child's stdin from the handle
/// [`write_script`] returned, or the PowerShell leg's `-File` — and
/// `args` stay the child's positional parameters), so both wait modes
/// ([`execute_script`]) and the Windows handoff spawn this same command:
/// the install prefix riding the child's environment as the installer's
/// own knob, so the script publishes exactly where the probe looks —
/// every other `PRIME_AGENT_RUST_*` knob passes through untouched.
fn installer_child(
    interpreter: &InstallerInterpreter,
    prefix: &Path,
    channel: Option<&'static str>,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(interpreter.program());
    command.env(ENV_PREFIX, prefix);
    // The check knobs are the PRE-FLIGHT's alone: a user shell carrying
    // either would make the real run validate and exit without
    // publishing, so the shared builder strips both (the pre-flight sets
    // its own back after this).
    command.env_remove("PRIME_AGENT_ROLLBACK_CHECK");
    command.env_remove("PRIME_AGENT_ARCHIVE_CHECK");
    // The requested channel wins; otherwise the update stays on the channel
    // the install marker records (the fetched script's own default is the
    // stable render, so a beta install would silently switch channels).
    if let Some(channel) = channel.or_else(|| installed_channel(prefix)) {
        command.env(ENV_RELEASE_CHANNEL, channel);
    }
    // The same stickiness for the download base: when the caller's shell
    // did not choose one, the script installs from the source the install
    // marker records (every base is servable: the GitHub releases bases
    // ride the funnel's release-asset script routes, the rest answer the
    // official or base-rendered scripts the child steers by env).
    if BaseOverride::from_env() == BaseOverride::Inherited {
        if let Some(base) = installed_base(prefix) {
            command.env(ENV_DOWNLOAD_BASE_URL, base);
        }
    }
    command
}

/// Exec the script and wait for it: `interpreter`'s invocation form
/// ([`InstallerInterpreter::invocation`]) under [`installer_child`]'s
/// env wiring and `args` as the child's positional parameters. The
/// script's own die messages already streamed with
/// [`InstallerOutput::Inherit`]; with [`InstallerOutput::Capture`] the
/// tail becomes the failure message.
///
/// # Errors
/// Returns the failure when the script cannot start or exits nonzero.
async fn execute_script(
    script_path: &Path,
    script: &std::fs::File,
    args: &[&std::ffi::OsStr],
    prefix: &Path,
    channel: Option<&'static str>,
    output: InstallerOutput,
    interpreter: &InstallerInterpreter,
) -> std::result::Result<(), UpdateFailure> {
    let mut command = installer_child(interpreter, prefix, channel);
    interpreter.invocation(&mut command, script_path, script, args)?;
    match output {
        InstallerOutput::Inherit => {
            let status = command.status().await.map_err(|error| UpdateFailure {
                message: format!("could not run the installer: {error}"),
            })?;
            if status.success() {
                return Ok(());
            }
            Err(UpdateFailure {
                message: match status.code() {
                    Some(code) => format!(
                        "the installer exited with code {code}; the current install was kept"
                    ),
                    None => {
                        "the installer was terminated by a signal; the current install was kept"
                            .to_string()
                    }
                },
            })
        }
        InstallerOutput::Capture => {
            // The TUI owns the terminal: no controlling tty, and the
            // stdin legs' child stdin is the script itself — so the
            // script cannot open /dev/tty to prompt and never reads a
            // terminal; the PowerShell leg is -NonInteractive on top.
            #[cfg(unix)]
            crate::platform::process::set_new_session(command.as_std_mut());
            let captured = command.output().await.map_err(|error| UpdateFailure {
                message: format!("could not run the installer: {error}"),
            })?;
            if captured.status.success() {
                return Ok(());
            }
            let tail = output_tail(&captured);
            let detail = tail.map_or_else(String::new, |tail| format!(":\n{tail}"));
            Err(UpdateFailure {
                message: match captured.status.code() {
                    Some(code) => {
                        format!("the installer exited with code {code}{detail}")
                    }
                    None => {
                        format!("the installer was terminated by a signal{detail}")
                    }
                },
            })
        }
    }
}

/// The last informative lines of a captured installer run (die messages go
/// to stderr; a silent failure falls back to stdout's last line).
fn output_tail(captured: &std::process::Output) -> Option<String> {
    let stderr = String::from_utf8_lossy(&captured.stderr);
    let mut last_stderr: Vec<String> = stderr
        .lines()
        .rev()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .take(3)
        .collect();
    if last_stderr.is_empty() {
        let stdout = String::from_utf8_lossy(&captured.stdout);
        last_stderr = stdout
            .lines()
            .rev()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .take(1)
            .collect();
    }
    last_stderr.reverse();
    let joined = last_stderr.join("\n");
    (!joined.is_empty()).then_some(joined)
}

/// The installed launcher's `--version` answer: the takeover layout's `bin/prime-agent` first, the
/// pre-takeover `bin/prime-agent-rust` second, and only answers stamped `-continuous.<commit>`
/// count (the TS product's launcher never matches).
async fn launcher_version(prefix: &Path) -> Option<String> {
    // The launcher names: the .cmd twin on Windows (the sh-script launcher
    // cannot be exec'd by CreateProcess; Rust runs .cmd through cmd.exe),
    // the sh launcher pair on unix (the pre-takeover name second).
    #[cfg(windows)]
    const LAUNCHER_NAMES: [&str; 2] = ["prime-agent.cmd", "prime-agent"];
    #[cfg(not(windows))]
    const LAUNCHER_NAMES: [&str; 2] = ["prime-agent", "prime-agent-rust"];
    for name in LAUNCHER_NAMES {
        let launcher = prefix.join("bin").join(name);
        if !launcher.is_file() {
            continue;
        }
        let probe = tokio::process::Command::new(&launcher)
            .arg("--version")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let Ok(output) = tokio::time::timeout(LAUNCHER_PROBE_TIMEOUT, probe).await else {
            continue;
        };
        let Ok(output) = output else {
            continue;
        };
        let first = String::from_utf8_lossy(&output.stdout);
        let version = first.lines().next().map(str::trim).unwrap_or_default();
        if running_commit(version).is_some() {
            return Some(version.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The platform map `prime-agent update --check` names on every
    /// supported platform pair (the same set install-rust.sh's uname case
    /// resolves): the Windows pair ships the MSVC target, and the refusal
    /// covers every other pair with the full matrix in the message.
    #[test]
    fn target_for_covers_the_published_matrix() {
        assert_eq!(target_for("macos", "aarch64"), Some("aarch64-apple-darwin"));
        assert_eq!(target_for("macos", "x86_64"), Some("x86_64-apple-darwin"));
        assert_eq!(
            target_for("linux", "x86_64"),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(
            target_for("linux", "aarch64"),
            Some("aarch64-unknown-linux-gnu")
        );
        assert_eq!(
            target_for("windows", "x86_64"),
            Some("x86_64-pc-windows-msvc")
        );
        // The unsupported pairs refuse; the message names the machine and
        // the full matrix (the Windows build included, so a refused
        // Windows-adjacent machine sees the MSVC target it needs).
        assert_eq!(target_for("windows", "aarch64"), None);
        assert_eq!(target_for("freebsd", "x86_64"), None);
        let message = no_build_message("windows", "aarch64");
        assert!(message.contains("windows aarch64"), "{message}");
        assert!(message.contains("x86_64-pc-windows-msvc"), "{message}");
    }

    /// The ps1-era marker encodings read back as their ASCII content: a
    /// UTF-16 or BOM-prefixed live marker (install.ps1's Set-Content
    /// followed $`PSDefaultParameterValues`) keeps the installer-ownership
    /// gate working instead of sending a ps1-updated machine down the
    /// managed path.
    #[test]
    fn installed_channel_reads_the_ps1_encoded_marker() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = dir.path().join("prefix");
        let payload = prefix.join("share/prime-agent");
        std::fs::create_dir_all(&payload).unwrap();
        let cases: [(&str, Vec<u8>, &str); 4] = [
            (
                "utf-16le",
                {
                    let text: Vec<u16> = "install-rust.sh channel beta\nversion 1\n"
                        .encode_utf16()
                        .collect();
                    let mut bytes = vec![0xFF, 0xFE];
                    for unit in &text {
                        bytes.extend_from_slice(&unit.to_le_bytes());
                    }
                    bytes
                },
                "beta",
            ),
            (
                "utf-8-bom",
                {
                    let mut bytes = vec![0xEF, 0xBB, 0xBF];
                    bytes.extend_from_slice(b"install-rust.sh channel stable\nversion 1\n");
                    bytes
                },
                "stable",
            ),
            (
                "utf-16be",
                {
                    let text: Vec<u16> = "install-rust.sh channel stable\nversion 1\n"
                        .encode_utf16()
                        .collect();
                    let mut bytes = vec![0xFE, 0xFF];
                    for unit in &text {
                        bytes.extend_from_slice(&unit.to_be_bytes());
                    }
                    bytes
                },
                "stable",
            ),
            (
                "ascii",
                b"install-rust.sh channel stable\nversion 1\n".to_vec(),
                "stable",
            ),
        ];
        for (name, bytes, expected) in cases {
            std::fs::write(payload.join(".prime-agent-install"), &bytes).unwrap();
            assert_eq!(
                installed_channel(&prefix),
                Some(expected),
                "{name}: the ps1-era encoding reads back as its ASCII channel"
            );
            assert_eq!(
                installed_version(&prefix).as_deref(),
                Some("1"),
                "{name}: the version line of an encoded marker reads too"
            );
        }
    }

    /// The installed-marker channel read: a beta install's update must
    /// stay on beta (the marker the installer writes at publish carries
    /// the channel), a stable or pre-marker install rides the script's
    /// own default, and a foreign marker is never treated as a channel.
    #[test]
    fn installed_channel_reads_the_publish_marker() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = dir.path().join("prefix");
        assert_eq!(installed_channel(&prefix), None);
        let share = prefix.join("share/prime-agent");
        std::fs::create_dir_all(&share).unwrap();
        std::fs::write(
            share.join(".prime-agent-install"),
            "install-rust.sh channel beta\nversion 0.10.0\n",
        )
        .unwrap();
        assert_eq!(installed_channel(&prefix), Some("beta"));
        std::fs::write(
            share.join(".prime-agent-install"),
            "install-rust.sh channel stable\nversion 0.10.0\n",
        )
        .unwrap();
        assert_eq!(installed_channel(&prefix), Some("stable"));
        std::fs::write(share.join(".prime-agent-install"), "nightly\n").unwrap();
        assert_eq!(installed_channel(&prefix), None);
        std::fs::write(
            share.join(".prime-agent-install"),
            "other installer channel beta\n",
        )
        .unwrap();
        assert_eq!(installed_channel(&prefix), None);
        std::fs::write(share.join(".prime-agent-install"), "channel beta\n").unwrap();
        assert_eq!(installed_channel(&prefix), None);
    }

    /// The marker's recorded download base: a v2 marker carries the source
    /// the install came from (http and https alike — the transport warning
    /// is the update's to print), a pre-base marker reads as `None` (the
    /// update then rides the default bucket), and a foreign or corrupt
    /// marker never steers the funnel onto another base.
    #[test]
    fn installed_base_reads_the_publish_marker() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = dir.path().join("prefix");
        let share = prefix.join("share/prime-agent");
        std::fs::create_dir_all(&share).unwrap();
        let write = |text: &str| std::fs::write(share.join(".prime-agent-install"), text).unwrap();
        write("install-rust.sh channel stable\nversion 0.10.3\n");
        assert_eq!(installed_base(&prefix), None, "a v1 marker records no base");
        write(
            "install-rust.sh channel stable\nversion 0.10.3\nbase https://mirror.example/releases\n",
        );
        assert_eq!(
            installed_base(&prefix).as_deref(),
            Some("https://mirror.example/releases")
        );
        write(
            "install-rust.sh channel stable\nversion 0.10.3\nbase http://intranet.example/dl\nallow-http 1\nprefix /opt/prime\n",
        );
        assert_eq!(
            installed_base(&prefix).as_deref(),
            Some("http://intranet.example/dl"),
            "an http base reads back (the transport warning is the update's)"
        );
        write("install-rust.sh channel stable\nversion 0.10.3\nbase ftp://attacker.example\n");
        assert_eq!(
            installed_base(&prefix),
            None,
            "a non-http scheme is a corrupt marker, not a base"
        );
        write("other installer channel stable\nversion 1\nbase https://attacker.example\n");
        assert_eq!(
            installed_base(&prefix),
            None,
            "a foreign marker never steers the base"
        );
        // A ps1-encoded v2 marker reads back through the normalization.
        let text: Vec<u16> =
            "install-rust.sh channel stable\nversion 1\nbase https://mirror.example\n"
                .encode_utf16()
                .collect();
        let mut bytes = vec![0xFF, 0xFE];
        for unit in &text {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(share.join(".prime-agent-install"), bytes).unwrap();
        assert_eq!(
            installed_base(&prefix).as_deref(),
            Some("https://mirror.example"),
            "the ps1-era encoding reads back as its ASCII base"
        );
    }

    /// Only the payload binary of a marked installer tree is
    /// installer-owned; the marker's version line is the installed
    /// version.
    #[test]
    fn installer_prefix_of_needs_the_payload_path_and_the_marker() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = dir.path().join("prefix");
        let payload = prefix.join("share/prime-agent");
        std::fs::create_dir_all(&payload).unwrap();
        let exe = payload.join(PAYLOAD_BINARY);
        assert_eq!(installer_prefix_of(&exe), None, "no marker: not owned");
        std::fs::write(
            payload.join(".prime-agent-install"),
            "install-rust.sh channel beta\nversion 0.9.9-beta.10\n",
        )
        .unwrap();
        assert_eq!(installer_prefix_of(&exe), Some(prefix.clone()));
        assert_eq!(installed_version(&prefix).as_deref(), Some("0.9.9-beta.10"));
        // A binary elsewhere in the tree, or a managed release, is not.
        assert_eq!(installer_prefix_of(&payload.join("other")), None);
        assert_eq!(
            installer_prefix_of(&prefix.join("releases/0.9.8/prime-agent")),
            None
        );
    }

    /// The handoff gate never opens on unix: a rename never fights a
    /// running image, so the local operations always wait for the
    /// installer — even a fully staged, marker-bearing payload prefix
    /// stays closed (the platform alone decides).
    #[test]
    #[cfg(not(windows))]
    fn caller_owns_payload_stays_false_for_a_staged_prefix_on_unix() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = dir.path().join("prefix");
        let payload = prefix.join("share/prime-agent");
        std::fs::create_dir_all(&payload).unwrap();
        std::fs::write(
            payload.join(".prime-agent-install"),
            "install-rust.sh channel beta\nversion 0.9.9-beta.10\n",
        )
        .unwrap();
        assert!(
            !caller_owns_payload(&prefix),
            "unix renames never fight a running image"
        );
    }

    /// The handoff gate opens only when the marker-bearing payload path
    /// IS this process's own image: the test binary never runs from a
    /// payload dir, so even a fully staged prefix stays closed on
    /// Windows — the true arm needs a real self-installed run (the e2e
    /// covers the wait path).
    #[test]
    #[cfg(windows)]
    fn caller_owns_payload_needs_the_running_image_on_windows() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = dir.path().join("prefix");
        let payload = prefix.join("share/prime-agent");
        std::fs::create_dir_all(&payload).unwrap();
        std::fs::write(
            payload.join(".prime-agent-install"),
            "install-rust.sh channel beta\nversion 0.9.9-beta.10\n",
        )
        .unwrap();
        assert!(
            !caller_owns_payload(&prefix),
            "a staged prefix never names this process's own image"
        );
    }

    /// Serve `body` over one plain HTTP request (the hermetic source the
    /// funnel fetches its mock installer from): bind an ephemeral loopback
    /// socket, answer the first request, return the URL the funnel uses.
    /// Unix only: its callers are the unix shell-installer tests.
    #[cfg(unix)]
    fn serve(body: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let address = listener.local_addr().expect("local address");
        std::thread::spawn(move || {
            use std::io::Write as _;
            if let Ok((mut stream, _)) = listener.accept() {
                // Read the request head first: answering before the request is drained can
                // reset the connection mid-write (the client then reports a broken response).
                let mut head = Vec::new();
                let mut byte = [0_u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match std::io::Read::read(&mut stream, &mut byte) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => head.push(byte[0]),
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{address}/install.sh")
    }

    /// The sandboxed preserve fixture: a session file under a home the installer
    /// world shares, with its exact bytes snapshotted for the byte-identity assert.
    struct Preserve {
        // The byte-identity readers are the unix `assert_untouched`
        // checks; on other platforms the fixture only stages the file.
        #[cfg_attr(not(unix), allow(dead_code))]
        session_file: PathBuf,
        #[cfg_attr(not(unix), allow(dead_code))]
        bytes: Vec<u8>,
    }

    impl Preserve {
        fn new(root: &Path) -> Self {
            let session_file = root
                .join("home")
                .join(".prime/agent/sessions/session.jsonl");
            std::fs::create_dir_all(session_file.parent().expect("session dir"))
                .expect("session dir");
            let bytes =
                b"{\"type\":\"user_message\",\"content\":\"the session the update must preserve\"}\n"
                    .to_vec();
            std::fs::write(&session_file, &bytes).expect("write session file");
            Self {
                session_file,
                bytes,
            }
        }

        /// Unix only: the update-flow tests that read the snapshot sit behind the unix gate.
        #[cfg(unix)]
        fn assert_untouched(&self) {
            let observed =
                std::fs::read(&self.session_file).expect("session file survives the update");
            assert_eq!(
                observed, self.bytes,
                "the session store is byte-identical after the update"
            );
        }
    }

    /// The mock installer the funnel downloads in the tests: it installs a launcher
    /// that answers a stamped `--version`, exactly the takeover's contract. Unix only.
    #[cfg(unix)]
    const MOCK_INSTALLER: &str = r#"#!/bin/sh
set -eu
# Fails only under a controlling terminal (a developer run or `script`); headless CI has none.
if ( : <>/dev/tty ) 2>/dev/null || [ -t 0 ]; then echo "the captured installer reached the terminal" >&2; exit 9; fi
mkdir -p "${PRIME_AGENT_RUST_PREFIX}/bin"
printf '#!/bin/sh\necho "9.9.9-continuous.0123456789abcdef"\n' > "${PRIME_AGENT_RUST_PREFIX}/bin/prime-agent"
chmod 0755 "${PRIME_AGENT_RUST_PREFIX}/bin/prime-agent"
printf '%s' "${PRIME_AGENT_RELEASE_CHANNEL:-}" > "${PRIME_AGENT_RUST_PREFIX}/channel"
echo "installed: 9.9.9-continuous.0123456789abcdef"
"#;

    /// The pre-takeover installer: the launcher carries the legacy `prime-agent-rust`
    /// name the probe still accepts. Unix only: same sh-script class as [`MOCK_INSTALLER`].
    #[cfg(unix)]
    const LEGACY_INSTALLER: &str = r#"#!/bin/sh
set -eu
mkdir -p "${PRIME_AGENT_RUST_PREFIX}/bin"
printf '#!/bin/sh\necho "9.9.8-continuous.fedcba9876543210"\n' > "${PRIME_AGENT_RUST_PREFIX}/bin/prime-agent-rust"
chmod 0755 "${PRIME_AGENT_RUST_PREFIX}/bin/prime-agent-rust"
echo "installed: 9.9.8-continuous.fedcba9876543210"
"#;

    fn sandbox() -> (tempfile::TempDir, Preserve, PathBuf) {
        let root = tempfile::tempdir().expect("sandbox root");
        let preserve = Preserve::new(root.path());
        let prefix = root.path().join("prefix/.local");
        std::fs::create_dir_all(&prefix).expect("prefix");
        (root, preserve, prefix)
    }

    /// The update knobs follow the channel, the override, and the recorded
    /// source: stable fetches the official domain's install endpoint,
    /// nightly (beta) fetches `install-beta.sh` from the download base (the
    /// domain forwards only `install.sh`), the override pins both — and the
    /// download base itself rides the install marker's recorded `base`
    /// line, with the explicit override as the escape hatch and the
    /// installer child carrying the recorded base to the script it execs.
    /// The knob probes read process env (the download base, the prefix,
    /// the override); the tests that touch it serialize on one lock.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn the_update_knobs_follow_the_channel_and_the_recorded_source() {
        let _env = env_lock();
        // The knobs are SAVED and RESTORED around the probe: the env is
        // process-global, so a pinned value in the surrounding
        // environment must survive it.
        let prior_override = std::env::var_os(ENV_INSTALLER_URL);
        let prior_base = std::env::var_os(ENV_DOWNLOAD_BASE_URL);
        let prior_prefix = std::env::var_os(ENV_PREFIX);
        std::env::remove_var(ENV_INSTALLER_URL);
        std::env::remove_var(ENV_DOWNLOAD_BASE_URL);
        // The prefix is pinned to a SANDBOX from the start: the download
        // base now reads the install marker through the prefix, so an
        // unpinned probe would consult the real home's marker and depend
        // on the machine it runs on.
        let (root, _preserve, prefix) = sandbox();
        std::env::set_var(ENV_PREFIX, &prefix);
        assert_eq!(
            installer_script_url(Some("stable")),
            "https://app.primeintellect.ai/prime-agent/install.sh"
        );
        assert_eq!(installer_script_url(None), OFFICIAL_INSTALLER_URL);
        assert_eq!(
            installer_script_url(Some("beta")),
            "https://pub-728493de92a943e2a9b2d17b4719f318.r2.dev/install-beta.sh"
        );
        std::env::set_var(ENV_DOWNLOAD_BASE_URL, "https://mirror.example/");
        assert_eq!(
            installer_script_url(Some("beta")),
            "https://mirror.example/install-beta.sh"
        );
        assert_eq!(installer_script_url(Some("stable")), OFFICIAL_INSTALLER_URL);
        std::env::set_var(ENV_INSTALLER_URL, "http://127.0.0.1:9/pinned.sh");
        assert_eq!(
            installer_script_url(Some("beta")),
            "http://127.0.0.1:9/pinned.sh"
        );
        assert_eq!(
            installer_script_url(Some("stable")),
            "http://127.0.0.1:9/pinned.sh"
        );
        // The download base follows the recorded source: with the knobs
        // clean, a v2 marker's `base` line is the effective base, and the
        // installer child carries it to the script it execs (the child's
        // explicit env, not the inherited one).
        std::env::remove_var(ENV_INSTALLER_URL);
        std::env::remove_var(ENV_DOWNLOAD_BASE_URL);
        let share = prefix.join("share/prime-agent");
        std::fs::create_dir_all(&share).unwrap();
        std::fs::write(
            share.join(".prime-agent-install"),
            "install-rust.sh channel stable\nversion 0.10.3\nbase https://mirror.example/dl\n",
        )
        .unwrap();
        std::env::set_var(ENV_PREFIX, &prefix);
        assert_eq!(
            download_base_url(),
            "https://mirror.example/dl",
            "without an override the recorded source rides"
        );
        let child_base = || {
            installer_child(
                &InstallerInterpreter::Bash(PathBuf::from("/bin/sh")),
                &prefix,
                None,
            )
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == ENV_DOWNLOAD_BASE_URL)
            .and_then(|(_, value)| value)
            .map(std::ffi::OsStr::to_string_lossy)
            .map(|value| value.to_string())
        };
        assert_eq!(
            child_base().as_deref(),
            Some("https://mirror.example/dl"),
            "the recorded base rides the child's env"
        );
        // The explicit override wins for the base too, and the child then
        // inherits the caller's choice instead of pinning the marker's.
        std::env::set_var(ENV_DOWNLOAD_BASE_URL, "https://explicit.example/");
        assert_eq!(
            download_base_url(),
            "https://explicit.example/",
            "the explicit override is the escape hatch"
        );
        assert_eq!(
            child_base(),
            None,
            "an overridden base is inherited, never re-pinned from the marker"
        );
        // A recorded GitHub releases base serves the installer script from
        // its release assets: the `latest/download/` redirect for stable,
        // the rolling `nightly` release for beta — the funnel installs
        // from the recorded source, never silently back to the official
        // domain (issue #17's PR-2).
        std::env::remove_var(ENV_DOWNLOAD_BASE_URL);
        std::fs::write(
            share.join(".prime-agent-install"),
            "install-rust.sh channel stable\nversion 0.10.3\nbase https://github.com/jinnnyang/prime-agent/releases\n",
        )
        .unwrap();
        assert_eq!(
            installer_script_url(Some("stable")),
            "https://github.com/jinnnyang/prime-agent/releases/latest/download/install.sh"
        );
        assert_eq!(
            installer_script_url(Some("beta")),
            "https://github.com/jinnnyang/prime-agent/releases/download/nightly/install-beta.sh"
        );
        // An explicit installer pin still wins over the recorded base's
        // routes (the pinned URL is what runs).
        std::env::set_var(ENV_INSTALLER_URL, "http://127.0.0.1:9/pinned.sh");
        assert_eq!(installer_script_url(None), "http://127.0.0.1:9/pinned.sh");
        drop(root);
        for (name, prior) in [
            (ENV_INSTALLER_URL, prior_override),
            (ENV_DOWNLOAD_BASE_URL, prior_base),
            (ENV_PREFIX, prior_prefix),
        ] {
            match prior {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    /// The PowerShell leg's route table mirrors the sh leg's: the GitHub
    /// releases bases serve the ps1 pair as release assets at the same
    /// paths, and every other base keeps the official/beta-file fallback.
    #[test]
    #[cfg(windows)]
    fn the_ps1_route_mirrors_the_sh_route_table() {
        let _env = env_lock();
        let prior_base = std::env::var_os(ENV_DOWNLOAD_BASE_URL);
        let prior_prefix = std::env::var_os(ENV_PREFIX);
        let (root, _preserve, prefix) = sandbox();
        std::env::set_var(ENV_PREFIX, &prefix);
        std::env::set_var(
            ENV_DOWNLOAD_BASE_URL,
            "https://github.com/jinnnyang/prime-agent/releases",
        );
        assert_eq!(
            installer_ps1_script_url(None),
            "https://github.com/jinnnyang/prime-agent/releases/latest/download/install.ps1"
        );
        assert_eq!(
            installer_ps1_script_url(Some("beta")),
            "https://github.com/jinnnyang/prime-agent/releases/download/nightly/install-beta.ps1"
        );
        std::env::set_var(ENV_DOWNLOAD_BASE_URL, "https://mirror.example/");
        assert_eq!(
            installer_ps1_script_url(Some("beta")),
            "https://mirror.example/install-beta.ps1"
        );
        assert_eq!(installer_ps1_script_url(None), OFFICIAL_PS1_INSTALLER_URL);
        drop(root);
        for (name, prior) in [
            (ENV_DOWNLOAD_BASE_URL, prior_base),
            (ENV_PREFIX, prior_prefix),
        ] {
            match prior {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    /// The system PowerShell leg resolves only at its fixed System32
    /// path, and only when the file exists there.
    #[test]
    #[cfg(windows)]
    fn the_system_power_shell_resolves_only_at_the_fixed_file() {
        assert!(InstallerInterpreter::system_power_shell(None).is_none());
        let root = tempfile::tempdir().expect("windir sandbox");
        let v1 = root.path().join("System32/WindowsPowerShell/v1.0");
        std::fs::create_dir_all(&v1).expect("the v1.0 dir");
        assert!(InstallerInterpreter::system_power_shell(Some(root.path().as_os_str())).is_none());
        std::fs::write(v1.join("powershell.exe"), b"stub").expect("the exe");
        let expected = v1.join("powershell.exe");
        match InstallerInterpreter::system_power_shell(Some(root.path().as_os_str())) {
            Some(InstallerInterpreter::PowerShell(resolved)) if resolved == expected => {}
            other => panic!("the fixed path answers exactly once: {other:?}"),
        }
    }

    /// The network decision's matrix: bash wins whenever it exists (an
    /// sh-installed machine keeps the sh script, override or not); with
    /// no bash, the override knob refuses the ps1 leg while a real
    /// WINDIR answers the system PowerShell.
    #[test]
    #[cfg(windows)]
    fn the_network_decision_matrix_orders_the_legs() {
        let bash = PathBuf::from("C:/Program Files/Git/bin/bash.exe");
        // (a) bash present: the sh leg wins, override or not.
        for override_pinned in [false, true] {
            let candidates = InterpreterCandidates {
                bash: Some(&bash),
                installer_url_override: override_pinned,
                windir: None,
            };
            match candidates.trusted().expect("the bash leg resolves") {
                InstallerInterpreter::Bash(resolved) => {
                    assert_eq!(resolved, bash);
                }
                other @ InstallerInterpreter::PowerShell(_) => {
                    panic!("the bash leg answers first: {other:?}")
                }
            }
        }
        // (b) no bash + override: refused (a pinned script runs under sh
        // or not at all), even when a PowerShell would be available.
        let sandbox = tempfile::tempdir().expect("the windir sandbox");
        let v1 = sandbox.path().join("System32/WindowsPowerShell/v1.0");
        std::fs::create_dir_all(&v1).expect("the v1.0 dir");
        std::fs::write(v1.join("powershell.exe"), b"stub").expect("the exe");
        let refused = InterpreterCandidates {
            bash: None,
            installer_url_override: true,
            windir: Some(sandbox.path().as_os_str()),
        }
        .trusted()
        .expect_err("the override refuses the ps1 leg");
        assert!(
            refused.message.contains("no Git Bash found"),
            "the refusal names the bash roots: {}",
            refused.message
        );
        // (c) no bash + no override: the system PowerShell leg answers.
        let candidates = InterpreterCandidates {
            bash: None,
            installer_url_override: false,
            windir: Some(sandbox.path().as_os_str()),
        };
        match candidates.trusted().expect("the ps1 leg resolves") {
            InstallerInterpreter::PowerShell(resolved) => {
                assert_eq!(resolved, v1.join("powershell.exe"));
            }
            other @ InstallerInterpreter::Bash(_) => {
                panic!("the ps1 leg answers without bash: {other:?}")
            }
        }
    }

    /// The PowerShell child runs the FILE form: `-File` with the temp
    /// script's path, no stdin, the interpreter's program (the fixed
    /// System32 exe — never a PATH name).
    #[test]
    #[cfg(windows)]
    fn the_power_shell_child_runs_the_file_form() {
        let program = PathBuf::from("C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe");
        let interpreter = InstallerInterpreter::PowerShell(program.clone());
        // A sandboxed dir instead of the shared temp directory: the fixed
        // test name cannot collide with a concurrent run, and the file is
        // swept even when an assert panics (the tempdir drops with the
        // unwinding locals).
        let sandbox = tempfile::tempdir().expect("the form sandbox");
        let script_path = sandbox.path().join("form-test.ps1");
        let script = std::fs::File::options()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&script_path)
            .expect("the temp script");
        let mut command = installer_child(&interpreter, Path::new("prefix"), None);
        interpreter
            .invocation(&mut command, &script_path, &script, &[])
            .expect("the form applies");
        let std_command = command.as_std();
        assert_eq!(std_command.get_program(), program.as_os_str());
        let args: Vec<_> = std_command
            .get_args()
            .map(std::ffi::OsStr::to_string_lossy)
            .collect();
        assert_eq!(
            args,
            vec![
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                script_path.to_string_lossy().as_ref(),
            ],
            "the ps1 leg runs the file form with no stdin"
        );
    }

    /// The bash child keeps the stdin form: `-s --` with the script
    /// riding stdin (the curl|sh one-liner's own invocation), so the
    /// fallback never displaces the sh-installed machine's continuity.
    #[test]
    #[cfg(windows)]
    fn the_bash_child_keeps_the_stdin_form() {
        let interpreter =
            InstallerInterpreter::Bash(PathBuf::from("C:/Program Files/Git/bin/bash.exe"));
        let (script, handle) = write_script(b"stub", "sh").expect("the temp script");
        let mut command = installer_child(&interpreter, Path::new("prefix"), None);
        interpreter
            .invocation(&mut command, &script, &handle, &[])
            .expect("the form applies");
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(std::ffi::OsStr::to_string_lossy)
            .collect();
        assert_eq!(
            args,
            vec!["-s", "--"],
            "the bash leg rides stdin (the script is never opened by name)"
        );
        let _ = std::fs::remove_file(&script);
    }

    #[test]
    fn the_target_matrix_covers_the_published_builds() {
        assert_eq!(target_for("macos", "aarch64"), Some("aarch64-apple-darwin"));
        assert_eq!(target_for("macos", "x86_64"), Some("x86_64-apple-darwin"));
        assert_eq!(
            target_for("linux", "x86_64"),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(
            target_for("linux", "aarch64"),
            Some("aarch64-unknown-linux-gnu")
        );
        // The Windows pair ships the MSVC build (the release channel's
        // fifth target since the windows ship).
        assert_eq!(
            target_for("windows", "x86_64"),
            Some("x86_64-pc-windows-msvc")
        );
        // Windows ARM64 (the only unsupported Windows shape) and every
        // other OS refuse.
        assert_eq!(target_for("windows", "aarch64"), None);
        assert!(
            current_target().is_ok(),
            "the test matrix runs on a supported platform"
        );
    }

    #[test]
    fn running_commit_reads_the_continuous_stamp() {
        assert_eq!(
            running_commit("0.5.2-continuous.07f42eaa3a6159f942c6c24beb0352ce120a192c"),
            Some("07f42eaa3a6159f942c6c24beb0352ce120a192c")
        );
        assert_eq!(
            running_commit(" 9.9.9-continuous.0123456 "),
            Some("0123456")
        );
        assert_eq!(
            running_commit("0.5.2"),
            None,
            "a dev build carries no stamp"
        );
        assert_eq!(
            running_commit("2.31.4"),
            None,
            "the TypeScript product's version never reads as a rust commit"
        );
    }

    /// The post-install provenance patch: the official stable script's v1
    /// write (no `base` line) gets the run's base re-recorded — the
    /// silent-reversion bug the spec review caught — while a script that
    /// already writes the base, an http source's consent line, and an
    /// encoded marker each behave as designed.
    #[test]
    fn re_record_base_restores_the_source_the_official_script_erased() {
        let dir = tempfile::TempDir::new().unwrap();
        let prefix = dir.path().join("prefix");
        let share = prefix.join("share/prime-agent");
        std::fs::create_dir_all(&share).unwrap();
        let marker = share.join(".prime-agent-install");
        // The v1 marker the official stable script wrote over the recorded
        // source.
        std::fs::write(&marker, "install-rust.sh channel stable\nversion 9.9.9\n").unwrap();
        re_record_base(&prefix, "https://mirror.example/dl");
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "install-rust.sh channel stable\nversion 9.9.9\nbase https://mirror.example/dl\n",
            "the erased source is re-recorded after the v1 write"
        );
        // An http source records its transport consent too.
        std::fs::write(&marker, "install-rust.sh channel stable\nversion 9.9.9\n").unwrap();
        re_record_base(&prefix, "http://intranet.example/dl");
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "install-rust.sh channel stable\nversion 9.9.9\nbase http://intranet.example/dl\nallow-http 1\n",
            "an http source re-records with its consent line"
        );
        // A script-written v2 marker passes through untouched.
        std::fs::write(
            &marker,
            "install-rust.sh channel stable\nversion 9.9.9\nbase https://fork.example/releases\n",
        )
        .unwrap();
        re_record_base(&prefix, "https://mirror.example/dl");
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "install-rust.sh channel stable\nversion 9.9.9\nbase https://fork.example/releases\n",
            "a script that recorded its own base is never overwritten"
        );
        // An encoded (ps1-written) marker is left alone: appending ASCII to
        // UTF-16 would corrupt it.
        let text: Vec<u16> = "install-rust.sh channel stable\nversion 1\n"
            .encode_utf16()
            .collect();
        let mut bytes = vec![0xFF, 0xFE];
        for unit in &text {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&marker, &bytes).unwrap();
        re_record_base(&prefix, "https://mirror.example/dl");
        assert_eq!(
            std::fs::read(&marker).unwrap(),
            bytes,
            "an encoded marker is never patched"
        );
    }

    /// The requested channel (an explicit flag or the saved setting) wins
    /// over the install marker; without one the marker's channel rides.
    #[tokio::test]
    #[cfg(unix)]
    async fn the_requested_channel_wins_over_the_install_marker() {
        let (root, _preserve, prefix) = sandbox();
        let share = prefix.join("share/prime-agent");
        std::fs::create_dir_all(&share).unwrap();
        std::fs::write(
            share.join(".prime-agent-install"),
            "install-rust.sh channel beta\nversion 0.10.0\n",
        )
        .unwrap();
        let url = serve(MOCK_INSTALLER);
        run_installer_from(
            &url,
            &mock_interpreter(),
            &prefix,
            Some("stable"),
            InstallerOutput::Capture,
        )
        .await
        .expect("the funnel installs");
        assert_eq!(
            std::fs::read_to_string(prefix.join("channel")).unwrap(),
            "stable"
        );
        let url = serve(MOCK_INSTALLER);
        run_installer_from(
            &url,
            &mock_interpreter(),
            &prefix,
            None,
            InstallerOutput::Capture,
        )
        .await
        .expect("the funnel installs");
        assert_eq!(
            std::fs::read_to_string(prefix.join("channel")).unwrap(),
            "beta"
        );
        drop(root);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn the_funnel_runs_the_downloaded_installer_and_preserves_the_session_store() {
        let (root, preserve, prefix) = sandbox();
        let installed = run_installer_from(
            &serve(MOCK_INSTALLER),
            &mock_interpreter(),
            &prefix,
            None,
            InstallerOutput::Capture,
        )
        .await
        .expect("the funnel installs the mock build");
        assert_eq!(
            installed.version.as_deref(),
            Some("9.9.9-continuous.0123456789abcdef"),
            "the probe reads the launcher's own --version answer"
        );
        let launcher = prefix.join("bin/prime-agent");
        assert!(launcher.is_file(), "the launcher landed");
        let mode = std::fs::metadata(&launcher).expect("launcher metadata");
        assert!(unix_mode_is_executable(&mode), "the launcher is executable");
        preserve.assert_untouched();
        drop(root);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn the_probe_still_reads_a_pre_takeover_launchers_version() {
        let (root, _preserve, prefix) = sandbox();
        let installed = run_installer_from(
            &serve(LEGACY_INSTALLER),
            &mock_interpreter(),
            &prefix,
            None,
            InstallerOutput::Capture,
        )
        .await
        .expect("the funnel installs the legacy-named build");
        assert_eq!(
            installed.version.as_deref(),
            Some("9.9.8-continuous.fedcba9876543210"),
            "the pre-takeover launcher answers through the probe"
        );
        drop(root);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn a_failed_installer_keeps_the_previous_install_and_reports_the_error() {
        let (root, preserve, prefix) = sandbox();
        std::fs::create_dir_all(prefix.join("bin")).expect("bin dir");
        let previous = prefix.join("bin/prime-agent");
        std::fs::write(&previous, "#!/bin/sh\necho 9.9.7-continuous.0000001\n")
            .expect("previous launcher");
        make_executable(&previous);

        let failure = run_installer_from(
            &serve("#!/bin/sh\necho 'install-rust.sh: the artifact download failed' >&2\nexit 3\n"),
            &mock_interpreter(),
            &prefix,
            None,
            InstallerOutput::Capture,
        )
        .await
        .expect_err("the failing script fails the update");
        assert!(
            failure.message.contains("the artifact download failed"),
            "the failure carries the script's own die message: {}",
            failure.message
        );
        assert!(failure.message.contains("code 3"), "{}", failure.message);
        let version = launcher_version(&prefix).await;
        assert_eq!(version.as_deref(), Some("9.9.7-continuous.0000001"));
        preserve.assert_untouched();
        drop(root);
    }

    #[tokio::test]
    async fn an_unfetchable_script_fails_without_installing() {
        let (_root, _preserve, prefix) = sandbox();
        let failure = run_installer_from(
            "http://127.0.0.1:9/install-rust.sh",
            &mock_interpreter(),
            &prefix,
            None,
            InstallerOutput::Capture,
        )
        .await
        .expect_err("the unreachable URL fails the update");
        assert!(
            failure.message.contains("could not download the installer"),
            "the failure names the fetch: {}",
            failure.message
        );
        assert!(
            !prefix.join("bin").exists(),
            "nothing landed from a failed fetch"
        );
    }

    /// The interpreter the unix funnel tests run under: the tests' mock
    /// scripts are sh. Windows callers never execute (their fetches fail
    /// first), but the parameter still needs a value.
    fn mock_interpreter() -> InstallerInterpreter {
        #[cfg(not(windows))]
        {
            InstallerInterpreter::Bash(PathBuf::from("/bin/sh"))
        }
        #[cfg(windows)]
        {
            // Never executed (the mock fetches fail first); the value only
            // satisfies the interpreter parameter.
            InstallerInterpreter::Bash(PathBuf::from("bash"))
        }
    }

    #[cfg(unix)]
    fn unix_mode_is_executable(metadata: &std::fs::Metadata) -> bool {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(permissions.mode() | 0o755);
        std::fs::set_permissions(path, permissions).expect("chmod");
    }
}
