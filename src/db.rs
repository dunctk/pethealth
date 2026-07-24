use crate::{
    auth,
    domain::{
        ClinicalTimeline, DEFAULT_HOUSEHOLD_ID, HealthEvent, KnowledgeArticle, MedicationAdherence,
        MedicationAdministration, MedicationPlan, MedicationPrescription, Pet, ProposedEvent,
        ShareGrant, SymptomObservation, TemporalLink, UserAccount, event_presentation,
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
        CREATE TABLE IF NOT EXISTS symptom_observations (
            event_id INTEGER PRIMARY KEY,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            episode_id TEXT NOT NULL,
            symptom TEXT NOT NULL,
            occurrence_count INTEGER,
            amount TEXT,
            contents TEXT,
            meal_relation TEXT,
            water_status TEXT,
            appetite_status TEXT,
            energy_status TEXT,
            pain_status TEXT,
            note TEXT,
            FOREIGN KEY (event_id) REFERENCES health_events(id) ON DELETE CASCADE,
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id)
        );
        CREATE INDEX IF NOT EXISTS idx_symptoms_tenant_pet_episode
            ON symptom_observations(household_id, pet_id, symptom, episode_id);
        CREATE TABLE IF NOT EXISTS medication_administrations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            name TEXT NOT NULL,
            active_ingredient TEXT,
            dose_value REAL,
            dose_unit TEXT,
            route TEXT,
            scheduled_at TEXT,
            administered_at TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'given',
            raw_input TEXT,
            created_at TEXT NOT NULL,
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id)
        );
        CREATE INDEX IF NOT EXISTS idx_meds_tenant_pet_time
            ON medication_administrations(household_id, pet_id, administered_at DESC);
        CREATE TABLE IF NOT EXISTS medication_prescriptions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            name TEXT NOT NULL,
            active_ingredient TEXT,
            concentration_value REAL,
            concentration_unit TEXT,
            dose_value REAL,
            dose_unit TEXT,
            frequency TEXT,
            route TEXT,
            instructions TEXT,
            started_on TEXT,
            ended_on TEXT,
            status TEXT NOT NULL DEFAULT 'active',
            raw_input TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id)
        );
        CREATE INDEX IF NOT EXISTS idx_prescriptions_tenant_pet_status
            ON medication_prescriptions(household_id, pet_id, status, created_at DESC);
        CREATE TABLE IF NOT EXISTS medication_adherence (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            prescription_id INTEGER NOT NULL,
            scheduled_for TEXT NOT NULL,
            expected_dose_value REAL,
            expected_dose_unit TEXT,
            actual_dose_value REAL,
            actual_dose_unit TEXT,
            status TEXT NOT NULL,
            reason TEXT,
            raw_input TEXT,
            recorded_at TEXT NOT NULL,
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id),
            FOREIGN KEY (prescription_id) REFERENCES medication_prescriptions(id)
        );
        CREATE INDEX IF NOT EXISTS idx_adherence_tenant_pet_date
            ON medication_adherence(household_id, pet_id, scheduled_for DESC);
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
        CREATE TABLE IF NOT EXISTS weight_entries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            weight_kg REAL NOT NULL,
            measured_at TEXT NOT NULL,
            note TEXT,
            created_at TEXT NOT NULL,
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id)
        );
        CREATE INDEX IF NOT EXISTS idx_weights_tenant_pet_time
            ON weight_entries(household_id, pet_id, measured_at DESC);
        CREATE TABLE IF NOT EXISTS lab_reports (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            household_id INTEGER NOT NULL,
            pet_id INTEGER NOT NULL,
            source_filename TEXT NOT NULL,
            document_hash TEXT NOT NULL,
            raw_text TEXT NOT NULL,
            test_date TEXT,
            imported_at TEXT NOT NULL,
            parse_status TEXT NOT NULL DEFAULT 'needs_review',
            FOREIGN KEY (household_id) REFERENCES households(id),
            FOREIGN KEY (pet_id) REFERENCES pets(id),
            UNIQUE (household_id, document_hash)
        );
        CREATE INDEX IF NOT EXISTS idx_lab_reports_tenant_pet
            ON lab_reports(household_id, pet_id, test_date DESC);
        CREATE TABLE IF NOT EXISTS lab_results (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            report_id INTEGER NOT NULL,
            test_name TEXT NOT NULL,
            value_text TEXT NOT NULL,
            value_numeric REAL,
            unit TEXT,
            reference_range TEXT,
            flag TEXT,
            FOREIGN KEY (report_id) REFERENCES lab_reports(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_lab_results_report ON lab_results(report_id, test_name);
        CREATE TABLE IF NOT EXISTS oauth_clients (
            client_id TEXT PRIMARY KEY,
            client_name TEXT NOT NULL,
            redirect_uris TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS oauth_codes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            code_hash TEXT NOT NULL UNIQUE,
            client_id TEXT NOT NULL,
            user_id INTEGER NOT NULL,
            redirect_uri TEXT NOT NULL,
            code_challenge TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            used_at TEXT,
            FOREIGN KEY (client_id) REFERENCES oauth_clients(client_id),
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_oauth_codes_active ON oauth_codes(code_hash, expires_at, used_at);
        CREATE TABLE IF NOT EXISTS oauth_tokens (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            access_token_hash TEXT NOT NULL UNIQUE,
            refresh_token_hash TEXT NOT NULL UNIQUE,
            client_id TEXT NOT NULL,
            user_id INTEGER NOT NULL,
            access_expires_at TEXT NOT NULL,
            refresh_expires_at TEXT NOT NULL,
            revoked_at TEXT,
            FOREIGN KEY (client_id) REFERENCES oauth_clients(client_id),
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_oauth_access_active ON oauth_tokens(access_token_hash, access_expires_at, revoked_at);
        CREATE INDEX IF NOT EXISTS idx_oauth_refresh_active ON oauth_tokens(refresh_token_hash, refresh_expires_at, revoked_at);
        CREATE TABLE IF NOT EXISTS oauth_device_codes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            device_code_hash TEXT NOT NULL UNIQUE,
            user_code_hash TEXT NOT NULL UNIQUE,
            client_id TEXT NOT NULL,
            code_challenge TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            user_id INTEGER,
            approved_at TEXT,
            consumed_at TEXT,
            FOREIGN KEY (client_id) REFERENCES oauth_clients(client_id),
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_oauth_device_active
            ON oauth_device_codes(device_code_hash, client_id, expires_at, consumed_at);
        CREATE INDEX IF NOT EXISTS idx_oauth_user_code_active
            ON oauth_device_codes(user_code_hash, expires_at, consumed_at);
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

pub async fn create_oauth_client(
    db: &DatabaseConnection,
    client_id: &str,
    client_name: &str,
    redirect_uris: &[String],
) -> anyhow::Result<()> {
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO oauth_clients(client_id,client_name,redirect_uris,created_at) VALUES(?,?,?,?)",
        [
            client_id.into(),
            client_name.into(),
            serde_json::to_string(redirect_uris)?.into(),
            Utc::now().to_rfc3339().into(),
        ],
    ))
    .await?;
    Ok(())
}

pub async fn oauth_client_redirects(
    db: &DatabaseConnection,
    client_id: &str,
) -> anyhow::Result<Option<Vec<String>>> {
    let Some(row) = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT redirect_uris FROM oauth_clients WHERE client_id=?",
            [client_id.into()],
        ))
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_str(
        &row.try_get::<String>("", "redirect_uris")?,
    )?))
}

