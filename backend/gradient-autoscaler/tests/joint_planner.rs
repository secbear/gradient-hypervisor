use gradient_autoscaler::build_planner::{
    BuildPlan, BuildPlanningContext, BuildScalingPolicy, BuildWaitReason, plan_build_capacity,
};
use gradient_autoscaler::capacity::*;
use gradient_autoscaler::joint_planner::{JointPlan, plan_joint_capacity};
use gradient_autoscaler::planner::Limits;
use gradient_autoscaler::state::{BuildLaunchIntent, Launch, LaunchState};
use std::collections::HashSet;
use time::Duration;
use uuid::Uuid;

const CAPTURED: &str = "2026-11-11T00:00:00";
const READY: &str = "2026-11-10T23:50:00";

fn registration() -> RegistrationCapabilities {
    RegistrationCapabilities {
        enable_build: true,
        enable_network: true,
        enable_eval: true,
        enable_secrets: true,
        enable_exec: true,
    }
}

fn builder(name: &str, ram: u64, slots: u32, roles: bool) -> CapacityProfileRequest {
    CapacityProfileRequest {
        profile: CapabilityProfile {
            name: name.into(),
            worker_id: Uuid::new_v4(),
            capabilities: CapacityCapabilities {
                build: true,
                eval: roles,
                network: roles,
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
        enable_network: true,
        registration: Some(registration()),
    }
}

fn role_only(name: &str, eval: bool, network: bool) -> BootstrapProfileRequest {
    BootstrapProfileRequest {
        profile: CapabilityProfile {
            name: name.into(),
            worker_id: Uuid::new_v4(),
            capabilities: CapacityCapabilities {
                eval,
                network,
                ..Default::default()
            },
            resources: BuilderResources {
                architectures: vec!["x86_64-linux".into()],
                system_features: vec![],
                max_concurrent_builds: u32::from(network),
                cpu_count: 0,
                ram_total_mb: 0,
                cpu_core_score: 0,
                build_cores: None,
            },
        },
        registration: registration(),
    }
}

fn resource(ram: u64) -> ResourceAssessment {
    ResourceAssessment {
        cores: 1,
        reserved_ram_mb: Some(ram),
        predicted_ram_mb: Some(ram),
        ram_fit: Some(RamFit::Fits),
        memory_assumption: MemoryAssumption::Predicted,
        own_samples: 1,
        build_time_known: true,
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
    extra_labels: Vec<String>,
    open: Vec<Launch>,
    policy: BuildScalingPolicy,
    limits: Limits,
    backoff: HashSet<String>,
}

impl Case {
    fn new(
        profiles: Vec<CapacityProfileRequest>,
        extras: Vec<BootstrapProfileRequest>,
        jobs: usize,
        ram: u64,
    ) -> Self {
        let labels = profiles
            .iter()
            .map(|profile| profile.profile.name.clone())
            .collect::<Vec<_>>();
        let extra_labels = extras
            .iter()
            .map(|profile| profile.profile.name.clone())
            .collect::<Vec<_>>();
        let general_profiles = profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| BootstrapCapacityProfile {
                profile: profile.profile.clone(),
                build_profile_index: Some(index),
            })
            .chain(extras.iter().map(|profile| BootstrapCapacityProfile {
                profile: profile.profile.clone(),
                build_profile_index: None,
            }))
            .collect();
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
            build_page: None,
            bootstrap: Some(BootstrapCapacityAssessment {
                observed_at: CAPTURED.into(),
                complete: true,
                demands: vec![],
                profiles: general_profiles,
                tracked_workers: vec![],
            }),
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
                                prediction_fit: Some(
                                    if ram <= profile.profile.resources.ram_total_mb {
                                        RamFit::Fits
                                    } else {
                                        RamFit::DoesNotFit
                                    },
                                ),
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
                bootstrap: Some(BootstrapCapacityRequest {
                    profiles: extras,
                    tracked_profiles: vec![],
                }),
                ..Default::default()
            },
            capacity,
            limits: Limits {
                allowed: labels.iter().chain(&extra_labels).cloned().collect(),
                max_launches: 10,
                max_launches_per_kind: 10,
            },
            labels,
            extra_labels,
            open: vec![],
            policy: BuildScalingPolicy {
                queue_wait: Duration::seconds(60),
                startup_grace: Duration::seconds(120),
                max_new_workers: 2,
                fallback_ram_mb: 2_000,
            },
            backoff: HashSet::new(),
        }
    }

    fn context(&self) -> BuildPlanningContext<'_> {
        BuildPlanningContext {
            connection: "ops",
            open: &self.open,
            candidate_labels: &self.labels,
            limits: &self.limits,
            backoff: &self.backoff,
            policy: &self.policy,
            now: parse_timestamp(CAPTURED).unwrap(),
        }
    }

    fn plan(&self) -> JointPlan {
        plan_joint_capacity(
            &self.request,
            &self.capacity,
            self.context(),
            &self.extra_labels,
        )
        .unwrap()
    }

    fn ordinary(&self) -> BuildPlan {
        plan_build_capacity(&self.request, &self.capacity, self.context()).unwrap()
    }

    fn bootstrap(&mut self) -> &mut BootstrapCapacityAssessment {
        self.capacity.bootstrap.as_mut().unwrap()
    }

    fn demand(&mut self, kind: BootstrapDemandKind, profiles: Vec<bool>) {
        let tracked = self.bootstrap().tracked_workers.len();
        self.bootstrap().demands.push(BootstrapCapacityDemand {
            kind,
            available: false,
            profiles,
            tracked_workers: vec![false; tracked],
        });
    }

    fn starting(&mut self, connected: bool, age: i64) {
        let profile = builder("starting", 8_000, 2, true);
        let saved_profile = profile.profile.clone();
        let index = self.request.tracked_profiles.len();
        self.request.tracked_profiles.push(profile.clone());
        self.capacity.tracked_workers.push(TrackedCapacityWorker {
            profile: profile.profile.clone(),
            connected,
            worker_index: None,
        });
        self.bootstrap()
            .tracked_workers
            .push(BootstrapTrackedWorker {
                profile: profile.profile,
                connected,
                build_tracked_index: Some(index),
            });
        for build in &mut self.capacity.builds {
            build
                .tracked_workers
                .push((!connected).then(|| ProfileAssessment {
                    resources: resource(1_000),
                    prediction_fit: Some(RamFit::Fits),
                }));
        }
        for demand in &mut self.bootstrap().demands {
            demand.tracked_workers.push(true);
        }
        let mut launch = Launch::new("ops", "pod", "starting");
        launch.worker_id = Some(saved_profile.worker_id);
        launch.dispatched_at = parse_timestamp(CAPTURED).unwrap() - Duration::seconds(age);
        launch.build_intent = Some(BuildLaunchIntent {
            profile: saved_profile,
            startup_deadline: launch.dispatched_at + self.policy.startup_grace,
            kubernetes: None,
        });
        launch.registration_started = true;
        self.open.push(launch);
    }
}

