# Protocol 15 coordinated rollout

Protocol 15.0 supports partial-answer phases and provider continuation within
the same outer turn. See the [SDK release record](sdk-releases.md#protocol-150-release-acceptance)
for proto/client SDK 0.15.0 registry publication and isolated registry-consumer
acceptance.
Publication and compatible builds do not authorize activation or service restarts.

## Compatible package closure

Tau re-exports independently built extension packages rather than rebuilding
their sources against the workspace SDK. Updating only Tau cannot migrate
those executables. This checkout pins the following published revisions. Each
passed independent review and CI and uses exact registry SDK 0.15.0 runtime
dependencies:

| Input | Revision |
| --- | --- |
| `tau-ext-pim` | `c2e4cf4d288969b03531db6a9a5cfab5d25dc7a9` |
| `tau-ext-rostra` | `92ee17bf0e9cdfc20e8b86818d1f79a397a1fa2e` |
| `tau-ext-slack` | `99d5f01b153949266289800259e19123e60d458f` |
| `tau-ext-swarm` | `86a440e97dd32ab6976f17d25067f35e8156087d` |
| `tau-ext-telegram` | `786b9eb46b361d79de0c0a83a333eae92bbc6aab` |
| `tau-ext-xmpp` | `d14b86c36fcf8b85c49edece69ee0f9857f1da28` |
| `tau-ext-zulip` | `1250ac46847f2dd00110274b58014de3b3a631b0` |

Telegram supplies both `tau-ext-telegram` and `tau-telegram-gateway`. Its
authenticated gateway protocol remains 0; it is not the harness protocol.
The separate tau-swarm server is not a harness extension and needs no pin
change for this rollout. Discord and GitHub service are outside this flake
closure; these pins establish no compatibility claim for them.

## Deployment handoff

1. Validate the candidate with full SelfCI and build the Tau package, all seven
   extension packages, and the Telegram gateway from the candidate's locked flake.
   Verify the built peers' protocol-15 greetings without configuring credentials
   or contacting live services.
2. Publish the approved application candidate through its normal publication
   gate. SDK tags identify the earlier immutable package source, not this later
   application integration.
3. Refresh the downstream Tau input **and all seven nested extension nodes**.
   A downstream flake that overrides these inputs can retain old peers despite
   updating the core pin. Check the resolved revisions and package outputs.
4. Coordinate harnesses, UI clients and configured extensions before separately
   authorized activation. Installed packages do not prove that old processes have
   stopped. Tau rejects protocol-14 extensions before configuration.
5. Preserve a recovery plan for durable state. New readers accept old journals;
   old readers cannot decode new partial-answer/continuation enum values.
   Physical framing is unchanged, and there is no downgrade migration.

`PartialAnswer` does not make text final, and only `Continue` requests another
sample in the same outer turn. Neither grants bridge send, route, or scheduling
authority. Native Codex standalone compaction retains its existing restriction:
automatic retry is permitted only before semantic output is accepted; a later
failure discards uncommitted output and needs a distinct explicit recovery request.
