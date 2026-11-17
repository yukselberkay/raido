#!/usr/bin/env bash
# Build and install Raido with everything it needs on this machine (macOS or Linux):
# a C compiler and linker, Git, curl, Rust (via rustup), the language servers for
# the languages Raido's editor understands (rust-analyzer, clangd, gopls when Go is
# installed, a Python server, bash-language-server with ShellCheck and shfmt; Docker's
# when Docker and Go are) from the system's package manager, JetBrains Mono Nerd
# Font, Raido itself (~/.cargo/bin/raido, made runnable by name from PATH), and the
# Raido skill that teaches AI agents how to use Raido. On Omarchy, packages come
# through `omarchy pkg add`, Raido follows the desktop's theme, and it gets an app
# launcher entry.
# Windows: scripts/install.ps1.
#
# Run it from a checkout (scripts/install.sh) or piped from GitHub
# (curl -fsSL …/scripts/install.sh | bash); outside a checkout it clones Raido
# first. Safe to run again: anything already installed is kept as is.
set -euo pipefail

usage="Usage: install.sh [--no-font] [--no-lsp] [--no-skill] [--no-launcher]
Installs a C compiler, Git, curl, Rust, language servers, JetBrains Mono Nerd Font,
Raido and its AI agent skill on macOS or Linux.
  --no-font      skip JetBrains Mono Nerd Font
  --no-lsp       skip the language servers (rust-analyzer, clangd, gopls, Python,
                 bash-language-server with ShellCheck and shfmt, Docker)
  --no-skill     skip the Raido skill for AI agents
  --no-launcher  Omarchy: skip Raido's app launcher entry (SUPER + SPACE)"

REPO_URL="https://github.com/yukselberkay/raido.git"
FONT_URL="https://github.com/ryanoasis/nerd-fonts/releases/latest/download/JetBrainsMono.tar.xz"
FONT_FAMILY="JetBrainsMono Nerd Font"
MIN_RUST="1.88"

with_font=1
with_lsp=1
with_skill=1
with_launcher=1
for arg in "$@"; do
    case "$arg" in
        --no-font) with_font=0 ;;
        --no-lsp) with_lsp=0 ;;
        --no-skill) with_skill=0 ;;
        --no-launcher) with_launcher=0 ;;
        -h | --help)
            echo "$usage"
            exit 0
            ;;
        *)
            echo "Unknown option: $arg (try --help)" >&2
            exit 2
            ;;
    esac
done

if [ -t 1 ]; then
    bold=$'\033[1m' green=$'\033[32m' yellow=$'\033[33m' red=$'\033[31m' reset=$'\033[0m'
else
    bold="" green="" yellow="" red="" reset=""
fi
step() { printf '\n%s==> %s%s\n' "$bold" "$*" "$reset"; }
ok() { printf '  %s✓%s %s\n' "$green" "$reset" "$*"; }
note() { printf '  %s!%s %s\n' "$yellow" "$reset" "$*"; }
die() {
    printf '%sError:%s %s\n' "$red" "$reset" "$*" >&2
    exit 1
}
have() { command -v "$1" >/dev/null 2>&1; }

if [ "$(id -u)" -eq 0 ]; then
    die "Run this as your own user, not root: Raido installs into your home directory and the script asks for sudo only for system packages."
fi

os="$(uname -s)"
case "$os" in
    Darwin | Linux) ;;
    *) die "This script supports macOS and Linux; on Windows run scripts/install.ps1 in PowerShell." ;;
esac

# Omarchy (https://omarchy.org): Arch Linux with its own commands for packages,
# themes and app launchers, which this script then uses.
omarchy=0
if [ "$os" = Linux ] && have omarchy && [ -d "${OMARCHY_PATH:-/usr/share/omarchy}" ]; then
    omarchy=1
fi

# Where `cargo install` puts programs.
cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
# Your terminal's PATH, before this script adds Cargo to its own: what `raido` has
# to be found on.
user_path="$PATH"

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

# --- System packages: compiler and linker, Git, curl, archive and font tools ---

macos_tools() {
    step "Xcode Command Line Tools (compiler, linker, Git)"
    if xcode-select -p >/dev/null 2>&1 && have cc && have git; then
        ok "installed ($(xcode-select -p))"
        return
    fi
    xcode-select --install >/dev/null 2>&1 || true
    die "macOS is installing the Command Line Tools in a separate window. When it finishes, run this script again."
}

