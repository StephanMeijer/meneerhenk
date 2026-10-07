//! The host backend: a workspace in a scratch directory on Henk's own host.
//!
//! It is not isolated. Commands run as Henk's user, with an empty
//! environment so that no secret Henk holds (§8.4) reaches them, a time
//! limit, and `HOME` in a scratch directory of its own, outside the tree.
//! Memory, cpu, pids and disk are not bounded; `henk config check` warns.
//!
//! The tree holds no `.git`. Its record of changes is a git directory
//! outside the tree, which nothing run in the tree can reach, and the
//! changeset is read from there.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use henk_domain::address::WorkspacePath;
use henk_domain::ignore::PathFilter;
use henk_domain::workspace::Profile;
use tokio::process::Command;

use super::{
    Budget, ExecResult, Exported, Hit, Pattern, Workspace, WorkspaceError, WorkspaceProvider,
    parse_raw_diff, tail,
};
use crate::git::{ScratchDir, git_command};

/// How long one git command on the workspace's record may take.
const GIT_TIMEOUT: Duration = Duration::from_mins(5);

/// Makes each workspace's directory names unique within the process.
static OPENED: AtomicU64 = AtomicU64::new(0);

/// Opens host workspaces.
#[derive(Debug, Default, Clone, Copy)]
pub struct HostProvider;

#[async_trait::async_trait]
impl WorkspaceProvider for HostProvider {
    async fn open(
        &self,
        source: &Path,
        profile: &Profile,
    ) -> Result<std::sync::Arc<dyn Workspace>, WorkspaceError> {
        Ok(std::sync::Arc::new(
            HostWorkspace::open(source, profile).await?,
        ))
    }
}

/// The scratch directories of one workspace, removed when dropped.
#[derive(Debug)]
struct Dirs {
    /// The tree the model and the commands work in.
    tree: ScratchDir,
    /// The git directory that records the tree, outside it.
    record: ScratchDir,
    /// `HOME` for commands, outside the tree.
    home: ScratchDir,
}

/// The paths of [`Dirs`], copied out so no lock is held while working.
#[derive(Debug, Clone)]
struct Paths {
    tree: PathBuf,
    record: PathBuf,
    home: PathBuf,
}

/// A workspace on the host.
#[derive(Debug)]
pub struct HostWorkspace {
    dirs: Mutex<Option<Dirs>>,
    budget: Budget,
}

fn io(path: &impl std::fmt::Display) -> impl FnOnce(std::io::Error) -> WorkspaceError {
    let path = path.to_string();
    move |source| WorkspaceError::Io { path, source }
}

