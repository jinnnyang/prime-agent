//! The versioned-store concern: the active-store link the venv path resolves
//! through, the build-aside stores a rebuild writes into, the live-kernel pid
//! markers the reclamation consults, and the stale-store reclamation. A
//! rebuild never deletes the store a live kernel runs from: the new store is
//! built aside under its own name and the link swaps under the bootstrap
//! lock, so a running interpreter keeps executing from the store it opened.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context};

use crate::platform::process::pid_exists;

/// A published store younger than this is never reclaimed: the spawner that
/// swapped it in may not have started its kernel yet, and the marker that
/// would protect it lands only after the spawn.
pub(in crate::kernel::bootstrap) const STORE_RECLAIM_MIN_AGE: Duration = Duration::from_secs(60);

/// Directory holding the versioned venv stores, beside the venv path:
/// `<base>.versions/`.
pub(in crate::kernel::bootstrap) fn versions_dir(venv_base: &Path) -> PathBuf {
    let name = venv_base.file_name().unwrap_or_default().to_string_lossy();
    venv_base.with_file_name(format!("{name}.versions"))
}

/// The store the venv path currently resolves through; `None` when the venv
/// path is not a link (the legacy in-place layout, or nothing at all).
pub(in crate::kernel::bootstrap) fn resolve_active_store(venv_base: &Path) -> Option<PathBuf> {
    let target = std::fs::read_link(venv_base).ok()?;
    if target.is_absolute() {
        Some(target)
    } else {
        Some(venv_base.parent().unwrap_or(Path::new("/")).join(target))
    }
}

/// The store directory name for a runtime identity: a bounded slice of the
/// runtime-source hash (64 bits makes a same-machine collision negligible).
pub(in crate::kernel::bootstrap) fn store_dir_name(runtime_identity: &str) -> String {
    let hex: String = runtime_identity
        .rsplit(':')
        .next()
        .unwrap_or(runtime_identity)
        .chars()
        .filter(char::is_ascii_hexdigit)
        .collect();
    format!("runtime-{}", &hex[..hex.len().min(16)])
}

/// Point the venv path at `store`. A legacy in-place venv still parked at the
/// path moves into the versions directory first; the move is refused by
/// Windows while one of its interpreters runs, which fails the bootstrap with
/// the remedy instead of destroying the directory.
pub(in crate::kernel::bootstrap) fn swap_active_store(
    venv_base: &Path,
    store: &Path,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(versions_dir(venv_base))?;
    if venv_base.exists() && !venv_is_link(venv_base) {
        let aside =
            versions_dir(venv_base).join(format!("legacy-{}", uuid::Uuid::new_v4().simple()));
        std::fs::rename(venv_base, &aside).with_context(|| {
            format!(
                "migrating the kernel venv at {} into the versioned store layout: a running \
                 Prime Agent process is still using it. Exit every Prime Agent session (or \
                 reboot) and retry; this one-time migration completes on the next start",
                venv_base.display()
            )
        })?;
        let _ = std::fs::remove_dir_all(&aside);
    }
    let link = venv_base.with_file_name(format!(
        "{}.swap-{}",
        venv_base.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::new_v4().simple()
    ));
    let _ = std::fs::remove_dir_all(&link);
    create_store_link(&link, store)?;
    // Removing the link drops the junction/symlink only, never the store it
    // targets; the rename into place runs under the bootstrap lock, so
    // concurrent starters wait out the swap.
    if venv_is_link(venv_base) {
        std::fs::remove_dir(venv_base)?;
    }
    std::fs::rename(&link, venv_base)
        .with_context(|| format!("moving the kernel venv link to {}", venv_base.display()))
}

fn venv_is_link(venv_base: &Path) -> bool {
    venv_base
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
}

