# Domain profiles — when a boundary earns a crate

The family has two layers. The **surface** (SPEC v1, Part 1 of DESIGN.md) is
domain-free and universal: every CLI gets `auth`, `config`, `self-update`,
`--json`, exit codes. A **domain profile** (SPEC §1.8) is a shared vocabulary
for one domain — canonical command spellings plus DTO shapes — packaged as a
`pk-cli-<domain>` crate. `utility/v1` (`pk-cli-utility`) is the first.

This document is the test for adding the next one. The failure mode it guards
against is speculative abstraction: a crate of DTOs nobody consumes is pure
maintenance; a premature canonical spelling is a migration everyone pays for
twice.

## The bar

A domain earns a profile when **all three** hold:

1. **≥ 2 shipped CLIs share the domain** — not "could share": both already
   expose the same concept under different spellings (the 2026-07 audit found
   `payments list` vs `transactions list` vs `history` for the same idea).
2. **A consumer exists that pays for the variance today.** Some driver, script,
   or agent currently carries per-CLI adapters — utiman's `balance-fields`
   fallback chains were exactly this. The profile is designed *from the
   consumer's needs*, not from the union of provider features.
3. **The shared shape is stable.** The concept means the same thing across
   providers and is not still being discovered. If two CLIs disagree about
   what a field *means* (not just what it's called), the domain isn't ready.

Two CLIs merely being *thematically similar* does not meet the bar. The
question is never "are these both smart-home CLIs?" — it is "who is writing
per-CLI glue today, and which shapes would delete that glue?"

## Current candidates (from the 2026-07-19 family audit)

| Domain | CLIs | Bar check | Verdict |
|---|---|---|---|
| utility portals | fpl, tojfl, lrfl, xfin | 4 CLIs, utiman pays the variance, shapes stable | **`utility/v1` — shipped** |
| smart home | govee, tplc | 2 CLIs share devices/power/light/scenes; **no consumer** drives both yet | watch — bar fails on (2). Revisit if a home dashboard or agent flow spans both |
| messaging | discord, slck | 2 CLIs share send/read/channels; no cross-CLI consumer; slck pre-spec | watch — bring slck to SPEC v1 first |
| commerce/registry | bl, tgt | item add/list vs product search — shapes overlap thinly | no — (1) is weak: the shared concept is ~one DTO (product/item) |
| trading | alpaca (+ private tooling) | one public CLI | no — (1) fails outright |

When a "watch" row later meets the bar, the audit that shows it (who is the
consumer, which glue exists) belongs in the PR that adds the profile.

## Design rules for a new profile

- **Name**: profile id `<domain>/v1`, crate `pk-cli-<domain>`. Singular,
  lower-kebab. The id is a contract string — it never changes; revisions bump
  the version (`<domain>/v2`), and a CLI may declare several.
- **Consumer-first**: start from what the driver/agent needs to render or
  decide, not from what providers expose. Every DTO field must have a consumer;
  provider extras stay provider-shaped (fpl's `usage appliances` is fine
  outside the profile).
- **Shapes follow §1.4**: schema-tagged (`<name>/v1`), snake_case, `Money` for
  money (never floats, never bare cents), ISO dates, omit-if-none. Quantities
  are numbers with an explicit `unit`.
- **Lists** emit `Paged<T>` (`<record>-list/v1`, records under `items`) and
  take `RangeArgs` (`--limit`/`--since`/`--until`). No new pagination flags.
- **Spellings**: noun-verb, plural nouns, `list|get` verbs, `ls` alias. When
  existing CLIs disagree, pick the spelling closest to the majority and keep
  old spellings as aliases for one major version (SPEC §1.2 rule).
- **Mutations** in a profile keep the §1.3 confirmation rules; if a mutation
  spends real money or is otherwise driver-unsafe, the profile must also
  define the safe hand-off verb (`pay open` is the model).
- **Declaration**: conforming CLIs add the id to `info.profiles` via
  `CliInfo::with_profiles`. Drivers must treat a missing `profiles` field as
  "no profiles", never as an error.
- **A profile is not a framework**: no traits that force an implementation,
  no provider client code, no async/runtime opinions. DTOs, arg structs,
  spellings, rendering helpers — nothing else.

## Process checklist

1. Show the bar is met (the three conditions, with the consumer named).
2. Write the profile section in DESIGN.md §1.8: command table + DTO list.
3. Add the `pk-cli-<domain>` crate: DTOs + shape tests (schema tag, omit-none).
4. Wire a demonstration into `example-cli` if it's the family's first profile
   of its kind, or into the closest real CLI otherwise.
5. Add a profile column/row to `conformance.md`; migrating CLIs track there.
6. CHANGELOG + minor version bump (adding a profile is additive).
7. Migrate the consumer (e.g. utiman fast path) in the same release window —
   a profile with no consumer migration shipped is a red flag for (2).

## Non-goals

- Cross-language ports of profile crates. The *contract* (spellings, DTO
  shapes, exit codes) is language-neutral and documented here; Go/JS CLIs
  implement it against their own shared libraries.
- Profiles for single-CLI domains, however large the CLI (pup's 51 product
  groups stay pup's).
- A shared "everything" DTO crate. Shapes live with their domain; the moment a
  DTO is generic to all CLIs it belongs in `pk-cli-core`, not a profile.
