//! Local translation through `scripts/translate_qwen.py` (Qwen on CUDA via transformers).

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Path of the bundled script, fixed at build time.
pub const DEFAULT_SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/translate_qwen.py");

pub struct Qwen {
    pub script: PathBuf,
    /// Interpreter to run the script with; `None` means `uv run --script`.
    pub python: Option<PathBuf>,
    /// Model id passed through to the script; `None` keeps the script's default.
    pub model: Option<String>,
    pub batch_lines: usize,
    /// Extra arguments passed verbatim to the script (e.g. `--device cpu`, `--quant 4bit`).
    pub extra_args: Vec<String>,
}

#[derive(Deserialize)]
struct Output {
    #[serde(default)]
    model: String,
    translations: Vec<String>,
}

impl Qwen {
    pub fn check(&self) -> Result<()> {
        if !self.script.is_file() {
            bail!("translation script not found: {} (pass --script)", self.script.display());
        }
        if let Some(py) = &self.python {
            if !py.is_file() {
                bail!("python interpreter not found: {}", py.display());
            }
        } else {
            Command::new("uv").arg("--version").output().context("uv not found on PATH (needed for `uv run --script`; or pass --python)")?;
        }
        Ok(())
    }

    pub fn describe(&self) -> String {
        format!(
            "qwen {} via {} ({})",
            self.model.as_deref().unwrap_or("script default"),
            self.python.as_ref().map_or("uv run".to_string(), |p| p.display().to_string()),
            self.script.display()
        )
    }

    /// Translate `lines`; results are cached in `work` keyed by model + target + lines.
    pub fn translate(&self, work: &Path, id: &str, lines: &[String], title: &str, target: &str) -> Result<Vec<String>> {
        let input = serde_json::json!({ "title": title, "target": target, "lines": lines });
        let input_text = serde_json::to_string_pretty(&input)?;
        let mut h = DefaultHasher::new();
        self.model.hash(&mut h);
        self.extra_args.hash(&mut h);
        self.batch_lines.hash(&mut h);
        // The script's own text is part of the key so prompt changes invalidate old results.
        fs::read(&self.script).with_context(|| format!("reading {}", self.script.display()))?.hash(&mut h);
        input_text.hash(&mut h);
        let key = format!("{:016x}", h.finish());
        let in_path = work.join(format!("{id}.qwen.{key}.in.json"));
        let out_path = work.join(format!("{id}.qwen.{key}.out.json"));

        if !out_path.is_file() {
            fs::write(&in_path, &input_text)?;
            let mut cmd = match &self.python {
                Some(py) => {
                    let mut c = Command::new(py);
                    c.arg(&self.script);
                    c
                }
                None => {
                    let mut c = Command::new("uv");
                    c.args(["run", "--script"]).arg(&self.script);
                    c
                }
            };
            cmd.arg("--input").arg(&in_path).arg("--output").arg(&out_path);
            cmd.arg("--batch-lines").arg(self.batch_lines.to_string());
            if let Some(m) = &self.model {
                cmd.arg("--model").arg(m);
            }
            cmd.args(&self.extra_args);
            let status = cmd.status().context("running the translation script")?;
            if !status.success() {
                bail!("translation script failed ({status})");
            }
        } else {
            eprintln!("[diolingo] reusing cached translation {}", out_path.display());
        }

        let text = fs::read_to_string(&out_path).with_context(|| format!("reading {}", out_path.display()))?;
        let out: Output = serde_json::from_str(&text).context("translation script wrote invalid JSON")?;
        if out.translations.len() != lines.len() {
            bail!("translation script returned {} lines for {} inputs", out.translations.len(), lines.len());
        }
        if !out.model.is_empty() {
            eprintln!("[diolingo] translated by {}", out.model);
        }
        Ok(out.translations)
    }
}
