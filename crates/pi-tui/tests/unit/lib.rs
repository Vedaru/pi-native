use super::*;
use crate::buffer::{Buffer, Color, Style};

fn row_text(buffer: &Buffer, y: usize) -> String {
    (0..buffer.width())
        .map(|x| buffer.cell(x, y).and_then(|cell| cell.ch).unwrap_or(' '))
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[test]
fn put_str_places_text_and_advances_by_display_width() {
    let mut buffer = Buffer::new(10, 1);
    let end = buffer.put_str(0, 0, "hi", Style::default());
    assert_eq!(end, 2);
    assert_eq!(row_text(&buffer, 0), "hi");
    // A wide grapheme occupies two columns.
    let mut wide = Buffer::new(10, 1);
    let end = wide.put_str(0, 0, "你好", Style::default());
    assert_eq!(end, 4);
    assert!(wide.cell(1, 0).unwrap().continuation);
    assert!(!wide.cell(0, 0).unwrap().continuation);
    assert!(!wide.cell(2, 0).unwrap().continuation);
}

#[test]
fn put_str_clips_at_the_right_edge() {
    let mut buffer = Buffer::new(3, 1);
    buffer.put_str(0, 0, "abcdef", Style::default());
    assert_eq!(row_text(&buffer, 0), "abc");
}

#[test]
fn put_str_joins_combining_marks() {
    let mut buffer = Buffer::new(10, 1);
    // e + combining acute is one cluster of width 1.
    let end = buffer.put_str(0, 0, "e\u{0301}x", Style::default());
    assert_eq!(end, 2);
    assert_eq!(buffer.cell(0, 0).unwrap().ch, Some('e'));
    assert_eq!(buffer.cell(1, 0).unwrap().ch, Some('x'));
}

#[test]
fn diff_first_frame_writes_every_cell() {
    let mut buffer = Buffer::new(3, 1);
    buffer.put_str(0, 0, "abc", Style::default());
    let output = diff(None, &buffer);
    assert!(output.contains("abc"), "{output:?}");
    assert!(
        output.contains("\x1b[1;1H"),
        "missing cursor home: {output:?}"
    );
}

#[test]
fn diff_emits_nothing_for_an_unchanged_frame() {
    let mut buffer = Buffer::new(4, 2);
    buffer.put_str(0, 0, "hi", Style::default());
    let runtime = diff(None, &buffer);
    assert!(!runtime.is_empty());
    let again = diff(Some(&buffer), &buffer);
    assert_eq!(again, "", "unchanged frame should emit nothing");
}

#[test]
fn diff_only_touches_the_changed_cells() {
    let mut before = Buffer::new(6, 1);
    before.put_str(0, 0, "abcdef", Style::default());
    let mut after = before.clone();
    after.put_str(2, 0, "XY", Style::default());
    let output = diff(Some(&before), &after);
    assert!(output.contains("XY"), "{output:?}");
    // The unchanged prefix is not re-emitted.
    assert!(!output.contains('a'), "{output:?}");
    assert!(!output.contains('f'), "{output:?}");
}

#[test]
fn diff_emits_style_sequences() {
    let mut before = Buffer::new(4, 1);
    before.put_str(0, 0, "hi", Style::default());
    let mut after = Buffer::new(4, 1);
    after.put_str(
        0,
        0,
        "hi",
        Style {
            fg: Some(Color::Rgb(255, 0, 0)),
            bold: true,
            ..Style::default()
        },
    );
    let output = diff(Some(&before), &after);
    assert!(output.contains("\x1b[1m"), "bold: {output:?}");
    assert!(
        output.contains("\x1b[38;2;255;0;0m"),
        "truecolor: {output:?}"
    );
}

#[test]
fn renderer_tracks_the_previous_frame() {
    let mut renderer = Renderer::new();
    let mut buffer = Buffer::new(3, 1);
    buffer.put_str(0, 0, "abc", Style::default());
    let first = renderer.render(buffer.clone());
    assert!(!first.is_empty());
    let second = renderer.render(buffer.clone());
    assert_eq!(second, "");
    renderer.invalidate();
    let after_clear = renderer.render(buffer);
    assert!(!after_clear.is_empty(), "invalidate forces a full redraw");
}

#[test]
fn wrap_line_breaks_on_width() {
    let rows = wrap_line("hello world", 5);
    assert_eq!(rows.len(), 2);
    assert_eq!(&"hello world"[rows[0].start..rows[0].end], "hello");
    assert_eq!(&"hello world"[rows[1].start..rows[1].end], "world");
}

#[test]
fn wrap_line_hard_splits_a_long_word() {
    let rows = wrap_line("abcdefgh", 3);
    let pieces: Vec<&str> = rows
        .iter()
        .map(|row| &"abcdefgh"[row.start..row.end])
        .collect();
    assert_eq!(pieces, vec!["abc", "def", "gh"]);
}

#[test]
fn wrap_line_handles_wide_graphemes() {
    // Four columns fit two wide characters per row.
    let text = "你好世界";
    let rows = wrap_line(text, 4);
    assert_eq!(rows.len(), 2);
    assert_eq!(&text[rows[0].start..rows[0].end], "你好");
    assert_eq!(&text[rows[1].start..rows[1].end], "世界");
}

#[test]
fn wrap_line_empty_is_one_empty_row() {
    let rows = wrap_line("", 10);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].width, 0);
}

