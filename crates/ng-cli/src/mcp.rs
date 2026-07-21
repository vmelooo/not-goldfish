//! `ng mcp install-browser-use`: registers the `browser-use` MCP server
//! (Playwright-driven browser automation over MCP) with a harness.

use std::path::PathBuf;

use anyhow::Context;
use clap::ValueEnum;
use ng_adapters::mcp::{browser_use_entry, register_mcp_claude, register_mcp_codex};

use crate::ui::Palette;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum McpHarness {
    Claude,
    Codex,
}

/// `--global` only affects Claude Code (project vs `~/.claude`); Codex's
/// `config.toml` has no per-project equivalent, so it's ignored there.
pub fn install_browser_use(harness: McpHarness, global: bool) -> anyhow::Result<()> {
    let (name, command, args) = browser_use_entry();

    let path = match harness {
        McpHarness::Claude if global => dirs::home_dir()
            .context("sem home dir")?
            .join(".claude/settings.json"),
        McpHarness::Claude => std::env::current_dir()?.join(".claude/settings.json"),
        McpHarness::Codex => dirs::home_dir()
            .context("sem home dir")?
            .join(".codex/config.toml"),
    };

    let backup: Option<PathBuf> = match harness {
        McpHarness::Claude => register_mcp_claude(&path, name, command, &args),
        McpHarness::Codex => register_mcp_codex(&path, name, command, &args),
    }
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    let p = Palette::detect();
    if let Some(backup) = backup {
        println!("{} {}", p.muted("backup:"), p.dim(backup.display()));
    }
    println!(
        "{} mcp server '{}' registrado em {}",
        p.ok("✓"),
        p.teal(name),
        p.bold(path.display())
    );
    println!(
        "{}",
        p.warn(
            "aviso: requer `uvx` instalado (https://docs.astral.sh/uv/) para o servidor browser-use subir"
        )
    );
    Ok(())
}
