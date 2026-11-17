//! The Network page (the Linux side of `netview.rs`): connect to shares
//! (with credentials when they ask), disconnect them, and browse the shares
//! of known servers. Connections go through GVFS (see net.rs), so they are
//! the desktop's own: they show up in every file manager too.

use std::collections::HashMap;

use eframe::egui::{self, Button, RichText, ScrollArea, TextEdit, Ui};

use super::{lossy, net, App, Msg, NETWORK, RED};

#[derive(Default)]
pub struct NetState {
    /// Share lists by server URL; `None` while loading.
    pub shares: HashMap<String, Option<Result<Vec<String>, String>>>,
    pub connect_text: String,
    pub busy: bool,
    /// Expand this server's shares when the page opens (from the sidebar).
    pub expand: Option<String>,
    /// A disconnect asked for from the sidebar: (local path, through GVFS).
    pub pending_disconnect: Option<(Vec<u8>, bool)>,
    pub login: Option<Login>,
}

impl NetState {
    pub fn dialog_open(&self) -> bool {
        self.login.is_some()
    }
}

/// The credentials dialog.
pub struct Login {
    url: String,
    creds: net::Credentials,
    message: Option<String>,
    focused: bool,
}

pub enum NetResult {
    /// Connected; open its local path.
    Connected { open: Option<Vec<u8>> },
    NeedsCredentials { url: String, message: String },
    Disconnected,
    Failed(String),
}

impl App {
    pub(super) fn open_network_page(&mut self) {
        // Server lists are fetched again when expanded after a refresh.
        self.net.shares.retain(|_, v| v.as_ref().is_some_and(|r| r.is_ok()));
    }

    fn remember_server(&mut self, server: String) {
        if !self.servers.iter().any(|s| s.eq_ignore_ascii_case(&server)) {
            self.servers.push(server);
            self.servers.sort_by_key(|s| s.to_lowercase());
            net::save_servers(&self.servers);
        }
    }

    fn forget_server(&mut self, server: &str) {
        self.servers.retain(|s| !s.eq_ignore_ascii_case(server));
        net::save_servers(&self.servers);
    }

    fn fetch_shares(&mut self, server: &str) {
        if self.net.shares.contains_key(server) {
            return;
        }
        self.net.shares.insert(server.to_string(), None);
        let (server, failed) = (server.to_string(), server.to_string());
        self.spawn_io_or(
            move || {
                let result = net::shares(&server);
                Some(Msg::Shares { server, result })
            },
            move |e| Msg::Shares { server: failed, result: Err(e) },
        );
    }

    /// Connect a share (or open a server's list of shares) and open it.
    pub(super) fn connect_share(&mut self, url: String, creds: Option<net::Credentials>) {
        if let Some(server) = net::server_of(&url) {
            self.remember_server(server.clone());
            if !net::is_share(&url) {
                self.net.expand = Some(server);
                if self.cwd != NETWORK {
                    self.navigate(NETWORK.to_vec(), true);
                }
                return;
            }
        }
        self.net.busy = true;
        self.error = None;
        let failed = url.clone();
        self.spawn_io_or(
            move || {
                let result = match net::connect(&url, creds.as_ref()) {
                    net::Connected::Ok => NetResult::Connected { open: net::local_path(&url) },
                    net::Connected::NeedsCredentials(message) => NetResult::NeedsCredentials { url, message },
                    net::Connected::Failed(e) => NetResult::Failed(e),
                };
                Some(Msg::NetDone(result))
            },
            move |e| Msg::NetDone(NetResult::Failed(format!("{failed}: {e}"))),
        );
    }

    pub(super) fn disconnect(&mut self, path: Vec<u8>, gvfs: bool) {
        if fsys_in_scope(&self.cwd, &path) {
            self.navigate(NETWORK.to_vec(), true);
        }
        self.net.busy = true;
        self.spawn_io_or(
            move || {
                Some(Msg::NetDone(match net::disconnect(&path, gvfs) {
                    Ok(()) => NetResult::Disconnected,
                    Err(e) => NetResult::Failed(format!("Could not disconnect: {e}")),
                }))
            },
            |e| Msg::NetDone(NetResult::Failed(e)),
        );
    }

