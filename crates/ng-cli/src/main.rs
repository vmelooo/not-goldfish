//! ng: the not-goldfish CLI.

mod clear;
mod daemon;
mod dispatch;
mod doctor;
mod gain;
mod i18n;
mod install;
mod mcp;
mod memory;
mod saver;
mod search;
mod status;
mod sync;
mod sync_context;
mod ui;
mod ui_cmd;
mod uninstall;
mod util;
mod wisdom;

use std::path::PathBuf;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

use i18n::Msgs;
use install::Harness;
use mcp::McpHarness;

#[derive(Parser)]
#[command(
    name = "ng",
    about = "not-goldfish: memória universal para harnesses de IA",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Registra os hooks do not-goldfish num harness (Claude Code por padrão)
    Install {
        /// Instalar no settings global em vez do projeto atual
        #[arg(long)]
        global: bool,
        /// Harness alvo: claude, gemini ou kimi
        #[arg(long, value_enum, default_value = "claude")]
        harness: Harness,
    },
    /// Remove os hooks do not-goldfish de um harness (inverso do install;
    /// banco e memórias capturadas ficam intactos)
    Uninstall {
        /// Remover do settings global em vez do projeto atual
        #[arg(long)]
        global: bool,
        /// Harness alvo: claude, gemini ou kimi
        #[arg(long, value_enum, default_value = "claude")]
        harness: Harness,
    },
    /// Imprime o script de autocompletar do `ng` para o shell dado
    /// (ex.: `ng completions bash >> ~/.bashrc` ou o dir de completions)
    Completions {
        /// Shell alvo: bash, zsh, fish, elvish ou powershell
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Busca na memória persistente
    Search {
        /// Termos de busca
        query: Vec<String>,
        /// Limitar ao projeto atual
        #[arg(long)]
        here: bool,
        /// Máximo de resultados
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Busca híbrida: recall por FTS, rerank por similaridade semântica
        #[arg(long)]
        semantic: bool,
        /// Saída JSON estável (para scripts)
        #[arg(long)]
        json: bool,
    },
    /// Higiene procedural lossless: colapsa itens frios da sessão ativa em
    /// stubs recuperáveis (com backup). Nada é perdido — tudo continua no
    /// banco e volta com `ng search`.
    Clear {
        /// Caminho do transcript a limpar (padrão: sessão Claude Code mais
        /// recente deste projeto)
        #[arg(long)]
        file: Option<PathBuf>,
        /// Orçamento de tokens alvo do contexto vivo (itens frios acima disso
        /// viram stub)
        #[arg(long, default_value_t = 4000)]
        target_tokens: i64,
        /// Só mostra o que seria colapsado, sem reescrever
        #[arg(long)]
        dry_run: bool,
    },
    /// Mostra o estado do banco e do daemon
    Status {
        /// Saída JSON estável (para scripts)
        #[arg(long)]
        json: bool,
    },
    /// Inicia o daemon em foreground (use um service manager para background)
    Daemon,
    /// Abre a UI web de gerenciamento de contexto (inicia o daemon em
    /// background se necessário)
    Ui,
    /// Diagnóstico do ambiente: binários, daemon, banco, hooks, UI, backlog
    Doctor,
    /// Integrações MCP (servidores registrados por comando)
    Mcp {
        #[command(subcommand)]
        action: McpCommand,
    },
    /// Sincroniza personas universais (~/.not-goldfish/personas) para o
    /// formato de subagente de cada harness
    Sync {
        /// Diretório de personas de origem (padrão: ~/.not-goldfish/personas)
        #[arg(long)]
        personas_dir: Option<PathBuf>,
        /// Sincronizar em ~/.claude/agents em vez do projeto atual
        #[arg(long)]
        global: bool,
    },
    /// Sugere modelo/categoria para um prompt (dispatch inteligente)
    Dispatch {
        /// Prompt a classificar (ignorado com --init)
        prompt: Vec<String>,
        /// Escreve o dispatch.toml padrão (comentado) para editar
        #[arg(long)]
        init: bool,
    },
    /// Mostra o grafo de sabedoria (entidades/decisões extraídas das sessões)
    Wisdom {
        /// Limitar ao projeto atual
        #[arg(long)]
        here: bool,
        /// Exporta em Markdown (para colar em CLAUDE.md/AGENTS.md)
        #[arg(long)]
        md: bool,
        /// Saída JSON estável (para scripts)
        #[arg(long, conflicts_with = "md")]
        json: bool,
        /// Reconstrói o grafo do zero re-ingerindo todo o histórico com as
        /// regras atuais (entities/relations são derivadas; events intocada).
        #[arg(long)]
        rebuild: bool,
    },
    /// Inspeciona e edita a memória própria do not-goldfish (ocultar é
    /// reversível — nada é apagado)
    Memory {
        #[command(subcommand)]
        action: memory::MemoryCommand,
    },
    /// Benefício acumulado desde a adoção: capturas, injeções, higiene
    Gain {
        /// Limitar ao projeto atual (cwd)
        #[arg(long)]
        here: bool,
        /// Saída JSON estável (para scripts)
        #[arg(long)]
        json: bool,
        /// Só contar a partir desta data (YYYY-MM-DD)
        #[arg(long)]
        since: Option<String>,
    },
    /// Savers externos (compressores de token plugáveis): init, list e o
    /// gate de medição bench — tudo OFF por default
    Saver {
        #[command(subcommand)]
        action: saver::SaverCommand,
    },
    /// (Re)gera .ng/ — projeção commitável da memória deste projeto
    SyncContext {
        /// Cria também .ng/config.toml comentado (nunca sobrescreve um existente)
        #[arg(long)]
        init: bool,
        /// Diretório do projeto (padrão: cwd)
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Registra o servidor MCP browser-use (requer `uvx` instalado)
    InstallBrowserUse {
        /// Harness alvo: claude ou codex
        #[arg(long, value_enum, default_value = "claude")]
        harness: McpHarness,
        /// (Só Claude Code) instalar no settings global em vez do projeto atual
        #[arg(long)]
        global: bool,
    },
}

fn main() -> anyhow::Result<()> {
    // Restore default SIGPIPE so `ng search | head` terminates quietly
    // instead of panicking on broken pipe.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    // Parsing stays on the derive; only the HELP text is localized in runtime.
    // Build the command, swap `about`/arg-help for the active language, then
    // parse. `get_matches` keeps clap's exit codes (--help → 0, error → 2).
    let cmd = localize_help(Cli::command(), Msgs::get());
    let matches = cmd.get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    match cli.command {
        Command::Install { global, harness } => install::install(harness, global),
        Command::Uninstall { global, harness } => uninstall::uninstall(harness, global),
        Command::Completions { shell } => {
            completions(shell, &mut std::io::stdout());
            Ok(())
        }
        Command::Search {
            query,
            here,
            limit,
            semantic,
            json,
        } => search::search(&query.join(" "), here, limit, semantic, json),
        Command::Clear {
            file,
            target_tokens,
            dry_run,
        } => clear::clear(file, target_tokens, dry_run),
        Command::Status { json } => status::status(json),
        Command::Daemon => daemon::daemon(),
        Command::Ui => ui_cmd::ui(),
        Command::Doctor => doctor::run(),
        Command::Mcp {
            action: McpCommand::InstallBrowserUse { harness, global },
        } => mcp::install_browser_use(harness, global),
        Command::Sync {
            personas_dir,
            global,
        } => sync::sync(personas_dir, global),
        Command::Dispatch { prompt, init } => dispatch::dispatch(&prompt.join(" "), init),
        Command::Wisdom {
            here,
            md,
            json,
            rebuild,
        } => wisdom::wisdom(here, md, json, rebuild),
        Command::Memory { action } => memory::memory(action),
        Command::Gain { here, json, since } => gain::gain(here, json, since),
        Command::Saver { action } => saver::saver(action),
        Command::SyncContext { init, dir } => sync_context::sync_context(init, dir),
    }
}

/// Emite o script de completion de `shell` em `out`. Separado do `main`
/// (que só passa stdout) para o teste capturar a saída num buffer.
fn completions(shell: clap_complete::Shell, out: &mut dyn std::io::Write) {
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, "ng", out);
}

