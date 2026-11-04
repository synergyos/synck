//! Named bearer tokens in `state.sqlite3`. A token is not a user: it names a
//! device, agent or person, grants `READ` or `WRITE` on listed repositories and
//! is revoked alone. Only the SHA-256 of its secret is stored. Token commands
//! run while the server serves; each request reads the current rows.

use std::fmt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore as _;
use rusqlite::{OptionalExtension as _, params};
use sha2::{Digest as _, Sha256};
use synck_vcs::types::RepoId;

use super::state::open_existing;
use super::{Error, Result};

const PREFIX: &str = "synck_v1_";
const MAX_NAME_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum Access {
    Read,
    Write,
}

impl Access {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "READ",
            Self::Write => "WRITE",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "READ" => Ok(Self::Read),
            "WRITE" => Ok(Self::Write),
            _ => Err(Error::State(format!("unknown token access {text:?}"))),
        }
    }
}

impl fmt::Display for Access {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug)]
pub struct TokenInfo {
    pub name: String,
    pub access: Access,
    pub repositories: Vec<RepoId>,
    pub created_at: SystemTime,
}

/// The secret is shown once, by `add`, and never stored.
struct Secret {
    id: String,
    secret: [u8; 32],
}

impl Secret {
    fn generate() -> Self {
        let mut id = [0_u8; 16];
        let mut secret = [0_u8; 32];
        rand::rng().fill_bytes(&mut id);
        rand::rng().fill_bytes(&mut secret);
        Self {
            id: URL_SAFE_NO_PAD.encode(id),
            secret,
        }
    }

    fn text(&self) -> String {
        format!("{PREFIX}{}_{}", self.id, URL_SAFE_NO_PAD.encode(self.secret))
    }
}

struct Presented {
    id: String,
    secret_sha256: [u8; 32],
}

impl Presented {
    fn parse(text: &str) -> Result<Self> {
        let rest = text.strip_prefix(PREFIX).ok_or(Error::Unauthenticated)?;
        if rest.len() != 66 || rest.as_bytes()[22] != b'_' {
            return Err(Error::Unauthenticated);
        }
        let id = URL_SAFE_NO_PAD
            .decode(&rest[..22])
            .map_err(|_| Error::Unauthenticated)?;
        let secret = URL_SAFE_NO_PAD
            .decode(&rest[23..])
            .map_err(|_| Error::Unauthenticated)?;
        if id.len() != 16 || secret.len() != 32 {
            return Err(Error::Unauthenticated);
        }
        Ok(Self {
            id: rest[..22].to_string(),
            secret_sha256: Sha256::digest(secret).into(),
        })
    }

    fn matches(&self, stored: &[u8]) -> bool {
        stored.len() == 32
            && self
                .secret_sha256
                .iter()
                .zip(stored)
                .fold(0_u8, |difference, (left, right)| difference | (left ^ right))
                == 0
    }
}

fn valid_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(Error::Invalid(format!(
            "a token name is 1 to {MAX_NAME_BYTES} letters, digits, '.', '_' or '-'"
        )));
    }
    Ok(())
}

/// Returns the token text, which is never shown again.
pub fn add(root: &Path, name: &str, access: Access, repositories: &[RepoId]) -> Result<String> {
    valid_name(name)?;
    if repositories.is_empty() {
        return Err(Error::Invalid("grant the token at least one repository".into()));
    }
    let secret = Secret::generate();
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::State(error.to_string()))?
        .as_secs() as i64;
    let mut conn = open_existing(root)?;
    let txn = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let taken: bool = txn.query_row(
        "SELECT EXISTS(SELECT 1 FROM tokens WHERE name = ?1)",
        [name],
        |row| row.get(0),
    )?;
    if taken {
        return Err(Error::Invalid(format!("a token named {name} exists; revoke it first")));
    }
    txn.execute(
        "INSERT INTO tokens (id, name, access, secret_sha256, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            secret.id,
            name,
            access.as_str(),
            Sha256::digest(secret.secret).as_slice(),
            created_at
        ],
    )?;
    for repo in repositories {
        txn.execute(
            "INSERT OR IGNORE INTO token_repositories (token_id, repo_id) VALUES (?1, ?2)",
            params![secret.id, repo.as_str()],
        )?;
    }
    txn.commit()?;
    Ok(secret.text())
}

pub fn list(root: &Path) -> Result<Vec<TokenInfo>> {
    let conn = open_existing(root)?;
    let mut tokens = conn.prepare("SELECT id, name, access, created_at FROM tokens ORDER BY name")?;
    let mut grants =
        conn.prepare("SELECT repo_id FROM token_repositories WHERE token_id = ?1 ORDER BY repo_id")?;
    let rows = tokens.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    let mut infos = Vec::new();
    for row in rows {
        let (id, name, access, created_at) = row?;
        let repositories = grants
            .query_map([&id], |row| row.get::<_, String>(0))?
            .map(|repo| {
                RepoId::parse(repo?).map_err(|error| Error::State(error.to_string()))
            })
            .collect::<Result<_>>()?;
        infos.push(TokenInfo {
            name,
            access: Access::parse(&access)?,
            repositories,
            created_at: UNIX_EPOCH + Duration::from_secs(created_at.max(0) as u64),
        });
    }
    Ok(infos)
}

/// Takes effect at the next request; returns false when no token has the name.
pub fn revoke(root: &Path, name: &str) -> Result<bool> {
    let conn = open_existing(root)?;
    Ok(conn.execute("DELETE FROM tokens WHERE name = ?1", [name])? == 1)
}

/// Unknown or wrong secrets are `Unauthenticated`; a repository the token was
/// not granted is `NotFound`, the same as one that is not served.
pub(crate) fn authorize(
    root: &Path,
    presented: &str,
    repo: Option<&RepoId>,
    need: Access,
) -> Result<()> {
    let presented = Presented::parse(presented)?;
    let conn = open_existing(root)?;
    let row: Option<(String, Vec<u8>)> = conn
        .query_row(
            "SELECT access, secret_sha256 FROM tokens WHERE id = ?1",
            [&presented.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((access, stored)) = row else {
        return Err(Error::Unauthenticated);
    };
    if !presented.matches(&stored) {
        return Err(Error::Unauthenticated);
    }
    let Some(repo) = repo else {
        return Err(Error::NotFound);
    };
    let granted: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM token_repositories WHERE token_id = ?1 AND repo_id = ?2)",
        params![presented.id, repo.as_str()],
        |row| row.get(0),
    )?;
    if !granted {
        return Err(Error::NotFound);
    }
    if Access::parse(&access)? < need {
        return Err(Error::Forbidden);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_text_round_trips_and_malformed_text_is_refused() {
        let secret = Secret::generate();
        let text = secret.text();
        assert_eq!(text.len(), 75);
        let presented = Presented::parse(&text).unwrap();
        assert_eq!(presented.id, secret.id);
        assert!(presented.matches(Sha256::digest(secret.secret).as_slice()));
        for text in ["", "synck_v1_a_b", "Bearer", &text[..74]] {
            assert!(matches!(Presented::parse(text), Err(Error::Unauthenticated)));
        }
    }
}