/// A junction on Windows (no privilege needed, unlike a directory symlink)
/// and a plain symlink elsewhere.
#[cfg(windows)]
fn create_store_link(link: &Path, store: &Path) -> anyhow::Result<()> {
    let mut command = std::process::Command::new("cmd");
    command.args(["/c", "mklink", "/J"]).arg(link).arg(store);
    crate::platform::process::set_no_window(&mut command);
    let output = command
        .output()
        .context("failed to run mklink for the kernel venv junction")?;
    if !output.status.success() {
        return Err(anyhow!(
            "creating the kernel venv junction {} -> {} failed: {}",
            link.display(),
            store.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn create_store_link(link: &Path, store: &Path) -> anyhow::Result<()> {
    std::os::unix::fs::symlink(store, link)
        .with_context(|| format!("linking {} -> {}", link.display(), store.display()))
}

/// Record a live kernel: `running/<pid>` under the versioned store the
/// interpreter resolves through. The reclamation skips any store with a live
/// marker and reaps the stale ones; a missed write only delays that store's
/// reclamation, so the write is best effort.
pub(crate) fn mark_kernel_running(python: &Path, pid: u32) {
    let Some(store) = interpreter_store_dir(python) else {
        return;
    };
    let running = store.join("running");
    if std::fs::create_dir_all(&running).is_err() {
        return;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    crate::platform::perms::set_private_mode(&mut options);
    let _ = options
        .open(running.join(pid.to_string()))
        .and_then(|mut file| writeln!(file, "{pid}"));
}

/// The versioned store the interpreter binary resolves through, `None` when
/// the interpreter does not live under a `*.versions` store (the override
/// python, or the legacy in-place layout).
fn interpreter_store_dir(python: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(python).ok()?;
    let store = real.ancestors().nth(2)?;
    let versions = store.parent()?;
    versions
        .file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(".versions"))
        .then(|| store.to_path_buf())
}

/// Reclaim stores nobody can be running from: interrupted `.building-<pid>`
/// leftovers whose owning pid is dead, and published stores older than
/// `min_age` whose every running marker is dead. Best effort: a failure is a
/// skip and the next bootstrap rechecks, never a setup error.
pub(in crate::kernel::bootstrap) fn reclaim_stale_stores(
    venv_base: &Path,
    min_age: Duration,
    report: &dyn Fn(&str),
) {
    let Ok(entries) = std::fs::read_dir(versions_dir(venv_base)) else {
        return;
    };
    let active = resolve_active_store(venv_base);
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = entry.file_name().into_string().ok() else {
            continue;
        };
        if active
            .as_ref()
            .is_some_and(|target| same_store(target, &path))
        {
            reap_dead_markers(&path);
            continue;
        }
        if let Some(pid) = building_owner_pid(&name) {
            if pid_exists(pid) {
                continue;
            }
        } else if store_younger_than(&path, min_age) || has_live_marker(&path) {
            continue;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            report(&format!("reclaimed the stale kernel venv store {name}"));
        }
    }
}

fn same_store(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn building_owner_pid(name: &str) -> Option<u32> {
    let raw = name.rsplit_once(".building-")?.1;
    raw.parse::<u32>().ok().filter(|pid| *pid > 0)
}

fn store_younger_than(store: &Path, min_age: Duration) -> bool {
    std::fs::symlink_metadata(store)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < min_age)
}

fn has_live_marker(store: &Path) -> bool {
    marker_pids(store).iter().any(|pid| pid_exists(*pid))
}

fn reap_dead_markers(store: &Path) {
    for pid in marker_pids(store) {
        if !pid_exists(pid) {
            let _ = std::fs::remove_file(store.join("running").join(pid.to_string()));
        }
    }
}

/// The live-marker pid names under a store's `running/` directory; unreadable
/// or malformed names are ignored.
fn marker_pids(store: &Path) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir(store.join("running")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter_map(|name| name.parse::<u32>().ok())
        .filter(|pid| *pid > 0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_store(store: &Path, marker: Option<u32>) {
        std::fs::create_dir_all(store.join("Scripts")).unwrap();
        std::fs::write(store.join("Scripts").join("python.exe"), "interpreter").unwrap();
        if let Some(pid) = marker {
            let running = store.join("running");
            std::fs::create_dir_all(&running).unwrap();
            std::fs::write(running.join(pid.to_string()), format!("{pid}\n")).unwrap();
        }
    }

    #[test]
    fn swap_publishes_the_new_store_and_leaves_the_live_store_intact() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("kernel-venv");
        let versions = versions_dir(&base);
        let live = versions.join("runtime-aaaaaaaaaaaaaaaa");
        let fresh = versions.join("runtime-bbbbbbbbbbbbbbbb");
        write_store(&live, Some(std::process::id()));
        std::fs::write(live.join("Scripts").join("site-marker"), "live payload").unwrap();
        write_store(&fresh, None);

        swap_active_store(&base, &live).unwrap();
        assert!(resolve_active_store(&base).is_none_or(|target| same_store(&target, &live)));
        swap_active_store(&base, &fresh).unwrap();

        assert!(resolve_active_store(&base).is_none_or(|target| same_store(&target, &fresh)));
        assert_eq!(
            std::fs::read_to_string(base.join("Scripts").join("python.exe")).unwrap(),
            "interpreter"
        );
        // The live store survives the swap untouched, marker included.
        assert_eq!(
            std::fs::read_to_string(live.join("Scripts").join("site-marker")).unwrap(),
            "live payload"
        );
        assert!(live
            .join("running")
            .join(std::process::id().to_string())
            .exists());
    }

    #[test]
    fn reclamation_skips_the_active_store_but_reaps_its_dead_markers() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("kernel-venv");
        let live = versions_dir(&base).join("runtime-aaaaaaaaaaaaaaaa");
        write_store(&live, Some(std::process::id()));
        swap_active_store(&base, &live).unwrap();
        let dead = std::process::id() + 10_000;
        std::fs::write(
            live.join("running").join(dead.to_string()),
            format!("{dead}\n"),
        )
        .unwrap();

        reclaim_stale_stores(&base, Duration::ZERO, &|_| panic!("nothing is stale here"));

        assert!(live.exists());
        assert!(!live.join("running").join(dead.to_string()).exists());
        assert!(live
            .join("running")
            .join(std::process::id().to_string())
            .exists());
    }

    #[test]
    fn reclamation_reaps_a_markerless_store_and_skips_a_live_one() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("kernel-venv");
        let versions = versions_dir(&base);
        let stale = versions.join("runtime-cccccccccccccccc");
        write_store(&stale, Some(std::process::id() + 10_000));
        let live = versions.join("runtime-dddddddddddddddd");
        write_store(&live, Some(std::process::id()));

        reclaim_stale_stores(&base, Duration::ZERO, &|_| {});

        assert!(!stale.exists());
        assert!(live.exists());
    }

    #[test]
    fn reclamation_respects_the_min_age_guard() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("kernel-venv");
        let fresh = versions_dir(&base).join("runtime-eeeeeeeeeeeeeeee");
        write_store(&fresh, None);

        reclaim_stale_stores(&base, STORE_RECLAIM_MIN_AGE, &|_| {});
        assert!(fresh.exists());
        reclaim_stale_stores(&base, Duration::ZERO, &|_| {});
        assert!(!fresh.exists());
    }

    #[test]
    fn reclamation_reaps_a_dead_building_leftover_and_keeps_a_live_one() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("kernel-venv");
        let versions = versions_dir(&base);
        let dead = versions.join(format!(
            "runtime-ffffffffffffffff.building-{}",
            std::process::id() + 10_000
        ));
        std::fs::create_dir_all(&dead).unwrap();
        let live = versions.join(format!(
            "runtime-1111111111111111.building-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&live).unwrap();

        reclaim_stale_stores(&base, Duration::ZERO, &|_| {});

        assert!(!dead.exists());
        assert!(live.exists());
    }

    #[cfg(unix)]
    #[test]
    fn swap_moves_a_legacy_venv_aside_before_linking() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("kernel-venv");
        let store = versions_dir(&base).join("runtime-aaaaaaaaaaaaaaaa");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("python"), "legacy interpreter").unwrap();

        swap_active_store(&base, &store).unwrap();

        assert!(resolve_active_store(&base).is_none_or(|target| same_store(&target, &store)));
        assert!(std::fs::read_dir(versions_dir(&base))
            .unwrap()
            .flatten()
            .all(|entry| entry.file_name().to_string_lossy().starts_with("runtime-")));
    }

    #[test]
    fn store_names_come_from_the_identity_hash() {
        assert_eq!(
            store_dir_name(
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            ),
            "runtime-0123456789abcdef"
        );
    }

    #[test]
    fn interpreter_store_dir_requires_a_versions_layout() {
        let root = tempfile::tempdir().unwrap();
        let store = root
            .path()
            .join("kernel-venv.versions")
            .join("runtime-aaaaaaaaaaaaaaaa");
        write_store(&store, None);
        let python = store.join("Scripts").join("python.exe");
        let canonical = std::fs::canonicalize(&store).unwrap();
        assert_eq!(
            interpreter_store_dir(&python).as_deref(),
            Some(canonical.as_path())
        );
        let stray = root.path().join("plain").join("python.exe");
        std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
        std::fs::write(&stray, "interpreter").unwrap();
        assert_eq!(interpreter_store_dir(&stray), None);
    }

    #[test]
    fn marker_write_is_a_noop_for_an_override_python() {
        // An interpreter outside a versions store must not grow markers.
        let root = tempfile::tempdir().unwrap();
        let python = root.path().join("override").join("python.exe");
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(&python, "interpreter").unwrap();
        mark_kernel_running(&python, std::process::id());
        assert!(std::fs::read_dir(python.parent().unwrap())
            .unwrap()
            .flatten()
            .all(|entry| entry.file_name().to_string_lossy() == "python.exe"));
    }
}
