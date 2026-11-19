//! Panels, menus and dialogs of the Linux UI, laid out as on Windows.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Align2, Button, Color32, FontId, Key, Layout, PointerButton, Pos2, Rect, RichText, ScrollArea, Sense, Stroke, TextEdit, Ui};

use super::*;

/// How long a finished, clean transfer stays in the jobs panel.
pub(super) const JOB_LINGER: Duration = Duration::from_secs(8);

impl App {
    pub(super) fn draw(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        if self.elevated {
            egui::Panel::top("elevated").show(ui, |ui| {
                ui.colored_label(RED, "⚠ FEx is running as root: nothing you do here will ask for a password. Unless you need that, close it and start FEx as yourself.");
            });
        }
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        if self.tabs.len() > 1 {
            egui::Panel::top("tabs").show(ui, |ui| self.tab_strip(ui));
        }
        // A hidden status bar still comes back to report a problem.
        if self.look.status_bar || self.error.is_some() || self.search_error.is_some() || self.notice.is_some() {
            egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        }
        if !self.jobs.is_empty() {
            egui::Panel::bottom("jobs").show(ui, |ui| {
                ui.add_space(2.0);
                self.jobs_panel(ui);
                ui.add_space(2.0);
            });
        }
        if self.look.sidebar {
            egui::Panel::left("sidebar").default_size(190.0).resizable(true).show(ui, |ui| self.sidebar(ui));
        }
        egui::CentralPanel::default().show(ui, |ui| {
            if self.cwd == NETWORK {
                self.network_page(ui);
            } else {
                self.table(ui);
            }
        });
        self.update_drag(ctx);
        self.update_tab_drag(ctx);
        self.dialogs(ctx);
        self.net_dialogs(ctx);
        self.appearance_window(ctx);
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.add_space(3.0);
        ui.horizontal(|ui| {
            let nav = |ui: &mut Ui, enabled: bool, icon: &str, tip: &str| {
                ui.add_enabled(enabled, Button::new(icon).frame(false).min_size(egui::vec2(22.0, 22.0))).on_hover_text(tip).clicked()
            };
            if self.look.nav_buttons {
                if nav(ui, !self.back.is_empty(), "⬅", "Back (Alt+←)") {
                    self.go_back();
                }
                if nav(ui, !self.forward.is_empty(), "➡", "Forward (Alt+→)") {
                    self.go_forward();
                }
                if nav(ui, parent_of(&self.cwd).is_some(), "⬆", "Up (Alt+↑)") {
                    self.go_up();
                }
            }
            let mut new_action = None;
            if self.look.new_button {
                ui.add_enabled_ui(is_real(&self.cwd), |ui| {
                    ui.menu_button("➕", |ui| self.new_menu(ui, &mut new_action))
                        .response
                        .on_hover_text("New folder or file (Ctrl+Shift+N, Ctrl+Alt+N)");
                });
            }
            if new_action.is_some() {
                let ctx = ui.ctx().clone();
                self.run_action(new_action, &ctx);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.menu_button("☰", |ui| self.main_menu(ui));
                let hint = match (self.regex, self.recursive) {
                    (false, true) => "Search",
                    (false, false) => "Filter this folder",
                    (true, true) => "Search (regex)",
                    (true, false) => "Filter this folder (regex)",
                };
                // Without the path bar the search box takes the whole row.
                let width = if self.look.path_bar { 200.0 } else { ui.available_width() };
                let mut search = TextEdit::singleline(&mut self.query).id(egui::Id::unique("search_bar")).hint_text(hint).desired_width(width);
                if self.search_error.is_some() {
                    search = search.text_color(RED);
                }
                if !self.look.search_bar {
                    if self.look.path_bar {
                        self.path_bar(ui);
                    }
                    return;
                }
                let resp = ui.add(search);
                let indexed = self.daemon_covers(&self.cwd) || self.indexes.iter().any(|i| i.covers(&self.cwd));
                let tip = self.search_error.clone().unwrap_or_else(|| {
                    format!("Keywords, *.ext wildcards, type:ext (Ctrl+F){}", if indexed && self.recursive { "\nThis folder is indexed" } else { "" })
                });
                let resp = resp.on_hover_text(tip);
                if resp.changed() {
                    self.query_dirty = Some(Instant::now());
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && self.query_dirty.is_some() {
                    self.start_search();
                }
                if self.look.path_bar {
                    self.path_bar(ui);
                }
            });
        });
        ui.add_space(3.0);
    }

    /// The ☰ menu: view options and windows.
    fn main_menu(&mut self, ui: &mut Ui) {
        if ui.add(Button::new("New tab").shortcut_text("Ctrl+T")).clicked() {
            self.open_tab(self.cwd.clone());
            ui.close();
        }
        if ui.add(Button::new("New window").shortcut_text("Ctrl+N")).clicked() {
            self.open_window(&self.cwd.clone(), None);
            ui.close();
        }
        ui.separator();
        if ui.add(Button::selectable(self.show_hidden, "Show hidden files").shortcut_text("Ctrl+H")).clicked() {
            self.toggle_hidden();
        }
        let sub = ui.add(Button::selectable(self.recursive, "Search subfolders")).on_hover_text("Search below this folder (uses the index)");
        let rx = ui.add(Button::selectable(self.regex, "Regular expressions"));
        if sub.clicked() {
            self.recursive = !self.recursive;
        }
        if rx.clicked() {
            self.regex = !self.regex;
        }
        if (sub.clicked() || rx.clicked()) && !self.query.is_empty() {
            self.start_search();
        }
        if ui
            .add(Button::selectable(self.report_network_space, "Network drive capacity"))
            .on_hover_text("Show free space for network volumes (queries can be slow)")
            .clicked()
        {
            self.report_network_space = !self.report_network_space;
        }
        ui.separator();
        if ui.button("Appearance and layout…").clicked() {
            self.look_open = true;
            self.look_font = self.look.font.clone();
            ui.close();
        }
        if ui.add(Button::new("Keyboard shortcuts").shortcut_text("F1")).clicked() {
            self.help_open = true;
            ui.close();
        }
    }

    /// Breadcrumbs for the open location: click one to go there, middle-click
    /// for a new tab, drop items on one to move them there. Clicking the empty
    /// part (or Ctrl+L) switches to typing a path.
    fn path_bar(&mut self, ui: &mut Ui) {
        let path_id = egui::Id::unique("path_bar");
        let focused = ui.memory(|m| m.has_focus(path_id));
        if !focused && !self.path_editing {
            let shown = lossy(&self.cwd);
            if self.path_text != shown {
                self.path_text = shown;
            }
        }
        if focused || self.path_editing {
            let resp = ui.add(TextEdit::singleline(&mut self.path_text).id(path_id).desired_width(ui.available_width()));
            if std::mem::take(&mut self.path_editing) {
                resp.request_focus();
                let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(self.path_text.chars().count()));
                let mut state = TextEdit::load_state(ui.ctx(), path_id).unwrap_or_default();
                state.cursor.set_char_range(Some(all));
                state.store(ui.ctx(), path_id);
                self.path_editing = true;
            }
            if resp.lost_focus() {
                self.path_editing = false;
                if ui.input(|i| i.key_pressed(Key::Enter)) {
                    self.submit_path();
                }
            }
            return;
        }

