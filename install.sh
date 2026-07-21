#!/usr/bin/env bash
# not-goldfish — instalador de um comando.
#
#   curl -fsSL https://raw.githubusercontent.com/vmelooo/not-goldfish/main/install.sh | bash
#
# Instala GLOBALMENTE (todos os projetos do Claude Code). Outro harness:
#   curl -fsSL .../install.sh | bash -s -- --harness kimi     # também: gemini
#
# O instalador provisiona sozinho as dependências que faltarem (toolchain C,
# git, curl e Rust via rustup). Para gerenciar isso à mão:
#   curl -fsSL .../install.sh | bash -s -- --skip-deps        # ou NG_SKIP_DEPS=1
#
# Também funciona a partir de um clone local (`./install.sh`). Seguro de rodar
# de novo: o build é incremental e `ng install` é idempotente (não duplica hooks).

set -euo pipefail

REPO_URL="https://github.com/vmelooo/not-goldfish"
# Onde o bootstrap mantém o checkout (curl|bash). Um clone local usa a si mesmo.
SRC_DIR="${NG_SRC_DIR:-$HOME/.not-goldfish/src}"

# ---- disciplina de cor ------------------------------------------------------
# ANSI só quando faz sentido: stdout é um terminal, sem NO_COLOR e TERM útil.
# Fora disso (pipe para log, CI, NO_COLOR) as variáveis ficam vazias e a
# saída é texto puro — nenhum byte de escape vaza para arquivo nenhum.
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-}" != "dumb" ]; then
    C_GOLD=$'\033[1;33m'   # o dourado do projeto = bright yellow
    C_DIM=$'\033[2m'
    C_RED=$'\033[31m'
    C_GREEN=$'\033[32m'
    C_CYAN=$'\033[36m'
    C_BOLD=$'\033[1m'
    C_RST=$'\033[0m'
else
    C_GOLD=; C_DIM=; C_RED=; C_GREEN=; C_CYAN=; C_BOLD=; C_RST=
fi

ok()   { printf '%s✓%s %s\n' "$C_GREEN" "$C_RST" "$1"; }
info() { printf '%s→%s %s\n' "$C_CYAN" "$C_RST" "$1"; }
fail() { printf '%s✗%s %s\n' "$C_RED" "$C_RST" "$1" >&2; exit 1; }
phase() { printf '\n%s==>%s %s%s%s\n' "$C_GOLD" "$C_RST" "$C_BOLD" "$1" "$C_RST"; }

# Cabeçalho de abertura: peixinho dourado + tagline. Sem cor, vira uma linha
# de texto limpa (nada de arte desalinhada em log).
banner() {
    if [ -n "$C_GOLD" ]; then
        printf '\n%s><(((º>%s %snot-goldfish%s\n' "$C_GOLD" "$C_RST" "$C_BOLD" "$C_RST"
        printf '%s        memória universal para agentes de IA%s\n' "$C_DIM" "$C_RST"
    else
        printf '\nnot-goldfish — memória universal para agentes de IA\n'
    fi
}

