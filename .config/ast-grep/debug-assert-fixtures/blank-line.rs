fn check(condition: bool) {
    // ast-grep-ignore: debug-assert-expression-must-not-mutate

    debug_assert!(condition);
}
