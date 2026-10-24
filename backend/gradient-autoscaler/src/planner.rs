use crate::gradient::TeamDemand;
use crate::kinds::{Kind, cheapest, covers};
use crate::state::{Launch, State};
use std::collections::{HashMap, HashSet};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

const FAILURE_WINDOW: Duration = Duration::minutes(30);
const FAILURES_BEFORE_BACKOFF: u32 = 2;
const BACKOFF: Duration = Duration::minutes(10);
const LINUX_SYSTEMS: [&str; 2] = ["x86_64-linux", "aarch64-linux"];

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub launch: Vec<String>,
    pub cancel: Vec<Uuid>,
    pub uncovered: Vec<(String, Vec<String>)>,
}

#[derive(Debug, Clone)]
pub struct Limits {
    pub allowed: Vec<String>,
    pub max_launches: u32,
    pub max_launches_per_kind: u32,
}

struct Planner<'a> {
    open: &'a [Launch],
    kinds: &'a [Kind],
    limits: &'a Limits,
    backoff: &'a HashSet<String>,
    used: Vec<bool>,
    per_label: HashMap<String, u32>,
    total: u32,
    plan: Plan,
}

pub fn plan(
    demand: &TeamDemand,
    open: &[Launch],
    kinds: &[Kind],
    limits: &Limits,
    backoff: &HashSet<String>,
) -> Plan {
    let mut planner = Planner::new(open, kinds, limits, backoff);
    for requirement in demand.builds.iter().filter(|r| r.build_count > 0) {
        planner.cover_build(&requirement.architecture, &requirement.required_features);
    }

    if demand.network > 0 || demand.secrets > 0 || demand.eval > 0 {
        planner.cover_network_and_eval();
    }

    planner.cancel_unneeded();
    planner.plan
}

pub async fn backed_off_labels(
    state: &State,
    connection: &str,
    kinds: &[Kind],
) -> anyhow::Result<HashSet<String>> {
    let now = OffsetDateTime::now_utc();
    let mut labels = HashSet::new();
    for kind in kinds {
        let recent = state
            .failures_since(connection, &kind.label, now - FAILURE_WINDOW)
            .await?;
        let latest = state
            .failures_since(connection, &kind.label, now - BACKOFF)
            .await?;
        if recent >= FAILURES_BEFORE_BACKOFF && latest > 0 {
            labels.insert(kind.label.clone());
        }
    }
    Ok(labels)
}

impl<'a> Planner<'a> {
    fn new(
        open: &'a [Launch],
        kinds: &'a [Kind],
        limits: &'a Limits,
        backoff: &'a HashSet<String>,
    ) -> Self {
        let mut per_label = HashMap::new();
        for launch in open {
            *per_label.entry(launch.label.clone()).or_default() += 1;
        }

        Planner {
            open,
            kinds,
            limits,
            backoff,
            used: vec![false; open.len()],
            per_label,
            total: u32::try_from(open.len()).unwrap_or(u32::MAX),
            plan: Plan::default(),
        }
    }

