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
use jj_lib::backend::TreeValue;
use jj_lib::merged_tree::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use tracing::instrument;

use crate::cli_util::print_unmatched_explicit_paths;
use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::command_error::user_error;
use crate::command_error::CommandError;
use crate::complete;
use crate::ui::Ui;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
enum ChmodMode {
    /// Make a path non-executable (alias: normal)
    // We use short names for enum values so that errors say that the possible values are `n, x`.
    #[value(name = "n", alias("normal"))]
    Normal,
    /// Make a path executable (alias: executable)
    #[value(name = "x", alias("executable"))]
    Executable,
}

/// Sets or removes the executable bit for paths in the repo
///
/// Unlike the POSIX `chmod`, `jj file chmod` also works on Windows, on
/// conflicted files, and on arbitrary revisions.
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct FileChmodArgs {
    mode: ChmodMode,
    /// The revision to update
    #[arg(
        long, short,
        default_value = "@",
        value_name = "REVSET",
        add = ArgValueCandidates::new(complete::mutable_revisions),
    )]
    revision: RevisionArg,
    /// Paths to change the executable bit for
    #[arg(
        required = true,
        value_name = "FILESETS",
        value_hint = clap::ValueHint::AnyPath,
        add = ArgValueCompleter::new(complete::all_revision_files),
    )]
    paths: Vec<String>,
}

#[instrument(skip_all)]
pub(crate) fn cmd_file_chmod(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &FileChmodArgs,
) -> Result<(), CommandError> {
    let executable_bit = match args.mode {
        ChmodMode::Executable => true,
        ChmodMode::Normal => false,
    };

    let mut workspace_command = command.workspace_helper(ui)?;
    let commit = workspace_command.resolve_single_rev(ui, &args.revision)?;
    workspace_command.check_rewritable(ui, [commit.id()])?;
    let tree = commit.tree()?;
    // TODO: No need to add special case for empty paths when switching to
    // parse_union_filesets(). paths = [] should be "none()" if supported.
    let fileset_expression = workspace_command.parse_file_patterns(ui, &args.paths)?;
    let matcher = fileset_expression.to_matcher();
    print_unmatched_explicit_paths(ui, &workspace_command, &fileset_expression, [&tree])?;

    let mut tx = workspace_command.start_transaction();
    let store = tree.store();
    let mut tree_builder = MergedTreeBuilder::new(commit.tree_id().clone());
    for (repo_path, result) in tree.entries_matching(matcher.as_ref()) {
        let mut tree_value = result?;
        let user_error_with_path = |msg: &str| {
            user_error(format!(
                "{msg} at '{}'.",
                tx.base_workspace_helper().format_file_path(&repo_path)
            ))
        };
        let all_files = tree_value
            .adds()
            .flatten()
            .all(|tree_value| matches!(tree_value, TreeValue::File { .. }));
        if !all_files {
            let message = if tree_value.is_resolved() {
                "Found neither a file nor a conflict"
            } else {
                "Some of the sides of the conflict are not files"
            };
            return Err(user_error_with_path(message));
        }
        for value in tree_value.iter_mut().flatten() {
            if let TreeValue::File { id: _, executable } = value {
                *executable = executable_bit;
            }
        }
        tree_builder.set_or_remove(repo_path, tree_value);
    }

    let new_tree_id = tree_builder.write_tree(store)?;
    tx.repo_mut()
        .rewrite_commit(&commit)
        .set_tree_id(new_tree_id)
        .write()?;
    tx.finish(
        ui,
        format!(
            "make paths {} in commit {}",
            if executable_bit {
                "executable"
            } else {
                "non-executable"
            },
            commit.id().hex(),
        ),
    )
}
