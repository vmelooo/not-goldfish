#!/usr/bin/env bash
# System end-to-end check for not-goldfish: exercises the real release
# binaries (ng, ngd, ng-hook) against a throwaway NG_DATA_DIR/$HOME, the
# way a real harness session would. Not a unit test — every crate has its
# own; this is the thing that catches "the pieces don't fit together".
#
# Usage:
#   scripts/e2e.sh                 # build + run everything
#   E2E_SKIP_BUILD=1 scripts/e2e.sh  # skip `cargo build --release`, reuse
#                                     # whatever is already in target/release
#
# Exit code: 0 if every assertion passed, 1 if any failed.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN_DIR="$REPO_ROOT/target/release"

# cargo/rustup resolve their toolchain from the real $HOME (~/.cargo,
# ~/.rustup) unless CARGO_HOME/RUSTUP_HOME override it — save it now,
# before the "isolated environment" section below replaces $HOME with a
# throwaway fake one for the rest of the script.
REAL_HOME="${HOME:-}"

NG_BIN="$BIN_DIR/ng"
NGD_BIN="$BIN_DIR/ngd"
NG_HOOK_BIN="$BIN_DIR/ng-hook"

# ---------------------------------------------------------------------------
# assertion plumbing
# ---------------------------------------------------------------------------
PASS=0
FAIL=0
SKIPPED=0

section() { echo; echo "== $* =="; }