pub async fn create_oauth_code(
    db: &DatabaseConnection,
    code_hash: &str,
    client_id: &str,
    user_id: i64,
    redirect_uri: &str,
    code_challenge: &str,
) -> anyhow::Result<()> {
    let now = Utc::now();
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO oauth_codes(code_hash,client_id,user_id,redirect_uri,code_challenge,expires_at) VALUES(?,?,?,?,?,?)",
        [
            code_hash.into(),
            client_id.into(),
            user_id.into(),
            redirect_uri.into(),
            code_challenge.into(),
            (now + Duration::minutes(5)).to_rfc3339().into(),
        ],
    ))
    .await?;
    Ok(())
}

pub async fn redeem_oauth_code(
    db: &DatabaseConnection,
    code_hash: &str,
    client_id: &str,
    redirect_uri: &str,
) -> anyhow::Result<Option<(i64, String)>> {
    let transaction = db.begin().await?;
    let Some(row) = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id,user_id,redirect_uri,code_challenge FROM oauth_codes WHERE code_hash=? AND client_id=? AND expires_at>? AND used_at IS NULL",
            [
                code_hash.into(),
                client_id.into(),
                Utc::now().to_rfc3339().into(),
            ],
        ))
        .await?
    else {
        return Ok(None);
    };
    let stored_redirect: String = row.try_get("", "redirect_uri")?;
    if stored_redirect != redirect_uri {
        return Ok(None);
    }
    let id: i64 = row.try_get("", "id")?;
    let result = transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE oauth_codes SET used_at=? WHERE id=? AND used_at IS NULL",
            [Utc::now().to_rfc3339().into(), id.into()],
        ))
        .await?;
    if result.rows_affected() != 1 {
        return Ok(None);
    }
    transaction.commit().await?;
    Ok(Some((
        row.try_get("", "user_id")?,
        row.try_get("", "code_challenge")?,
    )))
}

pub async fn create_oauth_tokens(
    db: &DatabaseConnection,
    client_id: &str,
    user_id: i64,
) -> anyhow::Result<(String, String, i64)> {
    let access = auth::new_session_token();
    let refresh = auth::new_session_token();
    let now = Utc::now();
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO oauth_tokens(access_token_hash,refresh_token_hash,client_id,user_id,access_expires_at,refresh_expires_at) VALUES(?,?,?,?,?,?)",
        [
            auth::token_hash(&access).into(),
            auth::token_hash(&refresh).into(),
            client_id.into(),
            user_id.into(),
            (now + Duration::hours(1)).to_rfc3339().into(),
            (now + Duration::days(30)).to_rfc3339().into(),
        ],
    ))
    .await?;
    Ok((access, refresh, 3600))
}

