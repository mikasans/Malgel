//! Property tests ("fuzzing" without a nightly toolchain): random input for
//! the editing operations, file decoding and the terminal's escape-sequence
//! parser and selection. The quick run is part of `cargo test`; the long
//! run is
//!
//!     MALGEL_FUZZ_SECS=600 cargo test --release fuzz_soak -- --ignored --nocapture
//!
//! which keeps generating cases for each target until the time is up.

use std::ops::Range;

use proptest::{
    prelude::*,
    test_runner::{Config, TestRunner},
};

use crate::{
    document::{self, Encoding, LineEnding},
    format::{self, Block, CommentStyle, Inline, LineMove},
    smart_edit,
};

/// Text that exercises Markdown structure, multi-byte chars and edges.
fn text_strategy() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        Just("\n".to_string()),
        Just("\r\n".to_string()),
        Just("\t".to_string()),
        Just("  ".to_string()),
        Just("- ".to_string()),
        Just("1. ".to_string()),
        Just("- [ ] ".to_string()),
        Just("> ".to_string()),
        Just("# ".to_string()),
        Just("| a | b |\n|---|---|\n".to_string()),
        Just("**".to_string()),
        Just("`".to_string()),
        Just("```\n".to_string()),
        Just("[x](http://e.com)".to_string()),
        Just("// ".to_string()),
        Just("中文".to_string()),
        Just("🙂".to_string()),
        Just("e\u{301}".to_string()),
        "[a-z ]{0,6}",
        any::<char>().prop_map(|c| c.to_string()),
    ];
    proptest::collection::vec(piece, 0..40).prop_map(|v| v.concat())
}

fn floor_boundary(text: &str, mut ix: usize) -> usize {
    ix = ix.min(text.len());
    while !text.is_char_boundary(ix) {
        ix -= 1;
    }
    ix
}

/// A selection on char boundaries, either way round.
fn selection_in(text: &str, a: usize, b: usize) -> Range<usize> {
    let (a, b) = (floor_boundary(text, a), floor_boundary(text, b));
    a.min(b)..a.max(b)
}

/// An edit must replace a real range and leave a real selection.
fn check_edit(text: &str, edit: &format::Edit) -> Result<(), TestCaseError> {
    let r = &edit.range;
    prop_assert!(r.start <= r.end && r.end <= text.len(), "range {r:?}");
    prop_assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
    let mut out = String::with_capacity(text.len() + edit.text.len());
    out.push_str(&text[..r.start]);
    out.push_str(&edit.text);
    out.push_str(&text[r.end..]);
    let s = &edit.selection;
    prop_assert!(s.start <= s.end && s.end <= out.len(), "selection {s:?}");
    prop_assert!(out.is_char_boundary(s.start) && out.is_char_boundary(s.end));
    Ok(())
}

fn edit_ops(text: &str, a: usize, b: usize, op: u8) -> Result<(), TestCaseError> {
    let sel = selection_in(text, a, b);
    let caret = sel.start;
    let style = CommentStyle::Line("//");
    let edit = match op % 18 {
        0 => smart_edit::enter(text, sel),
        1 => smart_edit::indent(text, sel),
        2 => smart_edit::outdent(text, sel),
        3 => smart_edit::tidy_table(text, sel),
        4 => smart_edit::table_tab(text, sel, false),
        5 => smart_edit::table_tab(text, sel, true),
        6 => smart_edit::paste_link(text, sel, "https://example.com/中"),
        7 => Some(format::toggle_inline(text, sel, Inline::Bold)),
        8 => Some(format::toggle_inline(text, sel, Inline::Code)),
        9 => Some(format::insert_link(text, sel)),
        10 => Some(format::insert_code_block(text, sel)),
        11 => Some(format::toggle_block(text, sel, Block::Heading(2))),
        12 => Some(format::toggle_block(text, sel, Block::TaskList)),
        13 => Some(format::cut_whole_line(text, caret)),
        14 => Some(format::delete_lines(text, sel)),
        15 => Some(format::paste_line_above(text, caret, "pasted 行")),
        16 => format::toggle_line_comment(text, sel, style),
        _ => format::move_lines(
            text,
            sel,
            if op & 1 == 0 {
                LineMove::Up
            } else {
                LineMove::Down
            },
        ),
    };
    if let Some(edit) = edit {
        check_edit(text, &edit)?;
    }
    if let Some(next) = format::next_occurrence(text, selection_in(text, a, b)) {
        prop_assert!(next.end <= text.len() && text.is_char_boundary(next.start));
        prop_assert!(text.is_char_boundary(next.end));
    }
    let _ = format::whole_line(text, caret);
    let _ = format::format_document(text);
    let _ = smart_edit::image_markdown(text, caret, "img/a b.png", "alt");
    Ok(())
}

