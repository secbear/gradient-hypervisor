use crate::capacity::BuildCapacityProfile;
use crate::kinds::Profile;
use crate::kubernetes::KubernetesLaunchSpec;
use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
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
    pub external_id: String,
    pub label: String,
    pub state: LaunchState,
    pub worker_id: Option<Uuid>,
    pub swapped: bool,
    pub dispatched_at: OffsetDateTime,
    pub build_intent: Option<BuildLaunchIntent>,
    pub registration_started: bool,
    pub secret_uid: Option<String>,
    pub pod_uid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildLaunchIntent {
    pub profile: BuildCapacityProfile,
    #[serde(with = "time::serde::rfc3339")]
    pub startup_deadline: OffsetDateTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubernetes: Option<KubernetesLaunchSpec>,
}

impl BuildLaunchIntent {
    fn validate(&self, launch: &Launch) -> anyhow::Result<()> {
        self.profile.validate()?;
        ensure!(
            launch.worker_id == Some(self.profile.worker_id),
            "build launch identity differs from its assessed profile"
        );
        ensure!(
            self.startup_deadline > launch.dispatched_at,
            "build launch startup deadline must follow dispatch"
        );
        if let Some(spec) = &self.kubernetes {
            spec.validate(launch, &self.profile)?;
        }
        Ok(())
    }
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
    pub fn new(connection: &str, external_id: &str, label: &str) -> Launch {
        Launch {
            id: Uuid::new_v4(),
            connection: connection.to_owned(),
            external_id: external_id.to_owned(),
            label: label.to_owned(),
            state: LaunchState::Dispatched,
            worker_id: None,
            swapped: false,
            dispatched_at: OffsetDateTime::now_utc(),
            build_intent: None,
            registration_started: false,
            secret_uid: None,
            pod_uid: None,
        }
    }

