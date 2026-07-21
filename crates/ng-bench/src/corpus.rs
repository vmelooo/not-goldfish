//! Deterministic multi-session corpus + ground truth for the with/without study.
//!
//! Every string is fixed (no `Date::now`, no `rand`): the same corpus is
//! rebuilt byte-for-byte on every run, so the benchmark is a reproducible
//! regression gate, not a moving target.
//!
//! Shape: each *task* has an early "establishing" session that states a FACT
//! (a decision, a file location, a fix) plus surrounding filler, and a later
//! session whose QUERY needs that fact. Hard negatives (lexically overlapping
//! but wrong facts) live in dedicated noise sessions to stress precision.

/// Fixed epoch base (2023-11-14T22:13:20Z). Offsets keep session ordering
/// sane without ever calling the clock.
pub const BASE_TS: i64 = 1_700_000_000;

/// Single project path for the whole synthetic corpus.
pub const PROJECT: &str = "/bench/not-goldfish";

/// One event to insert. `key` is a stable handle so tasks can point at their
/// gold event(s) without knowing the database row id assigned at insert time.
pub struct CorpusEvent {
    pub key: &'static str,
    pub session_id: &'static str,
    pub kind: &'static str,
    pub content: &'static str,
    pub tags: &'static str,
    pub ts_offset: i64,
}

/// Whether a task's later query reuses the gold document's own words (the easy
/// case FTS is built for) or deliberately avoids them (the semantic gap where a
/// lexical index is *expected* to struggle). Reporting these two classes
/// separately is the whole point: averaging them hides where lexical retrieval
/// is strong and where it is blind.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
pub enum TaskClass {
    /// Query and gold share exact tokens — FTS should win trivially.
    LexicalOverlap,
    /// Query paraphrases the gold with synonyms / morphology / pt-BR↔EN, so it
    /// shares (almost) no surface tokens with the gold. A real semantic
    /// embedder should win here; FTS and the char-trigram HashEmbedder are
    /// expected to be weak.
    SemanticGap,
}

impl TaskClass {
    pub fn label(self) -> &'static str {
        match self {
            TaskClass::LexicalOverlap => "lexical-overlap",
            TaskClass::SemanticGap => "semantic-gap",
        }
    }
}

/// One retrieval task: a later query that needs a fact established earlier.
pub struct Task {
    pub name: &'static str,
    /// Lexical-overlap vs semantic-gap — reported separately, never averaged.
    pub class: TaskClass,
    /// The later query text (what a coding agent would ask).
    pub query: &'static str,
    /// Session issuing the query — excluded from injection (its own context is
    /// already in the harness window).
    pub query_session: &'static str,
    /// Event key(s) that contain the ground-truth fact.
    pub gold_keys: &'static [&'static str],
    /// Session the agent would naively re-read in the WITHOUT arm to recover
    /// the fact (the establishing session). Its total tokens are the replay
    /// cost the tool must beat.
    pub replay_session: &'static str,
    /// Substring that must appear in a retrieved provenance snippet for the
    /// answer to count as grounded (hallucination proxy).
    pub needle: &'static str,
}

/// The full corpus: events to seed + tasks to evaluate.
pub struct Corpus {
    pub events: Vec<CorpusEvent>,
    pub tasks: Vec<Task>,
}

/// Establishing session helper: gold fact + two filler events, so replay cost
/// is realistically larger than a bounded top-k injection. Positional args are
/// deliberate here — a fixture builder reads clearer flat than wrapped in a
/// one-off params struct.
#[allow(clippy::too_many_arguments)]
fn est_session(
    events: &mut Vec<CorpusEvent>,
    session: &'static str,
    gold_key: &'static str,
    gold_tags: &'static str,
    gold: &'static str,
    filler_prompt: &'static str,
    filler_tool: &'static str,
    base_offset: i64,
) {
    events.push(CorpusEvent {
        key: gold_key,
        session_id: session,
        kind: "assistant",
        content: gold,
        tags: gold_tags,
        ts_offset: base_offset,
    });
    events.push(CorpusEvent {
        key: "",
        session_id: session,
        kind: "prompt",
        content: filler_prompt,
        tags: "",
        ts_offset: base_offset + 1,
    });
    events.push(CorpusEvent {
        key: "",
        session_id: session,
        kind: "tool_output",
        content: filler_tool,
        tags: "",
        ts_offset: base_offset + 2,
    });
}

