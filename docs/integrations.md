# AI Agent Framework & SDK Integrations

DroidPath exposes **zero-parser, ready-to-use JSON tool schemas** in OpenAI, Anthropic, and MCP formats via:

- **Static files in this repo:** `schemas/openai-tools.json`, `schemas/anthropic-tools.json`, `schemas/mcp-tools.json`
- **CLI export:** `droidpath schemas --format openai|anthropic|mcp|all` or `droidpath-mcp schemas --format openai`
- **Live HTTP endpoints (when running `--http`):**
  - `GET http://127.0.0.1:8766/schemas` (all formats)
  - `GET http://127.0.0.1:8766/schemas/openai.json`
  - `GET http://127.0.0.1:8766/schemas/anthropic.json`
  - `GET http://127.0.0.1:8766/schemas/mcp.json`

---

## 1. MCP (Claude Desktop, Cursor, Gemini CLI, Antigravity)

```bash
# One-command setup (deploys daemon + configures all detected MCP clients)
droidpath init
```

Or add manually to your MCP client config:

```json
{
  "mcpServers": {
    "droidpath": {
      "command": "droidpath-mcp",
      "args": ["--stdio"]
    }
  }
}
```

---

## 2. OpenAI SDK (Function Calling)

Load `schemas/openai-tools.json` (or fetch `GET /schemas/openai.json`) directly into `client.chat.completions.create(tools=...)`:

```python
import json, requests
from openai import OpenAI

client = OpenAI()
DROIDPATH_URL = "http://127.0.0.1:8766"

# 1. Fetch standardized OpenAI function-calling schemas directly from DroidPath
tools = requests.get(f"{DROIDPATH_URL}/schemas/openai.json").json()

resp = client.chat.completions.create(
    model="gpt-4.1",
    messages=[{"role": "user", "content": "Enable Night Light in Android Settings."}],
    tools=tools,
)

# 2. Forward tool call to DroidPath JSON-RPC endpoint
call = resp.choices[0].message.tool_calls[0]
rpc = requests.post(
    f"{DROIDPATH_URL}/message",
    json={
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": call.function.name,
            "arguments": json.loads(call.function.arguments),
        },
    },
).json()
print(rpc["result"]["content"][0]["text"])
```

---

## 3. Anthropic SDK (Claude 3.5 / 3.7 Sonnet + Opt-In Hybrid Vision)

```python
import anthropic, requests

client = anthropic.Anthropic()
DROIDPATH_URL = "http://127.0.0.1:8766"

# Fetch standardized Anthropic tool definitions (with input_schema)
tools = requests.get(f"{DROIDPATH_URL}/schemas/anthropic.json").json()

msg = client.messages.create(
    model="claude-3-7-sonnet-20250219",
    max_tokens=1024,
    tools=tools,
    messages=[
        {
            "role": "user",
            "content": "Inspect the current Android screen with visual context (include_vision=true) and tell me what icon is top-right.",
        }
    ],
)
```

When the model calls `android_get_ui_tree(include_vision=true)`, DroidPath returns **multimodal MCP content** (`[{type: "text", ...}, {type: "image", mimeType: "image/jpeg", data: "..."}]`), while automatically blocking screenshots on `FLAG_SECURE` / biometric screens.

---

## 4. LangChain (`langchain-mcp-adapters`)

```python
import asyncio
from langchain_mcp_adapters.client import MultiServerMCPClient
from langgraph.prebuilt import create_react_agent

async def main():
    async with MultiServerMCPClient(
        {
            "droidpath": {
                "command": "droidpath-mcp",
                "args": ["--stdio"],
                "transport": "stdio",
            }
        }
    ) as client:
        agent = create_react_agent("anthropic:claude-3-7-sonnet-latest", client.get_tools())
        result = await agent.ainvoke(
            {"messages": [("user", "Open Display settings and turn on Night Light.")]}
        )
        print(result["messages"][-1].content)

asyncio.run(main())
```

---

## 5. CrewAI (`crewai-tools` MCPServerAdapter)

```python
from crewai import Agent, Task, Crew
from crewai_tools import MCPServerAdapter
from mcp import StdioServerParameters

server_params = StdioServerParameters(command="droidpath-mcp", args=["--stdio"])

with MCPServerAdapter(server_params) as droidpath_tools:
    android_operator = Agent(
        role="Autonomous Android Operator",
        goal="Execute multi-step Android workflows with minimal token cost",
        backstory="Uses DroidPath Shadow DOM and Layer 3 A* routing to navigate Android deterministically.",
        tools=droidpath_tools,
    )
    task = Task(
        description="Navigate to Battery Saver in Android Settings using android_navigate_to_node(auto_explore=true).",
        expected_output="Confirmation JSON showing arrival at Battery Saver.",
        agent=android_operator,
    )
    Crew(agents=[android_operator], tasks=[task]).kickoff()
```

---

## 6. Self-Correcting Agentic Feedback Loop

Whenever `android_tap_element`, `android_explore_and_click`, or `android_navigate_to_node` encounters an unfound element or route deviation, DroidPath returns a structured, **PII-redacted `feedback` payload**:

```json
{
  "status": "element_not_found",
  "message": "No matching UI element found.",
  "feedback": {
    "failure_code": "element_not_found",
    "requested_target": "Night Mode",
    "current_screen_hash": "0x...",
    "current_package": "com.android.settings",
    "is_secure_screen": false,
    "screen_delta": {
      "screen_changed": true,
      "added_count": 2,
      "removed_count": 1,
      "updated_count": 1,
      "changed_nodes": [...]
    },
    "closest_candidates": [
      { "node_id": 3, "label": "Night Light", "center_x": 540, "center_y": 700, "similarity_score": 650 }
    ],
    "scrollable_available": true,
    "suggested_next_actions": [
      "Tap closest visible candidate 'Night Light' at (540, 700) via android_tap_element."
    ]
  }
}
```

Agents self-correct in **1 turn** without extra screenshot or tree-dump round trips.