linux_packages() {
    step "System packages (compiler, Git, curl, xz, fontconfig)"
    local missing=()
    have cc || missing+=(cc)
    have git || missing+=(git)
    have curl || missing+=(curl)
    have tar || missing+=(tar)
    have xz || missing+=(xz)
    if [ "$with_font" -eq 1 ]; then
        have fc-cache || missing+=(fontconfig)
    fi
    if [ "${#missing[@]}" -eq 0 ]; then
        ok "all present"
        return
    fi
    note "missing: ${missing[*]}"
    local sudo=""
    if have sudo; then
        sudo="sudo"
    else
        die "sudo is not available; install these yourself, then run this script again: ${missing[*]}"
    fi
    if [ "$omarchy" -eq 1 ]; then
        omarchy pkg add base-devel git curl tar xz fontconfig
    elif have apt-get; then
        $sudo apt-get update
        apt_updated=1
        $sudo apt-get install -y build-essential git curl ca-certificates tar xz-utils fontconfig
    elif have dnf; then
        $sudo dnf install -y gcc gcc-c++ make git curl tar xz fontconfig
    elif have yum; then
        $sudo yum install -y gcc gcc-c++ make git curl tar xz fontconfig
    elif have pacman; then
        $sudo pacman -S --needed --noconfirm base-devel git curl tar xz fontconfig
    elif have zypper; then
        $sudo zypper --non-interactive install gcc gcc-c++ make git curl tar xz fontconfig
    elif have apk; then
        $sudo apk add build-base git curl tar xz fontconfig
    else
        die "No supported package manager found (apt, dnf, yum, pacman, zypper, apk). Install: ${missing[*]}"
    fi
    ok "installed"
}

# --- Rust ---

version_at_least() {
    # version_at_least 1.99.1 1.88 → true
    [ "$(printf '%s\n%s\n' "$2" "$1" | sort -t. -k1,1n -k2,2n -k3,3n | head -n1)" = "$2" ]
}

install_rust() {
    step "Rust $MIN_RUST or newer"
    # Cargo from an earlier rustup install that this shell hasn't loaded yet.
    if ! have cargo && [ -f "$HOME/.cargo/env" ]; then
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
    fi
    if ! have cargo || ! have rustc; then
        note "installing rustup and the stable toolchain"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
            sh -s -- -y --profile minimal --default-toolchain stable
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
    fi
    local version
    version="$(rustc --version | awk '{print $2}')"
    if ! version_at_least "$version" "$MIN_RUST"; then
        if have rustup; then
            note "rustc $version is too old; updating stable"
            rustup update stable
            rustup default stable
            version="$(rustc --version | awk '{print $2}')"
        else
            die "rustc $version is older than $MIN_RUST and was not installed with rustup. Remove the distribution's Rust, or install rustup (https://rustup.rs), then run this script again."
        fi
    fi
    ok "rustc $version"
}

# --- Language servers: diagnostics, completion and hover in Raido's editor ---
#
# One for each language Raido knows: Rust, C and C++, Go, Python. Each comes from
# the system's own package manager where it has one (apt on Debian and Ubuntu, dnf
# on Fedora, pacman on Arch, zypper on openSUSE, apk on Alpine, Homebrew on macOS).
# Each is optional: one that can't be installed is noted and skipped, and Raido
# works without it.

apt_updated=0

# The system's package manager, or nothing.
package_manager() {
    if [ "$os" = Darwin ]; then
        if have brew; then echo brew; fi
        return
    fi
    local pm
    for pm in apt-get dnf yum pacman zypper apk; do
        if have "$pm"; then
            echo "$pm"
            return
        fi
    done
}

# "Arch Linux", "Ubuntu 24.04.1 LTS", "macOS 15.1".
system_name() {
    if [ "$os" = Darwin ]; then
        echo "macOS $(sw_vers -productVersion 2>/dev/null)"
    elif [ -r /etc/os-release ]; then
        (
            # shellcheck disable=SC1091
            . /etc/os-release
            echo "${PRETTY_NAME:-${NAME:-Linux}}"
        )
    else
        echo Linux
    fi
}

