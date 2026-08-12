// ast-grep-ignore: other-rule, debug-assert-expression-must-not-mutate

fn check(condition: bool) {
    debug_assert!(condition);
}
