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

use std::collections::HashMap;
use std::io;
use std::io::Read as _;

use clap_complete::ArgValueCandidates;
use itertools::Itertools as _;
use jj_lib::backend::Signature;
use jj_lib::commit::CommitIteratorExt as _;
use jj_lib::object_id::ObjectId as _;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::command_error::user_error;
use crate::command_error::CommandError;
use crate::complete;
use crate::description_util::description_template;
use crate::description_util::edit_description;
use crate::description_util::edit_multiple_descriptions;
use crate::description_util::join_message_paragraphs;
use crate::description_util::ParsedBulkEditMessage;
use crate::text_util::parse_author;
use crate::ui::Ui;

/// Update the change description or other metadata
///
/// Starts an editor to let you edit the description of changes. The editor
/// will be $EDITOR, or `pico` if that's not defined (`Notepad` on Windows).
#[derive(clap::Args, Clone, Debug)]
#[command(visible_aliases = &["desc"])]
pub(crate) struct DescribeArgs {
    /// The revision(s) whose description to edit (default: @)
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
    /// The change description to use (don't open editor)
    ///
    /// If multiple revisions are specified, the same description will be used
    /// for all of them.
    #[arg(
        long = "message",
        short,
        value_name = "MESSAGE",
        conflicts_with = "stdin"
    )]
    message_paragraphs: Vec<String>,
    /// Read the change description from stdin
    ///
    /// If multiple revisions are specified, the same description will be used
    /// for all of them.
    #[arg(long)]
    stdin: bool,
    /// Don't open an editor
    ///
    /// This is mainly useful in combination with e.g. `--reset-author`.
    #[arg(long, conflicts_with = "edit")]
    no_edit: bool,
    /// Open an editor
    ///
    /// Forces an editor to open when using `--stdin` or `--message` to
    /// allow the message to be edited afterwards.
    #[arg(long)]
    edit: bool,
    /// Reset the author to the configured user
    ///
    /// This resets the author name, email, and timestamp.
    ///
    /// You can use it in combination with the JJ_USER and JJ_EMAIL
    /// environment variables to set a different author:
    ///
    /// $ JJ_USER='Foo Bar' JJ_EMAIL=foo@bar.com jj describe --reset-author
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
pub(crate) fn cmd_describe(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &DescribeArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui)?;
    let commits: Vec<_> = if !args.revisions_pos.is_empty() || !args.revisions_opt.is_empty() {
        workspace_command
            .parse_union_revsets(ui, &[&*args.revisions_pos, &*args.revisions_opt].concat())?
    } else {
        workspace_command.parse_revset(ui, &RevisionArg::AT)?
    }
    .evaluate_to_commits()?
    .try_collect()?; // in reverse topological order
    if commits.is_empty() {
        writeln!(ui.status(), "No revisions to describe.")?;
        return Ok(());
    }
    workspace_command.check_rewritable(commits.iter().ids())?;
    let text_editor = workspace_command.text_editor()?;

    let mut tx = workspace_command.start_transaction();
    let tx_description = if commits.len() == 1 {
        format!("describe commit {}", commits[0].id().hex())
    } else {
        format!(
            "describe commit {} and {} more",
            commits[0].id().hex(),
            commits.len() - 1
        )
    };

    let shared_description = if args.stdin {
        let mut buffer = String::new();
        io::stdin().read_to_string(&mut buffer)?;
        Some(buffer)
    } else if !args.message_paragraphs.is_empty() {
        Some(join_message_paragraphs(&args.message_paragraphs))
    } else {
        None
    };

    // edit and no_edit are conflicting arguments and therefore it should not
    // be possible for both to be true at the same time.
    assert!(!(args.edit && args.no_edit));
    let use_editor = args.edit || (shared_description.is_none() && !args.no_edit);

    let commit_descriptions: Vec<(_, _)> = if !use_editor {
        commits
            .iter()
            .map(|commit| {
                let new_description = shared_description
                    .as_deref()
                    .unwrap_or_else(|| commit.description());
                (commit, new_description.to_owned())
            })
            .collect()
    } else {
        let temp_commits: Vec<(_, _)> = commits
            .iter()
            // Edit descriptions in topological order
            .rev()
            .map(|commit| -> Result<_, CommandError> {
                let mut commit_builder = tx.repo_mut().rewrite_commit(commit).detach();
                if let Some(description) = &shared_description {
                    commit_builder.set_description(description);
                } else if commit_builder.description().is_empty() {
                    commit_builder
                        .set_description(tx.settings().get_string("ui.default-description")?);
                }
                if args.reset_author {
                    let new_author = commit_builder.committer().clone();
                    commit_builder.set_author(new_author);
                }
                if let Some((name, email)) = args.author.clone() {
                    let new_author = Signature {
                        name,
                        email,
                        timestamp: commit_builder.author().timestamp,
                    };
                    commit_builder.set_author(new_author);
                }
                let temp_commit = commit_builder.write_hidden()?;
                Ok((commit.id(), temp_commit))
            })
            .try_collect()?;

        if let [(_, temp_commit)] = &*temp_commits {
            let template = description_template(ui, &tx, "", temp_commit)?;
            let description = edit_description(&text_editor, &template)?;
            vec![(&commits[0], description)]
        } else {
            let ParsedBulkEditMessage {
                descriptions,
                missing,
                duplicates,
                unexpected,
            } = edit_multiple_descriptions(ui, &text_editor, &tx, &temp_commits)?;
            if !missing.is_empty() {
                return Err(user_error(format!(
                    "The description for the following commits were not found in the edited \
                     message: {}",
                    missing.join(", ")
                )));
            }
            if !duplicates.is_empty() {
                return Err(user_error(format!(
                    "The following commits were found in the edited message multiple times: {}",
                    duplicates.join(", ")
                )));
            }
            if !unexpected.is_empty() {
                return Err(user_error(format!(
                    "The following commits were not being edited, but were found in the edited \
                     message: {}",
                    unexpected.join(", ")
                )));
            }

            let commit_descriptions = commits
                .iter()
                .map(|commit| {
                    let description = descriptions.get(commit.id()).unwrap().to_owned();
                    (commit, description)
                })
                .collect();

            commit_descriptions
        }
    };

    // Filter out unchanged commits to avoid rebasing descendants in
    // `transform_descendants` below unnecessarily.
    let commit_descriptions: HashMap<_, _> = commit_descriptions
        .into_iter()
        .filter(|(commit, new_description)| {
            new_description != commit.description()
                || args.reset_author
                || args.author.as_ref().is_some_and(|(name, email)| {
                    name != &commit.author_raw().name || email != &commit.author_raw().email
                })
        })
        .map(|(commit, new_description)| (commit.id(), new_description))
        .collect();

    let mut num_described = 0;
    let mut num_reparented = 0;
    // Even though `MutRepo::rewrite_commit` and `MutRepo::rebase_descendants` can
    // handle rewriting of a commit even if it is a descendant of another commit
    // being rewritten, using `MutRepo::transform_descendants` prevents us from
    // rewriting the same commit multiple times, and adding additional entries
    // in the predecessor chain.
    tx.repo_mut().transform_descendants(
        commit_descriptions
            .keys()
            .map(|&id| id.clone())
            .collect_vec(),
        |rewriter| {
            let old_commit_id = rewriter.old_commit().id().clone();
            let mut commit_builder = rewriter.reparent();
            if let Some(description) = commit_descriptions.get(&old_commit_id) {
                commit_builder = commit_builder.set_description(description);
                if args.reset_author {
                    let new_author = commit_builder.committer().clone();
                    commit_builder = commit_builder.set_author(new_author);
                }
                if let Some((name, email)) = args.author.clone() {
                    let new_author = Signature {
                        name,
                        email,
                        timestamp: commit_builder.author().timestamp,
                    };
                    commit_builder = commit_builder.set_author(new_author);
                }
                num_described += 1;
            } else {
                num_reparented += 1;
            }
            commit_builder.write()?;
            Ok(())
        },
    )?;
    if num_described > 1 {
        writeln!(ui.status(), "Updated {num_described} commits")?;
    }
    if num_reparented > 0 {
        writeln!(ui.status(), "Rebased {num_reparented} descendant commits")?;
    }
    tx.finish(ui, tx_description)?;
    Ok(())
}