#[test]
fn move_cursor_steps_graphemes() {
    let text = "ab\u{0301}c"; // clusters: "a", "b\u{0301}", "c"
    assert_eq!(move_cursor(text, 0, 1), 1);
    assert_eq!(move_cursor(text, 1, 1), 4); // skip the combining mark
    assert_eq!(move_cursor(text, 4, -1), 1);
    assert_eq!(move_cursor(text, 4, -2), 0);
}

#[test]
fn cursor_column_counts_wide_glyphs_as_two() {
    let text = "你x";
    assert_eq!(cursor_column(text, 0), 0);
    assert_eq!(cursor_column(text, "你".len()), 2);
    assert_eq!(cursor_column(text, text.len()), 3);
}

#[test]
fn select_dialog_moves_and_answers() {
    let dialog = Dialog::Select {
        title: "pick".into(),
        options: vec!["a".into(), "b".into(), "c".into()],
    };
    let mut state = DialogState::new(dialog);
    assert_eq!(state.handle_key("down"), DialogOutcome::Pending);
    assert_eq!(state.handle_key("down"), DialogOutcome::Pending);
    // Already at the last option: stays put.
    assert_eq!(state.handle_key("down"), DialogOutcome::Pending);
    assert_eq!(state.selection(), 2);
    assert_eq!(
        state.handle_key("enter"),
        DialogOutcome::Answered(Some("c".into()))
    );
}

#[test]
fn select_dialog_cancels() {
    let dialog = Dialog::Select {
        title: "pick".into(),
        options: vec!["a".into()],
    };
    let mut state = DialogState::new(dialog);
    assert_eq!(state.handle_key("escape"), DialogOutcome::Answered(None));
}

#[test]
fn confirm_dialog_answers_yes_and_no() {
    let confirm = DialogState::new(Dialog::Confirm {
        title: "Run".into(),
        message: "bash?".into(),
    });
    let mut yes = confirm.clone();
    assert_eq!(
        yes.handle_key("y"),
        DialogOutcome::Answered(Some("true".into()))
    );
    let mut no = confirm;
    assert_eq!(
        no.handle_key("escape"),
        DialogOutcome::Answered(Some("false".into()))
    );
}

#[test]
fn input_dialog_edits_and_submits() {
    let mut state = DialogState::new(Dialog::Input {
        title: "name".into(),
        value: String::new(),
    });
    assert_eq!(state.handle_key("h"), DialogOutcome::Pending);
    assert_eq!(state.handle_key("i"), DialogOutcome::Pending);
    assert_eq!(state.handle_key("backspace"), DialogOutcome::Pending);
    assert_eq!(state.handle_key("x"), DialogOutcome::Pending);
    assert_eq!(
        state.handle_key("enter"),
        DialogOutcome::Answered(Some("hx".into()))
    );
}

