use gradient_autoscaler::build_planner::{
    BuildPlanningContext, BuildScalingPolicy, BuildWaitReason, plan_build_capacity,
};
use gradient_autoscaler::capacity::*;
use gradient_autoscaler::planner::Limits;
use gradient_autoscaler::state::{Launch, LaunchState};
use std::collections::HashSet;
use time::Duration;
use uuid::Uuid;

const CAPTURED: &str = "2026-11-11T00:00:00";
const READY: &str = "2026-11-10T23:50:00";

fn profile(name: &str, ram: u64, slots: u32) -> CapacityProfileRequest {
    CapacityProfileRequest {
        profile: BuildCapacityProfile {
            name: name.into(),
            worker_id: Uuid::new_v4(),
            capabilities: CapacityCapabilities {
                build: true,
                ..Default::default()
            },
            resources: BuilderResources {
                architectures: vec!["x86_64-linux".into()],
                system_features: vec![],
                max_concurrent_builds: slots,
                cpu_count: 4,
                ram_total_mb: ram,
                cpu_core_score: 4_000,
                build_cores: None,
            },
        },
        enable_build: true,
        enable_network: false,
    }
}

fn resource(ram: Option<u64>) -> ResourceAssessment {
    ResourceAssessment {
        cores: 1,
        reserved_ram_mb: ram,
        predicted_ram_mb: ram,
        ram_fit: ram.map(|_| RamFit::Fits),
        memory_assumption: if ram.is_some() {
            MemoryAssumption::Predicted
        } else {
            MemoryAssumption::Unmeasured
        },
        own_samples: u32::from(ram.is_some()),
        build_time_known: ram.is_some(),
        time: CapacityTimeEstimate {
            cores: Some(1),
            ..Default::default()
        },
    }
}

struct Case {
    request: CapacityAssessmentRequest,
    capacity: CapacityAssessment,
    labels: Vec<String>,
    open: Vec<Launch>,
    policy: BuildScalingPolicy,
    limits: Limits,
    backoff: HashSet<String>,
}

impl Case {
    fn new(profiles: Vec<CapacityProfileRequest>, jobs: usize, ram: Option<u64>) -> Self {
        let labels = profiles
            .iter()
            .map(|profile| profile.profile.name.clone())
            .collect::<Vec<_>>();
        let capacity = CapacityAssessment {
            captured_at: CAPTURED.into(),
            server_draining: false,
            total_ready_builds: jobs,
            builds_truncated: false,
            workers: vec![],
            profiles: profiles
                .iter()
                .map(|profile| profile.profile.clone())
                .collect(),
            tracked_workers: vec![],
            builds: (0..jobs)
                .map(|_| CapacityBuild {
                    architecture: "x86_64-linux".into(),
                    required_features: vec![],
                    is_fixed_output: false,
                    ready_at: READY.into(),
                    workers: vec![],
                    profiles: profiles
                        .iter()
                        .map(|profile| {
                            Some(ProfileAssessment {
                                resources: resource(ram),
                                prediction_fit: ram.map(|ram| {
                                    if ram <= profile.profile.resources.ram_total_mb {
                                        RamFit::Fits
                                    } else {
                                        RamFit::DoesNotFit
                                    }
                                }),
                            })
                        })
                        .collect(),
                    tracked_workers: vec![],
                })
                .collect(),
        };
        Self {
            request: CapacityAssessmentRequest {
                profiles,
                include_waiting_builds: true,
                ..Default::default()
            },
            capacity,
            labels: labels.clone(),
            open: vec![],
            policy: BuildScalingPolicy {
                queue_wait: Duration::seconds(60),
                startup_grace: Duration::seconds(120),
                max_new_workers: 2,
                fallback_ram_mb: 2_000,
            },
            limits: Limits {
                allowed: labels,
                max_launches: 10,
                max_launches_per_kind: 10,
            },
            backoff: HashSet::new(),
        }
    }

