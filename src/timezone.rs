use crate::error::Error;
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::str::FromStr;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TimezoneConfig {
    #[serde(default = "default_true")]
    pub use_system: bool,
    #[serde(default)]
    pub manual_primary: Option<String>,
    #[serde(default)]
    pub additional: Vec<String>,
    #[serde(default)]
    pub last_known_system_tz: Option<String>,
    #[serde(default)]
    pub dismissed_change_to: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedTimezones {
    pub primary: String,
    pub display: Vec<String>,
    pub system: String,
    pub system_changed: bool,
    pub use_system: bool,
}

pub fn load_config(config_path: &Path, env_override: Option<&str>) -> TimezoneConfig {
    if let Some(json_str) = env_override {
        return serde_json::from_str(json_str).unwrap_or_default();
    }
    if config_path.exists() {
        let content = match std::fs::read_to_string(config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to read timezone config: {e}");
                return TimezoneConfig::default();
            }
        };
        return serde_json::from_str(&content).unwrap_or_default();
    }
    TimezoneConfig {
        use_system: true,
        ..Default::default()
    }
}

/// Strict variant for startup validation: reports parse/IO errors instead of
/// silently falling back to default. Returns `Ok(None)` when the file is
/// missing (a missing file is normal on first run, not an error). The
/// route-handler path keeps using `load_config` so a transient read failure
/// never 500s a live request.
pub fn try_load_config(config_path: &Path) -> Result<Option<TimezoneConfig>, String> {
    if !config_path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(config_path).map_err(|e| format!("read failed: {e}"))?;
    serde_json::from_str::<TimezoneConfig>(&content)
        .map(Some)
        .map_err(|e| format!("JSON parse failed: {e}"))
}

pub fn save_config(config: &TimezoneConfig, config_path: &Path) -> Result<(), Error> {
    let json = serde_json::to_string_pretty(config)?;
    crate::accounts::atomic_write_bytes(config_path, json.as_bytes(), /* secret */ false)?;
    Ok(())
}

pub fn detect_system_tz() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string())
}

pub fn validate_iana(name: &str) -> bool {
    Tz::from_str(name).is_ok()
}

pub fn resolve(cfg: &TimezoneConfig) -> ResolvedTimezones {
    let system = detect_system_tz();

    let primary = if cfg.use_system {
        system.clone()
    } else {
        cfg.manual_primary
            .clone()
            .filter(|s| validate_iana(s))
            .unwrap_or_else(|| system.clone())
    };

    let mut display = Vec::with_capacity(1 + cfg.additional.len());
    display.push(primary.clone());
    for tz in &cfg.additional {
        if validate_iana(tz) && !display.iter().any(|t| t == tz) {
            display.push(tz.clone());
        }
    }

    let system_changed = match &cfg.last_known_system_tz {
        Some(last) => {
            last != &system
                && cfg
                    .dismissed_change_to
                    .as_ref()
                    .map(|d| d != &system)
                    .unwrap_or(true)
        }
        None => false,
    };

    ResolvedTimezones {
        primary,
        display,
        system,
        system_changed,
        use_system: cfg.use_system,
    }
}

