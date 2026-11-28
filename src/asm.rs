//! Assembly files: which CPU one is written for, x86, ARM or RISC-V, so it's
//! highlighted with that CPU's registers and instructions; how it's written (GNU
//! as, NASM / MASM, Go's assembler); and the mark that comments a line out in it.
//!
//! `.s`, `.S` and `.asm` don't say which CPU they're for, so it's read in turn from
//! the project's `.asm-lsp.toml` (asm-lsp's settings, which name the instruction
//! set), the file's name (`memmove_arm64.s`, `start-riscv.S`), the registers and
//! instructions in it (`%rax`, `x0`, `a0`), the folders it's in (`arch/riscv/`),
//! and last the CPU Raido runs on.
use crate::editor::Buffer;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Arch {
    /// x86 and x86-64.
    X86,
    /// 32-bit ARM and Thumb.
    Arm,
    /// AArch64.
    Arm64,
    RiscV,
}

impl Arch {
    /// The grammar Raido highlights it with.
    pub fn grammar(self) -> &'static str {
        match self {
            Self::X86 => "Assembly (x86)",
            Self::Arm => "Assembly (ARM)",
            Self::Arm64 => "Assembly (AArch64)",
            Self::RiscV => "Assembly (RISC-V)",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::X86 => "x86",
            Self::Arm => "32-bit ARM",
            Self::Arm64 => "AArch64",
            Self::RiscV => "RISC-V",
        }
    }
    /// What asm-lsp's `instruction_set` calls it.
    pub fn instruction_set(self) -> &'static str {
        match self {
            Self::X86 => "x86/x86-64",
            Self::Arm => "arm",
            Self::Arm64 => "arm64",
            Self::RiscV => "riscv",
        }
    }
    /// The CPU Raido runs on, when it's one of these.
    pub fn host() -> Option<Self> {
        if cfg!(any(target_arch = "x86_64", target_arch = "x86")) {
            Some(Self::X86)
        } else if cfg!(target_arch = "aarch64") {
            Some(Self::Arm64)
        } else if cfg!(target_arch = "arm") {
            Some(Self::Arm)
        } else if cfg!(any(target_arch = "riscv64", target_arch = "riscv32")) {
            Some(Self::RiscV)
        } else {
            None
        }
    }
    fn from_instruction_set(set: &str) -> Option<Self> {
        match set.trim().to_ascii_lowercase().as_str() {
            "x86" | "x86-64" | "x86_64" | "x86/x86-64" => Some(Self::X86),
            "arm" => Some(Self::Arm),
            "arm64" | "aarch64" => Some(Self::Arm64),
            "riscv" | "risc-v" => Some(Self::RiscV),
            _ => None,
        }
    }
    /// The CPU a word in a file or folder name means: `amd64`, `aarch64`, `rv64gc`.
    fn from_word(word: &str) -> Option<Self> {
        Some(match word {
            "x86" | "x64" | "amd64" | "x8664" | "i386" | "i486" | "i586" | "i686" | "ia32"
            | "386" => Self::X86,
            "arm64" | "aarch64" | "arm64e" | "armv8" | "armv9" => Self::Arm64,
            "arm" | "arm32" | "aarch32" | "armv4" | "armv5" | "armv6" | "armv6m" | "armv7"
            | "armv7a" | "armv7m" | "armv7em" | "armhf" | "armel" | "thumb" | "thumb2"
            | "cortex" => Self::Arm,
            "riscv" | "riscv32" | "riscv64" => Self::RiscV,
            _ if ["rv32", "rv64"].iter().any(|isa| {
                word.strip_prefix(isa)
                    .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_lowercase()))
            }) =>
            {
                Self::RiscV
            }
            _ => return None,
        })
    }
}

/// How a file is written, which asm-lsp has to be told unless it's GNU as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Flavor {
    /// GNU as and Clang: AT&T or Intel syntax on x86, the usual on ARM and RISC-V.
    Gas,
    /// NASM, YASM or MASM.
    Intel,
    /// Go's assembler (Plan 9 syntax).
    Go,
}

/// What Raido makes of an assembly file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Asm {
    pub arch: Arch,
    pub flavor: Flavor,
}

/// An assembly file by its extension: `.s`, `.S`, `.asm`, `.sx`, `.nasm`, `.yasm`.
pub fn is_assembly(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "s" | "asm" | "sx" | "nasm" | "yasm"
        )
    })
}

/// How much of a file is read for its CPU and flavor.
const SAMPLE_LINES: usize = 400;

