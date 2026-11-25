//! The uv discovery concern: the PATH/PATHEXT executable search and the `ensure_uv` resolution.

use super::{anyhow, home_dir, Path, PathBuf};
use std::ffi::OsStr;
use std::fmt::Write as _;

#[cfg(windows)]
const UV_INSTALL_COMMAND: &str =
    "powershell -ExecutionPolicy ByPass -c \"irm https://astral.sh/uv/install.ps1 | iex\"";
#[cfg(not(windows))]
const UV_INSTALL_COMMAND: &str = "curl -LsSf https://astral.sh/uv/install.sh | sh";

/// What a lookup examined: the directories the safety filter admitted, and how many PATH entries
/// it refused to run from. A miss reports both, so "uv is not installed" and "uv sits somewhere
/// this process will not execute from" read differently.
#[derive(Default)]
struct UvSearch {
    searched: Vec<PathBuf>,
    skipped: usize,
}

/// Search `path_value` for `name`, recording every directory the safety filter let through into
/// `search` so a failed lookup can report where it actually looked.
fn find_executable(
    name: &str,
    path_value: &OsStr,
    project: Option<&Path>,
    search: &mut UvSearch,
) -> Option<PathBuf> {
    // The bare name on Unix; PATHEXT extension candidates on Windows
    // (TS `findExecutable` -> `windowsExecutableCandidates`).
    #[cfg(windows)]
    let candidates = windows_executable_candidates(name, std::env::var("PATHEXT").ok().as_deref());
    #[cfg(not(windows))]
    let candidates = vec![name.to_string()];
    for dir in std::env::split_paths(path_value) {
        // Relative or repository-contained PATH entries can select a
        // checked-in executable before any user action. An ancestor is safe
        // when the selected binary itself stays outside the project.
        if !crate::kernel::python_env::safe_executable_path(&dir, project) {
            search.skipped += 1;
            continue;
        }
        search.searched.push(dir.clone());
        for candidate in &candidates {
            let full_path = dir.join(candidate);
            if crate::kernel::python_env::safe_executable_path(&full_path, project)
                && full_path.is_file()
                && is_executable(&full_path)
            {
                return Some(full_path);
            }
        }
    }
    None
}

/// The extension order `windowsExecutableCandidates` uses when `PATHEXT`
/// yields nothing usable.
#[cfg(any(windows, test))]
const WINDOWS_PATHEXT_DEFAULT: [&str; 4] = [".COM", ".EXE", ".BAT", ".CMD"];

/// The supported PATHEXT extensions applied to `name`, in `PATHEXT` order, else the TS default
/// order. A name that already ends in a default extension is returned as-is. The bare name is
/// never a candidate: an extensionless file would shadow the real `.exe` and cannot be spawned.
#[cfg(any(windows, test))]
pub(super) fn windows_executable_candidates(name: &str, pathext: Option<&str>) -> Vec<String> {
    let extensions = pathext
        .unwrap_or("")
        .split(';')
        .map(str::trim)
        .map(str::to_lowercase)
        .filter(|ext| {
            WINDOWS_PATHEXT_DEFAULT
                .iter()
                .any(|default| default.eq_ignore_ascii_case(ext))
        })
        .collect::<Vec<_>>();
    let lower_name = name.to_lowercase();
    if WINDOWS_PATHEXT_DEFAULT
        .iter()
        .any(|ext| lower_name.ends_with(&ext.to_lowercase()))
    {
        return vec![name.to_string()];
    }
    let defaults: Vec<String> = WINDOWS_PATHEXT_DEFAULT
        .iter()
        .map(std::string::ToString::to_string)
        .collect();
    let source: &[String] = if extensions.is_empty() {
        &defaults
    } else {
        &extensions
    };
    let mut seen = std::collections::HashSet::new();
    source
        .iter()
        .map(|ext| format!("{name}{ext}"))
        .filter(|candidate| seen.insert(candidate.to_lowercase()))
        .collect()
}

fn is_executable(path: &Path) -> bool {
    crate::platform::perms::is_executable(path)
}

/// Find `uv` on PATH or at `~/.local/bin/uv` (`uv.exe` on Windows). Returns `Err` with install
/// guidance when missing: the Rust binary never auto-installs.
///
/// The lookup runs against the calling process's environment, which is a snapshot taken at
/// process start: a `uv` installed while Prime Agent is running sits on a PATH entry this
/// process never saw. The `~/.local/bin` probe is the fallback for exactly that case, and the
/// error names every location the resolver tried so the miss is diagnosable without a debugger.
pub(crate) fn ensure_uv(project: Option<&Path>) -> anyhow::Result<String> {
    resolve_uv(
        project,
        std::env::var_os("PATH").as_deref(),
        &home_dir().join(".local").join("bin"),
    )
}

