<!--
Thanks for the contribution. Keep the description focused on *why* — the diff
already says what. See CONTRIBUTING.md if this is your first PR here.
-->

## What and why

<!-- What changes, and what problem it solves. Link the issue if there is one. -->

Closes #

## How it was verified

<!--
Which tests you added, and anything you ran by hand. If the change touches
failover, upgrades, backups or auth, say whether you ran the relevant e2e
suite against a stand — CI does not run any of them.
-->

## Checklist

- [ ] `make lint test` passes
- [ ] `make ui-lint ui-test` passes (if the UI changed)
- [ ] New behaviour has a test
- [ ] Ran `make openapi ui-client` and committed the regenerated files
      **with** the code (if any handler or DTO changed) — never hand-edited
      `spec/openapi.yaml` or `ui/src/app/api/**`
- [ ] `make openapi-check ui-client-check` passes
- [ ] New user-visible UI strings go through Transloco (`ui/public/i18n/`)
- [ ] `deploy/k8s/` and `charts/pglord/` changed together, if either did
- [ ] `docs/user-guide.md` updated, if behaviour changed

## Anything reviewers should push back on

<!--
Optional, and genuinely useful: a trade-off you are unsure about, a design
decision you argued with, or a shortcut you took on purpose.
-->
