//! `mcp_servers`: the MCP servers the user has configured.
//!
//! `args`, `env` and `headers` are JSON text and decode at the read: a row whose
//! `env` will not parse fails the query, rather than starting a server without
//! its token — which fails every call with a 401 that looks like the user's key
//! is wrong.

use std::collections::BTreeMap;

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::types::{EpochMs, Json, SqlBool};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "mcp_servers")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub name: String,
    pub transport_type: McpTransport,
    pub command: Option<String>,
    pub args: Option<Json<Vec<String>>>,
    pub env: Option<Json<BTreeMap<String, String>>>,
    pub url: Option<String>,
    pub headers: Option<Json<BTreeMap<String, String>>>,
    /// Connect at startup. Not "usable": a disabled server can still be
    /// connected by hand from the settings page.
    pub is_enabled: SqlBool,
    pub sort_order: i32,
    pub created_at: EpochMs,
    pub updated_at: EpochMs,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// How a server is reached.
///
/// The column has no `CHECK`, so this type is the only thing holding the list
/// closed: an unknown spelling fails the read. The stored spelling
/// (`string_value`) and the wire spelling (serde) are one list; a test below
/// holds them together. `EnumIter` is SeaORM's re-export, which `ActiveEnum`
/// requires.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    EnumIter,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    DeriveActiveEnum,
)]
#[sea_orm(rs_type = "String", db_type = "Text")]
pub enum McpTransport {
    #[serde(rename = "stdio")]
    #[strum(serialize = "stdio")]
    #[sea_orm(string_value = "stdio")]
    Stdio,
    #[serde(rename = "streamablehttp")]
    #[strum(serialize = "streamablehttp")]
    #[sea_orm(string_value = "streamablehttp")]
    StreamableHttp,
}

impl McpTransport {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        value.parse().map_err(|_| format!("unknown MCP transport `{value}`"))
    }
}

/// A partial update: a field left `None` is not written. The nullable columns
/// are `Option<Option<_>>`, so `Some(None)` clears one.
#[derive(Debug, Default, DeriveIntoActiveModel)]
pub struct McpServerChangeset {
    pub name: Option<String>,
    pub transport_type: Option<McpTransport>,
    pub command: Option<Option<String>>,
    pub args: Option<Option<Json<Vec<String>>>>,
    pub env: Option<Option<Json<BTreeMap<String, String>>>>,
    pub url: Option<Option<String>>,
    pub headers: Option<Option<Json<BTreeMap<String, String>>>>,
    pub is_enabled: Option<SqlBool>,
    pub updated_at: Option<EpochMs>,
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveEnum, Iterable};

    use super::*;

    #[test]
    fn transport_contract_is_closed() {
        assert_eq!(McpTransport::parse("stdio").unwrap(), McpTransport::Stdio);
        assert_eq!(
            McpTransport::parse("streamablehttp").unwrap(),
            McpTransport::StreamableHttp
        );
        assert!(McpTransport::parse("sse").is_err());
        assert!(serde_json::from_str::<McpTransport>(r#""future""#).is_err());
    }

    #[test]
    fn stored_and_wire_spellings_are_one_list() {
        for transport in McpTransport::iter() {
            let wire = serde_json::to_value(transport).unwrap();
            assert_eq!(wire.as_str(), Some(transport.to_value().as_str()));
            assert_eq!(transport.as_str(), transport.to_value());
        }
    }
}
