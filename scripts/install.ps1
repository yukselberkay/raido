<#
.SYNOPSIS
Build and install Raido on Windows with everything it needs: Git, the Visual Studio
C++ build tools (Rust's linker), Rust (via rustup), rust-analyzer, JetBrains Mono
Nerd Font, Raido itself (%USERPROFILE%\.cargo\bin\raido.exe, put on your PATH), and
the Raido skill that teaches AI agents how to use Raido.

.DESCRIPTION
Run it from a checkout:
    powershell -ExecutionPolicy Bypass -File scripts\install.ps1
or straight from GitHub (outside a checkout it clones Raido first):
    irm https://raw.githubusercontent.com/yukselberkay/raido/main/scripts/install.ps1 | iex
With options from GitHub:
    & ([scriptblock]::Create((irm https://raw.githubusercontent.com/yukselberkay/raido/main/scripts/install.ps1))) -NoFont
Safe to run again: anything already installed is kept as is. Programs come from
winget (App Installer, built into Windows 10 and 11).

.PARAMETER NoFont
Skip JetBrains Mono Nerd Font.
.PARAMETER NoLsp
Skip rust-analyzer.
.PARAMETER NoSkill
Skip the Raido skill for AI agents.
#>
param(
    [switch]$NoFont,
    [switch]$NoLsp,
    [switch]$NoSkill
)

$ErrorActionPreference = 'Stop'
# Windows PowerShell 5.1: GitHub needs TLS 1.2, and the progress bar makes downloads crawl.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
$ProgressPreference = 'SilentlyContinue'

$RepoUrl = 'https://github.com/yukselberkay/raido.git'
$FontUrl = 'https://github.com/ryanoasis/nerd-fonts/releases/latest/download/JetBrainsMono.zip'
$FontFamily = 'JetBrainsMono Nerd Font'
$MinRust = [version]'1.88'
$CargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$CargoBin = Join-Path $CargoHome 'bin'
$Scratch = Join-Path ([IO.Path]::GetTempPath()) ("raido-install-" + [guid]::NewGuid().ToString('N'))

function Step([string]$Text) { Write-Host ''; Write-Host "==> $Text" -ForegroundColor White }
function Ok([string]$Text) { Write-Host "  $([char]0x2713) $Text" -ForegroundColor Green }
function Note([string]$Text) { Write-Host "  ! $Text" -ForegroundColor Yellow }
function Have([string]$Name) { [bool](Get-Command $Name -ErrorAction SilentlyContinue) }

# PATH as Windows has it now (machine, then user, then Cargo's), so programs that
# winget installed a moment ago are found in this window.
function Update-SessionPath {
    $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $user = [Environment]::GetEnvironmentVariable('Path', 'User')
    $env:Path = (@($machine, $user, $CargoBin) | Where-Object { $_ }) -join ';'
}

function Install-WithWinget([string]$Id, [string]$What, [string[]]$Extra = @()) {
    if (-not (Have 'winget')) {
        throw "winget (App Installer, from the Microsoft Store) is missing; install $What yourself, then run this script again."
    }
    Note "installing $What with winget (Windows may ask for permission)"
    & winget install --id $Id --exact --source winget --silent --accept-package-agreements --accept-source-agreements @Extra
    # Installed, but Windows wants a restart to finish (the installer's 3010 / 1641, or
    # winget's own REBOOT_REQUIRED codes): fine for now.
    $restart = @(3010, 1641, -1978334967, -1978334966)
    if ($restart -contains $LASTEXITCODE) {
        Note "$What is installed; restart Windows when convenient to finish"
    } elseif ($LASTEXITCODE -ne 0) {
        throw "winget could not install $What (exit code $LASTEXITCODE)"
    }
    Update-SessionPath
}

# --- Git ---

function Install-Git {
    Step 'Git'
    if (Have 'git') { Ok (& git --version); return }
    Install-WithWinget 'Git.Git' 'Git'
    if (-not (Have 'git')) { throw 'Git was installed but is not on PATH yet; open a new terminal and run this script again.' }
    Ok (& git --version)
}

# --- The C++ build tools: Rust's MSVC toolchain links with them ---

function Test-BuildTools {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) { return $false }
    $component = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
        'Microsoft.VisualStudio.Component.VC.Tools.ARM64'
    } else {
        'Microsoft.VisualStudio.Component.VC.Tools.x86.x64'
    }
    $found = & $vswhere -latest -products * -requires $component -property installationPath
    return [bool]$found
}

function Install-BuildTools {
    Step 'Visual Studio C++ build tools (linker for Rust)'
    if (Test-BuildTools) { Ok 'installed'; return }
    Note 'this is a large download (a few GB) and takes a while'
    Install-WithWinget 'Microsoft.VisualStudio.2022.BuildTools' 'the Visual Studio C++ build tools' @(
        '--override', '--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended'
    )
    if (-not (Test-BuildTools)) { throw 'The C++ build tools did not finish installing; run this script again once the Visual Studio Installer is done.' }
    Ok 'installed'
}

# --- Rust ---

function Get-RustVersion {
    $text = & rustc --version
    if ($text -match 'rustc (\d+\.\d+(\.\d+)?)') { return [version]$Matches[1] }
    return [version]'0.0'
}

function Install-Rust {
    Step "Rust $MinRust or newer"
    Update-SessionPath
    if (-not ((Have 'cargo') -and (Have 'rustc'))) {
        Install-WithWinget 'Rustlang.Rustup' 'Rust (rustup)'
        if (-not (Have 'rustup')) { throw 'rustup was installed but is not on PATH yet; open a new terminal and run this script again.' }
        & rustup default stable
        if ($LASTEXITCODE -ne 0) { throw 'rustup could not install the stable toolchain' }
    }
    $version = Get-RustVersion
    if ($version -lt $MinRust) {
        if (-not (Have 'rustup')) { throw "rustc $version is older than $MinRust and was not installed with rustup; install rustup (https://rustup.rs), then run this script again." }
        Note "rustc $version is too old; updating stable"
        & rustup update stable
        & rustup default stable
        $version = Get-RustVersion
    }
    Ok "rustc $version"
    if (-not $NoLsp -and (Have 'rustup')) {
        # Rust language support in Raido's editor (diagnostics, hover, completion).
        # Quietly; Windows PowerShell 5.1 would stop on its progress lines otherwise.
        & { $ErrorActionPreference = 'Continue'; & rustup component add rust-analyzer *> $null }
        if ($LASTEXITCODE -eq 0) { Ok 'rust-analyzer' } else { Note "rust-analyzer could not be added; Raido works without it" }
    }
}

# --- JetBrains Mono Nerd Font (text and explorer icons) ---

function Test-Font {
    $fonts = 'HKCU:\Software\Microsoft\Windows NT\CurrentVersion\Fonts', 'HKLM:\Software\Microsoft\Windows NT\CurrentVersion\Fonts'
    foreach ($key in $fonts) {
        $entries = Get-ItemProperty -Path $key -ErrorAction SilentlyContinue
        if ($entries -and ($entries.PSObject.Properties.Name -like "$FontFamily*")) { return $true }
    }
    return $false
}

function Install-Font {
    Step "$FontFamily (text and explorer icons)"
    if (Test-Font) { Ok "already installed; Raido's explorer uses its Nerd Font icons"; return }
    Note 'downloading the latest Nerd Fonts release'
    $zip = Join-Path $Scratch 'font.zip'
    $unpacked = Join-Path $Scratch 'font'
    Invoke-WebRequest -Uri $FontUrl -OutFile $zip -UseBasicParsing
    Expand-Archive -Path $zip -DestinationPath $unpacked -Force
    # Per-user fonts: no administrator rights needed (Windows 10 1809 and later).
    $target = Join-Path $env:LOCALAPPDATA 'Microsoft\Windows\Fonts'
    New-Item -ItemType Directory -Force -Path $target | Out-Null
    $registry = 'HKCU:\Software\Microsoft\Windows NT\CurrentVersion\Fonts'
    New-Item -Path $registry -Force -ErrorAction SilentlyContinue | Out-Null
    foreach ($file in Get-ChildItem -Path $unpacked -Recurse -Filter '*.ttf') {
        $dest = Join-Path $target $file.Name
        Copy-Item -Path $file.FullName -Destination $dest -Force
        $name = ($file.BaseName -creplace 'NerdFont', ' Nerd Font ' -creplace '-', ' ' -replace '\s+', ' ').Trim()
        New-ItemProperty -Path $registry -Name "$name (TrueType)" -Value $dest -PropertyType String -Force | Out-Null
    }
    Ok 'installed; set "JetBrainsMono Nerd Font Mono" as your terminal font (see below)'
}

# --- Raido ---

function Get-SourceDir {
    # The checkout holding this script, else the current directory, else a fresh clone.
    $candidates = @()
    if ($PSScriptRoot) { $candidates += (Split-Path -Parent $PSScriptRoot) }
    $candidates += (Get-Location).Path
    foreach ($dir in $candidates) {
        $manifest = Join-Path $dir 'Cargo.toml'
        if ((Test-Path $manifest) -and (Select-String -Path $manifest -Pattern '^name = "raido"' -Quiet)) {
            return $dir
        }
    }
    $clone = Join-Path $Scratch 'raido'
    & git clone --quiet --depth 1 $RepoUrl $clone
    if ($LASTEXITCODE -ne 0) { throw "git could not clone $RepoUrl" }
    return $clone
}

function Install-Raido {
    Step 'Raido'
    $src = Get-SourceDir
    Note "building from $src (the first build takes a few minutes)"
    & cargo install --path $src --locked --force
    if ($LASTEXITCODE -ne 0) { throw 'cargo could not build Raido' }
    $exe = Join-Path $CargoBin 'raido.exe'
    if (-not (Test-Path $exe)) { throw "cargo finished but $exe is missing" }
    Ok "$(& $exe --version) at $exe"
}

# Raido was called Odin: an install under that name (odin.exe) goes, so only Raido
# stays.
function Remove-Odin {
    $list = & cargo install --list | Out-String
    if ($list -match '(?m)^odin-editor ') {
        & cargo uninstall odin-editor | Out-Null
        if ($LASTEXITCODE -eq 0) { Ok 'removed the old odin command (Raido replaces it)' }
    }
}

# Make `raido` runnable by name: in this window now, and in every new one through
# your user PATH. The registry value is edited as stored, so %VARIABLES% in other
# entries stay as they are.
function Set-RaidoPath {
    Step 'raido on your PATH'
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
    try {
        $raw = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        $entries = @($raw -split ';' | Where-Object { $_ })
        $expanded = $entries | ForEach-Object { [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') }
        if ($expanded -contains $CargoBin.TrimEnd('\')) {
            Ok "$CargoBin is on your PATH"
        } else {
            $key.SetValue('Path', ((@($CargoBin) + $entries) -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString)
            # Tell Windows the environment changed, so new windows see it.
            [Environment]::SetEnvironmentVariable('RAIDO_INSTALL_REFRESH', '1', 'User')
            [Environment]::SetEnvironmentVariable('RAIDO_INSTALL_REFRESH', $null, 'User')
            Ok "added $CargoBin to your PATH"
        }
    } finally {
        $key.Close()
    }
    if (-not (($env:Path -split ';') -contains $CargoBin)) { $env:Path = "$CargoBin;$env:Path" }
    $found = Get-Command raido -ErrorAction SilentlyContinue | Select-Object -First 1
    $ours = Join-Path $CargoBin 'raido.exe'
    if ($found -and $found.Source -and ($found.Source -ne $ours)) {
        Note "another raido comes first on PATH: $($found.Source)"
        Note "run this Raido as $ours, or move $CargoBin before that folder in PATH"
    } else {
        Ok 'raido runs in this window now, and in every new terminal'
    }
}

# Raido's docs as an Agent Skills skill, so Claude Code, Codex, Gemini CLI and others
# can answer "how do I ... in Raido?" in any project. Optional: Raido works without it,
# and a failure here never fails the install.
function Install-Skill {
    Step 'Raido skill for AI agents'
    # Its messages are captured, not errors (see Install-Rust).
    $ErrorActionPreference = 'Continue'
    $output = & (Join-Path $CargoBin 'raido.exe') --install-skill 2>&1 | Out-String
    if ($LASTEXITCODE -eq 0) {
        $places = @($output -split "`n" | Where-Object { $_ -match '[\\/]raido\s*$' }).Count
        Ok "installed for this user (every project): $places place(s)"
    } else {
        Note "skipped ($($output.Trim())); run raido --install-skill later"
    }
}

function Show-Summary {
    Step 'Done'
    Write-Host '  Start Raido in any project:  raido C:\path\to\project'
    Write-Host '  Use Windows Terminal for full color and keys.'
    if (-not $NoFont) {
        Write-Host "  Font: Windows Terminal > Settings > Profiles > Defaults > Appearance >"
        Write-Host "  Font face: `"$FontFamily Mono`"."
    }
}

New-Item -ItemType Directory -Force -Path $Scratch | Out-Null
$failed = $false
try {
    if (-not $IsWindows -and $PSVersionTable.PSEdition -eq 'Core' -and $null -ne $IsWindows) {
        throw 'This script is for Windows; on macOS and Linux run scripts/install.sh.'
    }
    Install-Git
    Install-BuildTools
    Install-Rust
    if (-not $NoFont) { Install-Font }
    Install-Raido
    Remove-Odin
    Set-RaidoPath
    if (-not $NoSkill) { Install-Skill }
    Show-Summary
} catch {
    Write-Host "Error: $($_.Exception.Message)" -ForegroundColor Red
    $failed = $true
} finally {
    Remove-Item -Recurse -Force -Path $Scratch -ErrorAction SilentlyContinue
}
# A script file fails the run; pasted into a window (irm | iex), keep the window.
if ($failed -and $PSCommandPath) { exit 1 }