        let height = ui.spacing().interact_size.y;
        let (rect, bar) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), Sense::click());
        let visuals = ui.visuals().clone();
        ui.painter().rect(rect, 2.0, visuals.extreme_bg_color, visuals.widgets.inactive.bg_stroke, egui::StrokeKind::Inside);
        let font = egui::TextStyle::Button.resolve(ui.style());
        let crumbs = breadcrumbs(&self.cwd, &self.volumes);
        let galleys: Vec<_> = crumbs.iter().map(|(label, _)| ui.painter().layout_no_wrap(label.clone(), font.clone(), visuals.text_color())).collect();
        let sep = ui.painter().layout_no_wrap("›".into(), font.clone(), visuals.weak_text_color());
        let (pad, gap) = (6.0, sep.size().x + 4.0);
        // Show as many trailing segments as fit; earlier ones collapse into "…".
        let ellipsis = ui.painter().layout_no_wrap("… ›".into(), font.clone(), visuals.weak_text_color());
        let mut first = crumbs.len().saturating_sub(1);
        let mut width = galleys.last().map_or(0.0, |g| g.size().x + 2.0 * pad);
        while first > 0 {
            let w = galleys[first - 1].size().x + 2.0 * pad + gap;
            let reserve = if first > 1 { ellipsis.size().x + 8.0 } else { 0.0 };
            if width + w + reserve > rect.width() - 8.0 {
                break;
            }
            width += w;
            first -= 1;
        }
        let painter = ui.painter_at(rect);
        let mut x = rect.left() + 2.0;
        if first > 0 {
            let w = ellipsis.size().x;
            painter.galley(egui::pos2(x + 4.0, rect.center().y - ellipsis.size().y / 2.0), ellipsis, visuals.weak_text_color());
            x += 8.0 + w;
        }
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let (mut go, mut tab) = (None, None);
        for (i, ((_, location), galley)) in crumbs.iter().zip(galleys).enumerate().skip(first) {
            let r = Rect::from_min_size(egui::pos2(x, rect.top() + 1.0), egui::vec2(galley.size().x + 2.0 * pad, height - 2.0)).intersect(rect);
            let resp = ui.interact(r, ui.scope_id().with(("crumb", i)), Sense::click()).on_hover_text(lossy(location));
            let droppable = accepts_files(location);
            let drop_here = self.drag.is_some() && droppable && pointer.is_some_and(|p| r.contains(p));
            if drop_here {
                self.drop_target = Some(location.clone());
            }
            if droppable {
                self.zones.push((r, location.clone()));
            }
            if drop_here || resp.hovered() {
                painter.rect_filled(r, 2.0, visuals.widgets.hovered.weak_bg_fill);
            }
            if drop_here {
                painter.rect_stroke(r, 2.0, Stroke::new(1.5, visuals.selection.stroke.color), egui::StrokeKind::Inside);
            }
            let color = if i + 1 == crumbs.len() { visuals.strong_text_color() } else { visuals.text_color() };
            painter.galley(egui::pos2(r.left() + pad, rect.center().y - galley.size().y / 2.0), galley, color);
            if resp.clicked() && *location != self.cwd {
                go = Some(location.clone());
            } else if resp.middle_clicked() {
                tab = Some(location.clone());
            }
            x = r.right();
            if i + 1 < crumbs.len() {
                painter.galley(egui::pos2(x + 2.0, rect.center().y - sep.size().y / 2.0), sep.clone(), visuals.weak_text_color());
                x += gap;
            }
        }
        if bar.clicked() {
            self.path_editing = true;
        }
        bar.on_hover_cursor(egui::CursorIcon::Text);
        if let Some(location) = go {
            self.navigate(location, true);
        } else if let Some(location) = tab {
            self.open_tab(location);
        }
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        let report_network = self.report_network_space;
        let (show_drives, show_net_drives, show_capacity) = (self.look.drives, self.look.network_drives, self.look.capacity);
        if show_drives && show_capacity {
            let wanted: Vec<Vec<u8>> = self.volumes.iter().filter(|v| (!v.network || report_network) && (!v.network || show_net_drives)).map(|v| v.point.clone()).collect();
            for point in &wanted {
                self.request_space(point);
            }
        }
        let mut target = None;
        let mut tab_target = None;
        let mut action = None;
        let mut toggle_space = false;
        let dragging = self.drag.is_some();
        let mut hover = None;
        let mut zones = Vec::new();
        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let mut entry = |ui: &mut Ui, label: String, path: &[u8], tip: &str, cwd: &[u8]| -> egui::Response {
                let droppable = accepts_files(path);
                let hovered_drop = dragging && droppable && self.drop_target.as_deref() == Some(path);
                let resp = ui.selectable_label(cwd == path || hovered_drop, label).on_hover_text(tip);
                if droppable {
                    zones.push((resp.rect.intersect(ui.clip_rect()), path.to_vec()));
                }
                if dragging && droppable && resp.contains_pointer() {
                    hover = Some(path.to_vec());
                }
                if resp.clicked() {
                    target = Some(path.to_vec());
                } else if resp.middle_clicked() {
                    tab_target = Some(path.to_vec());
                }
                resp
            };
            ui.add_space(4.0);
            if self.look.quick_access {
                ui.weak("Quick access");
                entry(ui, "◷ Recent".into(), RECENT, "Recently opened files", &self.cwd);
                for (label, path) in &self.places {
                    let icon = if label == "Home" { "🏠" } else { "🗀" };
                    entry(ui, format!("{icon} {label}"), path, &lossy(path), &self.cwd);
                }
                ui.add_space(8.0);
            }
            if show_drives {
                ui.weak("Drives");
            }
            for v in self.volumes.iter().filter(|v| show_drives && (!v.network || show_net_drives)) {
                ui.horizontal(|ui| {
                    let response = entry(ui, v.label.clone(), &v.point, &v.tip, &self.cwd);
                    response.context_menu(|ui| {
                        if ui.button("Open in new tab").clicked() {
                            action = Some(Action::OpenInNewTab(v.point.clone()));
                            ui.close();
                        }
                        if ui.button("Copy path").clicked() {
                            ui.ctx().copy_text(lossy(&v.point));
                            ui.close();
                        }
                        if v.ejectable && ui.button("Safely eject").clicked() {
                            action = Some(Action::EjectDrive(v.point.clone()));
                            ui.close();
                        }
                        if v.gvfs && ui.button("Disconnect").clicked() {
                            self.net.pending_disconnect = Some((v.point.clone(), true));
                            ui.close();
                        }
                        if v.network && ui.add(Button::selectable(report_network, "Show network capacity")).clicked() {
                            toggle_space = true;
                            ui.close();
                        }
                    });
                    if v.ejectable && ui.small_button("⏏").on_hover_text(format!("Safely eject {}", lossy(fsys::base_name(&v.point)))).clicked() {
                        action = Some(Action::EjectDrive(v.point.clone()));
                    }
                    if show_capacity
                        && (!v.network || report_network)
                        && let Some((free, total)) = self.space.get(&v.point).and_then(|s| s.capacity)
                        && total > 0
                    {
                        capacity_bar(ui, free, total);
                    }
                });
            }
            // Removable drives plugged in but not mounted: no automounter, or unmounted earlier.
            for d in &self.drive_extras.unmounted {
                let name = d.name();
                let busy = self.drives_busy.contains(&d.device) || self.drives_busy.contains(&d.disk);
                ui.horizontal(|ui| {
                    let tip = format!(
                        "{} · {} · {}\nPlugged in but not mounted. Click to mount and open it.",
                        lossy(&d.device),
                        d.fstype,
                        fsys::format_size(d.size)
                    );
                    let response = ui.add_enabled(!busy, egui::Button::selectable(false, format!("🖴 {name}  (not mounted)"))).on_hover_text(tip);
                    if response.clicked() {
                        action = Some(Action::MountDrive { device: d.device.clone(), open: true });
                    }
                    response.context_menu(|ui| {
                        if ui.button("Mount and open").clicked() {
                            action = Some(Action::MountDrive { device: d.device.clone(), open: true });
                            ui.close();
                        }
                        if ui.button("Mount").clicked() {
                            action = Some(Action::MountDrive { device: d.device.clone(), open: false });
                            ui.close();
                        }
                        if ui.button("Power off").on_hover_text(format!("Power off {} so it can be unplugged", lossy(&d.disk))).clicked() {
                            action = Some(Action::PowerOffDrive(d.disk.clone()));
                            ui.close();
                        }
                    });
                    if busy {
                        ui.spinner();
                    } else if ui.small_button("⏏").on_hover_text(format!("Power off {name} so it can be unplugged")).clicked() {
                        action = Some(Action::PowerOffDrive(d.disk.clone()));
                    }
                });
            }
            // Removable volumes something else was mounted over: their files can't be reached.
            for c in &self.drive_extras.covered {
                let tip = format!(
                    "{} is mounted at {}, but {} was mounted at {} on top of it, so its files can't be reached. Unmount that to reach this drive.",
                    lossy(&c.source),
                    lossy(&c.point),
                    lossy(&c.by),
                    lossy(&c.by_point)
                );
                ui.add_enabled(false, egui::Button::selectable(false, format!("⚠ {}  (hidden)", lossy(fsys::base_name(&c.point)))))
                    .on_disabled_hover_text(tip);
            }
            ui.add_space(8.0);
            if show_drives {
                ui.add_space(8.0);
            }
            if !self.look.network {
                return;
            }
            entry(ui, "🖧 Network".into(), NETWORK, "Connect to shares, browse servers", &self.cwd);
            for s in &self.servers {
                let label = format!("    {}", s.split_once("://").map_or(s.as_str(), |(_, h)| h));
                let resp = ui.selectable_label(false, label).on_hover_text(s);
                if resp.clicked() {
                    self.net.expand = Some(s.clone());
                    target = Some(NETWORK.to_vec());
                }
            }
        });
        if let Some(p) = self.net.pending_disconnect.take() {
            self.disconnect(p.0, p.1);
        }
        if toggle_space {
            self.report_network_space = !self.report_network_space;
        }
        self.zones.extend(zones);
        if hover.is_some() {
            self.drop_target = hover;
        }
        if action.is_some() {
            let ctx = ui.ctx().clone();
            self.run_action(action, &ctx);
        }
        if let Some(t) = target {
            self.navigate(t, true);
        }
        if let Some(t) = tab_target.filter(|t| t != NETWORK) {
            self.open_tab(t);
        }
    }

    /// Tabs: click to switch, drag to reorder or tear off, middle-click to
    /// close. While items are dragged, a tab is a drop target and opens after
    /// a moment's hover.
    fn tab_strip(&mut self, ui: &mut Ui) {
        let (mut switch, mut close, mut close_others) = (None, None, None);
        let (mut drag_start, mut spring, mut drop_on) = (None, None, None);
        let mut rects = Vec::with_capacity(self.tabs.len());
        let mut zones = Vec::new();
        let carried = if self.drag.is_some() { ui.input(|i| i.pointer.hover_pos()) } else { None };
        let stroke = Stroke::new(1.5, ui.visuals().selection.stroke.color);
        ScrollArea::horizontal().show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, tab) in self.tabs.iter().enumerate() {
                    let active = i == self.active_tab;
                    let location = if active { &self.cwd } else { &tab.location };
                    let dragged = self.tab_drag == Some(i);
                    let group = ui.horizontal(|ui| {
                        let resp = ui.selectable_label(active || dragged, tab_label(location)).on_hover_text(lossy(location)).interact(Sense::drag());
                        if resp.clicked() {
                            switch = Some(i);
                        } else if resp.middle_clicked() {
                            close = Some(i);
                        } else if resp.drag_started_by(PointerButton::Primary) {
                            drag_start = Some(i);
                        }
                        resp.context_menu(|ui| {
                            if ui.add(Button::new("Close tab").shortcut_text("Ctrl+W")).clicked() {
                                close = Some(i);
                                ui.close();
                            }
                            if ui.button("Close other tabs").clicked() {
                                close_others = Some(i);
                                ui.close();
                            }
                        });
                        // Only the current tab shows its close button; middle-click closes any.
                        if active && ui.add(Button::new("✕").frame(false).small()).on_hover_text("Close tab (Ctrl+W)").clicked() {
                            close = Some(i);
                        }
                    });
                    let r = group.response.rect;
                    rects.push(r);
                    let droppable = accepts_files(location);
                    if droppable {
                        zones.push((r.intersect(ui.clip_rect()), location.clone()));
                    }
                    if carried.is_some_and(|p| r.contains(p)) {
                        spring = Some(i);
                        if droppable {
                            drop_on = Some(location.clone());
                            ui.painter().rect_stroke(r, 2.0, stroke, egui::StrokeKind::Inside);
                        }
                    }
                    ui.add_space(4.0);
                }
            });
        });
        self.tab_rects = rects;
        self.zones.extend(zones);
        if self.drag.is_some() && drop_on.is_some() {
            self.drop_target = drop_on;
        }
        // Spring-loading: rest a dragged item on a tab to open it and drop inside.
        match spring.filter(|&i| i != self.active_tab) {
            Some(i) => match self.tab_spring {
                Some((j, since)) if j == i && since.elapsed() >= TAB_SPRING => {
                    self.tab_spring = None;
                    switch = Some(i);
                }
                Some((j, since)) if j == i => ui.ctx().request_repaint_after(TAB_SPRING.saturating_sub(since.elapsed())),
                _ => {
                    self.tab_spring = Some((i, Instant::now()));
                    ui.ctx().request_repaint_after(TAB_SPRING);
                }
            },
            None => self.tab_spring = None,
        }
        if let Some(i) = drag_start {
            self.tab_drag = Some(i);
        }
        match (switch, close, close_others) {
            (Some(i), _, _) => self.switch_tab(i),
            (_, Some(i), _) => self.close_tab(i),
            (_, _, Some(i)) => self.close_other_tabs(i),
            _ => {}
        }
    }

    /// Follow a dragged tab: reorder within the strip; released away from
    /// the strip (or outside the window), it opens in a new window there.
    fn update_tab_drag(&mut self, ctx: &egui::Context) {
        let Some(index) = self.tab_drag.filter(|&i| i < self.tabs.len()) else {
            self.tab_drag = None;
            return;
        };
        let (pos, down) = ctx.input(|i| (i.pointer.latest_pos(), i.pointer.primary_down()));
        let Some(p) = pos else { return };
        let strip = self.tab_rects.iter().copied().reduce(|a, b| a.union(b));
        let near_strip = strip.is_some_and(|s| (p.y - s.center().y).abs() < s.height() / 2.0 + 30.0) && ctx.content_rect().contains(p);
        if !down {
            self.tab_drag = None;
            if !near_strip && self.tabs.len() > 1 {
                let origin = ctx.input(|i| i.viewport().inner_rect.map(|r| r.min)).unwrap_or_default();
                let location = self.tab_location(index);
                if self.open_window(&location, Some([origin.x + p.x - 80.0, origin.y + p.y - 12.0])) {
                    self.close_tab(index);
                }
            }
            return;
        }
        if near_strip {
            if let Some(j) = self.tab_rects.iter().position(|r| r.x_range().contains(p.x)) {
                let r = self.tab_rects[j];
                // Past the neighbour's middle, so the swap doesn't flip back.
                if (j > index && p.x > r.center().x) || (j < index && p.x < r.center().x) {
                    self.move_tab(index, j);
                    self.tab_drag = Some(j);
                }
            }
            return;
        }
        let text = format!("🗐 {} — release to open in a new window", tab_label(&self.tab_location(index)));
        egui::Area::new(egui::Id::unique("tab_drag_label")).order(egui::Order::Tooltip).fixed_pos(p + egui::vec2(16.0, 12.0)).interactable(false).show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| ui.add(egui::Label::new(text).extend()));
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if self.cwd == NETWORK {
                let shares = self.volumes.iter().filter(|v| v.network).count();
                ui.label(format!("{shares} connected shares · {} remembered servers", self.servers.len()));
            } else if let Some(r) = &self.results {
                let shown = self.view.len();
                let streaming = if r.all.is_none() { " · receiving…" } else { "" };
                let text = if r.total > shown { format!("{shown} shown of {} matches{streaming}", r.total) } else { format!("{} matches{streaming}", r.total) };
                let ms = |d: Duration| format!("{:.1} ms", d.as_secs_f64() * 1e3);
                let details = match r.details {
                    Some(d) => format!("details after {}", ms(d)),
                    None if !r.rows.is_empty() => format!("details {}%", r.filled_count * 100 / r.rows.len()),
                    None => String::new(),
                };
                ui.label(text).on_hover_text(format!("First rows after {}{}\n{details}", ms(r.first), r.all.map_or(String::new(), |d| format!(", all after {}", ms(d)))));
            } else if self.cwd == RECENT {
                ui.label(format!("{} recent files", self.view.len()));
            } else {
                let resp = if self.loading { ui.weak("Loading…") } else { ui.label(format!("{} items", self.view.len())) };
                let hidden = self.rows.len().saturating_sub(self.view.len());
                let hidden = if hidden > 0 && !self.show_hidden && self.matcher.is_none() { format!("\n{hidden} hidden (Ctrl+H shows them)") } else { String::new() };
                resp.on_hover_text(format!("Listed in {:.1} ms{hidden}", self.listed_in.as_secs_f64() * 1e3));
            }
            let (count, bytes) = {
                let src = self.source();
                self.view.iter().filter_map(|&i| src.get(i)).filter(|r| r.sel).fold((0usize, 0u64), |(n, b), r| (n + 1, b + if r.is_dir() { 0 } else { r.size }))
            };
            if count > 0 {
                ui.weak(format!("· {count} selected, {}", fsys::format_size(bytes)));
            }
            if let Some(e) = self.search_error.as_ref().or(self.error.as_ref()) {
                ui.colored_label(RED, format!("· {e}"));
            } else if let Some(n) = &self.notice {
                ui.weak(format!("· {n}"));
            }
            // Search details stay out of the way: one short word, the rest on hover.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                crate::update::status_widget(ui, &self.updater);
                if let (true, Some(d)) = (self.daemon_covers(&self.cwd), &self.daemon) {
                    let age = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |n| n.as_secs().saturating_sub(d.verified_ns / 1_000_000_000));
                    let live = if d.watched > 0 { "followed live" } else { "swept periodically" };
                    let paused = if d.paused.is_empty() { String::new() } else { format!(" (paused: {})", d.paused) };
                    let word = if self.searching { "Searching…" } else { "Index daemon" };
                    ui.weak(word).on_hover_text(format!("{} entries · {live} · verified {age} s ago{paused}", d.entries));
                } else if let Some(b) = &self.build {
                    ui.weak(format!("Indexing… {} entries", b.seen.load(Relaxed))).on_hover_text(format!("Indexing {}", lossy(&b.root)));
                } else if let Some(i) = self.indexes.iter().find(|i| i.covers(&self.cwd)) {
                    let word = if self.searching { "Searching…" } else { "Indexed" };
                    ui.weak(word).on_hover_text(format!(
                        "Index of {}: {} entries, {} folders, {:.0} MB, built in {:.2} s\nF5 rebuilds it",
                        lossy(&i.root),
                        i.entries,
                        i.dirs,
                        i.bytes() as f64 / 1048576.0,
                        i.build_time.as_secs_f64()
                    ));
                }
            });
        });
    }

    fn jobs_panel(&mut self, ui: &mut Ui) {
        let mut dismiss = Vec::new();
        for (n, job) in self.jobs.iter().enumerate() {
            ui.horizontal(|ui| {
                let verb = if job.kind == Kind::Move { ("Moving", "Moved") } else { ("Copying", "Copied") };
                let dest = lossy(&job.dest);
                let files_total = job.files_total.load(Relaxed);
                let files_done = job.files_done.load(Relaxed);
                let (bytes_total, bytes_done) = (job.bytes_total.load(Relaxed), job.bytes_done.load(Relaxed));
                let errors = job.error_count();
                match job.phase() {
                    Phase::Planning => {
                        ui.label(format!("Preparing: {} {} item(s) to {dest}…", verb.0.to_lowercase(), job.sources.len()));
                    }
                    Phase::AwaitingDecision => {
                        ui.label(format!("{} files already exist in {dest}", job.conflicts.load(Relaxed)));
                    }
                    Phase::Transferring => {
                        let elapsed = job.started.elapsed().as_secs_f64().max(0.001);
                        let rate = bytes_done as f64 / elapsed;
                        let frac = if bytes_total > 0 {
                            bytes_done as f32 / bytes_total as f32
                        } else if files_total > 0 {
                            files_done as f32 / files_total as f32
                        } else {
                            0.0
                        };
                        let eta = if rate > 0.0 && bytes_total > bytes_done {
                            let s = ((bytes_total - bytes_done) as f64 / rate) as u64;
                            format!(" · {}:{:02} left", s / 60, s % 60)
                        } else {
                            String::new()
                        };
                        ui.add(egui::ProgressBar::new(frac).desired_width(160.0).show_percentage());
                        ui.label(format!(
                            "{} {files_done}/{files_total} files · {} of {} · {}/s{eta} · {} threads → {dest}",
                            verb.0,
                            fsys::format_size(bytes_done),
                            fsys::format_size(bytes_total),
                            fsys::format_size(rate as u64),
                            job.threads.load(Relaxed),
                        ));
                    }
                    Phase::Done | Phase::Cancelled => {
                        let took = job.elapsed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).unwrap_or_default().as_secs_f32();
                        let what = if job.phase() == Phase::Cancelled { "Cancelled" } else { verb.1 };
                        let items = if files_done == 1 { "1 item".to_string() } else { format!("{files_done} items") };
                        let bytes = if bytes_done > 0 { format!(" ({})", fsys::format_size(bytes_done)) } else { String::new() };
                        ui.label(format!("{what} {items}{bytes} in {took:.1}s → {dest}"));
                    }
                }
                if errors > 0 && ui.link(RichText::new(format!("{errors} error(s)")).color(RED)).clicked() {
                    self.show_errors = Some(job.clone());
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if job.finished() {
                        if ui.small_button("✕").on_hover_text("Dismiss").clicked() {
                            dismiss.push(n);
                        }
                    } else if ui.small_button("Cancel").clicked() {
                        job.cancel();
                    }
                });
            });
        }
        // Clean, finished jobs fade out on their own after a few seconds.
        for (n, job) in self.jobs.iter().enumerate() {
            let old = job.elapsed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some_and(|e| job.started.elapsed() > e + JOB_LINGER);
            if job.phase() == Phase::Done && job.error_count() == 0 && old && !dismiss.contains(&n) {
                dismiss.push(n);
            }
        }
        dismiss.sort_unstable();
        for n in dismiss.into_iter().rev() {
            self.jobs.remove(n);
        }
    }

    fn table(&mut self, ui: &mut Ui) {
        let results = self.in_results();
        let folders = results || self.cwd == RECENT;
        let width = ui.available_width();
        let look = &self.look;
        let (show_date, show_size, show_type, show_folder) = (look.col_date, look.col_size, look.col_type && !folders, look.col_folder && folders);
        let date_w = if show_date { 140.0 } else { 0.0 };
        let size_w = if show_size { 90.0 } else { 0.0 };
        let type_w = if show_type { 120.0 } else { 0.0 };
        let flex = (width - date_w - size_w - type_w).max(120.0);
        let (name_w, folder_w) = if show_folder { (flex * 0.45, flex * 0.55) } else { (flex, 0.0) };
        let mut cols: Vec<(&str, Option<SortKey>, f32, f32, bool)> = Vec::new();
        let mut x = 0.0;
        let mut add = |label, key, w, right| {
            cols.push((label, key, x, w, right));
            x += w;
        };
        add("Name", Some(SortKey::Name), name_w, false);
        if show_folder {
            add("Folder", None, folder_w, false);
        }
        if show_date {
            add("Date modified", Some(SortKey::Modified), date_w, false);
        }
        if show_type {
            add("Type", Some(SortKey::Type), type_w, false);
        }
        if show_size {
            add("Size", Some(SortKey::Size), size_w, true);
        }

        let font = FontId::proportional(13.5);
        let visuals = ui.visuals().clone();
        let mods = ui.input(|i| i.modifiers);
        let mut action = None;

        let (hrect, _) = ui.allocate_exact_size(egui::vec2(width, ROW_H), Sense::hover());
        for (i, &(label, key, cx, w, right)) in cols.iter().enumerate() {
            let rect = Rect::from_min_size(egui::pos2(hrect.left() + cx, hrect.top()), egui::vec2(w, ROW_H));
            let resp = ui.interact(rect, ui.scope_id().with(("hdr", i)), Sense::click());
            if resp.hovered() && key.is_some() {
                ui.painter().rect_filled(rect, 2.0, visuals.widgets.hovered.weak_bg_fill);
            }
            let arrow = match key {
                Some(k) if k == self.sort && self.cwd != RECENT => if self.descending { " ▼" } else { " ▲" },
                _ => "",
            };
            cell(ui.painter(), rect, &format!("{label}{arrow}"), &font, visuals.strong_text_color(), right);
            if resp.clicked()
                && let Some(k) = key
            {
                action = Some(Action::Sort(k));
            }
        }
        ui.separator();
        let ctx = ui.ctx().clone();

        if self.view.is_empty() {
            let msg = if self.loading || self.searching || self.build.is_some() {
                "Working…"
            } else if self.error.is_some() && !results {
                "Folder could not be read"
            } else if self.cwd == RECENT {
                "No recently opened files"
            } else if self.matcher.is_some() {
                "No matches"
            } else {
                "This folder is empty"
            };
            let rest = ui.available_rect_before_wrap();
            let resp = ui.interact(rest, ui.scope_id().with("empty"), Sense::click());
            ui.painter().text(rest.center_top() + egui::vec2(0.0, 30.0), Align2::CENTER_CENTER, msg, font.clone(), visuals.weak_text_color());
            if accepts_files(&self.cwd) {
                self.zones.push((rest, self.cwd.clone()));
            }
            self.rows_rect = rest;
            self.background_menu(&resp, &mut action);
            self.run_action(action, &ctx);
            return;
        }

        let mut area = ScrollArea::vertical().auto_shrink(false).id_salt("table");
        if let Some(i) = self.scroll_to.take() {
            let top = i as f32 * ROW_H;
            let mut y = self.scroll_y;
            // Before the first frame has measured the list, show the row at the top.
            if top < y || self.view_h <= 0.0 {
                y = top;
            } else if top + ROW_H > y + self.view_h {
                y = top + ROW_H - self.view_h;
            }
            area = area.vertical_scroll_offset(y);
        } else if let Some(y) = self.scroll_set.take() {
            area = area.vertical_scroll_offset(y.max(0.0));
        }

        let check_cut = !self.cut_marks.is_empty();
        let typeahead_hit = self.typeahead.as_ref().filter(|t| t.last.elapsed() < TYPEAHEAD_RESET && !t.positions.is_empty());
        let mut renaming = self.renaming.take();
        let mut rename_done: Option<bool> = None;
        let pointer = ui.input(|i| i.pointer.interact_pos());
        let mut hover_drop: Option<Vec<u8>> = None;
        let mut row_zones = Vec::new();
        let in_archive = archive::is_location(&self.cwd);
        let writable = accepts_files(&self.cwd) || results || self.cwd == RECENT;
        let has_files = clip::has_files();
        let mut visible = 0..0;

        ui.spacing_mut().item_spacing.y = 0.0;
        let out = area.show_rows(ui, ROW_H, self.view.len() + BLANK_ROWS, |ui, range| {
            visible = range.clone();
            for vi in range {
                if vi >= self.view.len() {
                    // Trailing blank row: behaves like the empty space below.
                    let (_, resp) = ui.allocate_exact_size(egui::vec2(width, ROW_H), Sense::click_and_drag());
                    if resp.clicked() {
                        action = Some(Action::ClearSelection);
                    } else if resp.drag_started_by(PointerButton::Primary) {
                        let origin = ui.input(|i| i.pointer.press_origin()).unwrap_or(resp.rect.left_top());
                        action = Some(Action::StartBand { origin, additive: mods.ctrl });
                    }
                    self.background_menu(&resp, &mut action);
                    continue;
                }
                let Some(row) = self.row(vi) else { continue };
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, ROW_H), Sense::click_and_drag());
                let selected = row.sel;
                let full = row.full_path(&self.cwd);
                let is_drop = self.drag.is_some() && row.is_dir() && !selected && resp.contains_pointer() && accepts_files(&full);
                if row.is_dir() && accepts_files(&full) {
                    row_zones.push((rect.intersect(ui.clip_rect()), full.clone()));
                }
                if is_drop {
                    hover_drop = Some(full.clone());
                }
                let painter = ui.painter();
                if selected {
                    painter.rect_filled(rect, 2.0, visuals.selection.bg_fill);
                } else if is_drop || (self.drag.is_none() && self.band.is_none() && resp.hovered()) {
                    painter.rect_filled(rect, 2.0, visuals.widgets.hovered.weak_bg_fill);
                }
                if is_drop {
                    painter.rect_stroke(rect, 2.0, Stroke::new(1.5, visuals.selection.stroke.color), egui::StrokeKind::Inside);
                }
                if self.focus == Some(vi) && !selected {
                    painter.rect_stroke(rect, 2.0, Stroke::new(1.0, visuals.weak_text_color()), egui::StrokeKind::Inside);
                }
                let dim = check_cut && self.cut_marks.contains(&full);
                let mut color = if selected { visuals.selection.stroke.color } else { visuals.text_color() };
                let mut weak = if selected { color } else { visuals.weak_text_color() };
                if dim || row.is_hidden() {
                    color = color.gamma_multiply(0.55);
                    weak = weak.gamma_multiply(0.55);
                }
                let icon = if row.is_link() {
                    "↗"
                } else if row.is_dir() {
                    "🗀"
                } else {
                    "🗋"
                };
                // Sizes and dates of results stay blank until backfilled.
                let known = self.results.as_ref().is_none_or(|r| r.is_filled(row));
                let editing = renaming.as_ref().is_some_and(|r| r.path == full);
                let found = typeahead_hit.filter(|t| t.path == full);
                for &(label, _, cx, w, right) in &cols {
                    let r = Rect::from_min_size(egui::pos2(rect.left() + cx, rect.top()), egui::vec2(w, ROW_H));
                    if editing && label == "Name" {
                        continue;
                    }
                    if let (Some(t), "Name") = (found, label) {
                        highlighted_cell(painter, r, &format!("{icon}  "), &row.display(), &t.positions, &font, color, &visuals);
                        continue;
                    }
                    let (text, c) = match label {
                        "Name" => (format!("{icon}  {}", row.display()), color),
                        "Folder" => (lossy(row.dir.as_deref().unwrap_or(&self.cwd)), weak),
                        "Date modified" if known => (sys::format_time(row.mtime), weak),
                        "Type" => (row.type_name(), weak),
                        "Size" if known && !row.is_dir() => (fsys::format_size(row.size), weak),
                        _ => (String::new(), weak),
                    };
                    cell(painter, r, &text, &font, c, right);
                }
                if let (true, Some(rn)) = (editing, renaming.as_mut()) {
                    let r = Rect::from_min_size(egui::pos2(rect.left() + 24.0, rect.top() + 1.0), egui::vec2(name_w - 28.0, ROW_H - 2.0));
                    let id = egui::Id::unique("rename_box");
                    let edit = ui.put(r, TextEdit::singleline(&mut rn.text).id(id).font(font.clone()));
                    if !rn.focused {
                        edit.request_focus();
                        // Preselect the name without its extension.
                        let stem = match rn.text.rfind('.') {
                            Some(i) if i > 0 && !row.is_dir() => rn.text[..i].chars().count(),
                            _ => rn.text.chars().count(),
                        };
                        let range = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(stem));
                        let mut state = TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
                        state.cursor.set_char_range(Some(range));
                        state.store(ui.ctx(), id);
                        rn.focused = true;
                    } else if ui.input(|i| i.key_pressed(Key::Escape)) {
                        rename_done = Some(false);
                    } else if edit.lost_focus() {
                        rename_done = Some(true);
                    }
                }

                // Interaction
                if resp.double_clicked() {
                    action = Some(Action::Open(vi));
                } else if resp.middle_clicked() && row.is_dir() {
                    action = Some(Action::OpenInNewTab(full.clone()));
                } else if resp.clicked() {
                    action = Some(Action::Click { vi, ctrl: mods.ctrl, shift: mods.shift });
                } else if resp.secondary_clicked() {
                    action = Some(Action::Secondary(vi));
                } else if resp.drag_started_by(PointerButton::Primary) {
                    let origin = ui.input(|i| i.pointer.press_origin()).unwrap_or(rect.left_top());
                    // Dragging a name moves items; dragging elsewhere draws a selection box.
                    action = Some(if origin.x < rect.left() + name_w { Action::StartDrag(vi) } else { Action::StartBand { origin, additive: mods.ctrl } });
                }
                let is_dir = row.is_dir();
                let archive_file = !is_dir && !in_archive && archive::is_archive_file(&full);
                resp.context_menu(|ui| {
                    let mut item = |ui: &mut Ui, label: &str, shortcut: &str, a: Action| {
                        if ui.add(Button::new(label).shortcut_text(shortcut)).clicked() {
                            action = Some(a);
                            ui.close();
                        }
                    };
                    if archive_file {
                        item(ui, "Open archive", "Enter", Action::Open(vi));
                        item(ui, "Extract here", "", Action::ExtractArchive(vi));
                    } else {
                        item(ui, if is_dir { "Open folder" } else { "Open" }, "Enter", Action::Open(vi));
                    }
                    if is_dir {
                        item(ui, "Open in new tab", "Middle-click", Action::OpenInNewTab(full.clone()));
                        item(ui, "Open in new window", "", Action::OpenInNewWindow(full.clone()));
                    }
                    if folders {
                        item(ui, "Open file location", "", Action::OpenLocation(vi));
                    }
                    if in_archive {
                        item(ui, "Extract next to the archive", "", Action::ExtractSelected);
                        item(ui, "Copy archive path", "", Action::CopyPath(vi));
                    } else {
                        item(ui, "Create symbolic link", "", Action::CreateLink(vi));
                        if !is_dir {
                            item(ui, "Run as root", "Ctrl+S", Action::RunAs(vi));
                        }
                        ui.separator();
                        item(ui, "Cut", "Ctrl+X", Action::Clip { cut: true });
                        item(ui, "Copy", "Ctrl+C", Action::Clip { cut: false });
                        if is_dir && has_files {
                            item(ui, "Paste into folder", "", Action::Paste(Some(full.clone())));
                        }
                        item(ui, "Copy path", "", Action::CopyPath(vi));
                        if writable {
                            ui.separator();
                            item(ui, "Rename", "F2", Action::Rename(vi));
                            item(ui, "Move to trash", "Del", Action::Delete { permanent: false });
                        }
                        ui.separator();
                        item(ui, "Properties", "Alt+Enter", Action::Properties);
                    }
                });
            }
        });
        self.scroll_y = out.state.clamped_offset().y;
        // Empty space below the last row (show_rows sizes its content to the
        // rows exactly, so it's handled here): click clears, drag draws a
        // selection box, right-click is the folder's menu.
        let below = out.inner_rect.top() + (self.view.len() + BLANK_ROWS) as f32 * ROW_H - self.scroll_y;
        if below < out.inner_rect.bottom() - 1.0 {
            let rest = Rect::from_min_max(egui::pos2(out.inner_rect.left(), below.max(out.inner_rect.top())), out.inner_rect.max);
            let resp = ui.interact(rest, ui.scope_id().with("below_rows"), Sense::click_and_drag());
            if resp.clicked() {
                action = Some(Action::ClearSelection);
            } else if resp.drag_started_by(PointerButton::Primary) {
                let origin = ui.input(|i| i.pointer.press_origin()).unwrap_or(rest.left_top());
                action = Some(Action::StartBand { origin, additive: mods.ctrl });
            }
            self.background_menu(&resp, &mut action);
        }
        self.view_h = out.inner_rect.height();
        self.rows_rect = out.inner_rect;
        self.zones.extend(row_zones);
        if hover_drop.is_some() {
            self.drop_target = hover_drop;
        }
        self.prioritize_visible(visible);

        match rename_done {
            Some(true) => {
                if let Some(r) = renaming.take() {
                    self.commit_rename(r);
                }
            }
            Some(false) => renaming = None,
            None => {}
        }
        self.renaming = renaming;

        self.update_band(ui, pointer);
        self.typeahead_overlay(ui);
        self.run_action(action, &ctx);
    }

    /// Ask the backfill worker for the rows on screen first.
    fn prioritize_visible(&mut self, visible: std::ops::Range<usize>) {
        let missing: Vec<u32> = visible.filter_map(|vi| self.row(vi)).filter(|row| self.results.as_ref().is_some_and(|r| !r.is_filled(row))).map(|row| row.id).collect();
        if let Some(r) = self.results.as_mut()
            && !missing.is_empty()
            && missing != r.prioritized
            && let Some(b) = &r.backfill
        {
            let _ = b.send(Fill::Prioritize(missing.clone()));
            r.prioritized = missing;
        }
    }

    /// What's been typed, bottom-right of the list (red when nothing matches).
    fn typeahead_overlay(&self, ui: &Ui) {
        let Some(t) = self.typeahead.as_ref().filter(|t| t.last.elapsed() < TYPEAHEAD_RESET) else { return };
        let v = ui.visuals();
        let color = if t.positions.is_empty() { RED } else { v.strong_text_color() };
        let galley = ui.painter().layout_no_wrap(format!("🔍 {}", t.text), FontId::proportional(14.0), color);
        let size = galley.size() + egui::vec2(16.0, 8.0);
        let rect = Rect::from_min_size(self.rows_rect.right_bottom() - size - egui::vec2(12.0, 10.0), size);
        ui.painter().rect(rect, 4.0, v.extreme_bg_color, Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color), egui::StrokeKind::Inside);
        ui.painter().galley(rect.min + egui::vec2(8.0, 4.0), galley, color);
    }

    /// The New menu: a folder, then the templates (loaded on first open).
    fn new_menu(&self, ui: &mut Ui, action: &mut Option<Action>) {
        if ui.add(Button::new("Folder").shortcut_text("Ctrl+Shift+N")).clicked() {
            *action = Some(Action::NewFolder);
            ui.close();
        }
        if ui.add(Button::new("Text file").shortcut_text("Ctrl+Alt+N")).clicked() {
            *action = Some(Action::NewFile);
            ui.close();
        }
        ui.separator();
        match self.templates.as_ref() {
            Some(items) => {
                for (index, item) in items.iter().enumerate() {
                    let label = if item.extension.is_empty() { item.label.clone() } else { format!("{} ({})", item.label, item.extension) };
                    if ui.button(label).clicked() {
                        *action = Some(Action::NewItem(index));
                        ui.close();
                    }
                }
                if items.len() <= 1 {
                    ui.add(egui::Label::new(RichText::new("More: add files to ~/Templates").weak()).extend())
                        .on_hover_text("Any file in your Templates folder becomes a New item, as in other file managers");
                }
            }
            None => {
                if !self.templates_loading {
                    *action = Some(Action::LoadTemplates);
                }
                ui.spinner();
            }
        }
    }

    fn background_menu(&self, resp: &egui::Response, action: &mut Option<Action>) {
        let writable = accepts_files(&self.cwd);
        resp.context_menu(|ui| {
            let undo = self.history.next_undo().filter(|_| !self.history.busy);
            if ui.add_enabled(undo.is_some(), Button::new(undo.map_or("Undo".into(), |e| format!("Undo {}", e.describe()))).shortcut_text("Ctrl+Z")).clicked() {
                *action = Some(Action::Undo);
                ui.close();
            }
            if let Some(redo) = self.history.next_redo().filter(|_| !self.history.busy)
                && ui.add(Button::new(format!("Redo {}", redo.describe())).shortcut_text("Ctrl+Y")).clicked()
            {
                *action = Some(Action::Redo);
                ui.close();
            }
            ui.separator();
            if ui.add_enabled(clip::has_files() && writable, Button::new("Paste").shortcut_text("Ctrl+V")).clicked() {
                *action = Some(Action::Paste(None));
                ui.close();
            }
            ui.add_enabled_ui(writable, |ui| ui.menu_button("New", |ui| self.new_menu(ui, action)));
            if ui.add(Button::new("Refresh").shortcut_text("F5")).clicked() {
                *action = Some(Action::Refresh);
                ui.close();
            }
            ui.separator();
            if ui.add_enabled(writable, Button::new("Properties")).clicked() {
                *action = Some(Action::FolderProperties);
                ui.close();
            }
        });
    }

    /// Rubber-band selection: update the selected range and auto-scroll
    /// when the pointer leaves the list.
    fn update_band(&mut self, ui: &Ui, pointer: Option<Pos2>) {
        let Some(band) = &self.band else { return };
        let (Some(p), true) = (pointer, ui.input(|i| i.pointer.primary_down())) else {
            self.band = None;
            return;
        };
        let rr = self.rows_rect;
        let to_content = |y: f32| y - rr.top() + self.scroll_y;
        let (y0, y1) = {
            let cur = to_content(p.y.clamp(rr.top(), rr.bottom()));
            (band.origin_y.min(cur), band.origin_y.max(cur))
        };
        let first = (y0 / ROW_H).floor().max(0.0) as usize;
        let last = ((y1 / ROW_H).floor() as usize).min(self.view.len().saturating_sub(1));
        let base = band.base.clone();
        let origin_x = band.origin_x;
        let origin_y = band.origin_y;
        for vi in 0..self.view.len() {
            let inside = vi >= first && vi <= last && y1 > y0;
            self.set_sel(vi, base.get(vi).copied().unwrap_or(false) || inside);
        }
        let screen = Rect::from_two_pos(egui::pos2(origin_x, origin_y - self.scroll_y + rr.top()), egui::pos2(p.x, p.y.clamp(rr.top(), rr.bottom()))).intersect(rr);
        let sel = ui.visuals().selection.stroke.color;
        ui.painter().rect(screen, 0.0, sel.gamma_multiply(0.15), Stroke::new(1.0, sel), egui::StrokeKind::Inside);
        self.autoscroll(p);
    }

    fn autoscroll(&mut self, p: Pos2) {
        let rr = self.rows_rect;
        let dy = if p.y < rr.top() + 4.0 {
            -(rr.top() + 4.0 - p.y).min(60.0)
        } else if p.y > rr.bottom() - 4.0 {
            (p.y - rr.bottom() + 4.0).min(60.0)
        } else {
            0.0
        };
        if dy != 0.0 {
            self.scroll_set = Some(self.scroll_y + dy * 0.5);
            self.ctx.request_repaint();
        }
    }

    /// Floating label while dragging, and the drop itself on release.
    fn update_drag(&mut self, ctx: &egui::Context) {
        let Some(drag) = &mut self.drag else { return };
        let (pos, released, mods) = ctx.input(|i| (i.pointer.interact_pos(), i.pointer.primary_released(), i.modifiers));
        let target = self.drop_target.clone();
        let from_archive = drag.paths.first().is_some_and(|p| archive::is_location(p));
        let same = match (&target, &drag.same_volume) {
            _ if from_archive => false,
            (Some(t), Some((cached, same))) if cached == t => *same,
            (Some(t), _) => {
                let same = fileop::same_volume_quick(&drag.paths[0], t);
                drag.same_volume = Some((t.clone(), same));
                same
            }
            _ => false,
        };
        let kind = App::drop_kind(mods, same);
        if released {
            let paths = std::mem::take(&mut drag.paths);
            self.drag = None;
            match target {
                Some(t) if from_archive => self.extract_to(paths, t),
                Some(t) => {
                    self.start_job(kind, paths, t);
                }
                None => {}
            }
            return;
        }
        // Leaving the window hands the drag to X (XDND), so it can end in a
        // file manager, the desktop, another program or another FEx window.
        if let (Some(p), false) = (pos, from_archive)
            && !ctx.content_rect().contains(p)
        {
            let paths = std::mem::take(&mut drag.paths);
            self.drag = None;
            self.drag_out(paths);
            return;
        }
        if let Some(p) = pos {
            let n = drag.paths.len();
            let what = if n == 1 { lossy(fsys::base_name(&drag.paths[0])) } else { format!("{n} items") };
            let text = match &target {
                Some(t) => {
                    let verb = if from_archive { "Extract" } else if kind == Kind::Move { "Move" } else { "Copy" };
                    format!("{verb} {what} to {}", tab_label(t))
                }
                None => what,
            };
            egui::Area::new(egui::Id::unique("drag_label")).order(egui::Order::Tooltip).fixed_pos(p + egui::vec2(16.0, 12.0)).interactable(false).show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| ui.add(egui::Label::new(text).extend()));
            });
            self.autoscroll(p);
        }
    }

    /// Appearance and layout: changes show at once and are saved when the
    /// window closes. Only a switch to or from a see-through window needs a
    /// restart (the window system fixes that when the window is made).
    fn appearance_window(&mut self, ctx: &egui::Context) {
        use crate::linux::appearance::{Preset, Theme, DEFAULT_ACCENT};
        if !self.look_open {
            return;
        }
        let mut look = self.look.clone();
        let mut open = true;
        let mut restart = false;
        let mut font_apply = false;
        egui::Window::new("Appearance and layout")
            .id(egui::Id::unique("appearance"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .pivot(Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Start from:");
                for (p, label, tip) in [
                    (Preset::Full, "Everything", "Sidebar, toolbar, status bar and every column"),
                    (Preset::Compact, "Compact", "No capacity bars, network entries or Type column"),
                    (Preset::SearchOnly, "Search only", "Just the search box and the results"),
                ] {
                    if ui.button(label).on_hover_text(tip).clicked() {
                        look.apply_preset(p);
                    }
                }
            });
            ui.separator();
            egui::Grid::new("look_grid").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                ui.label("Theme");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut look.theme, Theme::Dark, "Dark");
                    ui.radio_value(&mut look.theme, Theme::Light, "Light");
                    ui.radio_value(&mut look.theme, Theme::Glass, "Frosted glass");
                });
                ui.end_row();
                ui.label("Accent");
                ui.horizontal(|ui| {
                    ui.color_edit_button_srgb(&mut look.accent);
                    if look.accent != DEFAULT_ACCENT && ui.small_button("Reset").clicked() {
                        look.accent = DEFAULT_ACCENT;
                    }
                });
                ui.end_row();
                if look.theme == Theme::Glass {
                    ui.label("Glass tint");
                    ui.color_edit_button_srgb(&mut look.glass_tint);
                    ui.end_row();
                    ui.label("Opacity");
                    let mut pct = (look.glass_opacity * 100.0).round();
                    if ui.add(egui::Slider::new(&mut pct, 30.0..=100.0).fixed_decimals(0).suffix("%")).changed() {
                        look.glass_opacity = pct / 100.0;
                    }
                    ui.end_row();
                    ui.label("");
                    ui.checkbox(&mut look.glass_blur, "Blur what's behind").on_hover_text("Where the compositor offers it (KDE's KWin, picom with blur); elsewhere the glass stays clear");
                    ui.end_row();
                }
                ui.label("Font");
                ui.horizontal(|ui| {
                    let r = ui.add(TextEdit::singleline(&mut self.look_font).hint_text("desktop default").desired_width(170.0));
                    if (r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))) || ui.button("Apply").clicked() {
                        font_apply = true;
                    }
                    if !look.font.is_empty() && ui.small_button("Default").clicked() {
                        self.look_font.clear();
                        font_apply = true;
                    }
                });
                ui.end_row();
                ui.label("Size");
                let mut pct = (look.scale * 100.0).round();
                // Applied on release: resizing the whole interface mid-drag would move the slider.
                let r = ui.add(egui::Slider::new(&mut pct, 80.0..=160.0).step_by(5.0).fixed_decimals(0).suffix("%"));
                if r.drag_stopped() || (r.changed() && !r.dragged()) {
                    look.scale = pct / 100.0;
                }
                ui.end_row();
            });
            if look.wants_transparency() != self.transparent {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.weak(if look.wants_transparency() { "The window becomes see-through after a restart." } else { "The window becomes opaque again after a restart." });
                    if ui.button("Restart FEx now").clicked() {
                        restart = true;
                    }
                });
            }
            ui.separator();
            ui.columns(3, |c| {
                c[0].strong("Toolbar");
                c[0].checkbox(&mut look.nav_buttons, "Back, forward, up");
                c[0].checkbox(&mut look.new_button, "New button");
                c[0].checkbox(&mut look.path_bar, "Path bar");
                c[0].checkbox(&mut look.search_bar, "Search box");
                c[0].checkbox(&mut look.status_bar, "Status bar");
                c[1].strong("Sidebar");
                c[1].checkbox(&mut look.sidebar, "Show the sidebar");
                c[1].add_enabled_ui(look.sidebar, |ui| {
                    ui.checkbox(&mut look.quick_access, "Quick access");
                    ui.checkbox(&mut look.drives, "Drives");
                    ui.add_enabled_ui(look.drives, |ui| {
                        ui.checkbox(&mut look.network_drives, "Network drives");
                        ui.checkbox(&mut look.capacity, "Capacity bars");
                    });
                    ui.checkbox(&mut look.network, "Network");
                });
                c[2].strong("Columns");
                c[2].checkbox(&mut look.col_date, "Date modified");
                c[2].checkbox(&mut look.col_type, "Type");
                c[2].checkbox(&mut look.col_size, "Size");
                c[2].checkbox(&mut look.col_folder, "Folder (search results)");
            });
            ui.add_space(4.0);
            ui.weak("The ☰ menu stays, so this window is always within reach.");
        });
        if font_apply {
            look.font = self.look_font.trim().to_string();
        }
        self.set_look(look);
        if restart {
            self.save_look();
            self.session().save();
            let at = ctx.input(|i| i.viewport().outer_rect.map(|r| [r.min.x, r.min.y]));
            if self.open_window(&self.cwd.clone(), at) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        if !open {
            self.look_open = false;
            self.save_look();
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if self.help_open {
            let mut open = true;
            egui::Window::new("Keyboard shortcuts").id(egui::Id::unique("help_window")).open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
                egui::Grid::new("shortcuts").num_columns(2).spacing([24.0, 4.0]).show(ui, |ui| {
                    for (keys, what) in SHORTCUTS {
                        if keys.is_empty() {
                            // A blank row (add_space isn't allowed in a grid).
                            ui.label("");
                            ui.end_row();
                            continue;
                        }
                        ui.strong(*keys);
                        ui.label(*what);
                        ui.end_row();
                    }
                });
            });
            self.help_open = open && !ctx.input(|i| i.key_pressed(Key::Escape));
        }

        // Conflict decision for the first job that needs one.
        if let Some(job) = self.jobs.iter().find(|j| j.phase() == Phase::AwaitingDecision).cloned() {
            egui::Modal::new(egui::Id::unique("conflict")).show(ctx, |ui| {
                ui.set_width(440.0);
                ui.heading("Files already exist");
                ui.label(format!(
                    "{} of the files being {} already exist in\n{}",
                    job.conflicts.load(Relaxed),
                    if job.kind == Kind::Move { "moved" } else { "copied" },
                    lossy(&job.dest)
                ));
                ui.add_space(8.0);
                let choices = [
                    (ops::Conflict::Replace, "Replace all", "Overwrite every existing file"),
                    (ops::Conflict::Update, "Update", "Only transfer files whose size or date differ (rsync-style quick check)"),
                    (ops::Conflict::Skip, "Skip all", "Leave existing files untouched"),
                    (ops::Conflict::KeepBoth, "Keep both", "Add a number to the incoming file's name"),
                ];
                for (c, label, tip) in choices {
                    if ui.add_sized([200.0, 24.0], Button::new(label)).on_hover_text(tip).clicked() {
                        job.resolve(c);
                    }
                }
                ui.add_space(4.0);
                if ui.button("Cancel").clicked() {
                    job.cancel();
                }
            });
        }

        if let Some((paths, permanent)) = self.confirm_delete.clone() {
            let mut close = false;
            egui::Modal::new(egui::Id::unique("confirm_delete")).show(ctx, |ui| {
                ui.set_width(380.0);
                let what = if paths.len() == 1 { format!("'{}'", lossy(fsys::base_name(&paths[0]))) } else { format!("these {} items", paths.len()) };
                if permanent {
                    ui.heading("Delete permanently?");
                    ui.label(format!("Permanently delete {what}? This can't be undone."));
                } else {
                    ui.heading("Move to trash?");
                    ui.label(format!("Move {what} to the trash?"));
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let go = ui.button(RichText::new(if permanent { "Delete" } else { "Move to trash" }).color(RED)).clicked() || ui.input(|i| i.key_pressed(Key::Enter));
                    if go {
                        let p = paths.clone();
                        self.spawn_io(move || {
                            if permanent {
                                let errors: Vec<String> = p.iter().filter_map(|x| fileop::delete_permanently(x).err()).collect();
                                (!errors.is_empty()).then(|| Msg::Error(errors.join("; ")))
                            } else {
                                let (done, errors) = trash::trash(&p);
                                Some(Msg::Trashed { done, errors })
                            }
                        });
                        close = true;
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        close = true;
                    }
                });
            });
            if close {
                self.confirm_delete = None;
            }
        }

        if let Some(job) = self.show_errors.clone() {
            let mut open = true;
            egui::Window::new("Transfer errors").open(&mut open).default_width(560.0).show(ctx, |ui| {
                let errors = job.errors.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                // Only the visible lines are laid out, however many there are.
                let row_h = ui.text_style_height(&egui::TextStyle::Body) + ui.spacing().item_spacing.y;
                ScrollArea::vertical().max_height(320.0).show_rows(ui, row_h, errors.len(), |ui, range| {
                    for e in &errors[range] {
                        ui.label(e);
                    }
                });
            });
            if !open {
                self.show_errors = None;
            }
        }

        if let Some(p) = &mut self.props {
            if !p.show(ctx) {
                self.props = None;
            } else if p.busy() {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
    }
}