/// Copies `from` into `to`, leaving out every `.git`. Links are copied as
/// links, never followed.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            std::fs::create_dir(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Every file under `dir`, relative to `root`, `.git` left out, at most
/// `cap`. Links are not followed.
fn walk(root: &Path, dir: &Path, only: Option<&PathFilter>, cap: usize, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        if out.len() >= cap {
            return;
        }
        let path = entry.path();
        if entry.file_name().eq_ignore_ascii_case(".git") {
            continue;
        }
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => walk(root, &path, only, cap, out),
            Ok(kind) if kind.is_file() => {
                if let Ok(relative) = path.strip_prefix(root) {
                    let relative = relative.to_string_lossy().into_owned();
                    if only.is_none_or(|o| o.matches(&relative)) {
                        out.push(relative);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The lines matching `pattern` in `files`, with `context` around each,
/// cut to `cap` matches as [`super::keep_matches`] does. `files` are taken
/// in path order, the order the other backends sort their hits in (the
/// walk's order puts `a/b` before `a-c`), so the search can stop reading
/// once it has `cap` matches. `text` reads one file, or gives `None` for a
/// file to skip.
fn search_files(
    mut files: Vec<String>,
    pattern: &Pattern,
    context: usize,
    cap: usize,
    mut text: impl FnMut(&str) -> Option<String>,
) -> Vec<Hit> {
    files.sort();
    let mut hits = Vec::new();
    let mut found = 0;
    for file in files {
        if found >= cap {
            break;
        }
        let Some(text) = text(&file) else {
            continue;
        };
        let lines: Vec<&str> = super::lines(&text).collect();
        let matched: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| pattern.is_match(line))
            .map(|(at, _)| at)
            .collect();
        found += matched.len();
        hits.extend(super::with_context(&file, &lines, &matched, context));
    }
    super::keep_matches(hits, cap, context)
}

/// `real`, which must be inside `root`, as a workspace path: a symbolic
/// link that leads out of the tree or into `.git` is refused here.
fn inside(root: &Path, real: &Path, shown: &WorkspacePath) -> Result<(), WorkspaceError> {
    let rest = real
        .strip_prefix(root)
        .map_err(|_| WorkspaceError::Refused(format!("{shown} leads out of the repository")))?;
    let rest = rest
        .to_str()
        .ok_or_else(|| WorkspaceError::Refused(format!("{shown} has a name that is not UTF-8")))?;
    WorkspacePath::parse_dir(rest)
        .map_err(|_| WorkspaceError::Refused(format!("{shown} leads into .git")))?;
    Ok(())
}

impl HostWorkspace {
    /// Copies `source`, without `.git`, into a new tree and records it.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when a directory cannot be made, the copy
    /// fails or git cannot record the tree.
    pub async fn open(source: &Path, profile: &Profile) -> Result<Self, WorkspaceError> {
        let n = OPENED.fetch_add(1, Ordering::Relaxed);
        let name = format!("henk-workspace-{}-{n}", std::process::id());
        let scratch = |part: &str| {
            ScratchDir::new(&format!("{name}-{part}"))
                .map_err(|e| WorkspaceError::Backend(format!("cannot make a directory: {e}")))
        };
        let dirs = Dirs {
            tree: scratch("tree")?,
            record: scratch("git")?,
            home: scratch("home")?,
        };
        copy_tree(source, dirs.tree.path())
            .map_err(|e| WorkspaceError::Backend(format!("cannot copy the checkout: {e}")))?;
        let workspace = Self {
            dirs: Mutex::new(Some(dirs)),
            budget: Budget::new(profile.limits.clone()),
        };
        // `--force`: the copy holds only what the checkout tracks, ignored
        // patterns included; later changes to those files are still seen.
        for args in [
            &["init", "--quiet"][..],
            &["add", "--all", "--force"],
            &["commit", "--quiet", "--allow-empty", "--message", "import"],
        ] {
            workspace.git(args).await?;
        }
        Ok(workspace)
    }

    fn paths(&self) -> Result<Paths, WorkspaceError> {
        let dirs = self
            .dirs
            .lock()
            .map_err(|_| WorkspaceError::Backend("the workspace is unavailable".to_owned()))?;
        let dirs = dirs
            .as_ref()
            .ok_or_else(|| WorkspaceError::Backend("the workspace is closed".to_owned()))?;
        Ok(Paths {
            tree: dirs.tree.path().to_path_buf(),
            record: dirs.record.path().to_path_buf(),
            home: dirs.home.path().to_path_buf(),
        })
    }

    /// The tree, resolved, for tests that look at it from outside.
    #[cfg(test)]
    pub(crate) fn tree(&self) -> PathBuf {
        self.paths().map(|p| p.tree).unwrap_or_default()
    }

    /// Git on the record, with the tree as its work tree. Nothing from the
    /// user's or the system's git configuration applies, hooks never run.
    async fn git(&self, args: &[&str]) -> Result<Vec<u8>, WorkspaceError> {
        let paths = self.paths()?;
        let mut full = vec![
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "user.name=Henk",
            "-c",
            "user.email=henk@workspace.invalid",
        ];
        full.extend_from_slice(args);
        let mut command = git_command(&paths.record, &full, None);
        command
            .env("GIT_DIR", &paths.record)
            .env("GIT_WORK_TREE", &paths.tree);
        let mut child = command
            .spawn()
            .map_err(|e| WorkspaceError::Backend(format!("git could not run: {e}")))?;
        drop(child.stdin.take());
        let name = args.first().copied().unwrap_or("");
        let output = tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| WorkspaceError::Backend(format!("git {name} took too long")))?
            .map_err(|e| WorkspaceError::Backend(format!("git {name}: {e}")))?;
        if !output.status.success() {
            return Err(WorkspaceError::Backend(format!(
                "git {name} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(output.stdout)
    }

    /// The real path of an existing `path`, refused when it is not inside
    /// the tree, or is inside `.git`, once symbolic links are followed.
    fn existing(&self, path: &WorkspacePath) -> Result<PathBuf, WorkspaceError> {
        let root = self.root()?;
        let real = root
            .join(path.as_str())
            .canonicalize()
            .map_err(|_| WorkspaceError::NotFound(path.to_string()))?;
        inside(&root, &real, path)?;
        Ok(real)
    }

    /// Where to write `path`. It must not be a symbolic link, and its
    /// directory, as far as it exists, must resolve inside the tree and out
    /// of `.git`. Everything is checked before a directory is made.
    fn writable(&self, path: &WorkspacePath) -> Result<PathBuf, WorkspaceError> {
        let root = self.root()?;
        let target = root.join(path.as_str());
        if target
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            return Err(WorkspaceError::Refused(format!(
                "{path} is a symbolic link; edit what it points to"
            )));
        }
        if target.is_dir() {
            return Err(WorkspaceError::Refused(format!("{path} is a directory")));
        }
        let parent = target
            .parent()
            .ok_or_else(|| WorkspaceError::Refused(format!("{path} has no directory")))?;
        let mut existing = parent;
        while !existing.exists() {
            existing = existing
                .parent()
                .ok_or_else(|| WorkspaceError::Refused(format!("{path} has no directory")))?;
        }
        let real = existing.canonicalize().map_err(io(path))?;
        inside(&root, &real, path)?;
        if !real.is_dir() {
            return Err(WorkspaceError::Refused(format!(
                "{path} is under a file, not a directory"
            )));
        }
        std::fs::create_dir_all(parent).map_err(io(path))?;
        Ok(target)
    }

    fn root(&self) -> Result<PathBuf, WorkspaceError> {
        self.paths()?
            .tree
            .canonicalize()
            .map_err(|e| WorkspaceError::Backend(format!("the tree is unavailable: {e}")))
    }
}

#[async_trait::async_trait]
impl Workspace for HostWorkspace {
    async fn exec(
        &self,
        argv: &[String],
        cwd: &WorkspacePath,
        timeout: Duration,
    ) -> Result<ExecResult, WorkspaceError> {
        let paths = self.paths()?;
        let dir = self.existing(cwd)?;
        let Some((program, rest)) = argv.split_first() else {
            return Err(WorkspaceError::Refused("an empty command".to_owned()));
        };
        let held = self.budget.reserve(timeout)?;
        let limit = held.granted();
        if limit.is_zero() {
            return Ok(self.budget.used_up());
        }
        let started = Instant::now();
        let child = Command::new(program)
            .args(rest)
            .current_dir(&dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &paths.home)
            .env("LANG", "C.UTF-8")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                return Ok(ExecResult {
                    code: None,
                    timed_out: false,
                    output: format!("could not start: {error}"),
                    duration: started.elapsed(),
                });
            }
        };
        let result = match tokio::time::timeout(limit, child.wait_with_output()).await {
            Ok(Ok(output)) => {
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                ExecResult {
                    code: output.status.code(),
                    timed_out: false,
                    output: tail(&text, self.budget.output_cap()),
                    duration: started.elapsed(),
                }
            }
            Ok(Err(error)) => ExecResult {
                code: None,
                timed_out: false,
                output: format!("could not wait for it: {error}"),
                duration: started.elapsed(),
            },
            // Dropping the future kills the child (`kill_on_drop`).
            Err(_) => ExecResult {
                code: None,
                timed_out: true,
                output: format!("stopped after {}s", limit.as_secs()),
                duration: started.elapsed(),
            },
        };
        held.settle(result.duration);
        Ok(result)
    }

    async fn read(&self, path: &WorkspacePath, max_bytes: u64) -> Result<Vec<u8>, WorkspaceError> {
        let real = self.existing(path)?;
        let meta = real.metadata().map_err(io(path))?;
        if !meta.is_file() {
            return Err(WorkspaceError::Refused(format!("{path} is not a file")));
        }
        if meta.len() > max_bytes {
            return Err(WorkspaceError::Refused(format!(
                "{path} is larger than {max_bytes} bytes"
            )));
        }
        std::fs::read(&real).map_err(io(path))
    }

    async fn write(&self, path: &WorkspacePath, content: &[u8]) -> Result<(), WorkspaceError> {
        let target = self.writable(path)?;
        std::fs::write(&target, content).map_err(io(path))
    }

    async fn list(
        &self,
        dir: &WorkspacePath,
        only: Option<&PathFilter>,
        cap: usize,
    ) -> Result<Vec<String>, WorkspaceError> {
        let root = self.root()?;
        let start = self.existing(dir)?;
        let mut files = Vec::new();
        walk(&root, &start, only, cap, &mut files);
        Ok(files)
    }

    async fn search(
        &self,
        dir: &WorkspacePath,
        pattern: &Pattern,
        only: Option<&PathFilter>,
        context: usize,
        max_file_bytes: u64,
        cap: usize,
    ) -> Result<Vec<Hit>, WorkspaceError> {
        let root = self.root()?;
        let start = self.existing(dir)?;
        let mut files = Vec::new();
        walk(&root, &start, only, usize::MAX, &mut files);
        Ok(search_files(files, pattern, context, cap, |file| {
            let full = root.join(file);
            if full.metadata().is_ok_and(|m| m.len() > max_file_bytes) {
                return None;
            }
            std::fs::read_to_string(&full).ok()
        }))
    }

    async fn export(&self) -> Result<Vec<Exported>, WorkspaceError> {
        self.git(&["add", "--all"]).await?;
        let diff = self
            .git(&[
                "diff",
                "--cached",
                "--raw",
                "-z",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                "--no-abbrev",
                "HEAD",
            ])
            .await?;
        let mut changes = Vec::new();
        for (raw, blob) in parse_raw_diff(&diff)? {
            let content = match blob {
                Some(sha) => self.git(&["cat-file", "blob", &sha]).await?,
                None => Vec::new(),
            };
            changes.push(Exported { raw, content });
        }
        Ok(changes)
    }

    async fn baseline(&self) -> Result<(), WorkspaceError> {
        self.git(&["add", "--all"]).await?;
        self.git(&["commit", "--quiet", "--allow-empty", "--message", "setup"])
            .await?;
        Ok(())
    }

    async fn close(&self) {
        if let Ok(mut dirs) = self.dirs.lock() {
            drop(dirs.take());
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use std::os::unix::fs::PermissionsExt as _;

    use henk_domain::workspace::{FileMode, Limits};

    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    fn p(path: &str) -> WorkspacePath {
        WorkspacePath::parse(path).unwrap()
    }

    /// A checkout with `src/a.rs`, `run.sh` (executable), `old.md` and a
    /// `.git` that must not be copied.
    fn source(name: &str) -> ScratchDir {
        let dir = ScratchDir::new(name).unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::create_dir_all(d.join(".git")).unwrap();
        std::fs::write(d.join(".git/config"), "[core]\n").unwrap();
        std::fs::write(d.join("src/a.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();
        std::fs::write(d.join("run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(d.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(d.join("old.md"), "old\n").unwrap();
        dir
    }

    async fn open(name: &str, limits: Limits) -> (ScratchDir, HostWorkspace) {
        let src = source(name);
        let ws = HostWorkspace::open(
            src.path(),
            &Profile {
                limits,
                ..Profile::default()
            },
        )
        .await
        .unwrap();
        (src, ws)
    }

    #[tokio::test]
    async fn the_import_leaves_git_behind() {
        let (_src, ws) = open("henk-ws-import", Limits::default()).await;
        assert!(!ws.tree().join(".git").exists());
        let files = ws.list(&WorkspacePath::root(), None, 100).await.unwrap();
        assert_eq!(files, ["old.md", "run.sh", "src/a.rs"]);
        assert!(ws.export().await.unwrap().is_empty(), "nothing changed yet");
    }

    #[tokio::test]
    async fn files_outside_the_tree_cannot_be_read_or_written() {
        let (_src, ws) = open("henk-ws-escape", Limits::default()).await;
        let tree = ws.tree();
        std::os::unix::fs::symlink("/etc", tree.join("etc")).unwrap();
        assert!(matches!(
            ws.read(&p("etc/passwd"), 1 << 20).await,
            Err(WorkspaceError::Refused(_))
        ));
        assert!(ws.read(&p("missing"), 1 << 20).await.is_err());
        for path in ["etc/x", "etc"] {
            assert!(ws.write(&p(path), b"y").await.is_err(), "{path}");
        }
        assert!(!Path::new("/etc/x").exists());
        assert!(
            ws.list(&p("etc"), None, 10).await.is_err(),
            "a link out is not listed"
        );
        assert!(
            ws.exec(&argv(&["true"]), &p("etc"), Duration::from_secs(5))
                .await
                .is_err(),
            "nor run in"
        );
    }

    #[tokio::test]
    async fn a_symlink_into_git_is_refused() {
        let (_src, ws) = open("henk-ws-gitlink", Limits::default()).await;
        let tree = ws.tree();
        // A check could make a .git in the tree and link to it.
        std::fs::create_dir_all(tree.join(".git/hooks")).unwrap();
        std::os::unix::fs::symlink(".git", tree.join("g")).unwrap();
        std::os::unix::fs::symlink(".git/hooks", tree.join("h")).unwrap();
        for path in ["g/hooks/pre-commit", "h/pre-commit", "g/config"] {
            assert!(
                matches!(
                    ws.write(&p(path), b"x").await,
                    Err(WorkspaceError::Refused(_))
                ),
                "{path}"
            );
        }
        assert!(!tree.join(".git/hooks/pre-commit").exists());
        assert!(!tree.join(".git/config").exists());
        // Nothing is made before the check: no directory under the link.
        assert!(ws.write(&p("g/new/dir/file"), b"x").await.is_err());
        assert!(!tree.join(".git/new").exists());
        assert!(ws.read(&p("h"), 10).await.is_err());
    }

    #[tokio::test]
    async fn search_finds_lines_and_skips_large_files() {
        let find = |source: &str| crate::workspace::Pattern::parse(source).unwrap();
        let src = source("henk-ws-search");
        let ws = HostProvider
            .open(src.path(), &Profile::default())
            .await
            .unwrap();
        ws.write(&p("src/big.rs"), "let x = 9;\n".repeat(10).as_bytes())
            .await
            .unwrap();
        let hits = ws
            .search(&WorkspacePath::root(), &find("let x"), None, 0, 50, 10)
            .await
            .unwrap();
        assert_eq!(
            hits,
            [Hit {
                path: "src/a.rs".to_owned(),
                line: 2,
                text: "    let x = 1;".to_owned(),
                matched: true,
            }]
        );
        let capped = ws
            .search(&p("src"), &find("let x"), None, 0, 1 << 20, 3)
            .await
            .unwrap();
        assert_eq!(capped.len(), 3);
        assert!(
            ws.search(&p("nope"), &find("x"), None, 0, 10, 10)
                .await
                .is_err()
        );
    }

    #[test]
    fn a_capped_search_stops_reading_once_it_has_its_matches() {
        let pattern = crate::workspace::Pattern::parse("hit").unwrap();
        // Out of path order on purpose, as the walk can give them.
        let files: Vec<String> = ["c", "a-b", "a/b", "b", "d"]
            .iter()
            .map(|f| (*f).to_owned())
            .collect();
        let mut read = Vec::new();
        let hits = search_files(files, &pattern, 1, 3, |file| {
            read.push(file.to_owned());
            Some("hit\nhit\nmiss\n".to_owned())
        });
        assert_eq!(read, ["a-b", "a/b"], "reads stop at the cap, in path order");
        let shown: Vec<(&str, usize, bool)> = hits
            .iter()
            .map(|h| (h.path.as_str(), h.line, h.matched))
            .collect();
        assert_eq!(
            shown,
            [
                ("a-b", 1, true),
                ("a-b", 2, true),
                ("a-b", 3, false),
                ("a/b", 1, true),
            ]
        );
    }

    #[tokio::test]
    async fn writes_make_directories_and_keep_modes() {
        let (_src, ws) = open("henk-ws-write", Limits::default()).await;
        ws.write(&p("new/deep/b.rs"), b"b\n").await.unwrap();
        ws.write(&p("run.sh"), b"#!/bin/sh\necho hi\n")
            .await
            .unwrap();
        assert_eq!(ws.read(&p("new/deep/b.rs"), 10).await.unwrap(), b"b\n");
        let mode = std::fs::metadata(ws.tree().join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "still executable");
        assert!(matches!(
            ws.read(&p("run.sh"), 3).await,
            Err(WorkspaceError::Refused(e)) if e.contains("larger")
        ));
        assert!(ws.write(&p("src"), b"x").await.is_err(), "a directory");
        assert!(
            ws.write(&p("old.md/x"), b"x").await.is_err(),
            "under a file"
        );
    }

    #[tokio::test]
    async fn export_reports_changes_and_modes() {
        let (_src, ws) = open("henk-ws-export", Limits::default()).await;
        let tree = ws.tree();
        ws.write(&p("src/a.rs"), b"fn main() {}\n").await.unwrap();
        ws.write(&p("new.md"), b"new\n").await.unwrap();
        std::fs::remove_file(tree.join("old.md")).unwrap();
        std::fs::set_permissions(tree.join("new.md"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::os::unix::fs::symlink("/etc/passwd", tree.join("link")).unwrap();
        let mut changes = ws.export().await.unwrap();
        changes.sort_by(|a, b| a.raw.path.cmp(&b.raw.path));
        let summary: Vec<(&str, FileMode, bool)> = changes
            .iter()
            .map(|c| (c.raw.path.as_str(), c.raw.mode, c.raw.deleted))
            .collect();
        assert_eq!(
            summary,
            [
                ("link", FileMode::Symlink, false),
                ("new.md", FileMode::Executable, false),
                ("old.md", FileMode::Regular, true),
                ("src/a.rs", FileMode::Regular, false),
            ]
        );
        assert_eq!(changes[1].content, b"new\n");
        assert_eq!(changes[3].content, b"fn main() {}\n");
        assert!(changes[0].content.is_empty(), "a link's target is not read");
        assert!(
            crate::workspace::checked_changeset(changes, 10).is_err(),
            "the link is refused"
        );
        assert_eq!(ws.export().await.unwrap().len(), 4, "export can repeat");
    }

    #[tokio::test]
    async fn a_command_sees_none_of_henks_environment_and_home_is_outside_the_tree() {
        let (_src, ws) = open("henk-ws-env", Limits::default()).await;
        let result = ws
            .exec(
                &argv(&["env"]),
                &WorkspacePath::root(),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        let mut names: Vec<&str> = result
            .output
            .lines()
            .filter_map(|line| line.split('=').next())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["HOME", "LANG", "PATH"], "{}", result.output);
        let home = result
            .output
            .lines()
            .find_map(|l| l.strip_prefix("HOME="))
            .unwrap();
        assert!(!Path::new(home).starts_with(ws.tree()), "{home}");

        // A cache written to HOME is not a change.
        ws.exec(
            &argv(&[
                "sh",
                "-c",
                "mkdir -p \"$HOME/.cache\" && echo c > \"$HOME/.cache/x\"",
            ]),
            &WorkspacePath::root(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(ws.export().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn passing_failing_and_slow_commands_are_told_apart() {
        let (_src, ws) = open("henk-ws-kinds", Limits::default()).await;
        let root = WorkspacePath::root();
        let short = Duration::from_millis(300);
        let ok = ws.exec(&argv(&["true"]), &root, short).await.unwrap();
        assert_eq!((ok.code, ok.timed_out), (Some(0), false));
        let broken = ws
            .exec(
                &argv(&["sh", "-c", "echo broken >&2; exit 3"]),
                &root,
                short,
            )
            .await
            .unwrap();
        assert_eq!(broken.code, Some(3));
        assert_eq!(broken.output.trim(), "broken");
        let slow = ws.exec(&argv(&["sleep", "5"]), &root, short).await.unwrap();
        assert!(slow.timed_out);
        assert!(slow.duration < Duration::from_secs(4));
        let missing = ws
            .exec(&argv(&["no-such-program-henk"]), &root, short)
            .await
            .unwrap();
        assert!(missing.output.starts_with("could not start"));
        let sub = ws.exec(&argv(&["pwd"]), &p("src"), short).await.unwrap();
        assert!(sub.output.trim().ends_with("/src"), "{}", sub.output);
    }

    #[tokio::test]
    async fn output_and_time_stay_within_the_profile() {
        let (_src, ws) = open(
            "henk-ws-limits",
            Limits {
                output_bytes: 64,
                command_secs: 1,
                run_secs: 2,
                ..Limits::default()
            },
        )
        .await;
        let root = WorkspacePath::root();
        let long = ws
            .exec(
                &argv(&["sh", "-c", "yes x | head -c 10000; echo END"]),
                &root,
                Duration::from_mins(1),
            )
            .await
            .unwrap();
        assert!(long.output.ends_with("END\n"), "{}", long.output);
        assert!(long.output.len() <= 64 + 20);
        let first = ws
            .exec(&argv(&["sleep", "5"]), &root, Duration::from_mins(1))
            .await
            .unwrap();
        assert!(first.timed_out, "command_secs holds over the asked limit");
        let second = ws
            .exec(&argv(&["sleep", "5"]), &root, Duration::from_mins(1))
            .await
            .unwrap();
        assert!(second.timed_out);
        let third = ws
            .exec(&argv(&["true"]), &root, Duration::from_mins(1))
            .await
            .unwrap();
        assert!(third.timed_out, "the run's time is used up");
        assert!(third.output.contains("used up"), "{}", third.output);
    }

    #[tokio::test]
    async fn closing_removes_everything_and_twice_is_fine() {
        let (_src, ws) = open("henk-ws-close", Limits::default()).await;
        let paths = ws.paths().unwrap();
        ws.close().await;
        ws.close().await;
        for dir in [&paths.tree, &paths.record, &paths.home] {
            assert!(!dir.exists(), "{}", dir.display());
        }
        assert!(matches!(
            ws.read(&p("src/a.rs"), 10).await,
            Err(WorkspaceError::Backend(_))
        ));

        let (_src, dropped) = open("henk-ws-drop", Limits::default()).await;
        let paths = dropped.paths().unwrap();
        drop(dropped);
        assert!(!paths.tree.exists() && !paths.record.exists() && !paths.home.exists());
    }
}

#[cfg(test)]
mod contract_tests {
    use std::sync::Arc;

    use super::HostProvider;
    use crate::workspace::WorkspaceProvider;

    #[tokio::test]
    async fn the_host_backend_keeps_the_workspace_contract() {
        let provider: Arc<dyn WorkspaceProvider> = Arc::new(HostProvider);
        crate::workspace::contract::every_backend_does_this(provider, "henk-host").await;
    }
}
