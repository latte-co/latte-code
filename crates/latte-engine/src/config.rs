//! User configuration loading. Secrets are resolved in memory only.
use serde::Deserialize;
use std::{collections::BTreeMap, env, fs, path::Path};
use thiserror::Error;

/// Latte Code's engine embedding configuration format.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub database: DatabaseConfig,
    pub runtime: RuntimeConfig,
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    pub path: String,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: ".latte/latte-code.db".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    pub command_buffer: usize,
    pub event_buffer: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            command_buffer: 32,
            event_buffer: 128,
        }
    }
}

#[derive(Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key: String,
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read configuration {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid JSONC configuration: {0}")]
    Parse(#[from] json5::Error),
    #[error("configuration value at {field} references missing environment variable {name}")]
    MissingEnvironment { field: String, name: String },
    #[error("invalid configuration: {0}")]
    Validation(String),
}

impl Config {
    /// Loads `.latte/latte-engine.jsonc` relative to `root`.
    pub fn load(root: &Path) -> Result<Self, ConfigError> {
        Self::load_path(&root.join(".latte/latte-engine.jsonc"))
    }

    /// Loads an explicit JSONC path, resolves `${NAME}` values, and validates it.
    pub fn load_path(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let mut config: Self = json5::from_str(&text)?;
        config.resolve_environment()?;
        config.validate()?;
        Ok(config)
    }

    fn resolve_environment(&mut self) -> Result<(), ConfigError> {
        self.resolve_environment_with(|name| env::var(name).ok())
    }

    fn resolve_environment_with(
        &mut self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<(), ConfigError> {
        for (name, provider) in &mut self.providers {
            provider.api_key = resolve_placeholder(
                &provider.api_key,
                &format!("providers.{name}.api_key"),
                &lookup,
            )?;
            provider.base_url = resolve_placeholder(
                &provider.base_url,
                &format!("providers.{name}.base_url"),
                &lookup,
            )?;
        }
        self.database.path = resolve_placeholder(&self.database.path, "database.path", &lookup)?;
        Ok(())
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.database.path.trim().is_empty() {
            return Err(ConfigError::Validation(
                "database.path must not be empty".into(),
            ));
        }
        if self.runtime.command_buffer == 0 || self.runtime.event_buffer == 0 {
            return Err(ConfigError::Validation(
                "runtime buffer sizes must be greater than zero".into(),
            ));
        }
        for (name, provider) in &self.providers {
            if provider.base_url.trim().is_empty() || provider.api_key.trim().is_empty() {
                return Err(ConfigError::Validation(format!(
                    "provider {name} requires base_url and api_key"
                )));
            }
        }
        Ok(())
    }
}

fn resolve_placeholder(
    value: &str,
    field: &str,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<String, ConfigError> {
    let Some(name) = value.strip_prefix("${").and_then(|v| v.strip_suffix('}')) else {
        return Ok(value.to_owned());
    };
    if name.is_empty() || name.contains("${") {
        return Err(ConfigError::Validation(format!(
            "{field} has an invalid environment placeholder"
        )));
    }
    lookup(name).ok_or_else(|| ConfigError::MissingEnvironment {
        field: field.into(),
        name: name.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jsonc_defaults_trailing_comma_and_environment() {
        let config: Config = json5::from_str(
            r#"{ // comment
          providers: { local: { base_url: "http://localhost", api_key: "${LATTE_TEST_KEY}", }, },
        }"#,
        )
        .unwrap();
        let mut config = config;
        config
            .resolve_environment_with(|name| (name == "LATTE_TEST_KEY").then(|| "secret".into()))
            .unwrap();
        config.validate().unwrap();
        assert_eq!(config.database, DatabaseConfig::default());
        assert_eq!(config.providers["local"].api_key, "secret");
        let debug = format!("{config:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("secret"));
    }

    #[test]
    fn rejects_missing_secret_and_invalid_buffer() {
        let mut missing: Config = json5::from_str(
            r#"{providers:{p:{base_url:"x",api_key:"${LATTE_MISSING_TEST_KEY}"}}}"#,
        )
        .unwrap();
        assert!(matches!(
            missing.resolve_environment_with(|_| None),
            Err(ConfigError::MissingEnvironment { .. })
        ));
        let invalid: Config = json5::from_str("{runtime:{command_buffer:0}}").unwrap();
        assert!(matches!(
            invalid.validate(),
            Err(ConfigError::Validation(_))
        ));
    }

    #[test]
    fn load_uses_root_relative_path_and_reports_read_and_parse_failures() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".latte")).unwrap();
        fs::write(
            root.path().join(".latte/latte-engine.jsonc"),
            r#"{database:{path:"state.db"},runtime:{command_buffer:1,event_buffer:2}}"#,
        )
        .unwrap();

        let loaded = Config::load(root.path()).unwrap();
        assert_eq!(loaded.database.path, "state.db");
        assert_eq!(loaded.runtime.command_buffer, 1);
        assert_eq!(loaded.runtime.event_buffer, 2);

        let missing = root.path().join("missing.jsonc");
        assert!(matches!(
            Config::load_path(&missing),
            Err(ConfigError::Read { path, .. }) if path == missing.display().to_string()
        ));
        let malformed = root.path().join("malformed.jsonc");
        fs::write(&malformed, "{").unwrap();
        assert!(matches!(
            Config::load_path(&malformed),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn environment_resolution_covers_every_secret_bearing_field_and_rejects_bad_placeholders() {
        let mut config: Config = json5::from_str(
            r#"{
                database:{path:"${DB_PATH}"},
                providers:{p:{base_url:"${BASE_URL}",api_key:"literal-key"}}
            }"#,
        )
        .unwrap();
        config
            .resolve_environment_with(|name| match name {
                "DB_PATH" => Some("resolved.db".into()),
                "BASE_URL" => Some("http://provider.invalid".into()),
                _ => None,
            })
            .unwrap();
        assert_eq!(config.database.path, "resolved.db");
        assert_eq!(config.providers["p"].base_url, "http://provider.invalid");
        assert_eq!(config.providers["p"].api_key, "literal-key");

        for placeholder in ["${}", "${OUTER${INNER}"] {
            assert!(matches!(
                resolve_placeholder(placeholder, "field", &|_| Some("value".into())),
                Err(ConfigError::Validation(message))
                    if message == "field has an invalid environment placeholder"
            ));
        }
        assert_eq!(
            resolve_placeholder("prefix-${NAME}", "field", &|_| None).unwrap(),
            "prefix-${NAME}"
        );
    }

    #[test]
    fn validation_names_empty_database_provider_and_each_zero_buffer() {
        let mut config = Config::default();
        config.database.path = "  ".into();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Validation(message)) if message == "database.path must not be empty"
        ));

        for (command_buffer, event_buffer) in [(0, 1), (1, 0)] {
            let config = Config {
                runtime: RuntimeConfig {
                    command_buffer,
                    event_buffer,
                },
                ..Config::default()
            };
            assert!(matches!(
                config.validate(),
                Err(ConfigError::Validation(message))
                    if message == "runtime buffer sizes must be greater than zero"
            ));
        }

        for provider in [
            ProviderConfig {
                base_url: " ".into(),
                api_key: "key".into(),
            },
            ProviderConfig {
                base_url: "http://provider.invalid".into(),
                api_key: " ".into(),
            },
        ] {
            let config = Config {
                providers: BTreeMap::from([("broken".into(), provider)]),
                ..Config::default()
            };
            assert!(matches!(
                config.validate(),
                Err(ConfigError::Validation(message))
                    if message == "provider broken requires base_url and api_key"
            ));
        }
    }
}
