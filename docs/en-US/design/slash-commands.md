# Slash command design

Status: **Proposed; not yet implemented.**

Latte Code currently has a local `Ctrl+P` command palette with four actions,
but composer text beginning with `/` is submitted as an ordinary prompt. This
document defines the target slash-command contract for the transcript TUI. It
does not claim that the commands described here are already available.

## 1. Reference findings

The design was informed by the following local source snapshots. Paths are
listed as evidence, not as dependencies of Latte Code.

| Agent | Snapshot | Relevant behavior |
| --- | --- | --- |
| Codex | `1f0566d3f59298d1bb88820a0d35294f1eeb07ea` | `codex-rs/tui/src/slash_command.rs` defines a typed built-in enum with aliases, descriptions, inline-argument support, platform visibility, and active-task availability. `bottom_pane/slash_commands.rs` applies one availability filter to lookup and popup construction. `chatwidget/slash_dispatch.rs` maps recognized commands to explicit application actions. |
| OpenCode | `c69abee0c73253aebae65e87e4e1b9bfa8c38021` | `packages/tui/src/keymap.tsx` derives slash entries from the same reachable command registry used by the command palette. `packages/opencode/src/command/index.ts` merges built-ins, configured prompt commands, MCP prompts, and skills. `packages/opencode/src/session/prompt.ts` expands prompt templates and routes them through the normal Session prompt path. |
| Claude Code local reference mirror | `5a774a2b62d7949c1d94e0b726281554d7893cfd` | `src/types/command.ts` separates local, local-UI, and prompt commands and carries aliases, availability, argument hints, source, sensitivity, and invocation policy. `src/utils/suggestions/commandSuggestions.ts` ranks exact, alias, prefix, description, and recent-use matches. `src/utils/processUserInput/processSlashCommand.tsx` applies remote-safety and user-invocation checks before dispatch. |

The reusable conclusions are:

- Command metadata, popup visibility, exact lookup, and dispatch availability
  must come from one catalog.
- Local UI actions, typed application actions, and Provider-visible prompt
  commands are different execution kinds.
- Availability must be checked again at dispatch time, not only when the popup
  is built.
- Dynamic command sources need visible provenance and collision handling.
- A command must never gain authority merely because it was entered with `/`.

Latte Code intentionally does **not** copy OpenCode's command-template shell
interpolation. Prompt templates are text expansion only; they cannot run a
shell, read environment variables, or perform file I/O while being resolved.

## 2. Goals and non-goals

Goals:

- Complete the primary Session loop with a transient new-Session draft and a
  global Session picker/resume path.
- Discover commands by typing `/` at the beginning of the composer.
- Make keyboard shortcuts, `Ctrl+P`, and slash aliases converge on the same
  command identifiers and availability rules.
- Preserve the pure TUI reducer and the privileged `latte-engine` boundary.
- Distinguish local controls from commands that become model-visible prompts.
- Support deterministic matching, arguments, aliases, disabled reasons, and
  future trusted prompt-command sources.
- Keep command validation errors, popup state, and Provider startup failures
  transient.

Non-goals for the first delivery:

- Arbitrary shell commands, executable plugins, or command handler scripts.
- Workspace-defined overrides of built-in commands.
- MCP prompt commands or model-invocable Skills.
- A second runtime API that bypasses `ThreadRuntimeService` or Engine effects.
- Slash-command parsing in `latte-code --json run`; headless automation keeps
  its explicit CLI subcommands and treats prompt strings literally.

## 3. Command model

The catalog exposes secret-free descriptors similar to:

```rust
pub struct CommandDescriptor {
    pub id: CommandId,
    pub name: String,
    pub aliases: Vec<String>,
    pub description: String,
    pub category: CommandCategory,
    pub kind: CommandKind,
    pub arguments: ArgumentPolicy,
    pub concurrency: ConcurrencyPolicy,
    pub source: CommandSource,
}

pub enum CommandKind {
    LocalUi,
    TypedAction,
    PromptTemplate,
}

pub enum CommandAvailability {
    Enabled,
    Disabled { reason: String },
    Hidden,
}
```