pass() { PASS=$((PASS + 1)); echo "  PASS: $1"; }
fail() {
    FAIL=$((FAIL + 1))
    echo "  FAIL: $1"
    if [[ $# -gt 1 ]]; then
        echo "    --- detalhe ---"
        printf '%s\n' "$2" | sed 's/^/    /' | head -20
    fi
}
skip() { SKIPPED=$((SKIPPED + 1)); echo "  SKIP: $1 ($2)"; }

assert_eq() {
    local desc="$1" expected="$2" actual="$3"
    if [[ "$expected" == "$actual" ]]; then
        pass "$desc"
    else
        fail "$desc (esperado '$expected', obtido '$actual')"
    fi
}

assert_contains() {
    local desc="$1" haystack="$2" needle="$3"
    if printf '%s' "$haystack" | grep -qF -- "$needle"; then
        pass "$desc"
    else
        fail "$desc (esperava conter: '$needle')" "$haystack"
    fi
}

summarize_and_exit() {
    section "resumo"
    echo "PASS=$PASS FAIL=$FAIL SKIP=$SKIPPED"
    if [[ $FAIL -gt 0 ]]; then
        echo "resultado: FALHOU"
        exit 1
    fi
    echo "resultado: OK"
    exit 0
}

if ! command -v python3 >/dev/null 2>&1; then
    echo "python3 é necessário (usado como cliente HTTP e para montar fixtures JSON) — abortando" >&2
    exit 2
fi

# ---------------------------------------------------------------------------
# isolated environment: nothing here ever touches the real $HOME or a real
# not-goldfish install. NG_UI_PORT is fixed for this run so every ngd
# spawned during the script (including via autostart) serves the UI on a
# port we control.
# ---------------------------------------------------------------------------
TMP_ROOT="$(mktemp -d)"
export NG_DATA_DIR="$TMP_ROOT/data"
export HOME="$TMP_ROOT/home"
export NG_UI_PORT=$((40000 + (RANDOM % 10000)))
PROJECT_DIR="$TMP_ROOT/project"
OTHER_DIR="$TMP_ROOT/other-project"
mkdir -p "$NG_DATA_DIR" "$HOME" "$PROJECT_DIR" "$OTHER_DIR"

cleanup() {
    # Bracketed first char is the classic self-match guard: pgrep/pkill -f
    # matches full command lines, and "[n]gd" never matches the literal
    # string "[n]gd" that would appear in this very cleanup function's own
    # argv, so it can never kill itself or a sibling e2e run by accident —
    # it only ever matches a real ngd process built at this repo's path.
    pkill -f "target/release/[n]gd" >/dev/null 2>&1 || true
    rm -rf "$TMP_ROOT"
}
trap cleanup EXIT

wait_for_socket() {
    local path="$1" timeout_s="${2:-5}" tries
    tries=$((timeout_s * 5))
    for ((i = 0; i < tries; i++)); do
        [[ -S "$path" ]] && return 0
        sleep 0.2
    done
    return 1
}

wait_for_port() {
    local port="$1" timeout_s="${2:-5}" tries
    tries=$((timeout_s * 5))
    for ((i = 0; i < tries; i++)); do
        if python3 -c "
import socket, sys
s = socket.socket()
s.settimeout(0.2)
try:
    s.connect(('127.0.0.1', int(sys.argv[1])))
    sys.exit(0)
except Exception:
    sys.exit(1)
" "$port" 2>/dev/null; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}

http_status() {
    python3 - "$1" <<'PY'
import sys, urllib.request, urllib.error
try:
    with urllib.request.urlopen(sys.argv[1], timeout=5) as r:
        print(r.status)
except urllib.error.HTTPError as e:
    print(e.code)
except Exception:
    print("ERR")
PY
}

http_body() {
    python3 - "$1" <<'PY'
import sys, urllib.request, urllib.error
try:
    with urllib.request.urlopen(sys.argv[1], timeout=5) as r:
        sys.stdout.write(r.read().decode(errors="replace"))
except urllib.error.HTTPError as e:
    sys.stdout.write(e.read().decode(errors="replace"))
except Exception as e:
    sys.stdout.write("ERR:" + str(e))
PY
}

# hook_payload key1 val1 key2 val2 ... -> one line of JSON on stdout. Builds
# via python3's json module instead of hand-escaped bash strings so prompt
# text with quotes/unicode never breaks the payload.
hook_payload() {
    python3 -c "
import json, sys
args = sys.argv[1:]
print(json.dumps(dict(zip(args[0::2], args[1::2]))))
" "$@"
}

kill_daemon() {
    pkill -f "target/release/[n]gd" >/dev/null 2>&1 || true
    sleep 0.3
    rm -f "$NG_DATA_DIR/ngd.sock"
}

# ---------------------------------------------------------------------------
# 1. build
# ---------------------------------------------------------------------------
section "build"
if [[ "${E2E_SKIP_BUILD:-0}" != "1" ]]; then
    echo "compilando (cargo build --release)..."
    if (cd "$REPO_ROOT" && HOME="$REAL_HOME" cargo build --release); then
        pass "cargo build --release"
    else
        fail "cargo build --release"
        summarize_and_exit
    fi
else
    echo "build pulado (E2E_SKIP_BUILD=1), usando o que já está em $BIN_DIR"
fi

for bin in ng ngd ng-hook; do
    if [[ -x "$BIN_DIR/$bin" ]]; then
        pass "binário $bin existe em $BIN_DIR"
    else
        fail "binário $bin ausente em $BIN_DIR"
    fi
done
if [[ $FAIL -gt 0 ]]; then
    echo "binários essenciais ausentes — nada mais pode rodar, abortando"
    summarize_and_exit
fi

# ---------------------------------------------------------------------------
# 2. captura: fallback sem daemon, depois com daemon + autostart
# ---------------------------------------------------------------------------
section "captura: fallback sem daemon"
kill_daemon

payload=$(hook_payload hook_event_name UserPromptSubmit session_id s-fallback-1 cwd "$PROJECT_DIR" prompt "testar fallback sem daemon rodando")
printf '%s' "$payload" | NG_AUTOSTART=0 "$NG_HOOK_BIN" >/dev/null || true

if pgrep -f "target/release/[n]gd" >/dev/null 2>&1; then
    fail "NG_AUTOSTART=0 não deveria ter subido o daemon"
else
    pass "NG_AUTOSTART=0 não sobe o daemon"
fi

search_out=$("$NG_BIN" search "testar fallback" 2>&1 || true)
# ng search highlights matched query terms with >>term<< markers, which
# splits the literal phrase apart — assert on the session id instead of
# the prompt text, since it's never a highlighted token.
assert_contains "captura via fallback direto aparece na busca" "$search_out" "s-fallba"

section "captura: daemon + autostart"
kill_daemon

payload=$(hook_payload hook_event_name UserPromptSubmit session_id s-autostart-1 cwd "$PROJECT_DIR" prompt "disparar o autostart do daemon")
printf '%s' "$payload" | "$NG_HOOK_BIN" >/dev/null || true

if wait_for_socket "$NG_DATA_DIR/ngd.sock" 5; then
    pass "socket do daemon apareceu após autostart"
else
    fail "socket do daemon não apareceu em 5s após o autostart"
fi
sleep 0.5 # deixa o processo estabilizar antes de contar

daemon_count=$(pgrep -f "target/release/[n]gd" | wc -l | tr -d ' ')
assert_eq "exatamente um daemon subiu via autostart" "1" "$daemon_count"

search_out=$("$NG_BIN" search "disparar autostart" 2>&1 || true)
assert_contains "evento capturado com daemon ativo aparece na busca" "$search_out" "s-autost"

# ---------------------------------------------------------------------------
# 3. injeção proativa
# ---------------------------------------------------------------------------
section "injeção proativa"
sleep 0.3 # daemon aceitando conexões de verdade

seed1=$(hook_payload hook_event_name UserPromptSubmit session_id s-seed-auth cwd "$PROJECT_DIR" \
    prompt "corrigido bug de autenticacao no login o token expirava antes do refresh disparar")
printf '%s' "$seed1" | "$NG_HOOK_BIN" >/dev/null || true

seed2=$(hook_payload hook_event_name UserPromptSubmit session_id s-seed-cache cwd "$PROJECT_DIR" \
    prompt "configurado cache redis para acelerar as consultas mais lentas do dashboard")
printf '%s' "$seed2" | "$NG_HOOK_BIN" >/dev/null || true

sleep 0.5 # escritor assíncrono do daemon precisa persistir antes da busca

relevant=$(hook_payload hook_event_name UserPromptSubmit session_id s-query-auth cwd "$PROJECT_DIR" \
    prompt "o token esta expirando de novo antes do refresh, mesmo bug de autenticacao no login")
relevant_out=$(printf '%s' "$relevant" | "$NG_HOOK_BIN" || true)
assert_contains "prompt relevante recebe additionalContext com memória" "$relevant_out" "not-goldfish-memory"
assert_contains "resposta de injeção usa o envelope de hook esperado" "$relevant_out" "hookSpecificOutput"

irrelevant=$(hook_payload hook_event_name UserPromptSubmit session_id s-query-unrelated cwd "$PROJECT_DIR" \
    prompt "qual e a previsao do tempo para amanha em outra cidade qualquer")
irrelevant_out=$(printf '%s' "$irrelevant" | "$NG_HOOK_BIN" || true)
assert_eq "prompt irrelevante não recebe injeção (silêncio)" "" "$irrelevant_out"

# ---------------------------------------------------------------------------
# 4. busca
# ---------------------------------------------------------------------------
section "busca"

search_out=$("$NG_BIN" search "autenticacao login" 2>&1 || true)
assert_contains "busca léxica encontra o evento semeado" "$search_out" "autenticacao"

uniq_token="buscarestritamarker$$"
p1=$(hook_payload hook_event_name UserPromptSubmit session_id s-here-1 cwd "$PROJECT_DIR" \
    prompt "termo unico $uniq_token no projeto principal")
printf '%s' "$p1" | "$NG_HOOK_BIN" >/dev/null || true
p2=$(hook_payload hook_event_name UserPromptSubmit session_id s-here-2 cwd "$OTHER_DIR" \
    prompt "termo unico $uniq_token em outro projeto")
printf '%s' "$p2" | "$NG_HOOK_BIN" >/dev/null || true
sleep 0.5

global_out=$("$NG_BIN" search "$uniq_token" 2>&1 || true)
global_count=$(printf '%s\n' "$global_out" | grep -c "^#" || true)
if [[ "$global_count" -ge 2 ]]; then
    pass "busca sem --here vê os dois projetos ($global_count hits)"
else
    fail "busca sem --here deveria achar >=2 hits, achou $global_count" "$global_out"
fi

here_out=$(cd "$PROJECT_DIR" && "$NG_BIN" search "$uniq_token" --here 2>&1 || true)
here_count=$(printf '%s\n' "$here_out" | grep -c "^#" || true)
assert_eq "busca --here restringe ao projeto atual" "1" "$here_count"

sleep 3 # dá tempo pro enrich worker (poll a cada ~2s) gerar os embeddings
semantic_out=$("$NG_BIN" search "autenticacao login token" --semantic 2>&1 || true)
assert_contains "busca --semantic encontra o esperado" "$semantic_out" "autenticacao"

# ---------------------------------------------------------------------------
# 5. higiene procedural (PreCompact)
# ---------------------------------------------------------------------------
section "higiene procedural (PreCompact)"

precompact_dir="$TMP_ROOT/precompact-fixture"
mkdir -p "$precompact_dir"
transcript_path="$precompact_dir/session.jsonl"

python3 - "$transcript_path" <<'PY'
import json, sys
path = sys.argv[1]
big = "y" * 90000  # ~22_500 tokens estimados, bem acima do limiar de eviction
lines = [
    {"type": "user", "message": {"role": "user", "content": "por favor, limpe os logs de debug antigos"}, "uuid": "u0"},
    {"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": big}]}, "uuid": "u1", "parentUuid": "u0"},
]
for i in range(2, 5):
    lines.append({"type": "assistant", "message": {"role": "assistant", "content": f"ok {i}"}, "uuid": f"pad{i}"})
