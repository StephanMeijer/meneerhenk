//! Workspaces (§3.5): where a model's file tools and a project's checks run,
//! what limits hold there, and which changes may leave it.
//!
//! A workspace is a backend choice: the host, or a sandbox host reached over
//! SSH; a container or a microVM is added as another [`BackendKind`]. Whatever the
//! backend, only a changeset leaves the workspace, and Henk's code checks it
//! here before it is applied to a fresh checkout and pushed.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::address::WorkspacePath;

/// Where a workspace runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// Processes on Henk's own host, as Henk's user. Not isolated.
    Host,
    /// A throwaway user per run on a sandbox host, over SSH (#84). Apart
    /// from Henk, but runs share that host's kernel, `/tmp` and network.
    Ssh,
    /// A Pod per workspace in a sandbox namespace of a Kubernetes cluster
    /// (#89): its own filesystem, processes and limits, no token for the
    /// cluster, and whatever network the namespace's policy allows.
    Kubernetes,
}

impl BackendKind {
    /// Whether a model may run commands of its own choosing there (`bash`,
    /// #85 and #172). Not on the host, where they would run as Henk's own
    /// user beside his configuration, database and keys (§8.4); on a backend
    /// apart from Henk, they run as the workspace's own user.
    #[must_use]
    pub const fn runs_model_commands(self) -> bool {
        !matches!(self, Self::Host)
    }

    /// Whether the backend keeps the workspace's processes away from Henk's
    /// own: its files, its user and its network.
    #[must_use]
    pub fn is_isolated(self) -> bool {
        match self {
            Self::Host | Self::Ssh => false,
            Self::Kubernetes => true,
        }
    }
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Host => "host",
            Self::Ssh => "ssh",
            Self::Kubernetes => "kubernetes",
        })
    }
}

/// What one workspace may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Seconds one command may run.
    pub command_secs: u64,
    /// Seconds all commands of one run may take together.
    pub run_secs: u64,
    /// Bytes of output kept per command: the end, where errors are.
    pub output_bytes: u64,
    /// Memory in MiB, when the backend can bound it.
    pub memory_mib: Option<u64>,
    /// CPUs, when the backend can bound them.
    pub cpus: Option<u64>,
    /// Processes, when the backend can bound them.
    pub pids: Option<u64>,
    /// Disk in MiB, when the backend can bound it.
    pub disk_mib: Option<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            command_secs: 10 * 60,
            run_secs: 30 * 60,
            output_bytes: 20 * 1024,
            memory_mib: None,
            cpus: None,
            pids: None,
            disk_mib: None,
        }
    }
}

impl Limits {
    /// The limits `backend` cannot enforce. Every backend enforces the time
    /// and output limits; the rest need isolation.
    #[must_use]
    pub fn unenforced_on(backend: BackendKind) -> Vec<&'static str> {
        match backend {
            BackendKind::Host | BackendKind::Ssh => vec!["memory", "cpu", "pids", "disk"],
            // A Pod's spec bounds memory, cpu and its scratch disk; the
            // number of processes is the node's setting (podPidsLimit).
            BackendKind::Kubernetes => vec!["pids"],
        }
    }

    /// The name of the first limit that is zero, if any.
    fn zero(&self) -> Option<&'static str> {
        [
            ("command_secs", Some(self.command_secs)),
            ("run_secs", Some(self.run_secs)),
            ("output_bytes", Some(self.output_bytes)),
            ("memory_mib", self.memory_mib),
            ("cpus", self.cpus),
            ("pids", self.pids),
            ("disk_mib", self.disk_mib),
        ]
        .into_iter()
        .find_map(|(name, value)| (value == Some(0)).then_some(name))
    }
}

/// One way to run a workspace: a backend, its image, its limits and how it
/// is prepared before the model starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// Where it runs.
    pub backend: BackendKind,
    /// The image a backend starts from; none on the host.
    pub image: Option<String>,
    /// What it may use.
    pub limits: Limits,
    /// The operator's setup commands, run in order in the tree's root before
    /// the model's first command (#93). They come from configuration only;
    /// nothing in the repository adds or changes one (§8.3).
    pub setup: Vec<Vec<String>>,
    /// The tool manager that provides the repository's toolchain, if any.
    pub toolchain: Option<Toolchain>,
    /// Whether reviews of the repository get workspaces too (#170): one
    /// per lane and one for the fact-checker, at the reviewed commit. Off
    /// by default, so a review reads through its MCP session only. Never on
    /// the host backend: see [`Profile::serves`].
    pub review: bool,
    /// Whether the planner gets a workspace too (#172): one copy of the
    /// repository at its default branch, set up as for any run, never
    /// exported. Off by default, and never on the host backend.
    pub plan: bool,
}