#[test]
fn notify_dialog_dismisses_without_a_value() {
    let mut state = DialogState::new(Dialog::Notify {
        message: "hello".into(),
    });
    assert_eq!(state.handle_key("enter"), DialogOutcome::Answered(None));
}

// ---- image protocols (VED-307) ----

use crate::image::{
    calculate_image_cell_size, encode_iterm2, encode_kitty, image_dimensions, image_fallback,
    render_image, Capabilities, CellDimensions, ImageDimensions, ImageProtocol, RenderOptions,
};

fn png_1x1() -> String {
    // A 1x1 PNG, base64 (signature + IHDR width/height = 1).
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
        .to_string()
}

#[test]
fn png_dimensions_are_read_from_magic_bytes() {
    let dimensions = image_dimensions(&png_1x1()).expect("png dimensions");
    assert_eq!(dimensions.width_px, 1);
    assert_eq!(dimensions.height_px, 1);
}

#[test]
fn unknown_bytes_have_no_dimensions() {
    assert!(image_dimensions("bm90IGFuIGltYWdl").is_none());
    assert!(image_dimensions("!!!not base64!!!").is_none());
}

#[test]
fn capabilities_detect_kitty_and_iterm2() {
    let kitty = Capabilities::detect_from(|key| match key {
        "KITTY_WINDOW_ID" => Some("1".to_string()),
        _ => None,
    });
    assert_eq!(kitty.images, Some(ImageProtocol::Kitty));

    let iterm = Capabilities::detect_from(|key| match key {
        "TERM_PROGRAM" => Some("iTerm.app".to_string()),
        _ => None,
    });
    assert_eq!(iterm.images, Some(ImageProtocol::Iterm2));

    // tmux disables images (the escapes are not forwarded).
    let tmux = Capabilities::detect_from(|key| match key {
        "TMUX" => Some("/tmp/tmux".to_string()),
        "KITTY_WINDOW_ID" => Some("1".to_string()),
        _ => None,
    });
    assert_eq!(tmux.images, None);
}

#[test]
fn cell_size_fits_within_the_width() {
    let size = calculate_image_cell_size(
        ImageDimensions {
            width_px: 900,
            height_px: 180,
        },
        80,
        None,
        CellDimensions::default(),
        false,
    );
    assert!(size.columns <= 80, "{size:?}");
    assert!(size.rows >= 1);
}

#[test]
fn cell_size_honors_max_height() {
    let size = calculate_image_cell_size(
        ImageDimensions {
            width_px: 100,
            height_px: 10_000,
        },
        80,
        Some(10),
        CellDimensions::default(),
        false,
    );
    assert!(size.rows <= 10, "{size:?}");
}

#[test]
fn kitty_encoding_uses_png_and_placement_controls() {
    let sequence = encode_kitty(&png_1x1(), Some(2), Some(1), Some(7), true);
    assert!(
        sequence.starts_with("\x1b_Ga=T,f=100,q=2,c=2,r=1,i=7;"),
        "{sequence}"
    );
    assert!(sequence.ends_with("\x1b\\"), "{sequence}");
}

#[test]
fn kitty_chunks_long_payloads() {
    let big = "A".repeat(9000);
    let sequence = encode_kitty(&big, None, None, None, true);
    assert!(
        sequence.contains("m=1;"),
        "first chunk marks more: {sequence:.40}"
    );
    assert!(sequence.contains("m=0;"), "last chunk clears more");
    // 9000 bytes / 4096 => three chunks.
    assert_eq!(sequence.matches("\x1b_G").count(), 3);
}

#[test]
fn kitty_move_cursor_false_adds_c1() {
    let sequence = encode_kitty("AAAA", None, None, None, false);
    assert!(sequence.contains("C=1"), "{sequence}");
}