The three kinds have deliberately different authority:

| Kind | Result | Provider-visible | Persistent |
| --- | --- | --- | --- |
| `LocalUi` | Pure reducer state change such as opening Help or entering Navigation | No | No |
| `TypedAction` | Existing or newly defined `ThreadUiAction` handled by the composition root | No, unless the resulting domain operation normally produces conversation content | Only the domain state owned by the called service |
| `PromptTemplate` | Bounded text expansion submitted through the ordinary Start/Follow-up path | Yes | The exact expanded user content and bounded invocation metadata |

The catalog contains metadata and identifiers, not arbitrary callbacks. A
built-in command resolves to a closed Rust enum. A future dynamic prompt
command resolves only to a `PromptCommandId`; it cannot manufacture a generic
Engine action.

## 4. One catalog for palette and slash input

`Ctrl+P` and the slash popup query the same catalog with the same
`CommandContext`. The context contains only the state needed for availability:

- Current focus and overlay ownership.
- Connection state.
- Whether a Session exists.
- Current Session lifecycle and pending request kind.
- Whether a submission or one queued follow-up already exists.
- Platform and enabled product features.
- Whether the Workspace is trusted for future Workspace command sources.

`Hidden` means the command does not apply to this build, platform, feature, or
user. `Disabled` means the command is relevant but unsafe in the current state;
the UI shows a bounded reason. Dispatch evaluates the context again so stale
popup state cannot execute a command after the Session changes.

The current private `PaletteCommand` list becomes catalog-backed. A command may
have a palette entry without a slash alias, but no second list of slash-only
built-in handlers is allowed.

## 5. Parsing and recognition

Slash recognition uses these deterministic rules:

1. Only composer content whose first byte is `/` is a candidate. Leading
   whitespace makes it an ordinary prompt.
2. The command name is the first token on the first logical line. The rest of
   the first line plus later lines is the opaque argument string.
3. Canonical names and aliases are lowercase ASCII and match
   `[a-z0-9][a-z0-9:_-]{0,63}`.
4. Lookup is case-sensitive and requires an exact canonical name or alias.
5. Internal newlines in arguments are preserved; only outer whitespace is
   trimmed. Platform shell parsing is never used.
6. A known command with invalid or forbidden arguments returns a local
   validation error and preserves the complete composer draft.
7. An unknown or syntactically invalid candidate remains an ordinary prompt.
   Therefore `/tmp/file`, `/a/b`, and an unknown `/example` are not captured by
   the command system.

Recognition is separate from suggestion matching. The popup may use exact,
prefix, alias-prefix, and bounded fuzzy matches, but dispatch never executes a
fuzzy result unless the user explicitly selects it.

Paste never executes a command. It only changes the composer and the user must
still submit or select a result.

## 6. Composer interaction

When the composer begins with `/` and the caret remains in the first token, a
popup opens directly above the composer. It shows at most ten rows containing:

- Canonical `/name`.
- Description.
- Source badge for non-built-ins.
- Argument hint.
- Disabled reason when applicable.

Interaction rules:

- Up/Down changes selection with wrapping; PageUp/PageDown moves by a page.
- `Tab` completes the selected canonical name and adds one space when the
  command accepts arguments.
- `Enter` executes an exact enabled no-argument command or the explicitly
  selected enabled result.
- Selecting a command with required arguments completes it into the composer
  instead of executing an incomplete invocation.
- `Esc` closes the slash popup first. A later `Esc` keeps the current behavior
  of entering transcript Navigation.
- Backspace removing the leading `/` closes the popup without changing other
  composer content.
- A disabled selection remains non-executable and displays its reason without
  clearing the draft.

The popup is not a centered modal and does not replace `Ctrl+P`. `Ctrl+P`
remains the complete action palette; slash input is the fast composer path.

