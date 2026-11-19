//! SQLite persistence using rusqlite, transactional versioned migrations, and bounded background work.

use std::{
  path::{Path, PathBuf},
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel},
  },
  thread,
  time::Duration,
};

use anyhow::{Context, Result, ensure};
use lintel_core::{
  context::AppContext,
  plugin::{Plugin, PluginLifecycle},
};
use rusqlite::{
  Connection, InterruptHandle, TransactionBehavior,
  hooks::{AuthAction, AuthContext, Authorization},
};
use tokio::sync::Semaphore;

const CAPACITY: usize = 64;
const HISTORY: &str = "__lintel_sqlite_migrations";

/// Immutable migration history. Once applied, keep its version and SQL unchanged.
/// SQL must not contain transaction control, ATTACH/DETACH, pragma arguments, or alter migration history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migration {
  pub version: u32,
  pub sql: String,
}

impl Migration {
  pub fn new(version: u32, sql: impl Into<String>) -> Self {
    Self {
      version,
      sql: sql.into(),
    }
  }
}

struct Inner {
  sender: SyncSender<Work>,
  state: Arc<WorkerState>,
  admission: Arc<Semaphore>,
  path: PathBuf,
}

type Work = Box<dyn FnOnce(&mut Connection) + Send>;

struct WorkerState {
  interrupt: InterruptHandle,
  closed: AtomicBool,
}

impl Drop for Inner {
  fn drop(&mut self) {
    self.state.closed.store(true, Ordering::Release);
    self.admission.close();
    self.state.interrupt.interrupt();
  }
}

struct WorkerLifetime(Arc<WorkerState>, Arc<Semaphore>);
impl Drop for WorkerLifetime {
  fn drop(&mut self) {
    self.0.closed.store(true, Ordering::Release);
    self.1.close();
  }
}

fn worker(mut connection: Connection, receiver: Receiver<Work>, state: Arc<WorkerState>, admission: Arc<Semaphore>) {
  let _lifetime = WorkerLifetime(state.clone(), admission);
  while !state.closed.load(Ordering::Acquire) {
    match receiver.recv_timeout(Duration::from_millis(25)) {
      Ok(work) if !state.closed.load(Ordering::Acquire) => work(&mut connection),
      Ok(_) | Err(RecvTimeoutError::Disconnected) => break,
      Err(RecvTimeoutError::Timeout) => {}
    }
  }
  // The connection and its final WAL checkpoint always close on this worker thread.
  drop(connection);
}

/// One serialized SQLite connection. At most 64 running or queued calls are admitted.
/// Opening and migrations are synchronous startup work; call `open` before presenting UI,
/// or use `spawn_blocking` when opening from an already-running application's update path.
/// The `__lintel_sqlite_migrations` table and `user_version` pragma belong to this service;
/// application call closures must leave both unchanged.
#[derive(Clone)]
pub struct SqliteDatabase {
  inner: Arc<Inner>,
}

