// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
#[cfg(not(test))]
use std::fs::File;
#[cfg(not(test))]
use std::io::BufReader;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::{Binding, Cli, HeaderArg, OutputFormat};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub enabled_bindings: Vec<String>,
    #[serde(default)]
    pub bearer_token: Option<String>,
    #[serde(default)]
    pub headers: Vec<String>,
    #[serde(default)]
    pub output: Option<String>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("failed to parse {0}: {1}")]
    Parse(PathBuf, serde_yaml::Error),
    #[error("invalid value in {0}: {1}")]
    Invalid(PathBuf, String),
}

const CONFIG_FILENAME: &str = ".a2a.yaml";

#[cfg(not(test))]
pub fn find_config_file() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let home = home_dir();
    find_config_file_from(&cwd, home.as_deref()).or_else(|| {
        home.as_ref()
            .map(|h| h.join(CONFIG_FILENAME))
            .filter(|p| p.is_file())
    })
}

#[cfg(not(test))]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn find_config_file_from(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        let candidate = dir.join(CONFIG_FILENAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        if home.is_some_and(|h| dir == h) {
            break;
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    None
}

#[cfg(not(test))]
pub fn load_config() -> Result<(Config, Option<PathBuf>), ConfigError> {
    match find_config_file() {
        Some(path) => {
            let file = File::open(&path).map_err(|e| ConfigError::Io(path.clone(), e))?;
            let config: Config = serde_yaml::from_reader(BufReader::new(file))
                .map_err(|e| ConfigError::Parse(path.clone(), e))?;
            Ok((config, Some(path)))
        }
        None => Ok((Config::default(), None)),
    }
}

pub fn apply_config(
    cli: &mut Cli,
    config: &Config,
    path: &Option<PathBuf>,
) -> Result<(), ConfigError> {
    let placeholder = PathBuf::from("<config>");
    let p = path.as_ref().unwrap_or(&placeholder);

    // Enabled bindings: CLI wins if any flags were given
    if cli.enabled_bindings.is_empty() && !config.enabled_bindings.is_empty() {
        for s in &config.enabled_bindings {
            let binding = parse_binding_str(s).ok_or_else(|| {
                ConfigError::Invalid(p.clone(), format!("unknown transport: {s:?}"))
            })?;
            cli.enabled_bindings.push(binding);
        }
    }

    // Bearer token: CLI wins
    if cli.bearer_token.is_none() {
        cli.bearer_token = config.bearer_token.clone();
    }

    // Headers: additive — config headers first, then CLI headers
    if !config.headers.is_empty() {
        let mut config_headers: Vec<HeaderArg> = Vec::new();
        for s in &config.headers {
            let h = crate::parse_header(s).map_err(|e| {
                ConfigError::Invalid(p.clone(), format!("invalid header {s:?}: {e}"))
            })?;
            config_headers.push(h);
        }
        config_headers.append(&mut cli.headers);
        cli.headers = config_headers;
    }

    // Output: config only fills in if CLI didn't specify
    if cli.output.is_none() {
        if let Some(s) = &config.output {
            let fmt = parse_output_format(s).ok_or_else(|| {
                ConfigError::Invalid(p.clone(), format!("unknown output format: {s:?}"))
            })?;
            cli.output = Some(fmt);
        }
    }

    Ok(())
}

fn parse_binding_str(s: &str) -> Option<Binding> {
    match s {
        "jsonrpc" => Some(Binding::Jsonrpc),
        "http-json" => Some(Binding::HttpJson),
        _ => None,
    }
}

