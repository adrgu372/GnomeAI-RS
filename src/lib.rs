#![recursion_limit = "256"]
#[path = "agent.rs"]
mod agent;
#[path = "app_dirs.rs"]
mod app_dirs;
#[path = "apply_patch.rs"]
mod apply_patch;
#[path = "avalonia_bridge.rs"]
mod avalonia_bridge;
#[path = "codex_app_server.rs"]
mod codex_app_server;
#[path = "config.rs"]
mod config;
#[path = "desktop.rs"]
mod desktop;
#[cfg(target_os = "linux")]
#[path = "desktop_a11y.rs"]
mod desktop_a11y;
#[path = "embeddings.rs"]
mod embeddings;
#[path = "firecrawl.rs"]
mod firecrawl;
#[path = "llama.rs"]
mod llama;
#[path = "mcp_client.rs"]
mod mcp_client;
#[path = "memory.rs"]
mod memory;
#[path = "memory_engine.rs"]
mod memory_engine;
#[path = "native_service.rs"]
mod native_service;
#[path = "node_protocol.rs"]
mod node_protocol;
#[path = "nodes.rs"]
mod nodes;
#[path = "openrouter.rs"]
mod openrouter;
#[path = "privilege.rs"]
mod privilege;
#[path = "protocol.rs"]
pub mod protocol;
#[path = "provider.rs"]
mod provider;
#[path = "provider_catalog.rs"]
mod provider_catalog;
#[cfg(target_os = "linux")]
#[path = "sandbox.rs"]
mod sandbox;
#[cfg(target_os = "macos")]
#[path = "sandbox_macos.rs"]
mod sandbox;
#[path = "skills.rs"]
mod skills;
#[path = "storage.rs"]
mod storage;
#[path = "store.rs"]
mod store;
#[path = "tooling.rs"]
mod tooling;
#[cfg(not(target_os = "android"))]
#[path = "coding_tools.rs"]
mod tools;
#[cfg(target_os = "android")]
#[path = "mobile_tools.rs"]
mod tools;
#[path = "transcribe.rs"]
mod transcribe;
#[path = "uploads.rs"]
mod uploads;
#[path = "verify.rs"]
mod verify;
#[path = "workspaces.rs"]
mod workspaces;

mod agent_runtime;
mod brave;
mod device_store;
mod mesh_transport;
mod native_bridge;
#[cfg(target_os = "android")]
#[path = "sandbox_android.rs"]
mod sandbox;
pub use agent_runtime::run_desktop;
