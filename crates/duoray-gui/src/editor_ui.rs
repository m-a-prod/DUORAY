//! Server context menu (ping one host, edit, revert) and the server editor.
//!
//! An edit replaces the server in the store and is re-applied after every
//! subscription update (see `Store::edit_server`). Saving the server the
//! tunnel runs on reconnects with the new config.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::MutexGuard;

use duoray_core::link::Profile;
use duoray_core::server::{Server, Source, display_name};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::server_edit::{self, Field, Span};
use crate::{
    App, AppWindow, CodeLine, CodeSpan, ConnState, EditField, EditSection, Shared, current, display_order, flash_status, key_of, ping_jobs,
    reconnect, render, row_of, save, server_key,
};

/// The server being edited, found again by key on save: a subscription
/// update may reorder the list while the editor is open.
pub struct Editor {
    sub_id: String,
    key: String,
    draft: Draft,
    /// Field flagged by the last failed save; cleared once it is edited.
    error_key: Option<&'static str>,
}

enum Draft {
    /// The text lives in the UI (`editor-json`).
    Json,
    Link(Box<Profile>),
}

/// What the JSON editor shows, kept on the UI thread so a keystroke can be
/// turned into a minimal model update.
#[derive(Default)]
struct Shown {
    code: Rc<VecModel<CodeLine>>,
    lines: Vec<Vec<Span>>,
    line_count: usize,
}

thread_local! {
    static SHOWN: RefCell<Shown> = RefCell::default();
}

pub fn install(ui: &AppWindow, app: &Shared) {
    ui.set_editor_mono(server_edit::mono_font().into());
    SHOWN.with_borrow(|s| ui.set_editor_code(s.code.clone().into()));

    ui.on_ping_server({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move |row| {
            let ui = ui_weak.unwrap();
            let st = app.lock().unwrap();
            let Some((sub, index)) = server_at(&ui, &st, row) else { return };
            let job = (key_of(sub, &sub.servers[index]), sub.servers[index].clone());
            ping_jobs(&ui, &app, st, vec![job]);
        }
    });

    ui.on_edit_server({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move |row| open(&ui_weak.unwrap(), &app, row)
    });

    ui.on_reset_server({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move |row| {
            let ui = ui_weak.unwrap();
            let st = app.lock().unwrap();
            let Some((sub, index)) = server_at(&ui, &st, row) else { return };
            let (sub_id, key) = (sub.id.clone(), key_of(sub, &sub.servers[index]));
            revert(&ui, &app, st, sub_id, key);
        }
    });

    ui.on_editor_json_changed({
        let ui_weak = ui.as_weak();
        move |text| check_json(&ui_weak.unwrap(), &text)
    });

    ui.on_editor_format({
        let ui_weak = ui.as_weak();
        move || {
            let ui = ui_weak.unwrap();
            let text = ui.get_editor_json();
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                let pretty = server_edit::pretty(&v);
                ui.set_editor_json(pretty.as_str().into());
                check_json(&ui, &pretty);
            } else {
                check_json(&ui, &text);
            }
        }
    });

    ui.on_editor_cancel({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            app.lock().unwrap().editor = None;
            ui_weak.unwrap().set_editor_open(false);
        }
    });

    ui.on_editor_field_changed({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move |index, value| {
            let ui = ui_weak.unwrap();
            let mut st = app.lock().unwrap();
            let Some(Editor { draft: Draft::Link(p), error_key, .. }) = st.editor.as_mut() else { return };
            let Some(key) = server_edit::fields(p).get(index as usize).map(|f| f.key) else { return };
            server_edit::set(p, key, &value);
            // Re-render only when fields come or go (or an error mark clears):
            // rebuilding the form while typing would reset the cursor.
            let clears_error = *error_key == Some(key);
            if clears_error {
                *error_key = None;
                ui.set_editor_status("".into());
            }
            if clears_error || server_edit::changes_layout(key) {
                show_fields(&ui, p, None);
            }
        }
    });

    ui.on_editor_as_json({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let ui = ui_weak.unwrap();
            let mut st = app.lock().unwrap();
            let Some(ed) = st.editor.as_mut() else { return };
            let Draft::Link(p) = &ed.draft else { return };
            let text = server_edit::pretty(&server_edit::link_to_json(p));
            ed.draft = Draft::Json;
            ed.error_key = None;
            ui.set_editor_json(text.as_str().into());
            check_json(&ui, &text);
            ui.set_editor_note(
                "Сервер сохранится как JSON-конфиг: в нём можно менять всё, что умеет xray. \
                 Вернуть форму можно через «Вернуть версию подписки»."
                    .into(),
            );
            ui.set_editor_mode(0);
        }
    });

    ui.on_editor_save({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || save_edit(&ui_weak.unwrap(), &app)
    });

    ui.on_editor_reset({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let ui = ui_weak.unwrap();
            let st = app.lock().unwrap();
            let Some(ed) = st.editor.as_ref() else { return };
            let (sub_id, key) = (ed.sub_id.clone(), ed.key.clone());
            revert(&ui, &app, st, sub_id, key);
        }
    });
}

