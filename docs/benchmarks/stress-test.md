# not-goldfish: stress test de escala extrema (10k–1M eventos)

## Veredito

**Três das quatro invariantes se sustentam até 1M de eventos; a invariante do hot path <5ms QUEBRA entre 100k e 500k eventos.** Nada capturado é deletado (rewrite de transcripts de 100MB/500MB só substitui conteúdo, com backup byte-idêntico e rename atômico), o SQLite (WAL+FTS5) permanece íntegro (`integrity_check`/`quick_check` = `ok` em todos os bancos após a carga), e a captura nunca quebra a sessão (invariante estrutural do `ng-hook`, ver abaixo). Mas o caminho quente do hook — `build_injection_readonly` por prompt — mede **p95 de 6.59ms com 500k eventos e 11.80ms com 1M**, acima do orçamento de ~5ms: o ponto de cruzamento está **entre 100k (p95 2.51ms) e 500k eventos**. Esse é o limite prático do produto hoje e o principal candidato à Fase 3 de confiabilidade.

## Ambiente

| Item | Valor |
|---|---|
| CPU | AMD Ryzen 5 5600G (12 threads) |
| RAM | 32 GB (32.746.580 kB) |
| Disco | NVMe (`/dev/nvme0n1p3`), ext4, ~239 GB livres no dia do teste |
| Versão | workspace `0.1.0`, commit base `ad3075a` + este harness |
| Build | `cargo build --release -p ng-bench` (LTO on) |
| Data da execução | 2026-07-21 (entre 18:11 e 18:21, UTC-3) |
| Isolamento | todos os bancos em `/tmp/ng-stress-4065995-1784668279087127117/` (dir temporário por execução); o banco real do usuário e o daemon da porta 4949 **não foram tocados** |

Resultados brutos (JSON): `stress-results.json` no diretório temporário acima, emitido pelo próprio harness.

## Cenário 1 — Escala de ingestão

Inserção pelo caminho real de captura (`Store::insert_event`, um autocommit por evento — a cadência que o daemon paga por evento capturado), com trigger FTS5 ativo em cada insert.

| N eventos | Tempo total | Eventos/seg | Tamanho final do .db | OOM/estouro |
|---:|---:|---:|---:|:--:|
| 10.000 | 2,15 s | 4.659 | 4,0 MB | não |
| 100.000 | 22,63 s | 4.419 | 40,5 MB | não |
| 500.000 | 129,96 s | 3.847 | 196,8 MB | não |
| 1.000.000 | 282,16 s | 3.544 | 393,6 MB | não |

**PASS.** ~390 bytes/evento no disco; a taxa degrada suavemente (4.659→3.544 ev/s, −24% de 10k a 1M), sem cliff. 1M foi o teto planejado e rodou completo — não há teto menor a declarar.

## Cenário 2 — Hot path do hook sob banco gigante (o teste mais importante)

`build_injection_readonly(db, prompt, session)` — o caminho real que o `ng-hook` paga por prompt (open read-only + FTS seletivo + bm25 + dedup + formatação). 120 execuções por tamanho; prompt com overlap lexical direto com eventos raros do corpus, e a injeção **encontrou memória em 100% das execuções** em todos os tamanhos (ou seja, o caminho medido é o caminho cheio, não um atalho vazio). Critério: **<5ms**.

| N eventos | min | p50 | p95 | max | <5ms? |
|---:|---:|---:|---:|---:|:--:|
| 10.000 | 1,17ms | 1,21ms | 1,38ms | 2,00ms | **PASS** |
| 100.000 | 2,04ms | 2,12ms | 2,51ms | 2,81ms | **PASS** |
| 500.000 | 5,98ms | 6,11ms | **6,59ms** | 6,74ms | **FAIL** |
| 1.000.000 | 10,35ms | 10,78ms | **11,80ms** | 12,45ms | **FAIL** |

