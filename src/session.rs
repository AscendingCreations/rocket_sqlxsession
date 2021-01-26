use chrono::{DateTime, Duration, Utc};
use futures::executor::block_on;
use parking_lot::{Mutex, RwLock, RwLockUpgradableReadGuard};
use rand::{rngs::OsRng, Rng};
use rocket::{
    fairing::{self, Fairing, Info},
    http::{Cookie, Status},
    outcome::Outcome,
    request::FromRequest,
    try_outcome, Request, Response, Rocket, State,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sqlx::{
    pool::PoolConnection,
    postgres::{PgConnectOptions, PgPool, PgPoolOptions},
    ConnectOptions,
};
use log::LevelFilter;

use std::{
    sync::Arc,
    borrow::Cow,
    collections::HashMap,
    fmt::{self, Display, Formatter},
};

pub use anyhow::Error;
/// An anyhow::Result with default return type of ()
pub type Result<T = ()> = std::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub struct SqlxSessionConfig {
    /// Sessions lifespan
    lifespan: Duration,
    /// Session cookie name
    cookie_name: Cow<'static, str>,
    /// Session cookie path
    cookie_path: Cow<'static, str>,
    /// Session ID character length
    cookie_len: usize,
    /// Session Database name
    database: Cow<'static, str>,
    /// Session Database username for login
    username: Cow<'static, str>,
    /// Session Database password for login
    password: Cow<'static, str>,
    /// Session Database Host address
    host: Cow<'static, str>,
    /// Session Database Port address
    port: u16,
    /// Session Database table name default is async_sessions
    table_name: Cow<'static, str>,
    /// Session Database Max Poll Connections. Can not be 0
    max_connections: u32,
    /// Session Memory lifespan, deturmines when to unload it from memory
    /// this works fine since the data can stay in the database till its needed
    /// if not yet expired.
    memory_lifespan: Duration,
    /// Log Level for the database
    log_level: LevelFilter,
}

impl Default for SqlxSessionConfig {
    fn default() -> Self {
        Self {
            /// Set to 6hour for default in Database Session stores.
            lifespan: Duration::hours(6),
            cookie_name: "sqlx_session".into(),
            cookie_path: "/".into(),
            cookie_len: 16,
            database: "".into(),
            username: "".into(),
            password: "".into(),
            host: "localhost".into(),
            port: 5432,
            table_name: "async_sessions".into(),
            max_connections: 5,
            /// Unload memory after 60mins if it has not been accessed.
            memory_lifespan: Duration::minutes(60),
            log_level: LevelFilter::Debug,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SQLxSessionData {
    pub id: String,
    pub data: HashMap<String, String>,
    pub expires: DateTime<Utc>,
    pub autoremove: DateTime<Utc>,
    pub destroy: bool,
}

impl Default for SQLxSessionData {
    fn default() -> Self {
        Self {
            id: "".into(),
            data: HashMap::new(),
            expires: Utc::now() + Duration::hours(6),
            destroy: false,
            autoremove: Utc::now() + Duration::hours(1),
        }
    }
}

impl SQLxSessionData {
    pub fn validate(&self) -> bool {
        if self.expires < Utc::now() {
            false
        } else {
            true
        }
    }
}

#[derive(Debug)]
pub struct SQLxTimers {
    pub last_expiry_sweep: DateTime<Utc>,
    pub last_database_expiry_sweep: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct SQLxSessionStore {
    pub client: PgPool,
    pub inner: Arc<RwLock<HashMap<String, Mutex<SQLxSessionData>>>>,
    pub config: SqlxSessionConfig,
    pub timers: Arc<RwLock<SQLxTimers>>,
}

impl SQLxSessionStore {
    pub fn new(client: PgPool, config: SqlxSessionConfig) -> Self {
        Self {
            client,
            inner: Default::default(),
            config,
            timers: Arc::new(RwLock::new(SQLxTimers {
                // the first expiry sweep is scheduled one lifetime from start-up
                last_expiry_sweep: Utc::now() + Duration::hours(1),
                // the first expiry sweep is scheduled one lifetime from start-up
                last_database_expiry_sweep: Utc::now() + Duration::hours(6),
            })),
        }
    }

    pub async fn migrate(&self) -> sqlx::Result<()> {
        let mut conn = self.client.acquire().await?;
        sqlx::query(&*self.substitute_table_name(
            r#"
            CREATE TABLE IF NOT EXISTS %%TABLE_NAME%% (
                "id" VARCHAR NOT NULL PRIMARY KEY,
                "expires" TIMESTAMP WITH TIME ZONE NULL,
                "session" TEXT NOT NULL
            )
            "#,
        ))
        .execute(&mut conn)
        .await?;

        Ok(())
    }

    fn substitute_table_name(&self, query: &str) -> String {
        query.replace("%%TABLE_NAME%%", &self.config.table_name)
    }

    async fn connection(&self) -> sqlx::Result<PoolConnection<sqlx::Postgres>> {
        self.client.acquire().await
    }

    pub async fn cleanup(&self) -> sqlx::Result<()> {
        let mut connection = self.connection().await?;
        sqlx::query(&self.substitute_table_name("DELETE FROM %%TABLE_NAME%% WHERE expires < $1"))
            .bind(Utc::now())
            .execute(&mut connection)
            .await?;

        Ok(())
    }

    pub async fn count(&self) -> sqlx::Result<i64> {
        let (count,) =
            sqlx::query_as(&self.substitute_table_name("SELECT COUNT(*) FROM %%TABLE_NAME%%"))
                .fetch_one(&mut self.connection().await?)
                .await?;

        Ok(count)
    }

    pub async fn load_session(&self, cookie_value: String) -> Result<Option<SQLxSessionData>> {
        let mut connection = self.connection().await?;

        let result: Option<(String,)> = sqlx::query_as(&self.substitute_table_name(
            "SELECT session FROM %%TABLE_NAME%% WHERE id = $1 AND (expires IS NULL OR expires > $2)"
        ))
        .bind(&cookie_value)
        .bind(Utc::now())
        .fetch_optional(&mut connection)
        .await?;

        Ok(result
            .map(|(session,)| serde_json::from_str(&session))
            .transpose()?)
    }

    pub async fn store_session(&self, session: SQLxSessionData) -> Result<()> {
        let id = session.id.clone();
        let string = serde_json::to_string(&session)?;
        let mut connection = self.connection().await?;

        sqlx::query(&self.substitute_table_name(
            r#"
            INSERT INTO %%TABLE_NAME%%
              (id, session, expires) SELECT $1, $2, $3
            ON CONFLICT(id) DO UPDATE SET
              expires = EXCLUDED.expires,
              session = EXCLUDED.session
            "#,
        ))
        .bind(&id)
        .bind(&string)
        .bind(&session.expires)
        .execute(&mut connection)
        .await?;

        Ok(())
    }

    pub async fn destroy_session(&self, id: &String) -> Result {
        let mut connection = self.connection().await?;
        sqlx::query(&self.substitute_table_name("DELETE FROM %%TABLE_NAME%% WHERE id = $1"))
            .bind(&id)
            .execute(&mut connection)
            .await?;

        Ok(())
    }

    pub async fn clear_store(&self) -> Result {
        let mut connection = self.connection().await?;
        sqlx::query(&self.substitute_table_name("TRUNCATE %%TABLE_NAME%%"))
            .execute(&mut connection)
            .await?;

        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SQLxSessionID(String);

impl Display for SQLxSessionID {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug)]
pub struct SQLxSession {
    store: SQLxSessionStore,
    id: SQLxSessionID,
}

#[rocket::async_trait]
impl<'a, 'r> FromRequest<'a, 'r> for SQLxSession {
    type Error = ();

    async fn from_request(request: &'a Request<'r>) -> Outcome<Self, (Status, Self::Error), ()> {
        let store: State<SQLxSessionStore> = try_outcome!(request.guard().await);
        Outcome::Success(SQLxSession {
            id: request.local_cache(|| {
                let store_ug = store.inner.upgradable_read();

                // Resolve session ID
                let id = if let Some(cookie) = request.cookies().get(&store.config.cookie_name) {
                    SQLxSessionID(cookie.value().to_string())
                } else {
                    SQLxSessionID("".to_string())
                };

                if let Some(m) = store_ug.get(&id.0) {
                    let mut inner = m.lock();

                    if inner.expires < Utc::now() || inner.destroy {
                        // Database Session expired, reuse the ID but drop data.
                        inner.data = HashMap::new();
                    }

                    // Session is extended by making a request with valid ID
                    inner.expires = Utc::now() + store.config.lifespan;
                    inner.autoremove = Utc::now() + store.config.memory_lifespan;

                    id
                } else {
                    // --- ID missing or session not found ---

                    // Get exclusive write access to the map
                    let mut store_wg = RwLockUpgradableReadGuard::upgrade(store_ug);

                    // This branch runs less often, and we already have write access,
                    // let's check if any sessions expired. We don't want to hog memory
                    // forever by abandoned sessions (e.g. when a client lost their cookie)
                    {
                        let timers = store.timers.upgradable_read();
                        // Throttle by memory lifespan - e.g. sweep every hour
                        if timers.last_expiry_sweep <= Utc::now() {
                            let mut timers = RwLockUpgradableReadGuard::upgrade(timers);
                            store_wg.retain(|_k, v| v.lock().autoremove > Utc::now());
                            timers.last_expiry_sweep = Utc::now() + store.config.memory_lifespan;
                        }
                    }

                    {
                        let timers = store.timers.upgradable_read();
                        // Throttle by database lifespan - e.g. sweep every 6 hours
                        if timers.last_database_expiry_sweep <= Utc::now() {
                            let mut timers = RwLockUpgradableReadGuard::upgrade(timers);
                            store_wg.retain(|_k, v| v.lock().autoremove > Utc::now());
                            let _ = block_on(store.cleanup());
                            timers.last_database_expiry_sweep = Utc::now() + store.config.lifespan;
                        }
                    }

                    let session = if !id.0.is_empty() {
                        // Attempt to load from database if fail then we will create a new session.
                        let mut sess = block_on(store.load_session(id.0.clone()))
                            .ok()
                            .flatten()
                            .unwrap_or(SQLxSessionData {
                                id: id.0.clone(),
                                data: HashMap::new(),
                                expires: Utc::now() + Duration::hours(6),
                                destroy: false,
                                autoremove: Utc::now() + store.config.memory_lifespan,
                            });

                        if (!sess.validate() || sess.destroy) {
                            sess.data = HashMap::new();
                            sess.expires = Utc::now() + Duration::hours(6);
                            sess.autoremove = Utc::now() + store.config.memory_lifespan;
                        }

                        sess
                    } else {
                        // Find a new unique ID - we are still safely inside the write guard
                        let new_id = SQLxSessionID(loop {
                            let token: String = OsRng
                                .sample_iter(&rand::distributions::Alphanumeric)
                                .take(store.config.cookie_len)
                                .map(char::from)
                                .collect();

                            if !store_wg.contains_key(&token) {
                                break token;
                            }
                        });

                        SQLxSessionData {
                            id: new_id.0.clone(),
                            data: HashMap::new(),
                            expires: Utc::now() + Duration::hours(6),
                            destroy: false,
                            autoremove: Utc::now() + store.config.memory_lifespan,
                        }
                    };

                    let new_id = SQLxSessionID(session.id.clone());

                    store_wg.insert(session.id.clone(), Mutex::new(session));

                    new_id
                }
            }).clone(),

            store: store.inner().clone(),
        })
    }
}

impl SQLxSession {
    pub fn tap<T: DeserializeOwned>(
        &self,
        func: impl FnOnce(&mut SQLxSessionData) -> Option<T>,
    ) -> Option<T> {
        let store_rg = self.store.inner.read();

        let mut instance = store_rg
            .get(&self.id.0)
            .expect("Session data unexpectedly missing")
            .lock();

        func(&mut instance)
    }

    pub fn destroy(&self) {
        self.tap(|sess| {
            sess.destroy = true;
            Some(1)
        });
    }

    pub fn get<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.tap(|sess| {
            let string = sess.data.get(key)?;
            serde_json::from_str(string).ok()
        })
    }

    pub fn set(&self, key: &str, value: impl Serialize) {
        let value = serde_json::to_string(&value).unwrap_or("".to_string());

        self.tap(|sess| {
            if sess.data.get(key) != Some(&value) {
                sess.data.insert(key.to_string(), value);
            }
            Some(1)
        });
    }

    pub fn remove(&self, key: &str) {
        self.tap(|sess| sess.data.remove(key));
    }

    pub fn clear_all(&self) {
        self.tap(|sess| {
            sess.data.clear();
            let _ = block_on(self.store.clear_store());
            Some(1)
        });
    }

    pub fn count(&self) -> i64 {
        block_on(self.store.count()).unwrap_or(0i64)
    }
}

/// Fairing struct
#[derive(Default)]
pub struct SqlxSessionFairing {
    poll: Option<PgPool>,
    config: SqlxSessionConfig,
}

impl SqlxSessionFairing {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set session database pools max connections limit.
    ///
    /// Call on the fairing before passing it to `rocket.attach()`
    pub fn set_max_connections(mut self, max: u32) -> SqlxSessionFairing {
        let max = std::cmp::max(max, 1);
        self.config.max_connections = max;
        self
    }

    /// Set session lifetime (expiration time) within database storage.
    ///
    /// Call on the fairing before passing it to `rocket.attach()`
    pub fn with_lifetime(mut self, time: Duration) -> Self {
        self.config.lifespan = time;
        self
    }

    /// Set session lifetime (expiration time) within Memory storage.
    ///
    /// Call on the fairing before passing it to `rocket.attach()`
    pub fn with_memory_lifetime(mut self, time: Duration) -> Self {
        self.config.memory_lifespan = time;
        self
    }

    /// Set session cookie name
    ///
    /// Call on the fairing before passing it to `rocket.attach()`
    pub fn with_cookie_name(mut self, name: impl Into<Cow<'static, str>>) -> Self {
        self.config.cookie_name = name.into();
        self
    }

    /// Set session cookie length
    ///
    /// Call on the fairing before passing it to `rocket.attach()`
    pub fn with_cookie_len(mut self, length: usize) -> Self {
        self.config.cookie_len = length;
        self
    }

    /// Set session cookie path
    ///
    /// Call on the fairing before passing it to `rocket.attach()`
    pub fn with_cookie_path(mut self, path: impl Into<Cow<'static, str>>) -> Self {
        self.config.cookie_path = path.into();
        self
    }

    /// Set session database name
    ///
    /// Call on the fairing before passing it to `rocket.attach()`
    pub fn with_database(mut self, database: impl Into<Cow<'static, str>>) -> Self {
        self.config.database = database.into();
        self
    }

    /// Set session username
    ///
    /// Call on the fairing before passing it to `rocket.attach()`
    pub fn with_username(mut self, username: impl Into<Cow<'static, str>>) -> Self {
        self.config.username = username.into();
        self
    }

    /// Set session user password
    ///
    /// Call on the fairing before passing it to `rocket.attach()`
    pub fn with_password(mut self, password: impl Into<Cow<'static, str>>) -> Self {
        self.config.password = password.into();
        self
    }

    /// Set session database table name
    ///
    /// Call on the fairing before passing it to `rocket.attach()`
    pub fn with_table_name(mut self, table_name: impl Into<Cow<'static, str>>) -> Self {
        self.config.table_name = table_name.into();
        self
    }

    /// Set session database hostname
    ///
    /// Call on the fairing before passing it to `rocket.attach()`
    pub fn with_host(mut self, host: impl Into<Cow<'static, str>>) -> Self {
        self.config.host = host.into();
        self
    }

    /// Set session database port
    ///
    /// Call on the fairing before passing it to `rocket.attach()`
    pub fn with_port(mut self, port: u16) -> Self {
        self.config.port = port;
        self
    }

    /// Set session database logging level
    ///
    /// Call on the fairing before passing it to `rocket.attach()`
    pub fn with_loglevel(mut self, level: LevelFilter) -> Self {
        self.config.log_level = level;
        self
    }

    /// Set session database Poll Directly used for sharing Poll.
    ///
    /// Call on the fairing before passing it to `rocket.attach()`
    pub fn with_poll(mut self, poll: PgPool) -> Self {
        self.poll = Some(poll);
        self
    }
}

#[rocket::async_trait]
impl Fairing for SqlxSessionFairing {
    fn info(&self) -> Info {
        Info {
            name: "SQLxSession",
            kind: fairing::Kind::Attach | fairing::Kind::Response,
        }
    }

    async fn on_attach(&self, rocket: Rocket) -> std::result::Result<Rocket, Rocket> {
        let session_store = if let Some(poll) = &self.poll {
            SQLxSessionStore::new(poll.clone(), self.config.clone())
        } else {
            let mut connect_opts = PgConnectOptions::new();
            connect_opts.log_statements(self.config.log_level);
            connect_opts = connect_opts.database(&self.config.database[..]);
            connect_opts = connect_opts.username(&self.config.username[..]);
            connect_opts = connect_opts.password(&self.config.password[..]);
            connect_opts = connect_opts.host(&self.config.host[..]);
            connect_opts = connect_opts.port(self.config.port);

            let pg_pool = match PgPoolOptions::new()
                .max_connections(self.config.max_connections)
                .connect_with(connect_opts)
                .await
            {
                Ok(n) => n,
                Err(_) => return Ok(rocket),
            };

            SQLxSessionStore::new(pg_pool, self.config.clone())
        };

        match session_store.migrate().await {
            Ok(()) => {}
            Err(_) => {}
        }

        Ok(rocket.manage(session_store))
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        let session_id = request.local_cache(|| SQLxSessionID("".to_string()));

        if !session_id.0.is_empty() {
            if let Outcome::Success(store) = request.guard::<State<SQLxSessionStore>>().await {
                let store_ug = store.inner.upgradable_read();

                if let Some(m) = store_ug.get(&session_id.0) {
                    let inner = m.lock().clone();
                    if inner.destroy {
                        let _ = block_on(store.destroy_session(&session_id.0));
                    } else {
                        let _ = block_on(store.store_session(inner));
                    }
                }
            }
            response.adjoin_header(
                Cookie::build(self.config.cookie_name.clone(), session_id.0.clone()).finish(),
            );
        }
    }
}
