use crate::github::{HostedRunner, SelfHostedRunner};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub systems: Vec<String>,
    pub features: Vec<String>,
    pub max_jobs: u32,
    pub ram_mb: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindSource {
    Standard,
    Larger,
    SelfHosted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kind {
    pub label: String,
    pub source: KindSource,
    pub cost: u32,
    pub profile: Option<Profile>,
}

impl Profile {
    fn new(system: &str, features: &[&str], max_jobs: u32, ram_mb: u64) -> Profile {
        Profile {
            systems: vec![system.to_owned()],
            features: features.iter().map(|f| (*f).to_owned()).collect(),
            max_jobs,
            ram_mb: Some(ram_mb),
        }
    }

    pub fn runs_linux(&self) -> bool {
        self.systems.iter().any(|system| system.ends_with("-linux"))
    }
}

impl Kind {
    fn standard(label: &str, cost: u32, profile: Profile) -> Kind {
        Kind {
            label: label.to_owned(),
            source: KindSource::Standard,
            cost,
            profile: Some(profile),
        }
    }

    pub fn runs_on(&self) -> String {
        serde_json::to_string(&[&self.label]).expect("a list of strings serializes")
    }
}

pub fn standard_kinds() -> Vec<Kind> {
    vec![
        Kind::standard(
            "ubuntu-latest",
            1,
            Profile::new(
                "x86_64-linux",
                &["benchmark", "big-parallel", "kvm", "nixos-test"],
                4,
                16384,
            ),
        ),
        Kind::standard(
            "ubuntu-24.04-arm",
            1,
            Profile::new(
                "aarch64-linux",
                &["benchmark", "big-parallel", "nixos-test"],
                4,
                16384,
            ),
        ),
        Kind::standard(
            "macos-latest",
            10,
            Profile::new("aarch64-darwin", &["benchmark", "big-parallel"], 3, 7168),
        ),
    ]
}

pub fn larger_kind(runner: &HostedRunner) -> Option<Kind> {
    let system = match runner.platform.as_str() {
        "linux-x64" => "x86_64-linux",
        "linux-arm64" => "aarch64-linux",
        _ => return None,
    };
    let cores = runner.machine_size_details.cpu_cores;
    let ram_mb = runner.machine_size_details.memory_gb.unwrap_or_default() * 1024;
    Some(Kind {
        label: runner.name.clone(),
        source: KindSource::Larger,
        cost: cores,
        profile: Some(Profile::new(
            system,
            &["benchmark", "big-parallel"],
            cores,
            ram_mb,
        )),
    })
}

pub fn self_hosted_kind(runner: &SelfHostedRunner) -> Option<Kind> {
    let label = runner.labels.iter().find(|label| label.kind == "custom")?;
    Some(Kind {
        label: label.name.clone(),
        source: KindSource::SelfHosted,
        cost: 0,
        profile: None,
    })
}

pub fn apply_learned(kinds: &mut [Kind], label: &str, profile: Profile) {
    if let Some(kind) = kinds.iter_mut().find(|kind| kind.label == label) {
        kind.profile = Some(profile);
    }
}

pub fn covers(profile: &Profile, system: &str, required: &[String]) -> bool {
    profile.systems.iter().any(|s| s == system)
        && required
            .iter()
            .all(|feature| profile.features.contains(feature))
}

pub fn cheapest<'a>(
    kinds: &'a [Kind],
    allowed: &[String],
    system: &str,
    required: &[String],
) -> Option<&'a Kind> {
    kinds
        .iter()
        .filter(|kind| allowed.contains(&kind.label))
        .filter(|kind| {
            kind.profile
                .as_ref()
                .is_some_and(|p| covers(p, system, required))
        })
        .min_by_key(|kind| kind.cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    fn self_hosted_with_profile(label: &str, system: &str) -> Kind {
        Kind {
            label: label.into(),
            source: KindSource::SelfHosted,
            cost: 0,
            profile: Some(Profile::new(system, &["big-parallel"], 8, 32768)),
        }
    }

    #[test]
    fn cheapest_prefers_self_hosted_then_standard() {
        let mut kinds = standard_kinds();
        kinds.push(self_hosted_with_profile("rack-1", "x86_64-linux"));
        let both = strings(&["ubuntu-latest", "rack-1"]);

        let pick = cheapest(&kinds, &both, "x86_64-linux", &strings(&["big-parallel"])).unwrap();
        assert_eq!(pick.label, "rack-1");

        let pick = cheapest(&kinds, &both, "x86_64-linux", &strings(&["kvm"])).unwrap();
        assert_eq!(pick.label, "ubuntu-latest");
    }

    #[test]
    fn macos_not_chosen_unless_allowed() {
        let kinds = standard_kinds();
        let defaults = strings(&["ubuntu-latest", "ubuntu-24.04-arm"]);
        assert!(cheapest(&kinds, &defaults, "aarch64-darwin", &[]).is_none());

        let with_macos = strings(&["ubuntu-latest", "macos-latest"]);
        let pick = cheapest(&kinds, &with_macos, "aarch64-darwin", &[]).unwrap();
        assert_eq!(pick.label, "macos-latest");
    }

    #[test]
    fn covers_requires_all_features() {
        let profile = Profile::new("aarch64-linux", &["big-parallel", "nixos-test"], 4, 16384);
        assert!(covers(
            &profile,
            "aarch64-linux",
            &strings(&["big-parallel"])
        ));
        assert!(!covers(
            &profile,
            "aarch64-linux",
            &strings(&["big-parallel", "kvm"])
        ));
        assert!(!covers(&profile, "x86_64-linux", &[]));
    }

    #[test]
    fn learned_profile_replaces_default() {
        let mut kinds = standard_kinds();
        let learned = Profile::new("x86_64-linux", &["kvm"], 2, 8192);
        apply_learned(&mut kinds, "ubuntu-latest", learned.clone());

        let allowed = strings(&["ubuntu-latest"]);
        assert!(
            cheapest(
                &kinds,
                &allowed,
                "x86_64-linux",
                &strings(&["big-parallel"])
            )
            .is_none()
        );
        assert_eq!(kinds[0].profile, Some(learned));
    }
}