/// A tool manager that installs what the repository's own configuration
/// asks for (its `mise.toml` or `.tool-versions`), and runs every command
/// with those tools. The repository's file is data the manager reads; the
/// steps that read it are fixed here (§8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Toolchain {
    /// mise, in its safe mode (`mise install`, then `mise exec --` per
    /// command).
    Mise,
}

impl fmt::Display for Toolchain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Mise => "mise",
        })
    }
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            backend: BackendKind::Host,
            image: None,
            limits: Limits::default(),
            setup: Vec::new(),
            toolchain: None,
            review: false,
            plan: false,
        }
    }
}

impl Profile {
    /// Whether a run of this kind opens a workspace with this profile. An
    /// address run always does; a review or a planner only when the profile
    /// says so and not on the host backend.
    ///
    /// A review workspace runs the setup stage on the pull request's code,
    /// and anyone who can open a pull request, from a fork too, decides
    /// what that code is. On the host it would run as Henk's own user,
    /// beside his configuration, database and keys (§8.4), so a review
    /// gets a workspace only on a backend apart from Henk. An address run
    /// is asked for by a colleague and touches only the repository's own
    /// branches (§3.5). A planner's workspace holds the default branch,
    /// but its model runs commands there, so it too stays off the host.
    #[must_use]
    pub const fn serves(&self, lane: EnvLane) -> bool {
        match lane {
            EnvLane::Address => true,
            EnvLane::Review => self.review && self.backend.runs_model_commands(),
            EnvLane::Plan => self.plan && self.backend.runs_model_commands(),
        }
    }

    fn refusal(&self, name: &str) -> Option<String> {
        if let Some(limit) = self.limits.zero() {
            return Some(format!(
                "workspace profile {name}: {limit} must be above zero"
            ));
        }
        if self.image.is_some() && self.backend != BackendKind::Kubernetes {
            return Some(format!(
                "workspace profile {name}: the {} backend has no image",
                self.backend
            ));
        }
        if self.review && self.backend == BackendKind::Host {
            return Some(format!(
                "workspace profile {name}: review = true needs a backend apart from Henk, such as ssh; on the host, a pull request's setup would run as Henk's own user"
            ));
        }
        if self.plan && self.backend == BackendKind::Host {
            return Some(format!(
                "workspace profile {name}: plan = true needs a backend apart from Henk, such as ssh; on the host, the planner's commands would run as Henk's own user"
            ));
        }
        if self.image.as_deref().is_some_and(|i| i.trim().is_empty()) {
            return Some(format!("workspace profile {name}: the image is empty"));
        }
        if self.setup.iter().any(|command| {
            command
                .first()
                .is_none_or(|program| program.trim().is_empty())
        }) {
            return Some(format!(
                "workspace profile {name}: a setup command is empty"
            ));
        }
        None
    }
}

/// Which profile each repository's workspace uses.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspacePolicy {
    /// For every repository not named in `repositories`.
    pub default: Profile,
    /// Named profiles.
    pub profiles: BTreeMap<String, Profile>,
    /// Repository path, as `owner/name`, to profile name.
    pub repositories: BTreeMap<String, String>,
}

