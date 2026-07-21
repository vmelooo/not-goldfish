//! `ng sync`: keeps harness-agnostic personas in sync with each harness's
//! own subagent format.
//!
//! [`run_sync`] is the pure(ish) core — it takes every path as a parameter
//! (no `$HOME`/cwd lookups inside), so it's testable against a tempdir the
//! same way `ng_adapters` itself is. [`sync`] resolves the real paths and
//! prints the CLI-facing summary.

use std::path::{Path, PathBuf};

use anyhow::Context;
use ng_adapters::personas::{default_personas, load_personas, sync_claude, sync_opencode, Persona};

use crate::i18n::{fill, Msgs};
use crate::ui::Palette;

pub struct SyncReport {
    pub personas_seeded: bool,
    pub persona_count: usize,
    pub agents_dir: PathBuf,
    pub opencode_backup: Option<PathBuf>,
}

/// Loads personas from `personas_dir` (seeding [`default_personas`] there
/// first if it's empty/missing — that seed write reuses [`sync_claude`]
/// since a personas dir is just another frontmatter-md directory), syncs
/// them into `agents_dir`, and additionally into `opencode_path` when it's
/// `Some` and already exists (an opencode.json the caller didn't create is
/// not something `ng sync` should conjure into existence).
pub fn run_sync(
    personas_dir: &Path,
    agents_dir: &Path,
    opencode_path: Option<&Path>,
) -> anyhow::Result<SyncReport> {
    let mut personas: Vec<Persona> = load_personas(personas_dir);
    let personas_seeded = personas.is_empty();
    if personas_seeded {
        sync_claude(&default_personas(), personas_dir).map_err(|e| anyhow::anyhow!("{e}"))?;
        personas = load_personas(personas_dir);
    }

    sync_claude(&personas, agents_dir).map_err(|e| anyhow::anyhow!("{e}"))?;

    let opencode_backup = match opencode_path {
        Some(path) if path.exists() => {
            sync_opencode(&personas, path).map_err(|e| anyhow::anyhow!("{e}"))?
        }
        _ => None,
    };

    Ok(SyncReport {
        personas_seeded,
        persona_count: personas.len(),
        agents_dir: agents_dir.to_path_buf(),
        opencode_backup,
    })
}

pub fn sync(personas_dir: Option<PathBuf>, global: bool) -> anyhow::Result<()> {
    let personas_dir = personas_dir.unwrap_or_else(|| ng_core::paths::data_dir().join("personas"));
    let agents_dir = if global {
        dirs::home_dir()
            .context("sem home dir")?
            .join(".claude/agents")
    } else {
        std::env::current_dir()?.join(".claude/agents")
    };
    let opencode_path = std::env::current_dir()?.join("opencode.json");

    let report = run_sync(&personas_dir, &agents_dir, Some(&opencode_path))?;

    let m = Msgs::get();
    let p = Palette::detect();
    if report.personas_seeded {
        println!(
            "{}",
            fill(
                m.sync_seeded,
                &[("{path}", &p.bold(personas_dir.display()))]
            )
        );
    }
    println!(
        "{} {}",
        p.ok("✓"),
        fill(
            m.sync_synced,
            &[
                ("{count}", &p.teal(report.persona_count)),
                ("{path}", &p.bold(report.agents_dir.display())),
            ]
        )
    );
    if let Some(backup) = report.opencode_backup {
        println!(
            "{}",
            p.dim(fill(m.sync_opencode, &[("{path}", &backup.display())]))
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_default_personas_and_syncs_three_files() {
        let tmp = tempfile::tempdir().unwrap();
        let personas_dir = tmp.path().join("personas"); // does not exist yet
        let agents_dir = tmp.path().join(".claude/agents");

        let report = run_sync(&personas_dir, &agents_dir, None).unwrap();
        assert!(report.personas_seeded);
        assert_eq!(report.persona_count, 3);

        let mut files: Vec<String> = std::fs::read_dir(&agents_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        assert_eq!(files, vec!["ceo.md", "dev.md", "pm.md"]);
    }

    #[test]
    fn does_not_reseed_when_personas_already_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let personas_dir = tmp.path().join("personas");
        let agents_dir = tmp.path().join("agents");
        run_sync(&personas_dir, &agents_dir, None).unwrap();

        let report = run_sync(&personas_dir, &agents_dir, None).unwrap();
        assert!(!report.personas_seeded);
        assert_eq!(report.persona_count, 3);
    }

    #[test]
    fn syncs_opencode_json_only_when_it_already_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let personas_dir = tmp.path().join("personas");
        let agents_dir = tmp.path().join("agents");
        let opencode_path = tmp.path().join("opencode.json");

        let report = run_sync(&personas_dir, &agents_dir, Some(&opencode_path)).unwrap();
        assert!(
            report.opencode_backup.is_none(),
            "opencode.json didn't exist, sync must not create it"
        );
        assert!(!opencode_path.exists());

        std::fs::write(&opencode_path, "{}").unwrap();
        let report = run_sync(&personas_dir, &agents_dir, Some(&opencode_path)).unwrap();
        assert!(
            report.opencode_backup.is_some(),
            "opencode.json existed, must be backed up before rewrite"
        );

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&opencode_path).unwrap()).unwrap();
        assert!(value["agent"]["ceo"].is_object());
    }
}
