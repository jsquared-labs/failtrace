pub fn borrow_conflict() {
    let mut data = vec![1, 2, 3];
    let first = &data[0];
    data.push(4);
    println!("{first}");
}

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