#[test]
fn a_versatile_builder_beats_smaller_and_role_only_candidates_with_one_launch() {
    for small in [false, true] {
        let builders = if small {
            vec![
                builder("small", 2_000, 2, true),
                builder("large", 16_000, 2, true),
            ]
        } else {
            vec![builder("large", 16_000, 2, true)]
        };
        let mut case = Case::new(builders, vec![role_only("roles", true, true)], 2, 8_000);
        case.policy.max_new_workers = 1;
        // The role-only candidate covers more rows but cannot run the oldest heavy build.
        let mut evaluation = vec![true; case.labels.len()];
        evaluation.push(true);
        case.demand(BootstrapDemandKind::Evaluation, evaluation.clone());
        case.demand(BootstrapDemandKind::InputFetch, evaluation);
        let mut download = vec![false; case.labels.len()];
        download.push(true);
        case.demand(BootstrapDemandKind::Download, download);
        let plan = case.plan();
        assert_eq!(plan.build.proposed_workers.len(), 1);
        assert_eq!(plan.build.proposed_workers[0].label, "large");
        assert_eq!(
            plan.build.proposed_workers[0].worker_id,
            case.request.profiles[case.labels.len() - 1]
                .profile
                .worker_id
        );
        assert!(plan.build.deferred_builds.is_empty());
        assert!(plan.capability_workers.is_empty());
        assert_eq!(plan.deferred_capabilities, [2]);
    }
}

#[test]
fn an_all_ram_unfit_build_sample_without_other_role_demands_never_seeds_workers() {
    let mut case = Case::new(
        vec![
            builder("tiny", 1_000, 2, true),
            builder("small", 2_000, 2, true),
        ],
        vec![role_only("eval", true, false)],
        MAX_BUILDS,
        8_000,
    );
    case.capacity.total_ready_builds = MAX_BUILDS + 1;
    case.capacity.builds_truncated = true;
    for build in &mut case.capacity.builds {
        for assessment in build.profiles.iter_mut().flatten() {
            assessment.resources.ram_fit = Some(RamFit::DoesNotFit);
        }
    }
    let plan = case.plan();
    assert!(plan.build.proposed_workers.is_empty());
    assert!(plan.capability_workers.is_empty());
    assert!(plan.deferred_capabilities.is_empty());
    assert_eq!(plan.build.deferred_builds.len(), MAX_BUILDS);
    assert!(
        plan.build
            .deferred_builds
            .iter()
            .all(|build| build.reason == BuildWaitReason::NoMatchingProfile)
    );
    assert!(plan.build.builds_truncated);
    assert_eq!(plan.build, case.ordinary());
}

