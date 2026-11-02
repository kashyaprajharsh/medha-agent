# Rust contribution standards

Use this guide when implementing, reviewing, or porting Rust code in Medha.
A contribution should make its behavior, ownership, failure handling, and
verification clear enough for another contributor to assess from the PR.

The rules about safety, permissions, and compatibility apply to every affected
change. API and style recommendations require judgment: explain an exception
when the existing contract or use case warrants it. Review existing code on its
actual behavior rather than treating a difference in style as a defect.

[CONTRIBUTING.md](CONTRIBUTING.md) covers environment setup, repository locations,
desktop development, and submitting a PR. The
[CI workflow](.github/workflows/ci.yml) defines the automated checks; the design
and review requirements below also need human review.

- [Establish the change contract](#establish-the-change-contract)
- [Implementation requirements](#implementation-requirements)
- [Shared work and asynchronous operations](#shared-work-and-asynchronous-operations)
- [Platform and unsafe boundaries](#platform-and-unsafe-boundaries)
- [Build configuration and dependencies](#build-configuration-and-dependencies)
- [Tests, documentation, and measurements](#tests-documentation-and-measurements)
- [Commands and verification evidence](#commands-and-verification-evidence)
- [Review requirements](#review-requirements)
- [Porting requirements](#porting-requirements)
- [Submission checklist](#submission-checklist)

## Establish the change contract

Before editing, read the affected functions, their callers, the relevant tests,
and any configuration that changes the execution path. Use the
[repository map](CONTRIBUTING.md#find-the-right-part-of-the-repository) to find
where the behavior belongs.

Identify the change as a fix, feature, refactor, or language port. Explain the
trigger and intended outcome. For a refactor, identify what must remain stable;
for a fix, show the failure being corrected. Separate observations from guesses
and resolve uncertainty by tracing the relevant path or constructing a focused
reproduction.

Pay particular attention to shared contracts: method signatures, serialized
messages, stored state, configuration defaults, ordering, authorization, and
recovery. An internal crate can have callers elsewhere in the workspace even
when it is not published. Keep unrelated changes out of the PR.

Check the applicable manifest before choosing a language feature or dependency.
The root [Cargo.toml](Cargo.toml) declares edition 2024 and minimum Rust 1.89.
Use [rust-toolchain.toml](rust-toolchain.toml) for the development toolchain, and
inspect [.cargo/config.toml](.cargo/config.toml) when changing target-specific
build behavior.

## Implementation requirements

### Ownership and state

Every resource or mutable value should have an understandable owner. Start with
ordinary owned values and borrows; add reference counting or synchronization
when sharing is part of the design.

For read-only arguments, accept views such as `&str`, `&[T]`, or `&Path`. Pass a
value by ownership when the function retains or consumes it, and use a mutable
borrow for an operation that updates it in place. Avoid APIs that take a borrow
and immediately clone it just to obtain ownership.

A clone should serve a purpose the reviewer can explain. When a borrow is hard
to express, first consider a smaller scope, an intermediate value, splitting
independent fields, or moving a value with `take` or `replace`. Copying immutable
data is reasonable when its cost and semantics are appropriate.

Use types to express valid state. Distinguish modes with enums, absence with
`Option`, and values with different units or identities with suitable wrappers.
Validate constrained values at their construction boundary. Boolean properties
are fine; several positional booleans selecting behavior usually need named
options or a mode type.

Keep cleanup guards alive for the intended scope. If a resource requires a
successful shutdown that can fail, expose an operation the caller can await or
check; leave destruction as bounded cleanup that does not panic.

### Interfaces between components

Make an interface expose what its consumers need while keeping implementation
details local. Choose visibility deliberately, including `pub(crate)` where the
consumer is another module in the same crate. Plain data and protocol records
may intentionally expose fields.

Preserve an ownership or dispatch type when it is part of an existing interface.
For example, [child-agent services](crates/runtime/src/agents.rs) use shared
trait objects, and [backend interfaces](crates/backend/src/lib.rs) include boxed
I/O and shared callbacks. Changing these shapes requires reviewing their
consumers, not just replacing a type in one signature.

Use standard traits where they give callers meaningful behavior. Provide useful
`Debug` output without secrets, meaningful equality and hashing, and `Default`
only when a valid default exists. Restrict `Copy` and `Clone` to types whose
resource and data semantics support them.

Custom equality, ordering, and hashing must describe the same value. Test these
relationships when implementations ignore fields or have non-obvious semantics.
Use `From`, `TryFrom`, or `FromStr` when implementing their standard conversion
and parsing contracts.

Choose functions, concrete types, generics, and trait objects according to the
actual callers. Add a trait for a real substitution or extension boundary, and
keep its requirements small. Introduce a builder or options object when it
clarifies construction; avoid generality with no consumer.

Consider the compatibility effect of changes to fields, enum variants, trait
bounds, object safety, or visibility. Adding `#[non_exhaustive]` or sealing an
existing trait also needs a consumer review. Check wire and stored formats
independently of source compatibility.

### Failure handling

Expected failures belong in the operation's return contract. Invalid input,
denied access, provider errors, unavailable services, and timeouts should be
observable without causing an unintended panic.

Use checked parsing, indexing, arithmetic, and conversions where external input
can violate a precondition. Match optional or fallible results directly instead
of checking them and then unwrapping them. Reserve assertions and `expect` for
internal invariants, and explain why the invariant should hold.

At a reusable API boundary, keep errors useful to callers: implement the relevant
error traits, preserve causes, and retain details needed to distinguish or
troubleshoot failures. Application code can add context using the existing
`anyhow` conventions. Avoid flattening useful error information into a generic
string. Check error lifetime and thread-safety bounds at task boundaries.

A poisoned mutex calls for a decision about the state it protects. Recovery must
have an argument for why using that state remains valid; failure propagation
must fit the component's contract. Poisoning itself is advisory and cannot be
the basis of an unsafe abstraction's soundness. See the
[standard library's mutex documentation](https://doc.rust-lang.org/std/sync/struct.Mutex.html#poisoning).

### Readability and output

Use Rust naming conventions and rustfmt. Name modules and types for their domain
role. Accessor and conversion names should communicate ownership and cost;
provide `is_empty()` when an interface exposes a meaningful `len()`.

Use an iterator pipeline, combinator, loop, or `match` according to which makes
the control flow easiest to inspect. Prefer explicit imports, with appropriate
exceptions for test modules or a framework's documented prelude. Use macros
when a normal function, type, or trait cannot express the requirement clearly.

Give meaningful constants names and document assumptions that cannot be inferred
from their types. Comments should explain an invariant, constraint, or surprising
choice rather than narrating each statement.

Preserve output contracts. If ordering is observable, make it explicit rather
than depending on hash-map iteration. Keep diagnostic logs separate from a CLI's
machine-readable stdout. Remove accidental debug output and redact credentials
or private data from formatting and logs.

## Shared work and asynchronous operations

Medha uses Tokio; the [kernel manifest](crates/kernel/Cargo.toml) documents that
runtime choice. Extend the existing runtime and lifecycle boundaries when adding
concurrent work.

State who owns a task, how callers stop it, how completion is observed, and what
happens to its resources on cancellation or failure. Bound queues, concurrency,
or time where the component needs a limit. Test the behavior affected by those
limits rather than assuming a timeout or cancellation request completes cleanup.

Keep critical sections small and avoid callbacks under a lock. A synchronous
mutex guard should not span an await. If an asynchronous lock must remain held
across an await, explain why the scope and lock ordering are safe. Keep related
invariants within a synchronization boundary that preserves them together.

Maintain required `Send` and `Sync` contracts; use compile-time checks where a
public type or future needs that guarantee. Schedule substantial blocking or
CPU work appropriately so async workers remain responsive, and define shutdown
behavior for work that cannot be interrupted immediately.

Changes to parallel execution must retain authorization before effects,
child-agent permission limits, and consistent recording of outcomes. Performance
work needs the same review of these invariants as other behavioral changes.

## Platform and unsafe boundaries

Use safe implementations when they meet the requirement. New unsafe code needs
a specific justification and a small boundary that can uphold its safety
conditions for every safe caller.

For each unsafe operation or implementation, write a safety explanation covering
the relevant pointer validity, lifetime, aliasing, initialization, alignment, or
thread-safety requirements. Document caller obligations on unsafe functions.
Do not use an unsafe conversion or marker-trait implementation to hide an
unresolved ownership or concurrency issue.

At a foreign interface, check the ABI, representation, pointer lengths, string
format, allocation ownership, and error behavior. Ensure unwinding is compatible
with the ABI and panic strategy; catching an unwind is not protection against
an aborting panic. Consult the
[Rustonomicon's FFI section](https://doc.rust-lang.org/nomicon/ffi.html#ffi-and-unwinding)
for the boundary being used.

Exercise applicable edge cases and platform paths. Record Miri, sanitizer, or
other analysis results when those tools are appropriate and available. Explain
coverage that could not be obtained rather than presenting one platform's result
as proof for another.

## Build configuration and dependencies

Keep code in the component that owns its responsibility. Review new layers,
crates, or runtime owners against the existing architecture before adding them.
Use shared manifest settings where appropriate, including deliberate inheritance
of workspace dependencies and lints.

Explain each new dependency's role and consider maintenance, build scripts,
procedural macros, compilation cost, and binary size. Use suitable version
requirements and update the lockfile for the dependency graph that changed.
The [desktop shell](apps/desktop/src-tauri/Cargo.toml) has its own workspace and
lockfile, so root workspace checks do not cover it completely.

Evaluate supported feature combinations explicitly when changing features or
conditional code. Explain changes that affect an API, message, or stored value.
Use stable language features compatible with the declared minimum Rust version.

Keep lint exceptions local and justified. Use `expect` with a reason where its
semantics fit; use `allow` when that behavior is specifically needed. Enforce the
warnings policy through the existing CI configuration rather than introducing
an unrestricted crate-wide `deny(warnings)` attribute. Review changes to
[workspace lints](Cargo.toml) or [Clippy settings](clippy.toml) as configuration
changes in their own right.

## Tests, documentation, and measurements

Prefer an end-to-end test through the real binary, or through the real agent
loop and tools, over a test of a helper. Add a unit test only for a security
edge case or operating-system behavior that no higher path can reach, and avoid
tests that restate the code. A regression test should reproduce the original
failure when feasible. For a
refactor with missing coverage, establish the relevant behavior before changing
its implementation.

Cover meaningful valid inputs, boundaries, and failures. For concurrent or
persistent behavior, include the interruption, retry, or recovery paths that
the change affects. Properties such as stable ordering, round trips, and
idempotence can be more useful than assertions that mirror implementation steps.

Keep fixtures, clocks, randomness, temporary files, and mock services controlled.
Make any platform, browser, language-server, or external-service prerequisite
explicit. Automated tests should not depend on a paid provider request.
Fuzz untrusted-input boundaries when useful for their risk and scope.

Document the contract of a new or changed public item: its preconditions,
ownership, effects, errors, reachable panics, and safety requirements where
applicable. Prefer examples that show recoverable error propagation. Public
examples and cross-references need verification too.

Measure a representative workload before claiming an optimization. Record the
input, toolchain, build settings, method, and observed result. Investigate
allocation, buffer reuse, batching, or contention as supported by that evidence.
Preserve resistance to malicious keys when considering a faster hasher for
untrusted input.

When a change affects a visible workflow, also exercise it in a branch-built
application or terminal. Automated coverage and a manual observation answer
different questions; report which one was actually obtained.

## Commands and verification evidence

For root-workspace Rust changes, run:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets --no-fail-fast
git diff --check
```

Run `cargo fmt --all` to apply formatting and then inspect the result. During
iteration, `cargo test -p kernel test_name` is a focused example; choose the
package and test filter that actually cover the change.

For changed library documentation or Rustdoc examples, also run:

```sh
cargo doc --workspace --no-deps
cargo test --workspace --doc
```

Review the warnings from documentation builds. Building docs does not enforce
missing documentation unless that lint is enabled; see the
[rustdoc lint reference](https://doc.rust-lang.org/rustdoc/lints.html).

Use [the complete contribution checklist](CONTRIBUTING.md#checks-before-opening-a-pull-request)
for the minimum-Rust check, CI helper tests, desktop checks, scenario runs, and
dependency security commands. Add feature-specific or pedantic checks when they
are relevant, and distinguish them from the baseline CI commands above.

In the PR, list the command, result, and relevant environment. Explain a failure,
skip, or check that was not run. A narrow test run supports only the behavior it
exercised; scenario validation is separate from a model evaluation.

## Review requirements

Review the requested scope and follow its effects into callers or dependencies
where needed. Record the revision and configuration being assessed. Start with
correctness, safety, permissions, and data integrity; then assess failure handling,
compatibility, tests, documentation, and clarity.

A finding needs enough information for the author to act:

- The code location, normally `path:line`.
- The affected requirement or contract.
- The input, event sequence, or code path that creates the problem.
- The expected result and the observed or reasoned consequence.
- A minimal correction or next verification step.

Label a reproduced failure as reproduced. For a source-based concern, explain
the reasoning and what remains to be tested. Combine repeated instances of the
same underlying issue and avoid turning a naming preference into a blocker.

| Review level | Appropriate use |
| --- | --- |
| Blocking | A demonstrated defect, unsound behavior, permission failure, data loss, or an unapproved contract break |
| Improvement needed | A specific reliability, diagnostic, testing, API, or maintenance problem with an explained cost |
| Suggestion | An optional simplification, naming change, or hypothesis needing further evidence |

Include the applicable check results and identify behavior or design decisions
that should be preserved. A scoped review should lead to a scoped fix, with
unrelated changes proposed separately.

## Porting requirements

Treat a language port as a compatibility change with an explicit acceptance
contract. Read the source implementation, its tests, and its consumers before
choosing the Rust API.

1. List the supported inputs, outputs, errors, ordering, side effects, and limits.
   Identify source behavior that is incidental to the implementation language.
2. Translate the existing tests with recognizable names and map each original
   case to its Rust counterpart. Add characterization cases for uncovered
   behavior that the port must retain.
3. Design the target representation around ownership, typed state, and explicit
   failure handling. Implement against those tests.
4. Compare the original and Rust implementations on the same representative,
   boundary, and error cases when the source environment can run.
5. Explain every intentional difference, including any changed bug or ambiguous
   behavior. Get agreement on changes to the accepted contract.

For example, source-language absence may become `Option`, recoverable failures
may become `Result`, manual resource management can become owned values and
cleanup guards, and closed choices can become enums. Choose the representation
from its meaning, not from a mechanical syntax substitution.

Check differences that can change outcomes: insertion order, stable sorting,
Unicode and whitespace handling, invalid encodings, numeric range and overflow,
missing values, and error distinctions. Add compatibility logic only where the
accepted contract requires it. Keep the implementation proportional to the
component, and avoid expanding its API or recreating a language runtime without
an actual requirement.

Alongside normal Rust checks, verify that the port builds:

```sh
cargo build --workspace --locked
```

For a substantial port, include component-level porting notes or a `PORTING.md`
with source version and licensing, API and test mappings, retained quirks,
intentional differences, and original/Rust verification results. Record which
comparisons could not run. Preserve applicable permissions and notices when
bringing source material into the repository.

## Submission checklist

- [ ] The PR states its scope, trigger, intended outcome, and affected contracts.
- [ ] The implementation keeps ownership and state transitions understandable.
- [ ] Errors, concurrency, cancellation, cleanup, and platform behavior have been
  reviewed where affected.
- [ ] Safety arguments and relevant tests accompany new unsafe code.
- [ ] The applicable checks pass, or their results and limitations are explained.
- [ ] Documentation matches the behavior, and performance claims have measurements.
- [ ] Review findings include locations, evidence, and actionable next steps.
- [ ] A language port includes its compatibility contract and test mappings.

Mark items that do not apply with a short reason. Submit the evidence using the
[PR template](.github/PULL_REQUEST_TEMPLATE.md).
