<p align="center">
  <img src="./assets/ui/mascot.png" width="256" alt="not-goldfish: o mascote peixinho neobrutalista que não esquece">
</p>

<p align="center">
  <img src="./assets/readme/hero.svg" width="100%" alt="not-goldfish — memória universal, cache e higiene de contexto para agentes de IA. Um terminal mostra memórias de sessões anteriores com proveniência sendo injetadas no prompt, com 95% menos tokens que reler o histórico">
</p>

<p align="center">
  <a href="https://github.com/vmelooo/not-goldfish/actions/workflows/ci.yml"><img src="https://github.com/vmelooo/not-goldfish/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/licença-MIT-f0b429?style=for-the-badge" alt="Licença MIT"></a>
  <img src="https://img.shields.io/badge/local--first-100%25_offline-2dd4bf?style=for-the-badge" alt="local-first, 100% offline">
</p>

<p align="center"><strong>Português</strong> · <a href="./README.en.md">English</a></p>

Um peixinho dourado esquece tudo a cada volta no aquário. Seu agente de código faz igual a cada sessão: esquece o que decidiu, queima tokens refazendo trabalho já feito e alucina onde deveria só lembrar. **not-goldfish** é a camada de memória que falta: captura tudo via hooks do harness, guarda num SQLite local, injeta memórias relevantes de volta no prompt — com proveniência — e emagrece o transcript sem perder nada.

## Por quê

- **Chega de reexplicar o projeto** a cada sessão nova — decisões, bugs corrigidos e convenções de sessões anteriores voltam sozinhos ao contexto.
- **Tokens não são de graça** — reempilhar o histórico inteiro para reencontrar um fato custa ~20x mais que injetar os 3 snippets certos.
- **Sem memória, o agente inventa** — cada memória injetada carrega id, harness e data citáveis; abaixo do limiar de relevância, silêncio.
- **Sessões longas incham** — a higiene lossless troca tool output frio por stubs recuperáveis antes do compact, sem apagar nada.

## Instalação

```bash
curl -fsSL https://raw.githubusercontent.com/vmelooo/not-goldfish/main/install.sh | bash
```

