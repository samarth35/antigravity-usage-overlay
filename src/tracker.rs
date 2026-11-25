use crate::config::AppConfig;
use chrono::{DateTime, Duration, Local, TimeZone};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, SystemTime};

#[derive(Debug, Clone)]
struct ParsedEvent {
    ts: DateTime<Local>,
    billable_tokens: u64,
    weighted_tokens: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_tokens: u64,
    model: String,
}

#[derive(Debug, Clone, Default)]
pub struct UsageSnapshot {
    pub window_5h_tokens: u64,
    pub window_5h_weighted: u64,
    pub window_5h_turns: u64,
    pub window_5h_since_cal_weighted: u64,
    pub weekly_tokens: u64,
    pub weekly_weighted: u64,
    pub weekly_turns: u64,
    pub weekly_since_cal_weighted: u64,
    pub today_tokens: u64,
    pub today_input: u64,
    pub today_output: u64,
    pub today_cache: u64,
    pub today_turns: u64,
    pub top_model: String,
    pub model_breakdown: Vec<(String, u64)>,
    pub today_cache_hit_pct: f64,
    pub burn_rate_per_hour: u64,
}

#[derive(Debug, Clone)]
pub struct QuotaState {
    pub five_hour_rem_pct: f64,
    pub weekly_rem_pct: f64,
    pub five_hour_countdown: Option<Duration>,
    pub weekly_countdown: Option<Duration>,
    pub calibrated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedQuotaClipboard {
    pub five_hour_rem_pct: f64,
    pub five_hour_reset: Duration,
    pub weekly_rem_pct: f64,
    pub weekly_reset: Duration,
}

pub struct UsageTracker {
    brain_dir: PathBuf,
    cache: HashMap<PathBuf, (Option<SystemTime>, u64, Vec<ParsedEvent>)>,
}

impl UsageTracker {
    pub fn new(brain_dir: impl Into<PathBuf>) -> Self {
        Self {
            brain_dir: brain_dir.into(),
            cache: HashMap::new(),
        }
    }

    fn discover_transcripts(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(&self.brain_dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let candidate = path
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl");
            if candidate.is_file() {
                out.push(candidate);
            }
        }
        out
    }

    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    fn parse_line_event(trimmed: &str) -> Option<ParsedEvent> {
        let val = serde_json::from_str::<Value>(trimmed).ok()?;
        let obj = val.as_object()?;
        if !obj.contains_key("input_tokens") && !obj.contains_key("output_tokens") {
            return None;
        }
        let created_at = obj.get("created_at").and_then(|v| v.as_str())?;
        let parsed_dt = DateTime::parse_from_rfc3339(created_at).ok()?;
        let ts = parsed_dt.with_timezone(&Local);
        let in_tok = obj.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let out_tok = obj
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_tok = obj
            .get("cache_read_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let model = obj
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let weighted = in_tok
            .saturating_add(out_tok.saturating_mul(4))
            .saturating_add(cache_tok / 10);

        Some(ParsedEvent {
            ts,
            billable_tokens: in_tok.saturating_add(out_tok),
            weighted_tokens: weighted,
            input_tokens: in_tok,
            output_tokens: out_tok,
            cache_tokens: cache_tok,
            model,
        })
    }

    fn read_events(&mut self, path: &Path) -> Vec<ParsedEvent> {
        let Ok(meta) = fs::metadata(path) else {
            return Vec::new();
        };
        let mtime = meta.modified().ok();
        let meta_len = meta.len();

        // Only trust metadata shortcut for older inactive files (> 2h idle).
        // Active transcripts open for append in Antigravity.exe can report stale NTFS metadata.
        if let Some((c_mtime, bytes_read, events)) = self.cache.get(path) {
            let is_idle_old = c_mtime
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > StdDuration::from_secs(7200));
            if is_idle_old && *c_mtime == mtime && *bytes_read == meta_len {
                return events.clone();
            }
        }

        let Ok(mut file) = File::open(path) else {
            return Vec::new();
        };
        let actual_len = file.metadata().map(|m| m.len()).unwrap_or(meta_len);

        let (start_offset, mut events) = match self.cache.get(path) {
            Some((_, bytes_read, cached_events)) if actual_len == *bytes_read => {
                return cached_events.clone();
            }
            Some((_, bytes_read, cached_events)) if actual_len > *bytes_read => {
                if file.seek(SeekFrom::Start(*bytes_read)).is_ok() {
                    (*bytes_read, cached_events.clone())
                } else {
                    (0, Vec::new())
                }
            }
            _ => (0, Vec::new()),
        };

        let mut reader = BufReader::new(file);
        let mut offset = start_offset;
        let mut line = String::new();

        loop {
            line.clear();
            let Ok(n) = reader.read_line(&mut line) else {
                break;
            };
            if n == 0 {
                break;
            }
            let has_newline = line.ends_with('\n');
            let trimmed = line.trim();
            if trimmed.is_empty() {
                if has_newline {
                    offset = offset.saturating_add(n as u64);
                    continue;
                }
                break;
            }
            if let Some(ev) = Self::parse_line_event(trimmed) {
                events.push(ev);
                offset = offset.saturating_add(n as u64);
            } else if has_newline {
                // Non-token or malformed complete line: advance past it
                offset = offset.saturating_add(n as u64);
            } else {
                // Partial line still being written without newline: retry on next poll
                break;
            }
        }

        self.cache.insert(path.to_path_buf(), (mtime, offset, events.clone()));
        events
    }

