use crate::build_planner::{
    BuildPlan, BuildPlanningContext, BuildScalingPolicy, PlannedBuildWorker, plan_build_capacity,
    plan_build_capacity_seeded, profile_fits_build, starting_profile_is_eligible,
    validate_build_planning,
};
use crate::capacity::{CapacityAssessment, CapacityAssessmentRequest};
use anyhow::ensure;
use std::cmp::Reverse;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedCapabilityWorker {
    /// Index in the bootstrap roster, including the ordinary builder prefix.
    pub profile_index: usize,
    pub label: String,
    pub worker_id: Uuid,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct JointPlan {
    pub build: BuildPlan,
    pub capability_workers: Vec<PlannedCapabilityWorker>,
    pub deferred_capabilities: Vec<usize>,
    pub bootstrap_complete: bool,
}

pub fn plan_joint_capacity(
    request: &CapacityAssessmentRequest,
    capacity: &CapacityAssessment,
    context: BuildPlanningContext<'_>,
    extra_labels: &[String],
) -> anyhow::Result<JointPlan> {
    let mut accounting = validate_build_planning(request, capacity, &context)?;
    ensure!(
        extra_labels.len()
            == request
                .bootstrap
                .as_ref()
                .map_or(0, |bootstrap| bootstrap.profiles.len()),
        "extra candidate kind labels do not match bootstrap profiles"
    );
    let mut result = JointPlan {
        bootstrap_complete: capacity
            .bootstrap
            .as_ref()
            .is_none_or(|bootstrap| bootstrap.complete),
        ..Default::default()
    };
    let Some(bootstrap) = &capacity.bootstrap else {
        result.build =
            plan_build_capacity_seeded(request, capacity, context, accounting, Vec::new())?;
        return Ok(result);
    };
    let intended_bootstrap = request
        .bootstrap
        .as_ref()
        .expect("validated bootstrap request");
    let starting = bootstrap
        .tracked_workers
        .iter()
        .enumerate()
        .map(|(index, worker)| {
            let intended = if let Some(build_index) = worker.build_tracked_index {
                &request.tracked_profiles[build_index].profile
            } else {
                &intended_bootstrap.tracked_profiles[index - request.tracked_profiles.len()].profile
            };
            // A role-only launch has no saved intent today. Unknown launches earn no credit.
            intended.capabilities.build
                && worker.profile == *intended
                && starting_profile_is_eligible(intended, worker.connected, &context)
        })
        .collect::<Vec<_>>();
    let mut covered = bootstrap
        .demands
        .iter()
        .map(|demand| {
            demand.available
                || demand
                    .tracked_workers
                    .iter()
                    .zip(&starting)
                    .any(|(compatible, eligible)| *compatible && *eligible)
        })
        .collect::<Vec<_>>();
    let earliest_unmet_build =
        if !capacity.server_draining && covered.iter().any(|covered| !covered) {
            let no_growth_policy = BuildScalingPolicy {
                max_new_workers: 0,
                ..context.policy.clone()
            };
            // Native simulation owns a separate ledger and budgets; actual accounting stays untouched.
            let no_growth = plan_build_capacity(
                request,
                capacity,
                BuildPlanningContext {
                    policy: &no_growth_policy,
                    ..context
                },
            )?;
            no_growth
                .deferred_builds
                .first()
                .map(|deferred| &capacity.builds[deferred.build_index])
        } else {
            None
        };
    let label_for = |index: usize, build_index: Option<usize>| {
        if let Some(build_index) = build_index {
            &context.candidate_labels[build_index]
        } else {
            &extra_labels[index - request.profiles.len()]
        }
    };
    let mut seeds = Vec::new();
    if !capacity.server_draining {
        for row in 0..bootstrap.demands.len() {
            if covered[row] {
                continue;
            }
            let candidate = bootstrap
                .profiles
                .iter()
                .enumerate()
                .filter(|(index, profile)| {
                    bootstrap.demands[row].profiles[*index]
                        && accounting.can_launch(
                            label_for(*index, profile.build_profile_index),
                            profile.profile.worker_id,
                            &context,
                        )
                })
                .min_by_key(|(index, profile)| {
                    let preferred = earliest_unmet_build.map_or(
                        profile.build_profile_index.is_none(),
                        |build| {
                            profile.build_profile_index.is_some_and(|build_index| {
                                profile_fits_build(
                                    build,
                                    build_index,
                                    &capacity.profiles[build_index],
                                    context.policy,
                                )
                            })
                        },
                    );
                    let roles = bootstrap
                        .demands
                        .iter()
                        .zip(&covered)
                        .filter(|(demand, covered)| !**covered && demand.profiles[*index])
                        .count();
                    (!preferred, Reverse(roles), *index)
                });
            let Some((index, candidate)) = candidate else {
                // A later throughput proposal may still cover this row; report it only at the end.
                continue;
            };
            let label = label_for(index, candidate.build_profile_index);
            ensure!(
                accounting.reserve(label, candidate.profile.worker_id, &context),
                "selected bootstrap worker exceeds launch limits"
            );
            if let Some(build_index) = candidate.build_profile_index {
                seeds.push(PlannedBuildWorker {
                    profile_index: build_index,
                    label: label.clone(),
                    worker_id: candidate.profile.worker_id,
                });
            } else {
                result.capability_workers.push(PlannedCapabilityWorker {
                    profile_index: index,
                    label: label.clone(),
                    worker_id: candidate.profile.worker_id,
                });
            }
            for (covered, demand) in covered.iter_mut().zip(&bootstrap.demands) {
                *covered |= demand.profiles[index];
            }
        }
    }
    result.build = plan_build_capacity_seeded(request, capacity, context, accounting, seeds)?;
    for (covered, demand) in covered.iter_mut().zip(&bootstrap.demands) {
        *covered |= result
            .build
            .proposed_workers
            .iter()
            .any(|worker| demand.profiles[worker.profile_index]);
    }
    result.deferred_capabilities = covered
        .into_iter()
        .enumerate()
        .filter_map(|(index, covered)| (!covered).then_some(index))
        .collect();
    Ok(result)
}
