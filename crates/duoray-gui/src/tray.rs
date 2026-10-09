//! The tray icon (menu bar on macOS): open, connect/disconnect, quit. The
//! window can then close to the tray while the VPN keeps running.
//!
//! Linux uses StatusNotifierItem over D-Bus (ksni, no GTK); Windows and macOS
//! use tray-icon, whose objects live on the main thread. Without a tray (GNOME
//! without the AppIndicator extension) `start` returns false and closing the
//! window quits as before.

use std::cell::RefCell;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Show,
    Toggle,
    Quit,
}

/// What the tray shows: the logo is grey while disconnected, powder pink when connected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub connected: bool,
    /// Tooltip line, e.g. "Подключено".
    pub status: String,
    /// The connect/disconnect item; empty disables it (nothing to do).
    pub toggle: String,
}

type Handler = Arc<dyn Fn(Action) + Send + Sync>;

thread_local! {
    static TRAY: RefCell<Option<imp::Tray>> = const { RefCell::new(None) };
    static LAST: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Creates the tray icon; `on_action` may run on another thread. Call on the
/// main thread once the event loop runs (macOS needs a running NSApp).
pub fn start(state: State, on_action: impl Fn(Action) + Send + Sync + 'static) -> bool {
    let tray = imp::Tray::new(&state, Arc::new(on_action));
    let ok = tray.is_some();
    TRAY.with(|t| *t.borrow_mut() = tray);
    LAST.with(|l| *l.borrow_mut() = Some(state));
    ok
}

pub fn available() -> bool {
    TRAY.with(|t| t.borrow().is_some())
}

/// Main thread only (the render path); cheap when nothing changed.
pub fn update(state: State) {
    if LAST.with(|l| l.borrow().as_ref() == Some(&state)) {
        return;
    }
    TRAY.with(|t| {
        if let Some(tray) = t.borrow().as_ref() {
            tray.update(&state);
        }
    });
    LAST.with(|l| *l.borrow_mut() = Some(state));
}

/// Gradient stops (offset, RGB) as in ui/assets/logo-{off,on}.svg.
const OFF: &[(f32, [u8; 3])] = &[(0.0, [0x3f, 0x3f, 0x46]), (0.4, [0x71, 0x71, 0x7a]), (0.75, [0xa1, 0xa1, 0xaa]), (1.0, [0xd4, 0xd4, 0xd8])];
const ON: &[(f32, [u8; 3])] = &[(0.0, [0xc9, 0x7f, 0xa3]), (0.5, [0xf4, 0xc2, 0xd7]), (1.0, [0xfd, 0xeb, 0xf3])];

fn gradient(stops: &[(f32, [u8; 3])], t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    let i = stops.iter().position(|s| s.0 >= t).unwrap_or(stops.len() - 1).max(1);
    let ((a, ca), (b, cb)) = (stops[i - 1], stops[i]);
    let f = if b > a { (t - a) / (b - a) } else { 0.0 };
    std::array::from_fn(|k| (ca[k] as f32 + (cb[k] as f32 - ca[k] as f32) * f).round() as u8)
}

/// The DUORAY mark (ui/assets/logo-*.svg: two mirrored "D"s, the right one at
/// half opacity, a diagonal gradient) as straight RGBA, centred in a square.
pub fn logo_rgba(size: u32, connected: bool) -> Vec<u8> {
    const W: f32 = 198.0;
    const H: f32 = 161.0;
    let stops = if connected { ON } else { OFF };
    // The left "D": a bar plus a half disc, minus the same shape inset.
    let d = |x: f32, y: f32| {
        let shape = |inset: f32, r: f32| {
            (x >= inset && x <= 47.5 && y >= inset && y <= H - inset)
                || (x >= 47.5 && (x - 47.5).powi(2) + (y - 80.5).powi(2) <= r * r)
        };
        shape(0.0, 80.5) && !shape(17.71, 62.79)
    };
    let scale = size as f32 / W;
    let top = (size as f32 - H * scale) / 2.0;
    const SS: u32 = 4;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut alpha = 0.0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / scale;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32 - top) / scale;
                    let left = if d(x, y) { 1.0 } else { 0.0 };
                    let right = if d(W - x, y) { 0.5 } else { 0.0 };
                    alpha += left + right * (1.0 - left);
                }
            }
            let a = (alpha / (SS * SS) as f32 * 255.0).round() as u8;
            // From the bottom-left corner (0) to the top-right one (1).
            let (x, y) = ((px as f32 + 0.5) / scale, (py as f32 + 0.5 - top) / scale);
            let t = (x * W + (H - y) * H) / (W * W + H * H);
            let c = gradient(stops, t);
            out.extend_from_slice(&[c[0], c[1], c[2], a]);
        }
    }
    out
}

#[cfg(target_os = "linux")]
mod imp {
    use ksni::blocking::{Handle, TrayMethods};

