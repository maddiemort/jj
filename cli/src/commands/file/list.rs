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
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::command_error::CommandError;
use crate::commit_templater::CommitTemplateLanguage;
use crate::commit_templater::TreeEntry;
use crate::complete;
use crate::ui::Ui;

/// List files in a revision
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct FileListArgs {
    /// The revision to list files in
    #[arg(
        long, short,
        default_value = "@",
        value_name = "REVSET",
        add = ArgValueCandidates::new(complete::all_revisions),
    )]
    revision: RevisionArg,

    /// Render each file entry using the given template
    ///
    /// All 0-argument methods of the [`TreeEntry` type] are available as
    /// keywords in the template expression. See [`jj help -k templates`] for
    /// more information.
    ///
    /// [`TreeEntry` type]:
    ///     https://jj-vcs.github.io/jj/latest/templates/#treeentry-type
    ///
    /// [`jj help -k templates`]:
    ///     https://jj-vcs.github.io/jj/latest/templates/
    #[arg(long, short = 'T')]
    template: Option<String>,

    /// Only list files matching these prefixes (instead of all files)
    #[arg(value_name = "FILESETS", value_hint = clap::ValueHint::AnyPath)]
    paths: Vec<String>,
}

#[instrument(skip_all)]
pub(crate) fn cmd_file_list(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &FileListArgs,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui)?;
    let commit = workspace_command.resolve_single_rev(ui, &args.revision)?;
    let tree = commit.tree()?;
    let matcher = workspace_command
        .parse_file_patterns(ui, &args.paths)?
        .to_matcher();
    let template = {
        let language = workspace_command.commit_template_language(ui)?;
        let text = match &args.template {
            Some(value) => value.to_owned(),
            None => workspace_command.settings().get("templates.file_list")?,
        };
        workspace_command
            .parse_template(
                ui,
                &language,
                &text,
                CommitTemplateLanguage::wrap_tree_entry,
            )?
            .labeled("file_list")
    };

    ui.request_pager();
    let mut formatter = ui.stdout_formatter();
    for (path, value) in tree.entries_matching(matcher.as_ref()) {
        let entry = TreeEntry {
            path,
            value: value?,
        };
        template.format(&entry, formatter.as_mut())?;
    }
    Ok(())
}
