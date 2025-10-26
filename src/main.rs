use axum::{
    Router,
    body::{Body, Bytes},
    extract::Request,
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::any,
};
use clap::Parser;
use futures::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::OnceLock;
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

static CONFIG: OnceLock<Config> = OnceLock::new();
static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

#[derive(Parser, Debug, Clone)]
#[command(name = "copilot-lmstudio-proxy")]
#[command(about = "A proxy to fix compatibility issues between GitHub Copilot Chat and LMStudio", long_about = None)]
struct Config {
    /// Port to listen on
    #[arg(short, long, default_value_t = 3000)]
    port: u16,

    /// LMStudio base URL
    #[arg(short, long, default_value = "http://localhost:1234")]
    lmstudio_url: String,

    /// Bind to all interfaces (0.0.0.0) instead of localhost only
    #[arg(short, long, default_value_t = false)]
    bind_all: bool,

    /// Enable CORS (Cross-Origin Resource Sharing)
    #[arg(short, long, default_value_t = false)]
    cors: bool,
}

#[tokio::main]
async fn main() {
    // Parse CLI arguments
    let config = Config::parse();
    CONFIG.set(config.clone()).expect("Failed to set config");

    // Initialize HTTP client (reused for all requests for connection pooling)
    let client = reqwest::Client::builder()
        .http1_only() // LMStudio might not support HTTP/2
        .build()
        .expect("Failed to create HTTP client");
    HTTP_CLIENT.set(client).expect("Failed to set HTTP client");

    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "copilot_lmstudio_proxy=info,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let bind_addr = if config.bind_all {
        format!("0.0.0.0:{}", config.port)
    } else {
        format!("127.0.0.1:{}", config.port)
    };

    info!("Copilot-LMStudio Proxy starting");
    info!("  Listening: http://{}", bind_addr);
    info!("  Upstream: {}", config.lmstudio_url);
    if config.cors {
        info!("  CORS: enabled");
    }

    let mut app = Router::new().fallback(any(proxy_handler));

    // Add CORS layer if enabled
    if config.cors {
        use tower_http::cors::{Any, CorsLayer};
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);
        app = app.layer(cors);
    }

    let listener = tokio::net::TcpListener::bind(&bind_addr).await.unwrap();

    info!("Proxy ready!");
    axum::serve(listener, app).await.unwrap();
}

async fn proxy_handler(req: Request) -> Result<Response, StatusCode> {
    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let uri = parts.uri.clone();
    let path = uri.path();
    let query = uri.query().unwrap_or("");

    info!(
        "{} {} {}",
        method,
        path,
        if query.is_empty() {
            ""
        } else {
            &format!("?{}", query)
        }
    );

    // Read the original body
    let body_bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            error!("Failed to read request body: {}", e);
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    let config = CONFIG.get().expect("Config not initialized");

    // Try to parse and fix the body if it's JSON
    let fixed_body_bytes = if !body_bytes.is_empty() && is_json_request(&parts.headers) {
        match fix_request_body(&body_bytes) {
            Ok(fixed) => fixed,
            Err(e) => {
                warn!("Could not fix request body: {}", e);
                body_bytes
            }
        }
    } else {
        body_bytes
    };

    // Build the upstream URL
    let lmstudio_base = config.lmstudio_url.trim_end_matches('/');
    let upstream_url = format!("{}{}", lmstudio_base, path);
    let upstream_url_with_query = if query.is_empty() {
        upstream_url.clone()
    } else {
        format!("{}?{}", upstream_url, query)
    };

    // Create upstream request using the shared client
    let client = HTTP_CLIENT.get().expect("HTTP client not initialized");
    let mut upstream_req = client.request(method.clone(), &upstream_url_with_query);

    // Copy headers (except Host and problematic headers)
    for (name, value) in parts.headers.iter() {
        let name_str = name.as_str();
        // Skip host and headers that might cause issues. Reqwest recalculates
        // connection management, compression, and body length on our behalf.
        if name_str == "host"
            || name_str.starts_with("sec-")
            || name_str == "connection"
            || name_str == "accept-encoding"
            || name_str == "content-length"
        {
            continue;
        }

        upstream_req = upstream_req.header(name, value);
    }

    // Add body
    upstream_req = upstream_req.body(fixed_body_bytes);

    // Send request to LMStudio
    let upstream_response = match upstream_req.send().await {
        Ok(resp) => resp,
        Err(e) => {
            error!("Failed to proxy request: {}", e);
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    let status = upstream_response.status();
    let mut headers = upstream_response.headers().clone();

    if !status.is_success() {
        warn!("Response: {}", status);
    }

    // Check if this is a streaming response
    let is_streaming = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);

    // Strip hop-by-hop and encoding headers after reqwest's automatic decompression
    sanitize_response_headers(&mut headers);

    if is_streaming {
        // Handle streaming response
        let stream = upstream_response.bytes_stream();
        let fixed_stream = stream.map(move |chunk_result| match chunk_result {
            Ok(chunk) => match fix_streaming_chunk(&chunk) {
                Ok(fixed) => Ok(fixed),
                Err(_) => Ok(chunk),
            },
            Err(e) => Err(std::io::Error::other(e)),
        });

        let body = Body::from_stream(fixed_stream);
        let mut response = Response::new(body);
        *response.status_mut() = status;
        *response.headers_mut() = headers;

        Ok(response)
    } else {
        // Handle non-streaming response
        let body_bytes = match upstream_response.bytes().await {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Failed to read response body: {}", e);
                return Err(StatusCode::BAD_GATEWAY);
            }
        };

        let fixed_body_bytes = if is_json_response(&headers) {
            match fix_response_body(&body_bytes) {
                Ok(fixed) => fixed,
                Err(_) => body_bytes,
            }
        } else {
            body_bytes
        };

        let mut response = Response::new(Body::from(fixed_body_bytes));
        *response.status_mut() = status;
        *response.headers_mut() = headers;

        Ok(response)
    }
}