/// The subscription shown and the store index of its server in list row `row`.
fn server_at<'a>(ui: &AppWindow, st: &'a App, row: i32) -> Option<(&'a duoray_core::store::Subscription, usize)> {
    let sub = current(ui, st)?;
    let index = *display_order(st, sub).get(usize::try_from(row).ok()?)?;
    Some((sub, index))
}

fn open(ui: &AppWindow, app: &Shared, row: i32) {
    let mut st = app.lock().unwrap();
    let Some((sub, index)) = server_at(ui, &st, row) else { return };
    let server = &sub.servers[index];
    let key = key_of(sub, server);
    let live = matches!(&st.conn_state, ConnState::Connected { key: k, .. } if *k == key);

    ui.set_editor_title(display_name(&server.name).1.into());
    ui.set_editor_subtitle(
        format!("{} · {}:{} · {}", server.protocol, server.address, server.port, sub.display_name()).into(),
    );
    let mut note = vec![];
    if !sub.is_manual() {
        note.push("Правка сохранится и после обновления подписки.");
    }
    if matches!(server.source, Source::Json(_)) {
        note.push("Раздел inbounds DUORAY при подключении заменяет своим.");
    }
    ui.set_editor_note(note.join(" ").into());
    ui.set_editor_edited(server.override_key.is_some());
    ui.set_editor_live(live);

    let draft = match &server.source {
        Source::Json(config) => {
            let text = server_edit::pretty(config);
            ui.set_editor_json(text.as_str().into());
            check_json(ui, &text);
            ui.set_editor_mode(0);
            Draft::Json
        }
        Source::Link(p) => {
            show_fields(ui, p, None);
            ui.set_editor_status("".into());
            ui.set_editor_valid(true);
            ui.set_editor_mode(1);
            Draft::Link(p.clone())
        }
    };
    st.editor = Some(Editor { sub_id: sub.id.clone(), key, draft, error_key: None });
    ui.set_settings_open(false);
    ui.set_routing_open(false);
    ui.set_editor_open(true);
}

fn save_edit(ui: &AppWindow, app: &Shared) {
    let mut st = app.lock().unwrap();
    let Some(ed) = st.editor.as_mut() else { return };
    let server = match &ed.draft {
        Draft::Json => {
            let text = ui.get_editor_json();
            match Server::from_json_text(&text) {
                Ok(s) => s,
                Err(_) => return check_json(ui, &text),
            }
        }
        Draft::Link(p) => match server_edit::validate(p) {
            Ok(()) => Server::from_link((**p).clone()),
            Err((key, msg)) => {
                ed.error_key = Some(key);
                show_fields(ui, p, Some((key, msg)));
                ui.set_editor_status("Проверьте отмеченное поле".into());
                return;
            }
        },
    };
    let (sub_id, old_key) = (ed.sub_id.clone(), ed.key.clone());
    let Some(index) = index_of(&st, &sub_id, &old_key) else {
        ui.set_editor_status("Сервера больше нет: подписка обновилась, пока редактор был открыт.".into());
        ui.set_editor_valid(false);
        return;
    };
    st.store.edit_server(&sub_id, index, server);
    finish(ui, app, st, &sub_id, index, old_key, "Сервер сохранён");
}

/// Restores the subscription's version of the server with `key`.
fn revert(ui: &AppWindow, app: &Shared, mut st: MutexGuard<App>, sub_id: String, key: String) {
    let Some(index) = index_of(&st, &sub_id, &key) else { return };
    if st.store.reset_server(&sub_id, index) {
        finish(ui, app, st, &sub_id, index, key, "Возвращена версия подписки");
    }
}

fn index_of(st: &App, sub_id: &str, key: &str) -> Option<usize> {
    let sub = st.store.get(sub_id)?;
    sub.servers.iter().position(|s| key_of(sub, s) == key)
}

