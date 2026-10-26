#![cfg(target_os = "linux")]

use glib::prelude::*;

#[test]
fn string_variant_iterators_preserve_values_in_both_directions() {
    let variant = ["alpha", "日本語", "", "omega"].to_variant();
    assert_eq!(variant.array_iter_str().unwrap().next(), Some("alpha"));
    assert_eq!(variant.array_iter_str().unwrap().nth(1), Some("日本語"));
    assert_eq!(variant.array_iter_str().unwrap().last(), Some("omega"));
    assert_eq!(variant.array_iter_str().unwrap().next_back(), Some("omega"));
    assert_eq!(
        variant.array_iter_str().unwrap().nth_back(2),
        Some("日本語")
    );

    let mut iter = variant.array_iter_str().unwrap();
    assert_eq!(iter.next(), Some("alpha"));
    assert_eq!(iter.next_back(), Some("omega"));
    assert_eq!(iter.next_back(), Some(""));
    assert_eq!(iter.next(), Some("日本語"));
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);
}
