//! Co-authoring a workbook over the mesh: the Craft seam's shared session bound to one open
//! workbook.
//!
//! Every cell travels as two entries in the seam's keyed register: `c/<sheet id>/<A1>` holds what
//! was typed (a value or a formula) and `f/<sheet id>/<A1>` the cell's style; `s/<sheet id>`
//! holds a sheet's name. A local edit is captured by comparing the workbook before and after it,
//! so every command that changes cells (typing, paste, fill, clear, every format) shares without
//! a line of its own here. When two authors write one cell at once, every replica keeps the same
//! value. Formulas recalculate on each replica from the shared inputs, so only inputs travel
//! (volatile functions such as `NOW` and `RAND` differ per peer).
//!
//! What cannot travel as cells is refused while shared, and the workbook is left as it was:
//! edits that move cells (inserting or deleting lines, sorting), removing, renaming or moving a
//! sheet, and changes outside the cells (names, merges, charts, tables, comments). Row heights,
//! column widths and view settings stay local.
//!
//! Undo while shared takes back only this author's own edits; redo is not shared.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use gridcraft_core::CellRef;
use gridcraft_model::{Sheet, Workbook};
use serde_json::{Value as Json, json};

use crate::{Result, Session};

/// The shared session and the workbook it is bound to.
pub struct CollabRuntime {
    shared: craft_collab::CollabSession,
    /// The bound workbook's `DocState::uid`.
    doc: u64,
    /// This author's tag, for the ids of sheets it adds.
    tag: u32,
}

/// A register key, decoded.
enum Entry {
    Sheet(u64),
    Input(u64, CellRef),
    Style(u64, CellRef),
}

fn sheet_key(id: u64) -> String {
    format!("s/{id}")
}

fn input_key(id: u64, c: CellRef) -> String {
    format!("c/{id}/{}", c.a1())
}

fn style_key(id: u64, c: CellRef) -> String {
    format!("f/{id}/{}", c.a1())
}

fn parse_key(key: &str) -> Option<Entry> {
    let mut parts = key.splitn(3, '/');
    let (kind, id) = (parts.next()?, parts.next()?.parse::<u64>().ok()?);
    match (kind, parts.next()) {
        ("s", None) => Some(Entry::Sheet(id)),
        ("c", Some(a1)) => Some(Entry::Input(id, CellRef::parse(a1)?)),
        ("f", Some(a1)) => Some(Entry::Style(id, CellRef::parse(a1)?)),
        _ => None,
    }
}

/// The style of a cell as it travels: the style itself (ids differ per replica), `None` for the
/// default style.
fn style_value(wb: &Workbook, sheet: &Sheet, c: CellRef) -> Option<String> {
    let id = sheet.cells.get(c).map(|cell| cell.style)?;
    if id == gridcraft_model::StyleId::DEFAULT {
        return None;
    }
    serde_json::to_string(wb.styles.get(id)).ok()
}

/// What a cell holds as it travels: its input, `None` when empty.
fn input_value(sheet: &Sheet, c: CellRef) -> Option<String> {
    Some(sheet.input_text(c)).filter(|t| !t.is_empty())
}

/// Every register entry a workbook holds: what a host seeds when it first shares.
fn entries_of(wb: &Workbook) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for sheet in &wb.sheets {
        out.push((sheet_key(sheet.id), sheet.name.clone()));
        for (c, _) in sheet.cells.iter() {
            if let Some(input) = input_value(sheet, c) {
                out.push((input_key(sheet.id, c), input));
            }
            if let Some(style) = style_value(wb, sheet, c) {
                out.push((style_key(sheet.id, c), style));
            }
        }
    }
    out
}

/// Gives every sheet a distinct id, keeping ids it already has: what a workbook needs before it
/// is first shared. Ids minted here use tag 0 (never shared).
fn ensure_sheet_ids(wb: &mut Workbook) {
    let mut seen = BTreeSet::new();
    for i in 0..wb.sheets.len() {
        let id = wb.sheets.get(i).map_or(0, |s| s.id);
        if id != 0 && seen.insert(id) {
            continue;
        }
        let fresh = wb.mint_id(0);
        seen.insert(fresh);
        if let Some(sheet) = wb.sheets.get_mut(i) {
            Arc::make_mut(sheet).id = fresh;
        }
    }
}