/// The start of `buffer`'s text, enough to tell its CPU by.
pub fn sample(buffer: &Buffer) -> String {
    let lines = buffer.lines().min(SAMPLE_LINES);
    let end = buffer.text.line_to_char(lines);
    buffer.text.slice(..end).to_string()
}

/// The assembly file at `path`, when it's one, with `text` (all of it or its start).
pub fn read(path: &Path, text: &str) -> Option<Asm> {
    if !is_assembly(path) {
        return None;
    }
    let arch = arch(path, text);
    Some(Asm {
        arch,
        flavor: flavor(path, text, arch),
    })
}

/// `read` for an open buffer, remembered for each of its revisions: the status bar
/// asks at every frame.
pub fn of(buffer: &Buffer) -> Option<Asm> {
    static SEEN: Mutex<Option<HashMap<PathBuf, (u64, Asm)>>> = Mutex::new(None);
    let path = buffer.path.as_deref().filter(|p| is_assembly(p))?;
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let seen = seen.get_or_insert_with(HashMap::new);
    if let Some((revision, asm)) = seen.get(path) {
        if *revision == buffer.revision {
            return Some(*asm);
        }
    }
    let asm = read(path, &sample(buffer))?;
    seen.insert(path.to_path_buf(), (buffer.revision, asm));
    Some(asm)
}

/// The CPU the assembly at `path` is written for.
fn arch(path: &Path, text: &str) -> Arch {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    configured(path)
        .or_else(|| matches!(extension.as_str(), "nasm" | "yasm").then_some(Arch::X86))
        .or_else(|| {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            from_name(stem)
        })
        .or_else(|| from_text(text))
        .or_else(|| {
            // The nearest folders only: arch/arm64/kernel/, not a home folder's name.
            let folders: Vec<_> = path.parent()?.components().collect();
            folders
                .iter()
                .rev()
                .take(4)
                .find_map(|folder| from_name(&folder.as_os_str().to_string_lossy()))
        })
        .or_else(Arch::host)
        .unwrap_or(Arch::X86)
}

/// The CPU a file or folder name says: `memmove_arm64`, `start-riscv`, `x86_64`.
fn from_name(name: &str) -> Option<Arch> {
    let name = name.to_ascii_lowercase();
    if name.contains("risc-v") || name.contains("risc_v") {
        return Some(Arch::RiscV);
    }
    name.split(['_', '-', '.', ' '])
        .find_map(Arch::from_word)
        .or_else(|| name.contains("x86_64").then_some(Arch::X86))
}

/// The instruction set asm-lsp's settings give the file at `path`: the nearest
/// `.asm-lsp.toml` above it, up to the folder with `.git` in it, as asm-lsp reads
/// it: the `[[project]]` it's in, else `[default_config]`.
pub fn configured(path: &Path) -> Option<Arch> {
    let path = absolute(path);
    let mut folder = path.parent();
    for _ in 0..32 {
        let dir = folder?;
        let settings = dir.join(".asm-lsp.toml");
        if settings.is_file() {
            return in_settings(&std::fs::read_to_string(settings).ok()?, dir, &path);
        }
        if dir.join(".git").exists() {
            return None;
        }
        folder = dir.parent();
    }
    None
}

#[derive(serde::Deserialize)]
struct Settings {
    default_config: Option<Config>,
    #[serde(rename = "project", default)]
    projects: Vec<Project>,
}

#[derive(serde::Deserialize)]
struct Config {
    instruction_set: Option<String>,
}

#[derive(serde::Deserialize)]
struct Project {
    path: PathBuf,
    instruction_set: Option<String>,
}

/// The instruction set `toml` (an `.asm-lsp.toml` in folder `dir`) gives `path`.
fn in_settings(toml: &str, dir: &Path, path: &Path) -> Option<Arch> {
    let settings: Settings = toml::from_str(toml).ok()?;
    let dir = absolute(dir);
    let set = settings
        .projects
        .iter()
        .find(|project| path.starts_with(absolute(&dir.join(&project.path))))
        .map(|project| project.instruction_set.as_deref())
        .unwrap_or_else(|| {
            settings
                .default_config
                .as_ref()
                .and_then(|c| c.instruction_set.as_deref())
        })?;
    Arch::from_instruction_set(set)
}

