#!/usr/bin/env bash
# Build and install Raido with everything it needs on this machine (macOS or Linux):
# a C compiler and linker, Git, curl, Rust (via rustup), rust-analyzer,
# JetBrains Mono Nerd Font, Raido itself (~/.cargo/bin/raido, made runnable by name
# from PATH), and the Raido skill that teaches AI agents how to use Raido.
# Windows: scripts/install.ps1.
#
# Run it from a checkout (scripts/install.sh) or piped from GitHub
# (curl -fsSL …/scripts/install.sh | bash); outside a checkout it clones Raido
# first. Safe to run again: anything already installed is kept as is.
set -euo pipefail

usage="Usage: install.sh [--no-font] [--no-lsp] [--no-skill]
Installs a C compiler, Git, curl, Rust, rust-analyzer, JetBrains Mono Nerd Font,
Raido and its AI agent skill on macOS or Linux.
  --no-font   skip JetBrains Mono Nerd Font
  --no-lsp    skip rust-analyzer
  --no-skill  skip the Raido skill for AI agents"

REPO_URL="https://github.com/yukselberkay/raido.git"
FONT_URL="https://github.com/ryanoasis/nerd-fonts/releases/latest/download/JetBrainsMono.tar.xz"
FONT_FAMILY="JetBrainsMono Nerd Font"
MIN_RUST="1.88"

with_font=1
with_lsp=1
with_skill=1
for arg in "$@"; do
    case "$arg" in
        --no-font) with_font=0 ;;
        --no-lsp) with_lsp=0 ;;
        --no-skill) with_skill=0 ;;
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
    if have apt-get; then
        $sudo apt-get update
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
    if [ "$with_lsp" -eq 1 ] && have rustup; then
        # Rust language support in Raido's editor (diagnostics, hover, completion).
        if rustup component add rust-analyzer >/dev/null 2>&1; then
            ok "rust-analyzer"
        else
            note "rust-analyzer could not be added; Raido works without it"
        fi
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

install_raido() {
    step "Raido"
    local src
    src="$(source_dir)"
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

summary() {
    step "Done"
    echo "  Start Raido in any project:  raido ~/path/to/project"
    if [ "$with_font" -eq 1 ]; then
        if [ "$os" = Darwin ] && [ "${TERM_PROGRAM:-}" = Apple_Terminal ]; then
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
if [ "$with_font" -eq 1 ]; then
    install_font
fi
install_raido
retire_odin
put_on_path
if [ "$with_skill" -eq 1 ]; then
    install_skill
fi
summary
