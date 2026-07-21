//! `ng uninstall`: remove os hooks do not-goldfish de um harness — o
//! inverso exato de `ng install`.
//!
//! A lógica de remoção (e a garantia de escrita atômica) vive em
//! `ng_adapters::hooks::uninstall_ng_hooks`; este módulo só resolve *qual*
//! settings.json cada harness usa (mesma resolução do install) e imprime o
//! resumo. Só entradas cujo comando invoca um binário `ng-hook` são
//! removidas — todo o resto do arquivo é preservado.

use ng_adapters::hooks::{uninstall_ng_hooks, UninstallOutcome};

use crate::install::{settings_dir, Harness};
use crate::ui::Palette;

pub fn uninstall(harness: Harness, global: bool) -> anyhow::Result<()> {
    let (settings_path, label) = match harness {
        Harness::Claude => (
            settings_dir(global, ".claude")?.join("settings.json"),
            "Claude Code",
        ),
        Harness::Kimi => (
            settings_dir(global, ".kimi")?.join("settings.json"),
            "Kimi Code",
        ),
        Harness::Gemini => (
            settings_dir(global, ".gemini")?.join("settings.json"),
            "Gemini CLI",
        ),
    };

    let p = Palette::detect();
    match uninstall_ng_hooks(&settings_path).map_err(|e| anyhow::anyhow!("{e}"))? {
        UninstallOutcome::NotInstalled => {
            println!(
                "{}",
                p.muted(format!(
                    "nenhum hook do not-goldfish em {} ({label}) — nada a fazer",
                    settings_path.display()
                ))
            );
        }
        UninstallOutcome::Removed { backup, events } => {
            if let Some(backup) = backup {
                println!("{} {}", p.muted("backup:"), p.dim(backup.display()));
            }
            println!(
                "{} hooks removidos de {} ({label})",
                p.ok("✓"),
                p.bold(settings_path.display())
            );
            println!("{}", p.dim(format!("eventos: {}", events.join(", "))));
            println!(
                "{}",
                p.dim("o banco not-goldfish e as memórias capturadas ficam intactos")
            );
        }
    }
    Ok(())
}