impl SqliteDatabase {
  pub fn open(path: impl AsRef<Path>, migrations: impl IntoIterator<Item = Migration>) -> Result<Self> {
    let path = path.as_ref();
    ensure!(!path.as_os_str().is_empty(), "SQLite database path must not be empty");
    let migrations: Vec<_> = migrations.into_iter().collect();
    validate_migrations(&migrations)?;
    let mut connection =
      Connection::open(path).with_context(|| format!("cannot open SQLite database {}", path.display()))?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "foreign_keys", true)?;
    let foreign_keys: bool = connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    ensure!(foreign_keys, "SQLite foreign key enforcement is unavailable");
    let mode: String = connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    ensure!(
      mode.eq_ignore_ascii_case("wal") || mode.eq_ignore_ascii_case("memory"),
      "SQLite WAL mode is unavailable: {mode}"
    );
    migrate(&mut connection, &migrations)?;
    let state = Arc::new(WorkerState {
      interrupt: connection.get_interrupt_handle(),
      closed: AtomicBool::new(false),
    });
    let admission = Arc::new(Semaphore::new(CAPACITY));
    let (sender, receiver) = sync_channel(CAPACITY);
    let worker_state = state.clone();
    let worker_admission = admission.clone();
    thread::Builder::new()
      .name("lintel-sqlite".into())
      .spawn(move || worker(connection, receiver, worker_state, worker_admission))
      .context("cannot start the SQLite connection worker")?;
    Ok(Self {
      inner: Arc::new(Inner {
        sender,
        state,
        admission,
        path: path.to_path_buf(),
      }),
    })
  }

  pub fn path(&self) -> &Path {
    &self.inner.path
  }
  pub fn is_closed(&self) -> bool {
    self.inner.state.closed.load(Ordering::Acquire)
  }

  /// Executes a closure off the UI thread, serially with all other calls on this database.
  /// A saturated queue returns LINTEL_SQLITE_BUSY immediately. Canceling this future
  /// can remove a call still awaiting admission to the connection; an already-dispatched
  /// synchronous Rust closure keeps its permit and connection until it finishes.
  /// Use `interrupt` to interrupt SQLite statements; arbitrary Rust code is not force-cancelable.
  pub async fn call<T, F>(&self, call: F) -> Result<T>
  where
    T: Send + 'static,
    F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
  {
    ensure!(!self.is_closed(), "LINTEL_SQLITE_CLOSED");
    let permit = self
      .inner
      .admission
      .clone()
      .try_acquire_owned()
      .map_err(|error| match error {
        tokio::sync::TryAcquireError::Closed => anyhow::anyhow!("LINTEL_SQLITE_CLOSED"),
        tokio::sync::TryAcquireError::NoPermits => anyhow::anyhow!("LINTEL_SQLITE_BUSY"),
      })?;
    let state = self.inner.state.clone();
    let admission = self.inner.admission.clone();
    let (response, received) = tokio::sync::oneshot::channel();
    let work: Work = Box::new(move |connection| {
      let _permit = permit;
      if response.is_closed() {
        return;
      }
      if state.closed.load(Ordering::Acquire) {
        let _ = response.send(Err(anyhow::anyhow!("LINTEL_SQLITE_CLOSED")));
        return;
      }
      // Connection is not unwind-safe. A panicking user closure therefore closes
      // it permanently rather than exposing a possibly unfinished transaction.
      let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| call(connection))) {
        Ok(result) => result,
        Err(_) => {
          state.closed.store(true, Ordering::Release);
          admission.close();
          Err(anyhow::anyhow!("SQLite call panicked; the connection was closed"))
        }
      };
      let _ = response.send(result);
    });
    self.inner.sender.try_send(work).map_err(|error| match error {
      TrySendError::Full(_) => anyhow::anyhow!("LINTEL_SQLITE_BUSY"),
      TrySendError::Disconnected(_) => anyhow::anyhow!("LINTEL_SQLITE_CLOSED"),
    })?;
    received
      .await
      .context("SQLite connection worker stopped before replying")?
  }

  /// Interrupts the SQL statement currently executing on this connection, if any.
  /// This does not terminate arbitrary synchronous Rust code inside a call closure.
  pub fn interrupt(&self) {
    self.inner.state.interrupt.interrupt();
  }

  /// Rejects new work and interrupts the current SQL statement without blocking the UI.
  /// The connection closes after in-flight synchronous work releases it. Existing external
  /// clones remain closed, including clones retained after their plugin has stopped.
  pub fn close(&self) {
    self.inner.state.closed.store(true, Ordering::Release);
    self.inner.admission.close();
    self.interrupt();
  }
}

fn validate_migrations(migrations: &[Migration]) -> Result<()> {
  let mut previous = 0;
  for migration in migrations {
    ensure!(
      migration.version > previous && migration.version <= i32::MAX as u32,
      "SQLite migration versions must be positive, unique, and strictly increasing"
    );
    ensure!(
      !migration.sql.trim().is_empty(),
      "SQLite migration {} is empty",
      migration.version
    );
    previous = migration.version;
  }
  Ok(())
}