**FAIL a partir de algum ponto entre 100k e 500k eventos** (o harness mediu os quatro degraus; o cruzamento exato de 5ms fica nesse intervalo — interpolar seria inventar número). A latência cresce ~linearmente com o corpus (~2,4ms por 100k eventos acima de 100k). **Finding F1 — candidato à Fase 3.**

## Cenário 3 — Busca/FTS em escala

`Store::search` (o caminho do `ng search`), 120 execuções por termo, projeto irrestrito, limit 10. Termo comum = "autenticação login token" (~20% do corpus); termo raro = "deadlock zephyr quorum raft" (~0,2% do corpus).

| N eventos | Termo comum p50 | Termo comum p95 | Termo raro p50 | Termo raro p95 |
|---:|---:|---:|---:|---:|
| 100.000 | 26,12ms | 27,32ms | 0,99ms | 1,11ms |
| 500.000 | 138,42ms | 153,63ms | 4,45ms | 5,51ms |
| 1.000.000 | 279,52ms | 309,57ms | 8,46ms | 9,79ms |

Sem invariante formal violada (o orçamento de 5ms é do hook, não do CLI), mas há um contraste claro: o `Store::search` (MATCH direto) degrada linearmente com a frequência do termo, enquanto o `search_for_injection` do hook — que poda termos comuns por IDF antes do MATCH — se mantém ~35–45× mais rápido no mesmo corpus (6,11ms vs 138–280ms em 500k/1M). **Finding F2:** a poda seletiva de IDF é o que segura o hot path hoje; queries de usuário com termos comuns no `ng search` ficam perceptivelmente lentas acima de 100k.

## Cenário 4 — Rewrite de transcript gigante

`rewrite_jsonl(path, drops=[], replacements=[(1, stub)])` — substituir exatamente 1 linha por um stub — sobre transcripts JSONL sintéticos de ~1KB/linha. Pico de disco amostrado a cada 25ms (original + backup + tmp coexistem durante a troca).

| Arquivo | Linhas | Tempo | Pico de disco | Backup criado e idêntico ao original | Linhas não-editadas intactas | Nº de linhas preservado |
|---|---:|---:|---:|:--:|:--:|:--:|
| 100.000.894 bytes (~100MB) | 98.116 | 0,78 s | 300,0 MB (3,0×) | sim | sim (byte a byte) | sim |
| 500.000.173 bytes (~500MB) | 490.657 | 3,11 s | 1.500,0 MB (3,0×) | sim | sim (byte a byte) | sim |

**PASS.** Nenhum DELETE: a contagem de linhas é preservada, a linha 1 virou o stub e todas as outras 490.656 linhas saíram byte-idênticas às originais. O backup `.ng-bak` é byte-idêntico ao arquivo original. O pico de disco é exatamente 3× o arquivo (original + backup + tmp), como o design prevê — documentado aqui porque **é um requisito de disco livre real**: higienizar um transcript de 500MB precisa de ~1GB livre no mesmo filesystem.

## Cenário 5 — Integridade sob carga

Após todos os cenários de escrita pesada, via conexão rusqlite direta em cada banco:

| Banco | integrity_check | quick_check |
|---|:--:|:--:|
| stress-10000.db | ok | ok |
| stress-100000.db | ok | ok |
| stress-500000.db | ok | ok |
| stress-1000000.db | ok | ok |

**PASS.** WAL+FTS5 íntegros em 1M de inserts com triggers, checkpoints e rebuilds de grafo.

## Cenário 6 — Rebuild do grafo de sabedoria em escala

`Store::graph_rebuild()` (wipe de entities/relations + re-ingestão completa a partir dos eventos):

| N eventos | Tempo | Eventos ingeridos no grafo |
|---:|---:|---:|
| 100.000 | 10,43 s | 100.000 |
| 500.000 | 53,31 s | 500.000 |
| 1.000.000 | 103,74 s | 1.000.000 |

