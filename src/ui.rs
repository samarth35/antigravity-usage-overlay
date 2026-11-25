use crate::auto_sync::{AutoSyncMessage, AutoSyncWorker, SyncSource};
use crate::config::{save_config, AppConfig};
use crate::tracker::{
    compute_quota_state, format_countdown, format_tokens, migrate_config_if_needed,
    parse_antigravity_settings_text, parse_duration, parse_iso_local, window_bounds, QuotaState,
    UsageSnapshot, UsageTracker,
};
use chrono::{Duration, Local};
use eframe::egui::{self, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use std::f32::consts::PI;
use std::path::PathBuf;
use std::time::{Duration as StdDuration, Instant};

const ORB_WIN: [f32; 2] = [62.0, 62.0];
const PEEK_WIN: [f32; 2] = [232.0, 66.0];
const CARD_WIN: [f32; 2] = [258.0, 224.0];

#[inline]
fn bg_glass(opacity: f32) -> Color32 {
    let alpha = (opacity.clamp(0.65, 1.0) * 255.0).round() as u8;
    Color32::from_rgba_unmultiplied(13, 16, 26, alpha)
}
#[inline]
fn surface_card(opacity: f32) -> Color32 {
    let alpha = (opacity.clamp(0.65, 1.0) * 245.0).round() as u8;
    Color32::from_rgba_unmultiplied(23, 28, 43, alpha)
}
#[inline]
fn rim_glass() -> Color32 {
    Color32::from_rgba_unmultiplied(255, 255, 255, 32)
}
#[inline]
fn track_dark() -> Color32 {
    Color32::from_rgba_unmultiplied(38, 46, 68, 220)
}
const TEXT_PRIMARY: Color32 = Color32::from_rgb(248, 250, 252);
const TEXT_SECONDARY: Color32 = Color32::from_rgb(148, 163, 184);
const TEXT_MUTED: Color32 = Color32::from_rgb(100, 116, 139);

fn remaining_color(rem_pct: f64) -> Color32 {
    if rem_pct >= 50.0 {
        Color32::from_rgb(34, 197, 94) // green
    } else if rem_pct >= 20.0 {
        Color32::from_rgb(245, 158, 11) // amber
    } else {
        Color32::from_rgb(244, 63, 94) // rose
    }
}

fn should_start_window_drag(ctx: &egui::Context, resp: &egui::Response, threshold: f32) -> bool {
    if !resp.dragged_by(egui::PointerButton::Primary) {
        return false;
    }
    ctx.input(|i| {
        i.pointer
            .press_origin()
            .zip(i.pointer.interact_pos())
            .is_some_and(|(a, b)| a.distance(b) > threshold)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Orb,
    Peek,
    Expanded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveTab {
    Usage,
    Details,
    Options,
}

pub struct UsageOverlayApp {
    config_path: PathBuf,
    config: AppConfig,
    tracker: UsageTracker,
    snapshot: UsageSnapshot,
    quota: QuotaState,
    mode: ViewMode,
    active_tab: ActiveTab,
    current_win_size: [f32; 2],
    sync_5h_pct: String,
    sync_5h_reset: String,
    sync_wk_pct: String,
    sync_wk_reset: String,
    sync_error: String,
    toast_msg: Option<(String, Instant)>,
    auto_worker: AutoSyncWorker,
    scanning_live: bool,
    last_auto_sync_at: Option<Instant>,
    last_poll: Instant,
    peek_entered_at: Option<Instant>,
    hover_leave_since: Option<Instant>,
    transition_guard_until: Option<Instant>,
    orb_anchor_pos: Option<Pos2>,
    expanded_pos: Option<Pos2>,
    drag_in_progress: bool,
    peek_on_left: bool,
    initialized_pos: bool,
}

impl UsageOverlayApp {
    pub fn new(config_path: PathBuf, mut config: AppConfig) -> Self {
        let mut tracker = UsageTracker::new(&config.brain_dir);
        let now = Local::now();
        let migrated = migrate_config_if_needed(&mut config, &mut tracker, now);
        let (w5_s, wk_s, cal) = window_bounds(&config, now);
        let snapshot = tracker.refresh(now, w5_s, wk_s, cal);
        let (quota, reset_changed) = compute_quota_state(&mut config, &snapshot, now);
        if migrated || reset_changed {
            let _ = save_config(&config_path, &config);
        }

        let five_pct_str = format!("{:.0}", quota.five_hour_rem_pct);
        let wk_pct_str = format!("{:.0}", quota.weekly_rem_pct);
        let five_reset_str = quota
            .five_hour_countdown
            .map(|d| format_countdown(Some(d)))
            .unwrap_or_else(|| "5h 0m".to_string());
        let wk_reset_str = quota
            .weekly_countdown
            .map(|d| format_countdown(Some(d)))
            .unwrap_or_else(|| "7d 0h".to_string());

        let auto_worker = AutoSyncWorker::start(config.auto_sync_enabled);
        Self {
            config_path,
            config,
            tracker,
            snapshot,
            quota,
            mode: ViewMode::Orb,
            active_tab: ActiveTab::Usage,
            current_win_size: ORB_WIN,
            sync_5h_pct: five_pct_str,
            sync_5h_reset: five_reset_str,
            sync_wk_pct: wk_pct_str,
            sync_wk_reset: wk_reset_str,
            sync_error: String::new(),
            toast_msg: None,
            auto_worker,
            scanning_live: false,
            last_auto_sync_at: None,
            last_poll: Instant::now(),
            peek_entered_at: None,
            hover_leave_since: None,
            transition_guard_until: None,
            orb_anchor_pos: None,
            expanded_pos: None,
            drag_in_progress: false,
            peek_on_left: false,
            initialized_pos: false,
        }
    }

    fn refresh_data(&mut self) {
        let now = Local::now();
        let (w5_s, wk_s, cal) = window_bounds(&self.config, now);
        self.snapshot = self.tracker.refresh(now, w5_s, wk_s, cal);
        let (quota, changed) = compute_quota_state(&mut self.config, &self.snapshot, now);
        self.quota = quota;
        if changed {
            let _ = save_config(&self.config_path, &self.config);
        }
        self.last_poll = Instant::now();
    }

    fn force_refresh(&mut self, ctx: &egui::Context) {
        let prev_tokens = self.snapshot.today_tokens;
        self.tracker.clear_cache();
        self.refresh_data();
        self.auto_worker.trigger_silent();
        let delta = self.snapshot.today_tokens.saturating_sub(prev_tokens);
        let msg = if delta > 0 {
            format!("Refreshed +{}", format_tokens(delta))
        } else {
            format!("Refreshed · {}t", self.snapshot.today_turns)
        };
        self.toast_msg = Some((msg, Instant::now()));
        ctx.request_repaint();
    }

    fn sync_window_size(&mut self, ctx: &egui::Context, next_size: [f32; 2]) {
        if self.current_win_size != next_size {
            self.current_win_size = next_size;
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(
                next_size[0],
                next_size[1],
            )));
        }
    }

    fn set_mode(&mut self, ctx: &egui::Context, target: ViewMode) {
        if self.mode == target {
            return;
        }
        let prev = self.mode;
        let outer_pos = ctx.input(|i| i.viewport().outer_rect.map(|r| r.min));
        let monitor_size = ctx.input(|i| i.viewport().monitor_size);
        let (mon_w, mon_h) = monitor_size
            .map(|m| (m.x, m.y))
            .unwrap_or((1920.0, 1080.0));

        // Guard against reading stale outer_rect during OS resize/reposition
        self.transition_guard_until = Some(Instant::now() + StdDuration::from_millis(320));

        match (prev, target) {
            (ViewMode::Orb, ViewMode::Peek) => {
                let anchor = self.orb_anchor_pos.or(outer_pos);
                if let Some(pos) = anchor {
                    self.orb_anchor_pos = Some(pos);
                    self.peek_on_left = pos.x + PEEK_WIN[0] + 10.0 > mon_w;
                    self.peek_entered_at = Some(Instant::now());
                    self.hover_leave_since = None;
                    self.mode = ViewMode::Peek;
                    self.sync_window_size(ctx, PEEK_WIN);
                    if self.peek_on_left {
                        let new_x = (pos.x - (PEEK_WIN[0] - ORB_WIN[0])).max(8.0);
                        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(Pos2::new(
                            new_x, pos.y,
                        )));
                    }
                    return;
                }
            }
            (ViewMode::Peek, ViewMode::Orb) => {
                self.peek_entered_at = None;
                self.hover_leave_since = None;
                self.mode = ViewMode::Orb;
                if let Some(anchor) = self.orb_anchor_pos {
                    let ax = anchor.x.clamp(8.0, (mon_w - ORB_WIN[0] - 8.0).max(8.0));
                    let ay = anchor.y.clamp(8.0, (mon_h - ORB_WIN[1] - 8.0).max(8.0));
                    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(Pos2::new(ax, ay)));
                }
                self.sync_window_size(ctx, ORB_WIN);
                return;
            }
            (ViewMode::Orb | ViewMode::Peek, ViewMode::Expanded) => {
                self.peek_entered_at = None;
                self.hover_leave_since = None;
                let anchor = self.orb_anchor_pos.or(outer_pos);
                if let Some(pos) = anchor {
                    self.orb_anchor_pos = Some(pos);
                    let mut nx = pos.x - (CARD_WIN[0] - ORB_WIN[0]);
                    let mut ny = pos.y - (CARD_WIN[1] - ORB_WIN[1]);
                    if nx < 8.0 {
                        nx = pos.x;
                    }
                    if ny < 8.0 {
                        ny = pos.y;
                    }
                    nx = nx.clamp(8.0, (mon_w - CARD_WIN[0] - 8.0).max(8.0));
                    ny = ny.clamp(8.0, (mon_h - CARD_WIN[1] - 8.0).max(8.0));
                    let exp = Pos2::new(nx, ny);
                    self.expanded_pos = Some(exp);
                    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(exp));
                }
            }
            (ViewMode::Expanded, ViewMode::Orb) => {
                if let Some(anchor) = self.orb_anchor_pos {
                    let ax = anchor.x.clamp(8.0, (mon_w - ORB_WIN[0] - 8.0).max(8.0));
                    let ay = anchor.y.clamp(8.0, (mon_h - ORB_WIN[1] - 8.0).max(8.0));
                    self.orb_anchor_pos = Some(Pos2::new(ax, ay));
                    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(Pos2::new(ax, ay)));
                }
            }
            _ => {}
        }

        self.mode = target;
        let next_size = match target {
            ViewMode::Orb => ORB_WIN,
            ViewMode::Peek => PEEK_WIN,
            ViewMode::Expanded => CARD_WIN,
        };
        self.sync_window_size(ctx, next_size);
    }

    fn populate_sync_fields(&mut self) {
        self.sync_5h_pct = format!("{:.0}", self.quota.five_hour_rem_pct);
        self.sync_wk_pct = format!("{:.0}", self.quota.weekly_rem_pct);
        self.sync_5h_reset = self
            .quota
            .five_hour_countdown
            .map(|d| format_countdown(Some(d)))
            .unwrap_or_else(|| "5h 0m".to_string());
        self.sync_wk_reset = self
            .quota
            .weekly_countdown
            .map(|d| format_countdown(Some(d)))
            .unwrap_or_else(|| "7d 0h".to_string());
        self.sync_error.clear();
    }

    fn open_options_tab(&mut self, ctx: &egui::Context) {
        self.populate_sync_fields();
        self.active_tab = ActiveTab::Options;
        self.set_mode(ctx, ViewMode::Expanded);
    }

    fn smart_sync_action(&mut self, _ctx: &egui::Context) {
        self.scanning_live = true;
        self.toast_msg = Some(("Scanning...".to_string(), Instant::now()));
        self.auto_worker.trigger_now();
    }

    fn process_auto_sync_messages(&mut self) {
        while let Some(msg) = self.auto_worker.try_recv() {
            self.scanning_live = false;
            match msg {
                AutoSyncMessage::Found(res) => {
                    let parsed = res.parsed;
                    let pct_5h_diff = (self.quota.five_hour_rem_pct - parsed.five_hour_rem_pct).abs();
                    let pct_wk_diff = (self.quota.weekly_rem_pct - parsed.weekly_rem_pct).abs();
                    let min_5h_diff = self
                        .quota
                        .five_hour_countdown
                        .map(|d| (d - parsed.five_hour_reset).num_minutes().abs())
                        .unwrap_or(999);
                    let min_wk_diff = self
                        .quota
                        .weekly_countdown
                        .map(|d| (d - parsed.weekly_reset).num_minutes().abs())
                        .unwrap_or(999);
                    let stale = self
                        .last_auto_sync_at
                        .is_none_or(|t| t.elapsed() >= StdDuration::from_secs(60));

                    if res.manual
                        || !self.quota.calibrated
                        || pct_5h_diff >= 0.8
                        || pct_wk_diff >= 0.8
                        || min_5h_diff >= 5
                        || min_wk_diff >= 20
                        || stale
                    {
                        self.commit_calibration(
                            parsed.five_hour_rem_pct,
                            parsed.five_hour_reset,
                            parsed.weekly_rem_pct,
                            parsed.weekly_reset,
                        );
                        self.populate_sync_fields();
                        self.last_auto_sync_at = Some(Instant::now());
                        let tag = match res.source {
                            SyncSource::UiAutomation => "Auto-Synced",
                            SyncSource::ScreenOcr => "OCR Synced",
                        };
                        self.toast_msg = Some((
                            format!(
                                "{tag} {:.0}% / {:.0}%",
                                parsed.five_hour_rem_pct, parsed.weekly_rem_pct
                            ),
                            Instant::now(),
                        ));
                    }
                }
                AutoSyncMessage::NotVisible { manual } => {
                    if manual {
                        self.toast_msg =
                            Some(("Open Antigravity Models page".to_string(), Instant::now()));
                    }
                }
                AutoSyncMessage::Error { message, manual } => {
                    if manual {
                        self.toast_msg = Some((
                            format!("Sync err: {}", message.chars().take(18).collect::<String>()),
                            Instant::now(),
                        ));
                    }
                }
            }
        }
    }

    fn commit_calibration(
        &mut self,
        pct_5h: f64,
        dur_5h: Duration,
        pct_wk: f64,
        dur_wk: Duration,
    ) {
        let now = Local::now();
        let reset_5h = now + dur_5h;
        let reset_wk = now + dur_wk;
        let w5_start = reset_5h - Duration::hours(5);
        let wk_start = reset_wk - Duration::days(7);

        let prev_cal = parse_iso_local(self.config.calibrated_at_iso.as_deref());
        let cal_snap = self
            .tracker
            .refresh(now, Some(w5_start), Some(wk_start), Some(now));

        let base_5h_used = 100.0 - pct_5h;
        let base_wk_used = 100.0 - pct_wk;

        let mut calibrated_5h_via_slope = false;
        if let Some(pcal) = prev_cal {
            let prev_5h_reset = parse_iso_local(self.config.window_5h_reset_iso.as_deref());
            if pcal >= w5_start
                && pcal < now
                && prev_5h_reset.is_some_and(|pr| (pr - reset_5h).num_minutes().abs() <= 20)
            {
                let delta_used = base_5h_used - self.config.window_5h_base_used_pct;
                if delta_used >= 2.0 {
                    let delta_wt = self.tracker.weighted_between(pcal, now);
                    if delta_wt > 0 {
                        self.config.window_5h_limit_tokens =
                            ((delta_wt as f64) / (delta_used / 100.0))
                                .round()
                                .clamp(3_000_000.0, 500_000_000.0) as u64;
                        calibrated_5h_via_slope = true;
                    }
                }
            }
        }
        if !calibrated_5h_via_slope && base_5h_used >= 0.5 && cal_snap.window_5h_weighted > 0 {
            self.config.window_5h_limit_tokens =
                ((cal_snap.window_5h_weighted as f64) / (base_5h_used / 100.0))
                    .round()
                    .clamp(3_000_000.0, 500_000_000.0) as u64;
        }

        let mut calibrated_wk_via_slope = false;
        if let Some(pcal) = prev_cal {
            let prev_wk_reset = parse_iso_local(self.config.weekly_reset_iso.as_deref());
            if pcal >= wk_start
                && pcal < now
                && prev_wk_reset.is_some_and(|pr| (pr - reset_wk).num_minutes().abs() <= 90)
            {
                let delta_used = base_wk_used - self.config.weekly_base_used_pct;
                if delta_used >= 2.0 {
                    let delta_wt = self.tracker.weighted_between(pcal, now);
                    if delta_wt > 0 {
                        self.config.weekly_limit_tokens =
                            ((delta_wt as f64) / (delta_used / 100.0))
                                .round()
                                .clamp(10_000_000.0, 500_000_000.0) as u64;
                        calibrated_wk_via_slope = true;
                    }
                }
            }
        }
        if !calibrated_wk_via_slope && base_wk_used >= 0.5 && cal_snap.weekly_weighted > 0 {
            self.config.weekly_limit_tokens =
                ((cal_snap.weekly_weighted as f64) / (base_wk_used / 100.0))
                    .round()
                    .clamp(10_000_000.0, 500_000_000.0) as u64;
        }

        self.config.metric_version = 2;
        self.config.window_5h_reset_iso = Some(reset_5h.to_rfc3339());
        self.config.weekly_reset_iso = Some(reset_wk.to_rfc3339());
        self.config.window_5h_base_used_pct = base_5h_used;
        self.config.weekly_base_used_pct = base_wk_used;
        self.config.calibrated_at_iso = Some(now.to_rfc3339());

        let _ = save_config(&self.config_path, &self.config);
        self.sync_error.clear();
        self.refresh_data();
    }

    fn reset_calibration(&mut self) {
        self.config.window_5h_limit_tokens = 14_000_000;
        self.config.weekly_limit_tokens = 54_000_000;
        self.config.window_5h_reset_iso = None;
        self.config.weekly_reset_iso = None;
        self.config.window_5h_base_used_pct = 0.0;
        self.config.weekly_base_used_pct = 0.0;
        self.config.calibrated_at_iso = None;
        let _ = save_config(&self.config_path, &self.config);
        self.refresh_data();
        self.populate_sync_fields();
        self.toast_msg = Some(("Sync cleared".to_string(), Instant::now()));
    }

    fn paste_sync_from_clipboard(&mut self, return_to_usage: bool) {
        let text = match arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
            Ok(t) => t,
            Err(_) => {
                if self.active_tab == ActiveTab::Options {
                    self.sync_error = "Clipboard is empty".to_string();
                } else {
                    self.toast_msg = Some(("Clipboard empty".to_string(), Instant::now()));
                }
                return;
            }
        };

        match parse_antigravity_settings_text(&text) {
            Ok(parsed) => {
                self.commit_calibration(
                    parsed.five_hour_rem_pct,
                    parsed.five_hour_reset,
                    parsed.weekly_rem_pct,
                    parsed.weekly_reset,
                );
                self.populate_sync_fields();
                if return_to_usage {
                    self.active_tab = ActiveTab::Usage;
                }
                self.toast_msg = Some((
                    format!(
                        "Synced {:.0}% / {:.0}%",
                        parsed.five_hour_rem_pct, parsed.weekly_rem_pct
                    ),
                    Instant::now(),
                ));
            }
            Err(err) => {
                if self.active_tab == ActiveTab::Options {
                    self.sync_error = err;
                } else {
                    self.toast_msg = Some((
                        "Copy Gemini quota text first".to_string(),
                        Instant::now(),
                    ));
                }
            }
        }
    }

    fn apply_manual_sync(&mut self) {
        let pct_5h: f64 = match self.sync_5h_pct.trim().trim_end_matches('%').parse() {
            Ok(v) if (0.0..=100.0).contains(&v) => v,
            _ => {
                self.sync_error = "5h Left % must be 0-100".to_string();
                return;
            }
        };
        let pct_wk: f64 = match self.sync_wk_pct.trim().trim_end_matches('%').parse() {
            Ok(v) if (0.0..=100.0).contains(&v) => v,
            _ => {
                self.sync_error = "Weekly Left % must be 0-100".to_string();
                return;
            }
        };
        let dur_5h = match parse_duration(&self.sync_5h_reset) {
            Ok(d) if d <= Duration::hours(6) => d,
            Ok(_) => {
                self.sync_error = "5h reset should be <= 5h".to_string();
                return;
            }
            Err(e) => {
                self.sync_error = e;
                return;
            }
        };
        let dur_wk = match parse_duration(&self.sync_wk_reset) {
            Ok(d) if d <= Duration::days(8) => d,
            Ok(_) => {
                self.sync_error = "Weekly reset should be <= 7d".to_string();
                return;
            }
            Err(e) => {
                self.sync_error = e;
                return;
            }
        };

        self.commit_calibration(pct_5h, dur_5h, pct_wk, dur_wk);
        self.active_tab = ActiveTab::Usage;
        self.toast_msg = Some(("Synced!".to_string(), Instant::now()));
    }

    fn persist_config(&self) {
        let _ = save_config(&self.config_path, &self.config);
    }

    fn persist_position(&mut self) {
        if let Some(pos) = self.orb_anchor_pos {
            self.config.pos_x = Some(pos.x);
            self.config.pos_y = Some(pos.y);
            self.persist_config();
        }
    }

    fn format_pct_label(&self, pct: f64, is_5h: bool) -> String {
        let estimated = !self.quota.calibrated
            || if is_5h {
                self.snapshot.window_5h_since_cal_weighted > 0
            } else {
                self.snapshot.weekly_since_cal_weighted > 0
            };
        if estimated {
            format!("~{pct:.0}%")
        } else {
            format!("{pct:.0}%")
        }
    }

    fn draw_context_menu_contents(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let expand_label = if self.mode == ViewMode::Expanded {
            "Collapse to Orb"
        } else {
            "Expand Card"
        };
        if ui.button(expand_label).clicked() {
            if self.mode == ViewMode::Expanded {
                self.set_mode(ctx, ViewMode::Orb);
            } else {
                self.set_mode(ctx, ViewMode::Expanded);
            }
            ui.close_menu();
        }
        if ui.button("Auto-Sync Now").clicked() {
            self.set_mode(ctx, ViewMode::Expanded);
            self.smart_sync_action(ctx);
            ui.close_menu();
        }
        if ui.button("Paste Sync (Clipboard)").clicked() {
            self.set_mode(ctx, ViewMode::Expanded);
            self.paste_sync_from_clipboard(true);
            ui.close_menu();
        }
        if ui.button("Usage Details").clicked() {
            self.active_tab = ActiveTab::Details;
            self.set_mode(ctx, ViewMode::Expanded);
            ui.close_menu();
        }
        if ui.button("Options & Sync").clicked() {
            self.open_options_tab(ctx);
            ui.close_menu();
        }
        if ui.button("Refresh Now").clicked() {
            self.force_refresh(ctx);
            ui.close_menu();
        }
        ui.separator();
        if ui.button("Quit").clicked() {
            self.persist_position();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            ui.close_menu();
        }
    }

    fn draw_orb_graphic(
        &self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        center: Pos2,
        radius: f32,
        hovered: bool,
    ) {
        let anim_5h = ctx.animate_value_with_time(
            egui::Id::new("orb_5h"),
            (self.quota.five_hour_rem_pct / 100.0) as f32,
            0.25,
        );
        let anim_wk = ctx.animate_value_with_time(
            egui::Id::new("orb_wk"),
            (self.quota.weekly_rem_pct / 100.0) as f32,
            0.25,
        );

        let c5 = remaining_color(self.quota.five_hour_rem_pct);
        let cw = remaining_color(self.quota.weekly_rem_pct);

        let glow_alpha = if hovered { 34 } else { 18 };
        painter.circle_filled(
            center,
            radius + 4.5,
            Color32::from_rgba_unmultiplied(c5.r(), c5.g(), c5.b(), glow_alpha / 2),
        );
        painter.circle_filled(
            center,
            radius + 2.0,
            Color32::from_rgba_unmultiplied(c5.r(), c5.g(), c5.b(), glow_alpha),
        );

        let rim = if hovered {
            Color32::from_rgba_unmultiplied(255, 255, 255, 80)
        } else {
            rim_glass()
        };
        painter.circle_filled(center, radius, bg_glass(self.config.opacity));
        painter.circle_stroke(center, radius, Stroke::new(1.1_f32, rim));

        draw_arc(
            painter,
            center,
            radius - 1.8,
            -PI * 0.78,
            0.28,
            Stroke::new(
                1.0_f32,
                Color32::from_rgba_unmultiplied(255, 255, 255, if hovered { 55 } else { 30 }),
            ),
        );

        let outer_r = radius - 5.2;
        let outer_w = 3.0_f32;
        painter.circle_stroke(center, outer_r, Stroke::new(outer_w, track_dark()));
        draw_capped_arc(painter, center, outer_r, -PI / 2.0, anim_wk, outer_w, cw);

        let inner_r = radius - 10.6;
        let inner_w = 3.0_f32;
        painter.circle_stroke(center, inner_r, Stroke::new(inner_w, track_dark()));
        draw_capped_arc(painter, center, inner_r, -PI / 2.0, anim_5h, inner_w, c5);

        painter.text(
            Pos2::new(center.x, center.y - 2.0),
            egui::Align2::CENTER_CENTER,
            format!("{:.0}%", self.quota.five_hour_rem_pct),
            FontId::proportional(10.5),
            TEXT_PRIMARY,
        );
        painter.text(
            Pos2::new(center.x, center.y + 7.5),
            egui::Align2::CENTER_CENTER,
            "5h",
            FontId::proportional(7.5),
            TEXT_MUTED,
        );
    }

    fn draw_mini_orb(&self, painter: &egui::Painter, center: Pos2, radius: f32) {
        painter.circle_filled(center, radius, bg_glass(self.config.opacity));
        painter.circle_stroke(center, radius, Stroke::new(1.0_f32, rim_glass()));
        let outer_r = radius - 2.4;
        painter.circle_stroke(center, outer_r, Stroke::new(1.8_f32, track_dark()));
        draw_arc(
            painter,
            center,
            outer_r,
            -PI / 2.0,
            (self.quota.weekly_rem_pct / 100.0) as f32,
            Stroke::new(1.8_f32, remaining_color(self.quota.weekly_rem_pct)),
        );
        let inner_r = radius - 5.2;
        painter.circle_stroke(center, inner_r, Stroke::new(1.8_f32, track_dark()));
        draw_arc(
            painter,
            center,
            inner_r,
            -PI / 2.0,
            (self.quota.five_hour_rem_pct / 100.0) as f32,
            Stroke::new(1.8_f32, remaining_color(self.quota.five_hour_rem_pct)),
        );
    }

    fn render_orb_mode(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(ORB_WIN[0], ORB_WIN[1]));
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());
        let hovered = resp.hovered();
        if hovered {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        self.draw_orb_graphic(ctx, ui.painter(), rect.center(), 25.0, hovered);

        if should_start_window_drag(ctx, &resp, 6.0) {
            self.drag_in_progress = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        } else if resp.drag_stopped() {
            self.drag_in_progress = false;
            if let Some(r) = ctx.input(|i| i.viewport().outer_rect) {
                self.orb_anchor_pos = Some(r.min);
                self.persist_position();
            }
        } else if resp.clicked_by(egui::PointerButton::Primary) && !self.drag_in_progress {
            self.set_mode(ctx, ViewMode::Expanded);
        } else if hovered && !self.drag_in_progress && self.config.hover_peek_enabled {
            self.hover_leave_since = None;
            self.set_mode(ctx, ViewMode::Peek);
        }

        resp.context_menu(|ui| self.draw_context_menu_contents(ui, ctx));
    }

    fn render_peek_mode(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let win_rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(PEEK_WIN[0], PEEK_WIN[1]));
        let resp = ui.allocate_rect(win_rect, Sense::click_and_drag());
        if resp.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        let (orb_rect, pill_rect) = if self.peek_on_left {
            let orb_x = PEEK_WIN[0] - ORB_WIN[0];
            (
                Rect::from_min_size(Pos2::new(orb_x, 2.0), Vec2::new(ORB_WIN[0], ORB_WIN[1])),
                Rect::from_min_size(
                    Pos2::new(2.0, 4.0),
                    Vec2::new(PEEK_WIN[0] - ORB_WIN[0] - 6.0, PEEK_WIN[1] - 8.0),
                ),
            )
        } else {
            (
                Rect::from_min_size(Pos2::new(0.0, 2.0), Vec2::new(ORB_WIN[0], ORB_WIN[1])),
                Rect::from_min_size(
                    Pos2::new(ORB_WIN[0] + 4.0, 4.0),
                    Vec2::new(PEEK_WIN[0] - ORB_WIN[0] - 6.0, PEEK_WIN[1] - 8.0),
                ),
            )
        };

        let painter = ui.painter();
        painter.rect_filled(pill_rect, CornerRadius::same(13), bg_glass(self.config.opacity));
        painter.rect_stroke(
            pill_rect,
            CornerRadius::same(13),
            Stroke::new(1.0_f32, rim_glass()),
            egui::StrokeKind::Inside,
        );

        self.draw_orb_graphic(ctx, painter, orb_rect.center(), 25.0, true);

        let pad_x = pill_rect.left() + 10.0;
        let right_x = pill_rect.right() - 10.0;
        let bar_w = (right_x - pad_x).max(40.0);

        let label_5h = self.format_pct_label(self.quota.five_hour_rem_pct, true);
        let label_wk = self.format_pct_label(self.quota.weekly_rem_pct, false);

        self.draw_peek_row(
            painter,
            pad_x,
            right_x,
            pill_rect.top() + 14.0,
            bar_w,
            "5-Hour",
            self.quota.five_hour_rem_pct,
            &label_5h,
            &format_countdown(self.quota.five_hour_countdown),
        );
        self.draw_peek_row(
            painter,
            pad_x,
            right_x,
            pill_rect.top() + 39.0,
            bar_w,
            "Weekly",
            self.quota.weekly_rem_pct,
            &label_wk,
            &format_countdown(self.quota.weekly_countdown),
        );

        let in_grace = self
            .peek_entered_at
            .is_some_and(|t| t.elapsed() < StdDuration::from_millis(280));
        let pointer_inside = ctx.input(|i| i.pointer.hover_pos().is_some());

        if should_start_window_drag(ctx, &resp, 6.0) {
            self.drag_in_progress = true;
            self.set_mode(ctx, ViewMode::Orb);
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        } else if resp.clicked_by(egui::PointerButton::Primary) && !self.drag_in_progress {
            self.set_mode(ctx, ViewMode::Expanded);
        } else if resp.hovered() || pointer_inside || in_grace {
            self.hover_leave_since = None;
        } else {
            let now = Instant::now();
            match self.hover_leave_since {
                None => self.hover_leave_since = Some(now),
                Some(t) if now.duration_since(t) >= StdDuration::from_millis(160) => {
                    self.hover_leave_since = None;
                    self.set_mode(ctx, ViewMode::Orb);
                }
                _ => {}
            }
        }

        resp.context_menu(|ui| self.draw_context_menu_contents(ui, ctx));
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_peek_row(
        &self,
        painter: &egui::Painter,
        left_x: f32,
        right_x: f32,
        y: f32,
        bar_w: f32,
        label: &str,
        rem_pct: f64,
        pct_text: &str,
        countdown: &str,
    ) {
        let color = remaining_color(rem_pct);
        painter.circle_filled(Pos2::new(left_x + 3.0, y), 2.5, color);
        painter.text(
            Pos2::new(left_x + 9.0, y),
            egui::Align2::LEFT_CENTER,
            format!("{label} · {countdown}"),
            FontId::proportional(9.5),
            TEXT_SECONDARY,
        );
        painter.text(
            Pos2::new(right_x, y),
            egui::Align2::RIGHT_CENTER,
            pct_text,
            FontId::proportional(10.5),
            color,
        );
        let track = Rect::from_min_size(Pos2::new(left_x, y + 7.5), Vec2::new(bar_w, 3.5));
        painter.rect_filled(track, CornerRadius::same(2), track_dark());
        let fill_w = (bar_w * (rem_pct / 100.0) as f32).clamp(0.0, bar_w);
        if fill_w > 0.5 {
            let fill = Rect::from_min_size(track.min, Vec2::new(fill_w, 3.5));
            painter.rect_filled(fill, CornerRadius::same(2), color);
        }
    }

    fn render_expanded_mode(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let card_rect = Rect::from_min_size(
            Pos2::new(1.0, 1.0),
            Vec2::new(CARD_WIN[0] - 2.0, CARD_WIN[1] - 2.0),
        );

        let painter = ui.painter().clone();
        painter.rect_filled(card_rect, CornerRadius::same(14), bg_glass(self.config.opacity));
        painter.rect_stroke(
            card_rect,
            CornerRadius::same(14),
            Stroke::new(1.0_f32, rim_glass()),
            egui::StrokeKind::Inside,
        );

        // Header bar
        let header_rect = Rect::from_min_size(
            Pos2::new(card_rect.left() + 10.0, card_rect.top() + 8.0),
            Vec2::new(card_rect.width() - 20.0, 22.0),
        );

        // Clickable mini-orb on the left to collapse back to orb
        let mini_orb_rect =
            Rect::from_min_size(header_rect.min, Vec2::new(20.0, header_rect.height()));
        let mini_resp = ui
            .interact(mini_orb_rect, ui.id().with("mini_orb_collapse"), Sense::click())
            .on_hover_text("Collapse to orb");
        if mini_resp.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        self.draw_mini_orb(
            &painter,
            Pos2::new(header_rect.left() + 9.0, header_rect.center().y),
            8.5,
        );
        if mini_resp.clicked_by(egui::PointerButton::Primary) {
            self.set_mode(ctx, ViewMode::Orb);
            return;
        }

        // Draggable title zone
        let right_controls_w = 86.0;
        let drag_zone = Rect::from_min_size(
            Pos2::new(header_rect.left() + 21.0, header_rect.top()),
            Vec2::new(
                (header_rect.width() - 21.0 - right_controls_w).max(32.0),
                header_rect.height(),
            ),
        );
        let drag_resp = ui.interact(drag_zone, ui.id().with("header_drag"), Sense::click_and_drag());
        if drag_resp.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::Grab);
        }
        if should_start_window_drag(ctx, &drag_resp, 4.0) {
            self.drag_in_progress = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        } else if drag_resp.drag_stopped() {
            self.drag_in_progress = false;
            if let Some(r) = ctx.input(|i| i.viewport().outer_rect) {
                self.expanded_pos = Some(r.min);
                let mon_size = ctx.input(|i| i.viewport().monitor_size);
                let (mw, mh) = mon_size.map(|m| (m.x, m.y)).unwrap_or((1920.0, 1080.0));
                let ax = (r.min.x + CARD_WIN[0] - ORB_WIN[0])
                    .clamp(8.0, (mw - ORB_WIN[0] - 8.0).max(8.0));
                let ay = (r.min.y + CARD_WIN[1] - ORB_WIN[1])
                    .clamp(8.0, (mh - ORB_WIN[1] - 8.0).max(8.0));
                self.orb_anchor_pos = Some(Pos2::new(ax, ay));
                self.persist_position();
            }
        }
        drag_resp.context_menu(|menu_ui| self.draw_context_menu_contents(menu_ui, ctx));

        let active_toast = self
            .toast_msg
            .as_ref()
            .filter(|(_, ts)| ts.elapsed() < StdDuration::from_secs(3))
            .map(|(msg, _)| msg.as_str());
        if let Some(toast) = active_toast {
            painter.text(
                Pos2::new(header_rect.left() + 24.0, header_rect.center().y),
                egui::Align2::LEFT_CENTER,
                toast,
                FontId::proportional(10.5),
                Color32::from_rgb(34, 197, 94),
            );
        } else {
            self.toast_msg = None;
            painter.text(
                Pos2::new(header_rect.left() + 24.0, header_rect.center().y),
                egui::Align2::LEFT_CENTER,
                "Antigravity",
                FontId::proportional(11.5),
                TEXT_PRIMARY,
            );
        }

        // Header right controls: [Sync] [-] [x]
        let btn_y = header_rect.center().y - 9.0;
        let close_rect = Rect::from_min_size(
            Pos2::new(header_rect.right() - 18.0, btn_y),
            Vec2::new(18.0, 18.0),
        );
        let min_rect = Rect::from_min_size(
            Pos2::new(close_rect.left() - 20.0, btn_y),
            Vec2::new(18.0, 18.0),
        );
        let sync_rect = Rect::from_min_size(
            Pos2::new(min_rect.left() - 44.0, btn_y),
            Vec2::new(40.0, 18.0),
        );

        if self.header_pill_button(
            ui,
            ctx,
            "hdr_sync",
            sync_rect,
            if self.scanning_live { "..." } else { "Sync" },
            "Auto-read quota from open Antigravity window",
            true,
        ) {
            self.smart_sync_action(ctx);
        }
        if self.header_pill_button(
            ui,
            ctx,
            "hdr_min",
            min_rect,
            "-",
            "Collapse to orb (Esc)",
            false,
        ) {
            self.set_mode(ctx, ViewMode::Orb);
        }
        if self.header_pill_button(ui, ctx, "hdr_close", close_rect, "x", "Quit", false) {
            self.persist_position();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // Divider
        let div_y = card_rect.top() + 34.0;
        painter.line_segment(
            [
                Pos2::new(card_rect.left() + 10.0, div_y),
                Pos2::new(card_rect.right() - 10.0, div_y),
            ],
            Stroke::new(1.0_f32, rim_glass()),
        );

        // Compact 3-tab selector bar
        let tab_bar_rect = Rect::from_min_size(
            Pos2::new(card_rect.left() + 12.0, div_y + 6.0),
            Vec2::new(card_rect.width() - 24.0, 22.0),
        );
        self.render_tab_bar(ui, ctx, tab_bar_rect);

        let body_rect = Rect::from_min_max(
            Pos2::new(card_rect.left() + 12.0, tab_bar_rect.bottom() + 8.0),
            Pos2::new(card_rect.right() - 12.0, card_rect.bottom() - 10.0),
        );

        match self.active_tab {
            ActiveTab::Usage => self.render_usage_tab(ui, ctx, &painter, body_rect),
            ActiveTab::Details => self.render_details_tab(ui, ctx, &painter, body_rect),
            ActiveTab::Options => self.render_options_tab(ui, ctx, body_rect),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn header_pill_button(
        &self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        id_str: &str,
        r: Rect,
        text: &str,
        tooltip: &str,
        accent: bool,
    ) -> bool {
        let resp = ui
            .interact(r, ui.id().with(id_str), Sense::click())
            .on_hover_text(tooltip);
        if resp.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let fill = if accent {
            if resp.hovered() {
                Color32::from_rgba_unmultiplied(34, 197, 94, 58)
            } else {
                Color32::from_rgba_unmultiplied(34, 197, 94, 32)
            }
        } else if resp.hovered() {
            surface_card(self.config.opacity)
        } else {
            Color32::TRANSPARENT
        };
        if fill != Color32::TRANSPARENT {
            ui.painter().rect_filled(r, CornerRadius::same(6), fill);
        }
        if accent {
            ui.painter().rect_stroke(
                r,
                CornerRadius::same(6),
                Stroke::new(
                    1.0_f32,
                    Color32::from_rgba_unmultiplied(34, 197, 94, 110),
                ),
                egui::StrokeKind::Inside,
            );
        }
        let text_col = if accent {
            Color32::from_rgb(74, 222, 128)
        } else if resp.hovered() {
            TEXT_PRIMARY
        } else {
            TEXT_SECONDARY
        };
        ui.painter().text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            text,
            FontId::proportional(if accent { 10.0 } else { 12.0 }),
            text_col,
        );
        resp.clicked()
    }

    fn render_tab_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, rect: Rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(7), surface_card(self.config.opacity));
        painter.rect_stroke(
            rect,
            CornerRadius::same(7),
            Stroke::new(1.0_f32, rim_glass()),
            egui::StrokeKind::Inside,
        );

        let tabs = [
            (ActiveTab::Usage, "Usage"),
            (ActiveTab::Details, "Details"),
            (ActiveTab::Options, "Options"),
        ];
        let tab_w = (rect.width() - 4.0) / 3.0;
        for (idx, (tab, label)) in tabs.into_iter().enumerate() {
            let tr = Rect::from_min_size(
                Pos2::new(rect.left() + 2.0 + idx as f32 * tab_w, rect.top() + 2.0),
                Vec2::new(tab_w, rect.height() - 4.0),
            );
            let resp = ui.interact(tr, ui.id().with(("tab", idx)), Sense::click());
            if resp.hovered() {
                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            let selected = self.active_tab == tab;
            if selected {
                ui.painter().rect_filled(
                    tr,
                    CornerRadius::same(5),
                    Color32::from_rgba_unmultiplied(45, 55, 82, 240),
                );
            }
            let col = if selected {
                TEXT_PRIMARY
            } else if resp.hovered() {
                TEXT_SECONDARY
            } else {
                TEXT_MUTED
            };
            ui.painter().text(
                tr.center(),
                egui::Align2::CENTER_CENTER,
                label,
                FontId::proportional(10.0),
                col,
            );
            if resp.clicked() {
                if tab == ActiveTab::Options {
                    self.populate_sync_fields();
                }
                self.active_tab = tab;
            }
        }
    }

    fn render_usage_tab(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        painter: &egui::Painter,
        rect: Rect,
    ) {
        let w = rect.width();
        let five_sub = if self.quota.calibrated {
            format!(
                "{} · resets {}",
                format_tokens(self.snapshot.window_5h_tokens),
                format_countdown(self.quota.five_hour_countdown)
            )
        } else {
            format!(
                "{} · click Sync or Ctrl+V",
                format_tokens(self.snapshot.window_5h_tokens)
            )
        };
        let label_5h = self.format_pct_label(self.quota.five_hour_rem_pct, true);
        self.draw_meter_section(
            ctx,
            painter,
            "bar_5h",
            rect.left(),
            rect.top(),
            w,
            "5-Hour Left",
            self.quota.five_hour_rem_pct,
            &label_5h,
            &five_sub,
        );

        let wk_sub = if self.quota.calibrated {
            format!(
                "{} · resets {}",
                format_tokens(self.snapshot.weekly_tokens),
                format_countdown(self.quota.weekly_countdown)
            )
        } else {
            format!(
                "{} · click Sync or Ctrl+V",
                format_tokens(self.snapshot.weekly_tokens)
            )
        };
        let label_wk = self.format_pct_label(self.quota.weekly_rem_pct, false);
        self.draw_meter_section(
            ctx,
            painter,
            "bar_wk",
            rect.left(),
            rect.top() + 42.0,
            w,
            "Weekly Left",
            self.quota.weekly_rem_pct,
            &label_wk,
            &wk_sub,
        );

        // Quick action row: [Auto Sync] [Manual] [Refresh]
        let btn_y = rect.top() + 86.0;
        let gap = 6.0;
        let bw = (w - gap * 2.0) / 3.0;
        let b_auto = Rect::from_min_size(Pos2::new(rect.left(), btn_y), Vec2::new(bw, 22.0));
        let b_manual =
            Rect::from_min_size(Pos2::new(rect.left() + bw + gap, btn_y), Vec2::new(bw, 22.0));
        let b_refresh = Rect::from_min_size(
            Pos2::new(rect.left() + (bw + gap) * 2.0, btn_y),
            Vec2::new(bw, 22.0),
        );

        if self.action_chip(
            ui,
            ctx,
            "act_auto",
            b_auto,
            if self.scanning_live { "Scanning" } else { "Auto Sync" },
            "Read quota automatically from open Antigravity window",
            true,
        ) {
            self.smart_sync_action(ctx);
        }
        if self.action_chip(
            ui,
            ctx,
            "act_manual",
            b_manual,
            "Manual",
            "Enter quota % and reset countdown manually",
            false,
        ) {
            self.open_options_tab(ctx);
        }
        if self.action_chip(
            ui,
            ctx,
            "act_refresh",
            b_refresh,
            "Refresh",
            "Force full transcript rescan and live sync check",
            false,
        ) {
            self.force_refresh(ctx);
        }

        // Compact footer strip
        let footer = Rect::from_min_max(
            Pos2::new(rect.left(), rect.top() + 114.0),
            Pos2::new(rect.right(), rect.bottom()),
        );
        let footer_resp = ui.interact(footer, ui.id().with("footer_menu"), Sense::click());
        footer_resp.context_menu(|menu_ui| self.draw_context_menu_contents(menu_ui, ctx));

        painter.rect_filled(footer, CornerRadius::same(7), surface_card(self.config.opacity));
        painter.rect_stroke(
            footer,
            CornerRadius::same(7),
            Stroke::new(1.0_f32, rim_glass()),
            egui::StrokeKind::Inside,
        );

        painter.text(
            Pos2::new(footer.left() + 8.0, footer.center().y),
            egui::Align2::LEFT_CENTER,
            format!(
                "Today {} · {}t · {}",
                format_tokens(self.snapshot.today_tokens),
                self.snapshot.today_turns,
                self.snapshot.top_model
            ),
            FontId::proportional(9.5),
            TEXT_SECONDARY,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn action_chip(
        &self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        id_str: &str,
        r: Rect,
        label: &str,
        tooltip: &str,
        accent: bool,
    ) -> bool {
        let resp = ui
            .interact(r, ui.id().with(id_str), Sense::click())
            .on_hover_text(tooltip);
        if resp.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let fill = if accent {
            if resp.hovered() {
                Color32::from_rgba_unmultiplied(34, 197, 94, 52)
            } else {
                Color32::from_rgba_unmultiplied(34, 197, 94, 28)
            }
        } else if resp.hovered() {
            Color32::from_rgba_unmultiplied(40, 48, 72, 235)
        } else {
            surface_card(self.config.opacity)
        };
        let stroke_col = if accent {
            Color32::from_rgba_unmultiplied(34, 197, 94, 100)
        } else {
            rim_glass()
        };
        ui.painter().rect_filled(r, CornerRadius::same(6), fill);
        ui.painter().rect_stroke(
            r,
            CornerRadius::same(6),
            Stroke::new(1.0_f32, stroke_col),
            egui::StrokeKind::Inside,
        );
        let text_col = if accent {
            Color32::from_rgb(74, 222, 128)
        } else if resp.hovered() {
            TEXT_PRIMARY
        } else {
            TEXT_SECONDARY
        };
        ui.painter().text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            label,
            FontId::proportional(9.8),
            text_col,
        );
        resp.clicked()
    }

    fn render_details_tab(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        painter: &egui::Painter,
        rect: Rect,
    ) {
        // Card 1: Today + Window & Burn Rate
        let top_card = Rect::from_min_size(rect.min, Vec2::new(rect.width(), 54.0));
        painter.rect_filled(top_card, CornerRadius::same(8), surface_card(self.config.opacity));
        painter.rect_stroke(
            top_card,
            CornerRadius::same(8),
            Stroke::new(1.0_f32, rim_glass()),
            egui::StrokeKind::Inside,
        );

        let lx = top_card.left() + 8.0;
        let rx = top_card.right() - 8.0;
        painter.text(
            Pos2::new(lx, top_card.top() + 12.0),
            egui::Align2::LEFT_CENTER,
            format!(
                "Today: In {} · Out {} · Cache {}",
                format_tokens(self.snapshot.today_input),
                format_tokens(self.snapshot.today_output),
                format_tokens(self.snapshot.today_cache)
            ),
            FontId::proportional(9.5),
            TEXT_PRIMARY,
        );
        painter.text(
            Pos2::new(lx, top_card.top() + 28.0),
            egui::Align2::LEFT_CENTER,
            format!(
                "Cache Hit: {:.0}%  ·  5h: {} ({}t)",
                self.snapshot.today_cache_hit_pct,
                format_tokens(self.snapshot.window_5h_tokens),
                self.snapshot.window_5h_turns
            ),
            FontId::proportional(9.2),
            TEXT_SECONDARY,
        );
        painter.text(
            Pos2::new(lx, top_card.top() + 43.0),
            egui::Align2::LEFT_CENTER,
            format!(
                "7d: {} ({}t)  ·  Rate: {}/hr",
                format_tokens(self.snapshot.weekly_tokens),
                self.snapshot.weekly_turns,
                format_tokens(self.snapshot.burn_rate_per_hour)
            ),
            FontId::proportional(9.2),
            TEXT_MUTED,
        );

        // Card 2: Top Models (7d)
        let mod_card = Rect::from_min_max(
            Pos2::new(rect.left(), top_card.bottom() + 8.0),
            Pos2::new(rect.right(), rect.bottom()),
        );
        let mod_resp = ui.interact(mod_card, ui.id().with("details_menu"), Sense::click());
        mod_resp.context_menu(|menu_ui| self.draw_context_menu_contents(menu_ui, ctx));

        painter.rect_filled(mod_card, CornerRadius::same(8), surface_card(self.config.opacity));
        painter.rect_stroke(
            mod_card,
            CornerRadius::same(8),
            Stroke::new(1.0_f32, rim_glass()),
            egui::StrokeKind::Inside,
        );

        painter.text(
            Pos2::new(mod_card.left() + 8.0, mod_card.top() + 12.0),
            egui::Align2::LEFT_CENTER,
            "Top Models (Active Window)",
            FontId::proportional(9.5),
            TEXT_SECONDARY,
        );

        let total_model_tok: u64 = self
            .snapshot
            .model_breakdown
            .iter()
            .map(|(_, t)| *t)
            .sum::<u64>()
            .max(1);
        let palette = [
            Color32::from_rgb(34, 197, 94),
            Color32::from_rgb(56, 189, 248),
            Color32::from_rgb(168, 85, 247),
        ];

        if self.snapshot.model_breakdown.is_empty() {
            painter.text(
                mod_card.center(),
                egui::Align2::CENTER_CENTER,
                "No model usage recorded yet",
                FontId::proportional(9.5),
                TEXT_MUTED,
            );
        } else {
            let bar_w = mod_card.width() - 16.0;
            for (idx, (name, tok)) in self.snapshot.model_breakdown.iter().enumerate() {
                let row_y = mod_card.top() + 27.0 + idx as f32 * 18.0;
                let share = (*tok as f64 / total_model_tok as f64) * 100.0;
                let col = palette[idx % palette.len()];

                painter.text(
                    Pos2::new(lx, row_y),
                    egui::Align2::LEFT_CENTER,
                    name,
                    FontId::proportional(9.3),
                    TEXT_PRIMARY,
                );
                painter.text(
                    Pos2::new(rx, row_y),
                    egui::Align2::RIGHT_CENTER,
                    format!("{share:.0}% · {}", format_tokens(*tok)),
                    FontId::proportional(9.0),
                    TEXT_SECONDARY,
                );

                let track = Rect::from_min_size(Pos2::new(lx, row_y + 6.0), Vec2::new(bar_w, 3.0));
                painter.rect_filled(track, CornerRadius::same(2), track_dark());
                let fill_w = (bar_w * (share / 100.0) as f32).clamp(2.0, bar_w);
                painter.rect_filled(
                    Rect::from_min_size(track.min, Vec2::new(fill_w, 3.0)),
                    CornerRadius::same(2),
                    col,
                );
            }
        }
    }

    fn render_options_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, rect: Rect) {
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
        child.spacing_mut().item_spacing = Vec2::new(4.0, 4.0);

        let paste_btn = egui::Button::new(
            egui::RichText::new("Paste from Antigravity Settings")
                .size(10.0)
                .color(Color32::from_rgb(74, 222, 128)),
        )
        .min_size(Vec2::new(rect.width(), 20.0))
        .fill(Color32::from_rgba_unmultiplied(34, 197, 94, 24))
        .stroke(Stroke::new(
            1.0_f32,
            Color32::from_rgba_unmultiplied(34, 197, 94, 90),
        ))
        .corner_radius(CornerRadius::same(6));
        if child.add(paste_btn).clicked() {
            self.paste_sync_from_clipboard(true);
        }

        egui::Grid::new("sync_grid")
            .num_columns(4)
            .spacing([4.0, 4.0])
            .show(&mut child, |ui| {
                ui.label(egui::RichText::new("5h").size(10.0).color(TEXT_SECONDARY));
                ui.add(
                    egui::TextEdit::singleline(&mut self.sync_5h_pct)
                        .desired_width(36.0)
                        .hint_text("97"),
                );
                ui.label(egui::RichText::new("%").size(9.5).color(TEXT_MUTED));
                ui.add(
                    egui::TextEdit::singleline(&mut self.sync_5h_reset)
                        .desired_width(72.0)
                        .hint_text("4h 47m"),
                );
                ui.end_row();

                ui.label(egui::RichText::new("Wk").size(10.0).color(TEXT_SECONDARY));
                ui.add(
                    egui::TextEdit::singleline(&mut self.sync_wk_pct)
                        .desired_width(36.0)
                        .hint_text("99"),
                );
                ui.label(egui::RichText::new("%").size(9.5).color(TEXT_MUTED));
                ui.add(
                    egui::TextEdit::singleline(&mut self.sync_wk_reset)
                        .desired_width(72.0)
                        .hint_text("6d 23h"),
                );
                ui.end_row();
            });

        child.horizontal(|ui| {
            let save_btn = egui::Button::new(
                egui::RichText::new("Save & Sync")
                    .size(10.0)
                    .strong()
                    .color(Color32::from_rgb(9, 13, 22)),
            )
            .fill(Color32::from_rgb(34, 197, 94))
            .corner_radius(CornerRadius::same(5));
            if ui.add(save_btn).clicked() {
                self.apply_manual_sync();
            }

            let reset_btn = egui::Button::new(
                egui::RichText::new("Reset")
                    .size(10.0)
                    .color(TEXT_SECONDARY),
            )
            .fill(surface_card(self.config.opacity))
            .corner_radius(CornerRadius::same(5));
            if ui.add(reset_btn).clicked() {
                self.reset_calibration();
            }

            if !self.sync_error.is_empty() {
                ui.label(
                    egui::RichText::new(&self.sync_error)
                        .size(9.0)
                        .color(Color32::from_rgb(244, 63, 94)),
                );
            }
        });

        child.separator();

        // Overlay preferences (saved live)
        let mut prefs_changed = false;
        child.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Opacity")
                    .size(9.8)
                    .color(TEXT_SECONDARY),
            );
            let slider = egui::Slider::new(&mut self.config.opacity, 0.65..=1.0)
                .show_value(false);
            if ui.add_sized([120.0, 16.0], slider).changed() {
                prefs_changed = true;
            }
            ui.label(
                egui::RichText::new(format!("{:.0}%", self.config.opacity * 100.0))
                    .size(9.5)
                    .color(TEXT_MUTED),
            );
        });

        child.horizontal(|ui| {
            if ui
                .checkbox(
                    &mut self.config.auto_sync_enabled,
                    egui::RichText::new("Auto").size(9.5).color(TEXT_SECONDARY),
                )
                .changed()
            {
                self.auto_worker.set_enabled(self.config.auto_sync_enabled);
                prefs_changed = true;
            }

            if ui
                .checkbox(
                    &mut self.config.always_on_top,
                    egui::RichText::new("Top").size(9.5).color(TEXT_SECONDARY),
                )
                .changed()
            {
                let level = if self.config.always_on_top {
                    egui::WindowLevel::AlwaysOnTop
                } else {
                    egui::WindowLevel::Normal
                };
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
                prefs_changed = true;
            }

            if ui
                .checkbox(
                    &mut self.config.hover_peek_enabled,
                    egui::RichText::new("Peek").size(9.5).color(TEXT_SECONDARY),
                )
                .changed()
            {
                prefs_changed = true;
            }

            let poll_label = format!("{}s", self.config.poll_interval_secs);
            let poll_btn = egui::Button::new(
                egui::RichText::new(poll_label)
                    .size(9.3)
                    .color(TEXT_SECONDARY),
            )
            .fill(surface_card(self.config.opacity))
            .corner_radius(CornerRadius::same(5));
            if ui.add(poll_btn).clicked() {
                self.config.poll_interval_secs = match self.config.poll_interval_secs {
                    3 => 5,
                    5 => 10,
                    _ => 3,
                };
                prefs_changed = true;
            }
        });

        if prefs_changed {
            self.persist_config();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_meter_section(
        &self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        id_tag: &str,
        x: f32,
        y: f32,
        w: f32,
        title: &str,
        rem_pct: f64,
        pct_text: &str,
        subtitle: &str,
    ) {
        let col = remaining_color(rem_pct);
        let anim_frac =
            ctx.animate_value_with_time(egui::Id::new(id_tag), (rem_pct / 100.0) as f32, 0.25);

        painter.text(
            Pos2::new(x, y + 6.0),
            egui::Align2::LEFT_CENTER,
            title,
            FontId::proportional(10.5),
            TEXT_SECONDARY,
        );
        painter.text(
            Pos2::new(x + w, y + 6.0),
            egui::Align2::RIGHT_CENTER,
            pct_text,
            FontId::proportional(11.5),
            col,
        );

        let bar = Rect::from_min_size(Pos2::new(x, y + 16.0), Vec2::new(w, 5.0));
        painter.rect_filled(bar, CornerRadius::same(3), track_dark());
        let fill_w = (w * anim_frac).clamp(0.0, w);
        if fill_w > 0.5 {
            painter.rect_filled(
                Rect::from_min_size(bar.min, Vec2::new(fill_w, 5.0)),
                CornerRadius::same(3),
                col,
            );
        }

        painter.text(
            Pos2::new(x + w, y + 29.0),
            egui::Align2::RIGHT_CENTER,
            subtitle,
            FontId::proportional(9.2),
            TEXT_MUTED,
        );
    }
}