fn decode_bytes(bytes: Vec<u8>) -> Result<(), TestCaseError> {
    let _ = document::looks_binary(&bytes);
    if let Ok(loaded) = document::decode(bytes) {
        prop_assert!(!loaded.text.contains("\r\n"));
    }
    Ok(())
}

fn encode_round_trip(text: &str, crlf: bool, enc: u8) -> Result<(), TestCaseError> {
    // What the editor holds: `\n` endings, no byte-order mark at the start.
    let text = text.replace('\r', "");
    let text = text.trim_start_matches('\u{feff}');
    let line_ending = if crlf {
        LineEnding::CrLf
    } else {
        LineEnding::Lf
    };
    let encoding = match enc % 3 {
        0 => Encoding::Utf8,
        1 => Encoding::Utf16Le,
        _ => Encoding::Utf16Be,
    };
    let bytes = document::encode(text, line_ending, encoding);
    let back = document::decode(bytes).map_err(|e| TestCaseError::fail(e.to_string()))?;
    prop_assert_eq!(back.text.as_str(), text);
    if encoding != Encoding::Utf8 || !text.is_empty() {
        prop_assert_eq!(back.encoding, encoding);
    }
    if text.contains('\n') {
        prop_assert_eq!(back.line_ending, line_ending);
    }
    Ok(())
}

/// Bytes biased toward escape sequences: CSI / OSC / DCS fragments, mode
/// switches, UTF-8 pieces and plain text.
fn term_bytes() -> impl Strategy<Value = Vec<u8>> {
    let piece = prop_oneof![
        Just(b"\x1b[".to_vec()),
        Just(b"\x1b]".to_vec()),
        Just(b"\x1bP".to_vec()),
        Just(b"\x1b[?1049h".to_vec()),
        Just(b"\x1b[?1049l".to_vec()),
        Just(b"\x1b[?2004h".to_vec()),
        Just(b"\x1b[2J".to_vec()),
        Just(b"\x1b[3J".to_vec()),
        Just(b"\x1b[r".to_vec()),
        Just(b"\x1b[5;10r".to_vec()),
        Just(b"\x1b[999;999H".to_vec()),
        Just(b"\x1b[38;2;1;2;3m".to_vec()),
        Just(b"\x1b[4:3m".to_vec()),
        Just(b"\x07".to_vec()),
        Just(b"\r\n".to_vec()),
        Just(b"\n\n\n\n\n\n".to_vec()),
        Just("中文🙂e\u{301}".as_bytes().to_vec()),
        "[0-9;?]{0,8}".prop_map(|s| s.into_bytes()),
        "[a-zA-Z ]{0,30}".prop_map(|s| s.into_bytes()),
        proptest::collection::vec(any::<u8>(), 0..16),
    ];
    proptest::collection::vec(piece, 0..60).prop_map(|v| v.concat())
}

fn terminal_feed(
    bytes: Vec<u8>,
    cols: u16,
    rows: u16,
    picks: Vec<(usize, usize, u8)>,
) -> Result<(), TestCaseError> {
    crate::term_session::fuzz_terminal(&bytes, cols, rows, &picks);
    let payload = crate::term_session::paste_payload(&String::from_utf8_lossy(&bytes), true);
    // The end marker appears exactly once, at the end.
    let marker = b"\x1b[201~";
    let count = payload
        .windows(marker.len())
        .filter(|w| w == marker)
        .count();
    prop_assert_eq!(count, 1);
    prop_assert!(payload.ends_with(marker));
    Ok(())
}

