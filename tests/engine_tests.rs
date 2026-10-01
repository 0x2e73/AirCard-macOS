#[test]
fn system_randomness_is_available() {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("OS random source is required before any device write");
    assert!(bytes.iter().any(|b| *b != 0));
}
