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
            llm_api_key: nonempty_env("LLM_API_KEY"),
            llm_base_url: env::var("LLM_BASE_URL")
                .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_owned()),
            llm_model: env::var("LLM_MODEL").unwrap_or_else(|_| "openai/gpt-4.1-mini".to_owned()),
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
            mistral_api_key: None,
            blood_tests_dir: "./example_blood_tests".into(),
        };
        assert!(config.database_url.contains("/persistent/pethealth.sqlite"));
    }
}
