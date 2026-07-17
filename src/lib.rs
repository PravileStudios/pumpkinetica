mod clipboard;
mod commands;
mod history;
mod parser;
mod paste;
mod selection;

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

use serde::{Deserialize, Serialize};

use pumpkin_plugin_api::commands::Command;
use pumpkin_plugin_api::events::{
    BlockBreakEvent, EventPriority, PlayerInteractEvent, PlayerLeaveEvent,
};
use pumpkin_plugin_api::{
    Context, Plugin, PluginMetadata, Result,
    command::CommandNode,
    command_wit::{ArgumentType, StringType},
    common::NamedColor,
    permission::{Permission, PermissionDefault, PermissionLevel},
    permissions, register_plugin,
    text::TextComponent,
    world,
};

use clipboard::Clipboard;
use commands::*;
use history::PlayerHistory;
use parser::PaletteEntry;
use selection::Selection;

// ── Plugin ──────────────────────────────────────────────────────────

const PLUGIN_NAME: &str = "pschematics";
pub(crate) const PLUGIN_VERSION: &str = "0.5.0";
const PREFIX: &str = "[PSchematics] ";

struct PSchematics;

register_plugin!(PSchematics);

pub(crate) static CONFIG: Mutex<Option<PluginConfig>> = Mutex::new(None);
pub(crate) static ACTIVE_PASTES: AtomicUsize = AtomicUsize::new(0);
pub(crate) static PLAYER_SELECTIONS: Mutex<Option<HashMap<String, Selection>>> = Mutex::new(None);
pub(crate) static PLAYER_CLIPBOARDS: Mutex<Option<HashMap<String, Clipboard>>> = Mutex::new(None);
pub(crate) static PLAYER_HISTORIES: Mutex<Option<HashMap<String, PlayerHistory>>> =
    Mutex::new(None);

impl Plugin for PSchematics {
    fn new() -> Self {
        PSchematics
    }

    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: PLUGIN_NAME.into(),
            version: PLUGIN_VERSION.into(),
            authors: vec!["PumpkinMC".into()],
            description: "Load and paste .litematica and .schem schematics into the world".into(),
            dependencies: vec![],
            permissions: vec![
                permissions::FS_READ_DATA.into(),
                permissions::FS_WRITE_DATA.into(),
            ],
        }
    }

    fn on_load(&mut self, context: Context) -> Result<()> {
        let data_folder = context.get_data_folder();
        let schematics_dir = format!("{}/schematics", data_folder);
        let _ = std::fs::create_dir_all(&schematics_dir);
        let config = load_config(&data_folder);
        *CONFIG.lock().unwrap() = Some(config);

        *PLAYER_SELECTIONS.lock().unwrap() = Some(HashMap::new());
        *PLAYER_CLIPBOARDS.lock().unwrap() = Some(HashMap::new());
        *PLAYER_HISTORIES.lock().unwrap() = Some(HashMap::new());

        // Wand event handlers
        context.register_event_handler::<PlayerInteractEvent, _>(
            selection::WandInteractHandler,
            EventPriority::Normal,
            true,
        )?;
        context.register_event_handler::<BlockBreakEvent, _>(
            selection::WandBreakCancelHandler,
            EventPriority::Normal,
            true,
        )?;
        context.register_event_handler::<PlayerLeaveEvent, _>(
            selection::PlayerCleanupHandler,
            EventPriority::Normal,
            false,
        )?;

        // ── Command tree ────────────────────────────────────────────

        let file_arg = CommandNode::argument("file", &ArgumentType::String(StringType::SingleWord))
            .execute(LoadHandler {
                schematics_dir: schematics_dir.clone(),
            });
        let load_node = CommandNode::literal("load");
        load_node.then(file_arg);

        let paste_here_node =
            CommandNode::literal("here").execute(PasteHandler { at_feet: true });
        let paste_node = CommandNode::literal("paste").execute(PasteHandler { at_feet: false });
        paste_node.then(paste_here_node);

        let list_node = CommandNode::literal("list").execute(ListHandler {
            schematics_dir: schematics_dir.clone(),
        });

        let info_node = CommandNode::literal("info").execute(InfoHandler);
        let status_node = CommandNode::literal("status").execute(StatusHandler);
        let reload_node = CommandNode::literal("reload").execute(ReloadHandler { data_folder });
        let help_node = CommandNode::literal("help").execute(HelpHandler);

        let pos1_node = CommandNode::literal("pos1").execute(Pos1Handler);
        let pos2_node = CommandNode::literal("pos2").execute(Pos2Handler);
        let wand_node = CommandNode::literal("wand").execute(WandHandler);
        let copy_node = CommandNode::literal("copy").execute(CopyHandler);

        let save_arg =
            CommandNode::argument("name", &ArgumentType::String(StringType::SingleWord))
                .execute(SaveHandler { schematics_dir });
        let save_node = CommandNode::literal("save");
        save_node.then(save_arg);

        let rotate_arg =
            CommandNode::argument("degrees", &ArgumentType::String(StringType::SingleWord))
                .execute(RotateHandler);
        let rotate_node = CommandNode::literal("rotate");
        rotate_node.then(rotate_arg);

        let flip_arg =
            CommandNode::argument("axis", &ArgumentType::String(StringType::SingleWord))
                .execute(FlipHandler);
        let flip_node = CommandNode::literal("flip");
        flip_node.then(flip_arg);

        let undo_node = CommandNode::literal("undo").execute(UndoHandler);
        let redo_node = CommandNode::literal("redo").execute(RedoHandler);

        let replace_to =
            CommandNode::argument("to", &ArgumentType::String(StringType::SingleWord))
                .execute(ReplaceHandler);
        let replace_from =
            CommandNode::argument("from", &ArgumentType::String(StringType::SingleWord));
        replace_from.then(replace_to);
        let replace_node = CommandNode::literal("replace");
        replace_node.then(replace_from);

        let set_arg = CommandNode::argument("block", &ArgumentType::String(StringType::SingleWord))
            .execute(SetHandler);
        let set_node = CommandNode::literal("set");
        set_node.then(set_arg);

        let cmd = Command::new(
            &["schematic".into(), "schem".into()],
            "Load and paste .litematica and .schem schematics",
        );
        let cmd = cmd.execute(HelpHandler);
        cmd.then(load_node);
        cmd.then(paste_node);
        cmd.then(list_node);
        cmd.then(info_node);
        cmd.then(status_node);
        cmd.then(reload_node);
        cmd.then(help_node);
        cmd.then(pos1_node);
        cmd.then(pos2_node);
        cmd.then(wand_node);
        cmd.then(copy_node);
        cmd.then(save_node);
        cmd.then(rotate_node);
        cmd.then(flip_node);
        cmd.then(undo_node);
        cmd.then(redo_node);
        cmd.then(replace_node);
        cmd.then(set_node);

        let _ = context.register_permission(&Permission {
            node: format!("{PLUGIN_NAME}:command.schematic"),
            description: "Allows use of /schematic commands".into(),
            default: PermissionDefault::Op(PermissionLevel::Two),
            children: vec![],
        });

        context.register_command(cmd, "command.schematic");

        Ok(())
    }
}

