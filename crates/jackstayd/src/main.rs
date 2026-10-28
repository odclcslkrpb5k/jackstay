//! `jackstayd`: Jackstay's clipboard manager (docs/daemon.md).

fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    // The daemon runs itself this way to decode an image, confined (crates/thumbnail).
    if args == ["--thumbnail"] {
        let status =
            jackstay_thumbnail::helper(&mut std::io::stdin().lock(), &mut std::io::stdout().lock());
        std::process::exit(status);
    }
    std::process::exit(jackstayd::daemon::run(&args));
}
