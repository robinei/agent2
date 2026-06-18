use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ExpectedResult {
    Pass,
    Fail,
    #[serde(rename = "skip")]
    Skip(String),
    #[serde(rename = "known-divergence")]
    KnownDivergence(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Expectations {
    /// Relative test path → expected result.
    #[serde(flatten)]
    pub entries: BTreeMap<String, ExpectedResult>,
}

impl Expectations {
    pub fn load(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Expectations::default());
        }
        let data = fs::read_to_string(path).map_err(|e| format!("read expectations: {e}"))?;
        serde_json::from_str(&data).map_err(|e| format!("parse expectations: {e}"))
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("serialize expectations: {e}"))?;
        fs::write(path, json).map_err(|e| format!("write expectations: {e}"))?;
        Ok(())
    }
}
