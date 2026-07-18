# SPEC-tau-cli-notice-filtering: Notice filtering

Harness/UI notices are filtered in the terminal UI, not at the harness emission site. The default threshold is `info`; `/set notice-level <level>` and persisted `cli.json` `notice_level` change what routine notices a UI renders. Critical notices and `always_show` warning diagnostics remain visible regardless of threshold. UI special-casing must use the stable `harness.notice.kind` field rather than parsing notice text.
