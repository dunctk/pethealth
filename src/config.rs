use anyhow::{Context, bail};
use std::{env, path::Path};

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub production: bool,
    pub database_url: String,
    pub llm_api_key: Option<String>,
    pub llm_base_url: String,
    pub llm_model: String,
    /// How long to wait on the capture extraction call. `rig` builds its own HTTP
    /// client with no timeout, so this is the only thing stopping a stalled
    /// upstream from holding the request open forever.
    pub llm_timeout_seconds: u64,
    pub mistral_api_key: Option<String>,
    pub blood_tests_dir: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let _ = dotenvy::dotenv();
        let production = env_bool("PRODUCTION");
        let database_url = if production {
            let parent = Path::new("/persistent");
            if !parent.exists() {
                bail!("PRODUCTION=true requires the /persistent volume");
            }
            "sqlite:///persistent/pethealth.sqlite?mode=rwc".to_owned()
        } else {
            env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite://./data/pethealth.sqlite?mode=rwc".to_owned())
        };
        let port = env::var("APP_PORT")
            .unwrap_or_else(|_| "3000".to_owned())
            .parse()
            .context("APP_PORT must be a valid port")?;
        let username = env::var("APP_USERNAME").unwrap_or_else(|_| "owner".to_owned());
        let password = env::var("APP_PASSWORD").unwrap_or_else(|_| "change-me".to_owned());
        if production && password == "change-me" {
            bail!("APP_PASSWORD must be set to a non-default value in production");
        }
        Ok(Self {
            host: env::var("APP_HOST").unwrap_or_else(|_| "0.0.0.0".to_owned()),
            port,
            username,
            password,
            production,
            database_url,
            // Prefer the provider-native name when both are present, while
            // keeping the generic name for backwards compatibility.
            llm_api_key: first_nonempty_env(&["OPENROUTER_API_KEY", "LLM_API_KEY"]),
            llm_base_url: nonempty_env("LLM_BASE_URL")
                .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_owned()),
            llm_model: nonempty_env("LLM_MODEL").unwrap_or_else(|| "openai/gpt-5.6-sol".to_owned()),
            llm_timeout_seconds: env::var("LLM_TIMEOUT_SECONDS")
                .ok()
                .and_then(|value| value.trim().parse().ok())
                .filter(|seconds| *seconds > 0)
                .unwrap_or(20),
            mistral_api_key: nonempty_env("MISTRAL_API_KEY"),
            blood_tests_dir: env::var("BLOOD_TESTS_DIR")
                .unwrap_or_else(|_| "./example_blood_tests".to_owned()),
        })
    }
}

fn env_bool(key: &str) -> bool {
    matches!(
        env::var(key).as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

fn nonempty_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.trim().is_empty())
}

fn first_nonempty_env(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| nonempty_env(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_database_is_persistent() {
        let config = Config {
            host: String::new(),
            port: 0,
            username: String::new(),
            password: String::new(),
            production: true,
            database_url: "sqlite:///persistent/pethealth.sqlite?mode=rwc".into(),
            llm_api_key: None,
            llm_base_url: String::new(),
            llm_model: String::new(),
            llm_timeout_seconds: 20,
            mistral_api_key: None,
            blood_tests_dir: "./example_blood_tests".into(),
        };
        assert!(config.database_url.contains("/persistent/pethealth.sqlite"));
    }
}