/// A sheet without what may change while shared: its cells, spill caches, row and column sizes
/// and view settings.
fn sheet_shape(sheet: &Sheet) -> Sheet {
    let mut s = sheet.clone();
    s.cells = Default::default();
    s.spill.clear();
    s.spill_ranges.clear();
    s.rows.clear();
    s.cols.clear();
    s.freeze = None;
    s.zoom = 100;
    s.show_gridlines = true;
    s.show_headings = true;
    s.show_formulas = false;
    s.show_zeros = true;
    s.view_active = CellRef::default();
    s.view_top_left = CellRef::default();
    s.view = Default::default();
    s
}

/// A workbook without its sheets, styles, active sheet and id counters: what must not change
/// while shared.
fn workbook_shape(wb: &Workbook) -> Workbook {
    let mut w = wb.clone();
    w.sheets.clear();
    w.styles = Default::default();
    w.active_sheet = 0;
    w.next_ids.clear();
    w
}

impl Session {
    /// The bound workbook's shared session, while one is shared.
    pub fn collab_session(&self) -> Option<&craft_collab::CollabSession> {
        self.collab.as_ref().map(|c| &c.shared)
    }

    /// The same, mutable.
    pub fn collab_session_mut(&mut self) -> Option<&mut craft_collab::CollabSession> {
        self.collab.as_mut().map(|c| &mut c.shared)
    }

    /// Whether a workbook is shared.
    pub fn collab_active(&self) -> bool {
        self.collab.is_some()
    }

    /// The uid of the shared workbook.
    pub fn collab_doc(&self) -> Option<u64> {
        self.collab.as_ref().map(|c| c.doc)
    }

    /// Shares the active workbook through `shared` (a session the mesh wiring built). A host
    /// gives its sheets their ids and seeds the register with every cell; a joiner's workbook is
    /// replaced by the host's snapshot when it arrives.
    pub fn collab_install(&mut self, mut shared: craft_collab::CollabSession) -> Result<()> {
        let tag = craft_collab::author_tag(shared.author());
        let d = self.doc_mut()?;
        let doc = d.uid;
        if !shared.awaiting_snapshot() && !shared.awaiting_grant() {
            let wb = Arc::make_mut(&mut d.wb);
            ensure_sheet_ids(wb);
            if let Some(update) = shared.texts_mut().register_seed(&entries_of(wb)) {
                shared.send_text(String::new(), update, None);
            }
            shared.texts_mut().register_changed();
        }
        self.collab = Some(CollabRuntime { shared, doc, tag });
        Ok(())
    }

    /// Ends sharing (the workbook stays as it is).
    pub fn collab_leave(&mut self) {
        self.collab = None;
    }

    /// The realm membership status the app read for this person (the Share window shows it).
    pub fn collab_set_realm(&mut self, status: Json) {
        self.realm = Some(status);
    }

    /// The realm membership status, or a bare not-signed-in answer.
    pub fn collab_realm(&self) -> Json {
        self.realm.clone().unwrap_or_else(|| json!({"signedIn": false}))
    }

    /// The session's status for agents and the Share window.
    pub fn collab_stats(&self) -> Json {
        let Some(c) = self.collab.as_ref() else { return json!({"active": false}) };
        let mut stats = c.shared.stats();
        if let Some(obj) = stats.as_object_mut() {
            obj.insert("cells".into(), json!(c.shared.texts().register_entries().iter().filter(|(k, _)| k.starts_with("c/")).count()));
        }
        stats
    }

    fn collab_bound(&self) -> Option<usize> {
        let doc = self.collab.as_ref()?.doc;
        self.docs.iter().position(|d| d.uid == doc)
    }

    /// Whether `id`'s run on the active workbook is a shared edit.
    pub(crate) fn collab_on_active(&self) -> bool {
        self.collab_bound().is_some_and(|i| i == self.active)
    }