# Install system packages with $pm (sudo for all but Homebrew). Its output goes to
# $scratch/packages.log; false when it fails.
pm_install() {
    local sudo=() log="$scratch/packages.log"
    : >"$log"
    if [ "$pm" != brew ] && [ "$(id -u)" -ne 0 ]; then
        if ! have sudo; then
            echo "sudo is not available" >"$log"
            return 1
        fi
        sudo=(sudo)
        # Ask for the password here, not hidden in the log.
        sudo -v || return 1
    fi
    case "$pm" in
        apt-get)
            if [ "$apt_updated" -eq 0 ]; then
                "${sudo[@]}" apt-get update >>"$log" 2>&1 || return 1
                apt_updated=1
            fi
            "${sudo[@]}" env DEBIAN_FRONTEND=noninteractive apt-get install -y "$@" ;;
        dnf | yum) "${sudo[@]}" "$pm" install -y "$@" ;;
        # Omarchy's own: pacman --needed, then a check that each one is there.
        pacman) if [ "$omarchy" -eq 1 ]; then
            omarchy pkg add "$@"
        else
            "${sudo[@]}" pacman -S --needed --noconfirm "$@"
        fi ;;
        zypper) "${sudo[@]}" zypper --non-interactive install "$@" ;;
        apk) "${sudo[@]}" apk add "$@" ;;
        brew) brew install "$@" ;;
        *)
            echo "no supported package manager" >>"$log"
            return 1
            ;;
    esac >>"$log" 2>&1
}

# The last line of the package log: why an install failed.
why() { tail -n 1 "$scratch/packages.log" 2>/dev/null; }

# Folders Raido also finds a server in when it isn't on PATH (off_path in
# src/lsp.rs): go install's for gopls, shfmt and docker-language-server; Homebrew's
# LLVM and Apple's Command Line Tools for clangd on macOS.
off_path() {
    case "$1" in
        gopls | shfmt | docker-language-server)
            [ -n "${GOBIN:-}" ] && echo "$GOBIN"
            [ -n "${GOPATH:-}" ] && echo "${GOPATH%%:*}/bin"
            echo "$HOME/go/bin"
            ;;
        clangd)
            if [ "$os" = Darwin ]; then
                echo /opt/homebrew/opt/llvm/bin
                echo /usr/local/opt/llvm/bin
                echo /Library/Developer/CommandLineTools/usr/bin
                echo /Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin
            fi
            ;;
    esac
}

# Whether Raido can start PROGRAM: on PATH, or in one of its off_path folders.
found() {
    local dir
    have "$1" && return 0
    while IFS= read -r dir; do
        [ -x "$dir/$1" ] && return 0
    done < <(off_path "$1")
    return 1
}

# The package with PROGRAM in it, for $pm: package_for clangd.
package_for() {
    case "$1:$pm" in
        clangd:apt-get) echo clangd ;;
        clangd:dnf | clangd:yum) echo clang-tools-extra ;;
        clangd:pacman) echo clang ;;
        clangd:zypper) echo clang-tools ;;
        clangd:apk) echo clang-extra-tools ;;
        clangd:brew) echo llvm ;;
        gopls:dnf | gopls:yum) echo golang-x-tools-gopls ;;
        gopls:apt-get | gopls:pacman | gopls:zypper | gopls:apk | gopls:brew) echo gopls ;;
        python:apt-get) echo python3-pylsp ;;
        python:dnf | python:yum) echo python3-lsp-server ;;
        python:pacman | python:brew) echo pyright ;;
        python:zypper) echo python3-python-lsp-server ;;
        python:apk) echo py3-lsp-server ;;
        shellcheck:apt-get | shellcheck:pacman | shellcheck:apk | shellcheck:brew) echo shellcheck ;;
        shellcheck:dnf | shellcheck:yum | shellcheck:zypper) echo ShellCheck ;;
        shfmt:apt-get | shfmt:dnf | shfmt:yum | shfmt:pacman | shfmt:zypper | shfmt:apk | shfmt:brew) echo shfmt ;;
        bash-language-server:pacman | bash-language-server:brew) echo bash-language-server ;;
        bash-language-server:dnf) echo nodejs-bash-language-server ;;
    esac
}

# How $pm is run, for messages: "omarchy pkg add" on Omarchy.
pm_name() {
    if [ "$pm" = pacman ] && [ "$omarchy" -eq 1 ]; then
        echo "omarchy pkg add"
    else
        echo "$pm"
    fi
}

# Install PACKAGE with $pm, saying so; false when there's none or it fails.
from_packages() {
    local package="$1"
    [ -n "$package" ] || return 1
    note "installing $package with $(pm_name)"
    pm_install "$package"
}