## 7. Dispatch and authority boundary

Dispatch follows this flow:

```text
composer text
-> parse candidate
-> exact catalog resolution
-> re-evaluate availability
-> validate arguments
-> dispatch by closed CommandKind
```

`LocalUi` commands call pure reducer transitions. `TypedAction` commands emit
an explicit `ThreadUiAction` variant. The `latte-code` composition root maps
that variant to a specific `ThreadRuntimeService` method or local terminal
action. `latte-engine` never receives a command name string and never exposes a
generic `execute_slash_command` method.

A future `PromptTemplate` command emits a bounded prompt-command request to
`latte-headless`. The resolver expands text and returns an ordinary prompt;
Start or Follow-up then uses the same submission identity, queue, Provider,
tool, permission, Effect, and recovery path as manually entered text.

Consequences:

- `/refresh` can only emit `RefreshSnapshots`.
- `/cancel` can only emit the existing typed `Cancel { thread_id }` action.
- `/help` cannot create a Session or call a Provider.
- A Prompt command can influence the model, but every later tool remains under
  Engine preparation, permission, fencing, and observation.
- No command may directly write files, start a process, alter SQLite, or append
  JSONL from the TUI reducer.

## 8. Lifecycle and concurrency

Each descriptor declares an explicit concurrency policy:

- `Always`: safe local inspection or terminal action.
- `SessionRequired`: needs a selected Session but not necessarily an idle Run.
- `IdleOnly`: changes Session configuration or lifecycle and requires `Ready`.
- `RunningOnly`: meaningful only for an active Run, such as `/cancel`.
- `PromptLike`: follows the same current submission and one-follow-up queue
  contract as ordinary composer text.

Permission, input-request, and reconciliation states continue to own their
entire key event. The slash popup cannot open in those states. In particular,
Enter cannot become an approval or reconciliation shortcut through a command.

Local and typed action commands are never put in the Provider follow-up queue.
Prompt commands use the existing single queued follow-up; the system does not
create a parallel command queue.

## 9. Persistence, history, and telemetry

The persistence contract follows the global data-storage design:

- Popup filters, selection, validation errors, disabled reasons, and local
  command output are in-memory presentation state.
- `LocalUi` invocations do not create Session, Run, SQLite, or JSONL records.
- `TypedAction` commands persist only the authoritative domain transition they
  invoke. The literal `/command` text is not a conversation message.
- A `PromptTemplate` command persists the exact expanded, Provider-visible
  user message. Bounded metadata may record canonical name, source class, and
  template SHA-256 so replay does not depend on the current template file.
- Failed template validation or expansion leaves no Session content and
  restores the original composer invocation.
- Provider startup failures remain transient as defined by the data-storage
  design.

Composer recall may retain a submitted invocation in process-local history,
but it is not Session history. Sensitive command arguments are never included
in telemetry. Telemetry may record an allowlisted built-in name or a coarse
source such as `user`/`workspace`/`mcp`; it does not record raw arguments,
template content, absolute paths, or credentials.

## 10. Prompt-command extension contract

Dynamic prompt commands are deferred until after the built-in catalog is
stable. The intended sources are:

```text
~/.latte/latte-code/commands/<name>.md
<workspace>/.latte/commands/<name>.md
```

User commands may be added first. Workspace commands remain hidden until Latte
Code has an explicit Workspace-trust decision. Merely opening a repository is
not consent to load its prompt commands.

Extension requirements:

- UTF-8 regular files only, opened without following symlinks.
- Maximum 128 discovered commands and 64 KiB per source file.
- Strict front matter for name, description, argument hint, and optional model
  policy; unknown fields are rejected.
- Built-in names and aliases are reserved and cannot be shadowed.
- Any canonical-name or alias collision between dynamic sources disables the
  conflicting commands and reports a local diagnostic; source order never
  silently selects a winner.
- `$ARGUMENTS` and bounded positional `$1` through `$9` are pure text
  substitutions using a platform-independent lexer.
