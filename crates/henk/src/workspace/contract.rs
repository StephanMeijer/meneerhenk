//! What every backend must do, as functions over a provider: each backend's
//! tests call them all (#81). They use only the `Workspace` interface, and
//! plant links and modes with commands, so they run against a backend
//! whose files are not on this machine.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

use std::sync::Arc;
use std::time::Duration;

use henk_domain::address::WorkspacePath;
use henk_domain::workspace::{FileMode, Limits, Profile};

use henk_domain::ignore::PathFilter;

use super::{Hit, Pattern, Workspace, WorkspaceError, WorkspaceProvider};
use crate::git::ScratchDir;

/// A checkout to import: two files, one of them executable, and a `.git`
/// that no tool may show.
pub fn source(name: &str) -> ScratchDir {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = ScratchDir::new(name).unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::write(
        dir.path().join("src/a.rs"),
        "fn main() {\n    let x = 1;\n}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("run.sh"), "#!/bin/sh\necho run\n").unwrap();
    std::fs::set_permissions(
        dir.path().join("run.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::write(dir.path().join(".git/config"), "[core]\n").unwrap();
    dir
}

fn path(text: &str) -> WorkspacePath {
    WorkspacePath::parse(text).unwrap()
}

fn sh(script: &str) -> Vec<String> {
    vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()]
}

async fn open(
    provider: &Arc<dyn WorkspaceProvider>,
    name: &str,
    limits: Limits,
) -> Arc<dyn Workspace> {
    let source = source(name);
    let profile = Profile {
        limits,
        ..Profile::default()
    };
    provider.open(source.path(), &profile).await.unwrap()
}

/// Runs every check against `provider`. `name` keeps scratch directories
/// of different backends apart.
pub async fn every_backend_does_this(provider: Arc<dyn WorkspaceProvider>, name: &str) {
    files_are_read_written_listed_and_searched(&provider, name).await;
    patterns_mean_the_same_beyond_ascii(&provider, name).await;
    links_out_of_the_tree_and_into_git_are_refused(&provider, name).await;
    commands_run_with_an_empty_environment_and_limits(&provider, name).await;
    the_changes_are_exported_with_their_modes(&provider, name).await;
    changes_count_from_the_baseline(&provider, name).await;
    a_closed_workspace_is_gone(&provider, name).await;
}

async fn files_are_read_written_listed_and_searched(
    provider: &Arc<dyn WorkspaceProvider>,
    name: &str,
) {
    let ws = open(provider, &format!("{name}-files"), Limits::default()).await;
    let a = ws.read(&path("src/a.rs"), 1024).await.unwrap();
    assert_eq!(a, b"fn main() {\n    let x = 1;\n}\n");
    assert!(matches!(
        ws.read(&path("src/a.rs"), 4).await,
        Err(WorkspaceError::Refused(_))
    ));
    assert!(matches!(
        ws.read(&path("src/none.rs"), 1024).await,
        Err(WorkspaceError::NotFound(_))
    ));
    assert!(matches!(
        ws.read(&path("src"), 1024).await,
        Err(WorkspaceError::Refused(_))
    ));

    ws.write(&path("src/deep/new.rs"), b"// new\n")
        .await
        .unwrap();
    assert_eq!(
        ws.read(&path("src/deep/new.rs"), 1024).await.unwrap(),
        b"// new\n"
    );
    assert!(matches!(
        ws.write(&path("src"), b"x").await,
        Err(WorkspaceError::Refused(_))
    ));
    assert!(matches!(
        ws.write(&path("run.sh/inside"), b"x").await,
        Err(WorkspaceError::Refused(_))
    ));

    let root = WorkspacePath::root();
    let files = ws.list(&root, None, 100).await.unwrap();
    assert_eq!(
        files,
        ["run.sh", "src/a.rs", "src/deep/new.rs"],
        ".git is never listed"
    );
    assert_eq!(ws.list(&root, None, 1).await.unwrap().len(), 1);
    let rust = PathFilter::new(["*.rs"]);
    assert_eq!(
        ws.list(&root, Some(&rust), 100).await.unwrap(),
        ["src/a.rs", "src/deep/new.rs"],
        "a name glob matches at any depth"
    );
    let top = PathFilter::new(["src/*.rs"]);
    assert_eq!(
        ws.list(&root, Some(&top), 1).await.unwrap(),
        ["src/a.rs"],
        "the glob is applied before the cap"
    );

    let find = |source: &str| Pattern::parse(source).unwrap();
    let hits = ws
        .search(&root, &find("let x"), None, 1024, 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!((hits[0].path.as_str(), hits[0].line), ("src/a.rs", 2));
    assert_eq!(hits[0].text, "let x = 1;");
    let shown = |hits: Vec<Hit>| -> Vec<(String, usize)> {
        hits.into_iter().map(|h| (h.path, h.line)).collect()
    };
    assert_eq!(
        shown(
            ws.search(&root, &find(r"\blet\s+x\s*=\s*\d+;$"), None, 1024, 10)
                .await
                .unwrap()
        ),
        [("src/a.rs".to_owned(), 2)],
        "classes, word boundaries and anchors"
    );
    assert_eq!(
        shown(
            ws.search(&root, &find("(?i)FN MAIN|^// NEW"), None, 1024, 10)
                .await
                .unwrap()
        ),
        [
            ("src/a.rs".to_owned(), 1),
            ("src/deep/new.rs".to_owned(), 1)
        ],
        "any case and alternation, sorted by path"
    );
    let deep = PathFilter::new(["src/deep/**"]);
    assert_eq!(
        shown(
            ws.search(&root, &find("."), Some(&deep), 1024, 10)
                .await
                .unwrap()
        ),
        [("src/deep/new.rs".to_owned(), 1)],
        "only the files the glob matches"
    );
    assert_eq!(
        ws.search(&root, &find("."), None, 1024, 2)
            .await
            .unwrap()
            .len(),
        2,
        "capped"
    );
    assert!(
        ws.search(&root, &find("core"), None, 1024, 10)
            .await
            .unwrap()
            .is_empty(),
        "nothing in .git is searched"
    );
    assert!(
        ws.search(&root, &find("let x"), None, 5, 10)
            .await
            .unwrap()
            .is_empty(),
        "a file over the size is skipped"
    );
    ws.close().await;
}

/// `\w`, `\s` and `\b` know letters and spaces beyond ASCII and `\d` is the
/// ASCII digits only, on every backend and whatever grep's version; a file
/// the run's user cannot read is skipped, not an error.
async fn patterns_mean_the_same_beyond_ascii(provider: &Arc<dyn WorkspaceProvider>, name: &str) {
    let ws = open(provider, &format!("{name}-unicode"), Limits::default()).await;
    ws.write(
        &path("u.txt"),
        "caf\u{e9} = 7\nx = \u{663}\nfoo\u{a0}bar\n".as_bytes(),
    )
    .await
    .unwrap();
    ws.write(&path("locked.txt"), b"locked 7\n").await.unwrap();
    let locked = ws
        .exec(
            &sh("chmod 000 locked.txt"),
            &WorkspacePath::root(),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert_eq!(locked.code, Some(0), "{}", locked.output);
    let only = PathFilter::new(["u.txt"]);
    let lines = async |source: &str| -> Vec<usize> {
        ws.search(
            &WorkspacePath::root(),
            &Pattern::parse(source).unwrap(),
            Some(&only),
            1024,
            10,
        )
        .await
        .unwrap()
        .into_iter()
        .map(|h| h.line)
        .collect()
    };
    assert_eq!(
        lines(r"\bcaf\w\b").await,
        [1],
        "a letter beyond ASCII is a word character"
    );
    assert!(
        lines(r"caf\b").await.is_empty(),
        "no boundary inside a word"
    );
    assert_eq!(lines(r"foo\sbar").await, [3], "a no-break space is a space");
    assert_eq!(
        lines("(?i)^CAF\u{c9}\\b").await,
        [1],
        "any case beyond ASCII"
    );
    assert_eq!(lines(r"\d").await, [1], "an Arabic-Indic digit is not \\d");
    assert_eq!(lines(r"[\d]").await, [1], "nor inside a class");
    assert_eq!(lines(r"^\D+$").await, [2, 3], "but it is \\D");
    assert_eq!(
        lines(r"\p{Nd}").await,
        [1, 2],
        "Unicode digits are asked for by name"
    );
    assert!(
        ws.search(
            &WorkspacePath::root(),
            &Pattern::parse("7").unwrap(),
            None,
            1024,
            10
        )
        .await
        .is_ok(),
        "an unreadable file does not fail the search"
    );
    ws.close().await;
}

async fn links_out_of_the_tree_and_into_git_are_refused(
    provider: &Arc<dyn WorkspaceProvider>,
    name: &str,
) {
    let ws = open(provider, &format!("{name}-links"), Limits::default()).await;
    let planted = ws
        .exec(
            // A backend may import without `.git`; the link must lead into one.
            &sh("mkdir -p .git && touch .git/config && ln -s /etc escape && ln -s .git into-git && ln -s src/a.rs alias.rs"),
            &WorkspacePath::root(),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert_eq!(planted.code, Some(0), "{}", planted.output);
    for (link, why) in [
        ("escape/passwd", "out of the repository"),
        ("into-git/config", ".git"),
    ] {
        let refused = ws.read(&path(link), 1 << 20).await;
        assert!(
            matches!(&refused, Err(WorkspaceError::Refused(m)) if m.contains(why)),
            "{link}: {refused:?}"
        );
    }
    assert_eq!(
        ws.read(&path("alias.rs"), 1024).await.unwrap(),
        b"fn main() {\n    let x = 1;\n}\n",
        "a link inside the tree reads what it points to"
    );
    assert!(matches!(
        ws.write(&path("alias.rs"), b"x").await,
        Err(WorkspaceError::Refused(m)) if m.contains("symbolic link")
    ));
    assert!(matches!(
        ws.write(&path("escape/new"), b"x").await,
        Err(WorkspaceError::Refused(_))
    ));
    assert!(matches!(
        ws.exec(&sh("true"), &path("escape"), Duration::from_secs(30))
            .await,
        Err(WorkspaceError::Refused(_))
    ));
    ws.close().await;
}

async fn commands_run_with_an_empty_environment_and_limits(
    provider: &Arc<dyn WorkspaceProvider>,
    name: &str,
) {
    let limits = Limits {
        command_secs: 2,
        run_secs: 4,
        output_bytes: 64,
        ..Limits::default()
    };
    let roomy = open(provider, &format!("{name}-env"), Limits::default()).await;
    let env = roomy
        .exec(
            &["env".to_owned()],
            &WorkspacePath::root(),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    let mut names: Vec<&str> = env
        .output
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, _)| k))
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["HOME", "LANG", "PATH"], "{}", env.output);
    let home = env
        .output
        .lines()
        .find_map(|l| l.strip_prefix("HOME="))
        .unwrap()
        .to_owned();
    let pwd = roomy
        .exec(
            &["pwd".to_owned()],
            &WorkspacePath::root(),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert_ne!(pwd.output.trim(), home, "HOME is not the tree");
    roomy.close().await;

    let ws = open(provider, &format!("{name}-exec"), limits).await;

    let failed = ws
        .exec(
            &sh("echo no; exit 3"),
            &path("src"),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert_eq!((failed.code, failed.timed_out), (Some(3), false));
    assert_eq!(failed.output.trim(), "no");

    let long = ws
        .exec(
            &sh("yes x | head -c 1000"),
            &WorkspacePath::root(),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert!(long.output.len() <= 64 + 20, "{}", long.output.len());
    assert!(long.output.starts_with("[... cut ...]"));

    let slow = ws
        .exec(
            &sh("sleep 30"),
            &WorkspacePath::root(),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert!(slow.timed_out, "the profile's 2 s per command holds");
    assert_eq!(slow.code, None);
    let _ = ws
        .exec(
            &sh("sleep 30"),
            &WorkspacePath::root(),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    let gone = ws
        .exec(&sh("true"), &WorkspacePath::root(), Duration::from_secs(30))
        .await
        .unwrap();
    assert!(
        gone.timed_out && gone.output.contains("used up"),
        "{}",
        gone.output
    );
    ws.close().await;
}

async fn the_changes_are_exported_with_their_modes(
    provider: &Arc<dyn WorkspaceProvider>,
    name: &str,
) {
    let ws = open(provider, &format!("{name}-export"), Limits::default()).await;
    assert!(ws.export().await.unwrap().is_empty(), "nothing changed yet");
    ws.write(&path("src/a.rs"), b"fn main() {}\n")
        .await
        .unwrap();
    let ran = ws
        .exec(
            &sh("rm run.sh && printf '#!/bin/sh\\n' > tool && chmod +x tool && ln -s src/a.rs link && mkdir -p .git && echo x > .git/HEAD2"),
            &WorkspacePath::root(),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert_eq!(ran.code, Some(0), "{}", ran.output);
    let mut changes = ws.export().await.unwrap();
    changes.sort_by(|a, b| a.raw.path.cmp(&b.raw.path));
    let seen: Vec<(&str, FileMode, bool)> = changes
        .iter()
        .map(|c| (c.raw.path.as_str(), c.raw.mode, c.raw.deleted))
        .collect();
    assert_eq!(
        seen,
        [
            ("link", FileMode::Symlink, false),
            ("run.sh", FileMode::Executable, true),
            ("src/a.rs", FileMode::Regular, false),
            ("tool", FileMode::Executable, false),
        ],
        "nothing in .git is a change"
    );
    assert_eq!(changes[2].content, b"fn main() {}\n");
    assert_eq!(
        ws.export().await.unwrap().len(),
        4,
        "exporting twice is fine"
    );
    ws.close().await;
}

async fn changes_count_from_the_baseline(provider: &Arc<dyn WorkspaceProvider>, name: &str) {
    let ws = open(provider, &format!("{name}-baseline"), Limits::default()).await;
    ws.write(&path("src/a.rs"), b"fn main() {}\n")
        .await
        .unwrap();
    ws.write(&path("generated.txt"), b"setup\n").await.unwrap();
    ws.baseline().await.unwrap();
    assert!(
        ws.export().await.unwrap().is_empty(),
        "what was there at the baseline is not a change"
    );
    ws.write(&path("src/a.rs"), b"fn main() { run(); }\n")
        .await
        .unwrap();
    let changes = ws.export().await.unwrap();
    let seen: Vec<&str> = changes.iter().map(|c| c.raw.path.as_str()).collect();
    assert_eq!(seen, ["src/a.rs"], "only what changed after it");
    assert_eq!(changes[0].content, b"fn main() { run(); }\n");
    ws.close().await;
}

async fn a_closed_workspace_is_gone(provider: &Arc<dyn WorkspaceProvider>, name: &str) {
    let ws = open(provider, &format!("{name}-close"), Limits::default()).await;
    ws.close().await;
    ws.close().await;
    assert!(matches!(
        ws.read(&path("src/a.rs"), 1024).await,
        Err(WorkspaceError::Backend(_))
    ));
}
