pub const WORKFLOW_FILE: &str = "gradient-worker.yml";
pub const WORKFLOW_PATH: &str = ".github/workflows/gradient-worker.yml";

const TEMPLATE: &str = include_str!("gradient-worker.yml");
const MARKER: &str = "# managed by gradient-gha-autoscaler v";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowAction {
    Create,
    Update,
    Keep,
    UserOwned,
}

pub fn render(version: &str) -> String {
    TEMPLATE.replace("@VERSION@", version)
}

pub fn decide(existing: Option<&str>, version: &str) -> WorkflowAction {
    let Some(existing) = existing else {
        return WorkflowAction::Create;
    };
    let first_line = existing.lines().next().unwrap_or_default();
    if !first_line.starts_with(MARKER) {
        WorkflowAction::UserOwned
    } else if existing == render(version) {
        WorkflowAction::Keep
    } else {
        WorkflowAction::Update
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_keeps_user_owned_file_without_marker() {
        let edited = render("0.1.0").replacen(MARKER, "# my own worker v", 1);
        assert_eq!(decide(Some(&edited), "0.1.0"), WorkflowAction::UserOwned);
        assert_eq!(
            decide(Some("on: push\n"), "0.1.0"),
            WorkflowAction::UserOwned
        );
    }

    #[test]
    fn decide_updates_older_marker() {
        assert_eq!(
            decide(Some(&render("0.0.9")), "0.1.0"),
            WorkflowAction::Update
        );
        assert_eq!(
            decide(Some(&render("0.1.0")), "0.1.0"),
            WorkflowAction::Keep
        );
        assert_eq!(decide(None, "0.1.0"), WorkflowAction::Create);
    }

    #[test]
    fn render_contains_no_secret_inputs() {
        let rendered = render("0.1.0");
        assert!(rendered.starts_with(&format!("{MARKER}0.1.0\n")));
        let inputs = rendered
            .split("inputs:\n")
            .nth(1)
            .and_then(|rest| rest.split("\npermissions:").next())
            .unwrap();
        let names: Vec<&str> = inputs
            .lines()
            .filter(|line| line.starts_with("      ") && !line.starts_with("        "))
            .map(|line| line.trim().trim_end_matches(':'))
            .collect();
        assert_eq!(
            names,
            [
                "runs_on",
                "autoscaler_url",
                "connection",
                "launch_id",
                "worker_flake",
                "idle_timeout",
                "max_lifetime"
            ]
        );
        assert!(
            !names
                .iter()
                .any(|name| name.contains("token") || name.contains("secret"))
        );
    }
}
