# Setup

Do zero ao `ng` funcionando: ~10 min na primeira vez (a maior parte é o build release).
Ao final, a primeira sessão do harness já captura e injeta memória.

## O que você ganha

- Captura de tudo que acontece nas sessões do harness num SQLite local.
- Injeção proativa de memórias relevantes de sessões anteriores a cada prompt.
- Busca manual de qualquer coisa: `ng search`.
- UI web local (`ng ui`) para inspecionar e editar o transcript vivo.

## Pré-requisitos

- **Nenhum pré-requisito manual** com o `install.sh`: ele provisiona sozinho o que faltar —
  toolchain C (`cc`/`gcc` + `make`), `git`, `curl` e o **Rust** via [rustup](https://rustup.rs/)
  (apt, dnf, yum, pacman, zypper, apk ou brew; no macOS, Xcode Command Line Tools).
  `--skip-deps` (ou `NG_SKIP_DEPS=1`) desliga esse passo. O toolchain C é necessário porque a
  feature `bundled` do `rusqlite` compila o SQLite a partir de fontes vendored no próprio build —
  então não precisa de `apt install libsqlite3-dev` nem equivalente.
- Um harness suportado instalado: Claude Code, Kimi Code ou Gemini CLI (o suporte a Codex e
  opencode existe na captura/parsing, mas a instalação automática de hooks hoje cobre os três
  primeiros — veja `ng install --harness` abaixo).

## 1. Build

```bash
git clone <url-do-seu-fork-ou-repo>
cd not-goldfish
cargo build --release
```

Os binários ficam em `target/release/`: `ng` (CLI), `ngd` (daemon) e `ng-hook` (o binário fino
que os hooks do harness chamam). `ng` localiza `ngd` e `ng-hook` automaticamente ao lado de si
mesmo — não precisa colocar nada no `PATH` manualmente, mas pode se preferir.

## 2. Instalar os hooks no harness

```bash
./target/release/ng install                    # Claude Code, projeto atual (.claude/settings.json)
./target/release/ng install --global            # Claude Code, todos os projetos (~/.claude/settings.json)
./target/release/ng install --harness kimi      # Kimi Code
./target/release/ng install --harness gemini    # Gemini CLI
```

`ng install` faz backup do `settings.json` existente (`settings.json.ng-backup`) antes de
tocar nele, e é idempotente — rodar de novo não duplica hooks já registrados.

## 3. Daemon e UI

O daemon (`ngd`) é opcional no dia a dia: sem ele, `ng-hook` grava direto no SQLite (fallback
síncrono — WAL + `busy_timeout` tornam isso seguro mesmo com sessões paralelas). Ele sobe sozinho
na primeira falha de conexão do hook, a menos que `NG_AUTOSTART=0` esteja definido.

Para abrir a UI web (o daemon é garantido no ar antes):

```bash
./target/release/ng ui   # abre http://127.0.0.1:4949
```

A UI liga **só em `127.0.0.1`** — nunca é exposta na rede, porque transcripts contêm prompts,
saídas de ferramentas e às vezes segredos.

Se preferir manter o daemon sempre rodando em vez de depender do autostart, aponte um service
manager (`systemd --user`, `launchd`, ...) para `ngd`.

## 4. Primeira busca

Use o harness normalmente por uma sessão — qualquer prompt, qualquer tool call, já foi capturado.
Depois:

```bash
./target/release/ng search "aquele bug que corrigi"
./target/release/ng search "token expira" --here       # restrito ao projeto atual (cwd)
./target/release/ng status                              # tamanho do banco, contagens, daemon up/down
```

Na sessão seguinte, memórias relevantes de sessões *anteriores* já aparecem sozinhas como
contexto adicional no início do prompt — não precisa pedir.

## 5. Higiene de contexto (`ng clear`)

Sessões longas acumulam tool output e prosa velha que só engordam o próximo compact. `ng clear`
faz uma passada de eviction **lossless**: os itens mais frios da sessão ativa viram um stub
recuperável no transcript, com backup automático antes de qualquer escrita — nada é apagado do
banco not-goldfish, só sai do transcript ativo do harness.

```bash
./target/release/ng clear --dry-run     # só mostra o que seria colapsado
./target/release/ng clear               # aplica (com backup automático)
./target/release/ng clear --target-tokens 8000
```

Depois de aplicar, é preciso dar resume na sessão do harness (ex. `claude --resume <id>`) para
ele reler o transcript do disco.

Há também um modo automático, acionado pelo hook `PreCompact` em vez de manual — veja
`NG_AUTO_HYGIENE` na tabela de variáveis de ambiente do README. É desligado por padrão.

## 6. Embedder semântico real (opcional)

Por padrão a busca híbrida (`ng search --semantic`) usa o `HashEmbedder` — feature-hashing de
trigramas, zero dependências pesadas, determinístico. Para trocar por um embedder semântico de
verdade (`model2vec-rs`, CPU-only, sem ONNX/GPU):

```bash
cargo build --release --features model2vec
export NG_EMBED_MODEL=/caminho/para/o/modelo   # diretório de um modelo model2vec já baixado
```

Sem `NG_EMBED_MODEL`, o carregador procura em `~/.not-goldfish/models/potion-multilingual-128M`
por padrão. A feature `model2vec` é `local-only`: nunca baixa modelo da rede, só carrega o que já
está em disco — mantém o build padrão leve e o binário offline-safe.

## 7. Rodar o benchmark

O estudo "com vs. sem a ferramenta" (estilo LoCoMo) é reprodutível e 100% offline:

```bash
cargo run -p ng-bench --release                          # roda o estudo, imprime a tabela, grava o JSON
cargo test -p ng-bench --release                          # gates de qualidade (pisos de regressão)
cargo run -p ng-bench --release --features model2vec      # inclui o braço com embedder semântico real
```

Resultados em `crates/ng-bench/results/latest.json`. Metodologia e números completos em
`docs/benchmarks/with-vs-without.md`.

## 8. `ng doctor`

Diagnóstico de ambiente em uma linha por checagem — rode depois de qualquer upgrade ou quando a
memória parecer estar falhando silenciosamente:

```bash
./target/release/ng doctor
```

Cobre: binários `ng-hook`/`ngd` encontrados, daemon respondendo, saúde do banco (`journal_mode`,
contagens), hooks registrados no `settings.json` do harness, UI respondendo, backlog de eventos
aguardando embedding. Cada aviso/falha (`!`/`✗`) já vem com a correção de uma linha embutida na
própria mensagem.

## 9. Comandos do dia a dia

Três comandos cobrem a rotina de acompanhar e curar a memória:

```bash
ng gain                  # benefício acumulado desde a adoção (--here, --json, --since YYYY-MM-DD)
ng memory list           # inspeciona a memória própria; hide/unhide são reversíveis, add insere manual
ng sync-context          # (re)gera .ng/ — projeção commitável da memória do projeto (--init, --dir)
```

`ng gain` reporta capturas, injeções (como custo declarado) e tokens líquidos economizados pela
higiene. `ng memory hide` só remove da busca/injeção — nada capturado é deletado, `unhide`
restaura. `ng sync-context` deriva `.ng/context.md` e `.ng/decisions.md` do banco (que segue
sendo a fonte de verdade), com escrita atômica e guarda contra sobrescrever arquivos do usuário.
Detalhes de cada um nas seções homônimas do `README.md`.

## Troubleshooting

**Instalação e hooks**

- **`ng-hook`/`ngd` não encontrados** — rode `cargo build --release` de novo; `ng` procura os
  binários irmãos ao lado de si mesmo, então um build parcial (só `ng-cli`) não é suficiente.
- **Hooks parecem não disparar** — rode `ng doctor`; ele confere se os hooks certos estão
  presentes no `settings.json` do harness (projeto ou global, dependendo de como você instalou).
- **Feature `model2vec` não compila** — ela ativa a dependência `model2vec-rs`; confira se o
  toolchain Rust está atualizado. O carregamento do modelo em si (`Model2VecEmbedder`) nunca
  baixa nada da rede — se `NG_EMBED_MODEL` não apontar para um diretório válido, ele falha
  explicitamente em vez de tentar buscar um modelo remoto.

**Memória e UI**

- **Nada aparece em `ng search`** — confira `ng status`: se o banco ainda não existe, é porque
  nenhuma sessão rodou com os hooks instalados ainda. Use o harness normalmente por um pouco e
  tente de novo.
- **Injeção de memória não aparece no prompt** — confira se `NG_INJECT` não está setado para
  `0`/`false`/`off`, e se `NG_INJECT_MAX_RANK` não está cortando demais os resultados.
- **UI não abre** — `ng ui` imprime a URL mesmo se o `xdg-open` falhar; abra manualmente em
  `http://127.0.0.1:4949` (ou a porta de `NG_UI_PORT`, se customizada).

Para a lista completa de variáveis de ambiente e a arquitetura dos crates, veja o `README.md`.