Sem pré-requisitos manuais: o instalador provisiona sozinho o que faltar — toolchain C (`cc`/`make`, pro SQLite compilado junto via `bundled`), `git`, `curl` e o [Rust via rustup](https://rustup.rs/) (apt, dnf, yum, pacman, zypper, apk ou brew; no macOS, Xcode Command Line Tools). Prefere gerenciar à mão? `--skip-deps`. Linux e macOS (no Windows, veja a seção abaixo). Primeira compilação: ~3–5 min; depois, incremental.

O script faz cinco coisas:

1. Instala as dependências que faltarem (toolchain C, git, curl, Rust).
2. Clona o repo em `~/.not-goldfish/src` (ou usa o checkout local).
3. Compila em release.
4. Coloca `ng`, `ngd` e `ng-hook` em `~/.cargo/bin` (o rustup já o mantém no PATH).
5. Registra os hooks em todos os projetos do Claude Code — com backup do `settings.json` (`settings.json.ng-backup`) antes de tocar nele.

Para conferir que deu certo:

```bash
ng doctor   # diagnóstico: binários, daemon, banco, hooks, UI
ng ui       # UI web local em http://127.0.0.1:4949
```

A partir da próxima sessão do harness, tudo é capturado e as memórias de sessões anteriores começam a voltar sozinhas ao prompt.

### Windows (via WSL2)

Windows nativo **não é suportado**: o daemon `ngd` fala por socket Unix. O caminho fácil e honesto é WSL2 — ~10 min incluindo o download.

1. Abra o PowerShell **como administrador** e rode `wsl --install` (reinicie se pedir).
2. Abra o **Ubuntu** pelo menu iniciar.
3. Rode o one-liner de instalação: `curl -fsSL https://raw.githubusercontent.com/vmelooo/not-goldfish/main/install.sh | bash` — ele instala o toolchain C e o Rust sozinho.
4. Pronto — os harnesses rodando **dentro do WSL** enxergam o `ng` normalmente.

**Variações** — outro harness, escopo por projeto, desinstalar:

```bash
curl -fsSL https://raw.githubusercontent.com/vmelooo/not-goldfish/main/install.sh | bash -s -- --harness kimi   # Kimi Code (também: gemini)
ng install      # hooks só no projeto atual (.claude/settings.json), em vez de global
ng uninstall    # inverso exato do install (--global p/ o escopo global); banco e memórias ficam intactos
```

**Atualizar** — rode o mesmo one-liner de novo: o checkout faz `git pull`, o build é incremental e `ng install` é idempotente, não duplica hooks.

<details>
<summary><b>Prefere não rodar <code>curl | bash</code>? Instalação manual</b></summary>

O mesmo fluxo a partir de um clone seu — o script detecta o checkout local e o usa, sem clonar nada:

```bash
git clone https://github.com/vmelooo/not-goldfish
cd not-goldfish
./install.sh
```

Ou 100% à mão, sem script nenhum:

```bash
cargo build --release
install -m 755 target/release/ng target/release/ngd target/release/ng-hook ~/.cargo/bin/
ng install --global
```

Os três binários gostam de morar lado a lado: `ng` localiza `ngd` e `ng-hook` primeiro no próprio diretório, com fallback pelo PATH.

</details>

Detalhes, embedder semântico opcional (`model2vec`) e troubleshooting: [`docs/SETUP.md`](docs/SETUP.md).

## Benchmarks

Metodologia estilo LoCoMo/mem0: um fato é plantado numa sessão e cobrado depois. Medido num **Claude Code puro** (sem outras ferramentas) vs. **Claude Code puro + not-goldfish** — `ng-bench` é auto-contido e offline. Detalhes e método: [`docs/benchmarks/with-vs-without.md`](docs/benchmarks/with-vs-without.md) · painel visual: [`docs/benchmarks/charts.html`](docs/benchmarks/charts.html).

![Acurácia por cenário — sem memória 0%, replay 100%, FTS 75%, híbrido 75%, model2vec+ANN 81%](docs/benchmarks/bench-accuracy.svg)

| Cenário | Acurácia | MRR | Grounding | Tokens injetados | Economia vs replay |
|---|---|---|---|---|---|
| Claude Code puro — sem memória | 0% | — | 0% | 0 | não sabe o fato |
| Claude Code puro — replay completo | 100% | 1.00 | 100% | ~1774 | 0% (baseline) |
| + not-goldfish — FTS | 75% | 0.69 | 75% | ~95 | **94,7%** |
| + not-goldfish — híbrido (hash) | 75% | 0.69 | 75% | ~89 | **95,0%** |
| + not-goldfish — model2vec + recall ANN | **81%** | **0.72** | **81%** | ~96 | **94,6%** |

**~95% menos tokens que reempilhar o histórico completo, com paridade ou ganho de recall.** O embedder real (model2vec, opt-in) + recall ANN entrega a melhor acurácia e grounding (81%).

![Economia de tokens — replay ~1774 vs. injeção ~95](docs/benchmarks/bench-tokens.svg)

Honesto sobre onde é mais difícil — o **gap semântico** (query sem nenhum termo lexical em comum com o fato). O recall ANN recuperou docs que o FTS nunca casa, subindo o acerto de 33% → 50%; os misses restantes agora são de ranking, não de recuperação:

![Gap semântico — FTS e hash 33%, model2vec + recall ANN 50%](docs/benchmarks/bench-semantic-gap.svg)

No *seu* ambiente, `ng gain` mostra o benefício acumulado desde a adoção:

```bash
ng gain                    # capturas, injeções e tokens líquidos economizados
ng gain --here --json      # só o projeto atual, saída estável p/ scripts
```

## Recursos

**Memória**

- **Captura resiliente** — cada `UserPromptSubmit`/`PostToolUse`/`SessionStart`/`Stop` vira um evento com proveniência (sessão, projeto, harness, timestamp); o hook roda em <5ms e nunca quebra a sessão.
- **Injeção proativa com proveniência** — a cada prompt, memórias relevantes de sessões *anteriores* entram sozinhas no contexto, com id/harness/data citáveis, limiar de relevância e orçamento rígido de tokens.
- **Busca híbrida** — FTS5 (bm25, poda por IDF) com rerank por similaridade semântica; embedder padrão zero-dependência, plugável por um real (`model2vec`) via feature flag.
- **Memória própria editável** — `ng memory list/add/hide/unhide` e a UI; ocultar é reversível, nada some.
- **`.ng/` commitável** — `ng sync-context` projeta a memória do projeto em `context.md`/`decisions.md` regeneráveis; o banco continua a fonte de verdade.

**Contexto**

- **Higiene lossless** — `ng clear` (manual) e o hook `PreCompact` (automático, opt-in) trocam itens frios do transcript por stubs recuperáveis, com backup byte-a-byte antes de qualquer rewrite. Nada é apagado do banco.
- **`ng gain`** — o benefício acumulado no *seu* ambiente: capturas, injeções e tokens líquidos economizados pela higiene, com contabilidade honesta (injeção é reportada como custo, não como economia).
- **Plugins de economia (savers)** — compressores de token externos plugam por contrato CLI; tudo OFF por default e só promovido depois de medido no seu workload (`ng saver bench`).
- **UI web local** — timeline do transcript com barra proporcional a tokens, busca ao lado, remover/substituir por stub com backup. Só em `127.0.0.1`, nunca `0.0.0.0`.

**Ecossistema**

- **Grafo de sabedoria** — `ng wisdom` mostra entidades/decisões extraídas das sessões; `--here --md` exporta Markdown para colar em `CLAUDE.md`/`AGENTS.md`.
- **Multi-harness** — instaladores de hooks (claude/kimi/gemini), registro de MCP (claude/codex), sync de personas (claude/opencode) e dispatch por keywords.

## Como funciona

<p align="center">
  <img src="./assets/readme/architecture.svg" width="100%" alt="Diagrama desenhado à mão: o harness dispara hooks para o ng-hook (<5ms), que fala com o daemon ngd e o SQLite; memórias com proveniência voltam ao prompt como additionalContext">
</p>

Em todo `UserPromptSubmit`, `ng-hook` busca memórias de sessões *anteriores* relevantes ao prompt (nunca a sessão corrente — ela já está no contexto) e as injeta como `additionalContext`:

```
<not-goldfish-memory>
Memórias de sessões anteriores relevantes ao pedido (proveniência entre colchetes; use `ng search <termos>` para recuperar mais):
- [#4821 · claude-code · 2026-07-14] corrigido bug de auth: token expirava antes do refresh disparar >>
- [#4790 · claude-code · 2026-07-12] decisão: usar WAL mode no SQLite p/ escrita concorrente
</not-goldfish-memory>
```

A higiene (manual via `ng clear` ou automática via `PreCompact` com `NG_AUTO_HYGIENE=1`) troca os itens mais frios do transcript por stubs curtos e recuperáveis — os últimos 20 itens e todo prompt real do usuário são intocáveis, um backup byte-a-byte (`*.ng-bak`) precede qualquer rewrite, e a troca de arquivo é sempre `rename` atômico. O modo automático é desligado por padrão: reescrever o transcript de uma sessão ao vivo é território sensível, e qualquer falha em qualquer etapa deixa a sessão intocada.

## Comandos

| Comando | O que faz |
|---|---|
| `ng install [--global] [--harness claude\|kimi\|gemini]` | Registra os hooks no harness (backup + idempotente). |
| `ng uninstall` | Remove só os hooks do not-goldfish; banco e memórias ficam intactos. |
| `ng search <termos> [--here] [--semantic] [--json]` | Busca na memória persistente (FTS5; `--semantic` liga o rerank híbrido). |
| `ng status [--json]` | Estado do banco e do daemon. |
| `ng daemon` | Inicia o `ngd` em foreground (use um service manager p/ background). |
| `ng ui` | Abre a UI web local (sobe o daemon se preciso). |
| `ng doctor` | Diagnóstico com correção de uma linha embutida em cada aviso/falha. |
| `ng clear [--dry-run] [--target-tokens N]` | Higiene lossless da sessão ativa: itens frios viram stubs recuperáveis. |
| `ng memory list\|add\|hide\|unhide` | Inspeciona/edita a memória própria (ocultar é reversível). |
| `ng gain [--here] [--since YYYY-MM-DD] [--json]` | Benefício acumulado: capturas, injeções, higiene. |
| `ng sync-context [--init] [--dir <path>]` | (Re)gera `.ng/` — projeção commitável da memória do projeto. |
| `ng wisdom [--here] [--md] [--json]` | Grafo de sabedoria (entidades/decisões das sessões). |
| `ng saver init\|list\|bench` | Savers externos: gate de medição, tudo OFF por default. |
| `ng sync [--global] [--personas-dir <path>]` | Sincroniza personas universais p/ o formato de cada harness. |
| `ng dispatch <prompt>\|--init` | Sugere categoria + modelo p/ um prompt (keywords pt/en). |
| `ng mcp install-browser-use [--harness claude\|codex]` | Registra o servidor MCP browser-use (requer `uvx`). |
| `ng completions <shell>` | Script de autocompletar (bash/zsh/fish/elvish/powershell). |

### Arquitetura

```
crates/
├── ng-core     — store SQLite (WAL, FTS5), tags léxicas, embeddings
│                 (HashEmbedder + busca híbrida), grafo de sabedoria
├── ng-hook     — binário fino chamado pelos hooks (<5ms), injeção proativa,
│                 gate de higiene PreCompact; fallback síncrono sem daemon
├── ngd         — daemon: writer único via socket Unix, worker de embedding/
│                 grafo em background, UI web (axum, thread própria)
├── ng-sessions — parsers tolerantes a versão dos transcripts de cada harness
│                 + rewrite seguro (backup + rename atômico)
├── ng-adapters — integração multi-harness pura/testável: hooks, MCP,
│                 personas, dispatch, watcher de transcripts
└── ng-cli      — o CLI `ng` (tabela acima)
```

Dados em `~/.not-goldfish/` (`NG_DATA_DIR` para customizar). Eventos são o log de fonte de verdade; busca semântica, eviction e UI são derivações desse log, nunca donas dele.

### Variáveis de ambiente

| Variável | Padrão | Efeito |
|---|---|---|
| `NG_DATA_DIR` | `~/.not-goldfish` | Raiz de dados: banco (`ng.db`), socket (`ngd.sock`), pid, lock. |
| `NG_INJECT` | ligado | `0`/`false`/`off` desliga a injeção proativa. |
| `NG_INJECT_LIMIT` | `3` | Máximo de memórias injetadas por prompt. |
| `NG_INJECT_MAX_RANK` | `-1.0` | Corte de relevância (bm25, menor = melhor). |
| `NG_INJECT_BUDGET` | `600` | Orçamento de tokens do bloco `<not-goldfish-memory>`. |
| `NG_AUTO_HYGIENE` | desligado | `1` habilita a eviction lossless automática no `PreCompact`. |
| `NG_HYGIENE_TARGET_TOKENS` | `20000` | Tokens que a passagem de higiene tenta liberar por `PreCompact`. |
| `NG_UI_PORT` | `4949` | Porta da UI web (sempre em `127.0.0.1`). |
| `NG_AUTOSTART` | ligado | `0` desativa o autostart do `ngd` pelo `ng-hook`. |
| `NG_HARNESS` | `claude-code` | Rótulo do harness gravado em cada evento capturado. |
| `NG_DEBUG_TIMING` | desligado | Imprime em stderr o tempo de cada etapa do hot path do hook. |

## Invariantes

- **Captura nunca quebra a sessão do harness** — `ng-hook` sempre sai com código 0, mesmo em falha; perder uma memória é aceitável, quebrar o fluxo do usuário não é.
- **Nada capturado é deletado** — higiene/eviction só substitui por stub recuperável ou remove do transcript *ativo*, nunca do banco nem do backup.
- **Hot path do hook tem orçamento de <5ms** — todo trabalho pesado (embeddings, grafo) roda em background no `ngd`.
- **A UI web liga só em `127.0.0.1`, nunca `0.0.0.0`** — transcripts contêm prompts, saídas de ferramentas e às vezes segredos.

## Documentação

| Doc | O que tem |
|---|---|
| [`docs/SETUP.md`](docs/SETUP.md) | Instalação detalhada, embedder semântico (`model2vec`), troubleshooting. |
| [`docs/benchmarks/with-vs-without.md`](docs/benchmarks/with-vs-without.md) | Metodologia LoCoMo-style, auditoria de viés, resultados por classe. |
| [`docs/benchmarks/charts.html`](docs/benchmarks/charts.html) | Painel visual do benchmark (abrir no navegador). |
| [`docs/research/tooling-gains.md`](docs/research/tooling-gains.md) | Pesquisa: ganhos de ferramentas de contexto. |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | Build, testes, lints e as invariantes que um PR não pode quebrar. |

## Contribuindo

Build, testes, lints e as invariantes que um PR não pode quebrar: [`CONTRIBUTING.md`](CONTRIBUTING.md).

## Licença

[MIT](LICENSE) © 2026 Vitor Mello.
