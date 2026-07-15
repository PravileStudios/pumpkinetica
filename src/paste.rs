use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use pumpkin_plugin_api::common::BlockPos;
use pumpkin_plugin_api::world;

use crate::{ACTIVE_PASTES, msg_success};

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
}

fn build_chunk_batches(queue: Vec<BlockPlacement>) -> Vec<ChunkBatch> {
    let mut chunk_map: HashMap<(i32, i32), Vec<(BlockPos, u16)>> = HashMap::new();
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
                    for &(pos, state_id) in &batch.blocks[s.block_offset..s.block_offset + to_place]
                    {
                        let local = BlockPos {
                            x: pos.x.rem_euclid(16),
                            y: pos.y,
                            z: pos.z.rem_euclid(16),
                        };
                        chunk.set_block_state(local, state_id);
                    }
                }
                None => {
                    let flags = world::BlockFlags::FORCE_STATE
                        | world::BlockFlags::SKIP_DROPS
                        | world::BlockFlags::SKIP_REDSTONE_WIRE_STATE_REPLACEMENT
                        | world::BlockFlags::SKIP_BLOCK_ADDED_CALLBACK;
                    for &(pos, state_id) in &batch.blocks[s.block_offset..s.block_offset + to_place]
                    {
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
