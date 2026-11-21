use gradient_autoscaler::capacity::{
    BootstrapCapacityRequest, BuildCapacityProfile, BuildPageRequest, BuilderResources,
    CapacityAssessment, CapacityAssessmentRequest, CapacityCapabilities, CapacityProfileRequest,
    MemoryAssumption, RamFit, RegistrationCapabilities, parse_timestamp,
};
use gradient_autoscaler::gradient::GradientClient;
use serde_json::{Value, json};
use time::Duration;
use url::Url;
use uuid::Uuid;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ROUTE: &str = "/api/v1/teams/ops/workers/capacity-assessment";
const PROJECT: &str = "70000000-0000-0000-0000-000000000001";
const PROSPECTIVE: &str = "70000000-0000-0000-0000-000000000002";
const LIVE: &str = "70000000-0000-0000-0000-000000000003";
const STARTING: &str = "70000000-0000-0000-0000-000000000004";
const ROLE: &str = "70000000-0000-0000-0000-000000000005";
const TRACKED_ROLE: &str = "70000000-0000-0000-0000-000000000006";

fn client(server: &MockServer) -> GradientClient {
    GradientClient::new(
        Url::parse(&format!("{}/ui", server.uri())).unwrap(),
        "GRADkey".into(),
    )
}

fn profile(name: &str, id: &str) -> CapacityProfileRequest {
    CapacityProfileRequest {
        profile: BuildCapacityProfile {
            name: name.into(),
            worker_id: id.parse().unwrap(),
            capabilities: CapacityCapabilities {
                core: true,
                federate: false,
                eval: true,
                build: true,
                cache: true,
                network: true,
                secrets: true,
                exec: true,
            },
            resources: BuilderResources {
                architectures: vec!["x86_64-linux".into()],
                system_features: vec!["kvm".into()],
                max_concurrent_builds: 2,
                cpu_count: 4,
                ram_total_mb: 8000,
                cpu_core_score: 4000,
                build_cores: None,
            },
        },
        enable_network: true,
        enable_build: true,
        registration: None,
    }
}

fn request() -> CapacityAssessmentRequest {
    CapacityAssessmentRequest {
        projects: Some(vec![PROJECT.parse().unwrap()]),
        profiles: vec![profile("candidate", PROSPECTIVE)],
        tracked_profiles: vec![profile("live", LIVE), profile("starting", STARTING)],
        include_waiting_builds: true,
        ..Default::default()
    }
}

fn wire_profile(name: &str, id: &str) -> Value {
    json!({
        "name": name,
        "worker_id": id,
        "capabilities": {
            "core": true, "federate": false, "eval": true, "build": true,
            "cache": true, "network": true, "secrets": true, "exec": true
        },
        "resources": {
            "architectures": ["x86_64-linux"], "system_features": ["kvm"],
            "max_concurrent_builds": 2, "cpu_count": 4, "ram_total_mb": 8000,
            "cpu_core_score": 4000, "build_cores": null
        }
    })
}

fn wire_resources() -> Value {
    json!({
        "cores": 2,
        "reserved_ram_mb": 3000,
        "predicted_ram_mb": 3000,
        "ram_fit": "fits",
        "memory_assumption": "predicted",
        "own_samples": 3,
        "build_time_known": true,
        "time": {
            "download_secs": 1.0, "path_secs": 2.0, "build_secs": 10.0,
            "upload_secs": 3.0, "eval_secs": 0.0,
            "nar_bytes": 100.0, "paths": 1.0, "output_nar_bytes": 50.0,
            "cores": 2, "fallbacks": ["future_metadata"]
        }
    })
}

fn wire_profile_assessment() -> Value {
    json!({ "resources": wire_resources(), "prediction_fit": "fits" })
}

fn wire_response() -> Value {
    let mut live_profile = wire_profile("live", LIVE);
    live_profile["resources"]["zone"] = Value::Null;
    live_profile["resources"]["endpoint"] = Value::Null;
    live_profile["resources"]["exec"] = Value::Null;
    json!({
        "captured_at": "2026-11-11T22:00:00.123456",
        "server_draining": false,
        "total_ready_builds": 1,
        "builds_truncated": false,
        "workers": [{
            "free_slots": 1, "available_ram_mb": 2000,
            "waiting_build": { "build_index": 0, "ram_credit_mb": 1000 }
        }],
        "profiles": [wire_profile("candidate", PROSPECTIVE)],
        "tracked_workers": [
            { "profile": live_profile, "connected": true, "worker_index": 0 },
            { "profile": wire_profile("starting", STARTING), "connected": false, "worker_index": null }
        ],
        "builds": [{
            "architecture": "x86_64-linux", "required_features": ["kvm"],
            "is_fixed_output": false, "ready_at": "2026-11-11T21:55:00.123456",
            "workers": [wire_resources()],
            "profiles": [wire_profile_assessment()],
            "tracked_workers": [null, wire_profile_assessment()]
        }]
    })
}

fn wire_registration() -> Value {
    json!({
        "enable_build": true, "enable_network": true, "enable_eval": true,
        "enable_secrets": true, "enable_exec": true
    })
}

fn wire_role_profile(name: &str, id: &str) -> Value {
    json!({
        "name": name, "worker_id": id,
        "capabilities": {
            "core": false, "federate": false, "cache": false, "build": false,
            "network": true, "eval": true, "secrets": true, "exec": true
        },
        "resources": {
            "architectures": ["aarch64-linux"], "system_features": [],
            "max_concurrent_builds": 1, "cpu_count": 0, "ram_total_mb": 0,
            "cpu_core_score": 0, "build_cores": null
        }
    })
}

