use crate::{
    auth,
    domain::{
        DEFAULT_HOUSEHOLD_ID, HealthEvent, KnowledgeArticle, Pet, ProposedEvent, ShareGrant,
        UserAccount, event_presentation,
    },
};
use anyhow::{Context, anyhow};
use chrono::{DateTime, Duration, Utc};
use rand::{Rng, distr::Alphanumeric};
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend, QueryResult,
    Statement, TransactionTrait,
};
use sha2::{Digest, Sha256};
use std::{fs, path::Path, time::Duration as StdDuration};

pub async fn connect(url: &str) -> anyhow::Result<DatabaseConnection> {
    create_parent(url)?;
    let mut options = ConnectOptions::new(url.to_owned());
    options
        .max_connections(1)
        .min_connections(1)
        .connect_timeout(StdDuration::from_secs(10))
        .acquire_timeout(StdDuration::from_secs(10))
        .sqlx_logging(false);
    let database = Database::connect(options).await?;
    for pragma in [
        "PRAGMA journal_mode=WAL",
        "PRAGMA synchronous=FULL",
        "PRAGMA foreign_keys=ON",
        "PRAGMA busy_timeout=5000",
    ] {
        database.execute(stmt(pragma)).await?;
    }
    Ok(database)
}

fn create_parent(url: &str) -> anyhow::Result<()> {
    let path = url
        .strip_prefix("sqlite://")
        .and_then(|value| value.split('?').next())
        .unwrap_or(url);
    if path == ":memory:" || path.is_empty() {
        return Ok(());
    }
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create database directory {}", parent.display())
            })?;
        }
    }
    Ok(())
}

pub async fn migrate(db: &DatabaseConnection) -> anyhow::Result<()> {
    db.execute(stmt(
        r#"
        CREATE TABLE IF NOT EXISTS households (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            username TEXT NOT NULL COLLATE NOCASE UNIQUE,
            email TEXT COLLATE NOCASE UNIQUE,
            display_name TEXT NOT NULL,
            password_hash TEXT NOT NULL,
            role TEXT NOT NULL DEFAULT 'owner' CHECK(role IN ('owner','member')),
            created_at TEXT NOT NULL,
            FOREIGN KEY (household_id) REFERENCES households(id)
        );
        CREATE INDEX IF NOT EXISTS idx_users_household ON users(household_id, id);
        CREATE TABLE IF NOT EXISTS sessions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            token_hash TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            revoked_at TEXT,
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_sessions_token_active
            ON sessions(token_hash, expires_at, revoked_at);
        CREATE TABLE IF NOT EXISTS pets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            name TEXT NOT NULL COLLATE NOCASE,
            species TEXT NOT NULL,
            breed TEXT,
            date_of_birth TEXT,
            weight_kg REAL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY (household_id) REFERENCES households(id),
            UNIQUE (household_id, name)
        );
        CREATE INDEX IF NOT EXISTS idx_pets_household ON pets(household_id, name);
        CREATE TABLE IF NOT EXISTS health_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            event_type TEXT NOT NULL,
            concept TEXT NOT NULL,
            summary TEXT NOT NULL,
            raw_input TEXT NOT NULL DEFAULT '',
            details TEXT,
            occurred_at TEXT NOT NULL,
            recorded_at TEXT NOT NULL,
            temporal_precision TEXT NOT NULL DEFAULT 'exact',
            source TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','undone')),
            undone_at TEXT,
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id)
        );
        CREATE INDEX IF NOT EXISTS idx_events_tenant_pet_time
            ON health_events(household_id, pet_id, occurred_at DESC);
        CREATE INDEX IF NOT EXISTS idx_events_tenant_concept
            ON health_events(household_id, concept, occurred_at DESC);
        CREATE TABLE IF NOT EXISTS audit_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            actor TEXT NOT NULL,
            action TEXT NOT NULL,
            record_type TEXT NOT NULL,
            record_id INTEGER NOT NULL,
            detail TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_audit_tenant_time
            ON audit_events(household_id, created_at DESC);
        CREATE TABLE IF NOT EXISTS knowledge_articles (
            concept TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            summary TEXT NOT NULL,
            monitoring TEXT NOT NULL,
            urgent_signs TEXT NOT NULL,
            source_url TEXT
        );
        CREATE TABLE IF NOT EXISTS share_grants (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            label TEXT NOT NULL,
            token_hash TEXT NOT NULL UNIQUE,
            permission TEXT NOT NULL DEFAULT 'read',
            expires_at TEXT NOT NULL,
            revoked_at TEXT,
            created_at TEXT NOT NULL,
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id)
        );
        CREATE INDEX IF NOT EXISTS idx_shares_tenant_pet
            ON share_grants(household_id, pet_id, expires_at DESC);
        "#,
    ))
    .await?;

    let now = Utc::now().to_rfc3339();
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT OR IGNORE INTO households(id, name, created_at) VALUES (?, ?, ?)",
        [
            DEFAULT_HOUSEHOLD_ID.into(),
            "My household".into(),
            now.into(),
        ],
    ))
    .await?;
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"INSERT OR IGNORE INTO knowledge_articles
           (concept,title,summary,monitoring,urgent_signs,source_url)
           VALUES (?,?,?,?,?,?)"#,
        [
            "vomiting".into(),
            "Vomiting".into(),
            "A useful record includes frequency, appearance, food or medication changes, and whether your pet can keep water down.".into(),
            "Record repeat episodes, appetite, drinking, energy, stool, and any suspected material eaten.".into(),
            "Repeated vomiting, blood, severe lethargy, pain, breathing difficulty, collapse, or inability to keep water down warrants prompt veterinary advice.".into(),
            "https://www.vet.cornell.edu/departments-centers-and-institutes/riney-canine-health-center/canine-health-information/vomiting".into(),
        ],
    )).await?;
    Ok(())
}

