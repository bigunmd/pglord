# Development under GOLD

This is the shared delivery contract for pglord contributors and agents. Start with
[AGENTS](../AGENTS.md), [contributing](../CONTRIBUTING.md) and the
[current architecture](architecture/c4.md). GOLD here is a process, not a prerequisite
plugin or an assertion that Git hooks are installed.

## 1. Frame the work

Select the scenario: investigation/review, development, architecture, or operations.
Record the outcome, scope/exclusions, acceptance criteria, risks and verification plan.
For small changes this can be a short conversation; substantial local records belong in
ignored `.gold/iterations/<task>/`. Keep secrets out and never force-add those records.

Use Balanced collaboration by default: propose a bounded approach, ask at meaningful
tradeoffs, then work within approval. Guided means smaller user-approved steps;
Delegated means greater execution autonomy within the same explicit boundaries.
Neither grants permission for deployment, publication, destructive recovery or Git actions.
A diagnosis or review alone does not authorize repairs.

## 2. Implement a bounded slice

Get explicit scope approval before implementation; reuse an approval covering the phase.
Choose a small end-to-end slice with a regression test tied to the failure or behavior.
Keep pure policy in core and external effects at the server/agent boundaries. Preserve
public error codes and compatibility unless a change was approved.

When delegating, give each reviewer/worker its baseline, task, expected output, allowed
side effects and disjoint write scope. Require evidence, not just a completion message.
The lead checks the integrated diff and owns the final verification claim.

Record a new tracked ADR only when a real architectural decision needs durable rationale.
Do not turn a reconstructed implementation detail into a fictional historical approval.

## 3. Verify at the affected boundary

| Change | Evidence to plan |
| --- | --- |
| Domain/rendering | Focused Rust tests, workspace tests, formatting and clippy |
| Public API/DTO | Router tests, OpenAPI/client regeneration and drift checks |
| ClusterSpec/CRD shape | Validation tests, CRD regeneration and drift check |
| UI | Adjacent unit tests, lint, production build, targeted browser checks when available |
| HA/backup/upgrade | Unit/contract tests plus explicitly authorized fault-path integration/E2E |
| Deployment/RBAC | Compare Helm and stand wiring; authorized template validation; live changes need separate approval |
| Documentation only | Source cross-check, local links, obsolete-reference search, scope and whitespace diff |

Commands are in the [Makefile](../Makefile). Dependencies must be available before build
checks; installing tools/dependencies is a separate side effect to scope. Generators and
some drift checks rewrite artifacts. Inspect the resulting diff. Stand and E2E targets
use Docker/minikube and can delete pods, volumes or clusters: name the disposable target
and obtain permission before execution. Do not silently redirect tests to a live cluster.

[CI](../.github/workflows/ci.yaml) currently runs Rust fmt/clippy/tests and OpenAPI drift,
UI lint/build and generated-client drift. UI unit tests, CRD drift and E2E are not CI gates.
The [image workflow](../.github/workflows/images.yaml) publishes images separately; running
local checks does not authorize triggering publication.

For critical state transitions, test partial failure and retry, not just success:
- A partitioned primary may still serve clients even when probes fail.
- Annotation CAS does not make spec changes and external effects transactional.
- Interrupted upgrades must never delete the live generation or initialize empty data.
- Restore must preserve the source repository and not promote before recovery finishes.
- Rolling operations must not proceed past an unready replacement unnoticed.

## 4. Review and accept

Check correctness/safety first, then load, test coverage and simplicity. A single server
can receive concurrent mutations, and timers run alongside handlers; one replica is not
one operation at a time. Report defects outside the approved slice rather than repairing
unrelated code.

Present changed behavior, exact checks and outcomes, unexecuted checks, remaining risks,
and rollback/recovery implications. Assess C4 at every closure: update affected context,
container or component views when architecture changes or documentation is inaccurate;
otherwise explain why existing views remain valid. Ask for acceptance of current evidence
before declaring delivery complete. A review's “ship” verdict is advisory, not acceptance.

No automatic commit, push, merge, remote issue closure, deployment or release. Preserve
required licensing and human attribution; do not invent authors or bypass hooks/signing.