# The Python language server Raido would start (the first of those it knows).
python_server() {
    local server
    for server in pyright-langserver basedpyright-langserver pylsp jedi-language-server; do
        if have "$server"; then
            echo "$server"
            return
        fi
    done
}

install_language_servers() {
    pm="$(package_manager)"
    step "Language servers for Rust, C/C++, Go, Python, shell and Docker ($(system_name)${pm:+, $(pm_name)})"
    if [ -z "$pm" ]; then
        if [ "$os" = Darwin ]; then
            note "Homebrew (https://brew.sh) is missing: only rust-analyzer and servers already here"
        else
            note "no supported package manager (apt, dnf, yum, pacman, zypper, apk): only rust-analyzer"
        fi
    fi

    # Rust: rustup's own. Its rust-analyzer command is there before the component
    # is, failing until it's added.
    if have rust-analyzer && rust-analyzer --version >/dev/null 2>&1; then
        ok "rust-analyzer (Rust)"
    elif have rustup && rustup component add rust-analyzer >/dev/null 2>&1; then
        ok "rust-analyzer (Rust)"
    else
        note "rust-analyzer could not be added; later: rustup component add rust-analyzer"
    fi

    # C and C++.
    if found clangd; then
        ok "clangd (C, C++)"
    elif from_packages "$(package_for clangd)" && found clangd; then
        ok "clangd (C, C++)"
    else
        note "clangd skipped${pm:+ ($(why))}; Raido works without it"
    fi

    # Go: gopls needs Go itself, so only where Go is installed.
    if found gopls; then
        ok "gopls (Go)"
    elif ! have go; then
        note "gopls skipped: Go isn't installed (install Go, then run this script again)"
    elif from_packages "$(package_for gopls)" && found gopls; then
        ok "gopls (Go)"
    elif note "installing gopls with go install" &&
        go install golang.org/x/tools/gopls@latest >"$scratch/packages.log" 2>&1 &&
        found gopls; then
        ok "gopls (Go)"
    else
        note "gopls skipped ($(why)); later: go install golang.org/x/tools/gopls@latest"
    fi

    # Python: pyright where the system has it, else the Python LSP Server; or, without
    # either, from uv, pipx or npm in your home folder.
    local python
    python="$(python_server)"
    if [ -z "$python" ] && from_packages "$(package_for python)"; then
        python="$(python_server)"
    fi
    if [ -z "$python" ] && have uv && note "installing basedpyright with uv" &&
        uv tool install basedpyright >"$scratch/packages.log" 2>&1; then
        python="$(python_server)"
    fi
    if [ -z "$python" ] && have pipx && note "installing python-lsp-server with pipx" &&
        pipx install python-lsp-server >"$scratch/packages.log" 2>&1; then
        python="$(python_server)"
    fi
    if [ -z "$python" ] && have npm && note "installing pyright with npm" &&
        npm install -g pyright >"$scratch/packages.log" 2>&1; then
        python="$(python_server)"
    fi
    if [ -n "$python" ]; then
        ok "$python (Python)"
    else
        note "no Python language server; later: npm install -g pyright, or pipx install python-lsp-server"
    fi

    # Shell scripts: the Bash language server, with ShellCheck for its warnings and
    # shfmt for :format (it quietly does without them). The tools come from the
    # system's packages, shfmt else from go install; the server from Homebrew, Arch or
    # Fedora, else npm.
    if found shellcheck; then
        ok "shellcheck (shell script warnings)"
    elif from_packages "$(package_for shellcheck)" && found shellcheck; then
        ok "shellcheck (shell script warnings)"
    else
        note "shellcheck skipped${pm:+ ($(why))}; shell scripts go without its warnings"
    fi
    if found shfmt; then
        ok "shfmt (shell script formatting)"
    elif from_packages "$(package_for shfmt)" && found shfmt; then
        ok "shfmt (shell script formatting)"
    elif have go && note "installing shfmt with go install" &&
        go install mvdan.cc/sh/v3/cmd/shfmt@latest >"$scratch/packages.log" 2>&1 &&
        found shfmt; then
        ok "shfmt (shell script formatting)"
    else
        note "shfmt skipped${pm:+ ($(why))}; later: go install mvdan.cc/sh/v3/cmd/shfmt@latest"
    fi
    if found bash-language-server; then
        ok "bash-language-server (shell scripts)"
    elif from_packages "$(package_for bash-language-server)" && found bash-language-server; then
        ok "bash-language-server (shell scripts)"
    elif have npm && note "installing bash-language-server with npm" &&
        npm install -g bash-language-server >"$scratch/packages.log" 2>&1 &&
        found bash-language-server; then
        ok "bash-language-server (shell scripts)"
    else
        note "bash-language-server skipped; later: npm install -g bash-language-server (Node.js 20 or later)"
    fi

    # Docker: Docker's own server, for Dockerfiles and Compose files. No package has
    # it, so go install builds it (fetching a newer Go when it needs one): only where
    # Go is installed, and only where Docker (or Podman) is, as it's a long build.
    local docker_server=github.com/docker/docker-language-server/cmd/docker-language-server@latest
    if found docker-language-server; then
        ok "docker-language-server (Dockerfile, Docker Compose)"
    elif ! have docker && ! have podman; then
        note "docker-language-server skipped: Docker isn't installed (Raido highlights Dockerfiles without it)"
    elif ! have go; then
        note "docker-language-server skipped: it's built with Go (install Go, then run this script again)"
    elif note "installing docker-language-server with go install (a few minutes)" &&
        go install "$docker_server" >"$scratch/packages.log" 2>&1 &&
        found docker-language-server; then
        ok "docker-language-server (Dockerfile, Docker Compose)"
    else
        note "docker-language-server skipped ($(why)); later: go install $docker_server"
    fi
}

