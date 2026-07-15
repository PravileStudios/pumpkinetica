use pumpkin_plugin_api::common::{BlockPos, Hand};
use pumpkin_plugin_api::events::{
    BlockBreakEvent, EventHandler, FromIntoEvent, InteractAction, PlayerInteractEvent,
    PlayerLeaveEvent,
};
use pumpkin_plugin_api::Server;

use crate::{
    LOADED_SCHEMATICS, PLAYER_CLIPBOARDS, PLAYER_HISTORIES, PLAYER_SELECTIONS, get_config,
    msg_info, normalize_item_name,
};

pub(crate) struct Selection {
    pub pos1: BlockPos,
    pub pos2: BlockPos,
}

impl Selection {
    pub fn bounds(&self) -> (BlockPos, BlockPos) {
        (
            BlockPos {
                x: self.pos1.x.min(self.pos2.x),
                y: self.pos1.y.min(self.pos2.y),
                z: self.pos1.z.min(self.pos2.z),
            },
            BlockPos {
                x: self.pos1.x.max(self.pos2.x),
                y: self.pos1.y.max(self.pos2.y),
                z: self.pos1.z.max(self.pos2.z),
            },
        )
    }

    pub fn dimensions(&self) -> (i32, i32, i32) {
        let (min, max) = self.bounds();
        (max.x - min.x + 1, max.y - min.y + 1, max.z - min.z + 1)
    }

    pub fn volume(&self) -> u64 {
        let (sx, sy, sz) = self.dimensions();
        sx as u64 * sy as u64 * sz as u64
    }
}

pub(crate) struct WandInteractHandler;

impl EventHandler<PlayerInteractEvent> for WandInteractHandler {
    fn handle(
        &self,
        _server: Server,
        mut event: <PlayerInteractEvent as FromIntoEvent>::Data,
    ) -> <PlayerInteractEvent as FromIntoEvent>::Data {
        let config = get_config();

        let Some(key) = event
            .player
            .get_item_in_hand(Hand::Right)
            .map(|i| i.get_registry_key())
        else {
            return event;
        };

        if normalize_item_name(&key) != normalize_item_name(&config.wand_item) {
            return event;
        }

        let Some(clicked_pos) = event.clicked_pos else {
            return event;
        };

        let is_pos1 = matches!(event.action, InteractAction::LeftClickBlock);
        let is_pos2 = matches!(event.action, InteractAction::RightClickBlock);

        if is_pos1 || is_pos2 {
            let mut sel = PLAYER_SELECTIONS.lock().unwrap();
            if let Some(ref mut map) = *sel {
                let player_name = event.player.get_name();
                let entry = map.entry(player_name).or_insert(Selection {
                    pos1: clicked_pos,
                    pos2: clicked_pos,
                });
                let label = if is_pos1 {
                    entry.pos1 = clicked_pos;
                    "Pos1"
                } else {
                    entry.pos2 = clicked_pos;
                    "Pos2"
                };
                event.player.send_system_message(
                    msg_info(&format!(
                        "{label} set to ({}, {}, {})",
                        clicked_pos.x, clicked_pos.y, clicked_pos.z
                    )),
                    false,
                );
            }
            event.cancelled = true;
        }

        event
    }
}

pub(crate) struct WandBreakCancelHandler;

impl EventHandler<BlockBreakEvent> for WandBreakCancelHandler {
    fn handle(
        &self,
        _server: Server,
        mut event: <BlockBreakEvent as FromIntoEvent>::Data,
    ) -> <BlockBreakEvent as FromIntoEvent>::Data {
        let config = get_config();

        let Some(ref player) = event.player else {
            return event;
        };

        let Some(item) = player.get_item_in_hand(Hand::Right) else {
            return event;
        };

        if normalize_item_name(&item.get_registry_key()) == normalize_item_name(&config.wand_item)
        {
            event.cancelled = true;
        }

        event
    }
}

pub(crate) struct PlayerCleanupHandler;

impl EventHandler<PlayerLeaveEvent> for PlayerCleanupHandler {
    fn handle(
        &self,
        _server: Server,
        event: <PlayerLeaveEvent as FromIntoEvent>::Data,
    ) -> <PlayerLeaveEvent as FromIntoEvent>::Data {
        let name = event.player.get_name();

        if let Some(ref mut map) = *PLAYER_SELECTIONS.lock().unwrap() {
            map.remove(&name);
        }
        if let Some(ref mut map) = *PLAYER_CLIPBOARDS.lock().unwrap() {
            map.remove(&name);
        }
        if let Some(ref mut map) = *PLAYER_HISTORIES.lock().unwrap() {
            map.remove(&name);
        }
        if let Some(ref mut map) = *LOADED_SCHEMATICS.lock().unwrap() {
            map.remove(&name);
        }

        event
    }
}
