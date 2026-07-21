# Contributing

## Build & test

```bash
cargo build --release                                   # todo o workspace
cargo test --workspace --release                         # todos os testes (-p <crate> para um só)
cargo fmt --all --check                                  # formatação — CI falha se não passar
cargo clippy --workspace --all-targets -- -D warnings     # lints — CI trata warning como erro
scripts/e2e.sh                                            # e2e do sistema real (ng, ngd, ng-hook)
```

`scripts/e2e.sh` exercita os binários de release de verdade contra um `NG_DATA_DIR`/`$HOME`
descartável — não é substituto dos testes unitários de cada crate, é o que pega "as peças não
se encaixam". Rode-o antes de qualquer mudança que toque a fronteira entre crates (hook ↔
daemon ↔ store).

## Invariantes que um PR não pode quebrar

Ver `CLAUDE.md` para o detalhamento; resumo:

- **Captura nunca quebra a sessão do harness.** `ng-hook` sempre sai com código 0, mesmo em
  falha de captura.
- **Nada capturado é deletado.** Eviction/higiene só substitui conteúdo por um stub
  recuperável ou remove do transcript *ativo* do harness — nunca do banco not-goldfish nem do
  backup em disco.
- **Hot path do hook tem orçamento de <5ms.** Trabalho pesado (embeddings, grafo) roda em
  background em `ngd`, nunca em `ng-hook`.
- **A UI web liga só em `127.0.0.1`.** Nunca `0.0.0.0` — transcripts contêm prompts, saídas de
  ferramentas e às vezes segredos.

## Crates

| Crate | O que é |
|---|---|
| `ng-core` | Store SQLite (WAL, FTS5), tags léxicas, embeddings (`HashEmbedder` + busca híbrida), grafo de sabedoria, `paths`, `timeutil`. |
| `ng-hook` | Binário fino chamado pelos hooks do harness (<5ms), injeção proativa de memória, gate de higiene `PreCompact`. |
| `ngd` | Daemon: writer único via socket Unix, worker de embedding/grafo em background, UI web (axum, thread própria). |
| `ng-sessions` | Parsers tolerantes a versão dos transcripts de cada harness + rewrite seguro (backup + rename atômico). |
| `ng-adapters` | Integração multi-harness pura/testável: instaladores de hooks, registro de MCP, sync de personas, dispatch, watcher de transcripts (Codex). |
| `ng-cli` | CLI `ng` (install, search, status, daemon, ui, doctor, mcp, sync, dispatch, wisdom, clear). |
| `ng-bench` | Estudo reprodutível "com vs. sem a ferramenta" (estilo LoCoMo) — ver `docs/benchmarks/with-vs-without.md`. |

Muitos arquivos pequenos, alta coesão, baixo acoplamento — extraia utilitários em vez de
inflar um módulo existente. Adicione testes junto da mudança, não depois; `cargo test
--workspace --release` e os pisos de qualidade em `crates/ng-bench/tests/quality_floors.rs`
são o gate de regressão real.

Antes de abrir um PR: `cargo fmt`, `cargo clippy -D warnings` e `cargo test --workspace`
passando localmente evitam voltas no CI.