# --- JetBrains Mono Nerd Font ---

font_installed() {
    if [ "$os" = Darwin ]; then
        compgen -G "$HOME/Library/Fonts/JetBrainsMonoNerdFont*" >/dev/null ||
            compgen -G "/Library/Fonts/JetBrainsMonoNerdFont*" >/dev/null
    else
        have fc-list && fc-list : family | grep -qi "$FONT_FAMILY"
    fi
}

# The font also carries the Nerd Font icons Raido's explorer shows by default
# (icons = true picks them as soon as a Nerd Font is installed).
install_font() {
    step "$FONT_FAMILY (text and explorer icons)"
    if font_installed; then
        ok "already installed; Raido's explorer uses its Nerd Font icons"
        return
    fi
    if [ "$os" = Darwin ] && have brew; then
        brew install --cask font-jetbrains-mono-nerd-font
    else
        note "downloading the latest Nerd Fonts release"
        curl -fsSL --proto '=https' --tlsv1.2 -o "$scratch/font.tar.xz" "$FONT_URL"
        mkdir -p "$scratch/font"
        tar -xJf "$scratch/font.tar.xz" -C "$scratch/font"
        local target
        if [ "$os" = Darwin ]; then
            target="$HOME/Library/Fonts"
        else
            target="${XDG_DATA_HOME:-$HOME/.local/share}/fonts/JetBrainsMonoNerdFont"
        fi
        mkdir -p "$target"
        find "$scratch/font" -name '*.ttf' -exec cp {} "$target/" \;
        if [ "$os" = Linux ]; then
            fc-cache -f "$target" >/dev/null
        fi
    fi
    if font_installed; then
        ok "installed; Raido's explorer uses its Nerd Font icons"
    else
        note "installed, but not visible yet; log out and back in if your terminal doesn't list it"
    fi
}

# --- Raido ---

source_dir() {
    # The checkout holding this script, else the current directory, else a fresh clone.
    local script="${BASH_SOURCE[0]:-}" candidates=() dir
    if [ -n "$script" ] && [ -f "$script" ]; then
        candidates+=("$(cd "$(dirname "$script")/.." && pwd)")
    fi
    candidates+=("$PWD")
    for dir in "${candidates[@]}"; do
        if grep -qs '^name = "raido"' "$dir/Cargo.toml"; then
            printf '%s\n' "$dir"
            return
        fi
    done
    git clone --quiet --depth 1 "$REPO_URL" "$scratch/raido" >&2
    printf '%s\n' "$scratch/raido"
}

# The checkout Raido was built from (its icon comes from there too).
raido_src=""

install_raido() {
    step "Raido"
    local src
    src="$(source_dir)"
    raido_src="$src"
    note "building from $src (the first build takes a few minutes)"
    cargo install --path "$src" --locked --force
    local bin="$cargo_bin/raido"
    [ -x "$bin" ] || die "cargo finished but $bin is missing"
    ok "$("$bin" --version) at $bin"
}

