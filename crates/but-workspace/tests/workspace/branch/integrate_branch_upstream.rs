use anyhow::Result;
use but_testsupport::visualize_commit_graph_all;
use but_workspace::branch::integrate_branch_upstream::{
    InteractiveIntegrationStep, get_initial_integration_steps_for_branch,
};

use crate::utils::{read_only_in_memory_scenario, read_only_in_memory_scenario_named};

#[test]
fn errors_when_branch_has_no_tracking_branch() -> Result<()> {
    let repo = read_only_in_memory_scenario("merge-with-two-branches-line-offset")
        .expect("fixture repo should be available");

    insta::assert_snapshot!(visualize_commit_graph_all(&repo)?, @r"
    *   2a6d103 (HEAD -> merge) Merge branch 'A' into merge
    |\  
    | * 7f389ed (A) add 10 to the beginning
    * | 91ef6f6 (B) add 10 to the end
    |/  
    * ff045ef (main) init
    ");

    let err = get_initial_integration_steps_for_branch(r("refs/heads/A"), &repo)
        .expect_err("branch without tracking must fail");

    assert!(
        err.to_string().contains("has no tracking branch"),
        "unexpected error: {err:#}"
    );

    Ok(())
}

#[test]
fn partitions_diverged_branch_into_local_then_remote() -> Result<()> {
    let repo = read_only_in_memory_scenario_named("with-remotes-no-workspace", "remote-diverged")?;

    insta::assert_snapshot!(visualize_commit_graph_all(&repo)?, @"
    * 1a265a4 (HEAD -> A) local change in A
    | * 89cc2d3 (origin/A) change in A
    |/  
    * d79bba9 new file in A
    * c166d42 (origin/main, origin/HEAD, main) init-integration
    ");

    let steps = get_initial_integration_steps_for_branch(r("refs/heads/A"), &repo)?;

    let local_tip = repo.rev_parse_single("A")?.detach();
    let upstream_tip = repo.rev_parse_single("origin/A")?.detach();
    let step_ids = pick_step_ids(&steps);

    assert_eq!(
        step_ids,
        vec![local_tip, upstream_tip],
        "expected local-only first, then remote-only for diverged history"
    );
    Ok(())
}

#[test]
fn matches_rewritten_commit_by_change_id_and_keeps_order() -> Result<()> {
    let mut repo = read_only_in_memory_scenario_named(
        "journey03",
        "01-rewritten-local-commit-is-paired-with-remote",
    )?;
    configure_tracking_for_branch_a(&mut repo)?;
    insta::assert_snapshot!(visualize_commit_graph_all(&repo)?, @"
    * 0b1ed50 (HEAD -> gitbutler/workspace) GitButler Workspace Commit
    * e9c9d74 (A) A2
    * 550b6ac A1
    | * ad92cce (origin/A) A2
    | * e1f216e A1
    |/  
    * fafd9d0 (origin/main, main) init
    ");

    let steps = get_initial_integration_steps_for_branch(r("refs/heads/A"), &repo)?;

    let local_only = repo.rev_parse_single("A~1")?.detach();
    let remote_only = repo.rev_parse_single("origin/A~1")?.detach();
    let local_and_remote = repo.rev_parse_single("A")?.detach();
    let step_ids = pick_step_ids(&steps);

    assert_eq!(
        step_ids,
        vec![local_only, remote_only, local_and_remote],
        "expected order local-only, remote-only, local-and-remote"
    );
    Ok(())
}

fn configure_tracking_for_branch_a(repo: &mut gix::Repository) -> Result<()> {
    let mut cfg = repo.config_snapshot_mut();
    cfg.set_raw_value(
        "remote.origin.fetch",
        gix::bstr::BStr::new(b"+refs/heads/*:refs/remotes/origin/*"),
    )?;
    cfg.set_raw_value("remote.origin.url", gix::bstr::BStr::new(b"."))?;
    cfg.set_raw_value("branch.A.remote", gix::bstr::BStr::new(b"origin"))?;
    cfg.set_raw_value("branch.A.merge", gix::bstr::BStr::new(b"refs/heads/A"))?;
    Ok(())
}

fn pick_step_ids(steps: &[InteractiveIntegrationStep]) -> Vec<gix::ObjectId> {
    steps
        .iter()
        .map(|step| match step {
            InteractiveIntegrationStep::Pick { commit_id, .. }
            | InteractiveIntegrationStep::Skip { commit_id, .. }
            | InteractiveIntegrationStep::PickUpstream { commit_id, .. } => *commit_id,
            InteractiveIntegrationStep::Squash { commits, .. } => {
                *commits.last().expect("squash step should contain commits")
            }
        })
        .collect()
}

fn r(name: &str) -> &gix::refs::FullNameRef {
    name.try_into().expect("statically known valid ref-name")
}
