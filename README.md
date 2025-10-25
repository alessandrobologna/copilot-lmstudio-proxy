# Copilot-LMStudio Proxy

A lightweight HTTP proxy that fixes compatibility issues between GitHub Copilot Chat and LMStudio.

## Issues Fixed

### Issue #1: Missing `input_tokens_details` in Responses API
- **Problem:** LMStudio doesn't include `input_tokens_details.cached_tokens` in usage responses
- **Fix:** Automatically adds `input_tokens_details: { cached_tokens: 0 }` to responses
- **Location:** `src/platform/endpoint/node/responsesApi.ts:468,471` in vscode-copilot-chat

### Issue #2: Tool Parameters Missing `type: "object"`
- **Problem:** Copilot sends tools with `parameters: {}` instead of `parameters: { type: "object", properties: {} }`
- **Fix:** Automatically adds `type: "object"` to all tool parameters
- **Affected Tools:** `terminal_last_command`, `terminal_selection`, and any tool with `inputSchema: undefined`

## Quick Start

### Build

```bash
# Debug build
cargo build

# Release build (optimized, single binary)
cargo build --release
```

The release binary will be at `target/release/copilot-lmstudio-proxy`

### Run

```bash
# Development (default: localhost:3000 -> http://localhost:1234)
cargo run

# Or run the release binary directly
./target/release/copilot-lmstudio-proxy

# With custom configuration
./target/release/copilot-lmstudio-proxy --port 8080 --lmstudio-url http://studio.local:1234

# Bind to all interfaces (accessible from network)
./target/release/copilot-lmstudio-proxy --bind-all

# Enable CORS for browser-based clients
./target/release/copilot-lmstudio-proxy --cors

# See all options
./target/release/copilot-lmstudio-proxy --help
```

**CLI Options:**
- `-p, --port <PORT>` - Port to listen on (default: 3000)
- `-l, --lmstudio-url <URL>` - LMStudio base URL (default: http://localhost:1234)
- `-b, --bind-all` - Bind to 0.0.0.0 instead of 127.0.0.1
- `-c, --cors` - Enable CORS (Cross-Origin Resource Sharing)

### Configure Copilot

Update your VS Code settings to point to the proxy:

```json
{
  "github.copilot.advanced.customOAIModels": {
    "your-model-name": {
      "name": "your-model-name",
      "url": "http://localhost:3000/v1",
      "toolCalling": true,
      "vision": false,
      "thinking": true,
      "maxInputTokens": 131072,
      "maxOutputTokens": 131072,
      "requiresAPIKey": false
    }
  }
}
```

## Features

- Fixes tool parameter schemas on-the-fly
- Adds missing usage token details
- Handles both streaming and non-streaming responses
- Zero-copy for non-fixable requests
- Detailed logging
- Single self-contained binary

## Configuration

Use CLI arguments to configure the proxy (see `--help` for all options). No code changes needed!

## Logging

Set the `RUST_LOG` environment variable for detailed logs:

```bash
# More verbose
RUST_LOG=debug cargo run

# Less verbose
RUST_LOG=warn cargo run
```

## How It Works

1. **Intercepts** requests from Copilot to LMStudio
2. **Fixes** tool parameters by adding `type: "object"` if missing
3. **Proxies** to LMStudio
4. **Fixes** responses by adding `input_tokens_details` if missing
5. **Returns** fixed response to Copilot

## Performance

- Compiled binary size: ~3MB (with `strip = true`)
- Memory usage: ~5MB idle
- Latency overhead: <1ms (JSON parsing only when needed)

## Background

### Why This Proxy Is Needed

Starting with LMStudio 0.3.29, two compatibility issues emerged between GitHub Copilot Chat and LMStudio:

**Issue #1: Missing Optional Chaining in Copilot**

GitHub Copilot Chat's Responses API handler (`responsesApi.ts:468,471`) attempts to access `usage?.input_tokens_details.cached_tokens` without proper optional chaining. When LMStudio returns a usage object without the `input_tokens_details` field (which is optional in the OpenAI spec), Copilot crashes with:
```
TypeError: Cannot read properties of undefined (reading 'cached_tokens')
```

This bug has existed in Copilot since August 2025 but was only exposed when LMStudio added Responses API support.

**Issue #2: Incomplete Tool Schema Validation in Copilot**

Copilot's tool schema normalizer (`toolSchemaNormalizer.ts:58-65`) exits early when `parameters === undefined`, failing to validate empty parameter objects `{}`. Meanwhile, LMStudio 0.3.29+ implemented strict JSON Schema validation requiring `type: "object"` for all function parameters, causing requests with malformed tool schemas to fail with:
```
Invalid discriminator value. Expected 'object' at tools.X.parameters.type
```

Affected tools include `terminal_last_command` and `terminal_selection`, which have `inputSchema: undefined` in Copilot's codebase.

### The Solution

This proxy sits between Copilot and LMStudio, transparently fixing both issues:
- Adds `input_tokens_details: { cached_tokens: 0 }` to all responses
- Adds `type: "object"` to tool parameters missing the field

Both bugs have been documented in the included `copilot-chat-issue.md` and `lmstudio-issue.md` files for upstream reporting.

## Contributing

Found a bug? Open an issue or PR!

## License

MIT

## Credits

Built to work around compatibility issues between:
- [GitHub Copilot Chat](https://github.com/microsoft/vscode-copilot-chat)
- [LMStudio](https://lmstudio.ai/)
