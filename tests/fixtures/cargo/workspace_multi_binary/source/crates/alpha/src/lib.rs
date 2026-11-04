pub fn alpha() -> bool {
    true
}

#[cfg(test)]
mod tests {
    #[test]
    fn alpha_passes() {
        assert!(super::alpha());
    }
}