# Raido was called Odin: an install under that name (its `odin` command and the
# link this script made to it) goes, so only Raido stays.
retire_odin() {
    if cargo install --list 2>/dev/null | grep -q '^odin-editor '; then
        cargo uninstall odin-editor >/dev/null 2>&1 &&
            ok "removed the old odin command (Raido replaces it)"
    fi
    local dir
    for dir in "$HOME/.local/bin" "$HOME/bin" /opt/homebrew/bin /usr/local/bin; do
        if [ -L "$dir/odin" ] && [ "$(readlink "$dir/odin")" = "$cargo_bin/odin" ]; then
            rm -f "$dir/odin" && ok "removed the old link $dir/odin"
        fi
    done
}

# The real file behind a path (symlinks followed), for comparing two `raido`s.
real_path() {
    local path="$1" link
    while link="$(readlink "$path")"; do
        case "$link" in
            /*) path="$link" ;;
            *) path="$(dirname "$path")/$link" ;;
        esac
    done
    printf '%s/%s\n' "$(cd "$(dirname "$path")" 2>/dev/null && pwd -P)" "$(basename "$path")"
}

# Make `raido` runnable by name: right away in this terminal when a folder already on
# PATH can take a link to it, and in every new terminal either way.
put_on_path() {
    step "raido on your PATH"
    local bin="$cargo_bin/raido" found dir
    found="$(PATH="$user_path" command -v raido 2>/dev/null || true)"
    if [ -n "$found" ] && [ "$(real_path "$found")" = "$(real_path "$bin")" ]; then
        ok "ready: raido runs from $found"
        return
    fi
    if [ -n "$found" ]; then
        note "another raido comes first on PATH: $found"
        note "run this Raido as $bin, or put $cargo_bin before that folder in PATH"
        return
    fi
    # A folder already on PATH that we may write to without sudo.
    for dir in "$HOME/.local/bin" "$HOME/bin" /opt/homebrew/bin /usr/local/bin; do
        case ":$user_path:" in
            *":$dir:"*) ;;
            *) continue ;;
        esac
        [ -d "$dir" ] && [ -w "$dir" ] || continue
        # Never replace a file that isn't our own link.
        if [ -e "$dir/raido" ] && [ ! -L "$dir/raido" ]; then
            continue
        fi
        ln -sf "$bin" "$dir/raido"
        ok "linked $dir/raido → $bin; raido runs in this terminal now"
        return
    done
    add_to_profile
}

# Put Cargo's bin folder on PATH for new terminals, in the startup file of your
# shell; once, marked so a second run doesn't add it again.
add_to_profile() {
    local marker="# Added by Raido's installer: programs installed with cargo"
    local line="export PATH=\"$cargo_bin:\$PATH\""
    local shell_name files=() file candidate
    shell_name="$(basename "${SHELL:-sh}")"
    case "$shell_name" in
        zsh) files=("${ZDOTDIR:-$HOME}/.zshrc") ;;
        bash)
            # Linux terminals read ~/.bashrc. macOS ones start login shells, which read
            # only the first of these that exists; a new ~/.bash_profile would hide
            # an existing ~/.profile.
            if [ "$os" = Darwin ]; then
                file="$HOME/.bash_profile"
                for candidate in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
                    if [ -f "$candidate" ]; then
                        file="$candidate"
                        break
                    fi
                done
                files=("$file")
            else
                files=("$HOME/.bashrc")
            fi
            ;;
        fish)
            file="${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/raido.fish"
            mkdir -p "$(dirname "$file")"
            printf '%s\nfish_add_path -g %s\n' "$marker" "$cargo_bin" >"$file"
            ok "added $cargo_bin to fish's PATH ($file)"
            note "open a new terminal to run raido by name (or run $bin now)"
            return
            ;;
        *) files=("$HOME/.profile") ;;
    esac
    for file in "${files[@]}"; do
        # Ours from an earlier run, or rustup's own line.
        if grep -qsF -e "$marker" -e ".cargo/env" -e "$cargo_bin" "$file"; then
            ok "$file already adds $cargo_bin to PATH"
        else
            printf '\n%s\n%s\n' "$marker" "$line" >>"$file"
            ok "added $cargo_bin to PATH in $file"
        fi
    done
    note "open a new terminal to run raido by name, or run now: export PATH=\"$cargo_bin:\$PATH\""
}

# Raido's docs as an Agent Skills skill, so Claude Code, Codex, Gemini CLI and others
# can answer "how do I … in Raido?" in any project. Optional: Raido works without it,
# and a failure here never fails the install.
install_skill() {
    step "Raido skill for AI agents"
    local log="$scratch/skill.log"
    if "$cargo_bin/raido" --install-skill >"$log" 2>&1; then
        ok "installed for this user (every project): $(grep -c '/raido$' "$log") place(s)"
    else
        note "skipped ($(tail -n 1 "$log")); run raido --install-skill later"
    fi
}

# --- Omarchy: the desktop's theme and an app launcher entry ---

omarchy_setup() {
    step "Omarchy"
    # Follow the desktop's theme, as Omarchy's own editors do: only for a new
    # config, so a theme you picked stays.
    local config="${XDG_CONFIG_HOME:-$HOME/.config}/raido/config.toml" theme
    local line='theme = "omarchy"'
    local why="# Follows the Omarchy theme: omarchy theme set <name> re-themes Raido too."
    theme=""
    if [ -f "$config" ]; then
        theme="$(sed -n 's/^theme *= *"\(.*\)".*/\1/p' "$config" | head -n 1)"
    fi
    if [ "$theme" = omarchy ]; then
        ok "Raido follows your Omarchy theme"
    elif [ -n "$theme" ]; then
        note "Raido keeps the theme you chose ($theme); :theme omarchy follows Omarchy's"
    else
        # A new config, or one without a theme: the line goes first, where TOML
        # keeps top-level settings (after a [table] it would belong to that).
        mkdir -p "$(dirname "$config")"
        {
            printf '%s\n%s\n' "$why" "$line"
            if [ -s "$config" ]; then
                printf '\n'
                cat "$config"
            fi
        } >"$scratch/config.toml"
        cp "$scratch/config.toml" "$config"
        ok "Raido follows your Omarchy theme ($line in $config)"
    fi
    if [ "$with_launcher" -eq 0 ]; then
        return
    fi
    # Like any TUI Omarchy installs: SUPER + SPACE finds it, and it opens in your
    # default terminal, tiled.
    local icon="$raido_src/docs/raido-icon.png"
    [ -f "$icon" ] || icon=utilities-terminal
    if omarchy tui install Raido "$cargo_bin/raido" tile "$icon" >"$scratch/launcher.log" 2>&1; then
        ok "in the app launcher: SUPER + SPACE, then Raido (omarchy tui remove Raido takes it out)"
    else
        note "no launcher entry ($(tail -n 1 "$scratch/launcher.log")); --no-launcher skips this"
    fi
}

