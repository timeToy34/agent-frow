# How it works

The engineering companion to the [README](../README.md): how the pieces fit,
what each rule in the product actually does, and the build-and-install
mechanics. The *histories* — why each rule exists, measured against real
sessions — are in [lessons.md](lessons.md).

## The pieces

```
agent-frow.exe            tray, window, ingress on 127.0.0.1:47115
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

The hook's reply to every event is empty, and it never prints to stdout: it is
registered on `PermissionRequest`, which is a decision hook on both agents, so
anything it printed that parsed could approve a real tool call.
`hook/tests/silence.rs` asserts zero bytes. The one exception is not a hook:
as Claude's status-line command (`--status`, see [Numbers](#numbers-context-and-limits))
it may, with `--tee`, write back exactly the bytes it was given so the user's
own status line after the pipe still renders — never a byte of its own, and
`silence.rs` asserts that too. The registered command string
must never change either — Codex records trust against a hash of it — which is
why it names `%LOCALAPPDATA%` rather than a build directory, and why the token
lives in a file rather than in the command.

## Building, installing, and the two locations

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

### The iCUE SDK

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

### The zip

`dist.ps1` builds the zip from a fresh release build; the release profile
links the C runtime statically, so nothing has to be installed first. Run it
on a checkout that is actually at the commit you mean to ship — cargo rebuilds
only what changed, and a zip the same size as the previous release is the tell
that nothing was.

```
dist/agent-frow-win64.zip
  agent-frow.exe          the app — it is also its own installer
  agent-frow-hook.exe     the shim the agents run
  iCUESDK.x64_2019.dll    Corsair lighting; the app runs without it
  README.txt              the install steps
  LICENSE.txt             MIT — the notice travels with every copy