fn absolute(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// What a file's code says about its CPU: points for the registers, instructions
/// and directives only one of them has. None when nothing tells them apart.
fn from_text(text: &str) -> Option<Arch> {
    let mut score = Score::default();
    for line in text.lines().take(SAMPLE_LINES) {
        score.line(line);
    }
    let arm = score.arm + score.arm32 + score.arm64;
    let best = score.x86.max(arm).max(score.riscv);
    if best == 0 {
        return None;
    }
    let winners = [score.x86, arm, score.riscv]
        .iter()
        .filter(|&&s| s == best)
        .count();
    if winners > 1 {
        return None;
    }
    Some(if best == score.x86 {
        Arch::X86
    } else if best == score.riscv {
        Arch::RiscV
    } else if score.arm32 > score.arm64 {
        Arch::Arm
    } else {
        Arch::Arm64
    })
}

#[derive(Default, Debug)]
struct Score {
    x86: u32,
    /// Either ARM.
    arm: u32,
    arm32: u32,
    arm64: u32,
    riscv: u32,
}

impl Score {
    fn line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() || line.len() > 512 {
            return;
        }
        // Comments and preprocessor lines tell nothing.
        if line.starts_with([';', '@', '*', '#']) || line.starts_with("//") {
            return;
        }
        let code = strip(line).to_ascii_lowercase();
        // 32-bit ARM's register lists: push {r4, lr}.
        if code
            .match_indices("{r")
            .any(|(at, _)| code[at + 2..].starts_with(|c: char| c.is_ascii_digit()))
        {
            self.arm32 += 2;
        }
        // RISC-V's memory operands: 8(sp), 0(a0), -16(s0). Not Go's a+0(FP).
        for (at, _) in code.match_indices('(') {
            let inside = code[at + 1..].split(')').next().unwrap_or_default().trim();
            if is_riscv_register(inside) && inside != "fp" {
                self.riscv += 2;
            }
        }
        for word in code
            .split(|c: char| !(c.is_ascii_alphanumeric() || "_.%$#@".contains(c)))
            .filter(|w| !w.is_empty())
        {
            self.word(word);
        }
    }

    fn word(&mut self, word: &str) {
        if let Some(name) = word.strip_prefix('%') {
            if is_x86_register(name) {
                self.x86 += 3;
            } else if matches!(name, "hi" | "lo" | "pcrel_hi" | "pcrel_lo" | "tprel_hi") {
                self.riscv += 3;
            }
            return;
        }
        if word.starts_with('$') && word[1..].starts_with(|c: char| c.is_ascii_digit()) {
            self.x86 += 1;
            return;
        }
        if word.starts_with('#') && word[1..].starts_with(|c: char| c.is_ascii_digit() || c == '-')
        {
            self.arm += 1;
            return;
        }
        if let Some((_, relocation)) = word.split_once('@') {
            if relocation.starts_with("page") || relocation.starts_with("gotpage") {
                self.arm64 += 3;
            }
            return;
        }
        let bare = x86_bare(word);
        if bare > 0 {
            self.x86 += bare;
            return;
        }
        let number = |prefix: &str, max: u32| {
            word.strip_prefix(prefix)
                .filter(|n| !n.is_empty() && n.len() <= 2 && !n.starts_with('0') || *n == "0")
                .and_then(|n| n.parse::<u32>().ok())
                .is_some_and(|n| n <= max)
        };
        if number("w", 30) || matches!(word, "xzr" | "wzr" | "wsp") {
            self.arm64 += 3;
        } else if number("x", 30) {
            // x0-x31 are RISC-V's registers' numbers too.
            self.arm64 += 1;
            self.riscv += 1;
        } else if number("r", 7) {
            self.arm32 += 2;
        } else if number("a", 7)
            || number("t", 6)
            || number("ft", 11)
            || number("fs", 11)
            || number("fa", 7)
            || matches!(word, "ra" | "zero" | "gp" | "tp")
        {
            self.riscv += 3;
        } else if number("s", 11) {
            // s0-s11 are RISC-V's; s0-s31 are ARM's single-precision registers too.
            self.riscv += 1;
            self.arm32 += 1;
        } else if word.starts_with('v') && is_arrangement(word) {
            self.arm64 += 3;
        } else if let Some(points) = x86_word(word) {
            self.x86 += points;
        } else if let Some((arm, arm32, arm64)) = arm_word(word) {
            self.arm += arm;
            self.arm32 += arm32;
            self.arm64 += arm64;
        } else if let Some(points) = riscv_word(word) {
            self.riscv += points;
        }
    }
}

/// `line` without its trailing comment and its strings, roughly: enough to count
/// its words.
fn strip(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut quote = None;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' {
                    chars.next();
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                ';' => break,
                '/' if matches!(chars.peek(), Some('/' | '*')) => break,
                // ARM's @ comment and # comments, not @PAGE or #1.
                '@' | '#' if out.ends_with(char::is_whitespace) || out.is_empty() => {
                    if c == '@' || matches!(chars.peek(), None | Some(' ' | '\t')) {
                        break;
                    }
                    out.push(c);
                }
                _ => out.push(c),
            },
        }
    }
    out
}

