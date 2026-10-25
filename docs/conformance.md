# The conformance matrix

Real clipboard clients copying and pasting through the real daemon on Xvfb (plan §5.7), in two
tiers: the core tier (M1), each toolkit's clipboard code driven directly, and the application
tier (M2), the owner's applications driven as a person drives them.

`crates/jackstayd/tests/conformance.rs` (the core tier, about 25 s) and
`crates/jackstayd/tests/applications.rs` (the application tier, about 2.5 min), run by `./check` as
ordinary tests. The oracle both share is `tests/conformance/matrix.rs`.

## Clients

| Client | Version (dev image) | As |
|---|---|---|
| xclip | 0.13 | source (one target, as given) and sink (any target, raw) |
| xsel | 1.2.1 | source and sink, text only, up to 1 MiB |
| GTK 3 | 3.24.52 | `clip-gtk3`: source and sink |
| GTK 4 | 4.22.4 | `clip-gtk4`: source and sink |
| Qt 6 | 6.10.2 | `clip-qt6`: source and sink |
| xterm | 407 | source (a triple click on CLIPBOARD) and sink (pasting into `cat`) |

The toolkit clients (`crates/jackstayd/tests/conformance/clients/`) are a page of C or C++ each,
built by the test with `pkg-config` into Cargo's test directory. They do nothing of their own, so
what the daemon meets is the toolkit's clipboard code. A copy offers either raw targets or the
toolkit's own `text`, `image` and `uris` APIs (`gtk_selection_data_set_text`/`_set_pixbuf`/
`_set_uris`; GDK content providers for a string, a texture and a file list;
`QMimeData::setText`/`setImageData`/`setUrls`). The APIs are what applications use, and they are
where conversions happen: STRING and COMPOUND_TEXT from UTF-8, PNG, JPEG and BMP re-encoded from
a pixbuf. A copying client quits when its stdin closes, through its toolkit's own exit, which is
where GTK 3 and Qt hand their targets to the clipboard manager.

Xvfb is 21.1.22 with BIG-REQUESTS, so a single property can hold nearly 16 MiB.

## What each row checks

A row is one source offering one payload. The oracle restates docs/selection.md rather than
calling the engine:

1. **While the source lives,** every sink pastes what the source itself serves (read with
   xclip). The daemon's capture finishes first: xclip serves one INCR transfer at a time, so a
   paste during a large capture fails for a reason that is not the daemon's.
2. **The daemon keeps** exactly the canonical target of each class offered ("Capture", step 4),
   less any the source refuses: the entry's size equals the sum of those targets' sizes.
3. **After the source quits** its own way (or, for xclip, xsel and xterm, is killed), every
   sink pastes the same bytes from the daemon, and every target the daemon offers pastes as it
   must: kept targets as kept; targets saved through `SAVE_TARGETS` as the source served them;
   text aliases by "Serving"'s rules (`STRING` only for Latin-1 text, `text/plain` only for ASCII).
   `COMPOUND_TEXT` and `TEXT` are decoded by GTK and Xlib, so their encoding is judged by an
   implementation that is not ours. Nothing else may be offered. A toolkit that takes 5 s or more
   to quit fails the row (it waited on the manager).

Payloads: small UTF-8 text with characters outside Latin-1; 20 MiB of it, past the 16 MiB a
property holds, so it crosses as INCR both ways; HTML beside text; PNGs of 16×16, 600×600 and
2600×2600 noise (stored, so their size follows the pixel count; written by the test, no encoder
dependency); URI lists.

| Source | Rows |
|---|---|
| xclip | small and large text, HTML, small and large PNG, URI list, each byte-identical |
| xsel | small and 1 MiB text |
| GTK 3, GTK 4, Qt 6 | text API small and large, text with HTML, image API small and medium, URI API, raw small and large text, raw large PNG |
| xterm | a selected line with characters outside Latin-1 |

Besides the rows:

- **A copy made just before quitting** (GTK 3, Qt 6): the client copies and exits at once, so
  only its `SAVE_TARGETS` handoff can save the copy. It must be kept, and the exit must not wait.
- **A target over `capture.per_target`** is not kept: served while its owner lives, gone with it,
  and the rest of the entry kept.
- **xterm pasting** what the daemon kept, through its UTF-8 path into its tty.

## Application tier

The owner's applications at the owner's versions (docs/dependencies.md), each in the image:

| Application | Version | Driven by |
|---|---|---|
| Firefox | 156.0 | keys and its context menu (xdotool), pages served by the test |
| Chrome for Testing | 155.0.8059.39 | the same |
| VS Code (Electron) | 1.137.0 | keys in its editor; what it pasted is saved and read back |
| LibreOffice Writer | 26.2.6.3, GTK 3 plugin (Xubuntu's) | UNO (`tests/conformance/office.py`): `.uno:Copy`, `.uno:Paste` |
| xfce4-terminal | 1.1.5 (VTE) | a triple click and Ctrl+Shift+C; pasting into `cat` |

**As a source,** each copy is a row as above (the core tier's clients are the sinks while the
application lives and after it quits), started afresh and quit its own way: Ctrl+Q (Firefox,
VS Code), closing the window (Chrome), LibreOffice's own exit through UNO (as Don't Save
answers it), the shell ending (xfce4-terminal). It has quit once its process has exited or the
daemon serves in its place, whichever comes first: under load Firefox's process can outlive its
windows by more than 30 s. An application's quit may take up to 9 s: the
daemon's 4 s `SAVE_TARGETS` budget and the application's own exit, and less than GTK 3's 10 s
wait on the manager (Chrome and VS Code wait on their own terms).

A source that renders a target anew on each request (LibreOffice, below) gives the test bytes the
daemon cannot have: for those targets the daemon's bytes must not be empty, must never change,
and must make up, with the rest, the size the live capture kept before the source quit.

| Application | Rows |
|---|---|
| Firefox, Chrome | a selection with markup; 20 MiB of selected text; Copy Image of a small and a 2600×2600 image; Copy Link |
| VS Code | selected code; 1 MiB selected |
| LibreOffice | Writer text; a small and a 2600×2600 image |
| xfce4-terminal | a selected line with characters outside Latin-1 |

Each row also checks what the application put there: the text selected, the link, the image's
size (from the PNG's header).

**As a sink,** each pastes what the daemon kept from a copy whose owner (xclip, or the GTK 3
client for text with HTML) has quit, and what it pasted is read back from the application:

| Application | Pastes | Read back |
|---|---|---|
| Firefox, Chrome | text, 20 MiB of text, text with HTML, a small and a 2600×2600 image | a paste event's data, sent to the test by the page: each type, and each image's pixels from a canvas, compared pixel for pixel |
| VS Code | text, 1 MiB of text | the file it saved |
| LibreOffice | text, a small and a 2600×2600 image | the document's text, and the pasted graphic's size in pixels |
| xfce4-terminal | text, 64 KiB of text | what `cat` read from its tty |

A key sent before a window, an editor or a menu is ready is lost, not queued: copies by key are
made again until something owns the clipboard, and pastes by key until they land (a browser's,
until its paste event begins, so that nothing is pasted twice).

The tests run one at a time. Each application holds inotify instances (VS Code about 10, a
browser 3), and `fs.inotify.max_user_instances` (128 on Ubuntu) counts every one the user holds,
in a container or on the desktop: a desktop can hold all but a dozen, and VS Code, starved,
may never open its window (the test then says so). Raising the limit, for instance to 1024 in
`/etc/sysctl.d/`, gives room to spare.

## What the applications do

Observed with the versions above. None is a fault of the daemon's, but each shapes what a
clipboard manager can promise.

- **LibreOffice renders some targets differently from one request to the next.** Its first
  export of a copy as RTF or HTML lacks styles that later exports have (the daemon, fetching
  first, keeps the first), its own document format (`application/x-openoffice-embed-source-xml`
  and `application/x-libreoffice-internal-id-<pid>`) differs on every export, and its `Link` is
  empty when `SAVE_TARGETS` asks for it at exit. These are checked against what the daemon
  serves, which must never change, rather than against what the test reads from LibreOffice.
- **LibreOffice ends `text/html`, `text/rtf`, `text/richtext` and `text/markdown` with a NUL**,
  and Qt drops it from `text/html` when it reads it (GTK does not). The matrix expects Qt's
  reading. Its Markdown, unlike its RTF and HTML, is the same on every request.
- **LibreOffice renders every format it offers when `SAVE_TARGETS` asks**: after copying a
  2600×2600 image its exit waits up to the daemon's 4 s budget (docs/selection.md,
  "CLIPBOARD_MANAGER", 3), against 0.2 to 2 s with no manager.