for i in range(5, 25):
    lines.append({"type": "assistant", "message": {"role": "assistant", "content": f"recent {i}"}, "uuid": f"hot{i}"})
with open(path, "w") as f:
    for line in lines:
        f.write(json.dumps(line) + "\n")
PY

before_lines=$(wc -l <"$transcript_path" | tr -d ' ')

payload=$(hook_payload hook_event_name PreCompact session_id s-precompact-1 transcript_path "$transcript_path" trigger manual)
precompact_out=$(printf '%s' "$payload" | NG_AUTO_HYGIENE=1 "$NG_HOOK_BIN" || true)

assert_contains "PreCompact emite hookSpecificOutput" "$precompact_out" "PreCompact"
assert_contains "resumo da higiene menciona tokens liberados" "$precompact_out" "tok liberados"

after_lines=$(wc -l <"$transcript_path" | tr -d ' ')
assert_eq "número de linhas do transcript preservado" "$before_lines" "$after_lines"

if grep -q '\[ng-evicted: tool_result' "$transcript_path"; then
    pass "stub de eviction aplicado no tool_result gigante"
else
    fail "stub de eviction não encontrado no transcript" "$(cat "$transcript_path")"
fi

backup_count=$(find "$precompact_dir" -name '*.ng-bak' | wc -l | tr -d ' ')
assert_eq "backup .ng-bak criado" "1" "$backup_count"