/// Points for an x86 register written without AT&T's %: rax, eax, r9d, xmm0 are
/// x86's alone; ax and al are x86's too, but sp and ip are everyone's, bl and bpl
/// are ARM's branches, and r8 is 32-bit ARM's.
fn x86_bare(word: &str) -> u32 {
    let short = [
        "ax", "bx", "cx", "dx", "si", "di", "bp", "al", "ah", "bh", "cl", "ch", "dl", "dh",
    ];
    if short.contains(&word) {
        1
    } else if word.len() >= 3 && !matches!(word, "bpl" | "spl") && is_x86_register(word) {
        3
    } else {
        0
    }
}

fn is_x86_register(name: &str) -> bool {
    let general = ["ax", "bx", "cx", "dx", "si", "di", "sp", "bp", "ip"];
    if let Some(base) = name.strip_prefix(['r', 'e']) {
        if general.contains(&base) {
            return true;
        }
    }
    if matches!(
        name,
        "al" | "ah" | "bl" | "bh" | "cl" | "ch" | "dl" | "dh" | "sil" | "dil" | "spl" | "bpl"
    ) || general.contains(&name)
    {
        return true;
    }
    // r8-r15 with their d, w and b parts; r8 alone is ARM's too.
    if let Some(rest) = name.strip_prefix('r') {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let suffix = &rest[digits.len()..];
        if let Ok(n) = digits.parse::<u32>() {
            return (8..=15).contains(&n) && matches!(suffix, "d" | "w" | "b");
        }
    }
    ["xmm", "ymm", "zmm"].iter().any(|kind| {
        name.strip_prefix(kind)
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    })
}

fn is_riscv_register(name: &str) -> bool {
    let numbered = |prefix: &str, max: u32| {
        name.strip_prefix(prefix)
            .and_then(|n| n.parse::<u32>().ok())
            .is_some_and(|n| n <= max)
    };
    numbered("a", 7)
        || numbered("t", 6)
        || numbered("s", 11)
        || numbered("x", 31)
        || matches!(name, "sp" | "ra" | "gp" | "tp" | "fp" | "zero")
}

/// An AArch64 vector register with its arrangement: v0.16b, v1.4s, v2.d.
fn is_arrangement(word: &str) -> bool {
    let Some((register, arrangement)) = word.split_once('.') else {
        return false;
    };
    let number = &register[1..];
    !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
        && matches!(
            arrangement.split('[').next().unwrap_or_default(),
            "8b" | "16b" | "4h" | "8h" | "2s" | "4s" | "1d" | "2d" | "1q" | "b" | "h" | "s" | "d"
        )
}

/// Points for x86's own instructions and directives.
fn x86_word(word: &str) -> Option<u32> {
    Some(match word {
        ".intel_syntax" | ".att_syntax" | ".code16" | ".code32" | ".code64" => 5,
        "qword" | "dword" | "ptr" | "movq" | "movl" | "movb" | "movw" | "movzx" | "movsx"
        | "movsxd" | "movabs" | "leaq" | "leal" | "pushq" | "popq" | "pushl" | "popl" | "cmpq"
        | "cmpl" | "cmpb" | "testq" | "testl" | "testb" | "xorl" | "xorq" | "addq" | "addl"
        | "subq" | "subl" | "andq" | "andl" | "orq" | "orl" | "incl" | "decl" | "callq"
        | "retq" | "cqo" | "cdq" | "cdqe" | "cltq" | "cpuid" | "rdtsc" | "syscall" | "sysenter"
        | "resb" | "resw" | "resd" | "resq" => 3,
        "db" | "dw" | "dd" | "dq" | "section" | "segment" | "bits" | "global" | "xchg" | "lea"
        | "test" | "imul" | "idiv" | "leave" | "hlt" | "cli" | "sti" | "lodsb" | "stosb"
        | "movsb" | "rep" | "int" => 2,
        "jmp" | "je" | "jne" | "jz" | "jnz" | "jg" | "jge" | "jl" | "jle" | "ja" | "jae" | "jb"
        | "jbe" | "js" | "jns" | "inc" | "dec" | "extern" | "byte" | "word" => 1,
        _ if word.starts_with("cmov") => 1,
        _ => return None,
    })
}

