use std::fs;

const CI_WORKFLOW: &str = ".github/workflows/ci.yml";

fn ci_workflow() -> String {
    fs::read_to_string(CI_WORKFLOW).expect("CI workflow should be readable")
}

#[test]
fn github_coverage_action_uploads_cobertura_report() {
    let workflow = ci_workflow();

    for expected in [
        "permissions:\n      contents: read\n      code-quality: write",
        "uses: taiki-e/install-action@91534edaf9fd796a162759d80d49cdff574bff2c # cargo-llvm-cov",
        "cargo llvm-cov --workspace --cobertura --output-path coverage.xml",
        "uses: actions/upload-code-coverage@abb5995db9e0199b0e2bb9dbd136fce4cb1ec4d3 # v1",
        "file: coverage.xml",
        "language: Rust",
        "label: code-coverage/cargo-llvm-cov",
        "fail-on-error: false",
        "github.event_name != 'pull_request' || github.event.pull_request.head.repo.full_name == github.repository",
    ] {
        assert!(
            workflow.contains(expected),
            "missing `{expected}` in {CI_WORKFLOW}"
        );
    }
}

#[test]
fn ci_still_runs_existing_rust_quality_gates() {
    let workflow = ci_workflow();

    for expected in ["cargo test", "cargo clippy", "cargo fmt --check"] {
        assert!(
            workflow.contains(expected),
            "missing `{expected}` in {CI_WORKFLOW}"
        );
    }
}
