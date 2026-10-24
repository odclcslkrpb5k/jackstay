//! The tray's menu as data, without a bus: the exact D-Bus types hosts expect, and what each item
//! does. A reply of the wrong type is dropped by `GLib`, and the panel shows no menu.

use jackstay_ui::tray::{Action, action, group_properties, layout};

#[test]
fn replies_have_the_types_dbusmenu_declares() {
    let full = layout(0);
    assert_eq!(full.type_().as_str(), "(u(ia{sv}av))");
    let root = full.child_value(1);
    assert_eq!(root.child_value(0).get::<i32>(), Some(0));
    let children = root.child_value(2);
    assert_eq!(children.n_children(), 4);
    let first = children.child_value(0).as_variant().unwrap_or_else(|| panic!("not boxed"));
    assert_eq!(first.type_().as_str(), "(ia{sv}av)", "each child boxed once");
    assert_eq!(first.child_value(0).get::<i32>(), Some(1));
    assert_eq!(layout(3).type_().as_str(), "(u(ia{sv}av))");
    assert_eq!(layout(3).child_value(1).child_value(2).n_children(), 0);
    let props = group_properties(&[1, 2]);
    assert_eq!(props.type_().as_str(), "(a(ia{sv}))");
    assert_eq!(props.child_value(0).n_children(), 2);
    assert_eq!(group_properties(&[]).child_value(0).n_children(), 4, "empty means all");
}

#[test]
fn each_item_does_what_it_says() {
    let labels = format!("{:?}", layout(0));
    for label in ["Show history", "Clear clipboard", "Clear history…", "separator", "submenu"] {
        assert!(labels.contains(label), "{label}");
    }
    assert_eq!(action(1), Some(Action::ShowHistory));
    assert_eq!(action(2), None, "the separator");
    assert_eq!(action(3), Some(Action::ClearClipboard));
    assert_eq!(action(4), Some(Action::ClearHistory));
    assert_eq!(action(0), None);
    assert_eq!(action(5), None);
}
