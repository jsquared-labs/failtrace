pub fn explode() {
    panic!("actual panic text");
}

#[cfg(test)]
mod tests {
    use super::explode;

    #[test]
    #[should_panic(expected = "expected panic text")]
    fn panic_message_matches_contract() {
        explode();
    }
}
