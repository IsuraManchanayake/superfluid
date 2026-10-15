//! `--<runtime>-<name> <value>` on `superfluid serve`: an option of one runtime, declared by its
//! adapter (`Runtime::options`), checked here against that declaration before a worker starts,
//! and forwarded to the runtime's worker as `--<name> <value>`. They are set once per process
//! after parsing, as `BASERT_LIB` is, so every worker spawned for that runtime gets them.

use std::sync::OnceLock;

use serde_json::{json, Value};
use superfluid_adapter_kit::{check_value, placeholder};

use crate::runtime_pick::RuntimeId;
use crate::runtimes::{Catalog, Runtime};

/// One `--<runtime>-<name> <value>` as given, resolved to the runtime it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Given {
    pub flag: String,
    pub runtime: RuntimeId,
    pub name: String,
    pub value: String,
}

/// Whether `flag` is shaped as a runtime option, `--<x>-<y>`: only then does `serve` keep it for
/// [`resolve`] instead of refusing it on the spot.
pub fn looks_like(flag: &str) -> bool {
    let Some(body) = flag.strip_prefix("--") else { return false };
    let shaped = body.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | '_'));
    shaped && body.split_once('-').is_some_and(|(id, name)| !id.is_empty() && !name.is_empty())
}

/// The runtime among `ids` that `flag` names, the longest id winning (`--mlx-x-y` is mlx's `x-y`
/// unless a runtime `mlx-x` is here), or `None` when no runtime here is named.
pub fn resolve(flag: &str, value: &str, ids: &[RuntimeId]) -> Option<Given> {
    let body = flag.strip_prefix("--")?;
    let mut best: Option<(RuntimeId, &str)> = None;
    for id in ids.iter().map(|id| id.base()) {
        let Some(name) = body.strip_prefix(id.name()).and_then(|rest| rest.strip_prefix('-')) else { continue };
        if !name.is_empty() && best.is_none_or(|(b, _)| id.name().len() > b.name().len()) {
            best = Some((id, name));
        }
    }
    best.map(|(runtime, name)| Given { flag: flag.to_string(), runtime, name: name.to_string(), value: value.to_string() })
}

/// The options among `given` for `runtime`, checked against what its worker declares, as the
/// `--<name> <value>` pairs its worker takes.
pub fn check(given: &[Given], runtime: &Runtime) -> Result<Vec<(String, String)>, String> {
    let id = runtime.id.base();
    let mut out = Vec::new();
    for g in given.iter().filter(|g| g.runtime == id) {
        let Some(decl) = runtime.options.iter().find(|o| o.name == g.name) else {
            let takes: Vec<String> = runtime.options.iter().map(|o| format!("--{}-{}", id.name(), o.name)).collect();
            return Err(if takes.is_empty() {
                format!("{}: the {} runtime takes no option of its own", g.flag, id.name())
            } else {
                format!("{}: the {} runtime takes no such option (it takes: {})", g.flag, id.name(), takes.join(", "))
            });
        };
        check_value(&decl.kind, &decl.choices, decl.least, &g.value).map_err(|e| format!("{}: {e}", g.flag))?;
        out.push((format!("--{}", g.name), g.value.clone()));
    }
    Ok(out)
}

/// The checked options per runtime, as the `--<name> <value>` pairs its worker takes.
pub type Settled = Vec<(RuntimeId, Vec<(String, String)>)>;

/// Every given option checked: against `primary` for its runtime, and against the catalog's
/// record of any other runtime named, which must be usable here to be checked at all.
pub fn settle(given: &[Given], primary: &Runtime, catalog: &Catalog) -> Result<Settled, String> {
    let mut ids: Vec<RuntimeId> = given.iter().map(|g| g.runtime).collect();
    ids.sort();
    ids.dedup();
    let mut out = Vec::new();
    for id in ids {
        let checked = if id == primary.id.base() {
            check(given, primary)?
        } else {
            let rt = catalog.require(id).map_err(|e| {
                let flag = given.iter().find(|g| g.runtime == id).map(|g| g.flag.as_str()).unwrap_or_default();
                format!("{flag}: the {} runtime is not usable here, so its options cannot be checked: {e}", id.name())
            })?;
            check(given, &rt)?
        };
        out.push((id, checked));
    }
    Ok(out)
}

static SET: OnceLock<Settled> = OnceLock::new();

/// Keeps the settled options for every worker this process starts; the first call wins.
pub fn set(settled: Settled) {
    let _ = SET.set(settled);
}

/// The `--<name> <value>` pairs a worker of `id` gets.
pub fn for_runtime(id: RuntimeId) -> Vec<(String, String)> {
    SET.get().and_then(|all| all.iter().find(|(r, _)| *r == id.base()).map(|(_, o)| o.clone())).unwrap_or_default()
}

/// `superfluid runtime options <id>`, for a person.
pub fn text(runtime: &Runtime) -> String {
    let id = runtime.id.base().name();
    if runtime.options.is_empty() {
        return format!("the {id} runtime takes no option of its own; `superfluid serve --help` lists the flags every runtime takes\n");
    }
    let rows: Vec<(String, String, String)> = runtime
        .options
        .iter()
        .map(|o| {
            let default = if o.default.is_empty() { "-".to_string() } else { o.default.clone() };
            (format!("--{id}-{} {}", o.name, placeholder(&o.kind, &o.choices)), default, o.about.clone())
        })
        .collect();
    let w1 = rows.iter().map(|r| r.0.chars().count()).max().unwrap_or(0);
    let w2 = rows.iter().map(|r| r.1.chars().count()).max().unwrap_or(0).max("DEFAULT".len());
    let mut out = format!("Options of the {id} runtime on `superfluid serve`:\n\n  {:<w1$}  {:<w2$}  \n", "OPTION", "DEFAULT");
    for (flag, default, about) in rows {
        out.push_str(&format!("  {flag:<w1$}  {default:<w2$}  {about}\n"));
    }
    out
}

