//! `--json` and `--jq`: every command result is one typed value. Human output, JSON, and jq
//! filters all start from it.

use anyhow::{Result, anyhow, bail};
use jaq_core::{
    Compiler, Ctx, Vars, data,
    load::{Arena, File, Loader},
    unwrap_valr,
};
use jaq_json::Val;
use serde::Serialize;

/// Output flags of the commands that return a typed result.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct OutputArgs {
    /// Print the complete result as JSON.
    #[arg(long)]
    pub json: bool,
    /// Filter the JSON result with a jq expression. Implies --json. Strings print as plain text.
    #[arg(long, value_name = "EXPR")]
    pub jq: Option<String>,
}

/// How a command prints its result. Made before the command changes state, so a bad filter
/// stops it first.
pub struct Output {
    mode: Mode,
}

enum Mode {
    Human,
    Json,
    Jq(Filter),
}

type Filter = jaq_core::Filter<data::JustLut<Val>>;

impl OutputArgs {
    pub fn output(&self) -> Result<Output> {
        let mode = match &self.jq {
            Some(expr) => Mode::Jq(compile(expr)?),
            None if self.json => Mode::Json,
            None => Mode::Human,
        };
        Ok(Output { mode })
    }
}

impl Output {
    /// Prints `value`: with `human` for text, else as JSON or through the filter.
    pub fn print<T: Serialize>(&self, value: &T, human: impl FnOnce(&T)) -> Result<()> {
        match &self.mode {
            Mode::Human => {
                human(value);
                Ok(())
            }
            Mode::Json => {
                println!("{}", serde_json::to_string_pretty(value)?);
                Ok(())
            }
            Mode::Jq(filter) => {
                let json = serde_json::to_value(value)?;
                let lines = run(filter, &json)
                    .map_err(|e| anyhow!("the operation succeeded, but the --jq filter failed: {e:#}"))?;
                for line in lines {
                    println!("{line}");
                }
                Ok(())
            }
        }
    }
}

fn compile(expr: &str) -> Result<Filter> {
    let defs = jaq_core::defs().chain(jaq_std::defs()).chain(jaq_json::defs());
    let funs = jaq_core::funs().chain(jaq_std::funs()).chain(jaq_json::funs());
    let arena = Arena::default();
    let modules = Loader::new(defs)
        .load(&arena, File { code: expr, path: () })
        .map_err(|e| anyhow!("invalid --jq expression {expr:?}: {} error(s)", e.len()))?;
    Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|e| anyhow!("invalid --jq expression {expr:?}: {} error(s)", e.len()))
}

/// Runs the filter. Strings print raw, other values as compact JSON.
fn run(filter: &Filter, input: &serde_json::Value) -> Result<Vec<String>> {
    let input = jaq_json::read::parse_single(serde_json::to_string(input)?.as_bytes())
        .map_err(|e| anyhow!("read JSON: {e:?}"))?;
    let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
    let mut lines = Vec::new();
    for v in filter.id.run((ctx, input)).map(unwrap_valr) {
        match v {
            Ok(Val::TStr(s)) => lines.push(String::from_utf8_lossy(&s).into_owned()),
            Ok(v) => lines.push(compact(&v)?),
            Err(e) => bail!("{e}"),
        }
    }
    Ok(lines)
}

/// Reformats jaq's JSON with serde so objects and arrays are compact.
fn compact(v: &Val) -> Result<String> {
    let text = v.to_string();
    Ok(match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(j) => serde_json::to_string(&j)?,
        Err(_) => text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jq(expr: &str, v: serde_json::Value) -> Result<Vec<String>> {
        run(&compile(expr)?, &v)
    }

    #[test]
    fn filters_select_and_print() {
        let rows = json!([{"name": "a", "status": {"state": "up"}}, {"name": "b", "status": {"state": "down"}}]);
        assert_eq!(jq(r#".[] | select(.status.state == "up") | .name"#, rows.clone()).unwrap(), ["a"]);
        assert_eq!(jq(".[0].status", rows.clone()).unwrap(), [r#"{"state":"up"}"#]);
        assert_eq!(jq("[.[].name]", rows.clone()).unwrap(), [r#"["a","b"]"#]);
        assert_eq!(jq("{n: length}", rows.clone()).unwrap(), [r#"{"n":2}"#]);
        assert!(jq(".[] | select(.name == \"z\")", rows).unwrap().is_empty());
        assert_eq!(jq(".", json!([])).unwrap(), ["[]"]);
    }

    #[test]
    fn bad_expressions_fail_before_running() {
        assert!(compile(".[").is_err());
        assert!(compile("nosuchfn").is_err());
        assert!(jq(".a.b", json!({"a": 1})).is_err());
    }
}
