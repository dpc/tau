# `debug_assert!` acknowledgments

`debug_assert!` expressions disappear in release builds. Keep the existing
Clippy `debug_assert_with_mut_call` denial, and also put this native ast-grep
suppression immediately above every directly parsed Rust invocation:

```rust
// ast-grep-ignore: debug-assert-expression-must-not-mutate
debug_assert!(state.is_consistent());
```

The rule ID is the required acknowledgment. Ast-grep accepts extra prose in
the comment, but does not validate it, so do not treat arbitrary prose as an
enforced explanation.

The structural rule matches `debug_assert!` and qualified/raw-identifier forms,
not `debug_assert_eq!`, aliases, or arbitrary similarly named macros. It is
lexical rather than name-resolving, so an imported alias is outside this
convention.

Rust tree-sitter keeps the contents of an outer macro invocation and a
`macro_rules!` body as opaque token trees. Therefore, it cannot find textual
`debug_assert!` tokens nested in `wrapper! { ... }`, `stringify!(...)`, or a
`macro_rules!` definition. Do not add a token-tree regex fallback: it would
misidentify strings and arbitrary tokens, and could not reliably attach an
acknowledgment to an invocation.

Ast-grep itself rejects stale, misplaced, and bare suppressions when scanned
with `--error`. It accepts rule-specific directive variants on line one followed
by a whitespace-only line as a whole-file suppression, so the scan-time
regression script explicitly rejects that form. Native suppression also scopes
to a source line rather than an AST node, so the same script rejects multiple
direct invocations on one line. These checks only close known
global/multiple-node suppression escape hatches; the structural rule remains
the authority for matching invocations.
