//! Loads the team's skills from `[skills].dir` at start (#72). Each skill is
//! a folder with a `SKILL.md`; only that file is read. Models never see a
//! path: `load_skill` serves the bodies from memory.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use henk_domain::skill::{Skill, SkillName};

use crate::config::{ConfigError, Settings};

/// Skills a folder may hold at most. Each one adds a catalogue line to
/// every prompt of an agent that has it.
const MAX_SKILLS: usize = 64;

/// The file a skill folder must hold.
const SKILL_FILE: &str = "SKILL.md";

/// The skills Henk loaded, by name.
#[derive(Debug, Default)]
pub struct SkillCatalog {
    /// Where they were read from.
    pub dir: Option<PathBuf>,
    /// The skills.
    pub skills: BTreeMap<SkillName, Arc<Skill>>,
    /// Other files in a skill's folder, which are not read.
    pub ignored: BTreeMap<SkillName, Vec<String>>,
}

impl SkillCatalog {
    /// The skills named, for one agent. Names were checked at load.
    #[must_use]
    pub fn select(&self, names: &[SkillName]) -> BTreeMap<SkillName, Arc<Skill>> {
        names
            .iter()
            .filter_map(|name| {
                self.skills
                    .get(name)
                    .map(|skill| (name.clone(), Arc::clone(skill)))
            })
            .collect()
    }
}

/// Loads the skills `settings` names into it. `config_dir` is the folder of
/// the configuration file, which a relative `[skills].dir` is resolved
/// against. Without `[skills]` this does nothing.
///
/// # Errors
///
/// Returns [`ConfigError::Skill`] when the folder cannot be read, a skill is
/// invalid, there are too many, or an agent lists a skill that is not there.
pub fn attach(settings: &mut Settings, config_dir: &Path) -> Result<(), ConfigError> {
    let Some(dir) = &settings.skills_dir else {
        return Ok(());
    };
    let dir = config_dir.join(dir);
    let catalog = load(&dir)?;
    let listed = settings
        .lanes
        .iter()
        .map(|lane| (format!("lane {}", lane.name), &lane.skills))
        .chain(
            settings
                .review
                .fact_check
                .iter()
                .map(|f| ("review.fact_check".to_owned(), &f.skills)),
        )
        .chain(
            settings
                .planning
                .iter()
                .map(|p| ("planning".to_owned(), &p.skills)),
        );
    for (what, names) in listed {
        if let Some(missing) = names.iter().find(|n| !catalog.skills.contains_key(*n)) {
            return Err(ConfigError::Skill(format!(
                "{what} lists skill \"{missing}\", which is not in {}",
                dir.display()
            )));
        }
    }
    settings.skills = Arc::new(catalog);
    Ok(())
}

/// Reads every `<dir>/<name>/SKILL.md`. A folder without one is skipped, a
/// `SKILL.md` that is a symbolic link is refused, and a file directly in
/// `dir` is ignored.
fn load(dir: &Path) -> Result<SkillCatalog, ConfigError> {
    let fail = |what: String| ConfigError::Skill(format!("{}: {what}", dir.display()));
    let entries = std::fs::read_dir(dir).map_err(|e| fail(e.to_string()))?;
    let mut folders = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|e| fail(e.to_string()))?;
        if entry.file_type().map_err(|e| fail(e.to_string()))?.is_dir() {
            folders.insert(entry.path());
        }
    }
    let mut catalog = SkillCatalog {
        dir: Some(dir.to_owned()),
        ..SkillCatalog::default()
    };
    for folder in folders {
        let file = folder.join(SKILL_FILE);
        // `symlink_metadata` does not follow links, so a linked `SKILL.md`
        // is refused instead of read from wherever it points.
        let meta = match std::fs::symlink_metadata(&file) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(fail(format!("{}: {e}", file.display()))),
        };
        if meta.file_type().is_symlink() {
            return Err(fail(format!(
                "{} is a symbolic link; a skill's {SKILL_FILE} must be a plain file",
                file.display()
            )));
        }
        if !meta.is_file() {
            continue;
        }
        let folder_name = folder
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| fail(format!("{} has no usable name", folder.display())))?;
        let text =
            std::fs::read_to_string(&file).map_err(|e| fail(format!("{}: {e}", file.display())))?;
        let skill = Skill::parse(folder_name, &text)
            .map_err(|e| fail(format!("{folder_name}/{SKILL_FILE}: {e}")))?;
        let ignored = other_files(&folder).map_err(|e| fail(e.to_string()))?;
        if !ignored.is_empty() {
            catalog.ignored.insert(skill.name().clone(), ignored);
        }
        catalog.skills.insert(skill.name().clone(), Arc::new(skill));
        if catalog.skills.len() > MAX_SKILLS {
            return Err(fail(format!("more than {MAX_SKILLS} skills")));
        }
    }
    Ok(catalog)
}