#[test]
fn iterm2_encoding_has_inline_and_size() {
    let sequence = encode_iterm2("AAAA", Some(10), Some("cat.png"), true);
    assert!(
        sequence.starts_with("\x1b]1337;File=inline=1;"),
        "{sequence}"
    );
    assert!(sequence.contains("width=10"), "{sequence}");
    assert!(sequence.contains("height=auto"), "{sequence}");
    assert!(
        sequence.contains("name="),
        "name should be base64: {sequence}"
    );
    assert!(sequence.ends_with('\x07'), "{sequence}");
    // size is the decoded byte length of "AAAA" = 3.
    assert!(sequence.contains("size=3"), "{sequence}");
}

#[test]
fn render_image_returns_cells_and_sequence() {
    let rendered = render_image(
        &png_1x1(),
        ImageDimensions {
            width_px: 9,
            height_px: 18,
        },
        ImageProtocol::Kitty,
        RenderOptions {
            max_width_cells: 2,
            image_id: Some(1),
            ..RenderOptions::default()
        },
    );
    assert_eq!(rendered.columns, 2);
    assert_eq!(rendered.rows, 2);
    assert_eq!(rendered.image_id, Some(1));
    assert!(rendered.sequence.contains("i=1"));
}

#[test]
fn image_fallback_mentions_type_and_size() {
    let fallback = image_fallback(
        "image/png",
        Some(ImageDimensions {
            width_px: 4,
            height_px: 3,
        }),
        Some("cat.png"),
    );
    assert_eq!(fallback, "[Image: cat.png [image/png] 4x3]");
}

// ---- interactive screen composition (VED-307) ----

use crate::screen::{cursor_sequence, Screen, TranscriptLine};