    fn plan(&self) -> gradient_autoscaler::build_planner::BuildPlan {
        plan_build_capacity(
            &self.request,
            &self.capacity,
            BuildPlanningContext {
                connection: "ops",
                open: &self.open,
                candidate_labels: &self.labels,
                limits: &self.limits,
                backoff: &self.backoff,
                policy: &self.policy,
                now: parse_timestamp(CAPTURED).unwrap(),
            },
        )
        .unwrap()
    }

    fn live(&mut self, slots: u32, ram: u64, assessments: Vec<Option<ResourceAssessment>>) {
        self.capacity.workers.push(CapacityWorker {
            free_slots: slots,
            available_ram_mb: Some(ram),
            waiting_build: None,
        });
        for (build, assessment) in self.capacity.builds.iter_mut().zip(assessments) {
            build.workers.push(assessment);
        }
    }

    fn starting(&mut self, connected: bool, live_index: Option<usize>, age: Duration) {
        let profile = profile("starting", 4_000, 2);
        let worker_id = profile.profile.worker_id;
        self.request.tracked_profiles.push(profile.clone());
        self.capacity.tracked_workers.push(TrackedCapacityWorker {
            profile: profile.profile,
            connected,
            worker_index: live_index,
        });
        for build in &mut self.capacity.builds {
            build
                .tracked_workers
                .push((!connected).then(|| ProfileAssessment {
                    resources: resource(Some(1_000)),
                    prediction_fit: Some(RamFit::Fits),
                }));
        }
        let mut launch = Launch::new("ops", "pod", "starting");
        launch.worker_id = Some(worker_id);
        launch.dispatched_at = parse_timestamp(CAPTURED).unwrap() - age;
        self.open.push(launch);
    }
}

#[test]
fn a_versatile_live_worker_is_debited_once_for_slots_and_ram() {
    for (slots, available, expected) in [(1, 10_000, 1), (10, 1_500, 1), (2, 2_000, 0)] {
        let mut case = Case::new(vec![profile("small", 4_000, 2)], 2, Some(1_000));
        case.live(slots, available, vec![Some(resource(Some(1_000))); 2]);
        assert_eq!(case.plan().proposed_workers.len(), expected);
    }
}

#[test]
fn a_basic_build_does_not_consume_the_only_specialist_worker() {
    for reverse in [false, true] {
        let mut case = Case::new(vec![profile("fallback", 4_000, 1)], 2, Some(1_000));
        case.request.profiles[0]
            .profile
            .resources
            .system_features
            .push("kvm".into());
        case.capacity.profiles[0]
            .resources
            .system_features
            .push("kvm".into());
        case.capacity.builds[1].required_features.push("kvm".into());
        case.live(1, 1_000, vec![Some(resource(Some(1_000))); 2]);
        case.live(1, 1_000, vec![Some(resource(Some(1_000))), None]);
        if reverse {
            case.capacity.workers.reverse();
            for build in &mut case.capacity.builds {
                build.workers.reverse();
            }
        }
        assert!(case.plan().proposed_workers.is_empty());
    }
}

#[test]
fn starting_capacity_preserves_the_only_specialist_budget() {
    let mut case = Case::new(vec![profile("fallback", 4_000, 1)], 2, Some(1_000));
    case.starting(false, None, Duration::seconds(30));
    case.starting(false, None, Duration::seconds(30));
    case.request.tracked_profiles[1].profile.name = "basic".into();
    case.capacity.tracked_workers[1].profile.name = "basic".into();
    for profile in &mut case.request.tracked_profiles {
        profile.profile.resources.max_concurrent_builds = 1;
    }
    for worker in &mut case.capacity.tracked_workers {
        worker.profile.resources.max_concurrent_builds = 1;
    }
    case.capacity.builds[1].tracked_workers[1] = None;
    let plan = case.plan();
    assert!(plan.proposed_workers.is_empty());
    assert_eq!(plan.credited_starting_workers.len(), 2);
}

