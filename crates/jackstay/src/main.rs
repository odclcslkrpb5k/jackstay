//! `jackstay`: the command line over `jackstayd`'s control socket (docs/daemon.md, "Control
//! socket").

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use jackstay_proto::control::{
    Dialer, Error, InputState, InputStatus, Kinds, LinkStatus, PeerState, Request, Response,
    Summary, printable,
};
use jackstayd::client::{Client, ClientError};
use jackstayd::ui_socket::{self, Ask};

const USAGE: &str = "\
usage: jackstay [--runtime-dir DIR] COMMAND
commands:
  status                     whether jackstayd runs, and what it holds
  list [--all]               the history, newest first (the first 20 without --all)
  select ID                  put an entry back on the clipboard
  delete ID                  delete an entry (and clear the clipboard if it is on it)
  pin ID | unpin ID          keep an entry through clears and limits, or not
  clear                      clear the clipboard, deleting what was on it unless pinned
  clear-history              delete every unpinned entry and clear the clipboard
  import-clipman [--remove]  import xfce4-clipman's history; --remove deletes its files after
  popup                      show the history popup at the pointer (for a hotkey; needs jackstay-ui)
  settings                   open the settings window (needs jackstay-ui)
  peers                      the link: this machine's fingerprint and each paired machine's state
  pair HOST                  pair with the machine at HOST (run it there too, naming this one)
  unpair NAME|FINGERPRINT    forget a paired machine
  switch [NAME|FINGERPRINT]  move the pointer to that neighbour, or back here (the primary only)";

/// Exit statuses: 0 done, 1 refused by the daemon, 2 bad usage, 3 the daemon (or, for `popup` and
/// `settings`, jackstay-ui) unreachable.
const REFUSED: u8 = 1;
const USAGE_ERROR: u8 = 2;
const UNREACHABLE: u8 = 3;

/// How many entries one page of `list --all` asks for.
const PAGE: u32 = 500;
/// Pairing waits up to two minutes for the other machine and the user (docs/link.md).
const PAIRING_WAIT: Duration = Duration::from_secs(150);

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err((code, message)) => {
            eprintln!("jackstay: {message}");
            // Run from a hotkey, nothing shows stderr.
            let skip = if args.first().is_some_and(|a| a == "--runtime-dir") { 2 } else { 0 };
            if args.get(skip).is_some_and(|a| a == "popup" || a == "settings") {
                notify(&message);
            }
            ExitCode::from(code)
        }
    }
}

