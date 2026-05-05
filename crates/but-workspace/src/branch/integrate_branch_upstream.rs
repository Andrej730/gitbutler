use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use anyhow::{Context as _, Result, bail};
use bstr::BStr;
use but_core::{
    MergeCommitChangesOutcome, RefMetadata, RepositoryExt,
    commit::{add_conflict_markers, write_conflicted_tree},
};
use but_rebase::commit::DateMode;
use but_rebase::graph_rebase::{
    Editor, LookupStep, Selector, Step, SuccessfulRebase, ToSelector,
    mutate::{InsertSide, SegmentDelimiter, SelectorSet},
};
use gix::{prelude::ObjectIdExt as _, remote::Direction};

use crate::branch::segment_disconnect::determine_parent_selector;

/// The steps to be followed when integrating upstream changes into the local one.
#[derive(Debug)]
pub enum InteractiveIntegrationStep {
    /// Skip a given commit, effectively removing it from the branch.
    Skip {
        /// The SHA of the commit being ignored.
        commit_id: gix::ObjectId,
    },
    /// Pick a commit, keeping it in the branch.
    Pick {
        /// The SHA of the commit being picked.
        commit_id: gix::ObjectId,
    },
    /// Pick an upstream commit.
    PickUpstream {
        /// The SHA of the upstream commit, integrating it into the branch.
        commit_id: gix::ObjectId,
    },
    /// Squash the commits into one.
    Squash {
        /// The SHAs of the commits to squash.
        commits: Vec<gix::ObjectId>,
        /// Optionally, the message to use for the squash commit.
        message: Option<String>,
    },
}

impl fmt::Display for InteractiveIntegrationStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Skip { commit_id } => write!(f, "skip {commit_id}"),
            Self::Pick { commit_id } => write!(f, "pick {commit_id}"),
            Self::PickUpstream { commit_id } => write!(f, "pick-upstream {commit_id}"),
            Self::Squash { commits, message } => {
                write!(f, "squash")?;
                for commit_id in commits {
                    write!(f, " {commit_id}")?;
                }
                if let Some(message) = message {
                    write!(f, " | message={message:?}")?;
                }
                Ok(())
            }
        }
    }
}

/// The necessay information about the integration to be performed.
#[derive(Debug)]
pub struct InteractiveIntegration {
    /// The list of steps to follow in order to integrate the upstream changes into the local.
    pub steps: Vec<InteractiveIntegrationStep>,
    /// Merge base between the upstream and the local reference.
    pub merge_base: gix::ObjectId,
}

impl fmt::Display for InteractiveIntegration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "merge-base {}", self.merge_base)?;
        for step in &self.steps {
            writeln!(f, "{step}")?;
        }
        Ok(())
    }
}

/// Integrate the upstream changes in the order of the provided steps.
///
/// `editor` - The graph editor handle.
///
/// `ref_name` - The full reference name of the local branch we're integrating the upstream changes into.
///
/// `steps` - The vector of steps in the application order (parent to child) that describe the actions to perform
///   for the integration of the changes.
pub fn integrate_branch_with_steps<'ws, 'meta, M: RefMetadata>(
    mut editor: Editor<'ws, 'meta, M>,
    ref_name: &gix::refs::FullNameRef,
    integration: InteractiveIntegration,
) -> Result<SuccessfulRebase<'ws, 'meta, M>> {
    if integration.steps.is_empty() {
        bail!("Integration steps cannot be empty")
    }

    let delimiter_child = editor.select_reference(ref_name)?;
    let delimiter_parent = editor.select_commit(integration.merge_base)?;
    let segment_delimiter = SegmentDelimiter {
        child: delimiter_child,
        parent: delimiter_parent,
    };
    let children_to_disconnect = SelectorSet::All;
    let parents_to_disconnect = determine_parent_selector(&editor, delimiter_parent)?;

    let children_to_reconnect = selected_edges_from_set(
        &editor,
        segment_delimiter.child,
        &children_to_disconnect,
        EdgeSelection::Children,
    )?;
    let parents_to_reconnect = selected_edges_from_set(
        &editor,
        segment_delimiter.parent,
        &parents_to_disconnect,
        EdgeSelection::Parents,
    )?;

    editor.disconnect_segment_from(
        segment_delimiter,
        children_to_disconnect,
        parents_to_disconnect,
        true,
    )?;

    let new_segment_delimiter = integration_steps_into_segment_nodes(
        &mut editor,
        ref_name,
        integration.merge_base,
        &integration.steps,
    )?;

    connect_segment_to_edges(
        &mut editor,
        new_segment_delimiter,
        &children_to_reconnect,
        &parents_to_reconnect,
    )?;

    editor.rebase()
}