pub async fn bootstrap_owner(
    db: &DatabaseConnection,
    username: &str,
    password: &str,
) -> anyhow::Result<()> {
    let exists = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id FROM users WHERE username=? COLLATE NOCASE",
            [username.into()],
        ))
        .await?
        .is_some();
    if exists {
        return Ok(());
    }
    let password_hash = auth::hash_password(password.to_owned()).await?;
    let now = Utc::now().to_rfc3339();
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"INSERT INTO users(household_id,username,email,display_name,password_hash,role,created_at)
           VALUES(?,?,?,?,?,'owner',?)"#,
        [
            DEFAULT_HOUSEHOLD_ID.into(),
            username.into(),
            Option::<String>::None.into(),
            "Owner".into(),
            password_hash.into(),
            now.into(),
        ],
    ))
    .await?;
    Ok(())
}

pub async fn create_account(
    db: &DatabaseConnection,
    email: &str,
    display_name: &str,
    password_hash: &str,
) -> anyhow::Result<UserAccount> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let household_row = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO households(name,created_at) VALUES(?,?) RETURNING id",
            [
                format!("{display_name}'s household").into(),
                now.clone().into(),
            ],
        ))
        .await?
        .ok_or_else(|| anyhow!("household insert returned no id"))?;
    let household_id: i64 = household_row.try_get("", "id")?;
    let user_row = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"INSERT INTO users(household_id,username,email,display_name,password_hash,role,created_at)
               VALUES(?,?,?,?,?,'owner',?) RETURNING id"#,
            [
                household_id.into(),
                email.into(),
                email.into(),
                display_name.into(),
                password_hash.into(),
                now.clone().into(),
            ],
        ))
        .await?
        .ok_or_else(|| anyhow!("user insert returned no id"))?;
    let id: i64 = user_row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        &format!("user:{id}"),
        "account.created",
        "user",
        id,
        "Owner account created",
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(UserAccount {
        id,
        household_id,
        username: email.into(),
        email: Some(email.into()),
        display_name: display_name.into(),
        initials: initials(display_name),
    })
}

pub async fn user_for_login(
    db: &DatabaseConnection,
    identifier: &str,
) -> anyhow::Result<Option<(UserAccount, String)>> {
    db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"SELECT id,household_id,username,email,display_name,password_hash FROM users
           WHERE username=? COLLATE NOCASE OR email=? COLLATE NOCASE LIMIT 1"#,
        [identifier.into(), identifier.into()],
    ))
    .await?
    .map(user_with_password_from_row)
    .transpose()
}

pub async fn create_session(db: &DatabaseConnection, user_id: i64) -> anyhow::Result<String> {
    let token = auth::new_session_token();
    let token_hash = auth::token_hash(&token);
    let now = Utc::now();
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO sessions(user_id,token_hash,created_at,expires_at) VALUES(?,?,?,?)",
        [
            user_id.into(),
            token_hash.into(),
            now.to_rfc3339().into(),
            (now + Duration::days(auth::SESSION_DAYS))
                .to_rfc3339()
                .into(),
        ],
    ))
    .await?;
    Ok(token)
}

