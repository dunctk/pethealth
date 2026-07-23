use anyhow::{Context, anyhow};
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use rand::{Rng, distr::Alphanumeric};
use sha2::{Digest, Sha256};

pub const SESSION_DAYS: i64 = 30;

pub async fn hash_password(password: String) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || {
        let mut bytes = [0_u8; 16];
        rand::rng().fill(&mut bytes);
        let salt = SaltString::encode_b64(&bytes)
            .map_err(|error| anyhow!("failed to encode password salt: {error}"))?;
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .map_err(|error| anyhow!("failed to hash password: {error}"))
    })
    .await
    .context("password hashing task failed")?
}

pub async fn verify_password(password: String, encoded: String) -> bool {
    tokio::task::spawn_blocking(move || {
        let Ok(hash) = PasswordHash::new(&encoded) else {
            return false;
        };
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    })
    .await
    .unwrap_or(false)
}

pub fn new_session_token() -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(64)
        .map(char::from)
        .collect()
}

pub fn token_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn passwords_are_salted_and_verified() {
        let first = hash_password("correct horse battery staple".into())
            .await
            .unwrap();
        let second = hash_password("correct horse battery staple".into())
            .await
            .unwrap();
        assert_ne!(first, second);
        assert!(verify_password("correct horse battery staple".into(), first).await);
        assert!(!verify_password("wrong password".into(), second).await);
    }
}