/// Build the deterministic corpus. 16 tasks (10 lexical-overlap + 6
/// semantic-gap), pt-BR + code, with distractors and hard negatives in shared
/// noise sessions. 72 events total.
pub fn build_corpus() -> Corpus {
    let mut events: Vec<CorpusEvent> = Vec::new();

    est_session(
        &mut events,
        "est-01",
        "g01",
        "porta servidor desenvolvimento 8080",
        "Decidimos mover o servidor de desenvolvimento da porta 3000 para a 8080 \
         porque o Grafana já ocupava a 3000 e havia conflito no bind. Atualizado no \
         `docker-compose.yml` e no `.env` (DEV_PORT=8080).",
        "Precisamos investigar por que o `npm run dev` falhava com EADDRINUSE ao subir \
         o servidor local nesta máquina de desenvolvimento.",
        "$ lsof -i :3000\nCOMMAND   PID  USER   FD  TYPE  NODE NAME\ngrafana  8123  dev   12u  IPv4  TCP *:3000 (LISTEN)\n# porta 3000 ocupada pelo Grafana",
        100,
    );

    est_session(
        &mut events,
        "est-02",
        "g02",
        "retry politica cliente http retry_policy",
        "A política de retry do cliente HTTP fica em `src/net/retry_policy.rs`, não em \
         `client.rs`. O `client.rs` só monta o request; o backoff exponencial e o \
         limite de tentativas moram no `retry_policy.rs`.",
        "Onde eu mexo para aumentar o número máximo de tentativas do cliente HTTP?",
        "src/net/\n  client.rs        // montagem do request\n  retry_policy.rs  // backoff + max_retries\n  mod.rs",
        200,
    );

    est_session(
        &mut events,
        "est-03",
        "g03",
        "bug autenticacao jwt utc fuso",
        "O bug de autenticação era o token JWT expirando cedo demais porque `sign_token` \
         usava o horário local em vez de UTC; o fuso do servidor adiantava o `exp`. \
         Corrigido forçando UTC no `sign_token`.",
        "Usuários relatam logout aleatório poucos minutos depois de logar; parece \
         expiração de token antes da hora.",
        "// antes\nlet exp = Local::now() + Duration::hours(1);\n// depois\nlet exp = Utc::now() + Duration::hours(1);",
        300,
    );

    est_session(
        &mut events,
        "est-04",
        "g04",
        "serializacao socket messagepack protocolo",
        "Trocamos a serialização do protocolo do socket de JSON para MessagePack para \
         reduzir latência e tamanho de payload. A API pública HTTP continua em JSON; \
         só o canal do socket interno virou MessagePack.",
        "Qual crate de serialização devo importar para escrever no canal do socket?",
        "// socket frame\nlet buf = rmp_serde::to_vec(&frame)?;  // MessagePack\nsocket.write_all(&buf).await?;",
        400,
    );

    est_session(
        &mut events,
        "est-05",
        "g05",
        "NG_EMBED_MODEL model2vec embedder variavel",
        "A variável de ambiente NG_EMBED_MODEL aponta para o diretório do modelo \
         model2vec usado pelo embedder semântico. Se ela estiver ausente ou o modelo \
         não carregar, o processo cai no HashEmbedder zero-dependência.",
        "O que eu preciso configurar para ligar o embedder semântico de verdade?",
        "$ export NG_EMBED_MODEL=/opt/models/potion-base-8M\n$ ng doctor | grep embed\nembedder: model2vec (potion-base-8M)",
        500,
    );

    est_session(
        &mut events,
        "est-06",
        "g06",
        "coluna tokens_est tabela events orcamento",
        "Adicionamos a coluna `tokens_est` na tabela `events` para orçamento de contexto: \
         é uma estimativa barata (bytes/4) calculada no insert, usada pela higiene de \
         contexto para decidir o que injetar sem reprocessar o conteúdo inteiro.",
        "Como o injetor sabe o custo aproximado de um evento sem reler o conteúdo?",
        "ALTER TABLE events ADD COLUMN tokens_est INTEGER NOT NULL DEFAULT 0;\n-- preenchido no insert via Event::tokens_est()",
        600,
    );

    est_session(
        &mut events,
        "est-07",
        "g07",
        "rodar testes ng-core cargo release",
        "Para rodar só os testes do ng-core, use `cargo test -p ng-core --release`. \
         Rodar o workspace inteiro em debug estoura o tempo de CI por causa do SQLite \
         bundled recompilando; `-p` isola o crate e `--release` corta o overhead.",
        "Qual comando roda apenas a suíte de testes do crate ng-core?",
        "$ cargo test -p ng-core --release\n    Finished release [optimized] target(s)\n     Running unittests src/lib.rs",
        700,
    );

    est_session(
        &mut events,
        "est-08",
        "g08",
        "busca lenta selective_fts_query max id full-scan",
        "A busca ficava lenta porque `selective_fts_query` fazia `COUNT(*)` na tabela \
         events, forçando um full-scan a cada query (~90ms em 100k eventos). Trocamos \
         por `MAX(id)`, que é um lookup O(log n) na folha mais à direita do b-tree.",
        "Por que cada busca estava custando dezenas de milissegundos mesmo com FTS?",
        "-- antes: full-scan\nSELECT COUNT(*) FROM events;\n-- depois: rightmost-leaf lookup\nSELECT COALESCE(MAX(id), 0) FROM events;",
        800,
    );

    est_session(
        &mut events,
        "est-09",
        "g09",
        "ui web 127.0.0.1 localhost seguranca bind",
        "A UI web tem que escutar apenas em 127.0.0.1, nunca em 0.0.0.0, porque os \
         transcripts contêm prompts, saídas de ferramentas e às vezes segredos — não é \
         uma superfície para expor na rede local.",
        "Em qual endereço o servidor axum da UI pode dar bind com segurança?",
        "let addr: SocketAddr = ([127, 0, 0, 1], port).into();\naxum::serve(listener, app).await?;  // loopback only",
        900,
    );

    est_session(
        &mut events,
        "est-10",
        "g10",
        "rusqlite bundled sqlite vendored dependencia",
        "Escolhemos `rusqlite` com a feature `bundled` para compilar o SQLite a partir \
         das fontes vendored. Assim o build não depende de `libsqlite3-dev` nem de \
         nenhuma lib de sistema — é offline-safe e reproduzível.",
        "Por que a gente não precisa instalar libsqlite3-dev para buildar o projeto?",
        "rusqlite = { version = \"0.31\", features = [\"bundled\"] }\n# SQLite compilado das fontes vendored",
        1000,
    );

    // Distractor / hard-negative sessions: lexically overlap the gold facts
    // ("porta", "servidor", "JSON", "token", "127") but state the WRONG thing.
    let noise: &[(&str, &str, &str, &str, i64)] = &[
        (
            "noise-a",
            "tool_output",
            "A porta do Postgres em produção é a 5432 e a do Redis é a 6379; nenhuma \
             delas muda por causa do servidor de desenvolvimento.",
            "porta postgres redis producao",
            2000,
        ),
        (
            "noise-a",
            "assistant",
            "O servidor de produção fica atrás de um proxy na porta 443 (HTTPS); só o \
             ambiente de desenvolvimento expõe portas altas diretamente.",
            "servidor producao 443 proxy",
            2010,
        ),
        (
            "noise-b",
            "assistant",
            "A API pública HTTP continua respondendo em JSON puro; clientes externos \
             não falam MessagePack e não devem ser migrados.",
            "api publica json externo",
            2100,
        ),
        (
            "noise-b",
            "tool_output",
            "O refresh token de sessão dura 30 dias e é guardado no banco; não confundir \
             com o access token JWT curto que expira em uma hora.",
            "refresh token sessao banco",
            2110,
        ),
        (
            "noise-c",
            "assistant",
            "O daemon ngd escuta em um socket Unix em `$XDG_RUNTIME_DIR/ng.sock`, não em \
             TCP; é o writer único do banco. Isso é separado da UI web.",
            "daemon socket unix writer",
            2200,
        ),
        (
            "noise-c",
            "tool_output",
            "O HashEmbedder usa char-trigramas em 256 dimensões e não lê variável de \
             ambiente nenhuma; é o fallback puro em Rust.",
            "hashembedder trigrama fallback",
            2210,
        ),
    ];
    for (session, kind, content, tags, offset) in noise {
        events.push(CorpusEvent {
            key: "",
            session_id: session,
            kind,
            content,
            tags,
            ts_offset: *offset,
        });
    }

    // One event per query session so `exclude_session` actually excludes real
    // in-session context (the current work), as the injection path does live.
    let query_ctx: &[(&str, &str, i64)] = &[
        (
            "qry-01",
            "Estou subindo o ambiente local de novo depois de formatar a máquina.",
            3000,
        ),
        (
            "qry-02",
            "Refatorando o cliente HTTP para ter timeout configurável.",
            3010,
        ),
        (
            "qry-03",
            "Revisando o fluxo de login antes do release.",
            3020,
        ),
        (
            "qry-04",
            "Escrevendo um novo worker que escreve no socket interno.",
            3030,
        ),
        (
            "qry-05",
            "Configurando a máquina de um colega novo no time.",
            3040,
        ),
        ("qry-06", "Mexendo na camada de higiene de contexto.", 3050),
        (
            "qry-07",
            "Preparando o pipeline de CI para rodar mais rápido.",
            3060,
        ),
        (
            "qry-08",
            "Investigando latência da busca em produção.",
            3070,
        ),
        ("qry-09", "Preparando a UI web para uma demo.", 3080),
        ("qry-10", "Ajustando o Cargo.toml do workspace.", 3090),
    ];
    for (session, content, offset) in query_ctx {
        events.push(CorpusEvent {
            key: "",
            session_id: session,
            kind: "prompt",
            content,
            tags: "",
            ts_offset: *offset,
        });
    }

    // ---------------------------------------------------------------------
    // Semantic-gap establishing sessions. The gold FACT is written in one
    // vocabulary; the later query (below) paraphrases it in a DIFFERENT
    // vocabulary (synonyms, morphology, pt-BR↔EN). Tags deliberately reuse only
    // the gold's OWN words — never the query's paraphrase — so the 2x tag
    // weight cannot leak the answer to the lexical arm. On these, FTS and the
    // char-trigram HashEmbedder are *expected* to be weak; a real semantic
    // embedder is the only arm that should win.
    // ---------------------------------------------------------------------
    est_session(
        &mut events,
        "est-11",
        "g11",
        "handle_request assincrona thread runtime bloqueio",
        "A função `handle_request` precisa virar assíncrona; hoje ela bloqueia a \
         thread do runtime a cada chamada de disco, serializando todo o servidor.",
        "Revisando o caminho quente do servidor sob carga.",
        "flamegraph: 78% do tempo preso em handle_request aguardando I/O de disco",
        1100,
    );

    est_session(
        &mut events,
        "est-12",
        "g12",
        "ordenacao bubble quicksort fila eventos latencia",
        "Trocamos o algoritmo de ordenação da fila de eventos de bubble sort para \
         quicksort para cortar a latência quando o lote é grande.",
        "Perfilando o processamento em lote.",
        "// antes: bubble sort O(n^2)\n// depois: quicksort O(n log n)",
        1200,
    );

    est_session(
        &mut events,
        "est-13",
        "g13",
        "workers embedding reduzido saturacao captura",
        "O número de workers de embedding foi reduzido de 8 para 2 porque acima \
         disso o processador saturava e a latência de captura subia.",
        "Ajustando o pool de background do ngd.",
        "load average 14.2 com 8 workers; 3.1 com 2 workers",
        1300,
    );

    est_session(
        &mut events,
        "est-14",
        "g14",
        "cache respostas lru 500 despejo entradas",
        "O cache de respostas do cliente usa política LRU com teto de 500 entradas; \
         ao encher, despeja a entrada usada menos recentemente.",
        "Mexendo na camada de cache do cliente.",
        "struct ResponseCache { map: LruCache<Key, Resp>, cap: 500 }",
        1400,
    );

    est_session(
        &mut events,
        "est-15",
        "g15",
        "rate limit api requisicoes minuto 429 token",
        "O rate limit da API pública é de 100 requisições por minuto por token; \
         acima disso a API responde 429 Too Many Requests.",
        "Documentando os limites da API pública.",
        "if window.count > 100 { return StatusCode::TOO_MANY_REQUESTS; } // 429",
        1500,
    );

    est_session(
        &mut events,
        "est-16",
        "g16",
        "release assinatura gpg artefatos rejeitar",
        "Passamos a assinar os artefatos de release com uma chave GPG; downloads \
         sem assinatura válida devem ser rejeitados pelo instalador.",
        "Endurecendo o pipeline de publicação.",
        "$ gpg --verify ng-1.0.tar.gz.sig ng-1.0.tar.gz\ngpg: Good signature",
        1600,
    );

    // Hard negatives for the partial-gap tasks (est-14, est-15): they share the
    // STRONG lexical tokens of the query ("cache", "API", "requisição") but
    // state the WRONG fact, so lexical recall pulls both and only a reranker
    // that understands the paraphrase can prefer the gold.
    let semantic_noise: &[(&str, &str, &str, &str, i64)] = &[
        (
            "noise-d",
            "assistant",
            "O cache de disco do compilador guarda artefatos compilados e não impõe \
             limite de entradas nem faz despejo — cresce até o disco encher.",
            "cache disco compilador artefatos",
            2300,
        ),
        (
            "noise-d",
            "tool_output",
            "O limite de tamanho de upload da API é de 10 MB por requisição; acima \
             disso a API responde 413 Payload Too Large.",
            "upload api tamanho 413 requisicao",
            2310,
        ),
    ];
    for (session, kind, content, tags, offset) in semantic_noise {
        events.push(CorpusEvent {
            key: "",
            session_id: session,
            kind,
            content,
            tags,
            ts_offset: *offset,
        });
    }

    // Query-session context for the semantic-gap queries (one in-session event
    // each, so `exclude_session` excludes real current-work context as the live
    // injection path does).
    let sem_query_ctx: &[(&str, &str, i64)] = &[
        (
            "sqry-11",
            "Otimizando o servidor para não travar sob carga.",
            3100,
        ),
        ("sqry-12", "Acelerando o processamento em lote.", 3110),
        (
            "sqry-13",
            "Dimensionando o paralelismo do enriquecimento.",
            3120,
        ),
        ("sqry-14", "Investigando uso de memória do cliente.", 3130),
        ("sqry-15", "Integrando um cliente novo contra a API.", 3140),
        (
            "sqry-16",
            "Escrevendo o passo de verificação do instalador.",
            3150,
        ),
    ];
    for (session, content, offset) in sem_query_ctx {
        events.push(CorpusEvent {
            key: "",
            session_id: session,
            kind: "prompt",
            content,
            tags: "",
            ts_offset: *offset,
        });
    }

    let tasks = vec![
        Task {
            name: "porta-do-servidor-dev",
            class: TaskClass::LexicalOverlap,
            query: "qual porta o servidor de desenvolvimento usa agora?",
            query_session: "qry-01",
            gold_keys: &["g01"],
            replay_session: "est-01",
            needle: "8080",
        },
        Task {
            name: "local-da-politica-de-retry",
            class: TaskClass::LexicalOverlap,
            query: "onde fica a política de retry do cliente HTTP no código?",
            query_session: "qry-02",
            gold_keys: &["g02"],
            replay_session: "est-02",
            needle: "retry_policy.rs",
        },
        Task {
            name: "fix-do-bug-jwt",
            class: TaskClass::LexicalOverlap,
            query: "como resolvemos o bug de expiração do token JWT?",
            query_session: "qry-03",
            gold_keys: &["g03"],
            replay_session: "est-03",
            needle: "UTC",
        },
        Task {
            name: "serializacao-do-socket",
            class: TaskClass::LexicalOverlap,
            query: "qual formato de serialização usamos no canal do socket agora?",
            query_session: "qry-04",
            gold_keys: &["g04"],
            replay_session: "est-04",
            needle: "MessagePack",
        },
        Task {
            name: "papel-da-NG_EMBED_MODEL",
            class: TaskClass::LexicalOverlap,
            query: "o que a variável NG_EMBED_MODEL controla?",
            query_session: "qry-05",
            gold_keys: &["g05"],
            replay_session: "est-05",
            needle: "model2vec",
        },
        Task {
            name: "coluna-de-orcamento-de-tokens",
            class: TaskClass::LexicalOverlap,
            query: "qual coluna guarda a estimativa de tokens na tabela events?",
            query_session: "qry-06",
            gold_keys: &["g06"],
            replay_session: "est-06",
            needle: "tokens_est",
        },
        Task {
            name: "comando-de-testes-ng-core",
            class: TaskClass::LexicalOverlap,
            query: "como rodo apenas os testes do crate ng-core?",
            query_session: "qry-07",
            gold_keys: &["g07"],
            replay_session: "est-07",
            needle: "cargo test -p ng-core",
        },
        Task {
            name: "otimizacao-da-busca-lenta",
            class: TaskClass::LexicalOverlap,
            query: "por que a busca estava lenta e o que trocamos para acelerar?",
            query_session: "qry-08",
            gold_keys: &["g08"],
            replay_session: "est-08",
            needle: "MAX(id)",
        },
        Task {
            name: "bind-seguro-da-ui-web",
            class: TaskClass::LexicalOverlap,
            query: "em qual endereço a UI web pode escutar com segurança?",
            query_session: "qry-09",
            gold_keys: &["g09"],
            replay_session: "est-09",
            needle: "127.0.0.1",
        },
        Task {
            name: "motivo-do-rusqlite-bundled",
            class: TaskClass::LexicalOverlap,
            query: "por que usamos rusqlite com a feature bundled?",
            query_session: "qry-10",
            gold_keys: &["g10"],
            replay_session: "est-10",
            needle: "bundled",
        },
        // ---- Semantic-gap tasks: query shares (almost) no tokens with gold ----
        // Full gap (pt-BR synonyms): "tratador de entrada / segurar o executor"
        // vs gold "handle_request / bloqueia a thread". No content-token overlap
        // → lexical recall cannot reach the gold at all.
        Task {
            name: "sg-handler-assincrono",
            class: TaskClass::SemanticGap,
            query: "como faço o tratador de entrada rodar sem prender o executor em \
                    toda operação de arquivo?",
            query_session: "sqry-11",
            gold_keys: &["g11"],
            replay_session: "est-11",
            needle: "assíncrona",
        },
        // Full gap (EN query over pt-BR gold): "sorting approach / big batches"
        // vs "ordenação / lote grande".
        Task {
            name: "sg-ordenacao-en",
            class: TaskClass::SemanticGap,
            query: "which sorting approach did we adopt to speed up big batches?",
            query_session: "sqry-12",
            gold_keys: &["g12"],
            replay_session: "est-12",
            needle: "quicksort",
        },
        // Full gap (morphology + synonym): "linhas de execução paralelas / \
        // enriquecimento / máquina" vs "workers de embedding / processador".
        Task {
            name: "sg-workers-paralelismo",
            class: TaskClass::SemanticGap,
            query: "quantas linhas de execução paralelas o enriquecimento deve \
                    manter sem sobrecarregar a máquina?",
            query_session: "sqry-13",
            gold_keys: &["g13"],
            replay_session: "est-13",
            needle: "2",
        },
        // Partial gap + hard negative (noise-d): shares "cache"/"respostas" with
        // gold AND with a wrong-fact distractor, so recall pulls both — only a
        // reranker that grasps "estratégia de expiração ⇒ LRU" prefers the gold.
        Task {
            name: "sg-cache-expiracao",
            class: TaskClass::SemanticGap,
            query: "qual estratégia de expiração o cache de respostas aplica \
                    quando enche?",
            query_session: "sqry-14",
            gold_keys: &["g14"],
            replay_session: "est-14",
            needle: "LRU",
        },
        // Partial gap + hard negative: shares "API"/"minuto" with gold and "API"
        // with the upload-limit distractor; "chamadas ⇒ requisições" is semantic.
        Task {
            name: "sg-rate-limit",
            class: TaskClass::SemanticGap,
            query: "quantas chamadas por minuto a API aceita antes de bloquear?",
            query_session: "sqry-15",
            gold_keys: &["g15"],
            replay_session: "est-15",
            needle: "100",
        },
        // Full gap (concept paraphrase): "binário baixado é autêntico e não foi
        // adulterado" vs gold "assinar artefatos com chave GPG".
        Task {
            name: "sg-assinatura-release",
            class: TaskClass::SemanticGap,
            query: "como o usuário confirma que o binário baixado é autêntico e \
                    não foi adulterado?",
            query_session: "sqry-16",
            gold_keys: &["g16"],
            replay_session: "est-16",
            needle: "GPG",
        },
    ];

    Corpus { events, tasks }
}
