use playground::calculator;
use playground::utils;

#[test]
fn add_positive() {
    assert_eq!(calculator::add(2, 3), 5);
}

#[test]
fn add_negative() {
    assert_eq!(calculator::add(-1, 1), 0);
}

#[test]
fn multiply_works() {
    assert_eq!(calculator::multiply(3, 4), 12);
}

#[test]
fn diff_works() {
    assert_eq!(calculator::diff(10, 3), 7);
    assert_eq!(calculator::diff(3, 10), 7);
}

#[test]
fn is_even_true() {
    assert!(utils::is_even(0));
    assert!(utils::is_even(4));
    assert!(utils::is_even(-2));
}

#[test]
fn is_even_false() {
    assert!(!utils::is_even(1));
    assert!(!utils::is_even(-3));
}

#[test]
fn max_works() {
    assert_eq!(utils::max(5, 3), 5);
    assert_eq!(utils::max(1, 9), 9);
}
