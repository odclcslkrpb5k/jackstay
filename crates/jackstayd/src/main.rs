//! `jackstayd`: Jackstay's clipboard manager (docs/daemon.md).

fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    std::process::exit(jackstayd::daemon::run(&args));
}