/// Glyphs the interface draws (icons, arrows, menu symbols). The UI font
/// rarely has them all; fontconfig names a font for each that does.
const ICON_GLYPHS: &str = "▲▼⏏☰✕↗⬅➡⬆⟳⚠◷🗀🗋🖴🖧🏠🔍🗐";

/// Where font lookups are remembered between runs.
fn font_cache_path() -> std::path::PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| std::path::PathBuf::from(sys::os(&sys::home())).join(".cache"))
        .join("fex/fonts")
}

/// Changes whenever fonts or fontconfig's setup may have: the modification
/// times of the font and fontconfig folders (fc-cache touches its cache
/// folders when fonts are installed), and what was asked for.
fn font_fingerprint(family: &str) -> String {
    let home = std::path::PathBuf::from(sys::os(&sys::home()));
    let mut dirs: Vec<std::path::PathBuf> = ["/etc/fonts", "/etc/fonts/conf.d", "/usr/share/fonts", "/usr/local/share/fonts", "/var/cache/fontconfig"].iter().map(Into::into).collect();
    for d in [".config/fontconfig", ".local/share/fonts", ".fonts", ".cache/fontconfig"] {
        dirs.push(home.join(d));
    }
    let mut out = format!("v1 {} {family}", env!("CARGO_PKG_VERSION"));
    for d in dirs {
        let t = std::fs::metadata(&d).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |t| t.as_nanos());
        out.push_str(&format!(" {t}"));
    }
    out
}

