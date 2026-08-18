# starship-fcs

A deterministic, offline-first flight control system for a fictional starship, written in Rust.

The premise: an LLM may one day sit in the loop as the ship's mind, and it must never be trusted with
the controls. So the ship is built the other way round — a safety kernel that is the sole authority
over every command, a control loop that survives with no model attached at all, and an append-only
recorder that makes any run reproducible bit-for-bit.

The core compiles and runs **offline with zero external crates**. `Cargo.lock` contains nothing but
the two workspace members, and it is meant to stay that way; network-backed provider adapters arrive
later, feature-gated and optional.

## Quick start

```sh
cargo run -p fcs -- run deep-space          # no actors aboard; the ship flies itself
cargo run -p fcs -- run crewed-deep-space   # a mock mind and a mock crew member aboard
cargo test --workspace
```

Each scenario advances 20 fixed 1-second ticks and prints one telemetry line per tick:

```
tick=1 t=1.00s env.ambient_temp_k=2.700 env.radiation_rate=0.000 ... sys.reactor.core_temp_k=307.027 ...
tick=2 t=2.00s env.ambient_temp_k=2.700 env.radiation_rate=0.000 ... sys.reactor.core_temp_k=313.984 ...
```

Channels carry a status marker: `*` for a spoofed reading, `!` for a dropout.

The crewed scenario adds what each actor said and what the kernel made of what it proposed. A
radiation spike lands partway through and degrades the link home:

```
tick=5 t=5.00s env.radiation_rate=4.000 sys.comms.signal_strength=0.100 ...
  ship_mind [mock:ship_mind]: all systems nominal
  engineer [mock:crew_agent]: signal down to 0.100, boosting transmit power
  -> CrewAgent: set_transmit_power comms Approved (applied)
  -> Autopilot: set_transmit_power comms Approved (applied)
```

Both actors run on their own provider instances, and neither's proposal reached the comms subsystem
without a kernel verdict first.

## The design in one loop

Every tick runs the same fixed sequence, in [`Ship::tick`](fcs-core/src/ship.rs):

```
clock ──▶ world ──▶ subsystems ──▶ telemetry ──▶ FDIR ──▶ actors ──▶ autopilot ──▶ safety kernel ──▶ apply ──▶ recorder
                                       │                                                │
                              the only view of reality                        the only path to state
```

Five properties hold that loop together.

**Determinism.** The [clock](fcs-core/src/clock.rs) is fixed-step; wall-clock time never enters the
simulation. World events drain FIFO. Every map on a decision or recording path is a `BTreeMap` or
`BTreeSet`, never a `HashMap`, so iteration order is stable and replay is exact.

**Telemetry is the only reality.** Nothing above the [telemetry](fcs-core/src/telemetry.rs) layer
reads the world or subsystem state directly — not FDIR, and not any actor. The sampler can
**spoof** a channel to an arbitrary value or mark it as a **dropout**, without ever writing back to
the state that produced the reading. That seam exists so the ship can be tested under false or
missing senses.

**The safety kernel is the sole authority.** Every command — including the autopilot's own — goes
through the same four-stage pipeline in [`SafetyKernel::review`](fcs-core/src/safety/mod.rs):

| Stage | What it checks |
| --- | --- |
| Schema validation | Verb exists on the target subsystem; args are known, present, and in range |
| Authorization | The issuing `Role` has been granted this `(target, verb)` pair |
| [Interlocks](fcs-core/src/safety/interlocks.rs) | Absolute physical limits — pure predicates, no side effects |
| Autonomy gate | Whether this role auto-executes at the current `AutonomyLevel` |

Interlocks are the part that matters. They run identically at every autonomy level and for every
role, and nothing overrides them: a fully authorized captain, at `Autonomous`, holding the physical
key, still cannot vent life support with crew aboard. Destructive verbs (`vent`, `jettison`,
`scuttle`) additionally require a physical key regardless of who is asking.

**The ship survives alone.** [FDIR](fcs-core/src/fdir.rs) evaluates each snapshot as a pure function
and any fault at all forces `SafeHold`. The [autopilot](fcs-core/src/autopilot.rs) then proposes
recovery commands — which still go through the full kernel, defense in depth — and the ship returns
to `Nominal` as soon as the faults clear. The mode is derived per tick, not latched.

The headline test for this is `no_llm_survival_recovers_from_reactor_overtemp_without_any_actor` in
[ship.rs](fcs-core/src/ship.rs): pin the reactor at its thermal ceiling, and with nobody aboard the
ship detects the fault, safes itself through the real kernel path, and recovers unattended. Its
companion, `a_crewed_ship_whose_actors_all_fail_flies_like_an_uncrewed_one`, holds the same line
from the other side: an actor that hangs or errors costs the ship a turn and changes nothing else.

**Actors only ever propose.** A [provider](fcs-core/src/provider/mod.rs) is text-in/text-out and
never sees a `Command`, a snapshot, or mutable state. An [actor](fcs-core/src/actors.rs) renders its
`Perception` — the telemetry line, mode, autonomy level, faults, recent events, recent dialogue —
to plain text, and turns the raw reply back into proposals through the strict `SAY:` / `DO:`
[protocol](fcs-core/src/protocol.rs) and nothing else. Malformed lines are dropped rather than
salvaged, the role comes from the actor's own identity rather than the wire, and `physical_key` has
no grammar at all, so a destructive verb proposed by a model always fails its interlock.

