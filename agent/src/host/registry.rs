//! Tool registry (8_HARNESS Step 5): JSON-schema'd tool definitions
//! with a handler the session loop runs on a worker thread per call.

use std::collections::HashMap;
use std::sync::Arc;

/// Hard cap on a tool/subagent result before it enters the log — an OOM
/// backstop, not a context guard (the LLM boundary clips independently).
pub const MAX_RESULT_BYTES: usize = 16 * 1024 * 1024;

/// Handlers take the positional argument array and run on a worker
/// thread; blocking is fine.
pub type ToolHandler = dyn Fn(serde_json::Value) -> Result<serde_json::Value, String> + Send + Sync;

pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON schema of the positional argument array.
    pub input_schema: serde_json::Value,
    pub handler: Box<ToolHandler>,
}

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<ToolDef>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, def: ToolDef) {
        self.tools.insert(def.name.clone(), Arc::new(def));
    }

    pub fn get(&self, name: &str) -> Option<&Arc<ToolDef>> {
        self.tools.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<ToolDef>> {
        self.tools.values()
    }

    /// This registry restricted to `allow` — a spawned agent's tool
    /// allowlist. Handlers are shared (`Arc`), so narrowing is a map
    /// filter, and the child's *card* is rendered from the narrowed
    /// registry so it never advertises a tool its calls would refuse.
    pub fn narrowed(&self, allow: &[String]) -> ToolRegistry {
        ToolRegistry {
            tools: self
                .tools
                .iter()
                .filter(|(name, _)| allow.iter().any(|a| a == *name))
                .map(|(name, def)| (name.clone(), Arc::clone(def)))
                .collect(),
        }
    }
}

/// Result-size guard: nothing oversized enters the log. Applied to tool
/// results and subagent results alike, on the worker side of the inbox.
pub fn guard_size(result: Result<serde_json::Value, String>) -> Result<serde_json::Value, String> {
    match result {
        Ok(v) => {
            let size = serde_json::to_string(&v).map(|s| s.len()).unwrap_or(0);
            if size > MAX_RESULT_BYTES {
                Err(format!(
                    "result too large: {size} bytes (limit {MAX_RESULT_BYTES}); \
                     return something smaller"
                ))
            } else {
                Ok(v)
            }
        }
        Err(mut e) => {
            e.truncate(MAX_RESULT_BYTES);
            Err(e)
        }
    }
}
