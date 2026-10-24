use jackstay_store::clipman::{ClipmanError, texts};
use jackstay_store::invariants::{self, glib_escape};
use proptest::prelude::*;

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|&i| i.to_owned()).collect()
}

#[test]
fn reads_the_list_oldest_first_with_glib_escapes() {
    let file = b"[texts]\ntexts=oldest;a\\;b;two\\nlines;\\stab\\there;back\\\\slash;newest;\n";
    assert_eq!(
        texts(file),
        Ok(strings(&["oldest", "a;b", "two\nlines", " tab\there", "back\\slash", "newest"]))
    );
    assert_eq!(texts(b"[texts]\ntexts=\\r"), Ok(strings(&["\r"])));
    assert_eq!(
        texts(b"[texts]\ntexts=raw\ttab;  inner  space"),
        Ok(strings(&["raw\ttab", "  inner  space"]))
    );
}

#[test]
fn empty_items_are_left_out_as_clipman_skips_them() {
    assert_eq!(texts(b"[texts]\ntexts=a;;b;"), Ok(strings(&["a", "b"])));
    assert_eq!(texts(b"[texts]\ntexts=;"), Ok(strings(&[])));
}

#[test]
fn only_the_texts_key_of_a_texts_group_counts_and_the_last_wins() {
    let file = b"# saved by clipman\n[images]\ntexts=no\n\n[texts]\nother=no\ntexts[de]=no\n  texts = first\n[after]\ntexts=no\n[texts]\ntexts=last;\n";
    assert_eq!(texts(file), Ok(strings(&["last"])));
    assert_eq!(texts(b""), Ok(strings(&[])));
    assert_eq!(texts(b"[texts]\n"), Ok(strings(&[])));
    assert_eq!(texts(b"[texts]\ntexts=\n"), Ok(strings(&[])));
    assert_eq!(texts(b"[texts] \t\ntexts=x\n"), Ok(strings(&["x"])));
    assert_eq!(texts(b"[texts]\ntexts=\x0b\x0c x\n"), Ok(strings(&["x"])), "GLib's spaces");
    assert_eq!(texts(b"[texts]\ntexts[sr@latin]=no\ntexts=x"), Ok(strings(&["x"])));
    assert_eq!(
        texts(b"[texts]\n a b =spaces inside a key are allowed\ntexts=x"),
        Ok(strings(&["x"]))
    );
}

#[test]
fn lines_end_as_glib_ends_them() {
    assert_eq!(texts(b"[texts]\r\ntexts=a;b\r\n"), Ok(strings(&["a", "b"])));
    assert_eq!(texts(b"[texts]\r\ntexts=a;b\r"), Ok(strings(&["a", "b\r"])), "no newline after it");
    assert_eq!(texts(b"[texts]\ntexts=a\0b;c\nx=1\n"), Ok(strings(&["a"])), "a NUL ends the line");
}

#[test]
fn refuses_what_glib_refuses() {
    let refused: [&[u8]; 13] = [
        b"junk\n[texts]\ntexts=a;\n",
        b"texts=a\n[texts]\ntexts=b\n",
        b"[texts]\n=x\n",
        b"[texts]\na[b=x\n",
        b"[texts]\na]=x\n",
        b"[texts]\na[b c]=x\n",
        b"[texts]\x0b\ntexts=a\n",
        b"[]\ntexts=a\n",
        b"[te[xts]\ntexts=a\n",
        b"[te\x01xts]\ntexts=a\n",
        b"[first]\nEncoding=ISO-8859-1\n[texts]\ntexts=a\n",
        b"[texts]\n[x\n",
        b"[texts]\n[de]=x\n",
    ];
    for file in refused {
        assert_eq!(
            texts(file),
            Err(ClipmanError::NotAKeyFile),
            "{}",
            String::from_utf8_lossy(file)
        );
    }
    assert_eq!(texts(b"[first]\nEncoding=UTF-8\n[texts]\ntexts=a\n"), Ok(strings(&["a"])));
    assert_eq!(
        texts(b"[first]\n[texts]\nEncoding=x\ntexts=a\n"),
        Ok(strings(&["a"])),
        "not the first group"
    );
    assert_eq!(texts(b"[texts]\ntexts=a\\x"), Err(ClipmanError::BadEscape));
    assert_eq!(texts(b"[texts]\ntexts=a\\"), Err(ClipmanError::BadEscape));
    assert_eq!(texts(b"[texts]\ntexts=\xff"), Err(ClipmanError::NotUtf8));
    assert_eq!(ClipmanError::BadEscape.to_string(), "clipman's texts hold a bad escape");
    assert_eq!(ClipmanError::NotUtf8.to_string(), "clipman's texts are not UTF-8");
    assert_eq!(ClipmanError::NotAKeyFile.to_string(), "clipman's file is not a valid key file");
}

#[test]
fn glib_escapes_leading_spaces_and_tabs_and_writes_inner_ones_raw() {
    assert_eq!(glib_escape(" \t a\tb c"), "\\s\\t\\sa\tb c");
    assert_eq!(glib_escape("\n x"), "\\n\\sx", "a newline does not end the leading run");
    assert_eq!(glib_escape("\\ x;"), "\\\\ x\\;");
}

proptest! {
    #[test]
    fn what_clipman_writes_reads_back(items in proptest::collection::vec(".+", 0..6)) {
        // GLib's writer leaves these leading spaces raw, and its reader strips them again.
        prop_assume!(!items.first().is_some_and(|i| i.starts_with(['\u{b}', '\u{c}'])));
        // A NUL ends GLib's line.
        prop_assume!(!items.iter().any(|i| i.contains('\0')));
        let list: String = items.iter().flat_map(|i| [glib_escape(i), ";".to_owned()]).collect();
        let file = format!("[texts]\ntexts={list}\n");
        prop_assert_eq!(texts(file.as_bytes()), Ok(items));
    }

    #[test]
    fn any_bytes_hold_the_invariants(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
        let mut file = b"[texts]\ntexts=".to_vec();
        file.extend_from_slice(&bytes);
        prop_assert_eq!(invariants::clipman(&file).err(), None);
        prop_assert_eq!(invariants::clipman(&bytes).err(), None);
    }
}
