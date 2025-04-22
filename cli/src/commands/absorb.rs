// Copyright 2024 The Jujutsu Authors
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
use jj_lib::absorb::absorb_hunks;
use jj_lib::absorb::split_hunks_to_trees;
use jj_lib::absorb::AbsorbSource;
use jj_lib::matchers::EverythingMatcher;
use pollster::FutureExt as _;
use tracing::instrument;

use crate::cli_util::print_updated_commits;
use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::command_error::CommandError;
use crate::complete;
use crate::diff_util::DiffFormat;
use crate::ui::Ui;

/// Move changes from a revision into the stack of mutable revisions
///
/// This command splits changes in the source revision and moves each change to
/// the closest mutable ancestor where the corresponding lines were modified
/// last. If the destination revision cannot be determined unambiguously, the
/// change will be left in the source revision.
///
/// The source revision will be abandoned if all changes are absorbed into the
/// destination revisions, and if the source revision has no description.
///
/// The modification made by `jj absorb` can be reviewed by `jj op show -p`.
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct AbsorbArgs {
    /// Source revision to absorb from
    #[arg(
        long, short,
        default_value = "@",
        value_name = "REVSET",
        add = ArgValueCandidates::new(complete::mutable_revisions),
    )]
    from: RevisionArg,
    /// Destination revisions to absorb into
    ///
    /// Only ancestors of the source revision will be considered.
    #[arg(
        long, short = 't', visible_alias = "to",
        default_value = "mutable()",
        value_name = "REVSETS",
        add = ArgValueCandidates::new(complete::mutable_revisions),
    )]
    into: Vec<RevisionArg>,
    /// Move only changes to these paths (instead of all paths)
    #[arg(value_name = "FILESETS", value_hint = clap::ValueHint::AnyPath)]
    paths: Vec<String>,
}

#[instrument(skip_all)]
pub(crate) fn cmd_absorb(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &AbsorbArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui)?;

    let source_commit = workspace_command.resolve_single_rev(ui, &args.from)?;
    let destinations = workspace_command
        .parse_union_revsets(ui, &args.into)?
        .resolve()?;

    let matcher = workspace_command
        .parse_file_patterns(ui, &args.paths)?
        .to_matcher();

    let repo = workspace_command.repo().as_ref();
    let source = AbsorbSource::from_commit(repo, source_commit)?;
    let selected_trees = split_hunks_to_trees(repo, &source, &destinations, &matcher).block_on()?;

    let path_converter = workspace_command.path_converter();
    for (path, reason) in selected_trees.skipped_paths {
        let ui_path = path_converter.format_file_path(&path);
        writeln!(ui.warning_default(), "Skipping {ui_path}: {reason}")?;
    }

    workspace_command.check_rewritable(ui, selected_trees.target_commits.keys())?;

    let mut tx = workspace_command.start_transaction();
    let stats = absorb_hunks(tx.repo_mut(), &source, selected_trees.target_commits)?;

    if let Some(mut formatter) = ui.status_formatter() {
        if !stats.rewritten_destinations.is_empty() {
            writeln!(
                formatter,
                "Absorbed changes into {} revisions:",
                stats.rewritten_destinations.len()
            )?;
            print_updated_commits(
                formatter.as_mut(),
                &tx.commit_summary_template(ui)?,
                stats.rewritten_destinations.iter().rev(),
            )?;
        }
        if stats.num_rebased > 0 {
            writeln!(
                formatter,
                "Rebased {} descendant commits.",
                stats.num_rebased
            )?;
        }
    }

    tx.finish(
        ui,
        format!(
            "absorb changes into {} commits",
            stats.rewritten_destinations.len()
        ),
    )?;

    if let Some(mut formatter) = ui.status_formatter() {
        if let Some(commit) = &stats.rewritten_source {
            let repo = workspace_command.repo().as_ref();
            if !commit.is_empty(repo)? {
                writeln!(formatter, "Remaining changes:")?;
                let diff_renderer = workspace_command.diff_renderer(vec![DiffFormat::Summary]);
                let matcher = &EverythingMatcher; // also print excluded paths
                let width = ui.term_width();
                diff_renderer.show_patch(ui, formatter.as_mut(), commit, matcher, width)?;
            }
        }
    }
    Ok(())
}
