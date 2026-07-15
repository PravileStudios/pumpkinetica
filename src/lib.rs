mod litematica;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use pumpkin_plugin_api::commands::{Command, CommandHandler};
use pumpkin_plugin_api::{
    Context, Plugin, PluginMetadata, Result, Server,
    command::{CommandError, CommandNode, CommandSender, ConsumedArgs},
    command_wit::{Arg, ArgumentType, StringType},
    common::{BlockPos, NamedColor},
    permission::{Permission, PermissionDefault, PermissionLevel},
    permissions,
    text::TextComponent,
    world::{self, BlockFlags},
    register_plugin,
};

use litematica::{PaletteEntry, Schematic};

const BLOCKS_PER_TICK: usize = 4096;
const MAX_CONCURRENT_PASTES: usize = 4;
const PREFIX: &str = "[PSchematics] ";

struct SchematicPasterPlugin;

static LOADED_SCHEMATICS: Mutex<Option<HashMap<String, LoadedSchematic>>> = Mutex::new(None);
static ACTIVE_PASTES: AtomicUsize = AtomicUsize::new(0);

struct LoadedSchematic {
    schematic: Schematic,
    palette_map: Vec<Vec<Option<u16>>>,
}

fn msg(text: &str, color: NamedColor) -> TextComponent {
    let prefix = TextComponent::text(PREFIX);
    prefix.color_named(NamedColor::Gold);
    prefix.bold(true);
    let body = TextComponent::text(text);
    body.color_named(color);
    prefix.add_child(body);
    prefix
}

fn msg_error(text: &str) -> TextComponent {
    msg(text, NamedColor::Red)
}

fn msg_success(text: &str) -> TextComponent {
    msg(text, NamedColor::Green)
}

fn msg_info(text: &str) -> TextComponent {
    msg(text, NamedColor::Aqua)
}

fn msg_warn(text: &str) -> TextComponent {
    msg(text, NamedColor::Yellow)
}

impl Plugin for SchematicPasterPlugin {
    fn new() -> Self {
        SchematicPasterPlugin
    }

    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: "pschematics".into(),
            version: "0.2.0".into(),
            authors: vec!["PumpkinMC".into()],
            description: "Load and paste .litematica schematics into the world".into(),
            dependencies: vec![],
            permissions: vec![
                permissions::FS_READ_DATA.into(),
                permissions::FS_WRITE_DATA.into(),
            ],
        }
    }

    fn on_load(&mut self, context: Context) -> Result<()> {
        {
            let mut map = LOADED_SCHEMATICS.lock().unwrap();
            *map = Some(HashMap::new());
        }

        let file_arg = CommandNode::argument("file", &ArgumentType::String(StringType::SingleWord))
            .execute(LoadHandler {
                data_folder: context.get_data_folder(),
            });
        let load_node = CommandNode::literal("load");
        load_node.then(file_arg);

        let paste_node = CommandNode::literal("paste").execute(PasteHandler);

        let list_node = CommandNode::literal("list").execute(ListHandler {
            data_folder: context.get_data_folder(),
        });

        let info_node = CommandNode::literal("info").execute(InfoHandler);

        let status_node = CommandNode::literal("status").execute(StatusHandler);

        let cmd = Command::new(
            &["schematic".into(), "schem".into()],
            "Load and paste .litematica schematics",
        );
        cmd.then(load_node);
        cmd.then(paste_node);
        cmd.then(list_node);
        cmd.then(info_node);
        cmd.then(status_node);

        let _ = context.register_permission(&Permission {
            node: "pschematics:command.schematic".into(),
            description: "Allows use of /schematic commands".into(),
            default: PermissionDefault::Op(PermissionLevel::Two),
            children: vec![],
        });

        context.register_command(cmd, "command.schematic");

        Ok(())
    }
}

register_plugin!(SchematicPasterPlugin);

fn resolve_palette(palette: &[PaletteEntry]) -> Vec<Option<u16>> {
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
            world::resolve_block_state(&entry.name, &props)
        })
        .collect()
}

struct LoadHandler {
    data_folder: String,
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

        let path = format!("{}/{}", self.data_folder, file_arg);
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => {
                sender.send_message(msg_error(&format!("Failed to read file: {e}")));
                return Ok(1);
            }
        };

        let schematic = match litematica::parse_litematica(&data) {
            Ok(s) => s,
            Err(e) => {
                sender.send_message(msg_error(&format!("Parse error: {e}")));
                return Ok(1);
            }
        };

        let palette_map: Vec<Vec<Option<u16>>> = schematic
            .regions
            .iter()
            .map(|r| resolve_palette(&r.palette))
            .collect();

        let total_blocks: usize = schematic
            .regions
            .iter()
            .map(|r| {
                let s = r.abs_size();
                (s[0] * s[1] * s[2]) as usize
            })
            .sum();

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

        let current = ACTIVE_PASTES.load(Ordering::Relaxed);
        if current >= MAX_CONCURRENT_PASTES {
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
        drop(map);

        let dimension = player_world.get_dimension();

        sender.send_message(msg_info(&format!("Pasting {total} blocks...")));

        schedule_paste(work_queue, tile_entities, dimension, total, player_name);

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

fn schedule_paste(
    queue: Vec<BlockPlacement>,
    tile_entities: Vec<TileEntityPlacement>,
    dimension: String,
    total: usize,
    player_name: String,
) {
    ACTIVE_PASTES.fetch_add(1, Ordering::Relaxed);

    let queue = std::sync::Arc::new(Mutex::new(queue));
    let te_queue = std::sync::Arc::new(Mutex::new(tile_entities));
    let queue_clone = queue.clone();
    let te_clone = te_queue.clone();
    let task_id = std::sync::Arc::new(Mutex::new(0u32));
    let task_id_clone = task_id.clone();

    let flags = BlockFlags::FORCE_STATE
        | BlockFlags::SKIP_DROPS
        | BlockFlags::SKIP_REDSTONE_WIRE_STATE_REPLACEMENT
        | BlockFlags::SKIP_BLOCK_ADDED_CALLBACK;

    let id = pumpkin_plugin_api::scheduler::schedule_repeating_task(0, 1, move |server| {
        let world = match server.get_world_by_name(&dimension) {
            Some(w) => w,
            None => return,
        };

        let mut queue_guard = queue_clone.lock().unwrap();
        if !queue_guard.is_empty() {
            let batch_end = std::cmp::min(BLOCKS_PER_TICK, queue_guard.len());
            let batch: Vec<BlockPlacement> = queue_guard.drain(..batch_end).collect();
            let remaining = queue_guard.len();
            drop(queue_guard);

            for placement in &batch {
                world.set_block_state(placement.pos, placement.state_id, flags);
            }

            if remaining == 0 {
                let mut te_guard = te_clone.lock().unwrap();
                for te in te_guard.drain(..) {
                    let _ = world.set_block_entity_nbt(te.pos, &te.nbt);
                }
                drop(te_guard);

                ACTIVE_PASTES.fetch_sub(1, Ordering::Relaxed);
                server.broadcast(&format!(
                    "{player_name}'s schematic paste complete! {total} blocks placed."
                ));

                let tid = *task_id_clone.lock().unwrap();
                pumpkin_plugin_api::scheduler::cancel_task(tid);
            }
        }
    });

    *task_id.lock().unwrap() = id;
}

struct ListHandler {
    data_folder: String,
}

impl CommandHandler for ListHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let entries = match std::fs::read_dir(&self.data_folder) {
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
                    .is_some_and(|ext| ext == "litematica" || ext == "litematic")
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
                .filter(|(state, _)| state.is_none())
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
