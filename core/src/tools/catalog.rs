//! The tool catalog: what every tool this app ships *is*, as data.
//!
//! The specs live beside the tools in Rust (`tools::spec`), and this writes
//! them out — with each tool's name, description, permission and argument
//! schema — to `tool-catalog.json` at the crate root, the way a web framework
//! generates its API schema from the handlers rather than having one written
//! beside them. The front end reads that file instead of scraping these
//! sources with regexes, so it can check its own tables against the real
//! argument names and effects.
//!
//! The file is checked in and a test compares it with what the code says:
//! `UPDATE_TOOL_CATALOG=1 cargo test -p meridian-core --lib tools::catalog`
//! rewrites it, and without the variable a stale file fails the suite.
//!
//! Static facts only. What changes per turn — the skill enum, the sub-agent
//! models, a provider-side search displacing ours — is not exported, and the
//! QQ tools' prose depends on the session, so only their flags are.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{Tool, ToolRegistry};

fn tool_entry(tool: &dyn Tool, bridged: bool) -> Value {
    json!({
        "name": tool.name(),
        "description": tool.description(),
        "permission": tool.default_permission().as_str(),
        "spec": tool.spec(),
        "parameters": tool.parameters_schema(),
        "bridged": bridged,
    })
}

/// The catalog as JSON. `registry` is the built-in set; the tools the ACP
/// bridge lends a hosted session are read off the bridge itself, so one that
/// exists only there (`conversation_usage`) is listed too.
pub fn catalog(registry: &ToolRegistry) -> Value {
    let lent = crate::acp::bridge::tools_for("catalog", Some("catalog"), std::path::PathBuf::from("/nonexistent"));
    let bridged: Vec<&str> = lent.iter().map(|t| t.name()).collect();

    let mut native: BTreeMap<&str, Value> = BTreeMap::new();
    for name in registry.builtin_names_where(|_| true) {
        let tool = registry.get(name).expect("a listed built-in resolves");
        native.insert(name, tool_entry(tool.as_ref(), bridged.contains(&name)));
    }
    for tool in &lent {
        native
            .entry(tool.name())
            .or_insert_with(|| tool_entry(tool.as_ref(), true));
    }

    json!({
        "generated_by": "meridian-core tools::catalog. Do not edit; run UPDATE_TOOL_CATALOG=1 cargo test -p meridian-core --lib tools::catalog",
        "native": native.into_values().collect::<Vec<_>>(),
        "bridge_prefix": format!("mcp__{}__", crate::acp::bridge::SERVER_NAME),
        "onebot": crate::onebot::qq_tool_spec_catalog(),
    })
}

#[cfg(test)]
mod tests {
    fn registry() -> super::ToolRegistry {
        super::ToolRegistry::new(
            std::path::PathBuf::from("/nonexistent"),
            std::path::PathBuf::from("/nonexistent"),
            std::sync::Arc::new(crate::redaction::RedactionEngine::disabled()),
        )
    }

    /// The checked-in file is what the code says. Desktop only (the module
    /// is): Android has no `run_command`, no bridge and no OneBot, and the file
    /// describes the desktop build the front end ships with.
    #[test]
    fn the_checked_in_catalog_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tool-catalog.json");
        let want = serde_json::to_string_pretty(&super::catalog(&registry())).unwrap() + "\n";
        if std::env::var("UPDATE_TOOL_CATALOG").as_deref() == Ok("1") {
            std::fs::write(&path, &want).unwrap();
            return;
        }
        let have = std::fs::read_to_string(&path).unwrap_or_default().replace("\r\n", "\n");
        assert!(
            have == want,
            "tool-catalog.json is stale: run `UPDATE_TOOL_CATALOG=1 cargo test -p meridian-core --lib tools::catalog` and commit the result"
        );
    }

    /// Every tool appears once, sorted, and the bridge's own tool is among them.
    #[test]
    fn every_tool_is_listed_once() {
        let catalog = super::catalog(&registry());
        let names: Vec<&str> = catalog["native"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted, "sorted and without repeats");
        assert!(names.contains(&"conversation_usage") && names.contains(&"read_file"));
        assert_eq!(catalog["bridge_prefix"], "mcp__meridian__");
    }
}