impl eframe::App for UsageOverlayApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.initialized_pos {
            self.initialized_pos = true;
            let mon = ctx.input(|i| i.viewport().monitor_size);
            let (mon_w, mon_h) = mon.map(|m| (m.x, m.y)).unwrap_or((1920.0, 1080.0));
            let px = self
                .config
                .pos_x
                .unwrap_or(mon_w - ORB_WIN[0] - 28.0)
                .clamp(8.0, (mon_w - ORB_WIN[0] - 8.0).max(8.0));
            let py = self
                .config
                .pos_y
                .unwrap_or(mon_h - ORB_WIN[1] - 76.0)
                .clamp(8.0, (mon_h - ORB_WIN[1] - 8.0).max(8.0));
            self.orb_anchor_pos = Some(Pos2::new(px, py));
            self.transition_guard_until = Some(Instant::now() + StdDuration::from_millis(320));
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(Pos2::new(px, py)));
        }

        if !ctx.input(|i| i.pointer.primary_down()) {
            self.drag_in_progress = false;
        }

        self.process_auto_sync_messages();

        let poll_secs = self.config.poll_interval_secs.clamp(2, 60);
        if self.last_poll.elapsed() >= StdDuration::from_secs(poll_secs) {
            self.refresh_data();
        }

        let guard_active = self
            .transition_guard_until
            .is_some_and(|until| Instant::now() < until);
        if self.mode == ViewMode::Orb && !guard_active {
            if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                if (rect.size().x - ORB_WIN[0]).abs() < 4.0 {
                    self.orb_anchor_pos = Some(rect.min);
                }
            }
        } else if self.mode == ViewMode::Expanded && !guard_active {
            if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                if (rect.size().x - CARD_WIN[0]).abs() < 4.0 && self.expanded_pos != Some(rect.min) {
                    self.expanded_pos = Some(rect.min);
                    let mon_size = ctx.input(|i| i.viewport().monitor_size);
                    let (mw, mh) = mon_size.map(|m| (m.x, m.y)).unwrap_or((1920.0, 1080.0));
                    let ax = (rect.min.x + CARD_WIN[0] - ORB_WIN[0])
                        .clamp(8.0, (mw - ORB_WIN[0] - 8.0).max(8.0));
                    let ay = (rect.min.y + CARD_WIN[1] - ORB_WIN[1])
                        .clamp(8.0, (mh - ORB_WIN[1] - 8.0).max(8.0));
                    self.orb_anchor_pos = Some(Pos2::new(ax, ay));
                }
            }
        }

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && self.mode == ViewMode::Expanded {
            self.set_mode(ctx, ViewMode::Orb);
        }

        let paste_hotkey = ctx.input(|i| {
            i.events
                .iter()
                .any(|e| matches!(e, egui::Event::Paste(_)))
        });
        if paste_hotkey
            && self.mode == ViewMode::Expanded
            && self.active_tab != ActiveTab::Options
        {
            self.paste_sync_from_clipboard(true);
        }

        let frame = egui::Frame::NONE.fill(Color32::TRANSPARENT);
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            match self.mode {
                ViewMode::Orb => self.render_orb_mode(ui, ctx),
                ViewMode::Peek => self.render_peek_mode(ui, ctx),
                ViewMode::Expanded => self.render_expanded_mode(ui, ctx),
            }
        });

        let needs_fast_repaint =
            self.mode == ViewMode::Peek || guard_active || ctx.input(|i| i.pointer.hover_pos().is_some());
        if needs_fast_repaint {
            ctx.request_repaint_after(StdDuration::from_millis(16));
        } else {
            ctx.request_repaint_after(StdDuration::from_secs(1));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.auto_worker.shutdown();
        self.persist_position();
    }
}