#[test]
fn proposed_capacity_preserves_the_only_specialist_budget() {
    let mut case = Case::new(
        vec![profile("versatile", 4_000, 2), profile("basic", 4_000, 2)],
        4,
        Some(1_000),
    );
    case.capacity.builds[0].profiles[1] = None;
    case.capacity.builds[1].profiles[0] = None;
    case.capacity.builds[3].profiles[1] = None;
    let plan = case.plan();
    assert_eq!(plan.proposed_workers.len(), 2);
    assert!(plan.deferred_builds.is_empty());
}

#[test]
fn a_selected_profile_adds_one_budget_and_retains_the_assessed_uuid() {
    let case = Case::new(vec![profile("small", 4_000, 2)], 3, Some(1_000));
    let plan = case.plan();
    assert_eq!(plan.proposed_workers.len(), 1);
    assert_eq!(
        plan.proposed_workers[0].worker_id,
        case.request.profiles[0].profile.worker_id
    );
    assert_eq!(plan.deferred_builds.len(), 1);
}

#[test]
fn sizing_uses_original_prediction_not_proportional_reservation_fit() {
    let mut case = Case::new(
        vec![profile("small", 2_000, 2), profile("large", 8_000, 2)],
        1,
        Some(6_000),
    );
    let small = case.capacity.builds[0].profiles[0].as_mut().unwrap();
    small.resources.reserved_ram_mb = Some(1_500);
    small.resources.ram_fit = Some(RamFit::Fits);
    small.resources.memory_assumption = MemoryAssumption::Proportional;
    let plan = case.plan();
    assert_eq!(plan.proposed_workers[0].label, "large");
}

#[test]
fn missing_history_uses_explicit_fallback_without_becoming_measured_zero() {
    let case = Case::new(vec![profile("small", 3_000, 4)], 2, None);
    let plan = case.plan();
    assert_eq!(plan.proposed_workers.len(), 1);
    assert_eq!(plan.deferred_builds.len(), 1);
    assert_eq!(
        case.capacity.builds[0].profiles[0]
            .as_ref()
            .unwrap()
            .resources
            .predicted_ram_mb,
        None
    );
}

#[test]
fn waiting_build_credit_belongs_to_its_one_scoped_build() {
    let mut case = Case::new(vec![profile("small", 4_000, 2)], 2, Some(1_000));
    case.live(2, 0, vec![Some(resource(Some(1_000))); 2]);
    case.capacity.workers[0].waiting_build = Some(CapacityWaitingBuild {
        build_index: 0,
        ram_credit_mb: 1_000,
    });
    assert_eq!(case.plan().proposed_workers.len(), 1);
}

#[test]
fn starting_capacity_is_credited_once_only_before_connection_and_expiry() {
    for (connected, age, expected) in [
        (false, 30, 0),
        (false, 120, 1),
        (false, -1, 1),
        (true, 30, 1),
    ] {
        let mut case = Case::new(vec![profile("small", 4_000, 2)], 2, Some(1_000));
        case.starting(connected, None, Duration::seconds(age));
        let plan = case.plan();
        assert_eq!(plan.proposed_workers.len(), expected);
        assert_eq!(
            plan.credited_starting_workers.len(),
            usize::from(expected == 0)
        );
    }
}

#[test]
fn a_newly_connected_worker_is_not_both_live_and_starting_capacity() {
    let mut case = Case::new(vec![profile("small", 4_000, 2)], 2, Some(1_000));
    case.live(1, 4_000, vec![Some(resource(Some(1_000))); 2]);
    case.starting(true, Some(0), Duration::seconds(30));
    let plan = case.plan();
    assert_eq!(plan.proposed_workers.len(), 1);
    assert!(plan.credited_starting_workers.is_empty());
}

#[test]
fn historical_connected_state_is_not_a_fresh_startup_lease() {
    let mut case = Case::new(vec![profile("small", 4_000, 2)], 1, Some(1_000));
    case.starting(false, None, Duration::seconds(10));
    case.open[0].state = LaunchState::Connected;
    assert_eq!(case.plan().proposed_workers.len(), 1);
}

