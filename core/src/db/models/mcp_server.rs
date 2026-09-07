use diesel::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::schema::mcp_servers;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::EnumString, strum::IntoStaticStr)]
pub enum McpTransport {
    #[serde(rename = "stdio")]
    #[strum(serialize = "stdio")]
    Stdio,
    #[serde(rename = "streamablehttp")]
    #[strum(serialize = "streamablehttp")]
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

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = mcp_servers)]
pub struct McpServerRow {
    pub id: String,
    pub name: String,
    pub transport_type: String,
    pub command: Option<String>,
    pub args: Option<String>,
    pub env: Option<String>,
    pub url: Option<String>,
    pub is_enabled: i32,
    pub sort_order: i32,
    pub created_at: i64,
    pub updated_at: i64,
    pub headers: Option<String>,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = mcp_servers)]
pub struct McpServerInsert<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub transport_type: &'a str,
    pub command: Option<&'a str>,
    pub args: Option<&'a str>,
    pub env: Option<&'a str>,
    pub url: Option<&'a str>,
    pub headers: Option<&'a str>,
    pub is_enabled: i32,
    pub sort_order: i32,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, AsChangeset, Default)]
#[diesel(table_name = mcp_servers)]
pub struct McpServerChangeset {
    pub name: Option<String>,
    pub transport_type: Option<String>,
    pub command: Option<Option<String>>,
    pub args: Option<Option<String>>,
    pub env: Option<Option<String>>,
    pub url: Option<Option<String>>,
    pub headers: Option<Option<String>>,
    pub is_enabled: Option<i32>,
    pub updated_at: Option<i64>,
}

#[cfg(test)]
mod tests {
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
}
