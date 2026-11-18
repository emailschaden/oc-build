use super::*;






#[test]
fn format_and_parse_round_trip() {
    for (window, label) in [(256_000, "256k"), (500_000, "500k"), (1_000_000, "1m")] {
        assert_eq!(format_window(window), label);
        assert_eq!(parse_window_token(label), Some(window));
    }

    assert_eq!(parse_window_token("500000"), Some(500_000));
    assert_eq!(parse_window_token("500K"), Some(500_000));
    assert_eq!(parse_window_token("junk"), None);
}



