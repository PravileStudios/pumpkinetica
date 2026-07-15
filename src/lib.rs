mod litematica;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use pumpkin_plugin_api::commands::{Command, CommandHandler};
use pumpkin_plugin_api::{
    Context, Plugin, PluginMetadata, Result, Server,
    command::{CommandError, CommandNode, CommandSender, ConsumedArgs},
    command_wit::{Arg, ArgumentType, StringType},
    common::{BlockPos, NamedColor},
    permission::{Permission, PermissionDefault, PermissionLevel},
    permissions,
    text::TextComponent,
    world,
    register_plugin,
};

use litematica::{PaletteEntry, Schematic};

const PLUGIN_NAME: &str = "pschematics";
const PLUGIN_VERSION: &str = "0.3.0";
const PREFIX: &str = "[PSchematics] ";

struct SchematicPasterPlugin;

static CONFIG: Mutex<Option<PluginConfig>> = Mutex::new(None);
static LOADED_SCHEMATICS: Mutex<Option<HashMap<String, LoadedSchematic>>> = Mutex::new(None);
static ACTIVE_PASTES: AtomicUsize = AtomicUsize::new(0);

#[derive(Serialize, Deserialize, Clone)]
struct PluginConfig {
    #[serde(default = "default_fallback_block")]
    fallback_block: String,
    #[serde(default = "default_blocks_per_tick")]
    blocks_per_tick: usize,
    #[serde(default = "default_max_concurrent_pastes")]
    max_concurrent_pastes: usize,
}

fn default_fallback_block() -> String { "minecraft:cobblestone".into() }
fn default_blocks_per_tick() -> usize { 4096 }
fn default_max_concurrent_pastes() -> usize { 4 }

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
        Ok(contents) => {
            serde_json::from_str(&contents).unwrap_or_else(|_| {
                let config = PluginConfig::default();
                let _ = std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap());
                config
            })
        }
        Err(_) => {
            let config = PluginConfig::default();
            let _ = std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap());
            config
        }
    }
}

fn get_config() -> PluginConfig {
    CONFIG.lock().unwrap().clone().unwrap_or_default()
}

struct LoadedSchematic {
    schematic: Schematic,
    palette_map: Vec<Vec<Option<u16>>>,
}

fn msg(text: &str, color: NamedColor) -> TextComponent {
    let prefix = TextComponent::text(PREFIX);
    prefix.color_named(NamedColor::Gold);
    let body = TextComponent::text(text);
    body.color_named(color);
    prefix.add_child(body);
    prefix
}

fn msg_error(text: &str) -> TextComponent { msg(text, NamedColor::Red) }
fn msg_success(text: &str) -> TextComponent { msg(text, NamedColor::Green) }
fn msg_info(text: &str) -> TextComponent { msg(text, NamedColor::Aqua) }
fn msg_warn(text: &str) -> TextComponent { msg(text, NamedColor::Yellow) }