fn picks() -> impl Strategy<Value = Vec<(usize, usize, u8)>> {
    proptest::collection::vec((0usize..200, 0usize..300, any::<u8>()), 0..12)
}

proptest! {
    #![proptest_config(Config { cases: 256, ..Config::default() })]

    #[test]
    fn edit_ops_keep_ranges_on_char_boundaries(
        text in text_strategy(), a in 0usize..400, b in 0usize..400, op in any::<u8>()
    ) {
        edit_ops(&text, a, b, op)?;
    }

    #[test]
    fn decoding_arbitrary_bytes_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
        decode_bytes(bytes)?;
    }

    #[test]
    fn utf16_bom_bytes_decode_or_fail_cleanly(
        bom in prop_oneof![Just(vec![0xFFu8, 0xFE]), Just(vec![0xFE, 0xFF])],
        body in proptest::collection::vec(any::<u8>(), 0..64)
    ) {
        decode_bytes([bom, body].concat())?;
    }

    #[test]
    fn encode_then_decode_round_trips(text in text_strategy(), crlf in any::<bool>(), enc in any::<u8>()) {
        encode_round_trip(&text, crlf, enc)?;
    }

    #[test]
    fn terminal_parser_and_selection_never_panic(
        bytes in term_bytes(), cols in 1u16..120, rows in 1u16..40, picks in picks()
    ) {
        terminal_feed(bytes, cols, rows, picks)?;
    }
}

/// Run `test` on fresh cases until `secs` are up; returns the case count.
/// Each round is a new runner with a new seed (a runner that has finished
/// its cases runs none on a second call).
fn soak<S: Strategy>(
    name: &str,
    secs: u64,
    strategy: S,
    test: impl Fn(S::Value) -> Result<(), TestCaseError>,
) -> u64 {
    use proptest::test_runner::{RngAlgorithm, TestRng};
    use std::sync::atomic::{AtomicU64, Ordering};
    let started = std::time::Instant::now();
    let deadline = started + std::time::Duration::from_secs(secs);
    let cases = AtomicU64::new(0);
    let counted = |value| {
        cases.fetch_add(1, Ordering::Relaxed);
        test(value)
    };
    let base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let mut round = 0u64;
    while std::time::Instant::now() < deadline {
        round += 1;
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&base.to_le_bytes());
        seed[8..16].copy_from_slice(&round.to_le_bytes());
        let mut runner = TestRunner::new_with_rng(
            Config {
                cases: 1_000,
                failure_persistence: None,
                ..Config::default()
            },
            TestRng::from_seed(RngAlgorithm::ChaCha, &seed),
        );
        if let Err(err) = runner.run(&strategy, counted) {
            panic!("{name}: {err} (seed base {base}, round {round})");
        }
    }
    let cases = cases.load(Ordering::Relaxed);
    eprintln!(
        "fuzz {name}: {cases} cases in {:.0}s, {round} rounds, no failure",
        started.elapsed().as_secs_f64()
    );
    cases
}

#[test]
#[ignore = "long: MALGEL_FUZZ_SECS per target"]
fn fuzz_soak() {
    let secs: u64 = std::env::var("MALGEL_FUZZ_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let only = std::env::var("MALGEL_FUZZ_TARGET").unwrap_or_default();
    let run = |name: &str| only.is_empty() || only == name;
    if run("edit") {
        soak(
            "edit",
            secs,
            (text_strategy(), 0usize..400, 0usize..400, any::<u8>()),
            |(text, a, b, op)| edit_ops(&text, a, b, op),
        );
    }
    if run("decode") {
        soak(
            "decode",
            secs,
            proptest::collection::vec(any::<u8>(), 0..2_000),
            decode_bytes,
        );
        soak(
            "encode",
            secs,
            (text_strategy(), any::<bool>(), any::<u8>()),
            |(text, crlf, enc)| encode_round_trip(&text, crlf, enc),
        );
    }
    if run("terminal") {
        soak(
            "terminal",
            secs,
            (term_bytes(), 1u16..120, 1u16..40, picks()),
            |(bytes, cols, rows, picks)| terminal_feed(bytes, cols, rows, picks),
        );
    }
}