impl WorkspacePolicy {
    /// The profile for `repo` (`owner/name`, compared without case), or the
    /// default.
    #[must_use]
    pub fn profile_for(&self, repo: &str) -> &Profile {
        self.repositories
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(repo))
            .and_then(|(_, profile)| self.profiles.get(profile))
            .unwrap_or(&self.default)
    }

    /// Every profile with its name; the default is called `default`.
    pub fn named(&self) -> impl Iterator<Item = (&str, &Profile)> {
        std::iter::once(("default", &self.default))
            .chain(self.profiles.iter().map(|(n, p)| (n.as_str(), p)))
    }

    /// Checks that every repository names a known profile and every limit
    /// is usable.
    ///
    /// # Errors
    ///
    /// Returns the first problem, in words.
    pub fn validate(&self) -> Result<(), String> {
        for (name, profile) in self.named() {
            if name.trim().is_empty() {
                return Err("a workspace profile has an empty name".to_owned());
            }
            if let Some(why) = profile.refusal(name) {
                return Err(why);
            }
        }
        for (repo, profile) in &self.repositories {
            if !self.profiles.contains_key(profile) {
                return Err(format!(
                    "workspace.repositories: {repo} names unknown profile {profile:?}"
                ));
            }
        }
        Ok(())
    }
}

/// A kind of run that may get a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvLane {
    /// A review lane (§3.2).
    Review,
    /// A planner (§4).
    Plan,
    /// An address run (§3.5).
    Address,
}

impl EnvLane {
    /// Whether what the run changed in its workspace is ever taken out.
    /// Only an address run's is, and only to become its one commit
    /// (§3.5); a review or a plan never changes the pull request (§8.2).
    #[must_use]
    pub const fn exports(self) -> bool {
        matches!(self, Self::Address)
    }
}

/// What a model can do in a workspace, through Henk's tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// List files.
    List,
    /// Read a file.
    Read,
    /// Search file contents.
    Search,
    /// Replace text in a file.
    Edit,
    /// Write a whole file.
    Write,
    /// Run a configured command.
    Exec,
}

/// The workspace tools a lane gets. Only an address run has a workspace
/// for now; reviews and plans read through their MCP sessions (§8.5).
#[must_use]
pub fn tools(lane: EnvLane) -> &'static [ToolKind] {
    match lane {
        EnvLane::Review | EnvLane::Plan => &[],
        EnvLane::Address => &[
            ToolKind::List,
            ToolKind::Read,
            ToolKind::Search,
            ToolKind::Edit,
            ToolKind::Write,
            ToolKind::Exec,
        ],
    }
}

/// What a path is in git's terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    /// A plain file.
    Regular,
    /// A plain file with the executable bit.
    Executable,
    /// A symbolic link.
    Symlink,
    /// A submodule.
    Gitlink,
}

impl FileMode {
    /// From git's octal mode, as in `100644`.
    #[must_use]
    pub fn from_git(mode: &str) -> Option<Self> {
        Some(match mode {
            "100644" | "100664" => Self::Regular,
            "100755" => Self::Executable,
            "120000" => Self::Symlink,
            "160000" => Self::Gitlink,
            _ => return None,
        })
    }
}

/// One change as a backend reports it, before Henk checks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawChange {
    /// The path as reported, relative to the workspace root.
    pub path: String,
    /// What the path is now; for a deletion, what it was.
    pub mode: FileMode,
    /// Whether the path is gone.
    pub deleted: bool,
}

/// What happens to one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    /// The file gets this content.
    Write {
        /// The new content.
        content: Vec<u8>,
        /// Whether it is executable.
        executable: bool,
    },
    /// The file is removed.
    Delete,
}

/// One checked change, ready to apply to a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Where.
    pub path: WorkspacePath,
    /// What.
    pub kind: ChangeKind,
}

