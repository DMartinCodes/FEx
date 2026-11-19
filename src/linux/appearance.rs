//! How the window looks and what it shows: theme (dark, light, frosted
//! glass), accent colour, font and interface size, and which parts of the
//! window are on screen (sidebar sections, toolbar items, status bar,
//! columns). Shared by every FEx window of the user, in
//! `$XDG_CONFIG_HOME/fex/appearance.txt` as `key<TAB>value` lines, so a
//! damaged line costs only that setting.
//!
//! Performance first: everything here is applied once, when it changes
//! (never per frame), and the default theme is opaque. Frosted glass asks
//! the window system for a transparent window, which is decided when the
//! window is created; it works with any compositing window manager, and
//! where there is none the window simply paints its tint opaque. Blur
//! behind the window is requested from compositors that offer it (KWin, and
//! picom builds that honour the KDE hint); elsewhere the glass is clear.

use eframe::egui::{self, Color32, Visuals};

use super::session::config_dir;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Theme {
    Dark,
    Light,
    Glass,
}

impl Theme {
    fn key(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
            Theme::Glass => "glass",
        }
    }

    fn from_key(s: &str) -> Option<Theme> {
        match s {
            "dark" => Some(Theme::Dark),
            "light" => Some(Theme::Light),
            "glass" => Some(Theme::Glass),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Appearance {
    pub theme: Theme,
    /// Selection and highlight colour.
    pub accent: [u8; 3],
    /// A fontconfig family for the interface; empty: the desktop's font.
    pub font: String,
    /// Interface size (egui zoom), 0.8 to 1.6.
    pub scale: f32,
    /// Frosted glass: the colour the window is tinted, how opaque the tint
    /// is (0.3 to 1.0), and whether to ask the compositor for blur.
    pub glass_tint: [u8; 3],
    pub glass_opacity: f32,
    pub glass_blur: bool,
    // What the window shows.
    pub sidebar: bool,
    pub quick_access: bool,
    pub drives: bool,
    /// Network volumes among the drives.
    pub network_drives: bool,
    pub capacity: bool,
    /// The Network entry and remembered servers.
    pub network: bool,
    pub nav_buttons: bool,
    pub new_button: bool,
    pub path_bar: bool,
    pub search_bar: bool,
    pub status_bar: bool,
    pub col_date: bool,
    pub col_type: bool,
    pub col_size: bool,
    /// The folder column of search results.
    pub col_folder: bool,
}

/// egui's own dark selection blue, the look FEx has always had.
pub const DEFAULT_ACCENT: [u8; 3] = [0, 92, 128];

impl Default for Appearance {
    fn default() -> Self {
        Appearance {
            theme: Theme::Dark,
            accent: DEFAULT_ACCENT,
            font: String::new(),
            scale: 1.0,
            glass_tint: [18, 22, 30],
            glass_opacity: 0.78,
            glass_blur: true,
            sidebar: true,
            quick_access: true,
            drives: true,
            network_drives: true,
            capacity: true,
            network: true,
            nav_buttons: true,
            new_button: true,
            path_bar: true,
            search_bar: true,
            status_bar: true,
            col_date: true,
            col_type: true,
            col_size: true,
            col_folder: true,
        }
    }
}

/// Layouts offered as starting points.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Preset {
    /// Everything.
    Full,
    /// Folders and search without the extras: no capacity bars, network
    /// entries or Type column.
    Compact,
    /// Just the search box and the results.
    SearchOnly,
}

fn hex(c: [u8; 3]) -> String {
    format!("{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

impl Appearance {
    fn path() -> std::path::PathBuf {
        config_dir().join("appearance.txt")
    }

    pub fn load() -> Appearance {
        let mut a = Appearance::default();
        let Ok(text) = std::fs::read_to_string(Self::path()) else { return a };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('\t') else { continue };
            let flag = || match v {
                "1" => Some(true),
                "0" => Some(false),
                _ => None,
            };
            let set = |b: &mut bool| {
                if let Some(f) = flag() {
                    *b = f;
                }
            };
            match k {
                "theme" => a.theme = Theme::from_key(v).unwrap_or(a.theme),
                "accent" => a.accent = parse_hex(v).unwrap_or(a.accent),
                "font" => a.font = v.trim().to_string(),
                "scale" => a.scale = v.parse::<f32>().ok().filter(|s| s.is_finite()).map_or(a.scale, |s| s.clamp(0.8, 1.6)),
                "glass_tint" => a.glass_tint = parse_hex(v).unwrap_or(a.glass_tint),
                "glass_opacity" => a.glass_opacity = v.parse::<f32>().ok().filter(|s| s.is_finite()).map_or(a.glass_opacity, |s| s.clamp(0.3, 1.0)),
                "glass_blur" => set(&mut a.glass_blur),
                "sidebar" => set(&mut a.sidebar),
                "quick_access" => set(&mut a.quick_access),
                "drives" => set(&mut a.drives),
                "network_drives" => set(&mut a.network_drives),
                "capacity" => set(&mut a.capacity),
                "network" => set(&mut a.network),
                "nav_buttons" => set(&mut a.nav_buttons),
                "new_button" => set(&mut a.new_button),
                "path_bar" => set(&mut a.path_bar),
                "search_bar" => set(&mut a.search_bar),
                "status_bar" => set(&mut a.status_bar),
                "col_date" => set(&mut a.col_date),
                "col_type" => set(&mut a.col_type),
                "col_size" => set(&mut a.col_size),
                "col_folder" => set(&mut a.col_folder),
                _ => {}
            }
        }
        a
    }

    /// Written to a temp file and renamed, so a crash never leaves half a file.
    pub fn save(&self) -> Result<(), String> {
        let b = |v: bool| if v { "1" } else { "0" };
        let lines = [
            ("theme", self.theme.key().to_string()),
            ("accent", hex(self.accent)),
            ("font", self.font.replace(['\t', '\n'], " ")),
            ("scale", format!("{:.2}", self.scale)),
            ("glass_tint", hex(self.glass_tint)),
            ("glass_opacity", format!("{:.2}", self.glass_opacity)),
            ("glass_blur", b(self.glass_blur).into()),
            ("sidebar", b(self.sidebar).into()),
            ("quick_access", b(self.quick_access).into()),
            ("drives", b(self.drives).into()),
            ("network_drives", b(self.network_drives).into()),
            ("capacity", b(self.capacity).into()),
            ("network", b(self.network).into()),
            ("nav_buttons", b(self.nav_buttons).into()),
            ("new_button", b(self.new_button).into()),
            ("path_bar", b(self.path_bar).into()),
            ("search_bar", b(self.search_bar).into()),
            ("status_bar", b(self.status_bar).into()),
            ("col_date", b(self.col_date).into()),
            ("col_type", b(self.col_type).into()),
            ("col_size", b(self.col_size).into()),
            ("col_folder", b(self.col_folder).into()),
        ];
        let text: String = lines.iter().map(|(k, v)| format!("{k}\t{v}\n")).collect();
        let path = Self::path();
        let fail = |e: std::io::Error| format!("Could not save the appearance settings ({}): {e}", path.display());
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(fail)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text).map_err(fail)?;
        std::fs::rename(&tmp, &path).map_err(fail)
    }

    /// Show what `preset` shows; theme, colours and fonts stay as they are.
    pub fn apply_preset(&mut self, preset: Preset) {
        let look = (self.theme, self.accent, self.font.clone(), self.scale, self.glass_tint, self.glass_opacity, self.glass_blur);
        *self = Appearance::default();
        (self.theme, self.accent, self.font, self.scale, self.glass_tint, self.glass_opacity, self.glass_blur) = look;
        match preset {
            Preset::Full => {}
            Preset::Compact => {
                self.capacity = false;
                self.network = false;
                self.network_drives = false;
                self.col_type = false;
            }
            Preset::SearchOnly => {
                self.sidebar = false;
                self.nav_buttons = false;
                self.new_button = false;
                self.path_bar = false;
            }
        }
    }

    /// Whether the window has to be created transparent for this look.
    pub fn wants_transparency(&self) -> bool {
        self.theme == Theme::Glass && self.glass_opacity < 0.995
    }

    fn glass_is_light(&self) -> bool {
        luminance(self.glass_tint) > 0.5
    }

    /// The egui colours for this look.
    pub fn visuals(&self) -> Visuals {
        let light = match self.theme {
            Theme::Light => true,
            Theme::Dark => false,
            Theme::Glass => self.glass_is_light(),
        };
        let mut v = if light { Visuals::light() } else { Visuals::dark() };
        let accent = Color32::from_rgb(self.accent[0], self.accent[1], self.accent[2]);
        v.selection.bg_fill = accent;
        v.selection.stroke.color = if luminance(self.accent) > 0.55 { Color32::BLACK } else { Color32::WHITE };
        v.hyperlink_color = mix(accent, if light { Color32::BLACK } else { Color32::WHITE }, 0.35);
        v.window_shadow = egui::Shadow::NONE;
        v.popup_shadow = egui::Shadow::NONE;
        if self.theme == Theme::Glass {
            let [r, g, b] = self.glass_tint;
            let a = (self.glass_opacity.clamp(0.3, 1.0) * 255.0).round() as u8;
            let tint = |alpha: u8| Color32::from_rgba_unmultiplied(r, g, b, alpha);
            v.panel_fill = tint(a);
            // Menus and dialogs stay nearly opaque: they hold text to read.
            v.window_fill = tint(a.max(240));
            // Fields and stripes read as slightly frosted panes on the tint.
            let pane = |k: f32| {
                let t = mix(Color32::from_rgb(r, g, b), if light { Color32::BLACK } else { Color32::WHITE }, k);
                Color32::from_rgba_unmultiplied(t.r(), t.g(), t.b(), a.max(200))
            };
            v.extreme_bg_color = pane(0.10);
            v.faint_bg_color = pane(0.05);
            v.code_bg_color = v.extreme_bg_color;
            v.widgets.noninteractive.bg_fill = v.panel_fill;
            v.widgets.noninteractive.weak_bg_fill = v.panel_fill;
        }
        v
    }

    /// The colour behind everything (what shows where nothing is painted).
    pub fn clear_color(&self, visuals: &Visuals) -> [f32; 4] {
        if self.wants_transparency() { [0.0; 4] } else { visuals.panel_fill.to_normalized_gamma_f32() }
    }

    /// Put colours and size into effect. Called when they change, not per frame.
    pub fn install(&self, ctx: &egui::Context) {
        let visuals = self.visuals();
        ctx.all_styles_mut(|s| {
            s.visuals = visuals.clone();
            // A plain, still look: no animations, so egui only redraws on input.
            s.animation_time = 0.0;
            s.spacing.scroll = egui::style::ScrollStyle::solid();
        });
        if (ctx.zoom_factor() - self.scale).abs() > 0.001 {
            ctx.set_zoom_factor(self.scale);
        }
    }
}

fn luminance(c: [u8; 3]) -> f32 {
    (0.2126 * c[0] as f32 + 0.7152 * c[1] as f32 + 0.0722 * c[2] as f32) / 255.0
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// Ask the compositor to blur what's behind the window (`on`), or stop.
/// KWin (and picom builds that honour it) read `_KDE_NET_WM_BLUR_BEHIND_REGION`
/// (empty: the whole window); other compositors ignore it, harmlessly.
pub fn request_blur(window: u32, on: bool) -> Result<(), String> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, PropMode};
    use x11rb::wrapper::ConnectionExt as _;
    let (conn, _) = x11rb::rust_connection::RustConnection::connect(None).map_err(|e| e.to_string())?;
    let atom = conn.intern_atom(false, b"_KDE_NET_WM_BLUR_BEHIND_REGION").map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?.atom;
    if on {
        conn.change_property32(PropMode::REPLACE, window, atom, AtomEnum::CARDINAL, &[]).map_err(|e| e.to_string())?;
    } else {
        conn.delete_property(window, atom).map_err(|e| e.to_string())?;
    }
    conn.flush().map_err(|e| e.to_string())
}