fn migrate(connection: &mut Connection, migrations: &[Migration]) -> Result<()> {
  let restricted = Arc::new(AtomicBool::new(false));
  let guard = restricted.clone();
  connection.authorizer(Some(move |context: AuthContext<'_>| {
    if guard.load(Ordering::Acquire) {
      authorize_migration(context)
    } else {
      Authorization::Allow
    }
  }))?;
  let result = (|| -> Result<()> {
    // Read the version only after acquiring the write transaction, so another opener
    // cannot apply the same migration between our version check and schema updates.
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let target = migrations.last().map_or(0, |migration| migration.version);
    ensure!(
      current >= 0 && current <= i64::from(target),
      "SQLite schema downgrade is unsupported: database={current}, requested={target}"
    );
    let managed: bool = transaction.query_row(
      "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
      [HISTORY],
      |row| row.get(0),
    )?;
    ensure!(
      managed || current == 0,
      "SQLite database has user_version {current} without managed migration history"
    );
    transaction.execute_batch(
      "CREATE TABLE IF NOT EXISTS __lintel_sqlite_migrations (version INTEGER PRIMARY KEY NOT NULL, sql TEXT NOT NULL)",
    )?;
    let history: Vec<(u32, String)> = {
      let mut query = transaction.prepare("SELECT version, sql FROM __lintel_sqlite_migrations ORDER BY version")?;
      query
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?
    };
    let applied: Vec<_> = migrations
      .iter()
      .filter(|migration| i64::from(migration.version) <= current)
      .collect();
    ensure!(
      history.len() == applied.len()
        && history
          .iter()
          .zip(&applied)
          .all(|((version, sql), migration)| *version == migration.version && sql == &migration.sql)
        && (current == 0
          || applied
            .last()
            .is_some_and(|migration| i64::from(migration.version) == current)),
      "SQLite applied migration history conflicts with the supplied versions or SQL"
    );
    for migration in migrations
      .iter()
      .filter(|migration| i64::from(migration.version) > current)
    {
      restricted.store(true, Ordering::Release);
      let result = transaction.execute_batch(&migration.sql);
      restricted.store(false, Ordering::Release);
      result.with_context(|| {
        format!(
          "SQLite migration {} failed; pending migrations were rolled back",
          migration.version
        )
      })?;
      transaction.execute(
        "INSERT INTO __lintel_sqlite_migrations(version,sql) VALUES (?1,?2)",
        rusqlite::params![migration.version, migration.sql],
      )?;
      transaction.pragma_update(None, "user_version", migration.version)?;
    }
    transaction.commit()?;
    Ok(())
  })();
  // Always remove the temporary policy, including on SQL errors, before the caller
  // receives the connection. Runtime closures retain ordinary rusqlite capabilities.
  let cleared = connection.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
  result?;
  cleared.context("cannot remove the SQLite migration authorizer")?;
  Ok(())
}

fn authorize_migration(context: AuthContext<'_>) -> Authorization {
  match context.action {
    AuthAction::Transaction { .. }
    | AuthAction::Savepoint { .. }
    | AuthAction::Attach { .. }
    | AuthAction::Detach { .. } => Authorization::Deny,
    AuthAction::Pragma {
      pragma_value: Some(_), ..
    } => Authorization::Deny,
    AuthAction::Insert { table_name }
    | AuthAction::Delete { table_name }
    | AuthAction::Update { table_name, .. }
    | AuthAction::DropTable { table_name }
    | AuthAction::AlterTable { table_name, .. }
    | AuthAction::CreateTable { table_name }
    | AuthAction::CreateTempTable { table_name }
    | AuthAction::DropTempTable { table_name }
    | AuthAction::CreateTrigger { table_name, .. }
    | AuthAction::DropTrigger { table_name, .. }
    | AuthAction::CreateTempTrigger { table_name, .. }
    | AuthAction::DropTempTrigger { table_name, .. }
    | AuthAction::CreateIndex { table_name, .. }
    | AuthAction::DropIndex { table_name, .. }
    | AuthAction::CreateTempIndex { table_name, .. }
    | AuthAction::DropTempIndex { table_name, .. }
    | AuthAction::CreateVtable { table_name, .. }
    | AuthAction::DropVtable { table_name, .. }
      if table_name.eq_ignore_ascii_case(HISTORY) =>
    {
      Authorization::Deny
    }
    _ => Authorization::Allow,
  }
}