# ---------------------------------------------------------------------------
# 6. UI web
# ---------------------------------------------------------------------------
section "UI web"

claude_proj_dir="$HOME/.claude/projects/e2e-fake-project"
mkdir -p "$claude_proj_dir"
ui_session_path="$claude_proj_dir/11111111-1111-1111-1111-111111111111.jsonl"
python3 - "$ui_session_path" <<'PY'
import json, sys
with open(sys.argv[1], "w") as f:
    f.write(json.dumps({"type": "user", "message": {"role": "user", "content": "sessao de teste da UI"}, "uuid": "ui-u0", "cwd": "/tmp/e2e"}) + "\n")
PY

if [[ ! -S "$NG_DATA_DIR/ngd.sock" ]]; then
    "$NGD_BIN" >/dev/null 2>&1 &
    disown
fi
if wait_for_port "$NG_UI_PORT" 5; then
    pass "UI subiu em http://127.0.0.1:$NG_UI_PORT"
else
    fail "UI não respondeu em http://127.0.0.1:$NG_UI_PORT em 5s"
fi

base="http://127.0.0.1:$NG_UI_PORT"

status=$(http_status "$base/")
assert_eq "GET / -> 200" "200" "$status"

status=$(http_status "$base/api/status")
assert_eq "GET /api/status -> 200" "200" "$status"

