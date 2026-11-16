//! Config file: load, validate, save (atomic), locate.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::topology::Topology;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Affinity,
    Cpusets,
    Both,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Affinity => "affinity",
            Mode::Cpusets => "cpusets",
            Mode::Both => "both",
        }
    }
    pub fn parse(s: &str) -> Option<Mode> {
        match s.to_ascii_lowercase().as_str() {
            "affinity" => Some(Mode::Affinity),
            "cpusets" => Some(Mode::Cpusets),
            "both" => Some(Mode::Both),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Profile {
    pub name: String,
    pub cpus: String,
}

fn default_true() -> bool {
    true
}
fn default_poll() -> u32 {
    1000
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Rule {
    #[serde(rename = "match")]
    pub pattern: String,
    pub profile: String,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default = "default_true")]
    pub enforce: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_poll")]
    pub poll_interval_ms: u32,
    /// "Start with Windows" uses an elevated Task Scheduler logon task
    /// (true, default) or the unelevated HKCU Run key (false).
    #[serde(default = "default_true")]
    pub elevated_autostart: bool,
    /// Path of the PresentMon console application, for `benchmark`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub present_mon: Option<String>,
    #[serde(default)]
    pub profiles: Vec<Profile>,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl Config {
    pub fn defaults(topo: &Topology) -> Config {
        Config {
            poll_interval_ms: 1000,
            elevated_autostart: true,
            present_mon: None,
            profiles: topo.default_profiles(),
            rules: Vec::new(),
        }
    }

    pub fn poll_ms(&self) -> u32 {
        self.poll_interval_ms.clamp(250, 5000)
    }

    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }

    pub fn rule_index(&self, pattern: &str) -> Option<usize> {
        self.rules.iter().position(|r| r.pattern.eq_ignore_ascii_case(pattern))
    }

    /// Create or update the rule for `pattern`. Returns true when created.
    pub fn upsert_rule(&mut self, pattern: &str, profile: &str, mode: Option<Mode>) -> bool {
        match self.rule_index(pattern) {
            Some(i) => {
                self.rules[i].profile = profile.to_string();
                if let Some(m) = mode {
                    self.rules[i].mode = m;
                }
                false
            }
            None => {
                self.rules.push(Rule {
                    pattern: pattern.to_string(),
                    profile: profile.to_string(),
                    mode: mode.unwrap_or_default(),
                    enforce: true,
                });
                true
            }
        }
    }
}

/// `%AppData%\Pinfold`, or `PINFOLD_DIR` when set (used for testing).
pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("PINFOLD_DIR") {
        return PathBuf::from(d);
    }
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("Pinfold")
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join("config.json")
}

pub fn log_path(dir: &Path) -> PathBuf {
    dir.join("pinfold.log")
}

pub fn mtime(dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(path(dir)).and_then(|m| m.modified()).ok()
}

pub fn load(dir: &Path) -> Result<Config, String> {
    let text =
        std::fs::read_to_string(path(dir)).map_err(|e| format!("cannot read config: {e}"))?;
    // Tolerate a UTF-8 BOM written by some editors.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    serde_json::from_str(text).map_err(|e| format!("config.json: {e}"))
}

/// Load the config; on first run create it from the topology.
/// A broken file is never overwritten: defaults are used in memory only.
/// The second value is a message for the log, if anything noteworthy happened.
pub fn load_or_create(dir: &Path, topo: &Topology) -> (Config, Option<String>) {
    if !path(dir).exists() {
        let cfg = Config::defaults(topo);
        let note = match save(dir, &cfg) {
            Ok(()) => format!("created {}", path(dir).display()),
            Err(e) => format!("config error: cannot write config: {e}"),
        };
        return (cfg, Some(note));
    }
    match load(dir) {
        Ok(cfg) => (cfg, None),
        Err(e) => (
            Config::defaults(topo),
            Some(format!("config error: {e}; using defaults, no rules active")),
        ),
    }
}

/// Atomic write: temp file, then replace.
pub fn save(dir: &Path, cfg: &Config) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut text = serde_json::to_string_pretty(cfg).map_err(std::io::Error::other)?;
    text.push('\n');
    let tmp = dir.join("config.json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spec_example() {
        let text = r#"{
          "pollIntervalMs": 1000,
          "profiles": [
            { "name": "Cache",     "cpus": "0-15"  },
            { "name": "Frequency", "cpus": "16-31" }
          ],
          "rules": [
            { "match": "cyberpunk2077.exe", "profile": "Cache", "mode": "affinity", "enforce": true },
            { "match": "D:\\Tools\\encoder.exe", "profile": "Frequency" }
          ]
        }"#;
        let cfg: Config = serde_json::from_str(text).unwrap();
        assert_eq!(cfg.rules.len(), 2);
        assert_eq!(cfg.rules[1].mode, Mode::Affinity);
        assert!(cfg.rules[1].enforce);
        assert!(cfg.profile("cache").is_some());
    }

    #[test]
    fn upsert() {
        let mut cfg = Config {
            poll_interval_ms: 100,
            elevated_autostart: true,
            present_mon: None,
            profiles: vec![],
            rules: vec![],
        };
        assert!(cfg.upsert_rule("Foo.exe", "Cache", None));
        assert!(!cfg.upsert_rule("foo.EXE", "Frequency", Some(Mode::Both)));
        assert_eq!(cfg.rules.len(), 1);
        assert_eq!(cfg.rules[0].profile, "Frequency");
        assert_eq!(cfg.rules[0].mode, Mode::Both);
        assert_eq!(cfg.poll_ms(), 250);
    }
}
