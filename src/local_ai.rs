//! Bring your own AI: any CLI or local model can be added to the AI launcher with
//! an `[ai.<name>]` table in config.toml (program, args, env). The built-in
//! "Local LLM" entry uses `[ai.local]` when present, and otherwise detects a local
//! runtime (Ollama, LM Studio) so a local model works without any setup.
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    io::Read,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

/// A user-defined AI launcher entry: `[ai.<name>]`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launcher {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment, e.g. `{ OPENAI_BASE_URL = "http://localhost:1234/v1" }`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Model substituted for `{model}` in args when `:ai <name>` gives none.
    #[serde(default)]
    pub model: String,
}

const HELP: &str = "add [ai.local] to config.toml, e.g. program = \"llama-cli\", args = [\"-m\", \"model.gguf\", \"-cnv\"] · or install Ollama (ollama.com) or LM Studio (lmstudio.ai)";

/// Program, arguments and extra environment to run a launcher with.
pub type Run = (String, Vec<String>, Vec<(String, String)>);

/// Program, arguments and environment for a launcher, with `{model}` filled in.
pub fn command(launcher: &Launcher, model: Option<&str>) -> Result<Run> {
    let model = model.filter(|m| !m.is_empty()).unwrap_or(&launcher.model);
    if launcher.args.iter().any(|a| a.contains("{model}")) && model.is_empty() {
        bail!("This launcher needs a model: `:ai <name> <model>` or set model in its config");
    }
    let args: Vec<String> = launcher
        .args
        .iter()
        .map(|a| a.replace("{model}", model))
        .collect();
    let env = launcher
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Ok((launcher.program.clone(), args, env))
}

/// The "Local LLM" entry: `[ai.local]` if configured, else a detected runtime.
pub fn local(
    configured: Option<&Launcher>,
    model: Option<&str>,
    ollama_models: impl Fn() -> Result<Vec<String>>,
    on_path: impl Fn(&str) -> bool,
) -> Result<Run> {
    if let Some(launcher) = configured {
        if !on_path(&launcher.program) {
            bail!("`{}` from [ai.local] is not on PATH", launcher.program);
        }
        return command(launcher, model);
    }
    if on_path("ollama") {
        let model = match model.filter(|m| !m.is_empty()) {
            Some(model) => model.to_string(),
            None => ollama_models()?
                .into_iter()
                .next()
                .context("Ollama has no models yet · pull one: ollama pull qwen3-coder")?,
        };
        return Ok(("ollama".into(), vec!["run".into(), model], Vec::new()));
    }
    if on_path("lms") {
        let mut args = vec!["chat".to_string()];
        args.extend(model.filter(|m| !m.is_empty()).map(String::from));
        return Ok(("lms".into(), args, Vec::new()));
    }
    bail!("No local LLM found · {HELP}")
}

/// Model names from `ollama list`, bounded so a stuck server cannot freeze Raido.
pub fn ollama_models() -> Result<Vec<String>> {
    let mut child = Command::new("ollama")
        .arg("list")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("Run ollama list")?;
    let mut stdout = child.stdout.take().context("ollama stdout")?;
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        let _ = send.send(text);
    });
    let text = receive.recv_timeout(Duration::from_secs(5));
    let _ = child.kill();
    let status = child.wait()?;
    match text {
        Ok(text) if status.success() || !text.trim().is_empty() => Ok(parse_list(&text)),
        _ => bail!("Ollama isn't running · start it: ollama serve  (or: systemctl start ollama)"),
    }
}

/// First column of `ollama list`, skipping the header row.
fn parse_list(text: &str) -> Vec<String> {
    text.lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "NAME                  ID              SIZE      MODIFIED\n\
qwen3-coder:latest    06c1097efce0    18 GB     2 days ago\n\
gpt-oss:20b           aa4295ac10c3    13 GB     5 weeks ago\n";

    fn models() -> Result<Vec<String>> {
        Ok(parse_list(LIST))
    }

    #[test]
    fn parses_ollama_list() {
        assert_eq!(models().unwrap(), vec!["qwen3-coder:latest", "gpt-oss:20b"]);
        assert!(parse_list("NAME ID SIZE MODIFIED\n").is_empty());
    }

    #[test]
    fn launchers_substitute_models_and_apply_env() {
        let mut launcher = Launcher {
            program: "aider".into(),
            args: vec!["--model".into(), "openai/{model}".into()],
            model: "qwen3".into(),
            ..Launcher::default()
        };
        assert_eq!(
            command(&launcher, None).unwrap(),
            (
                "aider".into(),
                vec!["--model".into(), "openai/qwen3".into()],
                Vec::new()
            )
        );
        assert_eq!(
            command(&launcher, Some("llama3")).unwrap().1[1],
            "openai/llama3"
        );
        launcher
            .env
            .insert("OPENAI_API_BASE".into(), "http://localhost:8080/v1".into());
        let (program, _, env) = command(&launcher, None).unwrap();
        assert_eq!(program, "aider");
        assert_eq!(
            env,
            vec![("OPENAI_API_BASE".into(), "http://localhost:8080/v1".into())]
        );
        launcher.model.clear();
        assert!(command(&launcher, None).is_err());
    }

    #[test]
    fn local_prefers_config_then_detects_ollama_then_lm_studio() {
        let configured = Launcher {
            program: "llama-cli".into(),
            args: vec!["-m".into(), "m.gguf".into()],
            ..Launcher::default()
        };
        assert_eq!(
            local(Some(&configured), None, models, |_| true).unwrap().0,
            "llama-cli"
        );
        assert_eq!(
            local(None, None, models, |p| p == "ollama").unwrap().1,
            vec!["run".to_string(), "qwen3-coder:latest".into()]
        );
        assert_eq!(
            local(None, Some("gpt-oss:20b"), models, |p| p == "ollama")
                .unwrap()
                .1[1],
            "gpt-oss:20b"
        );
        assert_eq!(local(None, None, models, |p| p == "lms").unwrap().0, "lms");
        let error = local(None, None, models, |_| false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("[ai.local]"), "{error}");
        let none = || Ok(Vec::new());
        let error = local(None, None, none, |_| true).unwrap_err().to_string();
        assert!(error.contains("ollama pull"), "{error}");
    }
}
