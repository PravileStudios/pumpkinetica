use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use pumpkin_plugin_api::common::BlockPos;
use pumpkin_plugin_api::world;

use crate::history::{BlockSnapshot, PlayerHistory, UndoEntry};
use crate::{ACTIVE_PASTES, PLAYER_HISTORIES, get_config, msg_success};

pub(crate) struct BlockPlacement {
    pub pos: BlockPos,
    pub state_id: u16,
}

pub(crate) struct TileEntityPlacement {
    pub pos: BlockPos,
    pub nbt: Vec<u8>,
}

struct ChunkBatch {
    chunk_x: i32,
    chunk_z: i32,
    blocks: Vec<(BlockPos, u16)>,
}

struct PasteState {
    batches: Vec<ChunkBatch>,
    batch_idx: usize,
    block_offset: usize,
    tile_entities: Vec<TileEntityPlacement>,
    record_undo: bool,
    old_snapshots: Vec<BlockSnapshot>,
    new_snapshots: Vec<BlockSnapshot>,
}

fn build_chunk_batches(queue: Vec<BlockPlacement>) -> Vec<ChunkBatch> {
    let estimated_chunks = (queue.len() / 256).max(16);
    let mut chunk_map: HashMap<(i32, i32), Vec<(BlockPos, u16)>> =
        HashMap::with_capacity(estimated_chunks);
    for p in queue {
        let cx = p.pos.x.div_euclid(16);
        let cz = p.pos.z.div_euclid(16);
        chunk_map
            .entry((cx, cz))
            .or_default()
            .push((p.pos, p.state_id));
    }
    chunk_map
        .into_iter()
        .map(|((cx, cz), blocks)| ChunkBatch {
            chunk_x: cx,
            chunk_z: cz,
            blocks,
        })
        .collect()
}

pub(crate) fn schedule_paste(
    queue: Vec<BlockPlacement>,
    tile_entities: Vec<TileEntityPlacement>,
    dimension: String,
    blocks_per_tick: usize,
    player_name: String,
    schematic_name: String,
    origin: BlockPos,
) {
    schedule_block_op(
        queue,
        tile_entities,
        dimension,
        blocks_per_tick,
        player_name,
        format!(
            "Paste '{schematic_name}' at ({}, {}, {})",
            origin.x, origin.y, origin.z
        ),
        true,
    );
}

pub(crate) fn schedule_block_op(
    queue: Vec<BlockPlacement>,
    tile_entities: Vec<TileEntityPlacement>,
    dimension: String,
    blocks_per_tick: usize,
    player_name: String,
    description: String,
    record_undo: bool,
) {
    ACTIVE_PASTES.fetch_add(1, Ordering::Relaxed);

    let total_blocks: usize = queue.len();
    let state = std::sync::Arc::new(Mutex::new(PasteState {
        batches: build_chunk_batches(queue),
        batch_idx: 0,
        block_offset: 0,
        tile_entities,
        record_undo,
        old_snapshots: if record_undo {
            Vec::with_capacity(total_blocks)
        } else {
            Vec::new()
        },
        new_snapshots: if record_undo {
            Vec::with_capacity(total_blocks)
        } else {
            Vec::new()
        },
    }));
    let state_clone = state.clone();
    let task_id = std::sync::Arc::new(Mutex::new(0u32));
    let task_id_clone = task_id.clone();
    let mut description_owned = Some(description.clone());
    let mut dimension_owned = Some(dimension.clone());
    let player_name_owned = Some(player_name.clone());

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
            let bi = s.batch_idx;
            let offset = s.block_offset;
            let batch_len = s.batches[bi].blocks.len();
            let blocks_left = batch_len - offset;
            let to_place = std::cmp::min(remaining, blocks_left);
            let record = s.record_undo;

            let chunk_x = s.batches[bi].chunk_x;
            let chunk_z = s.batches[bi].chunk_z;

            let flags = world::BlockFlags::FORCE_STATE
                | world::BlockFlags::SKIP_DROPS
                | world::BlockFlags::SKIP_REDSTONE_WIRE_STATE_REPLACEMENT
                | world::BlockFlags::SKIP_BLOCK_ADDED_CALLBACK;

            if record {
                let chunk = world.get_chunk(chunk_x, chunk_z);
                for i in offset..offset + to_place {
                    let (pos, state_id) = s.batches[bi].blocks[i];
                    let old_id = match &chunk {
                        Some(c) => c.get_block_state_id(BlockPos {
                            x: pos.x.rem_euclid(16),
                            y: pos.y,
                            z: pos.z.rem_euclid(16),
                        }),
                        None => world.get_block_state_id(pos),
                    };
                    s.old_snapshots.push(BlockSnapshot { pos, state_id: old_id });
                    s.new_snapshots.push(BlockSnapshot { pos, state_id });
                    world.set_block_state(pos, state_id, flags);
                }
            } else {
                for i in offset..offset + to_place {
                    let (pos, state_id) = s.batches[bi].blocks[i];
                    world.set_block_state(pos, state_id, flags);
                }
            }

            remaining -= to_place;

            if offset + to_place >= batch_len {
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

            s.batches = Vec::new();

            if s.record_undo && !s.old_snapshots.is_empty() {
                let config = get_config();
                let entry = UndoEntry {
                    description: description_owned.take().unwrap_or_default(),
                    dimension: dimension_owned.take().unwrap_or_default(),
                    old_states: std::mem::take(&mut s.old_snapshots),
                    new_states: std::mem::take(&mut s.new_snapshots),
                };
                if let Some(ref mut histories) = *PLAYER_HISTORIES.lock().unwrap() {
                    let pname = player_name_owned
                        .clone()
                        .unwrap_or_default();
                    let history = histories
                        .entry(pname)
                        .or_insert_with(PlayerHistory::new);
                    history.push_undo(entry, config.max_undo_history);
                }
            }

            drop(s);

            ACTIVE_PASTES.fetch_sub(1, Ordering::Relaxed);

            if let Some(player) = server.get_player_by_name(&player_name) {
                player.send_system_message(
                    msg_success(&format!("Completed: {description}")),
                    false,
                );
            }

            let tid = *task_id_clone.lock().unwrap();
            pumpkin_plugin_api::scheduler::cancel_task(tid);
        }
    });

    *task_id.lock().unwrap() = id;
}