Any number of actors may be aboard on any mix of providers. Each turn runs behind the
[watchdog](fcs-core/src/watchdog.rs), so a hung or errored one falls back to the autopilot for that
tick instead of blocking the loop, and every actor in a tick is handed the *same* perception — built
before any of them speaks — so who boarded first cannot change what anyone sees.

## Layout

```
fcs/         thin CLI binary — `fcs run <scenario>`
fcs-core/    the entire system; no external crates
  clock       fixed-step deterministic time
  world       environment state + deterministic event queue
  subsystems  reactor, life support, propulsion, navigation, comms
  telemetry   the perception seam (spoof / dropout)
  command     typed command model — closed enums, no free text on the control path
  safety      the kernel: schema → authorization → interlocks → autonomy
  fdir        fault detection and operating mode
  autopilot   deterministic recovery controller
  watchdog    guards an actor's turn; falls back to autopilot on hang or error
  protocol    the strict SAY:/DO: line grammar — the only way text becomes a proposal
  provider    the LLM seam: text in, text out, nothing else
    online/   feature-gated network adapters (anthropic, openai, ollama)
              + hand-rolled http/json and the HttpTransport seam
  actors      ShipMind and CrewAgent — untrusted, and only ever propose
  config      who is aboard and what backs them — the only file you edit to swap a model
  recorder    append-only flight recorder, replayable
  ship        the integration loop
```

## Online providers

The default build has no network code in it at all. The adapters live behind a cargo feature:

```sh
cargo run -p fcs --features online -- run crewed-deep-space
```

Which provider backs the crew is configuration, not code — `FCS_PROVIDER` re-backs every actor in a
scenario and `FCS_MODEL` names the model:

```sh
FCS_PROVIDER=ollama FCS_MODEL=llama3.1 cargo run -p fcs --features online -- run crewed-deep-space
```

| Provider | Endpoint | Key | Model |
| --- | --- | --- | --- |
| `mock` | none — in-process | none | none |
| `ollama` | `http://localhost:11434/api/chat` | none | required |
| `anthropic` | `https://api.anthropic.com/v1/messages` | `ANTHROPIC_API_KEY` | defaults to `claude-opus-5` |
| `openai` | `https://api.openai.com/v1/chat/completions` | `OPENAI_API_KEY` | required |

A spec names the *environment variable* that holds a key, never the key. Nothing that gets logged,
printed, or recorded has ever held one.

**Still zero external crates.** The adapters hand-roll HTTP/1.1 framing and just enough JSON, so
`Cargo.lock` contains the same two workspace members with the feature on as with it off. What that
costs is TLS: the bundled `TcpTransport` speaks plaintext, so **Ollama works out of the box** and
the hosted providers need a transport you supply.

```rust
impl HttpTransport for MyTlsTransport {
    fn name(&self) -> &str { "my-tls" }
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, TransportError> { … }
}

let ship = ShipConfig::new(1.0)
    .with_actor(ActorSpec::ship_mind(ProviderSpec::new(ProviderKind::Anthropic)))
    .build_with(|| Box::new(MyTlsTransport::new()))?;
```

Asked for an `https://` endpoint it cannot reach, `TcpTransport` refuses and says so — a ship that
believed it had an encrypted link and did not would be worse than one that stops.

Nothing retries. A rate limit, a timeout, a 500, an unreachable server: each becomes a
`ProviderError`, then a watchdog `TurnFailure`, and the autopilot flies that tick. Start the crewed
scenario with no Ollama running and you can watch it happen — both actors fail every tick and the
ship completes the flight anyway.

## Doctrine

These are the rules the code is held to, and reviews should enforce them:

- The core runtime stays deterministic, offline, and hermetic.
- The safety kernel is the authority for every command, no exceptions and no bypass.
- The LLM layer is untrusted and never mutates state directly — actors only ever *propose*.
- No `HashMap` iteration in any decision or recording path.
- `#![forbid(unsafe_code)]` in every crate; errors surface via `Result`; no `unwrap()` on the control path.
- Zero external crates, in **every** feature set. `Cargo.lock` holds the two workspace members and
  nothing else, with `--features online` or without it.
- A model can be swapped, mixed per actor, or removed entirely by touching `config` and `provider`.
  If a change like that needs `safety`, `subsystems`, or `ship` edited, the change is wrong.
- Every phase leaves the workspace compiling and the test suite green.

## Status

All six phases are complete: deterministic core, safety kernel, FDIR/autopilot/watchdog, the
replayable recorder, the full LLM seam (`LlmProvider`, the strict `SAY:` / `DO:` protocol, a
deterministic mock provider, `ShipMind`/`CrewAgent` actors behind the watchdog), and feature-gated
Anthropic/OpenAI/Ollama adapters with a configuration layer that assigns a provider per actor.

`cargo test --workspace` passes 160/160 offline, and 235/235 with `--features online`. `Cargo.lock`
contains two packages either way.

## License

Apache-2.0. See [LICENSE](LICENSE).