/// `superfluid runtime options <id> --json`.
pub fn json(runtime: &Runtime) -> Value {
    json!({"runtime": runtime.id.base().name(), "options": runtime.options.iter().map(|o| o.to_json()).collect::<Vec<_>>()})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtimes::{Info, Source, Worker};
    use superfluid_worker::check::OptionDecl;

    fn runtime(id: &str, options: Vec<OptionDecl>) -> Runtime {
        Runtime {
            id: RuntimeId::new(id),
            worker: Worker { bin: std::path::PathBuf::from("/nowhere"), prefix: Vec::new(), source: Source::Env },
            version: None,
            abi: None,
            capabilities: None,
            info: Info::default(),
            devices: Vec::new(),
            tokenizer: None,
            pull: None,
            options,
        }
    }

    fn decl(name: &str, kind: &str, choices: &[&str], default: &str) -> OptionDecl {
        OptionDecl {
            name: name.into(),
            kind: kind.into(),
            choices: choices.iter().map(|c| c.to_string()).collect(),
            least: None,
            default: default.into(),
            about: format!("about {name}"),
        }
    }

    #[test]
    fn a_flag_is_kept_for_resolution_only_when_shaped_as_an_option() {
        assert!(looks_like("--llamacpp-n-gpu-layers"));
        assert!(looks_like("--mlx-venv"));
        assert!(looks_like("--max-context"), "a flag serve knows never reaches here; the shape alone is judged");
        assert!(!looks_like("--port"), "no '-' after the runtime");
        assert!(!looks_like("--llamacpp-"), "an empty name");
        assert!(!looks_like("-ngl") && !looks_like("--Llamacpp-x") && !looks_like("--llamacpp-x=1"));
    }

    #[test]
    fn the_longest_runtime_id_names_the_option() {
        let ids = [RuntimeId::new("mlx"), RuntimeId::new("mlx-pro"), RuntimeId::new("llamacpp@v1-metal")];
        let g = resolve("--mlx-pro-chunk", "4", &ids).unwrap();
        assert_eq!((g.runtime.name(), g.name.as_str(), g.value.as_str()), ("mlx-pro", "chunk", "4"));
        let g = resolve("--mlx-prefill-chunk", "4", &ids).unwrap();
        assert_eq!((g.runtime.name(), g.name.as_str()), ("mlx", "prefill-chunk"));
        let g = resolve("--llamacpp-threads", "8", &ids).unwrap();
        assert_eq!((g.runtime.name(), g.name.as_str()), ("llamacpp", "threads"), "an install's id names its base runtime");
        assert!(resolve("--max-contxt", "8", &ids).is_none(), "no runtime max here");
        assert!(resolve("--mlx-", "8", &ids).is_none());
    }

    #[test]
    fn a_value_is_checked_against_the_declaration_and_the_worker_flag_is_built() {
        let rt = runtime("llamacpp", vec![decl("n-gpu-layers", "int", &[], "-1"), decl("flash-attn", "choice", &["auto", "on", "off"], "auto")]);
        let ids = [rt.id];
        let good = [resolve("--llamacpp-n-gpu-layers", "20", &ids).unwrap(), resolve("--llamacpp-flash-attn", "off", &ids).unwrap()];
        assert_eq!(check(&good, &rt).unwrap(), [("--n-gpu-layers".to_string(), "20".to_string()), ("--flash-attn".to_string(), "off".to_string())]);
        let bad = [resolve("--llamacpp-flash-attn", "yes", &ids).unwrap()];
        assert_eq!(check(&bad, &rt).unwrap_err(), "--llamacpp-flash-attn: one of auto, on, off expected, got \"yes\"");
        let mut floored = runtime("llamacpp", vec![decl("batch-size", "int", &[], "2048")]);
        floored.options[0].least = Some(1);
        let zero = [resolve("--llamacpp-batch-size", "0", &[floored.id]).unwrap()];
        assert_eq!(check(&zero, &floored).unwrap_err(), "--llamacpp-batch-size: at least 1 expected, got 0");
        let unknown = [resolve("--llamacpp-ngl", "20", &ids).unwrap()];
        assert_eq!(
            check(&unknown, &rt).unwrap_err(),
            "--llamacpp-ngl: the llamacpp runtime takes no such option (it takes: --llamacpp-n-gpu-layers, --llamacpp-flash-attn)"
        );
        let none = runtime("basert", Vec::new());
        let given = [resolve("--basert-threads", "2", &[none.id]).unwrap()];
        assert_eq!(check(&given, &none).unwrap_err(), "--basert-threads: the basert runtime takes no option of its own");
        assert!(check(&good, &none).unwrap().is_empty(), "another runtime's options are not its concern");
    }

    #[test]
    fn the_listing_shows_each_option_with_its_value_shape_and_default() {
        let rt = runtime("llamacpp", vec![decl("n-gpu-layers", "int", &[], "-1"), decl("flash-attn", "choice", &["auto", "on", "off"], ""), decl("lib", "text", &[], "the install's")]);
        let t = text(&rt);
        assert!(t.contains("--llamacpp-n-gpu-layers <N>"), "{t}");
        assert!(t.contains("--llamacpp-flash-attn <auto|on|off>"), "{t}");
        assert!(t.contains("--llamacpp-lib <value>") && t.contains("the install's") && t.contains("about lib"), "{t}");
        assert!(text(&runtime("basert", Vec::new())).starts_with("the basert runtime takes no option of its own"));
        assert_eq!(json(&rt)["options"][1]["choices"], json!(["auto", "on", "off"]));
    }
}