    use super::{Action, Handler, State, logo_rgba};

    struct Sni {
        state: State,
        on_action: Handler,
    }

    impl ksni::Tray for Sni {
        fn id(&self) -> String {
            "duoray".into()
        }
        fn title(&self) -> String {
            "DUORAY".into()
        }
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            [22, 32, 48, 64]
                .into_iter()
                .map(|size| {
                    // RGBA -> ARGB, as the spec wants.
                    let mut data = logo_rgba(size, self.state.connected);
                    for px in data.as_chunks_mut::<4>().0 {
                        px.rotate_right(1);
                    }
                    ksni::Icon { width: size as i32, height: size as i32, data }
                })
                .collect()
        }
        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip { title: "DUORAY".into(), description: self.state.status.clone(), ..Default::default() }
        }
        fn activate(&mut self, _x: i32, _y: i32) {
            (self.on_action)(Action::Show);
        }
        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::StandardItem;
            let item = |label: &str, enabled: bool, action: Action| {
                StandardItem {
                    label: label.into(),
                    enabled,
                    activate: Box::new(move |this: &mut Self| (this.on_action)(action)),
                    ..Default::default()
                }
                .into()
            };
            vec![
                item("Открыть DUORAY", true, Action::Show),
                item(&self.state.toggle, !self.state.toggle.is_empty(), Action::Toggle),
                ksni::MenuItem::Separator,
                item("Выход", true, Action::Quit),
            ]
        }
    }

    pub struct Tray(Handle<Sni>);

    impl Tray {
        pub fn new(state: &State, on_action: Handler) -> Option<Self> {
            Sni { state: state.clone(), on_action }.spawn().ok().map(Tray)
        }

        pub fn update(&self, state: &State) {
            let state = state.clone();
            self.0.update(move |t| t.state = state);
        }
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod imp {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    use super::{Action, Handler, State, logo_rgba};

    pub struct Tray {
        icon: TrayIcon,
        toggle: MenuItem,
    }

    fn icon(connected: bool) -> Option<Icon> {
        let size = 64;
        Icon::from_rgba(logo_rgba(size, connected), size, size).ok()
    }

    impl Tray {
        pub fn new(state: &State, on_action: Handler) -> Option<Self> {
            let show = MenuItem::new("Открыть DUORAY", true, None);
            let toggle = MenuItem::new(&state.toggle, !state.toggle.is_empty(), None);
            let quit = MenuItem::new("Выход", true, None);
            let menu = Menu::new();
            menu.append_items(&[&show, &toggle, &PredefinedMenuItem::separator(), &quit]).ok()?;
            let ids = [(show.id().clone(), Action::Show), (toggle.id().clone(), Action::Toggle), (quit.id().clone(), Action::Quit)];
            {
                let on_action = on_action.clone();
                MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                    if let Some((_, action)) = ids.iter().find(|(id, _)| *id == e.id) {
                        on_action(*action);
                    }
                }));
            }
            // Left click opens the window; the menu is on the right button.
            TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                    on_action(Action::Show);
                }
            }));
            let icon = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .with_tooltip(format!("DUORAY · {}", state.status))
                .with_icon(icon(state.connected)?)
                .build()
                .ok()?;
            Some(Tray { icon, toggle })
        }

        pub fn update(&self, state: &State) {
            let _ = self.icon.set_icon(icon(state.connected));
            let _ = self.icon.set_tooltip(Some(format!("DUORAY · {}", state.status)));
            self.toggle.set_text(&state.toggle);
            self.toggle.set_enabled(!state.toggle.is_empty());
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
mod imp {
    use super::{Handler, State};

    pub struct Tray;

    impl Tray {
        pub fn new(_: &State, _: Handler) -> Option<Self> {
            None
        }
        pub fn update(&self, _: &State) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_has_both_halves_and_transparent_corners() {
        let size = 64;
        let px = logo_rgba(size, true);
        assert_eq!(px.len(), (size * size * 4) as usize);
        let alpha = |x: u32, y: u32| px[((y * size + x) * 4 + 3) as usize];
        assert_eq!(alpha(0, 0), 0, "outside the mark");
        assert_eq!(alpha(size / 2, size / 2), 0, "the hollow middle");
        // Left bar is opaque, the right one half transparent.
        assert!(alpha(2, size / 2) > 200);
        assert!((100..160).contains(&alpha(size - 3, size / 2)));
        // Pink, darker at the bottom left than at the top right.
        let rgb = |x: u32, y: u32| &px[((y * size + x) * 4) as usize..][..3];
        assert!(rgb(2, size / 2)[0] > rgb(2, size / 2)[1]);
        assert!(rgb(2, size - 12).iter().map(|&c| c as u32).sum::<u32>() < rgb(size - 3, 12).iter().map(|&c| c as u32).sum::<u32>());
    }
}