    pub fn refresh(
        &mut self,
        now: DateTime<Local>,
        window_5h_start: Option<DateTime<Local>>,
        weekly_start: Option<DateTime<Local>>,
        calibrated_at: Option<DateTime<Local>>,
    ) -> UsageSnapshot {
        let start_5h = window_5h_start.unwrap_or_else(|| now - Duration::hours(5));
        let start_week = weekly_start.unwrap_or_else(|| now - Duration::days(7));
        let day_start = Local
            .from_local_datetime(&now.date_naive().and_hms_opt(0, 0, 0).expect("valid midnight"))
            .single()
            .unwrap_or(now - Duration::hours(24));

        let files = self.discover_transcripts();
        let active: HashSet<PathBuf> = files.iter().cloned().collect();
        self.cache.retain(|k, _| active.contains(k));

        let mut snap = UsageSnapshot::default();
        let mut model_counts: HashMap<String, u64> = HashMap::new();

        for path in files {
            for ev in self.read_events(&path) {
                if ev.ts >= start_5h {
                    snap.window_5h_tokens = snap.window_5h_tokens.saturating_add(ev.billable_tokens);
                    snap.window_5h_weighted =
                        snap.window_5h_weighted.saturating_add(ev.weighted_tokens);
                    snap.window_5h_turns += 1;
                    if let Some(cal) = calibrated_at {
                        if ev.ts > cal {
                            snap.window_5h_since_cal_weighted = snap
                                .window_5h_since_cal_weighted
                                .saturating_add(ev.weighted_tokens);
                        }
                    }
                }
                if ev.ts >= start_week {
                    snap.weekly_tokens = snap.weekly_tokens.saturating_add(ev.billable_tokens);
                    snap.weekly_weighted = snap.weekly_weighted.saturating_add(ev.weighted_tokens);
                    snap.weekly_turns += 1;
                    if let Some(cal) = calibrated_at {
                        if ev.ts > cal {
                            snap.weekly_since_cal_weighted = snap
                                .weekly_since_cal_weighted
                                .saturating_add(ev.weighted_tokens);
                        }
                    }
                    if !ev.model.is_empty() {
                        let weight = ev.billable_tokens.max(1);
                        let label = format_model_name(&ev.model);
                        *model_counts.entry(label).or_insert(0) += weight;
                    }
                }
                if ev.ts >= day_start {
                    snap.today_tokens = snap.today_tokens.saturating_add(ev.billable_tokens);
                    snap.today_input = snap.today_input.saturating_add(ev.input_tokens);
                    snap.today_output = snap.today_output.saturating_add(ev.output_tokens);
                    snap.today_cache = snap.today_cache.saturating_add(ev.cache_tokens);
                    snap.today_turns += 1;
                }
            }
        }

        let mut models_sorted: Vec<(String, u64)> = model_counts.into_iter().collect();
        models_sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        snap.top_model = models_sorted
            .first()
            .map(|(m, _)| m.clone())
            .unwrap_or_else(|| "None".to_string());
        snap.model_breakdown = models_sorted.into_iter().take(3).collect();

        let total_ctx = snap.today_input.saturating_add(snap.today_cache);
        snap.today_cache_hit_pct = if total_ctx > 0 {
            (snap.today_cache as f64 / total_ctx as f64) * 100.0
        } else {
            0.0
        };

        let elapsed_hours = ((now - start_5h).num_seconds().max(0) as f64 / 3600.0).max(0.25);
        snap.burn_rate_per_hour = (snap.window_5h_tokens as f64 / elapsed_hours).round() as u64;
        snap
    }