/// A desktop notification, if there is a display and `notify-send` (Xubuntu's xfce4-notifyd
/// answers it); otherwise nothing.
fn notify(message: &str) {
    if std::env::var_os("DISPLAY").is_some() {
        let _ = Command::new("notify-send")
            .args(["--app-name=Jackstay", "Jackstay", message])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

type Failure = (u8, String);

fn usage(why: &str) -> Failure {
    (USAGE_ERROR, format!("{why}\n{USAGE}"))
}

fn id(arg: Option<&OsString>) -> Result<u64, Failure> {
    arg.and_then(|a| a.to_str())
        .and_then(|a| a.parse().ok())
        .ok_or_else(|| usage("an entry ID is needed"))
}

/// Exit status for a failed exchange: unreachable if the daemon could not be talked to at all,
/// refused if it answered with something this client cannot use.
fn exit_for(e: &ClientError) -> u8 {
    match e {
        ClientError::Connect(_) | ClientError::Io(_) | ClientError::Closed => UNREACHABLE,
        ClientError::Protocol(_)
        | ClientError::Version
        | ClientError::Refused
        | ClientError::Request(_) => REFUSED,
    }
}

fn failed(e: &ClientError) -> Failure {
    (exit_for(e), e.to_string())
}

fn run(args: &[OsString]) -> Result<(), Failure> {
    let mut args = args.to_vec();
    let mut runtime: Option<PathBuf> = None;
    if args.first().is_some_and(|a| a == "--runtime-dir") {
        let dir =
            args.get(1).map(PathBuf::from).ok_or_else(|| usage("--runtime-dir needs a value"))?;
        runtime = Some(dir);
        args.drain(..2);
    }
    let command =
        args.first().and_then(|a| a.to_str()).ok_or_else(|| usage("no command"))?.to_owned();
    let rest = args.get(1..).unwrap_or_default();
    let mut all = false;
    let mut window = None;
    let request = match (command.as_str(), rest) {
        ("status", []) => Request::Status,
        // Not the daemon's: the UI's socket (docs/ui.md).
        ("popup", []) => {
            window = Some(Ask::Popup);
            Request::Status
        }
        ("settings", []) => {
            window = Some(Ask::Settings);
            Request::Status
        }
        ("list", []) => Request::List { offset: 0, limit: 20 },
        ("list", [flag]) if flag == "--all" => {
            all = true;
            Request::List { offset: 0, limit: PAGE }
        }
        ("select", [_]) => Request::Select { id: id(rest.first())? },
        ("delete", [_]) => Request::Delete { id: id(rest.first())? },
        ("pin", [_]) => Request::Pin { id: id(rest.first())?, pinned: true },
        ("unpin", [_]) => Request::Pin { id: id(rest.first())?, pinned: false },
        ("clear", []) => Request::Clear,
        ("clear-history", []) => Request::ClearHistory,
        ("import-clipman", []) => Request::ImportClipman { remove: false },
        ("import-clipman", [flag]) if flag == "--remove" => Request::ImportClipman { remove: true },
        ("peers", []) => Request::Peers,
        ("pair", [host]) => Request::Pair { host: text(host)? },
        ("unpair", [peer]) => Request::Unpair { peer: text(peer)? },
        ("switch", [peer]) => Request::Switch { peer: text(peer)? },
        ("switch", []) => Request::SwitchHome,
        ("help" | "--help" | "-h", []) => {
            println!("{USAGE}");
            return Ok(());
        }
        _ => return Err(usage(&format!("unknown command or arguments: {command}"))),
    };
    let runtime = match runtime {
        Some(r) => r,
        None => std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .ok_or((UNREACHABLE, "XDG_RUNTIME_DIR is not set".to_owned()))?
            .join("jackstay"),
    };
    if let Some(ask) = window {
        return ui_socket::request(&runtime, ask, Duration::from_secs(5)).map_err(|e| {
            let why = if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) {
                "jackstay-ui is not running".to_owned()
            } else {
                format!("jackstay-ui did not answer: {e}")
            };
            (UNREACHABLE, why)
        });
    }
    let mut client = Client::connect(&runtime.join("control"), Duration::from_secs(30))
        .map_err(|e| failed(&e))?;
    if all {
        return list_all(&mut client);
    }
    if let Request::Pair { host } = &request {
        return pair(&mut client, host);
    }
    let response = client.ask(&request).map_err(|e| failed(&e))?;
    show(&request, response)?;
    if request == Request::Status {
        // The link's line too, for a status that says everything.
        if let Ok(Response::Peers(l)) = client.ask(&Request::Peers) {
            print_link(&l, false);
        }
        if let Ok(Response::Input(i)) = client.ask(&Request::Input) {
            print_input(&i);
        }
    }
    Ok(())
}

fn text(arg: &OsString) -> Result<String, Failure> {
    arg.to_str().map(str::to_owned).ok_or_else(|| usage("arguments must be UTF-8"))
}

/// This machine's hostname, for the other machine's user to name it by.
fn this_machine() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| printable(h.trim()))
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "THIS-MACHINE".to_owned())
}