/// Why a changeset may not be pushed, or `None` when it may (§3.5). It is
/// refused whole: a path into `.git` or out of the repository, a symbolic
/// link or a submodule, or more changes than a run may make.
#[must_use]
pub fn changeset_refusal(changes: &[RawChange], max_files: usize) -> Option<String> {
    if changes.len() > max_files {
        return Some(format!(
            "{} files changed; a run may change at most {max_files}",
            changes.len()
        ));
    }
    for change in changes {
        if let Err(error) = WorkspacePath::parse(&change.path) {
            return Some(format!("a change is refused: {error}"));
        }
        if change.deleted {
            continue;
        }
        match change.mode {
            FileMode::Regular | FileMode::Executable => {}
            FileMode::Symlink => {
                return Some(format!(
                    "{:?} became a symbolic link; I push only plain files",
                    change.path
                ));
            }
            FileMode::Gitlink => {
                return Some(format!(
                    "{:?} became a submodule; I push only plain files",
                    change.path
                ));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn raw(path: &str, mode: FileMode, deleted: bool) -> RawChange {
        RawChange {
            path: path.to_owned(),
            mode,
            deleted,
        }
    }

    #[test]
    fn a_changeset_of_plain_files_inside_the_repository_passes() {
        let changes = [
            raw("src/a.rs", FileMode::Regular, false),
            raw("run.sh", FileMode::Executable, false),
            raw("old.md", FileMode::Regular, true),
            raw("gone-link", FileMode::Symlink, true),
        ];
        assert_eq!(changeset_refusal(&changes, 4), None);
        assert_eq!(changeset_refusal(&[], 1), None);
    }

    #[test]
    fn a_changeset_that_leaves_the_repository_or_touches_git_is_refused() {
        for path in [
            ".git/config",
            "sub/.git/HEAD",
            "../x",
            "/etc/passwd",
            "a\0b",
        ] {
            let refused = changeset_refusal(&[raw(path, FileMode::Regular, false)], 5);
            assert!(refused.is_some(), "{path:?}");
        }
        assert!(
            changeset_refusal(&[raw(".git/hooks/x", FileMode::Regular, true)], 5).is_some(),
            "a deletion inside .git too"
        );
    }

    #[test]
    fn links_and_submodules_are_refused() {
        let link = changeset_refusal(&[raw("l", FileMode::Symlink, false)], 5).unwrap();
        assert!(link.contains("symbolic link"), "{link}");
        let module = changeset_refusal(&[raw("m", FileMode::Gitlink, false)], 5).unwrap();
        assert!(module.contains("submodule"), "{module}");
        assert!(crate::text::is_in_style(&link) && crate::text::is_in_style(&module));
    }

    #[test]
    fn more_changes_than_the_limit_are_refused() {
        let changes: Vec<RawChange> = (0..3)
            .map(|i| raw(&format!("f{i}"), FileMode::Regular, false))
            .collect();
        assert_eq!(changeset_refusal(&changes, 3), None);
        let refused = changeset_refusal(&changes, 2).unwrap();
        assert_eq!(refused, "3 files changed; a run may change at most 2");
    }

    #[test]
    fn git_modes_are_read() {
        assert_eq!(FileMode::from_git("100644"), Some(FileMode::Regular));
        assert_eq!(FileMode::from_git("100755"), Some(FileMode::Executable));
        assert_eq!(FileMode::from_git("120000"), Some(FileMode::Symlink));
        assert_eq!(FileMode::from_git("160000"), Some(FileMode::Gitlink));
        assert_eq!(FileMode::from_git("040000"), None);
    }

    fn policy() -> WorkspacePolicy {
        let mut big = Profile::default();
        big.limits.command_secs = 3600;
        WorkspacePolicy {
            default: Profile::default(),
            profiles: BTreeMap::from([("big".to_owned(), big)]),
            repositories: BTreeMap::from([("o/heavy".to_owned(), "big".to_owned())]),
        }
    }

    #[test]
    fn a_repository_gets_its_profile_or_the_default() {
        let policy = policy();
        assert_eq!(policy.validate(), Ok(()));
        assert_eq!(policy.profile_for("O/Heavy").limits.command_secs, 3600);
        assert_eq!(policy.profile_for("o/other").limits.command_secs, 600);
        let names: Vec<&str> = policy.named().map(|(n, _)| n).collect();
        assert_eq!(names, ["default", "big"]);
    }

    #[test]
    fn an_unusable_policy_is_refused() {
        let mut unknown = policy();
        unknown
            .repositories
            .insert("o/x".to_owned(), "nope".to_owned());
        assert!(unknown.validate().unwrap_err().contains("unknown profile"));

        let mut zero = policy();
        zero.default.limits.output_bytes = 0;
        assert!(zero.validate().unwrap_err().contains("output_bytes"));

        let mut zero_memory = policy();
        zero_memory.default.limits.memory_mib = Some(0);
        assert!(zero_memory.validate().unwrap_err().contains("memory_mib"));

        let mut image = policy();
        image.default.image = Some("rust:1".to_owned());
        assert!(image.validate().unwrap_err().contains("no image"));

        let mut review_on_host = policy();
        review_on_host.profiles.get_mut("big").unwrap().review = true;
        let refused = review_on_host.validate().unwrap_err();
        assert!(refused.contains("workspace profile big"), "{refused}");
        assert!(refused.contains("apart from Henk"), "{refused}");
        assert!(crate::text::is_in_style(&refused), "{refused}");
        review_on_host.profiles.get_mut("big").unwrap().backend = BackendKind::Ssh;
        assert_eq!(review_on_host.validate(), Ok(()));
    }

    #[test]
    fn the_host_enforces_only_time_and_output() {
        assert_eq!(
            Limits::unenforced_on(BackendKind::Host),
            ["memory", "cpu", "pids", "disk"]
        );
        assert!(!BackendKind::Host.is_isolated());
        assert_eq!(BackendKind::Host.to_string(), "host");
        assert!(
            !BackendKind::Ssh.is_isolated(),
            "runs share the sandbox host"
        );
        assert_eq!(BackendKind::Ssh.to_string(), "ssh");
        assert_eq!(
            Limits::unenforced_on(BackendKind::Ssh),
            ["memory", "cpu", "pids", "disk"]
        );
    }

    #[test]
    fn a_pod_is_isolated_bounds_all_but_processes_and_has_an_image() {
        assert!(BackendKind::Kubernetes.is_isolated());
        assert_eq!(BackendKind::Kubernetes.to_string(), "kubernetes");
        assert_eq!(Limits::unenforced_on(BackendKind::Kubernetes), ["pids"]);
        let mut policy = WorkspacePolicy::default();
        policy.default.image = Some("sandbox:1".to_owned());
        assert!(policy.validate().is_err(), "the host has no image");
        policy.default.backend = BackendKind::Kubernetes;
        policy.default.review = true;
        assert_eq!(policy.validate(), Ok(()));
        assert!(policy.default.serves(EnvLane::Review));
        policy.default.image = Some(" ".to_owned());
        assert!(policy.validate().is_err(), "an empty image");
    }

    #[test]
    fn only_an_address_run_gets_workspace_tools() {
        assert!(tools(EnvLane::Review).is_empty());
        assert!(tools(EnvLane::Plan).is_empty());
        assert!(tools(EnvLane::Address).contains(&ToolKind::Exec));
    }

    #[test]
    fn a_planner_gets_a_workspace_only_off_the_host_and_when_asked() {
        let mut profile = Profile::default();
        assert!(!BackendKind::Host.runs_model_commands());
        assert!(BackendKind::Ssh.runs_model_commands());
        profile.plan = true;
        assert!(!profile.serves(EnvLane::Plan), "never on the host");
        let mut policy = WorkspacePolicy {
            default: profile.clone(),
            ..WorkspacePolicy::default()
        };
        let refused = policy.validate().unwrap_err();
        assert!(
            refused.contains("plan = true needs a backend apart from Henk"),
            "{refused}"
        );
        assert!(crate::text::is_in_style(&refused));
        policy.default.backend = BackendKind::Ssh;
        assert_eq!(policy.validate(), Ok(()));
        assert!(policy.default.serves(EnvLane::Plan));
        assert!(
            !policy.default.serves(EnvLane::Review),
            "review is its own switch"
        );
        assert!(!EnvLane::Plan.exports());
    }

    #[test]
    fn only_an_address_run_is_exported() {
        assert!(EnvLane::Address.exports());
        assert!(!EnvLane::Review.exports());
        assert!(!EnvLane::Plan.exports());
    }

    #[test]
    fn a_review_opens_a_workspace_only_when_its_profile_says_so() {
        let mut profile = Profile {
            backend: BackendKind::Ssh,
            ..Profile::default()
        };
        assert!(profile.serves(EnvLane::Address));
        assert!(!profile.serves(EnvLane::Review));
        assert!(!profile.serves(EnvLane::Plan));
        profile.review = true;
        assert!(profile.serves(EnvLane::Review));
        assert!(!profile.serves(EnvLane::Plan));
    }

    #[test]
    fn a_review_never_opens_a_workspace_on_the_host() {
        let profile = Profile {
            review: true,
            ..Profile::default()
        };
        assert_eq!(profile.backend, BackendKind::Host);
        assert!(
            !profile.serves(EnvLane::Review),
            "the pull request's setup would run as Henk"
        );
        assert!(profile.serves(EnvLane::Address));
    }
}