fn is_json_request(headers: &HeaderMap) -> bool {
    headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("application/json"))
        .unwrap_or(false)
}

fn is_json_response(headers: &HeaderMap) -> bool {
    headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("application/json"))
        .unwrap_or(false)
}

fn fix_request_body(body: &Bytes) -> Result<Bytes, Box<dyn std::error::Error>> {
    let mut json: Value = serde_json::from_slice(body)?;

    // Fix tools array (Issue #2)
    if let Some(tools) = json.get_mut("tools").and_then(|t| t.as_array_mut()) {
        let mut fixed_count = 0;
        for tool in tools.iter_mut() {
            // Handle both formats:
            // 1. OpenAI function calling: tool.function.parameters
            // 2. Direct format: tool.parameters
            let parameters_ref = if let Some(function) = tool.get_mut("function") {
                function.get_mut("parameters")
            } else {
                tool.get_mut("parameters")
            };

            if let Some(parameters) = parameters_ref {
                // If parameters is an object without a type field, or an empty object
                if parameters.is_object() {
                    let params_obj = parameters.as_object_mut().unwrap();
                    if !params_obj.contains_key("type") {
                        params_obj.insert("type".to_string(), json!("object"));
                        if !params_obj.contains_key("properties") {
                            params_obj.insert("properties".to_string(), json!({}));
                        }
                        fixed_count += 1;
                    }
                }
            }
        }
        if fixed_count > 0 {
            info!("Fixed {} tool parameter schema(s)", fixed_count);
        }
    }

    Ok(Bytes::from(serde_json::to_vec(&json)?))
}

fn fix_response_body(body: &Bytes) -> Result<Bytes, Box<dyn std::error::Error>> {
    let mut json: Value = serde_json::from_slice(body)?;

    // Fix usage details (Issue #1)
    if let Some(usage) = json.get_mut("usage").and_then(|u| u.as_object_mut()) {
        let mut fixed = false;

        if !usage.contains_key("input_tokens_details") {
            usage.insert(
                "input_tokens_details".to_string(),
                json!({"cached_tokens": 0}),
            );
            fixed = true;
        }

        if !usage.contains_key("output_tokens_details") {
            usage.insert(
                "output_tokens_details".to_string(),
                json!({"reasoning_tokens": 0}),
            );
            fixed = true;
        }

        if fixed {
            info!("Fixed usage details in response");
        }
    }

    Ok(Bytes::from(serde_json::to_vec(&json)?))
}