    /// After a local command changed the shared workbook (`before` is the workbook as it was):
    /// shares the cells it changed and the sheets it added, or refuses the edit (the caller puts
    /// `before` back) when it changed what cannot travel as cells.
    pub(crate) fn collab_capture(&mut self, before: &Arc<Workbook>) -> std::result::Result<(), String> {
        let Some(i) = self.collab_bound() else { return Ok(()) };
        let Some(tag) = self.collab.as_ref().map(|c| c.tag) else { return Ok(()) };
        let Some(d) = self.docs.get_mut(i) else { return Ok(()) };
        if Arc::ptr_eq(&d.wb, before) {
            return Ok(());
        }
        // A structural edit (a full recalculation) is fine when it only added sheets; one that
        // changed the cells of a sheet already there moved them.
        let moved = d.last_structural
            && before.sheets.iter().any(|old| d.wb.sheets.iter().find(|s| s.id == old.id).is_some_and(|new| !old.cells.changed_cells(&new.cells).is_empty()));
        if moved {
            return Err("moving cells (inserting or deleting rows and columns, sorting) isn't shared yet".into());
        }
        if workbook_shape(&d.wb) != workbook_shape(before) {
            return Err("this change isn't shared yet: only cells, their formats and new sheets travel".into());
        }
        // Sheets: every old one still there, in order, under its name; new ones get their ids.
        let old_ids: Vec<u64> = before.sheets.iter().map(|s| s.id).collect();
        let wb = Arc::make_mut(&mut d.wb);
        let added: Vec<usize> = wb.sheets.iter().enumerate().filter(|(_, sh)| sh.id == 0 || !old_ids.contains(&sh.id)).map(|(i, _)| i).collect();
        for i in added {
            let id = wb.mint_id(tag);
            if let Some(sheet) = wb.sheets.get_mut(i) {
                Arc::make_mut(sheet).id = id;
            }
        }
        order_shared_sheets(wb);
        let kept: Vec<u64> = wb.sheets.iter().map(|s| s.id).filter(|id| old_ids.contains(id)).collect();
        if kept != old_ids {
            return Err("removing or moving a sheet isn't shared yet".into());
        }
        let mut writes: Vec<(String, Option<String>)> = Vec::new();
        for sheet in &wb.sheets {
            let old = before.sheets.iter().find(|s| s.id == sheet.id);
            match old {
                None => writes.push((sheet_key(sheet.id), Some(sheet.name.clone()))),
                Some(old) if Arc::ptr_eq(old, sheet) => continue,
                Some(old) => {
                    if old.name != sheet.name {
                        return Err("renaming a sheet isn't shared yet".into());
                    }
                    if sheet_shape(old) != sheet_shape(sheet) {
                        return Err("this change isn't shared yet: only cells, their formats and new sheets travel".into());
                    }
                }
            }
            let empty = gridcraft_model::CellStore::default();
            let old_cells = old.map_or(&empty, |o| &o.cells);
            for c in old_cells.changed_cells(&sheet.cells) {
                let (input, style) = (input_value(sheet, c), style_value(wb, sheet, c));
                let (was_input, was_style) = match old {
                    Some(o) => (input_value(o, c), style_value(before, o, c)),
                    None => (None, None),
                };
                if input != was_input {
                    writes.push((input_key(sheet.id, c), input));
                }
                if style != was_style {
                    writes.push((style_key(sheet.id, c), style));
                }
            }
        }
        let Some(c) = self.collab.as_mut() else { return Ok(()) };
        for (key, value) in writes {
            let update = match value {
                Some(v) => c.shared.texts_mut().register_set(&key, &v),
                None => c.shared.texts_mut().register_remove(&key),
            };
            if let Some(update) = update {
                c.shared.send_text(String::new(), update, None);
            }
        }
        // Our own writes are in the changed list too; they are already applied.
        c.shared.texts_mut().register_changed();
        Ok(())
    }