/// Builds and inserts the integrated commit chain under `ref_name` down to `merge_base`.
///
/// Returns the delimiter spanning from the reference node to the deepest inserted parent.
fn integration_steps_into_segment_nodes<M: RefMetadata>(
    editor: &mut Editor<'_, '_, M>,
    ref_name: &gix::refs::FullNameRef,
    merge_base: gix::ObjectId,
    steps: &[InteractiveIntegrationStep],
) -> Result<SegmentDelimiter<Selector, Selector>> {
    // Step 1: We interpret the integration steps and transform them into graph steps disconnected from their parents.
    // We disconnect them in order to be able to allow for reordering.
    let segment_steps = integration_steps_to_segment_steps_for_editor(editor, ref_name, steps)?;

    // Step 2. We build the new local branch out of the steps.
    // We start by disconnecting all the parents of the local branch reference step, as we will connect it to the new
    // set of commits.
    let child_most = editor.select_reference(ref_name)?;
    disconnect_selector_from_all_parents(editor, child_most)?;
    let mut parent_most = child_most;

    for step in segment_steps.into_iter().skip(1) {
        if let Some(existing_parent) =
            already_connected_parent_for_step(editor, parent_most, &step)?
        {
            parent_most = existing_parent;
            continue;
        }

        parent_most = editor.insert(parent_most, step, InsertSide::Below)?;
    }

    // Step 3: Append the merge base at the bottom
    let merge_base_selector = editor.select_commit(merge_base)?;
    let merge_base_step = editor.lookup_step(merge_base_selector)?;
    parent_most = if let Some(existing_parent) =
        already_connected_parent_for_step(editor, parent_most, &merge_base_step)?
    {
        existing_parent
    } else {
        editor.insert(parent_most, merge_base_step, InsertSide::Below)?
    };

    Ok(SegmentDelimiter {
        child: child_most,
        parent: parent_most,
    })
}

/// Returns an already-connected parent selector for `child` when `step` points to an
/// existing pick node in the graph.
fn already_connected_parent_for_step<M: RefMetadata>(
    editor: &Editor<'_, '_, M>,
    child: Selector,
    step: &Step,
) -> Result<Option<Selector>> {
    let Step::Pick(pick) = step else {
        return Ok(None);
    };

    let Some(existing_pick) = editor.try_select_commit(pick.id) else {
        return Ok(None);
    };

    let direct_parents = editor.direct_parents(child)?;
    Ok(direct_parents
        .into_iter()
        .find_map(|(parent, _)| (parent == existing_pick).then_some(parent)))
}

/// Converts user-provided integration steps into graph `Step`s in insertion order.
///
/// While translating, it applies graph detachments for skip instructions and prepares
/// picks for insertion under the target reference.
fn integration_steps_to_segment_steps_for_editor<M: RefMetadata>(
    editor: &mut Editor<'_, '_, M>,
    ref_name: &gix::refs::FullNameRef,
    steps: &[InteractiveIntegrationStep],
) -> Result<Vec<Step>> {
    let mut out = vec![Step::Reference {
        refname: ref_name.to_owned(),
    }];

    // Interactive steps are parent->child for execution. For graph connectivity
    // from reference(child-most) toward parents, we append in reverse.
    for step in steps.iter().rev() {
        match step {
            InteractiveIntegrationStep::Skip { .. } => {}
            InteractiveIntegrationStep::Pick { commit_id, .. } => {
                out.push(existing_or_new_pick_step(editor, *commit_id)?);
            }
            InteractiveIntegrationStep::PickUpstream { commit_id } => {
                out.push(existing_or_new_pick_step(editor, *commit_id)?);
            }
            InteractiveIntegrationStep::Squash { commits, message } => {
                out.push(squash_step_for_editor(editor, commits, message.as_deref())?);
            }
        }
    }

    Ok(out)
}