fn draw_arc(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    start_angle: f32,
    fraction: f32,
    stroke: Stroke,
) {
    let clamped = fraction.clamp(0.0, 1.0);
    if clamped <= 0.001 || radius <= 0.5 {
        return;
    }
    let segments = 64;
    let total_angle = clamped * 2.0 * PI;
    let steps = ((segments as f32 * clamped).ceil() as usize).max(2);
    let points: Vec<Pos2> = (0..=steps)
        .map(|i| {
            let t = i as f32 / steps as f32;
            let ang = start_angle + t * total_angle;
            Pos2::new(center.x + radius * ang.cos(), center.y + radius * ang.sin())
        })
        .collect();
    painter.add(egui::Shape::line(points, stroke));
}

fn draw_capped_arc(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    start_angle: f32,
    fraction: f32,
    width: f32,
    color: Color32,
) {
    let clamped = fraction.clamp(0.0, 1.0);
    if clamped <= 0.005 || radius <= 0.5 {
        return;
    }
    draw_arc(painter, center, radius, start_angle, clamped, Stroke::new(width, color));
    let cap_r = width * 0.5;
    let start_pos = Pos2::new(
        center.x + radius * start_angle.cos(),
        center.y + radius * start_angle.sin(),
    );
    let end_angle = start_angle + clamped * 2.0 * PI;
    let end_pos = Pos2::new(
        center.x + radius * end_angle.cos(),
        center.y + radius * end_angle.sin(),
    );
    painter.circle_filled(start_pos, cap_r, color);
    painter.circle_filled(end_pos, cap_r, color);
}