    /// Applies what the peers sent and shares this author's active cell. The app calls this once
    /// per frame while shared.
    pub fn collab_tick(&mut self) {
        let Some(events) = self.collab.as_mut().map(|c| c.shared.tick()) else { return };
        for event in events {
            self.collab_apply(event);
        }
        let cursor = self.collab_bound().and_then(|i| self.docs.get(i)).and_then(|d| {
            let sheet = d.wb.sheets.get(d.wb.active_sheet)?;
            Some(json!({"sheet": sheet.id, "cell": d.selection.active.a1()}).to_string())
        });
        if let Some(c) = self.collab.as_mut() {
            c.shared.share_cursor(cursor);
        }
    }

    fn collab_apply(&mut self, event: craft_collab::Inbound) {
        match event {
            craft_collab::Inbound::Text { update, .. } => {
                let applied = self.collab.as_mut().map(|c| c.shared.texts_mut().apply_doc_update(&update));
                if matches!(applied, Some(Ok(()))) {
                    self.collab_follow_register();
                }
            }
            // Workbooks travel as cells, never as replayed commands.
            craft_collab::Inbound::Command { .. } => {
                if let Some(c) = self.collab.as_mut() {
                    c.shared.note_refused();
                }
            }
            craft_collab::Inbound::SnapshotWanted => {
                let data = self.collab_bound().and_then(|i| self.docs.get(i)).and_then(|d| serde_json::to_vec(&*d.wb).ok());
                if let (Some(data), Some(c)) = (data, self.collab.as_mut()) {
                    c.shared.answer_snapshot(data);
                }
            }
            craft_collab::Inbound::Snapshot { data, crdt } => {
                let joining = self.collab_session().is_some_and(craft_collab::CollabSession::awaiting_snapshot);
                let Ok(wb) = serde_json::from_slice::<Workbook>(&data) else { return };
                if !joining {
                    return;
                }
                if let Some(i) = self.collab_bound()
                    && let Some(d) = self.docs.get_mut(i)
                {
                    let mut wb = wb;
                    d.calc = gridcraft_calc::Calc::new();
                    d.calc.recalc_all(&mut wb);
                    d.wb = Arc::new(wb);
                    d.undo.clear();
                    d.redo.clear();
                    d.revision += 1;
                    d.selection = crate::Selection::at(CellRef::default());
                }
                if let Some(c) = self.collab.as_mut() {
                    c.shared.snapshot_loaded(&crdt);
                    // The snapshot is the register's state: nothing to re-apply.
                    c.shared.texts_mut().register_changed();
                }
            }
        }
    }

    /// Brings the shared workbook to what the register says for every key that changed (a peer's
    /// edit, an undo): sheets first, then each cell's input and style.
    fn collab_follow_register(&mut self) {
        let Some(c) = self.collab.as_mut() else { return };
        let keys = c.shared.texts_mut().register_changed();
        if keys.is_empty() {
            return;
        }
        let values: Vec<(Entry, Option<String>)> =
            keys.iter().filter_map(|k| parse_key(k).map(|e| (e, c.shared.texts().register_get(k)))).collect();
        let Some(i) = self.collab_bound() else { return };
        let Some(d) = self.docs.get_mut(i) else { return };
        let _ = crate::cmd::commit(d, |cx| {
            // New sheets, ordered by id after the ones already there (every replica the same).
            for (entry, value) in &values {
                if let (Entry::Sheet(id), Some(name)) = (entry, value)
                    && !cx.wb.sheets.iter().any(|s| s.id == *id)
                {
                    let mut sheet = Sheet::new(name.clone());
                    sheet.id = *id;
                    cx.wb.sheets.push(Arc::new(sheet));
                }
            }
            order_shared_sheets(&mut cx.wb);
            for (entry, value) in &values {
                match entry {
                    Entry::Sheet(_) => {}
                    Entry::Input(id, at) => {
                        let Some(si) = cx.wb.sheets.iter().position(|s| s.id == *id) else { continue };
                        let style = cx.wb.sheets.get(si).and_then(|s| s.cells.get(*at)).map(|cell| cell.style);
                        let cell = match value {
                            Some(input) => crate::cmd::edit::input_to_cell(input, si, *at, &mut cx.wb).ok().flatten(),
                            None => None,
                        };
                        let sheet = cx.sheet_mut(si)?;
                        match (cell, style) {
                            (Some(mut cell), Some(style)) => {
                                cell.style = style;
                                sheet.set_cell(*at, cell);
                            }
                            (Some(cell), None) => sheet.set_cell(*at, cell),
                            (None, Some(style)) => sheet.set_cell(*at, gridcraft_model::Cell { style, ..Default::default() }),
                            (None, None) => {
                                sheet.remove_cell(*at);
                            }
                        }
                        cx.touch(si, *at);
                    }
                    Entry::Style(id, at) => {
                        let Some(si) = cx.wb.sheets.iter().position(|s| s.id == *id) else { continue };
                        let style = match value.as_deref().and_then(|v| serde_json::from_str(v).ok()) {
                            Some(style) => cx.wb.styles.intern(style),
                            None => gridcraft_model::StyleId::DEFAULT,
                        };
                        cx.sheet_mut(si)?.set_style(*at, style);
                        cx.touch(si, *at);
                    }
                }
            }
            Ok(())
        });
        if let Some(d) = self.docs.get_mut(i) {
            d.revision += 1;
        }
    }