pub async fn resolve_session(
    db: &DatabaseConnection,
    token: &str,
) -> anyhow::Result<Option<UserAccount>> {
    let token_hash = auth::token_hash(token);
    db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"SELECT u.id,u.household_id,u.username,u.email,u.display_name
           FROM sessions s JOIN users u ON u.id=s.user_id
           WHERE s.token_hash=? AND s.revoked_at IS NULL AND s.expires_at>?"#,
        [token_hash.into(), Utc::now().to_rfc3339().into()],
    ))
    .await?
    .map(user_from_row)
    .transpose()
}

pub async fn revoke_session(db: &DatabaseConnection, token: &str) -> anyhow::Result<()> {
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE sessions SET revoked_at=? WHERE token_hash=? AND revoked_at IS NULL",
        [
            Utc::now().to_rfc3339().into(),
            auth::token_hash(token).into(),
        ],
    ))
    .await?;
    Ok(())
}

pub async fn update_password_and_revoke_sessions(
    db: &DatabaseConnection,
    user: &UserAccount,
    password_hash: &str,
) -> anyhow::Result<()> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let result = transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE users SET password_hash=? WHERE id=? AND household_id=?",
            [
                password_hash.into(),
                user.id.into(),
                user.household_id.into(),
            ],
        ))
        .await?;
    if result.rows_affected() != 1 {
        return Err(anyhow!("account was not found in its household"));
    }
    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE sessions SET revoked_at=? WHERE user_id=? AND revoked_at IS NULL",
            [now.clone().into(), user.id.into()],
        ))
        .await?;
    audit(
        &transaction,
        user.household_id,
        &user.audit_actor(),
        "account.password_changed",
        "user",
        user.id,
        "Password changed and sessions revoked",
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(())
}

pub async fn list_pets(db: &DatabaseConnection, household_id: i64) -> anyhow::Result<Vec<Pet>> {
    let rows = db.query_all(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT id,name,species,breed,date_of_birth,weight_kg FROM pets WHERE household_id=? ORDER BY name",
        [household_id.into()],
    )).await?;
    rows.into_iter().map(pet_from_row).collect()
}

pub async fn get_pet(
    db: &DatabaseConnection,
    household_id: i64,
    id: i64,
) -> anyhow::Result<Option<Pet>> {
    db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT id,name,species,breed,date_of_birth,weight_kg FROM pets WHERE household_id=? AND id=?",
        [household_id.into(), id.into()],
    )).await?.map(pet_from_row).transpose()
}

pub async fn find_pet_by_name(
    db: &DatabaseConnection,
    household_id: i64,
    name: &str,
) -> anyhow::Result<Option<Pet>> {
    db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT id,name,species,breed,date_of_birth,weight_kg FROM pets WHERE household_id=? AND name=? COLLATE NOCASE",
        [household_id.into(), name.into()],
    )).await?.map(pet_from_row).transpose()
}

pub async fn create_pet(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    name: &str,
    species: &str,
    breed: Option<&str>,
    weight_kg: Option<f64>,
) -> anyhow::Result<i64> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let row = transaction.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO pets(household_id,name,species,breed,weight_kg,created_at,updated_at) VALUES(?,?,?,?,?,?,?) RETURNING id",
        [household_id.into(), name.into(), species.into(), breed.into(), weight_kg.into(), now.clone().into(), now.clone().into()],
    )).await?.ok_or_else(|| anyhow!("pet insert returned no id"))?;
    let id: i64 = row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        actor,
        "pet.created",
        "pet",
        id,
        name,
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(id)
}

pub async fn create_health_event(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet: &Pet,
    proposal: &ProposedEvent,
    raw_input: &str,
    occurred_at: DateTime<Utc>,
    source: &str,
) -> anyhow::Result<i64> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let row = transaction.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"INSERT INTO health_events
           (household_id,pet_id,event_type,concept,summary,raw_input,details,occurred_at,recorded_at,temporal_precision,source)
           VALUES(?,?,?,?,?,?,?,?,?,?,?) RETURNING id"#,
        [
            household_id.into(), pet.id.into(), proposal.event_type.clone().into(),
            proposal.concept.clone().into(), proposal.summary.clone().into(), raw_input.into(),
            proposal.details.clone().into(), occurred_at.to_rfc3339().into(), now.clone().into(),
            (if proposal.minutes_ago.is_some() { "inferred_relative" } else { "inferred_now" }).into(), source.into(),
        ],
    )).await?.ok_or_else(|| anyhow!("event insert returned no id"))?;
    let id: i64 = row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        actor,
        "event.created",
        "health_event",
        id,
        raw_input,
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(id)
}

