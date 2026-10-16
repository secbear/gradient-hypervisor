use crate::kinds::Profile;
use anyhow::{Context, bail};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqlitePool};
use std::path::Path;
use std::str::FromStr;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone)]
pub struct State(SqlitePool);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LaunchState {
    Dispatched,
    Queued,
    Running,
    Connected,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub id: Uuid,
    pub connection: String,
    pub run_id: u64,
    pub label: String,
    pub state: LaunchState,
    pub worker_id: Option<Uuid>,
    pub swapped: bool,
    pub dispatched_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy)]
pub enum SecretKind {
    GitHubHook,
    GradientHook,
}

const OPEN_STATES: [LaunchState; 4] = [
    LaunchState::Dispatched,
    LaunchState::Queued,
    LaunchState::Running,
    LaunchState::Connected,
];

impl LaunchState {
    pub fn as_str(self) -> &'static str {
        match self {
            LaunchState::Dispatched => "dispatched",
            LaunchState::Queued => "queued",
            LaunchState::Running => "running",
            LaunchState::Connected => "connected",
            LaunchState::Completed => "completed",
            LaunchState::Failed => "failed",
            LaunchState::Cancelled => "cancelled",
        }
    }

    pub fn is_open(self) -> bool {
        OPEN_STATES.contains(&self)
    }

    pub fn is_waiting_for_runner(self) -> bool {
        matches!(self, LaunchState::Dispatched | LaunchState::Queued)
    }
}

impl FromStr for LaunchState {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> anyhow::Result<Self> {
        [
            OPEN_STATES.as_slice(),
            &[
                LaunchState::Completed,
                LaunchState::Failed,
                LaunchState::Cancelled,
            ],
        ]
        .concat()
        .into_iter()
        .find(|state| state.as_str() == value)
        .with_context(|| format!("unknown launch state {value:?}"))
    }
}

impl SecretKind {
    fn as_str(self) -> &'static str {
        match self {
            SecretKind::GitHubHook => "github_hook",
            SecretKind::GradientHook => "gradient_hook",
        }
    }
}

impl Launch {
    pub fn new(connection: &str, run_id: u64, label: &str) -> Launch {
        Launch {
            id: Uuid::new_v4(),
            connection: connection.to_owned(),
            run_id,
            label: label.to_owned(),
            state: LaunchState::Dispatched,
            worker_id: None,
            swapped: false,
            dispatched_at: OffsetDateTime::now_utc(),
        }
    }

    fn from_row(row: &SqliteRow) -> anyhow::Result<Launch> {
        let worker_id: Option<String> = row.try_get("worker_id")?;
        Ok(Launch {
            id: row.try_get::<String, _>("id")?.parse()?,
            connection: row.try_get("connection")?,
            run_id: u64::try_from(row.try_get::<i64, _>("run_id")?)?,
            label: row.try_get("label")?,
            state: row.try_get::<String, _>("state")?.parse()?,
            worker_id: worker_id.map(|id| id.parse()).transpose()?,
            swapped: row.try_get("swapped")?,
            dispatched_at: OffsetDateTime::from_unix_timestamp(row.try_get("dispatched_at")?)?,
        })
    }
}

fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