# Rodapé de sucesso: caixa dourada com os próximos passos (box-drawing só na
# ramificação colorida; sem cor, o mesmo conteúdo em texto simples).
footer() {
    if [ -z "$C_GOLD" ]; then
        printf '\nPronto! Próximos passos:\n'
        printf '  ng doctor   # confere que está tudo saudável\n'
        printf '  ng ui       # abre a UI web local (http://127.0.0.1:4949)\n'
        printf '\nA partir da próxima sessão do harness, tudo é capturado e `ng search <termos>` encontra.\n'
        return 0
    fi
    local w=60 line pad hr="" i
    local lines=(
        'Pronto! Próximos passos:'
        '  ng doctor   # confere que está tudo saudável'
        '  ng ui       # abre a UI web local (http://127.0.0.1:4949)'
        ''
        'A partir da próxima sessão do harness, tudo é capturado e'
        '`ng search <termos>` encontra.'
    )
    for ((i = 0; i < w + 2; i++)); do hr+="─"; done
    printf '\n%s╭%s╮%s\n' "$C_GOLD" "$hr" "$C_RST"
    for line in "${lines[@]}"; do
        pad=$((w - ${#line}))
        printf '%s│%s %s%*s %s│%s\n' "$C_GOLD" "$C_RST" "$line" "$pad" "" "$C_GOLD" "$C_RST"
    done
    printf '%s╰%s╯%s\n' "$C_GOLD" "$hr" "$C_RST"
}

# ---- flags do instalador ----------------------------------------------------
# --skip-deps (ou NG_SKIP_DEPS=1) pula o provisionamento de dependências.
# Todo o resto é repassado intacto para o `ng install` no fim (ex.: --harness).
NG_ARGS=()
for arg in "$@"; do
    case "$arg" in
        --skip-deps) NG_SKIP_DEPS=1 ;;
        *) NG_ARGS+=("$arg") ;;
    esac
done

# >>> deps: provisionamento de dependências (bloco extraível p/ teste de fumaça)
#
# Instala, ANTES do build, só o que FALTAR: toolchain C (cc/gcc + make — o
# rusqlite usa a feature "bundled" e compila o SQLite a partir de fonte C),
# git, curl e o Rust via rustup. Idempotente: numa máquina já pronta é no-op.
# Gerenciador detectado por presença de binário, nesta ordem.
SUDO=()
PKG=""

detect_pkg_manager() {
    local m
    for m in apt-get dnf yum pacman zypper apk brew; do
        if command -v "$m" >/dev/null 2>&1; then PKG="$m"; return 0; fi
    done
    PKG=""
    return 1
}

# Instala toolchain C / git / curl via o gerenciador em $PKG.
# $1=1 se falta compilador/make, $2=1 se falta git, $3=1 se falta curl.
install_system_deps() {
    local miss_c="$1" miss_git="$2" miss_curl="$3"
    local pkgs=() cmd=""
    case "$PKG" in
        apt-get)
            [ "$miss_c" -eq 1 ] && pkgs+=(build-essential)
            cmd="apt-get update && apt-get install -y" ;;
        dnf|yum)
            [ "$miss_c" -eq 1 ] && pkgs+=(gcc make)
            cmd="$PKG install -y" ;;
        pacman)
            [ "$miss_c" -eq 1 ] && pkgs+=(base-devel)
            cmd="pacman -S --needed --noconfirm" ;;
        zypper)
            [ "$miss_c" -eq 1 ] && pkgs+=(gcc make)
            cmd="zypper --non-interactive install" ;;
        apk)
            [ "$miss_c" -eq 1 ] && pkgs+=(build-base)
            cmd="apk add" ;;
        brew)
            # Compilador no macOS vem das Command Line Tools, nunca do brew.
            cmd="brew install" ;;
    esac
    [ "$miss_git" -eq 1 ] && pkgs+=(git)
    [ "$miss_curl" -eq 1 ] && pkgs+=(curl)
    [ "${#pkgs[@]}" -eq 0 ] && return 0

    info "vou instalar via $PKG: ${pkgs[*]}"

    # sudo: só se não for root, com aviso claro do que será instalado.
    if [ "$(id -u)" -ne 0 ] && [ "$PKG" != "brew" ]; then
        if command -v sudo >/dev/null 2>&1; then
            SUDO=(sudo)
            info "não sou root — usando sudo para: $cmd ${pkgs[*]} (pode pedir senha)"
        else
            fail "sem root e sem sudo — instale manualmente e rode de novo: $cmd ${pkgs[*]}"
        fi
    fi

    case "$PKG" in
        apt-get)
            "${SUDO[@]}" apt-get update
            "${SUDO[@]}" env DEBIAN_FRONTEND=noninteractive apt-get install -y "${pkgs[@]}" ;;
        dnf|yum) "${SUDO[@]}" "$PKG" install -y "${pkgs[@]}" ;;
        pacman)  "${SUDO[@]}" pacman -S --needed --noconfirm "${pkgs[@]}" ;;
        zypper)  "${SUDO[@]}" zypper --non-interactive install "${pkgs[@]}" ;;
        apk)     "${SUDO[@]}" apk add "${pkgs[@]}" ;;
        brew)    brew install "${pkgs[@]}" ;;
    esac
}

ensure_deps() {
    if [ "${NG_SKIP_DEPS:-0}" = "1" ]; then
        info "pulando o provisionamento de dependências (--skip-deps / NG_SKIP_DEPS=1)"
        return 0
    fi

    local os miss_c=0 miss_git=0 miss_curl=0 miss_cargo=0
    os="$(uname -s)"
    command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1 || miss_c=1
    command -v make  >/dev/null 2>&1 || miss_c=1
    command -v git   >/dev/null 2>&1 || miss_git=1
    command -v curl  >/dev/null 2>&1 || miss_curl=1
    command -v cargo >/dev/null 2>&1 || miss_cargo=1

    if [ "$miss_c" -eq 0 ] && [ "$miss_git" -eq 0 ] && [ "$miss_curl" -eq 0 ] && [ "$miss_cargo" -eq 0 ]; then
        ok "dependências ok (compilador C, make, git, curl, cargo) — nada a instalar"
        return 0
    fi

    if [ "$miss_c" -eq 1 ] || [ "$miss_git" -eq 1 ] || [ "$miss_curl" -eq 1 ]; then
        if [ "$os" = "Darwin" ]; then
            if [ "$miss_c" -eq 1 ]; then
                info "macOS: instalando as Command Line Tools do Xcode (compilador C + make)..."
                xcode-select --install 2>/dev/null || true
                info "se abriu um diálogo, conclua a instalação — o build valida o compilador no fim."
            fi
            if [ "$miss_git" -eq 1 ] || [ "$miss_curl" -eq 1 ]; then
                if command -v brew >/dev/null 2>&1; then
                    PKG=brew
                    install_system_deps 0 "$miss_git" "$miss_curl"
                else
                    info "sem brew: git e curl vêm junto com as Command Line Tools acima."
                fi
            fi
        elif detect_pkg_manager; then
            install_system_deps "$miss_c" "$miss_git" "$miss_curl"
        else
            # Honestidade: sem gerenciador reconhecido, diz exatamente o que falta.
            info "gerenciador de pacotes não reconhecido — instale manualmente:"
            info "  toolchain C (gcc + make; Debian/Ubuntu: build-essential), git e curl."
        fi
    fi

    if [ "$miss_cargo" -eq 1 ]; then
        command -v curl >/dev/null 2>&1 \
            || fail "curl não disponível para baixar o rustup — instale curl e rode de novo."
        info "instalando o Rust via rustup (não-interativo, https://sh.rustup.rs)..."
        curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y \
            || fail "instalação do rustup falhou — tente manualmente: https://rustup.rs/"
        # cargo disponível já nesta sessão
        [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
        case ":$PATH:" in
            *":$HOME/.cargo/bin:"*) : ;;
            *) PATH="$HOME/.cargo/bin:$PATH" ;;
        esac
        command -v cargo >/dev/null 2>&1 \
            || fail "cargo não apareceu após o rustup — abra um shell novo e rode de novo."
    fi

    # Revalidação: se ainda falta algo de sistema, avisa antes do build quebrar.
    local still=""
    if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1; then still="$still cc/gcc"; fi
    command -v make >/dev/null 2>&1 || still="$still make"
    command -v git  >/dev/null 2>&1 || still="$still git"
    command -v curl >/dev/null 2>&1 || still="$still curl"
    [ -n "$still" ] && info "atenção: ainda faltam:$still — o build pode falhar; instale-os e rode de novo."
    return 0
}
# <<< deps

