//! The Network page: map and disconnect drives, connect to servers with
//! credentials, and browse the shares of known servers.

use std::collections::HashMap;

use eframe::egui::{self, Button, Color32, RichText, ScrollArea, TextEdit, Ui};

use super::{FexApp, Msg, RED};
use crate::net::{self, Connection};
use crate::win;

#[derive(Default)]
pub struct NetState {
    pub connections: Vec<Connection>,
    /// Share lists by lowercased server; `None` while loading.
    pub shares: HashMap<String, Option<Result<Vec<String>, String>>>,
    pub connect_text: String,
    pub busy: bool,
    pub map: Option<MapDialog>,
}

pub struct MapDialog {
    remote: String,
    letters: Vec<char>,
    letter: char,
    persistent: bool,
    prompt: bool,
    error: Option<String>,
}

impl FexApp {
    /// Re-read mapped drives (local and cheap, but WNet can stall on a dead
    /// remembered connection, so it runs off the UI thread).
    pub(super) fn refresh_connections(&self) {
        self.spawn_io(|| Some(Msg::Connections(net::connections())));
    }

    pub(super) fn open_network_page(&mut self) {
        self.refresh_connections();
    }

    fn fetch_shares(&mut self, server: &str) {
        let key = server.to_lowercase();
        if self.net.shares.contains_key(&key) {
            return;
        }
        self.net.shares.insert(key, None);
        let (server, failed) = (server.to_string(), server.to_string());
        self.spawn_io_or(
            move || {
                let result = win::net_shares(&server).map_err(win::error_text);
                Some(Msg::Shares { server, result })
            },
            move |e| Msg::Shares { server: failed, result: Err(e) },
        );
    }

    /// Connect (without a drive letter) through Windows' credential prompt,
    /// then optionally open `open`.
    pub(super) fn connect_share(&mut self, remote: String, prompt: bool, open: Option<String>) {
        let hwnd = win::active_window();
        self.net.busy = true;
        self.spawn_io_or(
            move || Some(Msg::NetDone { result: net::connect(hwnd, &remote, None, false, prompt), open }),
            |e| Msg::NetDone { result: Err(e), open: None },
        );
    }

    fn disconnect(&mut self, name: String) {
        self.net.busy = true;
        self.spawn_io_or(
            move || Some(Msg::NetDone { result: net::disconnect(&name, false), open: None }),
            |e| Msg::NetDone { result: Err(e), open: None },
        );
    }

    fn open_map_dialog(&mut self, remote: String) {
        let letters = net::free_letters();
        let letter = letters.last().copied().unwrap_or('Z'); // Explorer also offers the highest free letter
        self.net.map = Some(MapDialog { remote, letters, letter, persistent: true, prompt: false, error: None });
    }