#[test]
fn fully_covered_heavy_builds_do_not_favor_builders_for_a_pure_eval_gap() {
    for starting in [false, true] {
        let mut case = Case::new(
            vec![builder("large", 16_000, 2, true)],
            vec![role_only("eval", true, false)],
            1,
            8_000,
        );
        case.policy.max_new_workers = 1;
        case.demand(BootstrapDemandKind::Evaluation, vec![true, true]);
        if starting {
            case.starting(false, 30);
            case.capacity.builds[0].tracked_workers[0]
                .as_mut()
                .unwrap()
                .resources = resource(8_000);
            case.bootstrap().demands[0].tracked_workers[0] = false;
        } else {
            case.capacity.workers.push(CapacityWorker {
                free_slots: 1,
                available_ram_mb: Some(8_000),
                waiting_build: None,
            });
            case.capacity.builds[0].workers.push(Some(resource(8_000)));
        }
        let before = case.open.clone();
        let plan = case.plan();
        assert_eq!(plan.capability_workers.len(), 1);
        assert_eq!(plan.capability_workers[0].label, "eval");
        assert!(plan.build.proposed_workers.is_empty());
        assert!(plan.build.deferred_builds.is_empty());
        assert!(plan.deferred_capabilities.is_empty());
        assert_eq!(
            plan.build.credited_starting_workers.len(),
            usize::from(starting)
        );
        assert_eq!(case.open, before);
    }
}

#[test]
fn without_ready_builds_role_only_profiles_win_but_builders_remain_eligible() {
    for blocked in [false, true] {
        let mut case = Case::new(
            vec![builder("large", 16_000, 2, true)],
            vec![role_only("eval", true, false)],
            0,
            8_000,
        );
        case.policy.max_new_workers = 1;
        case.demand(BootstrapDemandKind::Evaluation, vec![true, true]);
        if blocked {
            case.backoff.insert("eval".into());
        }
        let plan = case.plan();
        if blocked {
            assert!(plan.capability_workers.is_empty());
            assert_eq!(plan.build.proposed_workers.len(), 1);
            assert_eq!(plan.build.proposed_workers[0].label, "large");
        } else {
            assert_eq!(plan.capability_workers.len(), 1);
            assert_eq!(plan.capability_workers[0].label, "eval");
            assert!(plan.build.proposed_workers.is_empty());
        }
        assert!(plan.build.deferred_builds.is_empty());
        assert!(plan.deferred_capabilities.is_empty());
    }
}

#[test]
fn seeded_builders_share_the_native_ram_and_slot_budget_without_role_reservations() {
    for (slots, ram, deferred) in [(2, 2_000, 0), (1, 2_000, 1), (2, 1_500, 1)] {
        let mut case = Case::new(vec![builder("joint", ram, slots, true)], vec![], 2, 1_000);
        case.policy.max_new_workers = 1;
        case.demand(BootstrapDemandKind::Evaluation, vec![true]);
        case.demand(BootstrapDemandKind::InputFetch, vec![true]);
        let plan = case.plan();
        assert_eq!(plan.build.proposed_workers.len(), 1);
        assert_eq!(plan.build.deferred_builds.len(), deferred);
        assert!(plan.deferred_capabilities.is_empty());
        assert!(plan.capability_workers.is_empty());
    }
}

#[test]
fn essential_roles_cannot_be_starved_by_a_heavy_build_backlog() {
    let mut case = Case::new(
        vec![builder("throughput", 8_000, 1, false)],
        vec![role_only("eval", true, false)],
        6,
        8_000,
    );
    case.policy.max_new_workers = 1;
    case.demand(BootstrapDemandKind::Evaluation, vec![false, true]);
    let plan = case.plan();
    assert_eq!(plan.capability_workers.len(), 1);
    assert_eq!(plan.capability_workers[0].profile_index, 1);
    assert_eq!(plan.capability_workers[0].label, "eval");
    assert!(plan.build.proposed_workers.is_empty());
    assert_eq!(plan.build.deferred_builds.len(), 6);
    assert!(plan.deferred_capabilities.is_empty());
}