    fn from_row(row: &SqliteRow) -> anyhow::Result<Launch> {
        let worker_id: Option<String> = row.try_get("worker_id")?;
        let raw: Option<String> = row.try_get("build_intent")?;
        let launch = Launch {
            id: row.try_get::<String, _>("id")?.parse()?,
            connection: row.try_get("connection")?,
            external_id: row.try_get("external_id")?,
            label: row.try_get("label")?,
            state: row.try_get::<String, _>("state")?.parse()?,
            worker_id: worker_id.map(|id| id.parse()).transpose()?,
            swapped: row.try_get("swapped")?,
            dispatched_at: OffsetDateTime::from_unix_timestamp(row.try_get("dispatched_at")?)?,
            build_intent: raw.map(|raw| serde_json::from_str(&raw)).transpose()?,
            registration_started: row.try_get("registration_started")?,
            secret_uid: row.try_get("secret_uid")?,
            pod_uid: row.try_get("pod_uid")?,
        };
        if let Some(intent) = &launch.build_intent {
            intent.validate(&launch)?;
        }
        Ok(launch)
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
        if let Some(intent) = &launch.build_intent {
            intent.validate(launch)?;
        }
        sqlx::query(
            "INSERT INTO launch (id, connection, external_id, label, state, worker_id, swapped, dispatched_at, updated_at, build_intent, registration_started, secret_uid, pod_uid)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(launch.id.to_string())
        .bind(&launch.connection)
        .bind(&launch.external_id)
        .bind(&launch.label)
        .bind(launch.state.as_str())
        .bind(launch.worker_id.map(|id| id.to_string()))
        .bind(launch.swapped)
        .bind(launch.dispatched_at.unix_timestamp())
        .bind(now())
        .bind(launch.build_intent.as_ref().map(serde_json::to_string).transpose()?)
        .bind(launch.registration_started)
        .bind(&launch.secret_uid)
        .bind(&launch.pod_uid)
        .execute(&self.0)
        .await?;
        Ok(())
    }

    pub async fn claim_build_registration(&self, launch: &Launch) -> anyhow::Result<bool> {
        Ok(sqlx::query(
            "UPDATE launch SET registration_started = 1, updated_at = ?
             WHERE id = ? AND connection = ? AND build_intent IS NOT NULL
             AND registration_started = 0 AND state IN (?, ?, ?, ?)",
        )
        .bind(now())
        .bind(launch.id.to_string())
        .bind(&launch.connection)
        .bind(OPEN_STATES[0].as_str())
        .bind(OPEN_STATES[1].as_str())
        .bind(OPEN_STATES[2].as_str())
        .bind(OPEN_STATES[3].as_str())
        .execute(&self.0)
        .await?
        .rows_affected()
            == 1)
    }

    pub async fn bind_build_objects(
        &self,
        launch: &Launch,
        secret_uid: &str,
        pod_uid: Option<&str>,
    ) -> anyhow::Result<()> {
        ensure!(
            !secret_uid.is_empty() && pod_uid.is_none_or(|uid| !uid.is_empty()),
            "build object UID is empty"
        );
        let bound = sqlx::query(
            "UPDATE launch SET secret_uid = ?, pod_uid = coalesce(pod_uid, ?), updated_at = ?
             WHERE id = ? AND connection = ? AND build_intent IS NOT NULL
             AND (secret_uid IS NULL OR secret_uid = ?)
             AND (? IS NULL OR pod_uid IS NULL OR pod_uid = ?)
             AND state IN (?, ?, ?, ?)",
        )
        .bind(secret_uid)
        .bind(pod_uid)
        .bind(now())
        .bind(launch.id.to_string())
        .bind(&launch.connection)
        .bind(secret_uid)
        .bind(pod_uid)
        .bind(pod_uid)
        .bind(OPEN_STATES[0].as_str())
        .bind(OPEN_STATES[1].as_str())
        .bind(OPEN_STATES[2].as_str())
        .bind(OPEN_STATES[3].as_str())
        .execute(&self.0)
        .await?
        .rows_affected();
        ensure!(
            bound == 1,
            "build object identity changed or launch is closed"
        );
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

    pub async fn launch_by_external_id(
        &self,
        connection: &str,
        external_id: &str,
    ) -> anyhow::Result<Option<Launch>> {
        let row = sqlx::query("SELECT * FROM launch WHERE connection = ? AND external_id = ?")
            .bind(connection)
            .bind(external_id)
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
        self.move_launch(
            id,
            LaunchState::Cancelled,
            &waiting,
            " AND swapped = 0 AND build_intent IS NULL",
        )
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
             WHERE id = ? AND swapped = 0 AND state IN (?, ?, ?, ?)
             AND (build_intent IS NULL OR worker_id = ?)
             AND NOT EXISTS (
                SELECT 1 FROM launch reserved WHERE reserved.id != launch.id
                AND reserved.worker_id = ?
                AND (reserved.build_intent IS NOT NULL OR launch.build_intent IS NOT NULL)
             )",
        )
        .bind(worker_id.to_string())
        .bind(now())
        .bind(id.to_string())
        .bind(OPEN_STATES[0].as_str())
        .bind(OPEN_STATES[1].as_str())
        .bind(OPEN_STATES[2].as_str())
        .bind(OPEN_STATES[3].as_str())
        .bind(worker_id.to_string())
        .bind(worker_id.to_string())
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
    use crate::capacity::{BuilderResources, CapacityCapabilities};
    use time::Duration;

    fn prepared_launch(connection: &str, external: &str) -> Launch {
        let worker_id = Uuid::new_v4();
        let mut launch = Launch {
            worker_id: Some(worker_id),
            ..Launch::new(connection, external, "builder")
        };
        launch.build_intent = Some(BuildLaunchIntent {
            profile: BuildCapacityProfile {
                name: "builder".into(),
                worker_id,
                capabilities: CapacityCapabilities {
                    build: true,
                    ..Default::default()
                },
                resources: BuilderResources {
                    architectures: vec!["x86_64-linux".into()],
                    system_features: vec![],
                    max_concurrent_builds: 4,
                    cpu_count: 4,
                    cpu_core_score: 4_000,
                    ram_total_mb: 16_000,
                    build_cores: None,
                },
            },
            startup_deadline: launch.dispatched_at + Duration::minutes(2),
            kubernetes: None,
        });
        launch
    }

    #[tokio::test]
    async fn build_intent_survives_restart_and_state_changes_without_renewal() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite");
        let launch = prepared_launch("a", "1");
        let state = State::open(&path).await.unwrap();
        state.insert_launch(&launch).await.unwrap();
        assert!(state.claim_build_registration(&launch).await.unwrap());
        assert!(!state.claim_build_registration(&launch).await.unwrap());
        state
            .bind_build_objects(&launch, "secret-a", Some("pod-a"))
            .await
            .unwrap();
        state
            .advance_launch(launch.id, LaunchState::Queued)
            .await
            .unwrap();
        state.0.close().await;
        let restored = State::open(&path).await.unwrap();
        let saved = restored.launch("a", launch.id).await.unwrap().unwrap();
        assert_eq!(saved.worker_id, launch.worker_id);
        assert_eq!(saved.build_intent, launch.build_intent);
        assert_eq!(saved.state, LaunchState::Queued);
        assert!(saved.registration_started);
        assert_eq!(saved.secret_uid.as_deref(), Some("secret-a"));
        assert_eq!(saved.pod_uid.as_deref(), Some("pod-a"));
        assert!(
            restored
                .bind_build_objects(&saved, "secret-b", Some("pod-a"))
                .await
                .is_err()
        );
        assert!(
            restored
                .bind_build_objects(&saved, "secret-a", Some("pod-b"))
                .await
                .is_err()
        );
        assert!(!restored.claim_cancel(launch.id).await.unwrap());
        assert!(
            !restored
                .mark_swapped(launch.id, Uuid::new_v4())
                .await
                .unwrap()
        );
        assert!(
            restored
                .mark_swapped(launch.id, launch.worker_id.unwrap())
                .await
                .unwrap()
        );
        let saved = restored.launch("a", launch.id).await.unwrap().unwrap();
        assert_eq!(saved.build_intent, launch.build_intent);
    }

    #[tokio::test]
    async fn build_intent_rejects_identity_collision_and_mutation() {
        let state = State::memory().await.unwrap();
        let launch = prepared_launch("a", "1");
        state.insert_launch(&launch).await.unwrap();
        let duplicate = Launch {
            id: Uuid::new_v4(),
            connection: "b".into(),
            external_id: "2".into(),
            ..launch.clone()
        };
        assert!(state.insert_launch(&duplicate).await.is_err());
        let legacy_collision = Launch {
            id: Uuid::new_v4(),
            connection: "b".into(),
            external_id: "legacy".into(),
            build_intent: None,
            ..launch.clone()
        };
        assert!(state.insert_launch(&legacy_collision).await.is_err());
        let legacy = Launch::new("b", "swap", "builder");
        state.insert_launch(&legacy).await.unwrap();
        assert!(
            !state
                .mark_swapped(legacy.id, launch.worker_id.unwrap())
                .await
                .unwrap()
        );
        assert!(
            sqlx::query("UPDATE launch SET worker_id = ? WHERE id = ?")
                .bind(launch.worker_id.unwrap().to_string())
                .bind(legacy.id.to_string())
                .execute(&state.0)
                .await
                .is_err()
        );
        let legacy_first = Launch {
            worker_id: Some(Uuid::new_v4()),
            ..Launch::new("c", "legacy-first", "builder")
        };
        state.insert_launch(&legacy_first).await.unwrap();
        let mut prepared_second = prepared_launch("c", "prepared-second");
        prepared_second.worker_id = legacy_first.worker_id;
        prepared_second
            .build_intent
            .as_mut()
            .unwrap()
            .profile
            .worker_id = legacy_first.worker_id.unwrap();
        assert!(state.insert_launch(&prepared_second).await.is_err());
        for (column, value) in [
            ("worker_id", Uuid::new_v4().to_string()),
            ("build_intent", "{}".into()),
            ("label", "changed".into()),
            ("connection", "b".into()),
            ("external_id", "changed".into()),
            ("dispatched_at", "1".into()),
        ] {
            assert!(
                sqlx::query(&format!("UPDATE launch SET {column} = ? WHERE id = ?"))
                    .bind(value)
                    .bind(launch.id.to_string())
                    .execute(&state.0)
                    .await
                    .is_err(),
                "{column}"
            );
        }
        assert_eq!(
            state
                .launch("a", launch.id)
                .await
                .unwrap()
                .unwrap()
                .build_intent,
            launch.build_intent
        );
        assert_eq!(state.open_launches("a").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn old_launch_rows_migrate_without_assumed_build_capacity() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::from_str("sqlite::memory:").unwrap())
            .await
            .unwrap();
        let mut previous = sqlx::migrate!("./migrations");
        previous.migrations = std::borrow::Cow::Owned(
            previous
                .iter()
                .filter(|migration| migration.version < 3)
                .cloned()
                .collect(),
        );
        previous.run(&pool).await.unwrap();
        let legacy = Launch::new("a", "42", "builder");
        sqlx::query("INSERT INTO launch (id, connection, external_id, label, state, dispatched_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(legacy.id.to_string()).bind("a").bind("42").bind("builder").bind("queued")
            .bind(legacy.dispatched_at.unix_timestamp()).bind(now()).execute(&pool).await.unwrap();
        let state = State::migrated(pool).await.unwrap();
        let saved = state.launch("a", legacy.id).await.unwrap().unwrap();
        assert_eq!(saved.state, LaunchState::Queued);
        assert!(saved.build_intent.is_none());
        assert!(!saved.registration_started);
        assert!(state.claim_cancel(saved.id).await.unwrap());
    }

    #[tokio::test]
    async fn mark_swapped_only_once() {
        let state = State::memory().await.unwrap();
        let launch = Launch::new("a", "7", "ubuntu-latest");
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
        let swapped = Launch::new("a", "1", "ubuntu-latest");
        let cancelled = Launch::new("a", "2", "ubuntu-latest");
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
        let launch = Launch::new("a", "1", "ubuntu-latest");
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
        let own = Launch::new("team-a", "1", "ubuntu-latest");
        let other = Launch::new("team-b", "2", "ubuntu-latest");
        state.insert_launch(&own).await.unwrap();
        state.insert_launch(&other).await.unwrap();

        let open = state.open_launches("team-a").await.unwrap();
        assert_eq!(open.iter().map(|l| l.id).collect::<Vec<_>>(), [own.id]);
        assert!(
            state
                .launch_by_external_id("team-a", "2")
                .await
                .unwrap()
                .is_none()
        );
        assert!(state.launch("team-a", other.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn closed_launches_are_not_open() {
        let state = State::memory().await.unwrap();
        let launch = Launch::new("a", "1", "ubuntu-latest");
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
