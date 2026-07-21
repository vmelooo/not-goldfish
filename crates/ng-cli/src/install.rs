//! `ng install`: registers `ng-hook` in a harness's hook configuration.
//!
//! The actual merge/backup logic lives in `ng_adapters::hooks` (shared,
//! tested there against a tempdir) — this module only resolves *which*
//! settings file and command string each harness needs and prints the
//! CLI-facing summary.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::ValueEnum;
use ng_adapters::hooks::{
    install_claude_style, install_gemini, DEFAULT_CLAUDE_EVENTS, DEFAULT_GEMINI_EVENTS,
};
use ng_core::{paths, Store};

use crate::i18n::{fill, Msgs};
use crate::ui::Palette;
use crate::util::find_sibling_binary;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Harness {
    Claude,
    Gemini,
    Kimi,
}

pub fn install(harness: Harness, global: bool) -> anyhow::Result<()> {
    let m = Msgs::get();
    let hook_bin = find_sibling_binary("ng-hook").context(m.install_hook_not_found)?;

    let (settings_path, command, events, label) = match harness {
        Harness::Claude => (
            settings_dir(global, ".claude")?.join("settings.json"),
            // NG_HARNESS is left unset here on purpose: ng-hook already
            // defaults to "claude-code" when the env var is absent, so
            // Claude Code's hook command stays the plain binary path.
            hook_bin.to_string_lossy().into_owned(),
            DEFAULT_CLAUDE_EVENTS,
            "Claude Code",
        ),
        Harness::Kimi => (
            settings_dir(global, ".kimi")?.join("settings.json"),
            // Kimi Code clones Claude Code's hooks schema (see
            // ng_adapters::hooks docs), so it shares DEFAULT_CLAUDE_EVENTS;
            // what differs is the harness label recorded on each event,
            // set via an inline `env` prefix since the hook runs via shell.
            // [finding 08] hook_bin is shell-quoted: the harness invokes
            // this whole string through a shell, so an unquoted path
            // containing a space would silently split into extra argv
            // entries instead of naming one binary.
            format!("env NG_HARNESS=kimi {}", shell_quote(&hook_bin)),
            DEFAULT_CLAUDE_EVENTS,
            "Kimi Code",
        ),
        Harness::Gemini => (
            settings_dir(global, ".gemini")?.join("settings.json"),
            format!("env NG_HARNESS=gemini {}", shell_quote(&hook_bin)),
            DEFAULT_GEMINI_EVENTS,
            "Gemini CLI",
        ),
    };

    let backup = match harness {
        Harness::Gemini => install_gemini(&settings_path, &command),
        Harness::Claude | Harness::Kimi => install_claude_style(&settings_path, &command, events),
    }
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    let p = Palette::detect();
    if let Some(backup) = backup {
        println!("{} {}", p.muted(m.install_backup), p.dim(backup.display()));
    }
    println!(
        "{} {}",
        p.ok("✓"),
        fill(
            m.install_hooks_installed,
            &[
                ("{path}", &p.bold(settings_path.display())),
                ("{label}", &label),
            ]
        )
    );
    println!(
        "{}",
        p.dim(fill(m.install_events, &[("{list}", &events.join(", "))]))
    );

    // [finding 01] Warm up the schema (RW open runs the idempotent DDL)
    // once, here, so the very first prompt after install already has a
    // database — including the fts5vocab table injection's read-only hot
    // path depends on — to read instead of finding nothing and silently
    // skipping injection. Best-effort: a failure here (e.g. no write
    // access to NG_DATA_DIR yet) doesn't block install; the first capture
    // will create it the normal way instead.
    if let Err(err) = Store::open(&paths::db_path()) {
        eprintln!("{}", fill(m.install_db_init_warn, &[("{err}", &err)]));
    }

    println!("{}", p.dim(m.install_hint));
    Ok(())
}

/// [finding 08] POSIX single-quote `path` for embedding in a shell command
/// string. The `env NG_HARNESS=... <path>` commands above are executed by
/// the harness through a shell (`sh -c` or equivalent), so an unquoted
/// path containing a space (or any other shell metacharacter) would break
/// apart into multiple argv entries instead of naming one binary. Wraps in
/// `'...'` and escapes any embedded `'` as `'\''` (close quote, escaped
/// literal quote, reopen quote) — the standard POSIX technique, since a
/// single-quoted string cannot contain a literal `'` any other way.
fn shell_quote(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("'{}'", raw.replace('\'', r"'\''"))
}

/// Shared with `uninstall.rs` so both commands resolve the exact same
/// settings file for a given (harness, --global) pair.
pub(crate) fn settings_dir(global: bool, dir_name: &str) -> anyhow::Result<PathBuf> {
    if global {
        Ok(dirs::home_dir().context("sem home dir")?.join(dir_name))
    } else {
        Ok(std::env::current_dir()?.join(dir_name))
    }
}

#[cfg(test)]
mod shell_quote_tests {
    use super::*;
    use std::process::Command;

    /// Feeds `quoted` to a real `/bin/sh` inside `printf '%s' <quoted>` and
    /// returns what the shell actually produced — the thing that matters
    /// isn't that `shell_quote`'s output *looks* right, it's that a real
    /// POSIX shell parses it back into exactly the original string.
    fn round_trip_through_shell(quoted: &str) -> String {
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s' {quoted}"))
            .output()
            .expect("sh must be available to run this test");
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn round_trips_a_path_with_a_space() {
        let path = PathBuf::from("/opt/not goldfish/ng-hook");
        let quoted = shell_quote(&path);
        assert_eq!(round_trip_through_shell(&quoted), path.to_string_lossy());
    }

    #[test]
    fn round_trips_a_path_with_an_embedded_single_quote() {
        let path = PathBuf::from("/opt/it's a dir/ng-hook");
        let quoted = shell_quote(&path);
        assert_eq!(round_trip_through_shell(&quoted), path.to_string_lossy());
    }

    #[test]
    fn round_trips_a_plain_path_unchanged() {
        let path = PathBuf::from("/usr/local/bin/ng-hook");
        let quoted = shell_quote(&path);
        assert_eq!(round_trip_through_shell(&quoted), path.to_string_lossy());
    }

    #[test]
    fn kimi_install_command_survives_real_shell_word_splitting() {
        // The actual thing that matters: not just that shell_quote's output
        // round-trips in isolation, but that the full command string this
        // module builds (`env NG_HARNESS=kimi <quoted path>`) splits into
        // exactly the argv the harness's shell would hand to exec — one
        // "env", one "NG_HARNESS=kimi", one path, never more.
        let hook_bin = PathBuf::from("/opt/not goldfish/ng-hook");
        let command = format!("env NG_HARNESS=kimi {}", shell_quote(&hook_bin));

        let script = format!(r#"set -- {command}; for a in "$@"; do printf '%s\n' "$a"; done"#);
        let output = Command::new("sh").arg("-c").arg(script).output().unwrap();
        let argv: Vec<String> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();

        assert_eq!(
            argv,
            vec![
                "env".to_string(),
                "NG_HARNESS=kimi".to_string(),
                hook_bin.to_string_lossy().into_owned()
            ]
        );
    }
}