impl State {
    pub async fn open(path: &Path) -> anyhow::Result<State> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .with_context(|| format!("opening state file {}", path.display()))?;
        Self::migrated(pool).await
    }

    pub async fn memory() -> anyhow::Result<State> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::from_str("sqlite::memory:")?)
            .await?;
        Self::migrated(pool).await
    }

    async fn migrated(pool: SqlitePool) -> anyhow::Result<State> {
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(State(pool))
    }

    pub async fn insert_launch(&self, launch: &Launch) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO launch (id, connection, run_id, label, state, worker_id, swapped, dispatched_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(launch.id.to_string())
        .bind(&launch.connection)
        .bind(i64::try_from(launch.run_id)?)
        .bind(&launch.label)
        .bind(launch.state.as_str())
        .bind(launch.worker_id.map(|id| id.to_string()))
        .bind(launch.swapped)
        .bind(launch.dispatched_at.unix_timestamp())
        .bind(now())
        .execute(&self.0)
        .await?;
        Ok(())
    }

    pub async fn launch(&self, connection: &str, id: Uuid) -> anyhow::Result<Option<Launch>> {
        let row = sqlx::query("SELECT * FROM launch WHERE connection = ? AND id = ?")
            .bind(connection)
            .bind(id.to_string())
            .fetch_optional(&self.0)
            .await?;
        row.as_ref().map(Launch::from_row).transpose()
    }

    pub async fn launch_by_run(
        &self,
        connection: &str,
        run_id: u64,
    ) -> anyhow::Result<Option<Launch>> {
        let row = sqlx::query("SELECT * FROM launch WHERE connection = ? AND run_id = ?")
            .bind(connection)
            .bind(i64::try_from(run_id)?)
            .fetch_optional(&self.0)
            .await?;
        row.as_ref().map(Launch::from_row).transpose()
    }

    pub async fn open_launches(&self, connection: &str) -> anyhow::Result<Vec<Launch>> {
        let rows = sqlx::query(
            "SELECT * FROM launch WHERE connection = ? AND state IN (?, ?, ?, ?) ORDER BY dispatched_at",
        )
        .bind(connection)
        .bind(OPEN_STATES[0].as_str())
        .bind(OPEN_STATES[1].as_str())
        .bind(OPEN_STATES[2].as_str())
        .bind(OPEN_STATES[3].as_str())
        .fetch_all(&self.0)
        .await?;
        rows.iter().map(Launch::from_row).collect()
    }

    pub async fn advance_launch(&self, id: Uuid, to: LaunchState) -> anyhow::Result<bool> {
        let earlier: Vec<LaunchState> = OPEN_STATES.into_iter().filter(|s| *s < to).collect();
        self.move_launch(id, to, &earlier, "").await
    }

    pub async fn close_launch(&self, id: Uuid, to: LaunchState) -> anyhow::Result<bool> {
        self.move_launch(id, to, &OPEN_STATES, "").await
    }

    pub async fn claim_cancel(&self, id: Uuid) -> anyhow::Result<bool> {
        let waiting = [LaunchState::Dispatched, LaunchState::Queued];
        self.move_launch(id, LaunchState::Cancelled, &waiting, " AND swapped = 0")
            .await
    }

    async fn move_launch(
        &self,
        id: Uuid,
        to: LaunchState,
        from: &[LaunchState],
        condition: &str,
    ) -> anyhow::Result<bool> {
        if from.is_empty() {
            return Ok(false);
        }
        let placeholders = vec!["?"; from.len()].join(", ");
        let sql = format!(
            "UPDATE launch SET state = ?, updated_at = ? WHERE id = ? AND state IN ({placeholders}){condition}"
        );
        let mut query = sqlx::query(&sql)
            .bind(to.as_str())
            .bind(now())
            .bind(id.to_string());
        for state in from {
            query = query.bind(state.as_str());
        }
        Ok(query.execute(&self.0).await?.rows_affected() == 1)
    }

    pub async fn mark_swapped(&self, id: Uuid, worker_id: Uuid) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE launch SET swapped = 1, worker_id = ?, updated_at = ?
             WHERE id = ? AND swapped = 0 AND state IN (?, ?, ?, ?)",
        )
        .bind(worker_id.to_string())
        .bind(now())
        .bind(id.to_string())
        .bind(OPEN_STATES[0].as_str())
        .bind(OPEN_STATES[1].as_str())
        .bind(OPEN_STATES[2].as_str())
        .bind(OPEN_STATES[3].as_str())
        .execute(&self.0)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn profile(&self, connection: &str, label: &str) -> anyhow::Result<Option<Profile>> {
        let raw: Option<String> =
            sqlx::query_scalar("SELECT profile FROM profile WHERE connection = ? AND label = ?")
                .bind(connection)
                .bind(label)
                .fetch_optional(&self.0)
                .await?;
        Ok(raw.map(|raw| serde_json::from_str(&raw)).transpose()?)
    }

    pub async fn put_profile(
        &self,
        connection: &str,
        label: &str,
        profile: &Profile,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO profile (connection, label, profile) VALUES (?, ?, ?)
             ON CONFLICT (connection, label) DO UPDATE SET profile = excluded.profile",
        )
        .bind(connection)
        .bind(label)
        .bind(serde_json::to_string(profile)?)
        .execute(&self.0)
        .await?;
        Ok(())
    }

    pub async fn secret(
        &self,
        connection: &str,
        kind: SecretKind,
    ) -> anyhow::Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT value FROM secret WHERE connection = ? AND kind = ?")
                .bind(connection)
                .bind(kind.as_str())
                .fetch_optional(&self.0)
                .await?,
        )
    }

    pub async fn put_secret(
        &self,
        connection: &str,
        kind: SecretKind,
        value: &str,
    ) -> anyhow::Result<()> {
        if value.is_empty() {
            bail!("refusing to store an empty {} secret", kind.as_str());
        }
        sqlx::query(
            "INSERT INTO secret (connection, kind, value) VALUES (?, ?, ?)
             ON CONFLICT (connection, kind) DO UPDATE SET value = excluded.value",
        )
        .bind(connection)
        .bind(kind.as_str())
        .bind(value)
        .execute(&self.0)
        .await?;
        Ok(())
    }

    pub async fn failures_since(
        &self,
        connection: &str,
        label: &str,
        since: OffsetDateTime,
    ) -> anyhow::Result<u32> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM launch WHERE connection = ? AND label = ? AND state = ? AND updated_at >= ?",
        )
        .bind(connection)
        .bind(label)
        .bind(LaunchState::Failed.as_str())
        .bind(since.unix_timestamp())
        .fetch_one(&self.0)
        .await?;
        Ok(u32::try_from(count)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    #[tokio::test]
    async fn mark_swapped_only_once() {
        let state = State::memory().await.unwrap();
        let launch = Launch::new("a", 7, "ubuntu-latest");
        state.insert_launch(&launch).await.unwrap();
        let first_worker = Uuid::new_v4();

        assert!(state.mark_swapped(launch.id, first_worker).await.unwrap());
        assert!(!state.mark_swapped(launch.id, Uuid::new_v4()).await.unwrap());

        let stored = state.launch("a", launch.id).await.unwrap().unwrap();
        assert_eq!(stored.worker_id, Some(first_worker));
        assert!(stored.swapped);
    }

    #[tokio::test]
    async fn cancel_and_swap_exclude_each_other() {
        let state = State::memory().await.unwrap();
        let swapped = Launch::new("a", 1, "ubuntu-latest");
        let cancelled = Launch::new("a", 2, "ubuntu-latest");
        state.insert_launch(&swapped).await.unwrap();
        state.insert_launch(&cancelled).await.unwrap();

        assert!(
            state
                .mark_swapped(swapped.id, Uuid::new_v4())
                .await
                .unwrap()
        );
        assert!(!state.claim_cancel(swapped.id).await.unwrap());

        assert!(state.claim_cancel(cancelled.id).await.unwrap());
        assert!(
            !state
                .mark_swapped(cancelled.id, Uuid::new_v4())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn launch_state_never_moves_backwards_or_reopens() {
        let state = State::memory().await.unwrap();
        let launch = Launch::new("a", 1, "ubuntu-latest");
        state.insert_launch(&launch).await.unwrap();
        let current = |state: &State| {
            let state = state.clone();
            async move { state.launch("a", launch.id).await.unwrap().unwrap().state }
        };

        assert!(
            state
                .advance_launch(launch.id, LaunchState::Connected)
                .await
                .unwrap()
        );
        assert!(
            !state
                .advance_launch(launch.id, LaunchState::Running)
                .await
                .unwrap()
        );
        assert_eq!(current(&state).await, LaunchState::Connected);

        assert!(
            state
                .close_launch(launch.id, LaunchState::Completed)
                .await
                .unwrap()
        );
        assert!(
            !state
                .advance_launch(launch.id, LaunchState::Connected)
                .await
                .unwrap()
        );
        assert!(
            !state
                .close_launch(launch.id, LaunchState::Failed)
                .await
                .unwrap()
        );
        assert_eq!(current(&state).await, LaunchState::Completed);
    }

    #[tokio::test]
    async fn launches_are_per_connection() {
        let state = State::memory().await.unwrap();
        let own = Launch::new("team-a", 1, "ubuntu-latest");
        let other = Launch::new("team-b", 2, "ubuntu-latest");
        state.insert_launch(&own).await.unwrap();
        state.insert_launch(&other).await.unwrap();

        let open = state.open_launches("team-a").await.unwrap();
        assert_eq!(open.iter().map(|l| l.id).collect::<Vec<_>>(), [own.id]);
        assert!(state.launch_by_run("team-a", 2).await.unwrap().is_none());
        assert!(state.launch("team-a", other.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn closed_launches_are_not_open() {
        let state = State::memory().await.unwrap();
        let launch = Launch::new("a", 1, "ubuntu-latest");
        state.insert_launch(&launch).await.unwrap();
        assert!(
            state
                .close_launch(launch.id, LaunchState::Failed)
                .await
                .unwrap()
        );

        assert!(state.open_launches("a").await.unwrap().is_empty());
        let hour_ago = OffsetDateTime::now_utc() - Duration::hours(1);
        assert_eq!(
            state
                .failures_since("a", "ubuntu-latest", hour_ago)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            state
                .failures_since("a", "ubuntu-24.04-arm", hour_ago)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn secrets_and_profiles_are_per_connection() {
        let state = State::memory().await.unwrap();
        state
            .put_secret("a", SecretKind::GitHubHook, "one")
            .await
            .unwrap();
        state
            .put_secret("a", SecretKind::GitHubHook, "two")
            .await
            .unwrap();
        let profile = Profile {
            systems: vec!["x86_64-linux".into()],
            features: vec!["kvm".into()],
            max_jobs: 4,
            ram_mb: Some(16000),
        };
        state
            .put_profile("a", "ubuntu-latest", &profile)
            .await
            .unwrap();

        assert_eq!(
            state
                .secret("a", SecretKind::GitHubHook)
                .await
                .unwrap()
                .as_deref(),
            Some("two")
        );
        assert_eq!(
            state.secret("b", SecretKind::GitHubHook).await.unwrap(),
            None
        );
        assert_eq!(
            state.secret("a", SecretKind::GradientHook).await.unwrap(),
            None
        );
        assert_eq!(
            state.profile("a", "ubuntu-latest").await.unwrap(),
            Some(profile)
        );
        assert_eq!(state.profile("b", "ubuntu-latest").await.unwrap(), None);
    }
}
