//! `/plugins` and `/skills`: open the extensions modal.
//!
//! These commands always open the tabbed modal.
//! All hook/plugin management (install, uninstall, trust, etc.) is done through the modal's UI; no subcommands are passed through to the shell.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};
use crate::views::extensions_modal::ExtensionsTab;
use xai_grok_telemetry::events::ExtensionsModalTrigger;

/// Open the hooks/plugins modal on the Plugins tab.
pub struct PluginsCommand;

impl SlashCommand for PluginsCommand {
    slash_meta! {
        name: "plugins",
        aliases: ["plugin"],
        description: "View plugins",
        usage: "/plugins",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenExtensionsModal {
            tab: ExtensionsTab::Plugins,
            trigger: ExtensionsModalTrigger::SlashCommand,
        })
    }
}

/// Open the hooks/plugins modal on the Skills tab.
pub struct SkillsCommand;

impl SlashCommand for SkillsCommand {
    slash_meta! {
        name: "skills",
        description: "View skills",
        usage: "/skills",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        CommandResult::Action(Action::OpenExtensionsModal {
            tab: ExtensionsTab::Skills,
            trigger: ExtensionsModalTrigger::SlashCommand,
        })
    }
}
