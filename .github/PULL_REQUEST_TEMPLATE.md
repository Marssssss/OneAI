<!--
Thanks for the PR! Fill in the checklist below. Delete anything that doesn't apply.
Read CONTRIBUTING.md for the conventions (crate layering, 3-layer parser, permission model).
-->

## What & why

<!-- A short summary of the change and the motivation. Link any issue: "Closes #123". -->

## Change type

- [ ] Bug fix
- [ ] New feature / subsystem
- [ ] Refactor (no behavior change)
- [ ] Documentation
- [ ] Tooling / CI
- [ ] Test / eval

## Self-check

- [ ] `cargo fmt --all --check` is clean
- [ ] `cargo clippy --workspace --all-targets` adds **no new warnings**
- [ ] `cargo test --workspace` passes
- [ ] Crate layering respected (no lower→higher imports)
- [ ] New behavior has a test
- [ ] New public enum APIs use `#[non_exhaustive]`
- [ ] README.md and README_EN.md updated and kept in sync (if user-facing)

## Breaking changes

<!-- If this changes a public API, call it out and note migration steps. -->

## Notes for reviewer

<!-- Anything non-obvious, edge cases, or follow-ups you'd defer. -->
