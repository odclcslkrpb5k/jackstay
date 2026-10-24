//! `jackstay`: the command line over `jackstayd`'s control socket (docs/daemon.md, "Control
//! socket").

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use jackstay_proto::control::{Error, Kinds, Request, Response, Summary, printable};
use jackstayd::client::{Client, ClientError};
use jackstayd::ui_socket;

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
  popup                      show the history popup at the pointer (for a hotkey; needs jackstay-ui)";

/// Exit statuses: 0 done, 1 refused by the daemon, 2 bad usage, 3 the daemon (or, for `popup`,
/// jackstay-ui) unreachable.
const REFUSED: u8 = 1;
const USAGE_ERROR: u8 = 2;
const UNREACHABLE: u8 = 3;

/// How many entries one page of `list --all` asks for.
const PAGE: u32 = 500;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err((code, message)) => {
            eprintln!("jackstay: {message}");
            // Run from a hotkey, nothing shows stderr.
            let skip = if args.first().is_some_and(|a| a == "--runtime-dir") { 2 } else { 0 };
            if args.get(skip).is_some_and(|a| a == "popup") {
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
        ClientError::Protocol(_) | ClientError::Version | ClientError::Refused => REFUSED,
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
    let mut popup = false;
    let request = match (command.as_str(), rest) {
        ("status", []) => Request::Status,
        // Not the daemon's: the UI's socket (docs/ui.md).
        ("popup", []) => {
            popup = true;
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
    if popup {
        return ui_socket::request_popup(&runtime, Duration::from_secs(5)).map_err(|e| {
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
    let response = client.ask(&request).map_err(|e| failed(&e))?;
    show(&request, response)
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
        (
            Request::Select { .. }
            | Request::Delete { .. }
            | Request::Pin { .. }
            | Request::Clear
            | Request::ClearHistory,
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
