---
name: tau-self-knowledge
description: >
  Use this skill when the user asks about the Tau coding agent they are running
  in, including what Tau is, how it works, built-in self-knowledge, configuration,
  debugging, source code, community links, or where to find Tau-specific help.
advertise: true
---

# Tau self-knowledge

Tau is a coding agent harness.

To enable self-help it includes a built-in repository of skills with information about Tau itself.

## Build information

You are running inside Tau version `__TAU_SELF_KNOWLEDGE_VERSION__`, git revision `__TAU_SELF_KNOWLEDGE_HASH__`, built on `__TAU_SELF_KNOWLEDGE_BUILD_DATE__`.

## Built-in self-knowledge skills

- `tau-self-knowledge` — overview of built-in Tau-specific skills.
- `tau-self-knowledge-introduction` — conduct a short, conversational Tau onboarding.
- `tau-self-knowledge-architecture` — high-level overview of Tau architecture and core components.
- `tau-self-knowledge-harness` — harness daemon startup, runtime-dir sockets, initial UI stdio, attach behavior, foreground daemon APIs, socket activation, and embedded runs.
- `tau-self-knowledge-config` — directories, important config files, and provider setup commands.
- `tau-self-knowledge-secrets` — declared extension secrets, source resolution, Configure and Secret RPC delivery, provider credentials, redaction, and limits.
- `tau-self-knowledge-isolation` — supervised-extension state views, Linux namespaces and read-only mounts, component exceptions, and trusted-boundary limits.
- `tau-self-knowledge-cli-ui` — terminal UI behavior, commands, prompt history, key bindings, and prompt completions.
- `tau-self-knowledge-email` — secure configuration for the built-in `std-pim`/`std-email` email module.
- `tau-self-knowledge-ext-pim` — extension capabilities, configuration, OAuth, and approval workflow for the built-in `std-pim` email/calendar extension.
- `tau-self-knowledge-ext-rostra` — `std-rostra` configuration, Rostra tool authority, durable local state, synchronization, and following notifications.
- `tau-self-knowledge-ext-slack` — Slack Socket Mode setup, scopes, event subscriptions, routing, security modes, and troubleshooting.
- `tau-self-knowledge-ext-zulip` — Zulip extension configuration, message routing, and troubleshooting.
- `tau-self-knowledge-ext-swarm` — Tau Swarm endpoint pinning, credentials, blockers, updates, reconnects, and process-memory lifetime.
- `tau-self-knowledge-ext-provider-builtin` — extension details for built-in providers, model publication, ChatGPT/Codex, Chat Completions, and OpenRouter.
- `tau-self-knowledge-ext-rhai` — extension details for the disabled `std-rhai` trusted local scripting extension and Rhai event hooks.
- `tau-self-knowledge-ext-shell` — extension details for `core-shell` filesystem, shell, editing, directory-lock, and AGENTS.md discovery tools.
- `tau-self-knowledge-ext-std-notifications` — extension details for prompt/response sounds, idle notifications, OSC 1337, bells, and notification commands.
- `tau-self-knowledge-ext-test-dummy` — extension details for the disabled test-only dummy extension and restart/interception behavior.
- `tau-self-knowledge-ext-websearch` — extension details for `std-websearch`, hybrid Exa/Parallel search and fetch, failover, and explicit provider modes.
- `tau-self-knowledge-prompt-templating` — prompt fragment and system template variables, helpers, priorities, and examples.
- `tau-self-knowledge-source-code` — where to fetch Tau source code for debugging or detailed understanding.
- `tau-self-knowledge-community` — places to ask questions or talk about Tau.
- `tau-self-knowledge-debugging` — debugging workflow for Tau sessions, daemon behavior, logs, state, and provider request captures.
- `tau-self-knowledge-tracing` — content-free execution audits, bounded semantic traces, complete journal exports, and privacy boundaries.
- `tau-self-knowledge-e2e-testing` — manual E2E testing with `tau dev tmux`, scratch state, and opt-in provider profile access through `testing.yaml`.
When working _on_ Tau project, prefer the repository's local developer-centric skills when available.