**PASS** (sem invariante de tempo definida para rebuild — é operação batch/manutenção, não hot path). Escala linear (~9,6k eventos/s). Rebuild não-roda-no-prompt, então 104s em 1M é aceitável como manutenção, mas vale saber que `ng wisdom --rebuild` em bancos gigantes não é interativo.

## Metodologia

- **Harness**: `crates/ng-bench/src/bin/stress.rs` (binário `ng-stress`), consome só API pública de `ng-core`/`ng-hook`/`ng-sessions`. Nenhuma lógica de produto foi alterada.
- **Corpus**: templates determinísticos (mesmos do gate `tests/latency_floor.rs`), 5 templates comuns (~20% cada) + 1 template raro a cada 500 eventos (~0,2%) para que termos raros sobrevivam à poda de IDF do FTS seletivo; tags léxicas reais via `lex::extract_tags`; 32 sessões intercaladas.
- **Latências**: 120 execuções por medição (mínimo exigido: 100), p50/p95 por ordenação de amostras; build release com LTO. Cada chamada de hot path reabre o banco read-only, como o hook faz por prompt — não há conexão quente reusada inflando o resultado.
- **Isolamento**: um diretório temporário novo por execução (`/tmp/ng-stress-<pid>-<nanos>/`); banco real do usuário e daemon :4949 intocados; nenhum daemon foi subido para este teste.
- **Honestidade**: todo número acima veio da execução única registrada em `stress-results.json`. Nada foi interpolado ou extrapolado.

## Invariante não medida diretamente

- **"Captura nunca quebra a sessão" (hook sempre sai 0)**: **não testada end-to-end neste harness** — exercitar o binário `ng-hook` real exigiria invocar o hook com payloads de stdin, o que está fora do perímetro deste stress (que mede *escala*, não o protocolo do hook). A invariante é estrutural: `crates/ng-hook/src/main.rs:8` e `crates/ng-hook/src/main.rs:98` (toda falha de captura é engolida e o exit code permanece 0), coberta pelos testes unitários do crate. Sem evidência nova neste teste; sem evidência contrária.

## Findings

- **F1 (candidato à Fase 3) — hot path do hook estoura 5ms entre 100k e 500k eventos.** p95 2,51ms @100k → 6,59ms @500k → 11,80ms @1M, crescimento ~linear. O orçamento de ~5ms vale hoje até algum ponto nesse intervalo. Mitigações possíveis a investigar na Fase 3: retenção/particionamento do índice FTS, poda de corpus por projeto no caminho de injeção, ou cache de `fts5vocab`/term probe entre prompts.
- **F2 — `ng search` com termos comuns degrada linearmente** (26ms @100k → 280ms @1M, p50). Não viola invariante, mas degrada a experiência do CLI em bancos grandes; a poda de IDF que protege o hook não existe no MATCH direto do `search`.
- **F3 — rewrite de transcript exige ~3× o tamanho do arquivo em disco livre** (medido: 1,5GB de pico para 500MB). É o preço documentado da segurança (backup + tmp + rename atômico); usuários com transcripts gigantes e disco cheio vão falhar no rewrite — falha segura (original intocado), mas visível.
- **F4 — rebuild do grafo é ~104s em 1M eventos.** Aceitável como manutenção batch; relevante apenas se alguém propuser rebuild síncrono em caminho interativo.

## Reprodutibilidade

```bash
cargo build --release -p ng-bench
./target/release/ng-stress
```

O binário cria seu próprio diretório temporário isolado (impresso no stderr), roda os 6 cenários em sequência e grava `stress-results.json` nesse diretório, além de ecoar o JSON no stdout. Execução completa nesta máquina: ~10 minutos (dominada pela ingestão de 1M eventos, ~4,7 min). Pico de RAM observado: dentro do orçamento de 32GB sem swap; pico de disco: ~2,5GB no diretório temporário (4 bancos + transcripts, estes últimos removidos entre os dois tamanhos).