/// The UI font and the symbol fonts for `family`, through fontconfig (one
/// `fc-match` per symbol, so remembered in `font_cache_path`).
fn find_fonts(family: &str) -> (Option<String>, Vec<String>) {
    let cache = font_cache_path();
    let fingerprint = font_fingerprint(family);
    if let Ok(text) = std::fs::read_to_string(&cache) {
        let mut lines = text.lines();
        if lines.next() == Some(fingerprint.as_str()) {
            let (mut ui_font, mut symbols, mut ok) = (None, Vec::new(), true);
            for line in lines {
                match line.split_once('\t') {
                    Some(("ui", f)) => ui_font = Some(f.to_string()),
                    Some(("sym", f)) => symbols.push(f.to_string()),
                    _ => {}
                }
            }
            for f in ui_font.iter().chain(&symbols) {
                ok &= std::path::Path::new(f).exists();
            }
            if ok {
                return (ui_font, symbols);
            }
        }
    }
    let fc = |pattern: &str| -> Option<String> {
        let out = std::process::Command::new("fc-match").args(["-f", "%{file}", pattern]).output().ok()?;
        let f = String::from_utf8_lossy(&out.stdout).into_owned();
        // egui renders TrueType/OpenType outlines; color bitmap emoji fonts can't be drawn.
        let usable = (f.ends_with(".ttf") || f.ends_with(".otf")) && !f.to_ascii_lowercase().contains("color");
        usable.then_some(f)
    };
    let ui_font = fc(family);
    let mut symbols: Vec<String> = Vec::new();
    for c in ICON_GLYPHS.chars() {
        if let Some(f) = fc(&format!("sans-serif:charset={:x}", c as u32))
            && Some(&f) != ui_font.as_ref()
            && !symbols.contains(&f)
        {
            symbols.push(f);
        }
    }
    let mut text = format!("{fingerprint}\n");
    if let Some(f) = &ui_font {
        text.push_str(&format!("ui\t{f}\n"));
    }
    for f in &symbols {
        text.push_str(&format!("sym\t{f}\n"));
    }
    if let Some(dir) = cache.parent()
        && std::fs::create_dir_all(dir).is_ok()
    {
        let tmp = cache.with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &cache);
        }
    }
    (ui_font, symbols)
}

