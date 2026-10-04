//! Shared query/profile flags and persistent defaults. No config writes.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::args::Args;
use crate::codex_client::{CODEX_QUERY_REASONING, DEFAULT_CODEX_QUERY_MODEL};
use crate::model_client::{self, Env, ResolvedQueryModel};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    #[default]
    Codex,
    Ruddr,
}

#[derive(Debug, Default, Deserialize)]
pub struct QueryFlags {
    harness: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuerySettings {
    pub harness: Harness,
    pub model: String,
    pub effort: Option<String>,
}

impl Default for QuerySettings {
    fn default() -> Self {
        Self {
            harness: Harness::Codex,
            model: DEFAULT_CODEX_QUERY_MODEL.into(),
            // Codex supplies medium; other providers keep their own default.
            effort: None,
        }
    }
}

impl QueryFlags {
    pub fn parse(args: &mut Args) -> Self {
        Self {
            harness: args.value(&["--harness"]),
            model: args.value(&["--model"]),
            effort: args.value(&["--effort"]),
        }
    }

    pub fn resolve(self) -> Result<QuerySettings, String> {
        let root = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(crate::sources::home_dir()).join(".config"));
        resolve_with(
            self,
            &model_client::process_env,
            &root.join("dejavu/config.json"),
        )
    }
}

#[derive(Default, Deserialize)]
struct Config {
    #[serde(default)]
    query: QueryFlags,
}

fn resolve_with(flags: QueryFlags, env: Env, path: &Path) -> Result<QuerySettings, String> {
    let config: Config = match std::fs::read(path) {
        Ok(bytes) => {
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|error| format!("invalid dejavu config {}: {error}", path.display()))?;
            if !value.is_object() || value.get("query").is_some_and(|q| !q.is_object()) {
                return Err(format!(
                    "invalid dejavu config {}: expected an object with a query object",
                    path.display()
                ));
            }
            serde_json::from_value(value)
                .map_err(|error| format!("invalid dejavu config {}: {error}", path.display()))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(error) => {
            return Err(format!(
                "could not read dejavu config {}: {error}",
                path.display()
            ));
        }
    };
    let choose =
        |flag: Option<String>, key: &str, file: Option<String>| flag.or_else(|| env(key)).or(file);
    let harness = choose(flags.harness, "DEJAVU_QUERY_HARNESS", config.query.harness);
    let harness = match harness.as_deref().unwrap_or("codex") {
        "codex" => Harness::Codex,
        "ruddr" => Harness::Ruddr,
        other => {
            return Err(format!(
                "invalid query harness '{other}'; expected codex or ruddr"
            ));
        }
    };
    let model = choose(flags.model, "DEJAVU_QUERY_MODEL", config.query.model)
        .unwrap_or_else(|| DEFAULT_CODEX_QUERY_MODEL.into());
    let effort = choose(flags.effort, "DEJAVU_QUERY_EFFORT", config.query.effort);
    if model.trim().is_empty() || model.starts_with('-') {
        return Err("query model must not be empty or start with '-'".into());
    }
    if let Some(effort) = &effort
        && (effort.is_empty()
            || !effort
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            || effort.starts_with('-'))
    {
        return Err("query effort must be a nonempty level name".into());
    }
    Ok(QuerySettings {
        harness,
        model,
        effort,
    })
}

