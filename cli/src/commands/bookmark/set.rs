// Copyright 2020-2023 The Jujutsu Authors
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
use itertools::Itertools as _;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefNameBuf;

use super::is_fast_forward;
use crate::cli_util::has_tracked_remote_bookmarks;
use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::command_error::user_error_with_hint;
use crate::command_error::CommandError;
use crate::complete;
use crate::revset_util;
use crate::ui::Ui;

/// Create or update a bookmark to point to a certain commit
#[derive(clap::Args, Clone, Debug)]
pub struct BookmarkSetArgs {
    // TODO(#5374): Make required in jj 0.32+
    /// The bookmark's target revision
    //
    // Currently target revision defaults to the working copy if not specified, but in the near
    // future it will be required to explicitly specify it.
    #[arg(
        long, short,
        visible_alias = "to",
        value_name = "REVSET",
        add = ArgValueCandidates::new(complete::all_revisions),
    )]
    revision: Option<RevisionArg>,

    /// Allow moving the bookmark backwards or sideways
    #[arg(long, short = 'B')]
    allow_backwards: bool,

    /// The bookmarks to update
    #[arg(
        required = true,
        value_parser = revset_util::parse_bookmark_name,
        add = ArgValueCandidates::new(complete::local_bookmarks),
    )]
    names: Vec<RefNameBuf>,
}

pub fn cmd_bookmark_set(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &BookmarkSetArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui)?;
    if args.revision.is_none() {
        writeln!(
            ui.warning_default(),
            "Target revision was not specified, defaulting to the working copy (--revision=@). In \
             the near future it will be required to explicitly specify target revision."
        )?;
    }
    let target_commit = workspace_command
        .resolve_single_rev(ui, args.revision.as_ref().unwrap_or(&RevisionArg::AT))?;
    let repo = workspace_command.repo().as_ref();
    let bookmark_names = &args.names;
    let mut new_bookmark_count = 0;
    let mut moved_bookmark_count = 0;
    for name in bookmark_names {
        let old_target = repo.view().get_local_bookmark(name);
        // If a bookmark is absent locally but is still tracking remote bookmarks,
        // we are resurrecting the local bookmark, not "creating" a new bookmark.
        if old_target.is_absent() && !has_tracked_remote_bookmarks(repo.view(), name) {
            new_bookmark_count += 1;
        } else if old_target.as_normal() != Some(target_commit.id()) {
            moved_bookmark_count += 1;
        }
        if !args.allow_backwards && !is_fast_forward(repo, old_target, target_commit.id()) {
            return Err(user_error_with_hint(
                format!(
                    "Refusing to move bookmark backwards or sideways: {name}",
                    name = name.as_symbol()
                ),
                "Use --allow-backwards to allow it.",
            ));
        }
    }

    let mut tx = workspace_command.start_transaction();
    for bookmark_name in bookmark_names {
        tx.repo_mut().set_local_bookmark_target(
            bookmark_name,
            RefTarget::normal(target_commit.id().clone()),
        );
    }

    if let Some(mut formatter) = ui.status_formatter() {
        if new_bookmark_count > 0 {
            write!(
                formatter,
                "Created {new_bookmark_count} bookmarks pointing to "
            )?;
            tx.write_commit_summary(ui, formatter.as_mut(), &target_commit)??;
            writeln!(formatter)?;
        }
        if moved_bookmark_count > 0 {
            write!(formatter, "Moved {moved_bookmark_count} bookmarks to ")?;
            tx.write_commit_summary(ui, formatter.as_mut(), &target_commit)??;
            writeln!(formatter)?;
        }
    }
    if bookmark_names.len() > 1 && args.revision.is_none() {
        writeln!(ui.hint_default(), "Use -r to specify the target revision.")?;
    }

    tx.finish(
        ui,
        format!(
            "point bookmark {names} to commit {id}",
            names = bookmark_names.iter().map(|n| n.as_symbol()).join(", "),
            id = target_commit.id().hex()
        ),
    )?;
    Ok(())
}