fn bootstrap_request() -> CapacityAssessmentRequest {
    let mut input = request();
    input.profiles[0].registration = Some(serde_json::from_value(wire_registration()).unwrap());
    input.build_page = Some(BuildPageRequest::default());
    input.bootstrap = Some(serde_json::from_value(json!({
        "profiles": [{
            "profile": wire_role_profile("role", ROLE), "registration": wire_registration()
        }],
        "tracked_profiles": [{
            "profile": wire_role_profile("tracked-role", TRACKED_ROLE), "registration": wire_registration()
        }]
    })).unwrap());
    input
}

fn bootstrap_response() -> Value {
    let mut response = wire_response();
    response["build_page"] = json!({"remaining_ready_builds": 0, "next": null});
    response["bootstrap"] = json!({
        "observed_at": "2026-11-11T22:00:00.123456", "complete": true,
        "profiles": [
            { "profile": wire_profile("candidate", PROSPECTIVE), "build_profile_index": 0 },
            { "profile": wire_role_profile("role", ROLE), "build_profile_index": null }
        ],
        "tracked_workers": [
            { "profile": wire_profile("live", LIVE), "connected": true, "build_tracked_index": 0 },
            { "profile": wire_profile("starting", STARTING), "connected": false, "build_tracked_index": 1 },
            { "profile": wire_role_profile("tracked-role", TRACKED_ROLE), "connected": false, "build_tracked_index": null }
        ],
        "demands": [
            { "kind": "download", "available": false, "profiles": [true, true], "tracked_workers": [true, true, true] },
            { "kind": "substitution", "available": true, "profiles": [true, true], "tracked_workers": [true, true, true] },
            { "kind": "evaluation", "available": false, "profiles": [true, true], "tracked_workers": [true, true, true] },
            { "kind": "input_fetch", "available": false, "profiles": [true, true], "tracked_workers": [true, true, true] }
        ]
    });
    response
}

#[test]
fn bootstrap_operation_kinds_exclude_ordinary_builds() {
    use gradient_autoscaler::capacity::BootstrapDemandKind;
    assert_eq!(
        serde_json::to_value([
            BootstrapDemandKind::Download,
            BootstrapDemandKind::Substitution,
            BootstrapDemandKind::Evaluation,
            BootstrapDemandKind::InputFetch,
        ])
        .unwrap(),
        json!(["download", "substitution", "evaluation", "input_fetch"])
    );
    assert!(serde_json::from_value::<BootstrapDemandKind>(json!("build")).is_err());
}

async fn respond(server: &MockServer, response: Value) {
    Mock::given(method("POST"))
        .and(path(ROUTE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": false, "message": response
        })))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn capacity_assessment_sends_typed_payload_and_preserves_native_metadata() {
    let server = MockServer::start().await;
    let mut response = wire_response();
    response["profiles"][0]["capabilities"]["network"] = json!(false);
    response["tracked_workers"][1]["profile"]["capabilities"]["network"] = json!(false);
    response["builds"][0]["profiles"][0]["resources"]["predicted_ram_mb"] = json!(10000);
    response["builds"][0]["profiles"][0]["resources"]["memory_assumption"] = json!("proportional");
    response["builds"][0]["profiles"][0]["prediction_fit"] = json!("does_not_fit");
    let expected_profile = wire_profile("candidate", PROSPECTIVE);
    Mock::given(method("POST"))
        .and(path(ROUTE))
        .and(header("Authorization", "Bearer GRADkey"))
        .and(header("Accept", "application/json"))
        .and(body_json(json!({
            "projects": [PROJECT],
            "profiles": [{
                "profile": expected_profile,
                "enable_network": false, "enable_build": true
            }],
            "tracked_profiles": [
                { "profile": wire_profile("live", LIVE), "enable_network": true, "enable_build": true },
                { "profile": wire_profile("starting", STARTING), "enable_network": true, "enable_build": true }
            ],
            "include_waiting_builds": true
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": false, "message": response
        })))
        .expect(1)
        .mount(&server)
        .await;
    let mut input = request();
    input.profiles[0].enable_network = false;

    let assessment = client(&server)
        .capacity_assessment("ops", &input)
        .await
        .unwrap();
    assert_eq!(assessment.tracked_workers[0].worker_index, Some(0));
    assert!(assessment.tracked_workers[0].connected);
    assert!(!assessment.tracked_workers[1].profile.capabilities.network);
    assert!(assessment.tracked_workers[1].profile.capabilities.secrets);
    assert!(assessment.tracked_workers[1].profile.capabilities.eval);
    assert!(assessment.tracked_workers[1].profile.capabilities.exec);
    assert_eq!(
        assessment.workers[0]
            .waiting_build
            .as_ref()
            .unwrap()
            .ram_credit_mb,
        1000
    );
    let sizing = assessment.builds[0].profiles[0].as_ref().unwrap();
    assert_eq!(sizing.prediction_fit, Some(RamFit::DoesNotFit));
    assert_eq!(sizing.resources.ram_fit, Some(RamFit::Fits));
    assert_eq!(
        sizing.resources.memory_assumption,
        MemoryAssumption::Proportional
    );
    assert_eq!(sizing.resources.time.total_secs(), 16.0);
    assert_eq!(sizing.resources.time.fallbacks, ["future_metadata"]);
    assert_eq!(
        parse_timestamp(&assessment.captured_at).unwrap()
            - parse_timestamp(&assessment.builds[0].ready_at).unwrap(),
        Duration::minutes(5)
    );
}

