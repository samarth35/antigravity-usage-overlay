use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

fn legacy_metric_version() -> u32 {
    0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    #[serde(default = "legacy_metric_version")]
    pub metric_version: u32,
    pub window_5h_limit_tokens: u64,
    pub weekly_limit_tokens: u64,
    pub window_5h_reset_iso: Option<String>,
    pub weekly_reset_iso: Option<String>,
    pub window_5h_base_used_pct: f64,
    pub weekly_base_used_pct: f64,
    pub calibrated_at_iso: Option<String>,
    pub pos_x: Option<f32>,
    pub pos_y: Option<f32>,
    pub brain_dir: String,
    pub opacity: f32,
    pub always_on_top: bool,
    pub hover_peek_enabled: bool,
    pub poll_interval_secs: u64,
    pub auto_sync_enabled: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        let user_profile = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_else(|_| ".".to_string());
        let brain = PathBuf::from(user_profile)
            .join(".gemini")
            .join("antigravity")
            .join("brain");

        Self {
            metric_version: 2,
            window_5h_limit_tokens: 14_000_000,
            weekly_limit_tokens: 54_000_000,
            window_5h_reset_iso: None,
            weekly_reset_iso: None,
            window_5h_base_used_pct: 0.0,
            weekly_base_used_pct: 0.0,
            calibrated_at_iso: None,
            pos_x: None,
            pos_y: None,
            brain_dir: brain.to_string_lossy().into_owned(),
            opacity: 0.94,
            always_on_top: true,
            hover_peek_enabled: true,
            poll_interval_secs: 5,
            auto_sync_enabled: true,
        }
    }
}

pub fn default_config_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join("config.json");
        }
    }
    PathBuf::from("config.json")
}

pub fn load_config(path: &Path) -> AppConfig {
    let mut cfg = AppConfig::default();
    let Ok(content) = fs::read_to_string(path) else {
        return cfg;
    };
    let Ok(parsed) = serde_json::from_str::<AppConfig>(&content) else {
        return cfg;
    };

    cfg.metric_version = parsed.metric_version;
    if parsed.window_5h_limit_tokens > 0 {
        cfg.window_5h_limit_tokens = parsed.window_5h_limit_tokens;
    }
    if parsed.weekly_limit_tokens > 0 {
        cfg.weekly_limit_tokens = parsed.weekly_limit_tokens;
    }
    cfg.window_5h_reset_iso = parsed.window_5h_reset_iso;
    cfg.weekly_reset_iso = parsed.weekly_reset_iso;
    cfg.window_5h_base_used_pct = parsed.window_5h_base_used_pct.clamp(0.0, 100.0);
    cfg.weekly_base_used_pct = parsed.weekly_base_used_pct.clamp(0.0, 100.0);
    cfg.calibrated_at_iso = parsed.calibrated_at_iso;
    cfg.pos_x = parsed.pos_x;
    cfg.pos_y = parsed.pos_y;
    if !parsed.brain_dir.trim().is_empty() {
        cfg.brain_dir = parsed.brain_dir;
    }
    cfg.opacity = parsed.opacity.clamp(0.65, 1.0);
    cfg.always_on_top = parsed.always_on_top;
    cfg.hover_peek_enabled = parsed.hover_peek_enabled;
    cfg.poll_interval_secs = parsed.poll_interval_secs.clamp(2, 60);
    cfg.auto_sync_enabled = parsed.auto_sync_enabled;
    cfg
}

pub fn save_config(path: &Path, cfg: &AppConfig) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp_name = format!(
        "{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config.json"),
        std::process::id()
    );
    let tmp_path = path.with_file_name(tmp_name);
    let json = serde_json::to_string_pretty(cfg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    fs::write(&tmp_path, json)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}
