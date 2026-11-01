//! `example-cli mcp`: the demo devices as MCP tools (DESIGN.md, "MCP
//! profile"). A real CLI copies this shape: a toolset struct holding what
//! its commands already use, one read tool per question a person asks, one
//! tool per write, and the same provider calls and DTOs its commands use.

use pk_cli_mcp::{
    parse_args, ListArgs, Page, PageRequest, Server, ToolDef, ToolError, ToolOutput, Toolset,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{demo_devices, find_device, Device, BIN};

pub struct ExampleTools;

const DEVICES: &str = "example_devices";
const RENAME: &str = "example_device_rename";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DevicesArgs {
    /// Only devices in this room (case-insensitive), e.g. "Office".
    #[serde(default)]
    pub room: Option<String>,
    #[serde(flatten)]
    pub list: ListArgs,
}

/// The filters part of the query, which a cursor is bound to.
#[derive(Serialize)]
struct DevicesFilter<'a> {
    room: Option<&'a str>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DeviceItem {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    /// Detailed only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

impl DeviceItem {
    fn from(d: &Device, detailed: bool) -> Self {
        Self {
            name: d.name.clone(),
            room: d.room.clone(),
            id: detailed.then(|| d.id.clone()),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RenameArgs {
    /// The device to rename: its name, id, or a unique part of its name.
    pub device: String,
    /// The new name.
    pub name: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RenameOut {
    pub device: DeviceItem,
}

impl Toolset for ExampleTools {
    fn server(&self) -> Server {
        Server::new(BIN, env!("CARGO_PKG_VERSION"), "Example", "example")
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![
            ToolDef::read(
                DEVICES,
                "List devices",
                "Lists the user's devices with the room each is in, in the order the app lists them. Use it to \
                 answer what devices exist or what is in a room, and to find the exact name \
                 example_device_rename takes. Returns items with name and room (detailed adds \
                 id), plus returned and next_cursor. Narrow with room.",
            )
            .input::<DevicesArgs>()
            .output::<Page<DeviceItem>>(),
            ToolDef::write(
                RENAME,
                "Rename a device",
                "Renames one device. Use it only when the user asked for the new name. Takes \
                 the device by name or id and returns the device as read back after the change.",
                false,
            )
            .idempotent(true)
            .input::<RenameArgs>()
            .output::<RenameOut>(),
        ]
    }

    async fn call(&self, name: &str, args: Value) -> Result<ToolOutput, ToolError> {
        match name {
            DEVICES => devices(parse_args(args)?),
            RENAME => rename(parse_args(args)?),
            other => Err(ToolError::other(format!("no handler for {other}"))),
        }
    }
}

fn devices(args: DevicesArgs) -> Result<ToolOutput, ToolError> {
    let room = args
        .room
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty());
    let page = PageRequest::new(&args.list, &DevicesFilter { room })?;
    let detailed = args.list.format().is_detailed();
    // A real CLI: the provider read its `devices list` command makes.
    let matched: Vec<DeviceItem> = demo_devices()
        .iter()
        .filter(|d| match room {
            Some(r) => d
                .room
                .as_deref()
                .is_some_and(|dr| dr.eq_ignore_ascii_case(r)),
            None => true,
        })
        .map(|d| DeviceItem::from(d, detailed))
        .collect();
    ToolOutput::new(&page.page(matched, "room")?)
}

fn rename(args: RenameArgs) -> Result<ToolOutput, ToolError> {
    if args.name.trim().is_empty() {
        return Err(ToolError::usage("name is empty"));
    }
    let all = demo_devices();
    let device = find_device(&all, &args.device)
        .map_err(|e| ToolError::from(e).with_fix("Call example_devices for the exact names"))?;
    // A real CLI sends the write, then reads the device back and returns
    // what it read. The client asked the user before this call
    // (requiresUserInteraction), so there is no confirm prompt here: the
    // server has no terminal to ask on.
    let mut renamed = device.clone();
    renamed.name = args.name.trim().to_string();
    ToolOutput::new(&RenameOut {
        device: DeviceItem::from(&renamed, true),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pk_cli_mcp::conformance::{assert_conforms, assert_fits, call};
    use serde_json::json;

    #[test]
    fn the_tools_conform_to_the_mcp_profile() {
        assert_conforms(&ExampleTools);
    }

    #[test]
    fn devices_are_concise_by_default_and_filter_by_room() {
        let r = call(&ExampleTools, DEVICES, json!({"room": "office"}));
        let page = assert_fits(&ExampleTools, DEVICES, &r);
        assert_eq!(page["returned"], 2);
        assert!(page["items"][0].get("id").is_none());
        let r = call(
            &ExampleTools,
            DEVICES,
            json!({"response_format": "detailed"}),
        );
        let page = assert_fits(&ExampleTools, DEVICES, &r);
        assert_eq!(page["items"][0]["id"], "H6076_AA11BB22");
    }

    #[test]
    fn paging_hands_back_a_cursor_bound_to_the_filter() {
        let r = call(&ExampleTools, DEVICES, json!({"limit": 1}));
        let cursor = assert_fits(&ExampleTools, DEVICES, &r)["next_cursor"]
            .as_str()
            .unwrap()
            .to_string();
        let r = call(
            &ExampleTools,
            DEVICES,
            json!({"limit": 1, "cursor": cursor}),
        );
        assert_eq!(
            assert_fits(&ExampleTools, DEVICES, &r)["items"][0]["name"],
            "Desk Plug"
        );
        let r = call(
            &ExampleTools,
            DEVICES,
            json!({"cursor": cursor, "room": "Office"}),
        );
        assert!(
            r.is_error() && r.text().contains("different filters"),
            "{}",
            r.text()
        );
    }

    #[test]
    fn rename_resolves_by_name_and_reports_a_miss_with_the_list_tool() {
        let r = call(
            &ExampleTools,
            RENAME,
            json!({"device": "desk", "name": "Desk Lamp"}),
        );
        let out = assert_fits(&ExampleTools, RENAME, &r);
        assert_eq!(out["device"]["name"], "Desk Lamp");
        assert_eq!(out["device"]["id"], "KP115_CC33DD44");
        let r = call(
            &ExampleTools,
            RENAME,
            json!({"device": "garage", "name": "x"}),
        );
        assert!(r.is_error());
        assert!(r.text().starts_with("not_found:"), "{}", r.text());
        assert!(r.text().contains("example_devices"));
    }
}
