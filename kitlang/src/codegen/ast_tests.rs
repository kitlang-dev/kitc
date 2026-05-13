use super::*;

#[test]
fn test_include_without_lib() {
    let inc = Include::new("stdio.h".to_string());
    assert_eq!(inc.path, "stdio.h");
    assert!(inc.linked_lib.is_none());
}

#[test]
fn test_include_with_lib() {
    let inc = Include::with_lib("mylib.h".to_string(), "mylib".to_string());
    assert_eq!(inc.path, "mylib.h");
    assert_eq!(inc.linked_lib, Some("mylib".to_string()));
}