    /// Undoes this author's most recent shared edit: true when there was one.
    pub(crate) fn collab_undo(&mut self) -> Result<bool> {
        let undone = self.collab.as_mut().and_then(|c| c.shared.undo_local());
        if undone.is_none() {
            return Ok(false);
        }
        self.collab_follow_register();
        Ok(true)
    }

    /// The peers to draw on the shared workbook: name, colour, and their active cell on a sheet
    /// (by index).
    pub fn collab_peers(&self) -> Vec<(String, [u8; 3], Option<(usize, CellRef)>)> {
        let (Some(c), Some(d)) = (self.collab.as_ref(), self.collab_bound().and_then(|i| self.docs.get(i))) else { return Vec::new() };
        c.shared
            .peers()
            .map(|p| {
                let at = p.position.as_deref().and_then(|s| serde_json::from_str::<Json>(s).ok()).and_then(|v| {
                    let id = v.get("sheet")?.as_u64()?;
                    let cell = CellRef::parse(v.get("cell")?.as_str()?)?;
                    Some((d.wb.sheets.iter().position(|s| s.id == id)?, cell))
                });
                (p.name.clone(), p.color, at)
            })
            .collect()
    }
}

/// The order every replica gives the sheets: the ones the workbook had when first shared (tag 0)
/// as they were, then the ones added while shared by id; two under one name keep the lower id's
/// name. The active sheet stays the same sheet.
fn order_shared_sheets(wb: &mut Workbook) {
    let active = wb.sheets.get(wb.active_sheet).map(|s| s.id);
    let first_new = wb.sheets.iter().position(|s| s.id >> 32 != 0).unwrap_or(wb.sheets.len());
    if let Some(tail) = wb.sheets.get_mut(first_new..) {
        tail.sort_by_key(|s| s.id);
    }
    if let Some(active) = active {
        wb.active_sheet = wb.sheets.iter().position(|s| s.id == active).unwrap_or(0);
    }
    unique_sheet_names(wb);
}

