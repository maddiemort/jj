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

use clap_complete::ArgValueCandidates;
use clap_complete::ArgValueCompleter;
use indexmap::IndexSet;
use itertools::Itertools as _;
use jj_lib::copies::CopyRecords;
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::merge_commit_trees;
use tracing::instrument;

use crate::cli_util::print_unmatched_explicit_paths;
use crate::cli_util::short_commit_hash;
use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::command_error::user_error_with_hint;
use crate::command_error::CommandError;
use crate::complete;
use crate::diff_util::get_copy_records;
use crate::diff_util::DiffFormatArgs;
use crate::ui::Ui;

/// Compare file contents between two revisions
///
/// With the `-r` option, which is the default, shows the changes compared to
/// the parent revision. If there are several parent revisions (i.e., the given
/// revision is a merge), then they will be merged and the changes from the
/// result to the given revision will be shown.
///
/// With the `--from` and/or `--to` options, shows the difference from/to the
/// given revisions. If either is left out, it defaults to the working-copy
/// commit. For example, `jj diff --from main` shows the changes from "main"
/// (perhaps a bookmark name) to the working-copy commit.
#[derive(clap::Args, Clone, Debug)]
#[command(mut_arg("ignore_all_space", |a| a.short('w')))]
#[command(mut_arg("ignore_space_change", |a| a.short('b')))]
pub(crate) struct DiffArgs {
    /// Show changes in these revisions
    ///
    /// If there are multiple revisions, then then total diff for all of them
    /// will be shown. For example, if you have a linear chain of revisions
    /// A..D, then `jj diff -r B::D` equals `jj diff --from A --to D`. Multiple
    /// heads and/or roots are supported, but gaps in the revset are not
    /// supported (e.g. `jj diff -r 'A|C'` in a linear chain A..C).
    ///
    /// If a revision is a merge commit, this shows changes *from* the
    /// automatic merge of the contents of all of its parents *to* the contents
    /// of the revision itself.
    #[arg(
        long,
        short,
        value_name = "REVSETS",
        alias = "revision",
        add = ArgValueCandidates::new(complete::all_revisions)
    )]
    revisions: Option<Vec<RevisionArg>>,
    /// Show changes from this revision
    #[arg(
        long,
        short,
        conflicts_with = "revisions",
        value_name = "REVSET",
        add = ArgValueCandidates::new(complete::all_revisions)
    )]
    from: Option<RevisionArg>,
    /// Show changes to this revision
    #[arg(
        long,
        short,
        conflicts_with = "revisions",
        value_name = "REVSET",
        add = ArgValueCandidates::new(complete::all_revisions)
    )]
    to: Option<RevisionArg>,
    /// Restrict the diff to these paths
    #[arg(
        value_name = "FILESETS",
        value_hint = clap::ValueHint::AnyPath,
        add = ArgValueCompleter::new(complete::modified_revision_or_range_files),
    )]
    paths: Vec<String>,
    #[command(flatten)]
    format: DiffFormatArgs,
}

#[instrument(skip_all)]
pub(crate) fn cmd_diff(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &DiffArgs,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui)?;
    let repo = workspace_command.repo();
    let fileset_expression = workspace_command.parse_file_patterns(ui, &args.paths)?;
    let matcher = fileset_expression.to_matcher();

    let from_tree;
    let to_tree;
    let mut copy_records = CopyRecords::default();
    if args.from.is_some() || args.to.is_some() {
        let resolve_revision = |r: &Option<RevisionArg>| {
            workspace_command.resolve_single_rev(ui, r.as_ref().unwrap_or(&RevisionArg::AT))
        };
        let from = resolve_revision(&args.from)?;
        let to = resolve_revision(&args.to)?;
        from_tree = from.tree()?;
        to_tree = to.tree()?;

        let records = get_copy_records(repo.store(), from.id(), to.id(), &matcher)?;
        copy_records.add_records(records)?;
    } else {
        let revision_args = args
            .revisions
            .as_deref()
            .unwrap_or(std::slice::from_ref(&RevisionArg::AT));
        let revisions_evaluator = workspace_command.parse_union_revsets(ui, revision_args)?;
        let target_expression = revisions_evaluator.expression();
        let mut gaps_revset = workspace_command
            .attach_revset_evaluator(ui, target_expression.connected().minus(target_expression))?
            .evaluate_to_commit_ids()?;
        if let Some(commit_id) = gaps_revset.next() {
            return Err(user_error_with_hint(
                "Cannot diff revsets with gaps in.",
                format!(
                    "Revision {} would need to be in the set.",
                    short_commit_hash(&commit_id?)
                ),
            ));
        }
        let heads: Vec<_> = workspace_command
            .attach_revset_evaluator(ui, target_expression.heads())?
            .evaluate_to_commits()?
            .try_collect()?;
        let roots: Vec<_> = workspace_command
            .attach_revset_evaluator(ui, target_expression.roots())?
            .evaluate_to_commits()?
            .try_collect()?;

        // Collect parents outside of revset to preserve parent order
        let parents: IndexSet<_> = roots.iter().flat_map(|c| c.parents()).try_collect()?;
        let parents = parents.into_iter().collect_vec();
        from_tree = merge_commit_trees(repo.as_ref(), &parents)?;
        to_tree = merge_commit_trees(repo.as_ref(), &heads)?;

        for p in &parents {
            for to in &heads {
                let records = get_copy_records(repo.store(), p.id(), to.id(), &matcher)?;
                copy_records.add_records(records)?;
            }
        }
    }

    let diff_renderer = workspace_command.diff_renderer_for(&args.format)?;
    ui.request_pager();
    diff_renderer.show_diff(
        ui,
        ui.stdout_formatter().as_mut(),
        &from_tree,
        &to_tree,
        &matcher,
        &copy_records,
        ui.term_width(),
    )?;
    print_unmatched_explicit_paths(
        ui,
        &workspace_command,
        &fileset_expression,
        [&from_tree, &to_tree],
    )?;
    Ok(())
}