/// Pairs: starts it, shows the code when it comes, asks, and waits for the end.
fn pair(client: &mut Client, host: &str) -> Result<(), Failure> {
    client.set_timeout(PAIRING_WAIT).map_err(|e| failed(&e))?;
    client.send(&Request::Pair { host: host.to_owned() }).map_err(|e| failed(&e))?;
    let mut started = false;
    loop {
        match client.receive().map_err(|e| failed(&e))? {
            // The first Ok: pairing has started, and nothing comes until the other machine starts.
            Response::Ok if !started => {
                started = true;
                println!(
                    "Waiting for {} to start pairing too: run `jackstay pair {}` there within two \
                     minutes…",
                    printable(host),
                    this_machine()
                );
            }
            Response::Ok => {}
            Response::PairCode { name, fingerprint, code } => {
                // Every field cleaned, though the daemon made them: what reaches the terminal never
                // depends on what was sent.
                let (fingerprint, code) = (printable(&fingerprint), printable(&code));
                println!(
                    "{} (fingerprint {fingerprint}) shows the code {} {}.",
                    printable(&name),
                    code.get(..3).unwrap_or(&code),
                    code.get(3..).unwrap_or_default()
                );
                print!("Does the other machine show the same code? [y/N] ");
                let _ = std::io::Write::flush(&mut std::io::stdout());
                let mut answer = String::new();
                // No terminal, or end of input: no.
                let _ = std::io::stdin().read_line(&mut answer);
                let yes = matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES");
                client.send(&Request::PairAnswer { yes }).map_err(|e| failed(&e))?;
                if yes {
                    println!("Waiting for the other machine to answer…");
                }
            }
            Response::Paired { name, fingerprint } => {
                println!("Paired with {} ({}).", printable(&name), printable(&fingerprint));
                return Ok(());
            }
            Response::PairFailed { reason, detail } => {
                let detail = if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {}", printable(&detail))
                };
                return Err((REFUSED, format!("pairing failed: {}{detail}", reason.explain())));
            }
            Response::Error(e) => return Err(refused(e)),
            _ => return Err((REFUSED, "jackstayd sent an unexpected response".to_owned())),
        }
    }
}

fn print_link(l: &LinkStatus, full: bool) {
    if let Some(why) = &l.off {
        println!("link: off ({})", printable(why));
        return;
    }
    match (l.port, &l.bind_error) {
        (Some(port), _) => {
            println!("link: UDP port {port}, this machine {}", printable(&l.fingerprint));
        }
        (None, Some(why)) => println!("link: port not open ({}), retrying", printable(why)),
        (None, None) => println!("link: starting"),
    }
    if l.pairing {
        println!("  pairing is running");
    }
    if l.peers.is_empty() {
        println!("  no paired machines (jackstay pair HOST)");
    }
    for p in &l.peers {
        let state = match p.state {
            PeerState::Connected => "connected",
            PeerState::Suspect => "connected, silent",
            PeerState::Dialing => "not connected",
            PeerState::RefusedKey => "refused: it does not know this machine",
            PeerState::RefusedVersion => "refused: it runs another version",
        };
        let rtt =
            p.rtt_us.map_or_else(String::new, |us| format!(", {}.{:03} ms", us / 1000, us % 1000));
        println!("  {} {state}{rtt}", printable(&p.name));
        if full {
            println!(
                "    fingerprint {}, address {}, sessions {}",
                printable(&p.fingerprint),
                printable(&p.address),
                p.connects
            );
            // Only the machine that dialed may change its address without the link dropping.
            match p.dialed {
                Some(Dialer::Us) => println!("    dialed by this machine"),
                Some(Dialer::Peer) => println!("    dialed by {}", printable(&p.name)),
                None => {}
            }
        }
    }
}

fn print_input(i: &InputStatus) {
    let peer = printable(&i.peer);
    let detail = printable(&i.detail);
    match i.state {
        InputState::Off => println!("input: off ({detail})"),
        InputState::Waiting => println!("input: waiting ({detail})"),
        InputState::Here => println!("input: the pointer is here"),
        InputState::On => println!("input: the pointer is on {peer}"),
        InputState::Locked => println!("input: the pointer is locked to {peer}"),
        InputState::Secondary => println!("input: {peer} can drive this machine"),
        InputState::Driven => println!("input: {peer} drives this machine now"),
    }
}

