use std::path::{Path, PathBuf};

use projects::{ProjectAdapter, RustProjectAdapter};

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn executable_name(binary_name: &str) -> String {
    format!("{binary_name}{}", std::env::consts::EXE_SUFFIX)
}

#[test]
fn discovers_single_crate_rust_project() {
    let adapter = RustProjectAdapter;
    let project = adapter
        .discover(&fixture_path("single_crate"))
        .expect("single crate fixture should discover successfully");

    assert_eq!(project.services.len(), 1);
    assert!(project.issues.is_empty());

    let service = &project.services[0];
    assert_eq!(service.package_name, "single-app");
    assert_eq!(service.binary_name, "single-app");
    assert_eq!(service.check.program, "cargo");
    assert_eq!(
        service.check.args,
        vec!["check", "--package", "single-app", "--bin", "single-app"]
    );
    assert_eq!(
        service.executable.debug_path,
        fixture_path("single_crate")
            .join("target")
            .join("debug")
            .join(executable_name("single-app"))
    );
    assert!(
        service
            .watch_paths
            .contains(&fixture_path("single_crate").join("src"))
    );
}

#[test]
fn discovers_workspace_members_and_honors_default_run() {
    let adapter = RustProjectAdapter;
    let project = adapter
        .discover(&fixture_path("workspace"))
        .expect("workspace fixture should discover successfully");

    assert_eq!(project.services.len(), 2);
    assert!(project.issues.is_empty());

    let service_ids = project
        .services
        .iter()
        .map(|service| service.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(service_ids, vec!["api", "worker-daemon"]);

    let worker = project
        .services
        .iter()
        .find(|service| service.package_name == "worker")
        .expect("worker service should exist");
    assert_eq!(worker.binary_name, "worker-daemon");
    assert_eq!(
        worker.build.args,
        vec!["build", "--package", "worker", "--bin", "worker-daemon"]
    );
    assert!(
        worker.watch_paths.contains(
            &fixture_path("workspace")
                .join("crates")
                .join("worker")
                .join("src")
        )
    );
}

#[test]
fn reports_recoverable_issue_for_ambiguous_binary_packages() {
    let adapter = RustProjectAdapter;
    let project = adapter
        .discover(&fixture_path("ambiguous_binary"))
        .expect("ambiguous fixture should still discover the project");

    assert!(project.services.is_empty());
    assert_eq!(project.issues.len(), 1);

    let issue = &project.issues[0];
    assert_eq!(issue.package_name.as_deref(), Some("ambiguous-app"));
    assert!(
        issue
            .message
            .contains("multiple runnable binaries were found")
    );
}
