//! Stale chip ranges must not panic. After a skill injection or a rewind, a
//! restored chip range can be out of bounds or off a char boundary, and the
//! release profile aborts on a panic.

use super::*;
use crate::app::agent::ChipElement;

fn chip(range: std::ops::Range<usize>, kind: ElementKind) -> ChipElement {
    ChipElement {
        range,
        kind,
        display: None,
    }
}

#[test]
fn transformed_text_skips_an_image_chip_off_a_char_boundary() {
    // "x" is 1 byte and "é" is 2 bytes, so the rebased chip starts inside "é".
    let stash = StashedPrompt::from_submission(
        "xé[Image #1]".to_owned(),
        Vec::new(),
        vec![chip(2..12, KIND_IMAGE)],
    );
    let stash = stash.with_transformed_text("é[Image #1]".to_owned());
    assert_eq!(stash.text, "é[Image #1]");
    assert_eq!(stash.chip_elements.len(), 1);
}

#[test]
fn file_ref_lookup_ignores_an_out_of_bounds_element() {
    let mut pw = PromptWidget::new();
    pw.set_text("@a");
    pw.restore_chip_elements(&[chip(0..50, KIND_FILE_REF)]);
    assert_eq!(pw.file_ref_element_at_cursor(), None);
    assert_eq!(pw.file_ref_at_element_boundary(), None);
}

#[test]
fn text_without_image_chips_keeps_text_when_a_chip_is_off_a_char_boundary() {
    let mut pw = PromptWidget::new();
    pw.set_text("é");
    pw.restore_chip_elements(&[chip(1..2, KIND_IMAGE)]);
    assert_eq!(pw.text_without_image_chips(), "é");
}

#[test]
fn image_number_lookups_ignore_an_out_of_bounds_chip() {
    let mut pw = PromptWidget::new();
    pw.set_text("[Image #1]");
    pw.restore_chip_elements(&[chip(0..10, KIND_IMAGE), chip(4..40, KIND_IMAGE)]);
    pw.sync_images_with_textarea();
    pw.set_images(Vec::new());
}

#[test]
fn paste_chip_lookup_ignores_an_out_of_bounds_element() {
    let mut pw = PromptWidget::new();
    pw.set_text("ab");
    pw.restore_chip_elements(&[chip(0..50, KIND_PASTE)]);
    let elem = pw.textarea.elements()[0].clone();
    assert_eq!(pw.paste_text(&elem), None);
}
