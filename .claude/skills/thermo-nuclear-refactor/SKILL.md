---
name: thermo-nuclear-refactor
description: Run an extremely strict maintainability pass over a target folder or the WHOLE repo and actually perform the refactors — fixing abstraction quality, giant files, spaghetti-condition growth, duplication, inconsistent approaches (e.g. mixing date-fns with native Date), and non-functional style. Reads every file one by one and decides, per file, whether to split, merge, or leave it. Use for a thermo-nuclear refactor, thermonuclear code cleanup, deep maintainability rewrite, DRY/functional rewrite, or especially harsh restructuring of a folder or codebase.
disable-model-invocation: true
---

# Thermo-Nuclear Refactor

Use this skill to perform an unusually strict refactor of a target folder (or the whole repo) focused on implementation quality, maintainability, abstraction quality, DRY-ness, functional style, readability, and codebase health. This is not a review — you are expected to **read every file one by one and actually make the changes**, preserving behavior.

Above all, be **ambitious** about code structure. Do not merely apply local cleanups. Actively search for "code judo" moves: restructurings that preserve behavior while making the implementation dramatically simpler, smaller, more direct, and more elegant — then make them.

## Target

This skill can be pointed at a specific folder or run over the entire repo.

- If the user names a folder/path, treat that subtree as the **refactor scope**: read and refactor every file inside it.
- Still read enough of the surrounding repo (callers, shared types, tests, build config) to refactor safely and preserve behavior, but make changes only within scope unless a change outside scope is required to keep behavior identical (flag those explicitly).
- If no target is given, default to the whole repository.

## Core Prompt

Start from this baseline:

> Audit the target scope file by file and refactor it to meaningfully improve code quality without changing behavior.
> Rethink how the code is structured / implemented and rewrite it to be cleaner, DRY, and functional.
> Improve abstractions, modularity, reduce spaghetti code and duplication, improve succinctness and legibility.
> Be ambitious: if there is a clear path to improving the implementation that involves restructuring parts of the codebase, do it.
> Be extremely thorough and rigorous. Measure twice, cut once — then make the cut.

## Required Workflow

Do the work in this order. Do not skip steps.

1. **Inventory.** Walk the target scope. Build a map of packages/modules, entry points, and the biggest / messiest files. Note where tests live and how they run.
2. **Establish a behavior baseline.** Identify the test/build/typecheck commands. Run them and record the current passing state before touching anything. If there is no way to verify behavior, say so explicitly and proceed with extra caution.
3. **Per-file evaluation pass.** Open every file in scope, one by one, and make an explicit decision for each (see "Per-File Decision Procedure"). Record the verdict: leave / split / merge / extract / rewrite, with a one-line reason.
4. **Cross-file consistency pass.** Scan for places where the same kind of task is done two or more different ways, and pick one canonical approach (see "Standard 4: Converge on one way to do each thing"). Inventory imports/dependencies to spot mixed approaches.
5. **Plan.** From the per-file verdicts, cross-file duplication, and inconsistent approaches you found, list the high-conviction refactors in priority order (biggest structural wins first). Prefer changes that delete complexity over changes that move it around.
6. **Execute incrementally.** Refactor one cohesive unit at a time. After each unit, re-run tests/typecheck/build. Keep each step behavior-preserving.
7. **Verify.** After all changes, run the full test/build/typecheck suite again and confirm it matches the baseline. Behavior must be unchanged.
8. **Summarize.** Report what changed, why, what got simpler, and anything you deliberately left alone.

If behavior cannot be kept identical for a given change, stop and flag it rather than silently changing semantics.

## Per-File Decision Procedure

For each file in scope, judge structure by cohesion and responsibility — **not** by raw line or function count. A file with two functions can be perfectly fine, or two unrelated things glued together; decide deliberately.

**Split a file when:**
- It mixes unrelated responsibilities (e.g. data fetching + presentation + validation) that have no reason to change together.
- Distinct functions have non-overlapping dependencies/imports and could move out with no shared private state.
- It crosses ~1000 lines, or is large enough that finding things is hard.
- Different parts have different reasons to change or different audiences (public API vs internal helpers).

**Keep a file as-is (do NOT split) when:**
- The functions are tightly cohesive — they share private helpers, types, or state, or form one logical unit (e.g. a small module with one exported function and its two private helpers). Splitting these scatters logic and hurts readability.
- It is short and single-purpose. Two functions is not a reason to split.
- Splitting would create thin files that only re-export, or force callers to import from three places to do one thing.

**Merge/inline when:** a file exists only to re-export, wrap, or pass through, adding an import hop without clarifying anything.

The test is always: would the split/merge make the code *easier to reason about and change*? If not, leave it. Note the verdict and move on.

## Non-Negotiable Standards

Apply the baseline above, plus these explicit rules:

