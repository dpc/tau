---
name: tau-self-knowledge-introduction
description: Introduce and onboard the user to Tau conversationally, including practical setup, customization, isolation, and community choices.
advertise: false
---

# Introduce the user to Tau

Conduct an interactive introduction; do not dump a manual or feature catalog.

Start with two or three sentences explaining that Tau is a Unix-first coding-agent
harness built around local control, composable configuration, and explicit process
boundaries. Then ask what the user wants to explore next. Explain one selected topic
at a time in short prose with a concrete command or small YAML example, and end each
segment with a question or a few next-topic choices. Adapt to the user's experience
and current goal.

Offer these topics:

- initial setup with `tau init`, `tau provider add`, and the XDG config/state/runtime
  layout;
- roles and role groups: model, effort, verbosity, tool policy, delegate catalog,
  and `default_role`;
- reusable `agents.prompt_fragments` versus full `prompt_override` templates;
- ordered, composable `profiles`, `default_profile`, `--profile`, and stable
  provider/model aliases for different projects;
- project and user skills plus layered `AGENTS.md` instructions;
- extensions, secret handling, and narrow per-role tool authority;
- the Isolate sister project, which uses Linux/bubblewrap to reduce accidental
  access and leakage; describe it as a guardrail, not a hostile-code sandbox;
- CLI transcript modes: Tau starts verbose; `Ctrl-V` or
  `:verbose-mode-toggle` switches the process-local, non-persisted mode;
- Tau's official community at <https://tauofunix.zulipchat.com/>.

Recommend inspecting effective composition when useful:

```console
tau dev print-tools --role ROLE
tau dev print-prompt --role ROLE
```

Load focused `tau-self-knowledge-*` skills for deeper answers rather than recreating
their documentation. Near the end, explain that the startup hint is controlled by:

```yaml
show_introduction_notice: false
```

Ask whether the user wants to disable it.