fn row_string(buffer: &crate::buffer::Buffer, y: usize) -> String {
    (0..buffer.width())
        .map(|x| buffer.cell(x, y).and_then(|cell| cell.ch).unwrap_or(' '))
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[test]
fn screen_composes_the_editor_on_the_bottom_row() {
    let mut screen = Screen::new();
    screen.set_editor("hello");
    let frame = screen.compose(20, 5);
    assert_eq!(row_string(&frame.buffer, 4), "> hello");
    assert_eq!(frame.cursor, Some((7, 4)));
}

#[test]
fn screen_cursor_tracks_editing() {
    let mut screen = Screen::new();
    screen.insert('a');
    screen.insert('b');
    screen.insert('c');
    screen.move_cursor(-1);
    let frame = screen.compose(20, 3);
    // "> abc", cursor one grapheme back is after 'b'.
    assert_eq!(row_string(&frame.buffer, 2), "> abc");
    assert_eq!(frame.cursor, Some((4, 2)));
}

#[test]
fn screen_backspace_removes_last_grapheme() {
    let mut screen = Screen::new();
    screen.insert_str("ab");
    screen.backspace();
    assert_eq!(screen.editor(), "a");
    assert_eq!(screen.cursor(), 1);
    screen.backspace();
    screen.backspace();
    assert_eq!(screen.editor(), "");
}

#[test]
fn screen_transcript_fills_rows_above_the_editor() {
    let mut screen = Screen::new();
    screen.push_line(TranscriptLine::new("first"));
    screen.push_line(TranscriptLine::new("second"));
    let frame = screen.compose(20, 5);
    assert_eq!(row_string(&frame.buffer, 0), "first");
    assert_eq!(row_string(&frame.buffer, 1), "second");
    assert_eq!(row_string(&frame.buffer, 4), ">");
}

#[test]
fn screen_shows_the_tail_of_a_long_transcript() {
    let mut screen = Screen::new();
    for index in 0..10 {
        screen.push_line(TranscriptLine::new(format!("line{index}")));
    }
    let frame = screen.compose(20, 4);
    // The last three transcript rows plus the editor are visible.
    assert_eq!(row_string(&frame.buffer, 0), "line7");
    assert_eq!(row_string(&frame.buffer, 1), "line8");
    assert_eq!(row_string(&frame.buffer, 2), "line9");
    assert_eq!(row_string(&frame.buffer, 3), ">");
}

#[test]
fn screen_scroll_reveals_earlier_lines() {
    let mut screen = Screen::new();
    for index in 0..10 {
        screen.push_line(TranscriptLine::new(format!("line{index}")));
    }
    screen.scroll_up(4);
    let frame = screen.compose(20, 3);
    assert_eq!(row_string(&frame.buffer, 0), "line4");
    assert_eq!(row_string(&frame.buffer, 1), "line5");
}

#[test]
fn screen_renders_an_open_dialog_above_the_editor() {
    let mut screen = Screen::new();
    screen.set_dialog(crate::dialog::Dialog::Confirm {
        title: "Run".into(),
        message: "bash?".into(),
    });
    let frame = screen.compose(40, 4);
    assert!(row_string(&frame.buffer, 2).contains("Run bash?"));
    assert_eq!(row_string(&frame.buffer, 3), ">");
}

#[test]
fn cursor_sequence_moves_to_a_one_based_cell() {
    assert_eq!(cursor_sequence(Some((0, 0))), "\x1b[1;1H");
    assert_eq!(cursor_sequence(Some((4, 2))), "\x1b[3;5H");
    assert_eq!(cursor_sequence(None), "");
}

// ---- acceptance evidence: no full-frame re-render on large transcripts ----

#[test]
fn large_transcript_steady_state_emits_no_full_frame() {
    // A long transcript must not be re-emitted every frame: once the frame is
    // unchanged, the differential renderer emits nothing.
    let mut screen = Screen::new();
    for index in 0..5_000 {
        screen.push_line(TranscriptLine::new(format!("transcript line {index}")));
    }
    let frame = screen.compose(120, 40).buffer;
    let mut renderer = crate::render::Renderer::new();
    let first = renderer.render(frame.clone());
    assert!(!first.is_empty(), "the first frame must draw something");
    // Redraw the identical frame: nothing should be written.
    let second = renderer.render(frame.clone());
    assert_eq!(second, "", "an unchanged large frame must emit nothing");
    // The steady-state delta must be far smaller than a full repaint of the
    // transcript (no full-frame regression).
    assert!(
        first.len() < 40 * 120 * 8,
        "first frame unexpectedly large: {} bytes",
        first.len()
    );
}

#[test]
fn editing_only_repaints_the_editor_row() {
    let mut screen = Screen::new();
    for index in 0..5_000 {
        screen.push_line(TranscriptLine::new(format!("transcript line {index}")));
    }
    let before = screen.compose(120, 40).buffer;
    screen.insert('x');
    let after = screen.compose(120, 40).buffer;
    let delta = crate::render::diff(Some(&before), &after);
    // The change must be confined to the editor row (the last one).
    assert!(delta.contains('x'), "the typed character must be drawn");
    assert!(
        !delta.contains("transcript line"),
        "an editor keystroke must not re-emit the transcript: {} bytes",
        delta.len()
    );
    assert!(
        delta.len() < 256,
        "editor delta should be tiny: {} bytes",
        delta.len()
    );
}

#[test]
fn components_composite_into_a_screen_frame() {
    use crate::component::{composite, ComponentCache, ComponentSource};

    struct Static(Vec<String>);
    impl ComponentSource for Static {
        fn render(&mut self, _width: usize) -> Vec<String> {
            self.0.clone()
        }
        fn invalidate(&mut self) {}
    }

    let mut cache = ComponentCache::new(Static(vec!["boxed".into(), "custom".into()]));
    let lines = cache.lines(80).to_vec();
    let mut screen = Screen::new();
    screen.push_line(TranscriptLine::new("header"));
    let mut frame = screen.compose(80, 6);
    let written = composite(
        &mut frame.buffer,
        0,
        2,
        &lines,
        crate::buffer::Style::default(),
    );
    assert_eq!(written, 2);
    assert_eq!(row_string(&frame.buffer, 2), "boxed");
    assert_eq!(row_string(&frame.buffer, 3), "custom");
    // A second frame with the same width must not re-run the component.
    let _ = cache.lines(80);
    assert_eq!(cache.render_count(), 1, "unchanged component re-rendered");
}
