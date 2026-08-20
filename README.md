# Agent F-Row

[![CI](https://github.com/timeToy34/agent-frow/actions/workflows/ci.yml/badge.svg)](https://github.com/timeToy34/agent-frow/actions/workflows/ci.yml)

Shows what your coding agents are doing, on your keyboard's RGB F-row — and
one press on a lane's key brings that agent's window forward: its terminal,
desktop app, or IDE.

Windows only. Display only — it never sends anything to an agent, and cannot
approve, deny, or answer anything. Its reply to every hook is empty.

Supports four flavors: **Claude Code** and **Codex**, each running natively on
Windows and inside WSL.

![The Agent F-Row window: lanes, keyboard, agents](docs/screenshot.png)

Two keyboard facts up front: the summon keys are **F13–F24**, and the lighting
drives a Corsair keyboard through iCUE. On a Corsair board, remap the F-row to
F13–F24 in iCUE — **in the default profile**, or a profile switch silently
takes the summon keys with it. Without a Corsair board the window still shows
everything; the keys and the lighting are the part you'd be missing.

## Build and run

The workspace is the repository; it depends on nothing outside it.

```
cargo build                            # or: cargo build --release
target\debug\agent-frow.exe            # tray app and setup window
target\debug\agent-frow.exe doctor     # what is installed, and whether it works
target\debug\agent-frow.exe install --dry-run
```

**Two locations, and this is the one thing that catches you out:**

- `target\debug\` (or `target\release\`) is where `cargo build` writes the
  freshly compiled `agent-frow.exe`. Building updates *only* here.
- `%LOCALAPPDATA%\agent-frow\` is where the app **runs from** in normal use, and
  the only path an agent's hook ever names. Nothing but `install` copies a build
  there.

A bare **release** exe launched from anywhere else installs itself there and
hands over (that is the zip's whole setup story). Debug builds and the
explicit `agent-frow.exe run` subcommand never self-install — running a build
in place is the developer's move, and it stays theirs.

So after every rebuild, `install` is the step that actually deploys it:
`agent-frow.exe install` copies `agent-frow.exe`, `agent-frow-hook.exe`, and
`iCUESDK.x64_2019.dll` into `%LOCALAPPDATA%\agent-frow\` (renaming the running
copy aside so it can overwrite it) and registers the hook with every agent it
finds. **If you rebuild but do not install, the running tray app stays the old
one** — a fresh `target\...\agent-frow.exe` next to a stale
`%LOCALAPPDATA%\...\agent-frow.exe` is exactly that mistake.

Agents read hooks at startup, so restart them afterwards. **Codex additionally
requires you to run `/hooks` inside it and trust the entry** — until then it
looks installed and nothing happens, which is exactly what `doctor`'s
"registered but never seen" is there to tell you.

Developing against the live app: rebuild, then `install`, then relaunch
`%LOCALAPPDATA%\agent-frow\agent-frow.exe`. `shot.ps1` screenshots the
window to `window.png` for checking UI changes by looking, not by reasoning
about layout code. Both `shot.ps1` and `window.png` are development scratch and
are git-ignored.

Launching the app with `AGENT_FROW_DEBUG` set (any value) appends every hook
event's identity fields — `src`, event name, session id, `cwd`, agent id and
type — to `~/.agent-frow/events.log`, the diagnostic for questions like "what
does this agent actually report as its working directory?". Off by default;
the log self-truncates past 256 KB.

## How it fits together

```
agent-frow.exe            tray, setup window, ingress on 127.0.0.1:47115
   ▲  POST /hook, header x-agent-frow-token
agent-frow-hook.exe       what the agents run
   ▲                        ▲
Windows agents        WSL agents, via /mnt/c/... interop
```

A WSL agent runs the **Windows** executable through interop. It therefore
executes on the Windows side and reaches loopback directly, which is why there
is no component inside WSL, no host-only address, and no firewall rule.

Detection works the same way in reverse: `\\wsl.localhost\<distro>\home\<user>\`
is readable from Windows, so WSL agents are found and configured from here.

## What a lane shows

Six states: **Connected** (alive, nothing run yet), **Running**, **Waiting**
(needs you), **Done**, **Error**, and **Idle** — nothing heard for a while.
Idle is the one state no event sets: it reports silence, a fact about the
wire, not a guess about the agent. An interrupt lands here too — the agent
sits back at its prompt, and interrupting is something *you* did, not an
alarm. The transition table is
`app/src/state.rs` and it is a total function over (state, event) — no request
ids, no queues, no tombstones, no timers. **Activity clears Waiting**, so
nothing has to be correlated to anything.

None of its conditions are stylistic — each prevents an observed failure and
has a test named after it:

- `SessionStart{source: "compact"}` changes nothing. Claude compacts *mid-turn*;
  without the guard a live turn drops to Connected until the next `Stop`.
- An event carrying `agent_id` or `agent_type` is **liveness only**. Subagent
  events carry the *parent's* `session_id`, so acting on one lets a subagent
  finishing a tool clear a Waiting another one raised.
- `idle_prompt` demotes to Idle only **from Running**. It means "a prompt has
  sat unanswered", which is equally true of a permission dialog nobody has
  answered, so from Waiting it changes nothing. It is also why an interrupted
  turn dims about a minute late: no hook fires at the moment of an interrupt —
  Claude only reveals one by idling with a turn still open (~60 s).
- `PermissionDenied` promotes Waiting to Running: the prompt was answered with
  a no — by the user, a rule, or an interrupt — so it is no longer pending,
  and the turn is formally still open. If the interrupt killed the turn, the
  idle notification says so a minute later.
- `PostToolUse` promotes **only from Waiting**. Hook processes post
  concurrently, so one emitted before `Stop` can arrive after it.

Unknown `notification_type` values are ignored and **counted**, and shown in the
window as a number, so a new agent release is a line you can read rather than
behaviour nobody can explain. Same for hook events we do not register.

There is no handshake and nothing to seed: an event from a session we have never
seen creates it and infers the state from the event itself. An agent started
before the app, after it, or an hour ago all behave identically.

**Known limitation, stated in the window rather than hidden:** no agent emits an
event when you *answer* a prompt. The next observable event is that tool
finishing, so a lane can read Waiting while the approved tool already runs —
seconds usually, up to about a minute. Bounded and self-clearing.

**Silence is reported, never punished.** A session leaves only on `SessionEnd`
or the ✕ — never on a timer, because the user who stepped away comes back to
the board they left. Instead, silence demotes: Done and Connected dim to Idle
after 30 minutes, Running after 2 hours (a killed terminal stops glowing
blue). Waiting and Error hold through any amount of time — they are exactly
what you left to come back to. Any event from the agent revives
its lane where it stood.

## Lanes

Twelve F-row keys, in 3, 4 or 6 lanes. A session takes the lane bound to its
`(agent, project folder)` if that lane is free, else the first free unbound
lane; a bound lane is borrowed only when nothing else is free, and never by a
session that has not yet reported its working directory — a bind match cannot
be proven without one, and the session claims properly the moment it arrives.
The rest live **off the keyboard**: full cards in the window — state, note, timer,
Focus, dismiss — just no key and no light. An off-keyboard session stays fully
tracked, is promoted oldest-first the moment a lane frees or the layout grows,
and the landing spot for the next agent is drawn whenever every lane is taken,
so the board never runs out of room.

**Nothing ever takes a lane away from a session
that already has one** — lane position is identity, and a display you glance at
teaches you nothing if lane 2 moves while you are looking at it. The *user* has
two sanctioned exceptions: the ⏶⏷ buttons reorder lanes (everything — session,
name, colour, binding, keys — travels together), and an off-keyboard card's ⏶
takes the bottom lane, its incumbent stepping off the keyboard in trade.

A session's **project folder** is the *main* agent's launch directory: it is
taken from `SessionStart`'s `cwd` (authoritative) and, until that arrives, from
the first non-subagent event that carries one. A **subagent's** `cwd` never sets
it — a subagent working in `…/frontend` under a project rooted at `…/` must not
bind the lane to the subfolder, which was a real "bound to the wrong folder"
bug.

Lane names, colours, bindings and the lane count live in
`%LOCALAPPDATA%\agent-frow\settings.json`, written atomically. A file that does
not parse is refused and left exactly as it is: the window says so, and says
that changing anything will overwrite it.

The lane name is load-bearing: focus finds a terminal tab by it.

## The keyboard

Corsair, through the iCUE SDK. Two constraints:

- **Only ever the twelve LEDs we own.** The SDK sets only the LEDs it is
  handed, so naming twelve leaves every other key to the user's own profile —
  and touching more is a way of spelling "black out the whole keyboard".
- **Shared layer priority 128, never exclusive control.** Exclusivity is per
  *device*, not per LED, so asking for it stops iCUE rendering the keyboard and
  every key outside the F-row goes dark whether we paint it or not.

F13–F24 are registered as global hotkeys with `MOD_NOREPEAT`. That both keeps
the remapped keys out of other applications and makes Windows deliver the
physical input to Agent F-Row before it raises a terminal. A low-level keyboard
hook must not replace this: swallowing the key inside `WH_KEYBOARD_LL` leaves
the app without foreground permission, which makes summon fail specifically
when the Agent F-Row window is focused even though the Focus button works.

**The lane's colour is the base for everything, and colour change means
trouble.** Every ordinary state is the lane's own colour at some brightness and
motion; red is reserved for Error and nothing else may be red, which is why
lane colours should stay away from it.
The patterns (n = keys per lane):

| State | Pattern |
|---|---|
| empty lane | all off |
| Connected | all keys, base colour, 20% |
| Running | base 20% glow with one 100% light crossfading across n+1 slots |
| Waiting | leftmost key 100%; the rest double-pulse base up to 100% |
| Done | leftmost key 100%; the rest 20% |
| Error | leftmost key base 100%; the rest dark red, steady |
| Idle | leftmost key base 20%; the rest off |

The leftmost key at full brightness marks "this lane has something to say" and
names the lane by colour while saying it. The **Preview** row in the Keyboard
panel plays any pattern on the physical keys — every lane in its own colour,
keyboard only, self-expiring after 30 seconds — so a pattern can be judged by
looking at the keys instead of imagining them.

The SDK is **loaded at runtime, not linked**, so this builds on a machine with
no copy of it and runs on a machine with no Corsair keyboard — the difference is
a sentence in the window rather than a build failure. `install` copies
`iCUESDK.x64_2019.dll` alongside the executables if the build had one. To give
the build one: download the iCUE SDK from Corsair
([github.com/CorsairOfficial/cue-sdk](https://github.com/CorsairOfficial/cue-sdk),
Releases) and unzip it at `<repo>/iCUESDK` — the build script looks for
`iCUESDK/redist/x64/iCUESDK.x64_2019.dll`. The SDK is Corsair's proprietary
software under [its own EULA](https://corsairofficial.github.io/cue-sdk/#end-user-license-agreement)
and is deliberately not committed to this repository.

The lighting thread rechecks the session and the result of every write, and
reconnects when either says no. The old one checked once at startup and ignored
the rest, so an iCUE restart or a sleep left the F-row dark, silently, until the
application was restarted. It is also the application's clock: it evicts stale
sessions every frame, so lanes stay honest while the window is in the tray.

## Focus

Click a lane, and the window its agent runs in comes forward — a terminal with
the right tab in front, or the Claude/Codex desktop app, or an IDE. **The only
action in the product** — no approvals, no arrows, no key capture.

- **The hook reports its own Windows ancestry**, each ancestor as a pid plus
  the exe name that pid had at event time; the nearest ancestor whose pid
  still resolves to its recorded name and owns a real window is the host. This
  is what a WSL agent never had: its process ids mean nothing to Windows, but
  our hook runs Windows-side through interop, so the chain it reports is real —
  `powershell.exe → wsl.exe → wsl.exe → WindowsTerminal.exe`.
- **UI Automation selects the tab** when the host is Windows Terminal, because
  a terminal window's title is whichever tab is in front, so three agents
  sharing one window are indistinguishable to every window-level API Windows
  offers.

The exe-name check is what stops a recycled pid raising a bystander, and this
app's own window is excluded outright.
Within a matching pid a terminal-class window is preferred — that keeps
Windows Terminal's transient `PopupHost` out — else the topmost window that is
not a tool window, which keeps Electron splash screens and palettes out.
`explorer.exe` is never the host and is skipped by name.

The tab it looks for is **the lane's name**, then the project folder — which is
why naming a lane is a feature. When neither matches it says so and lists the
tabs that are there, rather than quietly leaving you looking at the right
terminal showing the wrong agent.

Three things it must keep doing (the histories are in
[docs/lessons.md](docs/lessons.md)):

- **Get foreground permission from the input, not by force.** Windows only lets
  a process change the foreground window if the user just gave input *to it*.
  The summon key arrives as a `WM_HOTKEY` posted to this app (see The
  keyboard), so the input *is* ours and the raise is allowed. No synthetic
  input, no foreground-lock tricks.
- **Activation and Z-order are separate.** `SetForegroundWindow` can leave a
  window the "foreground window" per the API while it is still drawn *behind*
  another. A topmost/not-topmost `SetWindowPos` pair forces the window
  physically to the top of the stack. Believe **what is visually on top**
  (`WindowFromPoint` at the window's centre), never `GetForegroundWindow`.
- **Run off the window's thread.** It needs its own COM apartment — the
  windowing library has already put the UI thread into the other one, and UI
  Automation then reads no tabs at all — and it can spend a quarter of a second
  waiting for the terminal to agree which tab is in front.

## Taking it to another machine

`dist.ps1` builds the zip from a fresh release build; the release profile
links the C runtime statically, so nothing has to be installed first.

```
dist/agent-frow-win64.zip
  agent-frow.exe          the app — it is also its own installer
  agent-frow-hook.exe     the shim the agents run
  iCUESDK.x64_2019.dll    Corsair lighting; the app runs without it
  README.txt              the three steps below
  LICENSE.txt             MIT — the notice travels with every copy
```

`iCUESDK.x64_2019.dll` is Corsair's, carried in the zip from the iCUE SDK's
`redist` folder and covered by [Corsair's EULA](https://corsairofficial.github.io/cue-sdk/#end-user-license-agreement),
not by this project's license; everything else in the zip is MIT.

On the new machine: unzip anywhere and run `agent-frow.exe` **once**. It
installs itself — binaries to `%LOCALAPPDATA%\agent-frow`, hooks registered
with every agent it finds — and hands over to the installed copy; the unzipped
folder can then be deleted. Restart the agents; for Codex, run `/hooks` inside
it and trust the entry. **Upgrading is the same gesture** with a newer zip: the
running copy is retired, replaced, and relaunched (Codex re-trusts only if the
registered command string itself changed). The window's Install/Remove buttons
remain for adding or removing individual agents later. For the keyboard: iCUE
with a Corsair board, and the F-row remapped to F13–F24 — put that remap in
the **default** profile, or a profile switch will silently take the summon
keys with it.

"Start with Windows" is a checkbox in the window's bottom bar — a per-user
Run-key entry pointing at the installed copy, visible and disableable in Task
Manager's Startup tab like any other. Not done: code signing (SmartScreen will
warn on a downloaded zip).

## Contributing

Issues and pull requests are welcome. **Before changing the state machine,
focus, the hook, or the lighting, read [docs/lessons.md](docs/lessons.md)** —
the constraints there were measured against real sessions, and in two cases
the agent documentation is wrong. CI checks formatting and lints/tests the
hook crate on Linux, and builds the whole workspace on Windows; the GUI only
compiles on Windows. If a change touches summon keys, focus, or lighting, say
what hardware you verified it on.

## License

MIT — see [LICENSE](LICENSE). The one exception is
`iCUESDK.x64_2019.dll` in release zips: that file is Corsair's proprietary iCUE
SDK redistributable, covered by
[Corsair's EULA](https://corsairofficial.github.io/cue-sdk/#end-user-license-agreement),
not by this project's license.