#[tokio::test]
async fn legacy_request_omits_new_fields_and_preserves_all_vs_no_projects() {
    for projects in [None, Some(Vec::new())] {
        let server = MockServer::start().await;
        let mut expected = json!({ "profiles": [] });
        if projects.is_some() {
            expected["projects"] = json!([]);
        }
        Mock::given(method("POST"))
            .and(path(ROUTE))
            .and(body_json(expected))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "error": false,
                "message": {
                    "captured_at": "2026-11-11T22:00:00", "server_draining": false,
                    "total_ready_builds": 0, "builds_truncated": false,
                    "workers": [], "profiles": [], "builds": []
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let input = CapacityAssessmentRequest {
            projects,
            ..Default::default()
        };
        let assessment = client(&server)
            .capacity_assessment("ops", &input)
            .await
            .unwrap();
        assert!(assessment.tracked_workers.is_empty());
        assert_eq!(assessment.total_ready_builds, 0);
    }
}

#[test]
fn registration_wire_requires_all_five_snake_case_permissions() {
    assert_eq!(
        serde_json::to_value(RegistrationCapabilities {
            enable_build: true,
            enable_network: false,
            enable_eval: true,
            enable_secrets: false,
            enable_exec: false,
        })
        .unwrap(),
        json!({
            "enable_build": true, "enable_network": false, "enable_eval": true,
            "enable_secrets": false, "enable_exec": false
        })
    );
    for field in [
        "enable_build",
        "enable_network",
        "enable_eval",
        "enable_secrets",
        "enable_exec",
    ] {
        let mut missing = wire_registration();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<RegistrationCapabilities>(missing).is_err(),
            "{field}"
        );
    }
    let mut unknown = wire_registration();
    unknown["enable_device"] = json!(true);
    assert!(serde_json::from_value::<RegistrationCapabilities>(unknown).is_err());
    assert_eq!(
        serde_json::to_value(BuildPageRequest::default()).unwrap(),
        json!({})
    );
    assert_eq!(
        serde_json::to_value(BootstrapCapacityRequest::default()).unwrap(),
        json!({"profiles": []})
    );
}

#[tokio::test]
async fn bootstrap_uses_literal_additive_wire_contract_with_general_roster() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(ROUTE))
        .and(body_json(json!({
            "projects": [PROJECT], "include_waiting_builds": true, "build_page": {},
            "profiles": [{
                "profile": wire_profile("candidate", PROSPECTIVE),
                "enable_network": true, "enable_build": true, "registration": wire_registration()
            }],
            "tracked_profiles": [
                { "profile": wire_profile("live", LIVE), "enable_network": true, "enable_build": true },
                { "profile": wire_profile("starting", STARTING), "enable_network": true, "enable_build": true }
            ],
            "bootstrap": {
                "profiles": [{ "profile": wire_role_profile("role", ROLE), "registration": wire_registration() }],
                "tracked_profiles": [{ "profile": wire_role_profile("tracked-role", TRACKED_ROLE), "registration": wire_registration() }]
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": false, "message": bootstrap_response()
        })))
        .expect(1)
        .mount(&server)
        .await;
    let assessment = client(&server)
        .capacity_assessment("ops", &bootstrap_request())
        .await
        .unwrap();
    let bootstrap = assessment.bootstrap.unwrap();
    assert_eq!(bootstrap.profiles.len(), 2);
    assert_eq!(bootstrap.tracked_workers.len(), 3);
    assert!(bootstrap.tracked_workers[0].connected);
    assert_eq!(bootstrap.profiles[1].profile.resources.cpu_count, 0);
    assert_eq!(bootstrap.profiles[1].build_profile_index, None);
    assert_eq!(assessment.profiles.len(), 1);
}

#[test]
fn bootstrap_masks_all_permissions_without_changing_legacy_tracked_defaults() {
    for (permission, capability) in [
        ("enable_build", "build"),
        ("enable_network", "network"),
        ("enable_eval", "eval"),
        ("enable_secrets", "secrets"),
        ("enable_exec", "exec"),
    ] {
        let mut input = bootstrap_request();
        let mut mask = wire_registration();
        mask[permission] = json!(false);
        if capability == "build" {
            input.profiles[0].registration = Some(serde_json::from_value(mask.clone()).unwrap());
            assert!(
                input
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("registration does not enable builds")
            );
            input.profiles[0].registration =
                Some(serde_json::from_value(wire_registration()).unwrap());
            input.bootstrap.as_mut().unwrap().profiles[0]
                .profile
                .capabilities
                .build = true;
        } else {
            input.profiles[0].registration = Some(serde_json::from_value(mask.clone()).unwrap());
        }
        input.bootstrap.as_mut().unwrap().profiles[0].registration =
            serde_json::from_value(mask.clone()).unwrap();
        input.bootstrap.as_mut().unwrap().tracked_profiles[0].registration =
            serde_json::from_value(mask).unwrap();
        if capability == "network" {
            input.bootstrap.as_mut().unwrap().profiles[0]
                .profile
                .resources
                .max_concurrent_builds = 0;
            input.bootstrap.as_mut().unwrap().tracked_profiles[0]
                .profile
                .resources
                .max_concurrent_builds = 0;
        }
        let mut response = bootstrap_response();
        if capability == "network" {
            response["bootstrap"]["profiles"][1]["profile"]["resources"]["max_concurrent_builds"] =
                json!(0);
            response["bootstrap"]["tracked_workers"][2]["profile"]["resources"]["max_concurrent_builds"] =
                json!(0);
        }
        if capability != "build" {
            response["profiles"][0]["capabilities"][capability] = json!(false);
            response["bootstrap"]["profiles"][0]["profile"]["capabilities"][capability] =
                json!(false);
        }
        response["bootstrap"]["profiles"][1]["profile"]["capabilities"][capability] = json!(false);
        response["bootstrap"]["tracked_workers"][1]["profile"]["capabilities"][capability] =
            json!(false);
        response["bootstrap"]["tracked_workers"][2]["profile"]["capabilities"][capability] =
            json!(false);
        let mut assessment: CapacityAssessment = serde_json::from_value(response).unwrap();
        assessment.validate_against(&input).unwrap();

        let tracked = &mut assessment.bootstrap.as_mut().unwrap().tracked_workers[2]
            .profile
            .capabilities;
        match capability {
            "build" => tracked.build = true,
            "network" => tracked.network = true,
            "eval" => tracked.eval = true,
            "secrets" => tracked.secrets = true,
            "exec" => tracked.exec = true,
            _ => unreachable!(),
        }
        assert!(assessment.validate_against(&input).is_err(), "{capability}");
        let mut response = serde_json::to_value(&assessment).unwrap();
        response["bootstrap"]["tracked_workers"][2]["profile"]["capabilities"][capability] =
            json!(false);
        assessment = serde_json::from_value(response).unwrap();
        // The prospective mask must be exact, not just a subset.
        if capability == "build" {
            assessment.bootstrap.as_mut().unwrap().profiles[1]
                .profile
                .capabilities
                .build = true;
        } else {
            assessment.bootstrap.as_mut().unwrap().profiles[0]
                .profile
                .capabilities = profile("candidate", PROSPECTIVE).profile.capabilities;
        }
        assert!(assessment.validate_against(&input).is_err(), "{capability}");
    }
}

