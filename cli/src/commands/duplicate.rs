// Copyright 2020 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io::Write as _;

use clap_complete::ArgValueCandidates;
use itertools::Itertools as _;
use jj_lib::backend::CommitId;
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::duplicate_commits;
use jj_lib::rewrite::duplicate_commits_onto_parents;
use jj_lib::rewrite::DuplicateCommitsStats;
use tracing::instrument;

use crate::cli_util::compute_commit_location;
use crate::cli_util::short_commit_hash;
use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::command_error::user_error;
use crate::command_error::CommandError;
use crate::complete;
use crate::ui::Ui;

/// Create new changes with the same content as existing ones
///
/// When none of the `--destination`, `--insert-after`, or `--insert-before`
/// arguments are provided, commits will be duplicated onto their existing
/// parents or onto other newly duplicated commits.
///
/// When any of the `--destination`, `--insert-after`, or `--insert-before`
/// arguments are provided, the roots of the specified commits will be
/// duplicated onto the destination indicated by the arguments. Other specified
/// commits will be duplicated onto these newly duplicated commits. If the
/// `--insert-after` or `--insert-before` arguments are provided, the new
/// children indicated by the arguments will be rebased onto the heads of the
/// specified commits.
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct DuplicateArgs {
    /// The revision(s) to duplicate (default: @)
    #[arg(
        value_name = "REVSETS",
        add = ArgValueCandidates::new(complete::all_revisions)
    )]
    revisions_pos: Vec<RevisionArg>,
    #[arg(
        short = 'r',
        hide = true,
        value_name = "REVSETS",
        add = ArgValueCandidates::new(complete::all_revisions)
    )]
    revisions_opt: Vec<RevisionArg>,
    /// The revision(s) to duplicate onto (can be repeated to create a merge
    /// commit)
    #[arg(
        long,
        short,
        value_name = "REVSETS",
        add = ArgValueCandidates::new(complete::all_revisions)
    )]
    destination: Option<Vec<RevisionArg>>,
    /// The revision(s) to insert after (can be repeated to create a merge
    /// commit)
    #[arg(
        long,
        short = 'A',
        visible_alias = "after",
        conflicts_with = "destination",
        value_name = "REVSETS",
        add = ArgValueCandidates::new(complete::all_revisions),
    )]
    insert_after: Option<Vec<RevisionArg>>,
    /// The revision(s) to insert before (can be repeated to create a merge
    /// commit)
    #[arg(
        long,
        short = 'B',
        visible_alias = "before",
        conflicts_with = "destination",
        value_name = "REVSETS",
        add = ArgValueCandidates::new(complete::mutable_revisions)
    )]
    insert_before: Option<Vec<RevisionArg>>,
}

#[instrument(skip_all)]
pub(crate) fn cmd_duplicate(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &DuplicateArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui)?;
    let to_duplicate: Vec<CommitId> =
        if !args.revisions_pos.is_empty() || !args.revisions_opt.is_empty() {
            workspace_command
                .parse_union_revsets(ui, &[&*args.revisions_pos, &*args.revisions_opt].concat())?
        } else {
            workspace_command.parse_revset(ui, &RevisionArg::AT)?
        }
        .evaluate_to_commit_ids()?
        .try_collect()?; // in reverse topological order
    if to_duplicate.is_empty() {
        writeln!(ui.status(), "No revisions to duplicate.")?;
        return Ok(());
    }
    if to_duplicate.last() == Some(workspace_command.repo().store().root_commit_id()) {
        return Err(user_error("Cannot duplicate the root commit"));
    }

    let location = if args.destination.is_none()
        && args.insert_after.is_none()
        && args.insert_before.is_none()
    {
        None
    } else {
        Some(compute_commit_location(
            ui,
            &workspace_command,
            args.destination.as_deref(),
            args.insert_after.as_deref(),
            args.insert_before.as_deref(),
            "duplicated commits",
        )?)
    };

    let mut tx = workspace_command.start_transaction();

    if let Some((parent_commit_ids, children_commit_ids)) = &location {
        if !parent_commit_ids.is_empty() {
            for commit_id in &to_duplicate {
                for parent_commit_id in parent_commit_ids {
                    if tx.repo().index().is_ancestor(commit_id, parent_commit_id) {
                        writeln!(
                            ui.warning_default(),
                            "Duplicating commit {} as a descendant of itself",
                            short_commit_hash(commit_id)
                        )?;
                        break;
                    }
                }
            }

            for commit_id in &to_duplicate {
                for child_commit_id in children_commit_ids {
                    if tx.repo().index().is_ancestor(child_commit_id, commit_id) {
                        writeln!(
                            ui.warning_default(),
                            "Duplicating commit {} as an ancestor of itself",
                            short_commit_hash(commit_id)
                        )?;
                        break;
                    }
                }
            }
        }
    }
    let num_to_duplicate = to_duplicate.len();
    let DuplicateCommitsStats {
        duplicated_commits,
        num_rebased,
    } = if let Some((parent_commit_ids, children_commit_ids)) = location {
        duplicate_commits(
            tx.repo_mut(),
            &to_duplicate,
            &parent_commit_ids,
            &children_commit_ids,
        )?
    } else {
        duplicate_commits_onto_parents(tx.repo_mut(), &to_duplicate)?
    };

    if let Some(mut formatter) = ui.status_formatter() {
        for (old_id, new_commit) in &duplicated_commits {
            write!(formatter, "Duplicated {} as ", short_commit_hash(old_id))?;
            tx.write_commit_summary(ui, formatter.as_mut(), new_commit)??;
            writeln!(formatter)?;
        }
        if num_rebased > 0 {
            writeln!(
                ui.status(),
                "Rebased {num_rebased} commits onto duplicated commits"
            )?;
        }
    }
    tx.finish(ui, format!("duplicate {num_to_duplicate} commit(s)"))?;
    Ok(())
}
