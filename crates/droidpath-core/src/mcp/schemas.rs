use serde_json::{json, Value};

use super::protocol::{DroidPathMcpServerCore, McpToolDefinition};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSchemaFormat {
    OpenAi,
    Anthropic,
    Mcp,
    All,
}

impl ToolSchemaFormat {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "openai" | "openai.json" => Some(Self::OpenAi),
            "anthropic" | "anthropic.json" | "claude" => Some(Self::Anthropic),
            "mcp" | "mcp.json" => Some(Self::Mcp),
            "all" | "bundle" => Some(Self::All),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Mcp => "mcp",
            Self::All => "all",
        }
    }
}

/// Single-source-of-truth exporter converting DroidPath's canonical MCP tool definitions
/// into ready-to-use OpenAI, Anthropic, and MCP JSON function-calling schemas.
pub struct ToolSchemaExporter;

impl ToolSchemaExporter {
    pub fn export_openai() -> Value {
        Self::to_openai(&DroidPathMcpServerCore::tool_definitions())
    }

    pub fn export_anthropic() -> Value {
        Self::to_anthropic(&DroidPathMcpServerCore::tool_definitions())
    }

    pub fn export_mcp() -> Value {
        Self::to_mcp(&DroidPathMcpServerCore::tool_definitions())
    }

    pub fn export_all() -> Value {
        let defs = DroidPathMcpServerCore::tool_definitions();
        json!({
            "schema_version": "1.0",
            "server": "droidpath-mcp",
            "server_version": DroidPathMcpServerCore::SERVER_VERSION,
            "mcp_protocol_version": DroidPathMcpServerCore::PROTOCOL_VERSION,
            "tool_count": defs.len(),
            "openai": Self::to_openai(&defs),
            "anthropic": Self::to_anthropic(&defs),
            "mcp": Self::to_mcp(&defs)
        })
    }

    pub fn export(format: ToolSchemaFormat) -> Value {
        match format {
            ToolSchemaFormat::OpenAi => Self::export_openai(),
            ToolSchemaFormat::Anthropic => Self::export_anthropic(),
            ToolSchemaFormat::Mcp => Self::export_mcp(),
            ToolSchemaFormat::All => Self::export_all(),
        }
    }

    pub fn to_openai(defs: &[McpToolDefinition]) -> Value {
        let tools: Vec<Value> = defs
            .iter()
            .map(|def| {
                json!({
                    "type": "function",
                    "function": {
                        "name": def.name,
                        "description": def.description,
                        "parameters": def.input_schema
                    }
                })
            })
            .collect();
        Value::Array(tools)
    }

    pub fn to_anthropic(defs: &[McpToolDefinition]) -> Value {
        let tools: Vec<Value> = defs
            .iter()
            .map(|def| {
                json!({
                    "name": def.name,
                    "description": def.description,
                    "input_schema": def.input_schema
                })
            })
            .collect();
        Value::Array(tools)
    }

    pub fn to_mcp(defs: &[McpToolDefinition]) -> Value {
        json!({
            "tools": defs
        })
    }
}