    pub fn weighted_between(&mut self, start: DateTime<Local>, end: DateTime<Local>) -> u64 {
        let files = self.discover_transcripts();
        let mut total = 0_u64;
        for path in files {
            for ev in self.read_events(&path) {
                if ev.ts >= start && ev.ts <= end {
                    total = total.saturating_add(ev.weighted_tokens);
                }
            }
        }
        total
    }
}

pub fn migrate_config_if_needed(
    cfg: &mut AppConfig,
    tracker: &mut UsageTracker,
    now: DateTime<Local>,
) -> bool {
    if cfg.metric_version >= 2 {
        return false;
    }

    let cal_at = parse_iso_local(cfg.calibrated_at_iso.as_deref());
    let reset_5h = parse_iso_local(cfg.window_5h_reset_iso.as_deref()).filter(|r| *r > now);
    let reset_wk = parse_iso_local(cfg.weekly_reset_iso.as_deref()).filter(|r| *r > now);

    if let (Some(cal), Some(r5)) = (cal_at, reset_5h) {
        if cfg.window_5h_base_used_pct >= 0.5 {
            let w5_start = r5 - Duration::hours(5);
            let wt = tracker.weighted_between(w5_start, cal);
            if wt > 0 {
                cfg.window_5h_limit_tokens = ((wt as f64) / (cfg.window_5h_base_used_pct / 100.0))
                    .round()
                    .clamp(3_000_000.0, 500_000_000.0) as u64;
            } else {
                cfg.window_5h_limit_tokens = 14_000_000;
            }
        } else {
            cfg.window_5h_limit_tokens = 14_000_000;
        }
    } else {
        cfg.window_5h_limit_tokens = 14_000_000;
    }

    if let (Some(cal), Some(rw)) = (cal_at, reset_wk) {
        if cfg.weekly_base_used_pct >= 0.5 {
            let wk_start = rw - Duration::days(7);
            let wt = tracker.weighted_between(wk_start, cal);
            if wt > 0 {
                cfg.weekly_limit_tokens = ((wt as f64) / (cfg.weekly_base_used_pct / 100.0))
                    .round()
                    .clamp(10_000_000.0, 500_000_000.0) as u64;
            } else {
                cfg.weekly_limit_tokens = 54_000_000;
            }
        } else {
            cfg.weekly_limit_tokens = 54_000_000;
        }
    } else {
        cfg.weekly_limit_tokens = 54_000_000;
    }

    cfg.metric_version = 2;
    true
}

pub fn parse_iso_local(raw: Option<&str>) -> Option<DateTime<Local>> {
    let s = raw?.trim();
    if s.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Local))
}

pub fn window_bounds(
    cfg: &AppConfig,
    now: DateTime<Local>,
) -> (
    Option<DateTime<Local>>,
    Option<DateTime<Local>>,
    Option<DateTime<Local>>,
) {
    let w5_start = parse_iso_local(cfg.window_5h_reset_iso.as_deref())
        .map(|reset| reset - Duration::hours(5))
        .filter(|start| *start <= now);
    let wk_start = parse_iso_local(cfg.weekly_reset_iso.as_deref())
        .map(|reset| reset - Duration::days(7))
        .filter(|start| *start <= now);
    let cal_at = parse_iso_local(cfg.calibrated_at_iso.as_deref());
    (w5_start, wk_start, cal_at)
}