#[test]
fn standalone_eval_and_network_workers_never_lend_build_capacity() {
    for (eval, network, kind) in [
        (true, false, BootstrapDemandKind::Evaluation),
        (false, true, BootstrapDemandKind::InputFetch),
    ] {
        let mut case = Case::new(
            vec![builder("builder", 4_000, 2, false)],
            vec![role_only("role", eval, network)],
            2,
            1_000,
        );
        case.demand(kind, vec![false, true]);
        let plan = case.plan();
        assert_eq!(plan.capability_workers.len(), 1);
        assert_eq!(plan.build.proposed_workers.len(), 1);
        assert_eq!(plan.build.proposed_workers[0].profile_index, 0);
        assert_ne!(
            plan.capability_workers[0].worker_id,
            plan.build.proposed_workers[0].worker_id
        );
        assert!(plan.build.deferred_builds.is_empty());
    }
}

#[test]
fn live_role_availability_is_independent_of_busy_build_resources_and_queue_grace() {
    let mut case = Case::new(vec![builder("joint", 4_000, 2, true)], vec![], 1, 1_000);
    case.demand(BootstrapDemandKind::Evaluation, vec![true]);
    case.bootstrap().demands[0].available = true;
    case.capacity.builds[0].ready_at = CAPTURED.into();
    case.capacity.workers.push(CapacityWorker {
        free_slots: 0,
        available_ram_mb: Some(0),
        waiting_build: None,
    });
    case.capacity.builds[0].workers.push(Some(resource(1_000)));
    let plan = case.plan();
    assert!(plan.build.proposed_workers.is_empty());
    assert!(plan.deferred_capabilities.is_empty());
    assert_eq!(
        plan.build.deferred_builds[0].reason,
        BuildWaitReason::QueueGrace
    );
}

#[test]
fn selected_builders_still_prefer_live_then_starting_capacity() {
    let mut case = Case::new(vec![builder("seed", 1_000, 1, true)], vec![], 3, 1_000);
    case.demand(BootstrapDemandKind::Evaluation, vec![true]);
    case.starting(false, 30);
    case.bootstrap().demands[0].tracked_workers[0] = false;
    case.capacity.workers.push(CapacityWorker {
        free_slots: 1,
        available_ram_mb: Some(1_000),
        waiting_build: None,
    });
    for (index, build) in case.capacity.builds.iter_mut().enumerate() {
        build.workers.push((index == 0).then(|| resource(1_000)));
    }
    case.capacity.tracked_workers[0]
        .profile
        .resources
        .max_concurrent_builds = 1;
    case.request.tracked_profiles[0]
        .profile
        .resources
        .max_concurrent_builds = 1;
    case.bootstrap().tracked_workers[0]
        .profile
        .resources
        .max_concurrent_builds = 1;
    case.open[0]
        .build_intent
        .as_mut()
        .unwrap()
        .profile
        .resources
        .max_concurrent_builds = 1;
    case.capacity.builds[2].tracked_workers[0] = None;
    let plan = case.plan();
    assert_eq!(plan.build.proposed_workers.len(), 1);
    assert_eq!(
        plan.build.credited_starting_workers,
        [case.open[0].worker_id.unwrap()]
    );
    assert!(plan.build.deferred_builds.is_empty());
}

#[test]
fn valid_starting_builders_cover_roles_and_resources_without_new_launch_debits() {
    let mut case = Case::new(vec![builder("joint", 4_000, 2, true)], vec![], 2, 1_000);
    case.demand(BootstrapDemandKind::Evaluation, vec![true]);
    case.demand(BootstrapDemandKind::InputFetch, vec![true]);
    case.starting(false, 30);
    case.policy.max_new_workers = 0;
    case.limits.max_launches = 1;
    let plan = case.plan();
    assert!(plan.build.proposed_workers.is_empty());
    assert!(plan.capability_workers.is_empty());
    assert!(plan.build.deferred_builds.is_empty());
    assert!(plan.deferred_capabilities.is_empty());
    assert_eq!(
        plan.build.credited_starting_workers,
        [case.open[0].worker_id.unwrap()]
    );
}

