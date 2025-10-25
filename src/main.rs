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

    info!("Starting Copilot-LMStudio Proxy");
    info!("Listening on: http://{}", bind_addr);
    info!("Proxying to: {}", config.lmstudio_url);
    if config.cors {
        info!("CORS: Enabled");
    }
    info!("");
    info!("Fixes:");
    info!("  1. Adds type: 'object' to tool parameters");
    info!("  2. Adds input_tokens_details to usage responses");
    info!("");

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
    let config = CONFIG.get().expect("Config not initialized");
    let lmstudio_base = config.lmstudio_url.trim_end_matches('/');
    let upstream_url = format!("{}{}", lmstudio_base, path);
    let upstream_url_with_query = if query.is_empty() {
        upstream_url.clone()
    } else {
        format!("{}?{}", upstream_url, query)
    };

    // Create upstream request
    let client = reqwest::Client::new();
    let mut upstream_req = client.request(method.clone(), &upstream_url_with_query);

    // Copy headers (except Host)
    for (name, value) in parts.headers.iter() {
        if name != "host" {
            upstream_req = upstream_req.header(name, value);
        }
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
    let headers = upstream_response.headers().clone();

    info!("Response: {}", status);

    // Check if this is a streaming response
    let is_streaming = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);

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
            if let Some(function) = tool.get_mut("function")
                && let Some(parameters) = function.get_mut("parameters")
            {
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