- Shell blocks, command substitution, environment expansion, includes outside
  the command root, and executable hooks are rejected.
- Catalog reload is atomic. Invalid dynamic files do not remove built-ins or
  partially replace the previous valid catalog.

MCP prompts and Skills may later adapt into `PromptTemplate` descriptors, but
they must carry source badges, explicit user-invocation capability, bounded
content, and the same collision and trust checks. They do not receive a new
execution kind.

## 11. Initial command inventory

The first product slice completes the Session loop rather than prioritizing the
commands that happen to be easiest to map from the current palette:

| Command | Aliases | Kind | Availability | Mapping |
| --- | --- | --- | --- | --- |
| `/new` | – | `LocalUi` | No active Run or blocking request | Switch to a transient `NewSessionDraft`; create no durable Session yet. |
| `/sessions [query]` | `/resume` | `TypedAction` plus local picker | No active Run or blocking request | With no argument, load and open the global Session picker. With an ID or title query, resolve and open that Session directly. |

`/new` does not clear, archive, or delete the current Session. The TUI needs an
explicit active-conversation target such as:

```rust
pub enum ActiveConversation {
    NewSessionDraft,
    Session(ThreadId),
}
```

Entering `NewSessionDraft` clears only the new draft composer and local
selection state. The previous Session remains discoverable. Its first prompt
uses the normal Start path and follows the data-storage commit point: no
Session row or content file is created until the first complete valid Provider
outcome.

`/sessions` is the canonical discovery command and `/resume` is an exact alias.
The no-argument form opens a bounded picker backed by the global SQLite
Session catalog. Rows contain only Session metadata such as title, Project,
Workspace, lifecycle, model, and update time; transcript JSONL is loaded only
after selection. An optional argument first tries an exact Session ID and then
a deterministic title match. Ambiguous titles stay in the picker instead of
silently choosing one.

Selecting a row emits an explicit typed open/resume action. It reloads the
chosen Session's JSONL conversation and SQLite control projection without
calling the Provider or adding a conversation entry. If its original Workspace
is unavailable, the existing data-storage rebinding contract requires an
explicit valid Workspace choice.

Until background Session ownership is designed, both commands are disabled
while a Run is active or a permission, input, or reconciliation request owns
the interaction. They never detach an active Run implicitly.

The following commands can reuse the same catalog immediately afterward, but
they do not define the primary product milestone:

| Command | Aliases | Kind | Mapping |
| --- | --- | --- | --- |
| `/help` | – | `LocalUi` | Open the existing Help overlay. |
| `/navigation` | `/nav` | `LocalUi` | Enter transcript Navigation. |
| `/refresh` | – | `TypedAction` | Emit `RefreshSnapshots`. |
| `/cancel` | – | `TypedAction` | Emit `Cancel { thread_id }` for an active cancellable Run. |
| `/quit` | `/exit`, `/q` | `TypedAction` | Emit `Quit`. |

Later built-ins may add `/status`, `/fork`, `/rename`, `/compact`, `/model`,
`/permissions`, and `/diff`, but only after each has a typed service contract
and lifecycle policy. `/init` and `/review` should be the first built-in
`PromptTemplate` commands after prompt expansion exists.

This inventory is a delivery order, not a compatibility promise. Command names
become public only when implemented, documented, and covered by final-binary
E2E tests.

## 12. Module placement

The intended implementation keeps responsibilities narrow:

```text
latte-tui/src/command.rs
  built-in identifiers, descriptors, parser, catalog matching, availability

latte-tui/src/thread.rs
  popup state, ActiveConversation, Session picker, reducer integration,
  rendering, typed action emission

latte-core/src/command.rs             (future PromptTemplate phase)
  secret-free dynamic descriptor and PromptCommandId wire types

latte-headless/src/command.rs         (future PromptTemplate phase)
  trusted discovery, validation, pure bounded template expansion

latte-code/src/lib.rs
  composition-root mapping from explicit ThreadUiAction variants to services,
  typed global Session catalog/open adapter
```