/// Points for ARM's own instructions and directives: for either ARM, for 32-bit
/// ARM, and for AArch64.
fn arm_word(word: &str) -> Option<(u32, u32, u32)> {
    Some(match word {
        ".thumb" | ".arm" | ".thumb_func" | ".syntax" | ".fpu" | ".eabi_attribute" | "cpsr"
        | "spsr" | "apsr" | "fpscr" | "bx" | "blx" | "movw" | "movt" | "ldmia" | "stmia"
        | "ldmfd" | "stmfd" | "stmdb" | "ldm" | "stm" | "vpush" | "vpop" | "it" | "ite" | "itt"
        | "cpsid" | "cpsie" => (0, 3, 0),
        "adrp" | "ldp" | "stp" | "ldur" | "stur" | "movz" | "movk" | "movn" | "csel" | "cset"
        | "csinc" | "cneg" | "ldxr" | "stxr" | "ldaxr" | "stlxr" | "ldar" | "stlr" | ".xword"
        | "eret" | "paciasp" | "autiasp" => (0, 0, 3),
        "ldr" | "str" | "ldrb" | "strb" | "ldrh" | "strh" | "ldrsb" | "ldrsh" | "ldrsw" | "adr"
        | "mrs" | "msr" | "cbz" | "cbnz" | "tbz" | "tbnz" | "svc" | "bl" | "blr" | "br"
        | "subs" | "adds" | "cmn" | "tst" | "bic" | "orr" | "eor" | "mvn" | "movs" | "lsl"
        | "lsr" | "asr" | "umull" | "smull" | "mla" | "dmb" | "dsb" | "isb" | "wfe" | "lr"
        | "pc" => (2, 0, 0),
        _ if word.starts_with("b.") => (0, 0, 3),
        _ if word.starts_with('v')
            && [".f32", ".f64", ".i32", ".u8", ".s32"]
                .iter()
                .any(|kind| word.ends_with(kind)) =>
        {
            (0, 3, 0)
        }
        _ => return None,
    })
}

/// Points for RISC-V's own instructions and directives.
fn riscv_word(word: &str) -> Option<u32> {
    Some(match word {
        ".option" | ".attribute" | "addi" | "addiw" | "slli" | "srli" | "srai" | "slliw"
        | "lui" | "auipc" | "jal" | "jalr" | "ecall" | "ebreak" | "mret" | "sret" | "lw" | "sw"
        | "lh" | "sh" | "lb" | "sb" | "lbu" | "lhu" | "lwu" | "ld" | "sd" | "li" | "la" | "mv"
        | "beqz" | "bnez" | "blez" | "bgez" | "bltz" | "bgtz" | "bltu" | "bgeu" | "sltu"
        | "slt" | "sltiu" | "slti" | "xori" | "ori" | "andi" | "csrr" | "csrw" | "csrrw"
        | "csrrs" | "csrrc" | "csrs" | "csrc" | "csrwi" | "csrsi" | "csrci" | "fence"
        | "fence.i" | "vsetvli" | "vsetivli" | "vsetvl" | "flw" | "fsw" | "fld" | "fsd"
        | "addw" | "subw" | "mulw" | "divw" => 3,
        _ if (word.starts_with("amo") || word.starts_with("lr.") || word.starts_with("sc."))
            && (word.contains(".w") || word.contains(".d")) =>
        {
            3
        }
        _ if word.starts_with('v')
            && [".v", ".vv", ".vx", ".vi", ".vf", ".vm"]
                .iter()
                .any(|kind| word.ends_with(kind)) =>
        {
            3
        }
        _ => return None,
    })
}

/// How the file at `path`, with `text`, is written for `arch`.
fn flavor(path: &Path, text: &str, arch: Arch) -> Flavor {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    // Go's assembly goes by GOARCH (asm_amd64.s, memmove_arm64.s) and names its
    // symbols against the static base: TEXT ·add(SB).
    let go_arch = [
        "_amd64", "_386", "_arm64", "_arm", "_riscv64", "_ppc64", "_ppc64le", "_s390x", "_mips64x",
        "_mipsx", "_loong64", "_wasm",
    ];
    let lines = || text.lines().take(SAMPLE_LINES);
    if go_arch.iter().any(|goarch| stem.ends_with(goarch)) && text.contains("(SB)")
        || lines().any(|line| line.trim_start().starts_with("TEXT") && line.contains("(SB)"))
    {
        return Flavor::Go;
    }
    if arch != Arch::X86 {
        return Flavor::Gas;
    }
    let (mut intel, mut gas) = (0, 0);
    for line in lines() {
        let line = line.trim_start().to_ascii_lowercase();
        let first = line.split_whitespace().next().unwrap_or_default();
        // NASM's comments too: ; is a statement separator to GNU as.
        if line.starts_with(';')
            || matches!(
                first,
                "section"
                    | "segment"
                    | "global"
                    | "extern"
                    | "bits"
                    | "[bits"
                    | "[section"
                    | "default"
                    | "org"
                    | "%define"
                    | "%macro"
                    | "%include"
                    | "%ifdef"
                    | ".model"
                    | ".code"
                    | "includelib"
                    | "option"
            )
            || line.split_whitespace().nth(1).is_some_and(|second| {
                matches!(
                    second,
                    "db" | "dw" | "dd" | "dq" | "equ" | "resb" | "resq" | "proc"
                )
            })
        {
            intel += 1;
        } else if first.starts_with('.') && !first.ends_with(':')
            || line.contains('%') && !first.starts_with('%')
        {
            gas += 1;
        }
    }
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let intel_named = matches!(extension.as_str(), "asm" | "nasm" | "yasm");
    if intel > gas || intel_named && gas == 0 {
        Flavor::Intel
    } else {
        Flavor::Gas
    }
}