/// The desktop's UI font (or the one chosen in Appearance), and fallback
/// fonts for the interface's symbols (as Windows uses Segoe UI Symbol),
/// found through fontconfig and loaded off the UI thread. egui's own fonts
/// stay last for anything else.
pub(super) fn load_fonts(ctx: &egui::Context, tx: Sender<Msg>, family: String) {
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let family = if family.trim().is_empty() { "sans-serif".to_string() } else { family };
        let (ui_font, symbol_fonts) = find_fonts(&family);
        let mut fonts = egui::FontDefinitions::default();
        let mut names = Vec::new();
        for (i, file) in ui_font.iter().chain(symbol_fonts.iter()).enumerate() {
            if let Ok(bytes) = std::fs::read(file) {
                let name = format!("system-{i}");
                fonts.font_data.insert(name.clone(), Arc::new(egui::FontData::from_owned(bytes)));
                names.push((name, i == 0 && ui_font.is_some()));
            }
        }
        let prop = fonts.families.entry(egui::FontFamily::Proportional).or_default();
        for (k, (name, _)) in names.iter().enumerate() {
            prop.insert(k, name.clone());
        }
        // Monospace text (the mode in Properties) gets the symbols as fallbacks only.
        let mono = fonts.families.entry(egui::FontFamily::Monospace).or_default();
        mono.extend(names.iter().filter(|(_, is_ui)| !is_ui).map(|(n, _)| n.clone()));
        ctx.set_fonts(fonts);
        let shown = ui_font.map(|f| f.rsplit('/').next().unwrap_or(&f).to_string());
        let _ = tx.send(Msg::Fonts(shown));
        ctx.request_repaint();
    });
}