There is no slash-command router in `latte-engine`. Privileged work remains in
existing Engine APIs and typed service methods.

## 13. Error and recovery behavior

- Parse or argument validation errors keep the draft byte-for-byte and show a
  bounded local message.
- A command that becomes disabled between popup selection and Enter is rejected
  locally after the second availability check.
- A failed local UI command closes no unrelated overlay and changes no durable
  state.
- A failed typed action uses the existing secret-safe `ThreadUiFeedback`
  channel and authoritative snapshot reload where required.
- A failed PromptTemplate load or expansion restores the invocation and does
  not call the Provider.
- Once a PromptTemplate has become an ordinary submitted prompt, recovery is
  exactly the normal Start/Follow-up recovery contract; command code does not
  infer success.

All error text is control-character filtered and bounded before presentation.

## 14. Required verification

Unit tests must cover at least:

- Candidate parsing, multi-line arguments, aliases, and command-name limits.
- Unknown slash text and absolute paths remaining ordinary prompts.
- Exact recognition being independent from fuzzy suggestions.
- Deterministic ranking and stable selection after filtering.
- Catalog collision, reserved built-in names, source badges, and atomic reload.
- Hidden/disabled/enabled availability and dispatch-time revalidation.
- Argument policies and draft preservation on every validation failure.
- No shell/environment/include expansion in PromptTemplate resolution.
- Unicode display width, narrow-terminal popup layout, and bounded rendering.
- Permission, input-request, and reconciliation event ownership.

Final-binary E2E must cover at least:

- Typing `/` shows the command popup and does not call the Provider.
- `/new` leaves the previous Session unchanged, switches to an empty transient
  draft, and creates no persistent Session before a valid Provider outcome.
- `/sessions` lists bounded metadata from the global catalog without loading
  every transcript or calling the Provider.
- `/resume <session-id>` opens the exact Session; ambiguous title matches remain
  in the picker and require explicit selection.
- Session selection reloads JSONL content and SQLite control state, and a
  missing Workspace requires explicit rebinding.
- `/new` and `/sessions` are rejected without losing the draft while a Run or
  blocking request is active.
- `/help` works before any Session exists and leaves persistent storage empty.
- `/refresh` goes through the projection adapter.
- `/cancel` uses the typed cancellation path for an active Run.
- `/quit` exits and restores terminal modes.
- `/tmp/file` and an unknown slash prefix are submitted as ordinary prompts.
- Invalid arguments preserve the composer and do not call the Provider.
- Slash commands cannot approve permission or acknowledge an unknown Effect.
- A future PromptTemplate sends the exact expanded prompt while its later tool
  request still passes through Engine permission and Effect gates.
- Popup rendering and keyboard behavior pass on Linux, macOS, and the supported
  Windows terminal harness.

These tests are part of the existing independent UT 95%, final-binary E2E 80%,
and all-target 90% coverage gates.

## 15. Delivery phases

1. Add the single built-in command catalog, parser, availability evaluation,
   aliases, and an explicit `ActiveConversation` target that can represent a
   transient new-Session draft.
2. Add the typed global Session catalog/open boundary and bounded Session
   picker; implement `/new` and `/sessions` with `/resume` as its alias.
3. Add final-binary E2E for new, discovery, direct resume, Workspace rebinding,
   active-Run blocking, and cross-platform popup/picker rendering.
4. Fold the existing Help, Navigation, Refresh, Cancel, and Quit actions into
   the same catalog and slash popup.
5. Add trusted user PromptTemplate discovery and pure bounded expansion;
   implement `/init` and `/review` on the ordinary prompt path.
6. Add Workspace trust and Workspace PromptTemplate sources.
7. Evaluate MCP prompt and Skill adapters only after provenance, collisions,
   limits, persistence metadata, and E2E gates are in place.
