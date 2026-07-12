# DESIGN-tau-ext-rhai-untagged-tools: Rhai tools are currently untagged

Status: unconfirmed

`register_tool` does not expose Tau `ToolTag`s yet. Rhai tools are registered
without tag metadata, so tag-based role/model policy will not match them until
the Rhai tool-registration API grows validated tag support.