/// Aplica os textos de ajuda do idioma `m` sobre o `clap::Command` derivado,
/// sem tocar no parsing. Enumera explicitamente cada subcomando (pelo nome de
/// CLI, kebab-case) e cada argumento (pelo id, = nome do campo Rust) que tem
/// ajuda visível hoje: `mut_subcommand` reescreve o `about` do subcomando e
/// `mut_arg` reescreve o `help` do argumento. Só o texto muda — nomes de
/// flags/subcomandos e exit codes seguem intactos.
///
/// Cuidado deliberado: `mut_arg` faz panic se o id não existir e
/// `mut_subcommand` CRIA um subcomando vazio se o nome não bater — por isso
/// há um teste (`localize_help_is_consistent_and_complete`) que valida o
/// comando resultante e confere o conjunto exato de subcomandos.
fn localize_help(cmd: clap::Command, m: &Msgs) -> clap::Command {
    cmd.about(m.help_about)
        .mut_subcommand("install", |c| {
            c.about(m.help_cmd_install)
                .mut_arg("global", |a| a.help(m.help_arg_install_global))
                .mut_arg("harness", |a| a.help(m.help_arg_install_harness))
        })
        .mut_subcommand("uninstall", |c| {
            c.about(m.help_cmd_uninstall)
                .mut_arg("global", |a| a.help(m.help_arg_uninstall_global))
                .mut_arg("harness", |a| a.help(m.help_arg_uninstall_harness))
        })
        .mut_subcommand("completions", |c| {
            c.about(m.help_cmd_completions)
                .mut_arg("shell", |a| a.help(m.help_arg_completions_shell))
        })
        .mut_subcommand("search", |c| {
            c.about(m.help_cmd_search)
                .mut_arg("query", |a| a.help(m.help_arg_search_query))
                .mut_arg("here", |a| a.help(m.help_arg_search_here))
                .mut_arg("limit", |a| a.help(m.help_arg_search_limit))
                .mut_arg("semantic", |a| a.help(m.help_arg_search_semantic))
                .mut_arg("json", |a| a.help(m.help_arg_search_json))
        })
        .mut_subcommand("clear", |c| {
            c.about(m.help_cmd_clear)
                .mut_arg("file", |a| a.help(m.help_arg_clear_file))
                .mut_arg("target_tokens", |a| a.help(m.help_arg_clear_target_tokens))
                .mut_arg("dry_run", |a| a.help(m.help_arg_clear_dry_run))
        })
        .mut_subcommand("status", |c| {
            c.about(m.help_cmd_status)
                .mut_arg("json", |a| a.help(m.help_arg_status_json))
        })
        .mut_subcommand("daemon", |c| c.about(m.help_cmd_daemon))
        .mut_subcommand("ui", |c| c.about(m.help_cmd_ui))
        .mut_subcommand("doctor", |c| c.about(m.help_cmd_doctor))
        .mut_subcommand("mcp", |c| {
            c.about(m.help_cmd_mcp)
                .mut_subcommand("install-browser-use", |c| {
                    c.about(m.help_cmd_mcp_install_browser_use)
                        .mut_arg("harness", |a| a.help(m.help_arg_mcp_ibu_harness))
                        .mut_arg("global", |a| a.help(m.help_arg_mcp_ibu_global))
                })
        })
        .mut_subcommand("sync", |c| {
            c.about(m.help_cmd_sync)
                .mut_arg("personas_dir", |a| a.help(m.help_arg_sync_personas_dir))
                .mut_arg("global", |a| a.help(m.help_arg_sync_global))
        })
        .mut_subcommand("dispatch", |c| {
            c.about(m.help_cmd_dispatch)
                .mut_arg("prompt", |a| a.help(m.help_arg_dispatch_prompt))
                .mut_arg("init", |a| a.help(m.help_arg_dispatch_init))
        })
        .mut_subcommand("wisdom", |c| {
            c.about(m.help_cmd_wisdom)
                .mut_arg("here", |a| a.help(m.help_arg_wisdom_here))
                .mut_arg("md", |a| a.help(m.help_arg_wisdom_md))
                .mut_arg("json", |a| a.help(m.help_arg_wisdom_json))
                .mut_arg("rebuild", |a| a.help(m.help_arg_wisdom_rebuild))
        })
        .mut_subcommand("memory", |c| {
            c.about(m.help_cmd_memory)
                .mut_subcommand("list", |c| {
                    c.about(m.help_cmd_memory_list)
                        .mut_arg("here", |a| a.help(m.help_arg_memory_list_here))
                        .mut_arg("all", |a| a.help(m.help_arg_memory_list_all))
                        .mut_arg("limit", |a| a.help(m.help_arg_memory_list_limit))
                })
                .mut_subcommand("hide", |c| {
                    c.about(m.help_cmd_memory_hide)
                        .mut_arg("id", |a| a.help(m.help_arg_memory_hide_id))
                })
                .mut_subcommand("unhide", |c| {
                    c.about(m.help_cmd_memory_unhide)
                        .mut_arg("id", |a| a.help(m.help_arg_memory_unhide_id))
                })
                .mut_subcommand("add", |c| {
                    c.about(m.help_cmd_memory_add)
                        .mut_arg("content", |a| a.help(m.help_arg_memory_add_content))
                        .mut_arg("project", |a| a.help(m.help_arg_memory_add_project))
                        .mut_arg("tags", |a| a.help(m.help_arg_memory_add_tags))
                })
        })
        .mut_subcommand("gain", |c| {
            c.about(m.help_cmd_gain)
                .mut_arg("here", |a| a.help(m.help_arg_gain_here))
                .mut_arg("json", |a| a.help(m.help_arg_gain_json))
                .mut_arg("since", |a| a.help(m.help_arg_gain_since))
        })
        .mut_subcommand("saver", |c| {
            c.about(m.help_cmd_saver)
                .mut_subcommand("init", |c| c.about(m.help_cmd_saver_init))
                .mut_subcommand("list", |c| {
                    c.about(m.help_cmd_saver_list)
                        .mut_arg("json", |a| a.help(m.help_arg_saver_list_json))
                })
                .mut_subcommand("bench", |c| {
                    c.about(m.help_cmd_saver_bench)
                        .mut_arg("name", |a| a.help(m.help_arg_saver_bench_name))
                        .mut_arg("sample", |a| a.help(m.help_arg_saver_bench_sample))
                })
        })
        .mut_subcommand("sync-context", |c| {
            c.about(m.help_cmd_sync_context)
                .mut_arg("init", |a| a.help(m.help_arg_sync_context_init))
                .mut_arg("dir", |a| a.help(m.help_arg_sync_context_dir))
        })
}