pub fn compute_quota_state(
    cfg: &mut AppConfig,
    snap: &UsageSnapshot,
    now: DateTime<Local>,
) -> (QuotaState, bool) {
    let mut changed = false;

    let mut reset_5h = parse_iso_local(cfg.window_5h_reset_iso.as_deref());
    if let Some(r5) = reset_5h {
        if now >= r5 {
            cfg.window_5h_base_used_pct = 0.0;
            cfg.window_5h_reset_iso = None;
            cfg.calibrated_at_iso = Some(now.to_rfc3339());
            reset_5h = None;
            changed = true;
        }
    }

    let mut reset_wk = parse_iso_local(cfg.weekly_reset_iso.as_deref());
    if let Some(rw) = reset_wk {
        if now >= rw {
            cfg.weekly_base_used_pct = 0.0;
            cfg.weekly_reset_iso = None;
            cfg.calibrated_at_iso = Some(now.to_rfc3339());
            reset_wk = None;
            changed = true;
        }
    }

    let cal_active = cfg.calibrated_at_iso.is_some();
    let lim_5h = cfg.window_5h_limit_tokens.max(1) as f64;
    let lim_wk = cfg.weekly_limit_tokens.max(1) as f64;

    let used_5h_pct = if cal_active {
        cfg.window_5h_base_used_pct + (snap.window_5h_since_cal_weighted as f64 / lim_5h) * 100.0
    } else {
        (snap.window_5h_weighted as f64 / lim_5h) * 100.0
    };

    let used_wk_pct = if cal_active {
        cfg.weekly_base_used_pct + (snap.weekly_since_cal_weighted as f64 / lim_wk) * 100.0
    } else {
        (snap.weekly_weighted as f64 / lim_wk) * 100.0
    };

    let five_hour_rem_pct = (100.0 - used_5h_pct).clamp(0.0, 100.0);
    let weekly_rem_pct = (100.0 - used_wk_pct).clamp(0.0, 100.0);

    let five_hour_countdown = reset_5h.map(|r| (r - now).max(Duration::zero()));
    let weekly_countdown = reset_wk.map(|r| (r - now).max(Duration::zero()));

    (
        QuotaState {
            five_hour_rem_pct,
            weekly_rem_pct,
            five_hour_countdown,
            weekly_countdown,
            calibrated: cal_active,
        },
        changed,
    )
}

pub fn format_tokens(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    if n < 1_000_000 {
        let s = format!("{:.1}K", n as f64 / 1_000.0);
        return s.replace(".0K", "K");
    }
    if n < 1_000_000_000 {
        let s = format!("{:.2}", n as f64 / 1_000_000.0);
        let trimmed = s.trim_end_matches('0').trim_end_matches('.');
        return format!("{trimmed}M");
    }
    let s = format!("{:.2}", n as f64 / 1_000_000_000.0);
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    format!("{trimmed}B")
}