/// Register FEx with the desktop (menus, the panel and Alt+Tab find its
/// name and logo through a desktop entry matching its window class), as
/// Windows does by embedding the icon in fex.exe. Written to the user's own
/// `~/.local/share` only when no FEx entry is installed yet.
pub(super) fn install_desktop_entry() {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| std::path::PathBuf::from(sys::os(&sys::home())).join(".local/share"));
    let installed = [data.clone(), "/usr/local/share".into(), "/usr/share".into()].iter().any(|d| d.join("applications/fex.desktop").exists());
    let Ok(exe) = std::env::current_exe() else { return };
    let icon_dir = data.join("icons/hicolor/256x256/apps");
    let icon = icon_dir.join("fex.png");
    if !icon.exists() && std::fs::create_dir_all(&icon_dir).is_ok() {
        let _ = std::fs::write(&icon, include_bytes!("../../assets/fex_icon.png"));
    }
    if installed {
        return;
    }
    let apps = data.join("applications");
    let exe = exe.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=FEx\nGenericName=File Explorer\nComment=Browse, search and manage files\n\
         Exec=\"{exe}\" %f\nIcon=fex\nTerminal=false\nStartupWMClass=fex\nStartupNotify=true\n\
         Categories=System;FileTools;FileManager;Utility;\nMimeType=inode/directory;\nKeywords=files;folders;explorer;search;\n"
    );
    if std::fs::create_dir_all(&apps).is_ok() {
        let _ = std::fs::write(apps.join("fex.desktop"), entry);
    }
}

