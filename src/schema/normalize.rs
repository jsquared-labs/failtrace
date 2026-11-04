use std::sync::OnceLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

pub fn normalize_message(message: &str, cwd: &str) -> String {
    let stripped = strip_ansi(message);
    let mut normalized = stripped.nfc().collect::<String>();
    let mut normalized_cwd = cwd.nfc().collect::<String>();
    normalized = normalized.replace('\\', "/");
    normalized_cwd = normalized_cwd.replace('\\', "/");

    if !normalized_cwd.is_empty() {
        let cwd_prefix = if normalized_cwd.ends_with('/') {
            normalized_cwd
        } else {
            format!("{normalized_cwd}/")
        };
        normalized = normalized.replace(&cwd_prefix, "<cwd>/");
    }

    normalized = hex_regex().replace_all(&normalized, "<N>").into_owned();
    normalized = decimal_regex().replace_all(&normalized, "<N>").into_owned();
    normalized = whitespace_regex()
        .replace_all(&normalized, " ")
        .into_owned();
    normalized.trim().to_owned()
}

fn strip_ansi(message: &str) -> String {
    ansi_regex().replace_all(message, "").into_owned()
}

fn ansi_regex() -> &'static Regex {
    static ANSI: OnceLock<Regex> = OnceLock::new();
    ANSI.get_or_init(|| {
        Regex::new(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]")
            .expect("ANSI regex compiles")
    })
}

fn hex_regex() -> &'static Regex {
    static HEX: OnceLock<Regex> = OnceLock::new();
    HEX.get_or_init(|| Regex::new(r"0x[0-9a-fA-F]+").expect("hex regex compiles"))
}

fn decimal_regex() -> &'static Regex {
    static DECIMAL: OnceLock<Regex> = OnceLock::new();
    DECIMAL.get_or_init(|| Regex::new(r"(?-u)\b\d+\b").expect("decimal regex compiles"))
}

fn whitespace_regex() -> &'static Regex {
    static WHITESPACE: OnceLock<Regex> = OnceLock::new();
    WHITESPACE.get_or_init(|| Regex::new(r"\s+").expect("whitespace regex compiles"))
}
