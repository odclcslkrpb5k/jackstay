use jackstayd::coexist::{
    input_rival, input_rival_named, is_clipman, panel_lists_clipman, same_display,
};

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

#[test]
fn synergy_and_its_successors_are_recognised_by_process_name() {
    assert_eq!(input_rival_named(b"synergys\n"), Some("synergys"));
    assert_eq!(input_rival_named(b"deskflow-core\n"), Some("deskflow-core"));
    assert_eq!(input_rival_named(b"input-leapc"), Some("input-leapc"));
    for not in [&b"synergy2\n"[..], b"jackstayd\n", b"barrierd\n", b"Synergy\n", b""] {
        assert_eq!(input_rival_named(not), None, "{}", String::from_utf8_lossy(not));
    }
}

#[test]
fn a_rival_is_found_among_processes() {
    let proc = std::env::temp_dir().join(format!("jackstay-proc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&proc);
    for (pid, comm) in [("1", "systemd\n"), ("42", "synergys\n"), ("self", "synergyc\n")] {
        std::fs::create_dir_all(proc.join(pid)).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(proc.join(pid).join("comm"), comm).unwrap_or_else(|e| panic!("{e}"));
    }
    assert_eq!(input_rival(&proc, None), Some("synergys"));
    // Its environment unreadable: it counts, whatever our display.
    assert_eq!(input_rival(&proc, Some(":0")), Some("synergys"));
    std::fs::remove_dir_all(proc.join("42")).unwrap_or_else(|e| panic!("{e}"));
    // Not a process: "self" is a link in the real /proc.
    assert_eq!(input_rival(&proc, None), None);
    let _ = std::fs::remove_dir_all(&proc);
}

#[test]
fn a_rival_on_another_display_is_none_of_ours() {
    let proc = std::env::temp_dir().join(format!("jackstay-proc-display-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&proc);
    let dir = proc.join("42");
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(dir.join("comm"), "deskflow-core\n").unwrap_or_else(|e| panic!("{e}"));
    let environ =
        |env: &[u8]| std::fs::write(dir.join("environ"), env).unwrap_or_else(|e| panic!("{e}"));
    environ(b"HOME=/home/u\0DISPLAY=:1\0SHELL=/bin/sh\0");
    assert_eq!(input_rival(&proc, Some(":0")), None);
    assert_eq!(input_rival(&proc, Some(":1.0")), Some("deskflow-core"));
    // Without our display, or without its own, it counts.
    assert_eq!(input_rival(&proc, None), Some("deskflow-core"));
    environ(b"HOME=/home/u\0XDISPLAY=:1\0");
    assert_eq!(input_rival(&proc, Some(":0")), Some("deskflow-core"));
    let _ = std::fs::remove_dir_all(&proc);
}

#[test]
fn displays_are_compared_without_their_screen() {
    assert!(same_display(":0", ":0.0"));
    assert!(same_display("unix:0", ":0.1"));
    assert!(same_display("host:10.0", "host:10"));
    assert!(!same_display(":0", ":1"));
    assert!(!same_display(":1", ":10"));
    assert!(!same_display("host:0", ":0"));
    assert!(!same_display("", ":0"));
}
