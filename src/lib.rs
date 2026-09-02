mod clipboard;
mod commands;
mod history;
mod ipc;
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

const PLUGIN_NAME: &str = "pumpkinetica";
pub(crate) const PLUGIN_VERSION: &str = "0.2.0";
const PREFIX: &str = "[Pumpkinetica] ";

struct Pumpkinetica;

register_plugin!(Pumpkinetica);

pub(crate) static CONFIG: Mutex<Option<PluginConfig>> = Mutex::new(None);
pub(crate) static SCHEMATICS_DIR: Mutex<Option<String>> = Mutex::new(None);
pub(crate) static ACTIVE_PASTES: AtomicUsize = AtomicUsize::new(0);
pub(crate) static PLAYER_SELECTIONS: Mutex<Option<HashMap<String, Selection>>> = Mutex::new(None);
pub(crate) static PLAYER_CLIPBOARDS: Mutex<Option<HashMap<String, Clipboard>>> = Mutex::new(None);
pub(crate) static PLAYER_HISTORIES: Mutex<Option<HashMap<String, PlayerHistory>>> =
    Mutex::new(None);

impl Plugin for Pumpkinetica {
    fn new() -> Self {
        Pumpkinetica
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
        *SCHEMATICS_DIR.lock().unwrap() = Some(schematics_dir.clone());

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

        let load_node = CommandNode::literal("load").then(
            CommandNode::argument("file", &ArgumentType::String(StringType::SingleWord))
                .execute(LoadHandler {
                    schematics_dir: schematics_dir.clone(),
                }),
        );

        let save_node = CommandNode::literal("save").then(
            CommandNode::argument("name", &ArgumentType::String(StringType::SingleWord))
                .execute(SaveHandler { schematics_dir: schematics_dir.clone() }),
        );

        let rotate_node = CommandNode::literal("rotate").then(
            CommandNode::argument("degrees", &ArgumentType::String(StringType::SingleWord))
                .execute(RotateHandler),
        );

        let flip_node = CommandNode::literal("flip").then(
            CommandNode::argument("axis", &ArgumentType::String(StringType::SingleWord))
                .execute(FlipHandler),
        );

        let replace_node = CommandNode::literal("replace").then(
            CommandNode::argument("from", &ArgumentType::String(StringType::SingleWord)).then(
                CommandNode::argument("to", &ArgumentType::String(StringType::SingleWord))
                    .execute(ReplaceHandler),
            ),
        );

        let set_node = CommandNode::literal("set").then(
            CommandNode::argument("block", &ArgumentType::String(StringType::SingleWord))
                .execute(SetHandler),
        );

        let cmd = Command::new(
            &["schematic".into(), "schem".into()],
            "Load and paste .litematica and .schem schematics",
        )
        .execute(HelpHandler)
        .then(load_node)
        .then(CommandNode::literal("paste").execute(PasteHandler))
        .then(CommandNode::literal("list").execute(ListHandler {
            schematics_dir: schematics_dir.clone(),
        }))
        .then(CommandNode::literal("info").execute(InfoHandler))
        .then(CommandNode::literal("status").execute(StatusHandler))
        .then(CommandNode::literal("reload").execute(ReloadHandler { data_folder }))
        .then(CommandNode::literal("help").execute(HelpHandler))
        .then(CommandNode::literal("pos1").execute(Pos1Handler))
        .then(CommandNode::literal("pos2").execute(Pos2Handler))
        .then(CommandNode::literal("wand").execute(WandHandler))
        .then(CommandNode::literal("copy").execute(CopyHandler))
        .then(save_node)
        .then(rotate_node)
        .then(flip_node)
        .then(CommandNode::literal("undo").execute(UndoHandler))
        .then(CommandNode::literal("redo").execute(RedoHandler))
        .then(replace_node)
        .then(set_node);

        let _ = context.register_permission(&Permission {
            node: format!("{PLUGIN_NAME}:command.schematic"),
            description: "Allows use of /schematic commands".into(),
            default: PermissionDefault::Op(PermissionLevel::Two),
            children: vec![],
        });

        context.register_command(cmd, "command.schematic");

        Ok(())
    }

    fn handle_ipc_message(&mut self, sender: String, message: Vec<u8>) -> Result<Vec<u8>, String> {
        ipc::dispatch(&sender, &message)
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
    /// When true, emit diagnostic log lines to the server console.
    #[serde(default = "default_debug")]
    pub debug: bool,
    /// When true, IPC callers may paste from absolute host paths (see
    /// `ipc_allowed_paste_dirs`). Off by default: without it, IPC paste is
    /// restricted to files inside the plugin's own schematics directory.
    #[serde(default = "default_ipc_allow_external_paths")]
    pub ipc_allow_external_paths: bool,
    /// Directories an IPC-supplied path must resolve inside for a paste to be
    /// accepted. Both the path and each root are canonicalized (symlinks and
    /// `..` collapsed) before the prefix check. Empty = deny all external paths.
    #[serde(default)]
    pub ipc_allowed_paste_dirs: Vec<String>,
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
fn default_debug() -> bool {
    false
}
fn default_ipc_allow_external_paths() -> bool {
    false
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
            debug: default_debug(),
            ipc_allow_external_paths: default_ipc_allow_external_paths(),
            ipc_allowed_paste_dirs: Vec::new(),
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
        // A malformed file falls back to defaults in memory but is left on disk
        // so a stray typo can't wipe the user's settings.
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => write_default_config(&path),
    }
}

pub(crate) fn get_config() -> PluginConfig {
    CONFIG.lock().unwrap().clone().unwrap_or_default()
}

pub(crate) fn schematics_dir() -> Option<String> {
    SCHEMATICS_DIR.lock().unwrap().clone()
}

// Emit a diagnostic line to the server console when debug mode is on.
pub(crate) fn debug_log(msg: &str) {
    let enabled = CONFIG.lock().unwrap().as_ref().is_some_and(|c| c.debug);
    if enabled {
        pumpkin_plugin_api::logging::log(
            pumpkin_plugin_api::logging::LogLevel::Info,
            &format!("{PREFIX}[debug] {msg}"),
        );
    }
}

// ── Messaging ───────────────────────────────────────────────────────

fn msg(text: &str, color: NamedColor) -> TextComponent {
    TextComponent::text(PREFIX)
        .color_named(NamedColor::Gold)
        .add_child(TextComponent::text(text).color_named(color))
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
