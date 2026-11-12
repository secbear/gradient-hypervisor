use crate::config::WorkerCapabilities;
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use time::error::{Parse, TryFromParsed};
use time::format_description::well_known::{Iso8601, Rfc3339};
use time::{OffsetDateTime, PrimitiveDateTime};
use uuid::Uuid;

pub const MAX_PROFILES: usize = 16;
pub const MAX_TRACKED_PROFILES: usize = 256;
pub const MAX_PROJECTS: usize = 256;
pub const MAX_BUILDS: usize = 128;
pub const MAX_WORKERS: usize = 256;
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityAssessmentRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projects: Option<Vec<Uuid>>,
    pub profiles: Vec<CapacityProfileRequest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracked_profiles: Vec<CapacityProfileRequest>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_waiting_builds: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityProfileRequest {
    pub profile: BuildCapacityProfile,
    pub enable_network: bool,
    pub enable_build: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildCapacityProfile {
    pub name: String,
    pub worker_id: Uuid,
    pub capabilities: CapacityCapabilities,
    pub resources: BuilderResources,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityCapabilities {
    pub core: bool,
    pub federate: bool,
    pub eval: bool,
    pub build: bool,
    pub cache: bool,
    pub network: bool,
    pub secrets: bool,
    pub exec: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuilderResources {
    pub architectures: Vec<String>,
    pub system_features: Vec<String>,
    pub max_concurrent_builds: u32,
    pub cpu_count: u32,
    pub ram_total_mb: u64,
    pub cpu_core_score: u32,
    pub build_cores: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapacityAssessment {
    pub captured_at: String,
    pub server_draining: bool,
    pub total_ready_builds: usize,
    pub builds_truncated: bool,
    pub workers: Vec<CapacityWorker>,
    pub profiles: Vec<BuildCapacityProfile>,
    pub builds: Vec<CapacityBuild>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracked_workers: Vec<TrackedCapacityWorker>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityWorker {
    pub free_slots: u32,
    pub available_ram_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_build: Option<CapacityWaitingBuild>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityWaitingBuild {
    pub build_index: usize,
    pub ram_credit_mb: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedCapacityWorker {
    pub profile: BuildCapacityProfile,
    pub connected: bool,
    pub worker_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapacityBuild {
    pub architecture: String,
    pub required_features: Vec<String>,
    pub is_fixed_output: bool,
    pub ready_at: String,
    pub workers: Vec<Option<ResourceAssessment>>,
    pub profiles: Vec<Option<ProfileAssessment>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracked_workers: Vec<Option<ProfileAssessment>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceAssessment {
    pub cores: u32,
    pub reserved_ram_mb: Option<u64>,
    pub predicted_ram_mb: Option<u64>,
    pub ram_fit: Option<RamFit>,
    pub memory_assumption: MemoryAssumption,
    pub own_samples: u32,
    pub build_time_known: bool,
    pub time: CapacityTimeEstimate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileAssessment {
    pub resources: ResourceAssessment,
    pub prediction_fit: Option<RamFit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RamFit {
    Fits,
    FitsOnceRamIsReleased,
    DoesNotFit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAssumption {
    Predicted,
    Unmeasured,
    Negligible,
    Proportional,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CapacityTimeEstimate {
    pub download_secs: f64,
    pub path_secs: f64,
    pub build_secs: f64,
    pub upload_secs: f64,
    pub eval_secs: f64,
    pub nar_bytes: f64,
    pub paths: f64,
    pub output_nar_bytes: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cores: Option<u32>,
    pub fallbacks: Vec<String>,
}

pub fn parse_timestamp(value: &str) -> anyhow::Result<OffsetDateTime> {
    match OffsetDateTime::parse(value, &Iso8601::DEFAULT) {
        Err(Parse::TryFromParsed(TryFromParsed::InsufficientInformation)) => {
            PrimitiveDateTime::parse(value, &Iso8601::DEFAULT).map(PrimitiveDateTime::assume_utc)
        }
        Ok(_) => OffsetDateTime::parse(value, &Rfc3339),
        result => result,
    }
    .context("invalid capacity timestamp")
}

impl CapacityAssessmentRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.profiles.len() <= MAX_PROFILES,
            "too many capacity profiles"
        );
        ensure!(
            self.tracked_profiles.len() <= MAX_TRACKED_PROFILES,
            "too many tracked capacity profiles"
        );
        ensure!(
            self.projects
                .as_ref()
                .is_none_or(|projects| projects.len() <= MAX_PROJECTS),
            "too many capacity projects"
        );
        let mut tracked_ids = HashSet::new();
        for requests in [&self.profiles, &self.tracked_profiles] {
            let mut names = HashSet::new();
            for request in requests {
                request.profile.validate()?;
                ensure!(
                    request.enable_build,
                    "capacity registration does not enable builds"
                );
                ensure!(
                    names.insert(&request.profile.name),
                    "duplicate capacity profile name"
                );
            }
        }
        for request in &self.tracked_profiles {
            ensure!(
                tracked_ids.insert(request.profile.worker_id),
                "duplicate tracked capacity worker ID"
            );
        }
        ensure!(
            self.profiles
                .iter()
                .all(|request| !tracked_ids.contains(&request.profile.worker_id)),
            "capacity worker ID is both prospective and tracked"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_REQUEST_BYTES,
            "capacity request exceeds 64 KiB"
        );
        Ok(())
    }
}

impl BuildCapacityProfile {
    pub(crate) fn registration_capabilities(&self) -> WorkerCapabilities {
        WorkerCapabilities {
            build: self.capabilities.build,
            network: self.capabilities.network,
            secrets: self.capabilities.secrets,
            exec: self.capabilities.exec,
            eval: self.capabilities.eval,
        }
    }

    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            !self.name.trim().is_empty(),
            "capacity profile name is empty"
        );
        ensure!(
            self.capabilities.build,
            "capacity profile does not permit builds"
        );
        ensure!(
            self.resources.cpu_count > 0,
            "capacity profile has no CPU capacity"
        );
        ensure!(
            self.resources.ram_total_mb > 0,
            "capacity profile has no build RAM budget"
        );
        ensure!(
            self.resources.max_concurrent_builds > 0,
            "capacity profile has no build slots"
        );
        Ok(())
    }

    fn validate_echo(&self, request: &CapacityProfileRequest, tracked: bool) -> anyhow::Result<()> {
        let intended = &request.profile;
        ensure!(
            self.name == intended.name && self.worker_id == intended.worker_id,
            "capacity profile identity does not match request"
        );
        ensure!(
            self.resources == intended.resources,
            "capacity profile resources do not match request"
        );
        let mut effective = intended.capabilities;
        effective.build &= request.enable_build;
        effective.network &= request.enable_network;
        if tracked && !self.capabilities.network {
            effective.network = false;
        }
        ensure!(
            self.capabilities == effective,
            "capacity profile capabilities do not match registration intersection"
        );
        Ok(())
    }
}

impl CapacityAssessment {
    pub fn validate_against(&self, request: &CapacityAssessmentRequest) -> anyhow::Result<()> {
        request.validate()?;
        ensure!(self.builds.len() <= MAX_BUILDS, "too many capacity builds");
        ensure!(
            self.workers.len() <= MAX_WORKERS,
            "too many capacity workers"
        );
        ensure!(
            self.profiles.len() == request.profiles.len(),
            "capacity profile count does not match request"
        );
        ensure!(
            self.tracked_workers.len() == request.tracked_profiles.len(),
            "tracked capacity worker count does not match request"
        );
        ensure!(
            self.total_ready_builds >= self.builds.len(),
            "capacity backlog is smaller than the build sample"
        );
        ensure!(
            self.builds_truncated == (self.total_ready_builds > self.builds.len()),
            "capacity truncation flag does not match backlog"
        );
        parse_timestamp(&self.captured_at)?;
        for (profile, intended) in self.profiles.iter().zip(&request.profiles) {
            profile.validate_echo(intended, false)?;
        }
        let mut correlated_indexes = HashSet::new();
        for (worker, intended) in self.tracked_workers.iter().zip(&request.tracked_profiles) {
            worker.profile.validate_echo(intended, true)?;
            if let Some(index) = worker.worker_index {
                ensure!(
                    worker.connected,
                    "unconnected tracked worker has a live index"
                );
                ensure!(
                    index < self.workers.len(),
                    "tracked capacity worker index is out of bounds"
                );
                ensure!(
                    correlated_indexes.insert(index),
                    "duplicate tracked capacity worker index"
                );
            }
        }
        for worker in &self.workers {
            if let Some(waiting) = &worker.waiting_build {
                ensure!(
                    request.include_waiting_builds,
                    "unsolicited waiting-build RAM credit"
                );
                ensure!(
                    waiting.build_index < self.builds.len(),
                    "waiting build index is out of bounds"
                );
                ensure!(
                    waiting.ram_credit_mb > 0,
                    "waiting-build RAM credit is zero"
                );
                let available = worker
                    .available_ram_mb
                    .context("waiting-build RAM credit has no RAM budget")?;
                ensure!(
                    available.checked_add(waiting.ram_credit_mb).is_some(),
                    "waiting-build RAM credit overflows the budget"
                );
            }
        }
        for build in &self.builds {
            parse_timestamp(&build.ready_at)?;
            ensure!(
                build.workers.len() == self.workers.len()
                    && build.profiles.len() == self.profiles.len()
                    && build.tracked_workers.len() == self.tracked_workers.len(),
                "capacity build matrix dimensions do not match headers"
            );
            for assessment in build.workers.iter().flatten() {
                assessment.validate()?;
            }
            for (assessment, profile) in build.profiles.iter().zip(&self.profiles) {
                if let Some(assessment) = assessment {
                    assessment.resources.validate_for(&profile.resources)?;
                }
            }
            for (assessment, worker) in build.tracked_workers.iter().zip(&self.tracked_workers) {
                ensure!(
                    !worker.connected || assessment.is_none(),
                    "connected tracked worker received starting capacity"
                );
                if let Some(assessment) = assessment {
                    assessment
                        .resources
                        .validate_for(&worker.profile.resources)?;
                }
            }
        }
        Ok(())
    }
}

impl ResourceAssessment {
    fn validate(&self) -> anyhow::Result<()> {
        ensure!(self.cores > 0, "capacity assessment has zero cores");
        self.time.validate()?;
        ensure!(
            self.time.cores.is_none_or(|cores| cores == self.cores),
            "capacity time cores do not match resource cores"
        );
        Ok(())
    }

    fn validate_for(&self, resources: &BuilderResources) -> anyhow::Result<()> {
        self.validate()?;
        let core_limit = resources
            .build_cores
            .filter(|cores| *cores > 0)
            .unwrap_or(resources.cpu_count)
            .min(resources.cpu_count);
        ensure!(
            self.cores <= core_limit,
            "capacity assessment exceeds profile cores"
        );
        Ok(())
    }
}

impl CapacityTimeEstimate {
    pub fn total_secs(&self) -> f64 {
        self.download_secs + self.path_secs + self.build_secs + self.upload_secs + self.eval_secs
    }

    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            [
                self.download_secs,
                self.path_secs,
                self.build_secs,
                self.upload_secs,
                self.eval_secs,
                self.nar_bytes,
                self.paths,
                self.output_nar_bytes,
                self.total_secs(),
            ]
            .into_iter()
            .all(|value| value.is_finite() && value >= 0.0),
            "capacity time estimate must be finite and nonnegative"
        );
        Ok(())
    }
}