pub async fn undo_event(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    event_id: i64,
) -> anyhow::Result<bool> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let result = transaction.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE health_events SET status='undone', undone_at=? WHERE household_id=? AND id=? AND status='active'",
        [now.clone().into(), household_id.into(), event_id.into()],
    )).await?;
    if result.rows_affected() == 1 {
        audit(
            &transaction,
            household_id,
            actor,
            "event.undone",
            "health_event",
            event_id,
            "Undo from timeline",
            &now,
        )
        .await?;
    }
    transaction.commit().await?;
    Ok(result.rows_affected() == 1)
}

pub async fn list_events(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: Option<i64>,
    limit: u64,
) -> anyhow::Result<Vec<HealthEvent>> {
    let (sql, values) = if let Some(pet_id) = pet_id {
        (
            r#"SELECT e.*,p.name AS pet_name FROM health_events e JOIN pets p ON p.id=e.pet_id
            WHERE e.household_id=? AND e.pet_id=? AND e.status='active' ORDER BY e.occurred_at DESC LIMIT ?"#,
            vec![household_id.into(), pet_id.into(), limit.into()],
        )
    } else {
        (
            r#"SELECT e.*,p.name AS pet_name FROM health_events e JOIN pets p ON p.id=e.pet_id
            WHERE e.household_id=? AND e.status='active' ORDER BY e.occurred_at DESC LIMIT ?"#,
            vec![household_id.into(), limit.into()],
        )
    };
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            sql,
            values,
        ))
        .await?;
    rows.into_iter().map(event_from_row).collect()
}

pub async fn count_related(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    concept: &str,
) -> anyhow::Result<u64> {
    let row = db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT COUNT(*) AS count FROM health_events WHERE household_id=? AND pet_id=? AND concept=? AND status='active'",
        [household_id.into(), pet_id.into(), concept.into()],
    )).await?.ok_or_else(|| anyhow!("count returned no row"))?;
    let count: i64 = row.try_get("", "count")?;
    Ok(count as u64)
}

pub async fn get_knowledge(
    db: &DatabaseConnection,
    concept: &str,
) -> anyhow::Result<Option<KnowledgeArticle>> {
    db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT concept,title,summary,monitoring,urgent_signs,source_url FROM knowledge_articles WHERE concept=?",
        [concept.into()],
    )).await?.map(|row| Ok(KnowledgeArticle {
        concept: row.try_get("", "concept")?, title: row.try_get("", "title")?,
        summary: row.try_get("", "summary")?, monitoring: row.try_get("", "monitoring")?,
        urgent_signs: row.try_get("", "urgent_signs")?, source_url: row.try_get("", "source_url")?,
    })).transpose()
}

pub async fn create_share(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet_id: i64,
    label: &str,
    days: i64,
) -> anyhow::Result<ShareGrant> {
    let token: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(48)
        .map(char::from)
        .collect();
    let hash = token_hash(&token);
    let now = Utc::now();
    let expires = now + Duration::days(days.clamp(1, 90));
    let transaction = db.begin().await?;
    let row = transaction.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO share_grants(household_id,pet_id,label,token_hash,expires_at,created_at) VALUES(?,?,?,?,?,?) RETURNING id",
        [household_id.into(), pet_id.into(), label.into(), hash.into(), expires.to_rfc3339().into(), now.to_rfc3339().into()],
    )).await?.ok_or_else(|| anyhow!("share insert returned no id"))?;
    let id: i64 = row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        actor,
        "share.created",
        "share_grant",
        id,
        label,
        &now.to_rfc3339(),
    )
    .await?;
    transaction.commit().await?;
    let pet = get_pet(db, household_id, pet_id)
        .await?
        .ok_or_else(|| anyhow!("pet not found"))?;
    Ok(ShareGrant {
        id,
        household_id,
        pet_id,
        pet_name: pet.name,
        label: label.into(),
        token: Some(token),
        expires_at: expires,
        revoked_at: None,
        status: "active".into(),
    })
}

