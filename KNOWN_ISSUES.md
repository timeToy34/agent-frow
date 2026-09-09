# Known issues

The central list of open bugs, known limitations, and workarounds for Agent
F-Row. Update issue status here; keep implementation details and historical
lessons in [docs/how-it-works.md](docs/how-it-works.md) and
[docs/lessons.md](docs/lessons.md).

Consolidated from the repository's records on **2026-09-06**. Dates below
describe recorded observations. Later upstream checks are recorded under
[manual changelog review](#manual-changelog-review); a documentation or
release-note change does not by itself verify behavior on an installed build.

## Open bugs

### KI-001: Codex stays Waiting after a response

**Status:** Open; reported 2026-09-06, cause not yet confirmed.

The lane can stay **Waiting** after the user responds and clear only when
another event is triggered. The affected response path — question, command
approval, or proposed plan — and the missing or delayed hook are still
unknown. The state table already clears Waiting on a main-agent
`UserPromptSubmit` or `PostToolUse`; that does not guarantee timely delivery.

**Workaround:** The lane updates when the agent reports further activity.
There is no confirmed way to make every response clear it immediately.
The known delay for approved commands is recorded separately as KI-003 below;
it does not establish the cause of this report.

**Next investigation:** Reproduce with the app launched with
`AGENT_FROW_DEBUG` set. Record the response type, agent version, and response
time, then compare `events.log` with the session's local rollout to identify
which hook was missing or late. See [diagnostics](docs/how-it-works.md#diagnostics).
Do not infer hook delivery from a tool's call/output timestamps alone.

### KI-002: Lane binds to a subfolder instead of the launch folder

**Status:** Historical open report from the 2026-08-18 release checklist;
needs reproduction on the current build.

An agent launched from a project root was shown as belonging to its
`frontend` subfolder. The prior change to take the main agent's
`SessionStart.cwd` as authoritative and ignore subagent working directories
was reported insufficient. The implementation docs describe that rule, but
the original report contains no verified resolution.

**Workaround:** None confirmed.

**Next investigation:** Launch the app with `AGENT_FROW_DEBUG` set, reproduce
the folder mismatch, and inspect the actual `cwd`, `agent_id`, and event
sequence in `events.log` before changing [the tracker](app/src/tracker.rs).
See [diagnostics](docs/how-it-works.md#diagnostics). Do not assume the hook's
reported working directory matches the launch directory.

### KI-015: Codex shows Waiting with automatic permission approval enabled

**Status:** Open investigation; reported twice on 2026-09-07.
Automatic review confirmed in session logs; the event that set the affected
lane to Waiting is still unconfirmed.

With Codex configured to approve permissions automatically, Agent F-Row
appears to receive a waiting signal even though no request is presented to
the user. The exact incoming event still needs to be captured.

**Log investigation, 2026-09-07 around 21:04 PDT:** The active
`ai-brand-dna` session was Codex CLI 0.153.4 in WSL. Its applied settings
recorded `approval_policy = "on-request"` together with
`approvals_reviewer = "auto_review"`. Its automatic reviewer returned
`allow` at 21:04:16.767 and 21:04:20.212, and both commands returned by
21:04:20.622. This verifies automatic approval activity around the report;
the affected lane was not independently identified during this inspection.

There is another possible source: that session called `request_user_input`
at 21:02:52.589 and received the answer at 21:03:07.210. A state left over
from that answered question cannot yet be distinguished from a new Waiting
caused by automatic review. At inspection, Agent F-Row's `events.log` had
not been updated since 2026-08-25; `lane-events.log` records ownership changes
only, and `hook.log` had no entries newer than 2026-09-02. These logs do not
establish which hook arrived at the app during this occurrence.

After the answer was recorded, five shell commands completed successfully
at 21:03:32.355–21:03:32.837. The next hook notification visible in that
Codex process's log was at 21:05:56.495, around turn completion; it did not
name the hook. These internal records do not prove `PostToolUse` was emitted
or delivered. The local hook configuration inspected afterward does include
an unfiltered, synchronous Agent F-Row `PostToolUse` hook. Registration on
disk does not establish that it ran in this session.

**Expected behavior:** Automatically approved work should remain Running;
Waiting should indicate that the agent actually needs user input.

**Code evidence:** The [state table](app/src/state.rs) maps a main-agent
`PermissionRequest` directly to Waiting, both for an existing session and
when adopting a new one, without checking the approval setting. This could
explain the symptom if Codex emits that hook for an automatic approval; it
does not establish that Codex did so in this report. Questions and proposed
plans can also set Waiting.

**Workaround:** None confirmed. Subsequent main-agent activity may clear the
state, but that would not prevent the false indication.

**Next investigation:** Identify the affected lane and reproduce with fresh
event logging enabled via `AGENT_FROW_DEBUG`. Record both `approval_policy`
and `approvals_reviewer`, plus the version, environment, and time of the
false indication. Compare `events.log` with the local session and automatic
reviewer rollouts to identify the event that set Waiting and whether any
user interaction was actually pending. See
[diagnostics](docs/how-it-works.md#diagnostics). Confirm how automatic and
interactive approvals differ before filtering permission events, so real
requests remain visible. Compare with KI-003's completion delay, but keep
this report distinct from [KI-001](#ki-001-codex-stays-waiting-after-a-response),
where the user had responded to a request.

## Known limitations and workarounds

These are documented integration, hardware, or distribution constraints.

| ID | Limitation | Workaround or current behavior |
|---|---|---|
| KI-003 | **Waiting can persist while an approved command runs.** The hook integration has no approval-decision event. In the recorded Codex behavior, `PostToolUse` arrives only when the command's process exits; yielding or polling a running process supplies no completion hook. | Further main-agent activity clears Waiting. A server or long install can leave it Waiting until a later command finishes. No timer guesses that approval occurred. Measured with Codex 0.149.1 on 2026-08-25: 42 seconds for a dev server, two minutes for an install. Related unresolved report: [KI-001](#ki-001-codex-stays-waiting-after-a-response). |
| KI-004 | **Codex errors remain unavailable; interrupt support is implemented locally.** Codex has no error-event registration here. | The app registers and parses `Interrupt` (upstream since 0.150.0), sets the main session to Connected with an Interrupted note, and filters delayed events from cancelled turns. Active subagents keep the display Running. Runtime validation is recorded below; error reporting remains open. See the [comparison](#codex-and-claude-code-hook-differences). |
| KI-005 | **Stock Keychron Ultra firmware ignores per-key brightness.** Affects the Ultra keyboards and V0 Ultra numpad: lit keys show at full brightness and dark keys show white. | The repository provides firmware with the per-key brightness patch, verified on a V3 Ultra 8K ANSI and two V0 Ultra ANSI numpads. Read the [firmware guide](firmware/keychron-ultra/README.md) before flashing; it covers model compatibility, recovery risks, and stock updates undoing the patch. |
| KI-006 | **Keychron lighting control does not work over Bluetooth.** The raw-HID lighting protocol is available over the cable or 2.4 GHz receiver. | Use either supported connection for lighting. Stored summon-key remaps still work over Bluetooth. See [keyboard setup](README.md#keyboards). |
| KI-007 | **A Stream Deck cannot be shared with the Elgato Stream Deck app.** Both applications paint keys and read presses. | Quit the Elgato app to let Agent F-Row use the deck. Starting it again hands the deck back within ten seconds. See [keyboard setup](README.md#keyboards). |
| KI-008 | **The Keychron Launcher key picker cannot enter the V0's required chords.** It cannot directly configure the remaining knob/M-key chords, Ctrl+Shift+F13–F20. | Export the existing keymap as a backup, then import the supplied V0 Ultra ANSI keymap over the cable. M5 becomes a lane key instead of Fn. See [keymap instructions](firmware/keychron-ultra/README.md#keymap-for-the-v0-ultra-numpad). |
| KI-009 | **Usage gauges can be stale or unavailable.** Each lane holds its session's latest reading, so quiet and active sessions on the same account can differ. Codex's bounded rollout tail may contain no general account-limit reading. | Gauges update when readings arrive. A missing reading stays at the last known value or shows a dash if none was received; model-specific readings must not replace the general allowance. Claude limits require a Pro or Max account and its status-line data after the first reply. See [gauge data sources](docs/how-it-works.md#numbers-context-and-limits). |
| KI-010 | **Codex lane reuse requires verified terminal identity.** Keeping a lane across conversations requires one Codex CLI agent per Windows Terminal tab, `WT_SESSION`, and readable session metadata. | Without that identity, conversations remain separate sessions and an older lane can remain until its session ends or is dismissed. Separate agents in the same folder must stay separate. See [terminal ownership](docs/how-it-works.md#codex-terminal-ownership). |
| KI-011 | **Release downloads are not code-signed.** Windows can warn when running a downloaded build. | Code signing remains unimplemented. See [installation](README.md#install). |

## Codex and Claude Code hook differences

Keep this comparison with the issue list as agent versions change. An
implementation difference is verified by this repository's code; an observed
runtime difference needs captured behavior from both agents. Where no
matching Claude Code reproduction exists, its behavior remains unverified
for that scenario.

| Behavior | Codex | Claude Code | Evidence and status |
|---|---|---|---|
| Questions and answers | The app registers `PreToolUse` for `request_user_input`; received `PostToolUse` clears Waiting. Recording the tool's answer does not prove hook delivery. | The app detects questions through `Notification`; subsequent main-agent activity clears Waiting. | Implemented paths: [registration](app/src/install.rs) and [state table](app/src/state.rs). Codex 0.153.4 WSL delivery is under investigation in KI-001/KI-015; no equivalent Claude trace was captured. |
| Automatic permission approval | 0.153.4 WSL logs confirm `on-request` plus `auto_review` and automatic `allow` decisions. Whether those decisions produced the reported Waiting remains unknown. | No matching automatic-approval trace has been captured for this report. | Open: [KI-015](#ki-015-codex-shows-waiting-with-automatic-permission-approval-enabled). Record approval mode and reviewer separately. |
| Long-running commands | In the 0.149.1 measurement, a yielded process supplied no `PostToolUse` until it exited; see KI-003. | The current adapter registers both `PostToolUse` and `PostToolUseFailure`. There is no equivalent timed background-command measurement here. | Historical Codex evidence: [lessons](docs/lessons.md). Recheck delivery after upgrades; do not assume identical timing. |
| Tool and turn failures | No separate failure hook is registered. Current docs describe Bash `PostToolUse` even for nonzero exits. | `PostToolUseFailure` and `StopFailure` are registered; the latter sets Error. | Implementation difference; upstream contracts: [Codex PostToolUse](https://learn.chatgpt.com/docs/hooks#posttooluse), [Claude Code failures](https://code.claude.com/docs/en/hooks#posttoolusefailure). KI-004 remains open. |
| Interrupts | `Interrupt` sets Connected with an Interrupted note; cancelled turn IDs protect a retry from delayed events. Active subagents keep the display Running until they stop. | The current adapter has no dedicated interrupt hook; it relies on subsequent activity or an applicable idle notification. | Implemented locally for Codex 0.150.0+. Verification status below; KI-004 still tracks missing error reporting. [Contract](https://learn.chatgpt.com/docs/hooks#interrupt). |
| Plan approval | `Stop` with a proposed-plan flag sets Waiting. A matching completed Plan in the rollout supplies the flag when the completion message omits it. | The recorded flow uses `ExitPlanMode` and `PermissionRequest`. | Implemented difference: [lessons](docs/lessons.md); Codex 0.153.2 workaround: KI-014. |
| Background hook execution | The app keeps hooks synchronous for compatibility with 0.147, which rejected `async`. Current Codex docs support background command hooks. | Current docs also support background command hooks. | Version-dependent compatibility constraint, not a permanent capability difference. [Codex background hooks](https://learn.chatgpt.com/docs/hooks#run-hooks-in-the-background), [Claude Code background hooks](https://code.claude.com/docs/en/hooks#run-hooks-in-the-background). |
| Context and usage gauges | Read from the session rollout; account limits require the general bucket. | Forwarded from the status-line JSON. | Implemented data-source difference: [numbers](docs/how-it-works.md#numbers-context-and-limits). Missing or stale readings: KI-009. |

For every new comparison, record the date, both agent versions when tested,
CLI/desktop and Windows/WSL environment, approval settings, action performed,
expected state, observed state, evidence, and linked issue ID. Label each
finding as observed, documented upstream, implemented locally, or unverified.

**Evidence rule:** Keep these stages distinct: the user responds; the agent
records a tool result; a hook is emitted; Agent F-Row receives it; the lane
changes state. A timestamp for one stage does not establish the next. Missing
log entries are inconclusive when the relevant trace was not enabled.

### Hook adoption candidates

**Reviewed 2026-09-08.** Interrupt was selected for implementation. Other
entries remain candidates or are explicitly deferred; adoption alone does
not establish actual hook delivery on an installed agent.

| Priority | Candidate | Potential use and limits |
|---|---|---|
| Implemented | **Codex `Interrupt`** — added in 0.150.0 | Connected with an Interrupted note, retained lane and launch folder, and protection against cancelled-turn events. It fires only for an active main-thread turn; it does not report an ordinary answer or approval. KI-004. [Contract](https://learn.chatgpt.com/docs/hooks#interrupt). |
| Deferred | **Claude Code `ElicitationResult`**, or the already received `Notification` subtype `elicitation_response` | User tested multiple MCP calls without a stuck state: their enclosing activity clears Waiting. Keep the existing handling. This hook is specific to MCP elicitation, not every question or permission prompt. [Result hook](https://code.claude.com/docs/en/hooks#elicitationresult), [notifications](https://code.claude.com/docs/en/hooks#notification). |
| Medium | **Claude Code quota-resume notifications** — added in 2.1.234 | `quota_auto_resume_stale` could show Waiting because Enter is required; `quota_auto_resume_fired` could show resumed activity. These subtypes are currently unrecognised. Verify the full stop/resume sequence before mapping states. [Contract](https://code.claude.com/docs/en/hooks#notification). |
| Deferred | **Both agents: `PreCompact` / `PostCompact`** | Usually occurs within a larger sequence; no separate waiting state or new handling is needed. [Codex](https://learn.chatgpt.com/docs/hooks#precompact), [Claude Code](https://code.claude.com/docs/en/hooks#precompact). |
| Deferred diagnostic | **Claude Code `CwdChanged`** | Could record directory changes when investigating KI-002. A session must stay attached to its initial launch folder when the agent changes directories; current cwd must not replace lane identity. [Contract](https://code.claude.com/docs/en/hooks#cwdchanged). |
| Lower | **Claude Code `PostToolBatch`** | One event after a tool batch resolves could simplify activity diagnostics. Existing per-tool events already cover much of this; it is not proof that a particular permission prompt was answered. [Contract](https://code.claude.com/docs/en/hooks#posttoolbatch). |

`Interrupt` uses the existing forwarded `turn_id`, a synchronous three-second
timeout, and an empty hook response. The tracker remembers cancelled turn IDs
for the session's lifetime, ignores duplicates and malformed interrupts, and
does not let late tool results replace the current turn identity. A delayed
interrupt for another turn is recorded for filtering without changing the
current lane. Payloads without turn IDs retain their existing activity rules;
an interrupt itself requires a usable ID. Subagent updates and `SessionEnd`
remain independent. The Interrupted note survives subagent completion.

**Implementation verification — 2026-09-08:** Windows application tests passed,
including cancellation from Running/Waiting, retries, delayed events,
subagent completion, malformed IDs, folder retention, and registration
preservation. Hook lint/silence tests, formatting, and the Windows release
build passed. The tested build is installed; Windows and WSL Codex
registrations contain Interrupt.

**Live WSL verification — 2026-09-08, Codex 0.153.4:** The user interrupted
this implementation session and confirmed that the behavior worked. The app's
`events.log` records `Interrupt` at **23:57:37.383 PDT** (line 532), followed
by `UserPromptSubmit` at **23:57:48.019 PDT** (line 533). Both use session
`01a07a2e-6075-7d73-846c-8846f6315496`; the prompt carries a new turn ID.
This verifies app-side hook reception and the user-observed cancellation
behavior. Retry races and surviving-subagent behavior are covered by automated
tests, rather than claimed as separate live reproductions. The WSL entry is
trusted. Windows Codex's entry still needs trust in Settings → Hooks and a
separate live test; Codex error reporting remains open.

The current Codex hook reference has no dedicated question-answer or
approval-result event. `Interrupt` therefore does not resolve KI-001/KI-015's
normal-response delivery investigation. Its specialized-tool coverage also
needs measurement for the exact question and code-mode paths.
[Codex hook reference](https://learn.chatgpt.com/docs/hooks).

### Manual changelog review

**Maintenance note:** Regularly check both agents' changelogs for improvements
in hook support, and check again after an agent upgrade or before changing
hook handling. This is a manual project-maintenance step, with no scheduled
or automated review.

- Read the [Codex changelog](https://learn.chatgpt.com/docs/changelog) and
  [Claude Code changelog](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md),
  then consult their [Codex](https://learn.chatgpt.com/docs/hooks) and
  [Claude Code](https://code.claude.com/docs/en/hooks) hook references for the
  exact behavior.
- Check new events, question/approval completion, automatic approvals,
  interrupts/errors, tool coverage, payload fields, async execution, trust,
  ordering, and delivery fixes. Compare them with the rows and open issues
  above.
- Record the review date, release versions, source links, relevant changes,
  and what needs testing. Keep issues open until the affected scenario is
  verified locally; release notes alone do not close them.

**First manual review — 2026-09-07:**

- Codex 0.150.0 (2026-08-26) added `Interrupt`, making the old absence claim
  obsolete. Follow up on KI-004's missing registration and parser support.
  Codex 0.153.0 also changed automatic-review behavior; that is relevant to
  KI-015 but does not establish a fix for Waiting.
  [Codex changelog](https://learn.chatgpt.com/docs/changelog).
- Claude Code 2.1.261 reports a fix for restored sessions losing hook output
  around parallel tool calls. The latest 2.1.263 entry contains only general
  reliability fixes. Neither entry proves the Codex Waiting issue resolved.
  [Claude Code changelog](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md).
- Codex's current hook reference documents tool-output hooks but also allows
  specialized tool paths to opt out. Verify `request_user_input` and nested
  code-mode command delivery on the affected 0.153.4 build; the general
  contract does not establish delivery for this occurrence.
  [Tool coverage](https://learn.chatgpt.com/docs/hooks#tool-coverage).

## Fixes included in 0.8.1

These reports have corresponding changes in the 0.8.1 source and release
package. The evidence below distinguishes automated checks from recorded
hardware behavior; publication alone does not establish a runtime result.
Older resolved bugs and the reasons behind their fixes remain in
[the lessons](docs/lessons.md#stories-behind-the-rules).

| Report | Current implementation | Evidence |
|---|---|---|
| KI-012: **Codex seven-day gauge showed 0% despite account usage** (investigated 2026-09-06). | General limits accept the `codex` bucket and legacy absent/null IDs; model-specific buckets such as Spark cannot overwrite them. Context counts can still update. KI-009 remains a limitation. | [Gauge parser and regression tests](app/src/gauges.rs). |
| KI-013: **A new Codex conversation left the old lane behind** (investigated 2026-09-06). | Verified CLI conversations in the same terminal tab reuse the lane, name, and colour. Delayed events cannot restore the old conversation; an explicit foreground start or prompt can switch back. KI-010 describes the scope. | [Tracker](app/src/tracker.rs), [terminal lane regression tests](app/tests/terminal_lanes.rs), [ownership journal](docs/how-it-works.md#diagnostics). |
| KI-014: **Codex plan approval was missed when the completion message was null** (observed with 0.153.2 on 2026-09-05). | On `Stop`, the app also checks the rollout tail for a completed, nonempty Plan matching that turn. Only the boolean is retained; another turn's plan cannot trigger Waiting. | [Rollout reader and regression tests](app/src/gauges.rs). |

### KI-016: V0 answer keys sometimes scroll the terminal

**Status:** Fixed for the reported rapid-press case; user verified 2026-09-09
after importing the new JSON into the V0 and running the updated app.
Reported 2026-09-09 with Codex in Windows Terminal. The modifier-normalization
fix was installed and improved behavior, but the user still reproduced
scrolling with very rapid V0 presses. Claude has not been tested for this report.

While Waiting, the Keychron V3 F-row reliably moves the prompt selection,
but the original V0 Up/Down controls sometimes scroll the whole terminal.
The V3 sends bare F13–F24; the original V0 map sent Ctrl+Shift+F13–F24.
Both paths previously injected an arrow without checking held modifiers.
Ctrl+Shift+Up/Down are Windows Terminal's scrolling shortcuts. Normalizing
modifiers improved the physical behavior but did not resolve rapid presses;
the remaining timing issue has not been isolated in a hardware trace.
[SendInput behavior](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput),
[Terminal scrolling shortcuts](https://learn.microsoft.com/en-us/windows/terminal/customize-settings/actions#scroll-up).

**Current change:** Preserve answer-on-press behavior, but replace the V0
row's summon/Up/Down/Enter chords with bare Intl1/Intl5/Intl6/Keypad Comma.
The app removes Ctrl+Shift+F21–F24 registrations entirely. The knob and M1–M5
retain Ctrl+Shift+F13–F20, so overlapping those controls with an answer can
still introduce modifiers. The shared sender keeps its modifier normalization
and partial-send cleanup; foreground verification and repeat suppression remain.

**Setup:** Update the app and import the revised
[V0 Launcher JSON](firmware/keychron-ultra/keymaps/keychron_v0_ultra_ansi.json)
together, exporting the current map first. No firmware flash is required.
The four spare key mappings and registrations were checked on US Windows
layout `00000409`; other layouts require validation. See
[keymap instructions](firmware/keychron-ultra/README.md#keymap-for-the-v0-ultra-numpad).
Use the V3 F-row until the new app and V0 keymap are both in place.

**Automated verification:** All 176 Windows library tests passed, including
the shipped-keymap/registration contract, removal of the old top-row chords,
individual V0 registration failures, and cleanup on F-row registration failure.
The existing focus/input checks cover all three answer keys across 16
left/right Ctrl/Shift combinations, partial-send recovery, and native scan
flags. The JSON checksum and preservation of every other key, the knob,
board ID, and second layer were checked; formatting and local documentation
links passed.

**Hardware verification:** In the test question, the user rapidly pressed
Up/Down on the V0 and reported **Selection moves only**, with no terminal
scrolling. The installed executable matched the tested release build;
Windows checks confirmed the new hotkeys were held and the old top-row
chords were free. Held-key behavior, focus/tab switches, overlapping knob/M
chords, and an equivalent Claude Code test were not separately confirmed in
this test. Record any new reproduction here.

## Maintaining this list

Add new reports with a symptom, status, observed date/version when known,
workaround, and next investigation or verification step. Keep the issue ID
when its status changes, and record what verified a fix. Link to this file
from other docs instead of maintaining another open-bug list.