    pub(super) fn map_dialog(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.net.map else { return };
        let mut close = false;
        let mut submit = None;
        egui::Modal::new(egui::Id::unique("map_drive")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.heading("Map network drive");
            ui.add_space(6.0);
            egui::Grid::new("map_grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Folder");
                ui.add(TextEdit::singleline(&mut d.remote).hint_text(r"\\server\share").desired_width(300.0));
                ui.end_row();
                ui.label("Drive");
                egui::ComboBox::from_id_salt("map_letter").selected_text(format!("{}:", d.letter)).show_ui(ui, |ui| {
                    for &l in d.letters.iter().rev() {
                        ui.selectable_value(&mut d.letter, l, format!("{l}:"));
                    }
                });
                ui.end_row();
            });
            ui.checkbox(&mut d.persistent, "Reconnect at sign-in");
            ui.checkbox(&mut d.prompt, "Connect using different credentials");
            if let Some(e) = &d.error {
                ui.colored_label(RED, e);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Map").clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let remote = d.remote.trim().trim_end_matches('\\').to_string();
                    match net::auth_target(&remote) {
                        Some(t) if !t.ends_with("IPC$") => submit = Some((remote, d.letter, d.persistent, d.prompt)),
                        _ => d.error = Some(r"Enter a shared folder like \\server\share".into()),
                    }
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
            });
        });
        if let Some((remote, letter, persistent, prompt)) = submit {
            let hwnd = win::active_window();
            self.net.busy = true;
            self.spawn_io_or(
                move || {
                    let result = net::connect(hwnd, &remote, Some(letter), persistent, prompt);
                    Some(Msg::NetDone { result, open: Some(format!("{letter}:\\")) })
                },
                |e| Msg::NetDone { result: Err(e), open: None },
            );
            close = true;
        }
        if close {
            self.net.map = None;
        }
    }

    pub(super) fn network_page(&mut self, ui: &mut Ui) {
        let mut go: Option<String> = None;
        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("Network");
                if self.net.busy {
                    ui.weak("working…");
                }
                if let Some(e) = &self.error {
                    ui.colored_label(RED, e.as_str());
                }
            });
            ui.add_space(6.0);

            // Connect bar
            ui.horizontal(|ui| {
                let resp = ui.add(TextEdit::singleline(&mut self.net.connect_text).hint_text(r"\\server  or  \\server\share").desired_width(320.0));
                let text = self.net.connect_text.trim().trim_end_matches('\\').to_string();
                let valid = text.starts_with(r"\\") && text.len() > 2;
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.add_enabled(valid, Button::new("Open")).clicked() || (enter && valid) {
                    go = Some(text.clone());
                }
                if ui.add_enabled(valid, Button::new("Map drive…")).clicked() {
                    self.open_map_dialog(text.clone());
                }
                if ui.add_enabled(valid, Button::new("Connect as…")).on_hover_text("Connect with other credentials").clicked() {
                    if let Some(t) = net::auth_target(&text) {
                        self.connect_share(t, true, Some(text.clone()));
                    }
                }
            });
            ui.add_space(8.0);

            self.mapped_section(ui, &mut go);
            self.servers_section(ui, &mut go);
        });
        if let Some(path) = go {
            self.navigate(path, true);
        }
    }

    fn mapped_section(&mut self, ui: &mut Ui, go: &mut Option<String>) {
        let conns = self.net.connections.clone();
        egui::CollapsingHeader::new(RichText::new(format!("Mapped drives and connections ({})", conns.len())).strong())
            .default_open(true)
            .show(ui, |ui| {
                if conns.is_empty() {
                    ui.weak("No network drives are mapped.");
                }
                egui::Grid::new("conn_grid").num_columns(4).spacing([16.0, 6.0]).striped(true).show(ui, |ui| {
                    for c in &conns {
                        ui.label(c.local.as_deref().map_or("—".to_string(), |l| format!("🖧 {l}")));
                        ui.label(&c.remote);
                        let status = match (c.connected, c.persistent) {
                            (true, true) => RichText::new("Connected · reconnects at sign-in"),
                            (true, false) => RichText::new("Connected"),
                            (false, _) => RichText::new("Disconnected · reconnects at sign-in").color(Color32::from_rgb(200, 160, 60)),
                        };
                        ui.label(status.small());
                        ui.horizontal(|ui| {
                            if ui.small_button("Open").clicked() {
                                *go = Some(c.local.as_ref().map_or(c.remote.clone(), |l| format!("{l}\\")));
                            }
                            if ui.small_button("Disconnect").clicked() {
                                self.disconnect(c.local.clone().unwrap_or_else(|| c.remote.clone()));
                            }
                        });
                        ui.end_row();
                    }
                });
                if ui.button("Map network drive…").clicked() {
                    self.open_map_dialog(String::new());
                }
            });
        ui.add_space(6.0);
    }

    /// Shares of `server`, fetched on first expand.
    fn share_list(&mut self, ui: &mut Ui, server: &str, go: &mut Option<String>) {
        self.fetch_shares(server);
        match self.net.shares.get(&server.to_lowercase()).cloned().flatten() {
            None => {
                ui.weak("loading shares…");
            }
            Some(Err(e)) => {
                ui.horizontal(|ui| {
                    ui.colored_label(RED, e);
                    if ui.small_button("Connect with credentials…").clicked() {
                        self.net.shares.remove(&server.to_lowercase());
                        self.connect_share(format!(r"{server}\IPC$"), true, None);
                    }
                });
            }
            Some(Ok(shares)) if shares.is_empty() => {
                ui.weak("No shared folders.");
            }
            Some(Ok(shares)) => {
                for share in shares {
                    let path = format!(r"{server}\{share}");
                    ui.horizontal(|ui| {
                        if ui.link(format!("🗀 {share}")).clicked() {
                            *go = Some(path.clone());
                        }
                        if ui.small_button("Map…").clicked() {
                            self.open_map_dialog(path.clone());
                        }
                    });
                }
            }
        }
    }

    fn servers_section(&mut self, ui: &mut Ui, go: &mut Option<String>) {
        let mut servers = self.servers.clone();
        // Servers behind mapped drives are worth showing too.
        for b in &net::servers_of(&self.net.connections) {
            if !servers.iter().any(|s| s.eq_ignore_ascii_case(b)) {
                servers.push(b.clone());
            }
        }
        egui::CollapsingHeader::new(RichText::new(format!("Servers ({})", servers.len())).strong()).default_open(true).show(ui, |ui| {
            if servers.is_empty() {
                ui.weak(r"Servers you open (\\name) and servers behind your mapped drives appear here.");
            }
            for server in servers {
                let remembered = self.servers.iter().any(|s| s.eq_ignore_ascii_case(&server));
                let resp = egui::CollapsingHeader::new(format!("🖧 {}", server.trim_start_matches('\\')))
                    .id_salt(("srv", server.to_lowercase()))
                    .show(ui, |ui| self.share_list(ui, &server, go));
                resp.header_response.context_menu(|ui| {
                    if ui.button("Open").clicked() {
                        *go = Some(server.clone());
                        ui.close();
                    }
                    if ui.button("Refresh shares").clicked() {
                        self.net.shares.remove(&server.to_lowercase());
                        ui.close();
                    }
                    if remembered && ui.button("Forget").clicked() {
                        self.forget_server(&server);
                        ui.close();
                    }
                    if !remembered && ui.button("Remember").clicked() {
                        self.remember_server(server.clone());
                        ui.close();
                    }
                });
            }
        });
        ui.add_space(6.0);
    }

}
