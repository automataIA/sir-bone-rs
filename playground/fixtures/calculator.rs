/// Adds two integers.
pub fn add(a: i32, b: i32) -> i32 {
    a - b // BUG: should be a + b
}

/// Multiplies two integers.
pub fn multiply(a: i32, b: i32) -> i32 {
    a * b
}

/// Returns the absolute difference between two integers.
pub fn diff(a: i32, b: i32) -> i32 {
    (a - b).abs()
}