pub async fn resolve_oauth_access_token(
    db: &DatabaseConnection,
    token: &str,
) -> anyhow::Result<Option<UserAccount>> {
    db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT u.id,u.household_id,u.username,u.email,u.display_name FROM oauth_tokens t JOIN users u ON u.id=t.user_id WHERE t.access_token_hash=? AND t.access_expires_at>? AND t.revoked_at IS NULL",
        [
            auth::token_hash(token).into(),
            Utc::now().to_rfc3339().into(),
        ],
    ))
    .await?
    .map(user_from_row)
    .transpose()
}

pub async fn refresh_oauth_tokens(
    db: &DatabaseConnection,
    refresh_token: &str,
    client_id: &str,
) -> anyhow::Result<Option<(String, String, i64)>> {
    let hash = auth::token_hash(refresh_token);
    let Some(row) = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT user_id FROM oauth_tokens WHERE refresh_token_hash=? AND client_id=? AND refresh_expires_at>? AND revoked_at IS NULL",
            [hash.clone().into(), client_id.into(), Utc::now().to_rfc3339().into()],
        ))
        .await?
    else {
        return Ok(None);
    };
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE oauth_tokens SET revoked_at=? WHERE refresh_token_hash=?",
        [Utc::now().to_rfc3339().into(), hash.into()],
    ))
    .await?;
    Ok(Some(
        create_oauth_tokens(db, client_id, row.try_get("", "user_id")?).await?,
    ))
}

pub async fn create_oauth_device_code(
    db: &DatabaseConnection,
    device_code_hash: &str,
    user_code_hash: &str,
    client_id: &str,
    code_challenge: &str,
) -> anyhow::Result<()> {
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO oauth_device_codes(device_code_hash,user_code_hash,client_id,code_challenge,expires_at) VALUES(?,?,?,?,?)",
        [
            device_code_hash.into(),
            user_code_hash.into(),
            client_id.into(),
            code_challenge.into(),
            (Utc::now() + Duration::minutes(10)).to_rfc3339().into(),
        ],
    ))
    .await?;
    Ok(())
}

pub async fn oauth_device_code_state(
    db: &DatabaseConnection,
    device_code_hash: &str,
    client_id: &str,
) -> anyhow::Result<Option<(Option<i64>, String)>> {
    db.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT user_id,code_challenge FROM oauth_device_codes WHERE device_code_hash=? AND client_id=? AND expires_at>? AND consumed_at IS NULL",
        [
            device_code_hash.into(),
            client_id.into(),
            Utc::now().to_rfc3339().into(),
        ],
    ))
    .await?
    .map(|row| {
        Ok((
            row.try_get("", "user_id")?,
            row.try_get("", "code_challenge")?,
        ))
    })
    .transpose()
}

pub async fn oauth_device_user_code_exists(
    db: &DatabaseConnection,
    user_code_hash: &str,
) -> anyhow::Result<bool> {
    Ok(db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id FROM oauth_device_codes WHERE user_code_hash=? AND expires_at>? AND consumed_at IS NULL",
            [user_code_hash.into(), Utc::now().to_rfc3339().into()],
        ))
        .await?
        .is_some())
}

pub async fn approve_oauth_device_code(
    db: &DatabaseConnection,
    user_code_hash: &str,
    user_id: i64,
) -> anyhow::Result<bool> {
    let result = db
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE oauth_device_codes SET user_id=?,approved_at=? WHERE user_code_hash=? AND expires_at>? AND approved_at IS NULL AND consumed_at IS NULL",
            [
                user_id.into(),
                Utc::now().to_rfc3339().into(),
                user_code_hash.into(),
                Utc::now().to_rfc3339().into(),
            ],
        ))
        .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn consume_oauth_device_code(
    db: &DatabaseConnection,
    device_code_hash: &str,
    client_id: &str,
    user_id: i64,
) -> anyhow::Result<bool> {
    let result = db
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE oauth_device_codes SET consumed_at=? WHERE device_code_hash=? AND client_id=? AND user_id=? AND expires_at>? AND consumed_at IS NULL",
            [
                Utc::now().to_rfc3339().into(),
                device_code_hash.into(),
                client_id.into(),
                user_id.into(),
                Utc::now().to_rfc3339().into(),
            ],
        ))
        .await?;
    Ok(result.rows_affected() == 1)
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