#[test]
fn stale_connected_cleaning_unregistered_and_unknown_workers_earn_no_credit() {
    for invalid in [
        "expired",
        "future",
        "connected",
        "historically-connected",
        "cleanup",
        "unregistered",
        "unknown",
        "mismatched",
        "missing-launch",
    ] {
        let mut case = Case::new(vec![builder("joint", 4_000, 2, true)], vec![], 1, 1_000);
        case.demand(BootstrapDemandKind::Evaluation, vec![true]);
        case.starting(
            invalid == "connected",
            if invalid == "expired" { 120 } else { 30 },
        );
        match invalid {
            "future" => {
                case.open[0].dispatched_at =
                    parse_timestamp(CAPTURED).unwrap() + Duration::seconds(1)
            }
            "historically-connected" => case.open[0].state = LaunchState::Connected,
            "cleanup" => case.open[0].cleanup_outcome = Some(LaunchState::Failed),
            "unregistered" => case.open[0].registration_started = false,
            "unknown" => case.open[0].build_intent = None,
            "mismatched" => {
                case.open[0]
                    .build_intent
                    .as_mut()
                    .unwrap()
                    .profile
                    .resources
                    .ram_total_mb += 1
            }
            "missing-launch" => case.open.clear(),
            _ => {}
        }
        case.policy.max_new_workers = 0;
        let plan = case.plan();
        assert!(plan.build.credited_starting_workers.is_empty(), "{invalid}");
        assert_eq!(plan.deferred_capabilities, [0], "{invalid}");
        assert_eq!(plan.build.deferred_builds.len(), 1, "{invalid}");
    }
}

#[test]
fn starting_credit_uses_the_frozen_deadline_and_full_general_profile() {
    let mut case = Case::new(vec![builder("joint", 4_000, 2, true)], vec![], 1, 1_000);
    case.demand(BootstrapDemandKind::Evaluation, vec![true]);
    case.starting(false, 120);
    case.policy.max_new_workers = 0;
    case.policy.startup_grace = Duration::hours(1);
    assert_eq!(case.plan().deferred_capabilities, [0]);
    case.open[0].build_intent.as_mut().unwrap().startup_deadline += Duration::seconds(1);
    assert!(case.plan().deferred_capabilities.is_empty());
    // The general roster can report narrowed grants, but it is no longer the saved snapshot.
    case.bootstrap().tracked_workers[0]
        .profile
        .capabilities
        .network = false;
    let plan = case.plan();
    assert_eq!(plan.deferred_capabilities, [0]);
    assert_eq!(plan.build.credited_starting_workers.len(), 1);
}

#[test]
fn connected_workers_are_not_starting_even_when_the_role_matrix_matches() {
    let mut case = Case::new(vec![builder("joint", 4_000, 2, true)], vec![], 2, 1_000);
    case.demand(BootstrapDemandKind::Evaluation, vec![true]);
    case.starting(true, 30);
    case.capacity.workers.push(CapacityWorker {
        free_slots: 1,
        available_ram_mb: Some(4_000),
        waiting_build: None,
    });
    case.capacity.tracked_workers[0].worker_index = Some(0);
    for build in &mut case.capacity.builds {
        build.workers.push(Some(resource(1_000)));
    }
    case.policy.max_new_workers = 0;
    let plan = case.plan();
    assert!(plan.build.credited_starting_workers.is_empty());
    assert_eq!(plan.build.deferred_builds.len(), 1);
    assert_eq!(plan.deferred_capabilities, [0]);
    case.bootstrap().demands[0].available = true;
    assert!(case.plan().deferred_capabilities.is_empty());
}

#[test]
fn an_unmapped_role_only_launch_has_no_generic_starting_intent() {
    let tracked = role_only("existing-eval", true, false);
    let mut case = Case::new(vec![], vec![role_only("eval", true, false)], 0, 1_000);
    case.demand(BootstrapDemandKind::Evaluation, vec![true]);
    case.request
        .bootstrap
        .as_mut()
        .unwrap()
        .tracked_profiles
        .push(tracked.clone());
    case.bootstrap()
        .tracked_workers
        .push(BootstrapTrackedWorker {
            profile: tracked.profile.clone(),
            connected: false,
            build_tracked_index: None,
        });
    case.bootstrap().demands[0].tracked_workers.push(true);
    let mut launch = Launch::new("ops", "existing-pod", "eval");
    launch.worker_id = Some(tracked.profile.worker_id);
    launch.dispatched_at = parse_timestamp(CAPTURED).unwrap() - Duration::seconds(30);
    launch.registration_started = true;
    case.open.push(launch);
    case.policy.max_new_workers = 0;
    let plan = case.plan();
    assert_eq!(plan.deferred_capabilities, [0]);
    assert!(plan.capability_workers.is_empty());
}