    fn kind(&self, label: &str) -> Option<&'a Kind> {
        self.kinds.iter().find(|kind| kind.label == label)
    }

    fn launchable(&self) -> Vec<Kind> {
        self.kinds
            .iter()
            .filter(|kind| {
                self.limits.allowed.contains(&kind.label) && !self.backoff.contains(&kind.label)
            })
            .cloned()
            .collect()
    }

    fn cover_build(&mut self, system: &str, features: &[String]) {
        let covering_open = (0..self.open.len()).find(|&i| {
            !self.used[i]
                && self
                    .kind(&self.open[i].label)
                    .and_then(|kind| kind.profile.as_ref())
                    .is_some_and(|profile| covers(profile, system, features))
        });
        if let Some(i) = covering_open {
            self.used[i] = true;
            return;
        }
        let launchable = self.launchable();
        if let Some(kind) = cheapest(&launchable, &self.limits.allowed, system, features) {
            self.try_launch(&kind.label, self.limits.max_launches_per_kind);
            return;
        }
        if !self.cover_with_trial(&launchable) {
            self.plan
                .uncovered
                .push((system.to_owned(), features.to_vec()));
        }
    }

    fn cover_with_trial(&mut self, launchable: &[Kind]) -> bool {
        let unknown: Vec<&Kind> = launchable
            .iter()
            .filter(|kind| kind.profile.is_none())
            .collect();
        for kind in &unknown {
            if let Some(i) = self
                .open
                .iter()
                .position(|launch| launch.label == kind.label)
            {
                self.used[i] = true;
                return true;
            }
            if self.plan.launch.contains(&kind.label) {
                return true;
            }
        }
        unknown.iter().any(|kind| self.try_launch(&kind.label, 1))
    }

    fn cover_network_and_eval(&mut self) {
        let runs_linux = |kind: Option<&Kind>| {
            kind.and_then(|k| k.profile.as_ref())
                .is_some_and(|p| p.runs_linux())
        };
        if let Some(i) = (0..self.open.len()).find(|&i| runs_linux(self.kind(&self.open[i].label)))
        {
            self.used[i] = true;
            return;
        }
        if self
            .plan
            .launch
            .iter()
            .any(|label| runs_linux(self.kind(label)))
        {
            return;
        }
        let launchable = self.launchable();
        let pick = LINUX_SYSTEMS
            .iter()
            .filter_map(|system| cheapest(&launchable, &self.limits.allowed, system, &[]))
            .min_by_key(|kind| kind.cost);
        match pick {
            Some(kind) => {
                self.try_launch(&kind.label, self.limits.max_launches_per_kind);
            }
            None => self.plan.uncovered.push(("linux".to_owned(), Vec::new())),
        }
    }

    fn try_launch(&mut self, label: &str, per_kind_cap: u32) -> bool {
        let count = self.per_label.entry(label.to_owned()).or_default();
        if self.total >= self.limits.max_launches || *count >= per_kind_cap {
            return false;
        }

        *count += 1;
        self.total += 1;
        self.plan.launch.push(label.to_owned());
        true
    }

    fn cancel_unneeded(&mut self) {
        for (launch, used) in self.open.iter().zip(&self.used) {
            if !used && !launch.swapped && launch.state.is_waiting_for_runner() {
                self.plan.cancel.push(launch.id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::kinds::standard_kinds;
    use crate::gradient::UnmetRequirement;
    use crate::state::LaunchState;

    fn limits() -> Limits {
        Limits {
            allowed: vec!["ubuntu-latest".into(), "ubuntu-24.04-arm".into()],
            max_launches: 4,
            max_launches_per_kind: 2,
        }
    }

    fn builds(requirements: &[(&str, &[&str])]) -> TeamDemand {
        TeamDemand {
            builds: requirements
                .iter()
                .map(|(system, features)| UnmetRequirement {
                    architecture: (*system).into(),
                    required_features: features.iter().map(|f| (*f).into()).collect(),
                    build_count: 5,
                })
                .collect(),
            network: 0,
            secrets: 0,
            eval: 0,
        }
    }

    fn open(label: &str, external_id: u64, state: LaunchState) -> Launch {
        Launch {
            state,
            ..Launch::new("ops", &external_id.to_string(), label)
        }
    }

    fn no_backoff() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn one_launch_per_missing_capability() {
        let demand = builds(&[
            ("x86_64-linux", &[]),
            ("x86_64-linux", &["kvm"]),
            ("aarch64-linux", &[]),
        ]);
        let plan = plan(&demand, &[], &standard_kinds(), &limits(), &no_backoff());
        assert_eq!(
            plan.launch,
            ["ubuntu-latest", "ubuntu-latest", "ubuntu-24.04-arm"]
        );
        assert!(plan.cancel.is_empty() && plan.uncovered.is_empty());
    }

    #[test]
    fn existing_open_launch_covers_requirement() {
        let launches = [open("ubuntu-latest", 1, LaunchState::Queued)];
        let demand = builds(&[("x86_64-linux", &["kvm"])]);
        let plan = plan(
            &demand,
            &launches,
            &standard_kinds(),
            &limits(),
            &no_backoff(),
        );
        assert_eq!(plan, Plan::default());
    }

    #[test]
    fn eval_gap_launches_linux_kind() {
        let demand = TeamDemand {
            eval: 1,
            ..TeamDemand::default()
        };
        let plan = plan(&demand, &[], &standard_kinds(), &limits(), &no_backoff());
        assert_eq!(plan.launch.len(), 1);
        assert!(plan.launch[0].starts_with("ubuntu-"));

        let running = [open("ubuntu-24.04-arm", 1, LaunchState::Running)];
        let covered = super::plan(
            &demand,
            &running,
            &standard_kinds(),
            &limits(),
            &no_backoff(),
        );
        assert!(covered.launch.is_empty());
    }

    #[test]
    fn caps_respected() {
        let demand = builds(&[
            ("x86_64-linux", &[]),
            ("x86_64-linux", &["kvm"]),
            ("x86_64-linux", &["nixos-test"]),
            ("aarch64-linux", &[]),
            ("aarch64-linux", &["big-parallel"]),
            ("aarch64-linux", &["nixos-test"]),
        ]);
        let plan = plan(&demand, &[], &standard_kinds(), &limits(), &no_backoff());
        let per = |label: &str| plan.launch.iter().filter(|l| *l == label).count();
        assert_eq!(plan.launch.len(), 4);
        assert_eq!((per("ubuntu-latest"), per("ubuntu-24.04-arm")), (2, 2));
        assert!(plan.uncovered.is_empty());
    }

    #[test]
    fn backed_off_kind_is_skipped() {
        let backoff = HashSet::from(["ubuntu-24.04-arm".to_string()]);
        let demand = builds(&[("aarch64-linux", &[]), ("x86_64-linux", &[])]);
        let plan = plan(&demand, &[], &standard_kinds(), &limits(), &backoff);
        assert_eq!(plan.launch, ["ubuntu-latest"]);
    }

    #[test]
    fn unneeded_queued_launch_is_cancelled() {
        let launches = [
            open("ubuntu-latest", 1, LaunchState::Queued),
            open("ubuntu-latest", 2, LaunchState::Running),
            open("ubuntu-24.04-arm", 3, LaunchState::Dispatched),
        ];
        let demand = builds(&[("aarch64-linux", &[])]);
        let plan = plan(
            &demand,
            &launches,
            &standard_kinds(),
            &limits(),
            &no_backoff(),
        );
        assert_eq!(plan.cancel, [launches[0].id]);
        assert!(plan.launch.is_empty());
    }

    #[test]
    fn swapped_queued_launch_is_not_cancelled() {
        let launches = [Launch {
            swapped: true,
            ..open("ubuntu-latest", 1, LaunchState::Queued)
        }];
        let plan = plan(
            &TeamDemand::default(),
            &launches,
            &standard_kinds(),
            &limits(),
            &no_backoff(),
        );
        assert!(plan.cancel.is_empty());
    }

    #[test]
    fn uncovered_requirement_reported() {
        let demand = builds(&[("aarch64-darwin", &[]), ("x86_64-linux", &["cuda"])]);
        let plan = plan(&demand, &[], &standard_kinds(), &limits(), &no_backoff());
        assert!(plan.launch.is_empty());
        assert_eq!(
            plan.uncovered,
            [
                ("aarch64-darwin".to_string(), vec![]),
                ("x86_64-linux".to_string(), vec!["cuda".to_string()])
            ]
        );
    }

    #[test]
    fn unknown_kind_gets_one_trial_launch() {
        let mut kinds = standard_kinds();
        kinds.push(Kind {
            label: "rack-1".into(),
            cost: 0,
            profile: None,
        });
        let mut limits = limits();
        limits.allowed.push("rack-1".into());
        let demand = builds(&[("riscv64-linux", &[]), ("x86_64-linux", &["cuda"])]);

        let first = plan(&demand, &[], &kinds, &limits, &no_backoff());
        assert_eq!(first.launch, ["rack-1"]);

        let trial = [open("rack-1", 9, LaunchState::Queued)];
        let second = plan(&demand, &trial, &kinds, &limits, &no_backoff());
        assert!(second.launch.is_empty() && second.cancel.is_empty());
    }

    #[tokio::test]
    async fn run_completed_without_worker_backs_off_its_kind() {
        let state = State::memory().await.unwrap();
        let limits = limits();
        for external_id in ["1", "2"] {
            let launch = Launch::new("ops", external_id, "ubuntu-latest");
            state.insert_launch(&launch).await.unwrap();
            state
                .close_launch(launch.id, LaunchState::Failed)
                .await
                .unwrap();
        }
        let kinds = standard_kinds();

        let backoff = backed_off_labels(&state, "ops", &kinds).await.unwrap();
        assert_eq!(backoff, HashSet::from(["ubuntu-latest".to_string()]));

        let demand = builds(&[("x86_64-linux", &["kvm"])]);
        let plan = plan(&demand, &[], &kinds, &limits, &backoff);
        assert!(plan.launch.is_empty());
        assert_eq!(plan.uncovered.len(), 1);
    }
}