banner
phase "[1/4] Dependências"
ensure_deps

# Descobrir a raiz do repositório. Dois caminhos:
#  1. Clone local: este script mora ao lado de um Cargo.toml → usa esse checkout.
#  2. curl|bash: sem checkout à mão → clona (ou atualiza) em $SRC_DIR.
phase "[2/4] Código"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd || true)"
if [ -n "$SCRIPT_DIR" ] && [ -f "$SCRIPT_DIR/Cargo.toml" ]; then
    REPO_DIR="$SCRIPT_DIR"
    ok "usando o checkout local em $REPO_DIR"
else
    command -v git >/dev/null 2>&1 || fail "git não encontrado — instale git (ou clone o repo à mão e rode ./install.sh)."
    if [ -d "$SRC_DIR/.git" ]; then
        info "atualizando o checkout em $SRC_DIR..."
        git -C "$SRC_DIR" pull --ff-only --quiet || fail "git pull falhou em $SRC_DIR — resolva ou apague o diretório e rode de novo."
    else
        info "clonando $REPO_URL em $SRC_DIR..."
        mkdir -p "$(dirname "$SRC_DIR")"
        git clone --depth 1 --quiet "$REPO_URL" "$SRC_DIR" || fail "git clone falhou."
    fi
    REPO_DIR="$SRC_DIR"
    ok "código em $REPO_DIR"
fi

cd "$REPO_DIR"

# Guarda final: o ensure_deps acima já provisionou o que faltava (toolchain C
# incluída — o SQLite é compilado junto, bundled); aqui só confirma o cargo.
phase "[3/4] Build"
command -v cargo >/dev/null 2>&1 || fail "cargo não encontrado. Instale o Rust via rustup (https://rustup.rs/) e rode de novo."
ok "toolchain Rust encontrada ($(cargo --version))"

# Build dos três binários: ng (CLI), ngd (daemon) e ng-hook (chamado pelos hooks).
info "compilando o workspace em modo release (a primeira vez demora alguns minutos)..."
cargo build --release
ok "binários compilados"

# Instalar os três binários num diretório do PATH, lado a lado — o ng localiza
# ngd/ng-hook como irmãos (mesmo diretório do próprio executável), então basta
# que os três morem juntos ali. ~/.cargo/bin é o destino natural: o rustup já o
# mantém no PATH (e cargo existir acima garante que ele existe).
BIN_DIR="${CARGO_HOME:-$HOME/.cargo}/bin"
mkdir -p "$BIN_DIR"
install -m 755 target/release/ng target/release/ngd target/release/ng-hook "$BIN_DIR/"
ok "ng, ngd e ng-hook instalados em $BIN_DIR"

case ":$PATH:" in
    *":$BIN_DIR:"*) : ;;
    *) info "atenção: $BIN_DIR não está no seu PATH — adicione-o para digitar só \`ng\`." ;;
esac

# Registrar os hooks GLOBALMENTE (~/.claude/settings.json — vale para todos os
# projetos), usando o ng já no PATH (ele resolve ng-hook/ngd sozinho). O
# --global é fixo aqui; flags extras (ex.: --harness) vêm depois, já sem o
# --skip-deps que é só do instalador. `ng install` faz backup do settings.json
# (settings.json.ng-backup) antes de tocar nele.
phase "[4/4] Hooks"
"$BIN_DIR/ng" install --global ${NG_ARGS[@]+"${NG_ARGS[@]}"}
ok "hooks instalados globalmente"

# Próximos passos — agora é só `ng`, sem caminho nenhum.
footer
