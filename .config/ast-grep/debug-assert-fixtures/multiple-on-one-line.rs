fn statements(first: bool, second: bool) {
    // ast-grep-ignore: debug-assert-expression-must-not-mutate
    debug_assert!(first);
    debug_assert!(second);
}

fn tuple(first: bool, second: bool) {
    // ast-grep-ignore: debug-assert-expression-must-not-mutate
    let _ = (debug_assert!(first), debug_assert!(second));
}