fn fix_streaming_chunk(chunk: &Bytes) -> Result<Bytes, Box<dyn std::error::Error>> {
    let chunk_str = std::str::from_utf8(chunk)?;

    // SSE format: "data: {...}\n\n"
    if !chunk_str.starts_with("data: ") {
        return Ok(chunk.clone());
    }

    // Extract the JSON part
    let data_line = chunk_str.trim_start_matches("data: ").trim();

    // Skip [DONE] marker
    if data_line == "[DONE]" {
        return Ok(chunk.clone());
    }

    // Try to parse and fix the JSON
    let mut json: Value = match serde_json::from_str(data_line) {
        Ok(j) => j,
        Err(_) => return Ok(chunk.clone()),
    };

    let mut fixed = false;

    // Fix for Responses API streaming
    if let Some(response) = json.get_mut("response")
        && let Some(usage) = response.get_mut("usage").and_then(|u| u.as_object_mut())
    {
        if !usage.contains_key("input_tokens_details") {
            usage.insert(
                "input_tokens_details".to_string(),
                json!({"cached_tokens": 0}),
            );
            fixed = true;
        }
        if !usage.contains_key("output_tokens_details") {
            usage.insert(
                "output_tokens_details".to_string(),
                json!({"reasoning_tokens": 0}),
            );
            fixed = true;
        }
    }

    if fixed {
        let fixed_json_str = serde_json::to_string(&json)?;
        let fixed_chunk = format!("data: {}\n\n", fixed_json_str);
        Ok(Bytes::from(fixed_chunk))
    } else {
        Ok(chunk.clone())
    }
}

fn sanitize_response_headers(headers: &mut HeaderMap) {
    // These headers no longer reflect reality after reqwest decompressed the payload.
    headers.remove("content-encoding");
    headers.remove("transfer-encoding");
    headers.remove("content-length");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use bytes::Bytes;

    #[test]
    fn fixes_missing_tool_parameter_schema() {
        let input = json!({
            "tools": [
                { "function": { "parameters": {} } },
                { "parameters": {} },
                {
                    "function": {
                        "parameters": {
                            "type": "object",
                            "properties": { "foo": { "type": "string" } }
                        }
                    }
                }
            ]
        });

        let bytes = Bytes::from(serde_json::to_vec(&input).unwrap());
        let fixed = fix_request_body(&bytes).expect("request body fix should succeed");
        let fixed_json: Value = serde_json::from_slice(&fixed).unwrap();
        let tools = fixed_json["tools"]
            .as_array()
            .expect("tools should remain an array");

        let first_params = tools[0]["function"]["parameters"].as_object().unwrap();
        assert_eq!(first_params["type"], "object");
        assert!(first_params["properties"].as_object().unwrap().is_empty());

        let second_params = tools[1]["parameters"].as_object().unwrap();
        assert_eq!(second_params["type"], "object");
        assert!(second_params["properties"].as_object().unwrap().is_empty());

        let third_params = tools[2]["function"]["parameters"].as_object().unwrap();
        assert_eq!(third_params["type"], "object");
        assert_eq!(
            third_params["properties"].as_object().unwrap()["foo"],
            json!({ "type": "string" })
        );
    }

    #[test]
    fn adds_missing_usage_details() {
        let input = json!({
            "usage": {}
        });

        let bytes = Bytes::from(serde_json::to_vec(&input).unwrap());
        let fixed = fix_response_body(&bytes).expect("response body fix should succeed");
        let fixed_json: Value = serde_json::from_slice(&fixed).unwrap();
        let usage = fixed_json["usage"].as_object().unwrap();

        assert_eq!(usage["input_tokens_details"], json!({ "cached_tokens": 0 }));
        assert_eq!(
            usage["output_tokens_details"],
            json!({ "reasoning_tokens": 0 })
        );
    }

    #[test]
    fn fixes_streaming_usage_chunks() {
        let chunk = Bytes::from("data: {\"response\":{\"usage\":{}}}\n\n");
        let fixed = fix_streaming_chunk(&chunk).expect("stream chunk fix should succeed");
        assert_ne!(fixed, chunk);

        let fixed_str = std::str::from_utf8(&fixed).unwrap();
        assert!(fixed_str.contains("input_tokens_details"));
        assert!(fixed_str.contains("output_tokens_details"));
    }

    #[test]
    fn leaves_done_streaming_marker_untouched() {
        let chunk = Bytes::from("data: [DONE]\n\n");
        let fixed = fix_streaming_chunk(&chunk).expect("[DONE] chunk fix should succeed");
        assert_eq!(fixed, chunk);
    }

    #[test]
    fn sanitizes_decompressed_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", HeaderValue::from_static("gzip"));
        headers.insert("transfer-encoding", HeaderValue::from_static("chunked"));
        headers.insert("content-length", HeaderValue::from_static("42"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));

        sanitize_response_headers(&mut headers);

        assert!(headers.get("content-encoding").is_none());
        assert!(headers.get("transfer-encoding").is_none());
        assert!(headers.get("content-length").is_none());
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
    }
}
