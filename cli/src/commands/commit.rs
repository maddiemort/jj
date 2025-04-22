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

use clap_complete::ArgValueCompleter;
use jj_lib::backend::Signature;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::command_error::user_error;
use crate::command_error::CommandError;
use crate::complete;
use crate::description_util::description_template;
use crate::description_util::edit_description;
use crate::description_util::join_message_paragraphs;
use crate::text_util::parse_author;
use crate::ui::Ui;

/// Update the description and create a new change on top.
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct CommitArgs {
    /// Interactively choose which changes to include in the first commit
    #[arg(short, long)]
    interactive: bool,
    /// Specify diff editor to be used (implies --interactive)
    #[arg(long, value_name = "NAME")]
    tool: Option<String>,
    /// The change description to use (don't open editor)
    #[arg(long = "message", short, value_name = "MESSAGE")]
    message_paragraphs: Vec<String>,
    /// Put these paths in the first commit
    #[arg(
        value_name = "FILESETS", 
        value_hint = clap::ValueHint::AnyPath,
        add = ArgValueCompleter::new(complete::modified_files),
    )]
    paths: Vec<String>,
    /// Reset the author to the configured user
    ///
    /// This resets the author name, email, and timestamp.
    ///
    /// You can use it in combination with the JJ_USER and JJ_EMAIL
    /// environment variables to set a different author:
    ///
    /// $ JJ_USER='Foo Bar' JJ_EMAIL=foo@bar.com jj commit --reset-author
    #[arg(long)]
    reset_author: bool,
    /// Set author to the provided string
    ///
    /// This changes author name and email while retaining author
    /// timestamp for non-discardable commits.
    #[arg(
        long,
        conflicts_with = "reset_author",
        value_parser = parse_author
    )]
    author: Option<(String, String)>,
}

#[instrument(skip_all)]
pub(crate) fn cmd_commit(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &CommitArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui)?;

    let commit_id = workspace_command
        .get_wc_commit_id()
        .ok_or_else(|| user_error("This command requires a working copy"))?;
    let commit = workspace_command.repo().store().get_commit(commit_id)?;
    let matcher = workspace_command
        .parse_file_patterns(ui, &args.paths)?
        .to_matcher();
    let advanceable_bookmarks = workspace_command.get_advanceable_bookmarks(commit.parent_ids())?;
    let diff_selector =
        workspace_command.diff_selector(ui, args.tool.as_deref(), args.interactive)?;
    let text_editor = workspace_command.text_editor()?;
    let mut tx = workspace_command.start_transaction();
    let base_tree = commit.parent_tree(tx.repo())?;
    let commit_summary = tx.format_commit_summary(ui, &commit)?;
    let format_instructions = || {
        format!(
            "\
You are splitting the working-copy commit: {commit_summary}

The diff initially shows all changes. Adjust the right side until it shows the
contents you want for the first commit. The remainder will be included in the
new working-copy commit.
",
        )
    };
    let tree_id = diff_selector.select(
        &base_tree,
        &commit.tree()?,
        matcher.as_ref(),
        format_instructions,
    )?;
    if !args.paths.is_empty() && tree_id == base_tree.id() {
        writeln!(
            ui.warning_default(),
            "The given paths do not match any file: {}",
            args.paths.join(" ")
        )?;
    }

    let mut commit_builder = tx.repo_mut().rewrite_commit(&commit).detach();
    commit_builder.set_tree_id(tree_id);
    if args.reset_author {
        commit_builder.set_author(commit_builder.committer().clone());
    }
    if let Some((name, email)) = args.author.clone() {
        let new_author = Signature {
            name,
            email,
            timestamp: commit_builder.author().timestamp,
        };
        commit_builder.set_author(new_author);
    }

    let description = if !args.message_paragraphs.is_empty() {
        join_message_paragraphs(&args.message_paragraphs)
    } else {
        if commit_builder.description().is_empty() {
            commit_builder.set_description(tx.settings().get_string("ui.default-description")?);
        }
        let temp_commit = commit_builder.write_hidden()?;
        let template = description_template(ui, &tx, "", &temp_commit)?;
        edit_description(&text_editor, &template)?
    };
    commit_builder.set_description(description);
    let new_commit = commit_builder.write(tx.repo_mut())?;

    let workspace_names = tx.repo().view().workspaces_for_wc_commit_id(commit.id());
    if !workspace_names.is_empty() {
        let new_wc_commit = tx
            .repo_mut()
            .new_commit(vec![new_commit.id().clone()], commit.tree_id().clone())
            .write()?;

        // Does nothing if there's no bookmarks to advance.
        tx.advance_bookmarks(advanceable_bookmarks, new_commit.id());

        for name in workspace_names {
            tx.repo_mut().edit(name, &new_wc_commit).unwrap();
        }
    }
    tx.finish(ui, format!("commit {}", commit.id().hex()))?;
    Ok(())
}