pub async fn create_weight(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet_id: i64,
    weight_kg: f64,
    measured_at: &str,
    note: Option<&str>,
) -> anyhow::Result<i64> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let row = transaction.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO weight_entries(household_id,pet_id,weight_kg,measured_at,note,created_at) VALUES(?,?,?,?,?,?) RETURNING id",
        [household_id.into(), pet_id.into(), weight_kg.into(), measured_at.into(), note.into(), now.clone().into()],
    )).await?.ok_or_else(|| anyhow!("weight insert returned no id"))?;
    let id: i64 = row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        actor,
        "weight.created",
        "weight_entry",
        id,
        &format!("{weight_kg:.2} kg"),
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(id)
}

pub async fn list_weights(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
) -> anyhow::Result<Vec<crate::domain::WeightEntry>> {
    let rows = db.query_all(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT id,pet_id,weight_kg,measured_at,note FROM weight_entries WHERE household_id=? AND pet_id=? ORDER BY measured_at DESC LIMIT 100",
        [household_id.into(), pet_id.into()],
    )).await?;
    rows.into_iter()
        .map(|row| {
            Ok(crate::domain::WeightEntry {
                id: row.try_get("", "id")?,
                pet_id: row.try_get("", "pet_id")?,
                weight_kg: row.try_get("", "weight_kg")?,
                measured_at: row
                    .try_get::<String>("", "measured_at")?
                    .parse()
                    .context("invalid weight timestamp")?,
                note: row.try_get("", "note")?,
            })
        })
        .collect()
}

pub async fn report_hash_exists(
    db: &DatabaseConnection,
    household_id: i64,
    document_hash: &str,
) -> anyhow::Result<bool> {
    Ok(db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id FROM lab_reports WHERE household_id=? AND document_hash=?",
            [household_id.into(), document_hash.into()],
        ))
        .await?
        .is_some())
}

pub async fn create_lab_report(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet_id: i64,
    source_filename: &str,
    document_hash: &str,
    raw_text: &str,
    test_date: Option<&str>,
    results: &[crate::ocr::ParsedLabResult],
) -> anyhow::Result<i64> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let row = transaction.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "INSERT INTO lab_reports(household_id,pet_id,source_filename,document_hash,raw_text,test_date,imported_at,parse_status) VALUES(?,?,?,?,?,?,?,?) RETURNING id",
        [household_id.into(), pet_id.into(), source_filename.into(), document_hash.into(), raw_text.into(), test_date.into(), now.clone().into(), (if results.is_empty() { "needs_review" } else { "parsed" }).into()],
    )).await?.ok_or_else(|| anyhow!("lab report insert returned no id"))?;
    let report_id: i64 = row.try_get("", "id")?;
    for result in results {
        transaction.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO lab_results(report_id,test_name,value_text,value_numeric,unit,reference_range,flag) VALUES(?,?,?,?,?,?,?)",
            [report_id.into(), result.test_name.clone().into(), result.value_text.clone().into(), result.value_numeric.into(), result.unit.clone().into(), result.reference_range.clone().into(), result.flag.clone().into()],
        )).await?;
    }
    audit(
        &transaction,
        household_id,
        actor,
        "lab_report.imported",
        "lab_report",
        report_id,
        source_filename,
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(report_id)
}

