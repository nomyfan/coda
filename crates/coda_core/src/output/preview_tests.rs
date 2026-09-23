use super::*;

fn numbered(count: usize) -> String {
    (1..=count).map(|n| format!("line {n}\n")).collect()
}

#[test]
fn output_that_fits_renders_exactly() {
    for text in ["", "a\nb\n", "a\nb", "你好\r\n世界", "\n\n"] {
        assert_eq!(
            Preview::of(text, 4096).render(100),
            (text.to_owned(), false)
        );
    }
}

#[test]
fn omitted_lines_are_named_with_their_exact_bytes() {
    let text = numbered(1000);
    let lines: Vec<_> = text.lines().collect();
    let (shown, cut) = Preview::of(&text, 4096).render(200);
    assert!(cut);
    assert!(shown.len() <= 200, "{shown}");
    let pieces: Vec<_> = shown.lines().collect();
    let marker = pieces
        .iter()
        .position(|piece| piece.starts_with("[... lines "))
        .expect("marker");
    let range = pieces[marker]
        .strip_prefix("[... lines ")
        .unwrap()
        .split(' ')
        .next()
        .unwrap();
    let (first, last) = range.split_once('-').unwrap();
    let (first, last): (usize, usize) = (first.parse().unwrap(), last.parse().unwrap());
    assert_eq!(pieces[..marker], lines[..first - 1]);
    assert_eq!(pieces[marker + 1..], lines[last..]);
    let size = |lines: &[&str]| lines.iter().map(|line| line.len() + 1).sum::<usize>();
    let (offset, omitted) = (size(&lines[..first - 1]), size(&lines[first - 1..last]));
    assert!(
        pieces[marker].ends_with(&format!(
            "omitted ({omitted} bytes at offset {offset}) ...]"
        )),
        "{}",
        pieces[marker]
    );
}

#[test]
fn chunk_boundaries_do_not_change_the_preview() {
    let text = format!("{}{}\n{}", numbered(300), "长".repeat(900), numbered(300));
    let whole = Preview::of(&text, 8192).render(1024);
    for size in [1, 7, 4096] {
        let mut preview = Preview::new(8192);
        for chunk in text.as_bytes().chunks(size) {
            preview.append(chunk);
        }
        assert_eq!(preview.render(1024), whole);
    }
}

#[test]
fn a_long_line_shows_its_start_and_names_the_bytes_left_out() {
    let text = format!("short\n{}\nend\n", "x".repeat(5000));
    let (shown, cut) = Preview::of(&text, 16384).render(16384);
    assert!(cut);
    assert_eq!(
        shown,
        format!(
            "short\n{} [line 2 truncated: 3000 more bytes at offset 2006]\nend",
            "x".repeat(2000)
        )
    );
}

#[test]
fn a_shortened_line_ends_on_a_character_boundary() {
    let (shown, _) = Preview::of(&"你".repeat(1000), 16384).render(16384);
    assert_eq!(
        shown,
        format!(
            "{} [line 1 truncated: 1002 more bytes at offset 1998]",
            "你".repeat(666)
        )
    );
}

#[test]
fn every_render_fits_its_limit() {
    let text = format!(
        "{}{}\n{}tail without newline",
        numbered(50),
        "界".repeat(3000),
        numbered(50)
    );
    let preview = Preview::of(&text, 4096);
    for limit in 0..1200 {
        assert!(preview.render(limit).0.len() <= limit, "limit {limit}");
    }
}

#[test]
fn the_tail_keeps_the_latest_lines_including_an_unterminated_one() {
    let text = format!("{}last", numbered(100));
    let (shown, cut) = Preview::of(&text, 4096).render(128);
    assert!(cut);
    assert!(shown.starts_with("line 1\n"), "{shown}");
    assert!(shown.ends_with("line 100\nlast"), "{shown}");
}

#[test]
fn a_preview_too_small_for_any_line_still_names_them_all() {
    let (shown, cut) = Preview::of(&numbered(10), 1024).render(55);
    assert!(cut);
    assert_eq!(shown, "[... lines 1-10 omitted (71 bytes at offset 0) ...]");
    assert_eq!(
        Preview::of(&numbered(10), 1024).render(10),
        (String::new(), true)
    );
}

#[test]
fn leading_lines_stop_at_a_line_end() {
    assert_eq!(leading_lines("ab\ncd\nef", 6), "ab\ncd");
    assert_eq!(leading_lines("ab\ncd", 99), "ab\ncd");
    assert_eq!(leading_lines("abcdef\ngh", 3), "");
    assert_eq!(leading_lines("你\n好好", 5), "你");
}

#[test]
fn channels_share_the_space_under_their_headings() {
    let preview = OutputPreview {
        prefix: "exit code: 1\n".into(),
        channels: vec![
            (Channel::Stdout, Preview::of(&numbered(1000), 4096)),
            (Channel::Stderr, Preview::of("warning\n", 4096)),
        ],
    };
    let (shown, cut) = preview.render(500);
    assert!(cut);
    assert!(shown.len() <= 500);
    assert!(shown.starts_with("exit code: 1\nline 1\n"), "{shown}");
    assert!(shown.ends_with("\nstderr:\nwarning\n"), "{shown}");
}

#[test]
fn an_oversized_prefix_is_shortened_to_the_limit() {
    let preview = OutputPreview {
        prefix: "错误".repeat(10),
        channels: vec![(Channel::Stdout, Preview::of("out", 64))],
    };
    assert_eq!(preview.render(7), ("错误".into(), true));
}

#[test]
fn a_prefix_runs_straight_into_a_lone_stderr() {
    let preview = OutputPreview {
        prefix: "exit code: 2\n".into(),
        channels: vec![
            (Channel::Stdout, Preview::of("", 64)),
            (Channel::Stderr, Preview::of("boom", 64)),
        ],
    };
    assert_eq!(
        preview.render(100),
        ("exit code: 2\nstderr:\nboom".into(), false)
    );
}

#[test]
fn empty_lines_are_charged_their_records() {
    let mut preview = Preview::new(4096);
    preview.append(&[b'\n'; 100_000]);
    let (h, t) = (preview.head.len(), preview.tail.len());
    assert!(h > 0 && t > 0);
    assert!((h + t) * LINE_OVERHEAD <= 4096);
    let (shown, _) = preview.render(4096);
    let marker = format!(
        "\n[... lines {}-{} omitted ({} bytes at offset {h}) ...]\n",
        h + 1,
        100_000 - t,
        100_000 - h - t
    );
    assert!(shown.contains(&marker), "{shown:?}");
}

#[test]
fn a_line_too_long_for_its_side_keeps_what_fits() {
    let mut preview = Preview::new(1024);
    preview.append(format!("{}\n", "x".repeat(900)).as_bytes());
    assert_eq!(preview.head.len(), 1);
    assert!(preview.head_bytes <= preview.head_cap);
    preview.append(format!("{}\n", "y".repeat(900)).as_bytes());
    assert_eq!(preview.tail.len(), 1);
    assert!(preview.tail_bytes <= preview.tail_cap);
    let (shown, _) = preview.render(4096);
    assert!(
        shown.starts_with("xxx") && shown.contains("\nyyy"),
        "{shown}"
    );
    assert!(shown.contains("[line 2 truncated: "), "{shown}");
}
