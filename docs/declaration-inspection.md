# Declaration inspection

`tau dev preview-declarations` prints one pretty JSON report to stdout. It previews
config-derived extension declarations, not the effective prompt, selected tools,
accepted model routes, credentials, service availability or runtime readiness.
Configured executables are trusted cooperative same-user code, not sandboxed code.

The command uses the selected config profile, environment extension enable list
and ordered extension CLI overrides. It preserves resolved command, argv, cwd and
tool prefix. It does not apply role/model policy or include harness-owned internal
tools. It never constructs a harness, daemon, session, agent, socket or credential
store. Unsupported extensions receive no Configure and never fall back to ordinary
startup.

## JSON schema 1

All top-level fields and per-origin fields below are always present:

| Field | Type | Meaning |
| --- | --- | --- |
| `schema` | integer | `1`; independent of extension protocol version. |
| `scope` | string | Human-readable reminder that this is not effective prompt/tools or runtime availability. |
| `runtime_unverified` | boolean | Always `true`. |
| `extensions` | array of origin objects | Sorted by configured `instance`, ascending; each origin occurs once. |
| `collisions` | array of collision objects | Unique duplicates, ordered by kind (`tool`, then `model`) and then name ascending. |
| `resolution_incomplete` | boolean | Config loading or launch resolution failed, independently of protocol/collection failures. |

An origin object has:

| Field | Type | Meaning |
| --- | --- | --- |
| `instance` | string | Configured/requested attribution, not the child's chosen name. |
| `outcome` | closed string | One of the outcomes below. |
| `inventory` | object or null | Present object only after a valid declaration completion; otherwise `null`. |
| `state_settings_omitted` | boolean | Configured role is `provider`; state-owned profiles were deliberately not inspected, including on failed/unsupported origins. |

Closed outcomes:

- `complete_declarations`: completion has no gaps and no caller-owned provider
  omission. This does not mean semantically validated runtime registrations.
- `partial`: completion has gaps or config-owned provider classification requires
  the state-profile omission.
- `unsupported`: compatible Hello did not advertise inspection support.
- `protocol_mismatch`: incompatible protocol major.
- `invalid_protocol`: malformed/unexpected exchange, including ordinary Ready or a
  Hello kind inconsistent with configured role (`provider` requires Provider;
  other roles require Tool).
- `unavailable`: launch resolution, spawning, pipe setup, or permitted configuration
  input could not be completed.
- `deadline`: the bounded exchange expired.
- `cleanup_failed`: bounded child cleanup did not confirm reaping; inventory is
  discarded even if completion had arrived.
- `limit`: launch, cumulative-input or Configure-frame budget was exhausted.

The non-null inventory has four required arrays, in **peer-provided order**:
`tools` (`ToolRegistrationDeclared`), `prompt_fragments`
(`ExtPromptFragmentPublish`), `providers` (objects with a required `models` array of
`ProviderModelInfo`), and `gaps` (closed strings `runtime_declarations`,
`context_discovery`, `invalid_configuration`). The exact nested declaration fields,
optional-field omission and types are the JSON serialization of the public
[protocol declarations](../crates/tau-proto/src/inspection.rs) and
[event metadata types](../crates/tau-proto/src/events.rs). Tool registrations
include their optional group and prompt fragment. These raw typed declarations
have not passed ordinary harness semantic validation or policy selection.

Each collision is `{ "kind": "tool" | "model", "name": string }`. Tool internal
routing names and visible aliases share one namespace; an identical internal name
and alias in the **same registration** count once. Duplicate slots across
registrations count as a collision, including within one origin. Model route IDs
have a separate namespace. The report never chooses a winner. Attribution remains
in each origin's inventory; collision entries do not duplicate that inventory.

Enabled malformed origins remain named `unavailable` entries, whether required or
optional. Unknown environment/CLI overrides prevent all launches: every
discoverable configured/requested origin is reported unavailable, including those
whose enabled selection could not be established. Config parse/load failure instead
returns `resolution_incomplete: true` and an empty `extensions` array: **no origins
were discoverable**, not “none configured.”

Exit is zero only when resolution is complete, collisions are empty, and every
origin is `complete_declarations`. An intentionally empty selection succeeds with
an empty report. All incomplete reports are still printed and exit nonzero with a
fixed stderr summary. CLI argument errors that happen before
collection can fail without JSON; stdout I/O failure can leave a partial report.
Neither closed collection diagnostics nor stderr include raw configuration or
child error text. Declaration payloads remain authored by the trusted executable.

Provider inspection reads only credential-free config-owned profiles, never
state-owned profiles, named key sources or secret records. Consequently provider
completion is always `partial`, even if config profiles are absent. This command
does not claim parity with the runtime provider inventory.