pub async fn list_lab_reports(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
) -> anyhow::Result<Vec<crate::domain::LabReport>> {
    let reports = db.query_all(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "SELECT r.id,r.pet_id,p.name,r.source_filename,r.raw_text,r.test_date,r.imported_at,r.parse_status FROM lab_reports r JOIN pets p ON p.id=r.pet_id WHERE r.household_id=? AND r.pet_id=? ORDER BY COALESCE(r.test_date,r.imported_at) DESC LIMIT 50",
        [household_id.into(), pet_id.into()],
    )).await?;
    let mut output = Vec::with_capacity(reports.len());
    for row in reports {
        let report_id: i64 = row.try_get("", "id")?;
        let result_rows = db.query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT id,test_name,value_text,value_numeric,unit,reference_range,flag FROM lab_results WHERE report_id=? ORDER BY id",
            [report_id.into()],
        )).await?;
        let results = result_rows
            .into_iter()
            .map(|result| {
                Ok(crate::domain::LabResult {
                    id: result.try_get("", "id")?,
                    test_name: result.try_get("", "test_name")?,
                    value_text: result.try_get("", "value_text")?,
                    value_numeric: result.try_get("", "value_numeric")?,
                    unit: result.try_get("", "unit")?,
                    reference_range: result.try_get("", "reference_range")?,
                    flag: result.try_get("", "flag")?,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        output.push(crate::domain::LabReport {
            id: report_id,
            pet_id: row.try_get("", "pet_id")?,
            pet_name: row.try_get("", "name")?,
            source_filename: row.try_get("", "source_filename")?,
            raw_text: row.try_get("", "raw_text")?,
            test_date: row.try_get("", "test_date")?,
            imported_at: row
                .try_get::<String>("", "imported_at")?
                .parse()
                .context("invalid lab timestamp")?,
            parse_status: row.try_get("", "parse_status")?,
            results,
        });
    }
    Ok(output)
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

#[allow(clippy::too_many_arguments)]
pub async fn create_symptom_event(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet: &Pet,
    raw_input: &str,
    occurred_at: DateTime<Utc>,
    symptom: &str,
    occurrence_count: Option<i64>,
    amount: Option<&str>,
    contents: Option<&str>,
    meal_relation: Option<&str>,
    water_status: Option<&str>,
    appetite_status: Option<&str>,
    energy_status: Option<&str>,
    pain_status: Option<&str>,
    note: Option<&str>,
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
            household_id.into(), pet.id.into(), "symptom".into(), symptom.into(),
            symptom_summary(symptom).into(), raw_input.into(), note.map(str::to_owned).into(),
            occurred_at.to_rfc3339().into(), now.clone().into(), "exact".into(), source.into(),
        ],
    )).await?.ok_or_else(|| anyhow!("symptom event insert returned no id"))?;
    let event_id: i64 = row.try_get("", "id")?;
    let episode_id = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT s.episode_id FROM symptom_observations s
           JOIN health_events e ON e.id=s.event_id
           WHERE s.household_id=? AND s.pet_id=? AND s.symptom=? AND e.status='active'
             AND e.occurred_at>=? ORDER BY e.occurred_at DESC LIMIT 1"#,
            [
                household_id.into(),
                pet.id.into(),
                symptom.into(),
                (occurred_at - Duration::hours(6)).to_rfc3339().into(),
            ],
        ))
        .await?
        .map(|existing| existing.try_get("", "episode_id"))
        .transpose()?
        .unwrap_or_else(|| format!("episode-{event_id}"));
    transaction.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"INSERT INTO symptom_observations
           (event_id,household_id,pet_id,episode_id,symptom,occurrence_count,amount,contents,meal_relation,water_status,appetite_status,energy_status,pain_status,note)
           VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)"#,
        [
            event_id.into(), household_id.into(), pet.id.into(), episode_id.into(), symptom.into(),
            occurrence_count.into(), amount.into(), contents.into(), meal_relation.into(),
            water_status.into(), appetite_status.into(), energy_status.into(), pain_status.into(),
            note.into(),
        ],
    )).await?;
    audit(
        &transaction,
        household_id,
        actor,
        "event.created",
        "health_event",
        event_id,
        raw_input,
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(event_id)
}

fn symptom_summary(symptom: &str) -> &str {
    match symptom {
        "vomiting" => "Vomited",
        "diarrhea" => "Had diarrhea",
        "reduced_appetite" => "Reduced appetite",
        _ => "Symptom recorded",
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn create_medication_administration(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet: &Pet,
    name: &str,
    active_ingredient: Option<&str>,
    dose_value: Option<f64>,
    dose_unit: Option<&str>,
    route: Option<&str>,
    administered_at: DateTime<Utc>,
    scheduled_at: Option<DateTime<Utc>>,
    status: &str,
    raw_input: Option<&str>,
) -> anyhow::Result<i64> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let row = transaction.query_one(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        r#"INSERT INTO medication_administrations
           (household_id,pet_id,name,active_ingredient,dose_value,dose_unit,route,scheduled_at,administered_at,status,raw_input,created_at)
           VALUES(?,?,?,?,?,?,?,?,?,?,?,?) RETURNING id"#,
        [
            household_id.into(), pet.id.into(), name.into(), active_ingredient.into(),
            dose_value.into(), dose_unit.into(), route.into(), scheduled_at.map(|at| at.to_rfc3339()).into(),
            administered_at.to_rfc3339().into(), status.into(), raw_input.into(), now.clone().into(),
        ],
    )).await?.ok_or_else(|| anyhow!("medication insert returned no id"))?;
    let id: i64 = row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        actor,
        "medication.created",
        "medication_administration",
        id,
        name,
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(id)
}

#[allow(clippy::too_many_arguments)]
pub async fn create_medication_prescription(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet: &Pet,
    name: &str,
    active_ingredient: Option<&str>,
    concentration_value: Option<f64>,
    concentration_unit: Option<&str>,
    dose_value: Option<f64>,
    dose_unit: Option<&str>,
    frequency: Option<&str>,
    route: Option<&str>,
    instructions: Option<&str>,
    started_on: Option<&str>,
    status: &str,
    raw_input: Option<&str>,
) -> anyhow::Result<i64> {
    let now = Utc::now().to_rfc3339();
    let transaction = db.begin().await?;
    let row = transaction
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"INSERT INTO medication_prescriptions
               (household_id,pet_id,name,active_ingredient,concentration_value,concentration_unit,dose_value,dose_unit,frequency,route,instructions,started_on,status,raw_input,created_at,updated_at)
               VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) RETURNING id"#,
            [
                household_id.into(), pet.id.into(), name.into(), active_ingredient.into(),
                concentration_value.into(), concentration_unit.into(), dose_value.into(),
                dose_unit.into(), frequency.into(), route.into(), instructions.into(),
                started_on.into(), status.into(), raw_input.into(), now.clone().into(),
                now.clone().into(),
            ],
        ))
        .await?
        .ok_or_else(|| anyhow!("prescription insert returned no id"))?;
    let id: i64 = row.try_get("", "id")?;
    audit(
        &transaction,
        household_id,
        actor,
        "medication.prescription.created",
        "medication_prescription",
        id,
        raw_input.unwrap_or(name),
        &now,
    )
    .await?;
    transaction.commit().await?;
    Ok(id)
}