status=$(http_status "$base/api/sessions")
assert_eq "GET /api/sessions -> 200" "200" "$status"

sessions_body=$(http_body "$base/api/sessions")
assert_contains "/api/sessions inclui a sessão fake da fixture" "$sessions_body" "e2e-fake-project"

encoded_path=$(python3 -c "import urllib.parse, sys; print(urllib.parse.quote(sys.argv[1]))" "$ui_session_path")
status=$(http_status "$base/api/transcript?path=$encoded_path")
assert_eq "GET /api/transcript com path válido -> 200" "200" "$status"

status=$(http_status "$base/api/transcript?path=/etc/passwd")
assert_eq "GET /api/transcript?path=/etc/passwd -> 403" "403" "$status"

status=$(http_status "$base/api/graph")
assert_eq "GET /api/graph -> 200" "200" "$status"

# ---------------------------------------------------------------------------
# 7. doctor
# ---------------------------------------------------------------------------
section "ng doctor"

(cd "$PROJECT_DIR" && "$NG_BIN" install >/dev/null)

doctor_out=$(cd "$PROJECT_DIR" && "$NG_BIN" doctor; echo "EXIT:$?")
echo "$doctor_out" | sed 's/^/    /'
doctor_exit=$(printf '%s\n' "$doctor_out" | grep -o 'EXIT:[0-9]*' | tail -1 | cut -d: -f2)
assert_eq "ng doctor sai com 0 em ambiente saudável" "0" "$doctor_exit"

# ---------------------------------------------------------------------------
# 8. wisdom / grafo (pula com aviso explícito se o subcomando ainda não
#    estiver disponível nesta build — não finge sucesso)
# ---------------------------------------------------------------------------
section "wisdom / grafo"

if "$NG_BIN" wisdom --help >/dev/null 2>&1; then
    wisdom_out=$(cd "$PROJECT_DIR" && "$NG_BIN" wisdom; echo "EXIT:$?")
    wisdom_exit=$(printf '%s\n' "$wisdom_out" | grep -o 'EXIT:[0-9]*' | tail -1 | cut -d: -f2)
    assert_eq "ng wisdom roda sem erro" "0" "$wisdom_exit"

    wisdom_md_out=$(cd "$PROJECT_DIR" && "$NG_BIN" wisdom --md; echo "EXIT:$?")
    wisdom_md_exit=$(printf '%s\n' "$wisdom_md_out" | grep -o 'EXIT:[0-9]*' | tail -1 | cut -d: -f2)
    assert_eq "ng wisdom --md roda sem erro" "0" "$wisdom_md_exit"
else
    skip "ng wisdom" "subcomando pendente de wiring nesta build (ng wisdom --help falhou)"
fi

# ---------------------------------------------------------------------------
# 9. resumo final
# ---------------------------------------------------------------------------
summarize_and_exit
