use clap::{ArgAction, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "gtab",
    about = "Ghostty Tab Workspace Manager",
    disable_version_flag = true,
    subcommand_precedence_over_arg = true
)]
pub struct Cli {
    #[arg(short = 'v', long = "version", action = ArgAction::SetTrue)]
    pub version: bool,

    #[arg(long = "shell-cd", hide = true, action = ArgAction::SetTrue)]
    pub shell_cd: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,

    #[arg(value_name = "name")]
    pub workspace: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Launch the interactive TUI.
    Tui,
    /// Configure the default Ghostty-local shortcut.
    Init,
    /// Print shell integration so the shortcut runs gtab without typing it.
    ShellInit {
        #[arg(value_name = "shell")]
        shell: Option<String>,
    },
    /// List saved workspaces.
    List,
    /// Save the current Ghostty window as a workspace.
    Save { name: String },
    /// Edit a workspace AppleScript file in $EDITOR.
    Edit { name: String },
    /// Rename a workspace.
    Rename { old: String, new: String },
    /// Remove a workspace.
    Remove { name: String },
    /// Show or update settings.
    Set {
        key: Option<String>,
        value: Option<String>,
    },
}