// ── Config ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct PluginConfig {
    #[serde(default = "default_fallback_block")]
    pub fallback_block: String,
    #[serde(default = "default_blocks_per_tick")]
    pub blocks_per_tick: usize,
    #[serde(default = "default_max_concurrent_pastes")]
    pub max_concurrent_pastes: usize,
    #[serde(default = "default_wand_item")]
    pub wand_item: String,
    #[serde(default = "default_max_undo_history")]
    pub max_undo_history: usize,
    #[serde(default = "default_max_selection_volume")]
    pub max_selection_volume: u64,
    /// Largest op (in blocks) that still records undo/redo snapshots. Bigger
    /// ops run without undo to bound memory (peak ≈ this × max_undo_history).
    #[serde(default = "default_max_undo_volume")]
    pub max_undo_volume: u64,
}

fn default_fallback_block() -> String {
    "minecraft:cobblestone".into()
}
fn default_blocks_per_tick() -> usize {
    4096
}
fn default_max_concurrent_pastes() -> usize {
    4
}
fn default_wand_item() -> String {
    "minecraft:wooden_axe".into()
}
fn default_max_undo_history() -> usize {
    20
}
fn default_max_selection_volume() -> u64 {
    10_000_000
}
fn default_max_undo_volume() -> u64 {
    1_000_000
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            fallback_block: default_fallback_block(),
            blocks_per_tick: default_blocks_per_tick(),
            max_concurrent_pastes: default_max_concurrent_pastes(),
            wand_item: default_wand_item(),
            max_undo_history: default_max_undo_history(),
            max_selection_volume: default_max_selection_volume(),
            max_undo_volume: default_max_undo_volume(),
        }
    }
}

fn write_default_config(path: &str) -> PluginConfig {
    let config = PluginConfig::default();
    if let Ok(json) = serde_json::to_string_pretty(&config) {
        let _ = std::fs::write(path, json);
    }
    config
}

pub(crate) fn load_config(data_folder: &str) -> PluginConfig {
    let path = format!("{}/config.json", data_folder);
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents)
            .unwrap_or_else(|_| write_default_config(&path)),
        Err(_) => write_default_config(&path),
    }
}

pub(crate) fn get_config() -> PluginConfig {
    CONFIG.lock().unwrap().clone().unwrap_or_default()
}

// ── Messaging ───────────────────────────────────────────────────────

fn msg(text: &str, color: NamedColor) -> TextComponent {
    let prefix = TextComponent::text(PREFIX);
    prefix.color_named(NamedColor::Gold);
    let body = TextComponent::text(text);
    body.color_named(color);
    prefix.add_child(body);
    prefix
}

pub(crate) fn msg_error(text: &str) -> TextComponent {
    msg(text, NamedColor::Red)
}
pub(crate) fn msg_success(text: &str) -> TextComponent {
    msg(text, NamedColor::Green)
}
pub(crate) fn msg_info(text: &str) -> TextComponent {
    msg(text, NamedColor::Aqua)
}
pub(crate) fn msg_warn(text: &str) -> TextComponent {
    msg(text, NamedColor::Yellow)
}

pub(crate) fn normalize_item_name(name: &str) -> &str {
    name.strip_prefix("minecraft:").unwrap_or(name)
}

// ── Block Resolution ────────────────────────────────────────────────

pub(crate) fn resolve_fallback_block(config: &PluginConfig) -> Option<u16> {
    if config.fallback_block == "skip" {
        return None;
    }
    world::resolve_block_state(&config.fallback_block, &[])
}

pub(crate) fn resolve_palette(palette: &[PaletteEntry], fallback: Option<u16>) -> Vec<Option<u16>> {
    palette
        .iter()
        .map(|entry| {
            if entry.name == "minecraft:air" || entry.name == "air" {
                return Some(0);
            }
            let props: Vec<(String, String)> = entry
                .properties
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            world::resolve_block_state(&entry.name, &props).or(fallback)
        })
        .collect()
}