pub async fn create_medication_adherence(
    db: &DatabaseConnection,
    household_id: i64,
    actor: &str,
    pet: &Pet,
    prescription: &MedicationPrescription,
    scheduled_for: &str,
    actual_dose_value: Option<f64>,
    actual_dose_unit: Option<&str>,
    status: &str,
    reason: Option<&str>,
    raw_input: Option<&str>,
) -> anyhow::Result<i64> {
    let now = Utc::now().to_rfc3339();
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"INSERT INTO medication_adherence
               (household_id,pet_id,prescription_id,scheduled_for,expected_dose_value,expected_dose_unit,actual_dose_value,actual_dose_unit,status,reason,raw_input,recorded_at)
               VALUES(?,?,?,?,?,?,?,?,?,?,?,?) RETURNING id"#,
            [
                household_id.into(), pet.id.into(), prescription.id.into(), scheduled_for.into(),
                prescription.dose_value.into(), prescription.dose_unit.clone().into(),
                actual_dose_value.into(), actual_dose_unit.into(), status.into(), reason.into(),
                raw_input.into(), now.clone().into(),
            ],
        ))
        .await?
        .ok_or_else(|| anyhow!("adherence insert returned no id"))?;
    let id: i64 = row.try_get("", "id")?;
    audit(
        db,
        household_id,
        actor,
        "medication.adherence.recorded",
        "medication_adherence",
        id,
        raw_input.unwrap_or(reason.unwrap_or("")),
        &now,
    )
    .await?;
    Ok(id)
}

pub async fn list_medications(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    limit: u64,
) -> anyhow::Result<Vec<MedicationAdministration>> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT m.*,p.name AS pet_name FROM medication_administrations m
           JOIN pets p ON p.id=m.pet_id WHERE m.household_id=? AND m.pet_id=?
           ORDER BY m.administered_at DESC LIMIT ?"#,
            [household_id.into(), pet_id.into(), limit.into()],
        ))
        .await?;
    rows.into_iter().map(medication_from_row).collect()
}

pub async fn list_prescriptions(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    limit: u64,
) -> anyhow::Result<Vec<MedicationPrescription>> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT m.*,p.name AS pet_name FROM medication_prescriptions m
           JOIN pets p ON p.id=m.pet_id WHERE m.household_id=? AND m.pet_id=?
           ORDER BY CASE WHEN m.status='active' THEN 0 ELSE 1 END, m.created_at DESC LIMIT ?"#,
            [household_id.into(), pet_id.into(), limit.into()],
        ))
        .await?;
    rows.into_iter().map(prescription_from_row).collect()
}

pub async fn get_prescription(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    prescription_id: i64,
) -> anyhow::Result<Option<MedicationPrescription>> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT m.*,p.name AS pet_name FROM medication_prescriptions m
           JOIN pets p ON p.id=m.pet_id WHERE m.household_id=? AND m.pet_id=? AND m.id=?"#,
            [household_id.into(), pet_id.into(), prescription_id.into()],
        ))
        .await?;
    row.map(|value| prescription_from_row(value)).transpose()
}

pub async fn list_adherence(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    limit: u64,
) -> anyhow::Result<Vec<MedicationAdherence>> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r#"SELECT a.*,p.name AS pet_name,m.name AS prescription_name FROM medication_adherence a
           JOIN pets p ON p.id=a.pet_id JOIN medication_prescriptions m ON m.id=a.prescription_id
           WHERE a.household_id=? AND a.pet_id=?
           ORDER BY a.scheduled_for DESC, a.id DESC LIMIT ?"#,
            [household_id.into(), pet_id.into(), limit.into()],
        ))
        .await?;
    rows.into_iter().map(adherence_from_row).collect()
}

pub async fn medication_plan(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    limit: u64,
) -> anyhow::Result<MedicationPlan> {
    Ok(MedicationPlan {
        prescriptions: list_prescriptions(db, household_id, pet_id, limit).await?,
        adherence: list_adherence(db, household_id, pet_id, limit).await?,
    })
}