#[cfg(test)]
mod cli_wiring_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn install_defaults_to_claude_harness_and_no_global() {
        let cli = Cli::try_parse_from(["ng", "install"]).unwrap();
        match cli.command {
            Command::Install { global, harness } => {
                assert!(!global);
                assert!(matches!(harness, Harness::Claude));
            }
            other => panic!("expected Install, got {other:?}"),
        }
    }

    #[test]
    fn install_accepts_harness_and_global_flags() {
        let cli = Cli::try_parse_from(["ng", "install", "--harness", "kimi", "--global"]).unwrap();
        match cli.command {
            Command::Install { global, harness } => {
                assert!(global);
                assert!(matches!(harness, Harness::Kimi));
            }
            other => panic!("expected Install, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["ng", "install", "--harness", "gemini"]).unwrap();
        match cli.command {
            Command::Install { harness, .. } => assert!(matches!(harness, Harness::Gemini)),
            other => panic!("expected Install, got {other:?}"),
        }
    }

    #[test]
    fn install_rejects_unknown_harness() {
        assert!(Cli::try_parse_from(["ng", "install", "--harness", "grok"]).is_err());
    }

    #[test]
    fn uninstall_defaults_to_claude_harness_and_no_global() {
        let cli = Cli::try_parse_from(["ng", "uninstall"]).unwrap();
        match cli.command {
            Command::Uninstall { global, harness } => {
                assert!(!global);
                assert!(matches!(harness, Harness::Claude));
            }
            other => panic!("expected Uninstall, got {other:?}"),
        }
    }

    #[test]
    fn uninstall_accepts_harness_and_global_flags() {
        let cli =
            Cli::try_parse_from(["ng", "uninstall", "--harness", "gemini", "--global"]).unwrap();
        match cli.command {
            Command::Uninstall { global, harness } => {
                assert!(global);
                assert!(matches!(harness, Harness::Gemini));
            }
            other => panic!("expected Uninstall, got {other:?}"),
        }
    }

    #[test]
    fn completions_requires_a_shell_and_rejects_unknown_ones() {
        assert!(Cli::try_parse_from(["ng", "completions"]).is_err());
        assert!(Cli::try_parse_from(["ng", "completions", "tcsh"]).is_err());
        let cli = Cli::try_parse_from(["ng", "completions", "zsh"]).unwrap();
        match cli.command {
            Command::Completions { shell } => assert_eq!(shell, clap_complete::Shell::Zsh),
            other => panic!("expected Completions, got {other:?}"),
        }
    }

    #[test]
    fn completions_for_bash_emit_a_script_naming_the_binary() {
        let mut buf = Vec::new();
        completions(clap_complete::Shell::Bash, &mut buf);
        let script = String::from_utf8(buf).unwrap();
        assert!(!script.is_empty());
        assert!(script.contains("ng"), "script must reference the binary");
        assert!(
            script.contains("uninstall"),
            "script must know the subcommands"
        );
    }

    #[test]
    fn mcp_install_browser_use_defaults_to_claude() {
        let cli = Cli::try_parse_from(["ng", "mcp", "install-browser-use"]).unwrap();
        match cli.command {
            Command::Mcp {
                action: McpCommand::InstallBrowserUse { harness, global },
            } => {
                assert!(matches!(harness, McpHarness::Claude));
                assert!(!global);
            }
            other => panic!("expected Mcp/InstallBrowserUse, got {other:?}"),
        }
    }

    #[test]
    fn mcp_install_browser_use_accepts_codex_harness() {
        let cli = Cli::try_parse_from(["ng", "mcp", "install-browser-use", "--harness", "codex"])
            .unwrap();
        match cli.command {
            Command::Mcp {
                action: McpCommand::InstallBrowserUse { harness, .. },
            } => {
                assert!(matches!(harness, McpHarness::Codex));
            }
            other => panic!("expected Mcp/InstallBrowserUse, got {other:?}"),
        }
    }

    #[test]
    fn sync_parses_personas_dir_and_global() {
        let cli =
            Cli::try_parse_from(["ng", "sync", "--personas-dir", "/tmp/p", "--global"]).unwrap();
        match cli.command {
            Command::Sync {
                personas_dir,
                global,
            } => {
                assert_eq!(personas_dir, Some(PathBuf::from("/tmp/p")));
                assert!(global);
            }
            other => panic!("expected Sync, got {other:?}"),
        }
    }

    #[test]
    fn sync_without_flags_has_no_personas_dir_override() {
        let cli = Cli::try_parse_from(["ng", "sync"]).unwrap();
        match cli.command {
            Command::Sync {
                personas_dir,
                global,
            } => {
                assert!(personas_dir.is_none());
                assert!(!global);
            }
            other => panic!("expected Sync, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_collects_prompt_words() {
        let cli = Cli::try_parse_from(["ng", "dispatch", "fix", "the", "bug"]).unwrap();
        match cli.command {
            Command::Dispatch { prompt, init } => {
                assert_eq!(
                    prompt,
                    vec!["fix".to_string(), "the".to_string(), "bug".to_string()]
                );
                assert!(!init);
            }
            other => panic!("expected Dispatch, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_init_flag_parses_without_prompt() {
        let cli = Cli::try_parse_from(["ng", "dispatch", "--init"]).unwrap();
        match cli.command {
            Command::Dispatch { prompt, init } => {
                assert!(prompt.is_empty());
                assert!(init);
            }
            other => panic!("expected Dispatch, got {other:?}"),
        }
    }

    #[test]
    fn clear_defaults_to_no_file_and_standard_budget() {
        let cli = Cli::try_parse_from(["ng", "clear"]).unwrap();
        match cli.command {
            Command::Clear {
                file,
                target_tokens,
                dry_run,
            } => {
                assert!(file.is_none());
                assert_eq!(target_tokens, 4000);
                assert!(!dry_run);
            }
            other => panic!("expected Clear, got {other:?}"),
        }
    }

    #[test]
    fn clear_parses_file_target_and_dry_run() {
        let cli = Cli::try_parse_from([
            "ng",
            "clear",
            "--file",
            "/tmp/s.jsonl",
            "--target-tokens",
            "8000",
            "--dry-run",
        ])
        .unwrap();
        match cli.command {
            Command::Clear {
                file,
                target_tokens,
                dry_run,
            } => {
                assert_eq!(file, Some(PathBuf::from("/tmp/s.jsonl")));
                assert_eq!(target_tokens, 8000);
                assert!(dry_run);
            }
            other => panic!("expected Clear, got {other:?}"),
        }
    }

    #[test]
    fn wisdom_parses_here_and_md_flags() {
        let cli = Cli::try_parse_from(["ng", "wisdom", "--here", "--md"]).unwrap();
        match cli.command {
            Command::Wisdom { here, md, .. } => {
                assert!(here);
                assert!(md);
            }
            other => panic!("expected Wisdom, got {other:?}"),
        }
    }

    #[test]
    fn wisdom_without_flags_defaults_false() {
        let cli = Cli::try_parse_from(["ng", "wisdom"]).unwrap();
        match cli.command {
            Command::Wisdom { here, md, .. } => {
                assert!(!here);
                assert!(!md);
            }
            other => panic!("expected Wisdom, got {other:?}"),
        }
    }

    #[test]
    fn search_parses_json_flag() {
        let cli = Cli::try_parse_from(["ng", "search", "algo", "--json"]).unwrap();
        match cli.command {
            Command::Search { json, .. } => assert!(json),
            other => panic!("expected Search, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["ng", "search", "algo"]).unwrap();
        match cli.command {
            Command::Search { json, .. } => assert!(!json),
            other => panic!("expected Search, got {other:?}"),
        }
    }

    #[test]
    fn status_parses_json_flag() {
        let cli = Cli::try_parse_from(["ng", "status", "--json"]).unwrap();
        match cli.command {
            Command::Status { json } => assert!(json),
            other => panic!("expected Status, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["ng", "status"]).unwrap();
        match cli.command {
            Command::Status { json } => assert!(!json),
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn wisdom_parses_json_flag() {
        let cli = Cli::try_parse_from(["ng", "wisdom", "--here", "--json"]).unwrap();
        match cli.command {
            Command::Wisdom { here, md, json, .. } => {
                assert!(here);
                assert!(!md);
                assert!(json);
            }
            other => panic!("expected Wisdom, got {other:?}"),
        }
    }

    #[test]
    fn wisdom_json_conflicts_with_md() {
        assert!(Cli::try_parse_from(["ng", "wisdom", "--json", "--md"]).is_err());
    }

    #[test]
    fn wisdom_parses_rebuild_flag() {
        let cli = Cli::try_parse_from(["ng", "wisdom", "--rebuild"]).unwrap();
        match cli.command {
            Command::Wisdom { rebuild, .. } => assert!(rebuild),
            other => panic!("expected Wisdom, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["ng", "wisdom"]).unwrap();
        match cli.command {
            Command::Wisdom { rebuild, .. } => assert!(!rebuild),
            other => panic!("expected Wisdom, got {other:?}"),
        }
    }

    #[test]
    fn gain_parses_here_json_and_since() {
        let cli = Cli::try_parse_from(["ng", "gain", "--here", "--json", "--since", "2026-01-01"])
            .unwrap();
        match cli.command {
            Command::Gain { here, json, since } => {
                assert!(here);
                assert!(json);
                assert_eq!(since.as_deref(), Some("2026-01-01"));
            }
            other => panic!("expected Gain, got {other:?}"),
        }
    }

    #[test]
    fn gain_without_flags_defaults_to_global_text_all_time() {
        let cli = Cli::try_parse_from(["ng", "gain"]).unwrap();
        match cli.command {
            Command::Gain { here, json, since } => {
                assert!(!here);
                assert!(!json);
                assert!(since.is_none());
            }
            other => panic!("expected Gain, got {other:?}"),
        }
    }

    #[test]
    fn saver_list_parses_with_and_without_json() {
        let cli = Cli::try_parse_from(["ng", "saver", "list", "--json"]).unwrap();
        match cli.command {
            Command::Saver {
                action: saver::SaverCommand::List { json },
            } => assert!(json),
            other => panic!("expected Saver/List, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["ng", "saver", "list"]).unwrap();
        match cli.command {
            Command::Saver {
                action: saver::SaverCommand::List { json },
            } => assert!(!json),
            other => panic!("expected Saver/List, got {other:?}"),
        }
    }

    #[test]
    fn saver_bench_requires_a_name_and_defaults_sample() {
        assert!(Cli::try_parse_from(["ng", "saver", "bench"]).is_err());
        let cli = Cli::try_parse_from(["ng", "saver", "bench", "headroom"]).unwrap();
        match cli.command {
            Command::Saver {
                action: saver::SaverCommand::Bench { name, sample },
            } => {
                assert_eq!(name, "headroom");
                assert_eq!(sample, 50);
            }
            other => panic!("expected Saver/Bench, got {other:?}"),
        }
    }

    #[test]
    fn saver_init_parses() {
        let cli = Cli::try_parse_from(["ng", "saver", "init"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Saver {
                action: saver::SaverCommand::Init
            }
        ));
    }

    #[test]
    fn sync_context_parses_init_and_dir() {
        let cli = Cli::try_parse_from(["ng", "sync-context", "--init", "--dir", "/tmp/x"]).unwrap();
        match cli.command {
            Command::SyncContext { init, dir } => {
                assert!(init);
                assert_eq!(dir, Some(PathBuf::from("/tmp/x")));
            }
            other => panic!("expected SyncContext, got {other:?}"),
        }
    }

    #[test]
    fn sync_context_without_flags_defaults_to_cwd_no_init() {
        let cli = Cli::try_parse_from(["ng", "sync-context"]).unwrap();
        match cli.command {
            Command::SyncContext { init, dir } => {
                assert!(!init);
                assert!(dir.is_none());
            }
            other => panic!("expected SyncContext, got {other:?}"),
        }
    }

    // `localize_help` reescreve `about`/`help` por nome. `mut_arg` faz panic
    // se o id não existir e `mut_subcommand` cria um subcomando espúrio se o
    // nome não bater — este teste é o gate desses dois erros: constrói o
    // comando localizado nos dois idiomas (qualquer id de arg errado já faz
    // panic aqui), valida a consistência interna via `debug_assert`, e confere
    // que o conjunto de subcomandos de topo é exatamente o esperado (nenhum
    // criado por engano a partir de um nome de `mut_subcommand` com typo).
    #[test]
    fn localize_help_is_consistent_and_complete() {
        for lang in [i18n::Lang::En, i18n::Lang::Pt] {
            let cmd = localize_help(Cli::command(), Msgs::for_lang(lang));

            // `help` embutido do clap não aparece aqui: ele só é injetado no
            // `build()` interno (no parse), não no `Command` de `command()`.
            let mut names: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
            names.sort_unstable();
            let mut expected = vec![
                "install",
                "uninstall",
                "completions",
                "search",
                "clear",
                "status",
                "daemon",
                "ui",
                "doctor",
                "mcp",
                "sync",
                "dispatch",
                "wisdom",
                "memory",
                "gain",
                "saver",
                "sync-context",
            ];
            expected.sort_unstable();
            assert_eq!(names, expected, "unexpected subcommand set for {lang:?}");

            // Panica se qualquer arg/subcomando ficou inconsistente.
            cmd.debug_assert();
        }
    }
}