/// Path-bar segments for a location: (label, location to open). A GVFS
/// share starts at the share itself (as `\\server\share` does on Windows),
/// not at `/run/user/...`.
pub(super) fn breadcrumbs(location: &[u8], volumes: &[Volume]) -> Vec<(String, Vec<u8>)> {
    let single = || vec![(lossy(location), location.to_vec())];
    if location == RECENT || location == NETWORK {
        return single();
    }
    if let Some((archive_path, inner)) = archive::parse_location(location) {
        let mut out = breadcrumbs(archive_path, volumes);
        if let Some(last) = out.last_mut() {
            last.1 = archive::root_location(archive_path);
        }
        let mut acc: Vec<u8> = Vec::new();
        for part in inner.split(|&b| b == b'/').filter(|p| !p.is_empty()) {
            if !acc.is_empty() {
                acc.push(b'/');
            }
            acc.extend_from_slice(part);
            out.push((lossy(part), archive::location(archive_path, &acc)));
        }
        return out;
    }
    let share = volumes.iter().filter(|v| v.gvfs && fsys::in_scope(location, &v.point)).max_by_key(|v| v.point.len());
    let (mut out, mut acc, rest) = match share {
        Some(v) => (vec![(v.label.trim_start_matches(['🖧', ' ']).to_string(), v.point.clone())], v.point.clone(), &location[v.point.len()..]),
        None => (vec![("/".to_string(), b"/".to_vec())], b"/".to_vec(), location),
    };
    for part in rest.split(|&b| b == b'/').filter(|p| !p.is_empty()) {
        acc = fsys::join(&acc, part);
        out.push((lossy(part), acc.clone()));
    }
    out
}