fn parse_output_format(s: &str) -> Option<OutputFormat> {
    match s {
        "pretty" => Some(OutputFormat::Pretty),
        "json" => Some(OutputFormat::Json),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Binding, OutputFormat};

    fn parse_config(yaml: &str) -> Config {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn empty_cli() -> Cli {
        use crate::Command;
        Cli {
            enabled_bindings: vec![],
            bearer_token: None,
            headers: vec![],
            output: None,
            command: Command::Discover(crate::DiscoverCommand {
                agent_ref: "http://localhost".to_string(),
                extended: false,
            }),
        }
    }

    #[test]
    fn test_find_config_in_start_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".a2a.yaml");
        std::fs::write(&cfg, "").unwrap();
        assert_eq!(find_config_file_from(dir.path(), None), Some(cfg));
    }

    #[test]
    fn test_find_config_walks_up() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().to_path_buf();
        let child = parent.join("child");
        std::fs::create_dir(&child).unwrap();

        let cfg = parent.join(".a2a.yaml");
        std::fs::write(&cfg, "").unwrap();

        assert_eq!(find_config_file_from(&child, None), Some(cfg));
    }

    #[test]
    fn test_find_config_stops_at_home() {
        let home = tempfile::tempdir().unwrap();
        let above = home.path().parent().unwrap().to_path_buf();
        // Put config above home — should NOT be found
        std::fs::write(above.join(".a2a.yaml"), "").unwrap();
        assert_eq!(
            find_config_file_from(home.path(), Some(home.path())),
            None
        );
    }

    #[test]
    fn test_find_config_found_at_home() {
        let home = tempfile::tempdir().unwrap();
        let cfg = home.path().join(".a2a.yaml");
        std::fs::write(&cfg, "").unwrap();
        assert_eq!(
            find_config_file_from(home.path(), Some(home.path())),
            Some(cfg)
        );
    }

    #[test]
    fn test_find_config_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(find_config_file_from(dir.path(), Some(dir.path())), None);
    }

    #[test]
    fn test_parse_valid_config() {
        let cfg = parse_config("enabled_bindings:\n  - jsonrpc\nbearer_token: tok\noutput: json\n");
        assert_eq!(cfg.enabled_bindings, vec!["jsonrpc"]);
        assert_eq!(cfg.bearer_token.as_deref(), Some("tok"));
        assert_eq!(cfg.output.as_deref(), Some("json"));
    }

    #[test]
    fn test_unknown_field_is_rejected() {
        let result = serde_yaml::from_str::<Config>("typo_field: value");
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_fills_empty_cli() {
        let mut cli = empty_cli();
        let cfg = parse_config("enabled_bindings:\n  - http-json\nbearer_token: tok\noutput: json\n");
        apply_config(&mut cli, &cfg, &None).unwrap();
        assert_eq!(cli.enabled_bindings, vec![crate::Binding::HttpJson]);
        assert_eq!(cli.bearer_token.as_deref(), Some("tok"));
        assert_eq!(cli.output, Some(OutputFormat::Json));
    }

    #[test]
    fn test_cli_bindings_take_precedence() {
        let mut cli = empty_cli();
        cli.enabled_bindings = vec![Binding::Jsonrpc];
        let cfg = parse_config("enabled_bindings:\n  - http-json\n");
        apply_config(&mut cli, &cfg, &None).unwrap();
        assert_eq!(cli.enabled_bindings, vec![Binding::Jsonrpc]);
    }

    #[test]
    fn test_headers_are_additive() {
        let mut cli = empty_cli();
        cli.headers = vec![HeaderArg {
            name: "X-Cli".to_string(),
            value: "cli".to_string(),
        }];
        let cfg = parse_config("headers:\n  - \"X-Config: cfg\"\n");
        apply_config(&mut cli, &cfg, &None).unwrap();
        assert_eq!(cli.headers.len(), 2);
        assert_eq!(cli.headers[0].name, "X-Config");
        assert_eq!(cli.headers[1].name, "X-Cli");
    }

    #[test]
    fn test_invalid_binding_errors() {
        let mut cli = empty_cli();
        let cfg = parse_config("enabled_bindings:\n  - bogus\n");
        let err = apply_config(&mut cli, &cfg, &None).unwrap_err();
        assert!(err.to_string().contains("unknown transport"));
    }

    #[test]
    fn test_invalid_output_errors() {
        let mut cli = empty_cli();
        let cfg = parse_config("output: bogus\n");
        let err = apply_config(&mut cli, &cfg, &None).unwrap_err();
        assert!(err.to_string().contains("unknown output format"));
    }

    #[test]
    fn test_invalid_header_errors() {
        let mut cli = empty_cli();
        let cfg = parse_config("headers:\n  - \"no-colon\"\n");
        let err = apply_config(&mut cli, &cfg, &None).unwrap_err();
        assert!(err.to_string().contains("invalid header"));
    }

    #[test]
    fn test_cli_output_takes_precedence() {
        let mut cli = empty_cli();
        cli.output = Some(OutputFormat::Pretty);
        let cfg = parse_config("output: json\n");
        apply_config(&mut cli, &cfg, &None).unwrap();
        assert_eq!(cli.output, Some(OutputFormat::Pretty));
    }
}
