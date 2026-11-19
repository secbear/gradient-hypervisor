use crate::capacity::{
    CapacityAssessment, CapacityAssessmentRequest, CapacityBuild, MemoryAssumption,
    ProfileAssessment, RamFit, ResourceAssessment, parse_timestamp,
};
use crate::planner::Limits;
use crate::state::{Launch, LaunchState};
use anyhow::ensure;
use std::collections::{HashMap, HashSet};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct BuildScalingPolicy {
    pub queue_wait: Duration,
    pub startup_grace: Duration,
    pub max_new_workers: u32,
    pub fallback_ram_mb: u64,
}

pub struct BuildPlanningContext<'a> {
    pub connection: &'a str,
    pub open: &'a [Launch],
    /// Kind labels in the same order as the request's prospective profiles.
    pub candidate_labels: &'a [String],
    pub limits: &'a Limits,
    pub backoff: &'a HashSet<String>,
    pub policy: &'a BuildScalingPolicy,
    pub now: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedBuildWorker {
    pub profile_index: usize,
    pub label: String,
    pub worker_id: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildWaitReason {
    QueueGrace,
    LaunchLimit,
    NoMatchingProfile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredBuild {
    pub build_index: usize,
    pub reason: BuildWaitReason,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct BuildPlan {
    pub proposed_workers: Vec<PlannedBuildWorker>,
    pub deferred_builds: Vec<DeferredBuild>,
    pub credited_starting_workers: Vec<Uuid>,
    pub builds_truncated: bool,
    pub server_draining: bool,
}

struct Budget {
    slots: u32,
    ram_mb: Option<u64>,
    waiting: Option<(usize, u64)>,
}

impl Budget {
    fn remaining_after(&self, build: usize, needed: u64) -> Option<u64> {
        if self.slots == 0 {
            return None;
        }
        let available = self.ram_mb?;
        let credit = self
            .waiting
            .filter(|(owner, _)| *owner == build)
            .map_or(0, |(_, credit)| credit);
        available.checked_add(credit)?.checked_sub(needed)
    }

    fn take(&mut self, build: usize, needed: u64) -> bool {
        let Some(remaining) = self.remaining_after(build, needed) else {
            return false;
        };
        self.slots -= 1;
        self.ram_mb = Some(remaining);
        if self.waiting.is_some_and(|(owner, _)| owner == build) {
            self.waiting = None;
        }
        true
    }
}

fn budget_order<'a, 'b>(
    budgets: impl Iterator<Item = (usize, &'a Budget)>,
    builds: impl Iterator<Item = (usize, &'b CapacityBuild)> + Clone,
    needed: impl Fn(&CapacityBuild, usize) -> Option<u64>,
) -> Vec<usize> {
    let mut order = budgets
        .map(|(budget_index, budget)| {
            let flexibility = builds
                .clone()
                .filter(|(build_index, build)| {
                    needed(build, budget_index)
                        .and_then(|ram| budget.remaining_after(*build_index, ram))
                        .is_some()
                })
                .count();
            (flexibility, budget_index)
        })
        .collect::<Vec<_>>();
    order.sort_unstable();
    order.into_iter().map(|(_, index)| index).collect()
}

fn ram_need(resources: &ResourceAssessment, policy: &BuildScalingPolicy) -> u64 {
    if resources.memory_assumption == MemoryAssumption::Negligible {
        return policy.fallback_ram_mb;
    }
    resources
        .predicted_ram_mb
        .or(resources.reserved_ram_mb)
        .unwrap_or(policy.fallback_ram_mb)
}

fn live_need(resources: &ResourceAssessment, policy: &BuildScalingPolicy) -> Option<u64> {
    (resources.ram_fit != Some(RamFit::DoesNotFit)).then(|| ram_need(resources, policy))
}

fn profile_need(assessment: &ProfileAssessment, policy: &BuildScalingPolicy) -> Option<u64> {
    if assessment.prediction_fit == Some(RamFit::DoesNotFit)
        || assessment.resources.ram_fit == Some(RamFit::DoesNotFit)
    {
        return None;
    }
    Some(ram_need(&assessment.resources, policy))
}

pub fn plan_build_capacity(
    request: &CapacityAssessmentRequest,
    capacity: &CapacityAssessment,
    context: BuildPlanningContext<'_>,
) -> anyhow::Result<BuildPlan> {
    capacity.validate_against(request)?;
    ensure!(
        request.include_waiting_builds,
        "build planning requires waiting-build budget credit"
    );
    ensure!(
        context.policy.queue_wait >= Duration::ZERO,
        "queue grace cannot be negative"
    );
    ensure!(
        context.policy.startup_grace > Duration::ZERO,
        "startup grace must be positive"
    );
    ensure!(
        context.policy.fallback_ram_mb > 0,
        "unknown-build RAM fallback must be positive"
    );
    ensure!(
        context.candidate_labels.len() == request.profiles.len(),
        "candidate kind labels do not match profiles",
    );

    let mut launch_ids = HashSet::new();
    let mut worker_ids = HashSet::new();
    let mut per_label = HashMap::<&str, u32>::new();
    for launch in context.open {
        ensure!(
            launch.connection == context.connection && launch.state.is_open(),
            "build planner received a launch outside its open connection scope"
        );
        ensure!(launch_ids.insert(launch.id), "duplicate open launch ID");
        if let Some(worker) = launch.worker_id {
            ensure!(worker_ids.insert(worker), "duplicate open worker ID");
        }
        let count = per_label.entry(&launch.label).or_default();
        *count = count.saturating_add(1);
    }

    let mut result = BuildPlan {
        builds_truncated: capacity.builds_truncated,
        server_draining: capacity.server_draining,
        ..Default::default()
    };
    if capacity.server_draining {
        return Ok(result);
    }
    let captured_at = parse_timestamp(&capacity.captured_at)?;
    let mut order = capacity
        .builds
        .iter()
        .enumerate()
        .map(|(index, build)| Ok((parse_timestamp(&build.ready_at)?, index)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    order.sort_unstable();
    let mut live = capacity
        .workers
        .iter()
        .map(|worker| Budget {
            slots: worker.free_slots,
            ram_mb: worker.available_ram_mb,
            waiting: worker
                .waiting_build
                .as_ref()
                .map(|waiting| (waiting.build_index, waiting.ram_credit_mb)),
        })
        .collect::<Vec<_>>();
    let mut starting = capacity
        .tracked_workers
        .iter()
        .enumerate()
        .map(|(tracked, worker)| {
            let launch = context
                .open
                .iter()
                .find(|launch| launch.worker_id == Some(worker.profile.worker_id));
            let eligible = launch.is_some_and(|launch| {
                let age = context.now - launch.dispatched_at;
                !worker.connected
                    && launch.cleanup_outcome.is_none()
                    && launch.state != LaunchState::Connected
                    && launch.registration_started
                    && age >= Duration::ZERO
                    && launch.build_intent.as_ref().is_some_and(|intent| {
                        intent.profile == request.tracked_profiles[tracked].profile
                            && context.now < intent.startup_deadline
                    })
            });
            Budget {
                slots: if eligible {
                    worker.profile.resources.max_concurrent_builds
                } else {
                    0
                },
                ram_mb: Some(worker.profile.resources.ram_total_mb),
                waiting: None,
            }
        })
        .collect::<Vec<_>>();
    let mut selected = Vec::<(usize, Budget)>::new();
    let mut used_starting = HashSet::new();
    let mut total = u32::try_from(context.open.len()).unwrap_or(u32::MAX);

    for (position, &(ready_at, index)) in order.iter().enumerate() {
        let build = &capacity.builds[index];
        let remaining = order[position..]
            .iter()
            .map(|(_, index)| (*index, &capacity.builds[*index]));
        let live_order = budget_order(
            live.iter().enumerate(),
            remaining.clone(),
            |build, worker| {
                build.workers[worker]
                    .as_ref()
                    .and_then(|assessment| live_need(assessment, context.policy))
            },
        );
        if live_order.into_iter().any(|worker| {
            build.workers[worker]
                .as_ref()
                .and_then(|assessment| live_need(assessment, context.policy))
                .is_some_and(|needed| live[worker].take(index, needed))
        }) {
            continue;
        }
        let starting_order = budget_order(
            starting.iter().enumerate(),
            remaining.clone(),
            |build, worker| {
                build.tracked_workers[worker]
                    .as_ref()
                    .and_then(|assessment| profile_need(assessment, context.policy))
            },
        );
        if starting_order.into_iter().any(|tracked| {
            let taken = build.tracked_workers[tracked]
                .as_ref()
                .and_then(|assessment| profile_need(assessment, context.policy))
                .is_some_and(|needed| starting[tracked].take(index, needed));
            if taken {
                used_starting.insert(tracked);
            }
            taken
        }) {
            continue;
        }
        let selected_order = budget_order(
            selected
                .iter()
                .enumerate()
                .map(|(index, (_, budget))| (index, budget)),
            remaining,
            |build, index| {
                build.profiles[selected[index].0]
                    .as_ref()
                    .and_then(|assessment| profile_need(assessment, context.policy))
            },
        );
        if selected_order.into_iter().any(|selected_index| {
            let (profile, budget) = &mut selected[selected_index];
            build.profiles[*profile]
                .as_ref()
                .and_then(|assessment| profile_need(assessment, context.policy))
                .is_some_and(|needed| budget.take(index, needed))
        }) {
            continue;
        }
        let has_eligible_live = build.workers.iter().any(Option::is_some);
        if has_eligible_live && captured_at - ready_at < context.policy.queue_wait {
            result.deferred_builds.push(DeferredBuild {
                build_index: index,
                reason: BuildWaitReason::QueueGrace,
            });
            continue;
        }
        let mut fitting = false;
        let mut launched = false;
        for (profile, candidate) in capacity.profiles.iter().enumerate() {
            let label = &context.candidate_labels[profile];
            if !context.limits.allowed.contains(label) || context.backoff.contains(label) {
                continue;
            }
            let Some(needed) = build.profiles[profile]
                .as_ref()
                .and_then(|assessment| profile_need(assessment, context.policy))
            else {
                continue;
            };
            let mut budget = Budget {
                slots: candidate.resources.max_concurrent_builds,
                ram_mb: Some(candidate.resources.ram_total_mb),
                waiting: None,
            };
            if !budget.take(index, needed) {
                continue;
            }
            fitting = true;
            if worker_ids.contains(&candidate.worker_id) {
                continue;
            }
            let count = per_label.get(label.as_str()).copied().unwrap_or(0);
            if total >= context.limits.max_launches
                || count >= context.limits.max_launches_per_kind
                || u32::try_from(result.proposed_workers.len()).unwrap_or(u32::MAX)
                    >= context.policy.max_new_workers
            {
                continue;
            }
            total = total.saturating_add(1);
            per_label.insert(label, count.saturating_add(1));
            worker_ids.insert(candidate.worker_id);
            selected.push((profile, budget));
            result.proposed_workers.push(PlannedBuildWorker {
                profile_index: profile,
                label: label.clone(),
                worker_id: candidate.worker_id,
            });
            launched = true;
            break;
        }
        if !launched {
            result.deferred_builds.push(DeferredBuild {
                build_index: index,
                reason: if fitting {
                    BuildWaitReason::LaunchLimit
                } else {
                    BuildWaitReason::NoMatchingProfile
                },
            });
        }
    }
    let mut used_starting = used_starting.into_iter().collect::<Vec<_>>();
    used_starting.sort_unstable();
    result.credited_starting_workers = used_starting
        .into_iter()
        .map(|index| capacity.tracked_workers[index].profile.worker_id)
        .collect();
    Ok(result)
}
