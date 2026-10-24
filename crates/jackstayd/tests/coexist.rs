use jackstayd::coexist::{is_clipman, panel_lists_clipman};

#[test]
fn clipman_is_recognised_as_a_process_or_a_panel_plugin() {
    assert!(is_clipman(b"xfce4-clipman\n", b"xfce4-clipman\0"));
    let wrapper = b"/usr/lib/x86_64-linux-gnu/xfce4/panel/wrapper-2.0\0/usr/lib/x86_64-linux-gnu/xfce4/panel/plugins/libclipman.so\x0012\x0025165831\x00xfce4-clipman-plugin\0Clipboard Manager\0";
    assert!(is_clipman(b"wrapper-2.0\n", wrapper));
    assert!(is_clipman(b"panel-12-clipma\n", b"x\0libclipman.so\0"));
    assert!(!is_clipman(b"xfce4-clipman-x\n", b"xfce4-clipman-settings\0"));
    assert!(!is_clipman(b"wrapper-2.0\n", b"/usr/lib/xfce4/panel/plugins/libclock.so\0clock\0"));
    assert!(!is_clipman(b"jackstayd\n", b"jackstayd\0--state-dir\0/home/u/libclipman.so.d\0"));
}

#[test]
fn the_panel_listing_is_read_by_plugin_id() {
    let with = "/plugins/clock-format     %R\n/plugins/plugin-1         applicationsmenu\n/plugins/plugin-12        xfce4-clipman-plugin\n";
    assert!(panel_lists_clipman(with));
    assert!(panel_lists_clipman("/plugins/plugin-3 clipman\n"));
    let without = "/plugins/plugin-1         applicationsmenu\n/plugins/plugin-12/popup-size  xfce4-clipman-plugin\n/plugins/notes  xfce4-clipman-plugin\n";
    assert!(!panel_lists_clipman(without));
    assert!(!panel_lists_clipman(""));
    // Xubuntu 26.04 with clipman as a stand-alone tray app (the shape seen live; the lists made
    // up): its settings and its tray icon's name appear, but no plugin is clipman.
    let tray = "/plugins/clipman/settings/save-on-quit               true\n\
                /plugins/plugin-5/known-legacy-items                 [notes,clipman,thunar]\n\
                /plugins/plugin-5/names-ordered                      [clipman,task manager]\n\
                /plugins/plugin-5                                    systray\n";
    assert!(!panel_lists_clipman(tray));
}