pub fn format_model_name(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "None".to_string();
    }
    let mut parts: Vec<&str> = trimmed.split('-').filter(|p| !p.is_empty()).collect();
    if parts.len() > 2 {
        if let Some(last) = parts.last() {
            let lower = last.to_ascii_lowercase();
            if matches!(lower.as_str(), "low" | "medium" | "high" | "lite") {
                parts.pop();
            }
        }
    }
    parts
        .into_iter()
        .map(|p| {
            if p.chars().all(|c| c.is_ascii_digit()) {
                p.to_string()
            } else {
                let mut chs = p.chars();
                match chs.next() {
                    Some(first) => format!("{}{}", first.to_ascii_uppercase(), chs.as_str()),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn parse_duration(input: &str) -> Result<Duration, String> {
    let s = input.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Err("Enter duration like 4h 47m or 6d 23h".to_string());
    }
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut total_mins: i64 = 0;
    let mut found_any = false;

    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let num_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if num_start == i {
            return Err("Invalid duration format (use e.g. 4h 47m)".to_string());
        }
        let val: i64 = s[num_start..i]
            .parse()
            .map_err(|_| "Invalid number in duration".to_string())?;

        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let unit_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        let unit = &s[unit_start..i];
        let mins = match unit {
            "d" | "day" | "days" => val.saturating_mul(24 * 60),
            "h" | "hr" | "hrs" | "hour" | "hours" => val.saturating_mul(60),
            "m" | "min" | "mins" | "minute" | "minutes" => val,
            _ => return Err("Use d, h, or m units (e.g. 6d 23h)".to_string()),
        };
        total_mins = total_mins.saturating_add(mins);
        found_any = true;
    }

    if !found_any || total_mins <= 0 {
        return Err("Duration must be greater than 0m".to_string());
    }
    Ok(Duration::minutes(total_mins))
}

pub fn format_countdown(dur: Option<Duration>) -> String {
    let Some(d) = dur else {
        return "rolling".to_string();
    };
    let total_mins = d.num_minutes().max(0);
    if total_mins == 0 {
        return "<1m".to_string();
    }
    let days = total_mins / (24 * 60);
    let hours = (total_mins % (24 * 60)) / 60;
    let mins = total_mins % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

fn extract_first_pct(segment: &str) -> Option<f64> {
    let bytes = segment.as_bytes();
    for (idx, &b) in bytes.iter().enumerate() {
        if b == b'%' && idx > 0 {
            let mut start = idx;
            while start > 0 && (bytes[start - 1].is_ascii_digit() || bytes[start - 1] == b'.') {
                start -= 1;
            }
            if start < idx {
                if let Ok(val) = segment[start..idx].parse::<f64>() {
                    if (0.0..=100.0).contains(&val) {
                        return Some(val);
                    }
                }
            }
        }
    }
    None
}

fn extract_embedded_duration(segment: &str) -> Option<Duration> {
    let lower_full = segment.to_ascii_lowercase();
    // Scope to the phrase after "refresh in" / "resets in" so labels like "5-hour limit" aren't counted
    let scoped = if let Some(idx) = lower_full.find("refresh in") {
        &lower_full[idx + "refresh in".len()..]
    } else if let Some(idx) = lower_full.find("resets in") {
        &lower_full[idx + "resets in".len()..]
    } else if let Some(idx) = lower_full.find("limit") {
        &lower_full[idx + "limit".len()..]
    } else {
        &lower_full[..]
    };

    let tokens: Vec<&str> = scoped
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();

    let mut total_mins: i64 = 0;
    let mut found = false;
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        // Combined token like "4h", "47m", "6d", "23h"
        let split_at = tok.find(|c: char| !c.is_ascii_digit()).unwrap_or(tok.len());
        if split_at > 0 && split_at < tok.len() {
            if let Ok(val) = tok[..split_at].parse::<i64>() {
                let unit = &tok[split_at..];
                let mult = match unit {
                    "d" | "day" | "days" => Some(24 * 60),
                    "h" | "hr" | "hrs" | "hour" | "hours" => Some(60),
                    "m" | "min" | "mins" | "minute" | "minutes" => Some(1),
                    _ => None,
                };
                if let Some(m) = mult {
                    total_mins = total_mins.saturating_add(val.saturating_mul(m));
                    found = true;
                }
            }
            i += 1;
            continue;
        }
        // Separate number followed by unit word like "6", "days", "23", "hours"
        if tok.chars().all(|c| c.is_ascii_digit()) && i + 1 < tokens.len() {
            if let Ok(val) = tok.parse::<i64>() {
                let mult = match tokens[i + 1] {
                    "d" | "day" | "days" => Some(24 * 60),
                    "h" | "hr" | "hrs" | "hour" | "hours" => Some(60),
                    "m" | "min" | "mins" | "minute" | "minutes" => Some(1),
                    _ => None,
                };
                if let Some(m) = mult {
                    total_mins = total_mins.saturating_add(val.saturating_mul(m));
                    found = true;
                    i += 2;
                    continue;
                }
            }
        }
        i += 1;
    }

    if found && total_mins > 0 {
        Some(Duration::minutes(total_mins))
    } else {
        None
    }
}

pub fn parse_antigravity_settings_text(text: &str) -> Result<ParsedQuotaClipboard, String> {
    let lower = text.to_ascii_lowercase();
    // Scope to Gemini Models section if multiple model groups were copied
    let scoped = if let Some(idx) = lower.find("gemini models") {
        let after = &text[idx..];
        let after_low = &lower[idx..];
        if let Some(end_idx) = after_low.find("claude and gpt") {
            &after[..end_idx]
        } else {
            after
        }
    } else {
        text
    };
    let scoped_low = scoped.to_ascii_lowercase();

    let wk_idx = scoped_low
        .find("weekly limit")
        .ok_or_else(|| "Copy the Gemini Models quota block in Antigravity Settings first".to_string())?;
    let five_idx = scoped_low
        .find("five hour limit")
        .or_else(|| scoped_low.find("5-hour limit"))
        .or_else(|| scoped_low.find("5h limit"))
        .ok_or_else(|| "Could not find Five Hour Limit in clipboard".to_string())?;

    let (wk_seg, five_seg) = if wk_idx < five_idx {
        (&scoped[wk_idx..five_idx], &scoped[five_idx..])
    } else {
        (&scoped[wk_idx..], &scoped[five_idx..wk_idx])
    };

    let weekly_rem_pct = extract_first_pct(wk_seg)
        .ok_or_else(|| "Could not read Weekly % from clipboard".to_string())?;
    let five_hour_rem_pct = extract_first_pct(five_seg)
        .ok_or_else(|| "Could not read 5-Hour % from clipboard".to_string())?;

    let weekly_reset = extract_embedded_duration(wk_seg)
        .filter(|d| *d <= Duration::days(8))
        .unwrap_or_else(|| Duration::days(7));
    let five_hour_reset = extract_embedded_duration(five_seg)
        .filter(|d| *d <= Duration::hours(6))
        .unwrap_or_else(|| Duration::hours(5));

    Ok(ParsedQuotaClipboard {
        five_hour_rem_pct,
        five_hour_reset,
        weekly_rem_pct,
        weekly_reset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_tokens() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(842), "842");
        assert_eq!(format_tokens(12_400), "12.4K");
        assert_eq!(format_tokens(2_000_000), "2M");
        assert_eq!(format_tokens(1_250_000), "1.25M");
    }

    #[test]
    fn test_format_model_name() {
        assert_eq!(format_model_name("gemini-4-neon-medium"), "Gemini 4 Neon");
        assert_eq!(format_model_name(""), "None");
    }

    #[test]
    fn test_parse_and_format_duration() {
        assert_eq!(
            parse_duration("4h 47m").unwrap(),
            Duration::minutes(4 * 60 + 47)
        );
        assert_eq!(
            parse_duration("6d 23h").unwrap(),
            Duration::minutes(6 * 24 * 60 + 23 * 60)
        );
        assert_eq!(
            parse_duration("1d 2h 15m").unwrap(),
            Duration::minutes(24 * 60 + 2 * 60 + 15)
        );
        assert_eq!(parse_duration("30m").unwrap(), Duration::minutes(30));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("0m").is_err());
        assert!(parse_duration("abc").is_err());

        assert_eq!(
            format_countdown(Some(Duration::minutes(6 * 24 * 60 + 23 * 60))),
            "6d 23h"
        );
        assert_eq!(
            format_countdown(Some(Duration::minutes(4 * 60 + 47))),
            "4h 47m"
        );
        assert_eq!(format_countdown(None), "rolling");
    }

    #[test]
    fn test_compute_quota_state_calibration_and_auto_reset() {
        let now = Local::now();
        let mut cfg = AppConfig {
            window_5h_limit_tokens: 10_000_000,
            weekly_limit_tokens: 100_000_000,
            window_5h_reset_iso: Some((now + Duration::hours(4)).to_rfc3339()),
            weekly_reset_iso: Some((now + Duration::days(6)).to_rfc3339()),
            window_5h_base_used_pct: 3.0,
            weekly_base_used_pct: 1.0,
            calibrated_at_iso: Some((now - Duration::minutes(10)).to_rfc3339()),
            ..Default::default()
        };
        let snap = UsageSnapshot {
            window_5h_since_cal_weighted: 200_000, // +2%
            weekly_since_cal_weighted: 1_000_000,  // +1%
            ..Default::default()
        };

        let (state, changed) = compute_quota_state(&mut cfg, &snap, now);
        assert!(!changed);
        assert!(state.calibrated);
        assert!((state.five_hour_rem_pct - 95.0).abs() < 0.01);
        assert!((state.weekly_rem_pct - 98.0).abs() < 0.01);

        // Simulate 5h window expiry
        cfg.window_5h_reset_iso = Some((now - Duration::minutes(1)).to_rfc3339());
        let (state2, changed2) = compute_quota_state(&mut cfg, &snap, now);
        assert!(changed2);
        assert_eq!(cfg.window_5h_base_used_pct, 0.0);
        assert!(cfg.window_5h_reset_iso.is_none());
        assert!((state2.five_hour_rem_pct - 98.0).abs() < 0.01);
    }

    #[test]
    fn test_parse_antigravity_settings_clipboard() {
        let sample = "\
Gemini Models
Weekly Limit Remaining
You have used some of your weekly limit, it will fully refresh in 6 days, 23 hours.
99%
Five Hour Limit Remaining
You have used some of your 5-hour limit, it will fully refresh in 4 hours, 47 minutes.
97%
Claude and GPT models
Weekly Limit Remaining
100%";
        let parsed = parse_antigravity_settings_text(sample).unwrap();
        assert_eq!(parsed.weekly_rem_pct, 99.0);
        assert_eq!(parsed.weekly_reset, Duration::minutes(6 * 24 * 60 + 23 * 60));
        assert_eq!(parsed.five_hour_rem_pct, 97.0);
        assert_eq!(parsed.five_hour_reset, Duration::minutes(4 * 60 + 47));

        let full = "Weekly Limit Remaining\n100%\nFive Hour Limit Remaining\n100%";
        let parsed_full = parse_antigravity_settings_text(full).unwrap();
        assert_eq!(parsed_full.weekly_rem_pct, 100.0);
        assert_eq!(parsed_full.weekly_reset, Duration::days(7));
        assert_eq!(parsed_full.five_hour_rem_pct, 100.0);
        assert_eq!(parsed_full.five_hour_reset, Duration::hours(5));

        assert!(parse_antigravity_settings_text("random clipboard text").is_err());
    }

    #[test]
    fn test_migrate_v0_to_v2_recomputes_weighted_limits() {
        let tmp = std::env::temp_dir().join(format!("agy_mig_test_{}", std::process::id()));
        let log_dir = tmp.join("conv1").join(".system_generated").join("logs");
        fs::create_dir_all(&log_dir).unwrap();
        let now = Local::now();
        let ev_ts = (now - Duration::minutes(30)).to_rfc3339();
        let line = serde_json::json!({
            "created_at": ev_ts,
            "model": "gemini-4-neon-medium",
            "input_tokens": 1_000_000_u64,
            "output_tokens": 250_000_u64,
            "cache_read_tokens": 10_000_000_u64
        });
        fs::write(log_dir.join("transcript.jsonl"), format!("{line}\n")).unwrap();
        // weighted = 1M + 4*250K + 10M/10 = 3M

        let mut cfg = AppConfig {
            metric_version: 0,
            window_5h_limit_tokens: 4_000_000,
            weekly_limit_tokens: 15_000_000,
            window_5h_reset_iso: Some((now + Duration::hours(4)).to_rfc3339()),
            weekly_reset_iso: Some((now + Duration::days(6)).to_rfc3339()),
            window_5h_base_used_pct: 25.0,
            weekly_base_used_pct: 10.0,
            calibrated_at_iso: Some((now - Duration::minutes(1)).to_rfc3339()),
            brain_dir: tmp.to_string_lossy().into_owned(),
            ..Default::default()
        };

        let mut tracker = UsageTracker::new(&cfg.brain_dir);
        assert!(migrate_config_if_needed(&mut cfg, &mut tracker, now));
        assert_eq!(cfg.metric_version, 2);
        // 3M / 0.25 = 12M
        assert_eq!(cfg.window_5h_limit_tokens, 12_000_000);
        // 3M / 0.10 = 30M
        assert_eq!(cfg.weekly_limit_tokens, 30_000_000);
        // Second call is a no-op
        assert!(!migrate_config_if_needed(&mut cfg, &mut tracker, now));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_model_breakdown_and_cache_hit_rate() {
        let tmp = std::env::temp_dir().join(format!("agy_bd_test_{}", std::process::id()));
        let log_dir = tmp.join("conv1").join(".system_generated").join("logs");
        fs::create_dir_all(&log_dir).unwrap();
        let now = Local::now();
        let ts = (now - Duration::minutes(15)).to_rfc3339();
        let lines = [
            serde_json::json!({
                "created_at": ts, "model": "gemini-4-neon-medium",
                "input_tokens": 1000, "output_tokens": 500, "cache_read_tokens": 3000
            }),
            serde_json::json!({
                "created_at": ts, "model": "gemini-4-hydrogen-high",
                "input_tokens": 4000, "output_tokens": 1000, "cache_read_tokens": 1000
            }),
            serde_json::json!({
                "created_at": ts, "model": "claude-4-sonnet",
                "input_tokens": 400, "output_tokens": 100, "cache_read_tokens": 0
            }),
        ];
        let body = lines.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n");
        fs::write(log_dir.join("transcript.jsonl"), body).unwrap();

        let mut tracker = UsageTracker::new(&tmp);
        let snap = tracker.refresh(now, None, None, None);
        assert_eq!(snap.top_model, "Gemini 4 Hydrogen");
        assert_eq!(snap.model_breakdown.len(), 3);
        assert_eq!(snap.model_breakdown[0], ("Gemini 4 Hydrogen".to_string(), 5000));
        assert_eq!(snap.model_breakdown[1], ("Gemini 4 Neon".to_string(), 1500));
        assert_eq!(snap.model_breakdown[2], ("Claude 4 Sonnet".to_string(), 500));
        // today_input = 5400, today_cache = 4000 -> 4000 / 9400 = 42.55%
        assert!((snap.today_cache_hit_pct - 42.55).abs() < 0.2);
        assert!(snap.burn_rate_per_hour > 0);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_incremental_append_and_clear_cache() {
        let tmp = std::env::temp_dir().join(format!("agy_inc_test_{}", std::process::id()));
        let log_dir = tmp.join("conv1").join(".system_generated").join("logs");
        fs::create_dir_all(&log_dir).unwrap();
        let log_path = log_dir.join("transcript.jsonl");
        let now = Local::now();
        let ts = (now - Duration::minutes(5)).to_rfc3339();
        let line1 = serde_json::json!({
            "created_at": ts, "model": "gemini-4-neon-medium",
            "input_tokens": 1000, "output_tokens": 200, "cache_read_tokens": 0
        });
        fs::write(&log_path, format!("{line1}\n")).unwrap();

        let mut tracker = UsageTracker::new(&tmp);
        let snap1 = tracker.refresh(now, None, None, None);
        assert_eq!(snap1.today_tokens, 1200);
        assert_eq!(snap1.today_turns, 1);

        // Append a second turn
        use std::io::Write;
        let mut f = fs::OpenOptions::new().append(true).open(&log_path).unwrap();
        let line2 = serde_json::json!({
            "created_at": ts, "model": "gemini-4-neon-medium",
            "input_tokens": 500, "output_tokens": 300, "cache_read_tokens": 0
        });
        writeln!(f, "{line2}").unwrap();
        drop(f);

        let snap2 = tracker.refresh(now, None, None, None);
        assert_eq!(snap2.today_tokens, 2000);
        assert_eq!(snap2.today_turns, 2);

        tracker.clear_cache();
        let snap3 = tracker.refresh(now, None, None, None);
        assert_eq!(snap3.today_tokens, 2000);
        assert_eq!(snap3.today_turns, 2);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_real_local_brain_transcripts() {
        let cfg = AppConfig::default();
        let mut tracker = UsageTracker::new(&cfg.brain_dir);
        let now = Local::now();
        let snap = tracker.refresh(now, None, None, None);
        assert!(snap.weekly_tokens >= snap.window_5h_tokens);
        assert!(snap.weekly_turns >= snap.window_5h_turns);
    }
}