#[test]
fn bootstrap_root_tracked_permissions_may_narrow_but_never_widen() {
    for (permission, capability) in [
        ("enable_build", "build"),
        ("enable_network", "network"),
        ("enable_eval", "eval"),
        ("enable_secrets", "secrets"),
        ("enable_exec", "exec"),
    ] {
        for registration in [
            None,
            Some(serde_json::from_value(wire_registration()).unwrap()),
        ] {
            let mut input = bootstrap_request();
            input.tracked_profiles[1].registration = registration;
            let mut response = bootstrap_response();
            response["tracked_workers"][1]["profile"]["capabilities"][capability] = json!(false);
            response["bootstrap"]["tracked_workers"][1]["profile"]["capabilities"][capability] =
                json!(false);
            if capability == "build" {
                response["builds"][0]["tracked_workers"][1] = Value::Null;
            }
            let assessment: CapacityAssessment = serde_json::from_value(response.clone()).unwrap();
            assessment.validate_against(&input).unwrap();
            if capability == "build" {
                let mut inconsistent = response.clone();
                inconsistent["builds"][0]["tracked_workers"][1] =
                    bootstrap_response()["builds"][0]["tracked_workers"][1].clone();
                let inconsistent: CapacityAssessment =
                    serde_json::from_value(inconsistent).unwrap();
                assert!(
                    inconsistent
                        .validate_against(&input)
                        .unwrap_err()
                        .to_string()
                        .contains("build-disabled tracked worker")
                );
            }

            let mut intended = serde_json::to_value(&input).unwrap();
            intended["tracked_profiles"][1]["registration"] = wire_registration();
            intended["tracked_profiles"][1]["registration"][permission] = json!(false);
            let intended: CapacityAssessmentRequest = serde_json::from_value(intended).unwrap();
            if capability == "build" {
                assert!(
                    intended
                        .validate()
                        .unwrap_err()
                        .to_string()
                        .contains("registration does not enable builds")
                );
                continue;
            }
            assessment.validate_against(&intended).unwrap();
            response["tracked_workers"][1]["profile"]["capabilities"][capability] = json!(true);
            let widened: CapacityAssessment = serde_json::from_value(response).unwrap();
            assert!(
                widened
                    .validate_against(&intended)
                    .unwrap_err()
                    .to_string()
                    .contains("capabilities"),
                "{capability}"
            );
        }

        let mut legacy_response = wire_response();
        legacy_response["tracked_workers"][1]["profile"]["capabilities"][capability] = json!(false);
        if capability == "build" {
            legacy_response["builds"][0]["tracked_workers"][1] = Value::Null;
        }
        let legacy: CapacityAssessment = serde_json::from_value(legacy_response).unwrap();
        assert_eq!(
            legacy.validate_against(&request()).is_ok(),
            capability == "network",
            "{capability}"
        );
    }
}

#[test]
fn bootstrap_preserves_independent_native_capabilities() {
    for capability in ["core", "federate", "cache"] {
        let mut input = serde_json::to_value(bootstrap_request()).unwrap();
        let mut response = bootstrap_response();
        input["bootstrap"]["profiles"][0]["profile"]["capabilities"][capability] = json!(true);
        input["bootstrap"]["tracked_profiles"][0]["profile"]["capabilities"][capability] =
            json!(true);
        response["bootstrap"]["profiles"][1]["profile"]["capabilities"][capability] = json!(true);
        response["bootstrap"]["tracked_workers"][2]["profile"]["capabilities"][capability] =
            json!(true);
        let input: CapacityAssessmentRequest = serde_json::from_value(input).unwrap();
        input.validate().unwrap();
        let assessment: CapacityAssessment = serde_json::from_value(response.clone()).unwrap();
        assessment.validate_against(&input).unwrap();
        assert_eq!(
            assessment.bootstrap.as_ref().unwrap().profiles[1].profile,
            input.bootstrap.as_ref().unwrap().profiles[0].profile
        );
        assert_eq!(
            assessment.bootstrap.as_ref().unwrap().tracked_workers[2].profile,
            input.bootstrap.as_ref().unwrap().tracked_profiles[0].profile
        );
        response["bootstrap"]["tracked_workers"][2]["profile"]["capabilities"][capability] =
            json!(false);
        let changed: CapacityAssessment = serde_json::from_value(response).unwrap();
        assert!(
            changed
                .validate_against(&input)
                .unwrap_err()
                .to_string()
                .contains("capabilities"),
            "{capability}"
        );
    }
}

