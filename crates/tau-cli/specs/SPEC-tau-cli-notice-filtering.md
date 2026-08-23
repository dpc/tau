# SPEC-tau-cli-notice-filtering: Notice filtering

Harness/UI notices are filtered in the terminal UI, not at the harness emission site. The default threshold is `info`; `:set notice-level <level>` and persisted `cli.json` `notice_level` change what routine notices a UI renders. Critical notices and `always_show` warning diagnostics remain visible regardless of threshold. UI special-casing must use the stable `harness.notice.kind` field rather than parsing notice text.

Compact transcript mode is a second, local projection over retained notices.
It hides every non-critical notice, including `always_show` warnings, without
discarding the retained payload; changing back to verbose mode restores those
notices at their original transcript positions. Critical notices remain visible
in both modes. This projection does not affect harness emission, protocol
delivery, journals, or model context.

Successful manual-compaction acceptance and start are routine `info` lifecycle
notices and use ordinary status styling. Pre-start and transaction failures
remain terminal tool errors and retain error presentation.