#[test]
fn a_tracked_profile_without_a_matching_launch_earns_no_startup_credit() {
    let mut case = Case::new(vec![profile("small", 4_000, 2)], 1, Some(1_000));
    case.starting(false, None, Duration::seconds(10));
    case.open[0].worker_id = None;
    assert_eq!(case.plan().proposed_workers.len(), 1);
    case.open.clear();
    assert_eq!(case.plan().proposed_workers.len(), 1);
}

#[test]
fn busy_capable_workers_get_queue_grace_but_missing_capacity_bootstraps_immediately() {
    let mut case = Case::new(vec![profile("small", 4_000, 2)], 1, Some(1_000));
    case.capacity.builds[0].ready_at = CAPTURED.into();
    assert_eq!(case.plan().proposed_workers.len(), 1);
    case.live(0, 4_000, vec![Some(resource(Some(1_000)))]);
    let plan = case.plan();
    assert!(plan.proposed_workers.is_empty());
    assert_eq!(plan.deferred_builds[0].reason, BuildWaitReason::QueueGrace);
}

#[test]
fn limits_backoff_and_server_drain_do_not_create_extra_workers() {
    let mut case = Case::new(vec![profile("small", 4_000, 1)], 1, Some(1_000));
    case.policy.max_new_workers = 0;
    assert_eq!(
        case.plan().deferred_builds[0].reason,
        BuildWaitReason::LaunchLimit
    );
    case.policy.max_new_workers = 2;
    case.limits.max_launches = 0;
    assert!(case.plan().proposed_workers.is_empty());
    case.limits.max_launches = 10;
    case.backoff.insert("small".into());
    assert_eq!(
        case.plan().deferred_builds[0].reason,
        BuildWaitReason::NoMatchingProfile
    );
    case.capacity.server_draining = true;
    let plan = case.plan();
    assert!(plan.deferred_builds.is_empty());
    assert!(plan.server_draining);
}

#[test]
fn open_launches_occupy_global_and_kind_limits_even_after_startup_expiry() {
    for global in [false, true] {
        let mut case = Case::new(vec![profile("small", 4_000, 1)], 1, Some(1_000));
        case.starting(false, None, Duration::seconds(120));
        if global {
            case.limits.max_launches = 1;
        } else {
            case.open[0].label = "small".into();
            case.limits.max_launches_per_kind = 1;
        }
        let plan = case.plan();
        assert!(plan.proposed_workers.is_empty());
        assert_eq!(plan.deferred_builds[0].reason, BuildWaitReason::LaunchLimit);
    }
}

#[test]
fn disallowed_profiles_do_not_launch_and_truncation_is_preserved() {
    let mut case = Case::new(vec![profile("small", 4_000, 1)], 1, Some(1_000));
    case.limits.allowed.clear();
    case.capacity.total_ready_builds = 100;
    case.capacity.builds_truncated = true;
    let plan = case.plan();
    assert!(plan.proposed_workers.is_empty());
    assert_eq!(
        plan.deferred_builds[0].reason,
        BuildWaitReason::NoMatchingProfile
    );
    assert!(plan.builds_truncated);
}

#[test]
fn negligible_memory_uses_the_operator_fallback_instead_of_zero() {
    let mut case = Case::new(vec![profile("small", 3_000, 2)], 2, Some(0));
    for build in &mut case.capacity.builds {
        build.profiles[0]
            .as_mut()
            .unwrap()
            .resources
            .memory_assumption = MemoryAssumption::Negligible;
    }
    let plan = case.plan();
    assert_eq!(plan.proposed_workers.len(), 1);
    assert_eq!(plan.deferred_builds.len(), 1);
}

#[test]
fn alternative_profiles_for_one_uuid_cannot_launch_two_workers() {
    let first = profile("first", 4_000, 1);
    let mut second = profile("second", 8_000, 1);
    second.profile.worker_id = first.profile.worker_id;
    let case = Case::new(vec![first, second], 2, Some(1_000));
    assert_eq!(case.plan().proposed_workers.len(), 1);
}
