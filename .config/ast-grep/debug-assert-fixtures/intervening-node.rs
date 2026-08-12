fn check(condition: bool) {
    // ast-grep-ignore: debug-assert-expression-must-not-mutate
    let checked = condition;
    debug_assert!(checked);
}