/// The mark that comments a line out in an assembly file: of the marks its
/// assembler reads as one, the one its comments already use, else the usual one:
/// `;` for NASM, `@` for 32-bit ARM, `//` for AArch64 and Go, `#` for GNU as on x86
/// and RISC-V. (`;` separates statements to GNU as, and `#` is a comment to it on
/// ARM only at the start of a line.)
pub fn comment(asm: Asm, text: &str) -> &'static str {
    let marks: &[&'static str] = match (asm.flavor, asm.arch) {
        (Flavor::Go, _) => &["//"],
        (Flavor::Intel, _) => &[";"],
        (Flavor::Gas, Arch::X86 | Arch::RiscV) => &["#", "//"],
        (Flavor::Gas, Arch::Arm) => &["@", "//"],
        // Apple's assembler takes ; for comments.
        (Flavor::Gas, Arch::Arm64) => &["//", ";"],
    };
    let mut counts = vec![0usize; marks.len()];
    for line in text.lines().take(SAMPLE_LINES) {
        let line = line.trim_start();
        if line.starts_with('#') && (is_preprocessor(line) || line.starts_with("#!")) {
            continue;
        }
        if let Some(at) = marks.iter().position(|mark| line.starts_with(mark)) {
            counts[at] += 1;
        }
    }
    let most = counts.iter().copied().max().unwrap_or(0);
    let at = counts.iter().position(|&n| n == most && n > 0).unwrap_or(0);
    marks[at]
}

/// A C preprocessor line in a `.S` file: `#include`, `#define`, `#if 0`.
fn is_preprocessor(line: &str) -> bool {
    let word: String = line
        .trim_start_matches('#')
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic() || *c == '_')
        .collect();
    matches!(
        word.as_str(),
        "include"
            | "include_next"
            | "import"
            | "define"
            | "undef"
            | "if"
            | "ifdef"
            | "ifndef"
            | "elif"
            | "else"
            | "endif"
            | "error"
            | "warning"
            | "pragma"
            | "line"
    )
}

/// Whether asm-lsp has settings for the project at `root`: its own `.asm-lsp.toml`,
/// or yours in asm-lsp's folder of your settings. Without them asm-lsp reads every
/// file as GNU as for the CPU it runs on.
pub fn lsp_settings(root: &Path) -> bool {
    if root.join(".asm-lsp.toml").is_file() {
        return true;
    }
    let Some(dirs) = directories::BaseDirs::new() else {
        return false;
    };
    let mut folders = vec![dirs.config_dir().to_path_buf()];
    if cfg!(target_os = "macos") {
        folders.push(dirs.home_dir().join(".config"));
    }
    folders
        .iter()
        .any(|folder| folder.join("asm-lsp").join(".asm-lsp.toml").is_file())
}

