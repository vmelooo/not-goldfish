# not-goldfish

Memória universal, cache e higiene de contexto para harnesses de IA (Claude Code, Kimi Code,
Gemini CLI, Codex). SQLite (WAL + FTS5) é a fonte de verdade; tudo mais (busca híbrida, grafo
de sabedoria, UI web) é derivado dela.

## Crates

| Crate | O que é |
|---|---|
| `ng-core` | Store SQLite (WAL, FTS5), tags léxicas, embeddings (`HashEmbedder` + busca híbrida), grafo de sabedoria, `paths`, `timeutil`. |
| `ng-hook` | Binário fino chamado pelos hooks do harness (<5ms), injeção proativa de memória, gate de higiene `PreCompact`. |
| `ngd` | Daemon: writer único via socket Unix, worker de embedding/grafo em background, UI web (axum, thread própria). |
| `ng-sessions` | Parsers tolerantes a versão dos transcripts de cada harness + rewrite seguro (backup + rename atômico). |
| `ng-adapters` | Integração multi-harness pura/testável: instaladores de hooks, registro de MCP, sync de personas, dispatch, watcher de transcripts (Codex). |
| `ng-cli` | CLI `ng` (install, search, status, daemon, ui, doctor, mcp, sync, dispatch, wisdom, memory, gain, sync-context). |

Detalhes de uso, variáveis de ambiente e arquitetura completa: `README.md`.

## Comandos

```bash
cargo build --release              # build de todo o workspace
cargo test --workspace --release   # todos os testes (rode -p <crate> para um só)
cargo fmt --all --check            # formatação (CI falha se não passar)
cargo clippy --workspace --all-targets -- -D warnings   # lints (CI trata warning como erro)
./target/release/ng doctor         # diagnóstico de ambiente instalado
```

SQLite é `bundled` (feature do `rusqlite` no `Cargo.toml` raiz) — compilado a partir de fontes
vendored, sem `apt install libsqlite3-dev` nem nenhuma dependência de sistema.

## Invariantes do projeto

- **Captura nunca quebra a sessão do harness.** `ng-hook` sempre sai com código 0, mesmo em
  falha de captura — perder uma memória é aceitável, quebrar o fluxo do usuário não é.
- **Nada capturado é deletado.** Eventos são o log de fonte de verdade; higiene/eviction só
  substitui conteúdo por um stub recuperável (`[ng-evicted: ...]`) ou remove do transcript
  *ativo* do harness — nunca do banco not-goldfish nem do backup em disco.
- **Hot path do hook tem orçamento de <5ms.** `ng-hook` é o binário mais fino possível: parse
  do payload, tags léxicas, entrega ao daemon (ou fallback síncrono se o daemon estiver fora).
  Qualquer trabalho pesado (embeddings, grafo) roda em background em `ngd`, nunca no hook.
- **A UI web liga só em `127.0.0.1`, nunca `0.0.0.0`.** Transcripts contêm prompts, saídas de
  ferramentas e às vezes segredos — não é uma superfície para expor na rede.

## Ironia registrada

O próprio not-goldfish gera Markdown de contexto durável via `ng wisdom --here --md`, pensado
para colar em `CLAUDE.md`/`AGENTS.md` de outros projetos. Isto é, o produto que este arquivo
descreve pode gerar arquivos como este próprio arquivo — dogfooding não intencional, mas real.