/// After a server changed: keep selection and the remembered server on it
/// (its key may have changed), drop its stale ping, reconnect if it is live.
fn finish(ui: &AppWindow, app: &Shared, mut st: MutexGuard<App>, sub_id: &str, index: usize, old_key: String, msg: &str) {
    let was_selected = server_key(ui, &st).as_deref() == Some(old_key.as_str());
    let live = matches!(&st.conn_state, ConnState::Connected { key, .. } if *key == old_key);
    let Some(new_key) = st.store.get(sub_id).map(|sub| key_of(sub, &sub.servers[index])) else { return };
    st.ping_results.remove(&old_key);
    if st.store.settings.last_server.as_deref() == Some(old_key.as_str()) {
        st.store.settings.last_server = Some(new_key.clone());
    }
    st.editor = None;
    ui.set_editor_open(false);
    save(&mut st);
    if was_selected && let Some(row) = current(ui, &st).and_then(|sub| row_of(&st, sub, &new_key)) {
        ui.set_current_server(row as i32);
    }
    flash_status(ui, app, &mut st, msg.into());
    render(ui, &st);
    drop(st);
    if live && was_selected {
        reconnect(ui, app);
    }
}

fn check_json(ui: &AppWindow, text: &str) {
    show_code(ui, text);
    match Server::from_json_text(text) {
        Ok(s) => {
            let mut status = format!("Конфиг в порядке · {} · {}:{}", s.protocol, s.address, s.port);
            if s.proxies > 1 {
                status += &format!(" · балансировщик, прокси: {}", s.proxies);
            }
            ui.set_editor_status(status.into());
            ui.set_editor_valid(true);
            ui.set_editor_error_offset(-1);
            ui.set_editor_error_line(0);
        }
        Err(e) => {
            let status = if e.line > 0 {
                format!("Строка {}, столбец {}: {}", e.line, e.column, e.message)
            } else {
                e.message.clone()
            };
            ui.set_editor_status(status.into());
            ui.set_editor_valid(false);
            ui.set_editor_error_offset(e.offset_in(text).map_or(-1, |o| o as i32));
            ui.set_editor_error_line(e.line as i32);
        }
    }
}

/// Updates the line numbers and highlighting. Only lines that differ from what
/// is shown are replaced: the common head and tail of the old and new line
/// lists stay, so typing touches one row and Enter inserts one.
fn show_code(ui: &AppWindow, text: &str) {
    let lines = server_edit::highlight(text);
    ui.set_editor_plain(lines.is_none());
    let lines = lines.unwrap_or_default();
    SHOWN.with_borrow_mut(|shown| {
        let count = text.split('\n').count();
        if count != shown.line_count {
            shown.line_count = count;
            ui.set_editor_lines((1..=count).map(|n| n.to_string()).collect::<Vec<_>>().join("\n").into());
        }
        let old = &shown.lines;
        let head = old.iter().zip(&lines).take_while(|(a, b)| a == b).count();
        let tail = old[head..].iter().rev().zip(lines[head..].iter().rev()).take_while(|(a, b)| a == b).count();
        let (old_mid, new_mid) = (old.len() - head - tail, lines.len() - head - tail);
        let row = |spans: &Vec<Span>| CodeLine {
            spans: ModelRc::new(VecModel::from(
                spans
                    .iter()
                    .map(|s| CodeSpan { col: s.col as i32, text: s.text.as_str().into(), kind: i32::from(s.kind) })
                    .collect::<Vec<_>>(),
            )),
        };
        for i in 0..old_mid.min(new_mid) {
            shown.code.set_row_data(head + i, row(&lines[head + i]));
        }
        for _ in new_mid..old_mid {
            shown.code.remove(head + new_mid);
        }
        for i in old_mid..new_mid {
            shown.code.insert(head + i, row(&lines[head + i]));
        }
        shown.lines = lines;
    });
}

fn show_fields(ui: &AppWindow, p: &Profile, error: Option<(&str, &str)>) {
    let mut sections: Vec<EditSection> = vec![];
    for (i, f) in server_edit::fields(p).into_iter().enumerate() {
        let Field { key, label, value, options, header, placeholder } = f;
        if !header.is_empty() || sections.is_empty() {
            sections.push(EditSection { title: header.into(), fields: ModelRc::default() });
        }
        let field = EditField {
            index: i as i32,
            label: label.into(),
            value: value.into(),
            options: ModelRc::new(VecModel::from(options.iter().map(|o| SharedString::from(*o)).collect::<Vec<_>>())),
            placeholder: placeholder.into(),
            error: error.filter(|(k, _)| *k == key).map(|(_, m)| m).unwrap_or_default().into(),
        };
        let last = sections.last_mut().unwrap();
        let mut fields: Vec<EditField> = last.fields.iter().collect();
        fields.push(field);
        last.fields = ModelRc::new(VecModel::from(fields));
    }
    ui.set_editor_sections(ModelRc::new(VecModel::from(sections)));
}
