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

use std::collections::HashSet;
use std::io::Write as _;

use clap_complete::ArgValueCandidates;
use itertools::Itertools as _;
use jj_lib::backend::CommitId;
use jj_lib::commit::CommitIteratorExt as _;
use jj_lib::object_id::ObjectId as _;
use jj_lib::refs::diff_named_ref_targets;
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::RewriteRefsOptions;
use tracing::instrument;

use crate::cli_util::has_tracked_remote_bookmarks;
use crate::cli_util::print_updated_commits;
use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::command_error::CommandError;
use crate::complete;
use crate::ui::Ui;

/// Abandon a revision
///
/// Abandon a revision, rebasing descendants onto its parent(s). The behavior is
/// similar to `jj restore --changes-in`; the difference is that `jj abandon`
/// gives you a new change, while `jj restore` updates the existing change.
///
/// If a working-copy commit gets abandoned, it will be given a new, empty
/// commit. This is true in general; it is not specific to this command.
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct AbandonArgs {
    /// The revision(s) to abandon (default: @)
    #[arg(
        value_name = "REVSETS",
        add = ArgValueCandidates::new(complete::mutable_revisions)
    )]
    revisions_pos: Vec<RevisionArg>,
    #[arg(
        short = 'r',
        hide = true,
        value_name = "REVSETS",
        add = ArgValueCandidates::new(complete::mutable_revisions)
    )]
    revisions_opt: Vec<RevisionArg>,
    // TODO: Remove in jj 0.34+
    #[arg(long, short, hide = true)]
    summary: bool,
    /// Do not delete bookmarks pointing to the revisions to abandon
    ///
    /// Bookmarks will be moved to the parent revisions instead.
    #[arg(long)]
    retain_bookmarks: bool,
    /// Do not modify the content of the children of the abandoned commits
    #[arg(long)]
    restore_descendants: bool,
}

#[instrument(skip_all)]
pub(crate) fn cmd_abandon(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &AbandonArgs,
) -> Result<(), CommandError> {
    if args.summary {
        writeln!(ui.warning_default(), "--summary is no longer supported.")?;
    }
    let mut workspace_command = command.workspace_helper(ui)?;
    let to_abandon: Vec<_> = if !args.revisions_pos.is_empty() || !args.revisions_opt.is_empty() {
        workspace_command
            .parse_union_revsets(ui, &[&*args.revisions_pos, &*args.revisions_opt].concat())?
    } else {
        workspace_command.parse_revset(ui, &RevisionArg::AT)?
    }
    .evaluate_to_commits()?
    .try_collect()?;
    if to_abandon.is_empty() {
        writeln!(ui.status(), "No revisions to abandon.")?;
        return Ok(());
    }
    let to_abandon_set: HashSet<&CommitId> = to_abandon.iter().ids().collect();
    workspace_command.check_rewritable(ui, to_abandon_set.iter().copied())?;

    let mut tx = workspace_command.start_transaction();
    let options = RewriteRefsOptions {
        delete_abandoned_bookmarks: !args.retain_bookmarks,
    };
    let mut num_rebased = 0;
    tx.repo_mut().transform_descendants_with_options(
        to_abandon_set.iter().copied().cloned().collect(),
        &options,
        |rewriter| {
            if to_abandon_set.contains(rewriter.old_commit().id()) {
                rewriter.abandon();
            } else if args.restore_descendants {
                rewriter.reparent().write()?;
                num_rebased += 1;
            } else {
                rewriter.rebase()?.write()?;
                num_rebased += 1;
            }
            Ok(())
        },
    )?;

    let deleted_bookmarks = diff_named_ref_targets(
        tx.base_repo().view().local_bookmarks(),
        tx.repo().view().local_bookmarks(),
    )
    .filter(|(_, (_old, new))| new.is_absent())
    .map(|(name, _)| name.to_owned())
    .collect_vec();

    if let Some(mut formatter) = ui.status_formatter() {
        writeln!(formatter, "Abandoned {} commits:", to_abandon.len())?;
        print_updated_commits(
            formatter.as_mut(),
            &tx.base_workspace_helper().commit_summary_template(ui)?,
            &to_abandon,
        )?;
        if !deleted_bookmarks.is_empty() {
            writeln!(
                formatter,
                "Deleted bookmarks: {}",
                deleted_bookmarks.iter().map(|n| n.as_symbol()).join(", ")
            )?;
        }
        if num_rebased > 0 {
            if args.restore_descendants {
                writeln!(
                    formatter,
                    "Rebased {num_rebased} descendant commits (while preserving their content) \
                     onto parents of abandoned commits",
                )?;
            } else {
                writeln!(
                    formatter,
                    "Rebased {num_rebased} descendant commits onto parents of abandoned commits",
                )?;
            }
        }
    }
    let transaction_description = if to_abandon.len() == 1 {
        format!("abandon commit {}", to_abandon[0].id().hex())
    } else {
        format!(
            "abandon commit {} and {} more",
            to_abandon[0].id().hex(),
            to_abandon.len() - 1
        )
    };
    tx.finish(ui, transaction_description)?;

    #[cfg(feature = "git")]
    if jj_lib::git::get_git_backend(workspace_command.repo().store()).is_ok() {
        let view = workspace_command.repo().view();
        if deleted_bookmarks
            .iter()
            .any(|name| has_tracked_remote_bookmarks(view, name))
        {
            writeln!(
                ui.warning_default(),
                "Remote bookmarks tracked by deleted bookmarks will be deleted on the next `jj \
                 git push`."
            )?;
        }
    }
    Ok(())
}