summary() {
    step "Done"
    echo "  Start Raido in any project:  raido ~/path/to/project"
    if [ "$omarchy" -eq 1 ] && [ "$with_launcher" -eq 1 ]; then
        echo "  Or from the app launcher: SUPER + SPACE, then Raido."
    fi
    if [ "$with_font" -eq 1 ]; then
        if [ "$omarchy" -eq 1 ]; then
            local font
            font="$(omarchy font current 2>/dev/null || true)"
            if [[ "$font" == JetBrainsMono* ]]; then
                echo "  Font: your terminals use $font already (Omarchy's default)."
            else
                echo "  Font: omarchy font set \"$FONT_FAMILY\", or inside Raido :font family \"$FONT_FAMILY\"."
            fi
        elif [ "$os" = Darwin ] && [ "${TERM_PROGRAM:-}" = Apple_Terminal ]; then
            echo "  macOS Terminal: Raido switches its tab to $FONT_FAMILY Mono at 14 pt while it runs."
        else
            echo "  Font: make \"$FONT_FAMILY Mono\" your terminal's font in its settings,"
            echo "  or inside Raido run :font family \"$FONT_FAMILY Mono\" (foot, Ghostty, Kitty, Alacritty)."
        fi
    fi
}

if [ "$os" = Darwin ]; then
    macos_tools
else
    linux_packages
fi
install_rust
if [ "$with_lsp" -eq 1 ]; then
    install_language_servers
fi
if [ "$with_font" -eq 1 ]; then
    install_font
fi
install_raido
retire_odin
put_on_path
if [ "$omarchy" -eq 1 ]; then
    omarchy_setup
fi
if [ "$with_skill" -eq 1 ]; then
    install_skill
fi
summary