/// Two sheets under one name: the later id is renamed, the same on every replica.
fn unique_sheet_names(wb: &mut Workbook) {
    let mut by_id: Vec<(u64, usize)> = wb.sheets.iter().enumerate().map(|(i, s)| (s.id, i)).collect();
    by_id.sort_unstable();
    let mut taken: BTreeMap<String, u64> = BTreeMap::new();
    for (id, i) in by_id {
        let Some(name) = wb.sheets.get(i).map(|s| s.name.clone()) else { continue };
        let mut candidate = name.clone();
        let mut n = 2;
        while taken.get(&candidate.to_lowercase()).is_some_and(|other| *other != id) {
            candidate = format!("{name} ({n})");
            n += 1;
        }
        taken.insert(candidate.to_lowercase(), id);
        if candidate != name
            && let Some(sheet) = wb.sheets.get_mut(i)
        {
            Arc::make_mut(sheet).name = candidate;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_collab::{CollabSession, MemoryTransport};

    const A: &str = "aaaaaaaa11111111";
    const B: &str = "bbbbbbbb22222222";

    /// A shares a workbook holding `seed`, B joins with a blank one and catches up.
    fn two_authors(seed: &[(&str, &str)]) -> (Session, Session) {
        let (ta, tb) = MemoryTransport::pair(A, B);
        let mut a = Session::new();
        a.new_workbook();
        for (cell, input) in seed {
            a.execute("cell.set", json!({"cell": cell, "input": input})).unwrap();
        }
        a.collab_install(CollabSession::host("book", "Ann", Box::new(ta), &[])).unwrap();
        let mut b = Session::new();
        b.new_workbook();
        b.collab_install(CollabSession::join("book", "Bob", Box::new(tb))).unwrap();
        settle(&mut a, &mut b);
        (a, b)
    }

    fn settle(a: &mut Session, b: &mut Session) {
        for _ in 0..4 {
            b.collab_tick();
            a.collab_tick();
        }
    }

    fn shown(s: &Session, sheet: usize, cell: &str) -> String {
        let d = s.doc().unwrap();
        d.wb.sheets[sheet].value(CellRef::parse(cell).unwrap()).display()
    }

    fn input(s: &Session, cell: &str) -> String {
        s.doc().unwrap().wb.sheets[0].input_text(CellRef::parse(cell).unwrap())
    }

    #[test]
    fn a_joiner_receives_the_workbook_and_both_edit_it() {
        let (mut a, mut b) = two_authors(&[("A1", "5")]);
        assert_eq!(shown(&b, 0, "A1"), "5", "the snapshot arrived");
        b.execute("cell.set", json!({"cell": "B1", "input": "=A1*2"})).unwrap();
        a.execute("cell.set", json!({"cell": "A2", "input": "7"})).unwrap();
        settle(&mut a, &mut b);
        for s in [&a, &b] {
            assert_eq!(shown(s, 0, "B1"), "10", "the formula recalculates on each replica");
            assert_eq!(shown(s, 0, "A2"), "7");
        }
        a.execute("cell.set", json!({"cell": "A1", "input": "6"})).unwrap();
        settle(&mut a, &mut b);
        assert_eq!(shown(&b, 0, "B1"), "12", "a precedent's change recalculates the peer's formula");
    }

    #[test]
    fn two_authors_writing_one_cell_end_with_the_same_value() {
        let (mut a, mut b) = two_authors(&[]);
        a.execute("cell.set", json!({"cell": "C3", "input": "ann"})).unwrap();
        b.execute("cell.set", json!({"cell": "C3", "input": "bob"})).unwrap();
        settle(&mut a, &mut b);
        assert_eq!(input(&a, "C3"), input(&b, "C3"));
        assert!(["ann", "bob"].contains(&input(&a, "C3").as_str()));
    }

    #[test]
    fn formatting_travels_by_the_cells_it_touched() {
        let (mut a, mut b) = two_authors(&[("A1", "x"), ("A2", "y")]);
        b.execute("home.bold", json!({"range": "A1:A2", "on": true})).unwrap();
        settle(&mut a, &mut b);
        let bold = |s: &Session, c: &str| {
            let d = s.doc().unwrap();
            let sh = &d.wb.sheets[0];
            d.wb.styles.get(sh.style_id(CellRef::parse(c).unwrap())).font.bold
        };
        assert!(bold(&a, "A1") && bold(&a, "A2"), "Bob's bold landed on Ann's side");
        assert_eq!(input(&a, "A1"), "x", "and the values stayed");
    }

    #[test]
    fn undo_takes_back_only_this_authors_edit_and_returns_to_the_shared_value() {
        let (mut a, mut b) = two_authors(&[("A1", "seeded")]);
        std::thread::sleep(std::time::Duration::from_millis(600));
        a.execute("cell.set", json!({"cell": "A1", "input": "ann's"})).unwrap();
        settle(&mut a, &mut b);
        std::thread::sleep(std::time::Duration::from_millis(600));
        b.execute("cell.set", json!({"cell": "B1", "input": "bob's"})).unwrap();
        settle(&mut a, &mut b);
        let r = a.execute("edit.undo", json!({})).unwrap();
        assert_eq!(r["undone"], true);
        settle(&mut a, &mut b);
        for s in [&a, &b] {
            assert_eq!(input(s, "A1"), "seeded", "Ann's edit is undone back to what was shared");
            assert_eq!(input(s, "B1"), "bob's", "Bob's edit stays");
        }
        assert!(a.execute("edit.redo", json!({})).is_err(), "redo isn't shared");
    }

    #[test]
    fn moving_cells_is_refused_and_leaves_the_workbook_as_it_was() {
        let (mut a, mut b) = two_authors(&[("A1", "1"), ("A2", "2")]);
        a.execute("selection.set", json!({"range": "A1"})).ok();
        let err = a.execute("home.insertRows", json!({"range": "A1"})).unwrap_err();
        assert!(err.to_string().contains("isn't shared"), "{err}");
        assert_eq!(input(&a, "A1"), "1", "nothing moved");
        settle(&mut a, &mut b);
        assert_eq!(input(&b, "A1"), "1");
        assert!(a.execute("home.deleteSheet", json!({})).is_err(), "removing a sheet is refused too");
    }

    #[test]
    fn sheets_added_at_once_get_distinct_ids_and_names_the_same_everywhere() {
        let (mut a, mut b) = two_authors(&[]);
        a.execute("home.insertSheet", json!({"name": "Plan"})).unwrap();
        b.execute("home.insertSheet", json!({"name": "Plan"})).unwrap();
        settle(&mut a, &mut b);
        let names = |s: &Session| s.doc().unwrap().wb.sheets.iter().map(|sh| (sh.id, sh.name.clone())).collect::<Vec<_>>();
        assert_eq!(names(&a), names(&b), "same sheets, same order, same names");
        assert_eq!(names(&a).len(), 3);
        let ids: BTreeSet<u64> = names(&a).iter().map(|(id, _)| *id).collect();
        assert_eq!(ids.len(), 3, "distinct ids");
        assert!(names(&a).iter().any(|(_, n)| n == "Plan (2)"), "{:?}", names(&a));
        assert!(ids.iter().all(|id| *id < 1 << 53), "ids fit a JSON number");

        // A cell on a sheet a peer added travels by that sheet's id.
        let plan = names(&a).iter().position(|(_, n)| n == "Plan").unwrap();
        a.execute("cell.set", json!({"sheet": plan, "cell": "A1", "input": "on plan"})).unwrap();
        settle(&mut a, &mut b);
        assert_eq!(shown(&b, plan, "A1"), "on plan");
    }

    #[test]
    fn a_peer_sees_this_authors_active_cell() {
        let (mut a, mut b) = two_authors(&[]);
        a.execute("selection.set", json!({"range": "C4"})).ok();
        settle(&mut a, &mut b);
        let peers = b.collab_peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].0, "Ann");
        assert_eq!(peers[0].2, Some((0, CellRef::parse("C4").unwrap())));
    }

    #[test]
    fn keys_round_trip_and_junk_keys_are_ignored() {
        let c = CellRef::parse("AB12").unwrap();
        assert!(matches!(parse_key(&input_key(7, c)), Some(Entry::Input(7, r)) if r == c));
        assert!(matches!(parse_key(&style_key(7, c)), Some(Entry::Style(7, r)) if r == c));
        assert!(matches!(parse_key(&sheet_key(9)), Some(Entry::Sheet(9))));
        for junk in ["", "x/1", "c/notanid/A1", "c/1/notacell", "s/1/extra"] {
            assert!(parse_key(junk).is_none(), "{junk}");
        }
    }
}