impl Plugin for SchematicPasterPlugin {
    fn new() -> Self {
        SchematicPasterPlugin
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

        let reload_node = CommandNode::literal("reload").execute(ReloadHandler {
            data_folder,
        });

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

register_plugin!(SchematicPasterPlugin);

fn resolve_fallback_block(config: &PluginConfig) -> Option<u16> {
    if config.fallback_block == "skip" {
        return None;
    }
    world::resolve_block_state(&config.fallback_block, &[])
}

fn resolve_palette(palette: &[PaletteEntry], fallback: Option<u16>) -> Vec<Option<u16>> {
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

struct LoadHandler {
    schematics_dir: String,
}

impl CommandHandler for LoadHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
        let player_name = player.get_name();

        let file_arg = match args.get_value("file") {
            Arg::Simple(s) => s,
            _ => {
                return Err(CommandError::InvalidConsumption(Some(
                    "Expected file name".into(),
                )));
            }
        };

        let path = format!("{}/{}", self.schematics_dir, file_arg);
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => {
                sender.send_message(msg_error(&format!("Failed to read file: {e}")));
                return Ok(1);
            }
        };

        let schematic = match litematica::parse_schematic(&data, &file_arg) {
            Ok(s) => s,
            Err(e) => {
                sender.send_message(msg_error(&format!("Parse error: {e}")));
                return Ok(1);
            }
        };

        let config = get_config();
        let fallback = resolve_fallback_block(&config);

        let palette_map: Vec<Vec<Option<u16>>> = schematic
            .regions
            .iter()
            .map(|r| resolve_palette(&r.palette, fallback))
            .collect();

        let total_blocks: usize = schematic
            .regions
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let [sx, sy, sz] = r.abs_size();
                let pm = &palette_map[i];
                let mut count = 0usize;
                for y in 0..sy {
                    for z in 0..sz {
                        for x in 0..sx {
                            let idx = r.get_palette_index(x, y, z) as usize;
                            if idx < pm.len() {
                                if let Some(state_id) = pm[idx] {
                                    if state_id != 0 {
                                        count += 1;
                                    }
                                }
                            }
                        }
                    }
                }
                count
            })
            .sum();

        let mut unresolved_count = 0usize;
        for (i, pm) in palette_map.iter().enumerate() {
            for (j, entry) in pm.iter().enumerate() {
                if entry.is_none() && schematic.regions[i].palette[j].name != "minecraft:air" {
                    unresolved_count += 1;
                }
            }
        }

        let region_count = schematic.regions.len();
        let name = schematic.name.clone();

        {
            let mut map = LOADED_SCHEMATICS.lock().unwrap();
            if let Some(ref mut m) = *map {
                m.insert(player_name, LoadedSchematic { schematic, palette_map });
            }
        }

        sender.send_message(msg_success(&format!(
            "Loaded '{name}' - {region_count} region(s), {total_blocks} blocks"
        )));

        if unresolved_count > 0 && config.fallback_block == "skip" {
            sender.send_message(msg_warn(&format!(
                "{unresolved_count} block type(s) skipped (unsupported). Use /schematic info for details."
            )));
        } else if unresolved_count > 0 {
            sender.send_message(msg_warn(&format!(
                "{unresolved_count} block type(s) replaced with {}. Use /schematic info for details.",
                config.fallback_block
            )));
        }

        Ok(0)
    }
}

struct PasteHandler;

impl CommandHandler for PasteHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
        let player_name = player.get_name();
        let pos = player.get_position();
        let origin = BlockPos {
            x: pos.0 as i32,
            y: pos.1 as i32,
            z: pos.2 as i32,
        };
        let player_world = player.get_world();

        let config = get_config();
        let current = ACTIVE_PASTES.load(Ordering::Relaxed);
        if current >= config.max_concurrent_pastes {
            sender.send_message(msg_error("Server is busy. Please wait and try again."));
            return Ok(1);
        }

        let map = LOADED_SCHEMATICS.lock().unwrap();
        let loaded = map
            .as_ref()
            .and_then(|m| m.get(&player_name))
            .ok_or_else(|| CommandError::CommandFailed(
                msg_error("No schematic loaded. Use /schematic load <file>"),
            ))?;

        let mut work_queue: Vec<BlockPlacement> = Vec::new();
        let mut tile_entities: Vec<TileEntityPlacement> = Vec::new();

        for (region_idx, region) in loaded.schematic.regions.iter().enumerate() {
            let [sx, sy, sz] = region.abs_size();
            let palette_map = &loaded.palette_map[region_idx];

            let offset_x = if region.size[0] < 0 {
                region.position[0] + region.size[0] + 1
            } else {
                region.position[0]
            };
            let offset_y = if region.size[1] < 0 {
                region.position[1] + region.size[1] + 1
            } else {
                region.position[1]
            };
            let offset_z = if region.size[2] < 0 {
                region.position[2] + region.size[2] + 1
            } else {
                region.position[2]
            };

            for y in 0..sy {
                for z in 0..sz {
                    for x in 0..sx {
                        let palette_idx = region.get_palette_index(x, y, z) as usize;

                        if palette_idx >= palette_map.len() {
                            continue;
                        }

                        let Some(state_id) = palette_map[palette_idx] else {
                            continue;
                        };

                        if state_id == 0 {
                            continue;
                        }

                        work_queue.push(BlockPlacement {
                            pos: BlockPos {
                                x: origin.x + offset_x + x,
                                y: origin.y + offset_y + y,
                                z: origin.z + offset_z + z,
                            },
                            state_id,
                        });
                    }
                }
            }

            for te in &region.tile_entities {
                tile_entities.push(TileEntityPlacement {
                    pos: BlockPos {
                        x: origin.x + offset_x + te.x,
                        y: origin.y + offset_y + te.y,
                        z: origin.z + offset_z + te.z,
                    },
                    nbt: te.raw_nbt.clone(),
                });
            }
        }

        let total = work_queue.len();
        let schematic_name = loaded.schematic.name.clone();
        drop(map);

        let dimension = player_world.get_dimension();

        sender.send_message(msg_info(&format!("Pasting '{schematic_name}' ({total} blocks)...")));

        schedule_paste(
            work_queue, tile_entities, dimension,
            config.blocks_per_tick, player_name,
            schematic_name, origin,
        );

        Ok(0)
    }
}

