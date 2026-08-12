fn one_line(condition: bool) {
    // ast-grep-ignore: debug-assert-expression-must-not-mutate
    debug_assert!(condition);
}

fn multiline(condition: bool) {
    // ast-grep-ignore: debug-assert-expression-must-not-mutate
    debug_assert!(
        condition,
        "the acknowledgment scan fixture must preserve a multiline macro invocation"
    );
}