- **xfce4-terminal asks before pasting a line break** ("Potentially Unsafe Paste"); the tests
  answer Paste.
- **Copy Image:** Chrome offers `image/png` and HTML only; Firefox offers about twenty image
  types through GTK, and refuses `image/jpeg`. The daemon keeps the PNG from either.
- **Chrome takes minutes to parse 20 MiB of HTML;** the large-text page fetches its text instead.

## Found by the matrix in Jackstay

- **Text kept two or three times over.** Capture fetched `text/plain` and
  `text/plain;charset=utf-8` beside `UTF8_STRING`, since they are `text/*`. GTK 3 and Qt offer
  all three for every text, and the engine serves the first two from the third anyway. A 20 MiB
  copy cost up to 60 MiB of history. Fixed in capture (docs/selection.md, step 4).
- **`SAVE_TARGETS` rendering the same text again.** On quit, GTK 3 and Qt list every target,
  and the engine fetched the text's other names too. GTK 3 takes over 20 s to render 20 MiB of
  non-ASCII text as `text/plain`, while the application waits to exit. Once a text is kept, the
  text names served from it are no longer fetched (docs/selection.md, "CLIPBOARD_MANAGER", 2).

## What the toolkits cannot do

Observed on Xvfb with the versions above; the matrix works around each one, and none is
something a clipboard manager can fix. To be confirmed on the owner's desktop with real
applications.

- **GTK 4 cannot serve its own text past 256 KiB.** A string content provider larger than that
  gives every reader nothing, GTK 4 included. Raw bytes work up to just under 16 MiB.
- **GTK 4 cannot own 16 MiB or more** (the server's largest request). It never uses INCR as an
  owner, and the copying process dies.
- **GTK 4 misreads xclip's INCR transfers.** 1 MiB arrives as 1 byte, 2 MiB as 1 MiB less one;
  GTK 4 then gives up mid-transfer, which leaves xclip stuck for every reader after it. GTK 4
  reads the daemon's INCR transfers correctly.
- **GTK 4 asks for `text/plain;charset=utf-8` as `UTF8_STRING`**, and gets nothing from an owner
  that offers only the former. Only raw-target owners do that.
- **GTK 3 hangs rendering a large non-ASCII text as `text/plain`** (over 20 s for 20 MiB).
- **GTK 3 offers `text/ico`** among an image's targets and refuses it for an image too large for
  ICO (over 256×256). The engine fetches it as a `text/*` target; the refusal costs nothing.
- **xsel can fail allocating a large INCR transfer** ("malloc error: Cannot allocate memory",
  intermittently at 20 MiB), and leaves the owner stuck in it.
- **Qt renders `image/cur` and `image/ico` differently each time it is asked** (same length,
  different bytes; sometimes two renders agree). So a target saved through `SAVE_TARGETS`
  beside the canonical ones is compared by length, and must read back the same every time.
- **Qt folds `text/x-moz-url` into `text/uri-list`** and never shows it as a format, so a Qt
  application cannot ask for it by name.