pub async fn list_shares(
    db: &DatabaseConnection,
    household_id: i64,
) -> anyhow::Result<Vec<ShareGrant>> {
    let rows = db.query_all(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"SELECT s.id,s.household_id,s.pet_id,p.name AS pet_name,s.label,s.expires_at,s.revoked_at
            FROM share_grants s JOIN pets p ON p.id=s.pet_id WHERE s.household_id=? ORDER BY s.created_at DESC"#,
        [household_id.into()],
    )).await?;
    rows.iter().map(share_from_row).collect()
}

pub async fn revoke_share(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    id: i64,
) -> anyhow::Result<bool> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let result = transaction.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE share_grants SET revoked_at=? WHERE household_id=? AND id=? AND revoked_at IS NULL",
        [now.clone().into(), household_id.into(), id.into()],
    )).await?;
    if result.rows_affected() == 1 {
        audit(
            &transaction,
            household_id,
            actor,
            "share.revoked",
            "share_grant",
            id,
            "Revoked by owner",
            &now,
        )
        .await?;
    }
    transaction.commit().await?;
    Ok(result.rows_affected() == 1)
}

pub async fn resolve_share(
    db: &DatabaseConnection,
    token: &str,
) -> anyhow::Result<Option<(ShareGrant, Pet)>> {
    let hash = token_hash(token);
    let Some(row) = db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"SELECT s.id,s.pet_id,p.name AS pet_name,s.label,s.expires_at,s.revoked_at,s.household_id,
                  p.species,p.breed,p.date_of_birth,p.weight_kg
            FROM share_grants s JOIN pets p ON p.id=s.pet_id
            WHERE s.token_hash=? AND s.revoked_at IS NULL AND s.expires_at>?"#,
        [hash.into(), Utc::now().to_rfc3339().into()],
    )).await? else { return Ok(None); };
    let grant = share_from_row(&row)?;
    let pet = Pet {
        id: row.try_get("", "pet_id")?,
        name: row.try_get("", "pet_name")?,
        species: row.try_get("", "species")?,
        breed: row.try_get("", "breed")?,
        date_of_birth: row.try_get("", "date_of_birth")?,
        weight_kg: row.try_get("", "weight_kg")?,
        initials: initials(&row.try_get::<String>("", "pet_name")?),
    };
    Ok(Some((grant, pet)))
}

async fn audit<C: ConnectionTrait>(
    db: &C,
    household_id: i64,
    actor: &str,
    action: &str,
    record_type: &str,
    record_id: i64,
    detail: &str,
    created_at: &str,
) -> anyhow::Result<()> {
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO audit_events(household_id,actor,action,record_type,record_id,detail,created_at) VALUES(?,?,?,?,?,?,?)",
        [household_id.into(), actor.into(), action.into(), record_type.into(), record_id.into(), detail.into(), created_at.into()],
    )).await?;
    Ok(())
}

fn user_from_row(row: QueryResult) -> anyhow::Result<UserAccount> {
    let display_name: String = row.try_get("", "display_name")?;
    Ok(UserAccount {
        id: row.try_get("", "id")?,
        household_id: row.try_get("", "household_id")?,
        username: row.try_get("", "username")?,
        email: row.try_get("", "email")?,
        initials: initials(&display_name),
        display_name,
    })
}

fn user_with_password_from_row(row: QueryResult) -> anyhow::Result<(UserAccount, String)> {
    let password_hash = row.try_get("", "password_hash")?;
    Ok((user_from_row(row)?, password_hash))
}

fn pet_from_row(row: QueryResult) -> anyhow::Result<Pet> {
    let name: String = row.try_get("", "name")?;
    Ok(Pet {
        id: row.try_get("", "id")?,
        initials: initials(&name),
        name,
        species: row.try_get("", "species")?,
        breed: row.try_get("", "breed")?,
        date_of_birth: row.try_get("", "date_of_birth")?,
        weight_kg: row.try_get("", "weight_kg")?,
    })
}

fn event_from_row(row: QueryResult) -> anyhow::Result<HealthEvent> {
    let event_type: String = row.try_get("", "event_type")?;
    let concept: String = row.try_get("", "concept")?;
    let occurred_at = DateTime::parse_from_rfc3339(&row.try_get::<String>("", "occurred_at")?)?
        .with_timezone(&Utc);
    let recorded_at = DateTime::parse_from_rfc3339(&row.try_get::<String>("", "recorded_at")?)?
        .with_timezone(&Utc);
    let (icon, tone) = event_presentation(&event_type, &concept);
    Ok(HealthEvent {
        id: row.try_get("", "id")?,
        pet_id: row.try_get("", "pet_id")?,
        pet_name: row.try_get("", "pet_name")?,
        event_type,
        concept,
        summary: row.try_get("", "summary")?,
        raw_input: row.try_get("", "raw_input")?,
        details: row.try_get("", "details")?,
        occurred_at,
        occurred_label: relative_time(occurred_at),
        recorded_at,
        source: row.try_get("", "source")?,
        status: row.try_get("", "status")?,
        icon,
        tone,
    })
}