#[test]
fn eval_only_bootstrap_profiles_need_neither_build_sizing_nor_transfer_slots() {
    let mut input = bootstrap_request();
    let bootstrap = input.bootstrap.as_mut().unwrap();
    for profile in bootstrap
        .profiles
        .iter_mut()
        .chain(&mut bootstrap.tracked_profiles)
    {
        profile.profile.capabilities.network = false;
        profile.profile.resources.max_concurrent_builds = 0;
    }
    input.validate().unwrap();
}

#[tokio::test]
async fn partial_bootstrap_and_first_page_backlog_remain_explicit() {
    let server = MockServer::start().await;
    let mut response = bootstrap_response();
    response["bootstrap"]["complete"] = json!(false);
    response["total_ready_builds"] = json!(129);
    response["builds_truncated"] = json!(true);
    response["build_page"] = json!({"remaining_ready_builds": 128, "next": "opaque-page"});
    respond(&server, response).await;
    let assessment = client(&server)
        .capacity_assessment("ops", &bootstrap_request())
        .await
        .unwrap();
    assert!(!assessment.bootstrap.as_ref().unwrap().complete);
    assert!(assessment.builds_truncated);
    assert_eq!(assessment.builds.len(), 1);
    assert_eq!(
        assessment.build_page.unwrap().next.as_deref(),
        Some("opaque-page")
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[test]
fn build_page_remaining_bounds_distinguish_first_and_subsequent_pages() {
    for after in [None, Some("opaque-page".to_owned())] {
        for remaining in [0, 1, 128, 129] {
            let mut input = bootstrap_request();
            input.build_page.as_mut().unwrap().after = after.clone();
            let mut response = bootstrap_response();
            response["total_ready_builds"] = json!(129);
            response["builds_truncated"] = json!(true);
            response["build_page"] = json!({
                "remaining_ready_builds": remaining,
                "next": if remaining > 0 { Some("next-page") } else { None }
            });
            let capacity: CapacityAssessment = serde_json::from_value(response).unwrap();
            let valid = if after.is_some() {
                remaining <= 128
            } else {
                remaining == 128
            };
            assert_eq!(
                capacity.validate_against(&input).is_ok(),
                valid,
                "after={after:?}, remaining={remaining}"
            );
        }
    }
}

#[tokio::test]
async fn malformed_bootstrap_matrices_mappings_and_missing_sections_are_errors() {
    for (pointer, replacement, expected) in [
        (
            "/bootstrap",
            Value::Null,
            "bootstrap capacity assessment is missing",
        ),
        ("/build_page", Value::Null, "build page is missing"),
        ("/bootstrap/observed_at", json!("not-a-date"), "timestamp"),
        ("/bootstrap/profiles", json!([]), "profile count"),
        ("/bootstrap/tracked_workers", json!([]), "worker count"),
        (
            "/bootstrap/demands/0/profiles",
            json!([true]),
            "matrix dimensions",
        ),
        (
            "/bootstrap/demands/0/tracked_workers",
            json!([]),
            "matrix dimensions",
        ),
        (
            "/bootstrap/profiles/0/build_profile_index",
            json!(1),
            "mapping",
        ),
        (
            "/bootstrap/profiles/1/build_profile_index",
            json!(0),
            "build mapping",
        ),
        (
            "/bootstrap/profiles/1/profile/worker_id",
            json!(PROSPECTIVE),
            "identity",
        ),
        (
            "/bootstrap/profiles/1/profile/resources/ram_total_mb",
            json!(1),
            "resources",
        ),
        (
            "/bootstrap/profiles/1/profile/capabilities/build",
            json!(true),
            "capabilities",
        ),
        (
            "/bootstrap/tracked_workers/0/build_tracked_index",
            Value::Null,
            "mapping",
        ),
        (
            "/bootstrap/tracked_workers/2/build_tracked_index",
            json!(1),
            "build mapping",
        ),
        (
            "/bootstrap/tracked_workers/0/connected",
            json!(false),
            "connection",
        ),
        (
            "/bootstrap/tracked_workers/2/profile/name",
            json!("wrong"),
            "identity",
        ),
        (
            "/bootstrap/tracked_workers/2/profile/capabilities/cache",
            json!(true),
            "capabilities",
        ),
        (
            "/bootstrap/demands",
            Value::Array(vec![
                json!({
                    "kind": "evaluation", "available": false, "profiles": [true, false],
                    "tracked_workers": [true, true, false]
                });
                513
            ]),
            "too many bootstrap demands",
        ),
        (
            "/build_page/remaining_ready_builds",
            json!(1),
            "remaining count",
        ),
        ("/build_page/next", json!("unexpected"), "cursor"),
    ] {
        let server = MockServer::start().await;
        let mut response = bootstrap_response();
        *response.pointer_mut(pointer).unwrap() = replacement;
        respond(&server, response).await;
        let error = client(&server)
            .capacity_assessment("ops", &bootstrap_request())
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{pointer}: {error:#}");
    }
}

#[test]
fn bootstrap_request_bounds_identity_and_builder_validation_are_not_weakened() {
    let valid = bootstrap_request();
    valid.validate().unwrap();
    for (pointer, replacement, expected) in [
        (
            "/profiles/0/registration",
            Value::Null,
            "requires prospective builder registration",
        ),
        (
            "/profiles/0/registration/enable_build",
            json!(false),
            "registration does not enable builds",
        ),
        (
            "/bootstrap/profiles/0/profile/worker_id",
            json!(PROSPECTIVE),
            "duplicate bootstrap worker ID",
        ),
        (
            "/bootstrap/tracked_profiles/0/profile/worker_id",
            json!(ROLE),
            "duplicate bootstrap worker ID",
        ),
        (
            "/bootstrap/profiles/0/profile/worker_id",
            json!(Uuid::nil()),
            "worker ID is nil",
        ),
        (
            "/bootstrap/profiles/0/profile/name",
            json!(" "),
            "name is empty",
        ),
        (
            "/bootstrap/profiles/0/profile/capabilities",
            json!({
                "core": true, "federate": true, "cache": true, "build": false,
                "eval": false, "network": false, "secrets": true, "exec": true
            }),
            "no supported role",
        ),
        (
            "/bootstrap/profiles/0/profile/resources/max_concurrent_builds",
            json!(0),
            "no transfer slots",
        ),
        (
            "/bootstrap/tracked_profiles/0/profile/resources/max_concurrent_builds",
            json!(0),
            "no transfer slots",
        ),
        (
            "/bootstrap/profiles/0/profile/capabilities/build",
            json!(true),
            "CPU capacity",
        ),
        ("/build_page/after", json!(""), "cursor is empty"),
    ] {
        let mut input = serde_json::to_value(&valid).unwrap();
        let (parent, field) = pointer.rsplit_once('/').unwrap();
        input.pointer_mut(parent).unwrap()[field] = replacement;
        let input: CapacityAssessmentRequest = serde_json::from_value(input).unwrap();
        assert!(
            input.validate().unwrap_err().to_string().contains(expected),
            "{pointer}"
        );
    }
    let mut duplicate = valid.clone();
    let mut alternative = duplicate.profiles[0].clone();
    alternative.profile.name = "alternative".into();
    duplicate.profiles.push(alternative);
    assert!(
        duplicate
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate bootstrap worker ID")
    );
    duplicate.bootstrap = None;
    duplicate.validate().unwrap();
    let mut prospective = valid.clone();
    let ordinary = prospective.profiles[0].clone();
    prospective.profiles = (0..16)
        .map(|index| {
            let mut profile = ordinary.clone();
            profile.profile.name = format!("candidate-{index}");
            profile
        })
        .collect();
    assert!(
        prospective
            .validate()
            .unwrap_err()
            .to_string()
            .contains("combined bootstrap profiles")
    );
    let mut tracked = valid;
    let ordinary = tracked.tracked_profiles[0].clone();
    tracked.tracked_profiles = (0..256)
        .map(|index| {
            let mut profile = ordinary.clone();
            profile.profile.name = format!("tracked-{index}");
            profile.profile.worker_id = Uuid::from_u128(index + 100);
            profile
        })
        .collect();
    assert!(
        tracked
            .validate()
            .unwrap_err()
            .to_string()
            .contains("combined bootstrap tracked")
    );
}

#[tokio::test]
async fn connected_worker_without_scoped_live_index_is_not_a_starter() {
    let server = MockServer::start().await;
    let mut response = wire_response();
    response["tracked_workers"][0]["worker_index"] = Value::Null;
    response["tracked_workers"][1]["connected"] = json!(true);
    response["builds"][0]["tracked_workers"][1] = Value::Null;
    respond(&server, response).await;
    let assessment = client(&server)
        .capacity_assessment("ops", &request())
        .await
        .unwrap();
    assert!(
        assessment
            .tracked_workers
            .iter()
            .all(|worker| worker.connected)
    );
    assert!(
        assessment
            .tracked_workers
            .iter()
            .all(|worker| worker.worker_index.is_none())
    );
    assert!(
        assessment.builds[0]
            .tracked_workers
            .iter()
            .all(Option::is_none)
    );
}

#[tokio::test]
async fn missing_prediction_and_time_history_remain_explicit() {
    let server = MockServer::start().await;
    let mut response = wire_response();
    response["builds"][0]["workers"][0]["ram_fit"] = json!("fits_once_ram_is_released");
    let sizing = &mut response["builds"][0]["profiles"][0];
    sizing["prediction_fit"] = Value::Null;
    sizing["resources"]["predicted_ram_mb"] = Value::Null;
    sizing["resources"]["memory_assumption"] = json!("unmeasured");
    sizing["resources"]["own_samples"] = json!(0);
    sizing["resources"]["build_time_known"] = json!(false);
    sizing["resources"]["time"]["build_secs"] = json!(0.0);
    sizing["resources"]["time"]["cores"] = Value::Null;
    sizing["resources"]["time"]["fallbacks"] = json!(["build_history"]);
    respond(&server, response).await;
    let assessment = client(&server)
        .capacity_assessment("ops", &request())
        .await
        .unwrap();
    let sizing = assessment.builds[0].profiles[0].as_ref().unwrap();
    assert_eq!(sizing.prediction_fit, None);
    assert_eq!(sizing.resources.predicted_ram_mb, None);
    assert_eq!(sizing.resources.reserved_ram_mb, Some(3000));
    assert_eq!(
        sizing.resources.memory_assumption,
        MemoryAssumption::Unmeasured
    );
    assert!(!sizing.resources.build_time_known);
    assert_eq!(sizing.resources.time.build_secs, 0.0);
    assert_eq!(sizing.resources.time.cores, None);
    assert_eq!(sizing.resources.time.fallbacks, ["build_history"]);
    assert_eq!(
        assessment.builds[0].workers[0].as_ref().unwrap().ram_fit,
        Some(RamFit::FitsOnceRamIsReleased)
    );
}

#[tokio::test]
async fn duplicate_prospective_uuid_alternatives_are_not_duplicate_supply() {
    let server = MockServer::start().await;
    let mut input = request();
    input.profiles.push(profile("alternative", PROSPECTIVE));
    let mut response = wire_response();
    response["profiles"]
        .as_array_mut()
        .unwrap()
        .push(wire_profile("alternative", PROSPECTIVE));
    response["builds"][0]["profiles"]
        .as_array_mut()
        .unwrap()
        .push(wire_profile_assessment());
    respond(&server, response).await;
    let assessment = client(&server)
        .capacity_assessment("ops", &input)
        .await
        .unwrap();
    assert_eq!(assessment.profiles.len(), 2);
    assert_eq!(
        assessment.profiles[0].worker_id,
        assessment.profiles[1].worker_id
    );
}

#[tokio::test]
async fn malformed_dimensions_identities_and_correlations_are_errors() {
    let cases = [
        ("/builds/0/workers", json!([]), "matrix dimensions"),
        ("/builds/0/profiles", json!([]), "matrix dimensions"),
        ("/builds/0/tracked_workers", json!([]), "matrix dimensions"),
        ("/profiles", json!([]), "profile count"),
        ("/tracked_workers", json!([]), "worker count"),
        ("/profiles/0/name", json!("wrong"), "profile identity"),
        ("/profiles/0/worker_id", json!(STARTING), "profile identity"),
        (
            "/profiles/0/resources/cpu_count",
            json!(5),
            "profile resources",
        ),
        (
            "/profiles/0/capabilities/secrets",
            json!(false),
            "profile capabilities",
        ),
        (
            "/tracked_workers/1/profile/name",
            json!("wrong"),
            "profile identity",
        ),
        (
            "/tracked_workers/1/profile/worker_id",
            json!(LIVE),
            "profile identity",
        ),
        (
            "/tracked_workers/1/profile/capabilities/eval",
            json!(false),
            "profile capabilities",
        ),
        (
            "/tracked_workers/1/worker_index",
            json!(0),
            "unconnected tracked worker",
        ),
        (
            "/tracked_workers/0/worker_index",
            json!(1),
            "index is out of bounds",
        ),
        (
            "/builds/0/tracked_workers/0",
            wire_profile_assessment(),
            "starting capacity",
        ),
        ("/total_ready_builds", json!(0), "backlog is smaller"),
        ("/builds_truncated", json!(true), "truncation flag"),
        (
            "/workers/0/waiting_build/build_index",
            json!(1),
            "waiting build index",
        ),
        (
            "/workers/0/waiting_build/ram_credit_mb",
            json!(0),
            "RAM credit is zero",
        ),
        ("/workers/0/available_ram_mb", Value::Null, "no RAM budget"),
        (
            "/workers/0/available_ram_mb",
            json!(u64::MAX),
            "overflows the budget",
        ),
        ("/captured_at", json!("yesterday"), "timestamp"),
        ("/builds/0/ready_at", json!("not-a-date"), "timestamp"),
        ("/builds/0/workers/0/cores", json!(0), "zero cores"),
        ("/builds/0/workers/0/time/cores", json!(1), "time cores"),
    ];
    for (pointer, replacement, expected) in cases {
        let server = MockServer::start().await;
        let mut response = wire_response();
        *response.pointer_mut(pointer).unwrap() = replacement;
        respond(&server, response).await;
        let error = client(&server)
            .capacity_assessment("ops", &request())
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{pointer}: {error:#}");
    }
}

#[tokio::test]
async fn profile_core_limits_apply_to_prospective_and_starting_assessments() {
    for pointer in [
        "/builds/0/profiles/0/resources",
        "/builds/0/tracked_workers/1/resources",
    ] {
        let server = MockServer::start().await;
        let mut response = wire_response();
        let resources = response.pointer_mut(pointer).unwrap();
        resources["cores"] = json!(5);
        resources["time"]["cores"] = json!(5);
        respond(&server, response).await;
        let error = client(&server)
            .capacity_assessment("ops", &request())
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("exceeds profile cores"),
            "{pointer}: {error:#}"
        );
    }
}

#[tokio::test]
async fn duplicate_live_correlation_and_network_widening_are_errors() {
    for duplicate_index in [true, false] {
        let server = MockServer::start().await;
        let mut input = request();
        let mut response = wire_response();
        let expected = if duplicate_index {
            response["tracked_workers"][1]["connected"] = json!(true);
            response["tracked_workers"][1]["worker_index"] = json!(0);
            response["builds"][0]["tracked_workers"][1] = Value::Null;
            "duplicate tracked capacity worker index"
        } else {
            input.tracked_profiles[1].enable_network = false;
            "profile capabilities"
        };
        respond(&server, response).await;
        let error = client(&server)
            .capacity_assessment("ops", &input)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error:#}");
    }
}

#[tokio::test]
async fn oversize_response_samples_and_workers_are_errors() {
    for (field, count, expected) in [
        ("builds", 129, "too many capacity builds"),
        ("workers", 257, "too many capacity workers"),
    ] {
        let server = MockServer::start().await;
        let mut response = wire_response();
        let entry = response[field][0].clone();
        response[field] = Value::Array(vec![entry; count]);
        respond(&server, response).await;
        let error = client(&server)
            .capacity_assessment("ops", &request())
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error:#}");
    }
}

#[tokio::test]
async fn unsafe_time_values_and_unknown_ram_semantics_are_errors() {
    for (pointer, replacement) in [
        ("/builds/0/workers/0/time/download_secs", json!(-1.0)),
        ("/builds/0/profiles/0/resources/time/nar_bytes", json!(-1.0)),
        (
            "/builds/0/tracked_workers/1/resources/time/paths",
            json!(-1.0),
        ),
        ("/builds/0/workers/0/ram_fit", json!("future_fit")),
        ("/builds/0/profiles/0/prediction_fit", json!("future_fit")),
        (
            "/builds/0/workers/0/memory_assumption",
            json!("future_basis"),
        ),
    ] {
        let server = MockServer::start().await;
        let mut response = wire_response();
        *response.pointer_mut(pointer).unwrap() = replacement;
        respond(&server, response).await;
        assert!(
            client(&server)
                .capacity_assessment("ops", &request())
                .await
                .is_err(),
            "{pointer}"
        );
    }
    let mut response: CapacityAssessment = serde_json::from_value(wire_response()).unwrap();
    for value in [f64::NAN, f64::INFINITY, -1.0] {
        response.builds[0].workers[0]
            .as_mut()
            .unwrap()
            .time
            .build_secs = value;
        assert!(response.validate_against(&request()).is_err());
    }
    let time = &mut response.builds[0].workers[0].as_mut().unwrap().time;
    time.build_secs = f64::MAX;
    time.download_secs = f64::MAX;
    assert!(response.validate_against(&request()).is_err());
}

#[tokio::test]
async fn waiting_credit_requires_explicit_opt_in() {
    let server = MockServer::start().await;
    respond(&server, wire_response()).await;
    let mut input = request();
    input.include_waiting_builds = false;
    let error = client(&server)
        .capacity_assessment("ops", &input)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unsolicited waiting-build RAM credit"),
        "{error:#}"
    );
}

#[tokio::test]
async fn request_bounds_and_tracked_identity_errors_fail_before_http() {
    let mut oversize_profiles = request();
    oversize_profiles.profiles = (0..17)
        .map(|i| profile(&format!("candidate-{i}"), PROSPECTIVE))
        .collect();
    let mut oversize_tracked = request();
    oversize_tracked.tracked_profiles = (0..257)
        .map(|i| {
            profile(
                &format!("tracked-{i}"),
                &Uuid::from_u128(i + 100).to_string(),
            )
        })
        .collect();
    let mut oversize_projects = request();
    oversize_projects.projects = Some(vec![PROJECT.parse().unwrap(); 257]);
    let mut oversize_body = request();
    oversize_body.profiles[0].profile.name = "x".repeat(64 * 1024);
    let mut duplicate = request();
    duplicate.tracked_profiles[1].profile.worker_id =
        duplicate.tracked_profiles[0].profile.worker_id;
    let mut overlapping = request();
    overlapping.profiles[0].profile.worker_id = LIVE.parse().unwrap();
    let mut unnamed = request();
    unnamed.profiles[0].profile.name = " ".into();
    let mut no_cpu = request();
    no_cpu.profiles[0].profile.resources.cpu_count = 0;
    let mut no_ram = request();
    no_ram.profiles[0].profile.resources.ram_total_mb = 0;
    let mut no_slots = request();
    no_slots.profiles[0].profile.resources.max_concurrent_builds = 0;
    let mut disabled = request();
    disabled.profiles[0].enable_build = false;
    let mut no_build = request();
    no_build.profiles[0].profile.capabilities.build = false;
    for (input, expected) in [
        (oversize_profiles, "too many capacity profiles"),
        (oversize_tracked, "too many tracked capacity profiles"),
        (oversize_projects, "too many capacity projects"),
        (oversize_body, "64 KiB"),
        (duplicate, "duplicate tracked capacity worker ID"),
        (overlapping, "both prospective and tracked"),
        (unnamed, "name is empty"),
        (no_cpu, "CPU capacity"),
        (no_ram, "RAM budget"),
        (no_slots, "build slots"),
        (disabled, "registration does not enable builds"),
        (no_build, "profile does not permit builds"),
    ] {
        let server = MockServer::start().await;
        let error = client(&server)
            .capacity_assessment("ops", &input)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error:#}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn server_failures_and_malformed_success_bodies_do_not_become_empty_capacity() {
    for status in [400, 403, 404, 409, 503] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(ROUTE))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                "error": true, "message": "capacity unavailable"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let error = client(&server)
            .capacity_assessment("ops", &request())
            .await
            .unwrap_err();
        assert!(error.to_string().contains(ROUTE), "{error:#}");
        assert!(error.to_string().contains(&status.to_string()), "{error:#}");
        assert!(
            error.to_string().contains("capacity unavailable"),
            "{error:#}"
        );
    }
    for body in [
        json!({ "error": false, "message": {} }),
        json!({ "error": false }),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(ROUTE))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        assert!(
            client(&server)
                .capacity_assessment("ops", &request())
                .await
                .is_err()
        );
    }
}

