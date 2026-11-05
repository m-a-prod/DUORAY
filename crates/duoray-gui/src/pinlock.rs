//! The PIN lock of the window (see `duoray_core::pin`): the lock screen at
//! start and when the window comes back from the tray, and setting, changing
//! or removing the PIN in the settings. The digits typed stay here; the pad
//! only shows how many.

use std::time::{Duration, Instant};

use duoray_core::pin::{self, PinLock};
use slint::ComponentHandle;

use crate::{App, AppWindow, Shared, flash_status, render, save};

/// What the pad is for. Its number is the UI's `pin-mode`.
#[derive(Default)]
pub enum Flow {
    #[default]
    None,
    /// The window is locked.
    Unlock,
    /// The current PIN, before removing or changing it.
    Verify(After),
    New,
    /// The new PIN again.
    Confirm(String),
}

#[derive(Clone, Copy)]
pub enum After {
    Remove,
    Change,
}

/// Wrong PINs in a row before the pad makes one wait.
const FREE_TRIES: u32 = 5;

#[derive(Default)]
pub struct State {
    flow: Flow,
    entered: String,
    error: String,
    failures: u32,
    wait_until: Option<Instant>,
}

pub fn install(ui: &AppWindow, app: &Shared) {
    let on = |f: fn(&AppWindow, &Shared, &mut App)| {
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || f(&ui_weak.unwrap(), &app, &mut app.lock().unwrap())
    };
    ui.on_pin_backspace(on(|ui, _, st| {
        st.pin.entered.pop();
        st.pin.error.clear();
        show(ui, st);
    }));
    ui.on_pin_submit(on(submit));
    ui.on_pin_cancel(on(|ui, _, st| {
        if matches!(st.pin.flow, Flow::Unlock) {
            return;
        }
        st.pin = State { failures: st.pin.failures, wait_until: st.pin.wait_until, ..Default::default() };
        ui.set_pin_enabled(st.store.settings.pin_lock.is_some());
        show(ui, st);
    }));
    // The switch has already flipped: it only starts the flow, and shows the
    // real state until the flow ends.
    ui.on_pin_toggle(on(|ui, _, st| {
        let on = st.store.settings.pin_lock.is_some();
        st.pin.flow = if on { Flow::Verify(After::Remove) } else { Flow::New };
        ui.set_pin_enabled(on);
        show(ui, st);
    }));
    ui.on_pin_change(on(|ui, _, st| {
        st.pin.flow = Flow::Verify(After::Change);
        show(ui, st);
    }));
    ui.on_pin_digit({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move |digit| {
            let ui = ui_weak.unwrap();
            let mut st = app.lock().unwrap();
            if matches!(st.pin.flow, Flow::None) || waiting(&st).is_some() || st.pin.entered.len() >= pin::MAX_LEN {
                return;
            }
            st.pin.entered.push_str(&digit);
            st.pin.error.clear();
            // A known length is checked on its last digit.
            if expected_len(&st).is_some_and(|n| st.pin.entered.len() == n) {
                submit(&ui, &app, &mut st);
            } else {
                show(&ui, &st);
            }
        }
    });
}

/// Locks the window, if a PIN is set.
pub fn lock(ui: &AppWindow, st: &mut App) {
    if st.store.settings.pin_lock.is_some() {
        st.pin.flow = Flow::Unlock;
        st.pin.entered.clear();
        st.pin.error.clear();
        show(ui, st);
    }
}

pub fn is_locked(st: &App) -> bool {
    matches!(st.pin.flow, Flow::Unlock)
}

fn submit(ui: &AppWindow, app: &Shared, st: &mut App) {
    let entered = std::mem::take(&mut st.pin.entered);
    match std::mem::take(&mut st.pin.flow) {
        Flow::None => {}
        flow @ (Flow::Unlock | Flow::Verify(_)) => {
            if let Some(secs) = waiting(st) {
                st.pin.flow = flow;
                return wrong(ui, st, format!("Слишком много попыток. Подождите {secs} с"));
            }
            let Some(lock) = st.store.settings.pin_lock.clone() else { return show(ui, st) };
            if !lock.matches(&entered) {
                st.pin.flow = flow;
                st.pin.failures += 1;
                let msg = match st.pin.failures.checked_sub(FREE_TRIES) {
                    // 30 s, then twice as long each time, up to 4 min.
                    Some(over) => {
                        let secs = 30u64 << over.min(3);
                        st.pin.wait_until = Some(Instant::now() + Duration::from_secs(secs));
                        format!("Неверный PIN-код. Подождите {secs} с")
                    }
                    None => "Неверный PIN-код".into(),
                };
                return wrong(ui, st, msg);
            }
            st.pin.failures = 0;
            match flow {
                Flow::Verify(After::Remove) => {
                    st.store.settings.pin_lock = None;
                    save(st);
                    ui.set_pin_enabled(false);
                    flash_status(ui, app, st, "Блокировка PIN-кодом выключена".into());
                }
                Flow::Verify(After::Change) => st.pin.flow = Flow::New,
                _ => {}
            }
        }
        Flow::New => {
            if pin::valid(&entered) {
                st.pin.flow = Flow::Confirm(entered);
            } else {
                st.pin.flow = Flow::New;
                return wrong(ui, st, format!("Нужно от {} до {} цифр", pin::MIN_LEN, pin::MAX_LEN));
            }
        }
        Flow::Confirm(first) => {
            if entered != first {
                st.pin.flow = Flow::New;
                return wrong(ui, st, "PIN-коды не совпали. Придумайте заново".into());
            }
            st.store.settings.pin_lock = Some(PinLock::new(&first));
            save(st);
            ui.set_pin_enabled(true);
            flash_status(ui, app, st, "PIN-код установлен".into());
        }
    }
    show(ui, st);
}

fn wrong(ui: &AppWindow, st: &mut App, msg: String) {
    st.pin.error = msg;
    ui.set_pin_error_tick(ui.get_pin_error_tick().wrapping_add(1));
    show(ui, st);
}

/// Seconds left before another try, after too many wrong ones.
fn waiting(st: &App) -> Option<u64> {
    let left = st.pin.wait_until?.checked_duration_since(Instant::now())?;
    Some(left.as_secs() + 1)
}

/// The length the pad checks on: the set PIN's, or the first entry's when
/// repeating a new one.
fn expected_len(st: &App) -> Option<usize> {
    match &st.pin.flow {
        Flow::Unlock | Flow::Verify(_) => st.store.settings.pin_lock.as_ref().map(|l| l.len),
        Flow::Confirm(first) => Some(first.len()),
        Flow::New | Flow::None => None,
    }
}

fn show(ui: &AppWindow, st: &App) {
    ui.set_pin_mode(match st.pin.flow {
        Flow::None => 0,
        Flow::Unlock => 1,
        Flow::Verify(_) => 2,
        Flow::New => 3,
        Flow::Confirm(_) => 4,
    });
    ui.set_pin_count(st.pin.entered.len() as i32);
    ui.set_pin_length(expected_len(st).unwrap_or(0) as i32);
    ui.set_pin_error(st.pin.error.as_str().into());
    render(ui, st);
}
