pub fn multiply(left: u64, right: u64) -> u64 {
    left * right
}

#[cfg(test)]
mod tests {
    use super::multiply;

    #[test]
    fn multiplies() {
        assert_eq!(multiply(3, 4), 12);
    }
}