/// Return the primary timezone parsed as a chrono_tz::Tz, falling back to UTC on parse failure.
pub fn primary_tz(cfg: &TimezoneConfig) -> Tz {
    let resolved = resolve(cfg);
    Tz::from_str(&resolved.primary).unwrap_or(Tz::UTC)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn validate_iana_accepts_known_names() {
        assert!(validate_iana("America/Los_Angeles"));
        assert!(validate_iana("Europe/London"));
        assert!(validate_iana("UTC"));
    }

    #[test]
    fn validate_iana_rejects_garbage() {
        assert!(!validate_iana("Not/A/Zone"));
        assert!(!validate_iana(""));
    }

    #[test]
    fn detect_system_tz_returns_something() {
        let tz = detect_system_tz();
        assert!(!tz.is_empty());
    }

    #[test]
    fn resolve_use_system_uses_detected() {
        let cfg = TimezoneConfig {
            use_system: true,
            manual_primary: Some("Europe/London".into()),
            ..Default::default()
        };
        let r = resolve(&cfg);
        assert_eq!(r.primary, detect_system_tz());
        assert_eq!(r.display, vec![detect_system_tz()]);
    }

    #[test]
    fn resolve_manual_overrides_when_use_system_false() {
        let cfg = TimezoneConfig {
            use_system: false,
            manual_primary: Some("Europe/London".into()),
            additional: vec!["America/New_York".into()],
            ..Default::default()
        };
        let r = resolve(&cfg);
        assert_eq!(r.primary, "Europe/London");
        assert_eq!(r.display, vec!["Europe/London", "America/New_York"]);
    }

    #[test]
    fn resolve_invalid_manual_falls_back_to_system() {
        let cfg = TimezoneConfig {
            use_system: false,
            manual_primary: Some("Bogus/Zone".into()),
            ..Default::default()
        };
        let r = resolve(&cfg);
        assert_eq!(r.primary, detect_system_tz());
    }

    #[test]
    fn resolve_dedups_additional() {
        let cfg = TimezoneConfig {
            use_system: false,
            manual_primary: Some("Europe/London".into()),
            additional: vec!["Europe/London".into(), "America/New_York".into()],
            ..Default::default()
        };
        let r = resolve(&cfg);
        assert_eq!(r.display, vec!["Europe/London", "America/New_York"]);
    }

    #[test]
    fn resolve_skips_invalid_additional() {
        let cfg = TimezoneConfig {
            use_system: false,
            manual_primary: Some("Europe/London".into()),
            additional: vec!["Bogus/Zone".into(), "America/New_York".into()],
            ..Default::default()
        };
        let r = resolve(&cfg);
        assert_eq!(r.display, vec!["Europe/London", "America/New_York"]);
    }

    #[test]
    fn system_changed_true_when_last_differs() {
        let cfg = TimezoneConfig {
            use_system: true,
            last_known_system_tz: Some("Antarctica/Vostok".into()),
            ..Default::default()
        };
        let r = resolve(&cfg);
        assert!(r.system_changed || r.system == "Antarctica/Vostok");
    }

    #[test]
    fn system_changed_false_when_dismissed() {
        let system = detect_system_tz();
        let cfg = TimezoneConfig {
            use_system: true,
            last_known_system_tz: Some("Antarctica/Vostok".into()),
            dismissed_change_to: Some(system.clone()),
            ..Default::default()
        };
        let r = resolve(&cfg);
        assert!(!r.system_changed);
    }

    #[test]
    fn system_changed_false_when_no_last_known() {
        let cfg = TimezoneConfig {
            use_system: true,
            ..Default::default()
        };
        let r = resolve(&cfg);
        assert!(!r.system_changed);
    }

    #[test]
    fn load_save_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tz.json");
        let cfg = TimezoneConfig {
            use_system: false,
            manual_primary: Some("Europe/Paris".into()),
            additional: vec!["America/New_York".into()],
            last_known_system_tz: Some("America/Los_Angeles".into()),
            dismissed_change_to: None,
        };
        save_config(&cfg, &path).unwrap();
        let loaded = load_config(&path, None);
        assert!(!loaded.use_system);
        assert_eq!(loaded.manual_primary.as_deref(), Some("Europe/Paris"));
        assert_eq!(loaded.additional, vec!["America/New_York".to_string()]);
    }

    #[test]
    fn load_default_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.json");
        let cfg = load_config(&path, None);
        assert!(cfg.use_system);
        assert!(cfg.manual_primary.is_none());
        assert!(cfg.additional.is_empty());
    }

    #[test]
    fn load_env_override_wins() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tz.json");
        let cfg = TimezoneConfig {
            use_system: false,
            manual_primary: Some("Europe/London".into()),
            ..Default::default()
        };
        save_config(&cfg, &path).unwrap();
        let override_json = r#"{"use_system":true,"manual_primary":null,"additional":[]}"#;
        let loaded = load_config(&path, Some(override_json));
        assert!(loaded.use_system);
        assert!(loaded.manual_primary.is_none());
    }

    #[test]
    fn try_load_missing_file_is_ok_none() {
        let result = try_load_config(Path::new("/nonexistent/path/tz.json"));
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn try_load_invalid_json_returns_err() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tz.json");
        std::fs::write(&path, "not json at all").unwrap();
        let err = try_load_config(&path).expect_err("should reject invalid JSON");
        assert!(err.contains("JSON parse failed"));
    }

    #[test]
    fn try_load_valid_file_returns_config() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tz.json");
        let cfg = TimezoneConfig {
            use_system: false,
            manual_primary: Some("Europe/London".into()),
            ..Default::default()
        };
        save_config(&cfg, &path).unwrap();
        let loaded = try_load_config(&path).unwrap().expect("should parse");
        assert!(!loaded.use_system);
        assert_eq!(loaded.manual_primary.as_deref(), Some("Europe/London"));
    }
}