pub fn resolve_model(
    agent_dir: &Path,
    settings: &QuerySettings,
) -> Result<ResolvedQueryModel, String> {
    if settings.harness == Harness::Codex {
        let mut model = model_client::resolve_query_model(agent_dir, Some(&settings.model))?;
        if model.provider == "codex" {
            if let Some(effort) = &settings.effort {
                model.reasoning_effort = Some(effort.clone());
            }
        } else if settings.effort.is_some() {
            return Err("--effort is unsupported by legacy HTTP/Pi transports; use --harness ruddr for provider effort".into());
        }
        return Ok(model);
    }
    let (provider, id) = settings
        .model
        .split_once('/')
        .unwrap_or(("codex", &settings.model));
    if !matches!(provider, "codex" | "claude" | "pi" | "opencode" | "droid") {
        return Err(format!(
            "unsupported ruddr provider '{provider}'; expected codex, claude, pi, opencode, or droid"
        ));
    }
    if id.trim().is_empty() || id.starts_with('-') {
        return Err("ruddr model ID must not be empty or start with '-'".into());
    }
    Ok(ResolvedQueryModel {
        provider: provider.into(),
        id: id.into(),
        context_window: 128_000,
        agent_dir: agent_dir.to_path_buf(),
        reasoning_effort: settings
            .effort
            .clone()
            .or_else(|| (provider == "codex").then(|| CODEX_QUERY_REASONING.into())),
        harness: Harness::Ruddr,
        direct: None,
        // Ruddr provider names do not identify entries in Pi's pricing catalog.
        cost: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_client::TempDir;

    #[test]
    fn resolves_each_setting_flag_then_env_then_file_then_default() {
        let dir = TempDir::new("dejavu-config-test-").unwrap();
        let path = dir.path().join("config.json");
        let none = |_: &str| None;
        assert_eq!(
            resolve_with(QueryFlags::default(), &none, &path).unwrap(),
            QuerySettings::default()
        );
        std::fs::write(
            &path,
            r#"{"query":{"harness":"ruddr","model":"claude/file","effort":"low"}}"#,
        )
        .unwrap();
        let file = resolve_with(QueryFlags::default(), &none, &path).unwrap();
        assert_eq!(
            file,
            QuerySettings {
                harness: Harness::Ruddr,
                model: "claude/file".into(),
                effort: Some("low".into())
            }
        );
        let env = |key: &str| {
            Some(
                match key {
                    "DEJAVU_QUERY_HARNESS" => "codex",
                    "DEJAVU_QUERY_MODEL" => "env-model",
                    "DEJAVU_QUERY_EFFORT" => "high",
                    _ => return None,
                }
                .into(),
            )
        };
        let from_env = resolve_with(QueryFlags::default(), &env, &path).unwrap();
        assert_eq!(
            from_env,
            QuerySettings {
                harness: Harness::Codex,
                model: "env-model".into(),
                effort: Some("high".into())
            }
        );
        let flags = QueryFlags {
            harness: Some("ruddr".into()),
            model: Some("pi/flag".into()),
            effort: Some("medium".into()),
        };
        assert_eq!(
            resolve_with(flags, &env, &path).unwrap(),
            QuerySettings {
                harness: Harness::Ruddr,
                model: "pi/flag".into(),
                effort: Some("medium".into())
            }
        );
        let mixed = resolve_with(
            QueryFlags {
                model: Some("flag".into()),
                ..QueryFlags::default()
            },
            &|key| (key == "DEJAVU_QUERY_EFFORT").then(|| "high".into()),
            &path,
        )
        .unwrap();
        assert_eq!(
            mixed,
            QuerySettings {
                harness: Harness::Ruddr,
                model: "flag".into(),
                effort: Some("high".into())
            }
        );
    }

    #[test]
    fn malformed_config_and_invalid_settings_are_clear_errors() {
        let dir = TempDir::new("dejavu-config-invalid-").unwrap();
        let path = dir.path().join("config.json");
        for json in ["{", "[]", r#"{"query":null}"#, r#"{"query":{"model":7}}"#] {
            std::fs::write(&path, json).unwrap();
            let error = resolve_with(QueryFlags::default(), &|_| Some("override".into()), &path)
                .unwrap_err();
            assert!(error.contains("invalid dejavu config"), "{error}");
            assert!(error.contains(path.to_str().unwrap()));
        }
        for (json, expected) in [
            (r#"{"query":{"harness":"other"}}"#, "invalid query harness"),
            (r#"{"query":{"model":""}}"#, "query model"),
            (r#"{"query":{"effort":""}}"#, "query effort"),
            (
                r#"{"query":{"effort":"high\nunsafe=true"}}"#,
                "query effort",
            ),
        ] {
            std::fs::write(&path, json).unwrap();
            assert!(
                resolve_with(QueryFlags::default(), &|_| None, &path)
                    .unwrap_err()
                    .contains(expected)
            );
        }
    }

    #[test]
    fn ruddr_routing_does_not_use_legacy_credentials_or_pi_switch() {
        let dir = TempDir::new("dejavu-routing-").unwrap();
        for prefix in ["codex/", "claude/", "pi/", "opencode/", "droid/", ""] {
            let settings = QuerySettings {
                harness: Harness::Ruddr,
                model: format!("{prefix}model/nested"),
                effort: Some("high".into()),
            };
            // A bare model has no slash; nested IDs retain their suffix after the provider.
            let settings = if prefix.is_empty() {
                QuerySettings {
                    model: "bare".into(),
                    ..settings
                }
            } else {
                settings
            };
            let model = resolve_model(dir.path(), &settings).unwrap();
            assert_eq!(
                model.provider,
                if prefix.is_empty() {
                    "codex"
                } else {
                    prefix.trim_end_matches('/')
                }
            );
            assert_eq!(
                model.id,
                if prefix.is_empty() {
                    "bare"
                } else {
                    "model/nested"
                }
            );
            assert_eq!(model.reasoning_effort.as_deref(), Some("high"));
            assert!(model.direct.is_none());
            assert_eq!(model.harness, Harness::Ruddr);
        }
        for id in ["other/model", "claude/", "claude/-flag"] {
            assert!(
                resolve_model(
                    dir.path(),
                    &QuerySettings {
                        harness: Harness::Ruddr,
                        model: id.into(),
                        effort: None
                    }
                )
                .is_err()
            );
        }
        let codex = resolve_model(dir.path(), &QuerySettings::default()).unwrap();
        assert_eq!(codex.reasoning_effort.as_deref(), Some("medium"));
        let legacy = resolve_model(
            dir.path(),
            &QuerySettings {
                model: "example/model".into(),
                ..QuerySettings::default()
            },
        )
        .unwrap();
        assert_eq!(legacy.provider, "example");
        assert_eq!(legacy.harness, Harness::Codex);
        assert!(legacy.reasoning_effort.is_none());
    }
}
