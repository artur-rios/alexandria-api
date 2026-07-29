# Workflow — Alexandria

How a single use case is delivered, from picking it up to closing it out.
The formal, normative version of this process lives in the
[Development Workflow Document](../requirements/Development%20Workflow%20Document.md);
this document is the operational form an implementer follows step by step.

> **One use case = one branch = one issue = one pull request.**

## Invocation

Work starts when a use case is named by its identifier, e.g. `UC-03`. If the
identifier is missing or ambiguous, ask which one before doing anything else.
One pass handles exactly **one use case**.

## The golden rule: pause at every stage boundary

The work is reviewed before it advances. Therefore:

- **The only status change made unattended is `Todo → In Progress`**, right after
  the branch is created. It signals that work has begun.
- **Every other stage transition requires explicit approval first.** Before moving
  to **Testing**, before opening a **pull request**, and before moving to
  **Done**, stop, show what was done, and ask. Do not batch these.
- **Never merge the pull request, never self-approve, never delete the branch.**
  Review, merge, and branch deletion are human actions. An agent may *prepare and
  push* the pull request.

When pausing, summarize what the stage completed, state what comes next, and wait
for a clear go-ahead.

## Workflow overview

```txt
Load specs → Refine (design → plan) → [approval] → Branch + issue→In Progress
  → Implement → [approval] → issue→Testing → Test until green → [approval]
  → Open PR → [human review + merge + delete branch] → [approval] → issue→Done
```

Steps 1–2 and every `[approval]` gate are where the implementer stops.

---

## Step 1 — Load the specifications

Read the relevant requirements documents before designing anything. Pull the
specifics for this use case; do not work from memory:

- [Use Case Specification Document](../requirements/Use%20Case%20Specification%20Document.md)
  — the target use case: actors, pre/postconditions, main flow, and every
  `AF-xx` alternative flow.
- [System Requirements Document](../requirements/System%20Requirements%20Document.md)
  — the `FR-xx` requirements traced to it, plus the data model, interface
  surface, and authorization matrix.
- [Development Workflow Document](../requirements/Development%20Workflow%20Document.md)
  — the normative delivery process.
- [Testing Specification Document](../requirements/Testing%20Specification%20Document.md)
  — how the tests will be written.
- [Technology Stack Document](../requirements/Technology%20Stack%20Document.md)
  — the libraries, versions, and patterns to build with.

Then locate the tracking issue for this use case.

## Step 2 — Refine the design and plan

The specification is the *what*; a repository-specific *how* is still needed
before coding.

1. **Design** — turn the specification and its traced requirements into a
   concrete design for this codebase: which components, interfaces, validation,
   domain behavior, and entry points are needed, and how each alternative flow
   maps to an error or failure response. Ground it in the patterns already
   present in the repository — SOLID and Command/Query for the Rust core, with
   both HTTP and FFI entry points kept consistent.
2. **Plan** — capture the result as a written, step-by-step implementation plan,
   sequenced test-first per the Testing Specification.

**Present the refined design and plan, and wait for approval before writing any
code.** This is the first review gate.

## Step 3 — Branch and move the issue to In Progress

Once the plan is approved, create the branch from an up-to-date `main` branch
using the naming pattern `feature/uc-##-use-case-name`:

```bash
git switch main && git pull
git switch -c feature/uc-01-index-library-files
```

Then — the **one** status change made without asking — move the issue to
**In Progress**.

## Step 4 — Implement

Execute the approved plan, following the repository's established patterns.
Implement the main flow **and every alternative flow** from the specification.
Grow the implementation and its tests together. Commit on the branch as you go.

## Step 5 — Pause for review before Testing

When the implementation is code-complete, **stop and ask** before advancing.
Summarize what was built. Only after approval, move the issue to **Testing**.

## Step 6 — Test until green

Following the [Testing Specification Document](../requirements/Testing%20Specification%20Document.md),
write the tests for this use case (main flow + each applicable `AF-xx`),
run the suite, fix failures, and **re-run until everything passes**:

```bash
cargo test
```

Report the passing results. **Do not open a pull request yet — stop and ask.**

## Step 7 — Open the pull request (after approval)

Once approved, push the branch and open a pull request into the `main` branch,
referencing the issue so the merge closes it. Then **hand off to a human** for
review and merge. Do **not** merge or delete the branch.

## Step 8 — Close out (after the human merges)

After the pull request is merged and the branch deleted, **ask** before finishing,
then move the issue to **Done** and confirm it is closed.

---

## Definition of Done

- [ ] Implemented on a `feature/uc-##-use-case-name` branch created from `main`.
- [ ] Main flow and every alternative flow implemented.
- [ ] Tests cover the use case per the Testing Specification.
- [ ] The full suite passes (`cargo test`).
- [ ] The pull request was reviewed by a human and merged.
- [ ] The branch was deleted.
- [ ] The issue is in **Done** and closed.