#[test]
fn an_unmapped_saved_builder_can_cover_roles_but_not_unassessed_build_resources() {
    let tracked = builder("tracked-general", 8_000, 2, true);
    let mut case = Case::new(vec![builder("builder", 4_000, 2, false)], vec![], 1, 1_000);
    case.demand(BootstrapDemandKind::Evaluation, vec![false]);
    case.request
        .bootstrap
        .as_mut()
        .unwrap()
        .tracked_profiles
        .push(BootstrapProfileRequest {
            profile: tracked.profile.clone(),
            registration: registration(),
        });
    case.bootstrap()
        .tracked_workers
        .push(BootstrapTrackedWorker {
            profile: tracked.profile.clone(),
            connected: false,
            build_tracked_index: None,
        });
    case.bootstrap().demands[0].tracked_workers.push(true);
    let mut launch = Launch::new("ops", "pod", "tracked-general");
    launch.worker_id = Some(tracked.profile.worker_id);
    launch.dispatched_at = parse_timestamp(CAPTURED).unwrap() - Duration::seconds(30);
    launch.registration_started = true;
    launch.build_intent = Some(BuildLaunchIntent {
        profile: tracked.profile,
        startup_deadline: launch.dispatched_at + Duration::seconds(120),
        kubernetes: None,
    });
    case.open.push(launch);
    case.policy.max_new_workers = 0;
    let plan = case.plan();
    assert!(plan.deferred_capabilities.is_empty());
    assert!(plan.build.credited_starting_workers.is_empty());
    assert_eq!(plan.build.deferred_builds.len(), 1);
}

#[test]
fn launch_caps_are_shared_by_role_seeds_builder_seeds_and_throughput() {
    for cap in ["cycle", "global", "label"] {
        let mut case = Case::new(
            vec![builder("builder", 4_000, 1, false)],
            vec![role_only("eval", true, false)],
            1,
            1_000,
        );
        case.demand(BootstrapDemandKind::Evaluation, vec![false, true]);
        match cap {
            "cycle" => case.policy.max_new_workers = 1,
            "global" => case.limits.max_launches = 1,
            "label" => {
                case.extra_labels[0] = "builder".into();
                case.limits.max_launches_per_kind = 1;
            }
            _ => unreachable!(),
        }
        let plan = case.plan();
        assert_eq!(plan.capability_workers.len(), 1, "{cap}");
        assert!(plan.build.proposed_workers.is_empty(), "{cap}");
        assert_eq!(
            plan.build.deferred_builds[0].reason,
            BuildWaitReason::LaunchLimit
        );
    }
    let mut case = Case::new(
        vec![
            builder("seed", 4_000, 1, true),
            builder("more", 4_000, 1, false),
        ],
        vec![],
        2,
        1_000,
    );
    case.demand(BootstrapDemandKind::Evaluation, vec![true, false]);
    case.policy.max_new_workers = 1;
    let plan = case.plan();
    assert_eq!(plan.build.proposed_workers.len(), 1);
    assert_eq!(plan.build.deferred_builds.len(), 1);
    case.policy.max_new_workers = 2;
    let plan = case.plan();
    assert_eq!(plan.build.proposed_workers.len(), 2);
    assert!(plan.build.deferred_builds.is_empty());
}

#[test]
fn existing_open_launches_use_caps_and_worker_ids_even_without_startup_credit() {
    for restriction in ["global", "label", "id"] {
        let mut case = Case::new(vec![], vec![role_only("eval", true, false)], 0, 1_000);
        case.demand(BootstrapDemandKind::Evaluation, vec![true]);
        let mut launch = Launch::new("ops", "pod", "eval");
        launch.worker_id = Some(
            case.request.bootstrap.as_ref().unwrap().profiles[0]
                .profile
                .worker_id,
        );
        match restriction {
            "global" => {
                launch.label = "other".into();
                launch.worker_id = None;
                case.limits.max_launches = 1;
            }
            "label" => {
                launch.worker_id = None;
                case.limits.max_launches_per_kind = 1;
            }
            _ => {}
        }
        case.open.push(launch);
        let plan = case.plan();
        assert!(plan.capability_workers.is_empty(), "{restriction}");
        assert_eq!(plan.deferred_capabilities, [0], "{restriction}");
    }
}

#[test]
fn duplicate_builder_id_in_a_response_is_rejected() {
    let mut case = Case::new(
        vec![
            builder("first", 4_000, 1, true),
            builder("second", 4_000, 1, true),
        ],
        vec![],
        2,
        1_000,
    );
    case.demand(BootstrapDemandKind::Evaluation, vec![true, true]);
    let duplicate = case.capacity.profiles[0].worker_id;
    case.capacity.profiles[1].worker_id = duplicate;
    case.bootstrap().profiles[1].profile.worker_id = duplicate;
    assert!(case.request.validate().is_ok());
    assert!(case.capacity.validate_against(&case.request).is_err());
    assert!(
        plan_joint_capacity(
            &case.request,
            &case.capacity,
            case.context(),
            &case.extra_labels,
        )
        .is_err()
    );
}