/// [`ensure_uv`] over an explicit environment view (PATH and the tool directory), so the search
/// order and the failure message are testable without mutating the process environment.
fn resolve_uv(
    project: Option<&Path>,
    path_value: Option<&OsStr>,
    tool_dir: &Path,
) -> anyhow::Result<String> {
    let mut search = UvSearch::default();
    if let Some(found) =
        path_value.and_then(|path| find_executable("uv", path, project, &mut search))
    {
        return Ok(found.to_string_lossy().to_string());
    }
    let uv_name = if cfg!(windows) { "uv.exe" } else { "uv" };
    let local_uv = tool_dir.join(uv_name);
    if (crate::kernel::python_env::safe_executable_path(&local_uv, project)
        || crate::kernel::python_env::safe_home_uv(&local_uv, project))
        && is_executable(&local_uv)
    {
        return Ok(local_uv.to_string_lossy().to_string());
    }
    search.searched.push(tool_dir.to_path_buf());

    let mut message = format!(
        "uv is required to set up the Python kernel. Install uv yourself: {UV_INSTALL_COMMAND}"
    );
    if search.searched.is_empty() {
        message.push_str("\nNo location was searched: PATH is unset.");
    } else {
        let locations: Vec<String> = search
            .searched
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        let _ = write!(message, "\nLooked for uv in: {}", locations.join(", "));
    }
    if search.skipped > 0 {
        let _ = write!(
            message,
            "\n{} PATH entr{} skipped as unsafe to run from.",
            search.skipped,
            if search.skipped == 1 {
                "y was"
            } else {
                "ies were"
            }
        );
    }
    message.push_str(
        "\nIf uv was installed after Prime Agent started, restart Prime Agent so it picks up the new PATH.",
    );
    Err(anyhow!(message))
}

#[cfg(all(test, unix))]
mod safe_uv_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn uv_search_skips_repository_paths_and_allows_safe_ancestor_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let alias = root.path().join("alias");
        let trusted = root.path().join("trusted");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&alias).unwrap();
        std::fs::create_dir_all(&trusted).unwrap();
        for path in [project.join("uv"), trusted.join("uv")] {
            std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        symlink(project.join("uv"), alias.join("uv")).unwrap();
        let path = std::env::join_paths([Path::new("."), &project, &alias, &trusted]).unwrap();
        assert_eq!(
            find_executable("uv", &path, Some(&project), &mut UvSearch::default()),
            Some(trusted.join("uv"))
        );

        let ancestor_uv = root.path().join("uv");
        std::fs::write(&ancestor_uv, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&ancestor_uv, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([root.path()]).unwrap();
        assert_eq!(
            find_executable("uv", &path, Some(&project), &mut UvSearch::default()),
            Some(ancestor_uv)
        );
    }

    #[test]
    fn uv_search_skips_sibling_directory_in_repository() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let project = repo.join("app");
        let sibling = repo.join("tools");
        let trusted = root.path().join("trusted");
        for dir in [&project, &sibling, &trusted] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::create_dir(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        for path in [sibling.join("uv"), trusted.join("uv")] {
            std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths([&sibling, &trusted]).unwrap();
        assert_eq!(
            find_executable("uv", &path, Some(&project), &mut UvSearch::default()),
            Some(trusted.join("uv"))
        );
    }

    #[test]
    fn uv_search_skips_enclosing_checkout_but_keeps_external_tool() {
        let root = tempfile::tempdir().unwrap();
        let outer = root.path().join("outer");
        let inner = outer.join("inner");
        let outer_tools = outer.join("tools");
        let trusted = root.path().join("trusted");
        for dir in [&inner, &outer_tools, &trusted] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for checkout in [&outer, &inner] {
            std::fs::create_dir(checkout.join(".git")).unwrap();
        }
        for path in [outer_tools.join("uv"), trusted.join("uv")] {
            std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths([&outer_tools, &trusted]).unwrap();
        assert_eq!(
            find_executable("uv", &path, Some(&inner), &mut UvSearch::default()),
            Some(trusted.join("uv"))
        );
    }

    #[test]
    fn uv_search_accepts_installed_tool_from_unmarked_filesystem_root() {
        let root = tempfile::tempdir().unwrap();
        let filesystem_root = root.path().ancestors().last().unwrap();
        if filesystem_root.join(".git").exists() {
            return;
        }
        let trusted = root.path().join("trusted");
        std::fs::create_dir(&trusted).unwrap();
        let uv = trusted.join("uv");
        std::fs::write(&uv, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&uv, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([Path::new("."), &trusted]).unwrap();
        assert_eq!(
            find_executable("uv", &path, Some(filesystem_root), &mut UvSearch::default()),
            Some(uv)
        );
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    #[test]
    fn a_missing_uv_names_every_location_it_searched() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let home = root.path().join("home");
        let tools = root.path().join("tools");
        let home_bin = home.join(".local").join("bin");
        for dir in [&project, &tools, &home_bin] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // An entry inside the project is refused by the safety filter, so the message also
        // reports it as skipped rather than silently omitting it.
        let path = std::env::join_paths([&project, &tools]).unwrap();
        let error = resolve_uv(Some(&project), Some(&path), &home_bin).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains(&tools.display().to_string()),
            "PATH directory missing from: {message}"
        );
        assert!(
            message.contains(&home_bin.display().to_string()),
            "home tool directory missing from: {message}"
        );
        assert!(
            message.contains("1 PATH entry was skipped as unsafe to run from."),
            "skipped entry not reported: {message}"
        );
    }

    #[test]
    fn uv_search_applies_the_vanished_project_rule() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let vanished = repo.join("app/gone");
        let sibling = repo.join("tools");
        let trusted = root.path().join("trusted");
        for dir in [repo.join("app"), sibling.clone(), trusted.clone()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::create_dir(repo.join(".git")).unwrap();
        let uv_name = if cfg!(windows) { "uv.exe" } else { "uv" };
        for dir in [&sibling, &trusted] {
            let uv = dir.join(uv_name);
            std::fs::write(&uv, "uv").unwrap();
            #[cfg(unix)]
            std::fs::set_permissions(&uv, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
        }
        let path = std::env::join_paths([&sibling, &trusted]).unwrap();
        assert_eq!(
            find_executable("uv", &path, Some(&vanished), &mut UvSearch::default()),
            Some(trusted.join(uv_name))
        );
    }
}
