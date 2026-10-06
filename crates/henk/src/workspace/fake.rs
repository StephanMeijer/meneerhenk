//! A workspace in memory, for tests: files in a map, commands answered from
//! a script, and a flag that tells a test the workspace was destroyed.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use henk_domain::address::WorkspacePath;
use henk_domain::workspace::{FileMode, Profile, RawChange};

use henk_domain::ignore::PathFilter;

use super::{ExecResult, Exported, Hit, Pattern, Workspace, WorkspaceError, WorkspaceProvider};

/// What a scripted command does: its exit code and output, and the files it
/// writes, as a check that changes the tree would.
#[derive(Debug, Clone, Default)]
pub struct Scripted {
    /// The exit code.
    pub code: i32,
    /// What it prints.
    pub output: String,
    /// Files it writes, by path.
    pub writes: Vec<(String, Vec<u8>)>,
    /// How long it takes.
    pub delay: Duration,
}

/// Content and executable bit by path.
type Files = BTreeMap<String, (Vec<u8>, bool)>;

/// Opens [`FakeWorkspace`]s that share one `closed` flag and one log of the
/// commands run.
#[derive(Debug, Default, Clone)]
pub struct FakeProvider {
    /// Answers by command line, joined with spaces.
    pub script: BTreeMap<String, Scripted>,
    /// Changes `export` reports on top of the real ones, to stand for what
    /// only another backend could produce (a link, a submodule).
    pub inject: Vec<Exported>,
    /// Set when the workspace is closed or dropped.
    pub closed: Arc<AtomicBool>,
    /// Every command run, in order.
    pub ran: Arc<Mutex<Vec<String>>>,
    /// How many workspaces were opened.
    pub opened: Arc<AtomicUsize>,
    /// How many are open now: neither closed nor dropped.
    pub live: Arc<AtomicUsize>,
    /// How many were dropped without being closed first.
    pub unclosed: Arc<AtomicUsize>,
}

impl FakeProvider {
    /// Whether the last workspace opened was destroyed.
    pub fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// How many workspaces were opened.
    pub fn opened(&self) -> usize {
        self.opened.load(Ordering::SeqCst)
    }

    /// How many are still open.
    pub fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    /// How many were only destroyed because they were dropped.
    pub fn unclosed(&self) -> usize {
        self.unclosed.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl WorkspaceProvider for FakeProvider {
    async fn open(
        &self,
        source: &Path,
        _profile: &Profile,
    ) -> Result<Arc<dyn Workspace>, WorkspaceError> {
        let mut files = Files::new();
        import(source, source, &mut files)
            .map_err(|e| WorkspaceError::Backend(format!("cannot import: {e}")))?;
        self.closed.store(false, Ordering::SeqCst);
        self.opened.fetch_add(1, Ordering::SeqCst);
        self.live.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(FakeWorkspace {
            base: Mutex::new(files.clone()),
            files: Mutex::new(files),
            done: AtomicBool::new(false),
            provider: self.clone(),
        }))
    }
}

fn import(root: &Path, dir: &Path, files: &mut Files) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            import(root, &entry.path(), files)?;
        } else if kind.is_file() {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(std::io::Error::other)?
                .to_string_lossy()
                .into_owned();
            let executable = entry.metadata()?.permissions().mode() & 0o111 != 0;
            files.insert(relative, (std::fs::read(&path)?, executable));
        }
    }
    Ok(())
}

/// A workspace in memory.
#[derive(Debug)]
pub struct FakeWorkspace {
    /// What `export` compares with: the import, or the last baseline.
    base: Mutex<Files>,
    files: Mutex<Files>,
    /// This workspace is closed.
    done: AtomicBool,
    provider: FakeProvider,
}

impl FakeWorkspace {
    fn files(&self) -> Result<std::sync::MutexGuard<'_, Files>, WorkspaceError> {
        if self.done.load(Ordering::SeqCst) {
            return Err(WorkspaceError::Backend(
                "the workspace is closed".to_owned(),
            ));
        }
        self.files
            .lock()
            .map_err(|_| WorkspaceError::Backend("poisoned".to_owned()))
    }

    fn base(&self) -> Result<std::sync::MutexGuard<'_, Files>, WorkspaceError> {
        self.base
            .lock()
            .map_err(|_| WorkspaceError::Backend("poisoned".to_owned()))
    }
}

fn under(dir: &WorkspacePath, path: &str) -> bool {
    dir.as_str().is_empty()
        || path
            .strip_prefix(dir.as_str())
            .is_some_and(|rest| rest.starts_with('/'))
}