pub async fn clinical_timeline(
    db: &DatabaseConnection,
    household_id: i64,
    pet_id: i64,
    limit: u64,
) -> anyhow::Result<ClinicalTimeline> {
    let events = list_events(db, household_id, Some(pet_id), limit).await?;
    let medications = list_medications(db, household_id, pet_id, limit).await?;
    let plan = medication_plan(db, household_id, pet_id, limit).await?;
    let temporal_links = events
        .iter()
        .filter(|event| event.symptom.is_some())
        .flat_map(|event| {
            medications.iter().filter_map(move |medication| {
                let minutes = (event.occurred_at - medication.administered_at).num_minutes();
                (0..=4_320).contains(&minutes).then_some(TemporalLink {
                    event_id: event.id,
                    medication_id: medication.id,
                    minutes_after_medication: minutes,
                })
            })
        })
        .collect();
    Ok(ClinicalTimeline {
        events,
        medications,
        prescriptions: plan.prescriptions,
        adherence: plan.adherence,
        temporal_links,
    })
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
            r#"SELECT e.*,p.name AS pet_name,
                s.episode_id AS symptom_episode_id,s.symptom AS symptom_kind,s.occurrence_count AS symptom_occurrence_count,
                s.amount AS symptom_amount,s.contents AS symptom_contents,s.meal_relation AS symptom_meal_relation,
                s.water_status AS symptom_water_status,s.appetite_status AS symptom_appetite_status,
                s.energy_status AS symptom_energy_status,s.pain_status AS symptom_pain_status,s.note AS symptom_note
            FROM health_events e JOIN pets p ON p.id=e.pet_id LEFT JOIN symptom_observations s ON s.event_id=e.id
            WHERE e.household_id=? AND e.pet_id=? AND e.status='active' ORDER BY e.occurred_at DESC LIMIT ?"#,
            vec![household_id.into(), pet_id.into(), limit.into()],
        )
    } else {
        (
            r#"SELECT e.*,p.name AS pet_name,
                s.episode_id AS symptom_episode_id,s.symptom AS symptom_kind,s.occurrence_count AS symptom_occurrence_count,
                s.amount AS symptom_amount,s.contents AS symptom_contents,s.meal_relation AS symptom_meal_relation,
                s.water_status AS symptom_water_status,s.appetite_status AS symptom_appetite_status,
                s.energy_status AS symptom_energy_status,s.pain_status AS symptom_pain_status,s.note AS symptom_note
            FROM health_events e JOIN pets p ON p.id=e.pet_id LEFT JOIN symptom_observations s ON s.event_id=e.id
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
    let symptom = match row.try_get::<Option<String>>("", "symptom_episode_id")? {
        Some(episode_id) => Some(SymptomObservation {
            event_id: row.try_get("", "id")?,
            episode_id,
            symptom: row.try_get("", "symptom_kind")?,
            occurrence_count: row.try_get("", "symptom_occurrence_count")?,
            amount: row.try_get("", "symptom_amount")?,
            contents: row.try_get("", "symptom_contents")?,
            meal_relation: row.try_get("", "symptom_meal_relation")?,
            water_status: row.try_get("", "symptom_water_status")?,
            appetite_status: row.try_get("", "symptom_appetite_status")?,
            energy_status: row.try_get("", "symptom_energy_status")?,
            pain_status: row.try_get("", "symptom_pain_status")?,
            note: row.try_get("", "symptom_note")?,
        }),
        None => None,
    };
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
        symptom,
    })
}

fn medication_from_row(row: QueryResult) -> anyhow::Result<MedicationAdministration> {
    let parse_at =
        |value: String| DateTime::parse_from_rfc3339(&value).map(|at| at.with_timezone(&Utc));
    let administered_at = parse_at(row.try_get("", "administered_at")?)?;
    let scheduled_at = row
        .try_get::<Option<String>>("", "scheduled_at")?
        .map(parse_at)
        .transpose()?;
    Ok(MedicationAdministration {
        id: row.try_get("", "id")?,
        pet_id: row.try_get("", "pet_id")?,
        pet_name: row.try_get("", "pet_name")?,
        name: row.try_get("", "name")?,
        active_ingredient: row.try_get("", "active_ingredient")?,
        dose_value: row.try_get("", "dose_value")?,
        dose_unit: row.try_get("", "dose_unit")?,
        route: row.try_get("", "route")?,
        administered_at,
        scheduled_at,
        status: row.try_get("", "status")?,
        raw_input: row.try_get("", "raw_input")?,
    })
}

fn prescription_from_row(row: QueryResult) -> anyhow::Result<MedicationPrescription> {
    Ok(MedicationPrescription {
        id: row.try_get("", "id")?,
        pet_id: row.try_get("", "pet_id")?,
        pet_name: row.try_get("", "pet_name")?,
        name: row.try_get("", "name")?,
        active_ingredient: row.try_get("", "active_ingredient")?,
        concentration_value: row.try_get("", "concentration_value")?,
        concentration_unit: row.try_get("", "concentration_unit")?,
        dose_value: row.try_get("", "dose_value")?,
        dose_unit: row.try_get("", "dose_unit")?,
        frequency: row.try_get("", "frequency")?,
        route: row.try_get("", "route")?,
        instructions: row.try_get("", "instructions")?,
        started_on: row.try_get("", "started_on")?,
        ended_on: row.try_get("", "ended_on")?,
        status: row.try_get("", "status")?,
        raw_input: row.try_get("", "raw_input")?,
    })
}