/// Every path in a skill folder other than its `SKILL.md`, relative to it.
/// A symbolic link is listed as itself and never followed, so a link cannot
/// reach outside the folder or loop back into it.
fn other_files(folder: &Path) -> std::io::Result<Vec<String>> {
    let mut found = Vec::new();
    let mut pending = vec![folder.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            // `DirEntry::file_type` does not follow links, unlike `Path::is_dir`.
            if entry.file_type()?.is_dir() {
                pending.push(path);
            } else if path != folder.join(SKILL_FILE) {
                let relative = path.strip_prefix(folder).unwrap_or(&path);
                found.push(relative.display().to_string());
            }
        }
    }
    found.sort();
    Ok(found)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;
    use crate::config::Config;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    const LANE_A: &str = r#"{ name = "lane-a", model = "proxy-fast" }"#;

    fn example(lane_a: &str) -> Result<Settings, ConfigError> {
        Config::parse(&crate::config::EXAMPLE.replacen(LANE_A, lane_a, 1))?.into_settings()
    }

    fn with_skills() -> Settings {
        let mut settings = example(LANE_A).unwrap();
        settings.skills_dir = Some("skills".to_owned());
        settings
    }

    fn name(name: &str) -> SkillName {
        SkillName::parse(name).unwrap()
    }

    #[test]
    fn skills_load_with_their_ignored_files() {
        let mut s = with_skills();
        attach(&mut s, Path::new(FIXTURES)).unwrap();
        let names: Vec<&str> = s.skills.skills.keys().map(SkillName::as_str).collect();
        assert_eq!(names, ["rust-errors", "sql-migrations"]);
        assert_eq!(
            s.skills.ignored[&name("sql-migrations")],
            ["examples/rollback.sql"]
        );
        assert!(!s.skills.ignored.contains_key(&name("rust-errors")));
    }

    #[test]
    fn an_agent_may_list_only_skills_that_exist() {
        let mut s = with_skills();
        s.lanes[0].skills = vec![name("no-such-skill")];
        let error = attach(&mut s, Path::new(FIXTURES)).unwrap_err().to_string();
        assert!(error.contains("no-such-skill"), "{error}");
        assert!(s.skills.skills.is_empty(), "nothing is attached on failure");
    }

    #[test]
    fn select_gives_an_agent_only_its_own_skills() {
        let mut s = with_skills();
        attach(&mut s, Path::new(FIXTURES)).unwrap();
        let picked = s.skills.select(&[name("rust-errors")]);
        assert_eq!(picked.keys().collect::<Vec<_>>(), [&name("rust-errors")]);
    }

    #[test]
    fn a_folder_that_cannot_be_read_is_an_error() {
        let mut s = with_skills();
        s.skills_dir = Some("no-such-folder".to_owned());
        assert!(matches!(
            attach(&mut s, Path::new(FIXTURES)),
            Err(ConfigError::Skill(_))
        ));
    }

    #[test]
    fn listing_skills_needs_a_skills_section() {
        let lane = r#"{ name = "lane-a", model = "proxy-fast", skills = ["rust-errors"] }"#;
        let error = example(lane).unwrap_err().to_string();
        assert!(error.contains("no [skills] section"), "{error}");
    }

    #[test]
    fn a_malformed_skill_name_is_refused() {
        let lane = r#"{ name = "lane-a", model = "proxy-fast", skills = ["Bad Name"] }"#;
        assert!(matches!(example(lane), Err(ConfigError::Syntax(_))));
    }

    #[cfg(unix)]
    #[test]
    fn links_in_a_skill_folder_are_listed_not_followed() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("henk-skill-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let outside = root.join("outside");
        let skill = root.join("skills").join("linked");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(skill.join("sub")).unwrap();
        std::fs::write(outside.join("secret.txt"), "x").unwrap();
        std::fs::write(skill.join(SKILL_FILE), "not parsed here").unwrap();
        symlink(outside.join("secret.txt"), skill.join("file-link")).unwrap();
        symlink(&outside, skill.join("dir-link")).unwrap();
        symlink(&skill, skill.join("sub").join("loop")).unwrap();

        let ignored = other_files(&skill).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(ignored, ["dir-link", "file-link", "sub/loop"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_skill_file_is_refused() {
        use std::os::unix::fs::symlink;
        let root =
            std::env::temp_dir().join(format!("henk-skill-file-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let skills = root.join("skills");
        let outside = root.join("outside.md");
        std::fs::create_dir_all(skills.join("rust-errors")).unwrap();
        std::fs::copy(
            Path::new(FIXTURES)
                .join("skills/rust-errors")
                .join(SKILL_FILE),
            &outside,
        )
        .unwrap();
        symlink(&outside, skills.join("rust-errors").join(SKILL_FILE)).unwrap();

        let result = load(&skills);
        std::fs::remove_dir_all(&root).unwrap();
        let error = result.unwrap_err().to_string();
        assert!(error.contains("symbolic link"), "{error}");
    }

    #[test]
    fn without_skills_nothing_is_read() {
        let mut s = example(LANE_A).unwrap();
        attach(&mut s, Path::new("/nonexistent")).unwrap();
        assert!(s.skills.skills.is_empty());
    }
}
