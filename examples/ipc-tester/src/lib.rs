use pumpkin_plugin_api::commands::{Command, CommandHandler};
use pumpkin_plugin_api::{
    Context, Plugin, PluginMetadata, Result, Server,
    command::{CommandError, CommandNode, CommandSender, ConsumedArgs},
    command_wit::{Arg, ArgumentType, StringType},
    common::NamedColor,
    ipc,
    permission::{Permission, PermissionDefault, PermissionLevel},
    register_plugin,
    text::TextComponent,
};
use serde_json::{Value, json};

const TARGET: &str = "pumpkinetica";
const PLUGIN_NAME: &str = "pumpkinetica-ipc-tester";

struct IpcTester;

register_plugin!(IpcTester);

impl Plugin for IpcTester {
    fn new() -> Self {
        IpcTester
    }

    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: PLUGIN_NAME.into(),
            version: "0.1.0".into(),
            authors: vec!["Indrajeeth".into()],
            description: "Drives Pumpkinetica over plugin IPC for testing".into(),
            dependencies: vec![TARGET.into()],
            permissions: vec![],
        }
    }

    fn on_load(&mut self, context: Context) -> Result<()> {
        let caps = CommandNode::literal("caps").execute(SendHandler::Caps);
        let list = CommandNode::literal("list").execute(SendHandler::List);
        let status = CommandNode::literal("status").execute(SendHandler::Status);

        let paste_arg =
            CommandNode::argument("name", &ArgumentType::String(StringType::SingleWord))
                .execute(SendHandler::Paste { by_path: false });
        let paste = CommandNode::literal("paste");
        paste.then(paste_arg);

        let path_arg = CommandNode::argument("path", &ArgumentType::String(StringType::Greedy))
            .execute(SendHandler::Paste { by_path: true });
        let pastepath = CommandNode::literal("pastepath");
        pastepath.then(path_arg);

        let cmd = Command::new(&["pptest".into()], "Send IPC requests to Pumpkinetica");
        cmd.then(caps);
        cmd.then(list);
        cmd.then(status);
        cmd.then(paste);
        cmd.then(pastepath);

        let _ = context.register_permission(&Permission {
            node: format!("{PLUGIN_NAME}:command.pptest"),
            description: "Allows use of /pptest".into(),
            default: PermissionDefault::Op(PermissionLevel::Two),
            children: vec![],
        });
        context.register_command(cmd, "command.pptest");
        Ok(())
    }
}

enum SendHandler {
    Caps,
    List,
    Status,
    Paste { by_path: bool },
}

impl CommandHandler for SendHandler {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let payload = match self {
            SendHandler::Caps => request("pumpkinetica:capabilities/v1", json!({})),
            SendHandler::List => request("pumpkinetica:list/v1", json!({})),
            SendHandler::Status => request("pumpkinetica:status/v1", json!({})),
            SendHandler::Paste { by_path } => {
                let player = sender.as_player().ok_or(CommandError::InvalidRequirement)?;
                let (px, py, pz) = player.get_position();
                let world = player.get_world().get_dimension();
                let key = if *by_path { "path" } else { "schematic" };
                let value = match args.get_value(key) {
                    Arg::Simple(s) => s,
                    _ => return Err(CommandError::InvalidConsumption(Some(key.into()))),
                };
                let mut obj = serde_json::Map::new();
                obj.insert(key.to_string(), Value::String(value));
                obj.insert("x".into(), (px.floor() as i32).into());
                obj.insert("y".into(), (py.floor() as i32).into());
                obj.insert("z".into(), (pz.floor() as i32).into());
                obj.insert("world".into(), Value::String(world));
                request("pumpkinetica:paste/v1", Value::Object(obj))
            }
        };

        let bytes = match serde_json::to_vec(&payload) {
            Ok(b) => b,
            Err(e) => {
                sender.send_message(msg(&format!("encode: {e}"), NamedColor::Red));
                return Ok(1);
            }
        };

        match ipc::send_ipc_message(TARGET, &bytes) {
            Ok(Ok(reply)) => {
                let text = String::from_utf8_lossy(&reply);
                let color = if reply_ok(&reply) {
                    NamedColor::Green
                } else {
                    NamedColor::Yellow
                };
                sender.send_message(msg(&format!("reply: {text}"), color));
                Ok(0)
            }
            // Inner Err = protocol-level failure (bad envelope, unknown kind, oversized).
            Ok(Err(protocol_err)) => {
                sender.send_message(msg(
                    &format!("protocol error: {protocol_err}"),
                    NamedColor::Red,
                ));
                Ok(1)
            }
            // Outer Err = host could not deliver: target not loaded or IPC denied.
            Err(_) => {
                sender.send_message(msg(
                    &format!("'{TARGET}' unreachable (not loaded or IPC denied)"),
                    NamedColor::Red,
                ));
                Ok(1)
            }
        }
    }
}

fn request(kind: &str, payload: Value) -> Value {
    json!({ "kind": kind, "payload": payload })
}

fn reply_ok(reply: &[u8]) -> bool {
    serde_json::from_slice::<Value>(reply)
        .ok()
        .and_then(|v| v.get("ok").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

fn msg(text: &str, color: NamedColor) -> TextComponent {
    let c = TextComponent::text(text);
    c.color_named(color);
    c
}