struct BlockPlacement {
    pos: BlockPos,
    state_id: u16,
}

struct TileEntityPlacement {
    pos: BlockPos,
    nbt: Vec<u8>,
}

struct ChunkBatch {
    chunk_x: i32,
    chunk_z: i32,
    blocks: Vec<(BlockPos, u16)>,
}

fn build_chunk_batches(queue: Vec<BlockPlacement>) -> Vec<ChunkBatch> {
    let mut chunk_map: HashMap<(i32, i32), Vec<(BlockPos, u16)>> = HashMap::new();
    for p in queue {
        let cx = p.pos.x.div_euclid(16);
        let cz = p.pos.z.div_euclid(16);
        chunk_map.entry((cx, cz)).or_default().push((p.pos, p.state_id));
    }
    chunk_map
        .into_iter()
        .map(|((cx, cz), blocks)| ChunkBatch { chunk_x: cx, chunk_z: cz, blocks })
        .collect()
}

struct PasteState {
    batches: Vec<ChunkBatch>,
    batch_idx: usize,
    block_offset: usize,
    tile_entities: Vec<TileEntityPlacement>,
}

fn schedule_paste(
    queue: Vec<BlockPlacement>,
    tile_entities: Vec<TileEntityPlacement>,
    dimension: String,
    blocks_per_tick: usize,
    player_name: String,
    schematic_name: String,
    origin: BlockPos,
) {
    ACTIVE_PASTES.fetch_add(1, Ordering::Relaxed);

    let state = std::sync::Arc::new(Mutex::new(PasteState {
        batches: build_chunk_batches(queue),
        batch_idx: 0,
        block_offset: 0,
        tile_entities,
    }));
    let state_clone = state.clone();
    let task_id = std::sync::Arc::new(Mutex::new(0u32));
    let task_id_clone = task_id.clone();

    let id = pumpkin_plugin_api::scheduler::schedule_repeating_task(0, 1, move |server| {
        let world = match server.get_world_by_name(&dimension) {
            Some(w) => w,
            None => {
                ACTIVE_PASTES.fetch_sub(1, Ordering::Relaxed);
                let tid = *task_id_clone.lock().unwrap();
                pumpkin_plugin_api::scheduler::cancel_task(tid);
                return;
            }
        };

        let mut s = state_clone.lock().unwrap();
        let mut remaining = blocks_per_tick;

        while remaining > 0 && s.batch_idx < s.batches.len() {
            let batch = &s.batches[s.batch_idx];
            let blocks_left = batch.blocks.len() - s.block_offset;
            let to_place = std::cmp::min(remaining, blocks_left);

            match world.get_chunk(batch.chunk_x, batch.chunk_z) {
                Some(chunk) => {
                    for &(pos, state_id) in &batch.blocks[s.block_offset..s.block_offset + to_place] {
                        let local = BlockPos {
                            x: pos.x.rem_euclid(16),
                            y: pos.y,
                            z: pos.z.rem_euclid(16),
                        };
                        chunk.set_block_state(local, state_id);
                    }
                }
                None => {
                    // Chunk not loaded — use world.set_block_state as fallback
                    let flags = world::BlockFlags::FORCE_STATE
                        | world::BlockFlags::SKIP_DROPS
                        | world::BlockFlags::SKIP_REDSTONE_WIRE_STATE_REPLACEMENT
                        | world::BlockFlags::SKIP_BLOCK_ADDED_CALLBACK;
                    for &(pos, state_id) in &batch.blocks[s.block_offset..s.block_offset + to_place] {
                        world.set_block_state(pos, state_id, flags);
                    }
                }
            }

            remaining -= to_place;

            if s.block_offset + to_place >= batch.blocks.len() {
                s.batch_idx += 1;
                s.block_offset = 0;
            } else {
                s.block_offset += to_place;
            }
        }

        if s.batch_idx >= s.batches.len() {
            for te in s.tile_entities.drain(..) {
                let _ = world.set_block_entity_nbt(te.pos, &te.nbt);
            }
            drop(s);

            ACTIVE_PASTES.fetch_sub(1, Ordering::Relaxed);

            if let Some(player) = server.get_player_by_name(&player_name) {
                player.send_system_message(
                    msg_success(&format!(
                        "Pasted '{}' at ({}, {}, {})",
                        schematic_name, origin.x, origin.y, origin.z
                    )),
                    false,
                );
            }

            let tid = *task_id_clone.lock().unwrap();
            pumpkin_plugin_api::scheduler::cancel_task(tid);
        }
    });

    *task_id.lock().unwrap() = id;
}

