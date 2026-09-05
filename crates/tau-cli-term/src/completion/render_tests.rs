use super::*;

fn view(count: usize, selected: Option<usize>) -> CompletionView {
    CompletionView {
        candidates: (0..count)
            .map(|i| Candidate {
                label: format!("item-{i}"),
                description: "description".to_owned(),
                replacement: format!("item-{i}"),
                cursor: format!("item-{i}").len(),
                acceptance: None,
            })
            .collect(),
        selected,
    }
}

fn block_text(block: &StyledBlock) -> String {
    block
        .content
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect()
}

#[test]
fn menu_height_is_capped_to_percent_of_terminal_height() {
    let block = render_menu_block(&view(20, None), &Theme::new(), 80, 24);
    let text = block_text(&block);

    assert_eq!(text.lines().count(), 7);
    assert!(text.contains("item-0"));
    assert!(text.contains("item-6"));
    assert!(!text.contains("item-7"));
}

#[test]
fn selected_candidate_stays_visible_inside_capped_menu() {
    let block = render_menu_block(&view(20, Some(15)), &Theme::new(), 80, 24);
    let text = block_text(&block);

    assert_eq!(text.lines().count(), 7);
    assert!(text.contains("item-15"));
    assert!(!text.contains("item-0"));
}

#[test]
fn long_candidates_are_truncated_to_one_terminal_row() {
    let view = CompletionView {
        candidates: vec![Candidate {
            label: "./this/is/a/very/long/path/that/would/wrap/without/truncation".to_owned(),
            description: "directory".to_owned(),
            replacement: String::new(),
            cursor: 0,
            acceptance: None,
        }],
        selected: None,
    };
    let block = render_menu_block(&view, &Theme::new(), 24, 24);
    let text = block_text(&block);

    assert_eq!(text.lines().count(), 1);
    assert!(display_width(text.as_str()) <= 24);
    assert!(text.contains('…'));
}

#[test]
fn emoji_candidates_are_truncated_by_grapheme_width() {
    let view = CompletionView {
        candidates: vec![Candidate {
            label: "⚠️x".to_owned(),
            description: "👩‍💻x".to_owned(),
            replacement: String::new(),
            cursor: 0,
            acceptance: None,
        }],
        selected: None,
    };
    let block = render_menu_block(&view, &Theme::new(), 6, 24);
    let text = block_text(&block);

    assert_eq!(text.lines().count(), 1);
    assert!(display_width(text.as_str()) <= 6, "{text:?}");
    assert!(text.contains('…'));
}

#[test]
fn very_narrow_completion_menu_does_not_wrap() {
    let view = CompletionView {
        candidates: vec![Candidate {
            label: "abcdef".to_owned(),
            description: "description".to_owned(),
            replacement: String::new(),
            cursor: 0,
            acceptance: None,
        }],
        selected: None,
    };
    let block = render_menu_block(&view, &Theme::new(), 3, 24);
    let text = block_text(&block);

    assert_eq!(text.lines().count(), 1);
    assert!(display_width(text.as_str()) <= 3, "{text:?}");
}