fn share_from_row(row: &QueryResult) -> anyhow::Result<ShareGrant> {
    let expires_at = DateTime::parse_from_rfc3339(&row.try_get::<String>("", "expires_at")?)?
        .with_timezone(&Utc);
    let revoked_raw: Option<String> = row.try_get("", "revoked_at")?;
    let revoked_at = revoked_raw
        .map(|value| DateTime::parse_from_rfc3339(&value).map(|v| v.with_timezone(&Utc)))
        .transpose()?;
    let status = if revoked_at.is_some() {
        "revoked"
    } else if expires_at < Utc::now() {
        "expired"
    } else {
        "active"
    };
    Ok(ShareGrant {
        id: row.try_get("", "id")?,
        household_id: row.try_get("", "household_id")?,
        pet_id: row.try_get("", "pet_id")?,
        pet_name: row.try_get("", "pet_name")?,
        label: row.try_get("", "label")?,
        token: None,
        expires_at,
        revoked_at,
        status: status.into(),
    })
}

fn token_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}
fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|part| part.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}
fn relative_time(at: DateTime<Utc>) -> String {
    let seconds = (Utc::now() - at).num_seconds().max(0);
    match seconds {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86399 => format!("{}h ago", seconds / 3600),
        _ => at.format("%d %b, %H:%M UTC").to_string(),
    }
}
fn stmt(sql: &str) -> Statement {
    Statement::from_string(DbBackend::Sqlite, sql.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> DatabaseConnection {
        let db = connect("sqlite::memory:").await.unwrap();
        migrate(&db).await.unwrap();
        db
    }

    #[tokio::test]
    async fn creates_and_undoes_tenant_scoped_event() {
        let db = test_db().await;
        let pet_id = create_pet(&db, 1, "user:1", "Milo", "Cat", None, None)
            .await
            .unwrap();
        let pet = get_pet(&db, 1, pet_id).await.unwrap().unwrap();
        let event_id = create_health_event(
            &db,
            1,
            "user:1",
            &pet,
            &ProposedEvent {
                pet_name: "Milo".into(),
                event_type: "symptom".into(),
                concept: "vomiting".into(),
                summary: "Vomited".into(),
                details: None,
                minutes_ago: None,
            },
            "Milo vomited just now",
            Utc::now(),
            "owner_agent",
        )
        .await
        .unwrap();
        assert_eq!(
            list_events(&db, 1, Some(pet_id), 20).await.unwrap().len(),
            1
        );
        assert!(!undo_event(&db, 99, "user:99", event_id).await.unwrap());
        assert!(undo_event(&db, 1, "user:1", event_id).await.unwrap());
        assert!(
            list_events(&db, 1, Some(pet_id), 20)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn accounts_sessions_and_pets_are_household_scoped() {
        let db = test_db().await;
        let alice = create_account(&db, "alice@example.com", "Alice", "hash-a")
            .await
            .unwrap();
        let bob = create_account(&db, "bob@example.com", "Bob", "hash-b")
            .await
            .unwrap();
        let pet_id = create_pet(
            &db,
            alice.household_id,
            &alice.audit_actor(),
            "Milo",
            "Cat",
            None,
            None,
        )
        .await
        .unwrap();

        assert!(
            get_pet(&db, bob.household_id, pet_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(list_pets(&db, bob.household_id).await.unwrap().is_empty());

        let token = create_session(&db, alice.id).await.unwrap();
        let resolved = resolve_session(&db, &token).await.unwrap().unwrap();
        assert_eq!(resolved.id, alice.id);
        assert_eq!(resolved.household_id, alice.household_id);
        update_password_and_revoke_sessions(&db, &alice, "new-hash")
            .await
            .unwrap();
        assert!(resolve_session(&db, &token).await.unwrap().is_none());
        let (_, stored_hash) = user_for_login(&db, "alice@example.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored_hash, "new-hash");

        let second_token = create_session(&db, alice.id).await.unwrap();
        revoke_session(&db, &second_token).await.unwrap();
        assert!(resolve_session(&db, &second_token).await.unwrap().is_none());
    }
}
