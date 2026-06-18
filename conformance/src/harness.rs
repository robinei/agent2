use std::collections::HashMap;
use std::path::PathBuf;

/// Pre-loaded harness files available for inclusion.
pub struct Harness {
    /// Path → source content (pre-resolved to avoid repeated disk I/O).
    entries: HashMap<String, String>,
    /// Directory for our adapted harness files (sta.js, assert.js, compareArray.js).
    local_harness: PathBuf,
    /// Directory for test262 includes (propertyHelper.js, etc.).
    test262_harness: PathBuf,
}

impl Harness {
    pub fn new(local_harness: PathBuf, test262_harness: PathBuf) -> Self {
        Harness {
            entries: HashMap::new(),
            local_harness,
            test262_harness,
        }
    }

    fn load_from(&mut self, dir: &std::path::Path, name: &str) -> Option<&str> {
        if self.entries.contains_key(name) {
            return self.entries.get(name).map(|s| s.as_str());
        }
        let path = dir.join(name);
        let content = std::fs::read_to_string(&path).ok()?;
        self.entries.insert(name.to_string(), content);
        self.entries.get(name).map(|s| s.as_str())
    }

    /// Build the full program source for a test: always-prepended harness
    /// files, then per-test `includes`, then the test body.
    pub fn build_source(&mut self, includes: &[String], test_body: &str) -> Result<String, String> {
        let mut parts: Vec<String> = Vec::new();

        // Always-included harness files from our adapted local copies.
        for required in ["sta.js", "assert.js", "compareArray.js"] {
            let dir = self.local_harness.clone();
            match self.load_from(&dir, required) {
                Some(s) => parts.push(s.to_string()),
                None => return Err(format!("failed to load harness/{required}")),
            }
        }

        // Per-test includes from the stock test262 harness directory.
        for inc in includes {
            let name = if inc.ends_with(".js") {
                inc.clone()
            } else {
                format!("{inc}.js")
            };
            let dir = self.test262_harness.clone();
            match self.load_from(&dir, &name) {
                Some(s) => parts.push(s.to_string()),
                None => return Err(format!("failed to load harness/{name}")),
            }
        }

        parts.push(test_body.to_string());

        Ok(parts.join("\n"))
    }
}