/// Entries are listed by offset, newest first: one copied or deleted while `--all` pages through
/// may shift the pages, so an entry can be shown twice or not at all.
fn list_all(client: &mut Client) -> Result<(), Failure> {
    let mut offset = 0u32;
    loop {
        let request = Request::List { offset, limit: PAGE };
        let response = client.ask(&request).map_err(|e| failed(&e))?;
        let Response::List { total, entries } = response else { return show(&request, response) };
        let count = u32::try_from(entries.len()).unwrap_or(u32::MAX);
        print_entries(&entries);
        offset = offset.saturating_add(count);
        if count == 0 || offset >= total {
            return Ok(());
        }
    }
}

fn refused(e: Error) -> Failure {
    (REFUSED, e.explain().to_owned())
}

fn show(request: &Request, response: Response) -> Result<(), Failure> {
    match (request, response) {
        (_, Response::Error(e)) => Err(refused(e)),
        (Request::Status, Response::Status(s)) => {
            println!("entries: {} ({} pinned)", s.entries, s.pinned);
            match s.current {
                Some(id) => println!("on the clipboard: entry {id}"),
                None if s.owns_clipboard => println!("on the clipboard: nothing (cleared)"),
                None => println!("on the clipboard: not known"),
            }
            if s.store_failing {
                println!("warning: the history on disk is not up to date (see the journal)");
            }
            Ok(())
        }
        (Request::List { .. }, Response::List { total, entries }) => {
            print_entries(&entries);
            let shown = entries.len();
            if usize::try_from(total).is_ok_and(|t| t > shown) {
                println!("({shown} of {total}; jackstay list --all for every entry)");
            }
            Ok(())
        }
        (Request::ImportClipman { remove }, Response::Imported { added, skipped }) => {
            println!("imported {added} entries from clipman ({skipped} skipped)");
            if !remove && added > 0 {
                println!(
                    "clipman's own files still hold its history: jackstay import-clipman --remove"
                );
            }
            Ok(())
        }
        (Request::Peers, Response::Peers(l)) => {
            print_link(&l, true);
            Ok(())
        }
        (
            Request::Select { .. }
            | Request::Delete { .. }
            | Request::Pin { .. }
            | Request::Clear
            | Request::ClearHistory
            | Request::Unpair { .. }
            | Request::Switch { .. }
            | Request::SwitchHome,
            Response::Ok,
        ) => Ok(()),
        _ => Err((REFUSED, "jackstayd sent an unexpected response".to_owned())),
    }
}

fn kinds(k: Kinds) -> String {
    let names = [
        (Kinds::TEXT, "text"),
        (Kinds::IMAGE, "image"),
        (Kinds::FILES, "files"),
        (Kinds::RICH_TEXT, "rich"),
        (Kinds::OTHER, "other"),
    ];
    let list: Vec<&str> = names.iter().filter(|(bit, _)| k.0 & bit != 0).map(|(_, n)| *n).collect();
    list.join(",")
}

fn ago(ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    let s = now.saturating_sub(ms) / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{} KiB", bytes / 1024),
        _ => format!("{} MiB", bytes / 1_048_576),
    }
}

fn print_entries(entries: &[Summary]) {
    for e in entries {
        let current = if e.current { "*" } else { " " };
        let pinned = if e.pinned { "P" } else { " " };
        // The daemon cleans both fields; they are cleaned again here, so what reaches the terminal
        // never depends on what was sent.
        let source = e.source.as_deref().map_or_else(|| "-".to_owned(), printable);
        println!(
            "{:>6} {current}{pinned} {:>4} {:>8} {:<11} {:<14} {}",
            e.id,
            ago(e.copied_at_ms),
            size(e.size),
            kinds(e.kinds),
            source,
            printable(&e.preview)
        );
    }
}