fn adherence_from_row(row: QueryResult) -> anyhow::Result<MedicationAdherence> {
    let recorded_at = DateTime::parse_from_rfc3339(&row.try_get::<String>("", "recorded_at")?)?
        .with_timezone(&Utc);
    Ok(MedicationAdherence {
        id: row.try_get("", "id")?,
        prescription_id: row.try_get("", "prescription_id")?,
        pet_id: row.try_get("", "pet_id")?,
        pet_name: row.try_get("", "pet_name")?,
        prescription_name: row.try_get("", "prescription_name")?,
        scheduled_for: row.try_get("", "scheduled_for")?,
        expected_dose_value: row.try_get("", "expected_dose_value")?,
        expected_dose_unit: row.try_get("", "expected_dose_unit")?,
        actual_dose_value: row.try_get("", "actual_dose_value")?,
        actual_dose_unit: row.try_get("", "actual_dose_unit")?,
        status: row.try_get("", "status")?,
        reason: row.try_get("", "reason")?,
        raw_input: row.try_get("", "raw_input")?,
        recorded_at,
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
    async fn clinical_timeline_keeps_symptom_facts_and_links_recent_medication() {
        let db = test_db().await;
        let pet_id = create_pet(&db, 1, "user:1", "Milo", "Cat", None, None)
            .await
            .unwrap();
        let pet = get_pet(&db, 1, pet_id).await.unwrap().unwrap();
        let medication_at = Utc::now() - Duration::minutes(30);
        let medication_id = create_medication_administration(
            &db,
            1,
            "user:1",
            &pet,
            "anti-nausea medicine",
            None,
            Some(8.0),
            Some("mg"),
            Some("oral"),
            medication_at,
            None,
            "given",
            Some("Gave an anti-nausea medicine before vomiting"),
        )
        .await
        .unwrap();
        let event_id = create_symptom_event(
            &db,
            1,
            "user:1",
            &pet,
            "Milo vomited again earlier",
            medication_at + Duration::minutes(30),
            "vomiting",
            Some(1),
            Some("small"),
            Some("food"),
            Some("after meal"),
            Some("drank"),
            Some("normal"),
            Some("normal"),
            Some("unknown"),
            Some("Owner recorded the episode"),
            "owner_form",
        )
        .await
        .unwrap();

        let timeline = clinical_timeline(&db, 1, pet_id, 20).await.unwrap();
        assert_eq!(timeline.events[0].id, event_id);
        assert_eq!(timeline.events[0].raw_input, "Milo vomited again earlier");
        assert_eq!(
            timeline.events[0]
                .symptom
                .as_ref()
                .unwrap()
                .contents
                .as_deref(),
            Some("food")
        );
        assert_eq!(timeline.medications[0].id, medication_id);
        assert_eq!(timeline.temporal_links.len(), 1);
        assert_eq!(timeline.temporal_links[0].minutes_after_medication, 30);
        assert!(
            clinical_timeline(&db, 2, pet_id, 20)
                .await
                .unwrap()
                .events
                .is_empty()
        );
    }

    #[tokio::test]
    async fn medication_plan_keeps_prescription_and_adherence_gaps_tenant_scoped() {
        let db = test_db().await;
        let pet_id = create_pet(&db, 1, "user:1", "Milo", "Cat", None, None)
            .await
            .unwrap();
        let pet = get_pet(&db, 1, pet_id).await.unwrap().unwrap();
        let prescription_id = create_medication_prescription(
            &db,
            1,
            "user:1",
            &pet,
            "daily medicine",
            None,
            None,
            None,
            Some(1.0),
            Some("mg"),
            Some("once daily"),
            Some("by mouth"),
            Some("Split into two halves and give with treats"),
            None,
            "active",
            Some("Daily medicine; split into two halves and give with treats"),
        )
        .await
        .unwrap();
        let prescription = get_prescription(&db, 1, pet_id, prescription_id)
            .await
            .unwrap()
            .unwrap();
        let adherence_id = create_medication_adherence(
            &db,
            1,
            "user:1",
            &pet,
            &prescription,
            "2026-01-02",
            Some(0.5),
            Some("mg"),
            "partial",
            Some("Only half of the dose was eaten"),
            Some("Over the last few days, only half of the dose was eaten"),
        )
        .await
        .unwrap();
        let plan = medication_plan(&db, 1, pet_id, 20).await.unwrap();
        assert_eq!(plan.prescriptions[0].id, prescription_id);
        assert_eq!(plan.prescriptions[0].dose_value, Some(1.0));
        assert_eq!(plan.adherence[0].id, adherence_id);
        assert_eq!(plan.adherence[0].status, "partial");
        assert_eq!(plan.adherence[0].actual_dose_value, Some(0.5));
        assert!(
            medication_plan(&db, 2, pet_id, 20)
                .await
                .unwrap()
                .prescriptions
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