```

`iCUESDK.x64_2019.dll` is Corsair's, carried in the zip from the iCUE SDK's
`redist` folder and covered by Corsair's EULA, not by this project's license;
everything else in the zip is MIT.

On a new machine, running the unzipped `agent-frow.exe` once installs it —
binaries to `%LOCALAPPDATA%\agent-frow`, hooks registered with every agent it
finds — and hands over to the installed copy. Upgrading is the same gesture
with a newer zip: the running copy is retired, replaced, and relaunched (Codex
re-trusts only if the registered command string itself changed). The port
bind doubles as the single-instance lock; a second instance shows a message
box rather than silently exiting. "Start with Windows" is a per-user Run-key
entry pointing at the installed copy, visible and disableable in Task
Manager's Startup tab like any other. Not done: code signing.

### Diagnostics

`agent-frow.exe doctor` reports what is installed, whether each agent has
actually been heard from, and whether Claude's status line is registered —
the numbers arrive through it. Launching the app with `AGENT_FROW_DEBUG` set (any
value) appends every hook event's identity fields — `src`, event name, session
id, `cwd`, agent id and type — to `~/.agent-frow/events.log`, the diagnostic
for questions like "what does this agent actually report as its working
directory?". Off by default; the log self-truncates past 256 KB. Failed hook
posts are logged by the hook itself to `~/.agent-frow/hook.log`.

## Events and the state machine

Six states: **Connected** (alive, nothing run yet), **Running**, **Waiting**
(needs you), **Done**, **Error**, and **Idle** — nothing heard for a while.
Idle is the one state no event sets: it reports silence, a fact about the
wire, not a guess about the agent. An interrupt lands here too — the agent
sits back at its prompt, and interrupting is something *you* did, not an
alarm. The transition table is `app/src/state.rs` and it is a total function
over (state, event) — no request ids, no queues, no tombstones, no timers.
**Activity clears Waiting**, so nothing has to be correlated to anything.

None of its conditions are stylistic — each prevents an observed failure and
has a test named after it:

- `SessionStart{source: "compact"}` changes nothing. Claude compacts *mid-turn*;
  without the guard a live turn drops to Connected until the next `Stop`.
- An event carrying `agent_id` or `agent_type` is **liveness only**. Subagent
  events carry the *parent's* `session_id`, so acting on one lets a subagent
  finishing a tool clear a Waiting another one raised. The tracker keeps a
  per-session subagent roster instead: `SubagentStart` enrols an id, every
  event carrying it is its heartbeat, `SubagentStop` retires it, and 30
  minutes of silence retires one that died unannounced. A resting lane with
  subagents still at work shows the Running pattern and "N subagents busy";
  Waiting and Error always win.
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
- A `Stop` carrying `proposed_plan` sets Waiting, not Done. Codex has no
  dialog for approving a plan: it ends the plan-mode turn with the plan in
  its final message and its UI asks "implement?" from that. The hook reports
  only that the `<proposed_plan>` tag is present — the message stays where
  it is — and the answer arrives as the next prompt.
- `PreToolUse` for `request_user_input` sets Waiting. It is the only tool
  name the table reads: Codex asks its questions through that tool, whose
  handler shows the dialog and blocks until it is answered, so the tool
  starting *is* the question appearing, and its `PostToolUse` *is* the
  answer. It is registered for Codex with that one tool as its matcher;
  Claude's questions arrive as a notification instead.

Unknown `notification_type` values are ignored and **counted**, and shown in the
window as a number, so a new agent release is a line you can read rather than
behaviour nobody can explain. Same for hook events we do not register.

There is no handshake and nothing to seed: an event from a session we have never
seen creates it and infers the state from the event itself. An agent started
before the app, after it, or an hour ago all behave identically.

**Known limitation, stated in the window rather than hidden:** no agent emits an
event when you *answer* a prompt. The next observable event is that tool
finishing, so a lane can read Waiting while the approved tool already runs —
seconds usually, up to about a minute. Codex stretches this: it reports a
command only once its process has exited, so a server or a long install you
allowed it to start holds Waiting until some later command finishes. A Codex
*question* is the opposite case — its `PostToolUse` fires the moment you
answer. Self-clearing in every case.

**Silence is reported, never punished.** A session leaves only on `SessionEnd`
or the ✕ — never on a timer, because the user who stepped away comes back to
the board they left. Instead, silence demotes: Done and Connected dim to Idle
after 30 minutes, Running after 2 hours (a killed terminal stops glowing
blue). Waiting and Error hold through any amount of time — they are exactly
what you left to come back to. Any event from the agent revives its lane where
it stood.

## Lane placement

Twelve F-row keys, in 3, 4 or 6 lanes. A session takes the first free lane.
The rest live **off the keyboard**: full cards in the window — state, note,
timer, Focus, dismiss — just no key and no light. An off-keyboard session stays
fully tracked, is promoted oldest-first the moment a lane frees or the layout
grows, and the landing spot for the next agent is drawn whenever every lane is
taken, so the board never runs out of room.

**Saved agents.** *Save* on a lane card remembers that `(agent, project
folder)` with that lane as its *preferred* lane. When the agent comes back it
takes its preferred lane if the lane is free; otherwise the first free lane;
otherwise it waits off the keyboard — and landing elsewhere never rewrites the
save (`claim()` in `app/src/tracker.rs` reads the settings and cannot write
them). A preference is not a reservation: an empty preferred lane is a free
lane to whoever comes next. One refinement: a session whose folder is not
known yet — one adopted from a subagent's event — takes a lane nobody prefers
while there is one, so an unidentified agent cannot sit down in a saved
agent's place during a restart. The window shows three groups: the lanes, the
off-keyboard sessions, and the saved agents that are not running; a running
saved agent is tagged on its card. The roster is where a save's agent and
preferred lane are edited, and where it is forgotten.

**Nothing ever takes a lane away from a session that already has one** — lane
position is identity, and a display you glance at teaches you nothing if lane
2 moves while you are looking at it. The *user* has two sanctioned exceptions:
the ⏶⏷ buttons reorder lanes (everything — session, name, colour, saved
preference, keys — travels together), and an off-keyboard card's ⏶ takes the
bottom lane, its incumbent stepping off the keyboard in trade.

A session's **project folder** is the *main* agent's launch directory: it is
taken from `SessionStart`'s `cwd` (authoritative) and, until that arrives, from
the first non-subagent event that carries one. A **subagent's** `cwd` never sets
it — a subagent working in `…/frontend` under a project rooted at `…/` must not
make the subfolder the lane's project, which was a real "bound to the wrong
folder" bug.

Lane names, colours, saved agents, the lane count and whether Settings is
unfolded live in `%LOCALAPPDATA%\agent-frow\settings.json`, written
atomically. A file that does not parse is refused and left exactly as it is:
the window says so, and says that changing anything will overwrite it. Saved
agents are stored as `{"agent", "folder", "lane"}` with the lane counted from
one, as the window counts; a pre-0.5 file's per-lane `bind` is read as a saved
agent preferring that lane and never written back.

The lane name is load-bearing: focus finds a terminal tab by it.

### Numbers: context and limits

Hooks carry none of these, on either side. The three a lane shows — how
much of the context window is used, and how much of the five-hour and the
seven-day limit — come from where each agent keeps them, and `app/src/gauges.rs`
turns both into one shape: three percentages, each possibly unknown.

- **Claude** hands them to its status-line command on every assistant
  message (and on a compact, a mode change, a config edit). `install` makes
  the hook that command, in `--status` mode: it posts a `StatusLine` record
  with the session id and three percentages, and nothing else out of that
  JSON — not the model, not the cost, not the directory. Where a status
  line already exists it is wrapped, not replaced: `hook --status --tee … |
  <yours>`, and `--tee` writes back exactly the bytes it read, so your line
  renders as it always did. `remove` unwraps it. The limits appear only for
  a Pro or Max account, and only after the first reply; until then the key
  shows a dash.
- **Codex** writes them into the session's rollout, one `token_count` line
  after each model response, and every Codex hook names that file
  (`transcript_path`). The hook forwards the path; the *app* reads the
  file's tail on its worker thread — the hook is a Windows process even for
  a WSL agent and cannot see `/home`, while the app knows every
  distribution and reaches it through `\\wsl.localhost`. Context follows
  the Codex TUI's arithmetic (twelve thousand tokens of baseline subtracted
  from both usage and window); the two limits are told apart by the length
  of their window, not by which slot they arrived in — the five-hour window
  is `primary` on one plan and absent on another.
- **A status line is numbers, not news.** It changes no state, counts as no
  event, revives no Idle lane — it also fires on a config edit — and one
  for a session the app does not hold is dropped, since Claude re-runs it
  after a session has ended. The limits are an account's, not a lane's:
  every lane of one account reads the same two.

## The keyboard

Five surfaces, one palette. `app/src/surface/palette.rs` says what the keys
look like — the twelve by F-row position, the numpad's nine by their own —
never by LED; each surface maps a position to what its device calls that LED,
and writes. The four device threads run for the life of the app: whatever is
plugged in lights up, and all of it does if all of it is; the fifth surface is
the screen, below under [The monitor](#the-monitor). Each is also a clock — through `surface/scene.rs`, which
decides when a frame is due — so lanes stay honest while the window is hidden.

Each device's line in the window has a tick. Unticked, its thread hands the
device back the way Quit does — the Keychron's snapshot restored, iCUE
disconnected, the deck reset to its logo — and stops looking until ticked
again, when it looks at once; the thread keeps ticking the scene, since it is
still a clock. The set is `disabled` in `settings.json`, by surface name;
absent means on.

### Corsair, through the iCUE SDK

Two constraints:

- **Only ever the twelve LEDs we own.** The SDK sets only the LEDs it is
  handed, so naming twelve leaves every other key to the user's own profile —
  and touching more is a way of spelling "black out the whole keyboard".
- **Shared layer priority 128, never exclusive control.** Exclusivity is per
  *device*, not per LED, so asking for it stops iCUE rendering the keyboard and
  every key outside the F-row goes dark whether we paint it or not.

Not every keyboard renders colour honestly — on some, blue overpowers and red
undershoots until a colour set on screen is unrecognisable on the keys.
**Colour balance** (🎨, beside ☀ brightness) is a per-channel gain, 0.25–2.00,
multiplied into what the keys are sent and nothing else — the window is never
corrected. Calibrate with a **Preview** pattern playing; above 1.00 clips on
already-full channels, so prefer pulling the strong channels down. Both are
**per device** (`Settings::tuning(surface)`, keyed by the surface's name):
each connected keyboard and the deck has its own line of controls, and a
settings file from before that has its one slider read as the tuning every
device starts from.

### Keychron Ultra, through its Launcher protocol

No firmware of ours and no driver. The Ultra series (Keychron's ZMK fork on a
Realtek RTL8762G) serves a VIA-compatible raw-HID interface — usage page
`0xFF60`, usage `0x61`, 32-byte reports — on the cable and through the 2.4 GHz
receiver, and that is the interface the Keychron Launcher itself uses. Not
over Bluetooth: the firmware drops this channel there, so the light needs the
cable or the receiver; the summon keys, being ordinary keycodes in the
keyboard's keymap, work over all three.

- **Only ever the twelve LEDs, again.** The keyboard's mixed mode splits the
  board into two regions, each running its own effect. The app gives the
  F-row to region 1, rendered per key from what it sends, and leaves region 0
  — everything else — running whatever the user had, or nothing if their
  lighting was off. "Nothing" is not the firmware's effect "none", though:
  a mixed frame ends as soon as region 0 reports finished, and "none" is
  finished after the board's first slice of LEDs, so the later LEDs are
  never repainted — and the Caps Lock indicator, which the firmware draws
  over its key and clears only by the key being repainted, stayed lit once
  pressed. So when the user had nothing running, region 0 runs the per-key
  effect too, every key stored black: it looks exactly like off, but every
  LED repaints each frame and Caps Lock can switch off.
- **Left as found.** Everything the app changes — effect, brightness, speed,
  colour, per-key type, every LED's stored colour and region, both effect
  lists — is read first and written back on the way out. Tray Quit
  exits without unwinding, so it hands the keyboard back explicitly before
  it goes. The snapshot also sits in `%LOCALAPPDATA%\agent-frow\keychron-state.json`
  while the app runs: a keyboard found already in the app's mode on the next
  start (the app was killed, or the keyboard re-enumerated after sleep) is
  restored from there rather than "snapshotted", which would capture the
  app's own work.
- **Never the flash.** Every change is to the keyboard's RAM. The protocol
  module has no way to spell either save command, and a test holds that; a
  power cycle is always a complete undo.
- **Per-key brightness needs the fixed firmware.** Stock Ultra firmware
  discards the per-key value byte and renders every key at the board's
  brightness — a dark key becomes white (with the lighting off, that is the
  whole board). [Keychron/zmk#9](https://github.com/Keychron/zmk/pull/9)
  fixes it; until Keychron ships it, that is a custom build flashed through
  the Launcher's manual upload — `firmware/keychron-ultra/` has the patch,
  the build and the guide. The keyboard's own brightness still scales the
  F-row on top of the app's slider.
- **The keyboard talks unasked.** A layer change is pushed on the same
  interface, so an exchange reads until the reply that echoes its command,
  skips the rest, and clears anything stale before writing. Without that, one
  push shifted every later reply by one and the surface reconnected forever.
- **The idle timer is the keyboard's.** After its own backlight timeout the
  F-row goes dark with the rest and returns on the next keypress; the app does
  not keep it awake.
- Found by usage page, never product id: the receiver ("Ultra-Link") is a
  different USB device with the same interface. Cable and receiver both
  plugged in is one keyboard on two paths, and the one that answers the
  handshake is used; switching between them reboots the keyboard, which is a
  reconnect like any other. `agent-frow doctor` lists what it finds and what
  each answers.

### Keychron V0 Ultra, the numpad

The same Launcher protocol as the Ultra above — same handshake, same mixed
mode, same never-the-flash rule, same snapshot-and-restore, one code path:
`surface/keychron_v0` reuses the Ultra's protocol, transport and session
under its own `Geometry`. What differs is the shape and the vocabulary:

- **Nine LEDs of twenty-six.** The four shape keys are a **top
  line** showing one agent in the classic four-key lane patterns; M1–M5 are
  an **agent column**, one key per lane — the resting glow, a low-to-full
  breathe for Running, the double pulse for Waiting (steady full instead
  while that agent's terminal is the foreground window — the top line does
  the pulsing), a single beat for Done, the fixed red for Error. The other
  seventeen keys stay on the user's own effect, exactly as the Ultra leaves
  the rest of its board alone.
- **Selection.** The top line shows the *selected* agent, and selection is
  the tracker's (`selected`/`locked` on `tracker.rs`), not the surface's.
  Unlocked, it chases the news: any lane state change pulls it there.
  Pressing the knob locks it — shown as the selected M key fading lane
  colour to white and back, white being the one shade no lane and no state
  may use — and turning the knob always moves it, locked or not.
- **Input is chords.** The knob and keys are remapped to Ctrl+Shift+F13–F24
  (bare F13–F24 belong to the F-row) by importing a keymap file into the
  Launcher — its key picker records only a physically pressed key, so a chord
  cannot be chosen from the list; `firmware/keychron-ultra/keymaps/` holds
  the file. The encoder takes a modified keycode like any key: knob CCW/CW/press
  select and lock; M1–M5 select + summon their lane; the top line's four
  keys are an F-row lane for the shown agent — any of them summons it, and
  while it is Waiting the three after the first are ⏶⏷Enter. Both keyboards
  bind through the one `lane_press` rule in `keys.rs`, the rule the lane
  pattern draws, so the picture and the keys cannot disagree. All of it
  lands in the same hotkey pump and the same summon/answer workers the
  F-row and the deck use.
- **Two Keychrons, one bus.** A V0 and a V3 Ultra answer the same handshake
  on the same usage pair, so the surfaces *claim* an interface before
  opening it (`keychron/hid.rs`) and each accepts only its own board —
  twenty-six LEDs is the numpad, anything else the F-row's. Snapshots live
  in separate files (`v0ultra-state.json`), and recall refuses a snapshot
  with the wrong LED count, so a crashed app can never cross-restore the
  boards.
- The foreground check behind Waiting's steady-full is window-level — the
  foreground window's process, verified against the session's recorded
  ancestry the way summon verifies it, cached for two seconds and asked
  only while a shown lane is Waiting. Two agents in tabs of one terminal
  window both read as foreground; that is the honest limit of window-level.

### Stream Deck, over HID

The one surface that is taken whole. A keyboard has keys of its own beside
ours; a deck has only the keys, and the Elgato app repaints every one from
its own profile and reads every press, so the two cannot share. Plain HID
through the `elgato-streamdeck` crate, which knows each model's report
layout, image size, rotation and encoding; no driver, nothing linked.

- **Only while its own software is not running.** `StreamDeck.exe` in the
  process list means the deck is the Elgato app's: the surface does not open
  it and says so in the window. Checked every ten seconds while the deck is
  held, too, and the deck is handed back — `reset`, which is its logo, what
  a deck shows with nothing driving it — the moment the app appears. That
  reset is also the exit, tray Quit included; "as found" for a deck is the
  logo.
- **One row per lane**, like the F-row: the first key is the lane's name,
  the last its state, and the keys between are the lane's body — carrying
  its numbers: context used, the five-hour limit, the seven-day limit, each
  as its short name over a percentage, or over a dash until it is known (on
  a three-key row, the context alone). In Error the state key's second line
  is the reason when there is one — "rate limit", "overloaded", "auth" —
  rather than the clock. The colours
  are `lane_colors` over the row — the marker, the runner crossing, the
  double-pulse, the dark red of Error — with the state key steadied: it
  rests at the lane's glow through Waiting's pulse, Error's red and Done's
  marker. The words are ink over one colour, in Segoe UI Bold — read from
  the Windows font folder, so nothing is shipped, and bold because a light
  face on a small LCD could not be read at arm's length; the window's own
  Ubuntu stands in if the file is missing: the name on the first key, the
  state word over the elapsed time on the last (in Waiting the time, counted
  in minutes, as the headline over the word), and in Waiting an up
  triangle, a down triangle and the word Enter on the three middle keys in
  place of the numbers — the triangles drawn, not typeset, about the height
  of the text's capitals, and Enter set by the same rule as a name. The
  name is set as words — a dot or a
  dash breaks like a space, on the deck only — over up to three lines, as
  large as the key allows. Ink is grey on a key that is dark on purpose (an
  empty lane is black with what it is for in grey — "next agent here" on
  the lowest free lane, "free" on the rest, its name if it has one — and
  Idle is one dim name key and the rest off), and otherwise decided for
  the lane rather than the
  instant: a lane's keys only ever run between its 20% glow and its full
  colour, so the ink that reads on each end is found (black above the
  luminance where black and white contrast equally, 0.179 by WCAG; white
  below) — if both ends agree, that is the ink, steady; if the full colour
  wants black where the glow wants white, the ink fades between the two
  along the same ramp the key is on. No shadow, and never a flip. A deck
  shows as many lanes as it has rows — three
  on a 15-key deck — and the window says which; the rows past the lanes are
  black. `surface/streamdeck/canvas.rs` is the drawing and has no device in
  it.
- **Brightness is the deck's own**, set from the deck's own slider; the
  pixels are never scaled and never colour-corrected — an LCD is a screen,
  like the window, so the deck's line has no colour balance.
- **A key is written when its face changes**, not when the scene says so:
  the elapsed time on a key ticks while the lane's state stands still, so
  every key is compared with what it was last told, and only a changed one
  is rasterised, encoded and sent. A resting deck costs nothing; a lane in
  motion rewrites its row ten times a second — its status key, counting
  minutes in Waiting, once a minute.
- **One thread for both directions.** The device handle cannot be shared,
  so the wait between frames is spent listening for a press, and a press is
  answered at once. Every key of a row summons its lane through the same
  `keys::summon_lane` the F-row's keys use; in Waiting the three middle keys
  go through `keys::answer_lane` instead — the same raise, then one key — as
  the three after a lane's first do on the F-row. The two surfaces speak one
  `keys::Press` and ask one `Tracker::answerable`. Each press on a thread of
  its own, since a raise can spend a quarter of a second on the terminal's
  tab strip. A preview is a look, not a
  question: its Waiting shows the answer keys and never types.
- **A deck press is not input to this process.** It arrives over USB on our
  thread, not as a keystroke Windows delivered to us, so the foreground
  permission a summon key gets is not granted here. The terminal is still
  brought to the top of the ordinary stack (the topmost/not-topmost pair
  needs no permission), and no synthetic input is used to *get* the
  keyboard. Whether the keyboard followed is then checked —
  `GetForegroundWindow` is the window, and for Windows Terminal, UI
  Automation says focus is not on the tab strip, where an arrow switches
  tabs — and only then is one key sent with `SendInput`. If not, the press
  has focused the window and the status bar says to press again; a
  keystroke whose destination cannot be told is not sent.
- Found by the driver's list and opened by serial; the first deck with a
  screen is the one taken. `agent-frow doctor` lists what is on the bus and
  whether the Elgato app has it, without opening anything.

### The F-row's keys

F13–F24 are registered as global hotkeys with `MOD_NOREPEAT`. That both keeps
the remapped keys out of other applications and makes Windows deliver the
physical input to Agent F-Row before it raises a terminal. A low-level keyboard
hook must not replace this: swallowing the key inside `WH_KEYBOARD_LL` leaves
the app without foreground permission, which makes summon fail specifically
when the Agent F-Row window is focused even though the Focus button works.

The F-row is always three lanes of four — `KEYBOARD_LANES × KEYS_PER_LANE` in
`settings.rs`. The lane count is a setting of its own, three to six; a lane
past the third has no keys and is shown in the window, in mini mode and on a
deck with the rows. `keys::press_of` turns a key index into a `Press`: the
lane is `index / 4`, and every key of a lane summons it — except while the
lane is *answerable* (`Tracker::answerable`: a session on it whose effective
state is Waiting, and no preview playing), when the three after the first are
Up, Down and Enter through the same `keys::answer_lane` a deck row uses:
raise, verify the terminal has the keyboard, then one `SendInput`. A hotkey is
input Windows delivered to this process, so the raise has foreground
permission — the deck's harder case, not a new one. `MOD_NOREPEAT` means a
held F14 is one Up, not a stream of them. Waiting's double-pulse on those
three keys was already the affordance: the keys that beat are the keys that
answer.

**The lane's colour is the base for everything, and colour change means
trouble.** Every ordinary state is the lane's own colour at some brightness and
motion; red is reserved for Error and nothing else may be red, which is why
lane colours should stay away from it. The patterns (four keys per lane):

| State | Pattern |
|---|---|
| empty lane | all off |
| Connected | all keys, base colour, 20% |
| Running | base 20% glow with one 100% light crossfading across five slots |
| Waiting | leftmost key 100%; the three answer keys double-pulse base up to 100% |
| Done | leftmost key 100%; the rest 20% |
| Error | leftmost key base 100%; the rest dark red, steady |
| Idle | leftmost key base 20%; the rest off |

The leftmost key at full brightness marks "this lane has something to say" and
names the lane by colour while saying it. The **Preview** row in the Keyboard
panel plays any pattern on the physical keys — every lane in its own colour,
keyboard only, self-expiring after 30 seconds — so a pattern can be judged by
looking at the keys instead of imagining them.

The lighting thread rechecks the session and the result of every write, and
reconnects when either says no. The old one checked once at startup and ignored
the rest, so an iCUE restart or a sleep left the F-row dark, silently, until the
application was restarted. It paints only on change or animation, and it is
also the application's clock while the window is hidden: it sweeps stale
sessions every frame, so lanes stay honest in the tray.

## The monitor

The screen is the fourth surface, in `app/src/surface/monitor.rs`. Mini mode
folds the window down to the deck's picture: one row per lane with a session
on it, then one per off-keyboard session, five keys to a row — the name, ctx,
5h, 7d, the state over how long, or in Waiting how long over the state —
built from the deck's own key vocabulary (`Face`, `Label`, `Ink`, `role`)
and lit by the deck's `row_colors`, which is
the keyboards' `palette::lane_colors` with the state key held steady. It
paints at ~30 frames a second while a row moves and at the window's resting
pace otherwise. A name is set the deck's way too — `name_words`, dots and
dashes breaking like spaces, the largest size that fits whole on up to three
lines, every row centred. Like the window it lives in, it is never
brightness-scaled or colour-corrected: those exist to make LEDs match the
screen. A row past the lanes is drawn in a neutral grey, since it has no lane
colour; an empty lane has no row, since a row that says nothing only costs
the space it takes.

- **A key is a summon and nothing more.** No ▲▼Enter here — the middle keys
  stay the numbers through Waiting: a click is a click, and the surface
  mirrors the agents rather than driving them. What the last focus did is
  shown over the rows for a few seconds, so a focus that found no tab is not
  silent.
- **No title bar, and the window is the user's to place.** The background is
  the drag handle (`ViewportCommand::StartDrag`), the bottom-right corner the
  resize (`BeginResize`), and it sits on top. Its place, width and *row
  height* are written to the settings file once it has held still for half a
  second — the size is kept by the row, so a row arriving or leaving changes
  the window by exactly one row and the keys keep the size the user gave
  them. A resize we asked for is not read back as the user's. At launch the
  viewport is built for the persisted mode and place, so a restart in mini
  mode opens where it was left and never flashes the full window.
- **The way in is the *Mini mode* button or a double-click on a card; the way
  back is a double-click.** Both are sensed *underneath* the widgets with
  `UiBuilder::sense`, so a card's own controls keep their clicks (see
  lessons).

## Focus

Click a lane, and the window its agent runs in comes forward — a terminal with
the right tab in front, or the Claude/Codex desktop app, or an IDE. **The
product's first action** — no approvals, no key capture. Its second is the one
keystroke the answer keys — a lane's three after its first on the F-row, a
row's middle three on a Stream Deck — can send while a lane is Waiting, and
that goes only to a terminal that verifiably has the keyboard.

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
app's own window is excluded outright. Within a matching pid a terminal-class
window is preferred — that keeps Windows Terminal's transient `PopupHost` out —
else the topmost window that is not a tool window, which keeps Electron splash
screens and palettes out. `explorer.exe` is never the host and is skipped by
name.

Windows Terminal hosts **every window in one process** — that is what lets a
tab be dragged out into a window of its own (1.22 and later) — so identity
finds the process and a matching pid can own several terminal windows. The tab
chooses between them: for each name tried, the window already showing it,
else the window holding it, else the topmost. That read costs one UI Automation
walk per window and happens only when there is more than one. Two unnamed lanes
on the same project share a tab title, and the first in Z-order wins — naming
the lane is the remedy, as it is for the tab itself.

The tab it looks for is **the lane's name**, then the project folder — which is
why naming a lane is a feature. That order is strict: a project tab that is
already showing never beats a lane-named tab that exists, which matters when
Claude and Codex share one project. When neither matches it says so and lists
the tabs that are there, rather than quietly leaving you looking at the right
terminal showing the wrong agent.

Three things it must keep doing (the histories are in [lessons.md](lessons.md)):

- **Get foreground permission from the input, not by force.** Windows only lets
  a process change the foreground window if the user just gave input *to it*.
  The summon key arrives as a `WM_HOTKEY` posted to this app, so the input *is*
  ours and the raise is allowed. No synthetic input, no foreground-lock tricks
  — synthetic input never gains the foreground; the one keystroke an answer
  key sends goes out only after the foreground is verified.
- **Activation and Z-order are separate.** `SetForegroundWindow` can leave a
  window the "foreground window" per the API while it is still drawn *behind*
  another. A topmost/not-topmost `SetWindowPos` pair forces the window
  physically to the top of the stack. Believe **what is visually on top**
  (`WindowFromPoint` at the window's centre), never `GetForegroundWindow`.
- **Run off the window's thread.** It needs its own COM apartment — the
  windowing library has already put the UI thread into the other one, and UI
  Automation then reads no tabs at all — and it can spend a quarter of a second
  waiting for the terminal to agree which tab is in front.

## The tray

Closing the window hides it (`Visible(false)`); the ingress and the lighting
keep running, and the tray icon brings it back — left click opens, right click
is Open/Quit, and Quit is the only exit. A hidden window runs no UI pass at
all: the 500 ms repaint is re-armed only while `IsWindowVisible` says so,
because egui cannot see `SW_HIDE` on Windows. Measured: zero UI-thread cycles
over 30 s hidden.