#[async_trait::async_trait]
impl Workspace for FakeWorkspace {
    async fn exec(
        &self,
        argv: &[String],
        _cwd: &WorkspacePath,
        _timeout: Duration,
    ) -> Result<ExecResult, WorkspaceError> {
        let line = argv.join(" ");
        if let Ok(mut ran) = self.provider.ran.lock() {
            ran.push(line.clone());
        }
        let Some(scripted) = self.provider.script.get(&line) else {
            return Ok(ExecResult {
                code: Some(127),
                timed_out: false,
                output: format!("{line}: not scripted"),
                duration: Duration::ZERO,
            });
        };
        tokio::time::sleep(scripted.delay).await;
        let mut files = self.files()?;
        for (path, content) in &scripted.writes {
            let executable = files.get(path).is_some_and(|f| f.1);
            files.insert(path.clone(), (content.clone(), executable));
        }
        Ok(ExecResult {
            code: Some(scripted.code),
            timed_out: false,
            output: scripted.output.clone(),
            duration: Duration::from_millis(1),
        })
    }

    async fn read(&self, path: &WorkspacePath, max_bytes: u64) -> Result<Vec<u8>, WorkspaceError> {
        let files = self.files()?;
        let (content, _) = files
            .get(path.as_str())
            .ok_or_else(|| WorkspaceError::NotFound(path.to_string()))?;
        if u64::try_from(content.len()).unwrap_or(u64::MAX) > max_bytes {
            return Err(WorkspaceError::Refused(format!(
                "{path} is larger than {max_bytes} bytes"
            )));
        }
        Ok(content.clone())
    }

    async fn write(&self, path: &WorkspacePath, content: &[u8]) -> Result<(), WorkspaceError> {
        let mut files = self.files()?;
        let executable = files.get(path.as_str()).is_some_and(|f| f.1);
        files.insert(path.as_str().to_owned(), (content.to_vec(), executable));
        Ok(())
    }

    async fn list(
        &self,
        dir: &WorkspacePath,
        only: Option<&PathFilter>,
        cap: usize,
    ) -> Result<Vec<String>, WorkspaceError> {
        let files = self.files()?;
        Ok(files
            .keys()
            .filter(|p| under(dir, p) && only.is_none_or(|o| o.matches(p)))
            .take(cap)
            .cloned()
            .collect())
    }

    async fn search(
        &self,
        dir: &WorkspacePath,
        pattern: &Pattern,
        only: Option<&PathFilter>,
        max_file_bytes: u64,
        cap: usize,
    ) -> Result<Vec<Hit>, WorkspaceError> {
        let files = self.files()?;
        let mut hits = Vec::new();
        for (path, (content, _)) in files
            .iter()
            .filter(|(p, _)| under(dir, p) && only.is_none_or(|o| o.matches(p)))
        {
            if u64::try_from(content.len()).unwrap_or(u64::MAX) > max_file_bytes {
                continue;
            }
            let Ok(text) = std::str::from_utf8(content) else {
                continue;
            };
            for (index, line) in text.lines().enumerate() {
                if pattern.is_match(line) {
                    hits.push(Hit {
                        path: path.clone(),
                        line: index + 1,
                        text: line.trim().to_owned(),
                    });
                    if hits.len() >= cap {
                        return Ok(hits);
                    }
                }
            }
        }
        Ok(hits)
    }

    async fn export(&self) -> Result<Vec<Exported>, WorkspaceError> {
        let files = self.files()?;
        let base = self.base()?;
        let mut changes = Vec::new();
        for (path, (content, executable)) in files.iter() {
            if base.get(path) == Some(&(content.clone(), *executable)) {
                continue;
            }
            changes.push(Exported {
                raw: RawChange {
                    path: path.clone(),
                    mode: if *executable {
                        FileMode::Executable
                    } else {
                        FileMode::Regular
                    },
                    deleted: false,
                },
                content: content.clone(),
            });
        }
        for (path, (_, executable)) in base.iter() {
            if !files.contains_key(path) {
                changes.push(Exported {
                    raw: RawChange {
                        path: path.clone(),
                        mode: if *executable {
                            FileMode::Executable
                        } else {
                            FileMode::Regular
                        },
                        deleted: true,
                    },
                    content: Vec::new(),
                });
            }
        }
        changes.extend(self.provider.inject.iter().cloned());
        Ok(changes)
    }

    async fn baseline(&self) -> Result<(), WorkspaceError> {
        let files = self.files()?;
        *self.base()? = files.clone();
        Ok(())
    }

    async fn close(&self) {
        self.destroy();
    }
}

impl FakeWorkspace {
    fn destroy(&self) {
        if !self.done.swap(true, Ordering::SeqCst) {
            self.provider.live.fetch_sub(1, Ordering::SeqCst);
        }
        self.provider.closed.store(true, Ordering::SeqCst);
    }
}

impl Drop for FakeWorkspace {
    fn drop(&mut self) {
        if !self.done.load(Ordering::SeqCst) {
            self.provider.unclosed.fetch_add(1, Ordering::SeqCst);
        }
        self.destroy();
    }
}