/// The X11 window id behind an eframe window, if it is one.
pub fn x11_window(frame: &eframe::Frame) -> Option<u32> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Xlib(h) => u32::try_from(h.window).ok(),
        RawWindowHandle::Xcb(h) => Some(h.window.get()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_survives_damage() {
        let dir = std::env::temp_dir().join(format!("fex-appearance-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: tests in this module don't race on the environment.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        let mut a = Appearance { theme: Theme::Glass, accent: [1, 2, 3], font: "Inter".into(), scale: 1.25, ..Default::default() };
        a.apply_preset(Preset::SearchOnly);
        a.save().unwrap();
        let b = Appearance::load();
        assert_eq!(a, b);
        assert!(!b.sidebar && b.search_bar && b.theme == Theme::Glass && b.font == "Inter");
        std::fs::write(Appearance::path(), "theme\tneon\nscale\t99\nsidebar\tmaybe\naccent\t#ff8000\n").unwrap();
        let c = Appearance::load();
        assert_eq!((c.theme, c.scale, c.sidebar, c.accent), (Theme::Dark, 1.6, true, [255, 128, 0]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn glass_is_translucent_but_menus_stay_readable() {
        let a = Appearance { theme: Theme::Glass, glass_opacity: 0.5, ..Default::default() };
        let v = a.visuals();
        assert!(v.panel_fill.a() < 200 && v.window_fill.a() >= 240);
        assert!(a.wants_transparency() && !Appearance::default().wants_transparency());
        assert_eq!(Appearance::default().clear_color(&Appearance::default().visuals())[3], 1.0);
    }
}