/// Why asm-lsp isn't started for `asm` in the project at `root`, if it isn't.
/// Without settings (`settings`: see [`lsp_settings`]) it reads every file as GNU
/// as for the CPU it runs on, and checks it by assembling it with gcc: it would
/// describe the wrong instructions and mark good lines as errors.
pub fn not_served(asm: Asm, settings: bool, root: &Path) -> Option<String> {
    let host = Arch::host();
    if settings || asm.flavor == Flavor::Gas && host == Some(asm.arch) {
        return None;
    }
    let (what, assembler) = match asm.flavor {
        Flavor::Gas => (format!("{} assembly", asm.arch.name()), "gas"),
        Flavor::Intel => ("NASM / MASM assembly".to_string(), "nasm"),
        Flavor::Go => (format!("Go's {} assembly", asm.arch.name()), "go"),
    };
    let read_as = match host {
        Some(host) if host != asm.arch => format!("{} (the CPU it runs on)", host.name()),
        _ => "GNU as".to_string(),
    };
    Some(format!(
        "asm-lsp would read this {what} as {read_as} · put a .asm-lsp.toml in {} with [default_config], assembler = \"{assembler}\" and instruction_set = \"{}\", then :lsp restart",
        root.display(),
        asm.arch.instruction_set()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arch_of(name: &str, text: &str) -> Arch {
        read(Path::new(name), text).expect("assembly").arch
    }

    #[test]
    fn the_code_says_which_cpu_it_is_for() {
        for (text, arch) in [
            (
                "main:\n    movq $60, %rax\n    xorl %edi, %edi\n    syscall\n",
                Arch::X86,
            ),
            (
                "section .text\nglobal _start\n_start:\n    mov rax, 60\n    syscall\n",
                Arch::X86,
            ),
            (
                "_start:\n    addi a0, zero, 1\n    li a7, 93\n    ecall\n",
                Arch::RiscV,
            ),
            (
                "    sd ra, 8(sp)\n    call printf\n    ld ra, 8(sp)\n    ret\n",
                Arch::RiscV,
            ),
            (
                "_start:\n    mov x0, #1\n    ldr x1, =msg\n    svc #0\n",
                Arch::Arm64,
            ),
            (
                "    adrp x0, msg@PAGE\n    add x0, x0, msg@PAGEOFF\n    bl _puts\n",
                Arch::Arm64,
            ),
            ("    ldp x29, x30, [sp], #16\n    ret\n", Arch::Arm64),
            (
                "    .thumb\n    ldr r0, =0x40021000\n    movs r1, #1\n    bx lr\n",
                Arch::Arm,
            ),
            (
                "    push {r4, lr}\n    bl foo\n    pop {r4, pc}\n",
                Arch::Arm,
            ),
        ] {
            assert_eq!(arch_of("code.s", text), arch, "{text}");
        }
        // Comments don't count, nor words in strings.
        let text = "# addi a0, zero, 1\n; li a7\nmsg: .asciz \"ecall a0 t0\"\n    movl $1, %eax\n";
        assert_eq!(arch_of("code.s", text), Arch::X86);
    }

    #[test]
    fn the_name_and_folders_say_it_before_the_code_cant() {
        assert_eq!(arch_of("memmove_arm64.s", ""), Arch::Arm64);
        assert_eq!(arch_of("asm_amd64.s", ""), Arch::X86);
        assert_eq!(arch_of("sys_386.s", ""), Arch::X86);
        assert_eq!(arch_of("start-riscv.S", ""), Arch::RiscV);
        assert_eq!(arch_of("boot_rv64gc.S", ""), Arch::RiscV);
        assert_eq!(arch_of("crt0.x86_64.s", ""), Arch::X86);
        assert_eq!(arch_of("vectors-armv7m.s", ""), Arch::Arm);
        assert_eq!(arch_of("kernel.nasm", "addi a0, zero, 1\n"), Arch::X86);
        // A name beats the code; the code beats the folders.
        assert_eq!(arch_of("entry_riscv64.s", "movq %rax, %rbx\n"), Arch::RiscV);
        assert_eq!(
            arch_of("arch/arm64/kernel/head.S", "    movq %rax, %rbx\n"),
            Arch::X86
        );
        assert_eq!(
            arch_of("arch/arm64/kernel/head.S", "    b 1f\n"),
            Arch::Arm64
        );
        assert_eq!(arch_of("arch/riscv/kernel/head.S", ""), Arch::RiscV);
        // Words inside other words don't count: armadillo isn't ARM.
        assert_eq!(from_name("armadillo"), None);
        // With nothing to go by, the CPU Raido runs on.
        assert_eq!(arch_of("empty.s", ""), Arch::host().unwrap_or(Arch::X86));
        assert!(!is_assembly(Path::new("notes.txt")));
        assert!(is_assembly(Path::new("BOOT.S")) && is_assembly(Path::new("x.asm")));
    }

    #[test]
    fn asm_lsps_settings_name_the_cpu_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join("fw/boot")).unwrap();
        std::fs::write(
            dir.path().join(".asm-lsp.toml"),
            "[default_config]\nassembler = \"gas\"\ninstruction_set = \"riscv\"\n\n\
             [[project]]\npath = \"fw\"\nassembler = \"gas\"\ninstruction_set = \"arm\"\n",
        )
        .unwrap();
        let x86 = "movq %rax, %rbx\n";
        let top = dir.path().join("start.s");
        std::fs::write(&top, x86).unwrap();
        assert_eq!(arch_of(top.to_str().unwrap(), x86), Arch::RiscV);
        let nested = dir.path().join("fw/boot/vectors.s");
        std::fs::write(&nested, x86).unwrap();
        assert_eq!(arch_of(nested.to_str().unwrap(), x86), Arch::Arm);
        // Settings for another CPU (z80) leave it to the code.
        std::fs::write(
            dir.path().join(".asm-lsp.toml"),
            "[default_config]\nassembler = \"gas\"\ninstruction_set = \"z80\"\n",
        )
        .unwrap();
        assert_eq!(arch_of(top.to_str().unwrap(), x86), Arch::X86);
        // Above the project's .git folder, settings aren't the project's.
        let inner = dir.path().join("inner");
        std::fs::create_dir_all(inner.join(".git")).unwrap();
        let file = inner.join("a.s");
        std::fs::write(&file, x86).unwrap();
        assert_eq!(configured(&file), None);
    }

    #[test]
    fn nasm_masm_and_go_are_told_from_gnu_as() {
        let flavor_of = |name: &str, text: &str| read(Path::new(name), text).unwrap().flavor;
        assert_eq!(
            flavor_of(
                "boot.asm",
                "section .text\nglobal _start\n_start:\n  mov eax, 1\n"
            ),
            Flavor::Intel
        );
        assert_eq!(
            flavor_of("msg.s", "msg db \"hi\", 10\nlen equ $ - msg\n"),
            Flavor::Intel
        );
        assert_eq!(
            flavor_of("main.asm", "main PROC\n  mov eax, 1\n  ret\nmain ENDP\n"),
            Flavor::Intel
        );
        assert_eq!(
            flavor_of("hello.s", "  .globl main\nmain:\n  movq $1, %rax\n"),
            Flavor::Gas
        );
        // GNU as files named .asm, as some projects do.
        assert_eq!(
            flavor_of(
                "start.asm",
                "  .text\n  .globl _start\n_start:\n  movl $1, %eax\n"
            ),
            Flavor::Gas
        );
        assert_eq!(
            flavor_of(
                "asm_amd64.s",
                "#include \"textflag.h\"\nTEXT ·add(SB), NOSPLIT, $0-24\n  MOVQ a+0(FP), AX\n"
            ),
            Flavor::Go
        );
        assert_eq!(
            flavor_of("start.s", "_start:\n  addi a0, zero, 1\n"),
            Flavor::Gas
        );
    }

    #[test]
    fn lines_are_commented_out_as_the_file_already_does() {
        let asm = |name: &str, text: &str| read(Path::new(name), text).unwrap();
        let with = |name: &str, text: &str| comment(asm(name, text), text);
        // As its comments are written.
        assert_eq!(with("a.s", "// set up\n    movq %rax, %rbx\n"), "//");
        assert_eq!(with("a.s", "; one\n; two\n# three\n    mov eax, 1\n"), ";");
        // A #include isn't a comment.
        assert_eq!(
            with("a.S", "#include <asm/unistd.h>\n_start:\n    li a7, 93\n"),
            "#"
        );
        // Else as the assembler writes them.
        assert_eq!(with("boot.asm", "section .text\n    mov eax, 1\n"), ";");
        assert_eq!(with("a.s", "    movq %rax, %rbx\n"), "#");
        assert_eq!(with("a.s", "    ldr x0, [sp]\n    ret\n"), "//");
        assert_eq!(with("a.s", "    .thumb\n    bx lr\n"), "@");
        assert_eq!(with("a.s", "    addi a0, zero, 1\n"), "#");
    }

    #[test]
    fn asm_lsp_serves_only_what_it_reads_right() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let gas = |arch| Asm {
            arch,
            flavor: Flavor::Gas,
        };
        let foreign = [Arch::X86, Arch::Arm64, Arch::RiscV]
            .into_iter()
            .find(|&arch| Some(arch) != Arch::host())
            .unwrap();
        // Without settings: GNU as for this CPU only.
        if let Some(host) = Arch::host() {
            assert_eq!(not_served(gas(host), false, root), None);
        }
        let reason = not_served(gas(foreign), false, root).expect("not served");
        assert!(reason.contains(".asm-lsp.toml"), "{reason}");
        assert!(reason.contains(foreign.instruction_set()), "{reason}");
        let nasm = Asm {
            arch: Arch::X86,
            flavor: Flavor::Intel,
        };
        assert!(not_served(nasm, false, root)
            .unwrap()
            .contains("assembler = \"nasm\""));
        // With settings, everything: they say how to read it.
        assert_eq!(not_served(gas(foreign), true, root), None);
        assert_eq!(not_served(nasm, true, root), None);
        // The project's own settings count.
        assert!(!root.join(".asm-lsp.toml").exists());
        std::fs::write(root.join(".asm-lsp.toml"), "[default_config]\n").unwrap();
        assert!(lsp_settings(root));
    }
}
