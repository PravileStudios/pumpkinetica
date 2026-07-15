mod commands;
mod parser;
mod paste;

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

use serde::{Deserialize, Serialize};

use pumpkin_plugin_api::commands::Command;
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

use commands::*;
use parser::{PaletteEntry, Schematic};

// ── Plugin ──────────────────────────────────────────────────────────

const PLUGIN_NAME: &str = "pschematics";
pub(crate) const PLUGIN_VERSION: &str = "0.4.0";
const PREFIX: &str = "[PSchematics] ";

struct PSchematics;

register_plugin!(PSchematics);

pub(crate) static CONFIG: Mutex<Option<PluginConfig>> = Mutex::new(None);
pub(crate) static LOADED_SCHEMATICS: Mutex<Option<HashMap<String, LoadedSchematic>>> =
    Mutex::new(None);
pub(crate) static ACTIVE_PASTES: AtomicUsize = AtomicUsize::new(0);

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

        {
            let mut map = LOADED_SCHEMATICS.lock().unwrap();
            *map = Some(HashMap::new());
        }

        let file_arg = CommandNode::argument("file", &ArgumentType::String(StringType::SingleWord))
            .execute(LoadHandler {
                schematics_dir: schematics_dir.clone(),
            });
        let load_node = CommandNode::literal("load");
        load_node.then(file_arg);

        let paste_node = CommandNode::literal("paste").execute(PasteHandler);

        let list_node = CommandNode::literal("list").execute(ListHandler {
            schematics_dir: schematics_dir.clone(),
        });

        let info_node = CommandNode::literal("info").execute(InfoHandler);
        let status_node = CommandNode::literal("status").execute(StatusHandler);
        let reload_node = CommandNode::literal("reload").execute(ReloadHandler { data_folder });
        let help_node = CommandNode::literal("help").execute(HelpHandler);

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

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            fallback_block: default_fallback_block(),
            blocks_per_tick: default_blocks_per_tick(),
            max_concurrent_pastes: default_max_concurrent_pastes(),
        }
    }
}

fn load_config(data_folder: &str) -> PluginConfig {
    let path = format!("{}/config.json", data_folder);
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|_| {
            let config = PluginConfig::default();
            let _ = std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap());
            config
        }),
        Err(_) => {
            let config = PluginConfig::default();
            let _ = std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap());
            config
        }
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

// ── Block Resolution ────────────────────────────────────────────────

pub(crate) struct LoadedSchematic {
    pub schematic: Schematic,
    pub palette_map: Vec<Vec<Option<u16>>>,
}

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
