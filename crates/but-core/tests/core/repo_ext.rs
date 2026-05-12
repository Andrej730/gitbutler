use bstr::ByteSlice as _;
use but_core::{PlannedCommitChange, RepositoryExt, plan_commit_changes_for_merge_for_tests};
use but_testsupport::{read_only_in_memory_scenario, visualize_commit_graph_all};

#[test]
fn merge_commit_changes_to_tree_matches_clean_octopus_merge() -> anyhow::Result<()> {
    let repo = read_only_in_memory_scenario("octopus-merge-with-redundant-input")?;

    insta::assert_snapshot!(visualize_commit_graph_all(&repo)?, @r"
    *-.   a7dcd9f (HEAD -> main) octopus
    |\ \  
    | | * 2a5954a (right) right
    | |/  
    |/|   
    | * cbaa825 (left) left-2
    | * 777f2d5 left-1
    |/  
    * 66df43d base
    ");

    let left_1 = repo.rev_parse_single("left~1")?.detach();
    let left_2 = repo.rev_parse_single("left")?.detach();
    let right = repo.rev_parse_single("right")?.detach();
    let expected_tree = repo.rev_parse_single("main^{tree}")?.detach();

    let actual_tree = repo.merge_commit_changes_to_tree(
        vec![left_1, left_2, right],
        repo.merge_options_fail_fast()?.0,
    )?;

    assert_eq!(actual_tree.tree_id, expected_tree);
    assert!(actual_tree.conflict.is_none());
    Ok(())
}

#[test]
fn merge_commit_changes_to_tree_excludes_unselected_parent_changes() -> anyhow::Result<()> {
    let repo = read_only_in_memory_scenario("merge-commits-excludes-unselected-parent")?;

    insta::assert_snapshot!(visualize_commit_graph_all(&repo)?, @"
    * fa946b5 (HEAD -> C) C
    * 2eb5a0f (B) B
    | * cec649d (A) A
    |/  
    * b301433 (main) M
    ");

    let a_commit = repo.rev_parse_single("A")?.detach();
    let c_commit = repo.rev_parse_single("C")?.detach();
    let merged_tree = repo.merge_commit_changes_to_tree(
        vec![a_commit, c_commit],
        repo.merge_options_fail_fast()?.0,
    )?;

    let file_a = repo
        .find_tree(merged_tree.tree_id)?
        .lookup_entry_by_path("file-a")?
        .expect("file-a should be present")
        .object()?;
    assert_eq!(file_a.data.as_bstr(), "a\n");

    let file_c = repo
        .find_tree(merged_tree.tree_id)?
        .lookup_entry_by_path("file-c")?
        .expect("file-c should be present")
        .object()?;
    assert_eq!(file_c.data.as_bstr(), "c\n");

    assert!(
        repo.find_tree(merged_tree.tree_id)?
            .lookup_entry_by_path("file-b")?
            .is_none(),
        "file-b should not be pulled in from C's unselected parent"
    );
    assert!(merged_tree.conflict.is_none());

    Ok(())
}

#[test]
fn merge_commit_changes_to_tree_reports_conflicts() -> anyhow::Result<()> {
    let repo = read_only_in_memory_scenario("merge-with-two-branches-conflict")?;

    insta::assert_snapshot!(visualize_commit_graph_all(&repo)?, @"
    * 88d7acc (A) 10 to 20
    | * 47334c6 (HEAD -> merge, B) 20 to 30
    |/  
    * 15bcd1b (main) init
    ");

    let a_commit = repo.rev_parse_single("A")?.detach();
    let b_commit = repo.rev_parse_single("B")?.detach();
    let merged = repo
        .merge_commit_changes_to_tree(vec![a_commit, b_commit], repo.merge_options_force_ours()?)?;

    let conflict = merged
        .conflict
        .expect("conflicting merge should report conflict metadata");
    assert_eq!(
        conflict.base_tree_id,
        repo.rev_parse_single("main^{tree}")?.detach()
    );
    assert_eq!(
        conflict.ours_tree_id,
        repo.rev_parse_single("A^{tree}")?.detach()
    );
    assert_eq!(
        conflict.theirs_tree_id,
        repo.rev_parse_single("B^{tree}")?.detach()
    );
    assert!(conflict.conflict_entries.has_entries());

    let merged_file = repo
        .find_tree(merged.tree_id)?
        .lookup_entry_by_path("file")?
        .expect("merged file should be present")
        .object()?;
    assert_eq!(
        merged_file.data.as_bstr(),
        "10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n"
    );

    Ok(())
}

#[test]
fn merge_commit_changes_to_tree_stops_folding_after_first_conflict() -> anyhow::Result<()> {
    let repo = read_only_in_memory_scenario("merge-commit-changes-fail-fast-after-conflict")?;

    insta::assert_snapshot!(visualize_commit_graph_all(&repo)?, @"
    * ea9d91a (HEAD -> C) C
    | * a1163f7 (B) B
    |/  
    | * 332e45d (A) A
    |/  
    * 66df43d (main) base
    ");

    let a_commit = repo.rev_parse_single("A")?.detach();
    let b_commit = repo.rev_parse_single("B")?.detach();
    let c_commit = repo.rev_parse_single("C")?.detach();
    let merged = repo.merge_commit_changes_to_tree(
        vec![a_commit, b_commit, c_commit],
        repo.merge_options_force_ours()?,
    )?;

    assert!(
        merged.conflict.is_some(),
        "the A/B merge should report a conflict"
    );

    let tree = repo.find_tree(merged.tree_id)?;
    let shared = tree
        .lookup_entry_by_path("shared.txt")?
        .expect("shared.txt should be present")
        .object()?;
    assert_eq!(shared.data.as_bstr(), "A\n");

    assert!(
        tree.lookup_entry_by_path("file-c")?.is_none(),
        "fail-fast folding should stop before applying C"
    );

    Ok(())
}

#[test]
fn merge_commit_changes_to_tree_preserves_noncontiguous_selected_changes() -> anyhow::Result<()> {
    let repo =
        read_only_in_memory_scenario("merge-commits-preserve-noncontiguous-selected-changes")?;

    insta::assert_snapshot!(visualize_commit_graph_all(&repo)?, @"
    * bf7c931 (HEAD -> B) D
    * fa946b5 C
    * 2eb5a0f B
    | * cec649d (A) A
    |/  
    * b301433 (main) M
    ");

    let a_commit = repo.rev_parse_single("A")?.detach();
    let b_commit = repo.rev_parse_single("B~2")?.detach();
    let d_commit = repo.rev_parse_single("B")?.detach();
    let merged_tree = repo.merge_commit_changes_to_tree(
        vec![a_commit, b_commit, d_commit],
        repo.merge_options_fail_fast()?.0,
    )?;

    let tree = repo.find_tree(merged_tree.tree_id)?;

    let file_a = tree
        .lookup_entry_by_path("file-a")?
        .expect("file-a should be present")
        .object()?;
    assert_eq!(file_a.data.as_bstr(), "a\n");

    let file_b = tree
        .lookup_entry_by_path("file-b")?
        .expect("file-b should be present")
        .object()?;
    assert_eq!(file_b.data.as_bstr(), "b\n");

    let file_d = tree
        .lookup_entry_by_path("file-d")?
        .expect("file-d should be present")
        .object()?;
    assert_eq!(file_d.data.as_bstr(), "d\n");

    assert!(
        tree.lookup_entry_by_path("file-c")?.is_none(),
        "file-c should not be pulled in when only B and D are selected"
    );
    assert!(merged_tree.conflict.is_none());

    Ok(())
}

#[test]
fn plan_commit_changes_for_merge_preserves_noncontiguous_selected_changes() -> anyhow::Result<()> {
    let repo =
        read_only_in_memory_scenario("merge-commits-preserve-noncontiguous-selected-changes")?;

    let a_commit = repo.rev_parse_single("A")?.detach();
    let b_commit = repo.rev_parse_single("B~2")?.detach();
    let d_commit = repo.rev_parse_single("B")?.detach();

    let plan = plan_commit_changes_for_merge_for_tests(&repo, vec![a_commit, b_commit, d_commit])?;

    insta::assert_snapshot!(labeled_plan_entries(&repo, &plan), @"
    A <- M
    B <- M
    D <- C
    ");
    Ok(())
}

#[test]
fn plan_commit_changes_for_merge_fixture_graph() -> anyhow::Result<()> {
    let fixture = simplify_fixture()?;

    insta::assert_snapshot!(visualize_commit_graph_all(&fixture.repo)?, @"
    * 8259b01 (HEAD -> right) right-3
    * 0a63ea6 right-2
    * 26b0bd5 right-1
    | * feaa00d (left) left-3
    | * 07bba81 left-2
    | * 4b6a0f2 left-1
    |/  
    | * f1b6511 (main) main-3
    | * 6bbd9db main-2
    | * 257ee22 main-1
    |/  
    * 6dbc49d base
    ");

    Ok(())
}

#[test]
fn plan_commit_changes_for_merge_collapses_contiguous_selected_chain() -> anyhow::Result<()> {
    let fixture = simplify_fixture()?;

    let plan = plan_commit_changes_for_merge_for_tests(
        &fixture.repo,
        vec![fixture.left_1, fixture.left_2, fixture.left_3],
    )?;

    insta::assert_snapshot!(labeled_plan_entries(&fixture.repo, &plan), @"left-3 <- base");
    Ok(())
}

#[test]
fn plan_commit_changes_for_merge_preserves_unrelated_branch_tips() -> anyhow::Result<()> {
    let fixture = simplify_fixture()?;

    let plan = plan_commit_changes_for_merge_for_tests(
        &fixture.repo,
        vec![
            fixture.left_1,
            fixture.left_3,
            fixture.main_2,
            fixture.right_1,
            fixture.right_3,
        ],
    )?;

    insta::assert_snapshot!(labeled_plan_entries(&fixture.repo, &plan), @"
    left-1 <- base
    left-3 <- left-2
    main-2 <- main-1
    right-1 <- base
    right-3 <- right-2
    ");
    Ok(())
}

#[test]
fn plan_commit_changes_for_merge_deduplicates_and_keeps_order_of_survivors() -> anyhow::Result<()> {
    let fixture = simplify_fixture()?;

    let plan = plan_commit_changes_for_merge_for_tests(
        &fixture.repo,
        vec![
            fixture.main_3,
            fixture.left_2,
            fixture.main_2,
            fixture.left_2,
            fixture.right_3,
            fixture.left_3,
            fixture.right_1,
        ],
    )?;

    insta::assert_snapshot!(labeled_plan_entries(&fixture.repo, &plan), @"
    main-3 <- main-1
    right-3 <- right-2
    left-3 <- left-1
    right-1 <- base
    ");
    Ok(())
}

struct SimplifyFixture {
    repo: gix::Repository,
    main_2: gix::ObjectId,
    main_3: gix::ObjectId,
    left_1: gix::ObjectId,
    left_2: gix::ObjectId,
    left_3: gix::ObjectId,
    right_1: gix::ObjectId,
    right_3: gix::ObjectId,
}

fn simplify_fixture() -> anyhow::Result<SimplifyFixture> {
    let repo = read_only_in_memory_scenario("three-branches-three-commits")?;

    let main_2 = repo.rev_parse_single("main~1")?.detach();
    let main_3 = repo.rev_parse_single("main")?.detach();
    let left_1 = repo.rev_parse_single("left~2")?.detach();
    let left_2 = repo.rev_parse_single("left~1")?.detach();
    let left_3 = repo.rev_parse_single("left")?.detach();
    let right_1 = repo.rev_parse_single("right~2")?.detach();
    let right_3 = repo.rev_parse_single("right")?.detach();

    Ok(SimplifyFixture {
        repo,
        main_2,
        main_3,
        left_1,
        left_2,
        left_3,
        right_1,
        right_3,
    })
}

fn labeled_plan_entries(repo: &gix::Repository, plan: &[PlannedCommitChange]) -> String {
    plan.iter()
        .map(|entry| {
            let commit =
                label_commit_by_subject(repo, entry.commit_id).unwrap_or_else(|| "unknown".into());
            let base =
                label_tree_by_subject(repo, entry.base_tree_id).unwrap_or_else(|| "unknown".into());
            format!("{commit} <- {base}")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn label_commit_by_subject(repo: &gix::Repository, commit_id: gix::ObjectId) -> Option<String> {
    let commit = repo.find_commit(commit_id).ok()?;
    commit_subject(commit.message_raw().ok()?)
}

fn label_tree_by_subject(repo: &gix::Repository, tree_id: gix::ObjectId) -> Option<String> {
    if tree_id == gix::ObjectId::empty_tree(repo.object_hash()) {
        return Some("empty".into());
    }

    [
        "main", "main~1", "main~2", "main~3", "left", "left~1", "left~2", "left~3", "right",
        "right~1", "right~2", "right~3", "A", "B", "B~1", "B~2", "C",
    ]
    .into_iter()
    .find_map(|spec| {
        let commit_id = repo.rev_parse_single(spec).ok()?.detach();
        let commit = repo.find_commit(commit_id).ok()?;
        let commit_tree = commit.tree_id().ok()?.detach();
        if commit_tree == tree_id {
            commit_subject(commit.message_raw().ok()?)
        } else {
            None
        }
    })
}

fn commit_subject(message: &[u8]) -> Option<String> {
    let subject = std::str::from_utf8(message).ok()?.lines().next()?.trim();
    Some(subject.to_string())
}
