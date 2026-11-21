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
pub const MAX_BOOTSTRAP_DEMANDS: usize = 512;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_page: Option<BuildPageRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap: Option<BootstrapCapacityRequest>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildPageRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration: Option<RegistrationCapabilities>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrationCapabilities {
    pub enable_build: bool,
    pub enable_network: bool,
    pub enable_eval: bool,
    pub enable_secrets: bool,
    pub enable_exec: bool,
}

impl From<WorkerCapabilities> for RegistrationCapabilities {
    fn from(capabilities: WorkerCapabilities) -> Self {
        Self {
            enable_build: capabilities.build,
            enable_network: capabilities.network,
            enable_eval: capabilities.eval,
            enable_secrets: capabilities.secrets,
            enable_exec: capabilities.exec,
        }
    }
}

impl RegistrationCapabilities {
    fn intersect(self, mut capabilities: CapacityCapabilities) -> CapacityCapabilities {
        capabilities.build &= self.enable_build;
        capabilities.network &= self.enable_network;
        capabilities.eval &= self.enable_eval;
        capabilities.secrets &= self.enable_secrets;
        capabilities.exec &= self.enable_exec;
        capabilities
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapCapacityRequest {
    pub profiles: Vec<BootstrapProfileRequest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracked_profiles: Vec<BootstrapProfileRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapProfileRequest {
    pub profile: CapabilityProfile,
    pub registration: RegistrationCapabilities,
}

pub type CapabilityProfile = BuildCapacityProfile;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_page: Option<BuildPageAssessment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap: Option<BootstrapCapacityAssessment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildPageAssessment {
    pub remaining_ready_builds: usize,
    pub next: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapCapacityAssessment {
    pub observed_at: String,
    pub complete: bool,
    pub demands: Vec<BootstrapCapacityDemand>,
    pub profiles: Vec<BootstrapCapacityProfile>,
    pub tracked_workers: Vec<BootstrapTrackedWorker>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapCapacityDemand {
    pub kind: BootstrapDemandKind,
    pub available: bool,
    pub profiles: Vec<bool>,
    pub tracked_workers: Vec<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapDemandKind {
    Download,
    Substitution,
    Evaluation,
    InputFetch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapCapacityProfile {
    pub profile: CapabilityProfile,
    pub build_profile_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapTrackedWorker {
    pub profile: CapabilityProfile,
    pub connected: bool,
    pub build_tracked_index: Option<usize>,
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
                    request.enable_build && request.effective_capabilities().build,
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
        if let Some(page) = &self.build_page {
            ensure!(
                page.after.as_ref().is_none_or(|after| !after.is_empty()),
                "capacity page cursor is empty"
            );
        }
        if let Some(bootstrap) = &self.bootstrap {
            ensure!(
                self.profiles.len() + bootstrap.profiles.len() <= MAX_PROFILES,
                "too many combined bootstrap profiles"
            );
            ensure!(
                self.tracked_profiles.len() + bootstrap.tracked_profiles.len()
                    <= MAX_TRACKED_PROFILES,
                "too many combined bootstrap tracked profiles"
            );
            ensure!(
                self.profiles
                    .iter()
                    .all(|profile| profile.registration.is_some()),
                "bootstrap requires prospective builder registration"
            );
            let mut ids = HashSet::new();
            for request in self.profiles.iter().chain(&self.tracked_profiles) {
                ensure!(
                    ids.insert(request.profile.worker_id),
                    "duplicate bootstrap worker ID"
                );
            }
            for (ordinary, extras) in [
                (&self.profiles, &bootstrap.profiles),
                (&self.tracked_profiles, &bootstrap.tracked_profiles),
            ] {
                let mut names = ordinary
                    .iter()
                    .map(|request| &request.profile.name)
                    .collect::<HashSet<_>>();
                for extra in extras {
                    extra.profile.validate_general(
                        extra.registration.intersect(extra.profile.capabilities),
                    )?;
                    ensure!(
                        names.insert(&extra.profile.name),
                        "duplicate bootstrap profile name"
                    );
                    ensure!(
                        ids.insert(extra.profile.worker_id),
                        "duplicate bootstrap worker ID"
                    );
                }
            }
        }
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

    fn validate_general(&self, capabilities: CapacityCapabilities) -> anyhow::Result<()> {
        ensure!(
            !self.name.trim().is_empty(),
            "capacity profile name is empty"
        );
        ensure!(!self.worker_id.is_nil(), "bootstrap worker ID is nil");
        ensure!(
            capabilities.build || capabilities.eval || capabilities.network,
            "bootstrap profile has no supported role"
        );
        if capabilities.build {
            self.validate()?;
        }
        if capabilities.network {
            ensure!(
                self.resources.max_concurrent_builds > 0,
                "bootstrap network profile has no transfer slots"
            );
        }
        Ok(())
    }

    fn validate_identity_and_resources(&self, intended: &Self) -> anyhow::Result<()> {
        ensure!(
            self.name == intended.name && self.worker_id == intended.worker_id,
            "capacity profile identity does not match request"
        );
        ensure!(
            self.resources == intended.resources,
            "capacity profile resources do not match request"
        );
        Ok(())
    }

    fn validate_echo(&self, request: &CapacityProfileRequest, tracked: bool) -> anyhow::Result<()> {
        let intended = &request.profile;
        self.validate_identity_and_resources(intended)?;
        let mut effective = request.effective_capabilities();
        if tracked && !self.capabilities.network {
            effective.network = false;
        }
        ensure!(
            self.capabilities == effective,
            "capacity profile capabilities do not match registration intersection"
        );
        Ok(())
    }

    fn validate_bootstrap_echo(
        &self,
        intended: &Self,
        effective: CapacityCapabilities,
        tracked: bool,
    ) -> anyhow::Result<()> {
        self.validate_identity_and_resources(intended)?;
        let expected = if tracked {
            // The server owns tracked registration and may narrow all five permissions.
            RegistrationCapabilities::from(self.registration_capabilities()).intersect(effective)
        } else {
            effective
        };
        ensure!(
            self.capabilities == expected,
            "bootstrap profile capabilities do not match registration intersection"
        );
        Ok(())
    }
}

impl CapacityProfileRequest {
    fn effective_capabilities(&self) -> CapacityCapabilities {
        let mut effective = self
            .registration
            .map_or(self.profile.capabilities, |registration| {
                registration.intersect(self.profile.capabilities)
            });
        effective.build &= self.enable_build;
        effective.network &= self.enable_network;
        effective
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
        match (&request.build_page, &self.build_page) {
            (Some(input), Some(page)) => {
                let remaining = self.total_ready_builds - self.builds.len();
                ensure!(
                    page.remaining_ready_builds <= remaining
                        && (input.after.is_some() || page.remaining_ready_builds == remaining),
                    "capacity page remaining count does not match backlog"
                );
                ensure!(
                    page.next.as_ref().is_none_or(|next| !next.is_empty())
                        && page.next.is_some() == (page.remaining_ready_builds > 0),
                    "capacity page cursor does not match remaining builds"
                );
            }
            (Some(_), None) => anyhow::bail!("requested capacity build page is missing"),
            (None, Some(_)) => anyhow::bail!("unsolicited capacity build page"),
            (None, None) => {}
        }
        for (profile, intended) in self.profiles.iter().zip(&request.profiles) {
            profile.validate_echo(intended, false)?;
        }
        let mut correlated_indexes = HashSet::new();
        for (worker, intended) in self.tracked_workers.iter().zip(&request.tracked_profiles) {
            if request.bootstrap.is_some() {
                worker.profile.validate_bootstrap_echo(
                    &intended.profile,
                    intended.effective_capabilities(),
                    true,
                )?;
            } else {
                worker.profile.validate_echo(intended, true)?;
            }
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
                    ensure!(
                        profile.capabilities.build,
                        "build-disabled profile received build capacity"
                    );
                    assessment.resources.validate_for(&profile.resources)?;
                }
            }
            for (assessment, worker) in build.tracked_workers.iter().zip(&self.tracked_workers) {
                ensure!(
                    !worker.connected || assessment.is_none(),
                    "connected tracked worker received starting capacity"
                );
                if let Some(assessment) = assessment {
                    ensure!(
                        worker.profile.capabilities.build,
                        "build-disabled tracked worker received starting build capacity"
                    );
                    assessment
                        .resources
                        .validate_for(&worker.profile.resources)?;
                }
            }
        }
        match (&request.bootstrap, &self.bootstrap) {
            (Some(input), Some(bootstrap)) => bootstrap.validate_against(request, input, self)?,
            (Some(_), None) => anyhow::bail!("requested bootstrap capacity assessment is missing"),
            (None, Some(_)) => anyhow::bail!("unsolicited bootstrap capacity assessment"),
            (None, None) => {}
        }
        Ok(())
    }
}

impl BootstrapCapacityAssessment {
    fn validate_against(
        &self,
        request: &CapacityAssessmentRequest,
        bootstrap: &BootstrapCapacityRequest,
        capacity: &CapacityAssessment,
    ) -> anyhow::Result<()> {
        parse_timestamp(&self.observed_at)?;
        ensure!(
            self.demands.len() <= MAX_BOOTSTRAP_DEMANDS,
            "too many bootstrap demands"
        );
        ensure!(
            self.profiles.len() == request.profiles.len() + bootstrap.profiles.len(),
            "bootstrap profile count does not match request"
        );
        ensure!(
            self.tracked_workers.len()
                == request.tracked_profiles.len() + bootstrap.tracked_profiles.len(),
            "bootstrap tracked worker count does not match request"
        );
        for (index, profile) in self.profiles.iter().enumerate() {
            if index < request.profiles.len() {
                ensure!(
                    profile.build_profile_index == Some(index),
                    "bootstrap build profile mapping does not match request order"
                );
                ensure!(
                    profile.profile == capacity.profiles[index],
                    "bootstrap mapped build profile does not match capacity profile"
                );
            } else {
                let intended = &bootstrap.profiles[index - request.profiles.len()];
                ensure!(
                    profile.build_profile_index.is_none(),
                    "extra bootstrap profile has a build mapping"
                );
                profile.profile.validate_bootstrap_echo(
                    &intended.profile,
                    intended
                        .registration
                        .intersect(intended.profile.capabilities),
                    false,
                )?;
            }
        }
        for (index, worker) in self.tracked_workers.iter().enumerate() {
            if let Some(intended) = request.tracked_profiles.get(index) {
                ensure!(
                    worker.build_tracked_index == Some(index),
                    "bootstrap tracked build mapping does not match request order"
                );
                let ordinary = &capacity.tracked_workers[index];
                ensure!(
                    worker.connected == ordinary.connected,
                    "bootstrap tracked connection does not match build capacity"
                );
                worker.profile.validate_bootstrap_echo(
                    &intended.profile,
                    ordinary.profile.capabilities,
                    true,
                )?;
            } else {
                let intended = &bootstrap.tracked_profiles[index - request.tracked_profiles.len()];
                ensure!(
                    worker.build_tracked_index.is_none(),
                    "extra bootstrap tracked worker has a build mapping"
                );
                worker.profile.validate_bootstrap_echo(
                    &intended.profile,
                    intended
                        .registration
                        .intersect(intended.profile.capabilities),
                    true,
                )?;
            }
        }
        for demand in &self.demands {
            ensure!(
                demand.profiles.len() == self.profiles.len()
                    && demand.tracked_workers.len() == self.tracked_workers.len(),
                "bootstrap demand matrix dimensions do not match headers"
            );
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