0. **Be ambitious about structural simplification.**
   - Do not stop at "this could be a bit cleaner."
   - Look for opportunities to reframe code so whole branches, helpers, modes, conditionals, or layers disappear entirely.
   - Prefer the solution that makes the code feel inevitable in hindsight.
   - Assume there is often a "code judo" move available: a re-organization that uses the existing architecture more effectively and makes the code dramatically simpler. Make that move.
   - If you see a path to delete complexity rather than rearrange it, take it.

1. **Right-size files by cohesion, with ~1000 lines as a hard ceiling.**
   - Treat any file over ~1000 lines as a strong code-quality smell and split it.
   - Below that, decide by the Per-File Decision Procedure above, not by line count alone.
   - Only leave a file large if there is a compelling structural reason and it is still clearly organized.

2. **Eliminate spaghetti, don't add to it.**
   - Hunt down ad-hoc conditionals, scattered special cases, and one-off branches inserted into unrelated flows, and refactor them out.
   - Push tangled logic into a dedicated abstraction, helper, state machine, policy object, or separate module.
   - Leave surrounding code easier to reason about than you found it.

3. **Be ruthlessly DRY — but not prematurely.**
   - Find copy-pasted or near-duplicate logic across the scope and collapse it into one canonical helper or function.
   - Reuse existing canonical utilities instead of writing near-duplicates.
   - Do not over-abstract: two superficially similar blocks that are likely to diverge are not duplication. Deduplicate genuine repetition of the same concept, not coincidental similarity.

4. **Converge on one way to do each thing.**
   - Find tasks that are accomplished two or more different ways across the scope and unify them on a single canonical approach. This is distinct from DRY: the logic may legitimately differ, but the *method* should be consistent.
   - Classic cases: a date library (e.g. date-fns) used in one place while native `Date` math or a second library (moment, dayjs, luxon) is used elsewhere; mixed HTTP clients (`fetch` vs `axios`); mixed state/data-fetching, schema/validation, logging, or styling approaches; `lodash` helpers alongside hand-rolled equivalents.
   - Pick the approach that is already dominant, best-supported, or clearly superior, migrate the outliers to it, and remove the now-unused dependency/path.
   - Don't force convergence where two approaches exist for a real reason (e.g. a sync path that genuinely can't use the async client) — but say why it was left.

5. **Prefer a functional style where it improves clarity.**
   - Favor pure functions, immutability, and expression-oriented code over in-place mutation and side-effect-heavy procedures.
   - Replace manual index loops that build up arrays/objects with map/filter/reduce (or comprehensions) when it reads more clearly; keep an explicit loop when that is genuinely simpler.
   - Push side effects (I/O, logging, state writes) to the edges; keep the core logic pure and easy to test.
   - Avoid shared mutable state and hidden temporal coupling.
   - Don't turn this into point-free golf — readability wins over cleverness.

6. **Clean the design, not just the surface.**
   - Where behavior can stay the same while structure becomes meaningfully cleaner, rewrite it to the cleaner version.
   - Do not leave "it works" implementations messy.
   - Strongly prefer removing moving pieces over spreading the same complexity around.

7. **Prefer direct, boring, maintainable code over hacky or magical code.**
   - Replace brittle, ad-hoc, or "magic" behavior with direct implementations.
   - Be skeptical of generic mechanisms that hide simple data-shape assumptions.
   - Delete thin abstractions, identity wrappers, or pass-through helpers that add indirection without buying clarity.

8. **Tighten types and boundaries.**
   - Remove unnecessary optionality, `unknown`, `any`, or cast-heavy code where a clearer type boundary can exist.
   - Prefer explicit typed models or shared contracts over loosely-shaped ad-hoc objects.
   - Where a branch relies on silent fallback to paper over an unclear invariant, make the boundary explicit instead.

9. **Keep logic in the canonical layer and reuse existing helpers.**
   - Pull feature logic out of shared paths; stop implementation details leaking through APIs.
   - Reuse existing canonical utilities/helpers instead of bespoke one-offs.
   - Move code to the right package, service, or module instead of normalizing architectural drift.

10. **Fix needless sequential orchestration and non-atomic updates when the cleaner structure is obvious.**
   - Parallelize independent work that was serialized for no good reason, when it also simplifies the flow.
   - Restructure related updates that can leave state half-applied into a more atomic form.
   - Don't over-index on micro-optimizations, but do remove avoidable orchestration complexity.

## Primary Questions To Drive Each Change

For every meaningful area, ask — and then act on the answer:

- Is there a "code judo" move that makes this dramatically simpler?
- Can this be reframed so fewer concepts, branches, or helper layers are needed?
- Does this file hold one cohesive responsibility, or should it be split / merged?
- Is this logic duplicated elsewhere in scope, and should it be a shared helper?
- Is this task done a different way elsewhere (different library, client, or idiom), and should they converge on one?
- Could this be expressed more clearly as pure functions / a functional pipeline?
- Does this improve the local architecture?
- Can branching complexity be replaced by a better abstraction?
- Is this logic in the right file and layer?
- Do repeated conditionals signal a missing model or helper?
- Is the implementation direct and legible, or does it rely on special cases?
- Is each abstraction earning its keep, or is it just a wrapper?
- Do casts, optionality, or ad-hoc object shapes obscure the real invariant?
- Is orchestration more sequential or less atomic than it needs to be?

## What To Hit Aggressively

Prioritize fixing:

- Complicated implementations where a cleaner reframing deletes whole categories of complexity.
- Files over 1000 lines, or files mixing unrelated responsibilities, that can be split into focused modules.
- Copy-pasted or near-duplicate logic that should be one shared helper.
- The same task done multiple ways (e.g. date-fns in one place, native `Date` or another date library in another; `fetch` vs `axios`; competing validation/state/logging idioms) that should converge on one canonical approach, dropping the redundant dependency.
- Mutation-heavy, side-effect-tangled procedures that read more clearly as pure functions / pipelines.
- Conditionals bolted onto unrelated code paths.
- One-off booleans, nullable modes, or flags that complicate control flow.
- Feature-specific logic leaking into general-purpose modules.
- Generic "magic" handling that hides simple structure.
- Thin wrappers, re-export-only files, or identity abstractions.
- Unnecessary casts, `any`, `unknown`, or optional params muddying the contract.
- Narrow edge-case handling stuffed into an already busy function.
- Bespoke helpers where a canonical utility already exists.
- Logic in the wrong layer/package.
- Sequential async flow where independent work could run in parallel.
- Partial-update logic that leaves state less atomic than necessary.

## Preferred Moves

When you fix a problem, prefer:

- Delete a whole layer of indirection rather than polishing it.
- Reframe the state model so conditionals disappear.
- Change the ownership boundary so a feature becomes a natural extension of an existing abstraction.
- Turn special-case logic into a simpler default flow with fewer exceptions.
- Extract a pure helper or function; collapse duplicates into it.
- Converge competing approaches (libraries, clients, idioms) onto one canonical choice and remove the redundant dependency.
- Split a large or incohesive file into smaller focused modules — and conversely, merge files that only re-export.
- Replace mutation + manual loops with map/filter/reduce pipelines where clearer.
- Push side effects to the edges and keep the core pure.
- Move feature-specific logic behind a dedicated abstraction.
- Replace condition chains with a typed model or explicit dispatcher.
- Separate orchestration from business logic.
- Collapse duplicate branches into a single clearer flow.
- Delete wrappers that don't clarify the API.
- Reuse the canonical helper instead of a near-duplicate.
- Make type boundaries explicit so control flow simplifies.
- Move logic to the package/module/layer that already owns the concept.
- Parallelize independent work when it also simplifies orchestration.
- Restructure related updates into a more atomic flow.

Don't settle for a merely cleaner version of the same messy idea when a much simpler idea is plausible. Don't just move complexity around — delete it.

## Tone Of The Summary

Be direct and serious about what you changed and why. Don't soften major restructurings into vague notes. If a part of the codebase was making things messier and you fixed it, say so plainly. If you spotted a dramatic simplification but couldn't safely make it without changing behavior, call it out as a follow-up.

## Hard Constraints

- **Behavior must be preserved.** Tests/build/typecheck must match the pre-refactor baseline. If you cannot verify, say so.
- **Stay in scope.** If pointed at a folder, change files outside it only when required to preserve behavior, and flag those changes.
- **No scope creep into new features.** This is restructuring, not feature work.
- **Each step stays green.** Don't leave the repo in a broken intermediate state across the whole pass.
- If a change can't be made behavior-preserving, stop and flag it instead of shipping a silent semantic change.

## Done When

- Every file in scope was read and given an explicit leave/split/merge/rewrite verdict.
- No file was pushed past 1000 lines without a strong, stated reason, and incohesive files were split.
- Genuine duplication was collapsed into canonical helpers; no near-duplicate was left behind.
- The same task is done one canonical way across the scope; mixed libraries/clients/idioms were converged and redundant dependencies removed (or the exception was stated).
- Core logic reads in a clear, largely functional style with side effects pushed to the edges.
- No new ad-hoc branching tangles an existing flow.
- No feature checks were scattered across shared code.
- No unnecessary abstraction, wrapper, re-export-only file, or cast-heavy contract was added.
- No canonical helper was duplicated and no logic was left in the wrong layer.
- Obvious decompositions and code-judo simplifications were actually performed.
- The full test/build/typecheck suite passes and matches the baseline.