/// Short tab title: the folder's own name, or the archive member.
pub(super) fn tab_label(location: &[u8]) -> String {
    if let Some((archive, inner)) = archive::parse_location(location) {
        return lossy(fsys::base_name(if inner.is_empty() { archive } else { inner }));
    }
    if !is_real(location) {
        return lossy(location);
    }
    let name = fsys::base_name(location);
    if name.starts_with(b"smb-share:") || name.contains(&b'=') { lossy(location).rsplit('/').next().unwrap_or("").to_string() } else { lossy(name) }
}

/// Used-space line beside a volume, as on Windows: 36 px wide, filled in
/// proportion to the space used, with free and total on hover.
fn capacity_bar(ui: &mut Ui, free: u64, total: u64) {
    let used_fraction = (total.saturating_sub(free) as f64 / total as f64) as f32;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(36.0, ROW_H), Sense::hover());
    let line = Rect::from_center_size(rect.center(), egui::vec2(rect.width(), 2.0));
    let visuals = ui.visuals();
    ui.painter().rect_filled(line, 0.0, visuals.widgets.inactive.bg_fill);
    let fill = Rect::from_min_max(line.min, egui::pos2(line.left() + line.width() * used_fraction, line.bottom()));
    ui.painter().rect_filled(fill, 0.0, visuals.selection.bg_fill);
    response.on_hover_text(format!("{} free of {}", fsys::format_size(free), fsys::format_size(total)));
}

/// Paint one table cell, clipped to its column.
fn cell(painter: &egui::Painter, rect: Rect, text: &str, font: &FontId, color: Color32, right: bool) {
    let clip = rect.shrink2(egui::vec2(4.0, 0.0));
    let p = painter.with_clip_rect(clip);
    if right {
        p.text(egui::pos2(clip.right(), rect.center().y), Align2::RIGHT_CENTER, text, font.clone(), color);
    } else {
        p.text(egui::pos2(clip.left(), rect.center().y), Align2::LEFT_CENTER, text, font.clone(), color);
    }
}

/// Name cell with type-to-find matches emphasized.
#[allow(clippy::too_many_arguments)]
fn highlighted_cell(painter: &egui::Painter, rect: Rect, prefix: &str, name: &str, positions: &[usize], font: &FontId, color: Color32, v: &egui::Visuals) {
    use egui::text::{LayoutJob, TextFormat};
    let plain = TextFormat { font_id: font.clone(), color, ..Default::default() };
    let hit = TextFormat { font_id: font.clone(), color: v.strong_text_color(), background: v.warn_fg_color.gamma_multiply(0.35), ..Default::default() };
    let mut job = LayoutJob::default();
    job.append(prefix, 0.0, plain.clone());
    let mut run = String::new();
    let mut run_hit = false;
    for (i, c) in name.chars().enumerate() {
        let h = positions.contains(&i);
        if h != run_hit && !run.is_empty() {
            job.append(&run, 0.0, if run_hit { hit.clone() } else { plain.clone() });
            run.clear();
        }
        run_hit = h;
        run.push(c);
    }
    job.append(&run, 0.0, if run_hit { hit } else { plain });
    let clip = rect.shrink2(egui::vec2(4.0, 0.0));
    let galley = painter.layout_job(job);
    let pos = egui::pos2(clip.left(), rect.center().y - galley.size().y / 2.0);
    painter.with_clip_rect(clip).galley(pos, galley, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breadcrumbs_cover_every_kind_of_location() {
        let crumbs = |l: &[u8]| breadcrumbs(l, &[]).into_iter().map(|(label, _)| label).collect::<Vec<_>>();
        assert_eq!(crumbs(b"/home/me"), ["/", "home", "me"]);
        assert_eq!(crumbs(b"/"), ["/"]);
        assert_eq!(crumbs(RECENT), ["Recent"]);
        assert_eq!(crumbs(b"zip:/d/a.zip|x/y"), ["/", "d", "a.zip", "x", "y"]);
        assert_eq!(breadcrumbs(b"zip:/d/a.zip|x", &[])[2].1, b"zip:/d/a.zip|");
        let share = Volume {
            label: "🖧 media on nas".into(),
            point: b"/run/user/1/gvfs/smb-share:server=nas,share=media".to_vec(),
            source: Vec::new(),
            tip: String::new(),
            network: true,
            gvfs: true,
            ejectable: false,
        };
        let got: Vec<String> = breadcrumbs(b"/run/user/1/gvfs/smb-share:server=nas,share=media/films", &[share]).into_iter().map(|c| c.0).collect();
        assert_eq!(got, ["media on nas", "films"]);
    }
}
