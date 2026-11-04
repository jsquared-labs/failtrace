pub fn parse_id(input: &str) -> u64 {
    input.parse().unwrap()
}

#[cfg(test)]
mod tests {
    use super::parse_id;

    #[test]
    fn parses_bad_input() {
        parse_id("not-a-number");
    }
}