/// Creates a synthetic pick step whose patch represents squashing `commit_ids`
/// together into a single commit.
///
/// The resulting commit uses the octopus merge-base as its sole parent so the
/// picked delta represents the combined change from that base to the merged
/// tree.
fn squash_step_for_editor<M: RefMetadata>(
    editor: &mut Editor<'_, '_, M>,
    commit_ids: &[gix::ObjectId],
    message: Option<&str>,
) -> Result<Step> {
    if commit_ids.len() < 2 {
        bail!("Squash step must have at least two commits");
    }

    let maybe_selectors = commit_ids
        .iter()
        .map(|commit_id| editor.try_select_commit(*commit_id))
        .collect::<Vec<_>>();
    let ordered_commit_ids = if maybe_selectors.iter().all(Option::is_some) {
        let ordered_selectors = editor.order_commit_selectors_by_parentage(
            maybe_selectors
                .into_iter()
                .map(|selector| selector.expect("checked all selectors are present"))
                .collect::<Vec<_>>(),
        )?;
        ordered_selectors
            .iter()
            .map(|selector| {
                editor
                    .find_selectable_commit(*selector)
                    .map(|(_, commit)| commit.id)
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        commit_ids.to_vec()
    };

    let merge_outcome = editor.repo().merge_commit_changes_to_tree(
        ordered_commit_ids.clone(),
        editor.repo().merge_options_force_ours()?,
    )?;
    let squashed_parent = editor
        .repo()
        .merge_base_octopus(ordered_commit_ids.iter().copied())
        .context("failed to compute squash merge-base")?
        .detach();

    let tip_commit_id = *ordered_commit_ids
        .last()
        .expect("validated non-empty squash commit list");
    let mut squashed_commit = editor.find_commit(tip_commit_id)?;
    squashed_commit.inner.parents = vec![squashed_parent].into();
    let commit_message = message
        .map(|message| message.as_bytes().to_vec())
        .unwrap_or_else(|| Vec::from(squashed_commit.message.clone()));
    apply_merge_commit_changes_outcome(
        editor.repo(),
        &mut squashed_commit,
        merge_outcome,
        commit_message,
    )?;

    let new_commit_id =
        editor.new_commit_untracked(squashed_commit, DateMode::CommitterUpdateAuthorKeep)?;
    Ok(Step::new_pick(new_commit_id))
}

fn apply_merge_commit_changes_outcome(
    repo: &gix::Repository,
    commit: &mut but_core::CommitOwned,
    outcome: MergeCommitChangesOutcome,
    message: Vec<u8>,
) -> Result<()> {
    if let Some(conflict) = outcome.conflict {
        commit.tree = write_conflicted_tree(
            repo,
            outcome.tree_id,
            conflict.base_tree_id,
            conflict.ours_tree_id,
            conflict.theirs_tree_id,
            &conflict.conflict_entries,
        )?;
        commit.message = add_conflict_markers(BStr::new(&message));
    } else {
        commit.tree = outcome.tree_id;
        commit.message = message.into();
    }

    Ok(())
}

/// Produces a pick step for `commit_id`, reusing an existing selectable commit when present.
///
/// Existing commits are detached from selected parent edges first so they can be safely
/// reconnected into the new integration chain.
fn existing_or_new_pick_step<M: RefMetadata>(
    editor: &mut Editor<'_, '_, M>,
    commit_id: gix::ObjectId,
) -> Result<Step> {
    if let Some(existing) = editor.try_select_commit(commit_id) {
        let parents_to_disconnect = determine_parent_selector(editor, existing)?;
        editor.disconnect_segment_from(
            SegmentDelimiter {
                child: existing,
                parent: existing,
            },
            SelectorSet::All,
            parents_to_disconnect,
            true,
        )?;

        return editor.lookup_step(existing);
    }

    Ok(Step::new_pick(commit_id))
}

/// Disconnects all parent edges from a single selector without reconnecting them.
///
/// This is used to isolate reference and skipped-commit nodes before rebuilding
/// integration connectivity.
fn disconnect_selector_from_all_parents<M: RefMetadata>(
    editor: &mut Editor<'_, '_, M>,
    selector: Selector,
) -> Result<()> {
    editor.disconnect_segment_from(
        SegmentDelimiter {
            child: selector,
            parent: selector,
        },
        SelectorSet::None,
        SelectorSet::All,
        true,
    )?;

    Ok(())
}

#[derive(Clone, Copy)]
enum EdgeSelection {
    Children,
    Parents,
}

/// Resolves concrete direct edges selected by a `SelectorSet` for either children or
/// parents of `target`, preserving edge order metadata.
fn selected_edges_from_set<M: RefMetadata>(
    editor: &Editor<'_, '_, M>,
    target: Selector,
    selectors: &SelectorSet,
    edge_selection: EdgeSelection,
) -> Result<Vec<(Selector, usize)>> {
    let available = match edge_selection {
        EdgeSelection::Children => editor.direct_children(target)?,
        EdgeSelection::Parents => editor.direct_parents(target)?,
    };

    match selectors {
        SelectorSet::All => Ok(available),
        SelectorSet::None => Ok(Vec::new()),
        SelectorSet::Some(some_selectors) => {
            let mut selected = Vec::new();
            for selector in some_selectors.as_slice() {
                let selector = selector.to_selector(editor)?;
                let Some((_, order)) = available
                    .iter()
                    .find(|(candidate, _)| *candidate == selector)
                else {
                    bail!("Selected edge endpoint wasn't found among direct neighbors")
                };
                selected.push((selector, *order));
            }
            Ok(selected)
        }
    }
}

/// Reconnects a newly built segment delimiter to previously selected child and parent
/// edge endpoints, assigning fresh edge orders after current maxima.
fn connect_segment_to_edges<M: RefMetadata>(
    editor: &mut Editor<'_, '_, M>,
    delimiter: SegmentDelimiter<Selector, Selector>,
    children: &[(Selector, usize)],
    parents: &[(Selector, usize)],
) -> Result<()> {
    let max_child_weight = editor
        .direct_children(delimiter.child)?
        .into_iter()
        .map(|(_, order)| order)
        .max()
        .unwrap_or(0);

    for (child, order) in children {
        let next_order = max_child_weight + *order + 1;
        editor.add_edge(*child, delimiter.child, next_order)?;
    }

    let max_parent_weight = editor
        .direct_parents(delimiter.parent)?
        .into_iter()
        .map(|(_, order)| order)
        .max()
        .unwrap_or(0);

    for (parent, order) in parents {
        let next_order = max_parent_weight + *order + 1;
        editor.add_edge(delimiter.parent, *parent, next_order)?;
    }

    Ok(())
}

/// Get the initial integration steps for a branch.
///
/// This basically just lists the upstream and local commits in the display order (child to parent) and creates a `Pick` step for each.
/// The user can then modify this in the UI.
///
/// `ref_name` - The full reference name of the local branch to get the integration steps for.
///
/// `repo` - The repository handle.
///
/// Returns a [the information about how to integrate the changes](InteractiveIntegration).
pub fn get_initial_integration_steps_for_branch(
    ref_name: &gix::refs::FullNameRef,
    repo: &gix::Repository,
) -> Result<InteractiveIntegration> {
    let (local_commits, upstream_commits, merge_base) =
        get_commits_until_merge_base(ref_name, repo)?;

    let upstream_by_id = upstream_commits.iter().copied().collect::<HashSet<_>>();
    let mut upstream_by_change_id = HashMap::<String, gix::ObjectId>::new();
    for commit_id in &upstream_commits {
        let change_id = effective_change_id(repo, *commit_id)?;
        // Keep the first seen (closest to tip) upstream commit for stable matching.
        upstream_by_change_id.entry(change_id).or_insert(*commit_id);
    }

    let mut matched_upstream = HashSet::new();
    let mut local_only_commits = Vec::new();
    let mut local_and_remote_commits = Vec::new();
    for commit_id in local_commits {
        if upstream_by_id.contains(&commit_id) {
            matched_upstream.insert(commit_id);
            local_and_remote_commits.push(commit_id);
            continue;
        }

        let change_id = effective_change_id(repo, commit_id)?;
        if let Some(upstream_commit_id) = upstream_by_change_id.get(&change_id) {
            matched_upstream.insert(*upstream_commit_id);
            local_and_remote_commits.push(commit_id);
        } else {
            local_only_commits.push(commit_id);
        }
    }

    let remote_only_commits = upstream_commits
        .into_iter()
        .filter(|id| !matched_upstream.contains(id));

    let mut initial_steps = Vec::new();

    for commit in local_only_commits {
        initial_steps.push(InteractiveIntegrationStep::Pick { commit_id: commit });
    }

    for upstream_commit in remote_only_commits {
        initial_steps.push(InteractiveIntegrationStep::PickUpstream {
            commit_id: upstream_commit,
        });
    }

    for commit in local_and_remote_commits {
        initial_steps.push(InteractiveIntegrationStep::Pick { commit_id: commit });
    }

    Ok(InteractiveIntegration {
        steps: initial_steps,
        merge_base,
    })
}

/// Computes local and upstream commit lists (tip to merge-base, first-parent) together
/// with their merge base for a branch and its tracking branch.
fn get_commits_until_merge_base(
    ref_name: &gix::refs::FullNameRef,
    repo: &gix::Repository,
) -> Result<(Vec<gix::ObjectId>, Vec<gix::ObjectId>, gix::ObjectId), anyhow::Error> {
    let (local_tip, upstream_ref_name, upstream_tip) =
        get_branch_tips_and_upstream(ref_name, repo)?;
    let cache = repo.commit_graph_if_enabled()?;
    let mut graph = repo.revision_graph(cache.as_ref());
    let merge_base = repo
        .merge_base_with_graph(local_tip.attach(repo), upstream_tip.attach(repo), &mut graph)
        .map(|id| id.detach())
        .map_err(|_| {
            anyhow::anyhow!(
                "No merge-base found between '{ref_name}' and its tracking branch '{upstream_ref_name}'"
            )
        })?;
    let local_commits = branch_commits_until(repo, local_tip, merge_base)?;
    let upstream_commits = branch_commits_until(repo, upstream_tip, merge_base)?;
    Ok((local_commits, upstream_commits, merge_base))
}

/// Resolves local/upstream branch tips and tracking reference name for `ref_name`.
fn get_branch_tips_and_upstream<'a>(
    ref_name: &'a gix::refs::FullNameRef,
    repo: &'a gix::Repository,
) -> Result<
    (
        gix::ObjectId,
        std::borrow::Cow<'a, gix::refs::FullNameRef>,
        gix::ObjectId,
    ),
    anyhow::Error,
> {
    let mut local_branch = repo
        .find_reference(ref_name)
        .with_context(|| format!("Couldn't find local branch '{ref_name}'"))?;
    let local_tip = local_branch.peel_to_id()?.detach();
    let upstream_ref_name = repo
        .branch_remote_tracking_ref_name(ref_name, Direction::Fetch)
        .transpose()?
        .ok_or_else(|| anyhow::anyhow!("Branch '{ref_name}' has no tracking branch"))?;
    let mut upstream_branch = repo
        .find_reference(upstream_ref_name.as_ref())
        .with_context(|| {
            format!(
                "Couldn't find tracking branch '{upstream_ref_name}' for local branch '{ref_name}'"
            )
        })?;
    let upstream_tip = upstream_branch.peel_to_id()?.detach();
    Ok((local_tip, upstream_ref_name, upstream_tip))
}

/// Returns first-parent commits reachable from `tip` until (excluding) `merge_base`.
fn branch_commits_until(
    repo: &gix::Repository,
    tip: gix::ObjectId,
    merge_base: gix::ObjectId,
) -> Result<Vec<gix::ObjectId>> {
    let traversal = tip
        .attach(repo)
        .ancestors()
        .with_hidden(Some(merge_base))
        .first_parent_only()
        .all()?;

    let mut out = Vec::new();
    for info in traversal {
        out.push(info?.id);
    }
    Ok(out)
}

/// Returns the effective change-id string for a commit, used for rewritten-commit matching.
fn effective_change_id(repo: &gix::Repository, commit_id: gix::ObjectId) -> Result<String> {
    Ok(but_core::Commit::from_id(commit_id.attach(repo))?
        .change_id()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(hex: &str) -> gix::ObjectId {
        gix::ObjectId::from_hex(hex.as_bytes()).expect("valid object id")
    }

    #[test]
    fn interactive_integration_step_display_is_stable() {
        let parent = oid("1111111111111111111111111111111111111111");
        let squash_parent = oid("2222222222222222222222222222222222222222");
        let squash_child = oid("3333333333333333333333333333333333333333");
        let upstream_child = oid("4444444444444444444444444444444444444444");

        let skip = InteractiveIntegrationStep::Skip { commit_id: parent };
        assert_eq!(skip.to_string(), format!("skip {parent}"));

        let pick = InteractiveIntegrationStep::Pick { commit_id: parent };
        assert_eq!(pick.to_string(), format!("pick {parent}"));

        let pick_upstream = InteractiveIntegrationStep::PickUpstream {
            commit_id: upstream_child,
        };
        assert_eq!(
            pick_upstream.to_string(),
            format!("pick-upstream {upstream_child}")
        );

        let squash_without_message = InteractiveIntegrationStep::Squash {
            commits: vec![squash_parent, squash_child],
            message: None,
        };
        assert_eq!(
            squash_without_message.to_string(),
            format!("squash {squash_parent} {squash_child}")
        );

        let squash_with_message = InteractiveIntegrationStep::Squash {
            commits: vec![squash_parent, squash_child],
            message: Some("hello \"world\"".to_string()),
        };
        assert_eq!(
            squash_with_message.to_string(),
            format!("squash {squash_parent} {squash_child} | message=\"hello \\\"world\\\"\"")
        );
    }
}
