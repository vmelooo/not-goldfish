# Claude Code adapter

not-goldfish integra com Claude Code via os hooks nativos do harness (mesmo
esquema clonado por Kimi Code). `ng install` já registra os quatro eventos
de captura padrão (`UserPromptSubmit`, `PostToolUse`, `SessionStart`,
`Stop`) — veja `crates/ng-cli/src/install.rs`.

Este documento cobre o evento `PreCompact`, que ainda não está no install
automático (será adicionado depois; até lá, registre manualmente com o
snippet abaixo).

## Evento `PreCompact`

Claude Code dispara `PreCompact` logo antes de compactar o contexto da
sessão (manual ou automaticamente, via `trigger`). O payload no stdin do
hook inclui, entre outros campos:

```json
{
  "hook_event_name": "PreCompact",
  "session_id": "...",
  "transcript_path": "/home/user/.claude/projects/<encoded-cwd>/<uuid>.jsonl",
  "trigger": "auto" 
}
```

`ng-hook` sempre captura `PreCompact` como um evento normal
(`kind: "precompact"`, sem conteúdo) — isso já acontece automaticamente,
independente de qualquer configuração. O que é opt-in é a **higiene
procedural**: reescrever o próprio arquivo de transcript da sessão viva
para liberar espaço antes do compact rodar em cima dele.

## `NG_AUTO_HYGIENE` (opt-in, desligado por padrão)

Reescrever o transcript de uma sessão ativa durante o compact é território
sensível — um bug aqui poderia corromper uma sessão em andamento. Por
isso a higiene do PreCompact só roda quando a variável de ambiente
`NG_AUTO_HYGIENE=1` está setada no processo que invoca `ng-hook`. Sem ela
(o padrão), `ng-hook` não lê nem escreve o arquivo de transcript — apenas
captura o evento normalmente.

Quando ativa, a cada `PreCompact`:

1. Carrega o transcript da sessão via `ng_sessions::claude::parse`.
2. Roda `ng_sessions::hygiene::score_items` + `plan_eviction` com um
   orçamento de `NG_HYGIENE_TARGET_TOKENS` tokens estimados (padrão:
   `20000`; ajustável, ex. `NG_HYGIENE_TARGET_TOKENS=5000`).
3. Aplica o plano via `apply_eviction_claude`, que **nunca apaga linhas**:
   troca só o `message.content` dos itens evictáveis por um stub
   recuperável (`[ng-evicted: ...]`), preservando `uuid`/`parentUuid`/etc.
   Um backup completo (`<arquivo>.<epoch>.ng-bak`) é sempre gravado antes
   da troca, e o conteúdo original também continua no banco do
   not-goldfish, capturado pelos hooks de captura normais.
4. Se algo foi de fato evictado, emite no stdout o JSON de hook do
   PreCompact com um resumo em `additionalContext` (itens viraram stub,
   tokens liberados, caminho do backup) — o Claude Code então compacta o
   transcript já enxuto.

Qualquer erro em qualquer etapa (payload malformado, transcript
ilegível, falha de rewrite) é silencioso: a higiene nunca pode ser a razão
de um compact — ou da sessão — quebrar.

Variáveis de ambiente relevantes:

| Variável | Padrão | Efeito |
|---|---|---|
| `NG_AUTO_HYGIENE` | desligado | `1` liga a higiene do PreCompact |
| `NG_HYGIENE_TARGET_TOKENS` | `20000` | orçamento de tokens a tentar liberar por passada |

## Registro manual do hook

Até o `ng install` cobrir `PreCompact` automaticamente, adicione a entrada
a mão em `.claude/settings.json` (projeto) ou `~/.claude/settings.json`
(global), no mesmo formato que `ng install` já usa para os outros eventos:

```json
{
  "hooks": {
    "PreCompact": [
      {
        "hooks": [
          { "type": "command", "command": "/path/to/ng-hook" }
        ]
      }
    ]
  }
}
```

Combine com a env var no ambiente onde o Claude Code roda, por exemplo no
shell profile ou num wrapper:

```bash
export NG_AUTO_HYGIENE=1
export NG_HYGIENE_TARGET_TOKENS=20000
```

Sem `NG_AUTO_HYGIENE=1`, registrar o hook é inofensivo: `ng-hook` só
captura o evento `PreCompact` como marcador, sem tocar no transcript.
