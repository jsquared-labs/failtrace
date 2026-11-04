pub fn beta() -> bool {
    false
}

#[cfg(test)]
mod tests {
    #[test]
    fn beta_fails() {
        assert!(super::beta());
    }
}