#[test]
fn timestamp_parser_preserves_naive_and_offset_timestamp_instants() {
    for value in ["2026-11-11T22:00:00", "2026-11-11T22:00:00.123456789"] {
        assert!(parse_timestamp(value).is_ok(), "{value}");
    }
    let expected = parse_timestamp("2026-11-11T22:00:00.123456789").unwrap();
    for value in [
        "2026-11-11T22:00:00.123456789Z",
        "2026-11-12T03:30:00.123456789+05:30",
        "2026-11-11T18:00:00.123456789-04:00",
    ] {
        assert_eq!(
            parse_timestamp(value).unwrap().unix_timestamp_nanos(),
            expected.unix_timestamp_nanos(),
            "{value}"
        );
    }
}

#[test]
fn timestamp_parser_rejects_bad_dates_and_offsets_without_naive_fallback() {
    for value in [
        "2026-02-30T22:00:00",
        "2026-02-30T22:00:00Z",
        "2026-11-11T22:00:00+99:00",
        "2026-11-11T22:00:00-99:00",
        "2026-11-11T22:00:00+05:60",
        "2026-11-11T22:00:00-04:99",
        "2026-11-11T22:00:00+05:",
        "2026-11-11T22:00:00+",
        "2026-11-11T22:00:00+bogus",
        "2026-11-11T22:00:00Zjunk",
    ] {
        assert!(parse_timestamp(value).is_err(), "{value}");
    }
}
