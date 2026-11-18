//! Keeping the sidebar's drives true to what Windows has mounted, and the
//! "Assign drive letter" dialog for drives that ended up without a usable
//! letter (see `volumes.rs`).

use eframe::egui::{self, Ui};

use super::{FexApp, Msg, RED};
use crate::volumes::{self, Snapshot};

pub struct LetterDialog {
    volume: String,
    name: String,
    /// Its letter in the mount manager (possibly taken here), if any.
    current: Option<char>,
    why: Option<String>,
    letters: Vec<char>,
    letter: char,
    busy: bool,
    error: Option<String>,
}

impl FexApp {
    /// Start re-reading drives in the background for the life of the window.
    pub(super) fn watch_drives(&self) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        volumes::watch(move |snapshot| {
            let alive = tx.send(Msg::Volumes(snapshot)).is_ok();
            ctx.request_repaint();
            alive
        });
    }

    /// Read drives now (after a refresh, eject or mapping) rather than at the
    /// watcher's next pass.
    pub(super) fn rescan_drives(&self) {
        self.spawn_io(|| Some(Msg::Volumes(volumes::scan(&mut volumes::Cache::default()))));
    }

    pub(super) fn set_volumes(&mut self, snapshot: Snapshot) {
        if snapshot == self.volumes {
            return;
        }
        let old = std::mem::replace(&mut self.volumes, snapshot);
        // A letter that now points elsewhere (another drive plugged in, a
        // mapping replaced) has stale free-space and eject facts.
        for d in &old.drives {
            if self.volumes.letter(&d.root).is_none_or(|now| now.target != d.target) {
                self.drive_space.remove(&d.root);
            }
        }
        let network = |s: &Snapshot| s.drives.iter().filter_map(|d| Some((d.root.clone(), d.network.clone()?))).collect::<Vec<_>>();
        if network(&old) != network(&self.volumes) {
            self.refresh_connections();
        }
        self.drives = self.volumes.drives.iter().map(|d| d.root.clone()).collect();
        // The dialog's drive was unplugged.
        if let Some(d) = &self.letter_dialog
            && !d.busy
            && !self.volumes.hidden.iter().any(|h| h.volume == d.volume)
            && !self.volumes.drives.iter().any(|l| l.volume.as_deref() == Some(d.volume.as_str()))
        {
            self.letter_dialog = None;
        }
    }

    /// Letters remembered for network drives, connected or not.
    fn remembered_letters(&self) -> Vec<char> {
        self.net.connections.iter().filter_map(|c| c.local.as_ref()?.chars().next()).collect()
    }

    pub(super) fn open_letter_dialog(&mut self, volume: String, name: String, current: Option<char>, why: Option<String>) {
        // A remembered mapping on the drive's own letter is the conflict being fixed, not a reason to avoid it.
        let remembered: Vec<char> = self.remembered_letters().into_iter().filter(|&l| Some(l) != current).collect();
        let letters = volumes::free_letters(&self.volumes, &remembered, &volume);
        let Some(&letter) = letters.first() else {
            self.error = Some("There is no free drive letter to give this drive".into());
            return;
        };
        self.letter_dialog = Some(LetterDialog { volume, name, current, why, letters, letter, busy: false, error: None });
    }

    pub(super) fn letter_dialog(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.letter_dialog else { return };
        let mut close = false;
        let mut submit = None;
        egui::Modal::new(egui::Id::unique("assign_letter")).show(ctx, |ui: &mut Ui| {
            ui.set_width(420.0);
            ui.heading(format!("Drive letter for {}", d.name));
            ui.add_space(6.0);
            if let Some(why) = &d.why {
                ui.label(why);
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                ui.label("New letter");
                ui.add_enabled_ui(!d.busy, |ui| {
                    egui::ComboBox::from_id_salt("assign_letter_combo").selected_text(format!("{}:", d.letter)).show_ui(ui, |ui| {
                        for &l in &d.letters {
                            ui.selectable_value(&mut d.letter, l, format!("{l}:"));
                        }
                    });
                });
            });
            ui.add_space(4.0);
            ui.weak("Windows asks for administrator approval, then uses this letter whenever the drive is plugged in.");
            if let Some(e) = &d.error {
                ui.colored_label(RED, e);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if d.busy {
                    ui.spinner();
                    ui.weak("Waiting for Windows…");
                } else {
                    if ui.button("Assign").clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        submit = Some((d.volume.clone(), d.current, d.letter));
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        close = true;
                    }
                }
            });
        });
        if let Some((volume, current, letter)) = submit {
            d.busy = true;
            d.error = None;
            self.spawn_io_or(
                move || {
                    let result = volumes::assign_letter(&volume, current, letter);
                    Some(Msg::LetterAssigned { result, letter })
                },
                move |e| Msg::LetterAssigned { result: Err(e), letter },
            );
        }
        if close {
            self.letter_dialog = None;
        }
    }

    pub(super) fn letter_assigned(&mut self, result: Result<(), String>, letter: char) {
        self.rescan_drives();
        match result {
            Ok(()) => {
                let name = self.letter_dialog.take().map(|d| d.name).unwrap_or_else(|| "The drive".into());
                self.notice = Some(format!("{name} is now {letter}:"));
            }
            Err(e) => match &mut self.letter_dialog {
                Some(d) => {
                    d.busy = false;
                    d.error = Some(e);
                }
                None => self.error = Some(e),
            },
        }
    }
}
