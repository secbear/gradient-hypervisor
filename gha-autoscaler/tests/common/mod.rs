#![allow(dead_code)]

use gradient_gha_autoscaler::config::Connection;
use std::path::PathBuf;
use url::Url;

pub fn connection(name: &str, repository: &str, gradient_url: &str) -> Connection {
    Connection {
        name: name.into(),
        gradient_url: Url::parse(gradient_url).unwrap(),
        team: "ops".into(),
        api_key_file: PathBuf::from("/k"),
        github_token_file: PathBuf::from("/g"),
        repository: repository.into(),
        allowed_kinds: vec!["ubuntu-latest".into(), "ubuntu-24.04-arm".into()],
        projects: None,
        max_launches: 4,
        max_launches_per_kind: 2,
        worker_flake: Some("github:wavelens/gradient/v2.0.0#gradient".into()),
        worker_substituters: vec!["https://cache.example.com".into()],
        worker_trusted_public_keys: vec!["cache.example.com-1:abc=".into()],
        idle_timeout: 120,
        max_lifetime: 21600,
    }
}