/// Registers one typed SQLite service stored beneath the application's local data directory.
/// `name` is a filename stem, not a path. Migrations complete before the service is published.
pub fn sqlite(
  name: impl Into<String>,
  migrations: impl IntoIterator<Item = Migration>,
) -> impl FnOnce(&mut AppContext) + Send + Sync {
  let name = name.into();
  let migrations = migrations.into_iter().collect();
  move |context| {
    context.register_plugin(Arc::new(SqlitePlugin {
      name,
      migrations,
      database: Mutex::new(None),
    }))
  }
}

struct SqlitePlugin {
  name: String,
  migrations: Vec<Migration>,
  database: Mutex<Option<Arc<SqliteDatabase>>>,
}
impl Plugin for SqlitePlugin {
  fn name(&self) -> &str {
    "lintel.sqlite"
  }
}
impl PluginLifecycle for SqlitePlugin {
  fn on_start(&self, context: &AppContext) -> Result<()> {
    ensure!(
      !self.name.is_empty()
        && self.name.len() <= 128
        && self.name != "."
        && self.name != ".."
        && self
          .name
          .bytes()
          .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)),
      "invalid SQLite database name"
    );
    let directory = context.app_info.project_dir.data_local_dir().join("databases");
    std::fs::create_dir_all(&directory)?;
    let database = Arc::new(SqliteDatabase::open(
      directory.join(format!("{}.sqlite3", self.name)),
      self.migrations.clone(),
    )?);
    *self
      .database
      .lock()
      .map_err(|_| anyhow::anyhow!("SQLite plugin state lock poisoned"))? = Some(database.clone());
    context.register_service(move || database.clone());
    Ok(())
  }
  fn on_stop(&self, context: &AppContext) {
    if let Ok(mut database) = self.database.lock()
      && let Some(database) = database.take()
    {
      database.close();
    }
    context.remove_service::<SqliteDatabase>();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn base() -> Migration {
    Migration::new(
      1,
      "CREATE TABLE notes(id INTEGER PRIMARY KEY, value TEXT NOT NULL); INSERT INTO notes VALUES (1,'old');",
    )
  }

  #[tokio::test]
  async fn migrations_advance_version_and_preserve_committed_data_across_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("app.sqlite3");
    let migrations = vec![
      base(),
      Migration::new(
        2,
        "ALTER TABLE notes ADD COLUMN extra TEXT; UPDATE notes SET extra='new';",
      ),
    ];
    let database = SqliteDatabase::open(&path, migrations.clone()).unwrap();
    let ui_thread = std::thread::current().id();
    let state = database
      .call(move |connection| {
        assert_ne!(
          std::thread::current().id(),
          ui_thread,
          "database work must run off the caller's thread"
        );
        let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let foreign_keys: bool = connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
        let mode: String = connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let row: (String, String) = connection.query_row("SELECT value,extra FROM notes WHERE id=1", [], |row| {
          Ok((row.get(0)?, row.get(1)?))
        })?;
        Ok((version, foreign_keys, mode, row))
      })
      .await
      .unwrap();
    assert_eq!(state, (2, true, "wal".to_owned(), ("old".into(), "new".into())));
    database.close();
    let reopened = SqliteDatabase::open(&path, migrations).unwrap();
    assert_eq!(
      reopened
        .call(|connection| Ok(connection.query_row("SELECT COUNT(*) FROM notes", [], |row| row.get::<_, u32>(0))?))
        .await
        .unwrap(),
      1
    );
    reopened.close();
  }

  #[test]
  fn failed_pending_batch_rolls_back_schema_rows_history_and_user_version() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("app.sqlite3");
    SqliteDatabase::open(&path, [base()]).unwrap().close();
    let result = SqliteDatabase::open(
      &path,
      [
        base(),
        Migration::new(
          2,
          "ALTER TABLE notes ADD COLUMN extra TEXT; UPDATE notes SET value='changed';",
        ),
        Migration::new(3, "INSERT INTO missing_table VALUES (1);"),
      ],
    );
    assert!(result.is_err());
    let connection = Connection::open(&path).unwrap();
    assert_eq!(
      connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
        .unwrap(),
      1
    );
    assert_eq!(
      connection
        .query_row("SELECT value FROM notes WHERE id=1", [], |row| row.get::<_, String>(0))
        .unwrap(),
      "old"
    );
    assert!(connection.prepare("SELECT extra FROM notes").is_err());
    assert_eq!(
      connection
        .query_row("SELECT COUNT(*) FROM __lintel_sqlite_migrations", [], |row| row
          .get::<_, u32>(0))
        .unwrap(),
      1
    );
  }

  #[test]
  fn conflicting_duplicate_down_and_unmanaged_versions_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("app.sqlite3");
    SqliteDatabase::open(&path, [base()]).unwrap().close();
    assert!(SqliteDatabase::open(&path, []).is_err());
    assert!(SqliteDatabase::open(&path, [Migration::new(1, "CREATE TABLE different(id INTEGER);")]).is_err());
    assert!(SqliteDatabase::open(&path, [base(), base()]).is_err());
    assert!(SqliteDatabase::open(&path, [Migration::new(2, "SELECT 1;"), base()]).is_err());
    let raw = Connection::open(&path).unwrap();
    assert_eq!(
      raw
        .query_row("SELECT value FROM notes", [], |row| row.get::<_, String>(0))
        .unwrap(),
      "old"
    );
    drop(raw);
    let unmanaged = directory.path().join("unmanaged.sqlite3");
    Connection::open(&unmanaged)
      .unwrap()
      .pragma_update(None, "user_version", 1)
      .unwrap();
    assert!(SqliteDatabase::open(&unmanaged, [base()]).is_err());
    for (index, migration) in [
      Migration::new(0, "SELECT 1;"),
      Migration::new(u32::MAX, "SELECT 1;"),
      Migration::new(1, " "),
    ]
    .into_iter()
    .enumerate()
    {
      let invalid = directory.path().join(format!("invalid-{index}"));
      assert!(SqliteDatabase::open(&invalid, [migration]).is_err());
      assert!(
        !invalid.exists(),
        "invalid declarations must be rejected before opening the file"
      );
    }
  }

  #[test]
  fn migration_sql_cannot_escape_its_transaction_or_modify_internal_history() {
    let directory = tempfile::tempdir().unwrap();
    for (index, forbidden) in [
      "COMMIT;",
      "ROLLBACK;",
      "SAVEPOINT escaped;",
      "PRAGMA user_version=0;",
      "DELETE FROM __lintel_sqlite_migrations;",
      "DROP TABLE __lintel_sqlite_migrations;",
      "CREATE TRIGGER corrupt AFTER INSERT ON __lintel_sqlite_migrations BEGIN DELETE FROM notes; END;",
      "CREATE TEMP TRIGGER corrupt AFTER INSERT ON main.__lintel_sqlite_migrations BEGIN DELETE FROM notes; END;",
      "CREATE TEMP TABLE __lintel_sqlite_migrations(version INTEGER PRIMARY KEY, sql TEXT);",
      "ATTACH DATABASE ':memory:' AS foreign_database;",
    ]
    .into_iter()
    .enumerate()
    {
      let path = directory.path().join(format!("blocked-{index}.sqlite3"));
      SqliteDatabase::open(&path, [base()]).unwrap().close();
      assert!(
        SqliteDatabase::open(
          &path,
          [
            base(),
            Migration::new(2, format!("UPDATE notes SET value='changed'; {forbidden}"))
          ]
        )
        .is_err(),
        "forbidden SQL was accepted: {forbidden}"
      );
      let raw = Connection::open(&path).unwrap();
      assert_eq!(
        raw
          .query_row("SELECT value FROM notes", [], |row| row.get::<_, String>(0))
          .unwrap(),
        "old"
      );
      assert_eq!(
        raw
          .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
          .unwrap(),
        1
      );
    }
  }

  #[tokio::test]
  async fn concurrent_read_modify_write_calls_are_serialized() {
    let directory = tempfile::tempdir().unwrap();
    let database = SqliteDatabase::open(
      directory.path().join("counter.sqlite3"),
      [Migration::new(
        1,
        "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES(0);",
      )],
    )
    .unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..40 {
      let database = database.clone();
      tasks.spawn(async move {
        database
          .call(|connection| {
            let transaction = connection.transaction()?;
            let value: u32 = transaction.query_row("SELECT value FROM counter", [], |row| row.get(0))?;
            std::thread::sleep(Duration::from_millis(1));
            transaction.execute("UPDATE counter SET value=?1", [value + 1])?;
            transaction.commit()?;
            Ok(())
          })
          .await
      });
    }
    while let Some(result) = tasks.join_next().await {
      result.unwrap().unwrap();
    }
    assert_eq!(
      database
        .call(|connection| Ok(connection.query_row("SELECT value FROM counter", [], |row| row.get::<_, u32>(0))?))
        .await
        .unwrap(),
      40
    );
    database.close();
  }

  #[tokio::test]
  async fn cancellation_keeps_actual_work_bounded_and_closed_handles_reject_new_calls() {
    let directory = tempfile::tempdir().unwrap();
    let database = SqliteDatabase::open(directory.path().join("queue.sqlite3"), []).unwrap();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let first = database.clone();
    let task = tokio::spawn(async move {
      first
        .call(move |_| {
          let _ = started.send(());
          blocked.recv_timeout(Duration::from_secs(5))?;
          Ok(())
        })
        .await
    });
    ready.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(
      database.inner.admission.available_permits(),
      CAPACITY - 1,
      "aborting the async waiter must not release running synchronous work"
    );
    let mut waiting = tokio::task::JoinSet::new();
    for _ in 1..CAPACITY {
      let database = database.clone();
      waiting.spawn(async move { database.call(|_| Ok(())).await });
    }
    tokio::time::timeout(Duration::from_secs(2), async {
      while database.inner.admission.available_permits() != 0 {
        tokio::task::yield_now().await;
      }
    })
    .await
    .unwrap();
    assert!(
      database
        .call(|_| Ok(()))
        .await
        .unwrap_err()
        .to_string()
        .contains("LINTEL_SQLITE_BUSY")
    );
    release.send(()).unwrap();
    while let Some(result) = waiting.join_next().await {
      result.unwrap().unwrap();
    }
    assert_eq!(database.inner.admission.available_permits(), CAPACITY);
    let retained = database.clone();
    database.close();
    assert!(
      retained
        .call(|_| Ok(()))
        .await
        .unwrap_err()
        .to_string()
        .contains("LINTEL_SQLITE_CLOSED")
    );
  }

  #[tokio::test]
  async fn explicit_interrupt_stops_sql_and_leaves_the_connection_usable() {
    let directory = tempfile::tempdir().unwrap();
    let database = SqliteDatabase::open(directory.path().join("interrupt.sqlite3"), []).unwrap();
    let (started, ready) = tokio::sync::oneshot::channel();
    let query_database = database.clone();
    let query = tokio::spawn(async move {
      query_database.call(move |connection| {
        let mut started = Some(started);
        connection.progress_handler(1000, Some(move || { if let Some(started) = started.take() { let _ = started.send(()); } false }))?;
        let result = connection.query_row("WITH RECURSIVE numbers(value) AS (VALUES(1) UNION ALL SELECT value+1 FROM numbers WHERE value<1000000000) SELECT sum(value) FROM numbers", [], |row| row.get::<_, i64>(0));
        connection.progress_handler(0, None::<fn() -> bool>)?;
        Ok(result?)
      }).await
    });
    tokio::time::timeout(Duration::from_secs(2), ready)
      .await
      .unwrap()
      .unwrap();
    database.interrupt();
    let result = tokio::time::timeout(Duration::from_secs(2), query)
      .await
      .unwrap()
      .unwrap();
    assert!(result.is_err());
    assert_eq!(
      database
        .call(|connection| Ok(connection.query_row("SELECT 42", [], |row| row.get::<_, u32>(0))?))
        .await
        .unwrap(),
      42
    );
    database.close();
  }

  #[tokio::test]
  async fn unwinding_user_panic_closes_connection_and_rolls_back_unfinished_work() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("panic.sqlite3");
    let database = SqliteDatabase::open(&path, [base()]).unwrap();
    let result = database
      .call::<(), _>(|connection| {
        connection.execute_batch("BEGIN IMMEDIATE; UPDATE notes SET value='changed';")?;
        panic!("isolated database closure panic");
      })
      .await;
    assert!(result.is_err());
    assert!(database.is_closed());
    assert!(database.call(|_| Ok(())).await.is_err());
    let raw = Connection::open(path).unwrap();
    assert_eq!(
      raw
        .query_row("SELECT value FROM notes", [], |row| row.get::<_, String>(0))
        .unwrap(),
      "old"
    );
  }
}
