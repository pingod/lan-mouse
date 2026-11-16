//! Visual arrangement of the screens lan-mouse can cross to.
//!
//! The daemon models a peer as sitting on exactly one edge of the local
//! screen (`Position`), so the editor mirrors that model: this screen in the
//! middle, the four edges around it, and one draggable tile per peer. Peers
//! that share an edge are drawn as separate tiles in the same cell, which is
//! also how `position_map` handles them (the first one in the list that owns
//! the edge takes the pointer).

use std::rc::Rc;

use gtk::{gdk, glib, prelude::*};
use lan_mouse_ipc::{ClientHandle, Position};

/// One peer that can be dragged around the local screen.
pub(super) struct Machine {
    pub handle: ClientHandle,
    pub hostname: String,
    pub position: Position,
    pub active: bool,
}

type OnMove = Rc<dyn Fn(ClientHandle, Position)>;

const CELL_WIDTH: i32 = 200;
const CELL_HEIGHT: i32 = 96;

/// Rebuild the whole grid. Cheap enough to run on every client update and
/// avoids keeping widget state in sync with the model by hand.
pub(super) fn rebuild(
    container: &gtk::Box,
    local_hostname: &str,
    machines: &[Machine],
    on_move: impl Fn(ClientHandle, Position) + 'static,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    let on_move: OnMove = Rc::new(on_move);
    let grid = gtk::Grid::builder()
        .column_spacing(12)
        .row_spacing(12)
        .halign(gtk::Align::Center)
        .build();

    for (column, row, edge) in [
        (0, 0, None),
        (1, 0, Some(Position::Top)),
        (2, 0, None),
        (0, 1, Some(Position::Left)),
        (1, 1, None),
        (2, 1, Some(Position::Right)),
        (0, 2, None),
        (1, 2, Some(Position::Bottom)),
        (2, 2, None),
    ] {
        let cell = match edge {
            Some(edge) => edge_cell(edge, machines, &on_move),
            None if column == 1 && row == 1 => local_cell(local_hostname),
            None => {
                /* keep the cross shape even though the corners are empty */
                let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
                spacer.set_size_request(CELL_WIDTH, CELL_HEIGHT);
                spacer
            }
        };
        grid.attach(&cell, column, row, 1, 1);
    }

    container.append(&grid);

    let hint = gtk::Label::builder()
        .label("Drag a machine onto an edge of this screen")
        .halign(gtk::Align::Center)
        .build();
    hint.add_css_class("dim-label");
    hint.add_css_class("caption");
    container.append(&hint);
}

fn edge_label(position: Position) -> &'static str {
    match position {
        Position::Left => "Left",
        Position::Right => "Right",
        Position::Top => "Top",
        Position::Bottom => "Bottom",
    }
}

fn edge_cell(position: Position, machines: &[Machine], on_move: &OnMove) -> gtk::Box {
    let cell = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .width_request(CELL_WIDTH)
        .height_request(CELL_HEIGHT)
        .build();
    cell.add_css_class("arrangement-cell");

    let title = gtk::Label::builder()
        .label(edge_label(position))
        .xalign(0.0)
        .build();
    title.add_css_class("dim-label");
    title.add_css_class("caption-heading");
    cell.append(&title);

    let tiles = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .vexpand(true)
        .valign(gtk::Align::Center)
        .build();

    let mut empty = true;
    for machine in machines
        .iter()
        .filter(|machine| machine.position == position)
    {
        tiles.append(&tile(machine));
        empty = false;
    }
    if empty {
        cell.add_css_class("empty");
        let placeholder = gtk::Label::builder().label("drop here").build();
        placeholder.add_css_class("dim-label");
        tiles.append(&placeholder);
    }
    cell.append(&tiles);

    /* Accept a tile dragged onto this edge and turn that into a
     * `FrontendRequest::UpdatePosition`, i.e. exactly the request the combo
     * box in the devices page sends. */
    let target = gtk::DropTarget::new(glib::Type::I64, gdk::DragAction::MOVE);
    let on_move = on_move.clone();
    target.connect_drop(move |_target, value, _x, _y| {
        let Ok(handle) = value.get::<i64>() else {
            return false;
        };
        on_move(handle as ClientHandle, position);
        true
    });
    let weak_cell = cell.downgrade();
    target.connect_enter(move |_target, _x, _y| {
        if let Some(cell) = weak_cell.upgrade() {
            cell.add_css_class("drop-hover");
        }
        gdk::DragAction::MOVE
    });
    let weak_cell = cell.downgrade();
    target.connect_leave(move |_target| {
        if let Some(cell) = weak_cell.upgrade() {
            cell.remove_css_class("drop-hover");
        }
    });
    cell.add_controller(target);
    cell
}

fn local_cell(hostname: &str) -> gtk::Box {
    let cell = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .width_request(CELL_WIDTH)
        .height_request(CELL_HEIGHT)
        .valign(gtk::Align::Fill)
        .build();
    cell.add_css_class("arrangement-local");

    let icon = gtk::Image::from_icon_name("computer-symbolic");
    icon.set_pixel_size(32);
    cell.append(&icon);

    let title = gtk::Label::new(Some("This screen"));
    title.add_css_class("heading");
    cell.append(&title);

    let name = gtk::Label::builder()
        .label(hostname)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .build();
    name.add_css_class("dim-label");
    name.add_css_class("caption");
    cell.append(&name);
    cell
}

fn tile(machine: &Machine) -> gtk::Box {
    let tile = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    tile.add_css_class("arrangement-tile");
    tile.set_cursor_from_name(Some("grab"));
    tile.set_tooltip_text(Some(&format!(
        "{} · {} edge, drag it to another edge",
        machine.hostname, machine.position
    )));

    let icon = gtk::Image::from_icon_name("video-display-symbolic");
    tile.append(&icon);

    let label = gtk::Label::builder()
        .label(&machine.hostname)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .xalign(0.0)
        .hexpand(true)
        .build();
    tile.append(&label);

    if machine.active {
        tile.add_css_class("active-peer");
    }

    let value = (machine.handle as i64).to_value();
    let source = gtk::DragSource::new();
    source.set_actions(gdk::DragAction::MOVE);
    source.connect_prepare(move |_source, _x, _y| Some(gdk::ContentProvider::for_value(&value)));
    tile.add_controller(source);
    tile
}
