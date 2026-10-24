use crate::github::client::{HostedRunner, SelfHostedRunner};
use crate::kinds::{Kind, Profile};

pub fn standard_kinds() -> Vec<Kind> {
    vec![
        standard(
            "ubuntu-latest",
            1,
            Profile::new(
                "x86_64-linux",
                &["benchmark", "big-parallel", "kvm", "nixos-test"],
                4,
                Some(16384),
            ),
        ),
        standard(
            "ubuntu-24.04-arm",
            1,
            Profile::new(
                "aarch64-linux",
                &["benchmark", "big-parallel", "nixos-test"],
                4,
                Some(16384),
            ),
        ),
        standard(
            "macos-latest",
            10,
            Profile::new(
                "aarch64-darwin",
                &["benchmark", "big-parallel"],
                3,
                Some(7168),
            ),
        ),
    ]
}

fn standard(label: &str, cost: u32, profile: Profile) -> Kind {
    Kind {
        label: label.to_owned(),
        cost,
        profile: Some(profile),
    }
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
        cost: cores,
        profile: Some(Profile::new(
            system,
            &["benchmark", "big-parallel"],
            cores,
            Some(ram_mb),
        )),
    })
}

pub fn self_hosted_kind(runner: &SelfHostedRunner) -> Option<Kind> {
    let label = runner.labels.iter().find(|label| label.kind == "custom")?;
    Some(Kind {
        label: label.name.clone(),
        cost: 0,
        profile: None,
    })
}

pub fn runs_on(kind: &Kind) -> String {
    serde_json::to_string(&[&kind.label]).expect("a list of strings serializes")
}