    pub(super) fn net_done(&mut self, result: NetResult) {
        self.net.busy = false;
        match result {
            NetResult::Connected { open: Some(path) } => self.navigate(path, true),
            NetResult::Connected { open: None } => self.notice = Some("Connected".into()),
            NetResult::NeedsCredentials { url, message } => {
                let mut creds = net::Credentials { user: std::env::var("USER").unwrap_or_default(), ..Default::default() };
                creds.domain = "WORKGROUP".into();
                self.net.login = Some(Login { url, creds, message: Some(message), focused: false });
            }
            NetResult::Disconnected => self.notice = Some("Disconnected".into()),
            NetResult::Failed(e) => self.error = Some(e),
        }
    }

    pub(super) fn net_dialogs(&mut self, ctx: &egui::Context) {
        let Some(login) = &mut self.net.login else { return };
        let mut close = false;
        let mut submit = None;
        egui::Modal::new(egui::Id::unique("net_login")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.heading("Connect to share");
            ui.label(RichText::new(&login.url).weak());
            if let Some(m) = &login.message {
                ui.colored_label(RED, m);
            }
            ui.add_space(6.0);
            let smb = login.url.starts_with("smb://");
            ui.add_enabled_ui(!login.creds.anonymous, |ui| {
                egui::Grid::new("login_grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    if smb {
                        ui.label("User");
                        let r = ui.add(TextEdit::singleline(&mut login.creds.user).desired_width(260.0));
                        if !login.focused {
                            r.request_focus();
                            login.focused = true;
                        }
                        ui.end_row();
                        ui.label("Domain");
                        ui.add(TextEdit::singleline(&mut login.creds.domain).desired_width(260.0));
                        ui.end_row();
                    }
                    ui.label("Password");
                    let r = ui.add(TextEdit::singleline(&mut login.creds.password).password(true).desired_width(260.0));
                    if !smb && !login.focused {
                        r.request_focus();
                        login.focused = true;
                    }
                    ui.end_row();
                });
            });
            ui.checkbox(&mut login.creds.anonymous, if smb { "Connect as guest" } else { "Connect anonymously" });
            if !smb {
                ui.weak("To connect as another user, put it in the address: sftp://user@host");
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Connect").clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = Some((login.url.clone(), login.creds.clone()));
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
            });
        });
        if let Some((url, creds)) = submit {
            self.net.login = None;
            self.connect_share(url, Some(creds));
        } else if close {
            self.net.login = None;
        }
    }

    pub(super) fn network_page(&mut self, ui: &mut Ui) {
        let mut go: Option<Vec<u8>> = None;
        let mut connect: Option<(String, bool)> = None;
        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("Network");
                if self.net.busy {
                    ui.spinner();
                    ui.weak("working…");
                }
                if let Some(e) = &self.error {
                    ui.colored_label(RED, e.as_str());
                }
            });
            ui.add_space(6.0);

            // Connect bar
            ui.horizontal(|ui| {
                let resp = ui.add(TextEdit::singleline(&mut self.net.connect_text).hint_text(r"smb://server/share,  sftp://user@host,  \\server\share").desired_width(340.0));
                let url = net::to_url(&self.net.connect_text);
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.add_enabled(url.is_some(), Button::new("Connect")).on_hover_text("Connect and open (a server: list its shares)").clicked() || (enter && url.is_some()) {
                    connect = url.clone().map(|u| (u, false));
                }
                if ui.add_enabled(url.as_deref().is_some_and(net::is_share), Button::new("Connect as…")).on_hover_text("Connect with other credentials").clicked() {
                    connect = url.map(|u| (u, true));
                }
            });
            ui.add_space(8.0);
            self.connected_section(ui, &mut go);
            self.servers_section(ui, &mut connect);
        });
        if let Some((url, ask)) = connect {
            if ask {
                self.net_done(NetResult::NeedsCredentials { url, message: String::new() });
                if let Some(l) = &mut self.net.login {
                    l.message = None;
                }
            } else {
                self.connect_share(url, None);
            }
        }
        if let Some(path) = go {
            self.navigate(path, true);
        }
        if let Some(p) = self.net.pending_disconnect.take() {
            self.disconnect(p.0, p.1);
        }
    }

    fn connected_section(&mut self, ui: &mut Ui, go: &mut Option<Vec<u8>>) {
        let shares: Vec<_> = self.volumes.iter().filter(|v| v.network).cloned().collect();
        egui::CollapsingHeader::new(RichText::new(format!("Connected shares ({})", shares.len())).strong()).default_open(true).show(ui, |ui| {
            if shares.is_empty() {
                ui.weak("No network shares are connected. Connect one above, or open a server below.");
            }
            egui::Grid::new("conn_grid").num_columns(3).spacing([16.0, 6.0]).striped(true).show(ui, |ui| {
                for v in &shares {
                    ui.label(&v.label);
                    ui.label(RichText::new(if v.gvfs { lossy(&v.point) } else { v.tip.clone() }).small().weak());
                    ui.horizontal(|ui| {
                        if ui.small_button("Open").clicked() {
                            *go = Some(v.point.clone());
                        }
                        let tip = if v.gvfs { "Disconnect this share" } else { "Unmount (only works for mounts users may unmount)" };
                        if ui.small_button("Disconnect").on_hover_text(tip).clicked() {
                            self.net.pending_disconnect = Some((v.point.clone(), v.gvfs));
                        }
                    });
                    ui.end_row();
                }
            });
        });
        ui.add_space(6.0);
    }

    /// Shares of `server`, fetched on first expand.
    fn share_list(&mut self, ui: &mut Ui, server: &str, connect: &mut Option<(String, bool)>) {
        self.fetch_shares(server);
        match self.net.shares.get(server).cloned().flatten() {
            None => {
                ui.weak("loading shares…");
            }
            Some(Err(e)) => {
                ui.horizontal(|ui| {
                    ui.colored_label(RED, e);
                    if ui.small_button("Retry").clicked() {
                        self.net.shares.remove(server);
                    }
                });
            }
            Some(Ok(shares)) if shares.is_empty() => {
                ui.weak("No shared folders.");
            }
            Some(Ok(shares)) => {
                for share in shares {
                    let url = format!("{server}/{share}");
                    ui.horizontal(|ui| {
                        if ui.link(format!("🗀 {share}")).clicked() {
                            *connect = Some((url.clone(), false));
                        }
                        if ui.small_button("Connect as…").clicked() {
                            *connect = Some((url.clone(), true));
                        }
                    });
                }
            }
        }
    }

    fn servers_section(&mut self, ui: &mut Ui, connect: &mut Option<(String, bool)>) {
        let mut servers = self.servers.clone();
        // Servers behind connected shares are worth showing too.
        for v in self.volumes.iter().filter(|v| v.gvfs) {
            let name = lossy(super::fsys::base_name(&v.point));
            let host = name.split(',').find_map(|kv| kv.split_once(':').map_or(kv, |(_, r)| r).strip_prefix("server=").or_else(|| kv.split_once(':').map_or(kv, |(_, r)| r).strip_prefix("host=")));
            let scheme = name.split(':').next().unwrap_or("smb").trim_end_matches("-share");
            if let Some(h) = host {
                let s = format!("{scheme}://{h}");
                if !servers.iter().any(|x| x.eq_ignore_ascii_case(&s)) {
                    servers.push(s);
                }
            }
        }
        let expand = self.net.expand.take();
        egui::CollapsingHeader::new(RichText::new(format!("Servers ({})", servers.len())).strong()).default_open(true).show(ui, |ui| {
            if servers.is_empty() {
                ui.weak("Servers you connect to, and servers behind connected shares, appear here.");
            }
            for server in servers {
                let remembered = self.servers.iter().any(|s| s.eq_ignore_ascii_case(&server));
                let mut header = egui::CollapsingHeader::new(format!("🖧 {}", server)).id_salt(("srv", server.to_lowercase()));
                if expand.as_deref() == Some(server.as_str()) {
                    header = header.open(Some(true));
                }
                let resp = header.show(ui, |ui| self.share_list(ui, &server, connect));
                resp.header_response.context_menu(|ui| {
                    if ui.button("Refresh shares").clicked() {
                        self.net.shares.remove(&server);
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

fn fsys_in_scope(path: &[u8], root: &[u8]) -> bool {
    super::fsys::in_scope(path, root)
}
