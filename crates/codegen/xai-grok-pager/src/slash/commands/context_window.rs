//! Context-window tokens for the `/model` picker (`/context-window` itself is removed).
// oc-build: opencode backend

use crate::slash::command::ArgItem;




/// Resolves a user token to one of the supported windows.
pub(crate) fn supported_window(token: &str, options: &[u64]) -> Option<std::num::NonZeroU64> {
    parse_window_token(token)
        .filter(|window| options.contains(window))
        .and_then(std::num::NonZeroU64::new)
}

/// Returns a short label such as `256k` for `256000`. `1048576` stays a raw count.
pub(crate) fn format_window(window: u64) -> String {
    if window >= 1_000_000 && window.is_multiple_of(1_000_000) {
        format!("{}m", window / 1_000_000)
    } else if window >= 1_000 && window.is_multiple_of(1_000) {
        format!("{}k", window / 1_000)
    } else {
        window.to_string()
    }
}

/// Accepts the short label (`256k`, `1m`, case-insensitive) or the raw token count.
pub(crate) fn parse_window_token(token: &str) -> Option<u64> {
    let token = token.to_ascii_lowercase();
    if let Some(thousands) = token.strip_suffix('k') {
        return thousands.parse::<u64>().ok()?.checked_mul(1_000);
    }
    if let Some(millions) = token.strip_suffix('m') {
        return millions.parse::<u64>().ok()?.checked_mul(1_000_000);
    }
    token.parse().ok()
}

/// Window rows for the `/model` picker.
pub(crate) fn window_arg_items(
    options: &[u64],
    active: Option<u64>,
    default: Option<u64>,
    insert_text_for: impl Fn(&str, u64) -> String,
) -> Vec<ArgItem> {
    options
        .iter()
        .enumerate()
        .map(|(idx, &window)| {
            let label = format_window(window);
            let active_suffix = if active == Some(window) {
                " (active)"
            } else {
                ""
            };
            let default_prefix = if default == Some(window) {
                "Default • "
            } else {
                ""
            };
            // The letter prefix keeps catalog order. The matcher sorts ties alphabetically
            let sort_prefix = char::from(b'a' + idx.min(25) as u8);
            let insert_text = insert_text_for(&label, window);
            ArgItem {
                display: format!("{label}{active_suffix}"),
                match_text: format!("{sort_prefix} {insert_text}"),
                insert_text,
                description: format!("{default_prefix}{window} tokens"),
            }
        })
        .collect()
}

pub(crate) fn unknown_window_error(token: &str, options: &[u64]) -> String {
    let offered = options
        .iter()
        .map(|&w| format_window(w))
        .collect::<Vec<_>>()
        .join(", ");
    format!("unknown context window '{token}'; use one of: {offered}")
}

#[cfg(test)]
#[path = "context_window_tests.rs"]
mod tests;