#[test]
fn duplicate_extra_ids_in_a_request_or_response_are_rejected() {
    for duplicate_request in [false, true] {
        let mut case = Case::new(
            vec![],
            vec![
                role_only("first", true, false),
                role_only("second", false, true),
            ],
            0,
            1_000,
        );
        case.demand(BootstrapDemandKind::Evaluation, vec![true, false]);
        case.demand(BootstrapDemandKind::InputFetch, vec![false, true]);
        let duplicate = case.bootstrap().profiles[0].profile.worker_id;
        case.bootstrap().profiles[1].profile.worker_id = duplicate;
        if duplicate_request {
            case.request.bootstrap.as_mut().unwrap().profiles[1]
                .profile
                .worker_id = duplicate;
        }
        assert_eq!(case.request.validate().is_err(), duplicate_request);
        assert!(case.capacity.validate_against(&case.request).is_err());
        assert!(
            plan_joint_capacity(
                &case.request,
                &case.capacity,
                case.context(),
                &case.extra_labels,
            )
            .is_err()
        );
    }
}

#[test]
fn backoff_allowed_labels_and_false_policy_rows_are_respected_without_retry_loops() {
    for blocked in ["backoff", "allowed", "policy"] {
        let mut case = Case::new(
            vec![builder("builder", 4_000, 2, false)],
            vec![role_only("roles", true, true)],
            1,
            1_000,
        );
        case.demand(BootstrapDemandKind::Evaluation, vec![false, true]);
        case.demand(BootstrapDemandKind::InputFetch, vec![false, true]);
        match blocked {
            "backoff" => {
                case.backoff.insert("roles".into());
            }
            "allowed" => case.limits.allowed.retain(|label| label != "roles"),
            "policy" => {
                for demand in &mut case.bootstrap().demands {
                    demand.profiles[1] = false;
                }
            }
            _ => unreachable!(),
        }
        let plan = case.plan();
        assert!(plan.capability_workers.is_empty(), "{blocked}");
        assert_eq!(plan.deferred_capabilities, [0, 1], "{blocked}");
        assert_eq!(plan.build.proposed_workers.len(), 1, "{blocked}");
    }
}

#[test]
fn a_registration_denied_role_does_not_cover_a_demand() {
    let mut case = Case::new(
        vec![builder("joint", 4_000, 2, true)],
        vec![role_only("eval", true, false)],
        1,
        1_000,
    );
    case.request.profiles[0]
        .registration
        .as_mut()
        .unwrap()
        .enable_eval = false;
    case.capacity.profiles[0].capabilities.eval = false;
    case.bootstrap().profiles[0].profile.capabilities.eval = false;
    case.demand(BootstrapDemandKind::Evaluation, vec![false, true]);
    case.policy.max_new_workers = 1;
    let plan = case.plan();
    assert_eq!(plan.capability_workers.len(), 1);
    assert!(plan.build.proposed_workers.is_empty());
}

#[test]
fn ties_prefer_more_uncovered_rows_then_configured_profile_order() {
    let mut case = Case::new(
        vec![],
        vec![
            role_only("first", true, true),
            role_only("versatile", true, true),
        ],
        0,
        1_000,
    );
    case.demand(BootstrapDemandKind::Evaluation, vec![true, true]);
    case.demand(BootstrapDemandKind::InputFetch, vec![false, true]);
    case.policy.max_new_workers = 1;
    let plan = case.plan();
    assert_eq!(plan.capability_workers[0].label, "versatile");
    assert!(plan.deferred_capabilities.is_empty());
    case.bootstrap().demands[1].profiles[0] = true;
    assert_eq!(case.plan().capability_workers[0].label, "first");
}

#[test]
fn lookahead_uses_the_oldest_unmet_build_not_response_order_or_reserved_ram() {
    for covered in [false, true] {
        let mut case = Case::new(
            vec![
                builder("small", 2_000, 2, true),
                builder("large", 8_000, 2, true),
            ],
            vec![],
            2,
            1_000,
        );
        case.capacity.builds[0].ready_at = CAPTURED.into();
        for (profile, assessment) in case.capacity.builds[1].profiles.iter_mut().enumerate() {
            let assessment = assessment.as_mut().unwrap();
            assessment.resources.predicted_ram_mb = Some(6_000);
            assessment.resources.reserved_ram_mb = Some(1_000);
            assessment.resources.memory_assumption = MemoryAssumption::Proportional;
            assessment.prediction_fit = Some(if profile == 0 {
                RamFit::DoesNotFit
            } else {
                RamFit::Fits
            });
        }
        if covered {
            case.capacity.workers.push(CapacityWorker {
                free_slots: 1,
                available_ram_mb: Some(6_000),
                waiting_build: None,
            });
            case.capacity.builds[0].workers.push(None);
            case.capacity.builds[1].workers.push(Some(resource(6_000)));
        }
        case.policy.max_new_workers = 1;
        case.demand(BootstrapDemandKind::Evaluation, vec![true, true]);
        let plan = case.plan();
        assert_eq!(plan.build.proposed_workers.len(), 1);
        assert_eq!(
            plan.build.proposed_workers[0].label,
            if covered { "small" } else { "large" }
        );
        assert!(plan.build.deferred_builds.is_empty());
    }
}

