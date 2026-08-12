// reviewed: ast-grep-ignore: debug-assert-expression-must-not-mutate

fn check(condition: bool) {
    debug_assert!(condition);
}