struct ListHandler {
    schematics_dir: String,
}

impl CommandHandler for ListHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let entries = match std::fs::read_dir(&self.schematics_dir) {
            Ok(e) => e,
            Err(_) => {
                sender.send_message(msg_warn("No schematics found."));
                return Ok(0);
            }
        };

        let mut files: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext == "litematica" || ext == "litematic" || ext == "schem")
            })
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();

        files.sort();

        if files.is_empty() {
            sender.send_message(msg_warn("No schematics found."));
        } else {
            let header = msg_info(&format!("Available schematics ({}):", files.len()));
            sender.send_message(header);
            for file in &files {
                let entry = TextComponent::text(&format!("  - {file}"));
                entry.color_named(NamedColor::Gray);
                sender.send_message(entry);
            }
        }

        Ok(0)
    }
}

struct InfoHandler;

impl CommandHandler for InfoHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
        let player_name = player.get_name();

        let map = LOADED_SCHEMATICS.lock().unwrap();
        let loaded = map
            .as_ref()
            .and_then(|m| m.get(&player_name))
            .ok_or_else(|| CommandError::CommandFailed(
                msg_error("No schematic loaded."),
            ))?;

        let schematic = &loaded.schematic;

        let header = msg_info(&format!("Schematic: {}", schematic.name));
        sender.send_message(header);

        for (i, region) in schematic.regions.iter().enumerate() {
            let [sx, sy, sz] = region.abs_size();
            let region_msg = TextComponent::text(&format!(
                "  {} - {}x{}x{}, {} blocks, {} tile entities",
                region.name, sx, sy, sz,
                region.palette.len(),
                region.tile_entities.len(),
            ));
            region_msg.color_named(NamedColor::Gray);
            sender.send_message(region_msg);

            let unresolved: Vec<&str> = loaded.palette_map[i]
                .iter()
                .zip(region.palette.iter())
                .filter(|(state, entry)| {
                    state.is_none() && entry.name != "minecraft:air" && entry.name != "air"
                })
                .map(|(_, entry)| entry.name.as_str())
                .collect();

            if !unresolved.is_empty() {
                let warn = TextComponent::text(&format!(
                    "    Unsupported: {}",
                    unresolved.join(", ")
                ));
                warn.color_named(NamedColor::Yellow);
                sender.send_message(warn);
            }
        }

        Ok(0)
    }
}

struct StatusHandler;

impl CommandHandler for StatusHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let active = ACTIVE_PASTES.load(Ordering::Relaxed);
        if active == 0 {
            sender.send_message(msg_info("No active paste operations."));
        } else {
            sender.send_message(msg_warn(&format!(
                "{active} paste operation(s) in progress."
            )));
        }
        Ok(0)
    }
}

struct HelpHandler;

impl CommandHandler for HelpHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        sender.send_message(msg_info(&format!("PSchematics v{PLUGIN_VERSION}")));

        let cmds = [
            ("/schematic load <file>", "Load a schematic file"),
            ("/schematic paste", "Paste loaded schematic at your position"),
            ("/schematic list", "List available schematic files"),
            ("/schematic info", "Show details of loaded schematic"),
            ("/schematic status", "Show active paste operations"),
            ("/schematic reload", "Reload config from disk"),
        ];

        for (cmd, desc) in &cmds {
            let line = TextComponent::text(&format!("  {cmd}"));
            line.color_named(NamedColor::Green);
            let detail = TextComponent::text(&format!(" - {desc}"));
            detail.color_named(NamedColor::Gray);
            line.add_child(detail);
            sender.send_message(line);
        }

        Ok(0)
    }
}

struct ReloadHandler {
    data_folder: String,
}

impl CommandHandler for ReloadHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let config = load_config(&self.data_folder);
        *CONFIG.lock().unwrap() = Some(config.clone());
        sender.send_message(msg_success(&format!(
            "Config reloaded. Fallback: {}, blocks/tick: {}, max pastes: {}",
            config.fallback_block, config.blocks_per_tick, config.max_concurrent_pastes
        )));
        Ok(0)
    }
}