#[test]
fn partial_sources_and_server_drain_preserve_pure_preview_semantics() {
    let mut case = Case::new(
        vec![builder("joint", 4_000, 2, true)],
        vec![role_only("eval", true, false)],
        1,
        1_000,
    );
    case.demand(BootstrapDemandKind::Evaluation, vec![true, true]);
    case.bootstrap().complete = false;
    case.capacity.total_ready_builds = 8;
    case.capacity.builds_truncated = true;
    case.open.push(Launch::new("ops", "unused", "old"));
    let before = case.open.clone();
    let plan = case.plan();
    assert!(!plan.bootstrap_complete);
    assert_eq!(plan.build.proposed_workers.len(), 1);
    assert!(plan.build.builds_truncated);
    assert_eq!(case.open, before);
    case.capacity.server_draining = true;
    let plan = case.plan();
    assert!(plan.build.server_draining);
    assert!(plan.build.builds_truncated);
    assert!(plan.build.proposed_workers.is_empty());
    assert!(plan.capability_workers.is_empty());
    assert!(plan.build.deferred_builds.is_empty());
    assert_eq!(plan.deferred_capabilities, [0]);
    assert_eq!(case.open, before);
    case.bootstrap().demands.clear();
    assert!(case.plan().deferred_capabilities.is_empty());
    assert_eq!(case.open, before);
}

#[test]
fn absent_or_already_available_bootstrap_keeps_the_ordinary_plan_unchanged() {
    let mut case = Case::new(
        vec![
            builder("first", 4_000, 1, true),
            builder("second", 4_000, 1, true),
        ],
        vec![],
        3,
        1_000,
    );
    case.request.bootstrap = None;
    case.capacity.bootstrap = None;
    let plan = case.plan();
    assert_eq!(plan.build, case.ordinary());
    assert!(plan.bootstrap_complete);
    assert!(plan.capability_workers.is_empty());
    assert!(plan.deferred_capabilities.is_empty());
    case.request.bootstrap = Some(BootstrapCapacityRequest::default());
    case.capacity.bootstrap = Some(BootstrapCapacityAssessment {
        observed_at: CAPTURED.into(),
        complete: true,
        demands: vec![],
        profiles: case
            .capacity
            .profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| BootstrapCapacityProfile {
                profile: profile.clone(),
                build_profile_index: Some(index),
            })
            .collect(),
        tracked_workers: vec![],
    });
    case.demand(BootstrapDemandKind::Evaluation, vec![true, true]);
    case.bootstrap().demands[0].available = true;
    assert_eq!(case.plan().build, case.ordinary());
}

#[test]
fn context_scope_duplicate_open_identities_and_label_dimensions_are_validated() {
    let mut case = Case::new(vec![builder("joint", 4_000, 2, true)], vec![], 0, 1_000);
    case.demand(BootstrapDemandKind::Evaluation, vec![true]);
    let launch = Launch::new("other", "pod", "joint");
    case.open.push(launch);
    assert!(plan_joint_capacity(&case.request, &case.capacity, case.context(), &[]).is_err());
    case.open[0].connection = "ops".into();
    case.open.push(case.open[0].clone());
    assert!(plan_joint_capacity(&case.request, &case.capacity, case.context(), &[]).is_err());
    case.open.pop();
    case.open[0].worker_id = Some(Uuid::new_v4());
    let mut duplicate_worker = case.open[0].clone();
    duplicate_worker.id = Uuid::new_v4();
    case.open.push(duplicate_worker);
    assert!(plan_joint_capacity(&case.request, &case.capacity, case.context(), &[]).is_err());
    case.open.clear();
    assert!(
        plan_joint_capacity(
            &case.request,
            &case.capacity,
            case.context(),
            &["extra".into()]
        )
        .is_err()
    );
    case.labels.clear();
    assert!(plan_joint_capacity(&case.request, &case.capacity, case.context(), &[]).is_err());
}
